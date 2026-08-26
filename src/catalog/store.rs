use crate::catalog::models::*;
use crate::catalog::Catalog;
use rusqlite::params;

/// Optional filters for a catalog search/browse. Empty vec / `None` = match everything; each of the
/// `category`/`volume`/`status` vecs is OR-combined (SQL `IN`), so the filters are multi-select.
#[derive(Default, Debug, Clone)]
pub struct SearchFilters {
    pub query: String,
    pub category: Vec<String>,
    pub volume: Vec<String>,
    pub status: Vec<String>,
    pub min_size: Option<i64>,
    pub max_size: Option<i64>,
    pub modified_after: Option<i64>,
    pub modified_before: Option<i64>,
}

/// `(size_bytes, modified_time, has_archive_entries, revive_floor)` -- see `Catalog::get_file_meta`
/// for what each field means.
pub type FileMeta = (i64, i64, bool, Option<i64>);

/// The full `files` column list, in one place. Every full-row SELECT uses this; the mapper
/// (`map_file_record`) reads results by column NAME, so this list and the mapper cannot drift.
/// A subdirectory sitting directly inside some folder, with totals for its entire subtree.
#[derive(Debug, Clone)]
pub struct FolderChild {
    pub name: String,
    pub path: String,
    pub file_count: i64,
    pub total_bytes: i64,
}

/// One row per archive that still has active catalogued entries, with what extracting it costs.
/// This is what the Extract page lists; the per-archive verdict is computed separately, against a
/// live mount.
#[derive(Debug, Clone)]
pub struct ArchiveRoot {
    pub relative_path: String,
    pub entries: i64,
    pub uncompressed_bytes: i64,
}

pub(crate) const FILE_COLUMNS: &str =
    "id, volume_id, relative_path, filename, extension, size_bytes, content_hash, \
     created_time, modified_time, accessed_time, category, container_chain, \
     status, first_seen_at, last_seen_at, original_path";

/// Where one archived entry row moves to once its archive has been extracted.
/// `container_chain` is `None` when the entry became a loose file, or the remaining chain when
/// its first hop was a nested archive that is now a file on disk (the row stays archived, just
/// re-pointed at the extracted inner archive).
pub struct EntryMove {
    pub id: i64,
    pub relative_path: String,
    pub container_chain: Option<String>,
}

impl Catalog {
    pub fn upsert_volume(&self, v: &Volume) -> anyhow::Result<()> {
        self.conn.execute(
            "INSERT INTO volumes(volume_id, label, identified_by, first_seen_at, last_seen_at)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(volume_id) DO UPDATE SET label=excluded.label,
                 identified_by=excluded.identified_by, last_seen_at=excluded.last_seen_at",
            params![
                v.volume_id,
                v.label,
                v.identified_by,
                v.first_seen_at,
                v.last_seen_at
            ],
        )?;
        Ok(())
    }

    /// `(size_bytes, modified_time, has_archive_entries, revive_floor)` for a loose file, or None if
    /// unknown.
    ///
    /// The third field exists so the incremental-skip path never has to open a file to learn
    /// whether it is an archive. Guessing from the filename would be wrong for a renamed zip, and
    /// a skip path that fails to touch an archive's entries lets the sweep mark present files
    /// missing.
    ///
    /// The fourth field is `Some(last_seen_at)` -- this row's own `last_seen_at`, from before this
    /// call touches it -- when this row's status is `missing`, and `None` otherwise. It tells the
    /// skip path not just THAT the whole archive just reappeared, but WHEN it went missing, which
    /// `touch_archive_entries` uses as a floor: an entry that was legitimately removed from the
    /// archive's content has a strictly smaller `last_seen_at` than the archive had when it went
    /// missing, so the floor is what separates "went missing together with the archive" from "was
    /// removed before the archive went missing" -- a plain boolean cannot make that distinction.
    pub fn get_file_meta(
        &self,
        volume_id: &str,
        relative_path: &str,
    ) -> anyhow::Result<Option<FileMeta>> {
        // These five statements run once or more PER FILE -- on a 20 TB corpus, on the order of 50
        // million times each -- so they use `prepare_cached` to avoid re-parsing/re-planning the
        // same SQL on every call. Everything else in this module (`forget_volume`, snapshots,
        // settings and pending-format handlers) runs at most once per scan and keeps plain
        // `execute`: caching a once-per-scan statement buys nothing and widens the diff for no
        // reason. The SQL text below must stay byte-identical to before, since the cache is keyed on
        // the exact string.
        let mut stmt = self.conn.prepare_cached(
            "SELECT size_bytes, IFNULL(modified_time,0),
                    EXISTS(SELECT 1 FROM files e
                            WHERE e.volume_id=?1 AND e.relative_path=?2
                              AND e.container_chain IS NOT NULL),
                    CASE WHEN status='missing' THEN last_seen_at ELSE NULL END
               FROM files
              WHERE volume_id=?1 AND relative_path=?2 AND container_chain IS NULL",
        )?;
        let row = stmt.query_row(params![volume_id, relative_path], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, i64>(2)? != 0,
                r.get::<_, Option<i64>>(3)?,
            ))
        });
        match row {
            Ok(v) => Ok(Some(v)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Invalidate the cached fingerprint the incremental-skip check compares against, without
    /// touching `status`, `content_hash`, or `last_seen_at`.
    ///
    /// Used by `api_resolve_format` (action `"descend"`) when an unfamiliar extension is approved:
    /// the file on disk has not changed, but the policy governing it has, so the skip check at
    /// `src/scanner.rs` (`old_size == size && old_mtime == mtime.unwrap_or(0)`) must fail on the
    /// next ordinary (`force=false`) pass, forcing a re-hash -- only a re-hash reaches
    /// `descend_archive`. Without this, a rescan takes the skip path forever and "approve" is a
    /// silent no-op until the next `--force` (see F-1 in the archive-descent-policy review).
    ///
    /// `modified_time = -1` is the sentinel. `unix_secs` (`src/scanner.rs`) computes a real file's
    /// mtime via `duration_since(UNIX_EPOCH)`, which is `Err` (not a negative number) for any
    /// pre-epoch or unreadable timestamp; that case is stored as SQL `NULL` and read back as `0`
    /// through `IFNULL(modified_time,0)` in `get_file_meta`. So a genuine `modified_time` is always
    /// `NULL` or `>= 0` -- `-1` can never collide with it, guaranteeing the comparison fails
    /// regardless of the file's real mtime.
    ///
    /// Scoped to exactly one `(volume_id, relative_path)` loose-file row (`container_chain IS
    /// NULL`, matching the unique index `idx_files_loose_identity`): approving one extension cannot
    /// touch any other file's fingerprint, and this never changes `status`, so it cannot mark
    /// anything `missing`.
    pub fn invalidate_scan_fingerprint(
        &self,
        volume_id: &str,
        relative_path: &str,
    ) -> anyhow::Result<()> {
        self.conn.execute(
            "UPDATE files SET modified_time = -1
             WHERE volume_id=?1 AND relative_path=?2 AND container_chain IS NULL",
            params![volume_id, relative_path],
        )?;
        Ok(())
    }

    /// Insert or update one loose file; sets status=active and last_seen_at=now.
    pub fn upsert_file(&self, f: &NewFile, now: i64) -> anyhow::Result<()> {
        self.conn
            .prepare_cached(
                "INSERT INTO files(volume_id, relative_path, filename, extension, size_bytes,
                 content_hash, created_time, modified_time, accessed_time, category,
                 container_chain, status, first_seen_at, last_seen_at)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,'active',?12,?12)
             ON CONFLICT(volume_id, relative_path) WHERE container_chain IS NULL DO UPDATE SET
                 filename=excluded.filename, extension=excluded.extension,
                 size_bytes=excluded.size_bytes, content_hash=excluded.content_hash,
                 created_time=excluded.created_time, modified_time=excluded.modified_time,
                 accessed_time=excluded.accessed_time, category=excluded.category,
                 status='active', last_seen_at=excluded.last_seen_at
                 WHERE files.status IN ('active','missing')",
            )?
            .execute(params![
                f.volume_id,
                f.relative_path,
                f.filename,
                f.extension,
                f.size_bytes,
                f.content_hash,
                f.created_time,
                f.modified_time,
                f.accessed_time,
                f.category.as_str(),
                f.container_chain,
                now
            ])?;
        Ok(())
    }

    /// Refresh last_seen/status for an unchanged file without re-hashing. Returns true if a row matched.
    pub fn touch_seen(
        &self,
        volume_id: &str,
        relative_path: &str,
        now: i64,
    ) -> anyhow::Result<bool> {
        let n = self
            .conn
            .prepare_cached(
                "UPDATE files SET last_seen_at=?3, status='active'
             WHERE volume_id=?1 AND relative_path=?2 AND container_chain IS NULL
               AND status IN ('active','missing')",
            )?
            .execute(rusqlite::params![volume_id, relative_path, now])?;
        Ok(n > 0)
    }

    /// Flag active files (loose or archived) on this volume not touched by the current scan as missing.
    /// `unreadable_prefixes` are directories this scan could not enumerate (permission denied, I/O
    /// error). Files beneath them were never visited, so they look untouched — but they are almost
    /// certainly still on disk. Sweeping them to `missing` says "your files are gone" about files
    /// that are merely unreadable, which is alarming and wrong (#7). They are excluded instead.
    pub fn mark_missing_scanned(
        &self,
        volume_id: &str,
        scan_started_at: i64,
        _now: i64,
        unreadable_prefixes: &[String],
    ) -> anyhow::Result<usize> {
        let mut sql = String::from(
            "UPDATE files SET status='missing'
             WHERE volume_id=?1 AND status='active' AND last_seen_at < ?2",
        );
        let mut args: Vec<Box<dyn rusqlite::ToSql>> =
            vec![Box::new(volume_id.to_string()), Box::new(scan_started_at)];
        for prefix in unreadable_prefixes {
            // The directory itself, and anything under it. LIKE metacharacters in real paths (`%`
            // and `_` are legal filename characters) are escaped, or a path containing one would
            // shield unrelated files from the sweep.
            sql.push_str(" AND relative_path <> ? AND relative_path NOT LIKE ? ESCAPE '\\'");
            let escaped = prefix
                .replace('\\', "\\\\")
                .replace('%', "\\%")
                .replace('_', "\\_");
            args.push(Box::new(prefix.clone()));
            args.push(Box::new(format!("{escaped}/%")));
        }
        let refs: Vec<&dyn rusqlite::ToSql> = args.iter().map(|b| b.as_ref()).collect();
        let n = self.conn.execute(&sql, refs.as_slice())?;
        Ok(n)
    }

    /// Insert/update one archive entry (a file inside an archive). Identity is
    /// (volume_id, archive_rel_path, container_chain) via idx_files_archived_identity.
    ///
    /// `archive_modified` is the containing archive's own mtime. A zip records a per-entry date, but
    /// we do not read it; inheriting the archive's date is what lets Browse's date filter include
    /// archived files instead of dropping every one (#10). It is updated on conflict so a repacked
    /// or replaced archive re-dates its entries.
    pub fn upsert_archive_entry(
        &self,
        volume_id: &str,
        archive_rel_path: &str,
        e: &crate::archive::ArchiveEntry,
        archive_modified: Option<i64>,
        now: i64,
    ) -> anyhow::Result<()> {
        self.conn
            .prepare_cached(
                "INSERT INTO files(volume_id, relative_path, filename, extension, size_bytes,
                 content_hash, created_time, modified_time, accessed_time, category,
                 container_chain, status, first_seen_at, last_seen_at)
             VALUES (?1,?2,?3,?4,?5,?6,NULL,?7,NULL,?8,?9,'active',?10,?10)
             ON CONFLICT(volume_id, relative_path, container_chain)
                 WHERE container_chain IS NOT NULL DO UPDATE SET
                 filename=excluded.filename, extension=excluded.extension,
                 size_bytes=excluded.size_bytes, content_hash=excluded.content_hash,
                 modified_time=excluded.modified_time,
                 category=excluded.category, status='active', last_seen_at=excluded.last_seen_at
                 WHERE files.status IN ('active','missing')",
            )?
            .execute(params![
                volume_id,
                archive_rel_path,
                e.filename,
                e.extension,
                e.size_bytes,
                e.content_hash,
                archive_modified,
                Category::from_extension(&e.extension).as_str(),
                e.container_chain,
                now
            ])?;
        Ok(())
    }

    /// Refresh last_seen/status for every archive entry under one archive file (unchanged-archive skip).
    ///
    /// `revive_floor` distinguishes two cases that both reach this function with the archive's own
    /// row unchanged in size/mtime, and that must be handled oppositely:
    ///
    /// - `Some(floor)`: the WHOLE archive disappeared (e.g. an unmounted drive) and has now
    ///   reappeared unchanged. `floor` is the `last_seen_at` the archive's own row had at the moment
    ///   it went missing (from `get_file_meta`). A `missing` entry revives only if ITS OWN
    ///   `last_seen_at >= floor` -- i.e. it was still present when the archive itself vanished, so it
    ///   went missing together with the archive, not before it. An entry with a smaller
    ///   `last_seen_at` was removed from the archive's real content by an earlier, real descend, and
    ///   must stay `missing` even though the archive has now returned -- reviving it would assert a
    ///   file is present that the archive demonstrably does not contain (a phantom entry that can
    ///   surface in Duplicates as a fake "safe" copy of a real file).
    /// - `None`: the archive was never absent (its own row was not `missing`); no `missing` entry
    ///   under it is revived at all -- only `active` rows are touched.
    ///
    /// `quarantined`/`purged` are never revived either way -- those are user decisions about files
    /// that were moved or deleted, and a scan must never silently flip them back to `active`.
    pub fn touch_archive_entries(
        &self,
        volume_id: &str,
        archive_rel_path: &str,
        now: i64,
        revive_floor: Option<i64>,
    ) -> anyhow::Result<usize> {
        let n = match revive_floor {
            Some(floor) => self
                .conn
                .prepare_cached(
                    "UPDATE files SET last_seen_at=?3, status='active'
                 WHERE volume_id=?1 AND relative_path=?2 AND container_chain IS NOT NULL
                   AND (status='active' OR (status='missing' AND last_seen_at>=?4))",
                )?
                .execute(params![volume_id, archive_rel_path, now, floor])?,
            None => self
                .conn
                .prepare_cached(
                    "UPDATE files SET last_seen_at=?3, status='active'
                 WHERE volume_id=?1 AND relative_path=?2 AND container_chain IS NOT NULL
                   AND status='active'",
                )?
                .execute(params![volume_id, archive_rel_path, now])?,
        };
        Ok(n)
    }

    /// The volume's last_seen_at (updated on every scan), if the volume exists.
    pub fn volume_last_seen(&self, volume_id: &str) -> anyhow::Result<Option<i64>> {
        let row = self.conn.query_row(
            "SELECT last_seen_at FROM volumes WHERE volume_id=?1",
            params![volume_id],
            |r| r.get::<_, i64>(0),
        );
        match row {
            Ok(t) => Ok(Some(t)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// For the given content hashes, those with >1 active copy in the catalog, mapped to their
    /// active copy count. Bounded by the passed hashes (indexed on content_hash).
    pub fn duplicate_counts(
        &self,
        hashes: &[String],
    ) -> anyhow::Result<std::collections::HashMap<String, i64>> {
        let mut out = std::collections::HashMap::new();
        if hashes.is_empty() {
            return Ok(out);
        }
        // Deduplicate the input so the IN-list stays small.
        let uniq: std::collections::HashSet<&String> = hashes.iter().collect();
        let placeholders = std::iter::repeat_n("?", uniq.len())
            .collect::<Vec<_>>()
            .join(",");
        // `INDEXED BY` because the planner gets this badly wrong: left to itself it picks
        // idx_files_dedup on `status=?` and walks every active row -- 1,271 ms on the real
        // catalogue -- rather than doing a handful of hash lookups, which is 1 ms. The choice is
        // not close and does not depend on the input, so it is stated rather than hoped for.
        // `query_plan_uses_the_hash_index` fails if this stops holding.
        let sql = format!(
            "SELECT content_hash, count(*) FROM files INDEXED BY idx_files_hash
             WHERE content_hash IN ({placeholders}) AND status='active'
             GROUP BY content_hash HAVING count(*) > 1"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let params: Vec<&dyn rusqlite::types::ToSql> = uniq
            .iter()
            .map(|h| *h as &dyn rusqlite::types::ToSql)
            .collect();
        let rows = stmt.query_map(params.as_slice(), |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
        })?;
        for row in rows {
            let (h, n) = row?;
            out.insert(h, n);
        }
        Ok(out)
    }

    /// Recompute the stored per-volume totals.
    ///
    /// Called at the same points that rebuild `directory_trees` -- after a completed scan, after
    /// quarantine, after purge -- because those are exactly when the totals change. Derived data:
    /// safe to drop, and a rescan always corrects it. Nothing destructive reads it, so a stale
    /// total is a display problem and never a safety one.
    pub fn refresh_volume_totals(&self, volume_id: &str) -> anyhow::Result<()> {
        self.conn.execute(
            "UPDATE volumes SET
                 active_files = (SELECT COUNT(*) FROM files
                                  WHERE volume_id=?1 AND status='active'),
                 active_bytes = (SELECT IFNULL(SUM(size_bytes),0) FROM files
                                  WHERE volume_id=?1 AND status='active')
               WHERE volume_id=?1",
            params![volume_id],
        )?;
        Ok(())
    }

    pub fn volume_stats(&self) -> anyhow::Result<Vec<(String, String, i64, i64)>> {
        // Stored totals when they exist. On the real catalogue the live aggregate below takes ~3 s
        // because it walks 3.1M rows for two numbers that change only when a scan runs; reading
        // them back is a two-row lookup. A volume whose totals were never computed (an existing
        // catalogue, before its next scan) falls through to the live query, so this is never wrong,
        // only sometimes slow.
        let mut pre = self.conn.prepare(
            "SELECT volume_id, COALESCE(NULLIF(display_name,''), label), active_files, active_bytes
               FROM volumes ORDER BY label",
        )?;
        let rows: Vec<(String, String, Option<i64>, Option<i64>)> = pre
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
            .collect::<Result<Vec<_>, _>>()?;
        if !rows.is_empty() && rows.iter().all(|(_, _, f, b)| f.is_some() && b.is_some()) {
            return Ok(rows
                .into_iter()
                .map(|(id, label, f, b)| (id, label, f.unwrap_or(0), b.unwrap_or(0)))
                .collect());
        }

        let mut stmt = self.conn.prepare(
            "SELECT v.volume_id, v.label,
                    count(f.id) FILTER (WHERE f.status='active'),
                    IFNULL(sum(f.size_bytes) FILTER (WHERE f.status='active'),0)
             FROM volumes v LEFT JOIN files f ON f.volume_id=v.volume_id
             GROUP BY v.volume_id, v.label ORDER BY v.label",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, i64>(3)?,
            ))
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Back-compat wrapper: text + category/volume/status filters, limit 1000.
    pub fn search(
        &self,
        query: &str,
        category: Option<&str>,
        volume: Option<&str>,
        status: Option<&str>,
    ) -> anyhow::Result<Vec<FileRecord>> {
        let f = SearchFilters {
            query: query.to_string(),
            category: category.map(str::to_string).into_iter().collect(),
            volume: volume.map(str::to_string).into_iter().collect(),
            status: status.map(str::to_string).into_iter().collect(),
            ..Default::default()
        };
        self.search_filtered(&f, 1000)
    }

    /// Build ` AND <col> IN (?,?,…)` for a multi-value filter, pushing each value as an arg.
    fn push_in_clause(
        sql: &mut String,
        args: &mut Vec<Box<dyn rusqlite::types::ToSql>>,
        col: &str,
        values: &[String],
    ) {
        if values.is_empty() {
            return;
        }
        let holders = std::iter::repeat_n("?", values.len())
            .collect::<Vec<_>>()
            .join(",");
        sql.push_str(&format!(" AND {col} IN ({holders})"));
        for v in values {
            args.push(Box::new(v.clone()));
        }
    }

    /// Full filtered search over the catalog.
    pub fn search_filtered(
        &self,
        f: &SearchFilters,
        limit: usize,
    ) -> anyhow::Result<Vec<FileRecord>> {
        let mut sql = format!("SELECT {FILE_COLUMNS} FROM files WHERE 1=1");
        let mut args: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
        let q = f.query.trim();
        if !q.is_empty() {
            sql.push_str(" AND id IN (SELECT rowid FROM files_fts WHERE files_fts MATCH ?)");
            // FTS prefix match on each token; quote as a literal string so punctuation
            // (", (, -, :) in the query can't be parsed as FTS5 query syntax.
            let match_expr = q
                .split_whitespace()
                .map(|t| format!("\"{}\"*", t.replace('"', "\"\"")))
                .collect::<Vec<_>>()
                .join(" ");
            args.push(Box::new(match_expr));
        }
        Self::push_in_clause(&mut sql, &mut args, "category", &f.category);
        Self::push_in_clause(&mut sql, &mut args, "volume_id", &f.volume);
        // Purged rows are a permanently-deleted audit record — the file (and its `_ToDelete` folder)
        // is gone from disk, so hide them from the default browse/search. They remain reachable only
        // by explicitly including status = 'purged' in the filter.
        if f.status.is_empty() {
            sql.push_str(" AND status != 'purged'");
        } else {
            Self::push_in_clause(&mut sql, &mut args, "status", &f.status);
        }
        if let Some(n) = f.min_size {
            sql.push_str(" AND size_bytes >= ?");
            args.push(Box::new(n));
        }
        if let Some(n) = f.max_size {
            sql.push_str(" AND size_bytes <= ?");
            args.push(Box::new(n));
        }
        if let Some(n) = f.modified_after {
            sql.push_str(" AND modified_time >= ?");
            args.push(Box::new(n));
        }
        if let Some(n) = f.modified_before {
            sql.push_str(" AND modified_time <= ?");
            args.push(Box::new(n));
        }
        sql.push_str(" ORDER BY relative_path LIMIT ?");
        args.push(Box::new(limit as i64));

        let mut stmt = self.conn.prepare(&sql)?;
        let arg_refs: Vec<&dyn rusqlite::types::ToSql> = args.iter().map(|b| b.as_ref()).collect();
        let rows = stmt.query_map(arg_refs.as_slice(), Self::map_file_record)?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// The subdirectories sitting directly inside `parent` on `volume_id`, largest first.
    ///
    /// Totals come from `directory_trees`, so each one covers the whole subtree beneath that folder
    /// — not just the rows a caller happens to have loaded. Browse used to infer both the folder
    /// structure and its sizes from a fixed-size slice of a path-ordered search, which made every
    /// size a partial sum and hid whole drives that had nothing in the slice.
    ///
    /// Matching is an equality test on the indexed generated column `parent_path`, not a `LIKE` or
    /// a prefix range: `%` and `_` are legal filename characters, and scanning a prefix range costs
    /// time proportional to the whole subtree rather than to the level being opened.
    pub fn folder_children(
        &self,
        volume_id: &str,
        parent: &str,
        limit: usize,
        offset: usize,
    ) -> anyhow::Result<Vec<FolderChild>> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT path, file_count, total_bytes FROM directory_trees
              WHERE volume_id=?1 AND parent_path=?2 AND path <> ''
              ORDER BY total_bytes DESC, path LIMIT ?3 OFFSET ?4",
        )?;
        let rows = stmt.query_map(
            params![volume_id, parent, limit as i64, offset as i64],
            |r| {
                let path: String = r.get(0)?;
                Ok(FolderChild {
                    name: path.rsplit('/').next().unwrap_or(&path).to_string(),
                    path,
                    file_count: r.get(1)?,
                    total_bytes: r.get(2)?,
                })
            },
        )?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// The files sitting directly inside `parent` on `volume_id`, hiding purged rows.
    ///
    /// `display_parent` already accounts for quarantine: a quarantined row's `relative_path` points
    /// into `_ToDelete`, so listing by that would grow a `_ToDelete` branch in the tree and lose the
    /// file from the folder the user is actually looking at. Archive entries keep their archive's
    /// `relative_path`, so they arrive in the right folder and the caller nests them under it.
    pub fn folder_files(
        &self,
        volume_id: &str,
        parent: &str,
        limit: usize,
        offset: usize,
    ) -> anyhow::Result<Vec<FileRecord>> {
        let sql = format!(
            "SELECT {FILE_COLUMNS} FROM files
              WHERE volume_id=?1 AND display_parent=?2 AND status <> 'purged'
              ORDER BY relative_path LIMIT ?3 OFFSET ?4"
        );
        let mut stmt = self.conn.prepare_cached(&sql)?;
        let rows = stmt.query_map(
            params![volume_id, parent, limit as i64, offset as i64],
            Self::map_file_record,
        )?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Count rows per status for the given text/category/volume context (status itself is not
    /// filtered). Lets the UI flag which kinds — active/missing/quarantined/purged — are present,
    /// including purged rows that the default search hides.
    pub fn status_counts(
        &self,
        query: &str,
        category: &[String],
        volume: &[String],
    ) -> anyhow::Result<std::collections::HashMap<String, i64>> {
        let mut sql = String::from("SELECT status, count(*) FROM files WHERE 1=1");
        let mut args: Vec<Box<dyn rusqlite::types::ToSql>> = Vec::new();
        let q = query.trim();
        if !q.is_empty() {
            sql.push_str(" AND id IN (SELECT rowid FROM files_fts WHERE files_fts MATCH ?)");
            let match_expr = q
                .split_whitespace()
                .map(|t| format!("\"{}\"*", t.replace('"', "\"\"")))
                .collect::<Vec<_>>()
                .join(" ");
            args.push(Box::new(match_expr));
        }
        Self::push_in_clause(&mut sql, &mut args, "category", category);
        Self::push_in_clause(&mut sql, &mut args, "volume_id", volume);
        sql.push_str(" GROUP BY status");

        let mut stmt = self.conn.prepare(&sql)?;
        let arg_refs: Vec<&dyn rusqlite::types::ToSql> = args.iter().map(|b| b.as_ref()).collect();
        let rows = stmt.query_map(arg_refs.as_slice(), |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?))
        })?;
        Ok(rows.collect::<Result<std::collections::HashMap<_, _>, _>>()?)
    }

    /// Id of the loose file (container_chain IS NULL) at this path, if catalogued, regardless of
    /// status. Exactly one such row can exist per (volume, path) — the loose-identity partial
    /// unique index guarantees it.
    pub fn loose_file_id(
        &self,
        volume_id: &str,
        relative_path: &str,
    ) -> anyhow::Result<Option<i64>> {
        let row = self.conn.query_row(
            "SELECT id FROM files WHERE volume_id=?1 AND relative_path=?2 AND container_chain IS NULL",
            params![volume_id, relative_path],
            |r| r.get::<_, i64>(0),
        );
        match row {
            Ok(id) => Ok(Some(id)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// Fetch a single file record by id.
    pub fn get_file(&self, id: i64) -> anyhow::Result<Option<FileRecord>> {
        let row = self.conn.query_row(
            &format!("SELECT {FILE_COLUMNS} FROM files WHERE id=?1"),
            params![id],
            Self::map_file_record,
        );
        match row {
            Ok(r) => Ok(Some(r)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    /// All currently-active rows sharing this content hash (loose or archived).
    pub fn active_copies(&self, hash: &str) -> anyhow::Result<Vec<FileRecord>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {FILE_COLUMNS} FROM files
             WHERE content_hash=?1 AND status='active' ORDER BY id"
        ))?;
        let rows = stmt
            .query_map(rusqlite::params![hash], Self::map_file_record)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// True if any loose row (any status) on this volume already uses this relative_path.
    pub fn loose_path_taken(&self, volume_id: &str, relative_path: &str) -> anyhow::Result<bool> {
        let n: i64 = self.conn.query_row(
            "SELECT count(*) FROM files WHERE volume_id=?1 AND relative_path=?2 AND container_chain IS NULL",
            rusqlite::params![volume_id, relative_path], |r| r.get(0))?;
        Ok(n > 0)
    }

    /// Move a file into quarantine: records where it moved to and where it came from.
    /// Also clears container_chain, so an extracted archive entry becomes a proper
    /// loose quarantined row (a no-op for files that were already loose).
    pub fn mark_quarantined(
        &self,
        id: i64,
        new_relative_path: &str,
        original_path: &str,
        now: i64,
    ) -> anyhow::Result<()> {
        self.conn.execute(
            "UPDATE files SET status='quarantined', relative_path=?2, original_path=?3,
                 container_chain=NULL, last_seen_at=?4 WHERE id=?1",
            params![id, new_relative_path, original_path, now],
        )?;
        Ok(())
    }

    /// Drop rows for paths the scanner no longer visits because they belong to the operating
    /// system (`$RECYCLE.BIN`, `System Volume Information`).
    ///
    /// Deleted rather than swept to `missing`, which is what would otherwise happen once the
    /// scanner stops walking them. "Missing" means *your file is not where the catalogue says it
    /// is* -- an alarm. These are files Windows itself already deleted, and 77,493 of them raising
    /// that alarm would be noise that trains the user to ignore the real ones.
    ///
    /// Safe to delete: nothing on disk is touched, and the rows describe data the tool should never
    /// have catalogued. A rescan re-adds them only if this rule changes.
    pub fn forget_system_paths(&self, volume_id: &str) -> anyhow::Result<usize> {
        let mut removed = 0usize;
        for dir in ["$RECYCLE.BIN", "System Volume Information"] {
            // No ESCAPE clause: neither name contains a LIKE metacharacter (`%` or `_`), and the
            // first attempt here wrote `ESCAPE '\'` in a Rust string, where `\'` is an escaped
            // quote -- so SQLite received `ESCAPE ''`, errored, and the failure was swallowed by a
            // best-effort caller. Anything added to this list must be checked for `%` and `_`.
            debug_assert!(
                !dir.contains('%') && !dir.contains('_'),
                "system dir name contains a LIKE metacharacter and needs escaping: {dir}"
            );
            removed += self.conn.execute(
                "DELETE FROM files
                  WHERE volume_id=?1 AND (relative_path=?2 OR relative_path LIKE ?3)",
                params![volume_id, dir, format!("{dir}/%")],
            )?;
        }
        Ok(removed)
    }

    /// A `last_seen_at` stamp for a new scan of this volume: wall-clock time, but never less than
    /// one past anything the volume already carries.
    ///
    /// The missing-file sweep flags rows with `last_seen_at < scan_started_at`. With a raw
    /// second-resolution clock that comparison silently stops working in two ordinary situations
    /// (#45): two scans within the same second make `t < t` false, and a clock that moved backwards
    /// makes `2000 < 1500` false. In both cases a deleted file stays **stale-active** -- the
    /// catalogue asserting a file is present when it is gone, which is the unsafe direction here,
    /// because deduplication can then offer it as the safe copy to keep while the user quarantines
    /// a real one.
    ///
    /// Making the stamp strictly greater restores the comparison without touching `<` itself.
    /// Changing `<` to `<=` would NOT do: it would sweep the files the current scan just touched.
    ///
    /// The stamp stays exactly wall-clock whenever the clock is sane. It only runs ahead when the
    /// clock is the thing that is wrong, and a monotonic "last seen" is the more useful reading of
    /// that column anyway. One consequence worth knowing: a clock that was once set far into the
    /// FUTURE pins every later stamp above that value permanently, because the column it reads can
    /// only go up. That is the price of never letting the sweep silently no-op.
    ///
    /// Cost: this walks every row for the volume, because `idx_files_volume` does not cover
    /// `last_seen_at`. Measured on the live catalogue -- 3.07 s cold for 400,000 rows, 119 ms warm
    /// -- which extrapolates to roughly six minutes at the 50 million rows a 20 TB corpus implies.
    /// It runs once per scan, against a scan measured in days, and is **zero on a fresh catalogue**
    /// because there are no rows to walk. Caching the value per volume would make it O(1) but needs
    /// an invariant this code does not currently have: `update_archive_hash` stamps an ACTIVE row
    /// with wall-clock time outside any scan, so a cached per-volume maximum could fall behind the
    /// rows it is meant to bound. Tracked separately rather than bolted onto a correctness fix.
    pub fn next_seen_stamp(&self, volume_id: &str, now: i64) -> anyhow::Result<i64> {
        let highest: Option<i64> = self.conn.query_row(
            "SELECT MAX(last_seen_at) FROM files WHERE volume_id=?1",
            params![volume_id],
            |r| r.get(0),
        )?;
        Ok(match highest {
            Some(h) if h >= now => h + 1,
            _ => now,
        })
    }

    /// Quarantine a row while KEEPING its `container_chain`.
    ///
    /// `mark_quarantined` clears the chain, which is right when an entry has been extracted into a
    /// real file. It is wrong when a whole archive is moved: the entries are still inside it, and
    /// clearing every chain would collapse them all onto the archive's own relative_path and
    /// violate the loose-identity unique index. Here the archive keeps its shape and only its
    /// location changes.
    pub fn mark_quarantined_in_place(
        &self,
        id: i64,
        new_relative_path: &str,
        original_path: &str,
        now: i64,
    ) -> anyhow::Result<()> {
        self.conn.execute(
            "UPDATE files SET status='quarantined', relative_path=?2, original_path=?3,
                 last_seen_at=?4 WHERE id=?1",
            params![id, new_relative_path, original_path, now],
        )?;
        Ok(())
    }

    /// An archive's currently-catalogued entries (active rows filed under this
    /// relative_path with a non-null container_chain).
    pub fn archive_entries(
        &self,
        volume_id: &str,
        archive_rel_path: &str,
    ) -> anyhow::Result<Vec<FileRecord>> {
        let mut stmt = self.conn.prepare(
            &format!(
                "SELECT {FILE_COLUMNS} FROM files
             WHERE volume_id=?1 AND relative_path=?2 AND container_chain IS NOT NULL AND status='active'
             ORDER BY id"
            ))?;
        let rows = stmt
            .query_map(params![volume_id, archive_rel_path], Self::map_file_record)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Every archive on this volume that still has active catalogued entries, one row per archive
    /// with the entry count and total uncompressed size extracting it would need.
    pub fn archive_roots(&self, volume_id: &str) -> anyhow::Result<Vec<ArchiveRoot>> {
        let mut stmt = self.conn.prepare(
            "SELECT relative_path, COUNT(*), COALESCE(SUM(size_bytes),0) FROM files
             WHERE volume_id=?1 AND container_chain IS NOT NULL AND status='active'
             GROUP BY relative_path ORDER BY SUM(size_bytes) DESC",
        )?;
        let rows = stmt
            .query_map(params![volume_id], |r| {
                Ok(ArchiveRoot {
                    relative_path: r.get(0)?,
                    entries: r.get(1)?,
                    uncompressed_bytes: r.get(2)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Rewrite archived entry rows in place, all or nothing.
    ///
    /// In place, rather than delete-and-reinsert, because `id`, `content_hash` and `first_seen_at`
    /// carry the file's whole history and every other table refers to the id. One transaction,
    /// because a half-applied conversion would leave the catalogue describing a layout that never
    /// existed on disk -- some entries pointing at files extraction actually wrote, others still
    /// claiming to live inside an archive that is about to move into quarantine. The unique index
    /// `idx_files_loose_identity` on `(volume_id, relative_path)` (`container_chain IS NULL`) is
    /// what catches a collision -- e.g. two entries extracting to the same loose path -- and the
    /// rollback on that error is the whole point of doing this as one transaction rather than one
    /// UPDATE per row.
    pub fn convert_archive_entries(&self, moves: &[EntryMove], now: i64) -> anyhow::Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        {
            let mut stmt = tx.prepare(
                "UPDATE files SET relative_path=?2, container_chain=?3, last_seen_at=?4
                 WHERE id=?1",
            )?;
            for m in moves {
                stmt.execute(params![m.id, m.relative_path, m.container_chain, now])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    /// Update a loose archive row's hash/size after a rebuild (repack).
    pub fn update_archive_hash(
        &self,
        id: i64,
        content_hash: &str,
        size_bytes: i64,
        now: i64,
    ) -> anyhow::Result<()> {
        self.conn.execute(
            "UPDATE files SET content_hash=?2, size_bytes=?3, last_seen_at=?4 WHERE id=?1",
            params![id, content_hash, size_bytes, now],
        )?;
        Ok(())
    }

    /// All quarantined rows for a volume, ordered by id.
    pub fn quarantined_rows(&self, volume_id: &str) -> anyhow::Result<Vec<FileRecord>> {
        let mut stmt = self.conn.prepare(&format!(
            "SELECT {FILE_COLUMNS} FROM files
             WHERE volume_id=?1 AND status='quarantined' ORDER BY id"
        ))?;
        let rows = stmt
            .query_map(params![volume_id], Self::map_file_record)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Mark a quarantined file as permanently purged.
    pub fn mark_purged(&self, id: i64, now: i64) -> anyhow::Result<()> {
        self.conn.execute(
            "UPDATE files SET status='purged', last_seen_at=?2 WHERE id=?1",
            params![id, now],
        )?;
        Ok(())
    }

    /// Total bytes that would be reclaimed by purging this volume's quarantined files.
    pub fn recoverable_bytes(&self, volume_id: &str) -> anyhow::Result<i64> {
        let n = self.conn.query_row(
            "SELECT IFNULL(sum(size_bytes),0) FROM files WHERE volume_id=?1 AND status='quarantined'",
            params![volume_id], |r| r.get(0))?;
        Ok(n)
    }

    /// Remove ALL catalog knowledge of a volume: its file rows (FTS cleaned up by triggers) and
    /// its `volumes` row. Never touches files on disk — a later rescan fully rebuilds the volume.
    /// Returns the number of file rows removed, and logs a `forget` audit action.
    ///
    /// All of it — the three deletes and the audit row — runs in one transaction: any error before
    /// commit rolls everything back (the `Transaction` guard rolls back on drop), so a mid-delete
    /// failure can never leave a half-forgotten volume or a delete without its audit entry.
    pub fn forget_volume(&self, volume_id: &str, now: i64) -> anyhow::Result<usize> {
        let label: String = self
            .conn
            .query_row(
                "SELECT label FROM volumes WHERE volume_id=?1",
                params![volume_id],
                |r| r.get(0),
            )
            .unwrap_or_else(|_| volume_id.to_string());
        let tx = self.conn.unchecked_transaction()?;
        let removed = self
            .conn
            .execute("DELETE FROM files WHERE volume_id=?1", params![volume_id])?;
        self.conn.execute(
            "DELETE FROM scan_errors WHERE volume_id=?1",
            params![volume_id],
        )?;
        self.conn.execute(
            "DELETE FROM pending_archive_formats WHERE volume_id=?1",
            params![volume_id],
        )?;
        // Not merely tidiness: directory_trees carries a foreign key to volumes, so with
        // foreign_keys=ON the DELETE below would FAIL outright while these rows existed.
        self.conn.execute(
            "DELETE FROM directory_trees WHERE volume_id=?1",
            params![volume_id],
        )?;
        self.conn
            .execute("DELETE FROM volumes WHERE volume_id=?1", params![volume_id])?;
        self.log_action(
            "forget",
            &serde_json::json!({
            "volume_id": volume_id, "label": label, "removed_files": removed })
            .to_string(),
            now,
        )?;
        tx.commit()?;
        Ok(removed)
    }

    /// Record the absolute path a volume was last scanned at (so a folder-drive can be recognized
    /// as connected later even though it isn't a disk mount root).
    pub fn set_volume_path(&self, volume_id: &str, path: &str, now: i64) -> anyhow::Result<()> {
        self.conn.execute(
            "UPDATE volumes SET last_scanned_path=?2, last_seen_at=?3 WHERE volume_id=?1",
            params![volume_id, path, now],
        )?;
        Ok(())
    }

    /// (volume_id, last_scanned_path) for every volume that has a remembered path.
    pub fn volume_paths(&self) -> anyhow::Result<Vec<(String, String)>> {
        let mut stmt = self.conn.prepare(
            "SELECT volume_id, last_scanned_path FROM volumes WHERE last_scanned_path IS NOT NULL",
        )?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    /// Set a volume's user display name and/or description. Each field updates independently:
    /// `None` leaves that column unchanged (partial update), `Some(s)` sets it — trimmed, with an
    /// empty-after-trim value clearing to NULL (which falls back to the detected label). Logs a
    /// `rename` audit action.
    pub fn set_volume_meta(
        &self,
        volume_id: &str,
        display_name: Option<&str>,
        description: Option<&str>,
        now: i64,
    ) -> anyhow::Result<()> {
        // None = leave unchanged; Some(s) = set (trim; empty clears to NULL / detected label).
        // A rename does NOT touch last_seen_at — it isn't a scan, and the Drives card renders
        // last_seen_at as "last scan", which must stay truthful.
        let to_val = |s: &str| -> Option<String> {
            let t = s.trim();
            if t.is_empty() {
                None
            } else {
                Some(t.to_string())
            }
        };
        if let Some(dn) = display_name {
            let v = to_val(dn);
            self.conn.execute(
                "UPDATE volumes SET display_name=?2 WHERE volume_id=?1",
                params![volume_id, v],
            )?;
        }
        if let Some(desc) = description {
            let v = to_val(desc);
            self.conn.execute(
                "UPDATE volumes SET description=?2 WHERE volume_id=?1",
                params![volume_id, v],
            )?;
        }
        self.log_action(
            "rename",
            &serde_json::json!({
            "volume_id": volume_id, "display_name": display_name, "description": description })
            .to_string(),
            now,
        )?;
        Ok(())
    }

    /// A volume's (display_name, description); both None if unset or the volume is unknown.
    pub fn volume_meta(&self, volume_id: &str) -> anyhow::Result<(Option<String>, Option<String>)> {
        let row = self.conn.query_row(
            "SELECT display_name, description FROM volumes WHERE volume_id=?1",
            params![volume_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        );
        match row {
            Ok(t) => Ok(t),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok((None, None)),
            Err(e) => Err(e.into()),
        }
    }

    /// volume_id → the name to show: the user display_name when set, else the detected label.
    pub fn effective_labels(&self) -> anyhow::Result<std::collections::HashMap<String, String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT volume_id, COALESCE(NULLIF(display_name,''), label) FROM volumes")?;
        let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
        Ok(rows.collect::<Result<std::collections::HashMap<_, _>, _>>()?)
    }

    /// Append an audit entry to actions_log.
    pub fn log_action(&self, action: &str, details_json: &str, now: i64) -> anyhow::Result<()> {
        self.conn.execute(
            "INSERT INTO actions_log(action, details, occurred_at) VALUES (?1,?2,?3)",
            params![action, details_json, now],
        )?;
        Ok(())
    }

    /// The most recent `limit` audit entries, newest first: (action, details_json, occurred_at).
    pub fn recent_actions(&self, limit: usize) -> anyhow::Result<Vec<(String, String, i64)>> {
        let mut stmt = self.conn.prepare(
            "SELECT action, IFNULL(details,''), occurred_at FROM actions_log
             ORDER BY occurred_at DESC, id DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit as i64], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
            ))
        })?;
        Ok(rows.collect::<Result<Vec<_>, _>>()?)
    }

    pub(crate) fn map_file_record(r: &rusqlite::Row) -> rusqlite::Result<FileRecord> {
        Ok(FileRecord {
            id: r.get("id")?,
            volume_id: r.get("volume_id")?,
            relative_path: r.get("relative_path")?,
            filename: r.get("filename")?,
            extension: r.get("extension")?,
            size_bytes: r.get("size_bytes")?,
            content_hash: r.get("content_hash")?,
            created_time: r.get("created_time")?,
            modified_time: r.get("modified_time")?,
            accessed_time: r.get("accessed_time")?,
            category: Category::from_db(&r.get::<_, String>("category")?),
            container_chain: r.get("container_chain")?,
            status: FileStatus::from_db(&r.get::<_, String>("status")?),
            first_seen_at: r.get("first_seen_at")?,
            last_seen_at: r.get("last_seen_at")?,
            original_path: r.get("original_path")?,
        })
    }

    /// Recompute this volume's directory hashes from its active rows.
    ///
    /// Derived data: dropped and rebuilt wholesale, never migrated. Cheap because every content
    /// hash is already stored -- this reads rows and sorts, it does not touch the drive, so it
    /// works for a volume that is not currently plugged in.
    pub fn rebuild_directory_trees(&self, volume_id: &str, now: i64) -> anyhow::Result<usize> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "DELETE FROM directory_trees WHERE volume_id=?1",
            params![volume_id],
        )?;

        let emitted = {
            // ORDER BY is load-bearing, not cosmetic: the fold closes a directory as soon as the
            // walk leaves it, so unordered rows would hash a fragment of a directory. Default
            // BINARY collation -- NOCASE would break the contiguity the fold relies on.
            // `stream_dir_hashes` re-checks the order per row rather than trusting this.
            let mut stmt = self.conn.prepare(
                "SELECT CASE WHEN container_chain IS NULL THEN relative_path
                             ELSE relative_path||'/'||container_chain END AS p,
                        container_chain IS NULL, content_hash, size_bytes
                   FROM files WHERE volume_id=?1 AND status='active'
                  ORDER BY p",
            )?;
            let rows = stmt.query_map(params![volume_id], |r| {
                Ok(crate::tree_hash::TreeInput {
                    path: r.get(0)?,
                    // A loose row is only a *candidate* archive root; the fold promotes it to a
                    // directory only when the very next row turns out to sit inside it.
                    is_archive_root: r.get::<_, i64>(1)? != 0,
                    content_hash: r.get(2)?,
                    size_bytes: r.get(3)?,
                })
            })?;

            // Nodes go straight to the database as they are finalised. Collecting them first is
            // what made this O(corpus) instead of O(tree depth).
            let mut sink = InsertSink {
                stmt: tx.prepare(
                    "INSERT INTO directory_trees(volume_id, path, dir_hash, file_count,
                                                 total_bytes, archive_root, computed_at)
                     VALUES (?1,?2,?3,?4,?5,?6,?7)",
                )?,
                now,
            };
            crate::tree_hash::stream_dir_hashes(
                volume_id,
                rows.map(|r| r.map_err(anyhow::Error::from)),
                &mut sink,
            )?
        };

        tx.commit()?;
        Ok(emitted)
    }

    /// Writes each finalised directory node straight into `directory_trees`.
    ///
    /// Exists so a rebuild never holds a collection of nodes: on a 20 TB corpus that collection was
    /// the whole memory problem.
    /// Maximal identical-tree groups across every volume, ranked by reclaimable bytes.
    pub fn tree_duplicate_groups(&self) -> anyhow::Result<Vec<crate::tree_hash::TreeGroup>> {
        let mut stmt = self.conn.prepare(
            "SELECT volume_id, path, dir_hash, file_count, total_bytes, archive_root
               FROM directory_trees
              WHERE dir_hash IN (SELECT dir_hash FROM directory_trees
                                  GROUP BY dir_hash HAVING COUNT(*)>1)",
        )?;
        let nodes = stmt
            .query_map([], |r| {
                Ok(crate::tree_hash::DirNode {
                    volume_id: r.get(0)?,
                    path: r.get(1)?,
                    dir_hash: r.get(2)?,
                    file_count: r.get(3)?,
                    total_bytes: r.get(4)?,
                    archive_root: r.get(5)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(crate::tree_hash::maximal_groups(&nodes))
    }
}

/// Writes each finalised directory node straight into `directory_trees`.
///
/// Exists so a rebuild never holds a collection of nodes: on a 20 TB corpus that collection WAS the
/// memory problem, at roughly 198 bytes per catalogued file.
struct InsertSink<'a> {
    stmt: rusqlite::Statement<'a>,
    now: i64,
}

impl crate::tree_hash::DirSink for InsertSink<'_> {
    fn emit(&mut self, n: crate::tree_hash::DirNode) -> anyhow::Result<()> {
        self.stmt.execute(params![
            n.volume_id,
            n.path,
            n.dir_hash,
            n.file_count,
            n.total_bytes,
            n.archive_root,
            self.now
        ])?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use crate::archive::ArchiveEntry;
    use crate::catalog::models::*;
    use crate::catalog::store::{EntryMove, SearchFilters};
    use crate::catalog::Catalog;

    fn mk_entry(chain: &str, hash: &str) -> ArchiveEntry {
        ArchiveEntry {
            container_chain: chain.into(),
            filename: chain.rsplit(['/', '›']).next().unwrap().trim().into(),
            extension: "jpg".into(),
            size_bytes: 42,
            content_hash: hash.into(),
        }
    }

    fn mk_file(vol: &str, path: &str, hash: &str) -> NewFile {
        NewFile {
            volume_id: vol.into(),
            relative_path: path.into(),
            filename: path.rsplit('/').next().unwrap().into(),
            extension: "txt".into(),
            size_bytes: 10,
            content_hash: hash.into(),
            created_time: Some(1),
            modified_time: Some(2),
            accessed_time: Some(3),
            category: Category::Document,
            container_chain: None,
        }
    }

    fn open_tmp() -> (tempfile::TempDir, Catalog) {
        let tmp = tempfile::tempdir().unwrap();
        let cat = Catalog::open(&tmp.path().join("c.db")).unwrap();
        cat.upsert_volume(&Volume {
            volume_id: "vol-1".into(),
            label: "Test HDD".into(),
            identified_by: "marker".into(),
            first_seen_at: 100,
            last_seen_at: 100,
        })
        .unwrap();
        (tmp, cat)
    }

    /// Enriching a result list must cost a few hash lookups, not a walk of every active row.
    ///
    /// Left to itself the planner chose `idx_files_dedup` on `status=?` and scanned 2.6M rows:
    /// 1,271 ms against 1 ms for the hash lookups, on every search and every folder opened. A
    /// timing assertion would be flaky, so this pins the plan instead.
    #[test]
    fn query_plan_uses_the_hash_index() {
        let (_t, cat) = open_tmp();
        cat.upsert_file(&mk_file("vol-1", "a.txt", "h1"), 200)
            .unwrap();
        let plan: Vec<String> = cat
            .conn
            .prepare(
                "EXPLAIN QUERY PLAN SELECT content_hash, count(*) FROM files
                 INDEXED BY idx_files_hash WHERE content_hash IN (?1) AND status='active'
                 GROUP BY content_hash HAVING count(*) > 1",
            )
            .unwrap()
            .query_map(["h1"], |r| r.get::<_, String>(3))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert!(
            plan.iter().any(|s| s.contains("idx_files_hash")),
            "duplicate_counts must resolve by content_hash, got {plan:?}"
        );
    }

    /// A folder's size must describe the folder, not whatever rows the caller loaded.
    ///
    /// The bug this replaces: Browse summed `size_bytes` over a fixed slice of a path-ordered
    /// search, so `Uni Small` was shown as 114.6 GB -- exactly the sum of the 3,000 rows it had
    /// fetched -- against a real 1.87 TB.
    #[test]
    fn folder_children_report_whole_subtree_totals() {
        let (_t, cat) = open_tmp();
        for i in 0..40 {
            let mut f = mk_file("vol-1", &format!("photos/{i}/img.jpg"), &format!("h{i}"));
            f.size_bytes = 1000;
            cat.upsert_file(&f, 200).unwrap();
        }
        cat.rebuild_directory_trees("vol-1", 300).unwrap();

        let top = cat.folder_children("vol-1", "", 10, 0).unwrap();
        assert_eq!(top.len(), 1, "one top-level folder");
        assert_eq!(top[0].name, "photos");
        assert_eq!(top[0].file_count, 40);
        assert_eq!(
            top[0].total_bytes, 40_000,
            "must total the whole subtree, not the direct children"
        );
    }

    /// Only the folders one level down, however many levels are stored beneath them.
    #[test]
    fn folder_children_are_direct_children_only() {
        let (_t, cat) = open_tmp();
        for p in ["a/b/c/deep.txt", "a/sibling.txt", "top.txt"] {
            cat.upsert_file(&mk_file("vol-1", p, p), 200).unwrap();
        }
        cat.rebuild_directory_trees("vol-1", 300).unwrap();

        let top = cat.folder_children("vol-1", "", 10, 0).unwrap();
        assert_eq!(top.iter().map(|c| &c.name).collect::<Vec<_>>(), ["a"]);
        let under_a = cat.folder_children("vol-1", "a", 10, 0).unwrap();
        assert_eq!(under_a.iter().map(|c| &c.name).collect::<Vec<_>>(), ["b"]);
        assert_eq!(under_a[0].path, "a/b", "children carry their full path");
    }

    /// `%` and `_` are ordinary filename characters. Matching by `LIKE` without escaping would let
    /// one folder's listing pull in a sibling's files.
    #[test]
    fn folder_listing_does_not_treat_filename_wildcards_as_patterns() {
        let (_t, cat) = open_tmp();
        for p in ["100%_done/kept.txt", "1008_done/other.txt"] {
            cat.upsert_file(&mk_file("vol-1", p, p), 200).unwrap();
        }
        cat.rebuild_directory_trees("vol-1", 300).unwrap();

        let files = cat.folder_files("vol-1", "100%_done", 10, 0).unwrap();
        assert_eq!(
            files.iter().map(|f| f.filename.clone()).collect::<Vec<_>>(),
            ["kept.txt"],
            "`100%_done` must not match `1008_done`"
        );
    }

    /// A quarantined row lives under `_ToDelete` on disk but belongs, on screen, to the folder it
    /// was taken from -- otherwise the tree grows a `_ToDelete` branch and the file disappears from
    /// where the user is looking for it.
    #[test]
    fn folder_files_show_quarantined_rows_at_their_original_location() {
        let (_t, cat) = open_tmp();
        cat.upsert_file(&mk_file("vol-1", "docs/report.txt", "h1"), 200)
            .unwrap();
        let id = cat.search("report", None, None, None).unwrap()[0].id;
        cat.mark_quarantined(id, "_ToDelete/docs/report.txt", "docs/report.txt", 300)
            .unwrap();

        let in_docs = cat.folder_files("vol-1", "docs", 10, 0).unwrap();
        assert_eq!(in_docs.len(), 1, "still listed where it came from");
        assert_eq!(in_docs[0].status, FileStatus::Quarantined);
        let in_quarantine = cat.folder_files("vol-1", "_ToDelete", 10, 0).unwrap();
        assert!(
            in_quarantine.is_empty(),
            "must not also appear under _ToDelete"
        );
    }

    /// Purged rows are a permanently-deleted audit record; the file is gone from disk.
    #[test]
    fn folder_files_hide_purged_rows() {
        let (_t, cat) = open_tmp();
        cat.upsert_file(&mk_file("vol-1", "docs/gone.txt", "h1"), 200)
            .unwrap();
        let id = cat.search("gone", None, None, None).unwrap()[0].id;
        cat.mark_quarantined(id, "_ToDelete/docs/gone.txt", "docs/gone.txt", 300)
            .unwrap();
        cat.conn
            .execute("UPDATE files SET status='purged' WHERE id=?1", [id])
            .unwrap();
        assert!(cat.folder_files("vol-1", "docs", 10, 0).unwrap().is_empty());
    }

    #[test]
    fn volume_last_seen() {
        let (_t, cat) = open_tmp();
        cat.upsert_volume(&crate::catalog::models::Volume {
            volume_id: "v".into(),
            label: "V".into(),
            identified_by: "marker".into(),
            first_seen_at: 5,
            last_seen_at: 42,
        })
        .unwrap();
        assert_eq!(cat.volume_last_seen("v").unwrap(), Some(42));
        assert_eq!(cat.volume_last_seen("nope").unwrap(), None);
    }

    #[test]
    fn upsert_is_idempotent_and_search_finds_it() {
        let (_t, cat) = open_tmp();
        cat.upsert_file(&mk_file("vol-1", "docs/thesis.txt", "hashA"), 200)
            .unwrap();
        cat.upsert_file(&mk_file("vol-1", "docs/thesis.txt", "hashA"), 250)
            .unwrap(); // same path again
        let hits = cat.search("thesis", None, None, None).unwrap();
        assert_eq!(hits.len(), 1); // one row, not two
        assert_eq!(hits[0].relative_path, "docs/thesis.txt");
    }

    #[test]
    fn duplicate_groups_counted_by_hash() {
        let (_t, cat) = open_tmp();
        cat.upsert_file(&mk_file("vol-1", "a.txt", "same"), 200)
            .unwrap();
        cat.upsert_file(&mk_file("vol-1", "b.txt", "same"), 200)
            .unwrap();
        cat.upsert_file(&mk_file("vol-1", "c.txt", "unique"), 200)
            .unwrap();
        assert_eq!(cat.duplicate_totals(0).unwrap().groups_all, 1);
    }

    #[test]
    fn duplicate_totals_ignore_all_missing_groups() {
        let (_t, cat) = open_tmp();
        cat.upsert_volume(&crate::catalog::models::Volume {
            volume_id: "v".into(),
            label: "V".into(),
            identified_by: "marker".into(),
            first_seen_at: 1,
            last_seen_at: 1,
        })
        .unwrap();
        // Two files sharing a hash, both marked missing -> not a reviewable group.
        let mut f = crate::catalog::models::NewFile {
            volume_id: "v".into(),
            relative_path: "a".into(),
            filename: "a".into(),
            extension: "".into(),
            size_bytes: 1,
            content_hash: "dup".into(),
            created_time: None,
            modified_time: None,
            accessed_time: None,
            category: crate::category::Category::Other,
            container_chain: None,
        };
        cat.upsert_file(&f, 1).unwrap();
        f.relative_path = "b".into();
        f.filename = "b".into();
        cat.upsert_file(&f, 1).unwrap();
        // Both rows have last_seen_at=1; a scan starting at 300 sweeps anything not seen this pass
        // (last_seen_at < 300) to missing. Signature: mark_missing_scanned(volume_id, scan_started_at, now).
        cat.mark_missing_scanned("v", 300, 300, &[]).unwrap();
        // active-only: no reviewable groups
        assert_eq!(cat.duplicate_totals(0).unwrap().groups_all, 0);
    }

    #[test]
    fn duplicate_counts_reports_only_multi_active_hashes() {
        let (_t, cat) = open_tmp();
        cat.upsert_volume(&crate::catalog::models::Volume {
            volume_id: "v".into(),
            label: "V".into(),
            identified_by: "marker".into(),
            first_seen_at: 1,
            last_seen_at: 1,
        })
        .unwrap();
        let mut f = crate::catalog::models::NewFile {
            volume_id: "v".into(),
            relative_path: "a".into(),
            filename: "a".into(),
            extension: "".into(),
            size_bytes: 1,
            content_hash: "dup".into(),
            created_time: None,
            modified_time: None,
            accessed_time: None,
            category: crate::category::Category::Other,
            container_chain: None,
        };
        cat.upsert_file(&f, 1).unwrap(); // dup copy 1
        f.relative_path = "b".into();
        f.filename = "b".into();
        cat.upsert_file(&f, 1).unwrap(); // dup copy 2
        f.relative_path = "u".into();
        f.filename = "u".into();
        f.content_hash = "uniq".into();
        cat.upsert_file(&f, 1).unwrap(); // unique
        let m = cat
            .duplicate_counts(&["dup".to_string(), "uniq".to_string(), "absent".to_string()])
            .unwrap();
        assert_eq!(m.get("dup").copied(), Some(2));
        assert_eq!(m.get("uniq"), None); // single copy -> not duplicated
        assert_eq!(m.get("absent"), None); // not in catalog
    }

    #[test]
    fn mark_missing_flags_files_not_seen_this_scan() {
        let (_t, cat) = open_tmp();
        // seen in an earlier scan at t=200
        cat.upsert_file(&mk_file("vol-1", "gone.txt", "h1"), 200)
            .unwrap();
        cat.upsert_file(&mk_file("vol-1", "kept.txt", "h2"), 200)
            .unwrap();
        // new scan starts at t=300; only kept.txt is re-seen
        cat.upsert_file(&mk_file("vol-1", "kept.txt", "h2"), 300)
            .unwrap();
        let n = cat.mark_missing_scanned("vol-1", 300, 300, &[]).unwrap();
        assert_eq!(n, 1);
        let missing = cat.search("gone", None, None, Some("missing")).unwrap();
        assert_eq!(missing.len(), 1);
    }

    #[test]
    fn an_unreadable_directory_shields_its_files_from_the_missing_sweep() {
        // #7: a directory that fails to enumerate means its files are never re-seen, so the sweep
        // would call them `missing` — telling the user their files are gone when they are merely
        // unreadable this pass.
        let (_t, cat) = open_tmp();
        cat.upsert_file(&mk_file("vol-1", "locked/a.txt", "h1"), 200)
            .unwrap();
        cat.upsert_file(&mk_file("vol-1", "locked/deep/b.txt", "h2"), 200)
            .unwrap();
        cat.upsert_file(&mk_file("vol-1", "elsewhere/c.txt", "h3"), 200)
            .unwrap();

        // A later scan enumerated nothing: "locked" failed, and c.txt really was deleted.
        let n = cat
            .mark_missing_scanned("vol-1", 300, 300, &["locked".to_string()])
            .unwrap();
        assert_eq!(n, 1, "only the genuinely absent file is swept");

        for still_active in ["locked/a.txt", "locked/deep/b.txt"] {
            let s: String = cat
                .conn
                .query_row(
                    "SELECT status FROM files WHERE relative_path=?1",
                    [still_active],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(s, "active", "{still_active} is unreadable, not gone");
        }
        let s: String = cat
            .conn
            .query_row(
                "SELECT status FROM files WHERE relative_path='elsewhere/c.txt'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(s, "missing", "a readable directory still gets swept");
    }

    #[test]
    fn a_percent_in_an_unreadable_path_does_not_shield_unrelated_files() {
        // `%` and `_` are legal in filenames and are LIKE wildcards. Unescaped, a directory named
        // "%" would match everything and silently disable the sweep for the whole volume.
        // A directory literally named "%" is the discriminating case: unescaped it becomes
        // LIKE '%/%', which matches EVERY path containing a slash and would silently disable the
        // sweep for the whole volume. Escaped, it matches only the directory actually named "%".
        let (_t, cat) = open_tmp();
        cat.upsert_file(&mk_file("vol-1", "%/a.txt", "h1"), 200)
            .unwrap();
        cat.upsert_file(&mk_file("vol-1", "unrelated/b.txt", "h2"), 200)
            .unwrap();

        let n = cat
            .mark_missing_scanned("vol-1", 300, 300, &["%".to_string()])
            .unwrap();
        assert_eq!(
            n, 1,
            "the unrelated file must still be swept; an unescaped wildcard would shield it"
        );

        let shielded: String = cat
            .conn
            .query_row(
                "SELECT status FROM files WHERE relative_path='%/a.txt'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(shielded, "active");
        let swept: String = cat
            .conn
            .query_row(
                "SELECT status FROM files WHERE relative_path='unrelated/b.txt'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(swept, "missing");
    }

    #[test]
    fn volume_stats_counts_active_files() {
        let (_t, cat) = open_tmp();
        cat.upsert_file(&mk_file("vol-1", "a.txt", "h1"), 200)
            .unwrap();
        cat.upsert_file(&mk_file("vol-1", "b.txt", "h2"), 200)
            .unwrap();
        let stats = cat.volume_stats().unwrap();
        assert_eq!(stats.len(), 1);
        let (volume_id, label, count, bytes) = &stats[0];
        assert_eq!(volume_id, "vol-1");
        assert_eq!(label, "Test HDD");
        assert_eq!(*count, 2);
        assert_eq!(*bytes, 20); // 2 files * size_bytes 10
    }

    #[test]
    fn archive_entry_upsert_is_idempotent_and_searchable() {
        let (_t, cat) = open_tmp();
        let e = mk_entry("photos.zip › vacation.jpg", "h-vac");
        cat.upsert_archive_entry("vol-1", "backups/old.zip", &e, None, 200)
            .unwrap();
        cat.upsert_archive_entry("vol-1", "backups/old.zip", &e, None, 250)
            .unwrap(); // same identity again
        let hits = cat.search("vacation", None, None, None).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(
            hits[0].container_chain.as_deref(),
            Some("photos.zip › vacation.jpg")
        );
        assert_eq!(hits[0].relative_path, "backups/old.zip");
    }

    #[test]
    fn converting_an_entry_keeps_its_id_hash_and_history() {
        let (_t, cat) = open_tmp();
        cat.upsert_archive_entry("vol-1", "bundle.zip", &mk_entry("a.txt", "h1"), None, 200)
            .unwrap();
        let before = cat
            .archive_entries("vol-1", "bundle.zip")
            .unwrap()
            .remove(0);

        cat.convert_archive_entries(
            &[EntryMove {
                id: before.id,
                relative_path: "bundle/a.txt".into(),
                container_chain: None,
            }],
            999,
        )
        .unwrap();

        let after = cat.get_file(before.id).unwrap().unwrap();
        assert_eq!(after.id, before.id, "id survives");
        assert_eq!(after.content_hash, before.content_hash, "hash survives");
        assert_eq!(
            after.first_seen_at, before.first_seen_at,
            "history survives"
        );
        assert_eq!(after.relative_path, "bundle/a.txt");
        assert!(after.container_chain.is_none(), "now a loose file");
    }

    #[test]
    fn a_nested_entry_is_repointed_at_the_extracted_inner_archive() {
        let (_t, cat) = open_tmp();
        cat.upsert_archive_entry(
            "vol-1",
            "bundle.zip",
            &mk_entry("inner.zip › deep.txt", "h1"),
            None,
            200,
        )
        .unwrap();
        let before = cat
            .archive_entries("vol-1", "bundle.zip")
            .unwrap()
            .remove(0);

        cat.convert_archive_entries(
            &[EntryMove {
                id: before.id,
                relative_path: "bundle/inner.zip".into(),
                container_chain: Some("deep.txt".into()),
            }],
            999,
        )
        .unwrap();

        let after = cat.get_file(before.id).unwrap().unwrap();
        assert_eq!(after.relative_path, "bundle/inner.zip");
        assert_eq!(after.container_chain.as_deref(), Some("deep.txt"));
    }

    #[test]
    fn a_failing_move_rolls_back_every_other_move() {
        let (_t, cat) = open_tmp();
        cat.upsert_file(&mk_file("vol-1", "bundle/a.txt", "h9"), 200)
            .unwrap(); // already occupies the loose path
        cat.upsert_archive_entry("vol-1", "bundle.zip", &mk_entry("a.txt", "h1"), None, 200)
            .unwrap();
        cat.upsert_archive_entry("vol-1", "bundle.zip", &mk_entry("b.txt", "h2"), None, 200)
            .unwrap();
        let rows = cat.archive_entries("vol-1", "bundle.zip").unwrap();

        let err = cat.convert_archive_entries(
            &[
                EntryMove {
                    id: rows[1].id,
                    relative_path: "bundle/b.txt".into(),
                    container_chain: None,
                },
                EntryMove {
                    id: rows[0].id,
                    relative_path: "bundle/a.txt".into(),
                    container_chain: None,
                },
            ],
            999,
        );

        assert!(
            err.is_err(),
            "the unique loose index must reject the collision"
        );
        let b = cat.get_file(rows[1].id).unwrap().unwrap();
        assert_eq!(
            b.container_chain.as_deref(),
            Some("b.txt"),
            "the first move must have been rolled back, not left half-applied"
        );
    }

    #[test]
    fn archive_entries_inherit_the_archive_modified_date() {
        // #10: a zip records per-entry dates but we do not read them; without this an archive entry
        // has NULL modified_time and every date filter drops it. It inherits the archive's date.
        let (_t, cat) = open_tmp();
        cat.upsert_archive_entry(
            "vol-1",
            "photos.zip",
            &mk_entry("holiday.jpg", "h1"),
            Some(1_700_000_000),
            200,
        )
        .unwrap();

        // Found within a window around the archive's date...
        let f = SearchFilters {
            modified_after: Some(1_600_000_000),
            modified_before: Some(1_800_000_000),
            ..Default::default()
        };
        assert_eq!(cat.search_filtered(&f, 100).unwrap().len(), 1);

        // ...and correctly excluded by a window that does not contain it.
        let before = SearchFilters {
            modified_before: Some(1_600_000_000),
            ..Default::default()
        };
        assert!(cat.search_filtered(&before, 100).unwrap().is_empty());

        // A rescan with a newer archive date re-dates the entry (updated on conflict).
        cat.upsert_archive_entry(
            "vol-1",
            "photos.zip",
            &mk_entry("holiday.jpg", "h1"),
            Some(1_900_000_000),
            300,
        )
        .unwrap();
        assert!(cat.search_filtered(&before, 100).unwrap().is_empty());
        let after = SearchFilters {
            modified_after: Some(1_850_000_000),
            ..Default::default()
        };
        assert_eq!(cat.search_filtered(&after, 100).unwrap().len(), 1);
    }

    #[test]
    fn an_intermediate_archive_name_is_searchable() {
        // "photos.zip" appears only in the container chain -- not in the filename, not in the
        // relative path. Before container_chain was indexed this query found nothing, which on a
        // catalog that is mostly archive entries hides most of the corpus from search.
        let (_t, cat) = open_tmp();
        cat.upsert_archive_entry(
            "vol-1",
            "backups/old.zip",
            &mk_entry("photos.zip › vacation.jpg", "h-vac"),
            None,
            200,
        )
        .unwrap();

        let hits = cat.search("photos", None, None, None).unwrap();
        assert_eq!(hits.len(), 1, "found via the intermediate archive name");
        assert_eq!(hits[0].filename, "vacation.jpg");

        // The trigger keeps the index in step with a delete, or the row would linger in search.
        cat.conn
            .execute("DELETE FROM files WHERE filename='vacation.jpg'", [])
            .unwrap();
        assert!(cat.search("photos", None, None, None).unwrap().is_empty());
    }

    #[test]
    fn archive_entry_dedupes_against_loose_file_by_hash() {
        let (_t, cat) = open_tmp();
        cat.upsert_file(&mk_file("vol-1", "loose/vacation.jpg", "same"), 200)
            .unwrap();
        cat.upsert_archive_entry(
            "vol-1",
            "old.zip",
            &mk_entry("vacation.jpg", "same"),
            None,
            200,
        )
        .unwrap();
        // The hash is known to be duplicated across the two rows...
        assert_eq!(
            cat.duplicate_counts(&["same".to_string()]).unwrap()["same"],
            2
        );
        // ...but a loose/archived pair is not reclaimable by quarantine, and one archived copy is
        // not archive-locked duplication either, so neither figure claims space that isn't there.
        let t = cat.duplicate_totals(0).unwrap();
        assert_eq!(t.groups_all, 0);
        assert_eq!(t.reclaimable_all_bytes, 0);
        assert_eq!(t.archive_locked_bytes, 0);
    }

    #[test]
    fn missing_sweep_covers_archive_entries() {
        let (_t, cat) = open_tmp();
        cat.upsert_archive_entry("vol-1", "old.zip", &mk_entry("gone.jpg", "h1"), None, 200)
            .unwrap();
        cat.upsert_archive_entry("vol-1", "old.zip", &mk_entry("kept.jpg", "h2"), None, 200)
            .unwrap();
        // rescan at 300 re-sees only kept.jpg
        cat.upsert_archive_entry("vol-1", "old.zip", &mk_entry("kept.jpg", "h2"), None, 300)
            .unwrap();
        let n = cat.mark_missing_scanned("vol-1", 300, 300, &[]).unwrap();
        assert_eq!(n, 1);
        assert_eq!(
            cat.search("gone", None, None, Some("missing"))
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn touch_archive_entries_refreshes_all_under_archive() {
        let (_t, cat) = open_tmp();
        cat.upsert_archive_entry("vol-1", "old.zip", &mk_entry("a.jpg", "h1"), None, 200)
            .unwrap();
        cat.upsert_archive_entry("vol-1", "old.zip", &mk_entry("b.jpg", "h2"), None, 200)
            .unwrap();
        let touched = cat
            .touch_archive_entries("vol-1", "old.zip", 300, None)
            .unwrap();
        assert_eq!(touched, 2);
        // after touch, a later sweep starting at 300 does NOT mark them missing
        let n = cat.mark_missing_scanned("vol-1", 300, 300, &[]).unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn touch_archive_entries_revives_entries_that_were_marked_missing() {
        // An archive can disappear (its entries swept to 'missing') and later reappear unchanged.
        // The skip path's touch must bring the entries back to 'active' when the caller supplies a
        // revive floor at or below the entry's own last_seen_at (i.e. the entry was still present
        // when the archive itself went missing) -- otherwise they are stuck 'missing' forever,
        // since nothing else ever touches them again.
        let (_t, cat) = open_tmp();
        cat.upsert_archive_entry("vol-1", "old.zip", &mk_entry("a.jpg", "h1"), None, 200)
            .unwrap();
        let id = cat
            .conn
            .query_row(
                "SELECT id FROM files WHERE relative_path='old.zip' AND container_chain IS NOT NULL",
                [],
                |r| r.get::<_, i64>(0),
            )
            .unwrap();
        cat.conn
            .execute("UPDATE files SET status='missing' WHERE id=?1", [id])
            .unwrap();

        // The entry's own last_seen_at is 200 (set by upsert_archive_entry above); a floor of 200
        // means "still present at the moment the archive went missing".
        let touched = cat
            .touch_archive_entries("vol-1", "old.zip", 300, Some(200))
            .unwrap();
        assert_eq!(touched, 1, "a missing entry must be revivable");

        let status: String = cat
            .conn
            .query_row("SELECT status FROM files WHERE id=?1", [id], |r| r.get(0))
            .unwrap();
        assert_eq!(status, "active");
    }

    #[test]
    fn an_entry_at_exactly_the_revive_floor_is_revived() {
        // Direct attack on the boundary comparison itself: an entry whose last_seen_at is EXACTLY
        // equal to the floor (the archive's own last_seen_at at the moment it went missing) is the
        // ordinary "went missing together with the archive" case, and it is what makes `>=` the
        // correct comparison rather than `>`. If the comparison were ever tightened to `>`, this is
        // what would silently break -- every normal whole-archive round-trip would stop reviving.
        let (_t, cat) = open_tmp();
        cat.upsert_archive_entry("vol-1", "old.zip", &mk_entry("a.jpg", "h1"), None, 200)
            .unwrap();
        let id = cat
            .conn
            .query_row(
                "SELECT id FROM files WHERE relative_path='old.zip' AND container_chain IS NOT NULL",
                [],
                |r| r.get::<_, i64>(0),
            )
            .unwrap();
        // Entry's last_seen_at is 200 (from upsert above). Mark it missing without touching
        // last_seen_at, exactly as mark_missing_scanned does.
        cat.conn
            .execute("UPDATE files SET status='missing' WHERE id=?1", [id])
            .unwrap();

        // Floor == 200, exactly equal to the entry's own last_seen_at.
        let touched = cat
            .touch_archive_entries("vol-1", "old.zip", 300, Some(200))
            .unwrap();
        assert_eq!(
            touched, 1,
            "an entry at exactly the floor must revive (>= not >)"
        );

        let status: String = cat
            .conn
            .query_row("SELECT status FROM files WHERE id=?1", [id], |r| r.get(0))
            .unwrap();
        assert_eq!(status, "active");
    }

    #[test]
    fn touch_archive_entries_does_not_revive_a_quarantined_entry() {
        // Quarantine/purge are user decisions about files that were moved or deleted. A scan must
        // never silently flip them back to 'active', even with a permissive revive floor (the
        // whole-archive-came-back case) -- quarantine/purge must win over that.
        let (_t, cat) = open_tmp();
        cat.upsert_archive_entry("vol-1", "old.zip", &mk_entry("a.jpg", "h1"), None, 200)
            .unwrap();
        let id = cat
            .conn
            .query_row(
                "SELECT id FROM files WHERE relative_path='old.zip' AND container_chain IS NOT NULL",
                [],
                |r| r.get::<_, i64>(0),
            )
            .unwrap();
        cat.conn
            .execute("UPDATE files SET status='quarantined' WHERE id=?1", [id])
            .unwrap();

        let touched = cat
            .touch_archive_entries("vol-1", "old.zip", 300, Some(200))
            .unwrap();
        assert_eq!(touched, 0, "a quarantined entry must not be touched");

        let status: String = cat
            .conn
            .query_row("SELECT status FROM files WHERE id=?1", [id], |r| r.get(0))
            .unwrap();
        assert_eq!(
            status, "quarantined",
            "quarantine must not be reverted by a scan"
        );
    }

    #[test]
    fn search_filtered_applies_size_and_status() {
        let (_t, cat) = open_tmp();
        let mut small = mk_file("vol-1", "small.txt", "h1");
        small.size_bytes = 10;
        let mut big = mk_file("vol-1", "big.txt", "h2");
        big.size_bytes = 5000;
        cat.upsert_file(&small, 200).unwrap();
        cat.upsert_file(&big, 200).unwrap();

        let f = SearchFilters {
            min_size: Some(1000),
            ..Default::default()
        };
        let hits = cat.search_filtered(&f, 100).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].filename, "big.txt");
    }

    #[test]
    fn search_filtered_empty_query_returns_all_filtered() {
        let (_t, cat) = open_tmp();
        cat.upsert_file(&mk_file("vol-1", "a.txt", "h1"), 200)
            .unwrap();
        cat.upsert_file(&mk_file("vol-1", "b.txt", "h2"), 200)
            .unwrap();
        let hits = cat.search_filtered(&SearchFilters::default(), 100).unwrap();
        assert_eq!(hits.len(), 2); // empty query = browse all
    }

    #[test]
    fn search_tolerates_fts_special_chars() {
        let (_t, cat) = open_tmp();
        cat.upsert_file(&mk_file("vol-1", "docs/report(final).pdf", "h1"), 200)
            .unwrap();

        let hits = cat.search("report(final)", None, None, None);
        assert!(hits.is_ok(), "special-char query must not error: {hits:?}");
        assert_eq!(hits.unwrap().len(), 1);

        let lone_quote = cat.search("\"", None, None, None);
        assert!(
            lone_quote.is_ok(),
            "lone quote query must not error: {lone_quote:?}"
        );
    }

    #[test]
    fn ranked_groups_list_their_members() {
        let (_t, cat) = open_tmp();
        cat.upsert_file(&mk_file("vol-1", "a.txt", "same"), 200)
            .unwrap();
        cat.upsert_file(&mk_file("vol-1", "b.txt", "same"), 200)
            .unwrap();
        cat.upsert_file(&mk_file("vol-1", "c.txt", "unique"), 200)
            .unwrap();
        let groups = cat.duplicate_groups_ranked(0, 10, None).unwrap();
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].copies, 2);
        let members = cat
            .duplicate_members_for(&["same".to_string()])
            .unwrap()
            .remove("same")
            .unwrap();
        assert_eq!(members.len(), 2);
    }

    #[test]
    fn reclaimable_by_volume_excludes_the_keep() {
        let (_t, cat) = open_tmp();
        cat.upsert_volume(&Volume {
            volume_id: "v".into(),
            label: "V".into(),
            identified_by: "marker".into(),
            first_seen_at: 1,
            last_seen_at: 1,
        })
        .unwrap();
        let mut f = NewFile {
            volume_id: "v".into(),
            relative_path: "a.bin".into(),
            filename: "a.bin".into(),
            extension: "bin".into(),
            size_bytes: 100,
            content_hash: "dup".into(),
            created_time: Some(10),
            modified_time: Some(10),
            accessed_time: None,
            category: Category::Other,
            container_chain: None,
        };
        cat.upsert_file(&f, 1).unwrap(); // keep (created 10)
        f.relative_path = "b.bin".into();
        f.filename = "b.bin".into();
        f.created_time = Some(20); // newer duplicate -> reclaimable
        cat.upsert_file(&f, 1).unwrap();
        f.relative_path = "u.bin".into();
        f.filename = "u.bin".into();
        f.content_hash = "uniq".into();
        f.size_bytes = 999; // unique -> not counted
        cat.upsert_file(&f, 1).unwrap();
        let map = cat.reclaimable_by_volume().unwrap();
        assert_eq!(map.get("v").copied().unwrap_or(0), 100); // only the non-keep duplicate
    }

    #[test]
    fn active_copies_returns_active_rows_for_hash() {
        let (_t, cat) = open_tmp();
        cat.upsert_file(&mk_file("vol-1", "a.txt", "same"), 200)
            .unwrap();
        cat.upsert_file(&mk_file("vol-1", "b.txt", "same"), 200)
            .unwrap();
        cat.upsert_file(&mk_file("vol-1", "c.txt", "unique"), 200)
            .unwrap();
        assert_eq!(cat.active_copies("same").unwrap().len(), 2);
        assert_eq!(cat.active_copies("unique").unwrap().len(), 1);
        assert_eq!(cat.active_copies("none").unwrap().len(), 0);
    }

    #[test]
    fn quarantine_then_purge_transitions_and_recoverable() {
        let (_t, cat) = open_tmp();
        let mut f = mk_file("vol-1", "Photos/a.jpg", "h");
        f.size_bytes = 2048;
        cat.upsert_file(&f, 200).unwrap();
        let id = cat.loose_file_id("vol-1", "Photos/a.jpg").unwrap().unwrap();

        cat.mark_quarantined(id, "_ToDelete/Photos/a.jpg", "Photos/a.jpg", 300)
            .unwrap();
        let rec = cat.get_file(id).unwrap().unwrap();
        assert_eq!(rec.status, FileStatus::Quarantined);
        assert_eq!(rec.relative_path, "_ToDelete/Photos/a.jpg");
        assert_eq!(rec.original_path.as_deref(), Some("Photos/a.jpg"));
        assert_eq!(cat.recoverable_bytes("vol-1").unwrap(), 2048);
        assert_eq!(cat.quarantined_rows("vol-1").unwrap().len(), 1);

        cat.mark_purged(id, 400).unwrap();
        assert_eq!(
            cat.get_file(id).unwrap().unwrap().status,
            FileStatus::Purged
        );
        assert_eq!(cat.recoverable_bytes("vol-1").unwrap(), 0);
    }

    #[test]
    fn log_action_appends() {
        let (_t, cat) = open_tmp();
        cat.log_action("quarantine", "{\"file_id\":1}", 100)
            .unwrap();
        cat.log_action("purge", "{\"volume_id\":\"v\"}", 200)
            .unwrap();
        let n: i64 = cat
            .conn
            .query_row("SELECT count(*) FROM actions_log", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 2);
    }

    #[test]
    fn recent_actions_returns_newest_first() {
        let (_t, cat) = open_tmp();
        cat.log_action("quarantine", "{\"file_id\":1}", 100)
            .unwrap();
        cat.log_action("purge", "{\"volume_id\":\"v\"}", 200)
            .unwrap();
        let rows = cat.recent_actions(10).unwrap();
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].0, "purge"); // newest first
        assert_eq!(rows[0].2, 200);
        assert_eq!(rows[1].0, "quarantine");
        // limit is respected
        assert_eq!(cat.recent_actions(1).unwrap().len(), 1);
    }

    #[test]
    fn touch_does_not_resurrect_missing_archive_entries() {
        let (_t, cat) = open_tmp();
        cat.upsert_archive_entry("vol-1", "old.zip", &mk_entry("a.jpg", "h1"), None, 200)
            .unwrap();
        cat.upsert_archive_entry("vol-1", "old.zip", &mk_entry("gone.jpg", "h2"), None, 200)
            .unwrap();
        // rescan at 300 re-sees only a.jpg -> gone.jpg swept to missing
        cat.upsert_archive_entry("vol-1", "old.zip", &mk_entry("a.jpg", "h1"), None, 300)
            .unwrap();
        cat.mark_missing_scanned("vol-1", 300, 300, &[]).unwrap();
        assert_eq!(
            cat.search("gone", None, None, Some("missing"))
                .unwrap()
                .len(),
            1
        );
        // a later incremental-skip touch must NOT resurrect gone.jpg: the archive itself was never
        // missing (revive_floor=None), so a genuinely-removed entry must stay missing.
        cat.touch_archive_entries("vol-1", "old.zip", 400, None)
            .unwrap();
        assert_eq!(
            cat.search("gone", None, None, Some("missing"))
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            cat.search("gone", None, None, Some("active"))
                .unwrap()
                .len(),
            0
        );
    }

    #[test]
    fn archive_entries_lists_only_that_archives_entries() {
        let (_t, cat) = open_tmp();
        let e = |chain: &str, hash: &str| crate::archive::ArchiveEntry {
            container_chain: chain.into(),
            filename: chain.rsplit('/').next().unwrap().into(),
            extension: "jpg".into(),
            size_bytes: 5,
            content_hash: hash.into(),
        };
        cat.upsert_archive_entry("vol-1", "a.zip", &e("x.jpg", "h1"), None, 100)
            .unwrap();
        cat.upsert_archive_entry("vol-1", "a.zip", &e("y.jpg", "h2"), None, 100)
            .unwrap();
        cat.upsert_archive_entry("vol-1", "b.zip", &e("z.jpg", "h3"), None, 100)
            .unwrap();
        let es = cat.archive_entries("vol-1", "a.zip").unwrap();
        assert_eq!(es.len(), 2);
        assert!(es
            .iter()
            .all(|r| r.relative_path == "a.zip" && r.container_chain.is_some()));
    }

    #[test]
    fn archive_roots_lists_each_container_once_with_its_totals() {
        let (_t, cat) = open_tmp();
        let e = |chain: &str, size: i64, hash: &str| crate::archive::ArchiveEntry {
            container_chain: chain.into(),
            filename: chain.rsplit('/').next().unwrap().into(),
            extension: "txt".into(),
            size_bytes: size,
            content_hash: hash.into(),
        };
        cat.upsert_archive_entry("vol-1", "a/bundle.zip", &e("one.txt", 10, "h1"), None, 100)
            .unwrap();
        cat.upsert_archive_entry("vol-1", "a/bundle.zip", &e("two.txt", 20, "h2"), None, 100)
            .unwrap();
        cat.upsert_archive_entry("vol-1", "b/other.zip", &e("three.txt", 5, "h3"), None, 100)
            .unwrap();

        let roots = cat.archive_roots("vol-1").unwrap();
        assert_eq!(roots.len(), 2, "one row per archive, not per entry");
        let bundle = roots
            .iter()
            .find(|r| r.relative_path == "a/bundle.zip")
            .unwrap();
        assert_eq!(bundle.entries, 2);
        assert_eq!(bundle.uncompressed_bytes, 30);
    }

    #[test]
    fn mark_quarantined_clears_container_chain() {
        let (_t, cat) = open_tmp();
        cat.upsert_archive_entry(
            "vol-1",
            "a.zip",
            &crate::archive::ArchiveEntry {
                container_chain: "x.jpg".into(),
                filename: "x.jpg".into(),
                extension: "jpg".into(),
                size_bytes: 5,
                content_hash: "h1".into(),
            },
            None,
            100,
        )
        .unwrap();
        // find the entry row id
        let id = cat.archive_entries("vol-1", "a.zip").unwrap()[0].id;
        cat.mark_quarantined(id, "_ToDelete/a.zip/x.jpg", "a.zip › x.jpg", 200)
            .unwrap();
        let rec = cat.get_file(id).unwrap().unwrap();
        assert_eq!(rec.status, FileStatus::Quarantined);
        assert_eq!(rec.container_chain, None); // now a loose quarantined row
        assert_eq!(rec.relative_path, "_ToDelete/a.zip/x.jpg");
        assert_eq!(rec.original_path.as_deref(), Some("a.zip › x.jpg"));
    }

    #[test]
    fn default_search_hides_purged_rows_but_status_filter_shows_them() {
        let (_t, cat) = open_tmp();
        cat.upsert_volume(&crate::catalog::models::Volume {
            volume_id: "v".into(),
            label: "D".into(),
            identified_by: "marker".into(),
            first_seen_at: 1,
            last_seen_at: 1,
        })
        .unwrap();
        let mk = |name: &str| crate::catalog::models::NewFile {
            volume_id: "v".into(),
            relative_path: name.into(),
            filename: name.into(),
            extension: "txt".into(),
            size_bytes: 1,
            content_hash: format!("h-{name}"),
            created_time: None,
            modified_time: None,
            accessed_time: None,
            category: crate::category::Category::Other,
            container_chain: None,
        };
        cat.upsert_file(&mk("keep.txt"), 1).unwrap();
        cat.upsert_file(&mk("_ToDelete/gone.txt"), 1).unwrap();
        let gone = cat
            .loose_file_id("v", "_ToDelete/gone.txt")
            .unwrap()
            .unwrap();
        cat.mark_purged(gone, 200).unwrap();

        // Default browse (no status filter) must not show the purged `_ToDelete` row.
        let def = cat.search("", None, None, None).unwrap();
        assert_eq!(def.len(), 1);
        assert_eq!(def[0].relative_path, "keep.txt");

        // Explicitly asking for purged still surfaces them (audit view).
        let purged = cat.search("", None, None, Some("purged")).unwrap();
        assert_eq!(purged.len(), 1);
        assert_eq!(purged[0].relative_path, "_ToDelete/gone.txt");
    }

    #[test]
    fn forget_volume_deletes_rows_and_fts_but_returns_count() {
        let (_t, cat) = open_tmp();
        cat.upsert_volume(&crate::catalog::models::Volume {
            volume_id: "v".into(),
            label: "Gone".into(),
            identified_by: "marker".into(),
            first_seen_at: 1,
            last_seen_at: 1,
        })
        .unwrap();
        let f = crate::catalog::models::NewFile {
            volume_id: "v".into(),
            relative_path: "a.txt".into(),
            filename: "a.txt".into(),
            extension: "txt".into(),
            size_bytes: 1,
            content_hash: "h".into(),
            created_time: None,
            modified_time: None,
            accessed_time: None,
            category: crate::category::Category::Other,
            container_chain: None,
        };
        cat.upsert_file(&f, 1).unwrap();
        let removed = cat.forget_volume("v", 500).unwrap();
        assert_eq!(removed, 1);
        assert_eq!(cat.volume_last_seen("v").unwrap(), None); // volume row gone
        assert!(cat.search("a", None, None, None).unwrap().is_empty()); // FTS row gone
        assert!(cat
            .recent_actions(5)
            .unwrap()
            .iter()
            .any(|(a, _, _)| a == "forget"));
    }

    #[test]
    fn volume_path_and_meta_round_trip() {
        let (_t, cat) = open_tmp();
        cat.upsert_volume(&crate::catalog::models::Volume {
            volume_id: "v".into(),
            label: "Detected".into(),
            identified_by: "marker".into(),
            first_seen_at: 1,
            last_seen_at: 1,
        })
        .unwrap();
        // path
        cat.set_volume_path("v", "/some/folder", 5).unwrap();
        assert_eq!(
            cat.volume_paths().unwrap(),
            vec![("v".to_string(), "/some/folder".to_string())]
        );
        // meta: set name + description
        cat.set_volume_meta("v", Some("My Photos"), Some("holiday pics"), 6)
            .unwrap();
        assert_eq!(
            cat.volume_meta("v").unwrap(),
            (
                Some("My Photos".to_string()),
                Some("holiday pics".to_string())
            )
        );
        assert_eq!(
            cat.effective_labels().unwrap().get("v").cloned(),
            Some("My Photos".to_string())
        );
        // clearing the name (empty) falls back to the detected label
        cat.set_volume_meta("v", Some("  "), None, 7).unwrap();
        assert_eq!(cat.volume_meta("v").unwrap().0, None);
        assert_eq!(
            cat.effective_labels().unwrap().get("v").cloned(),
            Some("Detected".to_string())
        );
    }

    #[test]
    fn set_volume_meta_partial_update_preserves_other_field() {
        let (_t, cat) = open_tmp();
        cat.upsert_volume(&crate::catalog::models::Volume {
            volume_id: "v".into(),
            label: "Detected".into(),
            identified_by: "marker".into(),
            first_seen_at: 1,
            last_seen_at: 1,
        })
        .unwrap();
        cat.set_volume_meta("v", Some("My Name"), Some("my desc"), 5)
            .unwrap();
        // Update only the description (name = None) -> name must survive.
        cat.set_volume_meta("v", None, Some("new desc"), 6).unwrap();
        assert_eq!(
            cat.volume_meta("v").unwrap(),
            (Some("My Name".to_string()), Some("new desc".to_string()))
        );
        // Update only the name -> description survives.
        cat.set_volume_meta("v", Some("Name2"), None, 7).unwrap();
        assert_eq!(
            cat.volume_meta("v").unwrap(),
            (Some("Name2".to_string()), Some("new desc".to_string()))
        );
        // Explicit clear of the name (empty) falls back to the label; description untouched.
        cat.set_volume_meta("v", Some(""), None, 8).unwrap();
        assert_eq!(cat.volume_meta("v").unwrap().0, None);
        assert_eq!(
            cat.effective_labels().unwrap().get("v").cloned(),
            Some("Detected".to_string())
        );
    }

    #[test]
    fn update_archive_hash_changes_hash_and_size() {
        let (_t, cat) = open_tmp();
        cat.upsert_file(&mk_file("vol-1", "a.zip", "OLD"), 100)
            .unwrap();
        let id = cat.loose_file_id("vol-1", "a.zip").unwrap().unwrap();
        cat.update_archive_hash(id, "NEW", 999, 200).unwrap();
        let rec = cat.get_file(id).unwrap().unwrap();
        assert_eq!(rec.content_hash, "NEW");
        assert_eq!(rec.size_bytes, 999);
    }

    #[test]
    fn get_file_meta_reports_whether_the_file_has_archive_entries() {
        let (_t, cat) = open_tmp();
        cat.upsert_volume(&crate::catalog::models::Volume {
            volume_id: "v".into(),
            label: "V".into(),
            identified_by: "marker".into(),
            first_seen_at: 1,
            last_seen_at: 1,
        })
        .unwrap();
        let mk = |rel: &str, chain: Option<&str>| crate::catalog::models::NewFile {
            volume_id: "v".into(),
            relative_path: rel.into(),
            filename: rel.rsplit('/').next().unwrap().into(),
            extension: "zip".into(),
            size_bytes: 10,
            content_hash: "H".into(),
            created_time: Some(1),
            modified_time: Some(5),
            accessed_time: Some(1),
            category: crate::category::Category::Other,
            container_chain: chain.map(|c| c.to_string()),
        };
        cat.upsert_file(&mk("plain.bin", None), 1).unwrap();
        cat.upsert_file(&mk("bundle.bak", None), 1).unwrap();
        cat.upsert_file(&mk("bundle.bak", Some("inner.txt")), 1)
            .unwrap();

        let (_, _, plain_has, plain_floor) = cat.get_file_meta("v", "plain.bin").unwrap().unwrap();
        let (_, _, bundle_has, bundle_floor) =
            cat.get_file_meta("v", "bundle.bak").unwrap().unwrap();
        assert!(!plain_has, "a loose file has no archive entries");
        assert!(
            bundle_has,
            "an archive's own row must report that it has entries"
        );
        assert_eq!(
            plain_floor, None,
            "a freshly-upserted file is active, not missing"
        );
        assert_eq!(bundle_floor, None);
        assert!(cat.get_file_meta("v", "absent.bin").unwrap().is_none());
    }

    fn put(cat: &Catalog, vol: &str, path: &str, hash: &str, size: i64, now: i64) {
        cat.upsert_file(
            &NewFile {
                volume_id: vol.into(),
                relative_path: path.into(),
                filename: path.rsplit('/').next().unwrap().into(),
                extension: "txt".into(),
                size_bytes: size,
                content_hash: hash.into(),
                created_time: None,
                modified_time: None,
                accessed_time: None,
                category: Category::Document,
                container_chain: None,
            },
            now,
        )
        .unwrap();
    }

    #[test]
    fn touch_seen_and_upsert_file_must_not_revive_a_quarantined_loose_row() {
        // The issue names touch_seen alongside upsert_archive_entry. Same question, same contract.
        let (_t, cat) = open_tmp();
        cat.upsert_file(&mk_file("vol-1", "photo.jpg", "H1"), 100)
            .unwrap();
        cat.conn
            .execute(
                "UPDATE files SET status='quarantined' WHERE relative_path='photo.jpg'",
                [],
            )
            .unwrap();

        cat.touch_seen("vol-1", "photo.jpg", 200).unwrap();
        let after_touch: String = cat
            .conn
            .query_row(
                "SELECT status FROM files WHERE relative_path='photo.jpg'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            after_touch, "quarantined",
            "touch_seen revived a quarantined row"
        );

        cat.upsert_file(&mk_file("vol-1", "photo.jpg", "H1"), 300)
            .unwrap();
        let after_upsert: String = cat
            .conn
            .query_row(
                "SELECT status FROM files WHERE relative_path='photo.jpg'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            after_upsert, "quarantined",
            "upsert_file revived a quarantined row"
        );
    }

    #[test]
    fn upserting_an_archive_entry_must_not_revive_a_quarantined_row() {
        // #46: the ON CONFLICT arm sets status='active' with no status filter, so re-descending an
        // archive would flip an entry the user deliberately quarantined back to live. Exercised at
        // the statement level, because that is where the defect is.
        let (_t, cat) = open_tmp();
        let e = mk_entry("inner.jpg", "H1");
        cat.upsert_archive_entry("vol-1", "old.zip", &e, None, 100)
            .unwrap();
        cat.conn
            .execute(
                "UPDATE files SET status='quarantined' WHERE container_chain='inner.jpg'",
                [],
            )
            .unwrap();

        // A later scan re-descends the same archive and sees the same entry.
        cat.upsert_archive_entry("vol-1", "old.zip", &e, None, 200)
            .unwrap();

        let status: String = cat
            .conn
            .query_row(
                "SELECT status FROM files WHERE container_chain='inner.jpg'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            status, "quarantined",
            "a scan must never reverse the user's quarantine decision"
        );
    }

    #[test]
    fn upserting_an_archive_entry_must_not_revive_a_purged_row() {
        // Purged is even more emphatic than quarantined: the file is gone from disk. Reviving it
        // would have the catalogue claim a deleted file is live.
        let (_t, cat) = open_tmp();
        let e = mk_entry("gone.jpg", "H2");
        cat.upsert_archive_entry("vol-1", "old.zip", &e, None, 100)
            .unwrap();
        cat.conn
            .execute(
                "UPDATE files SET status='purged' WHERE container_chain='gone.jpg'",
                [],
            )
            .unwrap();
        cat.upsert_archive_entry("vol-1", "old.zip", &e, None, 200)
            .unwrap();
        let status: String = cat
            .conn
            .query_row(
                "SELECT status FROM files WHERE container_chain='gone.jpg'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(status, "purged");
    }

    #[test]
    fn upserting_an_archive_entry_still_revives_a_missing_row() {
        // The other half of the contract: 'missing' MUST come back. An archive that vanished and
        // returned is the ordinary case, and refusing to revive it would strand real files.
        let (_t, cat) = open_tmp();
        let e = mk_entry("back.jpg", "H3");
        cat.upsert_archive_entry("vol-1", "old.zip", &e, None, 100)
            .unwrap();
        cat.conn
            .execute(
                "UPDATE files SET status='missing' WHERE container_chain='back.jpg'",
                [],
            )
            .unwrap();
        cat.upsert_archive_entry("vol-1", "old.zip", &e, None, 200)
            .unwrap();
        let status: String = cat
            .conn
            .query_row(
                "SELECT status FROM files WHERE container_chain='back.jpg'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(status, "active", "a returning file must come back to life");
    }

    #[test]
    fn volume_stats_uses_stored_totals_and_falls_back_when_absent() {
        // The totals are derived: refreshed where directory_trees is, and safe to be absent.
        // A catalogue that predates the columns must still report correct numbers -- slowly is
        // fine, wrong is not.
        let (_t, cat) = open_tmp();
        put(&cat, "vol-1", "a.txt", "H1", 10, 100);
        put(&cat, "vol-1", "b.txt", "H2", 25, 100);

        // Nothing stored yet: the live aggregate must still be right.
        let s = cat.volume_stats().unwrap();
        assert_eq!(s[0].2, 2, "fallback must count active files");
        assert_eq!(s[0].3, 35, "fallback must sum active bytes");

        cat.refresh_volume_totals("vol-1").unwrap();
        let s = cat.volume_stats().unwrap();
        assert_eq!(
            (s[0].2, s[0].3),
            (2, 35),
            "stored totals must match the live aggregate"
        );

        // And the stored path is genuinely being used: poison the column and watch it come back.
        cat.conn
            .execute(
                "UPDATE volumes SET active_files=999 WHERE volume_id='vol-1'",
                [],
            )
            .unwrap();
        assert_eq!(
            cat.volume_stats().unwrap()[0].2,
            999,
            "volume_stats must be reading the stored column, not recomputing"
        );
        cat.refresh_volume_totals("vol-1").unwrap();
        assert_eq!(cat.volume_stats().unwrap()[0].2, 2, "a refresh corrects it");
    }

    #[test]
    fn rebuilding_directory_trees_finds_an_identical_pair() {
        let (_t, cat) = open_tmp();
        for (path, hash, size) in [
            ("orig/a.txt", "H1", 10i64),
            ("orig/b.txt", "H2", 20),
            ("copy/a.txt", "H1", 10),
            ("copy/b.txt", "H2", 20),
            ("unique/z.txt", "H9", 5),
        ] {
            put(&cat, "vol-1", path, hash, size, 100);
        }
        let n = cat.rebuild_directory_trees("vol-1", 100).unwrap();
        assert!(n >= 3, "root plus three folders, got {n}");

        let groups = cat.tree_duplicate_groups().unwrap();
        assert_eq!(groups.len(), 1, "orig and copy, and nothing else");
        let mut paths: Vec<&str> = groups[0].members.iter().map(|m| m.path.as_str()).collect();
        paths.sort();
        assert_eq!(paths, vec!["copy", "orig"]);
        assert_eq!(groups[0].reclaimable_bytes, 30);
    }

    #[test]
    fn a_quarantined_file_removes_its_folder_from_the_groups() {
        // Only active rows participate: once a twin is quarantined the pair is no longer a
        // duplicate, and continuing to offer it would invite quarantining the last copy.
        let (_t, cat) = open_tmp();
        put(&cat, "vol-1", "orig/a.txt", "H1", 10, 100);
        put(&cat, "vol-1", "copy/a.txt", "H1", 10, 100);
        cat.rebuild_directory_trees("vol-1", 100).unwrap();
        assert_eq!(cat.tree_duplicate_groups().unwrap().len(), 1);

        let id: i64 = cat
            .conn
            .query_row(
                "SELECT id FROM files WHERE volume_id='vol-1' AND relative_path='copy/a.txt'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        cat.mark_quarantined(id, "_ToDelete/copy/a.txt", "copy/a.txt", 200)
            .unwrap();
        cat.rebuild_directory_trees("vol-1", 200).unwrap();
        assert!(
            cat.tree_duplicate_groups().unwrap().is_empty(),
            "the pair is gone once one side is quarantined"
        );
    }

    #[test]
    fn rebuilding_twice_is_idempotent() {
        let (_t, cat) = open_tmp();
        put(&cat, "vol-1", "d/a.txt", "H1", 1, 100);
        let first = cat.rebuild_directory_trees("vol-1", 100).unwrap();
        let second = cat.rebuild_directory_trees("vol-1", 200).unwrap();
        assert_eq!(first, second, "a rebuild must replace, not accumulate");
        let stored: i64 = cat
            .conn
            .query_row("SELECT COUNT(*) FROM directory_trees", [], |r| r.get(0))
            .unwrap();
        assert_eq!(stored as usize, second, "no duplicate rows left behind");
    }

    #[test]
    fn forgetting_a_volume_drops_its_directory_trees() {
        // directory_trees has a foreign key to volumes, so leaving these rows behind does not just
        // orphan them -- it makes `forget` fail outright under foreign_keys=ON.
        let (_t, cat) = open_tmp();
        put(&cat, "vol-1", "d/a.txt", "H1", 1, 100);
        cat.rebuild_directory_trees("vol-1", 100).unwrap();
        assert!(
            cat.conn
                .query_row("SELECT COUNT(*) FROM directory_trees", [], |r| r
                    .get::<_, i64>(0))
                .unwrap()
                > 0
        );

        cat.forget_volume("vol-1", 200).unwrap();
        let left: i64 = cat
            .conn
            .query_row("SELECT COUNT(*) FROM directory_trees", [], |r| r.get(0))
            .unwrap();
        assert_eq!(left, 0, "a forgotten volume must leave no phantom trees");
    }

    #[test]
    fn an_archive_entry_tree_is_stored_with_its_container() {
        // The archive_root column is what lets the review UI refuse to offer a rename for a folder
        // that lives inside a zip.
        let (_t, cat) = open_tmp();
        put(&cat, "vol-1", "x/backup.zip", "ZIPHASH", 999, 100);
        cat.conn
            .execute(
                "INSERT INTO files(volume_id, relative_path, filename, extension, size_bytes,
                     content_hash, category, container_chain, status, first_seen_at, last_seen_at)
                 VALUES ('vol-1','x/backup.zip','cfg','txt',7,'HI','document',
                         'Project/.git/config','active',100,100)",
                [],
            )
            .unwrap();
        cat.rebuild_directory_trees("vol-1", 100).unwrap();

        let root: Option<String> = cat
            .conn
            .query_row(
                "SELECT archive_root FROM directory_trees WHERE path='x/backup.zip/Project'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(root.as_deref(), Some("x/backup.zip"));

        let own: Option<String> = cat
            .conn
            .query_row(
                "SELECT archive_root FROM directory_trees WHERE path='x/backup.zip'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(own, None, "the archive itself is movable as one file");
    }
}
