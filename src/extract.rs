//! Extract one catalogued archive to a sibling folder, prove every entry against the catalogued
//! BLAKE3, then quarantine the original (#77).
//!
//! The unit of work is a whole archive: half-extracting one means the original still holds content
//! nothing else has, so it could never be quarantined. Every refusal below happens *before* a byte
//! is written, and every failure after that point deletes the destination and leaves the archive
//! exactly where it was.

use std::collections::HashMap;
use std::io::Read;
use std::path::Path;

use anyhow::Context;

use crate::catalog::Catalog;

/// Windows' classic path limit. Rust can write past it through `\\?\`, but Explorer and most
/// applications then cannot open the file — useless for data whose whole purpose is being reachable.
pub const MAX_PATH_CHARS: usize = 260;

/// The separator `archive::join_chain` puts between container levels.
pub const CHAIN_SEP: &str = " › ";

/// Everything before the last dot of the last segment; the archive's own folder name.
fn stem(name: &str) -> &str {
    let leaf = name.rsplit('/').next().unwrap_or(name);
    match leaf.rsplit_once('.') {
        Some((s, _)) if !s.is_empty() => s,
        _ => leaf,
    }
}

/// The sibling folder an archive extracts into: same parent, named after the archive's stem.
pub fn destination_dir(archive_rel: &str) -> String {
    match archive_rel.rsplit_once('/') {
        Some((parent, leaf)) => format!("{parent}/{}", stem(leaf)),
        None => stem(archive_rel).to_string(),
    }
}

/// Where an entry ends up once the *entire* tree has been extracted: each intermediate archive
/// segment becomes a folder named after its stem. This is the path the budget is measured against,
/// because an archive that cannot be fully unpacked must never be started.
pub fn final_relative_path(archive_rel: &str, chain: &str) -> String {
    let mut out = destination_dir(archive_rel);
    let segments: Vec<&str> = chain.split(CHAIN_SEP).collect();
    for (i, seg) in segments.iter().enumerate() {
        if i + 1 == segments.len() {
            out.push('/');
            out.push_str(seg);
        } else {
            // An intermediate segment is an archive; its stem is the folder its contents land in.
            // The segment may itself carry a directory prefix from inside its parent.
            match seg.rsplit_once('/') {
                Some((dir, leaf)) => {
                    out.push('/');
                    out.push_str(dir);
                    out.push('/');
                    out.push_str(stem(leaf));
                }
                None => {
                    out.push('/');
                    out.push_str(stem(seg));
                }
            }
        }
    }
    out
}

/// Where an entry lands after extracting **this one level**, plus whatever chain is left. A nested
/// entry's first hop is the nested archive file itself, written verbatim into the destination.
pub fn first_hop(archive_rel: &str, chain: &str) -> (String, Option<String>) {
    let dest = destination_dir(archive_rel);
    match chain.split_once(CHAIN_SEP) {
        Some((head, rest)) => (format!("{dest}/{head}"), Some(rest.to_string())),
        None => (format!("{dest}/{chain}"), None),
    }
}

/// Does this entry's fully-extracted path fit inside `MAX_PATH_CHARS`, measured from the actual
/// mount root? An assumed `E:\` is a guess, and a drive mounted anywhere else silently invalidates
/// the whole safety check.
pub fn fits_budget(mount_root: &Path, archive_rel: &str, chain: &str) -> bool {
    full_length(mount_root, archive_rel, chain) <= MAX_PATH_CHARS
}

/// The character count `fits_budget` compares, exposed so refusals can report the real number.
///
/// Counted in UTF-16 code units, not `char`s, because that is what Windows' MAX_PATH actually
/// measures: an astral character (e.g. most emoji) is one `char` but two UTF-16 code units, and
/// counting chars would under-count exactly the paths this check exists to catch.
pub fn full_length(mount_root: &Path, archive_rel: &str, chain: &str) -> usize {
    let root = mount_root.to_string_lossy();
    let sep = if root.ends_with('\\') || root.ends_with('/') {
        0
    } else {
        1
    };
    root.encode_utf16().count()
        + sep
        + final_relative_path(archive_rel, chain)
            .encode_utf16()
            .count()
}

/// Whether an archive can be extracted, or the single reason it cannot.
pub enum Scope {
    InScope {
        entries: usize,
        uncompressed_bytes: i64,
    },
    Refused(String),
}

/// Bytes that must remain free after an extraction. Refusing an archive is recoverable; filling a
/// drive that holds irreplaceable data is not.
pub const SPACE_HEADROOM_BYTES: u64 = 5 * 1024 * 1024 * 1024;

/// Every reason an archive can be refused, checked before anything is written. Ordered cheapest
/// first, and deliberately shared by the Extract page and the worker so the page cannot promise
/// something the worker will refuse.
///
/// Thin wrapper over [`scope_check_with_space`] that measures free space itself. Callers checking
/// a single archive against a mount want this; callers checking many archives against the SAME
/// mount in one pass (e.g. the archives-list page) should call `scope_check_with_space` directly
/// with one shared measurement -- see its doc comment for why.
pub fn scope_check(
    cat: &Catalog,
    mount_root: &Path,
    volume_id: &str,
    archive_rel: &str,
) -> anyhow::Result<Scope> {
    let free = crate::repack::available_space(mount_root);
    scope_check_with_space(cat, mount_root, volume_id, archive_rel, free)
}

/// Same check as [`scope_check`], but takes the mount's free space as a parameter instead of
/// measuring it.
///
/// `free_bytes` is a parameter, not an internal call to `repack::available_space`, because that
/// function does a full OS-level disk-list refresh (`sysinfo::Disks::new_with_refreshed_list()`)
/// on every call -- not a cached lookup or a single free-space syscall. A caller checking many
/// archives against one volume (the archives-list page, at the user's real catalogue: up to 1,806
/// archives in one request) must measure free space ONCE per volume and pass it to every
/// per-archive check, or the page blocks for as many full disk enumerations as it has archives.
/// Do not "simplify" this back to calling `available_space` in here -- that reintroduces the
/// per-archive re-enumeration this parameter exists to avoid.
///
/// `None` keeps `scope_check`'s existing meaning: free space could not be determined, so refuse
/// rather than guess.
pub fn scope_check_with_space(
    cat: &Catalog,
    mount_root: &Path,
    volume_id: &str,
    archive_rel: &str,
    free_bytes: Option<u64>,
) -> anyhow::Result<Scope> {
    let entries = cat.archive_entries(volume_id, archive_rel)?;
    if entries.is_empty() {
        return Ok(Scope::Refused(
            "no catalogued entries: nothing to verify an extraction against".into(),
        ));
    }

    for e in &entries {
        let chain = e.container_chain.as_deref().unwrap_or_default();
        if !fits_budget(mount_root, archive_rel, chain) {
            let n = full_length(mount_root, archive_rel, chain);
            return Ok(Scope::Refused(format!(
                "entry would need {n} characters, over the {MAX_PATH_CHARS} limit: {chain}"
            )));
        }
        // A purged or quarantined row can still own the loose path this entry would take.
        let target = final_relative_path(archive_rel, chain);
        if cat.loose_path_taken(volume_id, &target)? {
            return Ok(Scope::Refused(format!(
                "the catalogue already has a loose file at {target}"
            )));
        }
    }

    let dest = destination_dir(archive_rel);
    if mount_root.join(&dest).exists() {
        return Ok(Scope::Refused(format!(
            "destination folder {dest} already exists; refusing to merge into it"
        )));
    }

    let uncompressed: i64 = entries.iter().map(|e| e.size_bytes).sum();
    let required = uncompressed as u64 + SPACE_HEADROOM_BYTES;
    match free_bytes {
        Some(free) if free < required => {
            return Ok(Scope::Refused(format!(
                "needs {required} bytes free (content {uncompressed} + 5 GiB headroom), {free} available"
            )));
        }
        Some(_) => {}
        None => {
            return Ok(Scope::Refused(
                "could not determine free space on the drive; refusing rather than guessing".into(),
            ));
        }
    }

    Ok(Scope::InScope {
        entries: entries.len(),
        uncompressed_bytes: uncompressed,
    })
}

/// Zip slip guard shared by every archive format this extractor writes: a crafted entry name must
/// never write outside the destination. Checked via `Path::components()` rather than ad-hoc string
/// matching: `..`/`.`/empty segments are one case, but on Windows a drive-relative name like
/// `C:evil.txt` has no `..` and is not `is_absolute()` either (a prefix with no root) -- yet
/// `PathBuf::push` documents that joining such a path REPLACES the base rather than appending to
/// it, so `dest_abs.join` would silently escape the destination entirely. Rejecting every
/// component that is not `Component::Normal` catches `Prefix`, `RootDir`, `CurDir` and
/// `ParentDir` uniformly, on both platforms. Entry names in both formats are `/`-separated by
/// spec, so a literal `\` is a lying name regardless of what `components()` makes of it -- kept as
/// an explicit check. An EMPTY name is also checked explicitly: `Path::new("").components()`
/// yields no components at all, so `.all(..)` over it is vacuously true and would let an empty
/// name through -- which `dest_abs.join("")` resolves to `dest_abs` itself, i.e. the extractor
/// would try to write a file entry directly onto the destination directory.
///
/// Shared by `write_zip_level` and `write_7z_level` rather than duplicated: this check is a
/// security boundary, and duplicated logic across a security boundary is how the two copies drift
/// apart.
fn entry_name_escapes(name: &str) -> bool {
    name.is_empty()
        || name.contains('\\')
        || !Path::new(name)
            .components()
            .all(|c| matches!(c, std::path::Component::Normal(_)))
}

/// Write every file entry of ONE archive level into `dest_abs`, recreating the archive's own
/// layout: a nested archive lands as a file, not as a folder. Returns the destination-relative
/// paths written, so the caller can clean up precisely and verify what it asked for.
///
/// Dispatches on the archive's own content signature -- the same content-based decision
/// `archive::scan_level` makes when descending into it during a scan -- rather than trusting the
/// filename: a renamed 7z is still a 7z, and the deny-list check upstream already decides whether
/// an extension is extractable at all.
///
/// Any error leaves cleanup to the caller (`extract_archive` deletes the whole destination), which
/// is why neither format-specific function below ever tries to unwind half of its own work.
pub fn write_level(
    archive_path: &Path,
    dest_abs: &Path,
    limits: &crate::archive::ArchiveLimits,
) -> anyhow::Result<Vec<String>> {
    let mut head = [0u8; 6];
    let filled = {
        let mut f = std::fs::File::open(archive_path)?;
        let mut filled = 0;
        while filled < head.len() {
            match f.read(&mut head[filled..])? {
                0 => break,
                n => filled += n,
            }
        }
        filled
    };
    if crate::archive::looks_like_7z(&head[..filled]) {
        write_7z_level(archive_path, dest_abs, limits)
    } else {
        write_zip_level(archive_path, dest_abs, limits)
    }
}

/// The zip half of `write_level`'s contract. See `write_level`'s doc comment for the shared
/// promises (zip-slip guard, `entry_max_bytes` on actual bytes read, returned paths in archive
/// order, no partial-unwind cleanup).
fn write_zip_level(
    archive_path: &Path,
    dest_abs: &Path,
    limits: &crate::archive::ArchiveLimits,
) -> anyhow::Result<Vec<String>> {
    let file = std::fs::File::open(archive_path)?;
    let mut archive = zip::ZipArchive::new(std::io::BufReader::new(file))?;
    let mut written = Vec::new();

    for i in 0..archive.len() {
        let mut entry = archive.by_index(i)?;
        if !entry.is_file() {
            continue;
        }
        let name = entry.name().to_string();

        if entry_name_escapes(&name) {
            anyhow::bail!("entry name escapes the destination folder: {name}");
        }

        let declared = entry.size();
        let compressed = entry.compressed_size().max(1);
        if declared / compressed > limits.ratio_cap {
            anyhow::bail!(
                "entry {name} declares a {}:1 ratio, over the cap {}",
                declared / compressed,
                limits.ratio_cap
            );
        }

        let out_path = dest_abs.join(&name);
        if let Some(p) = out_path.parent() {
            std::fs::create_dir_all(p)?;
        }
        let mut out = std::io::BufWriter::new(std::fs::File::create(&out_path)?);
        let cap = limits.entry_max_bytes.unwrap_or(u64::MAX);
        let mut buf = [0u8; 64 * 1024];
        let mut total: u64 = 0;
        loop {
            let n = entry.read(&mut buf)?;
            if n == 0 {
                break;
            }
            total += n as u64;
            if total > cap {
                anyhow::bail!("entry {name} exceeds the size cap {cap} bytes");
            }
            std::io::Write::write_all(&mut out, &buf[..n])?;
        }
        std::io::Write::flush(&mut out)?;
        written.push(name);
    }
    Ok(written)
}

/// The 7z half of `write_level`'s contract. Same zip-slip guard and `entry_max_bytes` enforcement
/// as `write_zip_level`, via the shared `entry_name_escapes` and the same cap-on-actual-bytes loop.
///
/// `ratio_cap` does NOT apply here -- see `ArchiveLimits::ratio_cap`'s doc comment: solid
/// compression leaves `compressed_size` unfilled (0) for every entry but the first in each folder,
/// so `entry_max_bytes`, enforced on the real decoded byte count as it streams through, is the
/// sole guard for this format. `scan_7z_level` in `archive.rs` made and documented the identical
/// decision for the read side; this is the same tradeoff on the write side.
///
/// Uses `sevenz_rust2::SevenZReader::for_each_entries`, the same streaming idiom
/// `archive::scan_7z_level` established: each entry arrives as a live `&mut dyn Read`, decoded
/// lazily as the closure reads it, so a guard failure mid-entry (zip-slip, size cap) can be
/// surfaced as soon as it is detected without buffering the whole entry first.
fn write_7z_level(
    archive_path: &Path,
    dest_abs: &Path,
    limits: &crate::archive::ArchiveLimits,
) -> anyhow::Result<Vec<String>> {
    let file = std::fs::File::open(archive_path)?;
    let mut archive = sevenz_rust2::SevenZReader::new(
        std::io::BufReader::new(file),
        sevenz_rust2::Password::empty(),
    )?;
    let cap = limits.entry_max_bytes.unwrap_or(u64::MAX);
    let mut written = Vec::new();

    archive.for_each_entries(|entry, r| {
        if entry.is_directory {
            return Ok(true);
        }
        let name = entry.name.clone();

        if entry_name_escapes(&name) {
            return Err(sevenz_rust2::Error::other(format!(
                "entry name escapes the destination folder: {name}"
            )));
        }

        let out_path = dest_abs.join(&name);
        if let Some(p) = out_path.parent() {
            std::fs::create_dir_all(p)
                .map_err(|e| sevenz_rust2::Error::io_msg(e, "creating destination directory"))?;
        }
        let mut out = std::io::BufWriter::new(
            std::fs::File::create(&out_path)
                .map_err(|e| sevenz_rust2::Error::io_msg(e, "creating destination file"))?,
        );
        let mut buf = [0u8; 64 * 1024];
        let mut total: u64 = 0;
        loop {
            let n = r.read(&mut buf)?;
            if n == 0 {
                break;
            }
            total += n as u64;
            if total > cap {
                return Err(sevenz_rust2::Error::other(format!(
                    "entry {name} exceeds the size cap {cap} bytes"
                )));
            }
            std::io::Write::write_all(&mut out, &buf[..n])
                .map_err(|e| sevenz_rust2::Error::io_msg(e, "writing destination file"))?;
        }
        std::io::Write::flush(&mut out)
            .map_err(|e| sevenz_rust2::Error::io_msg(e, "flushing destination file"))?;
        written.push(name);
        Ok(true)
    })?;

    Ok(written)
}

/// Re-derive every catalogued chain from what is now on disk and compare hashes. Loose files are
/// hashed directly; a nested archive that was written as a file is descended into with the
/// scanner's own reader, so its chains come out identical to the ones already catalogued.
///
/// This is the step that makes quarantining the original safe. Without it, the original is removed
/// on nothing but the assumption that the extraction worked.
pub fn verify_destination(
    cat: &Catalog,
    volume_id: &str,
    archive_rel: &str,
    dest_abs: &Path,
    limits: &crate::archive::ArchiveLimits,
) -> anyhow::Result<()> {
    // chain -> hash, as it exists on disk right now.
    let mut found: HashMap<String, String> = HashMap::new();
    for entry in walkdir::WalkDir::new(dest_abs).into_iter() {
        // A dropped traversal error (a locked file, a permission-denied subdirectory, a disk
        // hiccup -- all plausible on an external HDD mid-verification) must not be silently
        // swallowed: `filter_map(Result::ok)` would turn a real I/O failure into a false
        // "missing entry" report below, naming the wrong problem. Name the path and the failure
        // instead, so the refusal says what actually happened.
        let entry = entry.map_err(|e| {
            let path = e
                .path()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| dest_abs.display().to_string());
            anyhow::anyhow!("could not read {path}: {e}")
        })?;
        if !entry.file_type().is_file() {
            continue;
        }
        let rel = entry
            .path()
            .strip_prefix(dest_abs)?
            .to_string_lossy()
            .replace('\\', "/");
        let ext = rel
            .rsplit('/')
            .next()
            .and_then(|l| l.rsplit_once('.'))
            .map(|(_, e)| e.to_ascii_lowercase())
            .unwrap_or_default();

        let descend = matches!(
            crate::archive::descent_for(&ext, &limits.deny_extensions, &limits.allow_extensions),
            crate::archive::Descent::Descend
        );
        if descend {
            let f = std::fs::File::open(entry.path())
                .with_context(|| format!("could not read {rel}"))?;
            let scanned = crate::archive::scan_archive(std::io::BufReader::new(f), limits);
            for e in scanned.entries {
                found.insert(
                    format!("{rel}{}{}", CHAIN_SEP, e.container_chain),
                    e.content_hash,
                );
            }
        }
        // A descendable archive is ALSO a file in its own right; record it either way, because a
        // catalogued entry may point at the archive itself when the scanner stopped at max depth.
        let hash = crate::hashing::hash_file(entry.path())
            .with_context(|| format!("could not read {rel}"))?;
        found.insert(rel, hash);
    }

    for row in cat.archive_entries(volume_id, archive_rel)? {
        let chain = row.container_chain.clone().unwrap_or_default();
        match found.get(&chain) {
            None => anyhow::bail!("extracted content is missing the catalogued entry {chain}"),
            Some(h) if h != &row.content_hash => anyhow::bail!(
                "hash mismatch for {chain}: catalogue {}, extracted {h}",
                row.content_hash
            ),
            Some(_) => {}
        }
    }
    Ok(())
}

/// What one successful extraction did.
#[derive(Debug)]
pub struct ExtractOutcome {
    pub entries_converted: usize,
    pub bytes_written: u64,
    /// Forward-slashed, relative to the mount root.
    pub dest_relative_path: String,
    /// False only when the original could not be quarantined — reported, never silent.
    pub quarantined: bool,
    /// Archives written by this level, relative to the mount root, for the caller to enqueue.
    pub nested_archives: Vec<String>,
}

/// Extract one catalogued archive, prove it, then quarantine the original.
///
/// The order below is the safety property, not an implementation detail: the archive is untouched
/// until verification has passed, and any failure after the first byte is written deletes the
/// destination and returns the drive to exactly its previous state.
pub fn extract_archive(
    cat: &Catalog,
    mount_root: &Path,
    expected_volume_id: &str,
    archive_rel: &str,
    limits: &crate::archive::ArchiveLimits,
    now: i64,
) -> anyhow::Result<ExtractOutcome> {
    // 1. Marker gate. Same rule as quarantine and repack: never write to an unidentified drive.
    match crate::volume::read_volume_id(mount_root) {
        Some(vid) if vid == expected_volume_id => {}
        Some(vid) => anyhow::bail!(
            "drive at {} is volume {vid}, not the expected {expected_volume_id}; aborting",
            mount_root.display()
        ),
        None => anyhow::bail!(
            "no identity marker at {}; refusing to extract on an unidentified drive",
            mount_root.display()
        ),
    }

    // 2. The deny list decides what is an archive at all. A .docx is a zip, and exploding one
    // destroys the document.
    let ext = archive_rel
        .rsplit('/')
        .next()
        .and_then(|l| l.rsplit_once('.'))
        .map(|(_, e)| e.to_ascii_lowercase())
        .unwrap_or_default();
    if !matches!(
        crate::archive::descent_for(&ext, &limits.deny_extensions, &limits.allow_extensions),
        crate::archive::Descent::Descend
    ) {
        anyhow::bail!("{ext} is not an extractable archive format on this configuration");
    }

    let archive_path = mount_root.join(archive_rel);
    if !archive_path.is_file() {
        anyhow::bail!(
            "archive {archive_rel} is not on disk at {}",
            mount_root.display()
        );
    }

    // 3-5. Every refusal, before a byte is written.
    match scope_check(cat, mount_root, expected_volume_id, archive_rel)? {
        Scope::InScope { .. } => {}
        Scope::Refused(reason) => anyhow::bail!("{archive_rel}: {reason}"),
    }

    let dest_rel = destination_dir(archive_rel);
    let dest_abs = mount_root.join(&dest_rel);

    // A cleanup that fails to remove a partial destination must not swallow the real error: log
    // it so a stray leftover folder is something the user can be told about, not a silent gap
    // between what the catalogue believes and what is actually on disk.
    let cleanup = |dest_abs: &Path, now: i64| {
        if let Err(e) = std::fs::remove_dir_all(dest_abs) {
            if e.kind() != std::io::ErrorKind::NotFound {
                let _ = cat.log_action(
                    "extract_cleanup_failed",
                    &serde_json::json!({
                        "volume_id": expected_volume_id, "archive": archive_rel,
                        "dest": dest_abs.display().to_string(), "error": e.to_string(),
                    })
                    .to_string(),
                    now,
                );
            }
        }
    };

    // 6-8. Write, verify, and on ANY failure remove everything this call created.
    let written = match write_level(&archive_path, &dest_abs, limits).and_then(|w| {
        verify_destination(cat, expected_volume_id, archive_rel, &dest_abs, limits).map(|_| w)
    }) {
        Ok(w) => w,
        Err(e) => {
            cleanup(&dest_abs, now);
            cat.log_action(
                "extract_failed",
                &serde_json::json!({
                    "volume_id": expected_volume_id, "archive": archive_rel, "error": e.to_string()
                })
                .to_string(),
                now,
            )?;
            return Err(e);
        }
    };

    let bytes_written: u64 = written
        .iter()
        .filter_map(|p| std::fs::metadata(dest_abs.join(p)).ok())
        .map(|m| m.len())
        .sum();

    // 9a. Row conversion. Still reversible: if this fails, the destination goes and the archive
    // stays, so disk and catalogue cannot disagree.
    let rows = cat.archive_entries(expected_volume_id, archive_rel)?;
    let mut moves = Vec::with_capacity(rows.len());
    for row in &rows {
        let chain = row.container_chain.clone().unwrap_or_default();
        let (rel, rest) = first_hop(archive_rel, &chain);
        moves.push(crate::catalog::store::EntryMove {
            id: row.id,
            relative_path: rel,
            container_chain: rest,
        });
    }
    if let Err(e) = cat.convert_archive_entries(&moves, now) {
        cleanup(&dest_abs, now);
        cat.log_action(
            "extract_failed",
            &serde_json::json!({
                "volume_id": expected_volume_id, "archive": archive_rel,
                "error": format!("catalogue conversion failed: {e}"),
            })
            .to_string(),
            now,
        )?;
        return Err(e.context("catalogue conversion failed; extraction rolled back"));
    }

    // 9b. Quarantine the original through the dedicated extraction path (see
    // `quarantine_extracted_archive`'s doc comment for why it, not `quarantine_files`, is correct
    // here). By this point the entry rows are already converted and the files are already on
    // disk: extraction itself has fully succeeded, so a HARD failure to quarantine (a
    // marker/volume mismatch from a drive unplugged mid-operation, say) must not be reported as a
    // failure of the whole operation -- that would tell the caller nothing happened when in fact
    // everything except the final rename did. It is recorded and folded into `quarantined: false`
    // instead, exactly like an ordinary guard skip.
    let archive_id = cat
        .loose_file_id(expected_volume_id, archive_rel)?
        .ok_or_else(|| anyhow::anyhow!("no loose catalogue row for {archive_rel}"))?;
    let q = match crate::quarantine::quarantine_extracted_archive(
        cat,
        mount_root,
        expected_volume_id,
        archive_id,
        now,
    ) {
        Ok(q) => q,
        Err(e) => {
            cat.log_action(
                "extract_quarantine_failed",
                &serde_json::json!({
                    "volume_id": expected_volume_id, "archive": archive_rel, "error": e.to_string()
                })
                .to_string(),
                now,
            )?;
            crate::quarantine::QuarantineOutcome {
                quarantined: 0,
                skipped: 1,
            }
        }
    };

    // 10. Anything written that is itself an archive is the caller's next job.
    let nested: Vec<String> = written
        .iter()
        .filter(|p| {
            let e = p
                .rsplit('/')
                .next()
                .and_then(|l| l.rsplit_once('.'))
                .map(|(_, e)| e.to_ascii_lowercase())
                .unwrap_or_default();
            matches!(
                crate::archive::descent_for(&e, &limits.deny_extensions, &limits.allow_extensions),
                crate::archive::Descent::Descend
            )
        })
        .map(|p| format!("{dest_rel}/{p}"))
        .collect();

    cat.log_action(
        "extract",
        &serde_json::json!({
            "volume_id": expected_volume_id, "archive": archive_rel, "dest": dest_rel,
            "entries": moves.len(), "bytes": bytes_written, "quarantined": q.quarantined == 1,
        })
        .to_string(),
        now,
    )?;

    Ok(ExtractOutcome {
        entries_converted: moves.len(),
        bytes_written,
        dest_relative_path: dest_rel,
        quarantined: q.quarantined == 1,
        nested_archives: nested,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

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

    fn write_zip(path: &Path, files: &[(&str, &[u8])]) {
        use std::io::Write;
        let f = std::fs::File::create(path).unwrap();
        let mut zw = zip::ZipWriter::new(f);
        let opts: zip::write::FileOptions<()> =
            zip::write::FileOptions::default().compression_method(zip::CompressionMethod::Stored);
        for (name, bytes) in files {
            zw.start_file(*name, opts).unwrap();
            zw.write_all(bytes).unwrap();
        }
        zw.finish().unwrap();
    }

    /// As `write_zip`, but a 7z, using the same `sevenz_rust2::SevenZWriter` idiom the crate's own
    /// `archive.rs` test fixtures already established.
    fn write_7z(path: &Path, files: &[(&str, &[u8])]) {
        let mut sz = sevenz_rust2::SevenZWriter::create(path).unwrap();
        for (name, bytes) in files {
            sz.push_archive_entry(
                sevenz_rust2::SevenZArchiveEntry::new_file(name),
                Some(*bytes),
            )
            .unwrap();
        }
        sz.finish().unwrap();
    }

    /// A catalogue that mirrors what a real scan produces for a small zip on a temp "drive": the
    /// archive itself catalogued as a loose file (as `scanner.rs` does via `upsert_file`), plus
    /// every entry inside it catalogued by running the real `archive::scan_archive` over the real
    /// bytes on disk -- so the hashes under test are the ones a genuine scan would have recorded,
    /// never hand-written, and cannot silently drift from what `write_level`/`verify_destination`
    /// actually produce.
    fn real_scan_fixture(
        tmp: &tempfile::TempDir,
        files: &[(&str, &[u8])],
    ) -> (Catalog, std::path::PathBuf) {
        real_scan_fixture_named(tmp, "bundle.zip", files)
    }

    /// As `real_scan_fixture`, but with a caller-chosen archive filename -- for the deny-list test,
    /// where the extension itself is the thing under test.
    fn real_scan_fixture_named(
        tmp: &tempfile::TempDir,
        archive_name: &str,
        files: &[(&str, &[u8])],
    ) -> (Catalog, std::path::PathBuf) {
        real_scan_fixture_writing(tmp, archive_name, files, write_zip)
    }

    /// As `real_scan_fixture`, but for `bundle.7z`, built through the real `sevenz_rust2` writer --
    /// so the hashes under test come from a genuine 7z, not a zip wearing a `.7z` name.
    fn real_scan_fixture_7z(
        tmp: &tempfile::TempDir,
        files: &[(&str, &[u8])],
    ) -> (Catalog, std::path::PathBuf) {
        real_scan_fixture_writing(tmp, "bundle.7z", files, write_7z)
    }

    /// Shared body of `real_scan_fixture_named` and `real_scan_fixture_7z`: everything but writing
    /// the archive's own bytes is identical between formats, so `write` -- `write_zip` or
    /// `write_7z` -- is the only piece that differs.
    fn real_scan_fixture_writing(
        tmp: &tempfile::TempDir,
        archive_name: &str,
        files: &[(&str, &[u8])],
        write: impl FnOnce(&Path, &[(&str, &[u8])]),
    ) -> (Catalog, std::path::PathBuf) {
        std::fs::write(tmp.path().join(crate::volume::MARKER), "vol-1").unwrap();
        let archive_path = tmp.path().join(archive_name);
        write(&archive_path, files);

        let cat = Catalog::open(&tmp.path().join("c.db")).unwrap();
        cat.upsert_volume(&crate::catalog::models::Volume {
            volume_id: "vol-1".into(),
            label: "T".into(),
            identified_by: "marker".into(),
            first_seen_at: 1,
            last_seen_at: 1,
        })
        .unwrap();

        // The archive's own loose row, exactly as the scanner records it -- extract_archive quarantines
        // through this row, so a fixture without it would fail at the quarantine step, not the step
        // under test.
        let ext = archive_name
            .rsplit('/')
            .next()
            .and_then(|l| l.rsplit_once('.'))
            .map(|(_, e)| e.to_string())
            .unwrap_or_default();
        cat.upsert_file(
            &crate::catalog::models::NewFile {
                volume_id: "vol-1".into(),
                relative_path: archive_name.to_string(),
                filename: archive_name.to_string(),
                extension: ext.clone(),
                size_bytes: std::fs::metadata(&archive_path).unwrap().len() as i64,
                content_hash: crate::hashing::hash_file(&archive_path).unwrap(),
                created_time: None,
                modified_time: None,
                accessed_time: None,
                category: crate::category::Category::from_extension(&ext),
                container_chain: None,
            },
            1,
        )
        .unwrap();

        // The archive's contents, via the real (recursive) scanner logic.
        let f = std::fs::File::open(&archive_path).unwrap();
        let scanned = crate::archive::scan_archive(std::io::BufReader::new(f), &test_limits());
        for e in scanned.entries {
            cat.upsert_archive_entry("vol-1", archive_name, &e, None, 1)
                .unwrap();
        }

        (cat, archive_path)
    }

    /// `bundle.zip` containing `inner.zip` containing `deep.txt`, catalogued by a real (recursive)
    /// scan -- so the flattened chain `inner.zip › deep.txt` is exactly what the scanner would have
    /// written, with no catalogue row for `inner.zip` itself.
    fn real_nested_scan_fixture(tmp: &tempfile::TempDir) -> (Catalog, std::path::PathBuf) {
        let inner_tmp = tmp.path().join("inner_tmp.zip");
        write_zip(&inner_tmp, &[("deep.txt", b"DEEP CONTENT")]);
        let inner_bytes = std::fs::read(&inner_tmp).unwrap();
        std::fs::remove_file(&inner_tmp).unwrap();

        real_scan_fixture(tmp, &[("inner.zip", &inner_bytes)])
    }

    /// Corrupt a catalogued entry's hash in place. `verify_destination` checks the catalogue's
    /// hash, so poisoning it here stands in for a genuinely corrupted extraction without having to
    /// tamper with bytes already proven correct by `write_level`.
    fn poison_entry_hash(cat: &Catalog, volume_id: &str, archive_rel: &str, chain: &str) {
        let n = cat
            .conn
            .execute(
                "UPDATE files SET content_hash='deadbeef' \
                 WHERE volume_id=?1 AND relative_path=?2 AND container_chain=?3",
                rusqlite::params![volume_id, archive_rel, chain],
            )
            .unwrap();
        assert_eq!(n, 1, "expected exactly one entry to poison");
    }

    #[test]
    fn a_small_zip_extracts_verifies_and_its_original_is_quarantined() {
        let tmp = tempfile::tempdir().unwrap();
        let (cat, _) = real_scan_fixture(&tmp, &[("a.txt", b"AAA"), ("sub/b.txt", b"BBB")]);

        let out =
            extract_archive(&cat, tmp.path(), "vol-1", "bundle.zip", &test_limits(), 100).unwrap();

        assert_eq!(out.entries_converted, 2);
        assert_eq!(out.dest_relative_path, "bundle");
        // Quarantine now runs through `quarantine_extracted_archive`, whose precondition is
        // "no catalogued row still points inside this archive" rather than "another copy of
        // these exact bytes exists elsewhere" -- see its doc comment in quarantine.rs for why
        // that is the correct proof for an archive that has just been converted. Both rows were
        // converted above, so the precondition holds and this must actually quarantine.
        assert!(out.quarantined);
        assert_eq!(
            std::fs::read(tmp.path().join("bundle/a.txt")).unwrap(),
            b"AAA"
        );
        assert!(
            !tmp.path().join("bundle.zip").is_file(),
            "original moved out of the way"
        );
        assert!(
            tmp.path().join("_ToDelete/bundle.zip").is_file(),
            "original is in quarantine, never deleted"
        );
        let loose = cat.loose_file_id("vol-1", "bundle/a.txt").unwrap();
        assert!(
            loose.is_some(),
            "extracted file is now a loose catalogue row"
        );
    }

    #[test]
    fn a_7z_extracts_and_verifies_against_its_catalogued_hashes() {
        let tmp = tempfile::tempdir().unwrap();
        let (cat, _) = real_scan_fixture_7z(&tmp, &[("a.txt", b"AAA")]);

        let out =
            extract_archive(&cat, tmp.path(), "vol-1", "bundle.7z", &test_limits(), 100).unwrap();

        assert_eq!(out.entries_converted, 1);
        assert_eq!(
            std::fs::read(tmp.path().join("bundle/a.txt")).unwrap(),
            b"AAA"
        );
        assert!(tmp.path().join("_ToDelete/bundle.7z").is_file());
    }

    #[test]
    fn a_corrupt_entry_fails_the_archive_and_leaves_the_drive_untouched() {
        let tmp = tempfile::tempdir().unwrap();
        let (cat, _) = real_scan_fixture(&tmp, &[("a.txt", b"AAA")]);
        // Poison the catalogue so verification cannot match: same effect as a corrupt extraction,
        // and it is the catalogue's hash that the guarantee is written against.
        poison_entry_hash(&cat, "vol-1", "bundle.zip", "a.txt");

        let err = extract_archive(&cat, tmp.path(), "vol-1", "bundle.zip", &test_limits(), 100)
            .unwrap_err();

        assert!(format!("{err}").contains("hash mismatch"), "{err}");
        assert!(!tmp.path().join("bundle").exists(), "destination deleted");
        assert!(
            tmp.path().join("bundle.zip").is_file(),
            "original untouched"
        );
        assert!(
            cat.archive_entries("vol-1", "bundle.zip").unwrap()[0]
                .container_chain
                .is_some(),
            "catalogue untouched"
        );
    }

    #[test]
    fn the_wrong_drive_is_refused_before_anything_is_written() {
        let tmp = tempfile::tempdir().unwrap();
        let (cat, _) = real_scan_fixture(&tmp, &[("a.txt", b"AAA")]);

        let err = extract_archive(
            &cat,
            tmp.path(),
            "vol-OTHER",
            "bundle.zip",
            &test_limits(),
            100,
        )
        .unwrap_err();

        assert!(format!("{err}").contains("vol-OTHER"), "{err}");
        assert!(!tmp.path().join("bundle").exists());
    }

    #[test]
    fn a_nested_archive_is_reported_for_a_follow_up_job() {
        let tmp = tempfile::tempdir().unwrap();
        let (cat, _) = real_nested_scan_fixture(&tmp); // bundle.zip > inner.zip > deep.txt

        let out =
            extract_archive(&cat, tmp.path(), "vol-1", "bundle.zip", &test_limits(), 100).unwrap();

        assert_eq!(out.nested_archives, vec!["bundle/inner.zip".to_string()]);
        assert!(
            tmp.path().join("bundle/inner.zip").is_file(),
            "inner archive is a real file"
        );
        let row = cat.archive_entries("vol-1", "bundle/inner.zip").unwrap();
        assert_eq!(
            row.len(),
            1,
            "the deep entry now hangs off the inner archive's new path"
        );
        assert_eq!(row[0].container_chain.as_deref(), Some("deep.txt"));
    }

    #[test]
    fn a_deny_listed_zip_document_is_never_extracted() {
        let tmp = tempfile::tempdir().unwrap();
        let (cat, _) = real_scan_fixture_named(&tmp, "report.docx", &[("word/doc.xml", b"<x/>")]);

        let err = extract_archive(
            &cat,
            tmp.path(),
            "vol-1",
            "report.docx",
            &test_limits(),
            100,
        )
        .unwrap_err();

        assert!(format!("{err}").contains("docx"), "{err}");
        assert!(
            tmp.path().join("report.docx").is_file(),
            "the document survives intact"
        );
    }

    #[test]
    fn write_level_recreates_the_archive_layout_verbatim() {
        let tmp = tempfile::tempdir().unwrap();
        let zip_path = tmp.path().join("bundle.zip");
        write_zip(&zip_path, &[("a.txt", b"AAA"), ("sub/b.txt", b"BBB")]);
        let dest = tmp.path().join("bundle");

        let written = write_level(&zip_path, &dest, &test_limits()).unwrap();

        assert_eq!(written, vec!["a.txt".to_string(), "sub/b.txt".to_string()]);
        assert_eq!(std::fs::read(dest.join("a.txt")).unwrap(), b"AAA");
        assert_eq!(std::fs::read(dest.join("sub/b.txt")).unwrap(), b"BBB");
    }

    #[test]
    fn write_level_refuses_a_path_that_escapes_the_destination() {
        let tmp = tempfile::tempdir().unwrap();
        let zip_path = tmp.path().join("evil.zip");
        write_zip(&zip_path, &[("../escaped.txt", b"NOPE")]);
        let dest = tmp.path().join("evil");

        let err = write_level(&zip_path, &dest, &test_limits()).unwrap_err();
        assert!(format!("{err}").contains("escapes"), "got: {err}");
        assert!(
            !tmp.path().join("escaped.txt").exists(),
            "zip-slip must write nothing"
        );
    }

    #[test]
    fn write_level_refuses_a_windows_drive_relative_entry_name() {
        // `C:evil.txt` has no `..` and Path::is_absolute() is false for it (a prefix with no
        // root), but PathBuf::push documents that joining such a path REPLACES the base path
        // rather than appending to it -- so `dest_abs.join("C:evil.txt")` would silently discard
        // `dest_abs` entirely on Windows. This is the exact bypass the components() check exists
        // to catch.
        let tmp = tempfile::tempdir().unwrap();
        let zip_path = tmp.path().join("evil2.zip");
        write_zip(&zip_path, &[("C:evil.txt", b"NOPE")]);
        let dest = tmp.path().join("evil2");

        let err = write_level(&zip_path, &dest, &test_limits()).unwrap_err();
        assert!(format!("{err}").contains("escapes"), "got: {err}");
        assert!(
            !tmp.path().join("evil.txt").exists(),
            "drive-relative zip-slip must write nothing"
        );
    }

    #[test]
    fn write_level_refuses_an_empty_entry_name() {
        // `Path::new("").components()` yields NO components, so `.all(..)` over an empty iterator
        // is vacuously true -- the components()-only guard would let an empty name through, and
        // `dest_abs.join("")` resolves to `dest_abs` itself, i.e. the extractor would try to write
        // a file entry directly onto the destination directory. This is not a hypothetical: the
        // `zip` crate itself refuses to CREATE an entry named "", so this test builds the zip's raw
        // bytes by hand to prove the guard still catches a name a crafted (not this tool's own)
        // archive could carry.
        let tmp = tempfile::tempdir().unwrap();
        let zip_path = tmp.path().join("evil3.zip");

        // `zip::write::FileOptions::start_file("", ..)` panics/errors in this crate version, so an
        // empty-named entry cannot be produced through the writer API -- confirmed by hand before
        // writing this test. Build the archive by hand: one stored, zero-length local file header
        // with a zero-length name, followed by a matching zero-length-name central directory record
        // and EOCD, per the ZIP local/central file header layout (APPNOTE.TXT 4.3.7 / 4.3.12).
        let mut bytes = Vec::new();
        let lfh_start = bytes.len() as u32;
        bytes.extend_from_slice(&0x04034b50u32.to_le_bytes()); // local file header signature
        bytes.extend_from_slice(&20u16.to_le_bytes()); // version needed
        bytes.extend_from_slice(&0u16.to_le_bytes()); // flags
        bytes.extend_from_slice(&0u16.to_le_bytes()); // method: stored
        bytes.extend_from_slice(&0u16.to_le_bytes()); // mod time
        bytes.extend_from_slice(&0u16.to_le_bytes()); // mod date
        bytes.extend_from_slice(&0u32.to_le_bytes()); // crc32 (empty content)
        bytes.extend_from_slice(&0u32.to_le_bytes()); // compressed size
        bytes.extend_from_slice(&0u32.to_le_bytes()); // uncompressed size
        bytes.extend_from_slice(&0u16.to_le_bytes()); // name length: 0
        bytes.extend_from_slice(&0u16.to_le_bytes()); // extra length: 0
                                                      // name (empty), extra (empty), data (empty) -- nothing to append

        let cdh_start = bytes.len() as u32;
        bytes.extend_from_slice(&0x02014b50u32.to_le_bytes()); // central directory signature
        bytes.extend_from_slice(&20u16.to_le_bytes()); // version made by
        bytes.extend_from_slice(&20u16.to_le_bytes()); // version needed
        bytes.extend_from_slice(&0u16.to_le_bytes()); // flags
        bytes.extend_from_slice(&0u16.to_le_bytes()); // method: stored
        bytes.extend_from_slice(&0u16.to_le_bytes()); // mod time
        bytes.extend_from_slice(&0u16.to_le_bytes()); // mod date
        bytes.extend_from_slice(&0u32.to_le_bytes()); // crc32
        bytes.extend_from_slice(&0u32.to_le_bytes()); // compressed size
        bytes.extend_from_slice(&0u32.to_le_bytes()); // uncompressed size
        bytes.extend_from_slice(&0u16.to_le_bytes()); // name length: 0
        bytes.extend_from_slice(&0u16.to_le_bytes()); // extra length: 0
        bytes.extend_from_slice(&0u16.to_le_bytes()); // comment length: 0
        bytes.extend_from_slice(&0u16.to_le_bytes()); // disk number start
        bytes.extend_from_slice(&0u16.to_le_bytes()); // internal attrs
        bytes.extend_from_slice(&0u32.to_le_bytes()); // external attrs
        bytes.extend_from_slice(&lfh_start.to_le_bytes()); // local header offset
                                                           // name (empty)
        let cdh_len = (bytes.len() as u32) - cdh_start;

        bytes.extend_from_slice(&0x06054b50u32.to_le_bytes()); // EOCD signature
        bytes.extend_from_slice(&0u16.to_le_bytes()); // disk number
        bytes.extend_from_slice(&0u16.to_le_bytes()); // disk with central dir
        bytes.extend_from_slice(&1u16.to_le_bytes()); // entries on this disk
        bytes.extend_from_slice(&1u16.to_le_bytes()); // total entries
        bytes.extend_from_slice(&cdh_len.to_le_bytes()); // central dir size
        bytes.extend_from_slice(&cdh_start.to_le_bytes()); // central dir offset
        bytes.extend_from_slice(&0u16.to_le_bytes()); // comment length: 0

        std::fs::write(&zip_path, &bytes).unwrap();

        let dest = tmp.path().join("evil3");

        let err = write_level(&zip_path, &dest, &test_limits()).unwrap_err();
        assert!(format!("{err}").contains("escapes"), "got: {err}");
        assert!(
            !dest.exists(),
            "empty-name zip-slip must write nothing onto the destination path"
        );
    }

    #[test]
    fn write_level_honours_the_entry_size_cap() {
        let tmp = tempfile::tempdir().unwrap();
        let zip_path = tmp.path().join("big.zip");
        write_zip(&zip_path, &[("big.bin", &vec![7u8; 4096])]);
        let dest = tmp.path().join("big");
        let mut limits = test_limits();
        limits.entry_max_bytes = Some(1024);

        let err = write_level(&zip_path, &dest, &limits).unwrap_err();
        assert!(
            format!("{err}").contains("1024"),
            "must report the cap: {err}"
        );
    }

    #[test]
    fn write_level_dispatches_to_7z_by_content_and_recreates_its_layout() {
        // `write_level` is the content-based dispatcher (same idea as `archive::scan_level`): a
        // `.7z` on disk, regardless of what write_zip_level would do with it, must come out through
        // the 7z reader with the archive's own layout intact.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("bundle.7z");
        write_7z(&path, &[("a.txt", b"AAA"), ("sub/b.txt", b"BBB")]);
        let dest = tmp.path().join("bundle");

        let written = write_level(&path, &dest, &test_limits()).unwrap();

        assert_eq!(written, vec!["a.txt".to_string(), "sub/b.txt".to_string()]);
        assert_eq!(std::fs::read(dest.join("a.txt")).unwrap(), b"AAA");
        assert_eq!(std::fs::read(dest.join("sub/b.txt")).unwrap(), b"BBB");
    }

    #[test]
    fn write_level_refuses_a_7z_entry_that_escapes_the_destination() {
        // Same zip-slip guard, proven against the 7z path: `write_7z_level` must reject exactly
        // what `write_zip_level` rejects, via the shared `entry_name_escapes`.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("evil.7z");
        write_7z(&path, &[("../escaped.txt", b"NOPE")]);
        let dest = tmp.path().join("evil");

        let err = write_level(&path, &dest, &test_limits()).unwrap_err();
        assert!(format!("{err}").contains("escapes"), "got: {err}");
        assert!(
            !tmp.path().join("escaped.txt").exists(),
            "zip-slip must write nothing"
        );
    }

    #[test]
    fn write_level_honours_the_entry_size_cap_for_7z() {
        // `entry_max_bytes` is the SOLE guard for 7z (no ratio_cap pre-filter, see
        // `ArchiveLimits::ratio_cap`'s doc comment), so it must be proven against the real
        // `sevenz_rust2` streaming decoder, not inferred from the zip test of the same shape.
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("big.7z");
        write_7z(&path, &[("big.bin", &vec![7u8; 4096])]);
        let dest = tmp.path().join("big");
        let mut limits = test_limits();
        limits.entry_max_bytes = Some(1024);

        let err = write_level(&path, &dest, &limits).unwrap_err();
        assert!(
            format!("{err}").contains("1024"),
            "must report the cap: {err}"
        );
    }

    #[test]
    fn destination_is_a_sibling_folder_named_after_the_stem() {
        assert_eq!(destination_dir("docs/bundle.zip"), "docs/bundle");
        assert_eq!(destination_dir("bundle.zip"), "bundle");
        // A dotted name keeps everything before the LAST dot.
        assert_eq!(destination_dir("a/my.backup.zip"), "a/my.backup");
    }

    #[test]
    fn a_top_level_entry_lands_directly_in_the_destination() {
        assert_eq!(
            final_relative_path("docs/bundle.zip", "notes/readme.txt"),
            "docs/bundle/notes/readme.txt"
        );
        assert_eq!(
            first_hop("docs/bundle.zip", "notes/readme.txt"),
            ("docs/bundle/notes/readme.txt".to_string(), None)
        );
    }

    #[test]
    fn a_nested_entry_lands_beside_its_archive_once_fully_extracted() {
        // Fully recursive: inner.zip becomes the folder `inner`, beside the inner.zip file.
        assert_eq!(
            final_relative_path("bundle.zip", "sub/inner.zip › deep/x.txt"),
            "bundle/sub/inner/deep/x.txt"
        );
        // One level only: the inner archive is written as a file, the rest of the chain survives.
        assert_eq!(
            first_hop("bundle.zip", "sub/inner.zip › deep/x.txt"),
            (
                "bundle/sub/inner.zip".to_string(),
                Some("deep/x.txt".to_string())
            )
        );
    }

    #[test]
    fn three_levels_collapse_one_hop_at_a_time() {
        assert_eq!(
            final_relative_path("b.zip", "i.zip › j.zip › x.txt"),
            "b/i/j/x.txt"
        );
        assert_eq!(
            first_hop("b.zip", "i.zip › j.zip › x.txt"),
            ("b/i.zip".to_string(), Some("j.zip › x.txt".to_string()))
        );
    }

    #[test]
    fn budget_is_measured_from_the_real_mount_root() {
        let short = std::path::Path::new("E:\\");
        let long = std::path::Path::new("C:\\mnt\\archive\\external\\drive-four");
        let chain = "x".repeat(200);
        // 3 + "bundle/" (7) + 200 = 210 under E:\, but 236+ under the nested mount.
        assert!(fits_budget(short, "bundle.zip", &chain));
        assert!(!fits_budget(
            long,
            "bundle.zip",
            &format!("{chain}{}", "y".repeat(40))
        ));
    }

    #[test]
    fn an_astral_character_counts_as_two_utf16_units_not_one_char() {
        // U+1F600 GRINNING FACE: one `char`, but a UTF-16 surrogate pair (2 code units) -- and
        // Windows' MAX_PATH is measured in UTF-16 code units, so undercounting it as 1 is the
        // direction that produces a path Explorer cannot open.
        let root = std::path::Path::new("C:\\mnt");
        let with_emoji = full_length(root, "bundle.zip", "\u{1F600}.txt");
        let with_ascii = full_length(root, "bundle.zip", "a.txt");
        assert_eq!(
            with_emoji,
            with_ascii + 1,
            "the emoji costs 2 units, 'a' costs 1"
        );
    }

    /// A catalogue for a temp "drive" (volume marker already written into `tmp`) with `bundle.zip`
    /// catalogued as an archive holding `entries`. The temp dir is passed in rather than created so
    /// callers can also populate the destination folder inside the same directory.
    fn catalog_with_entries(tmp: &tempfile::TempDir, entries: &[(&str, i64, &str)]) -> Catalog {
        std::fs::write(tmp.path().join(crate::volume::MARKER), "vol-1").unwrap();
        let cat = Catalog::open(&tmp.path().join("c.db")).unwrap();
        cat.upsert_volume(&crate::catalog::models::Volume {
            volume_id: "vol-1".into(),
            label: "T".into(),
            identified_by: "marker".into(),
            first_seen_at: 1,
            last_seen_at: 1,
        })
        .unwrap();
        for (chain, size, hash) in entries {
            cat.upsert_archive_entry(
                "vol-1",
                "bundle.zip",
                &crate::archive::ArchiveEntry {
                    container_chain: (*chain).to_string(),
                    filename: chain.rsplit('/').next().unwrap().to_string(),
                    extension: "txt".into(),
                    size_bytes: *size,
                    content_hash: (*hash).to_string(),
                },
                None,
                1,
            )
            .unwrap();
        }
        cat
    }

    #[test]
    fn an_archive_whose_entries_all_fit_is_in_scope() {
        let tmp = tempfile::tempdir().unwrap();
        let cat = catalog_with_entries(&tmp, &[("small.txt", 4, "h1")]);
        match scope_check(&cat, tmp.path(), "vol-1", "bundle.zip").unwrap() {
            Scope::InScope {
                entries,
                uncompressed_bytes,
            } => {
                assert_eq!(entries, 1);
                assert_eq!(uncompressed_bytes, 4);
            }
            Scope::Refused(r) => panic!("expected in scope, got {r}"),
        }
    }

    #[test]
    fn one_over_long_entry_refuses_the_whole_archive_and_names_it() {
        let long = format!("{}/x.txt", "d".repeat(300));
        let tmp = tempfile::tempdir().unwrap();
        let cat = catalog_with_entries(&tmp, &[(long.as_str(), 4, "h1")]);
        match scope_check(&cat, tmp.path(), "vol-1", "bundle.zip").unwrap() {
            Scope::Refused(r) => {
                assert!(r.contains("260"), "refusal must state the limit: {r}");
                assert!(r.contains("x.txt"), "refusal must name the entry: {r}");
            }
            Scope::InScope { .. } => panic!("300-character entry must be refused"),
        }
    }

    #[test]
    fn an_existing_destination_folder_refuses_rather_than_merging() {
        let tmp = tempfile::tempdir().unwrap();
        let cat = catalog_with_entries(&tmp, &[("a.txt", 4, "h1")]);
        std::fs::create_dir_all(tmp.path().join("bundle")).unwrap();
        match scope_check(&cat, tmp.path(), "vol-1", "bundle.zip").unwrap() {
            Scope::Refused(r) => assert!(r.contains("bundle"), "refusal must name the folder: {r}"),
            Scope::InScope { .. } => panic!("must never merge into an existing folder"),
        }
    }

    #[test]
    fn an_archive_with_no_catalogued_entries_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let cat = catalog_with_entries(&tmp, &[]);
        match scope_check(&cat, tmp.path(), "vol-1", "bundle.zip").unwrap() {
            Scope::Refused(r) => assert!(r.contains("no catalogued entries"), "{r}"),
            Scope::InScope { .. } => panic!("nothing to verify against means nothing to extract"),
        }
    }

    /// Build a catalogue whose `bundle.zip` holds `files`, and a destination folder already
    /// containing them -- i.e. the state immediately after `write_level` succeeded.
    fn extracted_fixture(
        tmp: &tempfile::TempDir,
        files: &[(&str, &[u8])],
    ) -> (Catalog, std::path::PathBuf) {
        let entries: Vec<(String, i64, String)> = files
            .iter()
            .map(|(n, b)| {
                (
                    (*n).to_string(),
                    b.len() as i64,
                    blake3::hash(b).to_hex().to_string(),
                )
            })
            .collect();
        let refs: Vec<(&str, i64, &str)> = entries
            .iter()
            .map(|(n, s, h)| (n.as_str(), *s, h.as_str()))
            .collect();
        let cat = catalog_with_entries(tmp, &refs);
        let dest = tmp.path().join("bundle");
        for (name, bytes) in files {
            let p = dest.join(name);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, bytes).unwrap();
        }
        (cat, dest)
    }

    /// `bundle.zip` (already extracted one level) holds `inner.zip` on disk, and `inner.zip` holds
    /// `deep.txt` -- but the catalogue has no row for `inner.zip` itself, only the flattened chain
    /// `inner.zip › deep.txt`, exactly as the scanner would have recorded it.
    fn nested_extracted_fixture(tmp: &tempfile::TempDir) -> (Catalog, std::path::PathBuf) {
        let deep = b"DEEP CONTENT";
        let hash = blake3::hash(deep).to_hex().to_string();
        let chain = format!("inner.zip{CHAIN_SEP}deep.txt");
        let cat = catalog_with_entries(tmp, &[(chain.as_str(), deep.len() as i64, hash.as_str())]);

        let dest = tmp.path().join("bundle");
        std::fs::create_dir_all(&dest).unwrap();
        write_zip(&dest.join("inner.zip"), &[("deep.txt", deep)]);
        (cat, dest)
    }

    #[test]
    fn verification_passes_when_every_catalogued_hash_is_present() {
        let tmp = tempfile::tempdir().unwrap();
        let (cat, dest) = extracted_fixture(&tmp, &[("a.txt", b"AAA")]);
        verify_destination(&cat, "vol-1", "bundle.zip", &dest, &test_limits()).unwrap();
    }

    #[test]
    fn verification_fails_loudly_on_a_content_mismatch() {
        let tmp = tempfile::tempdir().unwrap();
        let (cat, dest) = extracted_fixture(&tmp, &[("a.txt", b"AAA")]);
        std::fs::write(dest.join("a.txt"), b"TAMPERED").unwrap();

        let err =
            verify_destination(&cat, "vol-1", "bundle.zip", &dest, &test_limits()).unwrap_err();
        assert!(
            format!("{err}").contains("a.txt"),
            "must name the entry: {err}"
        );
        assert!(
            format!("{err}").contains("hash"),
            "must say what failed: {err}"
        );
    }

    #[test]
    fn verification_fails_when_a_catalogued_entry_is_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let (cat, dest) = extracted_fixture(&tmp, &[("a.txt", b"AAA")]);
        std::fs::remove_file(dest.join("a.txt")).unwrap();

        let err =
            verify_destination(&cat, "vol-1", "bundle.zip", &dest, &test_limits()).unwrap_err();
        assert!(format!("{err}").contains("missing"), "{err}");
    }

    #[test]
    fn a_nested_archive_is_verified_through_its_contents() {
        // bundle.zip contains inner.zip contains deep.txt. After one level, only inner.zip is on
        // disk -- and that is enough to prove the catalogued chain "inner.zip › deep.txt".
        let tmp = tempfile::tempdir().unwrap();
        let (cat, dest) = nested_extracted_fixture(&tmp);
        verify_destination(&cat, "vol-1", "bundle.zip", &dest, &test_limits()).unwrap();
    }

    #[test]
    fn a_max_depth_truncated_nested_archive_verifies_by_its_own_hash() {
        // When the scanner stops descending at `max_depth`, it catalogues the nested archive
        // itself as a LEAF entry: chain = "inner.zip" (not "inner.zip › deep.txt"), hash =
        // blake3 of inner.zip's own bytes. verify_destination must prove that row by hashing
        // the archive file directly, not by descending into it -- this is the double-recording
        // branch (`found.insert(rel, hash_file(...))` alongside the chain-based inserts) that
        // makes an intentionally-truncated archive extractable at all.
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("bundle");
        std::fs::create_dir_all(&dest).unwrap();
        let inner_path = dest.join("inner.zip");
        write_zip(&inner_path, &[("deep.txt", b"DEEP CONTENT")]);
        let inner_hash = crate::hashing::hash_file(&inner_path).unwrap();
        let inner_size = std::fs::metadata(&inner_path).unwrap().len() as i64;

        let cat = catalog_with_entries(&tmp, &[("inner.zip", inner_size, inner_hash.as_str())]);

        verify_destination(&cat, "vol-1", "bundle.zip", &dest, &test_limits()).unwrap();
    }
}
