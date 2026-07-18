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

- [ ] Write an authenticated API contract and CLI command dedicated to join
  tokens; don't overload API bearer-token commands.
- [ ] Require Admin authorisation, validate a bounded TTL and commit only the
  hash plus expiry to Raft. Print the plaintext exactly once.
- [ ] Prove two separately minted tokens enrol two distinct CSR-bearing nodes,
  while reuse, expiry, a non-Admin principal and a follower/leader transition
  fail safely.
- [ ] Add a three-node executable provisioning walkthrough only after the
  command exists. Until then, keep the documented first run to the truthful
  one-voter cluster and label the one-extra-node limit.

**Acceptance:** an authenticated operator can enrol enough nodes to form a
three-voter council without regenerating PKI or editing Raft state, and the
full sequence is covered by a real cluster test.

## Medium-value

### M1. Expose subsystem readiness and death as live evidence

- [ ] Add `Starting`, `Ready`, `Degraded` and `Stopped` state with last error/time
  for critical long-lived tasks (FUNC-3).
- [ ] Keep `/v1/health` as liveness; add readiness and authenticated capability
  evidence for scheduling and Phase 15.
- [ ] Restart only reconstructible tasks with explicit ownership and deadlines.

### M2. Repair cheap executable checks and platform lint

- [ ] Make `make examples` pass `--dry-run`, preserve useful error output and fix
  the two stale Phase 8 examples (FUNC-2).
- [ ] Gate Linux/Aya modules at module boundaries so the advertised macOS
  all-feature lint contract is coherent.
- [ ] Add dependency-advisory scanning with a pinned policy and CI ownership.

### M3. Make clustered registry defaults peer-reachable

- [ ] Derive a peer-reachable bind in cluster mode or reject an incomplete
  clustered registry configuration (FUNC-1).
- [ ] Include replication/P2P reachability and redundancy in capability and
  `wtf` evidence.

### M4. Carry the configured trust domain into workload identities

- [ ] Pass immutable cluster identity into the agent instead of hard-coding
  `default` (FUNC-4).
- [ ] Add a non-default-cluster SPIFFE issuance and verification test.

### M5. Preserve rootless published ports through Bun replacement

- [ ] Persist rootless proxy parameters and ownership, respawn them during
  adoption, and test a real replacement (FUNC-5).

### M6. Publish real deployment operation state

- [ ] Give deploys stable operation IDs, phases, start times and outcomes; expose
  active state plus bounded history (FUNC-6).
- [ ] Build `wtf` deploy-stuck logic only after this evidence exists.

### M7. Decide and document the v1 ingress/TLS contract

- [ ] Either implement and accept-test automatic ACME with production-safe
  defaults, or mark it deferred and correct the whitepaper/examples now.

### M8. Implement the corrected Phase 15 prerequisites and catalogue

- [ ] Follow §8.9 of the review: contracts/safety, capability/evidence API,
  leases/hermetic workload, ordinary catalogue, then chaos primitives.
- [ ] Implement authenticated real drain/kill and node-scoped pressure before
  C1/C2/C4/C5. Unsupported scenarios must not become green skips.
- [ ] Use explicit `Pass`, `Fail`, `Skipped` and `Unknown`; a full profile fails
  on missing required capabilities, timeout or unknown evidence.
- [ ] Continue with fingerprinted benchmarks, telemetry-backed `wtf`, observed
  source-namespace trace, documentation and real-cluster acceptance.

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
