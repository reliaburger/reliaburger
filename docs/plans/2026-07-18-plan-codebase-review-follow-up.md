# Codebase review follow-up TODO

**Created:** 18 July 2026

**Source review:** [2026-07-17-review-codebase-current-state.md](2026-07-17-review-codebase-current-state.md)

**Reviewed `main` SHA:** `8ba727f9332c35a1f6603a0f78f76a424513485c`

This is the ordered follow-up to the current-state review. It is a backlog, not
a claim that the remaining Phase 15 features have shipped. We should finish the
high-value gate before adding more diagnostic surface area to Bun. Within a
tier, order is intentional.

The labels describe scheduling value:

- **HIGH-VALUE / MUST FIX:** a security boundary, default path or published
  first-run experience is currently unsafe or broken. Do these first.
- **MEDIUM-VALUE:** important correctness, operability and Phase 15 prerequisite
  work. Start after the must-fix gate unless an item is needed by a must-fix.
- **OPTIONAL:** worthwhile simplification or experimentation that shouldn't
  delay correctness.

## High-value / must fix

### H0. Patch and continuously detect known dependency advisories

**Finding:** GitHub reported 12 open Dependabot alerts on the reviewed default
branch after the review was pushed: 2 high, 5 medium and 5 low. A fresh RustSec
scan then found newer `crossbeam-epoch`, `quick-xml` and `quinn-proto`
advisories that GitHub hadn't reported yet. The affected graph includes TLS,
archive extraction, cloud object-store parsing, system inspection and
development benchmarks. Upstream severity doesn't prove every transitive path
is reachable here, but known fixes are available and cheap compared with
carrying the uncertainty.

- [x] Upgrade `rustls-webpki` to 0.103.13, `quinn-proto` to 0.11.15, `tar` to
  0.4.46, `crossbeam-epoch` to 0.9.20 and `object_store`/`quick-xml` to
  0.14.1/0.41.0; take compatible patched `rand` releases.
- [x] Use `cargo tree -i` plus call-path review to record whether each advisory
  is direct, reachable transitive code, or compiled but unused.
- [x] Document compensating controls for the currently unpatched `thrift`
  excessive-allocation advisory and the `lru::IterMut` soundness advisory; pin
  follow-up owners rather than silently accepting them.
- [x] Add `cargo-deny` or `cargo-audit` to an owned CI/release gate with explicit,
  expiring exceptions for advisories that have no compatible fix.
- [x] Re-run portable, no-default, documentation and relevant extraction/TLS
  suites after lockfile changes.

**Acceptance:** every alert with a compatible patched release is gone. Every
remaining alert has a written reachability decision, compensating control,
owner and expiry/recheck date, and CI fails on new unacknowledged advisories.

#### H0 implementation audit record (18 July 2026)

`cargo audit` loaded 1,166 RustSec advisories and scanned 637 locked packages.
The first scan failed with four vulnerabilities. After the upgrades below it
passes with `warnings` denied and only the named exceptions in
`.cargo/audit.toml`. `make audit` makes those exceptions fail closed after 18
August 2026. The normal reusable CI workflow runs the gate on changes and
before release; a small scheduled workflow refreshes the advisory database each
Monday even when the repository is quiet.

| Package/advisory | Dependency and call-path decision | Disposition |
|---|---|---|
| `rustls-webpki` GHSA-82j2-j2ch-gfr8 and related findings | Runtime transitive dependency of `rustls`; Reliaburger constructs TLS and CRL verification paths. | Patched to 0.103.13. |
| `tar` GHSA-3pv8-6f4r-ffg2, GHSA-j4xf-2g29-59ph and GHSA-gchp-q4r4-x4ff | Direct and runtime reachable from image/build archive extraction in `grill::image` and `pickle::build`. | Patched to 0.4.46. |
| `quick-xml` RUSTSEC-2026-0194 and RUSTSEC-2026-0195 | Runtime transitive dependency of the directly used `object_store` cloud features. A malicious or compromised configured S3/GCS endpoint can reach response parsing. | Upgraded `object_store` 0.12.5 to 0.14.1 and `quick-xml` to 0.41.0; migrated the extension-trait and path APIs. |
| `crossbeam-epoch` RUSTSEC-2026-0204 | Runtime reachable through `sysinfo`; also present through Criterion in development. | Patched to 0.9.20. |
| `quinn-proto` GHSA-6xvm-j4wr-6v98 and RUSTSEC-2026-0185 | Present only in the lockfile as reqwest's optional HTTP/3 graph. `cargo tree --target all` prints no active path because Reliaburger doesn't enable HTTP/3. | Patched anyway, to 0.11.15. |
| `rand` GHSA-cq8v-f236-94qc | Direct and transitive versions are compiled; the advisory affects the old range. | Patched the 0.8 and 0.9 lines to 0.8.6 and 0.9.3. |
| `thrift` GHSA-2f9f-gq7v-9h6m | Runtime reachable through Parquet/DataFusion when the operator runs `relish logs-query remote` against a local or remote archive. There is no fixed `thrift` release. | **Temporary risk acceptance.** Query only Bun-produced Parquet in operator-controlled object stores; don't point the command at untrusted archives. A crafted trusted-store object can still cause process memory exhaustion, so this isn't a complete defence. Phase 15a/M2 owns replacement or upstream upgrade; recheck by 18 August 2026. GitHub Dependabot remains the detection source because RustSec doesn't currently carry this GHSA. |
| `lru` RUSTSEC-2026-0002 | Transitive through ratatui 0.29. Its layout cache uses `LruCache`, but source review finds no call to the affected `LruCache::iter_mut` API. | **Temporary exception.** No reachable affected call. Phase 15a/M2 owns the ratatui/lru upgrade; CI exception expires 18 August 2026. |
| `anyhow` RUSTSEC-2026-0190 | Direct error type. Repository call-path review finds no `Error::downcast_mut`, the affected API. No fixed release exists. | **Temporary exception.** Avoid `downcast_mut`; Phase 15a/M2 owns upstream recheck; CI exception expires 18 August 2026. |
| `bincode`, `rustls-pemfile`, `paste`, `proc-macro-error` informational advisories | `bincode` persists Raft vote/log-id metadata and needs a format migration. `rustls-pemfile` parses configured TLS material. `paste` and `proc-macro-error` are build-time transitive macros. These are maintenance notices rather than published vulnerabilities. | **Temporary exceptions.** Phase 15a/M2 owns migration/upgrade; CI exceptions expire 18 August 2026. |

The remaining `lru` and `thrift` GitHub alerts therefore stay visible. They are
not being called fixed. The acceptance decision is narrowly about known API
reachability and trusted input, with a forced re-review date.

**Verification:** `make audit`, `cargo fmt --all -- --check` and portable
all-target clippy pass. `make test` runs 2,633 tests (all pass; 39 separately
skipped), while `make test-no-default` runs 2,615 (all pass; 39 skipped).
`make test-doc` also passes and still discovers zero doctests, as tracked in
O3. Those portable suites include the image/build archive extraction, TLS/CRL,
snapshot upload, council backup, object-store export and remote log-query
tests. The macOS `make lint` all-feature target still fails at the known
Linux/Aya module boundary recorded in M2; the portable all-target clippy gate
passes, so that platform limitation isn't misreported as an H0 regression.

### H1. Contain the API authentication bootstrap window

**Finding:** SEC-1. Standalone Bun skips the empty-token bind guard while its
router still creates an empty token store, exposing administrative routes when
the operator binds it beyond loopback.

- [x] Write black-box startup tests covering standalone loopback, wildcard,
  non-loopback and hostname listeners, plus clustered empty-token startup.
- [x] Construct one explicit API authentication mode before router and listener
  creation. Don't let `Option<TokenStore>` decide security policy implicitly.
- [x] Permit an empty token store only on an IP-literal loopback listener (or a
  future Unix socket). Reject unresolved hostnames rather than assuming safety.
- [x] Make the startup error name both remedies: bind to loopback or configure a
  token/authenticated cluster identity.
- [x] Update the security design and book where they describe first-start API
  access.

**Acceptance:** remote administrative routes are never served with empty-token
authentication; loopback bootstrap remains usable; portable startup tests cover
both standalone and clustered construction.

**Delivered on the hardening branch:** Production Bun now constructs one token
store in every mode. Standalone validates the listener immediately after config
parsing; clustered Bun validates the Raft-populated store before bind. Five real
binary tests and three policy unit tests cover the boundary. Portable clippy,
2,633 default tests and the no-default H1 suite pass.

### H2. Fail closed when a declared egress policy can't be enforced

**Finding:** SEC-3. Workloads with an egress allowlist currently deploy after a
warning when eBPF or the required socket-address hooks are absent.

- [x] Write tests first for no eBPF handle, incomplete attachment, complete
  attachment, map update failure and enforcement loss after deployment.
- [x] Represent live IPv4/IPv6 TCP/connected-UDP and unconnected-UDP hook state
  plus the pre-start runtime contract as a typed node capability.
- [x] Reject placement/deployment of a policy-bearing workload unless the
  selected node reports every hook its policy needs.
- [x] Keep the agent-side pre-start check as a second fail-closed boundary so a
  stale scheduler capability can't create an enforcement gap.
- [x] Make later hook loss degrade readiness and expose the affected workloads;
  define whether Bun fences or stops them before implementing recovery.
- [x] Reconcile the security design's deny-by-default claim with the exact
  capability and failure contract.

**Acceptance:** a declared allowlist is either live on every required address
family or the workload isn't started. There is no warning-only success path.
Privileged Linux tests prove the hooks and map updates, while portable tests
prove capability/placement decisions.

**Delivered on the H2 branch:** Bun now requires `connect4`, `connect6`,
`sendmsg4` and `sendmsg6` plus a runtime that honours the prepared cgroup path.
This closes the previously undocumented unconnected-UDP `sendto()` bypass as
well as the startup window. Placement and agent admission both fail closed;
`allow_franchise` is refused explicitly while it remains unimplemented. A
one-second live check repairs a missing enforcement flag once, then stops the
affected app when a hook, map read or repair is lost. The incident remains in a
separate, rolling-upgrade-safe capability report and keeps the node unready
until all hooks recover. Stale capability reports are withdrawn rather than
trusted. Post-start policy installation has been removed, and pre-start writes
scrub recycled cgroup IDs before enabling their new policy.

Portable evidence: all-target compilation, warnings-as-errors clippy, 51 focused
egress tests, 36 focused reporting tests, 2,646 default tests and 2,628
no-default tests pass. Linux evidence: all targets compile with `--features
ebpf`; root-only tests pass for four-hook load, IPv4 and IPv6 TCP/UDP
allow/deny, create-program-start ordering, programming failure with no running
process, recycled-cgroup scrubbing and live hook-loss fencing.

### H3. Make `.internal` DNS reachable, supervised and schedulable

**Finding:** NET-1. Enabling the userspace responder with its default loopback
address writes an unreachable nameserver into runc containers. Bind failure also
happens in a detached task and doesn't fail Bun startup.

- [x] Write a real runc/netns test that resolves a mapped `.internal` service
  through the address written to the workload's `resolv.conf`.
- [x] Derive or configure a container-reachable responder address for runc; keep
  host loopback only for runtimes whose network model can reach it.
- [x] Bind the responder before Bun reports readiness and propagate bind/startup
  errors to the supervisor.
- [x] Publish DNS readiness and supported address families as live capabilities.
- [x] Refuse a deployment that requires `.internal` discovery when its selected
  runtime/node can't reach a ready responder.
- [x] Keep the TC implementation in `poc/dns-tc/` as evidence and a possible
  future fast path. Don't move it into production until it beats the corrected
  userspace design on simplicity or material performance.
- [x] Reconcile the DNS design, configuration reference and book with the runtime
  address rules and failure behaviour.

**Acceptance:** a default supported runc deployment can resolve `.internal`
names, a bind conflict prevents ready startup, and unsupported runtime/address
combinations fail before workload creation.

#### H3 implementation audit record (18 July 2026)

Rootful runc now derives the node side of its workload veth as the resolver
address. Bun binds UDP and TCP port 53 on that precise address before it starts
the agent or adopts a workload; Linux `IP_FREEBIND` closes the first-workload
ordering gap without opening wildcard port 53 or fighting the host resolver.
Each OCI bundle gets its own read-only `/etc/resolv.conf` bind mount, so nodes no
longer mutate a shared unpacked image rootfs.

DNS readiness (enabled, ready, IPv4/IPv6 and workload reachability) travels in
an additive reporting frame. The H2 positional bincode frame remains byte
compatible in both directions during a rolling update. DNS and egress evidence
have independent receive-time leases and leadership epochs, so one heartbeat
can't keep the other capability alive. The leader takes DNS-required mode from
its configuration rather than inferring it from currently fresh leases; losing
every DNS report therefore leaves zero eligible nodes instead of disabling the
placement constraint. The local supervisor repeats the admission check before
runtime creation.

The responder refuses every query from a source outside its private/loopback
ACL, bounds both upstream UDP work and DNS-over-TCP clients, and gives each TCP
client a deadline. Either serving loop ending makes the combined task end; Bun
then cancels the node so reporting leases expire. Rootless runc, ProcessGrill,
Apple Container, non-port-53 and IPv6-only/host-loopback runc configurations
fail before workload creation. They aren't described as working fallbacks.

Linux acceptance evidence: the checked-in ignored test
`runc_netns_resolves_internal_name_through_mounted_resolv_conf` ran as root in
the `reliaburger-test` VM. It created two real network namespaces and veths,
started concurrent Alpine runc workloads, printed both generated resolver files
and resolved `redis.internal` to its mapped VIP through the gateway; both
workloads exited 0. Strengthening the test from one workload to two exposed two
existing prerequisites: prefix-truncated veth names collided for long replica
IDs, and a second pull cleared the same content-addressed rootfs generation
under the first workload. Long interface names now use a stable whole-ID hash,
and image generations are serialised, completion-marked and reused without
destructive re-extraction.
The all-target Linux/eBPF Clippy gate also passes. Portable evidence: all-target
warnings-as-errors Clippy, 13 DNS wire tests, reporting wire/lease tests and the
documentation suite pass. `make test` runs 2,661 tests (all pass; 39 separately
skipped), and `make test-no-default` runs 2,643 (all pass; 39 skipped).

### H4. Make generated clusters use mTLS by default

**Finding:** SEC-2. `relish init` creates node identity material but leaves
`require_mtls = false`, so the supported cluster bootstrap path uses plaintext
transports unless an operator discovers and changes the switch.

- [x] Write init/config tests first which prove generated clustered configs
  require mTLS and deliberately generated development configs don't.
- [x] Set `require_mtls = true` in normal generated cluster configuration.
- [x] Replace the quiet false path with an explicit, conspicuous
  development-only plaintext choice and warning.
- [x] Add a real multi-node acceptance test that verifies Raft, reporting and
  cross-node calls use the configured identities.
- [x] Update the quick start, configuration reference, security design and book.

**Acceptance:** following the normal initialisation path produces encrypted,
mutually authenticated cluster transports without hand-editing configuration.

**Delivered:** normal `relish init` writes `require_mtls = true`; the explicit
`--development-plaintext` path, Lima dev generator and Bun startup all warn on
the local-only exception. Peer API clients now present their node certificate
and use the live CRL instead of server-authenticated TLS with a fresh empty
revocation view. The cluster-gated acceptance starts three real runtimes and
proves council convergence, all-node reporting and a certificate-bearing peer
API request. An end-to-end init-to-Bun test also caught and fixed the generated
security bootstrap's `0644`/required-`0600` permission mismatch, which had made
the supported bootstrap path reject its own output. Rebased on merged H3, the
H4 worktree passed 2,666 portable tests (40 skipped), 2,648
no-default tests (40 skipped), all 21 cluster acceptances, all-target Clippy
with warnings denied and doc tests.

### H6. Isolate writable runc root filesystems per workload

**Finding:** the strengthened H3 acceptance test found that replicas using the
same image also use the same unpacked rootfs generation with `readonly = false`.
H3 prevents a repeated pull from deleting that live generation, but it doesn't
make writes private: one compromised or merely untidy workload can still alter
files seen by another workload on the node.

- [x] Write a rootful-runc test in which two concurrent containers use the same
  image, mutate the same path and prove that neither observes the other's write.
- [x] Keep the content-addressed generation immutable and give every instance a
  writable overlay/snapshot (or an equivalently isolated upper layer).
- [x] Make create failure, normal stop, timeout, panic and Bun adoption clean up
  or recover the per-instance mount without deleting a live lower generation.
- [x] Define and test the rootless-runc behaviour separately; don't silently
  fall back to a shared writable tree when overlay support is unavailable.
- [x] Update the Grill design and Chapter 5 with the mount ownership/lifecycle.

**Acceptance:** two workloads using one image can't read or corrupt each
other's rootfs changes, while restart/adoption keeps each workload's own files
and cleanup leaves no mounts behind.

### H5. Replace the broken published first-run sequence with an executable one

**Finding:** DOC-1. The whitepaper quick start uses commands and output that
don't match current clap definitions or `relish init` behaviour.

- [x] Define one canonical standalone first run and one minimal clustered first
  run using the current binary boundaries and ports.
- [x] Add a documentation smoke test that executes or dry-runs every Reliaburger
  command in those sequences in temporary directories.
- [x] Generate/check command snippets against clap so flags such as positional
  `apply` paths and required `join --node-id` can't drift silently.
- [x] Correct the whitepaper, top-level README, `docs/README.md` and relevant book
  walkthroughs together.

**Acceptance:** a new operator can copy the published sequence on a supported
runtime, and CI rejects future command drift.

**Delivered on the H5 branch:** the portable path now builds all binaries,
forces ProcessGrill and applies a collision-free `proc-first-run.toml`. The
secure path creates a directory and mTLS cluster, starts Bun explicitly,
creates the first Admin token over the generated CA and applies an executable
BusyBox sample whose command and port don't depend on an ignored image
entrypoint. `--endpoint`/`RELIABURGER_ENDPOINT` make Relish usable against an
explicit API and keep the tests on ephemeral ports; plaintext endpoints are
restricted to IP-literal loopback. Four black-box tests run both real Bun
processes, the HTTPS/token path and the documented clap shapes. Portable
Clippy, 2,662 default tests, 2,644 no-default tests, all 21 cluster tests,
doctests and the dependency audit pass. macOS `--all-features` Clippy still
hits the pre-existing Linux/Aya target-boundary failure owned by M2; the
supported all-target portable gate passes with warnings denied.

### H7. Make post-bootstrap node enrolment possible

**Finding:** the executable H5 audit found a prerequisite that DOC-1's old
three-node story hid. `relish init` creates one 15-minute, single-use join
token. The second node consumes it, and no API or CLI can create another.
`RaftRequest::CreateJoinToken` and `sesame::join::generate_new_join_token`
exist, but nothing reachable connects them. `relish token create` creates an
API bearer token, not a join token. A fresh cluster therefore cannot enrol a
third node through the supported interface.

- [x] Write an authenticated API contract and CLI command dedicated to join
  tokens; don't overload API bearer-token commands.
- [x] Require Admin authorisation, validate a bounded TTL and commit only the
  hash plus expiry to Raft. Print the plaintext exactly once.
- [x] Prove two separately minted tokens enrol two distinct CSR-bearing nodes,
  while reuse, expiry, a non-Admin principal and a follower/leader transition
  fail safely.
- [x] Add a three-node executable provisioning walkthrough after the command
  exists, replacing the temporary one-extra-node limitation with tested
  issuance, enrolment and startup steps.

**Acceptance:** an authenticated operator can enrol enough nodes to form a
three-voter council without regenerating PKI or editing Raft state, and the
full sequence is covered by a real cluster test.

**Delivered:** `relish join-token create --ttl 15m` calls an Admin-only API,
accepts `1s..=1h`, and returns plaintext only after the hash and expiry commit.
A real-binary test creates two credentials, enrols two independently generated
CSRs, proves Deployer/reuse/expiry refusal, starts both Bun joiners and waits
for all three nodes to converge on the voter set. A separate three-member Raft
test proves a follower's refused token never appears in state and issuance
continues after leader replacement. The regression gate passes 2,670 portable,
2,652 no-default and all 21 gated cluster tests; warnings-as-errors Clippy,
doctests and the RustSec audit also pass.

## Medium-value

### M1. Expose subsystem readiness and death as live evidence

- [x] Add `Starting`, `Ready`, `Degraded` and `Stopped` state with last error/time
  for critical long-lived tasks (FUNC-3).
- [x] Keep `/v1/health` as liveness; add readiness and authenticated capability
  evidence for scheduling and Phase 15.
- [x] Restart only reconstructible tasks with explicit ownership and deadlines.

**Delivered:** one process-wide tracker pre-registers the complete critical
owner set before startup and records state transitions, last failure/time and
restart count. Authenticated `/v1/readiness` and `/v1/capabilities` expose it;
an independent rolling-safe reporting frame gives the scheduler a
receive-time/leadership-epoch lease whose absence fails closed. Unique
socket/channel owners never respawn. The reconstructible security refresher
uses explicit retry, recovery and shutdown bounds.

### M2. Repair cheap executable checks and platform lint

- [x] Make `make examples` pass `--dry-run`, preserve useful error output and fix
  the two stale Phase 8 examples (FUNC-2).
- [x] Gate Linux/Aya modules at module boundaries so the advertised macOS
  all-feature lint contract is coherent.
- [x] Add dependency-advisory scanning with a pinned policy and CI ownership.

**Delivered:** `make examples` now invokes the already-built Relish binary with
`--dry-run`, prints the captured diagnostic for a failing file and validates all
21 checked-in configs. The stale Phase 8 namespace and health durations are
corrected, and portable Linux CI owns the target. Aya-dependent branches now
require both the `ebpf` feature and Linux, while no-eBPF stubs remain available
on macOS; hosted macOS runs the same all-target/all-feature Clippy gate as Linux.
The merged RustSec policy remains change-, release- and weekly-gated with named,
expiring exceptions. All three checks pass on this change.

### M3. Make clustered registry defaults peer-reachable

- [x] Derive a peer-reachable bind in cluster mode or reject an incomplete
  clustered registry configuration (FUNC-1).
- [x] Include replication/P2P reachability and redundancy in capability and
  `wtf` evidence.

**Delivered:** standalone Bun retains the loopback registry default. Clustered
Bun derives that default to its gossip-advertised IP, accepts a wildcard or the
same explicit IP, and fails startup when an explicit listener excludes the
address peers use. Cluster registry reads and writes fail closed from the first
request, including when a misconfiguration leaves the service token absent.
`/v1/capabilities` publishes the bound socket, readiness, TLS/P2P state,
redundancy target, active membership and under-replicated layer count for Phase
15 diagnostics. Seven bind/evidence tests plus both clustered-bootstrap auth
regressions pass.

### M4. Carry the configured trust domain into workload identities

- [x] Pass immutable cluster identity into the agent instead of hard-coding
  `default` (FUNC-4).
- [x] Add a non-default-cluster SPIFFE issuance and verification test.

**Delivered:** `[cluster].name` is a validated DNS-style trust domain with a
backwards-compatible `default`. All three config generators persist the requested
cluster name. Bun passes it as immutable agent and API state, so app, job, OIDC
and persistent build-signer identities use one domain. The acceptance test
issues a `payments.prod` workload leaf, validates its CA chain and checks its
URI SAN; focused config, generator and signer tests cover the surrounding data
path.

### M5. Preserve rootless published ports through Bun replacement

- [x] Persist rootless proxy parameters and ownership, respawn them during
  adoption, and test a real replacement (FUNC-5).

**Delivered:** rootless runc now starts and owns `slirp4netns`, applies the OCI
port mapping through its API socket, and persists the socket, container PID,
mapping and owner PID/start-time fingerprint in schema-v2 instance records. An
adopter reclaims a surviving owner or safely replaces a missing one before it
reports the workload adopted. The real non-root Linux test kills the original
proxy, creates a replacement Grill, and proves the same host port serves the
same container afterwards. That test also exposed and removed an invented,
undelegated systemd cgroup path which made rootless runc fail at startup.

### M6. Publish real deployment operation state

- [x] Give deploys stable operation IDs, phases, start times and outcomes; expose
  active state plus bounded history (FUNC-6).
- [ ] M8: build `wtf` deploy-stuck logic from this evidence.

**Delivered:** every real Bun deploy worker receives a time-based monotonic ID
which appears as the first standalone SSE event. Its accepted, app, job and
routing phases, current target, timestamps and terminal outcome are queryable
through `/v1/deploys/active`; `/v1/deploys/operations` adds the newest 50
terminal records. Same-target concurrent deploys fail before mutation, a lost
SSE client doesn't lose the outcome, and a worker which disappears without a
terminal event becomes `unknown`. The existing per-app rollback history remains
a separate contract. Phase 15 `wtf` will consume this evidence in M8.

### M7. Decide and document the v1 ingress/TLS contract

- [x] Either implement and accept-test automatic ACME with production-safe
  defaults, or mark it deferred and correct the whitepaper/examples now.

**Decision:** ACME is deferred. The v1 route contract is deliberate plain HTTP,
`tls = "cluster"`, or `tls = "explicit"`; `auto`, `acme` and unknown values fail
route rebuild. Every plaintext request to a TLS route now gets a 308, including
the ACME challenge prefix which previously bypassed the redirect despite there
being no responder. Kubernetes Ingress TLS imports use the cluster CA and emit
a review warning because Kubernetes TLS Secret material isn't imported. The
whitepaper, Wrapper/Sesame/Bun/Brioche designs, Chapter 3 and Rust API comments
now distinguish this shipped contract from the deferred issuer design.

The same audit found a prerequisite for Phase 15: cluster leaves stay in an
in-memory cache until process replacement and explicit files load only at
startup. M8 must add certificate expiry evidence and should not make
`wtf certificate-expiry` green until renewal or hot reload is real.

### M8. Implement the corrected Phase 15 prerequisites and catalogue

- [x] Land schema-versioned result/evidence/profile contracts, one inherited
  absolute deadline, panic-safe ownership, independently verified cleanup and
  a typed server-owned safety policy. Unknown clusters are protected and the
  old client-side production override no longer exists.
- [x] Replace startup booleans with fresh expiring capability/evidence reports.
- [x] Add server-owned durable resource leases and a hermetic OCI workload.
  - [x] App/namespace-resource lease API, standalone/Raft ownership and
    restart-safe cleanup. Lease and resource counts, TTLs, forwarding and
    cleanup waits are server-bounded; reserved namespaces cannot bypass
    ownership.
  - [x] Make the runner use those leases for pass, failure, panic and timeout.
  - [x] Add a pinned multi-architecture OCI workload accepted through both
    runc and Apple Container. Keep the ProcessGrill helper as a separate
    profile.
- [x] Keep the delivered 39-case ordinary catalogue across all 13 groups.
- [ ] Add chaos primitives only after the safety, evidence and ownership gates.
- [x] Implement authenticated real drain/kill before C1/C2/C5.
- [x] Implement node-scoped pressure before C4. Unsupported scenarios must
  not become green skips.
- [x] Separate service-data-plane and council transport partitions; make the
  former prove a real source-cgroup/VIP/port eBPF effect and refuse delay or
  bandwidth until a TC packet path exists.
- [x] Use explicit `Pass`, `Fail`, `Skipped` and `Unknown`; a full profile fails
  on missing required capabilities, timeout or unknown evidence.
- [ ] Continue with fingerprinted benchmarks, telemetry-backed `wtf`, observed
  source-namespace trace, documentation and real-cluster acceptance.
- [ ] Add certificate expiry evidence and production rotation/renewal before
  treating the TLS-expiry diagnostic as an accepted capability.

**Capability/evidence tranche delivered:** authenticated
`GET /v1/capabilities` retains the v2 summary fields for compatibility and adds
schema-v3, 15-second evidence. Each fact is `available`, `unavailable` or
`unknown`; a stale snapshot is unknown, never absent. The report includes build
target/profile, runtime/version, rootless mode, kernel, architecture, cluster
identity, readiness, placement evidence and server-owned operation policy.
Stores without a published freshness timestamp remain unknown. Cluster-mode
nodes advertise drain and kill as available. Rootful Linux nodes advertise
node pressure only after the owned cgroup/controller startup probe succeeds and
the server has non-zero pressure ceilings.

`GET /v1/capabilities/cluster` fans out concurrently to current members with
one five-second absolute deadline. It presents the cluster service credential
over the configured cluster HTTP client, refuses an anonymous fallback, caps
responses at 1 MiB, checks schema, node identity and expiry, and retains every
failed peer as an explicit unknown result. Volume semantics, telemetry
freshness and certificate expiry remain honest unknown or unavailable
prerequisites.

**Node-failure tranche delivered:** `node-drain` publishes degraded critical
readiness while leaving gossip, Raft and reporting open. The scheduler
therefore withdraws the node and re-plans its placements, then admits it again
after reversal. `node-kill` reference-counts a shared transport gate across
inbound and outbound gossip, Raft and reporting; peers observe a genuine failed
member and ordinary failover runs. `--containers` additionally kills every
local workload instance.

Both operations require a real Admin credential, the server-owned
`alter_node_state` grant, an explicit `--acknowledge`, a named target and a
non-zero TTL. A source node forwards the caller's credential and the target
repeats the checks. Clearing a node fault has the same boundary:
`relish fault clear <id> --node <name> --acknowledge`. A general Deployer clear
leaves node faults intact. Automatic expiry is target-local and always remains
available; if gossip has already dropped the target from its live routing
table, manual reversal must address that node's still-live management endpoint
directly. A three-node acceptance observes a follower leave and rejoin through
the real transports, and proves a second voter failure is refused while the
first remains down. Durable lease ownership for node state is still open and
the chaos catalogue must not claim it yet.

**Node-pressure tranche delivered:** `relish fault node-pressure` routes to a
named node and requires Admin, the independent `saturate_capacity` operation,
explicit acknowledgement and server-owned CPU/memory ceilings. Both ceilings
default to zero and validate at or below 90%. A rootful Linux node creates one
dedicated cgroup and helper outside Bun, applies a whole-node CPU quota and
allocates only enough resident memory to reach the requested total-node usage.
Clear, TTL and graceful shutdown remove the helper and cgroup;
`PR_SET_PDEATHSIG` plus startup sweeping cover process death. Rootless,
non-Linux and missing-controller nodes publish unavailable evidence. A
privileged cgroup-v2 acceptance proves the effect, isolation and cleanup.

**Network-fault honesty tranche delivered:** service `Partition` and
gossip/Raft `CouncilPartition` are distinct variants with distinct safety
semantics. Bun rejects a service partition without a loaded eBPF path, resolves
the source app's live cgroup ids server-side, rolls back partial map writes and
records the exact keys for reversal. A root-only Linux acceptance proves a
source-scoped key returns `EPERM` before any packet reaches the backend and
that deleting the key restores the connection. The 22-test cluster gate proves
the real transport endpoint still enforces the voter quorum budget. Delay and
bandwidth now fail explicitly even on eBPF-capable nodes: the connect hook
cannot delay or pace packets and no TC program owns those contracts.

**Resource-lease tranche delivered:** a Deployer may create, inspect, renew and
release an app/namespace lease only when the server policy permits isolated
workload provisioning. The server chooses a 128-bit identifier and an isolated
`rbtest-*` namespace, clamps lifetime to the configured one-day hard ceiling,
and limits one control plane to 64 leases with 128 resources each. The owner
passes the lease id on `/v1/apply`; a standalone Bun persists app ownership
before deploying, while a cluster commits app or namespace desired state and
ownership in one Raft entry. Followers forward mutations with the caller's
credential, not the cluster service principal.

Standalone cleanup waits for the agent's stop result. Cluster cleanup removes
the owned desired state through Raft. Each step has a ten-second ceiling, and a
failed, timed-out or interrupted attempt leaves the durable lease in
`cleaning` for the one-second reaper to resume after process death or leadership
change. The `rbtest-*` prefix is now lease-only, so an ordinary apply can't race
ownership. This tranche owns apps and their namespace quota declaration. The
ownership models for jobs, faults, tokens, images, mounts and node state remain
open work.

**Hermetic workload delivered:** container cases use the official BusyBox
1.37.0 OCI index pinned at
`sha256:9532d8c39891ca2ecde4d30d7710e01fb739c87a8b9299685c63704296b16028`.
The index carries `linux/amd64` and `linux/arm64`; the real runc and Apple
Container gates both create, start and exercise their selected manifest. The
Apple proof exposed and fixed an invalid Docker-style `--` in
`container exec`. ProcessGrill stays separate and continues to run the
installed Bun's testapp.

## Optional

### O1. Split the integration seams by ownership

- [ ] Split `src/bun/agent.rs` and `src/bun/api.rs` by bounded context behind
  owned commands/events before adding substantial Phase 15 code.
- [ ] Split council command application and Relish command families where it
  reduces conflict or makes resource ownership explicit.

### O2. Replace repeated protocol/parsing code after compatibility tests

- [ ] Evaluate `hickory-proto` for userspace DNS and fuzz/compatibility-test it
  against the current codec before replacing anything.
- [ ] Evaluate `humantime` or one shared typed duration parser; consolidate
  repeated percentage/size parsing without changing accepted syntax silently.

### O3. Add useful public doctests

- [ ] Add small compiling examples for public configuration and client APIs;
  `cargo test --doc` currently discovers zero tests.

### O4. Revisit a TC DNS fast path only with production evidence

- [ ] If DNS profiles show a material bottleneck, extend the PoC evaluation to
  IPv6, TCP fallback, collision-safe keys, shared map ownership, observability
  and supported-kernel compatibility.
- [ ] Adopt a hybrid fast path only if the complete design remains simpler or
  materially better than userspace-only DNS.

### O5. Reconcile aspirational documentation mechanically

- [ ] Mark design/whitepaper capabilities as shipped, planned, experimental or
  historical, and keep executable command examples in tested includes.

## Delivery order and gates

Prefer one reviewable commit/PR per high-value item. H0 and H1 have no
architectural prerequisite and start first. H2 and H3 may introduce the minimum common live
capability type needed for their own fail-closed decisions; M1 generalises it
after those contracts are proven. H4 follows without waiting for M1. H6 closes
the newly proven rootfs isolation boundary, then H5 publishes a first-run path
against the settled secure defaults and commands. H7 then removes the newly
proven enrolment prerequisite before we call the high-value gate complete.

For every production change:

1. write failing behaviour tests first;
2. implement the smallest correct boundary;
3. update design docs and the relevant book chapter in the same change;
4. run formatting, clippy, portable and no-default tests;
5. run privileged Linux/cluster/runtime gates when the boundary needs them; and
6. record exact acceptance evidence before checking the progress item.

When H0-H7 are green together, rerun the complete review matrix. Only then mark
the high-value gate complete and resume the remaining Phase 15 feature order.
