# Duplicate cluster review — design

**Status:** accepted
**Date:** 2026-08-26
**Epic:** #27 (make duplicate review usable at millions-of-files scale)
**Builds on:** #38 / spec `2026-08-06-identical-tree-collapse` (one decision per *identical* folder)

## Why

Identical-tree collapse already handles folders that match completely. What it cannot reach is the
partial-overlap case: two folders sharing thousands of duplicate files, where one holds a handful of
extras. Their `dir_hash` differs, so nothing collapses, and every shared file becomes its own
decision.

That residue is what remains today. **Measured against the live catalogue on 2026-08-26:**

| | |
| --- | ---: |
| duplicate groups awaiting review | 13,783 |
| loose active rows within them | 63,336 |
| **distinct directory-set clusters** | **4,017** |
| clusters holding more than one group | 1,639 |
| singleton clusters (genuinely one-off) | 2,378 |
| clusters covering 50% of all groups | 424 |
| clusters covering 80% of all groups | 1,450 |

Clustering groups by *the set of directories their copies live in* turns 13,783 decisions into
4,017 — a 3.4× reduction. Not the 100× that tree collapse achieved, because 2,378 groups really are
independent, but it makes the remainder finishable.

### The finding that shapes the UI

**Decision count and space payoff are anti-correlated.** Ranking clusters by how many decisions they
save sends the user to the worthless ones first:

| ranked by | top 15 clusters look like | worth |
| --- | --- | --- |
| decision count | Xilinx/Vitis build output — `ps7_cortexa9_0`, `zynq_fsbl`, `export/`. 112–234 groups each | **0.003–0.009 GiB each** |
| reclaimable bytes | course material and lecture folders. 3–28 groups each | **0.72–4.98 GiB each**, ~23 GiB total |

So the list is ordered by bytes. A count-ordered list would spend 1,800 decisions to recover ~0.1
GiB before reaching anything that matters.

The single largest pattern visible in the data: on `D:`, the folder
`Bio-Inspired Artificial Intelligence [145763] - IACCA - 202122/` contains copies of eight *other*
course folders — a whole tree filed inside a sibling by accident. One decision, several GiB.

## Decisions

| Decision | Choice | Why |
| --- | --- | --- |
| Cluster key | **The set of `(volume_id, parent_directory)` a group's copies occupy** | This is the question the user actually asks once and answers for many files: *"between these folders, which one do I keep?"* |
| Rejected: pairwise clustering | **No** | Measured first: 13,783 groups produce **133,490** distinct directory *pairs*, because a group with N copies yields N-choose-2 pairs. Pairwise clustering makes the problem 10× worse. Recorded so it is not re-proposed |
| Ordering | **By reclaimable bytes, descending** | See above. Count-ordering is actively harmful |
| The decision itself | **An ordered preference over the cluster's directories** | Not "keep directory X": a cluster's groups do not all have a copy in every directory. The user ranks the directories; for each group the highest-ranked directory *present* keeps its copy and the rest are quarantined. One interaction, correct for partial membership |
| Granularity | **Cluster-level confirm, per-file execution** | The user confirms once; the worker still quarantines file by file, and the disk-aware last-copy guard still runs per file. A cluster decision can never bypass a safety check |
| Execution | **Enqueued as `Job::Files` on the shared serial queue** | The queue from the single-file-quarantine design. A cluster of 234 groups becomes queued work, not a blocking request |
| Never elect an unreadable directory | **A directory the scanner could not read is ineligible to be the keeper** | `E:` has 4 unreadable directories, contents unknown. Keeping a copy there while quarantining every copy we *can* see would trade a verified survivor for an unverified one |
| Unreadable directories otherwise | **Warned, not blocking** | Cluster review only acts on groups with ≥2 catalogued copies. An unreadable directory can hide *additional* copies; it cannot remove a known one. It makes the reclaimable total an undercount, which is worth saying, and does not make quarantining unsafe |
| Uncatalogued files | **Warned, not blocking** | Same argument. 12 on `D:`, 15 on `E:` as of 2026-08-26 |
| Archived copies | **Shown, never quarantined** | Existing rule. A copy inside a zip needs repack or extraction, not a rename. A cluster whose only redundant copies are archived offers no confirm |
| Derived data | **Computed on demand, never stored** | Like `duplicate_groups_ranked`, this is a query over `files`. Storing it would add a second thing to invalidate on every quarantine |
| The confirm carries the review floor | **`POST` takes `min_size`** | Cluster membership is computed over groups at or above the floor. Resolving the confirm floor-free would quarantine sub-floor groups in the same directories — a blast radius larger than the one the user was shown |

**Plan:** [`docs/superpowers/plans/2026-08-27-duplicate-cluster-review.md`](../plans/2026-08-27-duplicate-cluster-review.md)

## Architecture

One new module, `src/catalog/clusters.rs`, holding the query and the preference resolution. The web
layer stays thin — it renders and it enqueues.

```
GET /api/duplicate-clusters?limit&offset
        │
        ▼
catalog::clusters::ranked(min_size, limit, offset)
        │   groups loose active duplicates by content_hash
        │   keys each group by its set of (volume_id, parent_dir)
        │   sums (n-1) * size per cluster  →  reclaimable bytes
        ▼
   ClusterDto { id, dirs[], group_count, reclaimable_bytes, sample_paths[] }

POST /api/quarantine-cluster { cluster_id, preference: [dir_ref, ...] }
        │
        ▼
catalog::clusters::victims(cluster_id, preference) -> Vec<i64>
        │   per group: highest-ranked PRESENT directory keeps its copy
        │   every other loose copy becomes a victim
        ▼
   quarantine_queue.enqueue_files(volume_id, victims)
```

### Cluster identity

A cluster's identity is derived, not stored, so it must be reproducible across requests: the id is a
BLAKE3 of the sorted `(volume_id, parent_dir)` list. A client confirming a cluster sends that id
back, and the server recomputes the membership at execution time. If the catalogue has moved on and
the cluster no longer exists, the confirm is refused rather than applied to a stale set.

### Preference resolution

Given a cluster over directories `[A, B, C]` and a user preference `B > A > C`:

- a group with copies in A and B → the B copy is kept, A is quarantined
- a group with copies in A and C → the A copy is kept, C is quarantined
- a group with copies in C only (two files, same folder, different names) → the first by path is
  kept, deterministically, and the rest are quarantined

The rule is *"the highest-ranked directory present wins"*, which is why the decision is an ordering
rather than a single choice.

### UI

A **Clusters** section on the Duplicates page, above the per-file list and below identical folders —
the same escalation the tree-collapse design established, biggest decision first.

Each row shows: the directories with their volume labels, the group count, reclaimable bytes, and
three sample filenames so the user can tell course material from build output. Choosing the keeper
is a click on one directory; a second click on another sets second preference, and so on. Confirm
enqueues and the row collapses.

Rows are paged (100 at a time), matching `loadTrees`, because the full set is thousands of rows and
the top of a bytes-ordered list carries nearly all the value.

## Error handling

| Case | Behaviour |
| --- | --- |
| Cluster no longer exists at confirm time | Refused with a message; the list reloads. Never applied to a recomputed membership |
| Preference names a directory not in the cluster | Rejected as a bad request |
| Preference omits some of the cluster's directories | Accepted — unranked directories sort last, in path order |
| Every candidate keeper is an unreadable directory | Confirm is not offered for that cluster, and the reason is shown |
| A group's redundant copies are all archived | Counted and shown, not quarantined; the cluster reports how many it could not act on |
| Drive not mounted | Reported at request time, as with single-file quarantine |
| Last-copy guard refuses a file at execution | Reported through the queue's status poll as a skip, not an error |

## Testing

1. Two folders sharing three duplicate files, one holding an extra, form **one** cluster of three
   groups — the case tree collapse cannot reach.
2. Clusters are ordered by reclaimable bytes, not by group count. A 2-group / 5 GiB cluster ranks
   above a 200-group / 0.01 GiB cluster.
3. Preference resolution: a group present in only the second-ranked directory keeps that copy.
4. A group whose copies all sit in one directory keeps the first by path, deterministically.
5. Confirming a cluster enqueues exactly the victims, never the keepers.
6. A cluster id recomputed after the catalogue changed is refused, not silently reapplied.
7. A directory recorded as unreadable is never elected keeper.
8. Archived copies are counted and excluded from the victim list.
9. Pairwise clustering is not reintroduced: a group with three copies contributes to exactly one
   cluster.
10. Paging is stable — no cluster is repeated or skipped across pages.

## Out of scope

- Fully unattended auto-quarantine with no human confirm. The user chose cluster-level review, and
  the review GUI is the whole point of Phase 2.
- Cross-cluster preferences ("always prefer `E:`"). If the same answer keeps recurring, that is the
  signal to design it, with the recurrence measured first.
- Anything that would let a cluster decision skip the per-file last-copy guard.
