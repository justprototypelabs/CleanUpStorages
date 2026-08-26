//! Serial background queue for folder quarantines, so reviewing does not mean waiting (#66).
//!
//! **Serial is required, not a simplification.** SQLite has a single writer, and every item
//! re-checks that its files are still `active` immediately before moving them — a check that only
//! means anything if nothing else is mutating the catalogue at the same time.
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
    /// `"tree"` or `"files"`, so the UI can word its message without guessing from the label.
    pub kind: String,
    pub volume_id: String,
    /// The folder path for a tree job; `"3 files"` for a files job. There is no single path for a
    /// multi-file job, so this is deliberately a description rather than a location.
    pub label: String,
    /// Files whose catalogue rows were updated, or 0 when the item failed.
    pub files_updated: usize,
    /// Files the last-copy guard protected. A skip is not an error: it is the guard doing its job,
    /// and the reviewer still needs to see the number.
    pub skipped: usize,
    pub dest: Option<String>,
    /// Present exactly when the item failed. A refusal — drive swapped, tree no longer all active —
    /// is reported here rather than swallowed, because the user needs to know their click did not
    /// take effect.
    pub error_message: Option<String>,
}

#[derive(Clone, Serialize)]
pub struct QuarantineJobDto {
    pub kind: String,
    pub volume_id: String,
    pub label: String,
}

#[derive(Serialize)]
pub struct QuarantineStatus {
    pub running: Option<QuarantineJobDto>,
    pub pending: Vec<QuarantineJobDto>,
    pub recent: Vec<QuarantineResult>,
}

enum Job {
    Tree { volume_id: String, path: String },
    Files { volume_id: String, ids: Vec<i64> },
}

impl Job {
    fn volume_id(&self) -> &str {
        match self {
            Job::Tree { volume_id, .. } | Job::Files { volume_id, .. } => volume_id,
        }
    }

    fn kind(&self) -> &'static str {
        match self {
            Job::Tree { .. } => "tree",
            Job::Files { .. } => "files",
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
}

/// The shape both engines collapse to once they succeed.
struct Done {
    files_updated: usize,
    skipped: usize,
    dest: Option<String>,
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
                Job::Files { .. } => false,
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
                    }
                }
                Work::Files(ids) => {
                    let out = crate::quarantine::quarantine_files(&cat, &mount, &vid, &ids, now)?;
                    Done {
                        files_updated: out.quarantined,
                        skipped: out.skipped,
                        dest: None,
                    }
                }
            })
        })
        .await;

        let result = match joined {
            Ok(Ok(d)) => QuarantineResult {
                kind,
                volume_id: volume_id.clone(),
                label,
                files_updated: d.files_updated,
                skipped: d.skipped,
                dest: d.dest,
                error_message: None,
            },
            Ok(Err(e)) => QuarantineResult {
                kind,
                volume_id: volume_id.clone(),
                label,
                files_updated: 0,
                skipped: 0,
                dest: None,
                error_message: Some(e.to_string()),
            },
            Err(e) => QuarantineResult {
                kind,
                volume_id: volume_id.clone(),
                label,
                files_updated: 0,
                skipped: 0,
                dest: None,
                error_message: Some(format!("quarantine task failed: {e}")),
            },
        };

        let drained = {
            let mut inner = self.inner.lock().unwrap();
            inner.running = None;
            inner.recent.push_front(result);
            while inner.recent.len() > RECENT_CAP {
                inner.recent.pop_back();
            }
            inner.pending.is_empty()
        };

        // Rebuild ONCE, when there is nothing left to do. Rebuilding per item meant reprocessing
        // every row in the catalogue for each folder moved; the review list is stale in exactly the
        // same way after one item or twenty, so the work only needs doing when the user is about to
        // look again. Task 3 widens this to every volume the queue touched.
        if drained {
            let catalog_path = self.catalog_path.clone();
            let vid = volume_id.clone();
            let _ = tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
                let cat = crate::catalog::Catalog::open(&catalog_path)?;
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)?
                    .as_secs() as i64;
                cat.rebuild_directory_trees(&vid, now)?;
                cat.refresh_volume_totals(&vid)?;
                Ok(())
            })
            .await;
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
