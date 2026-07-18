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
branch after the review was pushed: 2 high, 5 medium and 5 low. The high alerts
affect `rustls-webpki` (malformed-CRL panic) and `quinn-proto` (unauthenticated
QUIC transport-parameter panic). Reliaburger also uses vulnerable `tar` versions
on extraction paths. Upstream severity doesn't prove every transitive path is
reachable here, but known fixes are available and cheap compared with carrying
the uncertainty.

- [ ] Upgrade `rustls-webpki` to at least 0.103.13, `quinn-proto` to at least
  0.11.14 and `tar` to at least 0.4.46; take compatible patched `rand` releases.
- [ ] Use `cargo tree -i` plus call-path review to record whether each advisory
  is direct, reachable transitive code, or compiled but unused.
- [ ] Document compensating controls for the currently unpatched `thrift`
  excessive-allocation advisory and the `lru::IterMut` soundness advisory; pin
  follow-up owners rather than silently accepting them.
- [ ] Add `cargo-deny` or `cargo-audit` to an owned CI/release gate with explicit,
  expiring exceptions for advisories that have no compatible fix.
- [ ] Re-run portable, no-default, documentation and relevant extraction/TLS
  suites after lockfile changes.

**Acceptance:** every alert with a compatible patched release is gone. Every
remaining alert has a written reachability decision, compensating control,
owner and expiry/recheck date, and CI fails on new unacknowledged advisories.

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
warning when eBPF or the required connect hooks are absent.

- [ ] Write tests first for no eBPF handle, IPv4-only attachment, dual-stack
  attachment, map update failure and enforcement loss after deployment.
- [ ] Represent live IPv4/IPv6 connect-hook state as a typed node capability.
- [ ] Reject placement/deployment of a policy-bearing workload unless the
  selected node reports every hook its policy needs.
- [ ] Keep the agent-side pre-start check as a second fail-closed boundary so a
  stale scheduler capability can't create an enforcement gap.
- [ ] Make later hook loss degrade readiness and expose the affected workloads;
  define whether Bun fences or stops them before implementing recovery.
- [ ] Reconcile the security design's deny-by-default claim with the exact
  capability and failure contract.

**Acceptance:** a declared allowlist is either live on every required address
family or the workload isn't started. There is no warning-only success path.
Privileged Linux tests prove the hooks and map updates, while portable tests
prove capability/placement decisions.

### H3. Make `.internal` DNS reachable, supervised and schedulable

**Finding:** NET-1. Enabling the userspace responder with its default loopback
address writes an unreachable nameserver into runc containers. Bind failure also
happens in a detached task and doesn't fail Bun startup.

- [ ] Write a real runc/netns test that resolves a mapped `.internal` service
  through the address written to the workload's `resolv.conf`.
- [ ] Derive or configure a container-reachable responder address for runc; keep
  host loopback only for runtimes whose network model can reach it.
- [ ] Bind the responder before Bun reports readiness and propagate bind/startup
  errors to the supervisor.
- [ ] Publish DNS readiness and supported address families as live capabilities.
- [ ] Refuse a deployment that requires `.internal` discovery when its selected
  runtime/node can't reach a ready responder.
- [ ] Keep the TC implementation in `poc/dns-tc/` as evidence and a possible
  future fast path. Don't move it into production until it beats the corrected
  userspace design on simplicity or material performance.
- [ ] Reconcile the DNS design, configuration reference and book with the runtime
  address rules and failure behaviour.

**Acceptance:** a default supported runc deployment can resolve `.internal`
names, a bind conflict prevents ready startup, and unsupported runtime/address
combinations fail before workload creation.

### H4. Make generated clusters use mTLS by default

**Finding:** SEC-2. `relish init` creates node identity material but leaves
`require_mtls = false`, so the supported cluster bootstrap path uses plaintext
transports unless an operator discovers and changes the switch.

- [ ] Write init/config tests first which prove generated clustered configs
  require mTLS and deliberately generated development configs don't.
- [ ] Set `require_mtls = true` in normal generated cluster configuration.
- [ ] Replace the quiet false path with an explicit, conspicuous
  development-only plaintext choice and warning.
- [ ] Add a real multi-node acceptance test that verifies Raft, reporting and
  cross-node calls use the configured identities.
- [ ] Update the quick start, configuration reference, security design and book.

**Acceptance:** following the normal initialisation path produces encrypted,
mutually authenticated cluster transports without hand-editing configuration.

### H5. Replace the broken published first-run sequence with an executable one

**Finding:** DOC-1. The whitepaper quick start uses commands and output that
don't match current clap definitions or `relish init` behaviour.

- [ ] Define one canonical standalone first run and one minimal clustered first
  run using the current binary boundaries and ports.
- [ ] Add a documentation smoke test that executes or dry-runs every Reliaburger
  command in those sequences in temporary directories.
- [ ] Generate/check command snippets against clap so flags such as positional
  `apply` paths and required `join --node-id` can't drift silently.
- [ ] Correct the whitepaper, top-level README, `docs/README.md` and relevant book
  walkthroughs together.

**Acceptance:** a new operator can copy the published sequence on a supported
runtime, and CI rejects future command drift.

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
after those contracts are proven. H4 follows without waiting for M1. H5 closes
the gate using the corrected defaults and commands.

For every production change:

1. write failing behaviour tests first;
2. implement the smallest correct boundary;
3. update design docs and the relevant book chapter in the same change;
4. run formatting, clippy, portable and no-default tests;
5. run privileged Linux/cluster/runtime gates when the boundary needs them; and
6. record exact acceptance evidence before checking the progress item.

When H0-H5 are green together, rerun the complete review matrix. Only then mark
the high-value gate complete and resume the remaining Phase 15 feature order.
