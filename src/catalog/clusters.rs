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
    /// Groups in this cluster whose copies also live inside archives. Counted and shown, never
    /// quarantined -- those need a repack or an extraction, not a rename.
    pub archived_group_count: i64,
    /// False when every directory in the set is one the scanner could not read, which makes the
    /// cluster unconfirmable.
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

/// One copy of a duplicated hash, as the clustering pass needs it.
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
    /// Every active row that belongs to a duplicate group at or above `min_size`, archived rows of
    /// those same hashes included so they can be counted.
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

    /// Clusters at or above `min_size`, ranked by reclaimable bytes descending, then by id so the
    /// order is total and paging is stable. Returns the page and the full count.
    pub fn duplicate_clusters(
        &self,
        min_size: i64,
        limit: usize,
        offset: usize,
    ) -> anyhow::Result<(Vec<Cluster>, usize)> {
        let rows = self.duplicate_copies(min_size)?;
        let unreadable = self.unreadable_dirs()?;
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
            if group.iter().any(|r| r.archived) {
                c.archived_group_count += 1;
            }
            if c.sample_names.len() < 3 {
                let name = loose[0].filename.clone();
                if !c.sample_names.contains(&name) {
                    c.sample_names.push(name);
                }
            }
        }

        // A cluster is confirmable only if SOMETHING in it can be elected keeper. Electing an
        // unreadable directory would trade a survivor we verified for one we did not.
        for c in acc.values_mut() {
            c.keepable = c.dirs.iter().any(|d| !is_unreadable(d, &unreadable));
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

/// What a confirm resolved to. Keepers are returned as well as victims so a caller (and a test)
/// can assert that the keepers were never enqueued.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClusterPlan {
    pub victims: Vec<i64>,
    pub keepers: Vec<i64>,
    /// Copies that live inside an archive. Counted so the UI can say what it could not act on;
    /// never included in `victims`.
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
                ClusterDir {
                    volume_id: "v1".into(),
                    dir: "dirA".into()
                },
                ClusterDir {
                    volume_id: "v1".into(),
                    dir: "dirB".into()
                },
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
        assert_eq!(
            holding_a.len(),
            1,
            "one cluster over {{A,B,C}}, not three pairs"
        );
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
        let a = ClusterDir {
            volume_id: "v1".into(),
            dir: "dirA".into(),
        };
        let b = ClusterDir {
            volume_id: "v1".into(),
            dir: "dirB".into(),
        };
        assert_eq!(cluster_id(&[a.clone(), b.clone()]), cluster_id(&[b, a]));
    }

    #[test]
    fn parent_dir_handles_both_separators_and_the_drive_root() {
        assert_eq!(parent_dir("a/b/c.txt"), "a/b");
        assert_eq!(parent_dir("a\\b\\c.txt"), "a\\b");
        assert_eq!(parent_dir("c.txt"), "");
    }

    fn dir(v: &str, d: &str) -> ClusterDir {
        ClusterDir {
            volume_id: v.into(),
            dir: d.into(),
        }
    }

    /// Path -> id, so the assertions can name files instead of integers.
    fn ids(cat: &Catalog) -> HashMap<String, i64> {
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
        assert_eq!(
            victims, want_v,
            "confirming enqueues victims, never keepers"
        );
    }

    #[test]
    fn a_group_missing_from_the_top_ranked_directory_falls_through_to_the_next() {
        let (_t, cat) = seed();
        // Hash E lives in dirB and dirC only, so its cluster does not contain dirA at all.
        cat.conn
            .execute_batch(
                "INSERT INTO files(volume_id,relative_path,filename,extension,size_bytes,
                     content_hash,created_time,modified_time,accessed_time,category,
                     container_chain,status,first_seen_at,last_seen_at) VALUES
                 ('v1','dirB/e1.txt','e1.txt','txt',100,'E',100,100,NULL,'other',NULL,'active',1,1),
                 ('v1','dirC/e2.txt','e2.txt','txt',100,'E',200,200,NULL,'other',NULL,'active',1,1);",
            )
            .unwrap();
        let id = ids(&cat);
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
        assert_eq!(plan.keepers, vec![id["dirC/e2.txt"]]);
        assert_eq!(plan.victims, vec![id["dirB/e1.txt"]]);
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
        assert_eq!(
            plan.keepers,
            vec![id["d/alpha.txt"]],
            "first by path, deterministically"
        );
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
        let archived_id: i64 = cat
            .conn
            .query_row(
                "SELECT id FROM files WHERE container_chain='pack.zip'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let (cs, _) = cat.duplicate_clusters(0, 100, 0).unwrap();
        let c = cs.iter().find(|c| c.group_count == 3).unwrap();
        let plan = cat
            .cluster_victims(&c.id, 0, &[dir("v1", "dirA"), dir("v1", "dirB")])
            .unwrap();
        assert_eq!(plan.archived_skipped, 1);
        assert!(
            !plan.victims.contains(&archived_id),
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
        assert_eq!(
            un,
            vec![ClusterDir {
                volume_id: "v1".into(),
                dir: "dirA".into()
            }]
        );
        assert!(is_unreadable(
            &ClusterDir {
                volume_id: "v1".into(),
                dir: "dirA".into()
            },
            &un
        ));
        assert!(
            is_unreadable(
                &ClusterDir {
                    volume_id: "v1".into(),
                    dir: "dirA/sub".into()
                },
                &un
            ),
            "a directory beneath an unopenable one is unverified too"
        );
        assert!(
            !is_unreadable(
                &ClusterDir {
                    volume_id: "v1".into(),
                    dir: "dirAB".into()
                },
                &un
            ),
            "prefix match must respect the separator, or dirAB inherits dirA's verdict"
        );
        assert!(
            !is_unreadable(
                &ClusterDir {
                    volume_id: "v2".into(),
                    dir: "dirA".into()
                },
                &un
            ),
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
    fn groups_whose_copies_are_also_archived_are_counted_not_acted_on() {
        let (_t, cat) = seed();
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
