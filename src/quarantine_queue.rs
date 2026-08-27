//! Serial background queue for folder AND single-file quarantines, so reviewing does not mean
//! waiting (#66).
//!
//! **Serial is required, not a simplification.** SQLite has a single writer, and every item
//! re-checks that its files are still `active` immediately before moving them. This queue is
//! serial only within itself, though: `scan_queue::run_worker` is a separate task that also
//! writes to the catalogue. The check above is safe regardless, because it lives inside the
//! quarantine engines (`src/quarantine.rs`, `src/tree_quarantine.rs`) and re-reads the row
//! immediately before acting — the engine's own pre-move check is the actual safety, not the
//! absence of other writers.
//!
//! What the queue changes is *who* waits. The reviewer confirms an item and moves on; the worker
//! drains the list in order. The expensive part — rebuilding the directory tree over every row —
//! happens **once when the queue empties**, not once per item, because twenty quarantines in a row
//! used to mean twenty full rebuilds.

use serde::Serialize;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

#[derive(Clone, Serialize)]
pub struct QuarantineResult {
    /// Monotonically increasing, assigned when the result is pushed. `recent` is capped at
    /// `RECENT_CAP` and reordered to newest-first, so nothing else about a result says whether the
    /// UI has already reported it; `seq` is what lets the poller derive "results I have not shown
    /// yet" instead of re-deriving a message from the whole (truncated) buffer every tick.
    pub seq: u64,
    /// `"tree"` or `"files"`, so the UI can word its message without guessing from the label.
    pub kind: String,
    pub volume_id: String,
    /// The folder path for a tree job; `"3 files"` for a files job. There is no single path for a
    /// multi-file job, so this is deliberately a description rather than a location.
    pub label: String,
    /// Files whose catalogue rows were updated, or 0 when the item failed.
    pub files_updated: usize,
    /// Files `quarantine_files` moved nobody for: could be the last-copy guard, a stale id, a file
    /// that is no longer a loose active entry, or an I/O error re-reading it. `quarantine_files`
    /// does not tell the queue which reason applied for a given id (see `src/quarantine.rs`), so
    /// this is deliberately a count, not a claim about cause — the reason string is still written
    /// to the action log. A skip is not necessarily an error: the reviewer still needs to see the
    /// number either way.
    pub skipped: usize,
    pub dest: Option<String>,
    /// Present exactly when the item failed. A refusal — drive swapped, tree no longer all active —
    /// is reported here rather than swallowed, because the user needs to know their click did not
    /// take effect.
    pub error_message: Option<String>,
}

#[derive(Clone, Serialize)]
pub struct QuarantineJobDto {
    /// `"tree"` or `"files"` — mirrors `QuarantineResult::kind`.
    pub kind: String,
    pub volume_id: String,
    /// What the status bar shows while this job runs or waits: a folder path for a tree job, or a
    /// count like `"3 files"` for a files job (see `Job::label`).
    pub label: String,
}

#[derive(Serialize)]
pub struct QuarantineStatus {
    pub running: Option<QuarantineJobDto>,
    pub pending: Vec<QuarantineJobDto>,
    pub recent: Vec<QuarantineResult>,
}

enum Job {
    Tree {
        volume_id: String,
        path: String,
    },
    Files {
        volume_id: String,
        ids: Vec<i64>,
    },
    /// Extract one archive, verify it, and quarantine the original. `depth` is 1 for an archive
    /// the user picked and increments for each nested archive the previous level produced, so the
    /// recursion is bounded by `ArchiveLimits::max_depth` exactly as the scanner's descent is.
    Extract {
        volume_id: String,
        path: String,
        depth: usize,
    },
}

impl Job {
    fn volume_id(&self) -> &str {
        match self {
            Job::Tree { volume_id, .. }
            | Job::Files { volume_id, .. }
            | Job::Extract { volume_id, .. } => volume_id,
        }
    }

    fn kind(&self) -> &'static str {
        match self {
            Job::Tree { .. } => "tree",
            Job::Files { .. } => "files",
            Job::Extract { .. } => "extract",
        }
    }

    /// What the status bar shows while this item runs.
    fn label(&self) -> String {
        match self {
            Job::Tree { path, .. } => path.clone(),
            Job::Files { ids, .. } => {
                format!(
                    "{} file{}",
                    ids.len(),
                    if ids.len() == 1 { "" } else { "s" }
                )
            }
            Job::Extract { path, .. } => path.clone(),
        }
    }

    fn dto(&self) -> QuarantineJobDto {
        QuarantineJobDto {
            kind: self.kind().to_string(),
            volume_id: self.volume_id().to_string(),
            label: self.label(),
        }
    }
}

struct Inner {
    pending: VecDeque<Job>,
    running: Option<Job>,
    recent: VecDeque<QuarantineResult>,
    /// Volumes mutated since the last drain. The rebuild at the end of a run has to cover all of
    /// them: a review session that quarantines on both drives would otherwise leave the first
    /// drive's directory index describing folders that have already moved.
    touched: std::collections::HashSet<String>,
    /// Next `seq` to assign. Only ever incremented, so a UI polling `status()` can tell "have I
    /// reported this one already" from the number alone, independent of `recent`'s 50-item cap.
    next_seq: u64,
}

impl Inner {
    /// Every job the queue still owns: the running one first, then those waiting.
    fn jobs(&self) -> impl Iterator<Item = &Job> {
        self.running.iter().chain(self.pending.iter())
    }
}

/// What the blocking half of a job actually has to do, lifted out of `Job` so the mutex is not held
/// across the `await`.
enum Work {
    Tree(String),
    Files(Vec<i64>),
    Extract { path: String, depth: usize },
}

/// The shape all three engines collapse to once they succeed.
struct Done {
    files_updated: usize,
    skipped: usize,
    dest: Option<String>,
    /// Archives this job wrote that are themselves extractable, with the depth they sit at.
    nested: Vec<(String, usize)>,
}

/// Pure refusal check for `max_archive_depth`: `Some(message)` when `depth` has gone past `max`,
/// `None` when it is still within bounds. Split out from `Work::Extract`'s arm so the "stopping
/// point must be reported, not silent" requirement can be tested directly, without spinning up a
/// worker, a catalogue, or a mounted drive just to check a string.
fn depth_refusal(path: &str, depth: usize, max: usize) -> Option<String> {
    if depth > max {
        Some(format!(
            "{path} sits {depth} archives deep, past the max_archive_depth of {max}; \
             extract it by hand or raise the limit"
        ))
    } else {
        None
    }
}

pub struct QuarantineQueue {
    catalog_path: PathBuf,
    mounts: crate::mounts::MountResolver,
    inner: Mutex<Inner>,
    notify: tokio::sync::Notify,
}

const RECENT_CAP: usize = 50;

impl QuarantineQueue {
    pub fn new(
        catalog_path: PathBuf,
        mounts: crate::mounts::MountResolver,
    ) -> Arc<QuarantineQueue> {
        Arc::new(QuarantineQueue {
            catalog_path,
            mounts,
            inner: Mutex::new(Inner {
                pending: VecDeque::new(),
                running: None,
                recent: VecDeque::new(),
                touched: std::collections::HashSet::new(),
                next_seq: 0,
            }),
            notify: tokio::sync::Notify::new(),
        })
    }

    /// Add a folder quarantine; returns how many are ahead of it (0 = starts next).
    ///
    /// Deliberately does no validation beyond de-duplication: the worker re-checks everything
    /// immediately before acting, and a check here would be stale by the time the item ran.
    pub fn enqueue_tree(self: &Arc<Self>, volume_id: String, path: String) -> usize {
        let pos = {
            let mut inner = self.inner.lock().unwrap();
            // Double-clicking a row must not queue the same folder twice. The second attempt would
            // fail harmlessly (the path is gone by then), but reporting it as an error would be
            // noise about something the user did not do wrong.
            let dup = inner.jobs().any(|j| match j {
                Job::Tree {
                    volume_id: v,
                    path: p,
                } => v == &volume_id && p == &path,
                Job::Files { .. } | Job::Extract { .. } => false,
            });
            if dup {
                return inner.pending.len();
            }
            inner.pending.push_back(Job::Tree { volume_id, path });
            inner.pending.len() - 1 + inner.running.is_some() as usize
        };
        self.notify.notify_one();
        pos
    }

    /// Add a single-file quarantine for ids on one volume.
    ///
    /// Ids already queued (pending or running) are filtered out rather than rejecting the whole
    /// request: a reviewer who double-clicks has still made one real decision, and the ids that are
    /// genuinely new must not be lost with the duplicates. Returns `None` when nothing was left to
    /// queue, so the caller can say "already queued" instead of reporting a phantom job.
    pub fn enqueue_files(self: &Arc<Self>, volume_id: String, ids: Vec<i64>) -> Option<usize> {
        let pos = {
            let mut inner = self.inner.lock().unwrap();
            let queued: std::collections::HashSet<i64> = inner
                .jobs()
                .filter_map(|j| match j {
                    Job::Files { volume_id: v, ids } if v == &volume_id => Some(ids),
                    _ => None,
                })
                .flatten()
                .copied()
                .collect();
            let fresh: Vec<i64> = ids.into_iter().filter(|id| !queued.contains(id)).collect();
            if fresh.is_empty() {
                return None;
            }
            inner.pending.push_back(Job::Files {
                volume_id,
                ids: fresh,
            });
            inner.pending.len() - 1 + inner.running.is_some() as usize
        };
        self.notify.notify_one();
        Some(pos)
    }

    /// Add an archive extraction; returns how many are ahead of it (0 = starts next).
    pub fn enqueue_extract(self: &Arc<Self>, volume_id: String, path: String) -> usize {
        self.enqueue_extract_at_depth(volume_id, path, 1)
    }

    /// Shared by `enqueue_extract` (depth 1, a user's click) and the post-job enqueue of nested
    /// archives a level wrote (depth + 1). De-duplicates the same way the other kinds do: an
    /// archive already queued, pending or running, must not be queued twice.
    fn enqueue_extract_at_depth(
        self: &Arc<Self>,
        volume_id: String,
        path: String,
        depth: usize,
    ) -> usize {
        let pos = {
            let mut inner = self.inner.lock().unwrap();
            let dup = inner.jobs().any(|j| match j {
                Job::Extract {
                    volume_id: v,
                    path: p,
                    ..
                } => v == &volume_id && p == &path,
                _ => false,
            });
            if dup {
                return inner.pending.len();
            }
            inner.pending.push_back(Job::Extract {
                volume_id,
                path,
                depth,
            });
            inner.pending.len() - 1 + inner.running.is_some() as usize
        };
        self.notify.notify_one();
        pos
    }

    pub fn status(&self) -> QuarantineStatus {
        let inner = self.inner.lock().unwrap();
        QuarantineStatus {
            running: inner.running.as_ref().map(Job::dto),
            pending: inner.pending.iter().map(Job::dto).collect(),
            recent: inner.recent.iter().cloned().collect(),
        }
    }

    /// Test-only: the catalogue this queue writes to. Tests need to read back what the worker did.
    #[cfg(test)]
    pub(crate) fn catalog_path_for_test(&self) -> &std::path::Path {
        &self.catalog_path
    }

    /// Background loop: drain the queue one item at a time, forever.
    pub async fn run_worker(self: Arc<Self>) {
        loop {
            let job = {
                let mut inner = self.inner.lock().unwrap();
                inner.pending.pop_front()
            };
            match job {
                Some(job) => self.run_job(job).await,
                None => self.notify.notified().await,
            }
        }
    }

    async fn run_job(self: &Arc<Self>, job: Job) {
        let (volume_id, kind, label) = (
            job.volume_id().to_string(),
            job.kind().to_string(),
            job.label(),
        );
        let work = match &job {
            Job::Tree { path, .. } => Work::Tree(path.clone()),
            Job::Files { ids, .. } => Work::Files(ids.clone()),
            Job::Extract { path, depth, .. } => Work::Extract {
                path: path.clone(),
                depth: *depth,
            },
        };
        {
            let mut inner = self.inner.lock().unwrap();
            inner.running = Some(job);
        }

        let mount = self.mounts.resolve(&volume_id);
        let catalog_path = self.catalog_path.clone();
        let vid = volume_id.clone();

        // Off the async runtime: quarantine re-hashes before it moves anything, so a multi-GB group
        // takes real time, and the largest single tree here is 326,569 files.
        let joined = tokio::task::spawn_blocking(move || -> anyhow::Result<Done> {
            let mount = mount.ok_or_else(|| anyhow::anyhow!("drive not connected"))?;
            let cat = crate::catalog::Catalog::open(&catalog_path)?;
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_secs() as i64;
            Ok(match work {
                Work::Tree(path) => {
                    let out =
                        crate::tree_quarantine::quarantine_tree(&cat, &mount, &vid, &path, now)?;
                    Done {
                        files_updated: out.files_updated,
                        skipped: 0,
                        dest: Some(out.dest_relative_path),
                        nested: Vec::new(),
                    }
                }
                Work::Files(ids) => {
                    let out = crate::quarantine::quarantine_files(&cat, &mount, &vid, &ids, now)?;
                    Done {
                        files_updated: out.quarantined,
                        skipped: out.skipped,
                        dest: None,
                        nested: Vec::new(),
                    }
                }
                Work::Extract { path, depth } => {
                    let cfg = crate::config::Config::default_paths()?;
                    let limits = crate::archive::ArchiveLimits::from_config(&cfg);
                    if let Some(msg) = depth_refusal(&path, depth, limits.max_depth) {
                        anyhow::bail!(msg);
                    }
                    let out =
                        crate::extract::extract_archive(&cat, &mount, &vid, &path, &limits, now)?;
                    Done {
                        files_updated: out.entries_converted,
                        skipped: usize::from(!out.quarantined),
                        dest: Some(out.dest_relative_path),
                        nested: out
                            .nested_archives
                            .into_iter()
                            .map(|p| (p, depth + 1))
                            .collect(),
                    }
                }
            })
        })
        .await;

        // Lifted out of `Done` here, before the match below consumes it, so it survives past the
        // point the result is recorded. `nested` is drained (see below) only once the result is
        // pushed, so a crash between the two would at worst lose queued nested archives rather
        // than the record of what this job itself did.
        let mut nested: Vec<(String, usize)> = Vec::new();

        // `seq` is assigned below, under the lock, so it is truly monotonic across concurrent
        // completions; 0 here is just a placeholder for the field.
        let mut result = match joined {
            Ok(Ok(d)) => {
                nested = d.nested;
                QuarantineResult {
                    seq: 0,
                    kind,
                    volume_id: volume_id.clone(),
                    label,
                    files_updated: d.files_updated,
                    skipped: d.skipped,
                    dest: d.dest,
                    error_message: None,
                }
            }
            Ok(Err(e)) => QuarantineResult {
                seq: 0,
                kind,
                volume_id: volume_id.clone(),
                label,
                files_updated: 0,
                skipped: 0,
                dest: None,
                error_message: Some(e.to_string()),
            },
            Err(e) => QuarantineResult {
                seq: 0,
                kind,
                volume_id: volume_id.clone(),
                label,
                files_updated: 0,
                skipped: 0,
                dest: None,
                error_message: Some(format!("quarantine task failed: {e}")),
            },
        };

        {
            let mut inner = self.inner.lock().unwrap();
            inner.running = None;
            inner.touched.insert(volume_id.clone());
            result.seq = inner.next_seq;
            inner.next_seq += 1;
            inner.recent.push_front(result);
            while inner.recent.len() > RECENT_CAP {
                inner.recent.pop_back();
            }
        }

        // Enqueue what this job produced now that its own result is recorded (rather than inside
        // extract.rs), so the queue stays the only thing that knows about the queue. This must
        // happen BEFORE the drained check below: a nested archive still waiting is "not drained",
        // and checking drained first would let the post-drain rebuild fire one job too early, with
        // a nested extraction sitting in `pending` behind it.
        for (path, depth) in nested {
            self.enqueue_extract_at_depth(volume_id.clone(), path, depth);
        }

        let (drained, touched) = {
            let mut inner = self.inner.lock().unwrap();
            let drained = inner.pending.is_empty();
            let touched = if drained {
                std::mem::take(&mut inner.touched)
            } else {
                std::collections::HashSet::new()
            };
            (drained, touched)
        };

        // Rebuild ONCE, when there is nothing left to do. Rebuilding per item meant reprocessing
        // every row in the catalogue for each folder moved; the review list is stale in exactly the
        // same way after one item or twenty, so the work only needs doing when the user is about to
        // look again.
        if drained {
            let catalog_path = self.catalog_path.clone();
            let outcome = tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
                let cat = crate::catalog::Catalog::open(&catalog_path)?;
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)?
                    .as_secs() as i64;
                for vid in &touched {
                    cat.rebuild_directory_trees(vid, now)?;
                    cat.refresh_volume_totals(vid)?;
                }
                drop(cat);
                // Best-effort: a failed snapshot must not look like a failed quarantine. The
                // quarantine already happened and is already recorded.
                let _ = crate::catalog::backup::snapshot_beside(&catalog_path, now);
                Ok(())
            })
            .await;
            // The quarantine itself already happened and is already recorded, so this is
            // deliberately best-effort -- but a silently discarded error here means the review
            // list can go stale (rebuild) or a rollback point can go missing (snapshot) with
            // nothing in the logs to explain either. See #79 for the wider question of whether
            // this loop should retry instead of just moving on; out of scope here.
            match outcome {
                Ok(Ok(())) => {}
                Ok(Err(e)) => {
                    tracing::warn!("post-drain rebuild/snapshot failed: {e:#}");
                }
                Err(e) => {
                    tracing::warn!("post-drain rebuild/snapshot task did not complete: {e}");
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn queue() -> Arc<QuarantineQueue> {
        QuarantineQueue::new(
            PathBuf::from("unused.db"),
            crate::mounts::MountResolver::Fixed(Default::default()),
        )
    }

    #[test]
    fn enqueue_reports_position_and_preserves_order() {
        let q = queue();
        assert_eq!(
            q.enqueue_tree("v".into(), "a".into()),
            0,
            "first starts next"
        );
        assert_eq!(q.enqueue_tree("v".into(), "b".into()), 1);
        assert_eq!(q.enqueue_tree("v".into(), "c".into()), 2);
        let s = q.status();
        let paths: Vec<&str> = s.pending.iter().map(|j| j.label.as_str()).collect();
        assert_eq!(paths, vec!["a", "b", "c"], "order must be preserved");
        assert!(s.running.is_none());
    }

    #[test]
    fn the_same_folder_cannot_be_queued_twice() {
        // Double-clicking a row would otherwise queue it again; the second attempt fails once the
        // path is gone, and reporting that as an error blames the user for a stutter.
        let q = queue();
        q.enqueue_tree("v".into(), "same".into());
        q.enqueue_tree("v".into(), "same".into());
        assert_eq!(q.status().pending.len(), 1);
    }

    #[test]
    fn the_same_path_on_a_different_drive_is_a_different_item() {
        // Both drives here were first seen as `D:\` and share folder names, so keying on path alone
        // would silently drop a real second decision.
        let q = queue();
        q.enqueue_tree("uni-big".into(), "Lezioni/Google Drive".into());
        q.enqueue_tree("uni-small".into(), "Lezioni/Google Drive".into());
        assert_eq!(q.status().pending.len(), 2);
    }

    #[test]
    fn a_files_job_queues_behind_a_tree_job() {
        // Both kinds share one worker on purpose: two queues would let a folder move and a file
        // move interleave, and each job's "still active?" check is only meaningful while nothing
        // else is writing.
        let q = queue();
        assert_eq!(q.enqueue_tree("v".into(), "copy".into()), 0);
        assert_eq!(q.enqueue_files("v".into(), vec![1, 2, 3]), Some(1));
        let s = q.status();
        let kinds: Vec<&str> = s.pending.iter().map(|j| j.kind.as_str()).collect();
        assert_eq!(kinds, vec!["tree", "files"]);
    }

    #[test]
    fn a_files_job_is_labelled_by_its_count_not_a_path() {
        // There is no single path for a multi-file job, and the status bar has to say something
        // truthful while it runs.
        let q = queue();
        q.enqueue_files("v".into(), vec![7, 8]);
        assert_eq!(q.status().pending[0].label, "2 files");
        q.enqueue_files("v".into(), vec![9]);
        assert_eq!(q.status().pending[1].label, "1 file");
    }

    #[test]
    fn ids_already_queued_are_not_queued_again() {
        // Double-clicking Confirm must not queue the same move twice. The second attempt would be
        // refused harmlessly by the guard, but reporting that as an error blames the user for a
        // stutter they did not cause.
        let q = queue();
        q.enqueue_files("v".into(), vec![1, 2]);
        assert_eq!(
            q.enqueue_files("v".into(), vec![2, 3]),
            Some(1),
            "the new id still queues"
        );
        let s = q.status();
        assert_eq!(s.pending.len(), 2);
        assert_eq!(
            s.pending[1].label, "1 file",
            "only id 3 survived the filter"
        );
        assert_eq!(
            q.enqueue_files("v".into(), vec![1]),
            None,
            "nothing new to do"
        );
        assert_eq!(q.status().pending.len(), 2, "no empty job was queued");
    }

    #[test]
    fn the_same_id_on_a_different_drive_is_a_different_item() {
        // Catalogue ids are unique across volumes, but the filter must key on the volume too so a
        // future change to id allocation cannot silently drop a real decision.
        let q = queue();
        q.enqueue_files("uni-big".into(), vec![1]);
        q.enqueue_files("uni-small".into(), vec![1]);
        assert_eq!(q.status().pending.len(), 2);
    }

    /// A fake mounted drive carrying its marker, two identical files, and a catalogue that knows
    /// about both. `copy/a.txt` is the redundant one; `a.txt` is the survivor.
    fn fake_drive() -> (tempfile::TempDir, PathBuf, Arc<QuarantineQueue>) {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("c.db");
        let drive = tmp.path().join("driveA");
        std::fs::create_dir_all(drive.join("copy")).unwrap();
        std::fs::write(drive.join(".cleanupstorages_id"), "vol-1").unwrap();
        std::fs::write(drive.join("a.txt"), b"SAME").unwrap();
        std::fs::write(drive.join("copy/a.txt"), b"SAME").unwrap();
        {
            let cat = crate::catalog::Catalog::open(&db).unwrap();
            cat.upsert_volume(&crate::catalog::models::Volume {
                volume_id: "vol-1".into(),
                label: "D".into(),
                identified_by: "marker".into(),
                first_seen_at: 1,
                last_seen_at: 1,
            })
            .unwrap();
            let mk = |path: &str| crate::catalog::models::NewFile {
                volume_id: "vol-1".into(),
                relative_path: path.into(),
                filename: path.rsplit('/').next().unwrap().into(),
                extension: "txt".into(),
                size_bytes: 4,
                content_hash: "SAMEHASH".into(),
                created_time: None,
                modified_time: None,
                accessed_time: None,
                category: crate::category::Category::Document,
                container_chain: None,
            };
            cat.upsert_file(&mk("a.txt"), 100).unwrap();
            cat.upsert_file(&mk("copy/a.txt"), 100).unwrap();
            cat.rebuild_directory_trees("vol-1", 100).unwrap();
        }
        let mut mounts = std::collections::HashMap::new();
        mounts.insert("vol-1".to_string(), drive.clone());
        let q = QuarantineQueue::new(db, crate::mounts::MountResolver::Fixed(mounts));
        (tmp, drive, q)
    }

    /// Wait for the queue to go idle, rather than assuming an instant.
    async fn drain(q: &Arc<QuarantineQueue>) {
        for _ in 0..400 {
            let s = q.status();
            if s.running.is_none() && s.pending.is_empty() && !s.recent.is_empty() {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        panic!("queue never drained");
    }

    fn loose_id(q: &Arc<QuarantineQueue>, path: &str) -> i64 {
        let cat = crate::catalog::Catalog::open_readonly(q.catalog_path_for_test()).unwrap();
        cat.loose_file_id("vol-1", path).unwrap().unwrap()
    }

    #[tokio::test]
    async fn a_files_job_moves_the_chosen_copy_and_leaves_the_survivor() {
        let (_t, drive, q) = fake_drive();
        let victim = loose_id(&q, "copy/a.txt");
        tokio::spawn(q.clone().run_worker());
        q.enqueue_files("vol-1".into(), vec![victim]);
        drain(&q).await;

        assert!(drive.join("_ToDelete/copy/a.txt").is_file(), "moved");
        assert!(!drive.join("copy/a.txt").exists(), "gone from its old home");
        assert!(drive.join("a.txt").is_file(), "the survivor stays put");

        let r = &q.status().recent[0];
        assert_eq!(r.kind, "files");
        assert_eq!(r.files_updated, 1);
        assert_eq!(r.skipped, 0);
        assert!(r.error_message.is_none(), "{:?}", r.error_message);
    }

    #[tokio::test]
    async fn the_last_copy_guard_reports_a_skip_not_an_error() {
        // Quarantining BOTH copies would leave nothing. The guard protects the second one, and the
        // reviewer has to see that number — a silent skip reads as success.
        let (_t, _drive, q) = fake_drive();
        let a = loose_id(&q, "a.txt");
        let b = loose_id(&q, "copy/a.txt");
        tokio::spawn(q.clone().run_worker());
        q.enqueue_files("vol-1".into(), vec![a, b]);
        drain(&q).await;

        let r = &q.status().recent[0];
        assert_eq!(r.files_updated, 1, "one copy moved");
        assert_eq!(r.skipped, 1, "the last copy was protected");
        assert!(r.error_message.is_none(), "a guarded skip is not a failure");
    }

    #[tokio::test]
    async fn seq_is_monotonic_across_results_so_the_ui_can_tell_what_it_has_not_reported_yet() {
        // F2: the UI polls `recent`, which the server caps at RECENT_CAP -- a stale failure or
        // skip from long ago must not stay visible forever just because it's still in the buffer,
        // and a real one must not be missed just because the buffer briefly overflowed. `seq` is
        // what lets the poller ask "results newer than the highest I've already shown" instead of
        // re-deriving a message from the whole truncated buffer every tick.
        let (_t, drive, q) = fake_drive();
        std::fs::write(drive.join("b.txt"), b"OTHER").unwrap();
        std::fs::write(drive.join("copy/b.txt"), b"OTHER").unwrap();
        {
            let cat = crate::catalog::Catalog::open(q.catalog_path_for_test()).unwrap();
            let mk = |path: &str| crate::catalog::models::NewFile {
                volume_id: "vol-1".into(),
                relative_path: path.into(),
                filename: path.rsplit('/').next().unwrap().into(),
                extension: "txt".into(),
                size_bytes: 5,
                content_hash: "OTHERHASH".into(),
                created_time: None,
                modified_time: None,
                accessed_time: None,
                category: crate::category::Category::Document,
                container_chain: None,
            };
            cat.upsert_file(&mk("b.txt"), 100).unwrap();
            cat.upsert_file(&mk("copy/b.txt"), 100).unwrap();
        }
        tokio::spawn(q.clone().run_worker());

        let first = loose_id(&q, "copy/a.txt");
        q.enqueue_files("vol-1".into(), vec![first]);
        drain(&q).await;
        let seq_first = q.status().recent[0].seq;

        let second = loose_id(&q, "copy/b.txt");
        q.enqueue_files("vol-1".into(), vec![second]);
        for _ in 0..400 {
            let s = q.status();
            if s.running.is_none() && s.pending.is_empty() && s.recent.len() >= 2 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        let recent = q.status().recent;
        assert_eq!(recent.len(), 2, "both jobs are recorded");
        assert!(
            recent[0].seq > seq_first,
            "the newer result must carry a strictly higher seq than the earlier one \
             (newest-first: recent[0]={}, first result was {})",
            recent[0].seq,
            seq_first
        );
        assert_eq!(recent[1].seq, seq_first, "seq is assigned once, in order");
    }

    #[tokio::test]
    async fn a_queue_spanning_two_drives_rebuilds_both_indexes() {
        // Today the drain rebuilds the LAST job's volume only. With files jobs enqueued per volume,
        // a review session touching both drives leaves one drive's directory index stale, and the
        // review list then shows folders that have already moved.
        let (_t, drive, q) = fake_drive();
        // A second volume on the same catalogue, with its own marker and its own duplicate pair.
        let drive_b = _t.path().join("driveB");
        std::fs::create_dir_all(drive_b.join("copy")).unwrap();
        std::fs::write(drive_b.join(".cleanupstorages_id"), "vol-2").unwrap();
        std::fs::write(drive_b.join("b.txt"), b"OTHER").unwrap();
        std::fs::write(drive_b.join("copy/b.txt"), b"OTHER").unwrap();
        {
            let cat = crate::catalog::Catalog::open(q.catalog_path_for_test()).unwrap();
            cat.upsert_volume(&crate::catalog::models::Volume {
                volume_id: "vol-2".into(),
                label: "E".into(),
                identified_by: "marker".into(),
                first_seen_at: 1,
                last_seen_at: 1,
            })
            .unwrap();
            let mk = |path: &str| crate::catalog::models::NewFile {
                volume_id: "vol-2".into(),
                relative_path: path.into(),
                filename: path.rsplit('/').next().unwrap().into(),
                extension: "txt".into(),
                size_bytes: 5,
                content_hash: "OTHERHASH".into(),
                created_time: None,
                modified_time: None,
                accessed_time: None,
                category: crate::category::Category::Document,
                container_chain: None,
            };
            cat.upsert_file(&mk("b.txt"), 100).unwrap();
            cat.upsert_file(&mk("copy/b.txt"), 100).unwrap();
            cat.rebuild_directory_trees("vol-2", 100).unwrap();
        }
        let mut mounts = std::collections::HashMap::new();
        mounts.insert("vol-1".to_string(), drive);
        mounts.insert("vol-2".to_string(), drive_b);
        let q = QuarantineQueue::new(
            q.catalog_path_for_test().to_path_buf(),
            crate::mounts::MountResolver::Fixed(mounts),
        );
        let a = {
            let cat = crate::catalog::Catalog::open_readonly(q.catalog_path_for_test()).unwrap();
            cat.loose_file_id("vol-1", "copy/a.txt").unwrap().unwrap()
        };
        let b = {
            let cat = crate::catalog::Catalog::open_readonly(q.catalog_path_for_test()).unwrap();
            cat.loose_file_id("vol-2", "copy/b.txt").unwrap().unwrap()
        };

        tokio::spawn(q.clone().run_worker());
        q.enqueue_files("vol-1".into(), vec![a]);
        q.enqueue_files("vol-2".into(), vec![b]);

        // Poll the EFFECT this test asserts -- vol-1's stale row disappearing -- and never a proxy
        // for it. `run_job` clears `running` and pushes its result into `recent` INSIDE the lock,
        // then awaits the rebuild afterwards, so a queue that merely looks idle can still have the
        // rebuild in flight. Waiting on "idle with two results" is therefore a race: it passed
        // locally and on macOS, where the rebuild won, and failed on the slower Windows runner.
        //
        // The failing-before-the-fix property is preserved: if the drain went back to rebuilding
        // only the last job's volume, this loop would exhaust its budget with `stale` still 1 and
        // the assertion below would still fail -- just slower.
        let read_stale = || {
            let cat = crate::catalog::Catalog::open_readonly(q.catalog_path_for_test()).unwrap();
            cat.conn
                .query_row(
                    "SELECT COUNT(*) FROM directory_trees WHERE volume_id='vol-1' AND path='copy'",
                    [],
                    |r| r.get::<_, i64>(0),
                )
                .unwrap()
        };
        // vol-1 ran FIRST, so it is the one the old code forgot to rebuild.
        let mut stale = read_stale();
        for _ in 0..400 {
            if stale == 0 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
            stale = read_stale();
        }
        assert_eq!(
            stale, 0,
            "vol-1's index must be rebuilt too, not just the last job's volume"
        );
    }

    #[tokio::test]
    async fn draining_writes_a_catalogue_snapshot() {
        // The synchronous handler used to snapshot after every mutating request. Moving the work to
        // the worker moved that responsibility with it, and folder quarantines — which never
        // snapshotted at all — gain the same net.
        let (_t, _drive, q) = fake_drive();
        let victim = loose_id(&q, "copy/a.txt");
        tokio::spawn(q.clone().run_worker());
        q.enqueue_files("vol-1".into(), vec![victim]);
        drain(&q).await;
        for _ in 0..200 {
            let backups = q
                .catalog_path_for_test()
                .parent()
                .unwrap()
                .join("catalog.backups");
            if std::fs::read_dir(&backups).map(|d| d.count()).unwrap_or(0) > 0 {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        panic!("draining the queue must snapshot the catalogue it mutated");
    }

    #[test]
    fn extract_jobs_queue_behind_quarantines_and_report_their_position() {
        let q = queue();
        assert_eq!(q.enqueue_tree("v".into(), "a".into()), 0);
        assert_eq!(q.enqueue_extract("v".into(), "b.zip".into()), 1);
        let s = q.status();
        assert_eq!(s.pending[1].kind, "extract");
        assert_eq!(s.pending[1].label, "b.zip");
    }

    #[test]
    fn the_same_archive_is_never_queued_twice() {
        let q = queue();
        q.enqueue_extract("v".into(), "b.zip".into());
        q.enqueue_extract("v".into(), "b.zip".into());
        assert_eq!(
            q.status().pending.len(),
            1,
            "a double click is one decision"
        );
    }

    #[test]
    fn depth_refusal_reports_the_path_depth_and_limit_when_over_and_stays_silent_when_within() {
        // The stopping point at `max_archive_depth` must be reported, not silently swallowed --
        // this is the pure helper `Work::Extract` calls before doing any real work.
        assert_eq!(
            depth_refusal("a.zip", 1, 8),
            None,
            "top-level archive is depth 1"
        );
        assert_eq!(
            depth_refusal("bundle/inner.zip", 8, 8),
            None,
            "exactly at the limit is fine"
        );
        let msg = depth_refusal("bundle/inner/deeper.zip", 9, 8)
            .expect("depth past the limit must be refused");
        assert!(
            msg.contains("bundle/inner/deeper.zip"),
            "names the path: {msg}"
        );
        assert!(msg.contains('9'), "names the depth: {msg}");
        assert!(msg.contains('8'), "names the limit: {msg}");
    }

    #[tokio::test]
    async fn a_files_job_for_an_unplugged_drive_fails_loudly() {
        // The reviewer clicked. If it did not happen they must be told, because the alternative is
        // believing a duplicate was handled when it was not.
        let (_t, _drive, q) = fake_drive();
        let victim = loose_id(&q, "copy/a.txt");
        let q = QuarantineQueue::new(
            q.catalog_path_for_test().to_path_buf(),
            crate::mounts::MountResolver::Fixed(Default::default()),
        );
        tokio::spawn(q.clone().run_worker());
        q.enqueue_files("vol-1".into(), vec![victim]);
        drain(&q).await;

        let r = &q.status().recent[0];
        assert_eq!(r.files_updated, 0);
        assert!(
            r.error_message
                .as_deref()
                .unwrap_or("")
                .contains("not connected"),
            "got {:?}",
            r.error_message
        );
    }
}
