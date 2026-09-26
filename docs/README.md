# Reliaburger Documentation

User guide for building and running Reliaburger. For managed Linux VMs on a
laptop, see the [quickstart guide](quickstart.md). For the architectural vision,
see the [whitepaper](whitepaper.md); for implementation status, see
[progress.md](progress.md).

0.1.0 hasn't shipped yet. The source builds and runs today; the signed public
installer arrives with the release. Release candidates follow a separate
[build and promotion procedure](releasing.md#metadata-and-publication), and the
[remaining work](plans/2026-09-22-v0.1.0-remaining-work.md) lists the acceptance
gates still open.

## 0.1.0 scope and limits

- **Container clusters run on rootful Linux Runc with eBPF.** Set
  `[ebpf] enabled = true`; policy recovery also needs bpffs at `/sys/fs/bpf`.
  Bun owns every Runc container durably. It records the original execution,
  service allocation and kernel policy before publishing a workload, adopts
  running containers again after a restart, and keeps those records until
  runtime exit and every required cleanup confirmation succeed. Changing the
  runtime or enforcement mode can't bypass existing ownership, and uncertain
  recovery holds back readiness instead of guessing.
- **Rootless Runc is standalone only.** Bun refuses `--cluster` when Runc runs
  without root, including when it picks Runc automatically. Rootless nodes get
  host-port forwarding through `slirp4netns`, without eBPF policy, workload DNS
  or resource limits.
- **Declarative image workloads need root mode.** App specs request writable
  root filesystems, and rootless Runc only supports read-only roots; see the
  [runc notes](#runc-linux).
- **macOS runs containers through a managed Linux VM**
  ([quickstart](quickstart.md)). Direct Apple Container is disabled; native
  macOS Bun runs process workloads.
- **Native processes are foreground-only.** See the
  [ProcessGrill contract](#processgrill-built-in-fallback).
- **Clusters start fresh.** There's no upgrade path from development builds:
  Bun refuses their state, snapshots and backups, so create a new cluster.
  Rolling upgrades need matching protocol and state formats; see the
  [compatibility policy](releasing.md#cluster-compatibility).
- **One Bun per writable image store.** Registry startup claims exclusive
  ownership of the image store's upload directory.
- **Test volumes have no snapshots.** Disposable test-volume snapshots aren't
  supported.

## Operating notes

### Names

App and job names, their namespaces and namespace declarations must be lowercase
DNS labels: 1–63 ASCII letters/digits/hyphens, with a letter or digit at each end.
Omit a namespace to use `default`; an explicitly empty namespace is invalid.
Bun also refuses fresh app/job IDs that belong to another workload, including
stopped cleanup owners. For example, `worker-g1` can't claim replica zero while
generation one of `worker` owns that ID in the same namespace. Use another name
or retire the existing owner first; runtime IDs are never silently renamed.

### When startup refuses

Bun refuses to start when a workload ownership record is unreadable or
malformed, when a record's canonical ID disagrees with its app, namespace or
replica, or when runtime adoption fails. It validates the whole inventory before
touching any instance and keeps records and identity files for recovery. Inspect
the reported path or runtime error, repair it and retry. Don't delete ownership
records to get past the refusal while workloads may still be running.

The registry's startup sweep works the same way. A busy owner, an unexpected
entry, a symlinked upload directory or a cleanup error refuses startup with
context. Repair the reported condition rather than removing a live owner's lock
file. A normal restart reclaims abandoned temporary uploads; clients restart
interrupted pushes.

### Stopping and retiring workloads

Stop, retire, rollback and halt all wait for confirmed cleanup: runtime exit,
kernel backend withdrawal, mount and network teardown, and removal of identity
and adoption records. If any step fails, Bun reports the error and keeps
ownership, so the cleanup can be retried; it never reports Stopped for a
workload it can't prove has stopped. Rolling and blue-green deployments keep
both generations when the old one's exit is uncertain. A failed final kernel
backend publication is a deployment error, and the running workload stays owned.

Each runtime confirmation step (accepting a stop request, accepting a
force-kill, and reporting exit after the kill) has its own deadline, separate
from the workload's drain grace:

```toml
[runtime]
stop_confirmation_timeout_secs = 10  # default; zero is rejected
```

Raise it on hosts where `runc kill` routinely answers slowly under load. A
stop that outlasts it is reported as unconfirmed and retried, never as
Stopped.

### Jobs and cron

Jobs record execution intent and their three-retry budget before launching, and
Bun replacement restores that budget. An observed failure can retry after
confirmed cleanup. An unknown exit status stays `unknown`, including across
further restarts, and ordinary apply won't repeat it. After checking the job's
external effects, explicitly request a new run on the same node:

```sh
relish apply jobs.toml --rerun-jobs
```

The manifest must contain only non-scheduled jobs. The API requires user
deployment authority and workload scope; internal service credentials can't
authorise a rerun. Explicit stop cancels pending retries but keeps an unknown
outcome.

Cron registrations and their latest claimed UTC minute persist before apply,
stop or launch is acknowledged, and Bun restores them before serving the API.
Missed minutes are skipped, and a crash between recording a firing and launching
it can skip that occurrence too. There's no catch-up and no exactly-once
promise. If a cron checkpoint write fails, Bun fences further cron changes and
firings until it restarts and reloads its state.

`relish test --filter jobs` creates durable leases on the receiving node for
batch jobs and cron registrations; keep using the same node endpoint for a
lease's lifetime. Creation needs an unscoped credential and the server's
isolated-workload test grant. Ordinary jobs stay node-local; there's no cluster
job scheduling.

### Registry behaviour

- A manifest push persists its catalogue before acknowledging. 201 means the
  catalogue accepted it (blob replication may still be running); 503 means the
  Raft commit wasn't confirmed, so retry.
- Clustered workers and followers forward writes to the authenticated leader.
  Repository reads, quota checks and `relish images` also use the leader's
  committed view; if the leader is unreachable you get 503, not an empty
  catalogue.
- Pushing identical bytes into another repository keeps independent tags and
  signatures; retiring one repository leaves the other copy intact.
- A chunked upload belongs to the credential that created it. Continue and
  complete it with that same credential; revoked or abandoned uploads expire
  through the normal reaper.
- Writes under `rbtest-…/` require the exact authenticated test lease owner and
  the `x-reliaburger-test-lease` header. Only the owning application lease can
  depend on those images.
- Direct pulls and Pickle verify pinned manifests, platform indexes and
  configuration bytes before caching them, and retry transient upstream errors
  up to four times within a fixed deadline. OCI index selection targets Linux
  containers even when the client runs on macOS.

### Test volumes and leases

Lease-owned test volumes and generated configuration have durable provisioning
records. Ordinary Stop and rescheduling keep their data; lease retirement
removes it only after confirmed runtime cleanup, and failed unmounts keep
cleanup pending. Host-source volumes and ordinary application data stay outside
test ownership. A worker that can't return to confirm cleanup keeps it pending;
see [decommissioning a node](#decommissioning-a-node).

### Reporting limits

Reporting refuses messages over 1 MiB or with more than 100 events;
`reporting_tree.max_events_per_report` only supports 100. Admission failures
appear in node logs. State snapshots refresh on the next tick and metrics retry
the last five minutes; longer gaps need the retained node-local data. See
[reporting admission](book/11-eyes-everywhere.md#reporting-has-an-admission-boundary).

### Configuration changes

Upgrade metadata endpoints are selected with `relish upgrade check --url`; node
TOML rejects the obsolete `[upgrades] release_url` key. Ingress uses unweighted
round-robin on Bun's shared runtime, with no separate strategy or worker-thread
setting. `relish setup` checks node version and critical subsystem readiness
before reporting success; a startup timeout returns an error with the log path.

## Prerequisites

### Rust toolchain

Reliaburger requires Rust 1.97 or later (2024 edition). CI checks every target
with the minimum compiler against `Cargo.lock`; release builds use Rust 1.98.0. Install via
[rustup](https://rustup.rs/):

```sh
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```

Follow the prompts (defaults are fine), then restart your shell or run:

```sh
source "$HOME/.cargo/env"
```

Verify:

```sh
rustc --version   # needs 1.97+
cargo --version
```

To use the same compiler as release builds:

```sh
rustup toolchain install 1.98.0
cargo +1.98.0 build --locked --bins
```

### Platform build tools

**Linux** (Debian/Ubuntu):

```sh
sudo apt install build-essential pkg-config
```

**Linux** (Fedora):

```sh
sudo dnf groupinstall "Development Tools"
```

**macOS**:

```sh
xcode-select --install
```

## Container runtimes (optional)

For 0.1.0, Bun selects Linux runc or the built-in process runtime. macOS containers run through managed Linux VMs. **ProcessGrill** (plain OS processes) is the built-in fallback that works everywhere without extra software — you don't need to install anything else to get started.

### runc (Linux)

[runc](https://github.com/opencontainers/runc) is the reference OCI container runtime. Docker and containerd use it under the hood.

**Install on Ubuntu/Debian:**

```sh
sudo apt install runc iproute2 nftables
```

**Install from GitHub releases:**

Download the latest binary from [github.com/opencontainers/runc/releases](https://github.com/opencontainers/runc/releases) and place it in your `PATH`.

Notes:
- Rootless runc is standalone only: Bun refuses `--cluster` without root. It supports read-only OCI roots and path-based test bundles, and uses `slirp4netns` for outbound networking and published ports, restoring that userspace network across Bun replacement. Declarative app specs currently request writable roots, so normal image workloads must use root mode until Reliaburger owns a safe unprivileged snapshotter; they never fall back to a shared writable image tree.
- Rootless resource limits remain unsupported and fail admission because Reliaburger doesn't create a delegated user cgroup. Ubuntu hosts that set `kernel.apparmor_restrict_unprivileged_userns=1` also need an AppArmor policy permitting the installed Bun binary (or that restriction disabled) before runc can write UID/GID maps.
- Bun keeps Runc bundles and state under `<storage.data>/instances/runc/` and uses the selected `storage.images` directory for its image cache. Explicit Runc selection and automatic detection both follow these node paths, including any storage fallback reported at startup.
- OCI images are pulled from Docker Hub automatically when the spec's `image` field is set (e.g. `alpine:latest`)
- Root-mode writable images use one private OverlayFS upper per workload over the shared content-addressed image generation. A restart or Bun adoption reuses that workload's upper; exit and kill unmount it.
- To run provisioned Linux runtime tests: `sudo make test-linux`
- OCI protocol tests use a local digest-pinned registry fixture; they need no public registry

Rootful runc reserves up to 509 container addresses per node durably. Exhaustion
refuses new creation. Addresses become reusable only after confirmed network
teardown and nftables forwarding inspection; retain `.network-leases.json` with the runtime bundle state across
restarts. Uncertain teardown keeps its reservation for explicit cleanup.

On a direct Linux host, its firewall must permit forwarding between Reliaburger's
container interfaces and the required destinations. Bun enables IPv4 forwarding
but does not override another firewall's DROP rules or policy. A host can reach
both containers while container-to-container traffic is still blocked. Use the
managed VM quickstart for a dedicated host configuration; check existing Docker
or operator firewall rules when diagnosing direct-host connectivity.

### macOS containers: managed Linux VMs

For 0.1.0, run containers through the [managed laptop quickstart](quickstart.md):

```sh
relish setup --quickstart --nodes 3
```

Relish provisions Linux VMs with runc through Lima. Native macOS Bun supports
foreground process workloads. Direct Apple Container selection is disabled,
even when its CLI is installed: interrupted CLI requests can outlive Bun and
mutate the Apple daemon, and their recovery guarantees are not yet complete.
The adapter and its manual development tests remain in the repository for future
work; they are outside the 0.1.0 runtime profile.

### Linux containers: run a cluster on your own Linux servers

For 0.1.0, you can also deploy reliaburger to pre-existing Linux VMs or bare-metal servers
if you already have that computing power available. Check this [procedure here](linux-servers.md).

### ProcessGrill (built-in fallback)

Spawns native processes on macOS and Linux, without namespaces, cgroups or rootfs
isolation. Useful for development and testing without a container runtime.

Bun records process intent before execution and reconciles it on startup, even
if a crash prevented the later adoption record. Completed jobs preserve their
exit code. An application deployment requires durable adoption metadata before
success is acknowledged; failed writes retain cleanup ownership and return an
error. Lost owner processes remain uncertain and require operator investigation.

Process `relish exec` commands use child owners too. Cancelling the runtime request or killing Bun
closes its owner socket and triggers command retirement; application cleanup
waits for those owners and their foreground children. Each application permits
sixteen concurrent exec requests, with a five-minute deadline, 64 KiB request
limit and 1 MiB combined stdout/stderr response limit. Commands inherit the host
environment and run without container isolation.

**0.1.0 contract: foreground workloads only.** The main process stays under Bun's
supervision and children must remain in its supervised process group. A service
can run unattended and spawn workers; foreground does not mean an open terminal.
Use the application's foreground/no-daemon option. A shell wrapper should `exec`
the server, or wait for its children. Do not detach into a new session or process
group, daemonise, or hand work to an external service manager.

If those restrictions don't fit, use **Linux containers**: runc on Linux or the
[managed Linux cluster](quickstart.md) on a laptop. Process groups are a
cooperative supervision contract, not a security boundary for untrusted code.
Bun keeps cleanup ownership whenever it can't confirm the process has gone.

No installation needed. This is what you get by default.

## Building

The portable test gate requires cargo-nextest 0.9.145 or newer. That release
fixes capture-pipe inheritance between concurrent macOS tests, which could make
a completed test appear to leak a child process. CI pins 0.9.145 and treats a
leak as a failure at the existing 100 ms deadline.

```sh
cargo install cargo-nextest --locked --version 0.9.145
```

The [Makefile](../Makefile) provides all build targets:

```sh
make build       # compile (debug)
make release     # compile (optimised)
make test        # portable nextest suite
make test-doc    # Rust documentation examples
make test-linux  # provisioned Linux runtime/kernel suite
make test-rootless-runc # non-root runc/slirp replacement proof
make lint        # clippy with warnings as errors
make audit       # RustSec advisory and dependency-maintenance gate
make fmt         # format with rustfmt
make ci          # portable format, lint and test checks
make clean       # remove build artefacts
```

Or use cargo directly:

```sh
cargo build
cargo test
```

## Testing and benchmarking

### Test suites

```sh
make test                  # portable nextest suite
make test-doc              # doctests (nextest does not run them)
make test-slow             # genuine wall-clock acceptance tests
sudo make test-linux       # runc, netns, eBPF, Btrfs, Buildah and root-only tests
make test-rootless-runc    # rootless runc port and replacement test (never sudo)
make test-cluster          # failover, healing, recovery, placement and chaos
make test-upgrade-node     # real single-node binary replacement
make test-upgrade-cluster  # real rolling cluster replacement
make coverage              # portable suite under line coverage (HTML and LCOV)
make audit                 # fail on new RustSec dependency findings
```

Tests that need hardware, credentials or a reboot (Apple Container, NVIDIA GPU,
S3, host reboot) aren't run by CI. The [test harness design](design/test-harness.md#tests-no-ci-job-runs)
lists them and how to run each.

`make test` runs only tests that can execute truthfully on an ordinary developer machine.
Provisioned tests use `#[ignore = "requires …"]`; their named target enables the prerequisite,
selects ignored tests only and fails if its filter finds no tests. Target-specific code uses
`#[cfg(...)]`, so Linux-only tests are reported separately rather than pretending to pass on
macOS. Retries are disabled.

The deferred Apple Container adapter has manual development tests on Apple silicon;
these are not a 0.1.0 acceptance gate. See the [test harness design](design/test-harness.md)
for the audit, exact suite contracts and CI mapping.

### Benchmarks

Gossip protocol benchmarks use [criterion](https://docs.rs/criterion) for statistical analysis with regression detection.

```sh
make bench         # reproducible transport and 5-250 node measurements
make bench-large   # reproducible 500 and 1,000 node measurements
```

CI runs both on pushes to `main`, nightly, and on pull requests that touch
`src/mustard/` or `benches/`. Nothing gates on the numbers yet, so other pull
requests skip the release build.

The fast benchmarks (`cargo bench --bench gossip`) are the ones to run regularly — they catch performance regressions in the gossip protocol. Results are stored in `target/criterion/` and criterion reports whether performance changed between runs.

The large benchmarks (`cargo bench --bench gossip_large`) test the same convergence logic
at 500 and 1000 nodes. They maintain a seeded, incrementally sorted peer index so the
benchmark measures protocol work rather than repeatedly allocating and sorting membership
snapshots. Setup and convergence are reported separately, and non-convergence fails rather
than returning a sentinel duration.

The 10k target checks the per-node production invariant: one real Mustard node ingests a
10,000-member table through bounded gossip messages, can select a probe target and exposes
every learned update through fixed-size dissemination batches. It also answers
anti-entropy push-pull requests within eight datagrams each, sweeping all 10,000 members.
Full 10,000-node all-to-all simulation would allocate 100 million membership records on one runner. That measures one
machine pretending to be a datacentre, and did not finish inside its 90-minute budget.
CI uploads Criterion data; it does not enforce regression percentages until measurements
are stable on consistent hardware.

## Running

### Portable first run

This path needs no container runtime. Build all binaries first because the
workload itself is the `testapp` binary:

```sh
cargo build --bins
target/debug/bun --runtime process
```

Leave Bun running. In a second terminal:

```sh
target/debug/relish apply examples/phase-1/proc-first-run.toml
target/debug/relish status
target/debug/relish top
```

You should see the `hello` workload in `Running` state. Open
<http://127.0.0.1:9117/> for the dashboard. ProcessGrill supervises a real OS
process, but it doesn't isolate it; use Linux runc (through the managed VM on macOS) for container
workloads.

### Node agent (bun)

The bun agent manages container lifecycle, health checks, and the local HTTP API.

```sh
cargo run --bin bun
```

Options:

| Flag | Default | Description |
|------|---------|-------------|
| `--config <path>` | (none) | Path to node config TOML file |
| `--listen <addr>` | `127.0.0.1:9117` | API listen address |
| `--runtime <name>` | `auto` | Runtime: `auto`, `process`, `runc` (Linux) |

Examples:

```sh
# Start with auto-detected runtime (default)
cargo run --bin bun

# Force process runtime (no container tools needed)
cargo run --bin bun -- --runtime process

# Use a custom loopback address
cargo run --bin bun -- --listen 127.0.0.1:9217

# Load node configuration from file
cargo run --bin bun -- --config node.toml
```

The agent prints which runtime it selected on startup:

```
bun: reliaburger node agent v0.1.0
bun: auto-detected runtime: process
bun: API server listening on 127.0.0.1:9117
```

Stop with `Ctrl-C` — the agent shuts down gracefully.

An empty token store is a local bootstrap window: administrative routes are
open so the first cluster token can be created. Bun therefore accepts only an
IP-literal loopback `--listen` address in that state. Wildcard, routable and
hostname listeners are rejected before subsystem startup. Initialise an
authenticated cluster and create the first admin token before exposing the API
on a non-loopback address.

After bootstrap, cluster-wide token management, node join tokens, secret
rotation, image signing, self-upgrades (start, apply, rollback, resume) and
leader elections require an **unscoped Admin** credential. An Admin
restricted by `--apps` or `--namespaces` cannot use those global operations.
Ordinary `[namespace]` quota and `[permission]` declarations also require an
unscoped Admin; a Deployer can apply workloads within its scope and configured
permissions. Apps and jobs receive the same admission checks before any part
of a manifest changes state. Lease-owned test namespace declarations retain
their separate ownership checks. An administrator overriding another credential's
lease ownership must also be unscoped, for both inspection and cleanup.

### Secure cluster initialisation

`relish init` generates the cluster PKI, the first node's identity and a
`reliaburger.toml` that requires mTLS. Start the cluster with that generated
configuration; no security switch needs hand-editing:

```sh
target/debug/relish init cluster --cluster-name prod --node-id node-01
sudo target/debug/bun --cluster --runtime runc --config cluster/reliaburger.toml
```

The output directory is created if it doesn't exist. Leave Bun running and,
from another terminal, mint the first administrator token over the generated
cluster CA before doing anything else:

```sh
export RELIABURGER_TOKEN="$(target/debug/relish \
  --ca-cert cluster/identity/root-ca.crt \
  token create --name first-admin --role admin)"

target/debug/relish --ca-cert cluster/identity/root-ca.crt apply cluster/app.toml
target/debug/relish --ca-cert cluster/identity/root-ca.crt status
```

This is a one-node Raft cluster: clustered code paths are live, but it cannot
survive a node failure. The API listens at `https://127.0.0.1:9117`. On macOS, use the [managed Linux VM quickstart](quickstart.md) for containers.

To grow this into a resilient three-voter council, mint one token per new
node. Join tokens are deliberately separate from API bearer tokens:

```sh
JOIN_NODE_02="$(target/debug/relish \
  --ca-cert cluster/identity/root-ca.crt \
  join-token create --node-id node-02 --ttl 15m)"
JOIN_NODE_03="$(target/debug/relish \
  --ca-cert cluster/identity/root-ca.crt \
  join-token create --node-id node-03 --ttl 15m)"

# Run this on node-02 after provisioning the binary, cluster master key and
# a node-specific config. Repeat with JOIN_NODE_03 and node-03.
target/debug/relish join --token "$JOIN_NODE_02" --node-id node-02 \
  --identity-dir identity \
  --ca-fingerprint sha256:<ROOT_CA_FINGERPRINT> \
  https://<CURRENT_LEADER>:9117
```

Set each joiner's `[cluster].join` to an existing member's gossip address
(port 9443 by default), give it unique storage paths and ports, and then start
`bun --cluster` with that config. `relish join` enrols identity only; it
doesn't provision or start the node. Token creation accepts `--ttl` from `1s`
to `1h` (default `15m`), requires an Admin bearer after bootstrap, commits only
the token hash and expiry to Raft, and prints the plaintext once. During an
election, retry against the current leader: a follower returns an error and
does not commit or disclose a usable token.

Keep `[cluster].name` identical on every node. `relish init`, `relish setup`
and `relish dev create` write it for you; Bun validates it as a DNS-style SPIFFE trust domain.
The value is immutable for the life of the process and becomes the prefix of
app, job and build-signer identities, for example
`spiffe://prod/ns/default/app/api`.

The generated security section contains the material paths and the secure
mode:

```toml
[security]
master_key_path = "cluster/prod-master.key"
bootstrap_path = "cluster/prod-security-bootstrap.json"
identity_dir = "cluster/identity"
require_mtls = true
```

With this mode, Raft and reporting require mutually authenticated node
certificates. Peer API calls also present their node certificate and check the
live revocation list; Relish and browsers may omit a client certificate and
authenticate with a bearer token or session cookie over TLS.

Deployment drains retain requests already using an ingress backend, including
captured failover candidates. The drain deadline cancels HTTP and WebSocket
work; completion requires the request guards to release. A stalled downstream
reader cannot prevent cancellation of the upstream response. Ordinary Stop uses
the same ingress drain before runtime retirement; automatic restart waits across
agent ticks for captured requests to release before killing its predecessor.
Stopped-runtime cleanup also retains identity/adoption records and address
ownership until captured requests release; an incomplete cleanup can be retried.

Automatic application restarts refresh DNS and ingress with the replacement's
confirmed address. Applications with health checks stay out of healthy routing
until a successful probe; failed backend publication retains cleanup ownership.

Cluster-signed ingress leaves renew on the first handshake after half their
validity period. Idle hosts renew when clients return; expired cached leaves
are never reused. Operator-supplied ingress certificate/key files reload once
per second as a validated pair. Invalid replacements retain the previous pair
only until its expiry; existing connections continue. Ingress disables TLS
session resumption, so reconnects validate the current certificate. API, registry
and ingress TLS connections have a one-hour lifetime, including WebSocket
upgrades. HTTP draining starts 30 seconds before that limit; clients of
long-lived streams must reconnect.

With a generated mTLS cluster configuration, Bun renews its node leaf at the
midpoint of the signed validity window. It contacts the current leader using
its existing TLS identity and service token, keeps the new private key local,
and persists the validated response before publishing it to API/registry,
Raft/reporting and internal HTTPS consumers. Failed requests or saves leave the
current identity installed and retry after five seconds. Leader changes don't
require a process restart.

Node identity persistence uses a private atomic `node.bundle.json`; the PEM
files are exports, and editing them does not update the running identity.
Diagnostics report the current serial, expiry and renewal worker state. A node
without the required master key/service token cannot renew automatically. An
identity that expired while offline needs authorised re-enrolment. Leaf renewal
does not rotate the cluster CAs; CA rotation remains separately tracked.

If you specifically need plaintext transports for an isolated local test,
make that exception explicit:

```sh
cargo run --bin relish -- init cluster --development-plaintext
```

That command writes `require_mtls = false`, embeds a warning in the generated
file and prints a warning. Bun repeats the warning whenever that config starts
in cluster mode. `relish dev create` does the same for its deliberately local
Lima-VM configuration. Do not reuse either config on a shared network.

### CLI (relish)

Relish is the command-line interface for interacting with a running bun agent.
Run it without a subcommand, or use `relish tui`, to open the interactive
terminal dashboard. The TUI needs a terminal of at least 80×24 cells.

```sh
cargo run --bin relish              # interactive TUI
cargo run --bin relish -- tui       # the same, explicitly
cargo run --bin relish -- <command>
```

Every command, grouped by task with a link to the manual chapter that explains
it, is under *Everything relish can do* in the [top-level README](../README.md).
That list is generated from relish's own command definitions, so it matches
the binary. `relish help COMMAND` (or `--help` on any command) shows every
flag.

Use `test --chaos --filter dead_worker_node_has_workloads_rescheduled` to select
an exact supported chaos scenario. Omitting the filter runs all five, including
rootful Linux node pressure; selecting a subset does not qualify the full suite.


TUI keys:

| Key | Action |
|-----|--------|
| `a`, `n`, `j`, `e`, `l`, `r` | Open apps, nodes, jobs, events, logs or routes |
| `s`, `?`, `:` | Search, help or command palette |
| Arrow keys, Enter | Select and open a row |
| Tab, Shift-Tab | Cycle app-detail tabs |
| `/` | Filter the current list |
| `f` | Toggle log following |
| Escape, `q` | Go back; quit from the dashboard |

### Self-upgrade (Phase 14)

`bun` can replace its own binary in place: the process `exec()`s the new
version, running workloads are *adopted* by the new binary (same pids, no
restarts), and a crash-looping upgrade automatically reverts to the previous
binary. On a cluster the Raft leader rolls the fleet: workers first (with
`--parallel`), council members one at a time, then the leader upgrades
itself last (in place; a ≥3-node council keeps quorum through the bounce).

Requirements:

- **A process supervisor.** Run bun under something that restarts it whenever
  it exits (systemd `Restart=always`, or any `while true; do bun ...; done`
  loop). Startup-side recovery does the rest — including the automatic
  symlink revert after a crash-looping upgrade.
- **Signatures.** Network upgrades need two Ed25519 signatures over the
  binary: one from the release key set compiled into the running binary, and
  one from the operator key configured as `upgrades.external_signing_key` in
  node.toml (generate one with `relish dev keygen`). Air-gapped
  `upgrade start --binary` needs only the release signature (expects
  `{binary}.sig` alongside the file — see `relish dev sign-binary`).
- **A versioned binary directory** (default: the directory of the running
  executable): `bun` is a symlink to `bun-vX.Y.Z`; previous versions are
  retained for rollback (`upgrades.retain_versions`, default 3).

The release private key must live outside any repository. The project key's
public half is compiled into `src/upgrade/keys.rs`; rotating it means shipping
a release that trusts both old and new keys, then dropping the old one.

### Dev cluster

`relish dev create` spins up a real multi-node Reliaburger cluster in Lima VMs — gossip membership, a Raft council that elects a leader, and live state reporting — not isolated single nodes. The same `bun` binary as production runs in each VM (started with `--cluster`).

```sh
relish dev create mycluster --nodes 3
limactl shell reliaburger-1 relish nodes     # all three nodes
limactl shell reliaburger-1 relish council   # council members + leader
relish dev destroy mycluster
```

Notes:

- **Lima required** (`brew install lima`). VMs use Lima's `user-v2` network so they can reach each other with no `socket_vmnet`/sudo setup; each node advertises its inter-VM IP. That network isn't routable from the host, so run `relish nodes`/`council` *inside* a node (`limactl shell reliaburger-1 …`), where the CLI reaches the local agent on `127.0.0.1:9117`.
- **Binaries are built from your current tree, not downloaded.** `dev create` builds `bun`/`relish` for Linux inside the persistent build VM (the same one `relish dev test` uses), so the **first `create` is slow** (a full build); later runs are incremental.
- `--bun <path>` / `--relish <path>` install a pre-built Linux binary instead, skipping the build.

Global flags:

| Flag | Default | Description |
|------|---------|-------------|
| `--output <format>` | `human` | Output format: `human`, `json`, `yaml` |
| `--endpoint <url>` | local API | Bun API base URL; overrides `RELIABURGER_ENDPOINT` |
| `--ca-cert <path>` | unset | Cluster root CA PEM; switches the local default to HTTPS |
| `--token <token>` | environment | API bearer token; overrides `RELIABURGER_TOKEN` |

Examples:

```sh
# Deploy the example app (agent must be running)
cargo run --bin relish -- apply examples/phase-1/proc-minimal-app.toml

# Preview without contacting an agent
cargo run --bin relish -- apply examples/phase-1/proc-minimal-app.toml --dry-run

# List running workloads
cargo run --bin relish -- status

# JSON output
cargo run --bin relish -- --output json status

# Show logs
cargo run --bin relish -- logs web

# Show last 20 lines
cargo run --bin relish -- logs web --tail 20

# Stream logs in real time
cargo run --bin relish -- logs web --follow

# Execute a command inside a running instance
cargo run --bin relish -- exec web echo hello

# Stop an app
cargo run --bin relish -- stop web

# Generate cluster PKI, identity and a starter config
cargo run --bin relish -- init myproject
```

`init` generates a cluster's CA hierarchy, age keypair, first-node identity,
master key and an mTLS-required `reliaburger.toml` (plus a sample `app.toml`)
in the target directory — it does not merely scaffold an empty project.

`apply --dry-run` prints the `ApplyPlan` without contacting an agent (always
exits 0). For `examples/phase-1/proc-minimal-app.toml`:

```
Relish apply plan:

  + app.web
      image     proc-grill:image-ignored
      replicas  1
      port      8080
      health    /healthz

  + app.worker
      image     proc-grill:image-ignored
      replicas  1
Plan: 2 to create, 0 to update, 0 to destroy.

(dry run — nothing deployed)
```

### TestApp utility

A built-in test HTTP server with configurable behaviour:

```sh
cargo run --bin testapp -- --mode healthy --port 8080
cargo run --bin testapp -- --mode unhealthy-after --count 5 --port 8080
cargo run --bin testapp -- --mode hang --port 8080
cargo run --bin testapp -- --mode slow --delay 3000 --port 8080
```

Used in the example configs to demonstrate health checks, restarts, and lifecycle transitions with ProcessGrill.

## Configuration

### Running real containers

On Linux with runc installed, you can run real Docker Hub images. On macOS, use the [managed Linux VM quickstart](quickstart.md):

```sh
# Terminal 1 — start the agent with a real runtime
cargo run --bin bun -- --runtime runc

# Terminal 2 — deploy nginx with health checks
cargo run --bin relish -- apply examples/phase-1/container-nginx.toml

# Check status (nginx should reach Running after health checks pass)
cargo run --bin relish -- status

# Or run a quick Alpine hello world job
cargo run --bin relish -- apply examples/phase-1/container-hello.toml
```

The first deploy will pull the image from Docker Hub, which takes a few seconds. Subsequent deploys reuse the cached image.

The `proc-*` examples use `command` to run local binaries and work without any container runtime. The `container-*` examples use `image` to pull and run real OCI containers.

`examples/kubernetes/podinfo.yaml` is a real Kubernetes application: the three-tier podinfo demo (a frontend, the backend it calls as `backend`, and redis as `redis`) plus a BusyBox load generator that keeps calling the frontend as `frontend`, all pinned by digest, with an ingress on `podinfo.localhost`. It needs a runc node with `[ebpf]`, `[dns]` and `[ingress]` enabled, which is what `relish setup --quickstart` builds. Its header lists every edit we made to upstream's manifests.

```bash
relish apply -f examples/kubernetes/podinfo.yaml
curl -H 'Host: podinfo.localhost' http://127.0.0.1:18080/
```

### Internal DNS on rootful runc

The `.internal` responder is opt-in and currently supports rootful runc on
Linux. It also requires the eBPF service data path: DNS returns a virtual IP,
and the connect hook turns that VIP into a healthy backend. Bun refuses any
other combination before it adopts or creates a workload.

```toml
[dns]
enabled = true
listen = "0.0.0.0:53"       # derive and bind this node's runc gateway
upstream = "8.8.8.8:53"
restrict_sources = true

[ebpf]
enabled = true
```

`0.0.0.0:53` is a derivation setting, not the socket Bun ultimately exposes.
Bun calculates the node-side veth gateway and binds that precise address with
Linux `IP_FREEBIND` before the first workload creates the interface. This avoids
host loopback, which a container namespace can't reach, and avoids claiming
wildcard port 53 from `systemd-resolved`. `/etc/resolv.conf` has no port syntax,
so other ports are rejected.

Short names such as `redis.internal` use the namespace of the isolated source
workload, as published by the runtime. Unknown or ambiguous source addresses are
refused; host tools should query `redis.<namespace>.internal`. The old
`dns.default_namespace` setting is no longer accepted.

Containers also get a Kubernetes-style search list (`search <namespace>.internal
internal`, `options ndots:2`), so an app can reach `redis:6379` in its own
namespace or `redis.default:6379` in another one, exactly as a pod would.

Runc receives a per-instance, read-only resolver file. Bun doesn't modify the
shared unpacked image. Both UDP and TCP must bind before the node reports DNS
ready; a later responder-task failure stops Bun so its capability expires.
Rootless runc, ProcessGrill and IPv6-only listeners remain
unsupported rather than silently falling back to broken host DNS.

## Configuration

Workloads are defined in TOML. Resources follow the Kubernetes units: `cpu`
takes cores (`"2"`, `"0.5"`) or millicores (`"250m"`), `memory` takes bytes or
`Ki`/`Mi`/`Gi`/`Ti`, and either can be a `request-limit` range such as
`cpu = "0.5-2"` or `memory = "256Mi-512Mi"`. The same CPU units apply to a
namespace `cpu` budget and to the node's `[resources] reserved_cpu`.

See [`examples/`](../examples/) for ready-to-apply configs:

| Example | Demonstrates |
|---------|-------------|
| **ProcessGrill** (`proc-*`) | **Runs local processes — no container runtime needed** |
| [`proc-first-run.toml`](../examples/phase-1/proc-first-run.toml) | Collision-free portable first run |
| [`proc-minimal-app.toml`](../examples/phase-1/proc-minimal-app.toml) | App with health check + worker |
| [`proc-restarts.toml`](../examples/phase-1/proc-restarts.toml) | App that goes unhealthy and gets restarted |
| [`proc-job-success.toml`](../examples/phase-1/proc-job-success.toml) | Job that runs to completion |
| [`proc-job-failure.toml`](../examples/phase-1/proc-job-failure.toml) | Job that fails and gets retried |
| [`proc-init-container.toml`](../examples/phase-1/proc-init-container.toml) | App with init container |
| [`proc-full-featured.toml`](../examples/phase-1/proc-full-featured.toml) | All Phase 1 features |
| [`proc-multi-app.toml`](../examples/phase-1/proc-multi-app.toml) | Multiple apps in one config |
| [`proc-volumes.toml`](../examples/phase-1/proc-volumes.toml) | Managed and HostPath volumes |
| **Real containers** (`container-*`) | **Pulls OCI images — requires Linux runc** |
| [`container-hello.toml`](../examples/phase-1/container-hello.toml) | Alpine hello world job |
| [`container-nginx.toml`](../examples/phase-1/container-nginx.toml) | nginx with health check |
| [`container-job-failure.toml`](../examples/phase-1/container-job-failure.toml) | Job that fails and gets retried |
| [`container-init-container.toml`](../examples/phase-1/container-init-container.toml) | App with init container |
| [`container-full-featured.toml`](../examples/phase-1/container-full-featured.toml) | All Phase 1 features |
| [`container-multi-app.toml`](../examples/phase-1/container-multi-app.toml) | Multiple apps in one config |
| [`container-volumes.toml`](../examples/phase-1/container-volumes.toml) | Managed and HostPath volumes |

### Images, the pull-through cache, and cluster registries (Phase 12)

External images (`docker.io`, `ghcr.io`, …) are served through a pull-through
cache by default: the first pull fetches from upstream and commits under a
`cache/<host>/<repo>` catalog entry; later pulls anywhere in the cluster are
served peer-to-peer. Cluster-pushed images download from multiple peers in
parallel (rarest layer first). Direct external pulls and Pickle upstream reads retry recognised rate-limit
and temporary gateway/service/server errors, refused or interrupted connections and
stalled reads up to four attempts. Each HEAD/manifest/config attempt may take 30 seconds
within a 2-minute total, and each layer attempt 120 seconds within 6 minutes; authentication,
malformed responses and digest failures still fail. Operational constraints:

- **`registry_port` must be uniform across the cluster** — peers derive each
  other's registry URLs from gossip IPs plus the local port setting.
- **`registry_bind` defaults to loopback for standalone Bun.** In cluster mode
  Bun derives the gossip-advertised IP from that default, so the address peers
  use is actually bound. An explicit different interface fails startup; an
  explicit wildcard remains valid.
- Cluster registry reads and writes require authentication from the first request and
  normally use the master-key-derived service token plus the node's cluster TLS
  identity. A misconfigured cluster without that token still fails closed. The
  open, tokenless bootstrap window exists only for a loopback standalone registry.
- Authenticated `GET /v1/capabilities` reports the selected listener, TLS/P2P
  state, redundancy target, active node count and under-replicated layer count.

```toml
[images]
registry_bind = "0.0.0.0"   # optional wildcard; default derives advertise IP
pull_through = true          # cache external images in the cluster
cache_recheck_secs = 3600    # how long a cached mutable tag is trusted
p2p_concurrency = 4          # parallel layer fetches per image pull
build_timeout_secs = 900     # ceiling per buildah stage
max_context_bytes = 268435456 # 256 MiB cap on an extracted build context

# Digest-pinned images try a mirror first and fall back to the upstream.
# Tag references never use a mirror; loopback mirrors speak plain HTTP.
mirrors = { "public.ecr.aws" = "mirror.internal:5000", "ghcr.io" = "mirror.internal:5000" }

[[images.external_registries]]
host = "ghcr.io"
username = "bot"
password_secret = "GHCR_TOKEN"   # environment variable, read at startup
```

A mirror only ever serves an image named by its `@sha256:` digest, and Bun
verifies the whole digest chain whichever registry answers, so a stale or
hostile mirror can make a pull slower but never change what runs. Authenticated
`GET /v1/capabilities` reports the configured mirrors, and `relish test` stages
its pinned fixture image through them.

### Volume snapshots and scheduled backups (Phase 12)

Managed volumes on a Btrfs-backed `[storage] volumes` directory are created as
subvolumes (size limits become qgroup quotas) and can be snapshotted — O(1)
copy-on-write — via `relish snapshot` or on a schedule:

```toml
[storage.snapshots]
interval_secs = 86400                 # 0 disables the loop
retain = 7                            # newest N kept per volume
upload_url = "s3://backups/burger"    # optional; file:// and gs:// work too
```

Snapshot archives upload as `.tar.gz` through `object_store`; credentials come
from each backend's standard environment variables. On non-Btrfs filesystems
snapshots return a clear error (sized volumes fall back to loop-mounted ext4).

### Apps

```toml
[app.web]
image = "proc-grill:image-ignored"
command = ["target/debug/testapp", "--mode", "healthy", "--port", "8080"]
port = 8080

[app.web.health]
path = "/healthz"
interval = 10
timeout = 5
```

The `image` field is required for the Linux runc runtime but **ignored by ProcessGrill**, which runs the `command` directly as an OS process. ProcessGrill examples use `proc-grill:image-ignored` to make this explicit. If no `command` is set, ProcessGrill falls back to `sleep 86400`.

On runc, an image app runs the way Kubernetes would run it. Everything below is optional:

```toml
[app.cache]
image = "public.ecr.aws/docker/library/redis:8.8.0"
# command = ["redis-server"]        # replaces the image's Entrypoint (and drops its Cmd)
args = ["--maxmemory", "64mb"]      # replaces the image's Cmd, keeps its Entrypoint
# working_dir = "/data"             # default: the image's WorkingDir, else /
# run_as_user = 999                 # default: the image's User, else 0
# run_as_group = 999
```

The image's `Env` is merged under the app's `env` (the app wins on a clash). Every rootful runc container runs in a user namespace: container uid 0 is host uid 2,000,000,000, so an image that runs as root (Redis, nginx) can `chown` its files and bind port 80 without being root on the node. Keep host ids `2000000000`–`2000065535` out of `/etc/subuid` and your directory service.

Volumes follow the container's user:

```toml
[[app.cache.volumes]]
path = "/data"                      # managed: handed to the container user on first mount

[[app.cache.volumes]]
path = "/import"
source = "/srv/import"              # host path: never chowned by Bun
```

A managed volume is `chown`ed to the container process's host uid and gid (container uid `u` is host uid `2000000000 + u`) the first time it's mounted, and left alone after that, so an entrypoint that hands `/data` to a service user keeps it that way across restarts. If the image's `USER` (or `run_as_user`) changes, files the previous user owned move to the new one. A host-path directory stays as you made it: own it by the mapped uid (`sudo chown 2000000999:2000000999 /srv/import` for container uid 999) or make it world-writable, otherwise Bun logs a warning at start and the container can only read it. There is no `fs_group`; `relish import` drops Kubernetes `fsGroup` with a warning.

An app that serves Prometheus metrics declares where, and every node scrapes its own instances of it (no Prometheus install needed):

```toml
[app.web]
image = "proc-grill:image-ignored"
command = ["target/debug/testapp", "--port", "8080"]
port = 8080
metrics = {}                              # scrape http://<instance>:8080/metrics
# metrics = { port = 9797, path = "/prom" } # a separate metrics listener
```

`port` defaults to the app's `port` and `path` to `/metrics`; an app with neither port is rejected. The metrics port needn't be published: the node scrapes the instance's own address. Samples are labelled `app` (`namespace/app`), `namespace`, `instance` and `node`, plus an `up` gauge per instance (1 when the last scrape succeeded). Read them with `relish metrics <app>` or on the app's dashboard page. The node-level `[metrics] app_scrape_interval_secs` (default 10) sets how often; `[[metrics.scrape_targets]]` still scrapes fixed URLs outside any app. Kubernetes imports fill `metrics` from the pod template's `prometheus.io/scrape`, `prometheus.io/port` and `prometheus.io/path` annotations.

### Jobs

Jobs are run-to-completion tasks. They retry up to 3 times with exponential backoff on failure.

```toml
[job.migrate]
image = "proc-grill:image-ignored"
command = ["echo", "migration complete"]
```

### Init containers

Init containers run sequentially before the main app starts. If any init container fails, the app transitions to Failed.

```toml
[app.web]
image = "proc-grill:image-ignored"
command = ["sleep", "60"]

[[app.web.init]]
command = ["echo", "initialising database"]
```

For the full configuration reference (resource limits, replicas, environment variables, volumes, secrets, namespaces), see the book chapter [Hello, Container](book/01-hello-container.md).

## Runtime auto-detection

When `--runtime auto` (the default), bun checks what's available:

1. **macOS**: uses ProcessGrill, even if the Apple Container CLI is installed
2. **Linux**: looks for `runc` in PATH → uses RuncGrill
3. **Fallback**: uses ProcessGrill (always available)

Override with `--runtime process` or, on Linux, `--runtime runc`. Direct `--runtime apple` selection refuses with guidance to the managed Linux VM quickstart. Selecting a runtime unavailable on your platform also produces an error.

## API

The bun agent exposes a local HTTP API on port 9117:

| Method | Path | Description |
|--------|------|-------------|
| `GET` | `/v1/health` | Agent liveness check |
| `GET` | `/v1/readiness` | Authenticated critical-subsystem readiness evidence (200 ready, 503 fenced) |
| `GET` | `/v1/capabilities` | Authenticated live DNS, egress and node-readiness evidence |
| `GET` | `/v1/capabilities/cluster` | Bounded authenticated capability evidence from every expected node |
| `GET` | `/v1/diagnostics` | Bounded local disk, cgroup throttling and public certificate evidence |
| `GET` | `/v1/diagnostics/apps` | Desired replicas, scheduled replicas and service exposure used by diagnostics |
| `POST` | `/v1/path` | Run fixed DNS and TCP probes from a local source workload and return live service/firewall evidence |
| `POST` | `/v1/test/leases` | Create a policy-authorised, server-owned Phase 15 app lease |
| `GET` | `/v1/test/leases/{id}` | Inspect an owned lease (or inspect any lease as unscoped Admin) |
| `POST` | `/v1/test/leases/{id}/renew` | Renew an active owned lease within the server TTL ceiling |
| `DELETE` | `/v1/test/leases/{id}` | Start cleanup; 202 while owners remain, 204 after confirmed retirement |
| `POST` | `/v1/nodes/decommission` | Unscoped Admin attestation; permanently retire a stopped/fenced identity and resolve its cluster lease duties |
| `POST` | `/v1/test/leases/retired` | Internal system-only acknowledgement of an exact lease/application/node retirement |
| `POST` | `/v1/deploys/operations/{id}/cancel` | Request node-local cooperative deploy cancellation (Deployer, all target scopes) |
| `POST` | `/v1/apply` | Deploy workloads (TOML body) |
| `GET` | `/v1/deploys/active` | Live accepted deploy operations, phases and current targets |
| `GET` | `/v1/deploys/operations` | Live operations plus the newest 50 terminal outcomes |
| `GET` | `/v1/deploys/history/{app}` | Per-app version/spec history used by rollback |
| `GET` | `/v1/status` | List all instances |
| `GET` | `/v1/status/{app}/{namespace}` | Status for a specific app |
| `POST` | `/v1/stop/{app}/{namespace}` | Stop an app |
| `GET` | `/v1/logs/{app}/{namespace}` | Captured stdout/stderr (`?tail=N&follow=true`) |
| `POST` | `/v1/exec/{app}/{namespace}` | Execute a command (JSON body: `{"command":["..."]}`) |
| `GET` | `/v1/cluster/nodes` | List cluster nodes (gossip membership) |
| `GET` | `/v1/cluster/council` | Council (Raft) status |
| `POST` | `/v1/cluster/join` | Join with a single-use token, node ID, CSR and format compatibility |
| `POST` | `/v1/cluster/renew` | Renew the authenticated TLS node’s CSR on the leader; requires the service token, current peer certificate and format compatibility |
| `GET` | `/v1/chaos/status` | Show the replicated node-experiment reservation, if any |

The CLI uses this API internally. You can also call it directly:

```sh
curl http://127.0.0.1:9117/v1/health
curl -H "Authorization: Bearer $RELIABURGER_TOKEN" \
  http://127.0.0.1:9117/v1/readiness
curl -H "Authorization: Bearer $RELIABURGER_TOKEN" \
  http://127.0.0.1:9117/v1/capabilities
curl -H "Authorization: Bearer $RELIABURGER_TOKEN" \
  http://127.0.0.1:9117/v1/deploys/active
```

Phase 15's command runner and ordinary 39-case catalogue are implemented. Its
app ownership API has landed too, and the runner creates a separate lease for
every case. `[testing].allowed_operations` must include
`"provision_isolated_workloads"`; unknown and production safety classes also
need `allow_protected_mutation = true`. Lease lifetimes default to a maximum of
3,600 seconds and must stay in the validated 1-86,400 second range. A leased
apply carries `X-Reliaburger-Test-Lease: <id>` and may contain apps plus the
lease's own namespace quota declaration. Bun reserves every `rbtest-*`
namespace for this path, persists ownership across a standalone restart or in
Raft, and retries interrupted cleanup. Cluster leases retain every former
placement owner until that worker confirms runtime retirement and saves its
checkpoint. A disconnected owner keeps cleanup pending (HTTP 202); the client
reports unknown if its bounded wait expires. Placement and lease reads require
a current leader with quorum. The runner releases the lease after a
pass, failure, panic or timeout. Container cases use the official BusyBox
1.37.0 multi-architecture OCI index pinned at
`sha256:9532d8c39891ca2ecde4d30d7710e01fb739c87a8b9299685c63704296b16028`.
The provisioned runc gate uses this immutable reference; ProcessGrill cases keep
using the node's installed Bun. The deferred Apple adapter has separate development tests.

### Decommissioning a node

If a worker cannot return to confirm cleanup, first stop Bun and all of its
workloads and fault helpers, or fence the machine outside the cluster. An
unscoped administrator can then permanently retire it:

```bash
relish --endpoint https://surviving-node:9117 decommission-node worker-a \
  --workloads-stopped --reason "powered off for maintenance"
```

The command records the authenticated operator, reason and original timestamp,
resolves that node's outstanding cluster lease placements and any node-chaos
cleanup obligation, and fences future scheduling. It returns the same record on
retry. A disconnected machine is not stopped by this command: `--workloads-stopped` is your attestation of external
shutdown or fencing. Returning machines require a new node name, fresh data and
identity directories, and a newly issued join token for that name. The retired
identity cannot be reinstated or renewed. Ordinary application data volumes are
not deleted by decommissioning; recover or reattach them separately after fencing.

Send the request to a surviving member with quorum. The current leader cannot
retire itself; stop or fence it and let the surviving voters elect a leader.
Retirement refuses an in-flight membership change, a fault still owned by
another node, or removal of the remaining quorum. Add replacement voters before
reducing a small council further. Without quorum, use the documented disaster-recovery
procedure instead of treating decommissioning as a consensus bypass.

Identity retirement propagates through the existing replicated security state
and live revocation refresh (up to five seconds on caught-up nodes). New TLS
handshakes then reject any certificate naming that identity, regardless of its
serial. API requests on existing connections also check the replicated record.
Partitions cannot receive the update until they reconnect, which is another
reason external fencing is required before this command.

The `workload-identity` group deploys a leased container and reads its public
certificate bundle through namespace-scoped exec on the owning node. It checks
the chain and validity against your configured `--ca-cert`, plus the exact
SPIFFE URI for the cluster, namespace and app. It never reads the workload
private key or token, and never trusts a CA merely because it appears in the
mounted bundle. Without an explicit CA, certificate verification reports
Unknown. The group's JWKS and scoped-token cases also work with ProcessGrill.

`relish test --chaos` adds five serial recovery scenarios: council leader
failure, worker death with live replicas, a minority council partition,
bounded node pressure, and node death during a rolling deploy. Run it
interactively and type `yes`, or pass `--yes` in automation. It refuses rather
than skipping when the cluster has fewer than three nodes, fresh node-kill or
node-pressure evidence is absent, the container workload can't run, or server
policy doesn't grant `provision_isolated_workloads`, `alter_node_state` and
`saturate_capacity`. There is no client override. Every case refreshes
capability evidence after entering the serial queue and records exact fault
ids for panic- and timeout-safe reversal; uncertain cleanup is `Unknown`.

`relish bench` is implemented too. It creates a separate durable lease per
suite, measures deployment and scheduling through public APIs, executes real
DNS queries and service-VIP transfers from a source container, and cleans up
after success, error, panic or timeout. Use `--quick --output json` for a
baseline and `--compare <file>` for a strict direction-aware comparison.
Leader reconstruction needs `--disruptive --yes`; capacity saturation needs
`--capacity --yes`. Server policy still decides whether either operation may
run. Capacity requires a live council scheduler and complete ready-node evidence.
Each leased app must become running before it counts. Only a typed scheduler
refusal for the next app ends the measurement successfully, followed by a final
running check for every counted app. Missing evidence, expired leases, runtime
failures and the hard safety limit fail the suite.

`relish wtf` collects bounded evidence from every expected node using the same
authenticated client identity. It diagnoses node and council health,
crashloops, stalled deploys, missing service backends, active faults and alerts,
disk pressure, actual cgroup throttling, certificate lifecycle and Pickle
redundancy. Use `--app <name>` for application scope or `--watch` for a
human refresh every 30 seconds (`--interval <secs>` changes the period). JSON and YAML use schema version 1. Exit status 0 means
all selected evidence was observed and healthy, 1 means a critical finding,
and 2 means warnings or unknown evidence. Bounded in-memory restart and deploy
history is deliberately reported as degraded, not silently accepted as a
complete historical record.

`relish path <source> --to <destination>` walks the network path from a source
workload to a destination, hop by hop. It finds a running source instance, then
asks that node to run fixed `nslookup` and TCP-connect probes inside the
source workload. Internal destinations derive their port from the live service
map; use `--namespace`, `--to-namespace` and `--port` when needed. The five
steps cover the actual DNS answer, live userspace and attached eBPF service
state, live attached firewall state, the faults active on the path, and the TCP
result. Each step is labelled `observed`, `inferred` or `unavailable`. A
failure exits 1; missing evidence or missing probe tools exits 2 rather than
pretending the path is healthy.

External probes are denied by default. They require an Admin credential,
`probe_external_destination` in `[testing].allowed_operations`, the protected
cluster gate where applicable, and an exact `host:port` entry. Wildcards and
CIDR entries do not match:

```toml
[testing]
safety_class = "development"
allowed_operations = ["probe_external_destination"]
external_probe_allowlist = ["example.com:443"]
```

Workload faults are opt-in too. Injection needs a Deployer-or-higher
credential, explicit `--acknowledge`, and this server policy:

```toml
[testing]
safety_class = "development"
allowed_operations = ["inject_workload_faults"]
```

An Admin role does not replace the operation grant. Production and unknown
clusters additionally need the server-owned `allow_protected_mutation = true`
gate. Bun ignores the request body's audit identity, derives the readable
fault owner and stable credential principal from authentication, and emits
machine-readable `fault.injected` and clear events. Reversal needs the same
role and operation grant, but not destructive acknowledgement or the
protected-cluster mutation switch.

Node-level faults have a separate privilege boundary. The caller must be an
Admin, `[testing].allowed_operations` must contain `"alter_node_state"`, and
the command must include `--acknowledge` and a non-zero duration. `node-drain`
publishes not-ready evidence so the scheduler moves placements while cluster
traffic stays live. `node-kill` closes gossip, Raft and reporting traffic;
`--containers` also kills local workloads. Both reverse on expiry. A manual
clear needs the owning node because fault IDs are node-local:
`relish fault clear <id> --node <node>`. Once peers have removed
the failed node from their live directory, point `--endpoint` at that node's
still-running management API. An ordinary clear never removes node faults.

Node pressure uses an independent `"saturate_capacity"` permission and two
server-owned ceilings. Both default to zero, so installing Bun doesn't enable
capacity saturation by accident:

```toml
[testing]
safety_class = "development"
allowed_operations = ["saturate_capacity"]
max_node_pressure_cpu_percent = 80
max_node_pressure_memory_percent = 90
```

The caller must still be an Admin and pass `--acknowledge`. Linux cgroup v2
must expose the CPU and memory controllers; rootless and non-Linux nodes report
the capability as unavailable. `relish fault node-pressure worker-2 --cpu
80% --memory 90% --duration 60s --acknowledge` creates a dedicated helper
cgroup, never moves Bun into it, and permits only one pressure helper per node.
The CPU limit applies across all cores. Memory is a target for total node
usage, not an extra percentage, and the helper cgroup has a hard ceiling at the
requested share of physical memory. Bun kills the helper and removes the
cgroup on clear, expiry and graceful shutdown; Linux parent-death signalling
plus a startup sweep cover process death. Pressure reversal is de-escalating,
so it does not need acknowledgement or the protected-cluster mutation switch.
It still needs an Admin credential and the server's `"saturate_capacity"`
grant, including through `--node`; it never inherits authority to reverse
drain or kill from an unrelated operation.

Standalone apply streams an `Accepted` SSE event before progress, containing
the operation ID used by these endpoints. Active records report `accepted`,
`deploying_apps`, `deploying_jobs` or `rebuilding_routes`; terminal records use
the `finished` phase plus an explicit `completed`, `failed`, `cancelled` or
`unknown` outcome. `unknown` means the worker ended without terminal evidence.
It is not treated as a green deploy. Concurrent operations may target different
apps, but Bun refuses a second operation for the same namespace/name, naming
the current ID, age and phase. A failed operation retains ownership until its
rollback worker finishes. Stalled event streams close without cancelling the
worker; query the accepted ID if the stream ends without completion.
`relish cancel-deploy <operation-id>` requests cooperative cancellation on the
selected node and waits up to 30 seconds for terminal evidence. Health waits can
be interrupted, while in-flight runtime work retains ownership until it finishes.
Pending, failed or unknown results exit non-zero. Apply the corrected config too:
cancellation doesn't change cluster desired state or undo completed workloads.

Apps and jobs must use distinct names within a namespace: configuration rejects a conflicting pair, and node admission
preserves an existing instance's kind until its ownership is removed. Use a
different name or namespace for the other kind.

Release maintainers: see [the build, signing and publication procedure](releasing.md).

Direct Apple Container is disabled for 0.1.0 while interrupted daemon-command
recovery remains unfinished. Use the managed Linux/runc quickstart on macOS.

Node chaos (kill, drain, pressure and council partitions) reserves one cluster-wide
experiment slot. A deadline triggers target-side fencing and reversal; only a
confirmed cleanup releases the slot. Failed or unreachable cleanup retains the
reservation across leader changes. Workload faults retain their replica limits.


### Encrypting secrets without cluster files

Authenticated clients can fetch the public age recipient with
`GET /v1/secret/public-key`. The JSON response contains `public_key` and
`generation`, never private key material. Scoped read-only credentials may
fetch it; deployments still require their normal permissions.

`relish secret pubkey` (with no directory) makes that request for you, with the
configured endpoint, token and CA:

```sh
relish secret encrypt --pubkey "$(relish secret pubkey)" 'value'
```

Use the resulting `ENC[AGE:...]` string in an app environment variable. A
follower may briefly report the previous generation during rotation; fetch
again and re-encrypt if that generation has been finalised before deployment.
`relish test --filter secrets-config` checks actual container decryption and
config-file mounting on a cluster with a container runtime.

### OCI release qualification

These scripts exercise crash and reboot recovery on real hosts. They're manual:
each one interrupts processes deliberately, so run them only on disposable
machines.

- `scripts/release/qualify-oci-interruptions.sh` needs a Linux host with Runc,
  static BusyBox, `ip`, `nft`, a C compiler and sudo. It kills Bun and cancels
  callers at each Runc lifecycle boundary, inside private network and mount
  namespaces, and keeps logs and test-binary checksums.
- `scripts/release/qualify-oci-reboot.sh --vm DISPOSABLE_LIMA_VM` runs from the
  host. It starts real OCI executions, force-stops the VM and checks recovery
  after the new kernel boot, including address retention until explicit
  release.
- `scripts/release/qualify-discovery-reboot.sh --vm DISPOSABLE_LIMA_VM` does the
  same for the complete standalone path, carrying discovery ownership through
  a power cut and then checking retirement, explicit redeployment and same-boot
  adoption.
- `scripts/release/qualify-storage-power-cut.sh --vm DISPOSABLE_LIMA_VM
  --fixture exporter|leases|backups --iterations N` cuts the VM's power at a
  random moment while worker processes export Parquet files (three exporters
  sharing one checkpoint and lock), mutate lease stores, or upload and prune
  council backups, each logging finished operations to a synced ledger. After
  the reboot it checks that every acknowledged export is at the destination
  byte for byte, that nothing was pruned before it was exported, that every
  acknowledged lease operation survived, that the newest acknowledged backups
  are intact and restorable, and that the lock and stores are reusable. It writes a Markdown
  record with a rule-of-three bound. It refuses the shared `reliaburger-test`
  VM.

Passing them doesn't close the release gates in the
[remaining-work table](plans/2026-09-22-v0.1.0-remaining-work.md).
