pub mod backup;
pub mod clusters;
pub mod dedup;
pub mod models;
pub mod pending_formats;
pub mod scan_errors;
pub mod scan_runs;
pub mod schema;
pub mod store;

use rusqlite::{Connection, OpenFlags};
use std::path::Path;

/// An open handle to the catalog database.
pub struct Catalog {
    pub conn: Connection,
}

impl Catalog {
    /// Open (creating if needed) the catalog at `path`, enabling WAL and the schema.
    pub fn open(path: &Path) -> anyhow::Result<Catalog> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        // NORMAL, not the default FULL: in WAL this drops one fsync per COMMIT and *cannot corrupt
        // the database* -- a power loss can lose the most recent commits, never leave a torn file.
        // What that costs here is at most the last batch of files, which are simply not yet
        // catalogued; the next scan re-hashes them through the ordinary incremental skip. No file on
        // disk is touched and nothing is marked missing, which is why a durability reduction is
        // acceptable here and would not be elsewhere in this project.
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        schema::apply(&conn)?;
        Ok(Catalog { conn })
    }

    /// Open the catalog READ-ONLY (no directory creation, no schema DDL, no WAL switch).
    /// The file must already exist. For query-only consumers like the browse server.
    pub fn open_readonly(path: &Path) -> anyhow::Result<Catalog> {
        let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        Ok(Catalog { conn })
    }

    /// Run PRAGMA integrity_check; true if the DB reports "ok".
    /// The file this catalogue is open on, asked of SQLite itself.
    ///
    /// Avoids threading a path through call chains that already hold a `Catalog` -- and cannot
    /// drift from the connection the way a separately-passed path could.
    pub fn db_path(&self) -> Option<std::path::PathBuf> {
        self.conn
            .query_row(
                "SELECT file FROM pragma_database_list WHERE name='main'",
                [],
                |r| r.get::<_, Option<String>>(0),
            )
            .ok()
            .flatten()
            .filter(|s| !s.is_empty())
            .map(std::path::PathBuf::from)
    }

    pub fn integrity_ok(&self) -> anyhow::Result<bool> {
        let result: String = self
            .conn
            .query_row("PRAGMA integrity_check", [], |r| r.get(0))?;
        Ok(result == "ok")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_readonly_reads_existing_catalog() {
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("catalog.db");
        {
            let cat = Catalog::open(&db).unwrap();
            cat.upsert_volume(&crate::catalog::models::Volume {
                volume_id: "vol-1".into(),
                label: "Test HDD".into(),
                identified_by: "marker".into(),
                first_seen_at: 1,
                last_seen_at: 1,
            })
            .unwrap();
        } // dropped: closes the write handle

        let ro = Catalog::open_readonly(&db).unwrap();
        let stats = ro.volume_stats().unwrap();
        assert_eq!(stats.len(), 1);
        assert_eq!(stats[0].0, "vol-1");
    }

    #[test]
    fn the_catalog_opens_in_wal_with_synchronous_normal() {
        // WAL + NORMAL is the pairing that makes dropping the per-commit fsync safe: it cannot
        // corrupt the file, only lose the most recent commits -- which a rescan rebuilds.
        let t = tempfile::tempdir().unwrap();
        let cat = Catalog::open(&t.path().join("c.db")).unwrap();
        let journal: String = cat
            .conn
            .query_row("PRAGMA journal_mode", [], |r| r.get(0))
            .unwrap();
        assert_eq!(journal.to_lowercase(), "wal");
        // 1 == NORMAL. FULL (2) would keep the fsync we are removing; OFF (0) would be unsafe.
        let sync: i64 = cat
            .conn
            .query_row("PRAGMA synchronous", [], |r| r.get(0))
            .unwrap();
        assert_eq!(sync, 1, "expected NORMAL");
    }
}
