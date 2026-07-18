# Plan: Relish UX improvements (setup, manual, source, README)

Implementation plan for [2026-07-05-ux-improvements.md](2026-07-05-ux-improvements.md).
Four features, four staged PRs (tests-first, one per feature).

## Context

Goal: lower the learning curve and make Reliaburger demonstrable. Today
`relish` is a management CLI plus a ratatui TUI; a newcomer must read the repo
to learn the platform, and there's no guided install. Four features fix that:

1. **`relish setup`** — detect/install `bun`, generate a config from answers.
2. **`relish manual`** — the whole feature set as an in-terminal, searchable
   reference with runnable examples and a `--web` HTML view.
3. **`relish source`** — ship and fuzzy-search the source the binary was built
   from.
4. **README revamp** — sell the project; move reference material into the
   manual.

Decisions taken with the user: setup **reuses the existing dual-signed upgrade
pipeline** (not a raw GitHub download); the manual is **system + starter
chapters** (machinery now, full content filled incrementally); manual/source
use an **interactive TUI reader** (pulldown-cmark parse → ratatui, nucleo
fuzzy search; `--web` serves pulldown-cmark HTML).

## Reuse map (found during exploration)

- **Upgrade pipeline** `src/upgrade/`: `metadata::fetch(url)` +
  `ReleaseMetadata{latest, releases}` + `platform_key()`/`artifact_for()`
  (`metadata.rs`); `fetch_binary` (`manager.rs:502`); `verify_binary`
  (`signing.rs:131`, dual Ed25519 + sha256); `store::stage`/`activate`
  (`store.rs:56/94`); `exec_current_symlink` (`manager.rs:293`);
  `UpgradeSection` defaults incl. `release_url` (`config/node.rs:243`).
- **Asset embedding** `src/brioche/assets.rs`: `#[derive(Embed)] #[folder=…]` +
  `static_asset_handler` (rust-embed + `mime_guess`), route
  `/ui/static/{*path}` (`bun/api.rs:265`). Copy for `docs/manual/` and `src/`.
- **CLI** `src/bin/relish.rs`: clap-derive `Command` enum + nested-subcommand
  pattern (`Token`/`DevAction`); dispatch match ~729-1041. Handlers in
  `src/relish/commands.rs` or dedicated modules (`upgrade.rs`, `chaos.rs`).
- **TUI** `src/relish/tui/`: async loop (`mod.rs`), `View` enum + view-stack
  navigation (`navigation.rs`), scrollable-buffer pattern (`views/logs.rs`),
  search input pattern (`views/search.rs`, currently substring), reusable
  `heading`/`row` widgets (`views/widgets.rs`).
- **Bun discovery** `src/relish/client.rs`:
  `BunClient::default_local().health()` = "is a bun running here".
- **Config** `src/config/node.rs`: `NodeConfig` (18 sections) +
  `parse`/`from_file`; serialise back with `toml`.
- **Examples** `examples/phase-N/*.toml` — embed and extract to CWD.

## New dependencies

- `pulldown-cmark` — one markdown parser → ratatui `Line`s (terminal) **and**
  HTML (`--web`). No separate `termimad`.
- `nucleo` (matcher) — fuzzy search for manual + source.
- Already present, reused: `rust-embed` (add its compression feature for the
  `src/` embed), `flate2`, `mime_guess`, `axum`.
- No prompt crate: `relish setup` reads stdin (project minimalism). Keep a pure
  `answers → NodeConfig` core so the I/O layer stays thin and testable.

## Staged implementation (one PR per feature, tests-first)

### PR 1 — `relish setup`
`Command::Setup` (+ `--yes`/`--dir` flags). Flow:
1. **Detect**: is a `bun` on PATH / in the install dir, and is one running
   (`BunClient::default_local().health()`)? Read its version if present.
2. **Resolve**: `metadata::fetch(release_url)` → `latest`; compare to installed.
   Pure decision fn → `NotInstalled | UpToDate | Upgradable{from,to}`.
3. **Install** (if needed): `fetch_binary` → `verify_binary` (dual-sig) →
   `store::stage` → `store::activate` into a chosen `binary_dir`
   (default `~/.reliaburger/bin`, overridable). Same safe path the node uses.
4. **Configure**: stdin Q&A (node name, single-node vs join+address, data dir,
   enable dns/ebpf/ingress) → build `NodeConfig` from defaults + answers →
   write `reliaburger.toml`. Offer to start bun.
   - Note: spec's "exec if newer" applies to the node's own self-upgrade; setup
     installs+configures and optionally launches, it does not re-exec relish.
- **Tests first**: install-decision fn over fixture versions; `answers →
  NodeConfig` round-trips through `NodeConfig::parse`; detection handles
  present/absent/running bun. Keep stdin out of the tested core.
- **Files**: new `src/relish/setup.rs`; `Command::Setup` in `relish.rs`; reuse
  `src/upgrade/*`, `src/config/node.rs`, `src/relish/client.rs`.

### PR 2 — `relish manual` (system + starter chapters) — builds the shared reader
- **Content**: create `docs/manual/NN_*.md` (brief, example-driven). Starter
  set: getting-started, deploy-an-app, cluster-basics, observability,
  networking/ingress, chaos — each linking embedded example configs. Structured
  so remaining subsystems are added later (no prose padding).
- **Embed**: `ManualAssets` (`#[folder="docs/manual/"]`) and `ExampleAssets`
  (`#[folder="examples/"]`), mirroring `brioche/assets.rs`.
- **Reader (shared, reused by PR3)**: pulldown-cmark → `Vec<Line>` renderer; a
  new `View::Manual` in the TUI reusing the `logs.rs` scroll offset + view-stack;
  chapter list + content pane; `nucleo` fuzzy search over headings + full text
  (replaces the substring search shape from `search.rs`). Extract a reusable
  `reader`/`fuzzy` component here.
- **Write examples to CWD**: `relish manual examples [--dir .]` extracts embedded
  `examples/**` to the cwd.
- **`--web`**: `relish manual --web` starts an axum server (reuse
  `static_asset_handler` shape) serving one self-contained HTML page —
  pulldown-cmark HTML of all chapters + inlined CSS + a client-side filter box —
  then opens the browser.
- **Tests first**: markdown→`Line`s (insta snapshot of headings/code styling);
  fuzzy query → expected ranked chapters; `--web` builder yields one page
  containing every chapter (substring asserts) + handler returns 200/`text/html`;
  `examples` writes expected files into a temp dir.
- **Files**: new `src/relish/manual/{mod,assets,render,web}.rs` + a TUI view;
  `docs/manual/*.md`; `Command::Manual`.

### PR 3 — `relish source` — reuses PR2's reader + fuzzy
- **Embed**: `SourceAssets` (`#[folder="src/"]`, rust-embed compression on;
  ~5 MB snapshot of 245 files — note binary-size bump, acceptable).
- **Browse/search**: reuse the PR2 reader; a path-list view + `nucleo` fuzzy
  over file paths (and optional full-text). Plain rendering (skip `syntect` to
  keep deps light; can add later).
- **`relish source ebpf`**: opens the search view with `ebpf` pre-seeded in the
  search state.
- **Tests first**: embedded listing non-empty and contains known paths
  (`src/bin/relish.rs`); fuzzy `ebpf` returns the ebpf paths; `source ebpf`
  seeds the query.
- **Files**: new `src/relish/source.rs` + shared reader view; `Command::Source`.

### PR 4 — README revamp (docs-only)
- **Remove** "Current status" (README:112-151) — already in `docs/progress.md`;
  replace with a one-line link.
- **Move** "Repo layout" (70-110) and the detailed "What's included" table into
  the manual (a repo-layout / components chapter); keep only a punchy teaser up
  top.
- **Restructure**: lead with the pitch (what it solves, who it's for, the
  one-binary story), a tight feature highlight, a **prominent book** feature,
  and a **`relish manual`** showcase (asciicast placeholder for the user to add).
  Tighten Quick start.
- **Tests**: none (prose); confirm links resolve and it renders on GitHub.

## Cross-cutting

- **Book/progress**: update `docs/book/13-a-room-with-a-view.md` (Relish) to
  cover setup/manual/source, and add a `docs/progress.md` section for these UX
  features (not a roadmap phase — a post-12b UX track). Update per CLAUDE.md as
  each PR lands.
- **Harness**: each PR green via `make ci` (fmt, clippy `-D warnings`,
  portable nextest, doctest, no-default); new tests in the portable suite;
  hold the coverage floor. No `make coverage` without 40+ GiB free.
- **Sequence**: PR1 (self-contained, high value) → PR2 (builds shared
  reader/fuzzy) → PR3 (reuses it) → PR4 (references the shipped manual).
  PR1 and PR4 are independent of PR2/PR3 if you prefer to reorder.

## Verification (end-to-end)

- `relish setup` on a machine with no bun: detects absence, installs+verifies a
  release into `~/.reliaburger/bin`, writes a valid `reliaburger.toml`
  (`NodeConfig::from_file` parses it), optionally starts bun and `relish status`
  works.
- `relish manual`: TUI opens, chapters scroll, `/` fuzzy-search jumps to a hit;
  `relish manual examples` drops runnable `.toml`s in cwd that `relish apply`
  accepts; `relish manual --web` serves one HTML page and opens the browser.
- `relish source`: lists embedded files, fuzzy filter works; `relish source
  ebpf` opens pre-filtered to the ebpf sources.
- README renders on GitHub, links resolve, reference material now lives in the
  manual.
- `make ci` green on each PR.
