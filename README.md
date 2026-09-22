<p align="center">
  <img src="assets/images/reliaburger_logo_v1.jpg" alt="Reliaburger" width="400">
</p>

# Reliaburger

One binary. A whole container platform.

Reliaburger is a batteries-included container orchestrator written in Rust,
for teams running 2-5,000 nodes who want containers in production without
the PhD. The things you normally assemble from a dozen projects — scheduling,
gossip clustering, Raft consensus, service discovery, ingress, mTLS PKI, an
OCI image registry, metrics, logs, dashboards, GitOps, chaos testing, even
rolling self-upgrade of the orchestrator itself — ship compiled into one
`bun` agent and one `relish` CLI.

No sidecars. No add-on shopping list. No YAML archaeology. You get:

- **A built-in guide.** `relish manual` provides searchable documentation
  and runnable examples without a repo checkout or internet connection.
- **A cluster that heals itself.** SWIM gossip membership, a self-healing
  Raft council, automatic rescheduling, council disaster recovery, and
  rolling binary upgrades where workloads survive the swap.
- **Security that's on by default.** Generated clusters require mTLS;
  joins are single-use-token, CSR-based; images can be signature-gated;
  secrets are encrypted at rest, with
  [public encryption keys available over the API](docs/README.md#encrypting-secrets-without-cluster-files).
  Cluster-signed ingress leaves renew on demand
  before expiry, and operator certificate files reload without a restart.
  Node leaves renew automatically through the current leader, and every node
  transport observes replacements. TLS connections have bounded lifetimes.
  `relish test --filter workload-identity` checks a container's SPIFFE certificate
  against the configured cluster CA, alongside JWKS and token-scope checks.
- **Batteries you'd otherwise deploy separately.** Built-in registry with
  P2P image distribution, time-series metrics with SQL, indexed logs,
  ingress with TLS and draining, web + terminal dashboards, and a fault
  injector for breaking things on purpose.

The full architectural vision lives in the [whitepaper](docs/whitepaper.md).
Install and usage details are in the [documentation](docs/README.md), and
implementation status in [progress.md](docs/progress.md).

0.1.0 requires a fresh cluster; development state is refused. Rolling upgrades
require matching explicit formats (currently protocol 19 and state 32). See the
[compatibility policy](docs/releasing.md#cluster-compatibility).
Registry uploads belong to their exact creating credential. Recovery reclaims
abandoned partial uploads after a crash and
requires one Bun per writable image store; see the [startup contract](docs/README.md).
Manifest pushes persist their local catalogue before acknowledgement and refuse
filesystem failures. Publication and garbage collection serialise their final
blob checks and deletion, with durable GC generations fencing delayed proposals. Clustered workers and followers forward writes to the
authenticated leader. Repository reads and configured quota checks also require
current authority; an unavailable leader returns 503 instead of an empty catalogue.
`relish images` uses that committed cluster view too.
Unconfirmed Raft commits return 503 for client retry;
201 confirms catalogue acceptance, with blob replication potentially still pending.
HTTP writes under `rbtest-…/` require their exact authenticated lease owner.
Registry workers confirm upload and metadata retirement after workloads stop;
only the owning application lease may depend on those images. Peer pulls retain
upload ownership through cancellation and failed cleanup. Storage nodes now hash and conditionally confirm their own copies under the GC
guard; the healer cannot replace stale holder lists. Physical registry recovery passes actual Bun death and three-node TLS
leader-change tests on macOS/Linux.
`relish test --filter image-registry` now stages a pinned runnable image under its
server lease, deploys the exact digest and checks its HTTP response. The real
Linux/runc catalogue passes all three cases with confirmed repository cleanup.
A disconnected writer keeps cleanup pending until it returns and confirms
retirement.
OCI index selection targets Linux containers even when the client runs on macOS.
Direct image pulls and Pickle verify pinned manifests, selected platform
manifests and configuration bytes before accepting them into the cache.

Lease-owned test volumes and generated configuration have durable provisioning
records. Ordinary Stop and rescheduling keep their data; lease retirement removes
it only after confirmed runtime cleanup. Failed unmounts keep cleanup pending.
Host-source volumes and ordinary application data remain outside test ownership.
Disposable test-volume snapshots are unsupported in 0.1.0.

Restart also refuses unreadable ownership records or uncertain runtime adoption,
preserving records and workload identities for recovery. Legacy aliases and
inconsistent stored instance identities refuse startup without runtime mutation.
Workload and namespace
names must be lowercase DNS labels; invalid names are refused before deployment.
Rollback and halt retain replacement ownership until runtime and artifact
cleanup are confirmed. Cluster test leases also retain former placement owners
through rescheduling and leader changes; an unavailable worker keeps cleanup
pending until it confirms retirement. After stopping or fencing that machine, an
administrator can use `relish decommission-node` to release its obligations and
permanently retire its identity. Returning machines need fresh enrolment under a
new name; see the [operator procedure](docs/README.md#decommissioning-a-node).
Node-local cron registrations survive restart. Cron skips missed or uncertain
firings after a crash; it does not promise catch-up or exactly-once job execution.
Job attempts retain their retry budget across Bun replacement. Unknown outcomes
require an explicit `relish apply jobs.toml --rerun-jobs`; ordinary apply cannot
silently replay them. See the [job recovery policy](docs/README.md).

## Quick start

Source builds require Rust 1.97 or later; releases use Rust 1.98.0 and the
committed lockfile.

```sh
cargo build --locked --bins

# Run the node agent — no container runtime needed for the first taste
target/debug/bun --runtime process

# In another terminal: deploy, inspect, explore
target/debug/relish apply examples/phase-1/proc-first-run.toml
target/debug/relish status
target/debug/relish            # interactive terminal dashboard
open http://localhost:9117/    # web dashboard
```

Process launches and completed job outcomes survive Bun replacement through
private durable owners. Application deployment acknowledges success only after
its recovery metadata is written; storage failures are deployment errors.
Process exec commands also have owners: Bun death retires them while preserving
the main workload, and application cleanup waits for their confirmed retirement.

Process mode supports foreground workloads: the main process stays under Bun's
supervision, and its children must remain in the supervised process group.
Use an application's foreground option; shell wrappers should `exec` the server
or wait for their children. Daemonising or detached workloads must use the Linux
container mode instead. See the [runtime contract](docs/README.md#processgrill-built-in-fallback).

With runc installed on Linux, the same flow runs real OCI images — and `relish init cluster` generates the PKI and mTLS
config for a secure multi-node cluster. The [documentation](docs/README.md)
has the full secure-cluster walkthrough. Runc bundles and state follow the node's
configured data directory, and its cache uses the selected images directory.
On macOS, use the [managed Linux VM quickstart](docs/quickstart.md) for containers.
Direct Apple Container is disabled for 0.1.0 pending daemon-command recovery.
Rootful Linux networking retains address
ownership across restarts and refuses subnet exhaustion. Runc retirement keeps
resource ownership when OCI deletion, rootfs unmount or network cleanup fails,
and retries before reporting Stopped. Normal Stop/Retire also preserve ownership
when identity-directory or adoption-record removal fails. Rolling and blue-green
deployments refuse completion if runtime exit is uncertain and keep both
generations available for cleanup. Rollout finalisation also retains ownership
when identity or adoption-record removal fails, so cleanup can be retried.
Explicit Stop and per-instance rollout retirement now confirm kernel backend
withdrawal before stopping the runtime. Refusal retains the original destination
and its address. Natural-exit and durable discovery recovery remain release gates.
Service retirement now confirms removal of grants to its exact allocated VIP before
releasing that destination. Refusal retains the service and its cleanup owner;
unrelated destination grants remain untouched. Durable discovery recovery remains open.
Failed final kernel backend publication now reports a deployment error and retains
the running workload’s ownership for cleanup or retry.
Retirement also fences automatic restarts before signalling the old runtime.
Deployment ingress drains count requests from route selection, including failover
candidates. A deadline cancels HTTP/WebSocket work; completion waits for actual
request release. Automatic restarts refresh the confirmed DNS/ingress address and
keep health-checked replacements unhealthy until a successful probe.
Rollout identities advance past adopted generations after Bun replacement. With rootful Linux DNS
enabled, short
service names resolve in the calling workload's namespace. Host tools use explicit
names such as `redis.payments.internal`; unknown sources cannot inherit a node's
namespace.

A blocked local rollout can be cancelled with `relish cancel-deploy <operation-id>`.
The command waits for owned work to finish before you submit the correction;
cluster users should also update the desired configuration. See the
[deployment guide](docs/README.md).

## The manual is in the binary

Reliaburger documents itself. `relish manual` opens the reference as a
searchable terminal reader — chapters, runnable examples, fuzzy search —
with no repo checkout and no internet:

```sh
relish manual              # read it in the terminal (/ to search)
relish manual --web        # the same manual as one page in your browser
relish manual examples     # drop the runnable example configs right here
```

<!-- asciinema: `relish manual` demo cast goes here -->

The source ships too. `relish source ebpf` opens a fuzzy search over the
exact `src/` tree the binary was compiled from. The platform carries its own
reference, examples and implementation wherever the binary goes.

## The book

This repository is also a book. *Building Reliaburger* walks through how
every subsystem was designed and built — teaching Rust and distributed
systems along the way, aimed at programmers coming from C, Python or Go:

0. [Preface](docs/book/00-preface.md)
1. [Hello, Container](docs/book/01-hello-container.md)
2. [Finding Friends](docs/book/02-finding-friends.md)
3. [Talking to Each Other](docs/book/03-talking-to-each-other.md)
4. [Trust No One](docs/book/04-trust-no-one.md)
5. [Where the Images Live](docs/book/05-where-the-images-live.md)
6. [Watching Everything](docs/book/06-watching-everything.md)
7. [Ship It](docs/book/07-ship-it.md)
8. [Breaking Things on Purpose](docs/book/08-breaking-things-on-purpose.md)
9. [The Full Package](docs/book/09-the-full-package.md)
10. [Locking It Down](docs/book/10-locking-it-down.md)
11. [Eyes Everywhere](docs/book/11-eyes-everywhere.md)
12. [Squeezing Every Drop](docs/book/12-squeezing-every-drop.md) *(in progress)*
13. [A Room with a View](docs/book/13-a-room-with-a-view.md)
14. [Changing the Tyres at Full Speed](docs/book/14-changing-the-tyres.md)
15. [Ready for Production](docs/book/15-ready-for-production.md) *(in progress)*
- [Appendix: Rust for C, Python, and Go Programmers](docs/book/16-appendix-rust.md)

## What's inside

Thirteen burger-named subsystems in one binary — Grill (runtimes), Mustard
(gossip), Council (Raft), Meat (scheduler), Onion (discovery/DNS/eBPF),
Wrapper (ingress), Sesame (security), Pickle (registry), Mayo (metrics),
Ketchup (logs), Lettuce (GitOps), Smoker (chaos), Brioche (dashboard). The component tour and
repo layout live in the manual (`relish manual`, "Under the hood") and the
[design docs](docs/design/).

## Try it

The managed quickstart records localhost ingress and authenticated registry
forwards. See the [port options](docs/quickstart.md#resume-stop-and-remove).

Use the source-based quick start above while we prepare the first release.
From a checkout, install cargo-nextest 0.9.145 or newer (see
[build prerequisites](docs/README.md#building)), then run the portable tests and
check the examples:

```sh
make test                    # run the portable nextest suite
make audit                   # check dependency advisories
make examples                # validate and dry-run every example config
```

With a running, configured cluster and `relish` on your PATH:

```sh
relish status
relish wtf                   # diagnose the live cluster
relish test --profile development
```

The managed laptop flow is now available in the source and undergoing VM
qualification. Once the signed release and website are published, the install
path will be:

```sh
curl -fsSL https://reliaburger.com/install.sh | bash
export PATH="$HOME/.reliaburger/bin:$PATH"
relish nodes
relish status
relish logs hello
relish dashboard             # Ctrl-C stops the browser connection
# Open http://localhost:18080/
relish local stop
relish local start
relish local destroy --yes   # permanently remove the cluster's VMs and data
```

See the [laptop quickstart](docs/quickstart.md) for prerequisites, single-node
setup, retries and development qualification. The public release is still pending.

See the [diagnostics guide](docs/manual/07_diagnostics.md) for test prerequisites,
profiles and interpreting results. Capacity benchmarks require live scheduler
admission and observed running workloads; missing evidence fails the measurement.

## Getting to 0.1.0

Node startup now verifies the agent response, version and critical subsystem
readiness before setup reports success. Managed setup also checks the council
and the sample app through ingress; end-to-end qualification is still in progress.

The core platform is implemented. We're preparing a release that takes a
laptop to three healthy Linux nodes and a working sample app, with no Rust
build or repo checkout. **Under five minutes is the target; the public
installer and that timing guarantee aren't available yet.**

The [0.1.0 release plan](docs/plans/2026-09-16-v0.1.0-release-plan.md)
sets out the remaining work, in order:

1. **Establish the release baseline.** Reconcile the older TODO lists,
   define supported platforms and run the complete test matrix.
2. **Finish correctness fixes.** Make readiness checks trustworthy, bound
   registry upload memory and close the remaining consequential bugs.
3. **Package and sign the release.** Publish Linux binaries, native macOS
   CLI binaries, eBPF assets and matching release metadata.
4. **Make laptop clusters reliable.** Use prebuilt assets and managed Linux
   VMs, with secure enrolment, resumable setup and access from the host CLI
   and browser.
5. **Add the installer and first-run flow.** Install, start the cluster and
   deploy a sample app, with clear progress, errors and cleanup commands.
6. **Prove the downloadable release works.** Test clean installations,
   restart and recovery, and measure the full three-node start with empty
   caches before publishing the five-minute claim.

The [candidate and promotion workflow](docs/releasing.md#metadata-and-publication)
preserves signed assets and publishes only the qualified bytes. Hosted candidate
creation and cold-install acceptance are still pending.

Each step includes tests and updates to the documentation and book. Detailed
acceptance gates and deferred features live in the release plan; implementation
history remains in [progress.md](docs/progress.md).

The [17 September codebase audit and completion plan](docs/plans/2026-09-17-codebase-completion-plan.md)
reconciles the older TODOs, records remaining correctness gaps and separates
release acceptance from deferred capabilities. The current checklist lives in
[progress.md](docs/progress.md).

The opt-in durable Runc adapter now covers rootful and rootless Linux containers.
Its rootful service path now retains a durable, generation-bound address reference
across natural exit. Checked discovery withdrawal permits reuse; lost original
discovery ownership refuses cleanup. If live policy is lost and backend withdrawal
fails, the owned path stops execution while retaining the address and cleanup
records. Exact service snapshot restoration and a durable discovery checkpoint
now preserve original allocations and cleanup permissions. An opt-in fresh Bun
agent journals publication and original address holds before launch. Standalone
Stop and automatic retry persist release permission before returning addresses;
confirmed service retirement removes its durable owner before freeing the VIP.
Original runtime/discovery correlation checks generations, allocations and cgroup
identity, including a hold saved before discovery acknowledgement. Opt-in
standalone startup recovery reserves original allocations, replays release
permissions and publishes only positively adopted runtimes with fresh health
checks. Remote acknowledgements, actual Bun/host crash qualification and
production selection remain open. Journal I/O runs on blocking workers that
retain exclusive ownership through caller cancellation.
Rolling and blue-green cutovers now confirm kernel backend publication before
exposing replacements in DNS and ingress; refusal preserves the original routing view.
Fresh deployments also stop before workload creation when initial service publication fails.
Health updates reach DNS and ingress after kernel confirmation; refused withdrawal
blocks restart and retries on later probes. Backend-capacity failures report a
failed deployment while retaining created workloads for cleanup.
Rootful and rootless recovery tests cover caller death before adoption, short
jobs, published ports and interrupted network-helper startup.
Rootless networking is ready before the workload starts. Command waits retry
transient owner-control failures within their original deadline; losing a status
response never counts as confirmed retirement. Production selection
and discovery recovery remain release blockers
in the [OCI ownership plan](docs/plans/2026-09-20-oci-launch-ownership.md).

Workers exclude their own advertised endpoints using their configured node identity,
even before council membership arrives. Delayed catalogues cannot restore a locally
retired endpoint in DNS, resolve responses or ingress routing. Remote retirement
acknowledgements and durable discovery ownership remain release work.

Egress cleanup now retains the stopped workload's binding and adoption record
until the kernel confirms removal. Failed policy rewrites stop affected workloads;
repeated cleanup failures remain retryable. An opt-in persistent kernel loader
now keeps policy through actual loader SIGKILL and recovers the same maps without
a detach window. The agent now records original workload policy before map
writes and restores it before adoption; cleanup retains positive retirement
evidence until metadata is gone. Network rules, diagnostics and adoption now use
the container’s verified original cgroup. All 39 physical kernel tests pass.
Pre-start namespace binding, discovery cleanup, host-reboot reconciliation and
production kernel selection remain release gates. Automatic application restarts now retire the
predecessor’s adoption and policy records before creating a successor; failed
cleanup blocks replacement while preserving the logical workload identity.
Rolling and blue-green generations now use separate cgroups, so retiring the
predecessor preserves the running replacement.

Log exports now preserve content generations, scope receipts to the destination,
and serialise durable checkpoint updates across agent and offline exports. Source
and checkpoint errors stop the export and prevent disk-pressure pruning.

Cluster-wide credential management and permission/quota declarations require
an unscoped Admin. App and job manifests enforce the caller's scope and configured
permissions before applying changes; see the [user guide](docs/README.md).

## Licence

[Apache 2.0](LICENSE)
