# Archive extraction Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Extract every catalogued archive whose entries all land within Windows' 260-character path
limit, prove the extraction against the catalogued BLAKE3 hashes, then quarantine the original —
unlocking 552.6 GiB of content that quarantine cannot reach while it sits inside a zip.

**Architecture:** A new deep module `src/extract.rs` owns the whole per-archive operation (path
budget, preflights, extraction, verification, rollback, catalogue conversion, quarantine of the
original). It is driven by a third `Job` variant on the existing serial queue in
`src/quarantine_queue.rs`, so extraction can never race the quarantine worker. A new Extract page
lists archives with their scope verdict and enqueues them.

**Tech Stack:** Rust 2021 (MSRV 1.82), `rusqlite` 0.31 (SQLite), `blake3`, `zip` 2,
`sevenz-rust2` (new), `axum` 0.7, plain HTML/CSS/JS with no build step.

**Spec:** [`docs/superpowers/specs/2026-08-26-archive-extraction-design.md`](../specs/2026-08-26-archive-extraction-design.md)
**Issue:** #77 (part of #75, supersedes the verdict in #72)

## Global Constraints

- **Reliability dominates.** No irreversible destructive action. The original archive is
  *quarantined* (a same-drive rename into `_ToDelete`), never deleted, and only after every
  catalogued entry has been re-hashed and matched.
- **On any failure, the drive must look exactly as it did before.** Delete the destination folder,
  leave the archive untouched, leave the catalogue untouched.
- **Never merge into an existing folder.** A destination that already exists is a refusal.
- **Every refusal is reported with its reason.** A skipped archive that looks like a success would
  leave the user believing content was unlocked when it was not.
- **Path budget is 260 characters**, computed against the *real* mount root resolved at runtime,
  never an assumed `E:\`.
- **Free-space floor:** refuse unless `sum(entry size_bytes) + 5 GiB` is available.
- **Deny list is reused unchanged** from `config::DEFAULT_DENY` + `settings.json`. `docx`, `xlsx`,
  `jar`, `apk`, `epub`, `ipa` are zip-format *documents*; exploding one destroys it.
- **Serial only.** Extraction runs on the existing `QuarantineQueue` worker, one job at a time.
- Conventional Commits, scopes from CONTRIBUTING.md. This work uses `extract`, `catalog`, `archive`,
  `web`, `scanner`.
- CI runs on Windows **and** macOS. Nothing may depend on a drive letter or on `\` separators;
  catalogue paths are stored with `/`.

## Deviation from the spec, already agreed

Spec step 10 said "any extracted file that is itself an archive is enqueued as a new
`Job::Extract`". That is kept, but the spec did not say what happens to the catalogue rows *under*
that nested archive. It is settled here:

- The scanner flattens nesting: a zip inside a zip yields leaf rows with
  `container_chain = "inner.zip › x.txt"`, and **no row exists for `inner.zip` itself**.
- Extraction writes **one level**. `inner.zip` appears in the destination folder as a real file,
  which is what the user asked for.
- A row whose chain has more than one segment is therefore **not** converted to loose. It is
  *re-pointed*: `relative_path` := `<dest>/inner.zip`, `container_chain` := the chain minus its
  first segment. The row stays archived, stays correct, and nothing dangles at the moved original.
- `inner.zip` has no catalogued hash of its own, so it cannot be hash-verified directly. It is
  verified **by its contents**: the destination is re-scanned with `archive::scan_archive`, exactly
  as the scanner would, and every catalogued chain must be present with a matching hash.
- The **path budget still uses the fully-recursive layout** (each intermediate archive segment
  becomes a sibling folder named after its stem), so an archive is only ever started if the whole
  eventual tree fits. This is what produces the spec's 1,512-archive figure.

The spec document is updated to say this in Task 12.

---

## File map

| File | Responsibility |
| --- | --- |
| `src/extract.rs` | **New.** The whole per-archive operation: path budget, preflights, extraction, verification, rollback, catalogue conversion, quarantine of the original. |
| `src/catalog/store.rs` | **Modify.** Two new methods: `archive_roots` (what the Extract page lists) and `convert_archive_entries` (the one-transaction row rewrite). |
| `src/quarantine_queue.rs` | **Modify.** `Job::Extract` variant, its `Work`/`Done` arms, depth-bounded recursion enqueue. |
| `src/archive.rs` | **Modify.** 7z detection + descent alongside the zip path. |
| `src/web.rs` | **Modify.** `GET /api/archives`, `POST /api/extract`, route wiring. |
| `src/web_ui.rs` | **Modify.** `extract_page`, NAV entry, glyph. |
| `src/lib.rs` | **Modify.** `pub mod extract;` |
| `Cargo.toml` | **Modify.** `sevenz-rust2` dependency. |
| `tests/extract_flow.rs` | **New.** End-to-end: scan → extract → verify → original in `_ToDelete`. |
| `docs/superpowers/specs/2026-08-26-archive-extraction-design.md` | **Modify.** Record the nesting decision above; mark Accepted. |
| `docs/TESTING-GUIDE.md`, `posts/POST-MATERIAL.md` | **Modify.** Walkthrough + build-in-public line. |

---

### Task 1: Path mapping and the 260-character budget

The pure arithmetic, alone, before anything touches a disk or a database. `container_chain` uses
` › ` (space, U+203A, space) as its separator — see `archive::join_chain`.

**Files:**
- Create: `src/extract.rs`
- Modify: `src/lib.rs`
- Test: inline `#[cfg(test)] mod tests` in `src/extract.rs`

**Interfaces:**
- Consumes: nothing.
- Produces:
  - `pub const MAX_PATH_CHARS: usize = 260;`
  - `pub const CHAIN_SEP: &str = " › ";`
  - `pub fn destination_dir(archive_rel: &str) -> String` — the sibling folder, forward slashes.
  - `pub fn final_relative_path(archive_rel: &str, chain: &str) -> String` — where the entry ends up
    once the *whole* tree has been extracted.
  - `pub fn first_hop(archive_rel: &str, chain: &str) -> (String, Option<String>)` — where the entry
    lands after **this one level**: the relative path written now, and the remaining chain (`None`
    when the entry became a loose file).

- [ ] **Step 1: Write the failing tests**

```rust
// src/extract.rs
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
        assert!(!fits_budget(long, "bundle.zip", &format!("{chain}{}", "y".repeat(40))));
    }
}
```

- [ ] **Step 2: Run the tests and watch them fail**

Run: `cargo test --lib extract::`
Expected: FAIL — `unresolved module or unlinked crate 'extract'` until `lib.rs` declares it, then
`cannot find function ...`.

- [ ] **Step 3: Implement**

```rust
// src/extract.rs
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
```

```rust
// src/lib.rs — add beside the other module declarations
pub mod extract;
```

- [ ] **Step 4: Run the tests**

Run: `cargo test --lib extract::`
Expected: PASS (5 tests).

- [ ] **Step 5: Commit**

```bash
git add src/extract.rs src/lib.rs
git commit -m "feat(extract): map archive entries to their extracted paths and budget them"
```

---

### Task 2: Scope check — is this archive extractable at all?

Everything that can refuse an archive before a byte is written, in one function, so the Extract page
and the worker ask the same question and cannot drift.

**Files:**
- Modify: `src/extract.rs`
- Modify: `src/catalog/store.rs` (new `archive_roots`)
- Test: inline in both files

**Interfaces:**
- Consumes: Task 1's `fits_budget`, `full_length`, `destination_dir`, `final_relative_path`.
- Produces:
  - `pub enum Scope { InScope { entries: usize, uncompressed_bytes: i64 }, Refused(String) }`
  - `pub fn scope_check(cat: &Catalog, mount_root: &Path, volume_id: &str, archive_rel: &str) -> anyhow::Result<Scope>`
  - `Catalog::archive_roots(&self, volume_id: &str) -> anyhow::Result<Vec<ArchiveRoot>>` with
    `pub struct ArchiveRoot { pub relative_path: String, pub entries: i64, pub uncompressed_bytes: i64 }`

- [ ] **Step 1: Write the failing tests**

```rust
// src/catalog/store.rs — inside the existing #[cfg(test)] mod tests
#[test]
fn archive_roots_lists_each_container_once_with_its_totals() {
    let (_tmp, cat) = test_catalog(); // existing helper in this module
    insert_entry(&cat, "vol-1", "a/bundle.zip", "one.txt", 10, "h1");
    insert_entry(&cat, "vol-1", "a/bundle.zip", "two.txt", 20, "h2");
    insert_entry(&cat, "vol-1", "b/other.zip", "three.txt", 5, "h3");

    let roots = cat.archive_roots("vol-1").unwrap();
    assert_eq!(roots.len(), 2, "one row per archive, not per entry");
    let bundle = roots.iter().find(|r| r.relative_path == "a/bundle.zip").unwrap();
    assert_eq!(bundle.entries, 2);
    assert_eq!(bundle.uncompressed_bytes, 30);
}
```

```rust
// src/extract.rs — inside mod tests
#[test]
fn an_archive_whose_entries_all_fit_is_in_scope() {
    let (tmp, cat) = fixture_with_entries(&[("small.txt", 4, "h1")]);
    match scope_check(&cat, tmp.path(), "vol-1", "bundle.zip").unwrap() {
        Scope::InScope { entries, uncompressed_bytes } => {
            assert_eq!(entries, 1);
            assert_eq!(uncompressed_bytes, 4);
        }
        Scope::Refused(r) => panic!("expected in scope, got {r}"),
    }
}

#[test]
fn one_over_long_entry_refuses_the_whole_archive_and_names_it() {
    let long = format!("{}/x.txt", "d".repeat(300));
    let (tmp, cat) = fixture_with_entries(&[(long.as_str(), 4, "h1")]);
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
    let (tmp, cat) = fixture_with_entries(&[("a.txt", 4, "h1")]);
    std::fs::create_dir_all(tmp.path().join("bundle")).unwrap();
    match scope_check(&cat, tmp.path(), "vol-1", "bundle.zip").unwrap() {
        Scope::Refused(r) => assert!(r.contains("bundle"), "refusal must name the folder: {r}"),
        Scope::InScope { .. } => panic!("must never merge into an existing folder"),
    }
}

#[test]
fn an_archive_with_no_catalogued_entries_is_refused() {
    let (tmp, cat) = fixture_with_entries(&[]);
    match scope_check(&cat, tmp.path(), "vol-1", "bundle.zip").unwrap() {
        Scope::Refused(r) => assert!(r.contains("no catalogued entries"), "{r}"),
        Scope::InScope { .. } => panic!("nothing to verify against means nothing to extract"),
    }
}
```

Add the fixture helper in `src/extract.rs`'s test module:

```rust
/// A temp "drive" with a volume marker and `bundle.zip` catalogued as an archive holding `entries`.
fn fixture_with_entries(entries: &[(&str, i64, &str)]) -> (tempfile::TempDir, Catalog) {
    let tmp = tempfile::tempdir().unwrap();
    crate::volume::write_volume_id(tmp.path(), "vol-1").unwrap();
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
            1,
        )
        .unwrap();
    }
    (tmp, cat)
}
```

> Check `upsert_archive_entry`'s real signature at `src/catalog/store.rs:235` before writing this
> helper and match it exactly; adapt the call, not the intent.
> `crate::volume::write_volume_id` is what the existing quarantine tests use to plant a marker —
> confirm its name at `src/volume.rs` and use the same one.

- [ ] **Step 2: Run and watch them fail**

Run: `cargo test --lib extract:: ; cargo test --lib store::tests::archive_roots`
Expected: FAIL — `cannot find function scope_check` / `no method named archive_roots`.

- [ ] **Step 3: Implement `archive_roots`**

```rust
// src/catalog/store.rs
/// One row per archive that still has active catalogued entries, with what extracting it costs.
/// This is what the Extract page lists; the per-archive verdict is computed separately, against a
/// live mount.
pub struct ArchiveRoot {
    pub relative_path: String,
    pub entries: i64,
    pub uncompressed_bytes: i64,
}

impl Catalog {
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
}
```

- [ ] **Step 4: Implement `scope_check`**

```rust
// src/extract.rs
use crate::catalog::Catalog;

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
pub fn scope_check(
    cat: &Catalog,
    mount_root: &Path,
    volume_id: &str,
    archive_rel: &str,
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
    match crate::repack::available_space(mount_root) {
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
```

- [ ] **Step 5: Run the tests**

Run: `cargo test --lib extract:: && cargo test --lib archive_roots`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
git add src/extract.rs src/catalog/store.rs
git commit -m "feat(extract): refuse an archive before writing a byte, with the reason"
```

---

### Task 3: Extract one level to the destination folder

Writing files. Still nothing touches the original archive or the catalogue.

**Files:**
- Modify: `src/extract.rs`
- Test: inline

**Interfaces:**
- Consumes: `first_hop`, `Scope`, `archive::ArchiveLimits`.
- Produces: `fn write_level(archive_path: &Path, dest_abs: &Path, limits: &ArchiveLimits) -> anyhow::Result<Vec<String>>`
  — returns the destination-relative paths written, in archive order.

- [ ] **Step 1: Write the failing test**

```rust
// src/extract.rs — mod tests
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
    assert!(!tmp.path().join("escaped.txt").exists(), "zip-slip must write nothing");
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
    assert!(format!("{err}").contains("1024"), "must report the cap: {err}");
}
```

`test_limits()` already exists in `src/quarantine.rs`'s test module — copy that helper into
`src/extract.rs`'s test module verbatim (it is 12 lines and duplicating it keeps the test modules
independent, which is how this codebase already does it).

- [ ] **Step 2: Run and watch it fail**

Run: `cargo test --lib extract::tests::write_level`
Expected: FAIL — `cannot find function write_level`.

- [ ] **Step 3: Implement**

```rust
// src/extract.rs
use std::io::Read;

/// Write every file entry of ONE archive level into `dest_abs`, recreating the archive's own
/// layout: a nested archive lands as a file, not as a folder. Returns the destination-relative
/// paths written, so the caller can clean up precisely and verify what it asked for.
///
/// Any error leaves cleanup to the caller (`extract_archive` deletes the whole destination), which
/// is why this function never tries to unwind half of its own work.
fn write_level(
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

        // Zip slip: a crafted name must never write outside the destination. Checked on the
        // components, not on the string, so `a/../../b` is caught as well as a leading `..`.
        if name.split('/').any(|c| c == ".." || c == "." || c.is_empty())
            || name.contains('\\')
            || Path::new(&name).is_absolute()
        {
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
```

- [ ] **Step 4: Run the tests**

Run: `cargo test --lib extract::`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/extract.rs
git commit -m "feat(extract): write one archive level, refusing escapes and bombs"
```

---

### Task 4: Verify the destination against the catalogue

The step that makes quarantining the original safe. Verification re-derives the same chains the
scanner would, so a nested archive is proven by its contents even though it has no hash of its own.

**Files:**
- Modify: `src/extract.rs`
- Test: inline

**Interfaces:**
- Consumes: `write_level`, `archive::scan_archive`, `archive::descent_for`.
- Produces: `fn verify_destination(cat: &Catalog, volume_id: &str, archive_rel: &str, dest_abs: &Path, limits: &ArchiveLimits) -> anyhow::Result<()>`
  — `Ok(())` only when every catalogued chain is present with a matching hash.

- [ ] **Step 1: Write the failing tests**

```rust
// src/extract.rs — mod tests
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

    let err = verify_destination(&cat, "vol-1", "bundle.zip", &dest, &test_limits()).unwrap_err();
    assert!(format!("{err}").contains("a.txt"), "must name the entry: {err}");
    assert!(format!("{err}").contains("hash"), "must say what failed: {err}");
}

#[test]
fn verification_fails_when_a_catalogued_entry_is_missing() {
    let tmp = tempfile::tempdir().unwrap();
    let (cat, dest) = extracted_fixture(&tmp, &[("a.txt", b"AAA")]);
    std::fs::remove_file(dest.join("a.txt")).unwrap();

    let err = verify_destination(&cat, "vol-1", "bundle.zip", &dest, &test_limits()).unwrap_err();
    assert!(format!("{err}").contains("missing"), "{err}");
}

#[test]
fn a_nested_archive_is_verified_through_its_contents() {
    // bundle.zip contains inner.zip contains deep.txt. After one level, only inner.zip is on
    // disk — and that is enough to prove the catalogued chain "inner.zip › deep.txt".
    let tmp = tempfile::tempdir().unwrap();
    let (cat, dest) = nested_extracted_fixture(&tmp);
    verify_destination(&cat, "vol-1", "bundle.zip", &dest, &test_limits()).unwrap();
}
```

Helpers for the test module:

```rust
/// Build a catalogue whose `bundle.zip` holds `files`, and a destination folder already containing
/// them — i.e. the state immediately after `write_level` succeeded.
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
```

> `catalog_with_entries` is `fixture_with_entries` from Task 2 with the temp dir passed in rather
> than created; refactor Task 2's helper to that shape and have both call it.
> `nested_extracted_fixture` builds `inner.zip` with `deep.txt` inside, writes it into
> `<dest>/inner.zip`, and catalogues one entry with chain `inner.zip › deep.txt` whose hash is
> `blake3(deep.txt contents)`.

- [ ] **Step 2: Run and watch them fail**

Run: `cargo test --lib extract::tests::verif`
Expected: FAIL — `cannot find function verify_destination`.

- [ ] **Step 3: Implement**

```rust
// src/extract.rs
use std::collections::HashMap;

/// Re-derive every catalogued chain from what is now on disk and compare hashes. Loose files are
/// hashed directly; a nested archive that was written as a file is descended into with the
/// scanner's own reader, so its chains come out identical to the ones already catalogued.
///
/// This is the step that makes quarantining the original safe. Without it, the original is removed
/// on nothing but the assumption that the extraction worked.
fn verify_destination(
    cat: &Catalog,
    volume_id: &str,
    archive_rel: &str,
    dest_abs: &Path,
    limits: &crate::archive::ArchiveLimits,
) -> anyhow::Result<()> {
    // chain -> hash, as it exists on disk right now.
    let mut found: HashMap<String, String> = HashMap::new();
    for entry in walkdir::WalkDir::new(dest_abs)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
    {
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
            let f = std::fs::File::open(entry.path())?;
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
        found.insert(rel, crate::hashing::hash_file(entry.path())?);
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
```

> `crate::hashing::hash_file` — check the real name and signature at `src/hashing.rs` (45 lines) and
> use it; do not add a second hashing helper.

- [ ] **Step 4: Run the tests**

Run: `cargo test --lib extract::`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/extract.rs
git commit -m "feat(extract): prove the destination against every catalogued hash"
```

---

### Task 5: Convert catalogue rows in one transaction

**Files:**
- Modify: `src/catalog/store.rs`
- Test: inline in `store.rs`

**Interfaces:**
- Produces:
  - `pub struct EntryMove { pub id: i64, pub relative_path: String, pub container_chain: Option<String> }`
  - `Catalog::convert_archive_entries(&self, moves: &[EntryMove], now: i64) -> anyhow::Result<()>`

- [ ] **Step 1: Write the failing test**

```rust
// src/catalog/store.rs — mod tests
#[test]
fn converting_an_entry_keeps_its_id_hash_and_history() {
    let (_tmp, cat) = test_catalog();
    insert_entry(&cat, "vol-1", "bundle.zip", "a.txt", 3, "h1");
    let before = cat.archive_entries("vol-1", "bundle.zip").unwrap().remove(0);

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
    assert_eq!(after.first_seen_at, before.first_seen_at, "history survives");
    assert_eq!(after.relative_path, "bundle/a.txt");
    assert!(after.container_chain.is_none(), "now a loose file");
}

#[test]
fn a_nested_entry_is_repointed_at_the_extracted_inner_archive() {
    let (_tmp, cat) = test_catalog();
    insert_entry(&cat, "vol-1", "bundle.zip", "inner.zip › deep.txt", 3, "h1");
    let before = cat.archive_entries("vol-1", "bundle.zip").unwrap().remove(0);

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
    let (_tmp, cat) = test_catalog();
    insert_file(&cat, "vol-1", "bundle/a.txt", "h9"); // already occupies the loose path
    insert_entry(&cat, "vol-1", "bundle.zip", "a.txt", 3, "h1");
    insert_entry(&cat, "vol-1", "bundle.zip", "b.txt", 3, "h2");
    let rows = cat.archive_entries("vol-1", "bundle.zip").unwrap();

    let err = cat.convert_archive_entries(
        &[
            EntryMove { id: rows[1].id, relative_path: "bundle/b.txt".into(), container_chain: None },
            EntryMove { id: rows[0].id, relative_path: "bundle/a.txt".into(), container_chain: None },
        ],
        999,
    );

    assert!(err.is_err(), "the unique loose index must reject the collision");
    let b = cat.get_file(rows[1].id).unwrap().unwrap();
    assert_eq!(
        b.container_chain.as_deref(),
        Some("b.txt"),
        "the first move must have been rolled back, not left half-applied"
    );
}
```

- [ ] **Step 2: Run and watch it fail**

Run: `cargo test --lib convert_archive_entries`
Expected: FAIL — `cannot find type EntryMove`.

- [ ] **Step 3: Implement**

```rust
// src/catalog/store.rs
/// Where one archived entry row moves to once its archive has been extracted. `container_chain` is
/// `None` when the entry became a loose file, or the remaining chain when its first hop was a
/// nested archive that is now a file on disk.
pub struct EntryMove {
    pub id: i64,
    pub relative_path: String,
    pub container_chain: Option<String>,
}

impl Catalog {
    /// Rewrite archived entry rows in place, all or nothing.
    ///
    /// In place, rather than delete-and-reinsert, because `id`, `content_hash` and `first_seen_at`
    /// carry the file's whole history and every other table refers to the id. One transaction,
    /// because a half-applied conversion would leave the catalogue describing a layout that never
    /// existed on disk.
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
}
```

> The codebase already uses `unchecked_transaction` elsewhere in `store.rs` because `Catalog` holds
> `conn` behind `&self` — follow whatever the surrounding methods do rather than changing the
> pattern. If the unique index does not fire inside the transaction as the third test expects,
> that is a real finding: report it rather than weakening the test.

- [ ] **Step 4: Run the tests**

Run: `cargo test --lib convert_archive_entries`
Expected: PASS (3 tests).

- [ ] **Step 5: Commit**

```bash
git add src/catalog/store.rs
git commit -m "feat(catalog): convert archived entry rows in place, all or nothing"
```

---

### Task 6: `extract_archive` — the whole operation, in order

**Files:**
- Modify: `src/extract.rs`
- Test: inline

**Interfaces:**
- Consumes: everything from Tasks 1–5, `quarantine::quarantine_files`, `volume::read_volume_id`.
- Produces:
  - `pub struct ExtractOutcome { pub entries_converted: usize, pub bytes_written: u64, pub dest_relative_path: String, pub quarantined: bool, pub nested_archives: Vec<String> }`
  - `pub fn extract_archive(cat: &Catalog, mount_root: &Path, expected_volume_id: &str, archive_rel: &str, limits: &ArchiveLimits, now: i64) -> anyhow::Result<ExtractOutcome>`

- [ ] **Step 1: Write the failing tests**

```rust
// src/extract.rs — mod tests
#[test]
fn a_small_zip_extracts_verifies_and_its_original_is_quarantined() {
    let tmp = tempfile::tempdir().unwrap();
    let (cat, _) = real_scan_fixture(&tmp, &[("a.txt", b"AAA"), ("sub/b.txt", b"BBB")]);

    let out = extract_archive(&cat, tmp.path(), "vol-1", "bundle.zip", &test_limits(), 100).unwrap();

    assert_eq!(out.entries_converted, 2);
    assert_eq!(out.dest_relative_path, "bundle");
    assert!(out.quarantined);
    assert_eq!(std::fs::read(tmp.path().join("bundle/a.txt")).unwrap(), b"AAA");
    assert!(!tmp.path().join("bundle.zip").is_file(), "original moved out of the way");
    assert!(
        tmp.path().join("_ToDelete/bundle.zip").is_file(),
        "original is in quarantine, never deleted"
    );
    let loose = cat.loose_file_id("vol-1", "bundle/a.txt").unwrap();
    assert!(loose.is_some(), "extracted file is now a loose catalogue row");
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
    assert!(tmp.path().join("bundle.zip").is_file(), "original untouched");
    assert!(
        cat.archive_entries("vol-1", "bundle.zip").unwrap()[0].container_chain.is_some(),
        "catalogue untouched"
    );
}

#[test]
fn the_wrong_drive_is_refused_before_anything_is_written() {
    let tmp = tempfile::tempdir().unwrap();
    let (cat, _) = real_scan_fixture(&tmp, &[("a.txt", b"AAA")]);

    let err = extract_archive(&cat, tmp.path(), "vol-OTHER", "bundle.zip", &test_limits(), 100)
        .unwrap_err();

    assert!(format!("{err}").contains("vol-OTHER"), "{err}");
    assert!(!tmp.path().join("bundle").exists());
}

#[test]
fn a_nested_archive_is_reported_for_a_follow_up_job() {
    let tmp = tempfile::tempdir().unwrap();
    let (cat, _) = real_nested_scan_fixture(&tmp); // bundle.zip > inner.zip > deep.txt

    let out = extract_archive(&cat, tmp.path(), "vol-1", "bundle.zip", &test_limits(), 100).unwrap();

    assert_eq!(out.nested_archives, vec!["bundle/inner.zip".to_string()]);
    assert!(tmp.path().join("bundle/inner.zip").is_file(), "inner archive is a real file");
    let row = cat.archive_entries("vol-1", "bundle/inner.zip").unwrap();
    assert_eq!(row.len(), 1, "the deep entry now hangs off the inner archive's new path");
    assert_eq!(row[0].container_chain.as_deref(), Some("deep.txt"));
}

#[test]
fn a_deny_listed_zip_document_is_never_extracted() {
    let tmp = tempfile::tempdir().unwrap();
    let (cat, _) = real_scan_fixture_named(&tmp, "report.docx", &[("word/doc.xml", b"<x/>")]);

    let err = extract_archive(&cat, tmp.path(), "vol-1", "report.docx", &test_limits(), 100)
        .unwrap_err();

    assert!(format!("{err}").contains("docx"), "{err}");
    assert!(tmp.path().join("report.docx").is_file(), "the document survives intact");
}
```

> `real_scan_fixture` writes a real zip onto the temp drive, plants the volume marker, and
> catalogues its entries by running `archive::scan_archive` over it and calling
> `upsert_archive_entry` — so the hashes under test are real, not hand-written. Build it once, in
> the test module, and have the nested and named variants share it.

- [ ] **Step 2: Run and watch them fail**

Run: `cargo test --lib extract::tests`
Expected: FAIL — `cannot find function extract_archive`.

- [ ] **Step 3: Implement**

```rust
// src/extract.rs
/// What one successful extraction did.
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
        anyhow::bail!("archive {archive_rel} is not on disk at {}", mount_root.display());
    }

    // 3-5. Every refusal, before a byte is written.
    let entries = match scope_check(cat, mount_root, expected_volume_id, archive_rel)? {
        Scope::InScope { entries, .. } => entries,
        Scope::Refused(reason) => anyhow::bail!("{archive_rel}: {reason}"),
    };
    let _ = entries;

    let dest_rel = destination_dir(archive_rel);
    let dest_abs = mount_root.join(&dest_rel);

    // 6-8. Write, verify, and on ANY failure remove everything this call created.
    let written = match write_level(&archive_path, &dest_abs, limits)
        .and_then(|w| verify_destination(cat, expected_volume_id, archive_rel, &dest_abs, limits).map(|_| w))
    {
        Ok(w) => w,
        Err(e) => {
            let _ = std::fs::remove_dir_all(&dest_abs);
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
        let _ = std::fs::remove_dir_all(&dest_abs);
        return Err(e.context("catalogue conversion failed; extraction rolled back"));
    }

    // 9b. Quarantine the original through the existing engine, so the marker check, the
    // rename-only rule and the action log all apply unchanged.
    let archive_id = cat
        .loose_file_id(expected_volume_id, archive_rel)?
        .ok_or_else(|| anyhow::anyhow!("no loose catalogue row for {archive_rel}"))?;
    let q = crate::quarantine::quarantine_files(
        cat,
        mount_root,
        expected_volume_id,
        &[archive_id],
        now,
    )?;

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
```

> **Watch this one:** `quarantine_files` runs the last-copy guard, and the archive's own content
> hash is unique to the archive, so the guard should pass. If it skips instead, the outcome must
> say so (`quarantined: false`) rather than claiming success — the caller reports it. Do not
> weaken the guard to make the test pass.

- [ ] **Step 4: Run the tests**

Run: `cargo test --lib extract::`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/extract.rs
git commit -m "feat(extract): extract, verify, convert rows, then quarantine the original"
```

---

### Task 7: `Job::Extract` on the serial queue

**Files:**
- Modify: `src/quarantine_queue.rs`
- Test: inline in `quarantine_queue.rs`

**Interfaces:**
- Consumes: `extract::extract_archive`, `ExtractOutcome`.
- Produces: `QuarantineQueue::enqueue_extract(self: &Arc<Self>, volume_id: String, path: String) -> usize`,
  and a `kind` of `"extract"` on `QuarantineResult` / `QuarantineJobDto`.

- [ ] **Step 1: Write the failing tests**

```rust
// src/quarantine_queue.rs — mod tests
#[test]
fn extract_jobs_queue_behind_quarantines_and_report_their_position() {
    let q = queue();
    assert_eq!(q.enqueue_tree("v".into(), "a".into()), 0);
    assert_eq!(q.enqueue_extract("v".into(), "b.zip".into()), 1);
    let s = q.status();
    assert_eq!(s.pending[1].kind, "extract");
    assert_eq!(s.pending[1].label, "b.zip");
}

#[test]
fn the_same_archive_is_never_queued_twice() {
    let q = queue();
    q.enqueue_extract("v".into(), "b.zip".into());
    q.enqueue_extract("v".into(), "b.zip".into());
    assert_eq!(q.status().pending.len(), 1, "a double click is one decision");
}

#[test]
fn depth_bounds_the_recursion_into_nested_archives() {
    assert!(within_depth("a.zip", 8), "a top-level archive is depth 1");
    // bundle/inner.zip is depth 2, bundle/inner/deeper.zip is depth 3, and so on.
    assert!(!within_depth_for_test(9, 8));
}
```

- [ ] **Step 2: Run and watch them fail**

Run: `cargo test --lib quarantine_queue::`
Expected: FAIL — `no method named enqueue_extract`.

- [ ] **Step 3: Implement**

```rust
// src/quarantine_queue.rs

// In `enum Job`:
    /// Extract one archive, verify it, and quarantine the original. `depth` is 1 for an archive
    /// the user picked and increments for each nested archive the previous level produced, so the
    /// recursion is bounded by `ArchiveLimits::max_depth` exactly as the scanner's descent is.
    Extract {
        volume_id: String,
        path: String,
        depth: usize,
    },

// `Job::volume_id`, `Job::kind` ("extract"), `Job::label` (the archive path) each gain their arm.

// In `enum Work`:
    Extract { path: String, depth: usize },

// `Done` gains:
    /// Archives this job wrote that are themselves extractable, with the depth they sit at.
    nested: Vec<(String, usize)>,
```

```rust
// The queued-work arm inside run_job's spawn_blocking closure:
Work::Extract { path, depth } => {
    let cfg = crate::config::Config::load();
    let limits = crate::archive::ArchiveLimits::from_config(&cfg);
    if depth > limits.max_depth {
        anyhow::bail!(
            "{path} sits {depth} archives deep, past the max_archive_depth of {}; \
             extract it by hand or raise the limit",
            limits.max_depth
        );
    }
    let out = crate::extract::extract_archive(&cat, &mount, &vid, &path, &limits, now)?;
    Done {
        files_updated: out.entries_converted,
        skipped: usize::from(!out.quarantined),
        dest: Some(out.dest_relative_path),
        nested: out
            .nested_archives
            .into_iter()
            .map(|p| (p, depth + 1))
            .collect(),
    }
}
```

```rust
// After the result is recorded, before the drain check, enqueue what this job produced. Doing it
// here (rather than inside extract.rs) keeps the queue the only thing that knows about the queue.
for (path, depth) in nested {
    self.enqueue_extract_at_depth(volume_id.clone(), path, depth);
}
```

```rust
/// Add an archive extraction; returns how many are ahead of it (0 = starts next).
pub fn enqueue_extract(self: &Arc<Self>, volume_id: String, path: String) -> usize {
    self.enqueue_extract_at_depth(volume_id, path, 1)
}

fn enqueue_extract_at_depth(
    self: &Arc<Self>,
    volume_id: String,
    path: String,
    depth: usize,
) -> usize {
    let pos = {
        let mut inner = self.inner.lock().unwrap();
        let dup = inner.jobs().any(|j| {
            matches!(j, Job::Extract { volume_id: v, path: p, .. } if v == &volume_id && p == &path)
        });
        if dup {
            return inner.pending.len();
        }
        inner.pending.push_back(Job::Extract { volume_id, path, depth });
        inner.pending.len() - 1 + inner.running.is_some() as usize
    };
    self.notify.notify_one();
    pos
}
```

> Every other arm of `Done` must gain `nested: Vec::new()`. Keep `Done` a plain struct; do not
> reach for an enum here — the two existing arms already collapse to this shape and a third fits.

- [ ] **Step 4: Run the tests**

Run: `cargo test --lib quarantine_queue::`
Expected: PASS. Then `cargo build` to confirm every match arm is exhaustive.

- [ ] **Step 5: Commit**

```bash
git add src/quarantine_queue.rs
git commit -m "feat(dedup): run archive extractions on the shared serial worker"
```

---

### Task 8: HTTP API — list archives with a verdict, enqueue one

**Files:**
- Modify: `src/web.rs`
- Test: `tests/browse_server.rs` (follow the existing harness in that file)

**Interfaces:**
- Consumes: `Catalog::archive_roots`, `extract::scope_check`, `QuarantineQueue::enqueue_extract`.
- Produces:
  - `GET /api/archives?volume_id=<id>` → `{"volumes":[{"volume_id","label","connected":bool,
    "archives":[{"relative_path","entries","uncompressed_bytes","in_scope":bool,"reason":null|"..."}]}]}`
  - `POST /api/extract` `{"volume_id","paths":[...]}` → `{"queued":n,"skipped":n,"refusals":[{"path","reason"}]}`

- [ ] **Step 1: Write the failing test**

```rust
// tests/browse_server.rs
#[test]
fn archives_endpoint_reports_scope_and_refuses_to_guess_for_an_offline_drive() {
    let addr = start_server(); // existing helper; extend its fixture with an archive entry
    let body = get(addr, "/api/archives");
    assert!(body.contains("\"connected\":false"), "offline drive must say so: {body}");
    assert!(
        !body.contains("\"in_scope\":true"),
        "no verdict may be issued without a live mount: {body}"
    );
}
```

- [ ] **Step 2: Run and watch it fail**

Run: `cargo test --test browse_server archives_endpoint`
Expected: FAIL — 404 from an unrouted path.

- [ ] **Step 3: Implement**

```rust
// src/web.rs — DTOs beside the existing ones
#[derive(serde::Serialize)]
struct ArchiveDto {
    relative_path: String,
    entries: i64,
    uncompressed_bytes: i64,
    /// `None` when the drive is not connected: scope is only ever computed against a live mount,
    /// because an assumed `E:\` silently invalidates the whole path-length check.
    in_scope: Option<bool>,
    reason: Option<String>,
}

#[derive(serde::Serialize)]
struct ArchiveVolumeDto {
    volume_id: String,
    label: String,
    connected: bool,
    archives: Vec<ArchiveDto>,
}

async fn api_archives(State(state): State<AppState>) -> Result<Json<Vec<ArchiveVolumeDto>>, (StatusCode, String)> {
    let cat = Catalog::open_readonly(&state.catalog_path).map_err(err500)?;
    let mounts = state.mounts.snapshot();
    let labels = cat.effective_labels().map_err(err500)?;
    let mut out = Vec::new();
    for (volume_id, _label_unused) in cat.volume_paths().map_err(err500)? {
        let mount = mounts.get(&volume_id);
        let mut archives = Vec::new();
        for root in cat.archive_roots(&volume_id).map_err(err500)? {
            let (in_scope, reason) = match mount {
                None => (None, None),
                Some(m) => match crate::extract::scope_check(&cat, m, &volume_id, &root.relative_path)
                    .map_err(err500)?
                {
                    crate::extract::Scope::InScope { .. } => (Some(true), None),
                    crate::extract::Scope::Refused(r) => (Some(false), Some(r)),
                },
            };
            archives.push(ArchiveDto {
                relative_path: root.relative_path,
                entries: root.entries,
                uncompressed_bytes: root.uncompressed_bytes,
                in_scope,
                reason,
            });
        }
        out.push(ArchiveVolumeDto {
            label: labels.get(&volume_id).cloned().unwrap_or_else(|| volume_id.clone()),
            connected: mount.is_some(),
            volume_id,
            archives,
        });
    }
    Ok(Json(out))
}

#[derive(serde::Deserialize)]
struct ExtractReq {
    volume_id: String,
    paths: Vec<String>,
}

#[derive(Default, serde::Serialize)]
struct ExtractQueuedDto {
    queued: usize,
    skipped: usize,
    refusals: Vec<Refusal>,
}

#[derive(serde::Serialize)]
struct Refusal {
    path: String,
    reason: String,
}

async fn api_extract(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Json<ExtractReq>,
) -> Result<Json<ExtractQueuedDto>, (StatusCode, String)> {
    check_csrf(&headers, &state)?;
    let cat = Catalog::open_readonly(&state.catalog_path).map_err(err500)?;
    let Some(mount) = state.mounts.snapshot().get(&body.volume_id).cloned() else {
        return Err((
            StatusCode::CONFLICT,
            format!("drive {} is not connected", body.volume_id),
        ));
    };
    let mut out = ExtractQueuedDto::default();
    for path in &body.paths {
        // Checked here so a bulk enqueue reports its refusals immediately rather than filling the
        // queue with items that will each fail one at a time. The worker re-checks regardless.
        match crate::extract::scope_check(&cat, &mount, &body.volume_id, path).map_err(err500)? {
            crate::extract::Scope::Refused(reason) => {
                out.skipped += 1;
                out.refusals.push(Refusal { path: path.clone(), reason });
            }
            crate::extract::Scope::InScope { .. } => {
                state.quarantine_queue.enqueue_extract(body.volume_id.clone(), path.clone());
                out.queued += 1;
            }
        }
    }
    Ok(Json(out))
}
```

```rust
// routes, beside the quarantine ones
.route("/api/archives", get(api_archives))
.route("/api/extract", post(api_extract))
.route("/extract", get(extract_page_h))
```

> `state.quarantine_queue` — use whatever field name `AppState` already gives the queue; check the
> struct near the top of `src/web.rs`. Same for `err500`, `check_csrf` and `Json` imports, all of
> which already exist in this file.

- [ ] **Step 4: Run the tests**

Run: `cargo test --test browse_server`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/web.rs tests/browse_server.rs
git commit -m "feat(web): list archives with a live scope verdict and enqueue extractions"
```

---

### Task 9: The Extract page

Seventh page, same shell, no CDN, no build step. Table per connected drive: path, entries, size,
verdict. Per-row **Extract**, plus **Enqueue all in scope on this drive**. Reuses the existing
`/api/quarantine/status` poller for progress, because extraction results arrive on the same queue.

**Files:**
- Modify: `src/web_ui.rs`, `src/web.rs`
- Test: `tests/browse_server.rs`

**Interfaces:**
- Consumes: `GET /api/archives`, `POST /api/extract`, `GET /api/quarantine/status`.
- Produces: `pub fn extract_page(csrf: &str) -> String`, NAV key `"extract"` → `/extract`.

- [ ] **Step 1: Write the failing test**

```rust
// tests/browse_server.rs
#[test]
fn extract_page_renders_and_is_in_the_nav() {
    let addr = start_server();
    let body = get(addr, "/extract");
    assert!(body.contains("Extract"), "page title present");
    assert!(body.contains("href=\"/extract\""), "nav entry present");
    let overview = get(addr, "/");
    assert!(overview.contains("href=\"/extract\""), "nav is on every page");
}
```

- [ ] **Step 2: Run and watch it fail**

Run: `cargo test --test browse_server extract_page`
Expected: FAIL — 404.

- [ ] **Step 3: Implement**

```rust
// src/web_ui.rs — add to NAV, after "duplicates"
NavItem { key: "extract", href: "/extract", label: "Extract" },

// and to glyph()
"extract" => "unarchive",
```

```rust
// src/web_ui.rs
/// The Extract page: every catalogued archive, its scope verdict against the live mount, and the
/// two ways to act on it. A refused archive shows its reason in place — a refusal the user cannot
/// see is indistinguishable from a bug.
pub fn extract_page(csrf: &str) -> String {
    let main = r##"
<h1>Extract archives</h1>
<p class="sub">Extracted beside the archive, verified against the catalogue, then the original is
moved to <code>_ToDelete</code>. One archive at a time. An archive whose contents would not fit
within 260 characters is refused whole.</p>
<div id="queue" class="card" hidden></div>
<div id="drives"></div>
"##;

    let script = r##"
const CSRF = document.querySelector('meta[name=csrf]').content;
const fmt = b => b > 1<<30 ? (b/(1<<30)).toFixed(1)+' GiB'
              : b > 1<<20 ? (b/(1<<20)).toFixed(1)+' MiB' : b+' B';

async function load(){
  const vols = await (await fetch('/api/archives')).json();
  const host = document.getElementById('drives');
  host.innerHTML = '';
  for(const v of vols){
    const card = document.createElement('section');
    card.className = 'card';
    const inScope = v.archives.filter(a => a.in_scope === true);
    const bulk = v.connected
      ? `<button class="primary" data-vol="${v.volume_id}" ${inScope.length?'':'disabled'}>
           Enqueue all ${inScope.length} in scope</button>`
      : `<span class="muted">drive not connected — no verdict</span>`;
    card.innerHTML = `<header><h2>${v.label}</h2>${bulk}</header>`;
    const rows = v.archives.map(a => {
      const verdict = a.in_scope === null ? '<span class="muted">unknown</span>'
        : a.in_scope ? '<span class="ok">in scope</span>'
        : `<span class="warn" title="${(a.reason||'').replace(/"/g,'&quot;')}">refused</span>`;
      const btn = a.in_scope === true
        ? `<button data-vol="${v.volume_id}" data-path="${a.relative_path}">Extract</button>` : '';
      return `<tr><td>${a.relative_path}</td><td class="num">${a.entries}</td>
              <td class="num">${fmt(a.uncompressed_bytes)}</td><td>${verdict}</td><td>${btn}</td>
              </tr>${a.reason ? `<tr class="reason"><td colspan="5">${a.reason}</td></tr>` : ''}`;
    }).join('');
    card.insertAdjacentHTML('beforeend',
      `<table><thead><tr><th>Archive</th><th class="num">Entries</th><th class="num">Content</th>
       <th>Scope</th><th></th></tr></thead><tbody>${rows}</tbody></table>`);
    host.appendChild(card);
  }
  host.onclick = async e => {
    const b = e.target.closest('button'); if(!b) return;
    b.disabled = true;
    const paths = b.dataset.path ? [b.dataset.path]
      : vols.find(v => v.volume_id === b.dataset.vol).archives
            .filter(a => a.in_scope === true).map(a => a.relative_path);
    const r = await fetch('/api/extract', {method:'POST',
      headers:{'content-type':'application/json','x-csrf-token':CSRF},
      body: JSON.stringify({volume_id: b.dataset.vol, paths})});
    const out = await r.json();
    // Refusals are shown, never swallowed: a skipped archive that looks like a success would leave
    // the user believing content was unlocked when it was not.
    if(out.refusals && out.refusals.length){
      alert(out.refusals.map(x => `${x.path}: ${x.reason}`).join('\n'));
    }
    poll();
  };
}

async function poll(){
  const s = await (await fetch('/api/quarantine/status')).json();
  const q = document.getElementById('queue');
  const items = (s.running ? [s.running] : []).concat(s.pending);
  q.hidden = items.length === 0;
  q.innerHTML = items.length
    ? `<b>${items.length} queued</b> — running: ${s.running ? s.running.label : 'none'}`
    : '';
  const done = s.recent.filter(r => r.kind === 'extract');
  if(done.length){
    q.hidden = false;
    q.insertAdjacentHTML('beforeend', '<ul>' + done.slice(0,10).map(r =>
      r.error_message ? `<li class="warn">${r.label}: ${r.error_message}</li>`
                      : `<li class="ok">${r.label}: ${r.files_updated} entries → ${r.dest}</li>`
    ).join('') + '</ul>');
  }
}

load(); setInterval(poll, 1500);
"##;

    shell("extract", csrf, "Extract", main, script)
}
```

```rust
// src/web.rs
async fn extract_page_h(State(state): State<AppState>) -> Html<String> {
    Html(crate::web_ui::extract_page(&state.csrf_token))
}
```

> Match the surrounding pages: check how `review_page` reads the CSRF token (a `<meta>` tag, a
> data attribute, or an inlined constant) and use the same mechanism rather than the `<meta>` shown
> here. Same for the CSS class names — reuse what the shell already defines instead of inventing
> `.ok`/`.warn` if equivalents exist.

- [ ] **Step 4: Run the tests**

Run: `cargo test --test browse_server && cargo test --lib web_ui`
Expected: PASS. `web_ui.rs` has an HTML/JS lint test that runs `node` when available — make sure it
still passes rather than skipping.

- [ ] **Step 5: Manual check**

Run: `cargo run -- browse`, open `/extract`, confirm a connected drive lists archives with verdicts
and a disconnected one says "drive not connected — no verdict".

- [ ] **Step 6: Commit**

```bash
git add src/web_ui.rs src/web.rs tests/browse_server.rs
git commit -m "feat(web): add the Extract page with per-archive and bulk enqueue"
```

---

### Task 10: `.7z` scanner descent

Descent lands **before** extraction: extraction verifies against catalogued entry hashes, and today
there are none for any `.7z`. Extracting one before a rescan would be an unverifiable write followed
by quarantining the only copy.

**Files:**
- Modify: `Cargo.toml`, `src/archive.rs`, `src/config.rs` (allow-list default)
- Test: inline in `src/archive.rs`, plus `tests/archive_scan.rs`

**Interfaces:**
- Produces: `pub fn looks_like_7z(prefix: &[u8]) -> bool`, and `scan_archive` transparently handling
  a 7z reader.

- [ ] **Step 1: Add the dependency and spike the API**

The spec names `sevenz-rust`; use **`sevenz-rust2`** instead — it is the maintained fork
(0.22.x vs 0.6.1, last released 2023). Note the substitution in the spec update in Task 12.

```toml
# Cargo.toml
sevenz-rust2 = { version = "0.22", features = ["compress"] }
```

`compress` is needed by the tests, to build a `.7z` fixture without shelling out to a 7-Zip binary
that CI does not have.

Write a throwaway spike first — `cargo test --lib archive::tests::sevenz_spike` — that creates a
`.7z` in a temp dir and reads its entries back, to pin down the real API names before writing the
implementation against a guess. Delete the spike once Step 3 passes.

- [ ] **Step 2: Write the failing tests**

```rust
// src/archive.rs — mod tests
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
    let out = scan_archive(std::io::BufReader::new(f), &test_limits());

    assert!(out.errors.is_empty(), "{:?}", out.errors);
    let mut chains: Vec<&str> = out.entries.iter().map(|e| e.container_chain.as_str()).collect();
    chains.sort();
    assert_eq!(chains, vec!["a.txt", "sub/b.txt"]);
    let a = out.entries.iter().find(|e| e.container_chain == "a.txt").unwrap();
    assert_eq!(a.content_hash, blake3::hash(b"AAA").to_hex().to_string());
    assert_eq!(a.size_bytes, 3);
}
```

```rust
// tests/archive_scan.rs — end to end through the CLI, mirroring scans_archive_and_finds_inner_file
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
```

`write_7z` is the same shape as this file's existing `write_zip`, built with `sevenz-rust2`'s
compressor (the `compress` feature added in Step 1). Pin its exact API with the spike before
writing it.

- [ ] **Step 3: Implement**

Dispatch inside `scan_level` on the *content* signature, not the extension — matching the existing
treatment of zips whose extension lies. `looks_like_7z` checks the 6-byte magic `37 7A BC AF 27 1C`.
The 7z branch iterates entries, stream-hashes each with the existing `hash_capped`, applies the same
`ratio_cap` and `entry_max_bytes` guards, and recurses into nested archives through the same
`budget`. Add `7z` to `config::DEFAULT_ALLOW` (check the constant's real name in `src/config.rs`)
so descent is on by default and still user-editable on the Scan page.

- [ ] **Step 4: Run the tests**

Run: `cargo test --lib archive:: && cargo test --test archive_scan`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add Cargo.toml Cargo.lock src/archive.rs src/config.rs tests/archive_scan.rs
git commit -m "feat(scanner): descend into .7z archives and catalogue their entries"
```

---

### Task 11: `.7z` extraction

**Files:**
- Modify: `src/extract.rs`
- Test: inline

**Interfaces:**
- Consumes: Task 10's reader; `write_level` becomes format-dispatching.

- [ ] **Step 1: Write the failing test**

```rust
// src/extract.rs — mod tests
#[test]
fn a_7z_extracts_and_verifies_against_its_catalogued_hashes() {
    let tmp = tempfile::tempdir().unwrap();
    let (cat, _) = real_scan_fixture_7z(&tmp, &[("a.txt", b"AAA")]);

    let out = extract_archive(&cat, tmp.path(), "vol-1", "bundle.7z", &test_limits(), 100).unwrap();

    assert_eq!(out.entries_converted, 1);
    assert_eq!(std::fs::read(tmp.path().join("bundle/a.txt")).unwrap(), b"AAA");
    assert!(tmp.path().join("_ToDelete/bundle.7z").is_file());
}
```

- [ ] **Step 2: Run and watch it fail**

Run: `cargo test --lib extract::tests::a_7z`
Expected: FAIL — the zip reader rejects a 7z header.

- [ ] **Step 3: Implement**

Rename the zip body to `write_zip_level`, add `write_7z_level` with the same contract (same
zip-slip guard on entry names, same `entry_max_bytes` and `ratio_cap` checks, same returned
`Vec<String>` of destination-relative paths in archive order), and make `write_level` read the
first 8 bytes and dispatch on the signature — the same content-based decision the scanner makes.

- [ ] **Step 4: Run the tests**

Run: `cargo test --lib extract::`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add src/extract.rs
git commit -m "feat(extract): extract .7z archives through the same verified path"
```

---

### Task 12: End-to-end test, docs, and the honest caveats

**Files:**
- Create: `tests/extract_flow.rs`
- Modify: `docs/superpowers/specs/2026-08-26-archive-extraction-design.md`,
  `docs/TESTING-GUIDE.md`, `posts/POST-MATERIAL.md`

- [ ] **Step 1: Write the end-to-end test**

```rust
// tests/extract_flow.rs
// Same harness as tests/repack_flow.rs: a real binary, a real drive, a real catalogue.
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

/// Wait for an observable end state, not for the queue to *look* idle: the queue reports empty in
/// the window between finishing one job and enqueueing the nested archive it produced. Commit
/// 940750a fixed exactly this race in the quarantine tests — do not reintroduce it.
fn wait_for(path: &std::path::Path) {
    for _ in 0..600 {
        if path.exists() {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    panic!("timed out waiting for {}", path.display());
}

#[test]
fn extract_unlocks_the_content_recurses_and_quarantines_each_archive() {
    let tmp = tempfile::tempdir().unwrap();
    let drive = tmp.path().join("drive");
    std::fs::create_dir_all(drive.join("sub")).unwrap();

    // inner.zip goes INSIDE bundle.zip, so the catalogue holds a two-segment chain.
    let inner = tmp.path().join("inner.zip");
    write_zip(&inner, &[("deep.txt", b"DEEP")]);
    let inner_bytes = std::fs::read(&inner).unwrap();
    write_zip(
        &drive.join("bundle.zip"),
        &[("a.txt", b"AAA"), ("sub/inner.zip", &inner_bytes)],
    );
    // A loose twin, so the unlocked content is visibly a duplicate afterwards.
    std::fs::write(drive.join("loose_a.txt"), b"AAA").unwrap();

    let data = tmp.path().join("appdata");
    let scan = bin()
        .env("CLEANUPSTORAGES_DATA_DIR", &data)
        .args(["scan"])
        .arg(&drive)
        .args(["--readonly-fallback", "fingerprint"])
        .output()
        .unwrap();
    assert!(
        scan.status.success(),
        "scan failed: {}",
        String::from_utf8_lossy(&scan.stderr)
    );

    // Start the server against the same data dir and POST the extraction. Reuse the request
    // helpers from tests/browse_server.rs (copy them in; the test crates are independent).
    let addr = start_server_for(&data);
    let csrf = csrf_token(addr);
    post_json(
        addr,
        "/api/extract",
        &csrf,
        &format!(
            r#"{{"volume_id":"{}","paths":["bundle.zip"]}}"#,
            volume_id_of(&data)
        ),
    );

    // The outer archive.
    wait_for(&drive.join("bundle/a.txt"));
    assert_eq!(std::fs::read(drive.join("bundle/a.txt")).unwrap(), b"AAA");
    assert!(
        drive.join("_ToDelete/bundle.zip").is_file(),
        "the original is quarantined, never deleted"
    );
    assert!(!drive.join("bundle.zip").is_file(), "original moved out of the way");

    // The nested one, enqueued by the first job and extracted by the same worker.
    wait_for(&drive.join("bundle/sub/inner/deep.txt"));
    assert_eq!(
        std::fs::read(drive.join("bundle/sub/inner/deep.txt")).unwrap(),
        b"DEEP"
    );
    assert!(drive.join("_ToDelete/bundle/sub/inner.zip").is_file());

    // The catalogue now describes loose files, not archive entries.
    let search = bin()
        .env("CLEANUPSTORAGES_DATA_DIR", &data)
        .args(["search", "a.txt"])
        .output()
        .unwrap();
    let out = String::from_utf8_lossy(&search.stdout).to_string();
    assert!(out.contains("bundle/a.txt"), "extracted file is catalogued loose: {out}");
    assert!(
        !out.contains("bundle.zip ›"),
        "no row may still point inside the archive that just moved: {out}"
    );
}
```

> `start_server_for`, `csrf_token`, `post_json` and `volume_id_of` do not exist yet. Copy the
> server-start and raw-HTTP helpers from `tests/browse_server.rs` and adapt them to take an existing
> data dir; `volume_id_of` reads the single volume out of the catalogue the scan just wrote. If
> starting the server against a CLI-created data dir turns out to need a flag the binary does not
> have, say so rather than weakening the test to a unit test — the point of this file is that the
> queue, the API and the engine are exercised together.

> Poll for the *observable end state* — the extracted file existing — not for the queue looking
> idle. `940750a` fixed exactly this race in the quarantine tests; do not reintroduce it.

- [ ] **Step 2: Run it**

Run: `cargo test --test extract_flow -- --nocapture`
Expected: PASS.

- [ ] **Step 3: Run everything**

Run: `cargo test && cargo clippy --all-targets -- -D warnings && cargo fmt --check`
Expected: all PASS. Fix anything that does not.

- [ ] **Step 4: Update the spec**

In `docs/superpowers/specs/2026-08-26-archive-extraction-design.md`:
- Status: `proposed` → `accepted, implemented in #77`.
- Replace step 10's description with the nesting decision recorded at the top of this plan (rows are
  re-pointed at the extracted inner archive, not left dangling; verification proves a nested archive
  through its contents).
- Note the dependency substitution: `sevenz-rust2` 0.22, not `sevenz-rust` 0.6.1, because the
  latter's last release was 2023.

- [ ] **Step 5: Update the testing guide**

Add an "Extract an archive" section to `docs/TESTING-GUIDE.md` against the sandbox from
`scripts/make-test-sandbox.ps1`: scan, open `/extract`, extract one archive, confirm the extracted
folder, confirm `_ToDelete` holds the original, confirm the Duplicates page now offers the unlocked
content.

- [ ] **Step 6: Append the build-in-public material line**

One line to `posts/POST-MATERIAL.md` with the real figures **from a real run** — the archives
extracted, the bytes unlocked, and the honest beat. Do not write a number that no run produced. The
candidate beat: #72 measured this as a net loss of ~385 GB and said no; the same measurement eight
days later came out a net gain of ~614 GiB, because the user had been editing the drives by hand in
between — the verdict was never about the algorithm.

- [ ] **Step 7: Commit and open the PR**

```bash
git add tests/extract_flow.rs docs/ posts/POST-MATERIAL.md
git commit -m "test(extract): prove the whole path end to end, and document it"
gh pr create --fill --base main
```

---

## Verification checklist

Before calling this done, each of the spec's twelve test requirements must map to a passing test:

| Spec test | Where |
| --- | --- |
| 1. Small zip extracts, verifies, original in `_ToDelete` | Task 6, Task 12 |
| 2. Over-long entry refuses the whole archive | Task 2 |
| 3. Budget computed from the real mount root | Task 1 |
| 4. Hash mismatch fails, destination deleted, original left | Task 6 |
| 5. Existing destination refuses, folder untouched | Task 2 |
| 6. Insufficient free space refuses before writing | Task 2 |
| 7. Zip inside a zip is extracted too | Task 6, Task 12 |
| 8. `max_archive_depth` stops recursion, reported not silent | Task 7 |
| 9. Rows converted in place — `id`, `content_hash` survive | Task 5 |
| 10. Deny-listed `.docx` never extracted | Task 6 |
| 11. `.7z` descended into, hashes correct | Task 10 |
| 12. `.7z` extracts and verifies | Task 11 |
