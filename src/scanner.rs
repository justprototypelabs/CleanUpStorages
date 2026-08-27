use std::path::Path;
use walkdir::WalkDir;

use crate::archive::{self, ArchiveLimits};
use crate::catalog::models::{NewFile, Volume};
use crate::catalog::Catalog;
use crate::category::Category;
use crate::hashing;
use crate::volume::VolumeIdentity;

/// Commit when EITHER bound is reached.
///
/// The byte bound is what makes a larger file count safe. A stopped or interrupted scan loses the
/// current uncommitted batch and re-hashes those files on resume; a file count alone cannot bound
/// that cost, because 200 video files and 200 text files are wildly different amounts of work.
///
/// Two limits on that, stated because the bound is easy to over-read:
///
/// - It cannot cap the re-work below **one file**. Bytes are added after a file is hashed, so a
///   single file larger than the bound already exceeds it alone — a 4 GB video is re-hashed in full
///   if the scan dies mid-file, and no batch policy can prevent that. What the bound does give is
///   that such a file is committed immediately afterwards instead of riding along in an open
///   transaction with its neighbours.
/// - Skipped files count toward it too, though re-doing a skipped file costs a stat and an indexed
///   lookup rather than a hash. That makes a large-file rescan commit more often than the
///   re-hashing rationale alone would require — conservative, never unsafe, and it matches the
///   file counter, which has always counted skips. The measurements below were taken with this
///   behaviour in place.
///
/// See docs/benchmarking-scans.md, "Write-path tuning (#26)", Task 4, for the measurements behind
/// both values.
const BATCH_MAX_FILES: usize = 1000;
const BATCH_MAX_BYTES: u64 = 64 * 1024 * 1024;

/// Optional live-progress sink for a scan. Each method fires once per counted event.
pub trait Progress: Send + Sync {
    fn on_hashed(&self);
    fn on_skipped(&self);
    fn on_error(&self);
    fn on_archive_entry(&self);
    /// Totals from the counting pass. Never called when counting is skipped, so a percentage is
    /// absent rather than wrong.
    fn on_total(&self, _files: u64, _bytes: u64) {}
    /// Bytes of the file just finished, hashed or skipped. Drives the rate and the ETA.
    fn on_bytes(&self, _bytes: u64) {}
}

/// Outcome of one `scan_volume` pass.
#[derive(Debug, Default)]
pub struct ScanSummary {
    pub hashed: usize,
    pub skipped: usize,
    pub errors: usize,
    pub marked_missing: usize,
    pub archive_entries: usize,
    /// Where this scan's time went. Measured always; see `scan_metrics`.
    pub metrics: crate::scan_metrics::MetricsSnapshot,
    /// True when the scan ended on a stop request rather than reaching the end of the tree.
    /// A stopped scan must not run the missing-sweep.
    pub stopped: bool,
}

/// Metadata timestamp (best-effort) as seconds since UNIX_EPOCH.
fn unix_secs(t: std::io::Result<std::time::SystemTime>) -> Option<i64> {
    t.ok()
        .and_then(|st| st.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
}

/// Directories that are the operating system's business, not the user's data.
///
/// `$RECYCLE.BIN` holds files Windows already considers DELETED, and `System Volume Information`
/// holds restore points. Cataloguing them was never intended, and on the real drives it did active
/// harm: 77,493 recycle-bin rows, and because `$` sorts before every letter they filled the entire
/// Browse page, which showed nothing else. They also formed 85 duplicate groups made only of
/// deleted files, offering the user decisions about data they had already thrown away.
const SYSTEM_DIRS: &[&str] = &["$RECYCLE.BIN", "System Volume Information"];

/// True if `path` is the identity marker file, lives under a `_ToDelete` quarantine dir, or lives
/// under a system directory the tool has no business cataloguing.
fn should_skip(path: &Path, file_name: &std::ffi::OsStr) -> bool {
    file_name == crate::volume::MARKER
        || path.components().any(|c| {
            let s = c.as_os_str();
            s == crate::volume::QUARANTINE_DIR
                || SYSTEM_DIRS.iter().any(|d| s.eq_ignore_ascii_case(d))
        })
}

/// Opens `path` for the top-level archive-detection peek. Factored out purely so a test can force
/// this one call to fail without also failing the hash step that runs just before it -- a real OS
/// lock (share violation, permission) held externally blocks both opens indiscriminately, so there
/// is no way to reproduce a detection-only failure with real file handles.
#[cfg(not(test))]
fn open_for_archive_detection(path: &Path) -> std::io::Result<std::fs::File> {
    std::fs::File::open(path)
}

#[cfg(test)]
thread_local! {
    static FORCE_DETECTION_OPEN_ERROR: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[cfg(test)]
fn open_for_archive_detection(path: &Path) -> std::io::Result<std::fs::File> {
    if FORCE_DETECTION_OPEN_ERROR.with(|f| f.get()) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "forced for test",
        ));
    }
    std::fs::File::open(path)
}

/// Path of `path` relative to `root`, normalized to forward slashes; `None` if not under `root`.
fn relative_path(path: &Path, root: &Path) -> Option<String> {
    path.strip_prefix(root)
        .ok()
        .map(|r| r.to_string_lossy().replace('\\', "/"))
}

/// Commit the current transaction and open the next one, resetting both accumulators.
fn rotate_batch(cat: &Catalog, in_batch: &mut usize, batch_bytes: &mut u64) -> anyhow::Result<()> {
    if *in_batch >= BATCH_MAX_FILES || *batch_bytes >= BATCH_MAX_BYTES {
        cat.conn.execute_batch("COMMIT; BEGIN")?;
        *in_batch = 0;
        *batch_bytes = 0;
    }
    Ok(())
}

/// Files and bytes a scan of `root` would process.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct TreeTotals {
    pub files: u64,
    pub bytes: u64,
}

/// Count the tree without reading any file contents — `readdir` + `stat` only.
///
/// This is what makes a real percentage possible for a folder that has never been scanned, which is
/// most of a first pass. It costs a metadata walk, so it scales with file count rather than with
/// terabytes. Errors are ignored: this is an estimate, and a directory the scan cannot read is
/// reported by the scan itself.
pub fn count_tree(root: &Path, stop: &crate::scan_control::StopFlag) -> TreeTotals {
    let mut totals = TreeTotals::default();
    for entry in WalkDir::new(root).into_iter().filter_map(|e| e.ok()) {
        if stop.is_requested() {
            break;
        }
        if !entry.file_type().is_file() {
            continue;
        }
        if should_skip(entry.path(), entry.file_name()) {
            continue;
        }
        if let Ok(meta) = entry.metadata() {
            totals.files += 1;
            totals.bytes += meta.len();
        }
    }
    totals
}

/// Recursively scan `root`, hashing new/changed files and skipping (but re-touching) unchanged
/// ones, then sweep any previously-active file not seen this pass into `missing`.
///
/// `force` bypasses the incremental skip and re-hashes every file. `now` is used both as the
/// scan's `last_seen_at` stamp and as `scan_started_at` for the missing-file sweep: because every
/// file touched this scan gets `last_seen_at == now`, `mark_missing_scanned` (which flags rows
/// with `last_seen_at < scan_started_at`) only ever catches files genuinely absent this pass.
///
/// The stamp actually used is `Catalog::next_seen_stamp`, which keeps it strictly above anything the
/// volume already carries. Raw wall-clock seconds broke the sweep whenever two scans shared a second
/// or the clock moved backwards (#45).
///
/// `metrics` is owned by the caller so a scan that bails part-way still yields what it measured
/// before it died — the multi-day run that fails late is the one most worth measuring.
#[allow(
    clippy::too_many_arguments,
    reason = "each parameter is an independent scan input; grouping them into a struct would add \
        indirection without reducing real complexity"
)]
pub fn scan_volume_with_progress(
    cat: &Catalog,
    root: &Path,
    identity: &VolumeIdentity,
    force: bool,
    now: i64,
    progress: Option<&dyn Progress>,
    metrics: &crate::scan_metrics::ScanMetrics,
    stop: &crate::scan_control::StopFlag,
    limits: &ArchiveLimits,
) -> anyhow::Result<ScanSummary> {
    // Monotonic per volume, not raw wall-clock (#45). The sweep below compares
    // `last_seen_at < scan_started_at`, which a second-resolution clock quietly breaks: two scans in
    // the same second make `t < t` false, and a clock that moved backwards makes `2000 < 1500`
    // false. Either way a deleted file stays stale-active, and the catalogue then claims a file is
    // present when it is gone -- the unsafe direction, since dedup can offer it as the copy to keep.
    //
    // Shadowing `now` is deliberate: every row this scan stamps must carry the same value the sweep
    // compares against, and one binding is how they cannot drift apart.
    // Rows for directories we no longer walk. Without this they would be swept to `missing` on
    // this very scan -- an alarm about files Windows already deleted (#66 follow-up).
    match cat.forget_system_paths(&identity.volume_id) {
        Ok(0) => {}
        Ok(n) => tracing::info!(rows = n, "dropped catalogued system-directory rows"),
        // Loud, not swallowed: the first version of this silently failed on a malformed
        // SQL escape and the rows simply stayed.
        Err(e) => tracing::error!("could not drop system-directory rows: {e}"),
    }

    let now = cat.next_seen_stamp(&identity.volume_id, now)?;
    let scan_started_at = now;
    let mut summary = ScanSummary::default();
    let mut in_batch = 0usize;
    let mut batch_bytes = 0u64;
    // Directories this pass could not enumerate. Their contents were never visited, so they must be
    // held back from the missing-sweep below (#7) — unreadable is not the same as gone.
    let mut unreadable_dirs: Vec<String> = Vec::new();
    cat.conn.execute_batch("BEGIN")?;

    let mut walker = WalkDir::new(root).into_iter();
    loop {
        let next = {
            let _t = metrics.timer(crate::scan_metrics::Phase::Walk);
            walker.next()
        };
        let Some(entry) = next else { break };
        if stop.is_requested() {
            summary.stopped = true;
            break;
        }
        let entry = match entry {
            Ok(e) => e,
            Err(err) => {
                let p = err
                    .path()
                    .map(|p| {
                        p.strip_prefix(root)
                            .unwrap_or(p)
                            .to_string_lossy()
                            .replace('\\', "/")
                    })
                    .unwrap_or_else(|| "<unknown>".to_string());
                cat.log_scan_error(
                    Some(&identity.volume_id),
                    &p,
                    &format!("walk: {err}"),
                    "walk",
                    err.io_error()
                        .map(crate::catalog::scan_errors::classify_io)
                        .unwrap_or("other"),
                    now,
                )?;
                summary.errors += 1;
                if let Some(pr) = progress {
                    pr.on_error();
                }
                // Only a known path can be scoped. An error with no path (rare) leaves the sweep
                // unrestricted, which self-heals on the next successful scan.
                if p != "<unknown>" {
                    unreadable_dirs.push(p);
                }
                continue;
            }
        };
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        let name = entry.file_name();
        if should_skip(path, name) {
            continue;
        }
        let Some(rel) = relative_path(path, root) else {
            continue;
        };

        // The failed stat is legitimately walk cost; the two SQLite writes that follow it are not,
        // so the guard drops before the error arm runs.
        let stat = {
            let _t = metrics.timer(crate::scan_metrics::Phase::Walk);
            entry.metadata()
        };
        let meta = match stat {
            Ok(m) => m,
            Err(e) => {
                // Still a file the walk considered, and it still cost a seek — but its size is
                // genuinely unknown, so it lands in bucket 0.
                metrics.record_file_seen(0);
                cat.log_scan_error(
                    Some(&identity.volume_id),
                    &rel,
                    &format!("metadata: {e}"),
                    "metadata",
                    e.io_error()
                        .map(crate::catalog::scan_errors::classify_io)
                        .unwrap_or("other"),
                    now,
                )?;
                summary.errors += 1;
                if let Some(p) = progress {
                    p.on_error();
                }
                let _ = cat.touch_seen(&identity.volume_id, &rel, now);
                continue;
            }
        };
        let size = meta.len() as i64;
        let mtime = unix_secs(meta.modified());
        metrics.record_file_seen(size);

        // Incremental skip: same size + mtime as catalogued -> just touch, don't re-hash.
        // `skip_check` is get_file_meta + touch_seen only; the batch COMMIT this path also
        // triggers is db_write (it is the fsync #26 targets), so the guard must be dead before
        // rotate_batch runs — otherwise a rescan books 100% of its fsyncs to skip_check and reads
        // as seek-bound.
        //
        // Also captures what the catalogue knew about this path BEFORE this pass touches it, for
        // the archive-detection fallback further down: `upsert_file` below unconditionally writes
        // this path's own row, so a query made AFTER it would find a row for every path -- even one
        // seen for the very first time -- and could never fall through to the extension test.
        //
        // Fetched unconditionally, `force` or not: `force` governs only whether the SKIP decision
        // below is allowed to fire, not whether we learn what the catalogue already knew. A forced
        // scan already re-hashes every byte of every file, so one more indexed lookup is noise
        // beside that -- and skipping the fetch under `force` is exactly what silently reopened
        // this defect for the documented `--force` recovery path.
        let prior_meta: Option<crate::catalog::store::FileMeta> = {
            let _t = metrics.timer(crate::scan_metrics::Phase::SkipCheck);
            cat.get_file_meta(&identity.volume_id, &rel)?
        };
        let is_unchanged = if force {
            false
        } else {
            match prior_meta {
                Some((old_size, old_mtime, has_archive_entries, revive_floor))
                    if old_size == size && old_mtime == mtime.unwrap_or(0) =>
                {
                    cat.touch_seen(&identity.volume_id, &rel, now)?;
                    // From the catalogue, not from the filename: a renamed zip has entries too,
                    // and missing them here would let the sweep mark present files missing.
                    if has_archive_entries {
                        // revive_floor is Some(last_seen_at) only if the archive's own row was
                        // ALSO missing a moment ago -- i.e. the whole archive vanished and came
                        // back. Only entries that were still present at that same moment (their own
                        // last_seen_at >= the floor) revive; an entry removed from the archive's
                        // real content by an earlier descend has a smaller last_seen_at and stays
                        // missing even though the archive returned.
                        cat.touch_archive_entries(&identity.volume_id, &rel, now, revive_floor)?;
                    }
                    true
                }
                _ => false,
            }
        };
        if is_unchanged {
            summary.skipped += 1;
            metrics.add_bytes_skipped(size);
            if let Some(p) = progress {
                p.on_bytes(size as u64);
            }
            if let Some(p) = progress {
                p.on_skipped();
            }
            in_batch += 1;
            batch_bytes += size as u64;
            {
                let _t = metrics.timer(crate::scan_metrics::Phase::DbWrite);
                rotate_batch(cat, &mut in_batch, &mut batch_bytes)?;
            }
            continue;
        }

        // As with the stat above: the failed read is hash cost, its error logging is not.
        let hashed = {
            let _t = metrics.timer(crate::scan_metrics::Phase::Hash);
            hashing::hash_file(path)
        };
        let hash = match hashed {
            Ok(h) => h,
            Err(e) => {
                cat.log_scan_error(
                    Some(&identity.volume_id),
                    &rel,
                    &format!("read: {e}"),
                    "read",
                    crate::catalog::scan_errors::classify_io(&e),
                    now,
                )?;
                summary.errors += 1;
                if let Some(p) = progress {
                    p.on_error();
                }
                let _ = cat.touch_seen(&identity.volume_id, &rel, now);
                continue;
            }
        };
        metrics.add_bytes_hashed(size);
        if let Some(p) = progress {
            p.on_bytes(size as u64);
        }

        let ext = path
            .extension()
            .map(|e| e.to_string_lossy().into_owned())
            .unwrap_or_default();
        let nf = NewFile {
            volume_id: identity.volume_id.clone(),
            relative_path: rel.clone(),
            filename: name.to_string_lossy().into_owned(),
            extension: ext.clone(),
            size_bytes: size,
            content_hash: hash,
            created_time: unix_secs(meta.created()),
            modified_time: mtime,
            accessed_time: unix_secs(meta.accessed()),
            category: Category::from_extension(&ext),
            container_chain: None,
        };
        {
            let _t = metrics.timer(crate::scan_metrics::Phase::DbWrite);
            cat.upsert_file(&nf, now)?;
            in_batch += 1;
            batch_bytes += size as u64;
            rotate_batch(cat, &mut in_batch, &mut batch_bytes)?;
        }
        summary.hashed += 1;
        if let Some(p) = progress {
            p.on_hashed();
        }

        // By content, not by name. Cheap here because the file has just been hashed, so it is warm
        // in the OS cache -- unlike the skip path above, which must never open anything.
        //
        // The head-magic check alone misses a prefixed/self-extracting zip (real readers locate the
        // central directory from the END of the file, not the start). The extension is used as a
        // HINT to do that extra tail read, never as the decision: `._Video.zip` (AppleDouble) also
        // reaches the tail check, finds no EOCD signature, and is correctly left a leaf.
        //
        // 7z has no such prefixed/self-extracting variant to worry about here: its signature is
        // always the first six bytes, so the head check alone is enough and there is no equivalent
        // tail fallback to gate on an extension hint.
        let is_archive = {
            let ext_looks_like_zip = path
                .extension()
                .map(|e| e.eq_ignore_ascii_case("zip"))
                .unwrap_or(false);
            let ext_looks_like_7z = path
                .extension()
                .map(|e| e.eq_ignore_ascii_case("7z"))
                .unwrap_or(false);
            let detected = open_for_archive_detection(path).and_then(|mut f| {
                let (_head, head_is_zip, head_is_7z) = archive::peek6(&mut f)?;
                if head_is_zip || head_is_7z {
                    return Ok(true);
                }
                if ext_looks_like_zip {
                    return archive::tail_has_eocd_signature(&mut f);
                }
                Ok(false)
            });
            match detected {
                Ok(v) => v,
                Err(e) => {
                    // A detection failure must never be silently indistinguishable from "not an
                    // archive": that would leave `descend_archive` unrun with no error logged, and
                    // the entries inside would be swept to `missing` with no way to self-heal on a
                    // later clean rescan (the archive's own row stays `active`, so the revive floor
                    // is `None`). Log it so #6's completeness audit surfaces it.
                    cat.log_scan_error(
                        Some(&identity.volume_id),
                        &rel,
                        &format!("archive detection: {e}"),
                        "read",
                        crate::catalog::scan_errors::classify_io(&e),
                        now,
                    )?;
                    summary.errors += 1;
                    if let Some(p) = progress {
                        p.on_error();
                    }
                    // Fall back on what the catalogue already knew BEFORE this scan touched this
                    // path (`prior_meta`, captured above), not on the filename: it is right for a
                    // renamed zip and for a .docx alike. Only when there is no prior row at all (a
                    // new file we could not open) does the extension remain the last resort.
                    match prior_meta {
                        Some((_, _, has_archive_entries, _)) => has_archive_entries,
                        None => ext_looks_like_zip || ext_looks_like_7z,
                    }
                }
            }
        };
        if is_archive {
            let descent_ext = path
                .extension()
                .map(|e| e.to_string_lossy().to_ascii_lowercase())
                .unwrap_or_default();
            match archive::descent_for(
                &descent_ext,
                &limits.deny_extensions,
                &limits.allow_extensions,
            ) {
                archive::Descent::Descend => {
                    let _t = metrics.timer(crate::scan_metrics::Phase::Archive);
                    descend_archive(
                        cat,
                        path,
                        &rel,
                        mtime,
                        identity,
                        limits,
                        now,
                        &mut summary,
                        &mut in_batch,
                        &mut batch_bytes,
                        progress,
                    )?;
                }
                archive::Descent::Leaf => {}
                archive::Descent::Unrecognised => {
                    // Left whole AND recorded: the user decides, the scanner does not guess.
                    cat.record_pending_format(&identity.volume_id, &rel, &descent_ext, size, now)?;
                }
            }
        }
    }

    // The final COMMIT and the missing-sweep are both real scan cost and both hit SQLite, so they
    // belong to db_write. Leaving them untimed would inflate the unaccounted gap and understate
    // exactly the fsync cost #26 targets.
    {
        let _t = metrics.timer(crate::scan_metrics::Phase::DbWrite);
        cat.conn.execute_batch("COMMIT")?;
        // THE rule: a scan that did not finish never sweeps. Every file the walk had not reached
        // yet looks untouched, so sweeping here would mark present files as missing.
        if !summary.stopped {
            summary.marked_missing = cat.mark_missing_scanned(
                &identity.volume_id,
                scan_started_at,
                now,
                &unreadable_dirs,
            )?;
        }
        // Outside the sweep guard on purpose: the file rule is keyed on `last_seen_at`, so a
        // stopped scan clears only paths it actually re-reached. The directory rule is the part
        // that needs a completed scan, and `completed` carries that.
        cat.clear_resolved_scan_errors(&identity.volume_id, scan_started_at, !summary.stopped)?;
    }
    summary.metrics = metrics.snapshot();
    Ok(summary)
}

/// Scan without progress reporting (CLI and tests). Delegates with `None`.
pub fn scan_volume(
    cat: &Catalog,
    root: &Path,
    identity: &VolumeIdentity,
    force: bool,
    now: i64,
    limits: &ArchiveLimits,
) -> anyhow::Result<ScanSummary> {
    let metrics = crate::scan_metrics::ScanMetrics::new();
    scan_volume_with_progress(
        cat,
        root,
        identity,
        force,
        now,
        None,
        &metrics,
        &crate::scan_control::StopFlag::new(),
        limits,
    )
}

/// Resolve identity, upsert the volume, and scan. `Ok(None)` iff a read-only drive was skipped.
///
/// The single shared definition of "how a scan works" — used by both the CLI's `cmd_scan` and
/// the web worker, so the two callers can never drift apart on volume-identity/upsert semantics.
#[allow(
    clippy::too_many_arguments,
    reason = "each parameter is an independent scan input; grouping them into a struct would add \
        indirection without reducing real complexity"
)]
pub fn run_scan(
    cat: &Catalog,
    mount_root: &Path,
    force: bool,
    fallback: crate::volume::ReadonlyMode,
    now: i64,
    progress: Option<&dyn Progress>,
    stop: &crate::scan_control::StopFlag,
    limits: &ArchiveLimits,
) -> anyhow::Result<Option<(VolumeIdentity, ScanSummary)>> {
    let identity = match crate::volume::resolve(mount_root, fallback)? {
        Some(id) => id,
        None => return Ok(None),
    };
    tracing::info!(volume = %identity.volume_id, label = %identity.label,
        identified_by = %identity.identified_by, "scanning volume");
    cat.upsert_volume(&Volume {
        volume_id: identity.volume_id.clone(),
        label: identity.label.clone(),
        identified_by: identity.identified_by.clone(),
        first_seen_at: now,
        last_seen_at: now,
    })?;
    // Remember where this volume was scanned so a folder-drive (not a disk root) can be recognized
    // as connected later. Best-effort: a bookkeeping failure must not fail the scan.
    let _ = cat.set_volume_path(&identity.volume_id, &mount_root.display().to_string(), now);

    // Best-effort throughout: a bookkeeping failure must never fail a scan. Started before the
    // scan opens its transaction, so the 'running' row is committed immediately and an
    // interrupted multi-day scan leaves a record.
    let run_id = cat
        .start_scan_run(
            Some(&identity.volume_id),
            &mount_root.display().to_string(),
            now,
            force,
        )
        .map_err(|e| tracing::warn!("could not record scan start: {e}"))
        .ok();

    // Beats for as long as this function runs, so a hard-killed scan stops looking alive (#36).
    // Held in a binding rather than dropped immediately -- `let _ =` would end it at once. Drop
    // covers the normal, error and panic paths; only a hard kill leaves the file behind, which is
    // precisely the signal.
    let _heartbeat = run_id
        .zip(cat.db_path())
        .map(|(id, db)| crate::scan_heartbeat::Heartbeat::start(&db, id));

    // Owned here, not inside the scan, so a scan that bails part-way still reports what it
    // measured before it died.
    let metrics = crate::scan_metrics::ScanMetrics::new();
    let result = scan_volume_with_progress(
        cat, mount_root, &identity, force, now, progress, &metrics, stop, limits,
    );

    if result.is_err() {
        // The scan bailed with its BEGIN still open; end it so the metrics UPDATE below is its
        // own transaction and survives. Nothing durable is lost -- it would have been rolled
        // back at connection close anyway.
        let _ = cat.conn.execute_batch("ROLLBACK");
    }

    if let Some(id) = run_id {
        let finished_at = crate::commands::now_secs();
        let outcome = match &result {
            Ok(summary) => {
                let status = if summary.stopped {
                    "cancelled"
                } else {
                    "completed"
                };
                cat.finish_scan_run(id, finished_at, status, None, summary)
            }
            Err(e) => {
                let msg = e.to_string();
                let partial = ScanSummary {
                    metrics: metrics.snapshot(),
                    ..Default::default()
                };
                cat.finish_scan_run(id, finished_at, "failed", Some(&msg), &partial)
            }
        };
        if let Err(e) = outcome {
            tracing::warn!("could not record scan result: {e}");
        }
    }

    let summary = result?;
    // Audit trail: one row per completed scan so the Overview "recent activity" feed can show it.
    let _ = cat.log_action(
        "scan",
        &serde_json::json!({
            "volume_id": identity.volume_id, "label": identity.label,
            "hashed": summary.hashed, "skipped": summary.skipped, "errors": summary.errors,
            "marked_missing": summary.marked_missing, "archive_entries": summary.archive_entries,
        })
        .to_string(),
        now,
    );
    Ok(Some((identity, summary)))
}

/// Open an on-disk archive file, catalog each entry, and log each non-fatal error.
#[allow(
    clippy::too_many_arguments,
    reason = "each parameter is an independent scan input; grouping them into a struct would add \
        indirection without reducing real complexity"
)]
fn descend_archive(
    cat: &Catalog,
    path: &Path,
    rel: &str,
    archive_mtime: Option<i64>,
    identity: &VolumeIdentity,
    limits: &ArchiveLimits,
    now: i64,
    summary: &mut ScanSummary,
    in_batch: &mut usize,
    batch_bytes: &mut u64,
    progress: Option<&dyn Progress>,
) -> anyhow::Result<()> {
    let file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) => {
            cat.log_scan_error(
                Some(&identity.volume_id),
                rel,
                &format!("archive open: {e}"),
                "archive_open",
                crate::catalog::scan_errors::classify_io(&e),
                now,
            )?;
            summary.errors += 1;
            if let Some(p) = progress {
                p.on_error();
            }
            return Ok(());
        }
    };
    let res = archive::scan_archive(file, limits);
    for entry in &res.entries {
        cat.upsert_archive_entry(&identity.volume_id, rel, entry, archive_mtime, now)?;
        summary.archive_entries += 1;
        if let Some(p) = progress {
            p.on_archive_entry();
        }
        *in_batch += 1;
        *batch_bytes += entry.size_bytes as u64;
        rotate_batch(cat, in_batch, batch_bytes)?;
    }
    for (ctx, reason) in &res.errors {
        let where_ = if ctx.is_empty() {
            rel.to_string()
        } else {
            format!("{rel} › {ctx}")
        };
        // `reason` comes from the zip crate, not an io::Error, so there is no ErrorKind to read.
        // Parsing the string is exactly what classification exists to avoid.
        cat.log_scan_error(
            Some(&identity.volume_id),
            &where_,
            reason,
            "archive_entry",
            "other",
            now,
        )?;
        summary.errors += 1;
        if let Some(p) = progress {
            p.on_error();
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::catalog::models::Volume;
    use crate::catalog::Catalog;
    use crate::volume::VolumeIdentity;
    use std::fs;

    fn ident() -> VolumeIdentity {
        VolumeIdentity {
            volume_id: "vol-1".into(),
            label: "T".into(),
            identified_by: "marker".into(),
        }
    }

    /// Limits for tests: the compiled-in defaults, with NO ambient environment read.
    fn test_limits() -> crate::archive::ArchiveLimits {
        crate::archive::ArchiveLimits {
            max_depth: 8,
            buffer_max_bytes: 2 * 1024 * 1024 * 1024,
            total_buffer_bytes: 2 * 1024 * 1024 * 1024,
            entry_max_bytes: Some(64 * 1024 * 1024 * 1024),
            ratio_cap: 10_000,
            deny_extensions: crate::config::DEFAULT_DENY
                .iter()
                .map(|s| s.to_string())
                .collect(),
            allow_extensions: Vec::new(),
        }
    }

    fn setup() -> (tempfile::TempDir, Catalog) {
        let tmp = tempfile::tempdir().unwrap();
        let cat = Catalog::open(&tmp.path().join("c.db")).unwrap();
        cat.upsert_volume(&Volume {
            volume_id: "vol-1".into(),
            label: "T".into(),
            identified_by: "marker".into(),
            first_seen_at: 1,
            last_seen_at: 1,
        })
        .unwrap();
        (tmp, cat)
    }

    /// Build a stored (uncompressed) zip in memory, for tests that write archive bytes to disk.
    fn make_zip(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut buf = std::io::Cursor::new(Vec::new());
        {
            let mut zw = zip::ZipWriter::new(&mut buf);
            let opts: zip::write::FileOptions<()> = zip::write::FileOptions::default()
                .compression_method(zip::CompressionMethod::Stored);
            for (name, bytes) in files {
                zw.start_file(*name, opts).unwrap();
                std::io::Write::write_all(&mut zw, bytes).unwrap();
            }
            zw.finish().unwrap();
        }
        buf.into_inner()
    }

    /// The status of the one archive entry with this filename, catalogued anywhere.
    fn status_of(cat: &Catalog, name: &str) -> String {
        cat.conn
            .query_row(
                "SELECT status FROM files WHERE filename=?1 AND container_chain IS NOT NULL",
                [name],
                |r| r.get(0),
            )
            .unwrap()
    }

    // A real scan over a file that genuinely fails to read, asserting the scanner itself (not the
    // catalogue layer) records the correct `kind` for its `"read"` call site. Platform-gated because
    // the honest way to make a read fail differs per OS, and CI runs both Windows and macOS.

    #[cfg(windows)]
    #[test]
    fn a_locked_file_is_recorded_with_the_read_phase_and_locked_kind() {
        use std::os::windows::fs::OpenOptionsExt;

        let (tmp, cat) = setup();
        let root = tmp.path().join("drive");
        std::fs::create_dir_all(&root).unwrap();
        let victim = root.join("locked.bin");
        std::fs::write(&victim, b"data").unwrap();

        // Hold the file open with an exclusive share mode (share_mode(0) = no sharing at all), so
        // the scanner's own open-for-hashing fails with ERROR_SHARING_VIOLATION (raw OS error 32)
        // for the duration of the scan.
        let _held = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&victim)
            .unwrap();

        let m = crate::scan_metrics::ScanMetrics::new();
        let stop = crate::scan_control::StopFlag::new();
        scan_volume_with_progress(
            &cat,
            &root,
            &ident(),
            false,
            100,
            None,
            &m,
            &stop,
            &test_limits(),
        )
        .unwrap();

        let (phase, kind): (String, String) = cat
            .conn
            .query_row(
                "SELECT phase, kind FROM scan_errors WHERE path='locked.bin'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(phase, "read");
        assert_eq!(kind, "locked");
    }

    #[cfg(unix)]
    #[test]
    fn an_unreadable_file_is_recorded_with_the_read_phase_and_permission_kind() {
        use std::os::unix::fs::PermissionsExt;

        let (tmp, cat) = setup();
        let root = tmp.path().join("drive");
        std::fs::create_dir_all(&root).unwrap();
        let victim = root.join("noperm.bin");
        std::fs::write(&victim, b"data").unwrap();
        let orig_perms = std::fs::metadata(&victim).unwrap().permissions();
        std::fs::set_permissions(&victim, std::fs::Permissions::from_mode(0o000)).unwrap();

        let m = crate::scan_metrics::ScanMetrics::new();
        let stop = crate::scan_control::StopFlag::new();
        let result = scan_volume_with_progress(
            &cat,
            &root,
            &ident(),
            false,
            100,
            None,
            &m,
            &stop,
            &test_limits(),
        );

        // Restore permissions before any assertion can early-return/panic, so the tempdir can
        // still be cleaned up regardless of outcome.
        std::fs::set_permissions(&victim, orig_perms).unwrap();
        result.unwrap();

        let (phase, kind): (String, String) = cat
            .conn
            .query_row(
                "SELECT phase, kind FROM scan_errors WHERE path='noperm.bin'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(phase, "read");
        assert_eq!(kind, "permission");
    }

    // Regression for the false-"complete" bug: a file catalogued by an earlier scan, then unreadable
    // on a re-scan, must land in `unverified` -- not have its own fresh error silently deleted by
    // the same scan's self-heal, which would leave the catalogue reporting "complete" over a file
    // whose stored hash was never re-verified. Drives a real scan twice (not the catalogue layer
    // directly) so the reproduction matches what actually happens in production: `touch_seen` bumps
    // `last_seen_at` to `now` on the error path, which equals `scan_started_at`.

    #[cfg(windows)]
    #[test]
    fn a_file_that_becomes_locked_after_being_catalogued_is_unverified_not_complete() {
        use std::os::windows::fs::OpenOptionsExt;

        let (tmp, cat) = setup();
        let root = tmp.path().join("drive");
        std::fs::create_dir_all(&root).unwrap();
        let victim = root.join("locked.bin");
        std::fs::write(&victim, b"data").unwrap();

        // First scan: the file is readable, so it gets catalogued normally.
        let m1 = crate::scan_metrics::ScanMetrics::new();
        let stop1 = crate::scan_control::StopFlag::new();
        let s1 = scan_volume_with_progress(
            &cat,
            &root,
            &ident(),
            false,
            100,
            None,
            &m1,
            &stop1,
            &test_limits(),
        )
        .unwrap();
        assert_eq!(s1.errors, 0);

        // Second scan: the file is now locked, so the re-read fails. `force=true` so the unchanged
        // fast-path (which never re-reads) doesn't hide the point of the test.
        let _held = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(0)
            .open(&victim)
            .unwrap();
        let m2 = crate::scan_metrics::ScanMetrics::new();
        let stop2 = crate::scan_control::StopFlag::new();
        let s2 = scan_volume_with_progress(
            &cat,
            &root,
            &ident(),
            true,
            200,
            None,
            &m2,
            &stop2,
            &test_limits(),
        )
        .unwrap();
        assert_eq!(s2.errors, 1, "the scan itself must count the failure");

        let c = cat.volume_completeness("vol-1").unwrap();
        assert_eq!(
            c.unverified, 1,
            "the error the scan just recorded must not be erased by its own self-heal"
        );
        assert_eq!(c.absent, 0);
        assert_eq!(c.unreadable_dirs, 0);
        assert_ne!(
            c.summary_line(),
            "Completeness: complete.",
            "a catalogue holding an unverified file must never report complete"
        );
    }

    #[cfg(unix)]
    #[test]
    fn a_file_that_becomes_unreadable_after_being_catalogued_is_unverified_not_complete() {
        use std::os::unix::fs::PermissionsExt;

        let (tmp, cat) = setup();
        let root = tmp.path().join("drive");
        std::fs::create_dir_all(&root).unwrap();
        let victim = root.join("noperm.bin");
        std::fs::write(&victim, b"data").unwrap();

        // First scan: the file is readable, so it gets catalogued normally.
        let m1 = crate::scan_metrics::ScanMetrics::new();
        let stop1 = crate::scan_control::StopFlag::new();
        let s1 = scan_volume_with_progress(
            &cat,
            &root,
            &ident(),
            false,
            100,
            None,
            &m1,
            &stop1,
            &test_limits(),
        )
        .unwrap();
        assert_eq!(s1.errors, 0);

        // Second scan: the file is now unreadable, so the re-read fails. `force=true` so the
        // unchanged fast-path (which never re-reads) doesn't hide the point of the test.
        let orig_perms = std::fs::metadata(&victim).unwrap().permissions();
        std::fs::set_permissions(&victim, std::fs::Permissions::from_mode(0o000)).unwrap();
        let m2 = crate::scan_metrics::ScanMetrics::new();
        let stop2 = crate::scan_control::StopFlag::new();
        let result = scan_volume_with_progress(
            &cat,
            &root,
            &ident(),
            true,
            200,
            None,
            &m2,
            &stop2,
            &test_limits(),
        );

        // Restore permissions before any assertion can early-return/panic, so the tempdir can
        // still be cleaned up regardless of outcome.
        std::fs::set_permissions(&victim, orig_perms).unwrap();
        let s2 = result.unwrap();
        assert_eq!(s2.errors, 1, "the scan itself must count the failure");

        let c = cat.volume_completeness("vol-1").unwrap();
        assert_eq!(
            c.unverified, 1,
            "the error the scan just recorded must not be erased by its own self-heal"
        );
        assert_eq!(c.absent, 0);
        assert_eq!(c.unreadable_dirs, 0);
        assert_ne!(
            c.summary_line(),
            "Completeness: complete.",
            "a catalogue holding an unverified file must never report complete"
        );
    }

    #[test]
    fn scans_hashes_and_reindex_skips() {
        let (tmp, cat) = setup();
        let root = tmp.path().join("drive");
        fs::create_dir_all(root.join("sub")).unwrap();
        fs::write(root.join("a.txt"), b"alpha").unwrap();
        fs::write(root.join("sub/b.txt"), b"beta").unwrap();

        let s1 = scan_volume(&cat, &root, &ident(), false, 100, &test_limits()).unwrap();
        assert_eq!(s1.hashed, 2);
        assert_eq!(s1.skipped, 0);

        // second scan: nothing changed -> both skipped (no re-hash)
        let s2 = scan_volume(&cat, &root, &ident(), false, 200, &test_limits()).unwrap();
        assert_eq!(s2.hashed, 0);
        assert_eq!(s2.skipped, 2);

        // both searchable
        assert_eq!(cat.search("a", None, None, None).unwrap().len(), 1);
    }

    /// Observes the heartbeat file from inside the scan, via the progress callback.
    struct HeartbeatWatcher {
        path: std::path::PathBuf,
        seen_alive: std::sync::atomic::AtomicBool,
    }
    impl Progress for HeartbeatWatcher {
        fn on_hashed(&self) {
            if self.path.exists() {
                self.seen_alive
                    .store(true, std::sync::atomic::Ordering::Relaxed);
            }
        }
        fn on_skipped(&self) {}
        fn on_error(&self) {}
        fn on_archive_entry(&self) {}
    }

    #[test]
    fn the_heartbeat_beats_for_the_whole_scan_and_is_gone_afterwards() {
        // Guards a one-character regression: changing `let _heartbeat = ..` to `let _ = ..` in
        // run_scan drops the Heartbeat immediately, which would silently report every scan as
        // interrupted two minutes in (#36). Nothing else notices -- the scan still succeeds, the
        // rows are still right -- so it has to be observed from INSIDE the scan.
        let tmp = tempfile::tempdir().unwrap();
        let db = tmp.path().join("c.db");
        let cat = Catalog::open(&db).unwrap();
        let root = tmp.path().join("drive");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join(".cleanupstorages_id"), "vol-1").unwrap();
        for i in 0..5 {
            fs::write(root.join(format!("f{i}.txt")), format!("data-{i}")).unwrap();
        }

        // run_id 1 is the first row this fresh catalogue writes.
        let watcher = HeartbeatWatcher {
            path: db.parent().unwrap().join("scan-heartbeats").join("1"),
            seen_alive: std::sync::atomic::AtomicBool::new(false),
        };
        let stop = crate::scan_control::StopFlag::new();
        run_scan(
            &cat,
            &root,
            false,
            crate::volume::ReadonlyMode::Fingerprint,
            100,
            Some(&watcher),
            &stop,
            &test_limits(),
        )
        .unwrap();

        assert!(
            watcher
                .seen_alive
                .load(std::sync::atomic::Ordering::Relaxed),
            "the heartbeat file must exist WHILE the scan runs"
        );
        assert!(
            !db.parent()
                .unwrap()
                .join("scan-heartbeats")
                .join("1")
                .exists(),
            "and must be removed on a clean finish, so only a hard kill leaves one behind"
        );
    }

    #[test]
    fn a_missing_loose_file_revives_when_it_comes_back() {
        // The other half of the #46 guard, end to end. touch_seen may now only move a row between
        // active and missing, and this is the case that guard must NOT break: a drive unplugged
        // mid-scan, or a file temporarily unavailable, leaves rows 'missing', and the next scan has
        // to bring them back. Getting this wrong would strand real files as permanently missing.
        let (tmp, cat) = setup();
        let root = tmp.path().join("drive");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join(".cleanupstorages_id"), "vol-1").unwrap();
        fs::write(root.join("a.txt"), b"HELLO").unwrap();
        let ident = crate::volume::VolumeIdentity {
            volume_id: "vol-1".into(),
            label: "T".into(),
            identified_by: "marker".into(),
        };
        scan_volume(&cat, &root, &ident, false, 100, &test_limits()).unwrap();

        fs::remove_file(root.join("a.txt")).unwrap();
        scan_volume(&cat, &root, &ident, false, 200, &test_limits()).unwrap();
        let status: String = cat
            .conn
            .query_row(
                "SELECT status FROM files WHERE relative_path='a.txt'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(status, "missing", "precondition: the file went missing");

        fs::write(root.join("a.txt"), b"HELLO").unwrap();
        scan_volume(&cat, &root, &ident, false, 300, &test_limits()).unwrap();
        let status: String = cat
            .conn
            .query_row(
                "SELECT status FROM files WHERE relative_path='a.txt'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            status, "active",
            "a returning file MUST revive; the quarantine guard must not strand it"
        );
    }

    #[test]
    fn a_deletion_is_swept_even_when_two_scans_share_a_second() {
        // #45: the sweep compares last_seen_at < scan_started_at, and the stamp came from
        // second-resolution wall-clock time. Two scans in the same second made `t < t` false, so a
        // file deleted in between stayed stale-active -- the catalogue asserting a file is present
        // when it is gone. That is the UNSAFE direction: dedup can then offer it as the safe copy
        // to keep while the user quarantines a real one.
        let (tmp, cat) = setup();
        let root = tmp.path().join("drive");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join(".cleanupstorages_id"), "vol-1").unwrap();
        fs::write(root.join("gone.txt"), b"DATA").unwrap();
        let ident = crate::volume::VolumeIdentity {
            volume_id: "vol-1".into(),
            label: "T".into(),
            identified_by: "marker".into(),
        };
        scan_volume(&cat, &root, &ident, false, 1000, &test_limits()).unwrap();

        fs::remove_file(root.join("gone.txt")).unwrap();
        // The SAME wall-clock second as the first scan.
        let s = scan_volume(&cat, &root, &ident, false, 1000, &test_limits()).unwrap();

        let status: String = cat
            .conn
            .query_row(
                "SELECT status FROM files WHERE relative_path='gone.txt'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            status, "missing",
            "a deleted file must be swept, not left active"
        );
        assert_eq!(s.marked_missing, 1);
    }

    #[test]
    fn a_deletion_is_swept_even_when_the_clock_moves_backwards() {
        // The other half of #45: NTP correction, a manual change, dual-boot, or a drive carried to
        // another machine. Last seen at 2000, rescanned with a stamp of 1500: 2000 < 1500 is false,
        // so the vanish was never registered.
        let (tmp, cat) = setup();
        let root = tmp.path().join("drive");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join(".cleanupstorages_id"), "vol-1").unwrap();
        fs::write(root.join("gone.txt"), b"DATA").unwrap();
        let ident = crate::volume::VolumeIdentity {
            volume_id: "vol-1".into(),
            label: "T".into(),
            identified_by: "marker".into(),
        };
        scan_volume(&cat, &root, &ident, false, 2000, &test_limits()).unwrap();

        fs::remove_file(root.join("gone.txt")).unwrap();
        let s = scan_volume(&cat, &root, &ident, false, 1500, &test_limits()).unwrap();

        let status: String = cat
            .conn
            .query_row(
                "SELECT status FROM files WHERE relative_path='gone.txt'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            status, "missing",
            "a clock moving backwards must not hide a deletion"
        );
        assert_eq!(s.marked_missing, 1);
    }

    #[test]
    fn a_present_file_is_never_swept_by_the_scan_that_just_saw_it() {
        // The failure mode of an over-eager fix: changing `<` to `<=` would sweep files the current
        // scan touched in the same second. Present files must survive every scan, always.
        let (tmp, cat) = setup();
        let root = tmp.path().join("drive");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join(".cleanupstorages_id"), "vol-1").unwrap();
        fs::write(root.join("here.txt"), b"DATA").unwrap();
        let ident = crate::volume::VolumeIdentity {
            volume_id: "vol-1".into(),
            label: "T".into(),
            identified_by: "marker".into(),
        };
        for stamp in [1000, 1000, 1000, 900] {
            let s = scan_volume(&cat, &root, &ident, false, stamp, &test_limits()).unwrap();
            assert_eq!(s.marked_missing, 0, "stamp {stamp} swept a present file");
        }
        let status: String = cat
            .conn
            .query_row(
                "SELECT status FROM files WHERE relative_path='here.txt'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(status, "active");
    }

    #[test]
    fn system_directories_are_not_catalogued_and_existing_rows_are_dropped() {
        // On the real drives 77,493 $RECYCLE.BIN rows were catalogued. Because `$` sorts before
        // every letter they filled the whole Browse page, which showed nothing else, and they
        // formed 85 duplicate groups made entirely of files Windows had already deleted.
        let (tmp, cat) = setup();
        let root = tmp.path().join("drive");
        fs::create_dir_all(root.join("$RECYCLE.BIN/S-1-5-21")).unwrap();
        fs::create_dir_all(root.join("System Volume Information")).unwrap();
        fs::create_dir_all(root.join("Photos")).unwrap();
        fs::write(root.join(".cleanupstorages_id"), "vol-1").unwrap();
        fs::write(root.join("$RECYCLE.BIN/S-1-5-21/$RABC.txt"), b"deleted").unwrap();
        fs::write(root.join("System Volume Information/tracking.log"), b"sys").unwrap();
        fs::write(root.join("Photos/keep.jpg"), b"REAL").unwrap();

        // A row that predates the rule, exactly as the live catalogue had.
        cat.upsert_file(
            &NewFile {
                volume_id: "vol-1".into(),
                relative_path: "$RECYCLE.BIN/S-1-5-21/old.bin".into(),
                filename: "old.bin".into(),
                extension: "bin".into(),
                size_bytes: 9,
                content_hash: "OLD".into(),
                created_time: None,
                modified_time: None,
                accessed_time: None,
                category: Category::Other,
                container_chain: None,
            },
            100,
        )
        .unwrap();

        let ident = crate::volume::VolumeIdentity {
            volume_id: "vol-1".into(),
            label: "T".into(),
            identified_by: "marker".into(),
        };
        let s = scan_volume(&cat, &root, &ident, false, 200, &test_limits()).unwrap();

        let paths: Vec<String> = cat
            .conn
            .prepare("SELECT relative_path FROM files")
            .unwrap()
            .query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(
            paths,
            vec!["Photos/keep.jpg".to_string()],
            "only real data may be catalogued; system dirs and their old rows must be gone"
        );
        assert_eq!(
            s.marked_missing, 0,
            "the dropped rows must NOT be reported as missing -- that is an alarm about files              Windows already deleted"
        );
    }

    #[test]
    fn deleted_file_becomes_missing() {
        let (tmp, cat) = setup();
        let root = tmp.path().join("drive");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("keep.txt"), b"x").unwrap();
        fs::write(root.join("gone.txt"), b"y").unwrap();
        scan_volume(&cat, &root, &ident(), false, 100, &test_limits()).unwrap();

        fs::remove_file(root.join("gone.txt")).unwrap();
        let s = scan_volume(&cat, &root, &ident(), false, 200, &test_limits()).unwrap();
        assert_eq!(s.marked_missing, 1);
        assert_eq!(
            cat.search("gone", None, None, Some("missing"))
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            cat.search("keep", None, None, Some("active"))
                .unwrap()
                .len(),
            1
        );
    }

    use std::io::Write as _;

    fn write_zip_file(path: &std::path::Path, files: &[(&str, &[u8])]) {
        let f = fs::File::create(path).unwrap();
        let mut zw = zip::ZipWriter::new(f);
        let opts: zip::write::FileOptions<()> =
            zip::write::FileOptions::default().compression_method(zip::CompressionMethod::Stored);
        for (name, bytes) in files {
            zw.start_file(*name, opts).unwrap();
            zw.write_all(bytes).unwrap();
        }
        zw.finish().unwrap();
    }

    #[test]
    fn scan_catalogs_archive_entries() {
        let (tmp, cat) = setup();
        let root = tmp.path().join("drive");
        fs::create_dir_all(&root).unwrap();
        write_zip_file(
            &root.join("photos.zip"),
            &[("trip/beach.jpg", b"sand"), ("note.txt", b"hi")],
        );

        let s = scan_volume(&cat, &root, &ident(), false, 100, &test_limits()).unwrap();
        // the zip file itself is a loose hashed file
        assert_eq!(s.hashed, 1);
        // its two entries are catalogued
        assert_eq!(s.archive_entries, 2);
        // inner file is searchable, with its container chain
        let hits = cat.search("beach", None, None, None).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].relative_path, "photos.zip");
        assert_eq!(hits[0].container_chain.as_deref(), Some("trip/beach.jpg"));
    }

    #[test]
    fn unchanged_archive_entries_survive_rescan() {
        let (tmp, cat) = setup();
        let root = tmp.path().join("drive");
        fs::create_dir_all(&root).unwrap();
        write_zip_file(&root.join("a.zip"), &[("x.txt", b"one")]);
        scan_volume(&cat, &root, &ident(), false, 100, &test_limits()).unwrap();

        // rescan unchanged: archive is skipped, but its entry must NOT be swept to missing
        let s = scan_volume(&cat, &root, &ident(), false, 200, &test_limits()).unwrap();
        assert_eq!(s.marked_missing, 0);
        assert_eq!(
            cat.search("x", None, None, Some("active")).unwrap().len(),
            1
        );
    }

    struct CountingProgress {
        hashed: std::sync::atomic::AtomicUsize,
        skipped: std::sync::atomic::AtomicUsize,
        errors: std::sync::atomic::AtomicUsize,
        arch: std::sync::atomic::AtomicUsize,
    }
    impl CountingProgress {
        fn new() -> Self {
            Self {
                hashed: 0.into(),
                skipped: 0.into(),
                errors: 0.into(),
                arch: 0.into(),
            }
        }
    }
    impl Progress for CountingProgress {
        fn on_hashed(&self) {
            self.hashed
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        fn on_skipped(&self) {
            self.skipped
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        fn on_error(&self) {
            self.errors
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
        fn on_archive_entry(&self) {
            self.arch.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
    }

    #[test]
    fn run_scan_resolves_upserts_and_scans() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("drive");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("x.txt"), b"hello").unwrap();
        let cat = Catalog::open(&tmp.path().join("c.db")).unwrap();

        let out = run_scan(
            &cat,
            &root,
            false,
            crate::volume::ReadonlyMode::Fingerprint,
            100,
            None,
            &crate::scan_control::StopFlag::new(),
            &test_limits(),
        )
        .unwrap();
        let (identity, summary) = out.expect("not skipped");
        assert_eq!(summary.hashed, 1);
        // the volume row exists after run_scan upserted it
        let stats = cat.volume_stats().unwrap();
        assert!(stats.iter().any(|(id, _, _, _)| id == &identity.volume_id));
    }

    #[test]
    fn run_scan_records_the_resolved_volume() {
        // Was `run_scan_logs_volume_resolution`, which captured tracing output and failed
        // intermittently under parallel execution (#39). tracing's max level is process-GLOBAL --
        // the maximum over registered subscribers -- and a thread-local default does not raise it,
        // so while other tests ran the `info!` could be filtered out before reaching this test's
        // subscriber. The assertion then failed on an empty string rather than on wrong content.
        //
        // A test that fails randomly teaches people to re-run red builds instead of reading them,
        // which on this project means the next real failure gets waved through as "the flaky one".
        //
        // So it asserts the same facts where they are actually durable. The log line reports
        // volume_id, label and identified_by, and `upsert_volume` persists exactly those on the
        // next statement -- with no dependence on global logging state.
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("drive");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("x.txt"), b"hi").unwrap();
        let cat = Catalog::open(&tmp.path().join("c.db")).unwrap();

        let out = run_scan(
            &cat,
            &root,
            false,
            crate::volume::ReadonlyMode::Fingerprint,
            100,
            None,
            &crate::scan_control::StopFlag::new(),
            &test_limits(),
        )
        .unwrap();

        let (identity, _summary) = out.expect("a writable drive must resolve to a volume");
        let (stored_label, identified_by): (String, String) = cat
            .conn
            .query_row(
                "SELECT label, identified_by FROM volumes WHERE volume_id=?1",
                [&identity.volume_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .expect("the resolved volume must be recorded in the catalogue");
        assert_eq!(stored_label, identity.label);
        assert_eq!(identified_by, identity.identified_by);
        assert!(
            !identity.volume_id.is_empty(),
            "a resolved volume needs an id"
        );
    }

    #[test]
    fn run_scan_logs_a_scan_action() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("drive");
        fs::create_dir_all(&root).unwrap();
        fs::write(root.join("x.txt"), b"hello").unwrap();
        let cat = Catalog::open(&tmp.path().join("c.db")).unwrap();

        let n = run_scan(
            &cat,
            &root,
            false,
            crate::volume::ReadonlyMode::Fingerprint,
            1234,
            None,
            &crate::scan_control::StopFlag::new(),
            &test_limits(),
        )
        .unwrap();
        assert!(n.is_some());
        let acts = cat.recent_actions(10).unwrap();
        assert!(acts
            .iter()
            .any(|(a, d, t)| a == "scan" && *t == 1234 && d.contains("\"hashed\"")));
    }

    #[test]
    fn progress_callbacks_match_summary() {
        use std::sync::atomic::Ordering::Relaxed;
        let (tmp, cat) = setup();
        let root = tmp.path().join("drive");
        fs::create_dir_all(root.join("sub")).unwrap();
        fs::write(root.join("a.txt"), b"alpha").unwrap();
        fs::write(root.join("sub/b.txt"), b"beta").unwrap();

        let p = CountingProgress::new();
        let m = crate::scan_metrics::ScanMetrics::new();
        let s = scan_volume_with_progress(
            &cat,
            &root,
            &ident(),
            false,
            100,
            Some(&p),
            &m,
            &crate::scan_control::StopFlag::new(),
            &test_limits(),
        )
        .unwrap();
        assert_eq!(p.hashed.load(Relaxed), s.hashed);
        assert_eq!(p.skipped.load(Relaxed), s.skipped);
        assert_eq!(p.errors.load(Relaxed), s.errors);
        assert_eq!(p.arch.load(Relaxed), s.archive_entries);
        assert_eq!(s.hashed, 2);
    }

    /// A temp dir containing `files` (name, byte length), plus an open catalog with the `ident()`
    /// volume already upserted (the `files` table's `volume_id` is FK-enforced).
    fn fixture_with_files(
        files: &[(&str, usize)],
    ) -> (tempfile::TempDir, Catalog, std::path::PathBuf) {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().join("drive");
        std::fs::create_dir_all(&root).unwrap();
        for (name, len) in files {
            std::fs::write(root.join(name), vec![b'x'; *len]).unwrap();
        }
        let cat = Catalog::open(&t.path().join("c.db")).unwrap();
        cat.upsert_volume(&Volume {
            volume_id: "vol-1".into(),
            label: "T".into(),
            identified_by: "marker".into(),
            first_seen_at: 1,
            last_seen_at: 1,
        })
        .unwrap();
        (t, cat, root)
    }

    #[test]
    fn scan_records_phase_timings_and_the_size_histogram() {
        let (_t, cat, root) = fixture_with_files(&[("a.txt", 10), ("big.bin", 5000)]);
        let s = scan_volume(&cat, &root, &ident(), false, 100, &test_limits()).unwrap();
        let m = &s.metrics;

        assert_eq!(m.files_seen, 2);
        assert_eq!(m.histogram[1], 1, "the 10-byte file");
        assert_eq!(m.histogram[2], 1, "the 5000-byte file");
        assert_eq!(m.bytes_hashed, 5010);
        assert_eq!(m.bytes_skipped, 0);
        // Upper bound only: on a fast disk these phases legitimately round to 0 ms. That the
        // timers accumulate at all is proven in scan_metrics with a controlled sleep.
        assert!(
            m.total_phase_ms() <= m.wall_ms,
            "phases {} exceeded wall {}",
            m.total_phase_ms(),
            m.wall_ms
        );
    }

    #[test]
    fn rescan_attributes_bytes_to_skipped_not_hashed() {
        let (_t, cat, root) = fixture_with_files(&[("a.txt", 10), ("b.txt", 20)]);
        scan_volume(&cat, &root, &ident(), false, 100, &test_limits()).unwrap();
        let s = scan_volume(&cat, &root, &ident(), false, 200, &test_limits()).unwrap();

        assert_eq!(s.skipped, 2, "second pass takes the incremental-skip path");
        assert_eq!(s.metrics.bytes_hashed, 0);
        assert_eq!(s.metrics.bytes_skipped, 30);
        assert_eq!(s.metrics.files_seen, 2, "skipped files are still 'seen'");
        assert_eq!(s.metrics.histogram[1], 2);
    }

    #[test]
    fn run_scan_records_a_completed_run() {
        let (_t, cat, root) = fixture_with_files(&[("a.txt", 10)]);
        let out = run_scan(
            &cat,
            &root,
            false,
            crate::volume::ReadonlyMode::Fingerprint,
            100,
            None,
            &crate::scan_control::StopFlag::new(),
            &test_limits(),
        )
        .unwrap();
        assert!(out.is_some());

        let runs = cat.recent_scan_runs(10).unwrap();
        assert_eq!(runs.len(), 1, "exactly one row per scan, not one per file");
        assert_eq!(runs[0].status, "completed");
        assert!(runs[0].finished_at.is_some());
        assert_eq!(runs[0].hashed, 1);
        assert_eq!(runs[0].metrics.files_seen, 1);
        assert!(!runs[0].root_path.is_empty());
    }

    #[test]
    fn a_failed_scan_records_failed_with_its_error_and_its_partial_metrics() {
        let (t, cat, root) = fixture_with_files(&[("a.txt", 10)]);
        let db = t.path().join("c.db");
        // Abort the very first file insert. RAISE(ABORT) undoes the statement but leaves the
        // scan's BEGIN open -- the exact shape that used to swallow the 'failed' row.
        cat.conn
            .execute_batch(
                "CREATE TRIGGER boom BEFORE INSERT ON files
                 BEGIN SELECT RAISE(ABORT, 'induced scan failure'); END",
            )
            .unwrap();

        let out = run_scan(
            &cat,
            &root,
            false,
            crate::volume::ReadonlyMode::Fingerprint,
            100,
            None,
            &crate::scan_control::StopFlag::new(),
            &test_limits(),
        );
        assert!(out.is_err(), "the induced trigger must fail the scan");
        drop(cat);

        // A fresh connection is the point: reading on the scan's own connection would see the
        // update inside its abandoned transaction and pass spuriously.
        let fresh = Catalog::open(&db).unwrap();
        let runs = fresh.recent_scan_runs(10).unwrap();
        assert_eq!(runs.len(), 1);
        assert_eq!(
            runs[0].status, "failed",
            "outcome must survive the rollback"
        );
        assert!(
            runs[0]
                .error_message
                .as_deref()
                .unwrap_or_default()
                .contains("induced scan failure"),
            "error lost: {:?}",
            runs[0].error_message
        );
        assert_eq!(
            runs[0].metrics.files_seen, 1,
            "partial measurement must survive the failure"
        );
    }

    #[test]
    fn a_metrics_write_failure_never_fails_the_scan() {
        let (_t, cat, root) = fixture_with_files(&[("a.txt", 10)]);
        // Drop the table out from under the run: recording must degrade, not propagate.
        cat.conn.execute_batch("DROP TABLE scan_runs").unwrap();
        let out = run_scan(
            &cat,
            &root,
            false,
            crate::volume::ReadonlyMode::Fingerprint,
            100,
            None,
            &crate::scan_control::StopFlag::new(),
            &test_limits(),
        );
        assert!(
            out.is_ok(),
            "a bookkeeping failure must not fail a scan: {out:?}"
        );
        assert_eq!(
            out.unwrap().unwrap().1.hashed,
            1,
            "the scan still did its work"
        );
    }

    #[test]
    fn count_tree_totals_files_and_bytes_and_skips_the_marker() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().join("drive");
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::write(root.join("a.bin"), vec![b'x'; 100]).unwrap();
        std::fs::write(root.join("sub/b.bin"), vec![b'y'; 250]).unwrap();
        // The identity marker is skipped by the scan, so it must not be counted either.
        std::fs::write(root.join(crate::volume::MARKER), b"vol-1").unwrap();

        let totals = count_tree(&root, &crate::scan_control::StopFlag::new());
        assert_eq!(totals.files, 2);
        assert_eq!(totals.bytes, 350);
    }

    #[test]
    fn count_tree_returns_promptly_when_stopped() {
        let t = tempfile::tempdir().unwrap();
        let root = t.path().join("drive");
        std::fs::create_dir_all(&root).unwrap();
        for i in 0..200 {
            std::fs::write(root.join(format!("f{i}.bin")), b"x").unwrap();
        }
        let stop = crate::scan_control::StopFlag::new();
        stop.request(); // already requested before we start
        let totals = count_tree(&root, &stop);
        assert!(
            totals.files < 200,
            "counting should stop early, got {}",
            totals.files
        );
    }

    #[test]
    fn a_stopped_scan_sweeps_nothing_and_reports_stopped() {
        // THE rule: a scan that did not finish must never mark files missing. Without the guard, every
        // file the walk had not reached yet would be flagged as gone.
        let (tmp, cat) = setup();
        let root = tmp.path().join("drive");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("a.txt"), b"one").unwrap();
        std::fs::write(root.join("b.txt"), b"two").unwrap();

        // First pass catalogues both files.
        let m = crate::scan_metrics::ScanMetrics::new();
        let stop = crate::scan_control::StopFlag::new();
        scan_volume_with_progress(
            &cat,
            &root,
            &ident(),
            false,
            100,
            None,
            &m,
            &stop,
            &test_limits(),
        )
        .unwrap();
        assert_eq!(cat.search("", None, None, Some("active")).unwrap().len(), 2);

        // Second pass is stopped before it starts: nothing is re-seen, so an unguarded sweep would
        // mark BOTH files missing.
        let stop2 = crate::scan_control::StopFlag::new();
        stop2.request();
        let m2 = crate::scan_metrics::ScanMetrics::new();
        let s = scan_volume_with_progress(
            &cat,
            &root,
            &ident(),
            false,
            300,
            None,
            &m2,
            &stop2,
            &test_limits(),
        )
        .unwrap();

        assert!(s.stopped, "the summary must report that it was stopped");
        assert_eq!(s.marked_missing, 0, "a stopped scan must not sweep");
        assert_eq!(
            cat.search("", None, None, Some("active")).unwrap().len(),
            2,
            "both files are still on disk and must stay active"
        );
    }

    #[test]
    fn an_unstopped_scan_still_sweeps() {
        // The guard must not disable the feature: a genuinely deleted file still becomes missing.
        let (tmp, cat) = setup();
        let root = tmp.path().join("drive");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("gone.txt"), b"bye").unwrap();

        let m = crate::scan_metrics::ScanMetrics::new();
        let stop = crate::scan_control::StopFlag::new();
        scan_volume_with_progress(
            &cat,
            &root,
            &ident(),
            false,
            100,
            None,
            &m,
            &stop,
            &test_limits(),
        )
        .unwrap();

        std::fs::remove_file(root.join("gone.txt")).unwrap();
        let m2 = crate::scan_metrics::ScanMetrics::new();
        let s = scan_volume_with_progress(
            &cat,
            &root,
            &ident(),
            false,
            300,
            None,
            &m2,
            &stop,
            &test_limits(),
        )
        .unwrap();
        assert!(!s.stopped);
        assert_eq!(s.marked_missing, 1);
    }

    #[test]
    fn a_resolved_file_error_clears_but_an_unreached_one_survives() {
        // THE rule, restated for errors: a stopped scan must clear only what it re-reached.
        // Without the last_seen_at predicate, stopping would wipe findings for the part of the
        // tree the run never visited -- silently reporting a catalogue as complete when it is not.
        let (tmp, cat) = setup();
        let root = tmp.path().join("drive");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("a.txt"), b"one").unwrap();

        // Two errors on record: one for a file that will be scanned, one for a path that will not.
        cat.log_scan_error(
            Some("vol-1"),
            "a.txt",
            "read: was locked",
            "read",
            "locked",
            50,
        )
        .unwrap();
        cat.log_scan_error(
            Some("vol-1"),
            "never/reached.bin",
            "read: i/o",
            "read",
            "io",
            50,
        )
        .unwrap();

        let m = crate::scan_metrics::ScanMetrics::new();
        let stop = crate::scan_control::StopFlag::new();
        scan_volume_with_progress(
            &cat,
            &root,
            &ident(),
            false,
            100,
            None,
            &m,
            &stop,
            &test_limits(),
        )
        .unwrap();

        let remaining: Vec<String> = cat
            .conn
            .prepare("SELECT path FROM scan_errors ORDER BY path")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(
            remaining,
            vec!["never/reached.bin".to_string()],
            "a.txt was re-catalogued so its error clears; the unreached path keeps its error"
        );
    }

    #[test]
    fn a_stopped_scan_does_not_clear_walk_errors() {
        // Only a completed scan proves a directory is readable again. A stopped scan may simply
        // never have got there.
        let (tmp, cat) = setup();
        let root = tmp.path().join("drive");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("a.txt"), b"one").unwrap();
        cat.log_scan_error(
            Some("vol-1"),
            "locked/dir",
            "walk: denied",
            "walk",
            "permission",
            50,
        )
        .unwrap();

        let m = crate::scan_metrics::ScanMetrics::new();
        let stop = crate::scan_control::StopFlag::new();
        stop.request(); // stopped before it starts
        let s = scan_volume_with_progress(
            &cat,
            &root,
            &ident(),
            false,
            100,
            None,
            &m,
            &stop,
            &test_limits(),
        )
        .unwrap();
        assert!(s.stopped);

        let n: i64 = cat
            .conn
            .query_row(
                "SELECT count(*) FROM scan_errors WHERE path='locked/dir'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 1, "a stopped scan never clears a walk error");
    }

    #[test]
    fn a_legacy_phase_null_error_clears_when_its_path_is_recatalogued() {
        // Rows written before `phase` existed have phase IS NULL. The spec promises they "clear
        // themselves on the next scan" -- same as any other file-path error, once re-seen.
        let (tmp, cat) = setup();
        let root = tmp.path().join("drive");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("a.txt"), b"one").unwrap();

        // Raw insert, no phase/kind -- mirrors a row from before this feature existed.
        cat.conn
            .execute(
                "INSERT INTO scan_errors(volume_id, path, reason, occurred_at)
                 VALUES ('vol-1', 'a.txt', 'read: was locked', 50)",
                [],
            )
            .unwrap();

        let m = crate::scan_metrics::ScanMetrics::new();
        let stop = crate::scan_control::StopFlag::new();
        scan_volume_with_progress(
            &cat,
            &root,
            &ident(),
            false,
            100,
            None,
            &m,
            &stop,
            &test_limits(),
        )
        .unwrap();

        let n: i64 = cat
            .conn
            .query_row(
                "SELECT count(*) FROM scan_errors WHERE path='a.txt'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 0, "a legacy error clears once its path is re-catalogued");
    }

    #[test]
    fn a_legacy_phase_null_error_survives_an_unreached_stopped_scan() {
        // The relaxation that lets legacy rows clear must not reopen the false-complete hazard:
        // a legacy row for a path a STOPPED scan never reached must still survive.
        let (tmp, cat) = setup();
        let root = tmp.path().join("drive");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("a.txt"), b"one").unwrap();

        cat.conn
            .execute(
                "INSERT INTO scan_errors(volume_id, path, reason, occurred_at)
                 VALUES ('vol-1', 'never/reached.bin', 'read: i/o', 50)",
                [],
            )
            .unwrap();

        let m = crate::scan_metrics::ScanMetrics::new();
        let stop = crate::scan_control::StopFlag::new();
        stop.request(); // stopped before it starts
        let s = scan_volume_with_progress(
            &cat,
            &root,
            &ident(),
            false,
            100,
            None,
            &m,
            &stop,
            &test_limits(),
        )
        .unwrap();
        assert!(s.stopped);

        let n: i64 = cat
            .conn
            .query_row(
                "SELECT count(*) FROM scan_errors WHERE path='never/reached.bin'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            n, 1,
            "a stopped scan must not clear a legacy error for an unreached path"
        );
    }

    #[test]
    fn a_renamed_zip_keeps_its_entries_active_across_an_unchanged_rescan() {
        // THE regression for this task. Once archives are detected by content, a renamed zip has
        // entries. If the skip path does not recognise it, those entries keep an old last_seen_at
        // and the sweep marks present files missing -- silent data loss in the catalogue.
        let (tmp, cat) = setup();
        let root = tmp.path().join("drive");
        std::fs::create_dir_all(&root).unwrap();

        // A real zip, deliberately NOT named .zip.
        let inner = {
            let mut buf = std::io::Cursor::new(Vec::new());
            {
                let mut zw = zip::ZipWriter::new(&mut buf);
                let opts: zip::write::FileOptions<()> = zip::write::FileOptions::default()
                    .compression_method(zip::CompressionMethod::Stored);
                zw.start_file("inside.txt", opts).unwrap();
                std::io::Write::write_all(&mut zw, b"payload").unwrap();
                zw.finish().unwrap();
            }
            buf.into_inner()
        };
        std::fs::write(root.join("archive.bak"), &inner).unwrap();

        // This regression is about the SKIP-PATH/revival mechanics for an already-descended
        // archive's entries, not about the descent decision itself -- so `bak` is explicitly
        // allow-listed here (unlike `test_limits()`) to keep that decision out of the way.
        let allow_bak = crate::archive::ArchiveLimits {
            allow_extensions: vec!["bak".to_string()],
            ..test_limits()
        };

        let m = crate::scan_metrics::ScanMetrics::new();
        let stop = crate::scan_control::StopFlag::new();
        scan_volume_with_progress(
            &cat,
            &root,
            &ident(),
            false,
            100,
            None,
            &m,
            &stop,
            &allow_bak,
        )
        .unwrap();

        let entries_active = || -> i64 {
            cat.conn
                .query_row(
                    "SELECT count(*) FROM files WHERE container_chain IS NOT NULL AND status='active'",
                    [],
                    |r| r.get(0),
                )
                .unwrap_or(-1)
        };
        assert_eq!(entries_active(), 1, "the renamed zip's entry is catalogued");

        // Second scan, nothing changed on disk: the skip path runs, then the sweep.
        let m2 = crate::scan_metrics::ScanMetrics::new();
        let stop2 = crate::scan_control::StopFlag::new();
        let s = scan_volume_with_progress(
            &cat,
            &root,
            &ident(),
            false,
            300,
            None,
            &m2,
            &stop2,
            &allow_bak,
        )
        .unwrap();

        assert_eq!(
            s.marked_missing, 0,
            "nothing on disk changed, so nothing may be marked missing"
        );
        assert_eq!(
            entries_active(),
            1,
            "the archive entry must still be active after an unchanged rescan"
        );
    }

    #[test]
    fn a_prefixed_zip_is_still_detected_and_descended_into() {
        // A self-extracting stub (or anything else that prepends bytes) leaves a file that does not
        // start with PK, yet is a perfectly valid zip: real readers locate the central directory
        // from the END of the file. The head-only check alone would regress this to undetected.
        let (tmp, cat) = setup();
        let root = tmp.path().join("drive");
        std::fs::create_dir_all(&root).unwrap();

        let zip_bytes = {
            let mut buf = std::io::Cursor::new(Vec::new());
            {
                let mut zw = zip::ZipWriter::new(&mut buf);
                let opts: zip::write::FileOptions<()> = zip::write::FileOptions::default()
                    .compression_method(zip::CompressionMethod::Stored);
                zw.start_file("inside.txt", opts).unwrap();
                std::io::Write::write_all(&mut zw, b"payload").unwrap();
                zw.finish().unwrap();
            }
            buf.into_inner()
        };
        // Prepend a fake self-extracting stub. The file is named .zip, but does not start with PK.
        let mut prefixed = b"MZ this is a fake SFX stub, not a zip signature".to_vec();
        prefixed.extend_from_slice(&zip_bytes);
        std::fs::write(root.join("installer.zip"), &prefixed).unwrap();

        let m = crate::scan_metrics::ScanMetrics::new();
        let stop = crate::scan_control::StopFlag::new();
        scan_volume_with_progress(
            &cat,
            &root,
            &ident(),
            false,
            100,
            None,
            &m,
            &stop,
            &test_limits(),
        )
        .unwrap();

        let inner_catalogued: i64 = cat
            .conn
            .query_row(
                "SELECT count(*) FROM files WHERE filename='inside.txt' AND container_chain IS NOT NULL",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            inner_catalogued, 1,
            "a prefixed zip's entries must be catalogued, not skipped as a leaf"
        );
    }

    /// F1: a failed open at the top-level archive-detection step must never be silently
    /// indistinguishable from "not an archive". Forces the detection `File::open` to fail (a real
    /// external lock would also fail the hash step that runs just before it, so a thread-local
    /// injection point is the only reproducible way to isolate this exact call).
    ///
    /// This test is DESIGNED to fail if the fix regresses to `.unwrap_or(false)`: with that old
    /// behaviour, `is_archive` silently becomes `false`, `descend_archive` never runs, no scan error
    /// is logged, and the archive's entry is swept to `missing` with no way to self-heal (the
    /// archive's own row stays `active`, so the revive floor is `None`).
    #[test]
    fn a_failed_detection_open_is_logged_and_does_not_lose_the_archives_entries() {
        let (tmp, cat) = setup();
        let root = tmp.path().join("drive");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("a.zip"), make_zip(&[("a.txt", b"payload")])).unwrap();

        FORCE_DETECTION_OPEN_ERROR.with(|f| f.set(true));
        let m = crate::scan_metrics::ScanMetrics::new();
        let stop = crate::scan_control::StopFlag::new();
        let scan_result = scan_volume_with_progress(
            &cat,
            &root,
            &ident(),
            false,
            100,
            None,
            &m,
            &stop,
            &test_limits(),
        );
        FORCE_DETECTION_OPEN_ERROR.with(|f| f.set(false));
        let summary = scan_result.unwrap();

        assert_eq!(
            summary.errors, 1,
            "a detection failure is a real scan error and must be counted"
        );
        let logged: i64 = cat
            .conn
            .query_row(
                "SELECT count(*) FROM scan_errors WHERE path='a.zip'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            logged, 1,
            "a detection failure must be recorded, not silent (this is the F1 defect)"
        );
        assert_eq!(
            status_of(&cat, "a.txt"),
            "active",
            "the archive's entry must not be swept to missing just because detection had to \
             fall back on the extension test"
        );
    }

    #[test]
    fn a_detection_failure_on_a_catalogued_archive_keeps_its_entries() {
        // The archive is named .bak, so the old extension-based fallback said "not an archive",
        // descend never ran, and the sweep took its entries -- with no way to self-heal.
        let (tmp, cat) = setup();
        let root = tmp.path().join("drive");
        std::fs::create_dir_all(&root).unwrap();
        let zip_bytes = {
            let mut buf = std::io::Cursor::new(Vec::new());
            {
                let mut zw = zip::ZipWriter::new(&mut buf);
                let opts: zip::write::FileOptions<()> = zip::write::FileOptions::default()
                    .compression_method(zip::CompressionMethod::Stored);
                zw.start_file("inside.txt", opts).unwrap();
                std::io::Write::write_all(&mut zw, b"payload").unwrap();
                zw.finish().unwrap();
            }
            buf.into_inner()
        };
        std::fs::write(root.join("backup.bak"), &zip_bytes).unwrap();

        // Approve .bak so the first scan descends and the entry exists. Task 3 made the limits an
        // argument, so this needs no environment or settings file.
        let limits = crate::archive::ArchiveLimits {
            allow_extensions: vec!["bak".to_string()],
            ..test_limits()
        };
        let stop = crate::scan_control::StopFlag::new();
        let m = crate::scan_metrics::ScanMetrics::new();
        let ident = ident();
        scan_volume_with_progress(&cat, &root, &ident, false, 100, None, &m, &stop, &limits)
            .unwrap();
        let active = || -> i64 {
            cat.conn
                .query_row(
                    "SELECT count(*) FROM files WHERE container_chain IS NOT NULL AND status='active'",
                    [],
                    |r| r.get(0),
                )
                .unwrap()
        };
        assert_eq!(active(), 1, "the entry is catalogued");

        // Now force the detection open to fail, with the file's content changed so the skip path
        // does not short-circuit.
        std::fs::write(root.join("backup.bak"), [&zip_bytes[..], b"x"].concat()).unwrap();
        FORCE_DETECTION_OPEN_ERROR.with(|f| f.set(true));
        let m2 = crate::scan_metrics::ScanMetrics::new();
        let r =
            scan_volume_with_progress(&cat, &root, &ident, false, 300, None, &m2, &stop, &limits);
        // Reset before unwrapping, so a failure cannot leave the hook set for other tests on this
        // thread.
        FORCE_DETECTION_OPEN_ERROR.with(|f| f.set(false));
        r.unwrap();

        assert_eq!(
            active(),
            1,
            "a detection failure must not cost the archive its entries"
        );
    }

    /// The `--force` variant of the above. `--force` is the documented recovery path for a stale
    /// catalogue, so a detection failure under `--force` must ALSO consult the catalogue rather
    /// than the filename -- otherwise the one recovery mechanism this defect tells users to reach
    /// for reopens the exact same loss it is supposed to fix.
    #[test]
    fn a_detection_failure_under_force_also_keeps_a_catalogued_archives_entries() {
        let (tmp, cat) = setup();
        let root = tmp.path().join("drive");
        std::fs::create_dir_all(&root).unwrap();
        let zip_bytes = {
            let mut buf = std::io::Cursor::new(Vec::new());
            {
                let mut zw = zip::ZipWriter::new(&mut buf);
                let opts: zip::write::FileOptions<()> = zip::write::FileOptions::default()
                    .compression_method(zip::CompressionMethod::Stored);
                zw.start_file("inside.txt", opts).unwrap();
                std::io::Write::write_all(&mut zw, b"payload").unwrap();
                zw.finish().unwrap();
            }
            buf.into_inner()
        };
        std::fs::write(root.join("backup.bak"), &zip_bytes).unwrap();

        let limits = crate::archive::ArchiveLimits {
            allow_extensions: vec!["bak".to_string()],
            ..test_limits()
        };
        let stop = crate::scan_control::StopFlag::new();
        let m = crate::scan_metrics::ScanMetrics::new();
        let ident = ident();
        scan_volume_with_progress(&cat, &root, &ident, false, 100, None, &m, &stop, &limits)
            .unwrap();
        let active = || -> i64 {
            cat.conn
                .query_row(
                    "SELECT count(*) FROM files WHERE container_chain IS NOT NULL AND status='active'",
                    [],
                    |r| r.get(0),
                )
                .unwrap()
        };
        assert_eq!(active(), 1, "the entry is catalogued");

        // Content need not even change here: `force=true` re-hashes and re-runs detection
        // regardless of size/mtime, which is the whole point of `--force`.
        FORCE_DETECTION_OPEN_ERROR.with(|f| f.set(true));
        let m2 = crate::scan_metrics::ScanMetrics::new();
        let r =
            scan_volume_with_progress(&cat, &root, &ident, true, 300, None, &m2, &stop, &limits);
        // Reset before unwrapping, so a failure cannot leave the hook set for other tests on this
        // thread.
        FORCE_DETECTION_OPEN_ERROR.with(|f| f.set(false));
        r.unwrap();

        assert_eq!(
            active(),
            1,
            "a detection failure under --force must not cost the archive its entries either -- \
             --force is the documented recovery path for this defect"
        );
    }

    #[test]
    fn a_revive_floor_does_not_resurrect_an_entry_removed_before_the_archive_went_missing() {
        // THE Finding-2 regression, reproduced end to end through the real scanner rather than the
        // store API directly:
        //
        //   t=100  bundle.zip = a.txt + gone.txt      -> both active
        //   t=200  rewritten without gone.txt         -> gone.txt correctly swept 'missing'
        //   t=300  archive moved out of the tree      -> archive row + a.txt swept 'missing'
        //   t=400  moved back, identical, same mtime  -> skip path, revive_floor = Some(...)
        //
        // A plain "was the archive missing" boolean cannot tell gone.txt (removed BEFORE the
        // archive went missing) apart from a.txt (missing only because it went missing WITH the
        // archive) -- both revive. The floor must let only a.txt come back.
        let (tmp, cat) = setup();
        let root = tmp.path().join("drive");
        std::fs::create_dir_all(&root).unwrap();
        let bundle_path = root.join("bundle.zip");
        let parked_path = tmp.path().join("bundle.zip.parked");

        // t=100: both entries present.
        std::fs::write(
            &bundle_path,
            make_zip(&[("a.txt", b"alpha"), ("gone.txt", b"beta")]),
        )
        .unwrap();
        let m1 = crate::scan_metrics::ScanMetrics::new();
        let stop1 = crate::scan_control::StopFlag::new();
        scan_volume_with_progress(
            &cat,
            &root,
            &ident(),
            false,
            100,
            None,
            &m1,
            &stop1,
            &test_limits(),
        )
        .unwrap();
        assert_eq!(status_of(&cat, "a.txt"), "active");
        assert_eq!(status_of(&cat, "gone.txt"), "active");

        // t=200: rewritten without gone.txt -- a real content change, so it redescends (the size
        // differs, which alone fails the skip check regardless of mtime resolution).
        std::fs::write(&bundle_path, make_zip(&[("a.txt", b"alpha")])).unwrap();
        let m2 = crate::scan_metrics::ScanMetrics::new();
        let stop2 = crate::scan_control::StopFlag::new();
        scan_volume_with_progress(
            &cat,
            &root,
            &ident(),
            false,
            200,
            None,
            &m2,
            &stop2,
            &test_limits(),
        )
        .unwrap();
        assert_eq!(status_of(&cat, "a.txt"), "active");
        assert_eq!(
            status_of(&cat, "gone.txt"),
            "missing",
            "gone.txt was genuinely removed from the archive's content"
        );

        // t=300: the archive itself leaves the tree -- its own row and a.txt's entry are swept.
        std::fs::rename(&bundle_path, &parked_path).unwrap();
        let m3 = crate::scan_metrics::ScanMetrics::new();
        let stop3 = crate::scan_control::StopFlag::new();
        scan_volume_with_progress(
            &cat,
            &root,
            &ident(),
            false,
            300,
            None,
            &m3,
            &stop3,
            &test_limits(),
        )
        .unwrap();
        assert_eq!(status_of(&cat, "a.txt"), "missing");
        assert_eq!(status_of(&cat, "gone.txt"), "missing");

        // t=400: moved back byte-identical (same size, same mtime since it's the same underlying
        // file, never rewritten) -- the skip path fires and touches the archive's entries.
        std::fs::rename(&parked_path, &bundle_path).unwrap();
        let m4 = crate::scan_metrics::ScanMetrics::new();
        let stop4 = crate::scan_control::StopFlag::new();
        scan_volume_with_progress(
            &cat,
            &root,
            &ident(),
            false,
            400,
            None,
            &m4,
            &stop4,
            &test_limits(),
        )
        .unwrap();

        assert_eq!(
            status_of(&cat, "a.txt"),
            "active",
            "a.txt went missing together with the archive, so it must revive with it"
        );
        assert_eq!(
            status_of(&cat, "gone.txt"),
            "missing",
            "gone.txt was removed from the archive BEFORE the archive went missing -- reviving it \
             would assert the archive contains a file it demonstrably does not"
        );
    }

    #[test]
    fn the_revive_floor_still_separates_after_a_second_round_trip() {
        // Adapted from a salvaged review attack: the archive goes missing and returns TWICE, with a
        // legitimate removal happening between the two round-trips. Does the floor correctly
        // separate "removed before the SECOND disappearance" from "present at the second
        // disappearance", given that the same entry (gone.txt) already lived through one revival
        // earlier in its history?
        let (tmp, cat) = setup();
        let root = tmp.path().join("drive");
        std::fs::create_dir_all(&root).unwrap();
        let bundle_path = root.join("bundle.zip");
        let parked_path = tmp.path().join("bundle.zip.parked");

        // t=100: a.txt + gone.txt present.
        std::fs::write(
            &bundle_path,
            make_zip(&[("a.txt", b"alpha"), ("gone.txt", b"beta")]),
        )
        .unwrap();
        let m = crate::scan_metrics::ScanMetrics::new();
        let s = crate::scan_control::StopFlag::new();
        scan_volume_with_progress(
            &cat,
            &root,
            &ident(),
            false,
            100,
            None,
            &m,
            &s,
            &test_limits(),
        )
        .unwrap();

        // t=200/300: archive vanishes (round-trip #1) and returns unchanged -- both entries revive
        // together (this is the ordinary, already-tested case).
        std::fs::rename(&bundle_path, &parked_path).unwrap();
        let m = crate::scan_metrics::ScanMetrics::new();
        let s = crate::scan_control::StopFlag::new();
        scan_volume_with_progress(
            &cat,
            &root,
            &ident(),
            false,
            200,
            None,
            &m,
            &s,
            &test_limits(),
        )
        .unwrap();
        std::fs::rename(&parked_path, &bundle_path).unwrap();
        let m = crate::scan_metrics::ScanMetrics::new();
        let s = crate::scan_control::StopFlag::new();
        scan_volume_with_progress(
            &cat,
            &root,
            &ident(),
            false,
            300,
            None,
            &m,
            &s,
            &test_limits(),
        )
        .unwrap();
        assert_eq!(status_of(&cat, "a.txt"), "active");
        assert_eq!(
            status_of(&cat, "gone.txt"),
            "active",
            "revived together at round-trip #1"
        );

        // t=400: gone.txt is now genuinely removed via a real descend (size changes).
        std::fs::write(&bundle_path, make_zip(&[("a.txt", b"alpha")])).unwrap();
        let m = crate::scan_metrics::ScanMetrics::new();
        let s = crate::scan_control::StopFlag::new();
        scan_volume_with_progress(
            &cat,
            &root,
            &ident(),
            false,
            400,
            None,
            &m,
            &s,
            &test_limits(),
        )
        .unwrap();
        assert_eq!(
            status_of(&cat, "gone.txt"),
            "missing",
            "genuinely removed this time"
        );

        // t=500/600: archive vanishes AGAIN (round-trip #2) and returns unchanged.
        std::fs::rename(&bundle_path, &parked_path).unwrap();
        let m = crate::scan_metrics::ScanMetrics::new();
        let s = crate::scan_control::StopFlag::new();
        scan_volume_with_progress(
            &cat,
            &root,
            &ident(),
            false,
            500,
            None,
            &m,
            &s,
            &test_limits(),
        )
        .unwrap();
        std::fs::rename(&parked_path, &bundle_path).unwrap();
        let m = crate::scan_metrics::ScanMetrics::new();
        let s = crate::scan_control::StopFlag::new();
        scan_volume_with_progress(
            &cat,
            &root,
            &ident(),
            false,
            600,
            None,
            &m,
            &s,
            &test_limits(),
        )
        .unwrap();

        assert_eq!(
            status_of(&cat, "a.txt"),
            "active",
            "a.txt was present at round-trip #2's disappearance, must revive"
        );
        assert_eq!(
            status_of(&cat, "gone.txt"),
            "missing",
            "ATTACK: gone.txt was removed BEFORE round-trip #2's disappearance (at t=400), so the \
             SECOND round-trip must not revive it either, even though it WAS revived by the FIRST \
             round-trip"
        );
    }

    #[test]
    fn a_document_container_is_catalogued_whole_and_a_renamed_zip_is_reported() {
        // The whole point of this branch: a .docx must not explode into its parts, and a zip with
        // an unfamiliar extension must be reported rather than silently descended or ignored.
        let (tmp, cat) = setup();
        let root = tmp.path().join("drive");
        std::fs::create_dir_all(&root).unwrap();
        let zip_bytes = {
            let mut buf = std::io::Cursor::new(Vec::new());
            {
                let mut zw = zip::ZipWriter::new(&mut buf);
                let opts: zip::write::FileOptions<()> = zip::write::FileOptions::default()
                    .compression_method(zip::CompressionMethod::Stored);
                zw.start_file("inside.txt", opts).unwrap();
                std::io::Write::write_all(&mut zw, b"payload").unwrap();
                zw.finish().unwrap();
            }
            buf.into_inner()
        };
        std::fs::write(root.join("thesis.docx"), &zip_bytes).unwrap();
        std::fs::write(root.join("backup.bak"), &zip_bytes).unwrap();
        std::fs::write(root.join("real.zip"), &zip_bytes).unwrap();

        let m = crate::scan_metrics::ScanMetrics::new();
        let stop = crate::scan_control::StopFlag::new();
        scan_volume_with_progress(
            &cat,
            &root,
            &ident(),
            false,
            100,
            None,
            &m,
            &stop,
            &test_limits(),
        )
        .unwrap();

        let entries: Vec<String> = cat
            .conn
            .prepare("SELECT relative_path FROM files WHERE container_chain IS NOT NULL ORDER BY relative_path")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(
            entries,
            vec!["real.zip".to_string()],
            "only the .zip is descended into"
        );

        let pending = cat.pending_formats().unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].extension, "bak");
        assert_eq!(pending[0].count, 1);

        // All three are still catalogued as ordinary files -- nothing is skipped.
        let loose: i64 = cat
            .conn
            .query_row(
                "SELECT count(*) FROM files WHERE container_chain IS NULL AND status='active'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(loose, 3);
    }

    #[test]
    fn approving_a_reported_extension_descends_it_on_the_very_next_ordinary_rescan() {
        // SC3: "Approving a reported extension descends into those files on the next scan." This
        // reproduces F-1 from the archive-descent-policy final review: without invalidating the
        // cached fingerprint, an unchanged file's (size, mtime) still matches the catalogue, so the
        // skip path at the top of this loop never re-opens it and `descend_archive` never runs --
        // only `--force` (a second multi-day pass over the whole drive) would descend it.
        let (tmp, cat) = setup();
        let root = tmp.path().join("drive");
        std::fs::create_dir_all(&root).unwrap();
        let zip_bytes = make_zip(&[("inside.txt", b"payload")]);
        std::fs::write(root.join("backup.bak"), &zip_bytes).unwrap();

        let m = crate::scan_metrics::ScanMetrics::new();
        let stop = crate::scan_control::StopFlag::new();
        scan_volume_with_progress(
            &cat,
            &root,
            &ident(),
            false,
            100,
            None,
            &m,
            &stop,
            &test_limits(),
        )
        .unwrap();

        let pending = cat.pending_formats().unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].extension, "bak");

        // What `api_resolve_format` (action "descend") does: invalidate the cached fingerprint for
        // every file pending under this extension, then clear the pending rows. The extension
        // joining the allow-list (the settings.json write in the real handler) is modeled here by
        // handing the second scan a `limits` value with "bak" already in `allow_extensions`.
        for (volume_id, relative_path) in cat.pending_format_paths("bak").unwrap() {
            cat.invalidate_scan_fingerprint(&volume_id, &relative_path)
                .unwrap();
        }
        cat.clear_pending_format("bak").unwrap();

        let mut limits2 = test_limits();
        limits2.allow_extensions = vec!["bak".to_string()];

        let m = crate::scan_metrics::ScanMetrics::new();
        let stop = crate::scan_control::StopFlag::new();
        let summary = scan_volume_with_progress(
            &cat,
            &root,
            &ident(),
            false, // ordinary rescan -- NOT --force. This is the criterion.
            200,
            None,
            &m,
            &stop,
            &limits2,
        )
        .unwrap();

        assert_eq!(
            summary.skipped, 0,
            "the cached fingerprint must be invalidated, or backup.bak takes the skip path and is \
             never re-opened"
        );
        assert_eq!(summary.hashed, 1);

        let entries: Vec<String> = cat
            .conn
            .prepare(
                "SELECT relative_path FROM files WHERE container_chain IS NOT NULL \
                 ORDER BY relative_path",
            )
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(
            entries,
            vec!["backup.bak".to_string()],
            "approving the extension must descend it on an ORDINARY (force=false) rescan"
        );

        let pending_after = cat.pending_formats().unwrap();
        assert!(
            pending_after.is_empty(),
            "the approved extension must not still be reported as pending"
        );
    }

    #[test]
    fn a_forced_rescan_does_not_double_count_pending_formats() {
        // force=true: the file is re-hashed every pass, so record_pending_format is called again --
        // the per-file upsert must replace the row, not add a second one.
        let (tmp, cat) = setup();
        let root = tmp.path().join("drive");
        std::fs::create_dir_all(&root).unwrap();
        let zip_bytes = {
            let mut buf = std::io::Cursor::new(Vec::new());
            {
                let mut zw = zip::ZipWriter::new(&mut buf);
                let opts: zip::write::FileOptions<()> = zip::write::FileOptions::default()
                    .compression_method(zip::CompressionMethod::Stored);
                zw.start_file("inside.txt", opts).unwrap();
                std::io::Write::write_all(&mut zw, b"payload").unwrap();
                zw.finish().unwrap();
            }
            buf.into_inner()
        };
        std::fs::write(root.join("backup.bak"), &zip_bytes).unwrap();
        let stop = crate::scan_control::StopFlag::new();
        for now in [100, 200] {
            let m = crate::scan_metrics::ScanMetrics::new();
            scan_volume_with_progress(
                &cat,
                &root,
                &ident(),
                true,
                now,
                None,
                &m,
                &stop,
                &test_limits(),
            )
            .unwrap();
        }
        let pending = cat.pending_formats().unwrap();
        assert_eq!(
            pending[0].count, 1,
            "a forced rescan re-hashes the file but must not accumulate a second row"
        );
    }

    #[test]
    fn an_incremental_rescan_still_reports_the_pending_format() {
        // THE bug this round fixes: an ordinary (force=false) second scan finds the file unchanged
        // and takes the skip path, which never opens the file and therefore never calls
        // record_pending_format again. With no clear-at-scan-start, the row from the first scan must
        // simply persist -- the report must not go blank just because nothing changed on disk.
        let (tmp, cat) = setup();
        let root = tmp.path().join("drive");
        std::fs::create_dir_all(&root).unwrap();
        let zip_bytes = {
            let mut buf = std::io::Cursor::new(Vec::new());
            {
                let mut zw = zip::ZipWriter::new(&mut buf);
                let opts: zip::write::FileOptions<()> = zip::write::FileOptions::default()
                    .compression_method(zip::CompressionMethod::Stored);
                zw.start_file("inside.txt", opts).unwrap();
                std::io::Write::write_all(&mut zw, b"payload").unwrap();
                zw.finish().unwrap();
            }
            buf.into_inner()
        };
        std::fs::write(root.join("backup.bak"), &zip_bytes).unwrap();
        let stop = crate::scan_control::StopFlag::new();

        let m1 = crate::scan_metrics::ScanMetrics::new();
        scan_volume_with_progress(
            &cat,
            &root,
            &ident(),
            false,
            100,
            None,
            &m1,
            &stop,
            &test_limits(),
        )
        .unwrap();
        let pending = cat.pending_formats().unwrap();
        assert_eq!(pending.len(), 1, "the first scan reports it");
        assert_eq!(pending[0].count, 1);

        // Second scan: nothing on disk changed, so this is the skip path.
        let m2 = crate::scan_metrics::ScanMetrics::new();
        scan_volume_with_progress(
            &cat,
            &root,
            &ident(),
            false,
            200,
            None,
            &m2,
            &stop,
            &test_limits(),
        )
        .unwrap();
        let pending = cat.pending_formats().unwrap();
        assert_eq!(
            pending.len(),
            1,
            "an incremental rescan must not empty the report"
        );
        assert_eq!(pending[0].extension, "bak");
        assert_eq!(pending[0].count, 1, "still exactly one file");
    }

    #[test]
    fn scanning_one_volume_does_not_touch_another_volumes_pending_formats() {
        let (tmp, cat) = setup();
        let root_a = tmp.path().join("drive-a");
        let root_b = tmp.path().join("drive-b");
        std::fs::create_dir_all(&root_a).unwrap();
        std::fs::create_dir_all(&root_b).unwrap();
        let zip_bytes = {
            let mut buf = std::io::Cursor::new(Vec::new());
            {
                let mut zw = zip::ZipWriter::new(&mut buf);
                let opts: zip::write::FileOptions<()> = zip::write::FileOptions::default()
                    .compression_method(zip::CompressionMethod::Stored);
                zw.start_file("inside.txt", opts).unwrap();
                std::io::Write::write_all(&mut zw, b"payload").unwrap();
                zw.finish().unwrap();
            }
            buf.into_inner()
        };
        std::fs::write(root_a.join("a.bak"), &zip_bytes).unwrap();
        std::fs::write(root_b.join("b.kra"), &zip_bytes).unwrap();

        let ident_a = VolumeIdentity {
            volume_id: "vol-a".into(),
            label: "A".into(),
            identified_by: "marker".into(),
        };
        let ident_b = VolumeIdentity {
            volume_id: "vol-b".into(),
            label: "B".into(),
            identified_by: "marker".into(),
        };
        cat.upsert_volume(&Volume {
            volume_id: "vol-a".into(),
            label: "A".into(),
            identified_by: "marker".into(),
            first_seen_at: 1,
            last_seen_at: 1,
        })
        .unwrap();
        cat.upsert_volume(&Volume {
            volume_id: "vol-b".into(),
            label: "B".into(),
            identified_by: "marker".into(),
            first_seen_at: 1,
            last_seen_at: 1,
        })
        .unwrap();
        let stop = crate::scan_control::StopFlag::new();
        let m = crate::scan_metrics::ScanMetrics::new();
        scan_volume_with_progress(
            &cat,
            &root_a,
            &ident_a,
            false,
            100,
            None,
            &m,
            &stop,
            &test_limits(),
        )
        .unwrap();
        let m2 = crate::scan_metrics::ScanMetrics::new();
        scan_volume_with_progress(
            &cat,
            &root_b,
            &ident_b,
            false,
            200,
            None,
            &m2,
            &stop,
            &test_limits(),
        )
        .unwrap();

        let pending = cat.pending_formats().unwrap();
        let exts: std::collections::BTreeSet<String> =
            pending.iter().map(|p| p.extension.clone()).collect();
        assert_eq!(
            exts,
            ["bak".to_string(), "kra".to_string()].into_iter().collect(),
            "scanning volume B must not clear or otherwise disturb volume A's report"
        );
    }

    #[test]
    fn the_batch_commits_on_bytes_even_when_the_file_count_is_low() {
        // The point of the byte bound: a handful of large files must still commit, or a stopped
        // scan would have to re-hash all of them. A count-only trigger cannot express this.
        let (tmp, cat) = setup();
        cat.conn.execute_batch("BEGIN").unwrap();
        // Written inside the open transaction, so it is invisible to any other connection until a
        // COMMIT actually happens. Reading it back from a SECOND connection is what makes this test
        // prove a commit rather than merely a counter reset -- an implementation that zeroed the
        // accumulators without committing would pass the assertions below on their own.
        cat.upsert_file(
            &NewFile {
                volume_id: "vol-1".into(),
                relative_path: "big.bin".into(),
                filename: "big.bin".into(),
                extension: "bin".into(),
                size_bytes: 1,
                content_hash: "h".into(),
                created_time: None,
                modified_time: None,
                accessed_time: None,
                category: Category::Other,
                container_chain: None,
            },
            1,
        )
        .unwrap();
        // Well below BATCH_MAX_FILES, well above BATCH_MAX_BYTES. Declared at their final values:
        // assigning then overwriting would trip `unused_assignments` under `-D warnings`.
        let mut in_batch = 3usize;
        let mut batch_bytes = BATCH_MAX_BYTES + 1;
        rotate_batch(&cat, &mut in_batch, &mut batch_bytes).unwrap();
        assert_eq!(in_batch, 0, "the byte bound must trigger a commit");
        assert_eq!(batch_bytes, 0, "and reset the byte accumulator");

        let other = Catalog::open_readonly(&tmp.path().join("c.db")).unwrap();
        let visible: i64 = other
            .conn
            .query_row(
                "SELECT COUNT(*) FROM files WHERE relative_path='big.bin'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            visible, 1,
            "the byte bound must actually COMMIT, not just reset the counters"
        );
        cat.conn.execute_batch("COMMIT").ok();
    }

    #[test]
    fn the_batch_still_commits_on_the_file_count() {
        let mut in_batch = BATCH_MAX_FILES;
        let mut batch_bytes = 0u64;
        let (_t, cat) = setup();
        cat.conn.execute_batch("BEGIN").unwrap();
        rotate_batch(&cat, &mut in_batch, &mut batch_bytes).unwrap();
        assert_eq!(in_batch, 0);
        cat.conn.execute_batch("COMMIT").ok();
    }
}
