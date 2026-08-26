# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project status

Phases 1 and 2 are implemented and the web UI is fully built out. The approved design lives in
[docs/superpowers/specs/2026-07-04-cleanupstorages-design.md](docs/superpowers/specs/2026-07-04-cleanupstorages-design.md)
— it remains the source of truth for architecture and behavior; read it before changing core behavior.
Phase 3 (reorganize into a clean taxonomy) is still deferred.

The repo is **public** (AGPL-3.0), CI runs on Windows + macOS, and tagging `vX.Y.Z` cuts a release
(`v0.2.0` is the first published one, with downloadable binaries). See the "Cutting a release"
section of [CONTRIBUTING.md](CONTRIBUTING.md).

## Build / test / run

- Build: `cargo build --release` → `target/release/cleanupstorages(.exe)`
- Test: `cargo test`
- Web UI: `cleanupstorages browse` (serves 127.0.0.1, opens a browser) — six pages (Overview, Browse,
  Duplicates, Drives, Scan, Console), all self-contained (no CDN/fonts/build step).
- CLI verbs: `scan`, `search`, `status`, `duplicates`, `quarantine`, `purge` (`--all`), `repack`, `forget`,
  `tidy`, `browse`. Global `-v/--verbose`; `RUST_LOG` overrides.
- Safe end-to-end walkthrough: [docs/TESTING-GUIDE.md](docs/TESTING-GUIDE.md) (+ `scripts/make-test-sandbox.ps1`).

## Project goal

CleanUpStorages helps the user bring order to thousands of GB of important, irreplaceable data (mixed personal +
academic) spread across multiple near-full external HDDs. Three phases:

1. **Catalog + search** — crawl each drive, hash every file, build a persistent searchable register (works even
   for drives not currently plugged in).
2. **Deduplicate** — visual review GUI ("Tinder-style" swipe + WinMerge-style compare) to confirm duplicates
   one-by-one; confirmed duplicates are soft-deleted to quarantine.
3. **Reorganize** — later, into a clean structure (target taxonomy deliberately undecided).

## Overriding constraint

**Reliability: nothing may ever be lost or corrupted.** This dominates every decision. The tool never performs
an irreversible destructive action on its own — duplicates move to a same-drive `_ToDelete` quarantine (a
rename, ~zero space), and the user empties it manually. Archive repacks (Case 4 in the spec) build a verified
temp copy and only swap after re-hashing every retained entry; the original is preserved in quarantine too. When
in doubt, prefer the option that cannot lose data, even if slower or more manual.

## Tech stack (decided)

- **Rust**, compiled to a single static binary per platform (Windows `.exe`, macOS arm64). No runtime/interpreter.
- **SQLite** catalog (`catalog.db`) stored on the computer, never on the HDDs. WAL mode + auto-snapshots.
- **BLAKE3** for content hashing (streamed, parallel).
- **`axum`** local web server (`127.0.0.1` only) for the review + search GUI; plain HTML/CSS/JS, no frontend
  build system. A CLI covers scan/search/status/purge.

## Git conventions

See [CONTRIBUTING.md](CONTRIBUTING.md). Trunk-based on `main`; feature work on `<type>/<kebab-desc>` branches
(types: `feat`/`fix`/`docs`/`refactor`/`test`/`chore`/`perf`/`build`/`ci`/`style`; scopes: `scanner`,
`catalog`, `dedup`, `archive`, `review`, `storage`, `cli`). Commits follow Conventional Commits.

## Backlog & issue tracking

Actionable work is tracked as **GitHub issues** (`gh issue list`), not scattered across docs:

- **Epics** — large features or themes — use GitHub **sub-issues** to hold their child tasks. Each
  child is a normal issue nested under the epic, so the epic shows live progress. Create epics with
  the `epic` label; link children with `gh api repos/OWNER/REPO/issues/PARENT/sub_issues -F sub_issue_id=CHILD_ID`
  (`-F`, not `-f`: the API wants an integer and `-f` sends a string, which fails with a 422). The id
  is the issue's **database id**, not its number — `gh api repos/OWNER/REPO/issues/N --jq .id`.
- **Where things live:** near-term/committed work → GitHub issues. Long-tail research / "someday"
  ideas stay in [docs/future-ideas.md](docs/future-ideas.md) until one is picked up, at which point
  it graduates into an issue (and, if substantial, its own spec + plan).
- **Still spec-first:** anything non-trivial goes idea → design spec (`docs/superpowers/specs/`) →
  implementation plan (`docs/superpowers/plans/`) via brainstorming before code. Link the spec and
  plan from the issue so the "why" travels with the work.
- **Labels:** `epic`, `bug`, `enhancement`, `refactor`, `ci`, `documentation`, `deferred`.
- The **reliability constraint** above binds every issue that touches file operations — a fix that
  could lose or corrupt data is never "done", regardless of the issue text.

## Documentation map

- Approved design spec: `docs/superpowers/specs/2026-07-04-cleanupstorages-design.md`
- Per-feature specs + plans: `docs/superpowers/specs/`, `docs/superpowers/plans/`
- How this was built with AI (the SDLC loop): `docs/ai-sdlc.md`
- Deferred-idea rationale (now migrated to issues; kept as the long-form "why"): `docs/future-ideas.md`
- Active backlog: GitHub issues (`gh issue list`)
- How to benchmark a scan (and the three traps): `docs/benchmarking-scans.md`
