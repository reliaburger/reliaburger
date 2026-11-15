# Plan: split the crate into `relish`, `bun` and a shared crate

Created 8 November 2026 for
[#1725](https://github.com/reliaburger/reliaburger/issues/1725), for the
maintainer to review. It says where the cut goes, what blocks a clean cut
today and how to break each block, how the book, the coverage floor, the
tests, the source bundle, the manual, packaging and CI follow, and in what
order the pull requests land. No code moves with this plan: the maintainer
decided on 7 November that the split runs right after 0.3.0 merges, so it
doesn't fight 0.3.0's open train
([#1725](https://github.com/reliaburger/reliaburger/issues/1725),
[#1673](https://github.com/reliaburger/reliaburger/issues/1673)).

Background: [F11 in the faster-feedback
plan](2026-11-06-plan-faster-feedback.md#f11-split-the-crate-later) and its
decision D6, and the relish-only macOS measurement on
[#1673](https://github.com/reliaburger/reliaburger/issues/1673) from draft
PR [#1699](https://github.com/reliaburger/reliaburger/pull/1699)
([run 42349545599](https://github.com/reliaburger/reliaburger/actions/runs/42349545599)).

## The short version

Building relish's tests on a hosted Mac took **23.5 of the job's 31
minutes**, and running them took one. The library's unit-test binary compiles
the whole crate, so a job that only wants `relish` still compiles Bun, the
council, the runtimes and the registry twice: once as a library and once as
a test harness. Filtering tests can't fix that. Only a crate boundary can.

The cut:

| Crate | Directory | Holds | Production lines | Test lines |
|---|---|---|---|---|
| `reliaburger-proto` (working name, D1) | `crates/proto/` | The configuration schema, its compiler and offline validation; the HTTP API's request and response types; the PKI both sides use; release verification; `compatibility`, `http_client`, `redact`, `file_lock` | about 31,000 | about 23,000 |
| `relish` | `crates/relish/` | `src/relish/**`, `src/bin/relish.rs`, the client side of `testkit`, `smoker::scenario`, the archive search | about 66,000 | about 34,000 |
| `reliaburger` (Bun; stays at the root) | `src/` | Everything that runs on a node: `bun`, `council`, `cluster`, `grill`, `pickle`, `meat`, `mayo`, `ketchup`, `onion`, `wrapper`, `lettuce`, `smoker`, `mustard`, `reporting`, `brioche`, the rest of `sesame` and `upgrade`, `bin/bun.rs`, `bin/testapp.rs` | about 196,000 | about 104,000 |

`relish` and `reliaburger` both depend on the shared crate and never on each
other. `reliaburger` takes `relish` as a dev-dependency only, for the cluster
suites that drive a real cluster with the real client.

What it buys, estimated from line counts and the one timed build below: the
relish-only macOS job compiles about a quarter of the crate it compiles
today. Our own crate is roughly 58% of a cold build (the dependencies are
the rest), so the 23.5-minute cold compile should fall to **about 14
minutes** and the job from 31 to **about 20**. With the dependency cache
warm, which is the usual case, the job should go from about 20 minutes to
**about 8**. A local edit to a relish file rebuilds
100,000 lines instead of 452,000; an edit to Bun doesn't rebuild relish at
all. And nextest's `rdeps()` can finally select tests by package, so a
relish-only change runs only relish's tests.

It's six steps, all on the first train after 0.3.0 merges:

1. **An edge guard** that fails CI when a new import crosses the future cut.
2. **Untangle in place**, five small PRs inside the one crate, each a pure
   refactor: the configuration owns its section types, validators move next
   to the schema, the API types get their own module, and so on.
3. **The mechanical move**: one PR of `git mv`s, the workspace manifest and
   re-exports, landed in a short freeze window.
4. **CI and the Makefile** select by package.
5. **Clean-up**: drop the re-exports and rewrite imports, subsystem by
   subsystem.
6. **The book** catches up (chapter 1 promised "We won't need to").

## How this was measured

- **The import graph.** A script read every `.rs` file under `src/`, cut each
  file at its `#[cfg(test)] mod tests`, dropped comment lines, and resolved
  every `crate::…` (and `reliaburger::…` in `src/bin/`) path, including
  grouped `use crate::{a, b}` imports, to the file that defines it. That
  gives a file-level graph of production imports and a separate one for
  tests, on `release-0-2-8` at
  [59257ec16](https://github.com/reliaburger/reliaburger/commit/59257ec16).
  Line counts are of those cut, comment-free files, so they're a little
  below `wc -l`'s 465,000.
- **The cut** was found by simulation: propose a set of files for the
  shared crate, list every import that leaves it, and keep a file in the set
  only if its own imports stay small and pure. What's left over are the
  blocking edges below. They're file-level: a file that holds both a wire
  type and the server code that fills it shows up as one edge, and the fix
  splits it.
- **Compile time:** one `cargo test --locked --no-run --timings --lib --bin
  relish --bin bun --test suite` from an empty target directory on the
  laptop (Apple silicon, 12 cores, 32 GB), see
  [the timed build](#the-timed-build). The laptop had another worktree
  building at the same time, so the absolute times are high; the shares are
  what count. A quiet run on the Studio is measurement M1 in the
  [faster-feedback plan](2026-11-06-plan-faster-feedback.md#m1-measure-the-compile-on-a-quiet-host).

## What depends on what today

Top-level modules, production imports only (`a → b` means a file in `a`
imports from `b`):

| Module | Production lines | Imports from |
|---|---|---|
| `bun` | 108,600 | 27 of the other 31 modules, **including `relish`** |
| `relish` | 57,700 | 21 modules: `bun`, `cluster`, `compatibility`, `config`, `council`, `file_lock`, `grill`, `http_client`, `ketchup`, `lettuce`, `mayo`, `meat`, `onion`, `pickle`, `policy`, `redact`, `sesame`, `smoker`, `testkit`, `upgrade`, `wrapper` |
| `grill` | 24,200 | `config`, `durable`, `file_lock`, `ketchup`, `meat`, `onion`, `pickle`, `sesame`, `testkit` |
| `sesame` | 12,500 | `bun`, `cluster`, `compatibility`, `config`, `council`, `firewall`, `http_client`, `meat`, `onion` |
| `pickle` | 11,000 | `cluster`, `compatibility`, `config`, `council`, `file_lock`, `grill`, `meat`, `mustard`, `sesame`, `testkit`, `upgrade` |
| `council` | 10,100 | `bun`, `cluster`, `compatibility`, `config`, `file_lock`, `grill`, `lettuce`, `meat`, `mustard`, `object_storage`, `onion`, `pickle`, `sesame`, `smoker`, `testkit`, `upgrade` |
| `config` | 5,900 | **15 modules**: `bun`, `council`, `firewall`, `grill`, `lettuce`, `meat`, `onion`, `pickle`, `policy`, `reporting`, `sesame`, `smoker`, `testkit`, `upgrade`, `wrapper` |
| `src/bin/relish.rs` | 3,200 | `bun`, `config`, `meat`, `policy`, `relish`, `sesame`, `upgrade` |

Three things stand out.

**One knot of 71,000 lines.** The largest strongly connected component of the
file graph has 123 files and 71,000 production lines: `bun` (21,900),
`grill` (12,800), `pickle` (8,600), `council` (6,600), `meat` (5,800),
`wrapper`, `config`, `reporting`, `testkit`, `mayo`, `upgrade`, `mustard`,
`policy`, `brioche`, `cluster`, `smoker` and `sesame`. Every file in it can
reach every other. You can't put any of them in a crate below the others
without cutting edges first.

**The configuration is a hub, not a leaf.** `config/node.rs` imports a type
from 13 subsystems, because each subsystem defined its own `node.toml`
section (`council::backup::BackupConfig`, `onion::dns::DnsConfig`,
`wrapper::types::WrapperConfig` and ten more). `config/validate.rs` calls 11
subsystems' validators. So the closure of `config/` alone is 248 files and
108,000 lines: the configuration "depends on" half the node. Everyone needs
the configuration, so this is the edge that matters most.

**relish reaches 183,000 of the 293,000 production lines.** Through its 95
direct imports outside `relish`, the CLI's transitive closure is 62% of the
crate. Most of those imports are wire types (`bun::agent::NodeStatus`,
`mayo::alert::AlertStatus`, `onion::trace::TraceResult`), but each sits in a
file that also holds server code, and the server code pulls in the knot.

And one edge runs backwards: `lettuce::sync`, GitOps on the node, calls
`relish::compile::compile_sources` in production. Bun depends on the CLI.

## The crate boundaries

### `reliaburger-proto`: what both sides read

The rule: **a file belongs here if `relish` and Bun both need it and it
doesn't touch a node's state.** Parsing, validating, serialising and
verifying are in; running, storing and scheduling are out.

| Moves here | From | Why both sides need it |
|---|---|---|
| `config/` (all of it, including `validate.rs`) | `src/config/` | `relish apply`, `relish lint`, `relish compile` and GitOps parse and check the same TOML |
| `relish::compile` | `src/relish/compile.rs` | GitOps and `relish apply` merge multi-file trees the same way (the backwards edge) |
| The section types `node.toml` names: `BackupConfig`, `DnsConfig`, `SourceAcl`, `ImageMirrors`, `WrapperConfig`, `GitOpsConfig`, `SmokerConfig`, `SecretProviderConfig`, `TokenLifetimePolicy`, `ClusterTestPolicy`, the runtime-knob and stop types, `DeployStrategy`, and the constants they default from | `council`, `onion`, `grill`, `wrapper`, `lettuce`, `smoker`, `sesame`, `testkit`, `meat`, `bun` | They're the schema |
| The offline validators `validate.rs` calls: cron parsing, deploy strategy, autoscale spec, task-array spec errors, task expansion, host-binary detection, ingress and egress syntax, CIDR parsing, public-key parsing, volume layout, owner names | `meat`, `wrapper`, `sesame`, `firewall`, `upgrade`, `grill`, `council` | A file is valid or not the same way everywhere |
| `policy/` minus the Raft wrapping | `src/policy/` | `relish policy test` and the leader evaluate the same CEL |
| A new `api` module: every request and response type of Bun's HTTP API | `bun::agent`, `bun::events`, `bun::cluster_view`, `bun::capabilities`, `bun::diagnostics`, `bun::deploy_operations`, `bun::top`, `bun::readiness`, `bun::audit`, `bun::api`, `bun::task_array_api`, plus `ketchup::types`, `ketchup::sse`, `mayo::alert`, `mayo::rollup`, `mayo::series`, `onion::trace`, `onion::types`, `smoker::types`, `wrapper::types`, `lettuce::types`, `pickle::types`, `policy::audit`, `cluster::capacity`, `cluster::retirement`, `council::types::CouncilNodeInfo`, `testkit::lease::TestLease`, `grill::ExecOutput`, `meat::scheduler::ScheduleError` | They're the protocol |
| The PKI: `sesame::{ca, cert, crypto, identity, identity_store, init, join, bootstrap, root_backup, ca_rotation, trust, mtls, token, auth, session, credentials, oidc, secret, secret_provider, egress, types}` | `src/sesame/` | `relish init`, `relish ca`, `relish join` and every mTLS client use the same certificates and tokens as Bun |
| Release verification: `upgrade::{metadata, keys, signing, store, version, types, marker, error}` | `src/upgrade/` | `relish upgrade` and Bun's self-upgrade check the same signatures (`manager.rs`, the self-upgrade engine, stays in Bun) |
| `pickle::{signing, capability}` and the build helpers both sides call (`tar_context`, `digest_of`, `BuildahJob`, `execute_build`) | `src/pickle/` | `relish build` and Bun's build runner run the same Buildah job |
| `ketchup::export` and the log schema (`log_schema`, `validate_grep`) | `src/ketchup/` | `relish logs-export` and Bun's disk-pressure export write the same archive |
| `compatibility.rs`, `http_client.rs`, `redact.rs`, `file_lock.rs`, `object_storage.rs`, `durable.rs` | `src/` | Small, pure, used by both |

About 31,000 production lines. It brings `rustls`, `rcgen`, `x509-parser`,
`ring`, `age`, `cel`, `object_store` and `toml_edit` with it, but no
`openraft`, `redb`, `axum`, `datafusion`, `aya` or `oci-client`.

### `relish`: the CLI

All of `src/relish/` and `src/bin/relish.rs`, plus three things that turn
out to be client-only:

- **`testkit`, except `lease.rs`.** The runner, the cases, `bench/`, `chaos/`,
  `oci.rs`, `registry.rs` and `pinned_images.rs` drive a cluster through
  `relish::client::BunClient` for `relish test`, `relish bench` and the
  quickstart. Bun uses only `testkit::lease` (test leases are a cluster
  feature, stored in the council) and `testkit::safety` (which goes to the
  shared crate).
- **`smoker::scenario`**, the fault-scenario file format. Only `relish fault`
  reads it, and it already imports `relish::fault`'s duration and percentage
  parsers, a second backwards edge that this move turns forwards.
- **`ketchup::remote_query`**, `relish logs-search`'s DataFusion query over
  exported Parquet. It's the only reason the CLI needs DataFusion. It stays a
  `relish` dependency (rust-cache keeps it), and the schema it reads comes
  from the shared crate.

About 66,000 production lines. `relish` keeps the `kubernetes` feature
(`k8s-openapi`), which only `src/relish/` and `src/bin/relish.rs` use, and
`embed-source`.

### `reliaburger`: Bun and the node

Everything else stays where it is, in `src/`, in the package called
`reliaburger`. Keeping the root package and its name means `tests/`,
`benches/`, `fuzz/` and the 110 book paths into node code don't move, and
`use reliaburger::…` keeps compiling in every gated suite. It keeps `ebpf`,
`fuzzing` and the executor helper. Renaming the package to `bun` is possible
later and isn't worth a second big rename now (D2).

### Features after the split

| Feature | Today | After |
|---|---|---|
| `kubernetes` (default) | the crate | `relish` only |
| `ebpf` | the crate | `reliaburger` only |
| `nightly-signing` | the crate (`upgrade/keys.rs`) | `reliaburger-proto`; `relish` and `reliaburger` each forward it (`nightly-signing = ["reliaburger-proto/nightly-signing"]`) |
| `embed-source` | the crate | `relish` only |
| `fuzzing` | the crate | `reliaburger` (the targets import the council and the runtimes); `fuzz/Cargo.toml` is unchanged |

## The blocking edges, and how to break each

Each one is a PR in step 2, inside the single crate, before anything moves.
The edge guard (step 1) records the current exceptions and each PR deletes
its lines from that list, so the list can only shrink.

### E1. The configuration imports its sections from subsystems

`config/node.rs` → 13 subsystems, `config/app.rs` → `grill::knobs`,
`grill::stop`, `meat::deploy_types`; `config/build.rs` →
`pickle::registry_auth::repository_namespace`; `config/types.rs` →
`sesame::secret_provider::SecretReference`; `config/error.rs` →
`policy::PolicyError`.

**Fix: invert them.** Each section type moves into `config` (a
`config/sections/` directory if `node.rs` gets too long), and the subsystem
imports it from there. The defaults' constants move with them.
`grill::knobs` is the one awkward file: it declares and parses the knobs
(schema) and turns them into an `OciSpec` (runtime). The declarations and
parsing go to `config`; the `OciSpec` conversion stays in `grill` as a free
function. `pickle::cosign::CosignKey::parse_all`, which `node.toml`'s trust
section calls to reject a bad key early, splits the same way: parsing goes
to the shared crate, verification stays in `pickle`.

### E2. Offline validation calls subsystem code

`config/validate.rs` calls `grill::volume::validate_managed_volume_layout`,
`meat::filter::host_binary`, `meat::job::JobDefinition::from_spec` (to count
a job's tasks), `meat::cron`, `meat::autoscaler`, `meat::task_array`,
`policy::check_policy`, `sesame::egress::validate_egress_entry`,
`wrapper::routing::validate_ingress`, `firewall::rules::parse_cidr`,
`upgrade::signing::parse_public_key`, `council::owner::validate_owner`,
`reporting::transport::MAX_EVENTS_PER_REPORT` and two `bun::health`
constants.

**Fix: the pure part moves next to the schema.** Each of these is a pure
function over configuration types, sitting in a file whose other half is
runtime code. The function moves; the runtime file calls it from its new
home. Two need a real split rather than a move: `JobDefinition::from_spec`
lives in a file that imports the task-array store, so the task count it
needs becomes a pure `task_count(&JobSpec)` in `config`; and
`meat::filter::host_binary` sits beside the placement filters, so the
host-binary test moves and the filters stay. The rule for what stays behind:
**anything that needs cluster state is the leader's admission check, not
offline validation.**

### E3. The policy engine names a Raft request

`policy/mod.rs` and `policy/audit.rs` import `council::types::RaftRequest`,
because a policy-fenced write wraps the requests it guards. The CEL
evaluation over `Config`, `AppSpec` and `JobSpec` moves to the shared crate;
the part that wraps the guarded writes (`writes: Vec<RaftRequest>`, which
becomes `RaftRequest::PolicyFenced`) moves to the Bun side, beside
`council::types` and its callers in `bun::api::apply` and
`lettuce::runner`.

### E4. GitOps calls the CLI's compiler

`lettuce::sync::parse_toml_files` calls
`relish::compile::compile_sources`. `compile.rs` is configuration code that
happens to live under `relish/`: it moves to `config::compile`. Both callers
follow.

### E5. The fault scenarios and the CLI import each other

`smoker::scenario` calls `relish::fault::{parse_duration, parse_delay_ns,
parse_percentage, parse_bandwidth}`, and `relish::fault` reads
`smoker::scenario`. Only `relish` reads scenario files, so `scenario.rs`
moves into `relish` and the cycle disappears.

### E6. The CLI reads Bun's API types from Bun's server files

`relish` imports more than 40 distinct paths from 13 files under `src/bun/`:
`agent` (`NodeStatus`, `InstanceStatus`, `ClusterInstanceStatus`,
`CouncilStatus`, `CouncilRole`, `ApplyEvent`, `ApplyResult`,
`CurrentResourceStatus`, `CouncilMemberInfo`), `events` (`ClusterEvent`,
`EventKind`, `EventSeverity` and the in-memory `EventStore` that `relish wtf`
replays), `cluster_view`, `capabilities`, `diagnostics`,
`deploy_operations`, `top`, `readiness`, `audit` (`AuditFilter`,
`AuditListing`), `api` (`ClusterFaultList`, `NodeMembershipInfo`,
`apply::PolicyRefusal`), `task_array_api`, `task_array_node::TaskResultRow`
and `job_apply::validate_rerun`. `bun/agent.rs` alone sits on a path to the
whole knot.

**Fix: a top-level `api` module**, with one file per resource, holding every
type that crosses the HTTP API. Bun's handlers and `relish::client` both
import from it. This is also the protocol made visible: a reviewer can see
every wire change in one directory, and the `compatibility` version bump
rule gets an obvious place to point. `validate_rerun` is pure and moves with
the task-array types.

### E7. The CLI reads other subsystems' types from runtime files

The same fix for the rest: `ketchup::{types, sse, follow}` and
`log_store::validate_grep`; `mayo::{types, alert, rollup, series}`;
`onion::{types, trace}` (and `sesame::firewall::FIREWALL_ALLOW`, the one
constant `trace` needs); `smoker::types`; `wrapper::types`;
`lettuce::types`; `pickle::types` (whose `save` calls
`sesame::identity::atomic_write_mode`, which moves too);
`cluster::{capacity::SchedulingRefusal, retirement}` and
`orchestrate::lease_retirement_bound` (a pure function of a duration);
`meat::scheduler::{ScheduleError, canonical_repository, split_repo_tag}`;
`council::types::CouncilNodeInfo` and
`council::selection::CouncilSelectionConfig::default().max_council_size`
(a constant); `grill::ExecOutput`; `testkit::lease::TestLease` (the type;
the lease store and reapers stay in Bun). Where a type's file is already
called `types.rs` and imports nothing heavy, the whole file moves.

### E8. The CLI runs node-local engines

These aren't types, and they're the real design question:

| Call | Command | Fix |
|---|---|---|
| `council::recovery::{recover_data_dir, reenrol_data_dir, load_recovery_state, persisted_voters}` | `relish council recover`, `relish council reenrol` | They open a stopped node's Raft log and `redb` store, so they need the council's storage engine. Move them behind `bun council recover` and `bun council reenrol`, and have the `relish` commands run the `bun` next to them on the same host, with the same flags and output. They only ever ran on a node, where `bun` is installed (D3) |
| `council::backup::BackupStore::from_url` | `relish council backups --from`, which lists the sealed backups in object storage | Listing and naming backups needs only `object_store`: that half of `BackupStore` moves to the shared crate with `BackupConfig` (E1); writing a snapshot, which needs the council, stays |
| `meat::task_array_store::TaskArrays::default()` with `TaskArrayWrite::RegisterManifest` | the local manifest check before a task-array submit | A trial registration against an empty store, to reuse its validation. Extract that validation as a pure `validate_manifest` in the shared crate; the store calls it too |
| `grill::ImageStore::new` | `relish bench --scenario` caches images | `relish` only needs a directory of pulled layers; give it a small cache over `oci-client` instead of the runtime's content store, or keep the bench scenario in a `reliaburger` binary (D4) |
| `grill::image::ImageReference`, `pickle::upstream::OciUpstream`, `pickle::store::compute_sha256` | `testkit`'s pinned images and image-registry case | `ImageReference` parsing and `ImageMirrors` are schema (E1); the upstream client is a thin `oci-client` wrapper that can move with them |

The alternative for all of E8, letting `relish` depend on `reliaburger`, is
rejected below: it puts the whole node back into relish's compile.

### E9. Tests that cross the cut

| Where | What | Fix |
|---|---|---|
| `bun/api/tests.rs` | uses `relish::client` and `relish::plan` to drive the API | These are end-to-end tests of client and server together; they move to `tests/suite/` in the root package, which dev-depends on `relish` |
| `bun/agent/tests/mod.rs`, `src/bin/bun.rs` tests | call `relish::commands::init` and `init_with_security` to write a node's config | `relish init` writes files from templates; the writer moves to `config::init` in the shared crate, and `relish init` calls it |
| `bun/authz.rs` | `include_str!("../sesame/auth.rs")` to check every handler names a scope | The path becomes `../../crates/proto/src/sesame/auth.rs`; the source-bundle job proves it's in the bundle |
| `src/lib.rs` | `#[path = "../tests/support/first_launch.rs"]` | Stays in the root package; unchanged |
| `tests/suite/` | 11 modules use `reliaburger::relish`, 11 run `CARGO_BIN_EXE_relish` | See [the tests](#the-tests) |
| Shared crate test helpers | Bun's tests build configs with `#[cfg(test)]` constructors in `config` | `cfg(test)` is per crate, so they become a `testing` module behind a `testing` feature that only dev-dependencies turn on |

### Two Rust rules the cut runs into

Both are worth a section in the book, because a Go or Python reader won't
expect them:

- **Inherent `impl` blocks must live in the type's crate.** Today
  `impl ImageMirrors { … }` can sit in `grill` because it's one crate. Once
  `ImageMirrors` moves to the shared crate, a method on it in `reliaburger`
  is a compile error. Each moved type takes its pure methods with it; the
  runtime ones become free functions or an extension trait on the Bun side.
- **The orphan rule.** `reliaburger` can't implement a trait it doesn't
  own (`Display`, `From`, `serde::Serialize`, or one from the shared crate)
  for a type it doesn't own. A `From<ProtoError> for BunError` in Bun is
  fine, because `BunError` is local; a `Display` for a shared-crate type
  must live in the shared crate.

And `pub(crate)` stops at the crate boundary: 40 items in the files bound
for the shared crate are `pub(crate)` today, and the ones Bun or `relish`
use become `pub`.

## The edge guard

A test in `tests/suite/` (or `scripts/ci/crate_edges.py`, with fixtures, if
the maintainer prefers the script set) that rebuilds the file graph this
plan used and fails when:

- anything outside `src/relish/` imports `crate::relish`;
- a file bound for the shared crate imports a file that isn't;
- a file bound for `relish` imports a file bound for `reliaburger`.

The bound-for lists and today's exceptions live in one checked-in file. The
guard lands first, so the knot can't grow while the untangling PRs land, and
it's deleted in the clean-up step, when the compiler enforces the same thing.

## How everything else follows

### The book

The book cites 204 `src/` paths. 35 are under `src/relish/`, 6 under
`src/bin/`, and about 50 name files bound for the shared crate
(`compatibility.rs` 13, `sesame` 12 of which most move, `policy` 6,
`upgrade` 5, `config` 4, `file_lock` 3, `redact` 2, `http_client` 1). Of the
17 captioned listings ("(from `src/…`)"), one is from a moving file
(`src/sesame/types.rs`). `scripts/ci/test_book.py` checks that each cited
path exists and each captioned listing still defines what it shows, so the
move PR can't leave a dead path: it rewrites them, and the test's
`REPO_PATH` pattern gains `crates/`.

Chapter 1, "Hello, Container", says we considered a workspace, chose one
crate, and "We won't need to" split. Step 6 gives that paragraph its
sequel: what one crate cost us at 450,000 lines, with the numbers from this
plan, and what a workspace is (`[workspace]`, members, path dependencies,
one `Cargo.lock`). The Rust lessons go where the readers meet them:
inherent impls and the orphan rule, `pub(crate)` against `pub`, a glob
re-export beside local modules, `use crate as relish;`. Chapter 15, "Ready
for Production", gets the CI change. `CLAUDE.md`'s project structure and
chapter table say where the crates are.

### The coverage floor

`COVERAGE_MIN_LINES` stays at 78.65 and is never lowered. `make coverage`
already passes `--workspace` to `cargo llvm-cov clean` and reports the whole
workspace, so the floor keeps measuring the same lines. Two scripts assume
one crate:

- `scripts/ci/coverage_owner.py` requires `reliaburger` among the workspace
  members (it stays) and compares
  `__CARGO_LLVM_COV_RUSTC_WRAPPER_CRATE_NAMES` with the metadata's member
  list; with three members that list has three names, and its fixture gains
  them.
- `scripts/ci/coverage_contract.py` requires `reliaburger` in the
  instrumented crate names; it should require all three, so a member that
  drops out of instrumentation fails rather than quietly shrinking the
  denominator.

The recipe text both embed (`MAKE_RECIPE`) changes only if the `coverage`
target does. The risk is the other direction: moving tests between crates
can move lines between "covered by a unit test" and "covered only by the
suite". The move PR runs `make coverage` and records before and after in
its description.

### The tests

- **`tests/suite/` in the root package** keeps every module that starts Bun
  or uses Bun's library, and gains `bun/api/tests.rs`'s client-and-server
  tests (E9). It dev-depends on `relish`, so `reliaburger::relish::…`
  becomes `relish::…`.
- **`crates/relish/tests/suite/`** is relish's own single portable binary:
  the `relish_*` modules, `wtf_watch`, `uninstall`, `user_directories` and
  the rest that only run the CLI. `docs/testing.md`'s "one portable suite
  binary" rule becomes "one per package".
- **`CARGO_BIN_EXE_relish`** only exists in the package that owns the
  binary ([Cargo
  docs](https://doc.rust-lang.org/cargo/reference/environment-variables.html#environment-variables-cargo-sets-for-crates)).
  Eleven suite modules, four gated suites (`kubernetes_demo`, `demos`,
  `examples`, `relish_test_catalogue`) and `tests/support/bun_process.rs`
  use it, two of them alongside `CARGO_BIN_EXE_bun`. Cargo's [artifact
  dependencies](https://doc.rust-lang.org/cargo/reference/unstable.html#artifact-dependencies)
  would solve it properly but are still unstable. Instead,
  `tests/support/bun_process.rs` finds `relish` beside `CARGO_BIN_EXE_bun`
  (both packages build into one `target/<profile>/`) and fails with "run
  with `--workspace`" if it's missing or older than the test binary. The
  Make targets already build with `cargo test --no-run` before running; they
  gain `--workspace`.
- **The 19 gated `tests/*.rs` suites that use `bun_process`**, and the 15
  that use `reliaburger::relish`, stay in the root package with the same
  `make` targets. Their imports change from `reliaburger::relish::` to
  `relish::`.
- **insta snapshots** are named after the module path: 168 files under
  `src/relish/` are `reliaburger__relish__…snap` and become `relish__…snap`.
  The move PR renames them with `git mv`, so no snapshot content changes.
- **`tests/contracts/manifest.json`** and
  `docs/testing/qualification-matrix.md` name each contract case's binary by
  nextest's binary id: 249 cases on `reliaburger` (the library's unit
  tests), 39 on `reliaburger::suite`, and so on. The cases whose tests move
  get `relish`, `relish::bin/relish`, `relish::suite` or
  `reliaburger-proto`; `scripts/ci/macos_selection.py apply` then rewrites
  the macOS gates from the Makefile's filterset, and its fixtures get the
  new ids.

### CI test selection

Today the [portable macOS job](../../.github/workflows/ci.yml) runs `make
lint-macos` (clippy on the library, `relish` and `bun`, all features) and
`make test-macos`, whose nextest filterset picks `binary_id(reliaburger::bin/relish)`,
the `relish::`, `grill::apple::` and one `grill::records` test in the
library, the suite's `relish_*` and `user_directories::macos_*` modules and
`runtime_selection` ([#1720](https://github.com/reliaburger/reliaburger/issues/1720)).
After the split:

| | Today | After |
|---|---|---|
| macOS lint | `clippy --lib --bin relish --bin bun --all-features` | `clippy -p reliaburger-proto -p relish --all-features`; Bun's compile check stays on Linux, where Bun ships |
| macOS tests | `--lib --bin relish --test suite --test runtime_selection -E '…'` | `-p reliaburger-proto -p relish`, plus the Apple runtime's tests (`grill::apple::`, `runtime_selection`) in a second, small `-p reliaburger -E` pass, or on the Studio runner (D5) |
| macOS doctests | `cargo test --doc -- relish::` (compiles every doctest) | `cargo test --doc -p relish` |
| A PR that touches only `crates/relish/` | the light set: portable Linux runs everything | `cargo nextest run --workspace -E 'rdeps(relish)'`, which is `relish` alone |
| A PR that touches `crates/proto/` | everything | everything (`rdeps(reliaburger-proto)` is all three) |
| `scripts/ci/select-jobs.sh` | paths under `src/` | the same rules per crate directory; a change under `crates/relish/` alone needs no cluster or Linux privileged job |

The Apple runtime (`grill::apple`) is the only macOS-only code outside
`relish`, and `runtime_selection` exercises it. Whether that pass stays on
the hosted Mac is D5.

### The source bundle and `relish source`

`build.rs` does four things today: packs the source bundle for `relish`,
sets `RELIABURGER_TARGET` and `RELIABURGER_GIT_SHA`, compiles the
reusable-executor helper and, with `ebpf`, the eBPF programs. After the
split, `crates/relish/build.rs` packs the bundle and sets the two variables;
the root `build.rs` keeps the helper, eBPF and the two variables. `pack.rs`
moves with `relish` and is still compiled into both the library and the
build script with `#[path]`.

The bundle has to stay buildable on its own (the CI `source bundle builds`
job extracts it into an empty directory and builds it). `pack.rs`'s
`ALLOWLIST` gains `crates` and keeps `src`, `Cargo.toml` (now the workspace
manifest), `Cargo.lock`, `build.rs` and the rest; its walk starts from the
workspace root (`CARGO_MANIFEST_DIR/../..`) instead of the package root.
`relish source` in a debug build reads the checkout through
`CARGO_MANIFEST_DIR` in eight places under `src/relish/`; each becomes the
workspace root. The paths `relish agent source` prints change from
`src/relish/…` to `crates/relish/src/…`, and nine files under
`docs/website/`, `docs/manual/` and `plugins/` cite them (the agents page
runs `relish agent source tree src/relish`), so the move PR rewrites them
and regenerates the Markdown copies (`make agent-docs`).

### The manual compiled into `relish`

`src/relish/manual/assets.rs` embeds `docs/manual/` and `examples/` with
`#[folder = …]`, which `rust-embed` resolves against the package's manifest
directory ([docs](https://docs.rs/rust-embed/8/rust_embed/)). From
`crates/relish/` they become `../../docs/manual/` and `../../examples/`. The
manual and examples don't move; `brioche/dist/`, embedded by Bun, stays
where it is too.

### Release packaging

`build.yml` builds `--features ebpf --bin bun --bin relish` on Linux and
`--bin relish` on macOS. In a workspace whose root is a package, Cargo
selects only that package by default, so `--bin relish` wouldn't be found
and a bare `--features` names the root's features. The Linux line becomes
`-p reliaburger -p relish --features reliaburger/ebpf`, and the nightly
build's `$NIGHTLY_FEATURES` becomes
`reliaburger/nightly-signing,relish/nightly-signing`.
`scripts/release/test_build_workflow.py` parses those `--bin` arguments and
gains the package flags. The binaries land in the same
`target/<target>/release/` directory, so `package.py`, notarisation, the
Homebrew formula and `install.sh` don't change. `relish --version` and `bun
--version` both read `CARGO_PKG_VERSION`: the three packages share
`version.workspace = true`, so a version bump still edits one line, in the
workspace's `[workspace.package]`.

### Everything else that reads a path

`Makefile` targets (`readme-commands`, `lint-macos`, `test-macos`), the
`ci.yml` path filters, `scripts/ci/workflow_assembly.py`'s shard list,
`docs/testing.md`, `docs/design/test-harness.md`, `CLAUDE.md`, `README.md`
and `docs/README.md`. Each is a mechanical edit in the move PR, found with
one `grep -rn 'src/relish\|src/bin/relish\|src/config\|src/sesame'`.

## The timed build

On the laptop, from an empty target directory, with another worktree
building beside it, so slow in absolute terms
([`--timings`](https://doc.rust-lang.org/cargo/reference/timings.html)):

| Unit | Starts at | Takes |
|---|---|---|
| Every dependency | 0 s | the last finishes at 131 s |
| The library as an ordinary library | 130 s | 106 s |
| The library as a unit-test harness | 131 s | **180 s** |
| `tests/suite` | 236 s | 24 s |
| `relish`, `bun` and `testapp`, and the two binaries' tests | 236 s | 2–8 s each |
| The whole build | | 311 s |

Three readings. Our crate is 180 of the 311 seconds, about 58%; the
dependencies are the rest, and rust-cache restores them on a warm run. The
two library units run side by side, and the unit-test harness is the long
pole: everything after it is small. And the binaries are cheap, because
nearly all the code is in the library: `src/bin/relish.rs` is 6,000 lines
and builds in 4 seconds.

By lines, `relish`'s share of those units after the split: the shared
crate's library about 11 s (31,000 of 293,000 production lines of the 106
s), `relish`'s library about 24 s, its test harness about 40 s (100,000 of
452,000 lines of the 180 s), its binary and suite about 8 s. That's about
83 unit-seconds against today's 330 or so, **25%**, and a critical path of about
60 s against 181, **33%**.

## The saving

Compile work is not proportional to lines (generic-heavy code such as
Bun's axum handlers and the Raft storage costs more per line than CLI
rendering does), so these are estimates to check with M1 after the move,
not promises.

**The relish-only macOS job.** It compiles, today: the library as a test
harness (452,000 lines including tests), the library again as an ordinary
library for the binaries (293,000), `relish`'s binary and its tests (6,000)
and the 37,000-line suite. After the split: the shared crate as a library
(31,000), `relish` as a library and as a test harness (66,000 + 100,000),
its binary and its own suite (about 10,000). That's about **210,000 against
790,000 line-compiles, 27%**, close to the 25% the
[timed build](#the-timed-build) gives by unit.

The hosted Mac has three cores, so parallelism helps it less than the
laptop, and the share of work is the better guide. If its cold 23.5 minutes
split like the laptop's (about 13.6 minutes our crate, 9.9 dependencies),
our crate's part falls to about 3.5–4.5 minutes, and the cold compile to
**about 14 minutes**. The lint step loses Bun's compile check and most of
clippy's work (4.6 + 1.2 minutes to about 2). The cold job goes from 31
minutes to **about 20**. A warm job, with the dependencies restored, goes
from about 20 minutes (13.6 compile, the lint, a minute of tests) to
**about 8**. The dependencies shrink a little too (no `openraft`, `redb`,
`aya` or `criterion` for `relish`), which these numbers don't count.

**Local builds.** An edit to a file in `crates/relish/` rebuilds `relish`
only; today it rebuilds the whole crate twice for `make ci`. An edit to
Bun's files no longer rebuilds `relish`. An edit to the shared crate
rebuilds everything, which is why the shared crate holds only what both
sides need, and why it's the smallest of the three.

**Linux.** The total work barely changes, but `relish` and `reliaburger`
compile in parallel once the shared crate is done, and rustc's front end is
mostly single-threaded per crate, so the portable Linux job should get
faster in wall-clock time. How much is M1's question.

**Test selection.** With three packages, `rdeps()` makes test-impact
analysis possible at last: the faster-feedback plan ruled it out on one
crate because "with one package nothing narrows the build"
([What isn't worth doing now](2026-11-06-plan-faster-feedback.md#what-isnt-worth-doing-now)).

## The order of pull requests

All of it goes on the first train after 0.3.0's train
([#490](https://github.com/reliaburger/reliaburger/pull/490)) merges into
`main`, following the [merge-train rules](../releasing.md#merge-trains-and-their-ci):
each PR into the train, one gate PR into `main`. Nothing goes into
`release-0-2-8` or `release-1-3-0`.

| # | PR | Touches | Conflicts with in-flight work |
|---|---|---|---|
| 1 | The edge guard and its exception list | one new test, one data file | none |
| 2a | E4 and E5: `compile.rs` to `config`, `scenario.rs` to `relish/` | 3 files | low |
| 2b | E1: the configuration owns its sections | `config/`, 13 subsystems' imports | medium: anything adding a `node.toml` field |
| 2c | E2 and E3: validators next to the schema; the policy engine split | `config/validate.rs`, the validator files, `policy/`, `council` | medium |
| 2d | E6 and E7: the `api` module | `src/api/` (new), the type files, `relish/` and `bun/` imports | high: every PR adding an API field |
| 2e | E8 and E9: `bun council recover` and `reenrol`; `validate_manifest`; the bench cache; `config::init`; moving `bun/api/tests.rs`'s end-to-end tests | `relish/commands.rs`, `bin/bun.rs`, tests | medium |
| 3 | **The move** | `git mv` of every file in the two new crates, the workspace manifest, re-exports, snapshot renames, `build.rs` split, every path in docs, Make, CI and release scripts | **all of them** |
| 4 | CI selects by package | `ci.yml`, `Makefile`, `select-jobs.sh`, `macos_selection.py`, the contract manifest | low |
| 5 | Clean-up: drop the re-exports, one subsystem per PR | imports | low each |
| 6 | The book, `CLAUDE.md`, `docs/testing.md` | docs | none |

### Keeping in-flight PRs mergeable

- **The move PR keeps every moved file byte-identical.** That's what lets
  Git's merge follow the rename: an open PR that edited
  `src/relish/commands.rs` merges cleanly after the file became
  `crates/relish/src/commands.rs`, because `ort` pairs the two by content
  ([git-merge](https://git-scm.com/docs/git-merge#Documentation/git-merge.txt-ort)).
  To keep the contents identical, each new crate's root carries aliases:
  `relish`'s `lib.rs` has `use crate as relish;` (so `crate::relish::client`
  still resolves) and `pub(crate) use reliaburger_proto::{api, config, …};`;
  Bun's `lib.rs` re-exports the moved modules
  (`pub use reliaburger_proto::{config, compatibility, …};`), and a module
  that's split, like `sesame`, keeps its Bun-side files and adds
  `pub use reliaburger_proto::sesame::*;`. The
  [`use` declaration rules](https://doc.rust-lang.org/reference/items/use-declarations.html)
  allow all three. Step 5 removes them.
- **One rename at a time.** The move PR renames and does nothing else; any
  content edit to a moved file goes in step 2 or step 5. Git's rename
  detection matches by similarity, and a file changed past 50% in the same
  commit stops being a rename.
- **A freeze window for the move.** The move PR opens, goes green and merges
  the same day. For that day, no other PR into the train merges, and the
  train's open PRs merge the train into their branches afterwards (merge
  commits, never a rebase). About 450 file renames are cheap to redo if
  something slips: regenerate the PR from a script rather than resolve
  conflicts in it. The script (a few dozen `git mv` lines and `sed` over the
  docs) is checked in with the PR, so a reviewer can rerun it.
- **Nothing to configure in Git.** Merges detect renames by default, and
  the limit (`merge.renameLimit`, 7,000 paths unless set,
  [git-config](https://git-scm.com/docs/git-config#Documentation/git-config.txt-mergerenameLimit))
  is far above the move's 450 or so. A merge that reports hitting it is the
  sign that something else got swept into the move PR.
- **The untangling PRs go first and fast.** Each is one subsystem's imports,
  so it conflicts only with PRs in that subsystem; merge each within a day
  of opening. Step 2d (the `api` module) conflicts with every PR that adds
  an API field, so it lands on a quiet day and announces itself on the
  train's issue.
- **The next train.** A train cut before the move (say a 0.3.x patch train
  branched from `main` before the split merged) merges `main` into itself
  like any other train, and Git follows the renames the same way. One
  rule for its authors: until it has merged `main`, don't add files under
  `src/relish/`; add them after.

## Risks

| Risk | Mitigation |
|---|---|
| The shared crate grows into a second monolith, and every edit to it rebuilds everything | The rule "both sides need it and it doesn't touch a node's state"; the edge guard, then the compiler; review any new dependency in `crates/proto/Cargo.toml` |
| E8's `relish council recover` running `bun` behaves differently from today | Same flags, same output, same exit codes, tested by the existing `council_disaster_recovery` suite, which already drives `relish` against Bun |
| A moved type loses an inherent method or hits the orphan rule in a way that needs a real redesign | Steps 2b–2d find these inside the single crate, where the compiler doesn't yet object: the guard flags the edges, and each PR moves methods with types. The move PR then only finds what the file graph missed |
| Coverage shifts when tests move between crates | Before and after numbers in the move PR; the floor is never lowered |
| A suite runs a stale `relish` binary found next to `bun` | The finder compares modification times with the test binary and fails loudly; the Make targets build with `--workspace` |
| In-flight PRs fail to merge after the move | Byte-identical moves, a same-day freeze, the move script checked in to regenerate rather than resolve |
| Line counts overstate the saving (the CLI is cheap per line, Bun's generics are expensive) | They likely understate it, for the same reason; M1 on the Studio after the move records the real numbers |
| The split pushes 0.3.x's own work back | It's the train's first job, before feature work; steps 2a–2e are each under a day |

## Alternatives considered

- **Cargo features instead of crates** (`--no-default-features --features
  relish-only` to compile out Bun). Rejected: a feature is still one crate,
  so the unit-test binary still has one front end and nextest still sees
  one package for `rdeps()`. It would need `#[cfg(feature = …)]` on every
  edge this plan lists, which is the same untangling work with none of the
  enforcement, and `cargo build --workspace --all-features` would hide every
  mistake. Feature unification would also turn the code on whenever any
  dependent asked for it ([Cargo
  features](https://doc.rust-lang.org/cargo/reference/features.html#feature-unification)).
- **Filtering tests only.** Already done in
  [#1720](https://github.com/reliaburger/reliaburger/issues/1720); it saves
  test time, which was one minute, and not compile time, which is 23.5.
- **Let `relish` depend on `reliaburger`.** The easiest split: move
  `src/relish/` out and depend on the rest. Rejected: the relish-only job
  would still compile 293,000 production lines of Bun as a library, so it
  saves the test harness's codegen but not the front end, perhaps a third of
  the compile rather than three quarters. It also keeps the backwards edges
  legal, so they'd grow back.
- **Split more finely** (a crate per subsystem: `council`, `grill`, `pickle`,
  the dashboard, as F11 sketched). Rejected for now: the 71,000-line knot
  means each extra crate needs its own untangling at least as big as E1–E9,
  and nothing measured says which subsystem would pay. Three crates take the
  win the measurement points at. The guard's machinery makes a later split
  of `reliaburger` cheaper to plan; M1 after this move says whether it's
  worth it.
- **`crates/bun/` with the root as a virtual workspace.** Cleaner on paper,
  but it moves the other 300,000 lines, every `tests/*.rs`, `benches/`,
  `fuzz/` and 110 book paths for no compile-time gain, and every in-flight
  PR then rides a rename. Rejected; D2 can revisit the name.
- **A separate repository for `relish`.** Rejected: one version, one
  changelog, one release and one source bundle are worth more than any
  isolation it would add.
- **Waiting for artifact dependencies** to solve `CARGO_BIN_EXE_relish`
  across packages. Rejected: unstable, with no date; the sibling-path finder
  is ten lines.

## Decisions for the maintainer

| | Question | Recommendation |
|---|---|---|
| D1 | The shared crate's name | `reliaburger-proto` in `crates/proto/` (the protocol and what both ends of it need); a burger name if you'd rather |
| D2 | Rename the root package to `bun`? | Not now; it'd rewrite every `use reliaburger::` in `tests/`, `benches/` and `fuzz/` for no build gain |
| D3 | `relish council recover` and `reenrol` run the `bun` beside them | Yes: they only make sense on a node, where `bun` is installed |
| D4 | `relish bench --scenario`'s image cache | A small `oci-client` cache in `relish`; if it grows, move the scenario to a `bun bench` subcommand |
| D5 | The Apple runtime's tests: a second small pass on the hosted Mac, or the Studio runner only | A second pass, until the Studio runner (F7) exists |
| D6 | The edge guard: land it in the 0.3.x train first, or earlier on a 0.2.x train to stop the knot growing now | Earlier, if a train has room: it's one test and a data file, and it moves no code |

## Sources

- Issue [#1725](https://github.com/reliaburger/reliaburger/issues/1725), this plan's issue and the maintainer's 7 November decision
- Issue [#1673](https://github.com/reliaburger/reliaburger/issues/1673), the 0.2.8 speed work and the relish-only macOS measurement
- Draft PR [#1699](https://github.com/reliaburger/reliaburger/pull/1699) and its [run 42349545599](https://github.com/reliaburger/reliaburger/actions/runs/42349545599)
- Issue [#1720](https://github.com/reliaburger/reliaburger/issues/1720), the relish-only macOS job
- [A faster test and feedback loop](2026-11-06-plan-faster-feedback.md) (#1614), F11, M1 and D6
- [0.3.0's merge train, #490](https://github.com/reliaburger/reliaburger/pull/490)
- [Merge trains and their CI](../releasing.md#merge-trains-and-their-ci)
- The code, at [59257ec16 on `release-0-2-8`](https://github.com/reliaburger/reliaburger/tree/59257ec16): [`Cargo.toml`](https://github.com/reliaburger/reliaburger/blob/59257ec16/Cargo.toml), [`build.rs`](https://github.com/reliaburger/reliaburger/blob/59257ec16/build.rs), [`src/lib.rs`](https://github.com/reliaburger/reliaburger/blob/59257ec16/src/lib.rs), [`src/config/node.rs`](https://github.com/reliaburger/reliaburger/blob/59257ec16/src/config/node.rs), [`src/config/validate.rs`](https://github.com/reliaburger/reliaburger/blob/59257ec16/src/config/validate.rs), [`src/lettuce/sync.rs`](https://github.com/reliaburger/reliaburger/blob/59257ec16/src/lettuce/sync.rs), [`src/smoker/scenario.rs`](https://github.com/reliaburger/reliaburger/blob/59257ec16/src/smoker/scenario.rs), [`src/relish/client.rs`](https://github.com/reliaburger/reliaburger/blob/59257ec16/src/relish/client.rs), [`src/relish/commands.rs`](https://github.com/reliaburger/reliaburger/blob/59257ec16/src/relish/commands.rs), [`src/relish/source_bundle/pack.rs`](https://github.com/reliaburger/reliaburger/blob/59257ec16/src/relish/source_bundle/pack.rs), [`src/relish/manual/assets.rs`](https://github.com/reliaburger/reliaburger/blob/59257ec16/src/relish/manual/assets.rs), [`src/bun/authz.rs`](https://github.com/reliaburger/reliaburger/blob/59257ec16/src/bun/authz.rs), [`tests/support/bun_process.rs`](https://github.com/reliaburger/reliaburger/blob/59257ec16/tests/support/bun_process.rs)
- CI and release: [`.github/workflows/ci.yml`](https://github.com/reliaburger/reliaburger/blob/59257ec16/.github/workflows/ci.yml), [`.github/workflows/build.yml`](https://github.com/reliaburger/reliaburger/blob/59257ec16/.github/workflows/build.yml), [`Makefile`](https://github.com/reliaburger/reliaburger/blob/59257ec16/Makefile), [`scripts/ci/macos_selection.py`](https://github.com/reliaburger/reliaburger/blob/59257ec16/scripts/ci/macos_selection.py), [`scripts/ci/coverage_owner.py`](https://github.com/reliaburger/reliaburger/blob/59257ec16/scripts/ci/coverage_owner.py), [`scripts/ci/coverage_contract.py`](https://github.com/reliaburger/reliaburger/blob/59257ec16/scripts/ci/coverage_contract.py), [`scripts/ci/test_book.py`](https://github.com/reliaburger/reliaburger/blob/59257ec16/scripts/ci/test_book.py), [`scripts/release/test_build_workflow.py`](https://github.com/reliaburger/reliaburger/blob/59257ec16/scripts/release/test_build_workflow.py)
- The book's single-crate decision: [chapter 1, "Hello, Container"](../book/01-hello-container.md)
- Cargo: [workspaces](https://doc.rust-lang.org/cargo/reference/workspaces.html), [features and feature unification](https://doc.rust-lang.org/cargo/reference/features.html#feature-unification), [environment variables Cargo sets for crates (`CARGO_BIN_EXE_<name>`)](https://doc.rust-lang.org/cargo/reference/environment-variables.html#environment-variables-cargo-sets-for-crates), [artifact dependencies (unstable)](https://doc.rust-lang.org/cargo/reference/unstable.html#artifact-dependencies), [`--timings`](https://doc.rust-lang.org/cargo/reference/timings.html)
- Rust: [inherent and trait implementations, and the orphan rule](https://doc.rust-lang.org/reference/items/implementations.html#orphan-rules), [`use` declarations](https://doc.rust-lang.org/reference/items/use-declarations.html), [visibility and `pub(crate)`](https://doc.rust-lang.org/reference/visibility-and-privacy.html)
- [nextest filtersets, including `rdeps()`](https://nexte.st/docs/filtersets/reference/)
- [rust-embed 8](https://docs.rs/rust-embed/8/rust_embed/)
- Git: [`git merge`'s `ort` strategy and rename detection](https://git-scm.com/docs/git-merge#Documentation/git-merge.txt-ort), [`merge.renameLimit`](https://git-scm.com/docs/git-config#Documentation/git-config.txt-mergerenameLimit)
