# Phase 14: Self-Upgrade — Detailed Implementation Plan (July 2026)

**Status:** planning complete, implementation not started.
**Roadmap section:** `docs/roadmap.md` lines 640–674 ("Phase 14: Self-Upgrade, Rolling Binary Replacement").
**Primary design source:** `docs/design/agent-bun.md` §4.4 (`UpgradeConfig`), §4.5 (binary layout), §5.5 (self-upgrade sequence). Whitepaper §20.
**Book chapter:** `docs/book/14-changing-the-tyres.md` ("Changing the Tyres at Full Speed") — currently a 3-line stub. Written alongside each step, not at the end.

This plan is written to be executed without re-deriving decisions. Every step lists the files to touch, the types to define, the tests to write **first**, the book section to write, and the pitfalls that will otherwise cost a day of debugging. Where this plan deviates from the design docs, the deviation is called out explicitly with a rationale (§3). Follow the plan; if you hit a genuine contradiction with the code, stop and flag it rather than improvising.

Assumption from the project owner: other missing wiring (see `docs/plans/review-2026-07.md`) is being fixed separately. This plan only depends on subsystems that already exist and work: gossip (mustard), Raft (council), the bun HTTP API, ProcessGrill/RunC, and the Pickle blob store. Where Phase 14 would otherwise depend on unwired plumbing, this plan routes around it (see D5, D6).

---

## 0. Verified current state (do not re-explore; these are checked facts)

- **Binaries** (`Cargo.toml`): `bun` (`src/bin/bun.rs`, node agent), `relish` (`src/bin/relish.rs`, CLI), `testapp` (test HTTP server). Version comes from `env!("CARGO_PKG_VERSION")`, currently `0.1.0`.
- **No upgrade code exists.** The only marker is `src/config/node.rs:32` — `// TODO(Phase 14): upgrades section`. No `upgrade` module, no `relish upgrade` subcommand, no version-comparison code.
- **Gossip** (`src/mustard/`): `GossipMessage` carries a protocol `version: u8` (`GossipMessage::VERSION = 1`, `src/mustard/message.rs`) and an HMAC. `MembershipUpdate` (`src/mustard/message.rs:126`) has **no binary-version field** and is bincode-encoded — bincode is not self-describing, so adding a field breaks decoding for older binaries mid-rolling-upgrade. Do not add fields to it in this phase (see D6).
- **Raft** (`src/council/`): `council.write(RaftRequest)` proposes; non-leaders get `ForwardToLeader { leader_id }`. `RaftRequest` is a bincode-encoded enum (`src/council/types.rs:97`) — **new variants must be appended at the end, never inserted or reordered** (bincode encodes the variant index). `DesiredState` (`src/council/types.rs:190`) snapshots as JSON, so new fields there are safe with `#[serde(default)]`. Durable log at `{data_dir}/raft/log.redb`, snapshot at `{data_dir}/raft/snapshot.redb` (redb).
- **Reporting tree** (`src/reporting/`): upstream-only. `ReportingMessage` (`src/reporting/types.rs:125`) has variants `Report`, `Ack`, `AggregatedReport`, `MetricsRollup` — there is **no leader→worker directive path**. The design doc's "upgrade directive via reporting tree" cannot be implemented as written (see D5).
- **Deploy orchestrator** (`src/meat/orchestrator.rs`, `src/meat/deploy_types.rs`): `DeployPhase` state machine (Pending → RunningPreDeps → Rolling → Completed / Reverting → RolledBack / Failed / Halted) with per-step `StepPhase`. This is the structural template for the cluster upgrade state machine.
- **Container runtimes** (`src/grill/`): `ProcessGrill` holds `child: Option<tokio::process::Child>` in memory only (`src/grill/process.rs:21`) — nothing survives a process restart. `RunC` drives containers with a **foreground `runc run` child** (`src/grill/runc.rs:302`), always passing `--root {state_dir}`. There is **no reattach/adoption logic anywhere**; on agent restart the supervisor sees empty runtime state and restarts everything. Phase 14 adds real reattach (see D1).
- **HTTP API** (`src/bun/api.rs`): axum on `:9117`, bearer-token auth middleware from `sesame::auth`, thin handlers that send `AgentCommand` (`src/bun/agent.rs:73`) over an mpsc channel and await a oneshot reply.
- **Signing infra**: `ring = "0.17"` is already a dependency and provides Ed25519 (`ring::signature::Ed25519KeyPair`, `UnparsedPublicKey` with `&ring::signature::ED25519`). Image signing (`src/pickle/signing.rs`) uses ECDSA P-256 over manifest digests — a reference for structure, but binary signing uses Ed25519 per the design doc, so it's a new module, not a reuse.
- **Pickle blob store**: content-addressed blob storage with an HTTP Distribution API, already wired and listening. Used here to distribute the new binary (leader pushes, nodes pull by digest).
- **Tests**: `tests/integration.rs` has a single-node in-process `TestHarness`; `tests/agent_cluster.rs` has an in-memory multi-node harness. **No test spawns the real compiled binary today.** Integration tests can locate it via `env!("CARGO_BIN_EXE_bun")`.
- **Dependencies to add**: `semver = "1"` (dtolnay's crate; do not hand-roll pre-release ordering). Nothing else.

---

## 1. Scope

### In scope

1. `upgrade` module (`src/upgrade/`): version handling, dual Ed25519 signature verification, binary store with atomic symlink swap, retention GC, node-level upgrade state machine with automatic revert, exec-in-place.
2. **Workload reattach across the binary swap**, for ProcessGrill (all platforms) *and* RunC (Linux) — decision D1. Containers/processes keep running while bun replaces itself; the new bun adopts them instead of restarting them.
3. Cluster-level rolling orchestration driven by the leader: workers (configurable parallelism) → council members (one at a time, quorum-checked) → leadership transfer → former leader last. State persisted in Raft; survives leader changes.
4. Automatic node-level rollback (crash-loop detection reverts the symlink) and leader-side pause on failure. Cluster-wide rollback is an explicit command, same rolling order.
5. Full `relish upgrade` command set from `docs/design/cli-relish.md`: `check`, `start <version>` (`--binary`, `--parallel`), `plan <version>` (`--cluster-size`), `status`, `rollback [version]`, `resume`.
6. Release metadata fetch (`upgrade check`) and binary download + verify + Pickle distribution.
7. All roadmap Phase 14 tests (unit + 5 integration scenarios), a real-binary test harness, book chapter 14, docs/progress/README updates.

### Out of scope (write `// TODO(Phase N)` where the seam appears)

- AppleGrill container reattach (macOS containers). ProcessGrill covers macOS testing; Apple Container adoption gets `// TODO(Phase 14 follow-up)`.
- systemd unit files / packaging. The supervisor contract is documented (§4.4); shipping a `.service` file is an install-tooling concern.
- Signed release *metadata* (the per-binary dual signatures are the trust anchor; metadata travels over HTTPS).
- Delta/patch upgrades, downgrade-compatibility checks of Raft snapshots beyond the serde rules in D11.
- `relish upgrade pause` (the leader pauses automatically on failure; `resume` un-pauses; an explicit pause command is trivial to add later and not in the roadmap).

### Decisions locked in with the project owner (2026-07-06)

| ID | Decision |
|----|----------|
| D1 | Full reattach in Phase 14: ProcessGrill (pidfile-based, cross-platform) **and** RunC (adoption via `runc state`, Linux). |
| D2 | External supervisor per the design doc: bun `exec()`s in place; crash-recovery relies on systemd in production and the test harness in tests. Startup-side logic detects a failed upgrade and reverts the symlink. No built-in watchdog process. |
| D3 | Runtime version override for tests — refined to a **sidecar file** (`{binary}.version`), not an env var. Rationale in D3 below. |
| D4 | Full CLI command set from cli-relish.md. |

---

## 2. Architecture overview

```
                        ┌────────────────────────────────────────────┐
 relish upgrade start   │ LEADER (bun, runs cluster orchestrator)    │
 ──HTTP──────────────►  │  1. download / --binary, verify dual sigs  │
                        │  2. push binary blob to Pickle             │
                        │  3. write ClusterUpgradeState to Raft      │
                        │  4. loop: pick next node per rolling order │
                        │     ──HTTP directive──► node /v1/upgrade   │
                        │     poll node /v1/version + /v1/health     │
                        │     check Raft quorum before council steps │
                        │  5. transfer leadership, upgrade self last │
                        └────────────────────────────────────────────┘
                                          │ directive (HTTP, token-authed)
                                          ▼
                        ┌────────────────────────────────────────────┐
                        │ EACH NODE (UpgradeManager inside bun)      │
                        │  a. fetch blob from Pickle by sha256       │
                        │  b. verify: sha256 + embedded sig +        │
                        │     external sig (node.toml key)           │
                        │  c. write staging binary + .sig sidecar    │
                        │  d. write marker.json (pre-upgrade         │
                        │     inventory, boot_attempts=0)            │
                        │  e. atomic symlink swap                    │
                        │  f. execv() new binary — PID unchanged,    │
                        │     workload processes remain children     │
                        │  g. new bun: read marker, adopt workloads, │
                        │     rejoin gossip, self-verify, commit,    │
                        │     report healthy                         │
                        │  crash-loop? supervisor restarts us, we    │
                        │  see boot_attempts ≥ max → revert symlink, │
                        │  exec previous binary, report failure      │
                        └────────────────────────────────────────────┘
```

Two state machines, deliberately separate:

- **Node-level** (`UpgradeMarker.phase`): `Staged → Executed → Verifying → Committed`, failure path `Verifying → RevertPending → Reverted`. Lives in a marker file on disk because it must survive `exec()` and crashes — Raft is not available during the swap.
- **Cluster-level** (`ClusterUpgradeState.phase`): `Preparing → UpgradingWorkers → UpgradingCouncil → TransferringLeadership → UpgradingLeader → Completed`, plus `Paused` and `RollingBack → RolledBack`. Lives in Raft (`DesiredState.active_upgrade`) because it must survive leader changes — that is precisely how "leader upgrades last" works: leadership moves to an upgraded node, whose orchestrator resumes from the Raft state and directs the former leader.

---

## 3. Key decisions and deviations from the design docs

**D1 — Full reattach (ProcessGrill + RunC).** The roadmap test "upgrade a single node, containers survive" requires it, and the design doc assumes a reconnect capability (§5.5 step 9b) that doesn't exist. Two mechanisms:
- `exec()` replaces the process image but keeps the PID, open children, and parent/child relationships. Workload processes spawned by the old bun (ProcessGrill children, `runc run` foreground children) are *still children of the new bun*. They keep running through the swap with zero interruption.
- What's lost is the in-memory bookkeeping (`tokio::process::Child` handles). So each runtime writes a per-instance **runtime record** to disk at start, and on startup the new bun **adopts** live instances from those records instead of restarting them.

**D2 — External supervisor.** Production runs bun under systemd (`Restart=always`). Tests run bun under a harness loop that respawns on exit. Bun itself contains only the *startup-side* recovery logic (marker inspection, symlink revert, exec of the previous version). This matches agent-bun.md §5.5 step 10 exactly and keeps bun single-process.

**D3 — Version override via sidecar file, not env var.** The original idea (env `RELIABURGER_VERSION_OVERRIDE`) is broken for exec-in-place: `execv` preserves the environment, so the new binary would inherit the *old* version string and report the wrong version; likewise a supervisor that restarts the reverted binary would pass the wrong value. Instead: at startup bun resolves its own executable path (`std::env::current_exe()` then `fs::canonicalize` — canonicalise explicitly, because `current_exe()` resolves symlinks on Linux via `/proc/self/exe` but may not on macOS) and, **in debug builds only** (`cfg!(debug_assertions)`), checks for a sidecar file `{resolved_exe}.version`; if present, its trimmed contents are the reported version. Otherwise `CARGO_PKG_VERSION`. Tests copy the same compiled binary to `bun-v0.1.0` / `bun-v0.2.0` and drop matching `.version` files next to them. Release builds never read sidecars.

**D4 — Full CLI set.** All eight forms from cli-relish.md §Upgrades. The roadmap milestone's `relish upgrade --version v0.2.0` spelling is superseded by the design doc's `relish upgrade start <version>`; the book notes this.

**D5 — Upgrade directives over HTTP, not the reporting tree.** The design doc says "receive upgrade directive from the leader (via reporting tree)", but the reporting tree has no downstream path (`ReportingMessage` is report/ack only) and retrofitting one is a protocol change with bincode-compat risk. Every node already runs a token-authenticated HTTP API, and the leader knows every node's address from gossip membership. The leader POSTs directives to each node's `/v1/upgrade/apply`. Same trust model as `relish` → node. Document the deviation in the book and in agent-bun.md (§10, doc-update step).

**D6 — Version discovery over HTTP, not gossip.** Adding `binary_version` to `MembershipUpdate` or `StateReport` breaks bincode decoding for old nodes mid-upgrade — the exact window where mixed versions must interoperate. Instead the leader polls `GET /v1/version` on each node (cheap, only during upgrades) and `relish upgrade status`/`check` aggregate the same way. `// TODO(Phase 14 follow-up): carry binary_version in gossip once the wire format is version-negotiated.`

**D7 — Binary distribution via Pickle blobs.** Per the design doc. The leader stores the verified binary as a content-addressed blob (its sha256 is already the directive's `binary_hash`); nodes fetch it from the registry endpoint. No new distribution channel. Fallback if Pickle wiring turns out broken in practice: a `GET /v1/upgrade/binary/{sha256}` streaming endpoint on the leader — but do not build this preemptively.

**D8 — `semver` crate** for version parsing/ordering (handles pre-release precedence correctly, e.g. `1.0.0-rc.1 < 1.0.0`). Wrap in a newtype; accept an optional leading `v`.

**D9 — Ed25519 via `ring`** (already a dependency). Signatures are detached, over the raw binary bytes. Two signatures: one against a **release key set compiled into the binary**, one against the **external key** from `node.toml` (`upgrades.external_signing_key`, `ed25519:`-prefixed base64). Network upgrades require both; air-gapped `--binary` upgrades require only the embedded signature (per the `UpgradeConfig` doc comment in agent-bun.md).

**D10 — Debug-only test hooks.** Three hooks, all gated on `cfg!(debug_assertions)` so release binaries ignore them: the `.version` sidecar (D3), a `{resolved_exe}.fail-boot` sidecar (bun exits with code 101 immediately after config load — used by the rollback integration test), and `upgrades.release_keys_override` in node.toml (lets integration tests substitute a test-generated release keypair for the compiled-in keys; release builds log a warning and ignore it).

**D11 — Serialisation compatibility rules** (these are load-bearing; violating them bricks mixed-version clusters):
- `RaftRequest`: append new variants at the end only.
- `DesiredState`: new fields with `#[serde(default)]` (JSON snapshot — safe).
- `MembershipUpdate`, `ReportingMessage`, `GossipMessage`: **frozen** this phase.
- New HTTP endpoints are inherently fine (old nodes 404, callers handle it).

**D12 — First-upgrade bootstrap caveat.** A v0.1.0 cluster contains no upgrade code, so the *first* deployment of upgrade-capable binaries is a manual rollout. Everything here targets upgrades *from* the first upgrade-capable version onward. Tests sidestep this via D3 (both "versions" are the same upgrade-capable build). State this plainly in the book — it's an honest and instructive limitation.

**D13 — Leadership transfer.** openraft 0.9 has no `transfer_leader`. Implement transfer as: leader picks an already-upgraded council member, POSTs to that node's internal `POST /v1/cluster/elect` endpoint, whose handler calls `raft.trigger().elect()`; the leader then waits (poll `council.current_leader()`) until leadership moves, with timeout + retry against a different member. If openraft is later bumped to a version with native transfer, swap the implementation — the seam is one function, `orchestrator::transfer_leadership()`.

---

## 4. On-disk artefacts and configuration

### 4.1 Binary directory layout (design doc §4.5, adapted to the real binary name)

```
{binary_dir}/                      # default: parent dir of the resolved current exe
  bun                  -> bun-v0.2.0        (symlink, the "entry point")
  bun-v0.1.0                                (previous version, kept for rollback)
  bun-v0.2.0                                (current version)
  bun-v0.2.0.sig                            (JSON signature envelope, §4.3)
```

The symlink name equals the binary file stem (`bun`). Versioned filenames are `bun-v{semver}`. Retention GC (default keep 3) deletes oldest `bun-v*` files **and** their `.sig` sidecars, never the symlink target or the rollback target recorded in a live marker.

**Atomic swap:** create the new symlink at `{binary_dir}/.bun.tmp-{pid}` with `std::os::unix::fs::symlink`, then `std::fs::rename` over `{binary_dir}/bun`. `rename(2)` is atomic on POSIX; there is never a moment without a valid entry point. Never `remove_file` + `symlink`.

### 4.2 Upgrade state directory

```
{data_dir}/upgrade/
  marker.json          # UpgradeMarker — exists only while an upgrade is in flight on this node
  history.jsonl        # append-only log of completed/reverted node upgrades (for `status` and the book)
{data_dir}/instances/
  {namespace}_{app}_{idx}.json   # InstanceRecord — one per running workload (written by grill, §8 step 5)
```

### 4.3 Signature envelope (`{binary}.sig`)

```json
{
  "schema": 1,
  "sha256": "hex…",
  "embedded": "base64 Ed25519 signature (release key set)",
  "external": "base64 Ed25519 signature or null (air-gapped)"
}
```

### 4.4 Supervisor contract (document in `docs/README.md` and the book)

The process manager running bun must: restart it whenever it exits (any code), not impose a start-rate limit tighter than 3 starts/60s, and pass through the original argv. bun handles everything else (upgrade markers, symlink reverts). Example systemd unit goes in the book chapter, not in this phase's code.

### 4.5 `node.toml` additions

```toml
[upgrades]
# Ed25519 public key, "ed25519:" + base64. Required for network upgrades;
# air-gapped (--binary) upgrades work without it.
external_signing_key = "ed25519:BASE64…"   # Option<String>, default None
retain_versions = 3                         # u32, default 3
release_url = "https://releases.reliaburger.dev/metadata.json"  # String, default as shown
binary_dir = "/usr/local/bin"               # Option<PathBuf>, default: dir of resolved current exe
boot_grace_secs = 30                        # new binary must survive this long
gossip_rejoin_secs = 60                     # …and rejoin gossip within this (cluster mode)
max_boot_attempts = 2                       # crash-loops beyond this trigger revert
# Debug builds only (ignored + warned in release builds): replaces the
# compiled-in release key set. Used by integration tests.
release_keys_override = ["ed25519:BASE64…"] # Option<Vec<String>>, default None
```

Rust struct `UpgradeSection` in `src/config/node.rs`, all fields `#[serde(default = …)]`, replacing the `TODO(Phase 14)` comment. Validation (extend the existing `NodeConfig` validation): `external_signing_key` and each `release_keys_override` entry must parse as `ed25519:` + valid base64 of exactly 32 bytes; `retain_versions >= 1`; `binary_dir` absolute if set.

---

## 5. Core types (define these verbatim; adjust only if compilation forces it)

All in `src/upgrade/types.rs` unless noted. Standard derives per house rules (`Debug, Clone` everywhere; `Serialize, Deserialize` on everything below since it all crosses a wire or hits disk; `PartialEq, Eq` where tests compare).

```rust
/// A reliaburger binary version. Wraps semver; accepts an optional leading "v".
/// Displays with the leading "v" (matches file names and CLI usage).
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct BinaryVersion(semver::Version);
// impl FromStr (strip optional 'v', parse, error type UpgradeError::InvalidVersion),
// impl Display ("v{}"), plus fn file_name(&self, stem: &str) -> String  ("bun-v1.2.3").
// Serialize/Deserialize as the display string (serde with = "…" helpers or manual impls).

/// Everything a node needs to perform one upgrade. Sent leader -> node as JSON.
pub struct UpgradeDirective {
    pub upgrade_id: String,            // uuid-ish; leader-generated; idempotency key
    pub target_version: BinaryVersion,
    pub binary_sha256: String,         // hex; also the Pickle blob digest
    pub embedded_signature: String,    // base64
    pub external_signature: Option<String>, // base64; None only for air-gapped
    pub source: BinarySource,
}

pub enum BinarySource {
    /// Fetch from the Pickle registry on this node/cluster, digest = binary_sha256.
    Pickle { registry_address: String },
    /// The binary is already at this local path (single-node --binary flow).
    LocalFile { path: PathBuf },
}

/// Node-local upgrade state. Persisted at {data_dir}/upgrade/marker.json.
/// Exists only while an upgrade is in flight on this node.
pub struct UpgradeMarker {
    pub schema: u32,                   // 1
    pub upgrade_id: String,
    pub previous_version: BinaryVersion,
    pub previous_binary: String,       // file name within binary_dir, e.g. "bun-v0.1.0"
    pub target_version: BinaryVersion,
    pub target_binary: String,
    pub phase: MarkerPhase,
    pub boot_attempts: u32,            // incremented by each post-swap startup
    pub pre_upgrade_instances: Vec<InstanceInventory>, // for self-verification
}

pub enum MarkerPhase {
    Staged,        // binary + sig written, symlink NOT yet swapped
    Executed,      // symlink swapped, exec called (written just before execv)
    Verifying,     // new binary booted, running self-checks
    RevertPending, // verification failed / crash loop; revert on next startup
    Reverted,      // old binary back in control; awaiting failure report to leader
}
// Committed is represented by DELETING the marker (single source of truth:
// no marker == no upgrade in flight). history.jsonl records the outcome.

pub struct InstanceInventory {
    pub namespace: String,
    pub app_name: String,
    pub instance_id: u32,
    pub pid: u32,
}

/// Cluster-wide upgrade state. Lives in Raft: DesiredState.active_upgrade.
pub struct ClusterUpgradeState {
    pub upgrade_id: String,
    pub target_version: BinaryVersion,
    pub binary_sha256: String,
    pub embedded_signature: String,
    pub external_signature: Option<String>,
    pub parallel: u32,                 // worker batch size, default 1
    pub direction: UpgradeDirection,   // Upgrade | Rollback
    pub phase: ClusterUpgradePhase,
    pub nodes: Vec<NodeUpgradeRecord>, // fixed at start from gossip membership
}

pub enum UpgradeDirection { Upgrade, Rollback }

pub enum ClusterUpgradePhase {
    Preparing,                 // leader verifying + pushing blob to Pickle
    UpgradingWorkers,
    UpgradingCouncil,
    TransferringLeadership,
    UpgradingLeader,           // former leader, directed by the new leader
    Completed,
    Paused { reason: String }, // failure or operator action; `resume` re-enters
}

pub struct NodeUpgradeRecord {
    pub node_id: String,
    pub address: String,               // API address for directives/polling
    pub role: NodeRole,                // Worker | Council | Leader (at plan time)
    pub from_version: Option<BinaryVersion>, // None until first /v1/version poll
    pub phase: NodeUpgradePhase,
}

pub enum NodeRole { Worker, Council, Leader }

pub enum NodeUpgradePhase {
    Pending,
    Directed,      // directive accepted (2xx from /v1/upgrade/apply)
    Verifying,     // node reports Executed/Verifying, leader polling
    Healthy,       // /v1/version == target && /v1/health ok && gossip Alive
    Failed { reason: String },
    RolledBack,
}
```

Error enum `src/upgrade/error.rs`, thiserror, lowercase messages, no trailing stops:

```rust
pub enum UpgradeError {
    InvalidVersion { input: String },
    HashMismatch { expected: String, actual: String },
    EmbeddedSignatureInvalid,
    ExternalSignatureInvalid,
    ExternalKeyRequired,               // network upgrade without external_signing_key
    AlreadyInFlight { upgrade_id: String },
    UnknownVersion { version: BinaryVersion },   // rollback target not on disk
    Io(#[from] std::io::Error),
    // …grow as needed; carry context per house style
}
```

---

## 6. HTTP API additions (`src/bun/api.rs`)

| Route | Auth | Purpose |
|---|---|---|
| `GET /v1/version` | none (like `/v1/health`) | `{ "version": "v0.1.0", "binary_path": "...", "upgrade_in_flight": bool }`. Leader polls this; keep it dependency-free and fast. |
| `POST /v1/upgrade/apply` | admin token | Body: `UpgradeDirective`. Node-level upgrade. Responds `202` immediately after validation + `AlreadyInFlight` check; work proceeds async. Idempotent on `upgrade_id` (re-POST of same id while in flight → `202` again, no-op). |
| `GET /v1/upgrade/status` | token | Node-level: marker (or "idle") + last `history.jsonl` entries. |
| `POST /v1/upgrade/rollback` | admin token | Node-level revert to `previous_binary` (or explicit version in body). Same staging-free swap+exec path. |
| `POST /v1/upgrade/start` | admin token | Cluster-level. Body: `{ target_version, parallel, source }`. Handler must run on the leader; non-leaders respond with the same forward-to-leader shape used by existing council writes (follow whatever `/v1/apply` does — do not invent a new convention). |
| `GET /v1/upgrade/cluster` | token | Cluster-level `ClusterUpgradeState` read from Raft (any node can serve it). |
| `POST /v1/upgrade/resume` | admin token | Clears `Paused`, re-enters the phase that paused. Leader-only like `start`. |
| `POST /v1/upgrade/cluster-rollback` | admin token | Starts a `direction: Rollback` cluster run. Leader-only. |
| `POST /v1/cluster/elect` | admin token | Internal: calls `raft.trigger().elect()` on this node (D13). |

Handlers follow the existing pattern: build an `AgentCommand`, send over `cmd_tx`, await oneshot. New `AgentCommand` variants: `UpgradeApply { directive, response }`, `UpgradeStatus { response }`, `UpgradeRollback { version: Option<BinaryVersion>, response }`. Cluster-level handlers talk to the council/orchestrator handle directly (they don't need the agent loop).

---

## 7. CLI additions (`src/bin/relish.rs` + `src/relish/`)

New `#[derive(Subcommand)] enum UpgradeAction` hung off the main `Command` enum (match the existing `TokenAction`/`SecretAction` style at `src/bin/relish.rs:231`):

```
relish upgrade check                    # fetch release_url metadata, compare to cluster versions
relish upgrade start <version>          # network upgrade
relish upgrade start --binary <path>    # air-gapped; relish uploads the file to the leader
relish upgrade start <version> --parallel <n>
relish upgrade plan <version> [--cluster-size <n>]   # offline preview: order, batches, duration estimate
relish upgrade status                   # cluster state + per-node table
relish upgrade rollback [<version>]
relish upgrade resume
```

Implementation lives in `src/relish/upgrade.rs` (new file, logic separated from the binary for testability, like the other relish modules). Client methods go on `src/relish/client.rs`. `plan` is computed client-side from `GET /v1/cluster/nodes` (or `--cluster-size` for a hypothetical cluster) — pure function, snapshot-tested with insta. `--binary` uploads via the Pickle blob push API, then calls `/v1/upgrade/start` with `source: Pickle`.

Air-gapped signing inputs: `--binary <path>` expects `<path>.sig` (the §4.3 JSON envelope) next to it; `--sig <path>` overrides the location.

Dev tooling for producing signatures (used by tests and eventually the release process), added to the existing `DevAction` enum: `relish dev keygen --out <dir>` (writes `release.key` / `release.pub`, Ed25519 via ring) and `relish dev sign-binary --key <path> <binary> [--external-key <path>]` (writes `<binary>.sig` envelope). Both are thin wrappers over `upgrade::signing` library functions.

---

## 8. Implementation steps

Rules for every step, no exceptions:

- **Tests first.** Write the listed failing tests, watch them fail, implement, watch them pass.
- **Book alongside.** Each step names its Chapter 14 section; write it in the same sitting. First-use Rust syntax must be explained (the target reader knows C/Python/Go, not Rust).
- **`make ci` before every commit** (fmt --check, clippy -D warnings, test). One commit per step, message noted below. Show the commit details and ask the owner before committing; never amend.
- House rules apply: no `.unwrap()` outside tests, thiserror in the library, `?` everywhere, tokio sync primitives only, no new `std::sync::Mutex`.

### Step 1 — `BinaryVersion` and the version override (small, unblocks everything)

**Files:** `Cargo.toml` (+`semver = "1"`), new `src/upgrade/mod.rs`, `src/upgrade/types.rs`, `src/upgrade/error.rs`, new `src/upgrade/version.rs`; register `pub mod upgrade;` in `src/lib.rs`.

**Implement:** `BinaryVersion` (§5) and `resolve_running_version(exe_path: &Path) -> BinaryVersion`: canonicalise, check `{path}.version` sidecar in debug builds (D3), fall back to `CARGO_PKG_VERSION`. Wire into `src/bin/bun.rs` startup banner and keep the resolved value on the agent for `/v1/version` later.

**Unit tests** (`#[cfg(test)]` in `version.rs`):
- `parses_with_and_without_leading_v`
- `rejects_garbage_and_empty_versions`
- `orders_semver_correctly` (0.1.0 < 0.2.0 < 0.10.0 < 1.0.0)
- `prerelease_sorts_before_release` (1.0.0-rc.1 < 1.0.0; rc.1 < rc.2)
- `display_roundtrips_with_v_prefix`
- `file_name_matches_layout` (`bun-v1.2.3`)
- `sidecar_overrides_version_in_debug_builds` (tempdir + fake exe file)

**Book §14.1 "What version am I?"** — why semver, pre-release precedence, newtype pattern recap, `FromStr`/`Display` traits, the exec-inherits-environment trap that motivated the sidecar (this is a genuinely good teaching moment — tell it as the war story it will otherwise become).

**Commit:** `Add BinaryVersion and version resolution (Phase 14, 1/12)`

### Step 2 — Dual-signature verification and signing tooling

**Files:** new `src/upgrade/signing.rs`, new `src/upgrade/keys.rs`; `src/relish/dev.rs` + `src/bin/relish.rs` (DevAction additions); `src/config/node.rs` (the `[upgrades]` section, §4.5, including validation).

**Implement:**
- `keys.rs`: `pub const EMBEDDED_RELEASE_KEYS: &[&str] = &["ed25519:…"];` — generate one real project keypair now with `relish dev keygen`, commit the **public** key here, and store the private key outside the repo (note in the book + a `docs/README.md` release-process stub). `pub fn release_keys(config: &UpgradeSection) -> Vec<PublicKey>` applies the debug-only override (D10) with a `tracing::warn!` when ignored in release builds.
- `signing.rs`: `parse_public_key("ed25519:BASE64") -> Result<[u8;32]>`, `sign(key_pem_or_raw, bytes) -> [u8;64]`, `verify_embedded(keys, bytes, sig)`, `verify_external(key, bytes, sig)`, and the umbrella `verify_binary(bytes, envelope: &SignatureEnvelope, release_keys, external_key: Option<…>, network: bool) -> Result<(), UpgradeError>` — enforcing sha256 match first, then embedded (any key in the set), then external (mandatory iff `network`). Use `ring::signature::{Ed25519KeyPair, UnparsedPublicKey, ED25519}`. Hash with the existing `sha2` dependency for consistency with Pickle digests.
- `SignatureEnvelope` (the §4.3 JSON) with load/store helpers.
- `relish dev keygen` / `relish dev sign-binary` as described in §7.

**Unit tests** (`signing.rs`):
- `verifies_correct_dual_signatures`
- `rejects_wrong_hash_before_checking_signatures`
- `rejects_tampered_binary_bytes`
- `rejects_signature_from_unknown_release_key`
- `accepts_any_key_in_the_release_set` (sign with key 2 of 2)
- `network_upgrade_requires_external_signature` (`ExternalKeyRequired`)
- `airgapped_upgrade_skips_external_signature`
- `parses_ed25519_prefixed_keys_and_rejects_bad_prefix_and_bad_length`
Config tests in `node.rs`: `upgrades_section_defaults`, `rejects_invalid_external_signing_key`, `release_keys_override_requires_valid_keys`.

**Book §14.2 "Trust, but verify twice"** — why two signatures (compromised CDN vs compromised build), Ed25519 vs the ECDSA used for images, detached signatures, `ring` and the story of auditing crypto deps, `include_str!`/consts vs config, why the override is debug-gated.

**Commit:** `Add dual Ed25519 binary signing and [upgrades] config (Phase 14, 2/12)`

### Step 3 — Binary store: staging, atomic symlink swap, retention GC

**Files:** new `src/upgrade/store.rs`.

**Implement `BinaryStore`** (constructed from `binary_dir` + stem, both discovered at startup: stem = symlink file name if the running exe was reached via symlink, else exe file stem with any `-vX.Y.Z` suffix stripped):
- `stage(version, bytes, envelope) -> Result<PathBuf>` — write `bun-vX.Y.Z` (0755, `fs::set_permissions` with `PermissionsExt::from_mode` — first `unsafe`-free FFI-ish moment for the book) + `.sig`, fsync file and directory.
- `activate(version) -> Result<()>` — the temp-symlink + rename dance (§4.1). Refuse if the target file is missing or not executable.
- `current_target() -> Result<BinaryVersion>` — read the symlink, parse the version suffix.
- `installed_versions() -> Vec<BinaryVersion>` — scan `bun-v*`, ignore unparseable names.
- `garbage_collect(retain: u32, protect: &[BinaryVersion]) -> Vec<BinaryVersion>` — delete oldest beyond `retain`, never deleting `protect` entries (current symlink target + marker's previous/target), removing `.sig` sidecars too; return what was deleted for logging.

**Unit tests** (tempdir-based, no real exec):
- `stage_writes_binary_and_signature_with_exec_permissions`
- `activate_swaps_symlink_atomically` (old target still present after swap)
- `activate_refuses_missing_target`
- `current_target_parses_version_from_symlink`
- `gc_keeps_newest_n_versions` (roadmap: keep 3, install 5, expect 2 oldest gone)
- `gc_never_deletes_protected_versions`
- `gc_removes_sig_sidecars_with_binaries`
- `installed_versions_ignores_foreign_files`

**Book §14.3 "The symlink two-step"** — why symlink+rename is atomic and `rm`+`ln` is not, fsync-the-directory, Unix permissions from Rust, `PathBuf`/`Path` idioms recap.

**Commit:** `Add binary store with atomic activation and retention GC (Phase 14, 3/12)`

### Step 4 — Marker file and the rollback state machine (pure logic, no exec yet)

**Files:** new `src/upgrade/marker.rs`.

**Implement:** `UpgradeMarker` load/store (atomic write: temp + rename, same trick as Step 3) and the pure transition function the whole phase pivots on:

```rust
/// Decision taken at every bun startup, before anything else runs.
pub enum StartupDecision {
    /// No marker: normal boot.
    NormalBoot,
    /// Marker present, we are the target version: run self-verification.
    VerifyUpgrade { marker: UpgradeMarker },
    /// Marker present, we are the target version, but boot_attempts exceeded
    /// max: revert the symlink and exec the previous binary.
    RevertAndExecPrevious { marker: UpgradeMarker },
    /// Marker present and we are the PREVIOUS version (phase RevertPending or
    /// Reverted): we're back in control after a failed upgrade. Report failure,
    /// archive marker to history, continue booting.
    CompleteRevert { marker: UpgradeMarker },
    /// Marker is stale/inconsistent (unknown phase combos, unparseable):
    /// archive it with a warning and boot normally. Never brick the node.
    ArchiveStaleMarker { reason: String },
}

pub fn decide_startup(
    marker: Option<UpgradeMarker>,
    running: &BinaryVersion,
    max_boot_attempts: u32,
) -> StartupDecision
```

Rules: marker with `phase: Staged` and `running == previous` → the swap never happened (crash between stage and activate) → `ArchiveStaleMarker`. `Executed`/`Verifying` with `running == target` → increment `boot_attempts`, persist, then `VerifyUpgrade` (or `RevertAndExecPrevious` if `boot_attempts > max`). `running == previous` with `RevertPending`/`Reverted` → `CompleteRevert`. Anything else → `ArchiveStaleMarker`. Enumerate exhaustively with `match`; no catch-all arm hiding cases.

**Unit tests** (roadmap: "Rollback state machine: upgrade → verify → commit, upgrade → verify → rollback") — table-driven over `decide_startup`:
- `no_marker_boots_normally`
- `fresh_upgrade_enters_verification` (attempts 0→1)
- `second_crash_still_verifies_when_under_limit`
- `crash_loop_beyond_limit_reverts` (attempts hits max+1)
- `previous_binary_after_revert_completes_the_revert`
- `staged_but_never_swapped_is_archived_stale`
- `version_matching_neither_side_is_archived_stale`
- `marker_roundtrips_through_disk` + `corrupt_marker_is_archived_not_fatal`

**Book §14.4 "A state machine you can trust your process to"** — modelling with exhaustive enums (the CLAUDE.md pattern shown in anger), why the decision is a pure function (testable without ever crashing a real process), why "no marker" means "committed".

**Commit:** `Add upgrade marker and startup decision state machine (Phase 14, 4/12)`

### Step 5 — Workload reattach: instance records + ProcessGrill + RunC adoption (biggest step; sub-commits allowed)

This is prerequisite ground work for "containers survive". It's also independently valuable: bun crashes today restart every workload.

**Files:** new `src/grill/records.rs`; `src/grill/mod.rs` (trait + shared types); `src/grill/process.rs`; `src/grill/runc.rs`; `src/grill/mock.rs`; `src/bun/supervisor.rs`; `src/bun/agent.rs` (startup ordering).

**5a. Instance records** (`src/grill/records.rs`):

```rust
pub struct InstanceRecord {
    pub schema: u32,                  // 1
    pub namespace: String,
    pub app_name: String,
    pub instance_id: u32,
    pub runtime: RuntimeKind,         // Process | Runc | Apple
    pub pid: u32,                     // ProcessGrill: workload pid; RunC: the `runc run` child pid
    pub pid_started_at: u64,          // process start time (see below) for pid-reuse detection
    pub runc_container_id: Option<String>,
    pub log_path: PathBuf,
    pub port: Option<u16>,
    pub spec_hash: String,            // hash of the OciSpec/config used to start it
}
```

Written atomically to `{data_dir}/instances/{ns}_{app}_{idx}.json` on successful start, deleted on stop/failure-cleanup. Pid start time via `sysinfo` (already a dependency): `System::process(Pid).start_time()`. A record is *live* iff the pid exists **and** its start time matches (tolerate ±2s — some platforms round differently). This kills the pid-reuse false-positive.

**5b. Grill trait extension** (`src/grill/mod.rs`): one new method, implemented by all runtimes:

```rust
/// Attempt to adopt a previously started instance from its on-disk record.
/// Returns Ok(true) if adopted (instance is live and now tracked),
/// Ok(false) if the instance is gone (record should be deleted and the
/// instance rescheduled through the normal path).
async fn adopt(&self, instance: &InstanceId, record: &InstanceRecord) -> Result<bool, GrillError>;
```

`MockGrill`: configurable per-instance answers, records adopt calls (for supervisor unit tests). AppleGrill: `Ok(false)` + `// TODO(Phase 14 follow-up): Apple Container adoption`.

**5c. ProcessGrill adoption** (`src/grill/process.rs`): liveness-check the record; if live, insert an entry with `child: None` and `adopted_pid: Some(pid)`. Everywhere the code currently uses the `Child` handle, handle the adopted case: `stop` already signals by raw pid via nix (`process.rs:197-200`) — generalise that path; `state`/`pid` use liveness polling. **Reaping:** adopted pids are usually still our children after exec (same PID), so add a reaper task in ProcessGrill: every 500ms, for each adopted pid call `nix::sys::wait::waitpid(pid, WNOHANG)`; on exit status → mark Stopped/Failed like the normal wait path; on `ECHILD` (we were fully restarted by the supervisor, so the workload was reparented away) → fall back to `kill(pid, 0)` liveness and treat disappearance as exit with unknown status. Both paths must feed the same state-transition code the `Child`-wait path uses today.

**5d. RunC adoption** (`src/grill/runc.rs`): record's pid is the foreground `runc run` child. Adoption checks: pid live (same start-time check) **and** `runc --root {state_dir} state {container_id}` reports `running`. If the runc process died but the container reports running (shouldn't happen with `runc run`, but belt and braces) → not adopted, force-delete per the existing release path (`runc.rs:117`). Same reaper approach for the adopted `runc run` pid. Log capture survives for free: stdout/err are redirected to `log_path` files (`runc.rs:26-28`), which the runc child keeps writing regardless of what bun does; ketchup just keeps tailing the same path from the record.

**5e. Supervisor integration** (`src/bun/supervisor.rs` + agent startup): on startup, *before* reconciling desired state, scan `{data_dir}/instances/`, attempt `adopt` for every record, delete records that return `false`, seed the supervisor's instance table with adopted entries (state Running, health checking resumes from the recovered spec). Only then run normal reconciliation — adopted instances must not be double-started. Restart-backoff counters start fresh (documented behaviour, matches today).

**Unit tests:**
- records: `record_roundtrips`, `liveness_rejects_reused_pid` (fake start time), `liveness_rejects_dead_pid`
- ProcessGrill (spawn real `sleep`/`testapp` processes in tests): `adopts_live_process_and_reports_running`, `adopt_returns_false_for_dead_pid`, `stop_kills_adopted_instance`, `reaper_detects_adopted_instance_exit`
- supervisor (MockGrill): `startup_adopts_recorded_instances_instead_of_restarting`, `startup_deletes_stale_records_and_reschedules`, `adopted_instances_resume_health_checks`
- RunC paths behind `#[cfg(target_os = "linux")]` mirroring existing runc test gating.

**Book §14.5 "Your children survive exec()"** — the single best systems lesson in the chapter: what `execve` preserves (PID, children, fds sans CLOEXEC) and what it destroys (all memory, including your `Child` handles); pid reuse and start-time fingerprinting; zombie reaping, `waitpid`, `ECHILD`; why Go/Python people rarely meet this and C people meet it constantly.

**Commit(s):** `Add instance records and workload adoption (Phase 14, 5/12)` — sub-commits `5a/5b`, `5c`, `5d`, `5e` acceptable if each passes `make ci`.

### Step 6 — Node-level UpgradeManager: stage → swap → exec → verify → commit

**Files:** new `src/upgrade/manager.rs`; `src/bin/bun.rs` (startup hook); `src/bun/agent.rs` (drain flag).

**Implement `UpgradeManager`** (owns `BinaryStore`, config, data-dir paths, resolved running version, original argv captured at startup):

- `apply(directive) -> Result<(), UpgradeError>`, the §5.5 node sequence:
  1. Reject if a marker exists (`AlreadyInFlight`), unless same `upgrade_id` (idempotent no-op).
  2. Obtain bytes per `BinarySource` (Pickle blob GET via reqwest, or local file read). Stream to a temp file; hash while streaming (`spawn_blocking` for the hash if buffered — but streaming hash with sha2 in the async read loop is fine and simpler; note the choice in the book).
  3. `signing::verify_binary(…, network = matches!(source, Pickle…))`.
  4. Set the agent's **drain flag** (`Arc<AtomicBool>` or a watch channel on the agent): supervisor stops *starting* new instances; running ones untouched. (Cluster-side cordoning is Step 8's job; this is defence in depth.)
  5. `store.stage(…)`; snapshot `pre_upgrade_instances` from the supervisor; write marker `Staged`.
  6. `store.activate(target)`; update marker to `Executed`; fsync.
  7. Flush logs/metrics (call the existing flush paths), `tracing::info!` a final line, then **exec**: `nix::unistd::execv(&CString::new(symlink_path)?, &original_argv_as_cstrings)`. Exec the *symlink* path (so `current_exe`+canonicalise finds the versioned file). If `execv` returns (only on error), revert the symlink, archive the marker, clear drain, return the error — the node must keep running the old version.
  - Rust/fd notes for the book and for correctness: Rust's std and tokio open everything `O_CLOEXEC`, so redb handles, listeners and sockets close atomically at exec; the new process re-opens them at startup like any boot. redb's file locks release when the fds close. There is a sub-second API/gossip blip; gossip tolerates it (incarnation bump on restart, existing behaviour).
- `run_startup(decision) -> StartupOutcome`, wired in `src/bin/bun.rs` *immediately after config load, before subsystems start*:
  - `VerifyUpgrade`: continue boot; after the API is up and (in cluster mode) gossip has rejoined, run self-verification with `tokio::time::timeout`: (a) all `pre_upgrade_instances` adopted and live (compare against supervisor state), (b) local `/v1/health` genuinely serving, (c) cluster mode: membership shows self Alive within `gossip_rejoin_secs`. Pass → append `history.jsonl`, delete marker, run `store.garbage_collect(retain, protect=[])`, clear drain. Fail → marker `RevertPending`, `std::process::exit(1)` (the supervisor restarts us; `decide_startup` sees attempts exceeded or phase RevertPending and reverts). Direct-exec revert (not exit) is also acceptable when verification fails deterministically — prefer marker+exit for one code path.
  - `RevertAndExecPrevious`: `store.activate(previous)`, marker → `RevertPending`, `execv` previous binary.
  - `CompleteRevert`: append history (failure), delete marker, keep a `failed_upgrade: Option<…>` note on the agent so `/v1/upgrade/status` and the leader's poll can see it. Continue normal boot.
  - `ArchiveStaleMarker`: rename marker to `marker.json.stale-{n}`, warn, boot.
- Also `rollback_to(version: Option<BinaryVersion>)` for `POST /v1/upgrade/rollback`: validate the target exists on disk, write a marker (previous/target swapped roles), activate, exec. No download, no signature re-check (the binary was verified when first staged; note this trust decision in the book).

Wire the three `AgentCommand` variants and the `GET /v1/version` + node-level routes from §6.

**Unit tests** (exec is untestable in-process; everything up to the exec boundary is):
- `apply_rejects_second_concurrent_upgrade`
- `apply_is_idempotent_for_same_upgrade_id`
- `apply_verifies_before_staging` (bad sig → nothing written, no marker, drain cleared)
- `apply_snapshots_running_instances_into_marker` (MockGrill/supervisor fixture)
- `verification_failure_marks_revert_pending` (drive the verify fn with a fixture where an inventory pid is dead)
- `successful_verification_commits_and_gcs` (marker deleted, history appended, old binaries pruned to retain)
- `rollback_rejects_version_not_on_disk`
Factor `apply` so the exec call is the last line behind a small `trait Execer` (test impl records the call instead of exec'ing) — one seam, no premature abstraction beyond it.

**Book §14.6 "Replacing yourself without dying"** — the exec syscall from Rust (`nix`, `CString`, why `execv` "never returns" and how the type system can't express that (`Infallible`/`!`)), CLOEXEC and what it buys us, drain semantics, why verification runs in the *new* process and reverts via marker+exit rather than clever in-process gymnastics.

**Commit:** `Add node-level UpgradeManager with exec-in-place and auto-revert (Phase 14, 6/12)`

### Step 7 — Single-node integration tests: the real-binary harness (roadmap tests 1, 2, 4, 5)

Do this **before** the cluster work — it validates the risky mechanics (exec, adoption, revert) with the smallest surface, and the harness is reused in Step 11.

**Files:** new `tests/self_upgrade.rs`, new `tests/common/real_node.rs` (or a `mod` in the test file; follow how existing tests share helpers).

**Harness (`RealNodeHarness`):**
- Layout per test (all under `tempfile::TempDir`): `bin/` (binary_dir), `data/`, `node.toml`.
- Copy `env!("CARGO_BIN_EXE_bun")` → `bin/bun-v0.1.0` and again → `bin/bun-v0.2.0`; write `.version` sidecars (`v0.1.0`, `v0.2.0`); symlink `bin/bun -> bun-v0.1.0`.
- Generate a test release keypair + external keypair in-process (library calls from `upgrade::signing`); write `node.toml` with `release_keys_override` (debug build honours it), `external_signing_key`, `binary_dir`, small timeouts (`boot_grace_secs = 5`, `gossip_rejoin_secs = 10`), ProcessGrill runtime, ephemeral `--listen` port.
- Sign `bin/bun-v0.2.0` with both keys → keep the envelope for directives (write `<binary>.sig` too).
- **Supervisor loop** (a `tokio::task` in the test): spawn `bin/bun` (the symlink) with `std::process::Command`/`tokio::process::Command`; on exit, if the harness isn't shutting down, respawn after 200ms. Expose `current_pid()`, `wait_healthy()` (poll `/v1/health`), `version()` (poll `/v1/version`).
- Directive helper: build `UpgradeDirective` with `source: LocalFile { path }` (single-node tests need no Pickle) and POST it with the admin token.

**Tests (names per roadmap):**
- `single_node_upgrade_preserves_running_containers` — start harness, deploy a `proc-*` testapp via `/v1/apply`, record its workload pid, POST the directive, wait until `/v1/version` reports `v0.2.0` and health is green, assert: workload pid unchanged **and** still serving HTTP, bun *process* pid unchanged (exec!), symlink points at `bun-v0.2.0`, marker gone, status shows the instance Running with no restart count bump.
- `single_node_rollback_reverts_to_previous_version` — after a successful upgrade, POST `/v1/upgrade/rollback`, expect `/v1/version` back to `v0.1.0`, workload still alive, symlink reverted.
- `failed_upgrade_triggers_automatic_rollback` — create `bin/bun-v0.2.0.fail-boot` (D10) before the directive. Expect: new binary exits 101, supervisor respawns it `max_boot_attempts` times, revert kicks in, `/v1/version` settles at `v0.1.0`, `/v1/upgrade/status` reports the failure, workload survived the whole circus (it was never stopped).
- `version_retention_gc_keeps_last_three` — stage bun-v0.2.0 → v0.3.0 → v0.4.0 → v0.5.0 through four sequential upgrades (copies + sidecars + signatures each); after the last commit assert `bin/` contains exactly v0.3.0, v0.4.0, v0.5.0 (+ sigs + symlink) and v0.1.0/v0.2.0 are gone.
- `upgrade_rejects_bad_external_signature` — directive with a corrupted external sig: `apply` returns an error, no marker, no symlink change, node still `v0.1.0`.

Generous timeouts (30s waits), and check how existing heavy tests serialise — if there's no convention, these are process-isolated by tempdirs and can run in parallel.

**Book §14.7 "Testing a program that replaces itself"** — `CARGO_BIN_EXE_*`, playing systemd in a test, debug-only failure injection as an honest technique, why these tests would be impossible in-process.

**Commit:** `Add real-binary self-upgrade integration harness and single-node tests (Phase 14, 7/12)`

### Step 8 — Cluster upgrade state in Raft + scheduler cordoning

**Files:** `src/council/types.rs`, `src/council/state_machine.rs`; `src/meat/` (filter); `src/upgrade/types.rs` already has the types.

**Implement:**
- `RaftRequest::UpgradeUpdate { state: Box<ClusterUpgradeState> }` and `RaftRequest::UpgradeClear { upgrade_id: String }` — **appended at the end of the enum** (D11).
- `DesiredState.active_upgrade: Option<ClusterUpgradeState>` with `#[serde(default)]`; apply logic: `UpgradeUpdate` replaces (last-writer-wins, only the leader writes), `UpgradeClear` sets `None` and appends a bounded history entry (`upgrade_history: Vec<…>`, keep last 20, `#[serde(default)]`).
- Scheduler cordoning: in meat's node filter, exclude nodes whose `NodeUpgradeRecord.phase` is `Directed`/`Verifying` in `active_upgrade`. One focused function + tests; don't refactor the filter.

**Unit tests:** `upgrade_update_replaces_active_state`, `upgrade_clear_archives_to_history`, `snapshot_with_active_upgrade_roundtrips`, `old_snapshot_without_upgrade_field_still_loads` (deserialise a JSON fixture captured before the field existed — this test is the D11 rule made executable), `scheduler_skips_nodes_mid_upgrade`.

**Book §14.8 "Where upgrade state lives"** — why Raft (leader dies mid-upgrade → new leader resumes), the bincode append-only rule with a diagram of what goes wrong otherwise, `#[serde(default)]` as a schema-evolution tool.

**Commit:** `Persist cluster upgrade state in Raft and cordon upgrading nodes (Phase 14, 8/12)`

### Step 9 — Leader-side rolling orchestrator

**Files:** new `src/upgrade/orchestrator.rs`; `src/cluster/runtime.rs` (spawn it); `src/bun/api.rs` (cluster routes from §6, incl. `/v1/cluster/elect`).

**Implement** a long-lived task (mirrors the meat orchestrator's shape), spawned in cluster mode, holding council + membership watch + an http client + CancellationToken:

- Loop: if not leader → sleep/watch. If leader and `active_upgrade` is Some and not `Completed/Paused` → drive one step, write the updated state via `council.write`, repeat. Every state mutation goes through Raft *before* acting on it (write-ahead: record "directing node X" → then POST), so a leader crash never loses track of an in-flight node.
- `start_upgrade(request)`: reject if one is active; build `nodes` from gossip membership (role from `is_council`/leader flags), all `Pending`, phase `Preparing`; obtain + verify the binary (leader-side, per §5.5 cluster step 2); push to Pickle; phase → `UpgradingWorkers`.
- **Workers:** batches of `parallel`; for each node: poll `/v1/version` (populate `from_version`; already at target → mark `Healthy`, skip — idempotent resume), POST directive → `Directed`; poll until `Healthy` (version==target && health ok && gossip Alive) or timeout/failure → node `Failed`, cluster `Paused { reason }`. All nodes healthy → next batch.
- **Council members** (non-leader): strictly one at a time; before each, check quorum headroom via openraft metrics (replication state — if the council is 3 nodes and one is already down, refuse and pause); after each, same health gate.
- **TransferringLeadership** (D13): pick an upgraded council member, POST `/v1/cluster/elect`, poll `current_leader()` up to 30s; on success this orchestrator instance stops being leader and goes dormant — the *new* leader's orchestrator (same code, running there all along) picks up the state, sees phase `TransferringLeadership` with itself as leader, advances to `UpgradingLeader` and directs the former leader like any other node. On failure, try another member, then pause.
- **Completed:** `UpgradeClear` with history entry.
- `resume`: clear `Paused`, re-enter (all steps are idempotent by version-polling first). Failed nodes stay `Failed` until they poll healthy at target (a node that auto-reverted reports old version + failure note → stays failed; the operator fixes and resumes, or rolls back).
- `rollback run`: `direction: Rollback`, target = previous version (from node records / explicit), **reverse order** (leader first is wrong — design says same order reversed: leader's directive goes through the same transfer dance *first*, then council reversed, then workers; directives use `/v1/upgrade/rollback` node calls, no Pickle step).

**Unit tests** — factor the step logic into a pure planner + effects trait (`trait NodeControl { async fn version(&self, addr); async fn direct(&self, addr, directive); async fn elect(&self, addr); }`) so the loop is testable with a mock without real HTTP:
- `workers_upgrade_in_batches_of_parallel`
- `worker_failure_pauses_the_upgrade`
- `council_members_upgrade_one_at_a_time_with_quorum_check`
- `quorum_risk_refuses_council_upgrade`
- `leadership_transfers_before_leader_upgrade`
- `resume_skips_nodes_already_at_target`
- `already_upgraded_cluster_completes_immediately`
- `rollback_walks_reverse_order`

**Book §14.9 "Workers first, leader last"** — why this order (blast radius, quorum arithmetic for 3/5/7), write-ahead orchestration, the leadership-transfer trick in openraft 0.9, idempotency as the resume mechanism.

**Commit:** `Add leader-side rolling upgrade orchestrator (Phase 14, 9/12)`

### Step 10 — `relish upgrade` CLI + release metadata

**Files:** new `src/relish/upgrade.rs`; `src/relish/client.rs`; `src/bin/relish.rs`; `src/upgrade/metadata.rs` (release metadata types + fetch).

**Implement:** §7 in full. Release metadata schema (serve-able by any static host):

```json
{ "schema": 1, "latest": "v0.2.0",
  "releases": [ { "version": "v0.2.0",
    "platforms": { "linux-x86_64": { "url": "…", "sha256": "…",
      "embedded_signature": "…", "external_signature": null } } } ] }
```

Platform key: `{os}-{arch}` from `std::env::consts`. `upgrade check` fetches metadata, gets cluster versions via `/v1/version` fan-out (addresses from `/v1/cluster/nodes`), prints a table. `start <version>`: resolve platform entry → the *leader* downloads (send url+expected hash+sigs in the start request; leader does the fetch — nodes never talk to the internet, only to Pickle). `plan`: pure function over membership (or `--cluster-size`), estimating batches and duration (assume ~45s/node, stated in output); insta snapshot tests. `status`: render `ClusterUpgradeState` as a table; snapshot test. Exit codes: non-zero on `Paused`/`Failed` for scriptability.

**Unit tests:** `metadata_parses_and_selects_platform`, `check_reports_upgrade_available`, insta snapshots for `plan`/`status` rendering, `start_requires_version_or_binary`, `binary_flag_reads_sig_envelope`.

**Book §14.10 "Driving it from relish"** — CLI ergonomics, `plan` as a dry-run culture artefact, metadata-over-HTTPS vs signatures-as-trust.

**Commit:** `Add relish upgrade command set and release metadata (Phase 14, 10/12)`

### Step 11 — Cluster integration tests (roadmap test 3 + orchestrated rollback)

**Files:** `tests/self_upgrade_cluster.rs`, extending the Step 7 harness to N nodes.

**Harness extension:** `RealClusterHarness::start(council: 3, workers: 1)` — each node its own tempdir/binary_dir/supervisor loop, `--cluster` with join addresses, ephemeral ports, shared test keys, shared gossip HMAC config (match whatever `tests/` cluster configs do today on the current branch). Helpers: `leader()` (poll `/v1/cluster/council`), `versions()` (fan-out `/v1/version`).

**Tests:**
- `rolling_upgrade_walks_workers_council_then_leader` — deploy a workload on a worker; `relish`-equivalent start via HTTP (`--binary`-style source: push blob to a node's Pickle, then `/v1/upgrade/start`); poll `/v1/upgrade/cluster` capturing the order nodes reach `Healthy`; assert worker first, then the two non-leader council members one at a time, leadership moved, former leader last; final state `Completed`; all four nodes at `v0.2.0`; workload pid unchanged throughout; Raft still has a leader and answers writes.
- `upgrade_failure_pauses_cluster_and_reverts_node` — `fail-boot` sidecar on **one worker's** staged binary only: that node crash-loops, auto-reverts (Step 4/6 machinery), leader marks it `Failed`, cluster `Paused`; other nodes untouched; then remove the sidecar, `resume`, expect `Completed`.
- `cluster_rollback_returns_every_node_to_previous_version` — after a completed upgrade, cluster rollback → everyone back at `v0.1.0`, reverse order observed.

Budget: these are the slowest tests in the repo (~1–2 min each). Use one shared build, no recompiles, and mark with a longer timeout; if CI needs it, gate behind `#[ignore]` + a `make test-upgrade` target — decide with the owner at implementation time, note the choice in progress.md.

**Book §14.11 "The five-node dress rehearsal"** — reading the test, what each assertion protects against, the fail-boot chaos-engineering cameo (this book's author will enjoy writing that bit).

**Commit:** `Add cluster rolling-upgrade integration tests (Phase 14, 11/12)`

### Step 12 — Documentation sweep and phase close-out

**Files:** `docs/book/14-changing-the-tyres.md` (assemble §14.1–14.11 + intro + "Lessons learned": exec-preserves-children, the env-var version trap, bincode append-only, D12's bootstrap caveat); `docs/progress.md` (tick Phase 14 items — only what's genuinely wired and tested, per the July review discipline; tag anything deferred); `docs/design/agent-bun.md` (record D5/D6/D13 deviations in §5.5 with a dated note); `docs/README.md` + top-level `README.md` (test counts, `relish upgrade` commands, supervisor contract, phase status); `docs/whitepaper.md` only if §20 contradicts what shipped (protocol-version-negotiation claim → soften to the D6 reality).

**Commit:** `Docs: Phase 14 complete — book chapter 14, progress, READMEs (Phase 14, 12/12)`

---

## 9. Test inventory (roadmap mapping)

| Roadmap requirement | Covered by |
|---|---|
| Unit: UpgradeManager signature verification | Step 2 (`signing.rs` suite) + Step 6 (`apply_verifies_before_staging`) |
| Unit: symlink management | Step 3 (`activate_*`, `current_target_*`) |
| Unit: version retention/GC | Step 3 (`gc_*`) |
| Unit: version comparison (semver, pre-release) | Step 1 |
| Unit: rollback state machine (upgrade→verify→commit / →rollback) | Step 4 (`decide_startup` table) + Step 6 (verify/commit fns) |
| Integration: single node upgrade, containers survive | Step 7 `single_node_upgrade_preserves_running_containers` |
| Integration: single node rollback | Step 7 `single_node_rollback_reverts_to_previous_version` |
| Integration: full rolling upgrade (workers, council, leader last) | Step 11 `rolling_upgrade_walks_workers_council_then_leader` |
| Integration: failure triggers automatic rollback | Step 7 (node-level) + Step 11 (cluster pause + revert + resume) |
| Integration: retention GC keeps 3 | Step 7 `version_retention_gc_keeps_last_three` |

---

## 10. Gotchas checklist (read before each step; each one is a real failure mode)

1. **Never insert/reorder `RaftRequest` variants** — append only (bincode). Same for any bincode-on-the-wire enum. `MembershipUpdate`/`ReportingMessage`/`GossipMessage` are frozen this phase.
2. **Env vars survive exec** — that's why version override and fail-boot are sidecar files, not env vars. Don't "simplify" back to env vars.
3. **`current_exe()` symlink behaviour differs by OS** — always `fs::canonicalize` the result before deriving the version or the binary_dir.
4. **`execv` wants `CString`s and the symlink path** — exec the symlink (stable identity), pass the original argv captured at startup, and remember args contain the config path the new process must re-read.
5. **Adopted pids may not be your children** — after a supervisor respawn (crash path), workloads were reparented; `waitpid` gives `ECHILD`. The reaper must fall back to `kill(pid, 0)` polling. After a plain exec they *are* still your children.
6. **Pid reuse** — never trust a stored pid without the start-time fingerprint.
7. **Symlink swap** — temp + `rename`, and fsync the directory; plain unlink+symlink has a window with no binary.
8. **GC vs rollback** — never GC the marker's `previous_binary` while a marker exists; pass `protect` explicitly.
9. **The orchestrator writes to Raft before acting** — direct-then-record loses a node on leader crash.
10. **Idempotency is the resume story** — every orchestrator step starts by polling reality (`/v1/version`), not by trusting its own record.
11. **Quorum arithmetic** — with 3 council members, one already-unhealthy member means council upgrades must refuse to start; check openraft metrics, don't count gossip states alone.
12. **tokio + exec** — do the final marker write and log flush *synchronously* before `execv`; nothing async survives it, and buffered tracing lines will vanish.
13. **Don't test the exec path in-process** — unit tests stop at the `Execer` seam; only the real-binary harness crosses it.
14. **Debug-only hooks must be `cfg!(debug_assertions)`-gated** at the read site, with a release-build warning for `release_keys_override` — this is a security boundary, not a convenience.
15. **British English in docs/comments**, American in serde attrs; behaviour-sentence test names; `make ci` before every commit; ask the owner before each commit; never amend.

---

## 11. Definition of done

- All Step 1–11 tests green under `make ci` on macOS (ProcessGrill paths) and Linux (plus runc adoption + cluster tests).
- `relish upgrade start`/`status`/`rollback`/`resume`/`check`/`plan` work against a real cluster started from the repo (manual smoke: the Step 11 harness doubles as the recipe).
- `docs/progress.md` Phase 14 items ticked honestly (wired + tested, not library-only), deferred items tagged (`AppleGrill adoption`, `gossip version field`).
- Book chapter 14 complete, first-use Rust syntax explained, no AI-tell prose (see the style guide in CLAUDE.md).
- READMEs updated (test counts, commands, supervisor contract).
