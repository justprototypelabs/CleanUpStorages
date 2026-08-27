use std::io::Write;
use std::process::Command;

fn bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_cleanupstorages"))
}

fn write_zip(path: &std::path::Path, files: &[(&str, &[u8])]) {
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
fn scans_7z_and_finds_inner_file() {
    let tmp = tempfile::tempdir().unwrap();
    let drive = tmp.path().join("drive");
    std::fs::create_dir_all(&drive).unwrap();
    write_7z(
        &drive.join("memories.7z"),
        &[("2019/thesis_backup.pdf", b"important")],
    );
    let data = tmp.path().join("appdata");

    let scan = bin()
        .env("CLEANUPSTORAGES_DATA_DIR", &data)
        .arg("scan")
        .arg(&drive)
        .arg("--readonly-fallback")
        .arg("fingerprint")
        .output()
        .unwrap();
    assert!(
        scan.status.success(),
        "scan failed: {}",
        String::from_utf8_lossy(&scan.stderr)
    );

    let search = bin()
        .env("CLEANUPSTORAGES_DATA_DIR", &data)
        .arg("search")
        .arg("thesis_backup")
        .output()
        .unwrap();
    assert!(search.status.success());
    let out = String::from_utf8_lossy(&search.stdout).to_string();
    assert!(out.contains("memories.7z"), "output: {out}");
    assert!(
        out.contains("2019/thesis_backup.pdf"),
        "the entry's chain must be catalogued: {out}"
    );
}

/// Minor finding from the Task 10 review: this task introduces a format pairing (zip vs. 7z) whose
/// lying-extension combination was never exercised end to end -- only `descent_for()` unit tests
/// covered a renamed *zip*. Content, not extension, must decide both "is this an archive at all"
/// (`scanner.rs`'s top-level sniff) and "which format is it" (`archive::scan_level`'s dispatch), in
/// both directions.
#[test]
fn a_7z_renamed_with_a_zip_extension_is_still_descended_into_as_7z() {
    let tmp = tempfile::tempdir().unwrap();
    let drive = tmp.path().join("drive");
    std::fs::create_dir_all(&drive).unwrap();
    // Real 7z bytes, saved under a .zip name.
    write_7z(
        &drive.join("liar.zip"),
        &[("2019/thesis_backup.pdf", b"important")],
    );
    let data = tmp.path().join("appdata");

    let scan = bin()
        .env("CLEANUPSTORAGES_DATA_DIR", &data)
        .arg("scan")
        .arg(&drive)
        .arg("--readonly-fallback")
        .arg("fingerprint")
        .output()
        .unwrap();
    assert!(
        scan.status.success(),
        "scan failed: {}",
        String::from_utf8_lossy(&scan.stderr)
    );

    let search = bin()
        .env("CLEANUPSTORAGES_DATA_DIR", &data)
        .arg("search")
        .arg("thesis_backup")
        .output()
        .unwrap();
    assert!(search.status.success());
    let out = String::from_utf8_lossy(&search.stdout).to_string();
    assert!(out.contains("liar.zip"), "output: {out}");
    assert!(
        out.contains("2019/thesis_backup.pdf"),
        "a 7z's contents must be catalogued even though the extension lies: {out}"
    );
}

#[test]
fn a_zip_renamed_with_a_7z_extension_is_still_descended_into_as_zip() {
    let tmp = tempfile::tempdir().unwrap();
    let drive = tmp.path().join("drive");
    std::fs::create_dir_all(&drive).unwrap();
    // Real zip bytes, saved under a .7z name.
    write_zip(
        &drive.join("liar.7z"),
        &[("2019/thesis_backup.pdf", b"important")],
    );
    let data = tmp.path().join("appdata");

    let scan = bin()
        .env("CLEANUPSTORAGES_DATA_DIR", &data)
        .arg("scan")
        .arg(&drive)
        .arg("--readonly-fallback")
        .arg("fingerprint")
        .output()
        .unwrap();
    assert!(
        scan.status.success(),
        "scan failed: {}",
        String::from_utf8_lossy(&scan.stderr)
    );

    let search = bin()
        .env("CLEANUPSTORAGES_DATA_DIR", &data)
        .arg("search")
        .arg("thesis_backup")
        .output()
        .unwrap();
    assert!(search.status.success());
    let out = String::from_utf8_lossy(&search.stdout).to_string();
    assert!(out.contains("liar.7z"), "output: {out}");
    assert!(
        out.contains("2019/thesis_backup.pdf"),
        "a zip's contents must be catalogued even though the extension lies: {out}"
    );
}

#[test]
fn scans_archive_and_finds_inner_file() {
    let tmp = tempfile::tempdir().unwrap();
    let drive = tmp.path().join("drive");
    std::fs::create_dir_all(&drive).unwrap();
    write_zip(
        &drive.join("memories.zip"),
        &[("2019/thesis_backup.pdf", b"important")],
    );
    let data = tmp.path().join("appdata");

    let scan = bin()
        .env("CLEANUPSTORAGES_DATA_DIR", &data)
        .arg("scan")
        .arg(&drive)
        .arg("--readonly-fallback")
        .arg("fingerprint")
        .output()
        .unwrap();
    assert!(
        scan.status.success(),
        "scan failed: {}",
        String::from_utf8_lossy(&scan.stderr)
    );

    let search = bin()
        .env("CLEANUPSTORAGES_DATA_DIR", &data)
        .arg("search")
        .arg("thesis_backup")
        .output()
        .unwrap();
    assert!(search.status.success());
    let out = String::from_utf8_lossy(&search.stdout);
    assert!(out.contains("memories.zip"), "output: {out}");
    assert!(
        out.contains("2019/thesis_backup.pdf"),
        "expected container chain in output: {out}"
    );
}
