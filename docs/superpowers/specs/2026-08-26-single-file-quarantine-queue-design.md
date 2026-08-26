# Single-file quarantine queue — design

**Status:** proposed
**Date:** 2026-08-26
**Epic:** #27 (make duplicate review usable at millions-of-files scale)
**Builds on:** #66 (serial queue for folder quarantines)

## Why

Confirming a single duplicate blocks the reviewer. [`api_quarantine`](../../../src/web.rs#L924)
runs `quarantine::quarantine_files` inside the request — on a blocking thread, but still awaited
before the response returns. Quarantine re-hashes both copies before it moves anything, so a
multi-GB group takes real time, and the review page says so out loud rather than looking hung:

> `"Verifying content, then quarantining… (large files take a while)"` — [web_ui.rs:1011](../../../src/web_ui.rs#L1011)

The folder path already solved this. #66 added a serial background queue
([`quarantine_queue.rs`](../../../src/quarantine_queue.rs)) so confirming a folder returns
immediately and the worker drains the list in order. The comment in the review UI states the
intent plainly: *"the request only ENQUEUES, so the reviewer can confirm the next folder straight
away instead of waiting out a move of 326,569 files"*.

Single-file confirm never received the same treatment. It is the path the reviewer uses most.

**Measured against the live catalogue on 2026-08-26** (2 volumes, 1,872,066 active rows):

| | |
| --- | ---: |
| duplicate groups awaiting review | 13,783 |
| loose active rows inside those groups | 63,336 |
| reclaimable by a plain move | 57.5 GiB |

Every one of those confirms currently pays a synchronous re-hash. This is the difference between a
review session and a review week — and it compounds with the two designs that follow it, both of
which generate quarantine work in bulk.

## Decisions

| Decision | Choice | Why |
| --- | --- | --- |
| Reuse the existing queue | **One worker, two job kinds** | Serial execution is a correctness requirement, not a convenience. SQLite has a single writer, and every job re-checks that its files are still `active` immediately before moving them — a check that only means anything if nothing else is mutating the catalogue at the same time. A second queue would break exactly that guarantee |
| Job shape | `Job::Tree { volume_id, path }` and `Job::Files { volume_id, ids }` | The two engines have genuinely different inputs (`tree_quarantine::quarantine_tree` takes a path, `quarantine::quarantine_files` takes ids). An enum keeps them distinct instead of smuggling ids through a string |
| Grouping by volume | **One `Files` job per volume** | `quarantine_files` is already per-mount. The handler already groups ids by volume; it now enqueues one job per group rather than looping inline |
| `POST /api/quarantine` response | **Becomes an acknowledgement**, not a result | Mirrors `/api/quarantine-tree`. The work has not happened yet when the response is written, so reporting `quarantined`/`skipped` counts would be a lie |
| Stale decisions | **Worker re-checks; refusals surface through the status poll** | A reviewer who outruns the worker may confirm a group whose keep-copy a previous job has already moved. The last-copy guard inside `quarantine_files` re-runs at execution time, so such a decision is *refused*, never executed wrongly. This is the same argument #66 made for folders |
| Rebuild on drain | **Rebuild every volume touched since the last drain** | Today [`run_job`](../../../src/quarantine_queue.rs#L200) rebuilds the directory index for the *last job's* volume only. With single-file jobs enqueued per volume, a queue spanning both drives leaves one drive's index stale. Pre-existing; fixed here because this is the code being changed |
| De-duplicating enqueues | `Tree` by `(volume_id, path)`, `Files` by id overlap | Double-clicking Confirm must not queue the same work twice. The second attempt would fail harmlessly, but reporting it as an error is noise about something the user did not do wrong |
| Verification | **Unchanged** | Re-hash-before-move, the last-copy guard, and rename-only all stay exactly where they are, inside the engines. This design moves *who waits*, never *what is checked* |

## Architecture

Nothing new is introduced. `quarantine_queue` grows a second job kind and the existing single-file
handler stops doing the work itself.

```
review page  ──POST /api/quarantine──▶  api_quarantine
                                            │  group ids by volume
                                            │  enqueue one Files job per volume
                                            ▼
                                    QuarantineQueue (serial)
                                            │
                        ┌───────────────────┴───────────────────┐
                   Job::Tree                                Job::Files
              tree_quarantine::quarantine_tree      quarantine::quarantine_files
                        └───────────────────┬───────────────────┘
                                            ▼
                             on drain: rebuild_directory_trees
                                     for EVERY volume touched
                                            ▲
review page  ──GET /api/quarantine/status───┘  (existing poller, existing shape)
```

### Types

`Job` becomes an enum. Both DTOs — `QuarantineJobDto` (what is running / pending) and
`QuarantineResult` (what finished) — gain enough shape to describe either kind without the UI
having to guess:

- `kind: "tree" | "files"` — so a message can say *"folder moved"* or *"3 files quarantined"*.
- `label: String` — the folder path for a tree job; a short summary (`"3 files"`) for a files job.
  The existing `path` field is renamed to `label` **on both DTOs**, because for a files job there is
  no single path. The poller reads `running.path` today ([web_ui.rs:1157](../../../src/web_ui.rs#L1157));
  it is updated with them, so the rename is not half-applied.
- `files_updated: usize` — already means the right thing for both.
- `skipped: usize` — **new**; a files job can partially succeed (the last-copy guard protects some
  ids and not others). A tree job reports 0.
- `dest: Option<String>`, `error_message: Option<String>` — unchanged.

### API

`POST /api/quarantine` returns `{ "queued": <n>, "ahead": <n> }` instead of
`{ quarantined, skipped, unmounted_volumes, errors }`.

**This is a breaking change to a public HTTP surface.** It is taken deliberately: the old response
described work that, after this change, has not happened yet. Both consumers live in this repo —
the review page and the Console verb at [web_ui.rs:1681](../../../src/web_ui.rs#L1681) — and both
are updated here.

Volumes that are not mounted are reported in the acknowledgement (`unmounted_volumes`), because
that *is* knowable at request time and the reviewer should not wait on a poll to learn their drive
is unplugged.

### UI

- `#confirm` enqueues, advances to the next group immediately, and calls the existing
  `pollQuarantine()`. The *"Verifying content…"* message is replaced by a queued acknowledgement.
- `pollQuarantine()` already renders `running` / `pending` / `recent` and already surfaces
  `error_message`. It needs only to word its summary from `kind` rather than assuming "folder".
- The Console verb prints the acknowledgement and says the work was queued.

## Error handling

| Case | Behaviour |
| --- | --- |
| Drive not connected at request time | Reported in the acknowledgement; those ids are not enqueued |
| Drive disconnected between enqueue and run | Job fails, `error_message` set, surfaced by the poller. Nothing partial is left behind — the engines are rename-only and check the mount marker first |
| Last-copy guard refuses some ids | Job succeeds with `skipped > 0`. The poller reports both numbers; a skip is not an error |
| Worker panics | `spawn_blocking` join error becomes an `error_message` on that item; the worker loop continues. One bad item must not stop the queue |
| Catalogue write fails mid-job | The engine's existing transaction handling applies unchanged |

A refusal is never swallowed. The reviewer clicked; if it did not take effect they have to be told,
because the alternative is believing a duplicate was handled when it was not.

## Testing

Extending [`quarantine_queue.rs`'s existing test module](../../../src/quarantine_queue.rs#L219):

1. A `Files` job moves the requested ids and leaves the keep-copy alone.
2. A `Files` job whose ids are no longer `active` is refused, with `error_message` set — not
   silently skipped.
3. Enqueuing the same id set twice queues one job.
4. `Tree` and `Files` jobs interleave in submission order; the worker never runs two at once.
5. **A queue spanning two volumes rebuilds the directory index for both** — the regression this
   design fixes. Fails against today's code.
6. Partial success: three ids where the guard protects one reports `files_updated: 2, skipped: 1`.
7. `POST /api/quarantine` returns an acknowledgement without having moved anything yet
   (existing web tests asserting the synchronous shape are updated, not deleted).

## Out of scope

- Cancelling a queued item. Nothing in the review flow needs it yet, and a cancel that races the
  worker is its own design.
- Persisting the queue across restarts. The catalogue is the durable record; an interrupted queue
  means some confirms did not happen, which the next review pass shows again.
- Any change to what the engines verify.
