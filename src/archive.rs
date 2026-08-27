//! Reading into zip archives (recursively) to catalog their contents.

use std::io::{Read, Seek};

use crate::config::Config;
use crate::hashing;

/// Tunable limits for archive descent, grouped by what each one actually protects.
#[derive(Debug, Clone)]
pub struct ArchiveLimits {
    /// Recursion bound.
    pub max_depth: usize,
    /// MEMORY: the most one nested archive may hold in RAM. Nested archives must be buffered so
    /// they can be both hashed and re-opened with `Seek` to recurse, so this is a real bound.
    pub buffer_max_bytes: u64,
    /// MEMORY: bytes of nested-archive buffer live at once across a whole descent.
    pub total_buffer_bytes: u64,
    /// CATALOGUE: the largest leaf file we will record. `None` is unlimited, and safe: leaves are
    /// stream-hashed in 64 KiB chunks, so their size costs no memory.
    pub entry_max_bytes: Option<u64>,
    /// TIME: declared uncompressed/compressed. With a generous leaf ceiling this is what stops a
    /// genuine bomb streaming for a long time before its size cap trips.
    pub ratio_cap: u64,
    /// Zip-format extensions always treated as a leaf, never descended -- checked first and always
    /// wins, including over `zip` itself.
    pub deny_extensions: Vec<String>,
    /// Zip-format extensions (other than `zip` itself) approved for descent.
    pub allow_extensions: Vec<String>,
}

impl ArchiveLimits {
    pub fn from_config(cfg: &Config) -> ArchiveLimits {
        ArchiveLimits {
            max_depth: cfg.max_archive_depth,
            buffer_max_bytes: cfg.archive_buffer_max_bytes,
            total_buffer_bytes: cfg.archive_total_buffer_bytes,
            entry_max_bytes: cfg.archive_entry_max_bytes,
            ratio_cap: cfg.archive_ratio_cap,
            deny_extensions: cfg.archive_deny_extensions.clone(),
            allow_extensions: cfg.archive_allow_extensions.clone(),
        }
    }

    /// One line for the CLI, printed before a scan starts: these values decide what will and will
    /// not be catalogued, and a multi-day scan is a bad time to discover them.
    pub fn summary_line(&self) -> String {
        let entry = match self.entry_max_bytes {
            Some(b) => human_bytes(b),
            None => "unlimited".to_string(),
        };
        format!(
            "Archive limits: ratio cap {}, largest entry {}, nested buffer {} (total {}), depth {}",
            self.ratio_cap,
            entry,
            human_bytes(self.buffer_max_bytes),
            human_bytes(self.total_buffer_bytes),
            self.max_depth
        )
    }
}

/// Format a byte count with whatever unit keeps it legible: F6 -- formatting everything as
/// `{:.0} GB` made a 500 MB ceiling and a 0-byte ceiling both print `0 GB`, on the one line the
/// user is meant to sanity-check before committing a multi-day scan.
fn human_bytes(b: u64) -> String {
    const KB: f64 = 1024.0;
    const MB: f64 = KB * 1024.0;
    const GB: f64 = MB * 1024.0;
    let bf = b as f64;
    if bf >= GB {
        format!("{:.1} GB", bf / GB)
    } else if bf >= MB {
        format!("{:.1} MB", bf / MB)
    } else if bf >= KB {
        format!("{:.1} KB", bf / KB)
    } else {
        format!("{b} bytes")
    }
}

/// What to do with a file that is already known to be zip format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Descent {
    /// Catalogue what is inside.
    Descend,
    /// A known container -- catalogue the file itself and leave it whole.
    Leaf,
    /// Zip format with an unfamiliar extension. Treated as a leaf, and reported so the user can
    /// decide, rather than guessed at: a five-day unattended scan cannot ask.
    Unrecognised,
}

/// The naming policy for a file already established to be zip or 7z format.
///
/// A renamed zip/7z and a `.docx` are indistinguishable by magic bytes alone, so the difference has
/// to be an explicit rule rather than something implied by a policy name.
///
/// The deny-list is checked FIRST and always wins -- including over `zip` and `7z` themselves.
/// Silently overriding an explicit choice would be worse than obeying one the user can see and undo.
///
/// `zip` and `7z` are both natively descended, same as each other: there is no `DEFAULT_ALLOW`
/// constant, and `archive_allow_extensions` defaults to empty, so making 7z opt-in via the allow
/// list would have shipped a new default into every existing user's `settings.json` semantics --
/// the allow list is user-editable state, and that would have been the more surprising change. A
/// user who wants `.7z` files left whole adds `7z` to the deny list instead.
///
/// `extension` is lowercase and dot-free ("" when the name has none). A dotted value never
/// matches either list and therefore reads as `Unrecognised` -- safe, but wrong, so callers
/// must strip the dot.
pub fn descent_for(extension: &str, deny: &[String], allow: &[String]) -> Descent {
    let ext = extension.to_ascii_lowercase();
    let has = |list: &[String]| list.iter().any(|e| e.eq_ignore_ascii_case(&ext));
    if has(deny) {
        return Descent::Leaf;
    }
    if ext == "zip" || ext == "7z" || has(allow) {
        return Descent::Descend;
    }
    Descent::Unrecognised
}

/// True if these leading bytes carry a zip signature.
///
/// By content, not by extension: `._Video.zip` is a macOS AppleDouble sidecar that merely borrows
/// the name, and a zip renamed to `.bak` is still a zip. The extension lies in both directions.
pub fn looks_like_zip(prefix: &[u8]) -> bool {
    matches!(
        prefix,
        [b'P', b'K', 0x03, 0x04, ..] | [b'P', b'K', 0x05, 0x06, ..] | [b'P', b'K', 0x07, 0x08, ..]
    )
}

/// The 7z format signature: `37 7A BC AF 27 1C`, always the first six bytes of a 7z file (unlike
/// zip, 7z has no prefixed/self-extracting variant this scanner needs to detect, so no tail check
/// is needed alongside this).
const SEVENZ_SIGNATURE: [u8; 6] = [0x37, 0x7A, 0xBC, 0xAF, 0x27, 0x1C];

/// True if these leading bytes carry a 7z signature. By content, not by extension -- the same
/// reasoning as `looks_like_zip`: a renamed `.7z` is still a 7z, and nothing today lies about being
/// one, but the check should not assume the filename is honest either.
pub fn looks_like_7z(prefix: &[u8]) -> bool {
    prefix.starts_with(&SEVENZ_SIGNATURE)
}

/// Read up to 6 leading bytes from a stream (6, not 4: the longer of the zip and 7z signatures),
/// reporting whether they look like a zip and/or a 7z. The bytes are returned so a caller reading a
/// non-seekable stream (an archive entry) can chain them back -- dropping them would silently
/// truncate the content hash. `pub(crate)` so the scanner's top-level detection shares this instead
/// of re-implementing the same peek loop.
pub(crate) fn peek6<R: Read + ?Sized>(r: &mut R) -> std::io::Result<(Vec<u8>, bool, bool)> {
    let mut buf = [0u8; 6];
    let mut filled = 0;
    while filled < 6 {
        match r.read(&mut buf[filled..])? {
            0 => break,
            n => filled += n,
        }
    }
    let head = buf[..filled].to_vec();
    let is_zip = looks_like_zip(&head);
    let is_7z = looks_like_7z(&head);
    Ok((head, is_zip, is_7z))
}

/// The largest an End Of Central Directory record plus its trailing comment can occupy: a fixed
/// 22-byte record plus up to 64 KiB of comment (the comment length field is 16 bits).
const EOCD_MAX_TAIL: u64 = 64 * 1024 + 22;

/// True if the last `min(EOCD_MAX_TAIL, file length)` bytes of a seekable stream contain the EOCD
/// signature `PK\x05\x06`.
///
/// A zip does not have to START with `PK`: a self-extracting stub, or any tool that prepends data,
/// leaves valid zip content whose central directory a real reader locates from the END of the
/// file, not the start. `looks_like_zip` alone therefore misses prefixed zips. This is the fallback
/// for exactly that case -- deliberately gated on the filename already claiming to be a zip (see
/// the scanner call site), so it is not paid for every ordinary file on the drive.
///
/// Seek-only: an archive entry being read mid-descent is not seekable, so nested entries keep using
/// the head-only `looks_like_zip` check in `scan_level` above and a prefixed zip nested inside
/// another archive is not detected. Only the top-level scanner (which holds a real `File`) uses this.
pub(crate) fn tail_has_eocd_signature<R: Read + std::io::Seek>(r: &mut R) -> std::io::Result<bool> {
    use std::io::SeekFrom;
    let len = r.seek(SeekFrom::End(0))?;
    let window = EOCD_MAX_TAIL.min(len);
    r.seek(SeekFrom::End(-(window as i64)))?;
    let mut buf = vec![0u8; window as usize];
    r.read_exact(&mut buf)?;
    Ok(buf.windows(4).any(|w| w == [b'P', b'K', 0x05, 0x06]))
}

/// One hashed leaf entry found while scanning an archive.
#[derive(Debug, Clone)]
pub struct ArchiveEntry {
    pub container_chain: String,
    pub filename: String,
    pub extension: String,
    pub size_bytes: i64,
    pub content_hash: String,
}

/// Result of scanning one archive level: hashed leaf entries plus any skipped/error notes.
#[derive(Debug, Default)]
pub struct ArchiveScanResult {
    pub entries: Vec<ArchiveEntry>,
    pub errors: Vec<(String, String)>,
}

/// Extension (lowercased, no dot) of an internal entry name, or "" if none.
fn entry_extension(name: &str) -> String {
    let leaf = name.rsplit('/').next().unwrap_or(name);
    match leaf.rsplit_once('.') {
        Some((_, ext)) if !ext.is_empty() => ext.to_ascii_lowercase(),
        _ => String::new(),
    }
}

/// Join a parent chain and a child name with the guillemet separator.
fn join_chain(prefix: &str, name: &str) -> String {
    if prefix.is_empty() {
        name.to_string()
    } else {
        format!("{prefix} › {name}")
    }
}

/// Read up to `cap` bytes; `Err` if the stream exceeds `cap` (bomb guard for buffering).
fn read_capped<R: Read>(mut reader: R, cap: u64) -> Result<Vec<u8>, String> {
    let mut buf = Vec::new();
    let mut limited = (&mut reader).take(cap + 1);
    limited
        .read_to_end(&mut buf)
        .map_err(|e| format!("read error: {e}"))?;
    if buf.len() as u64 > cap {
        return Err(format!("zip bomb: nested archive exceeds cap {cap}"));
    }
    Ok(buf)
}

/// Stream-hash a reader in 64 KiB chunks, enforcing an actual-byte cap.
/// Returns (lowercase-hex hash, bytes_read), or Err if the stream exceeds `cap`.
fn hash_capped<R: Read>(mut reader: R, cap: u64) -> Result<(String, u64), String> {
    let mut hasher = blake3::Hasher::new();
    let mut buf = [0u8; 64 * 1024];
    let mut total: u64 = 0;
    loop {
        let n = reader
            .read(&mut buf)
            .map_err(|e| format!("read error: {e}"))?;
        if n == 0 {
            break;
        }
        total += n as u64;
        if total > cap {
            return Err(format!("zip bomb: decompressed content exceeds cap {cap}"));
        }
        hasher.update(&buf[..n]);
    }
    Ok((hasher.finalize().to_hex().to_string(), total))
}

/// Scan an archive (recursively) from a seekable reader. Leaf files are stream-hashed; nested
/// archives are buffered (bounded by `limits.entry_max_bytes`) and descended into up to
/// `limits.max_depth` levels. Entries exceeding the zip-bomb caps are skipped with an error note.
///
/// Format is decided by content, not by the caller: the reader may hold a zip or a 7z, and
/// `scan_level` sniffs the signature itself, exactly as the entry-level nested-archive check below
/// does for entries found while already inside one. This is what lets a 7z nested inside a zip (or
/// vice versa) recurse correctly with no special-casing at the call site.
pub fn scan_archive<R: Read + Seek>(reader: R, limits: &ArchiveLimits) -> ArchiveScanResult {
    let mut result = ArchiveScanResult::default();
    let mut budget = limits.total_buffer_bytes;
    scan_level(reader, "", 1, limits, &mut budget, &mut result);
    result
}

/// Scan one archive level, dispatching on the container's own content signature. `chain_prefix` is
/// the container chain of THIS archive ("" at top level); `depth` is 1 for a top-level archive.
/// `budget` is the bytes still available for buffering nested archives; it is shared by every level
/// of one descent, so ancestors' live buffers count against their descendants.
fn scan_level<R: Read + Seek>(
    mut reader: R,
    chain_prefix: &str,
    depth: usize,
    limits: &ArchiveLimits,
    budget: &mut u64,
    result: &mut ArchiveScanResult,
) {
    let mut head = [0u8; 6];
    let mut filled = 0;
    while filled < 6 {
        match reader.read(&mut head[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(e) => {
                result
                    .errors
                    .push((chain_prefix.to_string(), format!("read error: {e}")));
                return;
            }
        }
    }
    if let Err(e) = reader.seek(std::io::SeekFrom::Start(0)) {
        result
            .errors
            .push((chain_prefix.to_string(), format!("seek error: {e}")));
        return;
    }
    if looks_like_7z(&head[..filled]) {
        scan_7z_level(reader, chain_prefix, depth, limits, budget, result);
    } else {
        scan_zip_level(reader, chain_prefix, depth, limits, budget, result);
    }
}

/// One archive entry already known to be a file, with its leading bytes already peeked (`head`).
/// If those bytes say it is a nested archive, buffer it (bounded by the shared `budget`) and
/// recurse through `scan_level`; the recursion re-sniffs the buffer, so the nested archive's own
/// format need not be known here. Otherwise stream-hash it as a leaf. Shared by the zip and 7z
/// entry loops so both formats recurse identically and pay into the same buffer budget.
#[allow(
    clippy::too_many_arguments,
    reason = "each parameter is an independent scan input; grouping them into a struct would add \
        indirection without reducing real complexity (same rationale as scan_volume_with_progress)"
)]
fn handle_entry<R: Read>(
    chain: String,
    filename: String,
    extension: String,
    head: Vec<u8>,
    is_nested_archive: bool,
    entry: R,
    depth: usize,
    limits: &ArchiveLimits,
    budget: &mut u64,
    result: &mut ArchiveScanResult,
) {
    // Chain the peeked bytes back in front, so both branches below see the entire entry.
    let mut entry = std::io::Cursor::new(head).chain(entry);

    if is_nested_archive {
        // Nested archive: buffer it (bounded) so we can BOTH hash it and re-open it with Seek
        // to recurse. Only archives are buffered — large leaf files stream (see else branch).
        // Cap this buffer by whatever the whole descent has left, not just the per-entry limit.
        let cap = limits.buffer_max_bytes.min(*budget);
        if cap == 0 {
            result.errors.push((
                chain,
                format!(
                    "nested-archive buffer budget exhausted ({} bytes total)",
                    limits.total_buffer_bytes
                ),
            ));
            return;
        }
        let bytes = match read_capped(&mut entry, cap) {
            Ok(b) => b,
            Err(reason) => {
                // Budget pressure from legitimate ancestors is not a bomb; saying so would
                // send the user hunting for a hostile file that does not exist.
                let reason = if cap < limits.buffer_max_bytes {
                    format!(
                        "nested archive skipped: only {cap} of the {} byte buffer budget \
                         remained (ancestor archives hold the rest)",
                        limits.total_buffer_bytes
                    )
                } else {
                    reason
                };
                result.errors.push((chain, reason));
                return;
            }
        };
        let mut slice: &[u8] = &bytes;
        let content_hash = match hashing::hash_reader(&mut slice) {
            Ok(h) => h,
            Err(e) => {
                result.errors.push((chain, format!("read error: {e}")));
                return;
            }
        };
        result.entries.push(ArchiveEntry {
            container_chain: chain.clone(),
            filename,
            extension,
            size_bytes: bytes.len() as i64,
            content_hash,
        });
        if depth >= limits.max_depth {
            result.errors.push((
                chain,
                format!("max archive depth exceeded ({} levels)", limits.max_depth),
            ));
            return;
        }
        // This buffer stays alive for the whole nested scan, so charge it to the shared budget
        // for exactly that long and release it once the recursion (and the Vec) is done.
        let held = bytes.len() as u64;
        *budget -= held;
        scan_level(
            std::io::Cursor::new(bytes),
            &chain,
            depth + 1,
            limits,
            budget,
            result,
        );
        *budget += held;
    } else {
        // Leaf file: stream-hash with an actual-byte cap (declared size may lie); record the TRUE length.
        // `u64::MAX` when unlimited: `hash_capped` still counts real bytes, so a lying header
        // cannot escape -- there is simply no ceiling to trip.
        let cap = limits.entry_max_bytes.unwrap_or(u64::MAX);
        match hash_capped(&mut entry, cap) {
            Ok((content_hash, actual)) => {
                result.entries.push(ArchiveEntry {
                    container_chain: chain,
                    filename,
                    extension,
                    size_bytes: actual as i64,
                    content_hash,
                });
            }
            Err(reason) => {
                result.errors.push((chain, reason));
            }
        }
    }
}

/// Scan one zip-format level: `reader` is already known (by `scan_level`) to be a zip.
fn scan_zip_level<R: Read + Seek>(
    reader: R,
    chain_prefix: &str,
    depth: usize,
    limits: &ArchiveLimits,
    budget: &mut u64,
    result: &mut ArchiveScanResult,
) {
    let mut archive = match zip::ZipArchive::new(reader) {
        Ok(a) => a,
        Err(e) => {
            result
                .errors
                .push((chain_prefix.to_string(), format!("unreadable archive: {e}")));
            return;
        }
    };

    for i in 0..archive.len() {
        let mut entry = match archive.by_index(i) {
            Ok(e) => e,
            Err(e) => {
                result.errors.push((
                    join_chain(chain_prefix, &format!("#{i}")),
                    format!("unreadable archive entry: {e}"),
                ));
                continue;
            }
        };
        if !entry.is_file() {
            continue;
        }
        let name = entry.name().to_string();
        let chain = join_chain(chain_prefix, &name);
        let uncompressed = entry.size();
        let compressed = entry.compressed_size().max(1);

        // Ratio is checked for both branches: it is the cheap pre-filter that stops us buffering
        // or streaming something absurd. Declared sizes can lie, which is why `read_capped` and
        // `hash_capped` re-check the real byte counts downstream.
        if uncompressed / compressed > limits.ratio_cap {
            result.errors.push((
                chain,
                format!(
                    "zip bomb: ratio {} exceeds cap {}",
                    uncompressed / compressed,
                    limits.ratio_cap
                ),
            ));
            continue;
        }

        let filename = name.rsplit('/').next().unwrap_or(&name).to_string();
        let extension = entry_extension(&name);

        let (head, is_zip, is_7z) = match peek6(&mut entry) {
            Ok(v) => v,
            Err(e) => {
                result.errors.push((chain, format!("read error: {e}")));
                continue;
            }
        };
        handle_entry(
            chain,
            filename,
            extension,
            head,
            is_zip || is_7z,
            entry,
            depth,
            limits,
            budget,
            result,
        );
    }
}

/// Scan one 7z-format level: `reader` is already known (by `scan_level`) to be a 7z.
///
/// `sevenz_rust2::SevenZReader::for_each_entries` hands each entry to a closure as a live,
/// streaming `&mut dyn Read` (decoded lazily, chunk by chunk, as the closure reads it) -- exactly
/// what `handle_entry`'s leaf path needs to stream-hash in bounded memory, and what its
/// nested-archive path needs to buffer under the shared budget.
fn scan_7z_level<R: Read + Seek>(
    reader: R,
    chain_prefix: &str,
    depth: usize,
    limits: &ArchiveLimits,
    budget: &mut u64,
    result: &mut ArchiveScanResult,
) {
    let mut archive = match sevenz_rust2::SevenZReader::new(reader, sevenz_rust2::Password::empty())
    {
        Ok(a) => a,
        Err(e) => {
            result.errors.push((
                chain_prefix.to_string(),
                format!("unreadable 7z archive: {e}"),
            ));
            return;
        }
    };

    let outcome = archive.for_each_entries(|entry, r| {
        if entry.is_directory {
            return Ok(true);
        }
        let name = entry.name.clone();
        let chain = join_chain(chain_prefix, &name);
        let filename = name.rsplit('/').next().unwrap_or(&name).to_string();
        let extension = entry_extension(&name);

        // Unlike the zip path above, there is no ratio pre-filter here. 7z declares a real
        // per-entry uncompressed size, but NOT a reliable per-entry compressed size: solid
        // compression packs many files into one shared folder, and this crate only fills in
        // `compressed_size` for the first file of each folder (0 for the rest). A ratio computed
        // from that would be wrong for nearly every entry in an ordinary multi-file .7z. So the
        // real guard here is `entry_max_bytes`, enforced on the actual decompressed byte count as
        // it streams through `handle_entry`'s leaf path below -- the same protection `hash_capped`
        // already gives every format against a header that lies about its size.
        let (head, is_zip, is_7z) = match peek6(r) {
            Ok(v) => v,
            Err(e) => {
                result.errors.push((chain, format!("read error: {e}")));
                return Ok(true);
            }
        };
        handle_entry(
            chain,
            filename,
            extension,
            head,
            is_zip || is_7z,
            r,
            depth,
            limits,
            budget,
            result,
        );
        Ok(true)
    });
    if let Err(e) = outcome {
        result.errors.push((
            chain_prefix.to_string(),
            format!("7z archive read error: {e}"),
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Cursor, Write};

    #[test]
    fn the_descent_rule_separates_archives_from_document_containers() {
        let deny: Vec<String> = ["docx", "jar", "epub"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let allow: Vec<String> = vec![];
        assert_eq!(descent_for("zip", &deny, &allow), Descent::Descend);
        assert_eq!(
            descent_for("7z", &deny, &allow),
            Descent::Descend,
            "7z is natively descended, same as zip -- no allow-list entry required"
        );
        assert_eq!(
            descent_for("docx", &deny, &allow),
            Descent::Leaf,
            "a document container"
        );
        assert_eq!(descent_for("jar", &deny, &allow), Descent::Leaf);
        // The renamed-zip case from #42: unfamiliar, so reported rather than guessed at.
        assert_eq!(descent_for("bak", &deny, &allow), Descent::Unrecognised);
        assert_eq!(
            descent_for("", &deny, &allow),
            Descent::Unrecognised,
            "no extension"
        );
    }

    #[test]
    fn an_approved_extension_is_descended_into() {
        let deny: Vec<String> = vec!["docx".into()];
        let allow: Vec<String> = vec!["bak".into()];
        assert_eq!(descent_for("bak", &deny, &allow), Descent::Descend);
    }

    #[test]
    fn the_deny_list_wins_over_everything() {
        // Deliberate: if a user denies `zip`, or denies something they also allowed, the visible
        // choice must win. Silently overriding them would be worse than obeying an undoable choice.
        let deny: Vec<String> = vec!["zip".into(), "bak".into()];
        let allow: Vec<String> = vec!["bak".into()];
        assert_eq!(descent_for("zip", &deny, &allow), Descent::Leaf);
        assert_eq!(descent_for("bak", &deny, &allow), Descent::Leaf);
    }

    #[test]
    fn a_user_who_denies_7z_is_obeyed_even_though_it_is_native() {
        // 7z is descended by default same as zip, with no allow-list entry needed -- but that
        // default must still be a user-editable choice, not baked in unconditionally.
        let deny: Vec<String> = vec!["7z".into()];
        assert_eq!(descent_for("7z", &deny, &[]), Descent::Leaf);
    }

    #[test]
    fn extension_matching_ignores_case() {
        let deny: Vec<String> = vec!["docx".into()];
        assert_eq!(descent_for("DOCX", &deny, &[]), Descent::Leaf);
        assert_eq!(descent_for("ZIP", &[], &[]), Descent::Descend);
    }

    /// `Config::default_paths()` reads the ambient environment. This scopes
    /// `CLEANUPSTORAGES_DATA_DIR` to a throwaway tempdir for the test's duration and restores
    /// whatever was there before on drop (even on panic), using the same mutex `config.rs` uses so
    /// the two never race on the process-global env var. The #41/#42 review found this exact test
    /// shape reading the user's real settings file without this guard.
    struct ScopedDataDir {
        _lock: std::sync::MutexGuard<'static, ()>,
        _dir: tempfile::TempDir,
        prev: Option<String>,
    }
    impl ScopedDataDir {
        fn new() -> Self {
            let lock = crate::config::ENV_GUARD
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let dir = tempfile::tempdir().unwrap();
            let prev = std::env::var("CLEANUPSTORAGES_DATA_DIR").ok();
            std::env::set_var("CLEANUPSTORAGES_DATA_DIR", dir.path());
            ScopedDataDir {
                _lock: lock,
                _dir: dir,
                prev,
            }
        }
    }
    impl Drop for ScopedDataDir {
        fn drop(&mut self) {
            match &self.prev {
                Some(v) => std::env::set_var("CLEANUPSTORAGES_DATA_DIR", v),
                None => std::env::remove_var("CLEANUPSTORAGES_DATA_DIR"),
            }
        }
    }

    #[test]
    fn the_default_deny_list_covers_the_formats_that_prompted_this() {
        let _scope = ScopedDataDir::new();
        let cfg = Config::default_paths().unwrap();
        for e in ["docx", "xlsx", "pptx", "jar", "apk", "epub", "odt"] {
            assert!(
                cfg.archive_deny_extensions.iter().any(|d| d == e),
                "{e} must be denied by default"
            );
        }
        assert!(
            cfg.archive_allow_extensions.is_empty(),
            "nothing is approved until the user says so"
        );
    }

    #[test]
    fn limits_from_config() {
        // F2: a `Config` literal, not `Config::default_paths()` -- that call reads whatever
        // `settings.json` exists in the AMBIENT data directory (no ENV_GUARD, no scoped data dir),
        // so this test would fail the moment the user saves any limit from the UI, and it would
        // `create_dir_all` the real app-data directory as a side effect.
        let cfg = Config {
            catalog_path: std::path::PathBuf::from("unused/catalog.db"),
            snapshot_retention: 10,
            max_archive_depth: 8,
            archive_buffer_max_bytes: 2 * 1024 * 1024 * 1024,
            archive_total_buffer_bytes: 2 * 1024 * 1024 * 1024,
            archive_entry_max_bytes: Some(64 * 1024 * 1024 * 1024),
            archive_ratio_cap: 10_000,
            archive_deny_extensions: Vec::new(),
            archive_allow_extensions: Vec::new(),
        };
        let l = ArchiveLimits::from_config(&cfg);
        assert_eq!(l.max_depth, 8);
        assert_eq!(l.buffer_max_bytes, 2 * 1024 * 1024 * 1024);
        assert_eq!(l.entry_max_bytes, Some(64 * 1024 * 1024 * 1024));
        assert_eq!(l.ratio_cap, 10_000);
        assert_eq!(l.total_buffer_bytes, 2 * 1024 * 1024 * 1024);
    }

    // Build an in-memory zip: Vec of (name, bytes).
    fn make_zip(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut buf = Cursor::new(Vec::new());
        {
            let mut zw = zip::ZipWriter::new(&mut buf);
            let opts: zip::write::FileOptions<()> = zip::write::FileOptions::default()
                .compression_method(zip::CompressionMethod::Stored);
            for (name, bytes) in files {
                zw.start_file(*name, opts).unwrap();
                zw.write_all(bytes).unwrap();
            }
            zw.finish().unwrap();
        }
        buf.into_inner()
    }

    // Deflated, so entries have a real compression ratio. `make_zip` stores uncompressed, which
    // pins every ratio at 1 and makes ratio-cap tests silently unable to fail.
    fn make_zip_deflated(files: &[(&str, &[u8])]) -> Vec<u8> {
        let mut buf = Cursor::new(Vec::new());
        {
            let mut zw = zip::ZipWriter::new(&mut buf);
            let opts: zip::write::FileOptions<()> = zip::write::FileOptions::default()
                .compression_method(zip::CompressionMethod::Deflated);
            for (name, bytes) in files {
                zw.start_file(*name, opts).unwrap();
                zw.write_all(bytes).unwrap();
            }
            zw.finish().unwrap();
        }
        buf.into_inner()
    }

    #[test]
    fn zip_signatures_are_recognised_by_content_not_name() {
        assert!(looks_like_zip(b"PK\x03\x04rest"));
        assert!(looks_like_zip(b"PK\x05\x06")); // empty archive
        assert!(looks_like_zip(b"PK\x07\x08")); // spanned
        assert!(!looks_like_zip(b"\x00\x05\x16\x07")); // AppleDouble magic
        assert!(!looks_like_zip(b"PK")); // too short to be a signature
        assert!(!looks_like_zip(b""));
    }

    #[test]
    fn an_applesdouble_sidecar_named_zip_is_treated_as_a_leaf() {
        // ._Video.zip is macOS metadata about Video.zip. Probing it as an archive is what produced
        // "invalid Zip archive: Could not find EOCD" against a file that was never a zip.
        let sidecar = b"\x00\x05\x16\x07\x00\x02\x00\x00Mac OS X        ";
        let zip = make_zip(&[("._Video.zip", sidecar)]);
        let res = scan_archive(Cursor::new(zip), &limits());
        assert_eq!(
            res.errors,
            Vec::<(String, String)>::new(),
            "it is not an archive, so no archive error"
        );
        assert_eq!(res.entries.len(), 1, "it is catalogued as an ordinary file");
        assert_eq!(res.entries[0].filename, "._Video.zip");
        assert_eq!(res.entries[0].size_bytes, sidecar.len() as i64);
    }

    #[test]
    fn a_zip_renamed_to_another_extension_is_still_descended_into() {
        // Missed entirely today: the extension check says no, so its contents were never catalogued.
        let inner = make_zip(&[("inner.txt", b"hello")]);
        let outer = make_zip(&[("backup.bak", &inner)]);
        let res = scan_archive(Cursor::new(outer), &limits());
        let names: Vec<&str> = res.entries.iter().map(|e| e.filename.as_str()).collect();
        assert!(
            names.contains(&"inner.txt"),
            "expected to descend into the renamed zip, got {names:?}"
        );
    }

    #[test]
    fn peeking_does_not_change_an_entrys_hash() {
        // The peeked bytes must be chained back, or every entry that survives detection hashes
        // four bytes short -- silently wrong content hashes, which is unrecoverable at dedup time.
        let body = b"the quick brown fox jumps over the lazy dog";
        let zip = make_zip(&[("plain.txt", body)]);
        let res = scan_archive(Cursor::new(zip), &limits());
        let expected = {
            let mut r: &[u8] = body;
            hashing::hash_reader(&mut r).unwrap()
        };
        assert_eq!(res.entries.len(), 1);
        assert_eq!(
            res.entries[0].content_hash, expected,
            "hash must cover the whole entry"
        );
        assert_eq!(res.entries[0].size_bytes, body.len() as i64);
    }

    #[test]
    fn a_highly_compressible_file_is_catalogued_not_rejected() {
        // 400 KB of zeros deflates to a few hundred bytes -- a ratio in the high hundreds, which
        // is what a Vivado bitstream or an MRI export actually looks like. Under the old cap of
        // 200 every one of these was silently dropped from the catalogue.
        let zip = make_zip_deflated(&[("bitstream.bit", &vec![0u8; 400 * 1024])]);
        let res = scan_archive(Cursor::new(zip), &limits());
        assert_eq!(
            res.errors,
            Vec::<(String, String)>::new(),
            "no entry should be refused"
        );
        assert_eq!(res.entries.len(), 1);
        assert_eq!(res.entries[0].filename, "bitstream.bit");
        assert_eq!(res.entries[0].size_bytes, 400 * 1024);
    }

    #[test]
    fn an_absurd_ratio_is_still_refused() {
        // The cap still has a job: with a generous leaf ceiling it is what stops a real bomb
        // streaming for a long time. A tiny cap proves the check is reachable at all.
        let zip = make_zip_deflated(&[("bomb.bin", &vec![0u8; 400 * 1024])]);
        let tight = ArchiveLimits {
            ratio_cap: 2,
            ..limits()
        };
        let res = scan_archive(Cursor::new(zip), &tight);
        assert!(res.entries.is_empty(), "the entry must not be catalogued");
        assert_eq!(res.errors.len(), 1);
        assert!(
            res.errors[0].1.contains("ratio"),
            "got {:?}",
            res.errors[0].1
        );
    }

    #[test]
    fn a_leaf_file_larger_than_the_buffer_bound_is_still_catalogued() {
        // The leaf path streams in constant memory, so the nested-archive buffer bound must not
        // apply to it. This is the 34 GB rejection, in miniature.
        let zip = make_zip(&[("big.mov", &vec![7u8; 64 * 1024])]);
        let small_buffer = ArchiveLimits {
            buffer_max_bytes: 1024, // far smaller than the entry
            total_buffer_bytes: 1024,
            entry_max_bytes: None, // unlimited leaf ceiling
            ..limits()
        };
        let res = scan_archive(Cursor::new(zip), &small_buffer);
        assert_eq!(res.errors, Vec::<(String, String)>::new());
        assert_eq!(res.entries.len(), 1);
        assert_eq!(res.entries[0].size_bytes, 64 * 1024);
    }

    #[test]
    fn a_leaf_ceiling_when_set_is_enforced() {
        let zip = make_zip(&[("big.mov", &vec![7u8; 64 * 1024])]);
        let capped = ArchiveLimits {
            entry_max_bytes: Some(1024),
            ..limits()
        };
        let res = scan_archive(Cursor::new(zip), &capped);
        assert!(res.entries.is_empty());
        assert_eq!(res.errors.len(), 1);
    }

    fn limits() -> ArchiveLimits {
        ArchiveLimits {
            max_depth: 8,
            buffer_max_bytes: 2 * 1024 * 1024 * 1024,
            entry_max_bytes: Some(64 * 1024 * 1024 * 1024),
            ratio_cap: 10_000,
            total_buffer_bytes: 2 * 1024 * 1024 * 1024,
            deny_extensions: Vec::new(),
            allow_extensions: Vec::new(),
        }
    }

    #[test]
    fn the_limits_summary_names_every_value_including_unlimited() {
        let l = limits();
        let s = l.summary_line();
        assert!(s.contains("10000"), "the ratio cap must be visible: {s}");
        assert!(s.contains("depth 8"), "got {s}");

        let unlimited = ArchiveLimits {
            entry_max_bytes: None,
            ..limits()
        };
        let u = unlimited.summary_line();
        assert!(
            u.contains("unlimited"),
            "an unlimited ceiling must say so rather than printing a huge number: {u}"
        );
    }

    #[test]
    fn the_limits_summary_distinguishes_small_byte_values_from_zero() {
        // F6: formatting every value as `{:.0} GB` made a 500 MB ceiling and a 0-byte ceiling both
        // print `0 GB`, on the one line the user is meant to sanity-check before a multi-day scan.
        let zero = ArchiveLimits {
            entry_max_bytes: Some(0),
            ..limits()
        };
        let half_gb = ArchiveLimits {
            entry_max_bytes: Some(500 * 1024 * 1024),
            ..limits()
        };
        let z = zero.summary_line();
        let h = half_gb.summary_line();
        assert_ne!(
            z, h,
            "a 0-byte ceiling and a 500 MB ceiling must not print identically"
        );
        assert!(z.contains("0 bytes"), "got {z}");
        assert!(h.contains("MB"), "got {h}");
    }

    #[test]
    fn hashes_flat_entries() {
        let zip = make_zip(&[("a.txt", b"alpha"), ("dir/b.pdf", b"beta")]);
        let res = scan_archive(Cursor::new(zip), &limits());
        assert!(res.errors.is_empty(), "errors: {:?}", res.errors);
        assert_eq!(res.entries.len(), 2);
        let a = res.entries.iter().find(|e| e.filename == "a.txt").unwrap();
        // hash matches hashing::hash_reader over the same bytes
        let mut raw: &[u8] = b"alpha";
        assert_eq!(a.content_hash, hashing::hash_reader(&mut raw).unwrap());
        assert_eq!(a.container_chain, "a.txt");
        assert_eq!(a.size_bytes, 5);
        let b = res.entries.iter().find(|e| e.filename == "b.pdf").unwrap();
        assert_eq!(b.container_chain, "dir/b.pdf");
        assert_eq!(b.extension, "pdf");
    }

    #[test]
    fn rejects_oversized_entry() {
        // entry_max_bytes tiny -> the entry is skipped and logged, not hashed.
        let zip = make_zip(&[("big.bin", b"0123456789")]);
        let small = ArchiveLimits {
            entry_max_bytes: Some(4),
            ..limits()
        };
        let res = scan_archive(Cursor::new(zip), &small);
        assert!(res.entries.is_empty());
        assert_eq!(res.errors.len(), 1);
        assert!(
            res.errors[0].1.contains("zip bomb"),
            "reason: {}",
            res.errors[0].1
        );
    }

    // Wrap an existing zip's bytes as a single entry inside an outer zip.
    fn nest_zip(inner_name: &str, inner_zip: Vec<u8>, alongside: &[(&str, &[u8])]) -> Vec<u8> {
        let mut buf = Cursor::new(Vec::new());
        {
            let mut zw = zip::ZipWriter::new(&mut buf);
            let opts: zip::write::FileOptions<()> = zip::write::FileOptions::default()
                .compression_method(zip::CompressionMethod::Stored);
            zw.start_file(inner_name, opts).unwrap();
            zw.write_all(&inner_zip).unwrap();
            for (name, bytes) in alongside {
                zw.start_file(*name, opts).unwrap();
                zw.write_all(bytes).unwrap();
            }
            zw.finish().unwrap();
        }
        buf.into_inner()
    }

    #[test]
    fn descends_into_nested_archive() {
        let inner = make_zip(&[("vacation.jpg", b"pixels")]);
        let outer = nest_zip("photos.zip", inner, &[("readme.txt", b"hi")]);
        let res = scan_archive(Cursor::new(outer), &limits());
        assert!(res.errors.is_empty(), "errors: {:?}", res.errors);
        // readme.txt (direct) + vacation.jpg (nested); the inner photos.zip itself is also an entry
        let jpg = res
            .entries
            .iter()
            .find(|e| e.filename == "vacation.jpg")
            .unwrap();
        assert_eq!(jpg.container_chain, "photos.zip › vacation.jpg");
        assert!(res
            .entries
            .iter()
            .any(|e| e.container_chain == "readme.txt"));
        // the nested archive is itself catalogued as an entry (an identical inner zip is a dup)
        assert!(res
            .entries
            .iter()
            .any(|e| e.container_chain == "photos.zip"));
    }

    #[test]
    fn a_nested_chain_shares_one_buffer_budget_across_depth() {
        // level3 sits inside level2 sits inside the top archive. Each nested zip is small enough
        // for entry_max_bytes on its own; together they exceed the shared budget, which is the
        // failure mode a per-entry cap cannot see (worst case is max_depth x entry_max_bytes).
        let level3 = make_zip(&[("leaf.txt", &[b'x'; 400][..])]);
        let level2 = nest_zip("level3.zip", level3, &[]);
        let top = nest_zip("level2.zip", level2, &[]);

        let generous = ArchiveLimits {
            buffer_max_bytes: 64 * 1024,
            total_buffer_bytes: 64 * 1024,
            ..limits()
        };
        let ok = scan_archive(Cursor::new(top.clone()), &generous);
        assert!(
            ok.entries.iter().any(|e| e.filename == "leaf.txt"),
            "with budget to spare the whole chain is scanned: {:?}",
            ok.errors
        );

        // Same per-entry cap, but the descent may only hold the outermost buffer at once.
        let held = level2_len(&top);
        let tight = ArchiveLimits {
            buffer_max_bytes: 64 * 1024,
            total_buffer_bytes: held, // exactly enough for level2.zip, nothing left for level3.zip
            ..limits()
        };
        let res = scan_archive(Cursor::new(top), &tight);
        assert!(
            !res.entries.iter().any(|e| e.filename == "leaf.txt"),
            "the deepest level must not be buffered once the budget is spent"
        );
        assert!(
            res.errors.iter().any(|(_, m)| m.contains("budget")),
            "the refusal must be reported, not silent: {:?}",
            res.errors
        );
        // The levels that did fit are still catalogued — the budget skips, it does not abort.
        assert!(res.entries.iter().any(|e| e.filename == "level2.zip"));
        assert!(
            !res.errors.iter().any(|(_, m)| m.contains("zip bomb")),
            "budget pressure from legitimate ancestors must not be reported as a bomb: {:?}",
            res.errors
        );
    }

    #[test]
    fn a_partially_constrained_buffer_is_not_called_a_zip_bomb() {
        // Budget leaves SOME room but not enough: the read fails inside read_capped, whose own
        // message says "zip bomb". The caller must relabel it.
        let inner = make_zip(&[("leaf.txt", &[b'y'; 800][..])]);
        let top = nest_zip("inner.zip", inner, &[]);
        let held = level2_len_named(&top, "inner.zip");
        let tight = ArchiveLimits {
            buffer_max_bytes: 64 * 1024,
            total_buffer_bytes: held - 1, // one byte short of the nested archive
            ..limits()
        };
        let res = scan_archive(Cursor::new(top), &tight);
        assert!(!res.entries.iter().any(|e| e.filename == "leaf.txt"));
        let msgs = format!("{:?}", res.errors);
        assert!(
            msgs.contains("buffer budget"),
            "expected a budget message: {msgs}"
        );
        assert!(!msgs.contains("zip bomb"), "must not blame a bomb: {msgs}");
    }

    /// Uncompressed length of the named entry inside `top`.
    fn level2_len_named(top: &[u8], name: &str) -> u64 {
        let mut z = zip::ZipArchive::new(Cursor::new(top.to_vec())).unwrap();
        let n = z.by_name(name).unwrap().size();
        n
    }

    /// Uncompressed length of the single nested `level2.zip` entry inside `top`.
    fn level2_len(top: &[u8]) -> u64 {
        let mut z = zip::ZipArchive::new(Cursor::new(top.to_vec())).unwrap();
        let n = z.by_name("level2.zip").unwrap().size();
        n
    }

    /// Build an in-memory 7z (Vec of (name, bytes)), same shape as `make_zip` above.
    fn write_7z(path: &std::path::Path, files: &[(&str, &[u8])]) {
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

    #[test]
    fn a_7z_signature_is_recognised_regardless_of_extension() {
        assert!(looks_like_7z(&[0x37, 0x7A, 0xBC, 0xAF, 0x27, 0x1C, 0, 0]));
        assert!(!looks_like_7z(b"PK\x03\x04"));
    }

    #[test]
    fn scanning_a_7z_hashes_its_entries_like_any_other_archive() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("bundle.7z");
        write_7z(&path, &[("a.txt", b"AAA"), ("sub/b.txt", b"BBB")]);

        let f = std::fs::File::open(&path).unwrap();
        let out = scan_archive(std::io::BufReader::new(f), &limits());

        assert!(out.errors.is_empty(), "{:?}", out.errors);
        let mut chains: Vec<&str> = out
            .entries
            .iter()
            .map(|e| e.container_chain.as_str())
            .collect();
        chains.sort();
        assert_eq!(chains, vec!["a.txt", "sub/b.txt"]);
        let a = out
            .entries
            .iter()
            .find(|e| e.container_chain == "a.txt")
            .unwrap();
        assert_eq!(a.content_hash, blake3::hash(b"AAA").to_hex().to_string());
        assert_eq!(a.size_bytes, 3);
    }

    #[test]
    fn stops_at_max_depth() {
        let inner = make_zip(&[("deep.txt", b"x")]);
        let outer = nest_zip("mid.zip", inner, &[]);
        // max_depth = 1: the top archive's direct entries are scanned, but mid.zip is not descended.
        let shallow = ArchiveLimits {
            max_depth: 1,
            ..limits()
        };
        let res = scan_archive(Cursor::new(outer), &shallow);
        assert!(res.entries.iter().any(|e| e.container_chain == "mid.zip")); // still catalogued as a file
        assert!(!res.entries.iter().any(|e| e.filename == "deep.txt")); // not descended
        assert!(res
            .errors
            .iter()
            .any(|(_, r)| r.contains("max archive depth")));
    }
}
