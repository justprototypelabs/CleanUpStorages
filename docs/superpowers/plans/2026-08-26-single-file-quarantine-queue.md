# Single-File Quarantine Queue Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make confirming a single duplicate in the review UI return immediately, by routing it through the serial background queue that already exists for folder quarantines.

**Architecture:** `quarantine_queue::Job` becomes an enum with two variants — `Tree` (existing, calls `tree_quarantine::quarantine_tree`) and `Files` (new, calls `quarantine::quarantine_files`). One worker drains both kinds serially, because serial execution is what makes each job's pre-move `active` re-check meaningful. `POST /api/quarantine` stops doing the work and returns an acknowledgement instead.

**Tech Stack:** Rust 1.82, `axum` 0.7, `tokio` (rt-multi-thread), `rusqlite` 0.31 (bundled SQLite), `serde`. Tests are `#[tokio::test]` + `tower::ServiceExt::oneshot`, `tempfile` for fixtures.

**Spec:** [docs/superpowers/specs/2026-08-26-single-file-quarantine-queue-design.md](../specs/2026-08-26-single-file-quarantine-queue-design.md)

## Global Constraints

- **Nothing may ever be lost or corrupted.** Quarantine is a rename into the same drive's `_ToDelete`. This plan changes *who waits*, never *what is verified*. Do not touch the body of `quarantine::quarantine_files` or `tree_quarantine::quarantine_tree`.
- **Serial worker, one at a time.** SQLite has a single writer and every job re-checks `active` immediately before moving. Never add a second queue or parallelise the worker.
- Run `cargo test` (whole suite) before every commit. Run `cargo fmt` and `cargo clippy -- -D warnings` before every commit.
- Conventional Commits, scope `dedup` or `review`. Branch: `feat/single-file-quarantine-queue`.
- Every new `pub` item gets a doc comment saying *why*, matching the density already in `src/quarantine_queue.rs`.
- No new dependencies.

---

### Task 1: Queue accepts a second job kind

**Files:**
- Modify: `src/quarantine_queue.rs` (types, `enqueue`, `status`; tests at the bottom)

**Interfaces:**
- Consumes: nothing (first task).
- Produces:
  - `pub fn enqueue_tree(self: &Arc<Self>, volume_id: String, path: String) -> usize` — renamed from `enqueue`.
  - `pub fn enqueue_files(self: &Arc<Self>, volume_id: String, ids: Vec<i64>) -> Option<usize>` — `None` when every id was already queued.
  - `QuarantineJobDto { kind: String, volume_id: String, label: String }`
  - `QuarantineResult { kind: String, volume_id: String, label: String, files_updated: usize, skipped: usize, dest: Option<String>, error_message: Option<String> }`

- [ ] **Step 1: Write the failing tests**

Add to the `mod tests` block at the bottom of `src/quarantine_queue.rs`. Also update the three existing tests in that module to call `enqueue_tree` instead of `enqueue`, and to read `j.label` instead of `j.path`.

```rust
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
        assert_eq!(s.pending[1].label, "1 file", "only id 3 survived the filter");
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
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib quarantine_queue`
Expected: FAIL — `no method named 'enqueue_tree'`, `no method named 'enqueue_files'`, `no field 'kind'`, `no field 'label'`.

- [ ] **Step 3: Change the types**

In `src/quarantine_queue.rs`, replace the `QuarantineResult`, `QuarantineJobDto`, `Job` and `Inner` definitions:

```rust
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
                format!("{} file{}", ids.len(), if ids.len() == 1 { "" } else { "s" })
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
```

`Inner` is unchanged in shape for this task (`pending`, `running`, `recent`).

- [ ] **Step 4: Replace `enqueue` with the two typed entry points**

Delete the existing `enqueue` method and put these in its place:

```rust
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
                Job::Tree { volume_id: v, path: p } => v == &volume_id && p == &path,
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
            inner.pending.push_back(Job::Files { volume_id, ids: fresh });
            inner.pending.len() - 1 + inner.running.is_some() as usize
        };
        self.notify.notify_one();
        Some(pos)
    }
```

Add this helper to `Inner` so both entry points walk running-then-pending the same way:

```rust
impl Inner {
    /// Every job the queue still owns: the running one first, then those waiting.
    fn jobs(&self) -> impl Iterator<Item = &Job> {
        self.running.iter().chain(self.pending.iter())
    }
}
```

- [ ] **Step 5: Update `status` to emit the DTOs**

```rust
    pub fn status(&self) -> QuarantineStatus {
        let inner = self.inner.lock().unwrap();
        QuarantineStatus {
            running: inner.running.as_ref().map(Job::dto),
            pending: inner.pending.iter().map(Job::dto).collect(),
            recent: inner.recent.iter().cloned().collect(),
        }
    }
```

- [ ] **Step 6: Make `run_job` compile against the enum**

`run_job` is rewritten properly in Task 2. For now, keep it building by destructuring only the `Tree` case and leaving `Files` unimplemented — the worker gains its real `Files` arm in the next task:

```rust
    async fn run_job(self: &Arc<Self>, job: Job) {
        let (volume_id, kind, label) =
            (job.volume_id().to_string(), job.kind().to_string(), job.label());
        {
            let mut inner = self.inner.lock().unwrap();
            inner.running = Some(job);
        }

        let outcome: anyhow::Result<(usize, usize, Option<String>)> = match {
            let inner = self.inner.lock().unwrap();
            match inner.running.as_ref().unwrap() {
                Job::Tree { path, .. } => Some(path.clone()),
                Job::Files { .. } => None,
            }
        } {
            Some(path) => {
                let mount = self.mounts.resolve(&volume_id);
                let catalog_path = self.catalog_path.clone();
                let vid = volume_id.clone();
                tokio::task::spawn_blocking(move || -> anyhow::Result<(usize, usize, Option<String>)> {
                    let mount = mount.ok_or_else(|| anyhow::anyhow!("drive not connected"))?;
                    let cat = crate::catalog::Catalog::open(&catalog_path)?;
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)?
                        .as_secs() as i64;
                    let out = crate::tree_quarantine::quarantine_tree(&cat, &mount, &vid, &path, now)?;
                    Ok((out.files_updated, 0, Some(out.dest_relative_path)))
                })
                .await
                .unwrap_or_else(|e| Err(anyhow::anyhow!("quarantine task failed: {e}")))
            }
            None => Err(anyhow::anyhow!("files jobs land in Task 2")),
        };

        let result = match outcome {
            Ok((files_updated, skipped, dest)) => QuarantineResult {
                kind,
                volume_id: volume_id.clone(),
                label,
                files_updated,
                skipped,
                dest,
                error_message: None,
            },
            Err(e) => QuarantineResult {
                kind,
                volume_id: volume_id.clone(),
                label,
                files_updated: 0,
                skipped: 0,
                dest: None,
                error_message: Some(e.to_string()),
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
```

- [ ] **Step 7: Update the one production caller**

In `src/web.rs`, `api_quarantine_tree` calls `.enqueue(...)`. Change it to `.enqueue_tree(...)`. Nothing else about that handler changes.

- [ ] **Step 8: Run the tests**

Run: `cargo test --lib quarantine_queue`
Expected: PASS (all seven tests in the module).

Run: `cargo test`
Expected: PASS. The web tests still exercise the synchronous `/api/quarantine`, which is untouched so far.

- [ ] **Step 9: Commit**

```bash
cargo fmt && cargo clippy -- -D warnings && cargo test
git add src/quarantine_queue.rs src/web.rs
git commit -m "refactor(dedup): give the quarantine queue a second job kind

Job becomes an enum so single-file quarantines can share the folder
queue rather than getting a second one. Serial execution is what makes
each job's pre-move active check mean anything, so one worker must own
both kinds.

The worker still only executes tree jobs; files jobs are wired up next."
```

---

### Task 2: The worker executes single-file jobs

**Files:**
- Modify: `src/quarantine_queue.rs` (`run_job`, tests)

**Interfaces:**
- Consumes: `Job::Files { volume_id, ids }`, `enqueue_files`, `QuarantineResult { kind, label, files_updated, skipped, error_message }` from Task 1.
- Produces: a worker that calls `crate::quarantine::quarantine_files(&cat, &mount, &vid, &ids, now) -> anyhow::Result<QuarantineOutcome { quarantined, skipped }>` and records both numbers.

- [ ] **Step 1: Write the failing tests**

The existing test module has no fixture with a real drive. Add one, then three tests. Put all of this in `mod tests` in `src/quarantine_queue.rs`:

```rust
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
            r.error_message.as_deref().unwrap_or("").contains("not connected"),
            "got {:?}",
            r.error_message
        );
    }
```

Add this test-only accessor next to `QuarantineQueue`'s other methods, so the tests can open the same catalogue the queue writes to:

```rust
    /// Test-only: the catalogue this queue writes to. Tests need to read back what the worker did.
    #[cfg(test)]
    pub(crate) fn catalog_path_for_test(&self) -> &std::path::Path {
        &self.catalog_path
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib quarantine_queue`
Expected: FAIL — the three new `#[tokio::test]`s fail with `error_message: Some("files jobs land in Task 2")`.

- [ ] **Step 3: Rewrite `run_job` so each variant runs its own engine**

Replace the whole of `run_job` from Task 1 with this. The awkward re-lock from Task 1 disappears: match on the job *before* storing it as running.

```rust
    async fn run_job(self: &Arc<Self>, job: Job) {
        let (volume_id, kind, label) =
            (job.volume_id().to_string(), job.kind().to_string(), job.label());
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
                    let out = crate::tree_quarantine::quarantine_tree(&cat, &mount, &vid, &path, now)?;
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
```

Add the two small carrier types above `impl QuarantineQueue`:

```rust
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
```

- [ ] **Step 4: Run the tests**

Run: `cargo test --lib quarantine_queue`
Expected: PASS (all ten tests).

Run: `cargo test`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
cargo fmt && cargo clippy -- -D warnings && cargo test
git add src/quarantine_queue.rs
git commit -m "feat(dedup): run single-file quarantines on the shared worker

Files jobs now call quarantine::quarantine_files, and a guarded skip is
reported as a number rather than an error - the guard doing its job is
not a failure, but the reviewer still has to see it."
```

---

### Task 3: Drain rebuilds every volume the queue touched, and snapshots

**Files:**
- Modify: `src/quarantine_queue.rs` (`Inner`, `run_job` drain block, tests)

**Interfaces:**
- Consumes: `Done`, `Work`, `run_job` from Task 2.
- Produces: no new public API. `Inner` gains `touched: std::collections::HashSet<String>`.

- [ ] **Step 1: Write the failing tests**

```rust
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
        for _ in 0..400 {
            let s = q.status();
            if s.running.is_none() && s.pending.is_empty() && s.recent.len() == 2 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }

        // vol-1 ran FIRST, so it is the one today's code forgets to rebuild.
        let cat = crate::catalog::Catalog::open_readonly(q.catalog_path_for_test()).unwrap();
        let stale = cat
            .conn
            .query_row(
                "SELECT COUNT(*) FROM directory_trees WHERE volume_id='vol-1' AND path='copy'",
                [],
                |r| r.get::<_, i64>(0),
            )
            .unwrap();
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
            let backups = q.catalog_path_for_test().parent().unwrap().join("catalog.backups");
            if std::fs::read_dir(&backups).map(|d| d.count()).unwrap_or(0) > 0 {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        panic!("draining the queue must snapshot the catalogue it mutated");
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib quarantine_queue`
Expected: FAIL — `a_queue_spanning_two_drives_rebuilds_both_indexes` finds a stale `directory_trees` row for `vol-1`; `draining_writes_a_catalogue_snapshot` panics with its message.

- [ ] **Step 3: Track touched volumes on `Inner`**

```rust
struct Inner {
    pending: VecDeque<Job>,
    running: Option<Job>,
    recent: VecDeque<QuarantineResult>,
    /// Volumes mutated since the last drain. The rebuild at the end of a run has to cover all of
    /// them: a review session that quarantines on both drives would otherwise leave the first
    /// drive's directory index describing folders that have already moved.
    touched: std::collections::HashSet<String>,
}
```

Initialise it in `QuarantineQueue::new`:

```rust
            inner: Mutex::new(Inner {
                pending: VecDeque::new(),
                running: None,
                recent: VecDeque::new(),
                touched: std::collections::HashSet::new(),
            }),
```

- [ ] **Step 4: Rebuild every touched volume, then snapshot**

Replace the drain block at the end of `run_job`:

```rust
        let (drained, touched) = {
            let mut inner = self.inner.lock().unwrap();
            inner.running = None;
            inner.touched.insert(volume_id.clone());
            inner.recent.push_front(result);
            while inner.recent.len() > RECENT_CAP {
                inner.recent.pop_back();
            }
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
            let _ = tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
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
        }
```

- [ ] **Step 5: Run the tests**

Run: `cargo test --lib quarantine_queue`
Expected: PASS (all twelve tests).

Run: `cargo test`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
cargo fmt && cargo clippy -- -D warnings && cargo test
git add src/quarantine_queue.rs
git commit -m "fix(dedup): rebuild every volume the queue touched, and snapshot on drain

The drain rebuilt the last job's volume only, so a session spanning both
drives left the first drive's directory index describing folders that
had already moved. Files jobs enqueue per volume, which turns that from
rare into routine.

Snapshotting moves here with the work: folder quarantines never
snapshotted at all, and now do."
```

---

### Task 4: `POST /api/quarantine` enqueues instead of doing the work

**Files:**
- Modify: `src/web.rs` — `QuarantineResultDto` and `api_quarantine` (around line 912-980); tests `quarantine_moves_the_chosen_copy` (~2688), `quarantine_reports_unmounted_volume_without_error` (~2745), and the snapshot test (~4090)

**Interfaces:**
- Consumes: `enqueue_files(volume_id, ids) -> Option<usize>` from Task 1.
- Produces: `POST /api/quarantine` → `{ "queued": <usize>, "position": <usize>, "skipped": <usize>, "unmounted_volumes": [<String>] }`.

- [ ] **Step 1: Write the failing tests**

Replace `quarantine_moves_the_chosen_copy` with the queued equivalent, and add a new acknowledgement test. Keep `quarantine_requires_csrf_token` exactly as it is.

```rust
    #[tokio::test]
    async fn quarantine_is_queued_and_the_worker_completes_it() {
        // The POST now ENQUEUES rather than doing the work, so the reviewer can confirm the next
        // group immediately instead of waiting out a re-hash (#66 for folders, this for files).
        let (_t, db, state) = seed_dupes();
        let drive = match &state.mounts {
            crate::mounts::MountResolver::Fixed(m) => m["vol-1"].clone(),
            _ => unreachable!(),
        };
        std::fs::create_dir_all(drive.join("copy")).unwrap();
        std::fs::write(drive.join("a.jpg"), b"DUP").unwrap();
        std::fs::write(drive.join("copy/a.jpg"), b"DUP").unwrap();
        let victim = {
            let cat = crate::catalog::Catalog::open_readonly(&db).unwrap();
            cat.loose_file_id("vol-1", "copy/a.jpg").unwrap().unwrap()
        };
        tokio::spawn(state.quarantine_queue.clone().run_worker());

        let (status, json) = post_json(
            state.clone(),
            "/api/quarantine",
            Some("T"),
            serde_json::json!({ "quarantine_ids": [victim] }),
        )
        .await;
        assert_eq!(status, axum::http::StatusCode::OK);
        assert_eq!(json["queued"], 1, "one id accepted");
        assert_eq!(json["position"], 0, "nothing ahead of it");

        for _ in 0..400 {
            if drive.join("_ToDelete/copy/a.jpg").is_file() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        assert!(drive.join("_ToDelete/copy/a.jpg").is_file(), "the queued move must happen");
        assert!(!drive.join("copy/a.jpg").exists());
        assert!(drive.join("a.jpg").exists(), "survivor stays");
    }

    #[tokio::test]
    async fn quarantine_reports_an_unmounted_volume_without_queueing_it() {
        // Whether a drive is plugged in IS knowable at request time, so the reviewer learns it from
        // the response rather than from a poll thirty seconds later.
        let (_t, db, state) = seed_dupes();
        {
            let cat = crate::catalog::Catalog::open(&db).unwrap();
            cat.upsert_volume(&crate::catalog::models::Volume {
                volume_id: "vol-2".into(),
                label: "Unplugged".into(),
                identified_by: "marker".into(),
                first_seen_at: 1,
                last_seen_at: 1,
            })
            .unwrap();
            cat.upsert_file(
                &crate::catalog::models::NewFile {
                    volume_id: "vol-2".into(),
                    relative_path: "x.jpg".into(),
                    filename: "x.jpg".into(),
                    extension: "jpg".into(),
                    size_bytes: 3,
                    content_hash: "DUPHASH".into(),
                    created_time: None,
                    modified_time: None,
                    accessed_time: None,
                    category: crate::category::Category::Image,
                    container_chain: None,
                },
                100,
            )
            .unwrap();
        }
        let id = {
            let cat = crate::catalog::Catalog::open_readonly(&db).unwrap();
            cat.loose_file_id("vol-2", "x.jpg").unwrap().unwrap()
        };
        let (status, json) = post_json(
            state,
            "/api/quarantine",
            Some("T"),
            serde_json::json!({ "quarantine_ids": [id] }),
        )
        .await;
        assert_eq!(status, axum::http::StatusCode::OK);
        assert_eq!(json["queued"], 0, "nothing could be queued");
        assert!(json["unmounted_volumes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v == "vol-2"));
    }

    #[tokio::test]
    async fn quarantine_ids_that_do_not_resolve_are_reported_skipped() {
        let (_t, _db, state) = seed_dupes();
        let (status, json) = post_json(
            state,
            "/api/quarantine",
            Some("T"),
            serde_json::json!({ "quarantine_ids": [999_999] }),
        )
        .await;
        assert_eq!(status, axum::http::StatusCode::OK);
        assert_eq!(json["skipped"], 1);
        assert_eq!(json["queued"], 0);
    }
```

For the snapshot test around line 4090 (`the snapshot belongs beside the catalogue being mutated`): the request no longer mutates, so the snapshot now happens on drain. Spawn the worker before the request and poll the backups directory:

```rust
        tokio::spawn(state.quarantine_queue.clone().run_worker());
        // ... existing request ...
        assert_eq!(res.status(), axum::http::StatusCode::OK);

        let own = db.parent().unwrap().join("catalog.backups");
        let mut n_own = 0;
        for _ in 0..400 {
            n_own = std::fs::read_dir(&own).map(|d| d.count()).unwrap_or(0);
            if n_own > 0 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        assert_eq!(
            n_own, 1,
            "the snapshot belongs beside the catalogue being mutated"
        );
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib web::tests::quarantine`
Expected: FAIL — `json["queued"]` is null because the handler still returns `{quarantined, skipped, ...}`.

- [ ] **Step 3: Replace the response type and the handler**

In `src/web.rs`, replace `QuarantineResultDto` and the body of `api_quarantine`:

```rust
#[derive(Serialize, Default)]
struct QuarantineQueuedFilesDto {
    /// How many ids were actually handed to the worker.
    queued: usize,
    /// Position of the last job enqueued; 0 means it starts next.
    position: usize,
    /// Ids that resolve to nothing, or that were already queued. Not an error.
    skipped: usize,
    /// Volumes that are not currently mounted. Knowable now, so it is answered now rather than
    /// leaving the reviewer to discover it from a status poll.
    unmounted_volumes: Vec<String>,
}

/// Enqueue single-file quarantines and return immediately.
///
/// Returns an acknowledgement rather than an outcome. The work has not happened when this response
/// is written, so reporting `quarantined`/`skipped` counts here would be a lie — those numbers
/// arrive through `/api/quarantine/status` once the worker has run.
///
/// All destructive safety (marker check, disk-aware last-copy guard, rename-only) lives in
/// `quarantine::quarantine_files` and runs inside the worker, immediately before it acts — which is
/// the only point at which those checks are not already stale.
async fn api_quarantine(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Json<QuarantineReq>,
) -> Result<Json<QuarantineQueuedFilesDto>, (StatusCode, String)> {
    check_csrf(&headers, &state)?;

    let cat = Catalog::open(&state.catalog_path).map_err(err500)?;

    // Group requested ids by their volume; ids that don't resolve to a file are counted skipped.
    let mut by_volume: std::collections::HashMap<String, Vec<i64>> =
        std::collections::HashMap::new();
    let mut out = QuarantineQueuedFilesDto::default();
    for id in &body.quarantine_ids {
        match cat.get_file(*id).map_err(err500)? {
            Some(rec) => by_volume.entry(rec.volume_id).or_default().push(*id),
            None => out.skipped += 1,
        }
    }

    let mounts = state.mounts.snapshot();
    for (volume_id, ids) in by_volume {
        if !mounts.contains_key(&volume_id) {
            out.skipped += ids.len();
            out.unmounted_volumes.push(volume_id);
            continue;
        }
        let n = ids.len();
        match state.quarantine_queue.enqueue_files(volume_id, ids) {
            Some(position) => {
                out.queued += n;
                out.position = position;
            }
            // Every id was already queued. Not a failure, and not something to report as an error:
            // the reviewer double-clicked, and the decision is already on its way.
            None => out.skipped += n,
        }
    }

    Ok(Json(out))
}
```

Delete the now-unused `snapshot_best_effort(&state, now)` call and the `let now = now_secs()?;` line from this handler. `snapshot_best_effort` itself stays — other handlers still use it. Verify with `grep -n "snapshot_best_effort" src/web.rs` that at least one other caller remains; if none does, delete the function too.

- [ ] **Step 4: Run the tests**

Run: `cargo test --lib web::tests::quarantine`
Expected: PASS.

Run: `cargo test`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
cargo fmt && cargo clippy -- -D warnings && cargo test
git add src/web.rs
git commit -m "feat(review): POST /api/quarantine enqueues instead of blocking

Breaking change to the response shape, taken deliberately: the old body
described work that has not happened yet. Unmounted drives are still
answered synchronously, because that much IS knowable at request time."
```

---

### Task 5: Review page confirms without waiting

**Files:**
- Modify: `src/web_ui.rs` — `#confirm` handler (~1005-1016), `pollQuarantine` (~1149-1175), Console verb (~1681)

**Interfaces:**
- Consumes: `POST /api/quarantine` → `{queued, position, skipped, unmounted_volumes}` (Task 4); `GET /api/quarantine/status` → `{running, pending, recent}` where job DTOs carry `kind` and `label` (Tasks 1-3).
- Produces: no interface others depend on.

- [ ] **Step 1: Write the failing test**

In `src/web.rs`'s test module, next to `review_page_is_self_contained_and_has_token`:

```rust
    #[tokio::test]
    async fn review_page_confirms_without_promising_a_wait() {
        // The old copy said "Verifying content, then quarantining… (large files take a while)" —
        // true when the request blocked, a lie now that it queues.
        let (_t, _db, state) = seed_dupes();
        let body = get_text(state, "/review").await;
        assert!(
            !body.contains("Verifying content"),
            "the blocking wording must be gone"
        );
        assert!(body.contains("Queued"), "the reviewer is told it was queued");
        assert!(
            body.contains("st.running.label"),
            "the poller reads label, not the removed path field"
        );
    }
```

If no `get_text` helper exists in the test module, add one modelled on `post_json`:

```rust
    async fn get_text(state: AppState, uri: &str) -> String {
        use axum::body::Body;
        use axum::http::Request;
        use tower::ServiceExt;
        let app = build_router_with(state);
        let res = app
            .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap();
        let bytes = axum::body::to_bytes(res.into_body(), 10_000_000).await.unwrap();
        String::from_utf8(bytes.to_vec()).unwrap()
    }
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test --lib review_page_confirms_without_promising_a_wait`
Expected: FAIL — the page still contains "Verifying content".

- [ ] **Step 3: Rewrite the confirm handler**

Replace the `#confirm` click handler in `src/web_ui.rs`. Note the `\n` escapes: a real newline inside these JS string literals is a syntax error that kills the whole page script.

```javascript
$("#confirm").addEventListener("click",async()=>{
  const g=groups[idx]; if(!g)return;
  const victims=g.members.filter(m=>m.id!==keepId&&m.is_loose).map(m=>m.id);
  if(victims.length===0){ $("#msg").textContent="Nothing to quarantine (the other copies are inside archives)."; return; }
  // The request only ENQUEUES, so the reviewer moves to the next group straight away instead of
  // waiting out a re-hash. Failures surface through the status poll, not from this call.
  try{
    const j=await apiPost("/api/quarantine",{quarantine_ids:victims});
    let m="Queued "+j.queued+" file"+(j.queued===1?"":"s")+".";
    if(j.skipped) m+=" "+j.skipped+" skipped.";
    if(j.unmounted_volumes&&j.unmounted_volumes.length) m+=" Some drives not connected.";
    $("#msg").textContent=m;
    idx++; render();
    pollQuarantine();
  }catch(e){ $("#msg").textContent="Could not queue: "+e; }
});
```

- [ ] **Step 4: Teach the poller both job kinds**

Replace the body of the `tick` function inside `pollQuarantine` in `src/web_ui.rs`:

```javascript
    let st; try{ st=await apiGet("/api/quarantine/status"); }catch(e){ return; }
    const busy=!!st.running || st.pending.length>0;
    const bar=$("#qstatus");
    if(busy){
      const now=st.running?st.running.label:"";
      bar.textContent="Quarantining "+now+(st.pending.length?" — "+st.pending.length+" queued":"");
      bar.style.display="";
    }else{ bar.style.display="none"; }
    // Worker failures have to reach the user: they clicked, and it did not happen. A tree that is
    // no longer wholly active, or a drive swapped mid-queue, is refused rather than forced.
    const failed=st.recent.filter(r=>r.error_message);
    if(failed.length){
      $("#msg").textContent=failed.length+" could not be quarantined — "+failed[0].error_message;
    }else if(st.recent.length){
      const folders=st.recent.filter(r=>!r.error_message&&r.kind==="tree").length;
      const files=st.recent.filter(r=>!r.error_message&&r.kind==="files")
                           .reduce((a,r)=>a+r.files_updated,0);
      // A guarded skip is not a failure, but hiding it would let the reviewer believe a copy was
      // handled when the last-copy guard deliberately kept it.
      const skipped=st.recent.reduce((a,r)=>a+(r.skipped||0),0);
      const parts=[];
      if(folders) parts.push(folders+" folder"+(folders===1?"":"s"));
      if(files) parts.push(files+" file"+(files===1?"":"s"));
      let m=parts.length?parts.join(" and ")+" moved to _ToDelete.":"";
      if(skipped) m+=" "+skipped+" kept by the last-copy guard.";
      if(m) $("#msg").textContent=m;
    }
    if(qWasBusy && !busy){ clearInterval(qTimer); qTimer=null; await loadTrees(); }
    qWasBusy=busy;
```

- [ ] **Step 5: Update the Console verb**

At `src/web_ui.rs` line ~1681, the Console prints the raw response. Make it say what happened:

```javascript
      const r=await apiPost("/api/quarantine",{quarantine_ids:ids});
      printJSON(r); print("Queued — the worker runs these in order; watch /api/quarantine/status."); return; }
```

- [ ] **Step 6: Run the tests**

Run: `cargo test`
Expected: PASS, including `review_page_confirms_without_promising_a_wait`.

- [ ] **Step 7: Verify by hand**

```bash
cargo build --release
./target/release/cleanupstorages browse
```

On the Duplicates page: confirm a group and check that the next group appears immediately, the status bar names the running item, and the summary reports files moved. Confirm a group where both copies are the only two — the summary must say the last-copy guard kept one.

- [ ] **Step 8: Commit**

```bash
cargo fmt && cargo clippy -- -D warnings && cargo test
git add src/web_ui.rs src/web.rs
git commit -m "feat(review): confirm a duplicate without waiting for the move

Advancing to the next group no longer waits on a re-hash. The status bar
reports the running item and the last-copy guard's skips, so a kept copy
is never mistaken for a handled one."
```

---

## Self-Review

**Spec coverage:**

| Spec requirement | Task |
| --- | --- |
| One worker, two job kinds | 1, 2 |
| `Job::Tree` / `Job::Files` enum | 1 |
| One `Files` job per volume | 4 |
| `POST /api/quarantine` becomes an acknowledgement | 4 |
| Stale decisions refused by the guard, surfaced by the poller | 2 (test), 5 (rendering) |
| Rebuild every volume touched since the last drain | 3 |
| De-duplicating enqueues (`Tree` by path, `Files` by id) | 1 |
| Verification unchanged | Global constraint; no task edits the engines |
| `kind` / `label` / `skipped` on both DTOs | 1 |
| Unmounted volumes answered at request time | 4 |
| UI reuses the existing poller | 5 |
| Console verb updated | 5 |
| Testing items 1-7 in the spec | 1 (3, 4), 2 (1, 2, 6), 3 (5), 4 (7) |

**Type consistency:** `enqueue_tree`/`enqueue_files` are named identically in Tasks 1, 3, 4. `QuarantineResult` fields (`kind`, `volume_id`, `label`, `files_updated`, `skipped`, `dest`, `error_message`) are used consistently in Tasks 1-5. `Work` and `Done` are introduced in Task 2 and not referenced earlier. `catalog_path_for_test` is introduced in Task 2 and reused in Task 3.

**Known deviation:** Task 1 Step 6 leaves `run_job` in a deliberately awkward intermediate state (a re-lock to read the running job, and a `Files` arm that errors). It exists only so Task 1 compiles and commits on its own; Task 2 Step 3 replaces the whole function. A reviewer should not accept Task 1 as final shape.
