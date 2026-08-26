//! Extract one catalogued archive to a sibling folder, prove every entry against the catalogued
//! BLAKE3, then quarantine the original (#77).
//!
//! The unit of work is a whole archive: half-extracting one means the original still holds content
//! nothing else has, so it could never be quarantined. Every refusal below happens *before* a byte
//! is written, and every failure after that point deletes the destination and leaves the archive
//! exactly where it was.

use std::path::Path;

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
pub fn full_length(mount_root: &Path, archive_rel: &str, chain: &str) -> usize {
    let root = mount_root.to_string_lossy();
    let sep = if root.ends_with('\\') || root.ends_with('/') {
        0
    } else {
        1
    };
    root.chars().count() + sep + final_relative_path(archive_rel, chain).chars().count()
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
