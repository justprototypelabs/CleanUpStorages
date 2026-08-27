# Duplicate cluster review — implementation plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Turn 13,783 remaining duplicate-group decisions into ~4,017 by clustering groups on the
*set of directories* their copies occupy, and letting the user rank those directories once per
cluster.

**Architecture:** One new catalog module, `src/catalog/clusters.rs`, holding both the derived query
(`Catalog::duplicate_clusters`) and the preference resolution (`Catalog::cluster_victims`). Nothing
is stored: like `duplicate_groups_ranked`, a cluster is a query over `files`. The web layer stays
thin — `GET /api/duplicate-clusters` renders, `POST /api/quarantine-cluster` resolves victims and
hands them to the existing serial `QuarantineQueue` as a `Job::Files`. Per-file execution, and
therefore the disk-aware last-copy guard, is untouched.

**Tech Stack:** Rust, rusqlite/SQLite, blake3 (cluster identity), axum (web), plain JS in
`src/web_ui.rs` (no build step).

**Spec:** [`docs/superpowers/specs/2026-08-26-duplicate-cluster-review-design.md`](../specs/2026-08-26-duplicate-cluster-review-design.md)
(currently on branch `docs/dedup-finish-specs`; Task 7 lands it on this branch)

**Issue:** #78, child of epic #75.

## Global Constraints

- **Nothing may ever be lost or corrupted.** A cluster decision never bypasses a per-file check:
  the confirm only enqueues ids, and `quarantine_queue`'s worker still runs the disk-aware
  last-copy guard on every single file.
- **Archived copies are never quarantined.** Only rows with `container_chain IS NULL` may become
  victims. Archived copies are counted and reported, never acted on.
- **A directory the scanner could not read is never elected keeper.** "Could not read" means a
  `scan_errors` row with `IFNULL(phase,'') = 'walk'` for that `(volume_id, path)`, or for any
  ancestor of it.
- **Nothing derived is stored.** No new table, no new column, no migration. A cluster is recomputed
  on every request; a stale cluster id is refused, never reapplied to a recomputed membership.
- **Ordering is by reclaimable bytes, descending.** Never by group count — the spec measured that
  count-ordering spends ~1,800 decisions to recover ~0.1 GiB.
- **Pairwise clustering is not to be reintroduced.** The cluster key is the *set* of directories a
  group occupies (13,783 groups → 4,017 clusters); the pairwise key produced 133,490.
- The keep rule for files inside one directory is `crate::catalog::dedup::KEEP_ORDER`'s spirit —
  but see Task 3: within a single directory the tie-break is **`relative_path` ascending, then
  `id`**, deterministically, as the spec requires.
- Rust edition/toolchain and lint settings as already configured; `cargo clippy` must stay clean.

## Deviation from the spec, deliberate

The spec's `POST /api/quarantine-cluster` body is `{ cluster_id, preference }`. This plan adds
**`min_size`** to both the GET and the POST. Reason: cluster membership is computed over duplicate
groups at or above the review floor. If the confirm recomputed membership floor-free, a cluster the
user saw as "12 groups, 5 GiB" could quietly quarantine dozens of extra sub-floor groups in the same
directories — a blast radius larger than the one displayed, which the reliability constraint forbids.
The client sends back the floor it rendered with. Task 7 records this in the spec.

## File Structure

| File | Responsibility |
| --- | --- |
| Create: `src/catalog/clusters.rs` | Cluster query, cluster identity, keeper eligibility, preference resolution. Unit tests live in its `#[cfg(test)] mod tests`. |
| Modify: `src/catalog/mod.rs` | Add `pub mod clusters;` |
| Modify: `src/web.rs` | Two handlers + their DTOs + two routes |
| Modify: `src/web_ui.rs` | `#duplicate clusters` section markup + its JS on the Review page |
| Modify: `tests/review_flow.rs` | End-to-end: list clusters, confirm one, assert only victims moved |
| Create: `docs/superpowers/specs/2026-08-26-duplicate-cluster-review-design.md` | Bring the spec onto this branch, mark accepted, record the `min_size` deviation |

---

### Task 1: The cluster query

**Files:**
- Create: `src/catalog/clusters.rs`
- Modify: `src/catalog/mod.rs`
- Test: `src/catalog/clusters.rs` (`#[cfg(test)] mod tests`)

**Interfaces:**
- Consumes: `crate::catalog::Catalog`, `crate::catalog::dedup::DEFAULT_MIN_SIZE`
- Produces:
  - `pub fn parent_dir(relative_path: &str) -> &str`
  - `pub struct ClusterDir { pub volume_id: String, pub dir: String }` (derives `Debug, Clone, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize`)
  - `pub struct Cluster { pub id: String, pub dirs: Vec<ClusterDir>, pub group_count: i64, pub reclaimable_bytes: i64, pub sample_names: Vec<String>, pub archived_group_count: i64, pub keepable: bool }`
  - `pub fn cluster_id(dirs: &[ClusterDir]) -> String`
  - `impl Catalog { pub fn duplicate_clusters(&self, min_size: i64, limit: usize, offset: usize) -> anyhow::Result<(Vec<Cluster>, usize)> }`

In this task `archived_group_count` is always `0` and `keepable` always `true`; Task 2 fills both in.
They are declared now so later tasks do not have to change the struct's shape.

- [ ] **Step 1: Write the failing tests**

Create `src/catalog/clusters.rs` with the test module only (the code follows in Step 3):

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::Catalog;

    /// Two folders sharing three duplicate files, one of them holding an extra file that is also
    /// duplicated elsewhere. `dirA`/`dirB` share hashes A, B, C. Hash D is shared between `dirA`
    /// and `dirC`, so it forms a SECOND cluster -- the case identical-tree collapse cannot reach.
    fn seed() -> (tempfile::TempDir, Catalog) {
        let t = tempfile::tempdir().unwrap();
        let cat = Catalog::open(&t.path().join("c.db")).unwrap();
        cat.conn
            .execute_batch(
                "INSERT INTO volumes(volume_id,label,identified_by,first_seen_at,last_seen_at)
                     VALUES ('v1','V1','marker',1,1);
                 INSERT INTO files(volume_id,relative_path,filename,extension,size_bytes,
                     content_hash,created_time,modified_time,accessed_time,category,
                     container_chain,status,first_seen_at,last_seen_at) VALUES
                 ('v1','dirA/a.txt','a.txt','txt',100,'A',100,100,NULL,'other',NULL,'active',1,1),
                 ('v1','dirB/a.txt','a.txt','txt',100,'A',200,200,NULL,'other',NULL,'active',1,1),
                 ('v1','dirA/b.txt','b.txt','txt',100,'B',100,100,NULL,'other',NULL,'active',1,1),
                 ('v1','dirB/b.txt','b.txt','txt',100,'B',200,200,NULL,'other',NULL,'active',1,1),
                 ('v1','dirA/c.txt','c.txt','txt',100,'C',100,100,NULL,'other',NULL,'active',1,1),
                 ('v1','dirB/c.txt','c.txt','txt',100,'C',200,200,NULL,'other',NULL,'active',1,1),
                 ('v1','dirA/d.txt','d.txt','txt', 10,'D',100,100,NULL,'other',NULL,'active',1,1),
                 ('v1','dirC/d.txt','d.txt','txt', 10,'D',200,200,NULL,'other',NULL,'active',1,1);",
            )
            .unwrap();
        (t, cat)
    }

    #[test]
    fn partial_overlap_folders_form_one_cluster_of_three_groups() {
        let (_t, cat) = seed();
        let (cs, total) = cat.duplicate_clusters(0, 100, 0).unwrap();
        assert_eq!(total, 2, "dirA+dirB, and dirA+dirC");
        let top = &cs[0];
        assert_eq!(top.group_count, 3, "A, B and C share one directory set");
        assert_eq!(top.reclaimable_bytes, 300);
        assert_eq!(
            top.dirs,
            vec![
                ClusterDir { volume_id: "v1".into(), dir: "dirA".into() },
                ClusterDir { volume_id: "v1".into(), dir: "dirB".into() },
            ]
        );
        assert_eq!(top.sample_names, vec!["a.txt", "b.txt", "c.txt"]);
    }

    #[test]
    fn clusters_are_ordered_by_bytes_not_by_group_count() {
        let (_t, cat) = seed();
        // Make the dirA+dirC cluster hold MORE groups but far fewer bytes: count-ordering would
        // put it first, which the spec measured as actively harmful.
        cat.conn
            .execute_batch(
                "INSERT INTO files(volume_id,relative_path,filename,extension,size_bytes,
                     content_hash,created_time,modified_time,accessed_time,category,
                     container_chain,status,first_seen_at,last_seen_at) VALUES
                 ('v1','dirA/e.txt','e.txt','txt',1,'E',100,100,NULL,'other',NULL,'active',1,1),
                 ('v1','dirC/e.txt','e.txt','txt',1,'E',200,200,NULL,'other',NULL,'active',1,1),
                 ('v1','dirA/f.txt','f.txt','txt',1,'F',100,100,NULL,'other',NULL,'active',1,1),
                 ('v1','dirC/f.txt','f.txt','txt',1,'F',200,200,NULL,'other',NULL,'active',1,1),
                 ('v1','dirA/g.txt','g.txt','txt',1,'G',100,100,NULL,'other',NULL,'active',1,1),
                 ('v1','dirC/g.txt','g.txt','txt',1,'G',200,200,NULL,'other',NULL,'active',1,1);",
            )
            .unwrap();
        let (cs, _) = cat.duplicate_clusters(0, 100, 0).unwrap();
        assert_eq!(cs[0].group_count, 3);
        assert_eq!(cs[0].reclaimable_bytes, 300, "3 x 100 B ranks first");
        assert_eq!(cs[1].group_count, 4, "4 groups, 13 B, ranks second");
        assert_eq!(cs[1].reclaimable_bytes, 13);
    }

    #[test]
    fn a_group_with_three_copies_lands_in_exactly_one_cluster() {
        let (_t, cat) = seed();
        cat.conn
            .execute(
                "INSERT INTO files(volume_id,relative_path,filename,extension,size_bytes,
                     content_hash,created_time,modified_time,accessed_time,category,
                     container_chain,status,first_seen_at,last_seen_at) VALUES
                 ('v1','dirC/a.txt','a.txt','txt',100,'A',300,300,NULL,'other',NULL,'active',1,1)",
                [],
            )
            .unwrap();
        let (cs, _) = cat.duplicate_clusters(0, 100, 0).unwrap();
        let holding_a: Vec<_> = cs.iter().filter(|c| c.dirs.len() == 3).collect();
        assert_eq!(holding_a.len(), 1, "one cluster over {A,B,C}, not three pairs");
        assert_eq!(holding_a[0].group_count, 1);
        assert_eq!(holding_a[0].reclaimable_bytes, 200, "two redundant copies");
    }

    #[test]
    fn the_floor_removes_small_clusters_from_the_list() {
        let (_t, cat) = seed();
        let (cs, total) = cat.duplicate_clusters(50, 100, 0).unwrap();
        assert_eq!(total, 1, "the 10-byte D group falls below the floor");
        assert_eq!(cs[0].group_count, 3);
    }

    #[test]
    fn paging_repeats_nothing_and_skips_nothing() {
        let (_t, cat) = seed();
        let (first, total) = cat.duplicate_clusters(0, 1, 0).unwrap();
        let (second, _) = cat.duplicate_clusters(0, 1, 1).unwrap();
        let (empty, _) = cat.duplicate_clusters(0, 1, 2).unwrap();
        assert_eq!(total, 2);
        assert_eq!(first.len(), 1);
        assert_eq!(second.len(), 1);
        assert!(empty.is_empty());
        assert_ne!(first[0].id, second[0].id);
    }

    #[test]
    fn the_id_is_the_directory_set_and_does_not_depend_on_order() {
        let a = ClusterDir { volume_id: "v1".into(), dir: "dirA".into() };
        let b = ClusterDir { volume_id: "v1".into(), dir: "dirB".into() };
        assert_eq!(cluster_id(&[a.clone(), b.clone()]), cluster_id(&[b, a]));
    }

    #[test]
    fn parent_dir_handles_both_separators_and_the_drive_root() {
        assert_eq!(parent_dir("a/b/c.txt"), "a/b");
        assert_eq!(parent_dir("a\\b\\c.txt"), "a\\b");
        assert_eq!(parent_dir("c.txt"), "");
    }

    #[test]
    fn archived_copies_never_form_a_cluster_on_their_own() {
        let t = tempfile::tempdir().unwrap();
        let cat = Catalog::open(&t.path().join("c.db")).unwrap();
        cat.conn
            .execute_batch(
                "INSERT INTO volumes(volume_id,label,identified_by,first_seen_at,last_seen_at)
                     VALUES ('v1','V1','marker',1,1);
                 INSERT INTO files(volume_id,relative_path,filename,extension,size_bytes,
                     content_hash,created_time,modified_time,accessed_time,category,
                     container_chain,status,first_seen_at,last_seen_at) VALUES
                 ('v1','x/one.zip','a.txt','txt',100,'A',100,100,NULL,'other','one.zip','active',1,1),
                 ('v1','y/two.zip','a.txt','txt',100,'A',200,200,NULL,'other','two.zip','active',1,1);",
            )
            .unwrap();
        let (cs, total) = cat.duplicate_clusters(0, 100, 0).unwrap();
        assert_eq!(total, 0, "a rename cannot act on a file inside a zip");
        assert!(cs.is_empty());
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib clusters`
Expected: FAIL to compile — `module clusters not found` / `duplicate_clusters not found`.

- [ ] **Step 3: Write the implementation**

Add to `src/catalog/mod.rs`, keeping the module list alphabetical:

```rust
pub mod clusters;
```

Prepend to `src/catalog/clusters.rs`, above the test module:

```rust
//! Duplicate groups clustered by the SET of directories their copies occupy.
//!
//! Identical-tree collapse (#38) handles folders that match completely. This handles the partial
//! overlap it cannot reach: two folders sharing thousands of duplicates where one holds extras.
//!
//! Derived, never stored -- like `duplicate_groups_ranked`, this is a query over `files`. Storing
//! it would add a second thing to invalidate on every quarantine.
//!
//! The key is the directory **set**. Clustering by directory *pairs* was measured first and is 10x
//! worse: 13,783 groups yield 133,490 distinct pairs, because a group with N copies yields
//! N-choose-2 of them. The set gives 4,017.

use crate::catalog::Catalog;
use std::collections::HashMap;

/// The directory part of a catalogued relative path.
///
/// Splits on BOTH separators: paths are stored as the scanning platform produced them, and a
/// catalogue built on Windows is read on macOS by the same binary.
pub fn parent_dir(relative_path: &str) -> &str {
    match relative_path.rfind(['/', '\\']) {
        Some(i) => &relative_path[..i],
        None => "",
    }
}

/// One directory a cluster spans. `dir` is relative to the volume root, `""` at the root itself.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
pub struct ClusterDir {
    pub volume_id: String,
    pub dir: String,
}

/// A set of directories, and every duplicate group whose copies occupy exactly that set.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Cluster {
    /// BLAKE3 over the sorted directory list. Derived, so it must be reproducible across requests:
    /// the client sends it back and the server recomputes membership from it.
    pub id: String,
    pub dirs: Vec<ClusterDir>,
    pub group_count: i64,
    pub reclaimable_bytes: i64,
    /// Up to three filenames, so course material can be told from build output at a glance.
    pub sample_names: Vec<String>,
    /// Groups in this cluster whose redundant copies are all inside archives. Counted and shown,
    /// never quarantined -- those need a repack or an extraction, not a rename.
    pub archived_group_count: i64,
    /// False when every directory in the set is one the scanner could not read, which makes the
    /// cluster unconfirmable. Filled in by `duplicate_clusters`.
    pub keepable: bool,
}

/// Cluster identity: BLAKE3 over the sorted `(volume_id, dir)` list.
///
/// Order-independent by construction, because the caller may hold the dirs in any order and the id
/// must survive a round trip through the client.
pub fn cluster_id(dirs: &[ClusterDir]) -> String {
    let mut sorted = dirs.to_vec();
    sorted.sort();
    sorted.dedup();
    let mut h = blake3::Hasher::new();
    for d in &sorted {
        h.update(d.volume_id.as_bytes());
        h.update(b"\x1f");
        h.update(d.dir.as_bytes());
        h.update(b"\x1e");
    }
    h.finalize().to_hex().to_string()
}

/// One loose copy, as the clustering pass needs it.
pub(crate) struct CopyRow {
    pub id: i64,
    pub volume_id: String,
    pub relative_path: String,
    pub filename: String,
    pub size_bytes: i64,
    pub content_hash: String,
    pub archived: bool,
}

impl Catalog {
    /// Every loose active row that belongs to a duplicate group at or above `min_size`, plus the
    /// archived rows of those same hashes so they can be counted.
    ///
    /// One pass, ordered by hash: the grouping happens in Rust because the cluster key is a SET,
    /// which SQL cannot express without a string aggregation that would then have to be parsed
    /// back. The set is bounded by the duplicate rows themselves (63,336 on the real catalogue).
    pub(crate) fn duplicate_copies(&self, min_size: i64) -> anyhow::Result<Vec<CopyRow>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT id, volume_id, relative_path, filename, size_bytes, content_hash,
                    container_chain IS NOT NULL AS archived
               FROM files
              WHERE status = 'active'
                AND content_hash IN (
                    SELECT content_hash FROM files
                     WHERE status = 'active' AND container_chain IS NULL AND size_bytes >= ?1
                     GROUP BY content_hash HAVING COUNT(*) > 1)
              ORDER BY content_hash, relative_path, id",
        )?;
        let rows = stmt.query_map(rusqlite::params![min_size], |r| {
            Ok(CopyRow {
                id: r.get(0)?,
                volume_id: r.get(1)?,
                relative_path: r.get(2)?,
                filename: r.get(3)?,
                size_bytes: r.get(4)?,
                content_hash: r.get(5)?,
                archived: r.get(6)?,
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Clusters at or above `min_size`, ranked by reclaimable bytes descending, then by id so the
    /// order is total and paging is stable. Returns the page and the full count.
    pub fn duplicate_clusters(
        &self,
        min_size: i64,
        limit: usize,
        offset: usize,
    ) -> anyhow::Result<(Vec<Cluster>, usize)> {
        let rows = self.duplicate_copies(min_size)?;
        let mut acc: HashMap<String, Cluster> = HashMap::new();

        for group in group_by_hash(&rows) {
            let loose: Vec<&CopyRow> = group.iter().copied().filter(|r| !r.archived).collect();
            if loose.len() < 2 {
                continue;
            }
            let mut dirs: Vec<ClusterDir> = loose
                .iter()
                .map(|r| ClusterDir {
                    volume_id: r.volume_id.clone(),
                    dir: parent_dir(&r.relative_path).to_string(),
                })
                .collect();
            dirs.sort();
            dirs.dedup();
            let id = cluster_id(&dirs);
            let size = loose.iter().map(|r| r.size_bytes).min().unwrap_or(0);
            let c = acc.entry(id.clone()).or_insert_with(|| Cluster {
                id,
                dirs,
                group_count: 0,
                reclaimable_bytes: 0,
                sample_names: Vec::new(),
                archived_group_count: 0,
                keepable: true,
            });
            c.group_count += 1;
            c.reclaimable_bytes += (loose.len() as i64 - 1) * size;
            if c.sample_names.len() < 3 {
                let name = loose[0].filename.clone();
                if !c.sample_names.contains(&name) {
                    c.sample_names.push(name);
                }
            }
        }

        let mut out: Vec<Cluster> = acc.into_values().collect();
        out.sort_by(|a, b| {
            b.reclaimable_bytes
                .cmp(&a.reclaimable_bytes)
                .then(a.id.cmp(&b.id))
        });
        let total = out.len();
        Ok((out.into_iter().skip(offset).take(limit).collect(), total))
    }
}

/// Split a hash-ordered row list into per-hash slices, without copying the rows.
pub(crate) fn group_by_hash(rows: &[CopyRow]) -> Vec<Vec<&CopyRow>> {
    let mut out: Vec<Vec<&CopyRow>> = Vec::new();
    for r in rows {
        match out.last_mut() {
            Some(g) if g[0].content_hash == r.content_hash => g.push(r),
            _ => out.push(vec![r]),
        }
    }
    out
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --lib clusters`
Expected: PASS (8 tests). Then `cargo clippy --all-targets -- -D warnings`.

- [ ] **Step 5: Commit**

```bash
git add src/catalog/clusters.rs src/catalog/mod.rs
git commit -m "feat(dedup): cluster duplicate groups by the set of directories they occupy"
```

---

### Task 2: Keeper eligibility and archived groups

**Files:**
- Modify: `src/catalog/clusters.rs`
- Test: `src/catalog/clusters.rs`

**Interfaces:**
- Consumes: `Cluster`, `ClusterDir`, `CopyRow`, `group_by_hash`, `parent_dir` from Task 1
- Produces:
  - `impl Catalog { pub fn unreadable_dirs(&self) -> anyhow::Result<Vec<ClusterDir>> }`
  - `pub fn is_unreadable(dir: &ClusterDir, unreadable: &[ClusterDir]) -> bool`
  - `duplicate_clusters` now fills `archived_group_count` and `keepable`

- [ ] **Step 1: Write the failing tests**

Append inside `mod tests` in `src/catalog/clusters.rs`:

```rust
    #[test]
    fn a_directory_the_walk_could_not_open_is_never_keepable() {
        let (_t, cat) = seed();
        // phase='walk' is exactly what `volume_completeness` counts as an unreadable directory.
        cat.conn
            .execute(
                "INSERT INTO scan_errors(volume_id,path,reason,occurred_at,phase,kind)
                     VALUES ('v1','dirA','denied',1,'walk','permission')",
                [],
            )
            .unwrap();
        let un = cat.unreadable_dirs().unwrap();
        assert_eq!(un, vec![ClusterDir { volume_id: "v1".into(), dir: "dirA".into() }]);
        assert!(is_unreadable(&ClusterDir { volume_id: "v1".into(), dir: "dirA".into() }, &un));
        assert!(
            is_unreadable(&ClusterDir { volume_id: "v1".into(), dir: "dirA/sub".into() }, &un),
            "a directory beneath an unopenable one is unverified too"
        );
        assert!(
            !is_unreadable(&ClusterDir { volume_id: "v1".into(), dir: "dirAB".into() }, &un),
            "prefix match must respect the separator, or dirAB inherits dirA's verdict"
        );
        assert!(
            !is_unreadable(&ClusterDir { volume_id: "v2".into(), dir: "dirA".into() }, &un),
            "a path is only unreadable on the volume that failed to walk it"
        );
        // dirB is still readable, so the cluster stays confirmable.
        let (cs, _) = cat.duplicate_clusters(0, 100, 0).unwrap();
        assert!(cs[0].keepable);
    }

    #[test]
    fn a_cluster_whose_every_directory_is_unreadable_is_not_confirmable() {
        let (_t, cat) = seed();
        cat.conn
            .execute_batch(
                "INSERT INTO scan_errors(volume_id,path,reason,occurred_at,phase,kind) VALUES
                 ('v1','dirA','denied',1,'walk','permission'),
                 ('v1','dirB','denied',1,'walk','permission');",
            )
            .unwrap();
        let (cs, _) = cat.duplicate_clusters(0, 100, 0).unwrap();
        let three = cs.iter().find(|c| c.group_count == 3).unwrap();
        assert!(
            !three.keepable,
            "keeping a copy we cannot verify while quarantining the ones we can is a trade down"
        );
    }

    #[test]
    fn groups_whose_redundant_copies_are_all_archived_are_counted_not_acted_on() {
        let (_t, cat) = seed();
        // Hash Z: one loose copy in dirA, one inside a zip. Only ONE loose copy, so the group is
        // not actionable -- but it is still part of what the user is looking at.
        cat.conn
            .execute_batch(
                "INSERT INTO files(volume_id,relative_path,filename,extension,size_bytes,
                     content_hash,created_time,modified_time,accessed_time,category,
                     container_chain,status,first_seen_at,last_seen_at) VALUES
                 ('v1','dirA/z.txt','z.txt','txt',100,'Z',100,100,NULL,'other',NULL,'active',1,1),
                 ('v1','dirB/z.txt','z.txt','txt',100,'Z',150,150,NULL,'other',NULL,'active',1,1),
                 ('v1','dirB/pack.zip','z.txt','txt',100,'Z',200,200,NULL,'other','pack.zip','active',1,1);",
            )
            .unwrap();
        let (cs, _) = cat.duplicate_clusters(0, 100, 0).unwrap();
        let top = cs.iter().find(|c| c.group_count == 4).unwrap();
        assert_eq!(
            top.archived_group_count, 1,
            "Z also lives in an archive; that copy is reported, never renamed"
        );
        assert_eq!(
            top.reclaimable_bytes, 400,
            "the archived copy contributes no reclaimable bytes"
        );
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib clusters`
Expected: FAIL — `cannot find function unreadable_dirs` / `is_unreadable`.

- [ ] **Step 3: Write the implementation**

Add to `src/catalog/clusters.rs`, after `cluster_id`:

```rust
/// Is `dir` a directory the scanner could not open, or beneath one?
///
/// Beneath counts: if the walk never opened a parent, nothing under it was catalogued, so its
/// contents are unknown for the same reason.
pub fn is_unreadable(dir: &ClusterDir, unreadable: &[ClusterDir]) -> bool {
    unreadable.iter().any(|u| {
        u.volume_id == dir.volume_id
            && (u.dir == dir.dir
                // The separator check is what stops `dirAB` inheriting `dirA`'s verdict.
                || (dir.dir.len() > u.dir.len()
                    && dir.dir.starts_with(&u.dir)
                    && matches!(dir.dir.as_bytes()[u.dir.len()], b'/' | b'\\')))
    })
}
```

And inside `impl Catalog`:

```rust
    /// Directories the walk could not open, across every volume.
    ///
    /// `phase='walk'` is the same predicate `volume_completeness` buckets as `unreadable_dir`; it
    /// is deliberately not re-derived from the message text, which is OS-localised.
    pub fn unreadable_dirs(&self) -> anyhow::Result<Vec<ClusterDir>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT IFNULL(volume_id,''), path FROM scan_errors
              WHERE IFNULL(phase,'') = 'walk' ORDER BY volume_id, path",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(ClusterDir {
                volume_id: r.get(0)?,
                dir: r.get(1)?,
            })
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }
```

In `duplicate_clusters`, load the unreadable list once before the loop:

```rust
        let rows = self.duplicate_copies(min_size)?;
        let unreadable = self.unreadable_dirs()?;
        let mut acc: HashMap<String, Cluster> = HashMap::new();
```

Inside the per-group body, after `c.group_count += 1;`, count archived groups:

```rust
            if group.iter().any(|r| r.archived) {
                c.archived_group_count += 1;
            }
```

And after the accumulation loop, before sorting:

```rust
        // A cluster is confirmable only if SOMETHING in it can be elected keeper. Electing an
        // unreadable directory would trade a survivor we verified for one we did not.
        for c in acc.values_mut() {
            c.keepable = c.dirs.iter().any(|d| !is_unreadable(d, &unreadable));
        }
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --lib clusters`
Expected: PASS (11 tests). Then `cargo clippy --all-targets -- -D warnings`.

- [ ] **Step 5: Commit**

```bash
git add src/catalog/clusters.rs
git commit -m "feat(dedup): never elect an unreadable directory as a cluster's keeper"
```

---

### Task 3: Preference resolution

**Files:**
- Modify: `src/catalog/clusters.rs`
- Test: `src/catalog/clusters.rs`

**Interfaces:**
- Consumes: everything from Tasks 1 and 2
- Produces:
  - `pub enum ClusterResolveError { NoSuchCluster, UnknownDirectory(ClusterDir), NotKeepable }` (implements `std::fmt::Display` and `std::error::Error`)
  - `pub struct ClusterPlan { pub victims: Vec<i64>, pub keepers: Vec<i64>, pub archived_skipped: i64 }`
  - `impl Catalog { pub fn cluster_victims(&self, cluster_id: &str, min_size: i64, preference: &[ClusterDir]) -> Result<ClusterPlan, ClusterResolveError> }`

`cluster_victims` returns `Result<_, ClusterResolveError>` rather than `anyhow::Result` because the
web layer must map each case to a different HTTP status; a database failure is folded into
`NoSuchCluster` only if it prevents the lookup — see the implementation, which propagates it as a
distinct variant `Db(String)`.

- [ ] **Step 1: Write the failing tests**

Append inside `mod tests`:

```rust
    fn dir(v: &str, d: &str) -> ClusterDir {
        ClusterDir { volume_id: v.into(), dir: d.into() }
    }

    /// Path -> id, so the assertions can name files instead of integers.
    fn ids(cat: &Catalog) -> std::collections::HashMap<String, i64> {
        let mut stmt = cat
            .conn
            .prepare("SELECT relative_path, id FROM files WHERE container_chain IS NULL")
            .unwrap();
        let rows = stmt
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))
            .unwrap();
        rows.map(|r| r.unwrap()).collect()
    }

    #[test]
    fn the_highest_ranked_directory_present_keeps_its_copy() {
        let (_t, cat) = seed();
        let id = ids(&cat);
        let (cs, _) = cat.duplicate_clusters(0, 100, 0).unwrap();
        let c = cs.iter().find(|c| c.group_count == 3).unwrap();
        // Rank dirB first: every group in this cluster has a copy in dirB.
        let plan = cat
            .cluster_victims(&c.id, 0, &[dir("v1", "dirB"), dir("v1", "dirA")])
            .unwrap();
        let mut keepers = plan.keepers.clone();
        keepers.sort();
        let mut want = vec![id["dirB/a.txt"], id["dirB/b.txt"], id["dirB/c.txt"]];
        want.sort();
        assert_eq!(keepers, want);
        let mut victims = plan.victims.clone();
        victims.sort();
        let mut want_v = vec![id["dirA/a.txt"], id["dirA/b.txt"], id["dirA/c.txt"]];
        want_v.sort();
        assert_eq!(victims, want_v, "confirming enqueues victims, never keepers");
    }

    #[test]
    fn a_group_missing_from_the_top_ranked_directory_falls_through_to_the_next() {
        let (_t, cat) = seed();
        let id = ids(&cat);
        // Add hash E to dirB only twice, so its group has no copy in dirA at all; and put the
        // third copy of A in dirC so this cluster spans three directories.
        cat.conn
            .execute_batch(
                "INSERT INTO files(volume_id,relative_path,filename,extension,size_bytes,
                     content_hash,created_time,modified_time,accessed_time,category,
                     container_chain,status,first_seen_at,last_seen_at) VALUES
                 ('v1','dirB/e1.txt','e1.txt','txt',100,'E',100,100,NULL,'other',NULL,'active',1,1),
                 ('v1','dirC/e2.txt','e2.txt','txt',100,'E',200,200,NULL,'other',NULL,'active',1,1);",
            )
            .unwrap();
        let id2 = ids(&cat);
        let (cs, _) = cat.duplicate_clusters(0, 100, 0).unwrap();
        let c = cs
            .iter()
            .find(|c| c.dirs == vec![dir("v1", "dirB"), dir("v1", "dirC")])
            .unwrap();
        // dirA is ranked first but is not in this cluster at all -- rejected as unknown.
        let err = cat
            .cluster_victims(&c.id, 0, &[dir("v1", "dirA")])
            .unwrap_err();
        assert!(matches!(err, ClusterResolveError::UnknownDirectory(_)));
        // Ranking dirC first keeps the dirC copy; dirB is quarantined.
        let plan = cat
            .cluster_victims(&c.id, 0, &[dir("v1", "dirC"), dir("v1", "dirB")])
            .unwrap();
        assert_eq!(plan.keepers, vec![id2["dirC/e2.txt"]]);
        assert_eq!(plan.victims, vec![id2["dirB/e1.txt"]]);
        let _ = id;
    }

    #[test]
    fn two_copies_in_one_directory_keep_the_first_by_path() {
        let t = tempfile::tempdir().unwrap();
        let cat = Catalog::open(&t.path().join("c.db")).unwrap();
        cat.conn
            .execute_batch(
                "INSERT INTO volumes(volume_id,label,identified_by,first_seen_at,last_seen_at)
                     VALUES ('v1','V1','marker',1,1);
                 INSERT INTO files(volume_id,relative_path,filename,extension,size_bytes,
                     content_hash,created_time,modified_time,accessed_time,category,
                     container_chain,status,first_seen_at,last_seen_at) VALUES
                 ('v1','d/zeta.txt','zeta.txt','txt',100,'A',100,100,NULL,'other',NULL,'active',1,1),
                 ('v1','d/alpha.txt','alpha.txt','txt',100,'A',200,200,NULL,'other',NULL,'active',1,1);",
            )
            .unwrap();
        let id = ids(&cat);
        let (cs, _) = cat.duplicate_clusters(0, 100, 0).unwrap();
        let plan = cat.cluster_victims(&cs[0].id, 0, &[]).unwrap();
        assert_eq!(plan.keepers, vec![id["d/alpha.txt"]], "first by path, deterministically");
        assert_eq!(plan.victims, vec![id["d/zeta.txt"]]);
    }

    #[test]
    fn an_unranked_directory_sorts_last_in_path_order() {
        let (_t, cat) = seed();
        let id = ids(&cat);
        let (cs, _) = cat.duplicate_clusters(0, 100, 0).unwrap();
        let c = cs.iter().find(|c| c.group_count == 3).unwrap();
        // Empty preference: dirA sorts before dirB, so dirA keeps.
        let plan = cat.cluster_victims(&c.id, 0, &[]).unwrap();
        let mut victims = plan.victims.clone();
        victims.sort();
        let mut want = vec![id["dirB/a.txt"], id["dirB/b.txt"], id["dirB/c.txt"]];
        want.sort();
        assert_eq!(victims, want);
    }

    #[test]
    fn archived_copies_are_skipped_and_counted_never_quarantined() {
        let (_t, cat) = seed();
        cat.conn
            .execute(
                "INSERT INTO files(volume_id,relative_path,filename,extension,size_bytes,
                     content_hash,created_time,modified_time,accessed_time,category,
                     container_chain,status,first_seen_at,last_seen_at) VALUES
                 ('v1','dirB/pack.zip','a.txt','txt',100,'A',300,300,NULL,'other','pack.zip','active',1,1)",
                [],
            )
            .unwrap();
        let id = ids(&cat);
        let (cs, _) = cat.duplicate_clusters(0, 100, 0).unwrap();
        let c = cs.iter().find(|c| c.group_count == 3).unwrap();
        let plan = cat
            .cluster_victims(&c.id, 0, &[dir("v1", "dirA"), dir("v1", "dirB")])
            .unwrap();
        assert_eq!(plan.archived_skipped, 1);
        assert!(
            !plan.victims.contains(&id["dirB/pack.zip"]),
            "a copy inside a zip needs a repack, not a rename"
        );
    }

    #[test]
    fn a_cluster_id_from_a_stale_catalogue_is_refused_not_reapplied() {
        let (_t, cat) = seed();
        let (cs, _) = cat.duplicate_clusters(0, 100, 0).unwrap();
        let c = cs.iter().find(|c| c.group_count == 3).unwrap();
        let stale = c.id.clone();
        // The catalogue moves on: every dirB copy is quarantined by something else, so the
        // {dirA,dirB} set no longer describes any group.
        cat.conn
            .execute(
                "UPDATE files SET status='quarantined' WHERE relative_path LIKE 'dirB/%'",
                [],
            )
            .unwrap();
        let err = cat.cluster_victims(&stale, 0, &[]).unwrap_err();
        assert!(matches!(err, ClusterResolveError::NoSuchCluster));
    }

    #[test]
    fn a_cluster_with_no_readable_directory_refuses_to_resolve() {
        let (_t, cat) = seed();
        let (cs, _) = cat.duplicate_clusters(0, 100, 0).unwrap();
        let c = cs.iter().find(|c| c.group_count == 3).unwrap();
        let id = c.id.clone();
        cat.conn
            .execute_batch(
                "INSERT INTO scan_errors(volume_id,path,reason,occurred_at,phase,kind) VALUES
                 ('v1','dirA','denied',1,'walk','permission'),
                 ('v1','dirB','denied',1,'walk','permission');",
            )
            .unwrap();
        let err = cat.cluster_victims(&id, 0, &[]).unwrap_err();
        assert!(matches!(err, ClusterResolveError::NotKeepable));
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --lib clusters`
Expected: FAIL — `cannot find function cluster_victims`.

- [ ] **Step 3: Write the implementation**

Add to `src/catalog/clusters.rs`:

```rust
/// What a confirm resolved to. Keepers are returned as well as victims so a caller (and a test)
/// can assert that the keepers were never enqueued.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClusterPlan {
    pub victims: Vec<i64>,
    pub keepers: Vec<i64>,
    /// Redundant copies that live inside an archive. Counted so the UI can say what it could not
    /// act on; never included in `victims`.
    pub archived_skipped: i64,
}

/// Why a cluster confirm could not be turned into a victim list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClusterResolveError {
    /// The catalogue moved on and this directory set no longer describes any group. Refused rather
    /// than applied to a recomputed membership, which would act on files the user never saw.
    NoSuchCluster,
    /// The preference names a directory that is not part of this cluster.
    UnknownDirectory(ClusterDir),
    /// Every directory in the set is one the scanner could not read.
    NotKeepable,
    Db(String),
}

impl std::fmt::Display for ClusterResolveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoSuchCluster => write!(
                f,
                "this cluster no longer exists — the catalogue changed since the list was loaded"
            ),
            Self::UnknownDirectory(d) => write!(
                f,
                "{}/{} is not one of this cluster's directories",
                d.volume_id, d.dir
            ),
            Self::NotKeepable => write!(
                f,
                "every directory in this cluster is one the scan could not read, so no copy can be \
                 elected to keep"
            ),
            Self::Db(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for ClusterResolveError {}

impl Catalog {
    /// Resolve a user's directory ranking into the exact rows to quarantine.
    ///
    /// The rule is "the highest-ranked directory PRESENT wins", which is why the decision is an
    /// ordering rather than a single choice: a cluster's groups do not all have a copy in every
    /// directory. Directories the user did not rank sort last, in path order.
    ///
    /// Membership is recomputed here from `cluster_id`, never trusted from the client, and the
    /// `min_size` floor must be the one the list was rendered with -- resolving floor-free would
    /// quarantine sub-floor groups the user was never shown.
    pub fn cluster_victims(
        &self,
        cluster_id_hex: &str,
        min_size: i64,
        preference: &[ClusterDir],
    ) -> Result<ClusterPlan, ClusterResolveError> {
        let rows = self
            .duplicate_copies(min_size)
            .map_err(|e| ClusterResolveError::Db(e.to_string()))?;
        let unreadable = self
            .unreadable_dirs()
            .map_err(|e| ClusterResolveError::Db(e.to_string()))?;

        // Every group whose directory set hashes to this id.
        let mut members: Vec<Vec<&CopyRow>> = Vec::new();
        let mut dirs: Vec<ClusterDir> = Vec::new();
        for group in group_by_hash(&rows) {
            let loose: Vec<&CopyRow> = group.iter().copied().filter(|r| !r.archived).collect();
            if loose.len() < 2 {
                continue;
            }
            let mut d: Vec<ClusterDir> = loose
                .iter()
                .map(|r| ClusterDir {
                    volume_id: r.volume_id.clone(),
                    dir: parent_dir(&r.relative_path).to_string(),
                })
                .collect();
            d.sort();
            d.dedup();
            if cluster_id(&d) == cluster_id_hex {
                dirs = d;
                members.push(group);
            }
        }
        if members.is_empty() {
            return Err(ClusterResolveError::NoSuchCluster);
        }
        for p in preference {
            if !dirs.contains(p) {
                return Err(ClusterResolveError::UnknownDirectory(p.clone()));
            }
        }
        // Ranked directories first in the user's order; the rest after, in path order (`dirs` is
        // already sorted). An unreadable directory is pushed out of contention entirely.
        let mut ranked: Vec<&ClusterDir> = preference.iter().collect();
        for d in &dirs {
            if !ranked.contains(&d) {
                ranked.push(d);
            }
        }
        ranked.retain(|d| !is_unreadable(d, &unreadable));
        if ranked.is_empty() {
            return Err(ClusterResolveError::NotKeepable);
        }

        let mut plan = ClusterPlan {
            victims: Vec::new(),
            keepers: Vec::new(),
            archived_skipped: 0,
        };
        for group in members {
            plan.archived_skipped += group.iter().filter(|r| r.archived).count() as i64;
            let loose: Vec<&CopyRow> = group.iter().copied().filter(|r| !r.archived).collect();
            // `duplicate_copies` orders by (hash, relative_path, id), so "first by path" is simply
            // the first match -- which is what makes the single-directory case deterministic.
            let keeper = ranked.iter().find_map(|want| {
                loose.iter().find(|r| {
                    r.volume_id == want.volume_id && parent_dir(&r.relative_path) == want.dir
                })
            });
            let Some(keeper) = keeper else {
                // Every copy of this group sits in an unreadable directory. Quarantining them all
                // would leave no verified survivor, so the group is left alone.
                continue;
            };
            plan.keepers.push(keeper.id);
            plan.victims
                .extend(loose.iter().filter(|r| r.id != keeper.id).map(|r| r.id));
        }
        Ok(plan)
    }
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --lib clusters`
Expected: PASS (18 tests). Then `cargo clippy --all-targets -- -D warnings`.

- [ ] **Step 5: Commit**

```bash
git add src/catalog/clusters.rs
git commit -m "feat(dedup): resolve a directory ranking into the exact rows to quarantine"
```

---

### Task 4: `GET /api/duplicate-clusters`

**Files:**
- Modify: `src/web.rs` (route table around `src/web.rs:72`, handler next to `api_tree_duplicates`)
- Test: `src/web.rs` (`#[cfg(test)] mod tests`)

**Interfaces:**
- Consumes: `Catalog::duplicate_clusters`, `Cluster`, `ClusterDir`
- Produces: `GET /api/duplicate-clusters?limit&offset&min_size` returning
  `{ clusters: [{ id, dirs: [{ volume_id, dir, volume_label, mounted, unreadable }], group_count, reclaimable_bytes, sample_names, archived_group_count, keepable }], total, min_size }`

- [ ] **Step 1: Write the failing test**

Append to `mod tests` in `src/web.rs` (follow the surrounding style for building a test catalogue
and calling the handler; if the existing tests drive HTTP, do the same here):

```rust
    #[tokio::test]
    async fn duplicate_clusters_endpoint_labels_dirs_and_pages() {
        let t = tempfile::tempdir().unwrap();
        let db = t.path().join("c.db");
        {
            let cat = crate::catalog::Catalog::open(&db).unwrap();
            cat.conn
                .execute_batch(
                    "INSERT INTO volumes(volume_id,label,identified_by,first_seen_at,last_seen_at)
                         VALUES ('vol-1','Photos','marker',1,1);
                     INSERT INTO files(volume_id,relative_path,filename,extension,size_bytes,
                         content_hash,created_time,modified_time,accessed_time,category,
                         container_chain,status,first_seen_at,last_seen_at) VALUES
                     ('vol-1','dirA/a.txt','a.txt','txt',100,'A',100,100,NULL,'other',NULL,'active',1,1),
                     ('vol-1','dirB/a.txt','a.txt','txt',100,'A',200,200,NULL,'other',NULL,'active',1,1);",
                )
                .unwrap();
        }
        let body = get_json(&db, "/api/duplicate-clusters?limit=10&offset=0&min_size=0").await;
        assert_eq!(body["total"], 1);
        let c = &body["clusters"][0];
        assert_eq!(c["group_count"], 1);
        assert_eq!(c["reclaimable_bytes"], 100);
        assert_eq!(c["dirs"][0]["volume_label"], "Photos");
        assert_eq!(c["dirs"][0]["dir"], "dirA");
        assert_eq!(c["keepable"], true);
        assert_eq!(body["min_size"], 0);
    }
```

If `mod tests` in `src/web.rs` has no `get_json` helper, add one that starts the router the way the
existing tests do and returns `serde_json::Value`; reuse whatever helper is already there instead of
writing a second one.

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test --lib duplicate_clusters_endpoint`
Expected: FAIL — 404 from the router, or a missing-handler compile error.

- [ ] **Step 3: Write the implementation**

Add the route beside the other duplicate routes in `src/web.rs`:

```rust
        .route("/api/duplicate-clusters", get(api_duplicate_clusters))
```

And the handler, next to `api_tree_duplicates`:

```rust
#[derive(Deserialize)]
struct ClusterPageParams {
    limit: Option<usize>,
    offset: Option<usize>,
    min_size: Option<i64>,
}

#[derive(Serialize)]
struct ClusterDirDto {
    volume_id: String,
    dir: String,
    volume_label: String,
    mounted: bool,
    /// The scan could not open this directory, so it can never be elected keeper.
    unreadable: bool,
}

#[derive(Serialize)]
struct ClusterDto {
    id: String,
    dirs: Vec<ClusterDirDto>,
    group_count: i64,
    reclaimable_bytes: i64,
    sample_names: Vec<String>,
    archived_group_count: i64,
    keepable: bool,
}

#[derive(Serialize)]
struct ClustersDto {
    clusters: Vec<ClusterDto>,
    total: usize,
    /// Echoed back so the client confirms with the floor it actually rendered.
    min_size: i64,
}

/// Duplicate groups clustered by the set of directories they occupy (#78).
///
/// Ranked by reclaimable bytes, never by group count: the spec measured that count-ordering spends
/// ~1,800 decisions recovering ~0.1 GiB before it reaches anything worth having.
async fn api_duplicate_clusters(
    State(state): State<AppState>,
    Query(p): Query<ClusterPageParams>,
) -> Result<Json<ClustersDto>, (StatusCode, String)> {
    let cat = Catalog::open_readonly(&state.catalog_path).map_err(err500)?;
    // `effective_labels`, not `volume_stats`: two drives first seen as `D:\` must not render as
    // identical rows with identical buttons (#62).
    let labels = cat.effective_labels().map_err(err500)?;
    let mounts = state.mounts.snapshot();
    let unreadable = cat.unreadable_dirs().map_err(err500)?;
    let min_size = p
        .min_size
        .unwrap_or(crate::catalog::dedup::DEFAULT_MIN_SIZE)
        .max(0);
    let limit = p.limit.unwrap_or(100).clamp(1, 1000);
    let offset = p.offset.unwrap_or(0);
    let (clusters, total) = cat
        .duplicate_clusters(min_size, limit, offset)
        .map_err(err500)?;
    let clusters = clusters
        .into_iter()
        .map(|c| ClusterDto {
            id: c.id,
            dirs: c
                .dirs
                .into_iter()
                .map(|d| ClusterDirDto {
                    volume_label: labels
                        .get(&d.volume_id)
                        .cloned()
                        .unwrap_or_else(|| d.volume_id.clone()),
                    mounted: mounts.contains_key(&d.volume_id),
                    unreadable: crate::catalog::clusters::is_unreadable(&d, &unreadable),
                    volume_id: d.volume_id,
                    dir: d.dir,
                })
                .collect(),
            group_count: c.group_count,
            reclaimable_bytes: c.reclaimable_bytes,
            sample_names: c.sample_names,
            archived_group_count: c.archived_group_count,
            keepable: c.keepable,
        })
        .collect();
    Ok(Json(ClustersDto {
        clusters,
        total,
        min_size,
    }))
}
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test --lib duplicate_clusters_endpoint` then `cargo test`
Expected: PASS. Then `cargo clippy --all-targets -- -D warnings`.

- [ ] **Step 5: Commit**

```bash
git add src/web.rs
git commit -m "feat(web): serve duplicate clusters ranked by reclaimable bytes"
```

---

### Task 5: `POST /api/quarantine-cluster`

**Files:**
- Modify: `src/web.rs`
- Test: `tests/review_flow.rs`

**Interfaces:**
- Consumes: `Catalog::cluster_victims`, `ClusterResolveError`, `state.quarantine_queue.enqueue_files`
- Produces: `POST /api/quarantine-cluster { cluster_id, min_size, preference: [{volume_id, dir}] }`
  returning `{ queued, skipped, archived_skipped, position, unmounted_volumes: [] }`

- [ ] **Step 1: Write the failing test**

Append to `tests/review_flow.rs`, following the file's existing `start(...)` harness:

```rust
/// A cluster confirm enqueues exactly the victims, and the worker moves only those files.
#[test]
fn confirming_a_cluster_quarantines_the_victims_and_leaves_the_keepers() {
    let tmp = tempfile::tempdir().unwrap();
    let drive = tmp.path().join("drive");
    std::fs::create_dir_all(drive.join("dirA")).unwrap();
    std::fs::create_dir_all(drive.join("dirB")).unwrap();
    for n in ["a.txt", "b.txt"] {
        std::fs::write(drive.join("dirA").join(n), b"same-content").unwrap();
        std::fs::write(drive.join("dirB").join(n), b"same-content").unwrap();
    }
    // b.txt must differ from a.txt, or all four files are one group.
    std::fs::write(drive.join("dirA/b.txt"), b"other-content").unwrap();
    std::fs::write(drive.join("dirB/b.txt"), b"other-content").unwrap();

    let db = tmp.path().join("c.db");
    {
        let cat = cleanupstorages::catalog::Catalog::open(&db).unwrap();
        let hash_a = blake3::hash(b"same-content").to_hex().to_string();
        let hash_b = blake3::hash(b"other-content").to_hex().to_string();
        cat.conn
            .execute(
                "INSERT INTO volumes(volume_id,label,identified_by,first_seen_at,last_seen_at)
                     VALUES ('vol-1','Drive','marker',1,1)",
                [],
            )
            .unwrap();
        for (path, hash) in [
            ("dirA/a.txt", &hash_a),
            ("dirB/a.txt", &hash_a),
            ("dirA/b.txt", &hash_b),
            ("dirB/b.txt", &hash_b),
        ] {
            cat.conn
                .execute(
                    "INSERT INTO files(volume_id,relative_path,filename,extension,size_bytes,
                         content_hash,created_time,modified_time,accessed_time,category,
                         container_chain,status,first_seen_at,last_seen_at)
                     VALUES ('vol-1',?1,?2,'txt',12,?3,100,100,NULL,'other',NULL,'active',1,1)",
                    rusqlite::params![
                        path,
                        path.split('/').next_back().unwrap(),
                        hash.as_str()
                    ],
                )
                .unwrap();
        }
    }

    let addr = start(db.clone(), drive.clone());
    let list = get(addr, "/api/duplicate-clusters?limit=10&offset=0&min_size=0");
    let list: serde_json::Value = serde_json::from_str(&list).unwrap();
    assert_eq!(list["total"], 1, "both groups share {dirA, dirB}");
    let cluster_id = list["clusters"][0]["id"].as_str().unwrap().to_string();
    assert_eq!(list["clusters"][0]["group_count"], 2);

    let body = serde_json::json!({
        "cluster_id": cluster_id,
        "min_size": 0,
        "preference": [{"volume_id": "vol-1", "dir": "dirA"}],
    });
    let resp = post(addr, "/api/quarantine-cluster", &body.to_string());
    let resp: serde_json::Value = serde_json::from_str(&resp).unwrap();
    assert_eq!(resp["queued"], 2, "the two dirB copies");

    // Wait for the serial worker to drain, then assert on the disk.
    wait_for_quarantine_idle(addr);
    assert!(drive.join("dirA/a.txt").exists(), "keepers stay put");
    assert!(drive.join("dirA/b.txt").exists());
    assert!(!drive.join("dirB/a.txt").exists(), "victims moved to _ToDelete");
    assert!(!drive.join("dirB/b.txt").exists());
    assert!(drive.join("_ToDelete").exists());
}

/// A cluster id from a list loaded before the catalogue changed is refused, not reapplied.
#[test]
fn a_stale_cluster_confirm_is_refused() {
    // Same seed as above, but quarantine the dirB rows in the catalogue before confirming.
    // (Copy the seed block; do not factor it out unless the file already shares helpers.)
    // Assert the POST returns 409 and that no file moved.
}
```

Reuse the file's existing `get` / `post` helpers; if it has none, add small ones next to `start`.
`wait_for_quarantine_idle` polls `/api/quarantine/status` until `running` is null and `pending` is
empty, with a bounded number of attempts — model it on whatever `tests/quarantine_flow.rs` already
does rather than inventing a second waiting style. Fill in the second test's body with the same seed
block, then:

```rust
    // The catalogue moves on: the dirB rows are already gone.
    {
        let cat = cleanupstorages::catalog::Catalog::open(&db).unwrap();
        cat.conn
            .execute(
                "UPDATE files SET status='quarantined' WHERE relative_path LIKE 'dirB/%'",
                [],
            )
            .unwrap();
    }
    let (status, _body) = post_status(addr, "/api/quarantine-cluster", &body.to_string());
    assert_eq!(status, 409);
    assert!(drive.join("dirA/a.txt").exists());
    assert!(drive.join("dirB/a.txt").exists(), "nothing was applied");
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test --test review_flow cluster`
Expected: FAIL — 404 from `/api/quarantine-cluster`.

- [ ] **Step 3: Write the implementation**

Route, beside `/api/quarantine-tree`:

```rust
        .route("/api/quarantine-cluster", post(api_quarantine_cluster))
```

Handler, next to `api_quarantine`:

```rust
#[derive(Deserialize)]
struct QuarantineClusterReq {
    cluster_id: String,
    /// The review floor the client rendered the cluster with. Resolving with a different floor
    /// would act on groups whose blast radius the user was never shown.
    min_size: Option<i64>,
    #[serde(default)]
    preference: Vec<crate::catalog::clusters::ClusterDir>,
}

#[derive(Serialize, Default)]
struct QuarantineClusterDto {
    queued: usize,
    skipped: usize,
    /// Redundant copies inside archives. Reported so the user knows what the confirm could not
    /// reach; a zip entry needs a repack or an extraction, not a rename.
    archived_skipped: i64,
    position: usize,
    unmounted_volumes: Vec<String>,
}

/// Confirm one cluster: resolve the user's directory ranking into ids and enqueue them (#78).
///
/// The confirm only ENQUEUES. Every file still goes through the worker's disk-aware last-copy
/// guard one at a time, so a cluster decision can never bypass a safety check.
async fn api_quarantine_cluster(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Json<QuarantineClusterReq>,
) -> Result<Json<QuarantineClusterDto>, (StatusCode, String)> {
    check_csrf(&headers, &state)?;
    use crate::catalog::clusters::ClusterResolveError as E;

    let cat = Catalog::open_readonly(&state.catalog_path).map_err(err500)?;
    let min_size = body
        .min_size
        .unwrap_or(crate::catalog::dedup::DEFAULT_MIN_SIZE)
        .max(0);
    let plan = cat
        .cluster_victims(&body.cluster_id, min_size, &body.preference)
        .map_err(|e| match e {
            // 409, not 404: the request was well-formed, the world moved. The client reloads.
            E::NoSuchCluster => (StatusCode::CONFLICT, e.to_string()),
            E::UnknownDirectory(_) => (StatusCode::BAD_REQUEST, e.to_string()),
            E::NotKeepable => (StatusCode::CONFLICT, e.to_string()),
            E::Db(_) => (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()),
        })?;

    let mut out = QuarantineClusterDto {
        archived_skipped: plan.archived_skipped,
        ..Default::default()
    };
    let mut by_volume: std::collections::HashMap<String, Vec<i64>> =
        std::collections::HashMap::new();
    for id in plan.victims {
        match cat.get_file(id).map_err(err500)? {
            Some(rec) => by_volume.entry(rec.volume_id).or_default().push(id),
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
            // Already queued: the reviewer double-clicked, and the decision is on its way.
            None => out.skipped += n,
        }
    }
    Ok(Json(out))
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test --test review_flow cluster` then `cargo test`
Expected: PASS. Then `cargo clippy --all-targets -- -D warnings`.

- [ ] **Step 5: Commit**

```bash
git add src/web.rs tests/review_flow.rs
git commit -m "feat(web): confirm a duplicate cluster by ranking its directories"
```

---

### Task 6: The Clusters section on the Review page

**Files:**
- Modify: `src/web_ui.rs` (markup after the `#treesec` section at `src/web_ui.rs:917-925`; JS after
  `loadTrees`/`renderGroups` at `src/web_ui.rs:1042-1136`)

**Interfaces:**
- Consumes: `GET /api/duplicate-clusters`, `POST /api/quarantine-cluster`, and the page's existing
  `apiGet`, `apiPost`, `esc`, `fmtN`, `fmtB`, `pollQuarantine`, `$`

- [ ] **Step 1: Add the markup**

Insert immediately after the closing `</section>` of `#treesec`, so the escalation runs
identical folders → clusters → per-file, biggest decision first:

```html
  <section id="clustersec" style="display:none;margin-bottom:26px">
    <h2 style="font-size:15px;margin:0 0 4px">Folders that overlap</h2>
    <p class="mut" style="font-size:12px;margin:0 0 12px">Folders that share many duplicates without
      matching exactly. Click the folders in the order you prefer them — for each duplicate the
      highest-ranked folder that has a copy keeps it, and the others move to
      <span class="mono">_ToDelete</span>. Nothing is deleted until you purge.</p>
    <div id="clusterlist"></div>
  </section>
```

- [ ] **Step 2: Add the JS**

Append after `confirmTree`, before the `let qTimer=null;` line:

```js
// ---- Overlapping folders (#78) ---------------------------------------------------------------
// Ordered by reclaimable BYTES, never by decision count: the measurement behind this section found
// the two anti-correlated -- count-ordering spends ~1,800 decisions on build output worth 0.1 GiB
// before reaching the course folders worth ~23 GiB.
let clusterOffset=0, clusterTotal=0, clusterFloor=0;
async function loadClusters(more){
  if(!more) clusterOffset=0;
  clusterFloor=minSize;
  let data; try{
    data=await apiGet("/api/duplicate-clusters?limit=100&min_size="+clusterFloor+"&offset="+clusterOffset);
  }catch(e){ return; }
  clusterTotal=data.total;
  const sec=$("#clustersec"), host=$("#clusterlist");
  if(!data.clusters.length && !more){ sec.style.display="none"; return; }
  sec.style.display="";
  if(!more) host.textContent="";
  clusterOffset += data.clusters.length;
  for(const c of data.clusters) host.appendChild(clusterCard(c));
  const old=document.getElementById("clustermore"); if(old) old.remove();
  if(clusterOffset < clusterTotal){
    const b=document.createElement("button");
    b.id="clustermore"; b.className="linkbtn"; b.style.cssText="margin-top:12px;font-size:12.5px";
    b.textContent="Load more ("+(clusterTotal-clusterOffset).toLocaleString()+" clusters left)";
    b.addEventListener("click", async ()=>{ b.disabled=true; b.textContent="Loading...";
      await loadClusters(true); });
    host.parentNode.insertBefore(b, host.nextSibling);
  }
}

function clusterCard(c){
  const box=document.createElement("div");
  box.className="card";
  box.style.cssText="padding:12px 14px;margin-bottom:10px";
  const head=document.createElement("div");
  head.style.cssText="font-size:13px;margin-bottom:8px";
  // Blast radius first: this is one click standing in for hundreds of decisions.
  head.innerHTML=`<strong>${fmtN(c.group_count)} duplicate groups</strong> <span class="mut">· ${
    esc(fmtB(c.reclaimable_bytes))} reclaimable · ${esc(c.sample_names.join(", "))}</span>`;
  box.appendChild(head);

  const pref=[]; // ClusterDir objects, in the order the user clicked them
  const rows=new Map();
  for(const d of c.dirs){
    const row=document.createElement("div");
    row.className="row";
    row.style.cssText="justify-content:space-between;gap:12px;padding:4px 0;flex-wrap:wrap";
    const label=document.createElement("span");
    label.className="mono";
    label.style.cssText="font-size:12px;word-break:break-all";
    label.textContent=d.volume_label+" / "+(d.dir||"(drive root)");
    row.appendChild(label);
    const rank=document.createElement("span");
    rank.className="mut"; rank.style.fontSize="11.5px";
    row.appendChild(rank);
    if(d.unreadable){
      rank.textContent="the scan could not read this folder — cannot be kept";
    }else if(!d.mounted){
      rank.textContent="drive not connected";
    }else{
      const btn=document.createElement("button");
      btn.className="linkbtn"; btn.style.fontSize="12.5px";
      btn.textContent="Prefer this folder";
      btn.addEventListener("click",()=>{
        if(pref.some(p=>p.volume_id===d.volume_id&&p.dir===d.dir)) return;
        pref.push({volume_id:d.volume_id,dir:d.dir});
        for(const [k,el] of rows){ const i=pref.findIndex(p=>k===p.volume_id+"\u0000"+p.dir);
          el.textContent = i<0 ? "" : (i===0?"keep first":"then keep #"+(i+1)); }
        confirm_.disabled=false;
      });
      row.appendChild(btn);
    }
    rows.set(d.volume_id+"\u0000"+d.dir, rank);
    box.appendChild(row);
  }
  if(c.archived_group_count){
    const n=document.createElement("div");
    n.className="mut"; n.style.cssText="font-size:11.5px;margin-top:6px";
    n.textContent=fmtN(c.archived_group_count)+" of these also have a copy inside an archive — "
      +"those need a repack, and are left alone.";
    box.appendChild(n);
  }
  const confirm_=document.createElement("button");
  confirm_.className="btn btn-primary";
  confirm_.style.cssText="margin-top:10px;font-size:12.5px";
  confirm_.textContent="Quarantine the rest";
  confirm_.disabled=true;
  if(!c.keepable){
    confirm_.style.display="none";
    const w=document.createElement("div");
    w.className="mut"; w.style.cssText="font-size:11.5px;margin-top:6px";
    w.textContent="Every folder here is one the scan could not read, so there is no verified copy "
      +"to keep. Nothing can be confirmed for this cluster.";
    box.appendChild(w);
  }
  confirm_.addEventListener("click", async ()=>{
    // The line breaks below MUST stay as the two-character escape \n — a real newline inside this
    // string is a syntax error that kills the whole page script.
    if(!confirm("Quarantine the redundant copies in "+fmtN(c.group_count)+" duplicate groups?\n\n"
      +"Keeping, in order:\n"+pref.map((p,i)=>(i+1)+". "+p.volume_id+" / "+(p.dir||"(drive root)")).join("\n")
      +"\n\n"+fmtB(c.reclaimable_bytes)+" reclaimable. Nothing is deleted until you purge.")) return;
    confirm_.disabled=true; confirm_.textContent="Queued";
    try{
      const r=await apiPost("/api/quarantine-cluster",
        {cluster_id:c.id, min_size:clusterFloor, preference:pref});
      if(r.skipped) $("#msg").textContent=r.skipped+" copies were skipped (drive not connected, or already queued).";
      pollQuarantine();
    }catch(e){
      // A refused confirm (stale cluster) lands here. Reload rather than retry: the list the user
      // was looking at no longer describes the catalogue.
      $("#msg").textContent="Could not queue: "+e.message;
      confirm_.disabled=false; confirm_.textContent="Quarantine the rest";
      loadClusters();
    }
  });
  box.appendChild(confirm_);
  return box;
}
```

- [ ] **Step 3: Wire it into the page's load points**

Three edits, all in the Review page script:

1. After the existing `loadTrees();` call at the bottom, add `loadClusters();`
2. In `pollQuarantine`, where it currently reloads trees on the busy→idle edge
   (`if(qWasBusy && !busy){ ... await loadTrees(); }`), also `await loadClusters();`
3. In the `#minsize` change handler, add `loadClusters();` so the floor the clusters were computed
   with always matches the one the user selected.

- [ ] **Step 4: Verify by hand**

Run: `cargo run --release -- browse`
Check: the Review page shows a **Folders that overlap** section under Identical folders; clicking a
folder shows "keep first", a second shows "then keep #2"; confirming shows the queue status line and
the rows disappear once the worker drains. Confirm the browser console is clean — a JS syntax error
here silently blanks the per-file list too.

- [ ] **Step 5: Commit**

```bash
git add src/web_ui.rs
git commit -m "feat(review): rank overlapping folders once instead of deciding file by file"
```

---

### Task 7: Spec, docs and issue

**Files:**
- Create: `docs/superpowers/specs/2026-08-26-duplicate-cluster-review-design.md`
- Modify: `CLAUDE.md` if the CLI/route list it describes changed (it does not — no new verb)
- Modify: `posts/POST-MATERIAL.md`

- [ ] **Step 1: Bring the spec onto this branch**

```bash
git checkout origin/docs/dedup-finish-specs -- docs/superpowers/specs/2026-08-26-duplicate-cluster-review-design.md
```

- [ ] **Step 2: Mark it accepted and record the deviation**

Change `**Status:** proposed` to `**Status:** accepted`, and append to the Decisions table:

```markdown
| The confirm carries the review floor | **`POST` takes `min_size`** | Cluster membership is computed over groups at or above the floor. Resolving the confirm floor-free would quarantine sub-floor groups in the same directories — a blast radius larger than the one the user was shown |
```

- [ ] **Step 3: Append the material line**

Add one line to `posts/POST-MATERIAL.md` in the file's existing format, covering: 13,783 groups →
4,017 clusters (3.4×); the pairwise trap (133,490 pairs, 10× worse); and the anti-correlation —
count-ordered top 15 clusters are Xilinx build output worth 0.003–0.009 GiB each, byte-ordered top 15
are course folders worth 0.72–4.98 GiB each.

- [ ] **Step 4: Full verification**

Run: `cargo test && cargo clippy --all-targets -- -D warnings && cargo fmt --check`
Expected: all green. Every claim of completeness must quote this output.

- [ ] **Step 5: Commit and open the PR**

```bash
git add docs posts
git commit -m "docs(dedup): accept the cluster-review spec and record the floor deviation"
git push -u origin feat/duplicate-cluster-review
gh pr create --title "Cluster duplicate groups by directory set (#78)" --body "Closes #78."
```

---

## Self-review notes

- **Spec coverage:** cluster key (T1), rejected pairwise (T1 test), byte ordering (T1 test), the
  ordering-not-a-choice decision (T3), cluster-level confirm / per-file execution (T5), `Job::Files`
  execution (T5), never elect an unreadable directory (T2/T3), unreadable + uncatalogued warned not
  blocking (T2 `keepable`, T6 copy), archived shown never quarantined (T2/T3), derived not stored
  (T1), cluster identity + stale refusal (T1/T3), preference resolution examples (T3 tests), UI
  placement and paging (T6), all ten testing requirements (T1: 1, 2, 9, 10; T2: 7, 8; T3: 3, 4, 6;
  T5: 5 end to end).
- **Known gap, deliberate:** the spec's "Drive not mounted → reported at request time" is handled the
  way `api_quarantine` already does it — counted into `skipped` with the volume named — rather than
  failing the whole confirm, so a cluster spanning a connected and a disconnected drive still does
  the half it can.
