# Archive extraction — design

**Status:** proposed
**Date:** 2026-08-26
**Supersedes the verdict in:** #72 (*"costs more space than it recovers"* — no longer true; see below)
**Related:** #19 (cross-drive scratch for repack), #27 (review at scale)

## Why

912.2 GiB of duplicated content is locked inside archives (622.6 GiB on `E:`, 289.6 GiB on `D:`).
Quarantine cannot touch it — quarantine is a rename, and you cannot rename one entry out of a zip.
Only two things reach it: repack (Case 4, already built, removes an entry in place) and extraction.

**Issue #72 measured extraction on 2026-08-18 and said no.** That verdict is re-measured here on
2026-08-26 and it has flipped:

| | #72 (18 Aug) | today (26 Aug) |
| --- | ---: | ---: |
| expansion cost | +682.7 GiB (`E:` alone) | **+184.0 GiB** (`E:`), +114.5 GiB (`D:`) |
| duplicates unlocked | 297 GB | **912.2 GiB** |
| fits in free space? | no (2053.8 needed, 1983.8 free) | **yes** (1159.2 needed, 2052 free) |
| verdict | net **loss** ~385 GB | net **gain** ~614 GiB |

The drives changed between those readings because the user edits them by hand outside the tool.
Numbers in this document are dated for the same reason, and must be re-measured before execution
rather than trusted.

**#72's other objection survives intact, and is now the binding constraint.** Extracted entry paths
routinely exceed Windows' 260-character limit. The longest `container_chain` **alone** is 485
characters, before any drive letter or parent folder. Rust can write such a path through the `\\?\`
prefix, but Explorer and most applications then cannot open it — an unacceptable outcome for data
whose entire purpose is being reachable.

So extraction is built for the subset that fits, and only that subset:

| | archives | content |
| --- | ---: | ---: |
| **in scope** — every entry lands within 260 chars | **1,512** | **552.6 GiB** |
| out of scope — at least one entry would not | 294 | 1036.2 GiB |

Split by drive, in scope: `E:` 922 archives / 192.1 GiB, `D:` 590 archives / 360.6 GiB.

The 294 out-of-scope archives are the whales — the 180 GiB, 147 GiB and 90 GiB course zips. The
user extracts and relocates those by hand, having accepted that explicitly. This design does not
attempt them and must not silently try.

## Decisions

| Decision | Choice | Why |
| --- | --- | --- |
| Scope | **Only archives where *every* entry fits within 260 characters** | Half-extracting an archive means the original cannot be quarantined (it still holds content nothing else has), so the unit of work is the whole archive or nothing |
| Destination | **A sibling folder beside the archive**, named after the archive's stem | Chosen over a short drive-root folder. It preserves the original folder context completely; it costs reach (1,512 archives / 552.6 GiB instead of 1,765 / 742.8 GiB) and that trade was made deliberately |
| Path budget | **Computed against the real mount root at runtime, per entry** | Several in-scope archives sit at 258 characters — two from failure. An assumed 3-character root (`E:\`) is a guess, and a drive mounted anywhere else silently invalidates the whole safety check |
| Order | **One archive at a time** | A user requirement. Also what keeps the transient space cost bounded to a single archive (max 32.04 GiB in scope; median 1.9 MiB) |
| Free space | **Preflight before each archive; refuse rather than fill the drive** | Requires `uncompressed_size + 5 GiB` free. Refusing an item is recoverable; filling a drive holding irreplaceable data is not |
| Verification | **Re-hash every extracted file against the entry hash already in the catalogue, before the original is quarantined** | The catalogue already holds a BLAKE3 for every entry. Extraction that cannot be proven correct must not be followed by removing the only other copy |
| Failure | **Delete the partial extraction, leave the archive untouched** | The archive is the source of truth until the extraction is proven. A failed attempt must leave the drive exactly as it was |
| Original archive | **Quarantined to `_ToDelete`, never deleted** | The project's standing rule. It is also the second recovery net if verification was somehow wrong |
| Catalogue update | **Convert each entry row in place** (`relative_path` := extracted path, `container_chain` := NULL) | Preserves `id`, `content_hash` and `first_seen_at`, so history survives and no rescan is needed to make the extracted files reviewable. Deleting and re-inserting would orphan every reference |
| Collision | **Refuse the archive if the destination folder already exists** | `idx_files_loose_identity` is unique on `(volume_id, relative_path)` for loose rows. Merging into an existing folder risks overwriting a file that is not a copy |
| Nesting | **Recurse until no archive remains**, bounded by `max_archive_depth` (currently 8) | The user's choice. The catalogue holds archives 5 deep. The path-budget check already accounts for the fully-recursive layout, because `container_chain` describes it |
| Worker | **A third job kind on the existing serial quarantine worker** | Extraction ends by quarantining the original and by rewriting catalogue rows. Running it beside the quarantine worker would put two writers in a race that each job's pre-move `active` check cannot survive |
| `.7z` | **Scanner descent *and* extraction, via `sevenz-rust`** | User's explicit choice, made knowing the payoff is ~400 MB across 5 files. Descent must land first: extraction verifies against catalogued entry hashes, and today **zero** `.7z` contents are catalogued |
| Deny list | **Reused unchanged** from `config::DEFAULT_DENY` + `settings.json` | `docx`, `xlsx`, `jar`, `apk`, `epub`, `ipa` and the rest are zip-format *documents*. Exploding one destroys it. That list already exists and is user-editable on the Scan page |

## Architecture

Extraction is a third `Job` variant on the queue built in the single-file-quarantine design, not a
new worker.

```
Extract page ──POST /api/extract──▶ enqueue Job::Extract { volume_id, path }
                                              │
                                    QuarantineQueue (serial, one at a time)
                                              │
                          ┌───────────────────┼───────────────────┐
                     Job::Tree           Job::Files          Job::Extract
                                                                  │
                                                                  ▼
                                                     extract::extract_archive
```

`src/extract.rs` is new and owns the whole per-archive operation. It is a deep module: the caller
hands it a volume, a path and a mount, and gets back a result. Every decision about path budgets,
verification, ordering and rollback lives inside it.

### The per-archive algorithm

Each numbered step must complete before the next begins. The archive is untouched until step 7.

1. **Resolve** the mount and confirm the drive marker matches the expected `volume_id`.
2. **Load** every catalogued entry for this archive: `container_chain`, `content_hash`, `size_bytes`.
3. **Path preflight.** For each entry, compute the full destination path from the *actual* mount
   root. If any exceeds 260 characters, refuse the whole archive and name the offending entry.
4. **Collision preflight.** If the destination folder exists, refuse and name it.
5. **Space preflight.** If free space on the volume is below `sum(size_bytes) + 5 GiB`, refuse and
   report both numbers.
6. **Extract** every entry to the destination folder, honouring `ArchiveLimits` (`entry_max_bytes`,
   `ratio_cap`, `max_depth`) exactly as the scanner does.
7. **Verify.** Re-hash every extracted file with BLAKE3 and compare against the catalogued
   `content_hash`. Any mismatch, or any missing file, fails the archive.
8. **On failure:** delete the destination folder and stop. The archive and the catalogue are
   unchanged. Report why.
9. **On success:** in one transaction, rewrite each entry row (`relative_path` := extracted relative
   path, `container_chain` := NULL). Then quarantine the archive root row through the existing
   `quarantine::quarantine_files`, so the marker check, rename-only rule and action log all apply
   unchanged.
10. **Recurse.** Any extracted file that is itself an in-scope archive is enqueued as a new
    `Job::Extract`, up to `max_archive_depth`.

Step 7 is the one that makes step 9 safe. Without it, quarantining the original removes the only
proven copy of the content.

### `.7z` support

Two changes, in this order:

1. **Scanner descent.** `src/archive.rs` gains a 7z reader alongside the zip one, so `.7z` contents
   are hashed and catalogued like any other entries. Detection stays content-based, matching the
   existing treatment of zips whose extension lies.
2. **Extraction.** `src/extract.rs` dispatches on the detected format.

Descent must ship first. Extraction verifies against catalogued entry hashes, and there are
currently none for any `.7z` — so extracting one before a rescan would be an unverifiable write
followed by quarantining the only copy. That is precisely the sequence the reliability constraint
forbids.

The honest accounting: this is a new dependency and a second format path through the scanner, for
5 files totalling ~400 MB. It is being built because it was asked for, not because the arithmetic
justifies it.

## Error handling

| Case | Behaviour |
| --- | --- |
| Any entry would exceed 260 chars | Archive refused before anything is written; the offending entry is named |
| Destination folder exists | Refused; the folder is named. Never merged into |
| Insufficient free space | Refused; required and available are both reported |
| Drive disconnected mid-extraction | Extraction fails; partial destination deleted on the next attempt via the collision check, which refuses until the user clears it |
| Hash mismatch on any entry | Whole archive fails, destination deleted, archive untouched |
| Entry exceeds `entry_max_bytes` or `ratio_cap` | Archive refused, limits reported. Same guards the scanner uses |
| Unsupported format behind a lying extension | Recorded in `pending_archive_formats`, exactly as the scanner does today |
| Catalogue transaction fails after files are on disk | Destination deleted, archive untouched. Disk and catalogue must never disagree |

Every refusal is reported with its reason. A skipped archive that looks like a success would leave
the user believing content was unlocked when it was not.

## Testing

1. A small zip extracts, verifies, and its original lands in `_ToDelete` — contents byte-identical.
2. An archive with one entry whose path would exceed 260 chars is refused **whole**; nothing is
   written; the archive stays put.
3. Path budget is computed from the real mount root: the same archive passes under `E:\` and is
   refused under a deeply-nested mount.
4. A corrupted entry (hash mismatch) fails the archive, deletes the destination, leaves the original.
5. An existing destination folder causes a refusal, and the existing folder is not modified.
6. Insufficient free space refuses before writing a byte.
7. A zip inside a zip is extracted, then its inner archive is enqueued and extracted too.
8. `max_archive_depth` stops recursion, and the stopping point is reported rather than silent.
9. Catalogue rows are converted in place: the entry's `id` and `content_hash` survive, and
   `container_chain` becomes NULL.
10. A deny-listed extension (`.docx`) is never extracted, even when its content is a valid zip.
11. A `.7z` is descended into by the scanner and its entries appear with correct hashes.
12. A `.7z` extracts and verifies against those hashes.

## Out of scope

- The 294 over-long archives (1036.2 GiB). Manual, by explicit decision.
- Any rule that skips low-value archives. Noted for the record: the largest in-scope archive is
  `PMW/ELEDIA-VM.SIMULATORS.HFSS-2021R1.zip` — 32.04 GiB in **2 entries**, a VM disk image — and
  `Debian10-005.zip` is 8.01 GiB in 2 entries. Extracting these yields large blobs and no dedup
  benefit. They are extracted anyway, because "every archive" was the instruction; the Extract page
  shows entry counts so they can be skipped by hand.
- Re-compressing anything. Extraction is one-way here; repack (Case 4) remains the in-place tool.
- Cross-drive extraction (#19 covers scratch space on near-full drives).
