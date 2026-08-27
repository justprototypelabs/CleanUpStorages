# Hands-on testing guide (safe sandbox)

This walks you through **every feature** of CleanUpStorages on a disposable test folder, so you can
try it end-to-end without touching any real data.

## The one safety rule

Two things keep this 100% safe:

1. **A throwaway sandbox.** All test files live under `Documents\cleanup-sandbox\` — junk we generate.
   The tool only ever writes a tiny hidden `.cleanupstorages_id` marker into `DriveA`/`DriveB`, moves
   confirmed duplicates into a same-drive `_ToDelete` folder (reversible), and only deletes when *you*
   run `purge` — and even then only inside the sandbox.
2. **An isolated catalog.** We point the catalog at `cleanup-sandbox\catalog` via the
   `CLEANUPSTORAGES_DATA_DIR` environment variable, so your real catalog (if any) is never touched.

> Nothing in this guide reads, moves, or deletes anything outside `Documents\cleanup-sandbox\`.

---

## 0. Build + create the sandbox

From the repo root (`Documents\Home\CleanUpStorages`), in **PowerShell**:

```powershell
# Build the release binary
cargo build --release

# Create (or re-create) the test sandbox: two "drives" with duplicates, photos, and archives
.\scripts\make-test-sandbox.ps1

# Point the catalog at the sandbox so nothing real is touched (this shell only)
$env:CLEANUPSTORAGES_DATA_DIR = "$env:USERPROFILE\Documents\cleanup-sandbox\catalog"

# A short alias for the binary
Set-Alias cus ".\target\release\cleanupstorages.exe"
```

> Re-run `.\scripts\make-test-sandbox.ps1` any time to reset to a pristine sandbox (it wipes and rebuilds
> the folder, removing markers and the catalog).

What the sandbox contains:

```
cleanup-sandbox\
  catalog\                       <- isolated catalog (CLEANUPSTORAGES_DATA_DIR)
  DriveA\
    notes.txt                    unique
    report.txt                   ] four identical copies -> a duplicate group
    copy\report.txt              ]   (spanning both drives AND inside the zip)
    photos\sunset.png            ] two identical photos -> a duplicate group
    photos\sunset_copy.png       ]   (real 64x64 images, so thumbnails render)
    bundle.zip                   contains keep.txt + report.txt (= the shared report)
    nested.zip                   contains inner.zip which contains deep.txt
  DriveB\
    report.txt                   the 4th copy of the shared report (a survivor on another drive)
    misc.txt                     unique
```

---

## 1. Scan the two drives

```powershell
cus scan "$env:USERPROFILE\Documents\cleanup-sandbox\DriveA"
cus scan "$env:USERPROFILE\Documents\cleanup-sandbox\DriveB"
```

✅ **Expect:** DriveA reports `7 hashed … 4 archive entries` (the 4 = `bundle.zip`'s two entries + `nested.zip`'s
`inner.zip` and `inner.zip › deep.txt` — recursive archive scanning). DriveB reports `2 hashed`. Each scan also
prints where it saved a **catalog snapshot** (auto-backup).

Try an incremental re-scan — it should say everything is `unchanged`:

```powershell
cus scan "$env:USERPROFILE\Documents\cleanup-sandbox\DriveA"     # Done: 0 hashed, 11 unchanged, ...
```

---

## 2. Inventory: status, search, duplicates

```powershell
cus status
```
✅ **Expect:** `Duplicate groups (same content hash): 2`, and a per-volume line for DriveA and DriveB with a
`recoverable: 0 MiB in _ToDelete` figure (nothing quarantined yet).

```powershell
cus search report                # find every "report" file, even inside the zip
cus search sunset --category photo
```
✅ **Expect:** `report` matches the loose copies on both drives **and** `bundle.zip › report.txt` (search sees
inside archives). Drives that aren't plugged in would still show here — the catalog persists.

```powershell
cus duplicates
```
✅ **Expect:** two groups — the 2 photos, and the **4 report copies** (one is `bundle.zip › report.txt`, one is
on DriveB). **Note the `#id` numbers** — you'll use them below. (Ids differ each fresh scan.)

---

## 3. The web UI (the visual experience)

```powershell
cus browse
```
This prints a `http://127.0.0.1:PORT` URL and opens your browser. Leave it running (Ctrl+C to stop). In another
PowerShell window you can watch the request log if you started it with `RUST_LOG=info` (see step 7).

The UI opens on the **Overview** dashboard; the left sidebar switches between the six pages
(Overview, Browse, Duplicates, Drives, Scan, Console).

**New in the visual overhaul — worth a look:**
- **Theme toggle** (sidebar footer): **Auto / Light / Dark**. Auto follows Windows; Light/Dark force it
  and your choice is remembered. Check that the **dropdown menus** on Browse now match the theme
  (previously they showed light popups in dark mode).
- **Scan page:** while a scan runs you get a progress bar + live **count tiles** (Hashed / Unchanged /
  Errors / Archive entries) and a recent-scans list with done/error pills; the "force full rescan" is a
  toggle switch.
- **Drives page:** **Edit…** now opens an inline form (name + description, Save/Cancel) instead of
  browser prompts. (Your scanned folder-drives should also show **connected**, not offline.)

Try each:

- **Overview (home `/`):** the dashboard — total files catalogued across N drives, the duplicate-groups
  count with a "Review duplicates" button, a reclaimable-space bar per drive, and a **Recent activity**
  feed (your scans/quarantines/purges show up here as you do them).
- **Browse:** type in the search box — results filter live. Filter by drive/type/status. Note that a
  file inside the zip shows its location as `bundle.zip › report.txt`.
- **Drives:** one card per catalogued drive with a real capacity bar (used/total), last-scan time, and
  reclaimable figure. Buttons: **Rescan** (re-queues a scan — labelled "Repair (rescan)" if the drive had
  scan errors), **Forget…** (removes the drive from the *catalog only* — a confirm dialog spells out that
  files on the drive are **not** deleted; rescan re-adds it), and a global **Purge all quarantines**
  (confirm-gated; the only real delete). Try **Forget** on one drive, then re-scan it from the Scan page —
  it comes back.
- **Console:** a terminal-style panel that runs the app's own commands (only those — it's not a shell).
  Type `help`, then e.g. `status`, `search report`, `drives`, `duplicates`. Output prints as JSON scrollback.
- **Scan a drive:** this is the UI scanner.
  - Your two drives appear under **Detected drives** (click one to fill the path), or use **Browse…** for the
    native folder picker, or paste a path.
  - Click **Scan** and watch the **live counts** tick up; the **Recent scans** list shows the result. (Try
    scanning `DriveA` again — you'll see it complete near-instantly, all unchanged.)
- **Review duplicates →:** steps through each duplicate group Tinder-style.
  - The **photos** group shows **thumbnails** side by side. The suggested "keep" is highlighted; click another
    card to change which one you keep.
  - Click **Keep selected, quarantine the rest** — the other copy moves to `_ToDelete` (reversible). The group
    advances.
  - For the **report** group, the loose copies show a **Remove?**-style action; the one inside `bundle.zip`
    shows **Remove from archive** (that's the Case-4 repack — it rebuilds the zip without that entry, keeping a
    recovery copy). Try it on the archived copy.

✅ **Expect:** after quarantining, the survivor still exists on disk; the removed file now lives under
`DriveA\_ToDelete\...`. Nothing is gone for good yet.

Stop the server with **Ctrl+C** when done.

---

## 4. Quarantine + reclaim from the CLI

Same actions, from the terminal. Pick a duplicate `#id` from `cus duplicates` (a **loose** one — e.g. a
`copy/report.txt` id), then:

```powershell
# Move that copy to DriveA's _ToDelete (reversible; a same-drive rename, zero extra space)
cus quarantine "$env:USERPROFILE\Documents\cleanup-sandbox\DriveA" <id>

cus status        # DriveA now shows some "recoverable" MiB in _ToDelete
```
✅ **Expect:** the file is now under `DriveA\_ToDelete\...` (open the folder to confirm), and `status` shows it
as reclaimable. It is **not deleted** — you could move it back by hand.

Safety demo — try to quarantine **every** remaining copy at once (pass several ids): the tool **refuses to
remove the last surviving copy** and skips it. You can never end up with zero copies.

When you're happy, reclaim the space (**the only real delete — and only inside the sandbox**):

```powershell
cus purge "$env:USERPROFILE\Documents\cleanup-sandbox\DriveA"
```
✅ **Expect:** `DriveA\_ToDelete\` is emptied/removed; `status` recoverable drops to 0.

The Drives page's buttons have CLI equivalents too:

```powershell
cus purge --all                                                   # purge every connected drive's _ToDelete at once
cus forget "$env:USERPROFILE\Documents\cleanup-sandbox\DriveB"    # drop DriveB from the catalog (files on disk kept)
cus status                                                        # DriveB no longer listed…
cus scan "$env:USERPROFILE\Documents\cleanup-sandbox\DriveB"      # …until you rescan, which re-adds it
```
✅ **Expect:** `forget` removes the volume's catalog entries only — the files under `DriveB\` are untouched, and
a rescan brings the drive back. `purge --all` reports which drives it purged and skips any not connected.

---

## 5. Repack an in-zip duplicate from the CLI (Case 4)

If you didn't do it in the UI: find the `#id` of `bundle.zip › report.txt` from `cus duplicates` (it must
still have a surviving copy elsewhere — it does, on DriveB), then:

```powershell
cus repack "$env:USERPROFILE\Documents\cleanup-sandbox\DriveA" <id>
```
✅ **Expect:** `bundle.zip` is rebuilt **without** `report.txt` but still containing `keep.txt`; the removed
item and the **original archive** are both saved under `_ToDelete` (two recovery nets). Verify:

```powershell
Add-Type -AssemblyName System.IO.Compression.FileSystem
[System.IO.Compression.ZipFile]::OpenRead("$env:USERPROFILE\Documents\cleanup-sandbox\DriveA\bundle.zip").Entries.FullName
# -> should list keep.txt, and NOT report.txt
```

---

## 6. Extract an archive (unlock archived duplicates)

`bundle.zip` on DriveA holds `keep.txt` and `report.txt` — and `report.txt` is one of the 4-copy
duplicate group, but locked: quarantine can't touch an entry inside a zip, only a loose file. This
is what extraction is for.

```powershell
cus browse
```

Open the **Extract** page from the sidebar.

- **Expect:** `bundle.zip` and `nested.zip` are both listed, each showing entry counts and a scope
  verdict (in scope / out of scope, based on whether every extracted path would fit under 260
  characters on this drive).
- Click **Extract** on `bundle.zip`. It queues immediately and runs in the background — the row
  shows queued → running → done without blocking the page.

✅ **Expect**, once it finishes:

```powershell
Get-ChildItem "$env:USERPROFILE\Documents\cleanup-sandbox\DriveA\bundle"
# -> keep.txt, report.txt
Get-ChildItem "$env:USERPROFILE\Documents\cleanup-sandbox\DriveA\_ToDelete"
# -> bundle.zip (the original, quarantined, never deleted)
```

`DriveA\bundle.zip` itself is gone from its old spot (moved, not deleted) and `DriveA\bundle\` now
holds its contents as ordinary loose files.

Go to the **Duplicates** page and find the report group again: the entry that used to read
`bundle.zip › report.txt` with a **Remove from archive** action now shows as a plain loose file
(`bundle/report.txt`) with the ordinary **Remove?** quarantine action — extraction unlocked it, so
it no longer needs a repack to reclaim.

Try `nested.zip` too, if you want to see recursion: it contains `inner.zip`, which contains
`deep.txt`. Extracting `nested.zip` unlocks `inner.zip` as a loose file, which is itself an in-scope
archive — the queue enqueues it automatically, and it extracts in turn without you clicking
anything a second time. End state: `DriveA\nested\inner\deep.txt` on disk, and **two** archives
(`nested.zip` and `nested/inner.zip`) both sitting in `_ToDelete`.

---

## 7. Reliability spot-checks (optional)

- **Rename recognition:** rename `DriveA` to `DriveA-moved` in Explorer, then
  `cus scan "...\DriveA-moved"` — the tool reads the hidden marker and knows it's the **same volume** (not a new
  one), so it re-uses the existing catalog entries.
- **Missing files:** delete `DriveA\notes.txt`, re-scan DriveA — `cus search notes` shows it flagged
  `[MISSING]` (the catalog never forgets where a file was).
- **Corrupt-catalog guard:** the tool refuses to scan/act on a catalog that fails its integrity check and
  points you to the latest snapshot in `catalog\catalog.backups\`.

---

## 8. Observability (see the UI↔API connection)

```powershell
$env:RUST_LOG = "info"
cus browse
```
Now every click in the browser prints a line to this terminal, e.g.:
```
INFO request{id=42 method=POST uri=/api/quarantine}: finished processing request status=200 latency=42ms
```
Failures are logged too (a rejected request logs a `WARN` with the same `id=`). For more detail use
`$env:RUST_LOG = "debug"` or the global `-v` flag on any CLI command:
```powershell
cus -v scan "$env:USERPROFILE\Documents\cleanup-sandbox\DriveA"
```
Reset it with `Remove-Item Env:RUST_LOG` when done.

---

## 9. Clean up

Everything was disposable. To remove all trace:

```powershell
Remove-Item -Recurse -Force "$env:USERPROFILE\Documents\cleanup-sandbox"
Remove-Item Env:CLEANUPSTORAGES_DATA_DIR -ErrorAction SilentlyContinue
```

That deletes the test files, the isolated catalog, and its snapshots. Your real data was never involved.

---

## Feature checklist

| Feature | Where | ✅ |
| --- | --- | --- |
| Scan a drive (CLI) | §1 | |
| Incremental re-scan (skips unchanged) | §1 | |
| Recursive archive scanning (nested zip) | §1 (4 archive entries) | |
| Status / per-volume totals / recoverable space | §2, §4 | |
| Search (incl. inside archives, offline drives) | §2 | |
| List duplicate groups | §2 | |
| Web browse + live search + filters | §3 | |
| Web scan (detected drives + folder picker + live progress) | §3 | |
| Review GUI: thumbnails + keep/quarantine | §3 | |
| Overview dashboard (stats + reclaimable + activity feed) | §3 | |
| Drives page (capacity, rescan, forget, purge-all) | §3 | |
| Console (client-side command REPL) | §3 | |
| Forget a drive (catalog-only; files kept) | §3, §4 | |
| Purge all quarantines at once | §3, §4 | |
| Quarantine (reversible) | §3, §4 | |
| Never-remove-last-copy guard | §4 | |
| Purge (reclaim space) | §4 | |
| Repack an in-zip duplicate (Case 4) | §3, §5 | |
| Cross-drive / archived duplicate detection | §2 (4-copy group) | |
| Extract an archive, incl. nested recursion | §6 | |
| Volume re-recognition after rename | §7 | |
| Missing-file tracking | §7 | |
| Catalog snapshots / integrity guard | §1, §7 | |
| Request logging + tracing | §8 | |
