# Reliaburger current-state review

**Review date:** 17–18 July 2026

**Reviewed branch:** `codex/codebase-review-2026-07-17`

**Reviewed `main` SHA:** `8ba727f9332c35a1f6603a0f78f76a424513485c`

**Scope:** code, tests, examples, the whitepaper, `docs/design/`, and the historical Phase 15 proposal in `docs/plans/2026-07-06-plan-chaos.md`

**Change policy:** review only. The tracked changes are this document and the isolated TC DNS proof under `poc/dns-tc/`; production code and the historical Phase 15 plan remain untouched.

## Executive verdict

Reliaburger is an unusually substantial prototype. It has real implementations behind most of its named subsystems, a large and generally thoughtful test suite, sensible Rust data modelling, and more operational testing than many projects at this age. The portable suite passed 2,628 tests, the no-default suite passed 2,610, the privileged eBPF suite passed 20, the six real-cluster suites passed 20, both real-binary upgrade suites passed 8, and the Apple Container acceptance checks passed 2. Combined line coverage is 81.24%. This isn't a façade around a collection of stubs.

It isn't production-ready yet. Four facts dominate the review:

1. Security defaults don't match the stated design. A standalone Bun can expose an empty-token API on a non-loopback address, inter-node mTLS is opt-in even after `relish init` creates an identity, and declared egress allowlists run unenforced when eBPF isn't available.
2. The default userspace DNS configuration points runc containers at host loopback, which their network namespaces cannot reach. The responder also starts in a detached task, so a bind failure doesn't fail node start or deployment.
3. The whitepaper and detailed design documents frequently describe the target system as if it were the current system. The advertised quick start is not valid against the current CLI. Numerous command names, defaults, ports and implemented capabilities have drifted.
4. Maintainability is being limited by scale inside a single crate: `src/bun/agent.rs` is 8,888 lines and `src/bun/api.rs` is 5,635 lines. Those two files are now coordination points for too many independent concerns.

No new remotely exploitable memory-safety defect was found. There is one production `unsafe` block, and it has a concrete `// SAFETY:` argument (`src/smoker/types.rs:356`). Filesystem extraction, upload identifiers, digests and command arguments generally receive careful validation. The more immediate security risks are fail-open configuration and authentication behaviour rather than memory unsafety.

Phase 15 is separate planned work, not a regression. Its testing/CI/coverage/benchmark foundations are already delivered (`docs/progress.md:1286-1314`), but the July plan's baseline and several proposed behaviours are stale. The corrected plan starts with contracts and safety, then capability/evidence APIs, resource leases and a hermetic OCI workload. Node failure/drain and node-scoped resource pressure are prerequisites, not test cases that may turn into green skips.

The DNS reassessment changes one sentence, not the production recommendation. DNS synthesis is impossible at the *current* `cgroup/sendmsg4` and `cgroup/recvmsg4` socket-address hooks because their `bpf_sock_addr` context exposes addresses and ports, not message bytes. It is entirely feasible in eBPF at a packet hook. A TC-ingress proof of concept loaded and JIT-compiled on Linux 6.8, synthesised a mapped `.internal` A answer, handled 64 concurrent requests, recalculated valid checksums, passed unmatched and malformed traffic upstream, and proved with packet capture that matched queries never reached upstream. TC is therefore a credible future fast path. It is not simpler than the current userspace responder once IPv6, TCP fallback, EDNS, fragments, namespace policy, lifecycle and observability are included, so this review recommends keeping userspace DNS for now and fixing its deployment correctness first.

## Priority backlog

`P0` means stop-ship/data loss or an immediately exploitable critical issue. `P1` should be fixed before calling the system production-capable. `P2` is important correctness or operability work. `P3` is maintainability or documentation debt.

| Priority | Finding | Recommended action |
|---|---|---|
| P1 | GitHub reports 12 open dependency advisories, including high-severity `rustls-webpki` and `quinn-proto` issues | Upgrade the five dependencies with fixed releases immediately; assess actual call-path exposure for unpatched `thrift` and `lru`; add owned advisory scanning so this can't disappear between reviews. |
| P1 | Standalone non-loopback API can be unauthenticated | Apply the empty-token bind guard in every mode; reject unresolved bind hostnames unless explicitly marked safe; make authenticated TLS the normal clustered path. |
| P1 | mTLS is opt-in and `relish init` doesn't enable it | Make generated clusters set `require_mtls = true`; preserve an explicit development-only plaintext mode; add a real multi-node encrypted-transport acceptance test. |
| P1 | Egress allowlists fail open when eBPF is absent | Refuse a deployment that declares egress policy unless the selected node reports both connect hooks live. Never turn a declared policy into a warning. |
| P1 | Default runc DNS is unreachable and bind failure is non-fatal | Bind the responder on a container-reachable address, make responder readiness a startup capability, and refuse deployments that require `.internal` DNS when it is unavailable. |
| P1 | The documented first-run path is not executable | Replace the quick start with commands generated/tested from current clap definitions and add a smoke test for the published sequence. |
| P2 | Cluster registry defaults disable replication and P2P | Derive a peer-reachable bind in cluster mode or require it during validation; keep loopback only for standalone mode. |
| P2 | Cluster subsystem death leaves an unreported degraded node | Surface task state in `/v1/health` and capabilities; either restart reconstructible tasks or make readiness fail. |
| P2 | Workload SPIFFE trust domain is hard-coded to `default` | Carry the configured cluster name into the agent and certificate request path. |
| P2 | Phase 15 depends on missing node-level chaos primitives | Implement authenticated drain/kill and node-scoped pressure with ownership, leases and recovery before C1/C2/C4/C5. |
| P2 | `make examples` is not a dry run; two examples are stale | Add `--dry-run` to the target, declare the missing namespace in `build-job.toml`, and update the health interval type in `proc-exec-app.toml`. |
| P2 | macOS all-feature lint fails | Gate Linux/Aya modules at the module boundary or stop asking macOS CI to build impossible Linux feature combinations. Prefer the former. |
| P2 | Ingress defaults and ACME don't match the design | Decide whether v1 promises automatic TLS. If yes, implement ACME and enable Wrapper by default; otherwise change the whitepaper and examples now. |
| P2 | Rootless port forwarding is lost after adoption | Persist enough proxy configuration to respawn the proxy during adoption and test Bun replacement in rootless mode. |
| P2 | Active-deploy API always returns an empty list | Persist or publish live deployment state before building `wtf`'s deploy-stuck diagnosis. |
| P3 | `agent.rs` and `api.rs` are god modules | Split by bounded context behind owned command/event interfaces; do not add Phase 15 directly to either file. |
| P3 | DNS, duration and percentage parsing are hand-written repeatedly | Evaluate `hickory-proto` for DNS and `humantime`/typed shared parsers for durations; remove duplicate parsers after compatibility tests. |
| P3 | Public docs have no executable Rust examples | Add small doctests for the public configuration and client APIs; `cargo test --doc` currently runs zero tests. |

No P0 finding was established in this review. That statement is deliberately narrower than “there are no P0 bugs”. Local dependency advisory scanning wasn't available because neither `cargo-audit` nor `cargo-deny` is installed. When the review branch was pushed, GitHub reported 12 open Dependabot alerts on the default branch: 2 high, 5 medium and 5 low. The high alerts are a malformed-CRL panic in `rustls-webpki` (fixed in 0.103.13) and unauthenticated QUIC transport-parameter panic in `quinn-proto` (fixed in 0.11.14). The medium set includes three `tar` extraction/header advisories (fixed by 0.4.46) and an unpatched excessive-allocation issue in `thrift`. This post-run evidence moves advisory remediation into the high-value gate; upstream severity is not, by itself, a claim that every path is remotely reachable in Reliaburger.

## Review method and evidence

The review used the checked-out SHA, not historical issue descriptions. Historical comments and progress entries were treated as leads and then checked against current source. Source counts came from `rg --files`, `wc -l`, and Rust-token searches. Command compatibility came from current `relish --help` output, subcommand help, parsing/dry-running all 20 example TOMLs, and normalising every explicit `relish …` example in `docs/whitepaper.md` and `docs/design/*.md` into a command family. Repeated examples are reported once with their count/classification; conceptual pseudocode and third-party administration commands are classified separately rather than executed against the host.

The test matrix and exact outcomes appear in the reproducibility appendix. Platform constraints are kept separate from product failures. The review host was Apple silicon running macOS 26.3.1; privileged Linux work ran in the existing Ubuntu arm64 Lima VM with kernel 6.8.0-124, four CPUs and 8 GiB RAM. The initial parallel Linux link exhausted that VM's memory; a single-job rebuild and all selected tests completed. That is a build-resource limitation, not a product test failure, though the size of the build remains relevant.

## 1. Honest quality review

### 1.1 Maintainability and layout

The top-level vocabulary works. `bun`, `meat`, `mustard`, `onion`, `sesame`, `pickle`, `mayo`, `ketchup`, `wrapper`, `lettuce` and `smoker` map cleanly to the design documents. Most library modules expose concrete domain types, use enums for state machines, and keep platform code behind `cfg` gates. The code usually prefers explicit dependencies and testable pure functions over trait-heavy abstraction. That makes an unfamiliar codebase surprisingly navigable despite its size.

The layout stops scaling at the two main integration seams:

| File | Lines | Problem |
|---|---:|---|
| `src/bun/agent.rs` | 8,888 | Deployment, health, identity, service maps, firewall, egress, faults, adoption, upgrades and reconciliation share one actor and one test module. |
| `src/bun/api.rs` | 5,635 | Router construction, state, handlers, auth wiring and most API tests are colocated. |
| `src/council/state_machine.rs` | 2,870 | Many Raft domains share one persistence boundary; some coupling is inherent, but command/application logic can still be split. |
| `src/bin/bun.rs` | 1,989 | Configuration, subsystem construction, mode policy and long-lived task wiring are all in `main`. |
| `src/relish/commands.rs` | 1,898 | Unrelated CLI operations and their fixtures are converging in one module. |
| `src/bun/build_runner.rs` | 1,853 | Build orchestration, safety limits, process management and API-facing state are tightly grouped. |

The whole repository contains 433 files, about 124,975 lines of Rust under `src/`, 15,254 Rust lines under `tests/`, and 46,625 Markdown lines under `docs/`. Large source files aren't automatically bad, but the agent and API files now make independent work conflict-prone and make it hard to state which subsystem owns cleanup. Phase 15 would make this worse if it added capabilities, test runner state and trace handlers directly to them.

The production code has a single genuine `unsafe` block, used to view a `#[repr(C)]` POD struct as bytes. Its comment states the layout and padding invariants (`src/smoker/types.rs:345-365`). No `std::sync::Mutex` was found in async production paths; its only occurrence is in an upgrade test. External commands normally use `Command::new` plus separate arguments. The deliberate exceptions are process/script workloads and development VM bootstrap, where shell execution is the feature; the process workload path applies an explicit binary allowlist (`src/grill/process_workload.rs:84-143`).

There are, however, a meaningful number of production `unwrap()`/`expect()` calls despite the project rule forbidding them. Examples include CA construction (`src/sesame/init.rs:158-161`), SAN conversion (`src/sesame/ca.rs:238-241`), HTTP client creation (`src/relish/client.rs:181`), header construction throughout `src/pickle/api.rs`, and generated-config serialisation (`src/relish/commands.rs:421`). Many are invariant-backed and unlikely to panic, but the invariants aren't encoded consistently and the policy says they should return errors or carry an explicit infallibility justification. Treat this as P3 hardening, not evidence that the system is generally panic-prone.

### 1.2 Documentation quality

At source level, documentation is above average. The tree contains roughly 10,981 doc-comment lines and public types usually explain their role. Complex security and distributed-system decisions often have a useful reason in the code. The book's Phase 15 foundation is already 303 lines and the test-harness design is concise and accurate about suite taxonomy (`docs/book/15-ready-for-production.md`, `docs/design/test-harness.md`).

At product level, documentation is the least reliable part of the repository. The detailed design documents mix four states without a consistent marker: shipped behaviour, an historical design, an intended v1 feature, and open-ended teaching material. Some documents do include excellent status notes, for example the Onion DNS decision log (`docs/design/discovery-onion.md:52-56`) and the full-council recovery qualification (`docs/design/gossip-mustard.md:811-869`). Others still use future architecture in present tense. The CLI design says every operational capability is in one binary and every TUI view has an equivalent CLI command (`docs/design/cli-relish.md:5-20`), while Cargo builds three binaries and many documented commands don't exist (`Cargo.toml:8-20`).

The whitepaper quick start is currently harmful rather than merely aspirational:

- `relish init` generates PKI and starter files but does not start Bun (`src/relish/commands.rs:351-442`).
- `relish join` requires `--node-id`, omitted by the example (`src/bin/relish.rs:125-145`).
- `relish apply -f myapp.toml` is invalid; apply accepts a positional path (`src/bin/relish.rs:34-48`).
- The example uses a 9443 cluster endpoint and a global `--agent`; the client defaults to local port 9117 and clap has no `--agent` option (`src/relish/client.rs:191-200`, `src/bin/relish.rs:10-30`).
- The displayed `init` output claims an agent and dashboard are running, which the command does not do (`docs/whitepaper.md:340-356`).

This should be fixed before adding more prose. A small documentation-smoke test should execute the canonical quick start with temporary directories and assert clap acceptance at each step.

### 1.3 Test adequacy

The test engineering is strong. There are approximately 2,737 `#[test]` attributes, 196 in-file test modules and 70 reasoned ignores. Portable tests use nextest with retries disabled and no-tests-selected treated as failure. Required wall-clock, privileged Linux, cluster, upgrade and Apple suites are deliberately separated (`Makefile:16-47`, `docs/design/test-harness.md:48-71`). The suite covers real runc/eBPF/Buildah behaviour, real multi-node gossip/Raft behaviour and real binary replacement rather than relying only on mocks.

The combined default/no-default coverage result is 81.24% of lines and 82.57% of regions. That is useful evidence, not a completeness certificate. Important gaps remain:

- `cargo test --doc` passes but discovers zero doctests.
- The main binary is only 14.60% line-covered and the Relish binary 61.51%; process startup, mode combinations and published CLI sequences need black-box coverage.
- The aggregate is dominated by highly covered pure modules. `src/bun/api.rs` is 55.99% by line and `src/bun/agent.rs` 73.82%, precisely where state/lifecycle mistakes are costly.
- Apple checks cover creation/adoption only. They don't prove the 39 Phase 15 behaviours on that runtime.
- Cluster tests passed but printed repeated failure to unmount `/var/lib/reliaburger/volumes/.identity/...` without superuser privilege. A green assertion suite can still leave cleanup debt.
- Dependency advisories weren't scanned in this environment.

The suite also exposes one portability-contract error: `make lint` asks every platform to compile `--all-features`, but Aya is a target-Linux dependency while feature-gated modules are still referenced on macOS. macOS clippy fails with 54 unresolved-Aya/target errors; the same command passes on Linux. This is a product build-definition failure, not a macOS platform limitation, because the Makefile advertises the target as portable (`Makefile:58-59`).

### 1.4 Resource limits and shutdown

The newer code shows good defensive work. Build contexts have byte and entry limits, reject traversal and links, strip setuid bits, have per-stage timeouts, and kill process groups (`src/bun/build_runner.rs:500-566,850-900`). Pickle validates upload IDs before constructing paths (`src/pickle/store.rs:65-75,210-275`), validates Dockerfile paths and canonicalised contexts (`src/pickle/build.rs:313-355`), and revalidates content digests on read (`src/pickle/store.rs:153-180`). Token hashing is moved off the async executor. Long-running test and runtime tasks increasingly use cancellation tokens.

Shutdown is still uneven. `spawn_supervised` logs when a critical cluster task exits, deliberately does not restart it, and leaves the rest of the node serving (`src/cluster/runtime.rs:560-580`). That can be a valid availability choice only if readiness and capabilities expose the degradation; they currently do not. DNS bind failure is even weaker: the responder is spawned and a bind error is only printed (`src/bin/bun.rs:841-860`). Rootless port-proxy state is not rebuilt on adoption (`src/grill/netns.rs:100-110`). Phase 15 needs a general resource lease/reaper rather than assuming namespace teardown covers volumes, images, tokens, faults, mounts and node state.

## 2. Bugs and security findings

### SEC-1 (P1): standalone non-loopback API bypasses the empty-token guard

The API middleware intentionally allows requests without an auth context during the initial empty-token window (`src/sesame/auth.rs:527-530,652-655`). Bun protects a fresh *clustered* node by refusing a non-loopback bind when its council-backed token store is empty (`src/bin/bun.rs:264-285,1174-1186`). In standalone mode `api_token_store` is `None`; router construction silently replaces it with a new empty store (`src/bun/api.rs:252-255`), but the bind guard's `if let Some(store)` is skipped. Starting standalone Bun with `--listen 0.0.0.0:9117` therefore exposes protected apply, exec, fault, token and secret routes without authentication.

There is a second, smaller hole in the guard: an unparseable hostname is accepted without resolving whether it is loopback (`src/bin/bun.rs:271-275`).

**Impact:** remote administrative access on a standalone node that is deliberately bound beyond loopback. Process workloads remain separately allowlisted, but container deploy, stop, exec, fault injection and state disclosure are enough to make this P1.

**Fix:** always construct one explicit auth mode before binding. If no token exists, accept only an IP literal that is loopback or a Unix socket. Do not infer safety from a hostname. Add a black-box standalone startup test for `0.0.0.0`, a routable literal, `localhost`, and a hostname resolving to a non-loopback address.

### SEC-2 (P1): mTLS design and generated defaults disagree

The whitepaper says all communication after join is mTLS (`docs/whitepaper.md:534-545`), and Sesame's first design principle promises mTLS on a fresh cluster with no configuration (`docs/design/security-sesame.md:20-24`). Current `SecuritySection` derives a default `require_mtls = false`, pinned by a unit test (`src/config/node.rs:225-240,1069-1076`). `relish init` writes the first node identity into the generated config but never sets `require_mtls` (`src/relish/commands.rs:391-421`). Bun deliberately ignores the on-disk identity when the flag is false and warns that transports are plaintext (`src/bin/bun.rs:112-117,511-530`).

**Impact:** an operator following the supported initialisation path gets authenticated material but plaintext cluster transports unless they know to edit a non-advertised switch. That violates the security boundary assumed by the design and by several Phase 15 cross-node operations.

**Fix:** generated clustered configs must enable mTLS. Give development plaintext a named, conspicuous mode rather than a false default. Test Raft, reporting and cross-node agent calls with packet inspection or transport assertions, not merely with an identity object present.

### SEC-3 (P1): declared egress policy can run unenforced

Bun explicitly continues when the eBPF object cannot load or the binary lacks the feature (`src/bin/bun.rs:620-659`). For a workload with an egress allowlist, both post-start and pre-start paths log `NOT enforced` and return success when no eBPF handle exists (`src/bun/agent.rs:4063-4084,4203-4248`). Once eBPF is loaded, the code does correctly fail closed on resolver/map errors and a missing IPv6 hook (`src/bun/agent.rs:4097-4108,4249-4255`). The insecure transition is “policy requested, enforcement subsystem absent”.

This contradicts the zero-configuration deny-by-default claim (`docs/design/security-sesame.md:22`) and the documented refusal when both connect hooks aren't available (`docs/design/security-sesame.md:893-902`).

**Impact:** operators can believe a workload is restricted while it has unrestricted network access. This is a policy-bypass vulnerability, even though the warning is honest in logs.

**Fix:** make policy enforcement a scheduler capability. Any app with an allowlist can land only on a node reporting both hooks attached. A race or later eBPF loss should fail readiness and stop/fence affected workloads according to an explicit policy.

### NET-1 (P1): the default DNS responder is unreachable from runc containers

`DnsSection` defaults to `127.0.0.53:53` (`src/config/node.rs:55-85`). Bun extracts that IPv4 address and passes it unchanged to the runtime (`src/bin/bun.rs:333-352`). Runc's own API documentation correctly says host loopback is unreachable inside a container network namespace and instructs callers to use the bridge/gateway address (`src/grill/runc.rs:103-108`). It nevertheless writes the provided address to the rootfs and turns write failure into a warning (`src/grill/runc.rs:279-292`). Separately, Bun starts the responder in a detached task, and bind failure only reaches stderr (`src/bin/bun.rs:841-860`).

`[dns]` is disabled by default, so ordinary public DNS continues to work. The bug appears when an operator enables the feature using its default listen address: `.internal` resolution is then configured but unreachable for runc workloads. The progress checklist says the opposite, including startup/deploy fail-closed behaviour (`docs/progress.md:955-967`).

**Fix:** derive a listener per runtime/network, bind to the runc bridge address, and configure Apple/ProcessGrill explicitly. Bind UDP and TCP before reporting the capability. Propagate readiness into deployment admission. Never mutate a shared image rootfs's `resolv.conf`; mount a per-instance file.

### AUTH/DATA positive findings

No path traversal was found in the reviewed image/build paths. The current code rejects traversal Dockerfile paths and tar entries (`src/pickle/build.rs:313-355,840-870`), validates upload IDs (`src/pickle/store.rs:65-75`), restricts replication redirects to the same origin (`src/pickle/replication.rs:509-550`), and checks digests when blobs are read. Master secrets are written mode 0600 on Unix (`src/relish/commands.rs:375-383`), and node identity storage has dedicated permissions. Git commands pass untrusted branch/URL/path values as arguments and use `--` where appropriate (`src/lettuce/git.rs:60-70`). These controls should be retained.

### FUNC-1 (P2): default cluster registry disables its cluster features

`registry_bind` defaults to `127.0.0.1` (`src/config/node.rs:520-533,596-605`). Peer replication and P2P pulls address the node by its gossip IP, so Bun warns that they will not work in cluster mode (`src/bin/bun.rs:1490-1500`). The real cluster upgrade suite produced this warning on every node. The registry does support authenticated writes and TLS when cluster auth/identity is present (`src/bin/bun.rs:1407-1447`); the defect is the mode-insensitive default.

**Fix:** require a peer-reachable bind when council mode and redundancy/P2P are enabled. Keep loopback for an explicit standalone registry. Include registry reachability and redundancy in capabilities and `wtf` evidence.

### FUNC-2 (P2): `make examples` doesn't do what its description says

The target says “Dry-run every example config” but invokes `relish apply "$f"` without `--dry-run` (`Makefile:61-74`). With no agent, all 20 examples fail after planning with `bun agent not reachable — nothing was deployed`. Running the intended command manually yields 18 valid dry runs and two genuine example failures:

```text
examples/phase-8/build-job.toml
  namespace in "build \"my-api\"": references unknown namespace "production"

examples/phase-8/proc-exec-app.toml
  line 17: interval = "10s": invalid type: string "10s", expected u64
```

**Fix:** add `--dry-run`, show parse errors rather than suppressing all output, and correct those two files. Keep this target in portable CI; it is cheap and protects the book/design examples.

### FUNC-3 (P2): critical cluster task failure isn't machine-visible

The runtime wrapper logs an early task exit and intentionally neither restarts it nor changes node state (`src/cluster/runtime.rs:560-580`). A node can therefore return the normal fast health response while gossip/reporting/reconciliation is dead. Phase 15's capability API and `wtf` cannot diagnose what the process doesn't expose.

**Fix:** maintain per-subsystem `Starting/Ready/Degraded/Stopped` state with last error/time. `/v1/health` should remain liveness; add readiness and include evidence in capabilities. Restart only tasks whose sockets/channels can be recreated safely.

### FUNC-4 (P2): workload identities use the wrong trust domain outside the default cluster

The agent hard-codes `cluster_name = "default"` when constructing workload SPIFFE URIs (`src/bun/agent.rs:5378-5391`). A cluster initialised with another name therefore issues workload identities under the wrong trust domain even though node PKI uses the configured name.

**Fix:** pass an immutable cluster identity into the agent constructor. Add a non-default-cluster certificate acceptance test.

### FUNC-5 (P2): rootless forwarding disappears after Bun replacement

Root-mode port mappings are kernel state and can be rediscovered, but rootless mappings are userspace proxy tasks. Adoption rebuilds only a handle and has a Phase 15 TODO to respawn the proxy (`src/grill/netns.rs:100-110`). Existing workloads can remain “running” after self-upgrade while their published ports are dead.

**Fix:** persist the host/container port tuple and proxy ownership in the adoption record, respawn before marking adoption healthy, and include a real rootless upgrade test.

### FUNC-6 (P2): active deploy state is a placeholder

`GET /v1/deploys/active` always returns `{"active_deploys":[]}` because deployments run synchronously in the agent (`src/bun/api.rs:3341-3350`). That is a missing API contract, not just a UI omission; the historical Phase 15 `deploy-stuck` diagnosis depends on it.

**Fix:** give deploy orchestration an explicit operation ID, start time, phase and terminal outcome, publish it while active, and keep a bounded durable history.

### Other confirmed limitations

- `FaultType::NodeDrain` and `NodeKill` are parsed but deliberately rejected as unimplemented cluster operations (`src/bun/agent.rs:3000-3012`). Treat this as a Phase 15 prerequisite.
- Pickle accepts and ignores OCI cross-repository mount parameters (`src/pickle/api.rs:426-437`). This is protocol incompleteness; clients fall back to upload, so it is not data corruption.
- GPU placement exists, but assigned `/dev/nvidia*` devices are not placed into the OCI spec (`docs/design/agent-bun.md:963`). A scheduled GPU app may not receive the device.
- Upstream image trust verification remains deferred (`src/meat/scheduler.rs:267`); the current policy covers Pickle-hosted images only (`src/config/node.rs:573-591`).
- Ingress rejects `auto`/`acme`; ACME is not implemented (`src/wrapper/types.rs:28-48`, `src/wrapper/routing.rs:882-885`). This contradicts the whitepaper's default automatic TLS promise (`docs/whitepaper.md:141,490`).
- The chaos council-partition command still sends the fault through the provided client rather than the selected target node (`src/relish/chaos.rs:109-114`).
- Jobs are explicitly not cluster-scheduled yet (`src/bun/api.rs:1155-1161`). This should be called out wherever the batch/job design implies transparent cluster placement.

## 3. Missing and half-finished features

This section distinguishes unfinished product work from defects in something that claims to work. Phase 15 items are assessed separately below.

| Area | Current state | Classification and evidence |
|---|---|---|
| Automatic ingress TLS | `cluster` certificates and static/self-signed serving exist; `auto`/`acme` are rejected | Missing v1 feature if the whitepaper remains normative. `src/wrapper/types.rs:28-48`; `docs/whitepaper.md:141,490`. |
| Wrapper default | `[ingress] enabled` defaults false | Design divergence. The ingress design says Wrapper runs on every node by default (`docs/design/ingress-wrapper.md:16-18`); code says explicit opt-in (`src/config/node.rs:167-199`). |
| Onion DNS deployment | Robust userspace UDP/TCP responder exists; runtime wiring/default is broken for runc | Partial/defective, covered by NET-1. Core resolver tests are strong (`src/onion/dns.rs`). |
| Onion IPv6 and unconnected UDP VIPs | TCP/connected-socket IPv4 path is prioritised; IPv6/unconnected UDP deferred | Explicitly deferred, not a regression (`docs/design/discovery-onion.md:1056-1068`). |
| eBPF DNS object | Compiled but never loaded; both hooks return pass | Dead historical implementation. `build.rs:20`; `ebpf/onion_dns.bpf.c:141-191`; loader only requires connect hooks (`src/onion/ebpf/loader.rs:29-100`). |
| Cluster job scheduling | Job API executes locally; high-throughput batch allocation exists | Partial. `src/bun/api.rs:1155-1161`, `src/bun/batch.rs`. |
| Node drain and kill | CLI/fault model exists; agent rejects both | Unimplemented prerequisite. `src/bun/agent.rs:3000-3012`. |
| Node-scoped pressure | Smoker CPU/memory/disk faults target workload cgroups | Missing prerequisite for the proposed node-exhaustion chaos scenario. `src/smoker/resource.rs`. |
| GPU runtime isolation | Discovery and placement work; device passthrough does not | Half-finished. `docs/design/agent-bun.md:963`, `src/bun/gpu.rs`, `src/meat/scheduler.rs`. |
| Registry cross-repository mount | Parameters accepted and ignored | Protocol feature missing. `src/pickle/api.rs:426-437`. |
| Registry reachability/redundancy | Replication/P2P code exists; default cluster bind prevents it | Functional default defect, FUNC-1. |
| GitOps wrong-branch handling | Authenticated webhook exists; configured-branch short-circuit and `Retry-After` absent | Known partial implementation. `docs/design/gitops-lettuce.md:551-561`. |
| Workload trust | Pickle signing enforced when configured; upstream images exempt | Explicit gap. `src/config/node.rs:573-591`, `src/meat/scheduler.rs:267`. |
| Process workload isolation | Allowlist, user, resource and mount controls exist | Substantial implementation. Keep ProcessGrill test cases separate because it is intentionally not an OCI runtime. |
| Full-council recovery | Operator-triggered sealed backup/local snapshot restore works; automatic pre-seeded recovery is proposed | Honest partial evolution, correctly marked in `docs/design/gossip-mustard.md:811-869`. |
| Federation/franchises | Described in the whitepaper and CLI design; no current command or production subsystem | Unimplemented future architecture. Do not present it as v1 current state. |
| CLI parity | Core operational commands exist; many design commands do not | Broad design divergence; see command appendix. |

Several historical “missing” notes have now been delivered and should not be repeated in future plans: WebSocket proxying exists (`src/wrapper/websocket.rs`); the test-harness taxonomy, deterministic async cleanup, required CI, Criterion foundations and combined coverage are complete (`docs/progress.md:1291-1305`); real Apple adoption is covered manually; and full-council operator recovery is implemented. Revalidating these avoids planning the same work twice.

## 4. Architectural review

### 4.1 Split ownership, not the binary

The single-distributable goal doesn't require a single Rust crate or two enormous modules. Keep one release artefact if desired, but introduce internal crates or strict modules with owned state and message contracts:

```text
bun process
  startup/mode policy
    ├── agent core (desired/actual reconciliation)
    ├── workload lifecycle (runtime, health, adoption)
    ├── network policy (service map, firewall, egress, DNS capability)
    ├── identity/secrets
    ├── deployment operations
    ├── diagnostics/capabilities
    └── API adapters
```

The important change is ownership. Each subsystem should own its resource ledger and expose commands/events, not share a wider mutable `Agent` merely because everything runs in one process. API handlers should translate HTTP to domain requests and not contain domain state machines. `src/bin/bun.rs` should construct a validated `NodeMode` (`Standalone`, `BootstrapCluster`, `JoiningCluster`, perhaps `Development`) so auth, TLS, registry and bind defaults can't drift independently.

For the Raft state machine, keep one consensus log but split command application by domain. A top-level exhaustive enum can delegate to `apps`, `security`, `images`, `deployments`, `upgrades` and `backups`, with deterministic state collected in one serialisable root. This preserves ordering while reducing the 2,870-line implementation surface.

### 4.2 Make capabilities live evidence

Configuration and compile features aren't capabilities. A useful capability record must answer:

- what was requested;
- what this particular node/runtime/kernel supports;
- whether it is live now;
- why it is unavailable/degraded;
- when and how it was observed.

Use states such as `Available`, `Disabled`, `Unsupported`, `Unavailable` and `Degraded`, with per-node evidence. A single `ebpf: true` is insufficient when connect4 is attached, connect6 failed, TC isn't present, or maps can't be updated. This same API should drive scheduling admission, Phase 15 skips, readiness, `wtf` and trace. It eliminates several parallel sources of “truth”.

### 4.3 Model operations and resources explicitly

Deployment, build, batch, fault and upgrade work all benefit from a shared operation envelope: ID, owner/principal, scope, created/start/deadline timestamps, phase, resources, terminal outcome and cleanup status. Do not force all implementations into one trait; share the wire contract and lease semantics first.

For destructive tests and chaos, add a runner-owned resource lease. Every created namespace, app, volume, snapshot, image tag, token, fault, partition and temporary node state is registered before or immediately after creation. Leases have a TTL and a server-side reaper. Cleanup is idempotent and restricted to resources carrying the run ID. A timed-out task, panic or killed Relish process must not be the only owner of cleanup logic.

### 4.4 Libraries worth evaluating

“Use a crate” is not automatically simpler. These substitutions have a concrete payoff and should be prototyped behind compatibility tests:

| Current code | Candidate | Why/conditions |
|---|---|---|
| 1,043-line custom DNS codec/responder (`src/onion/dns.rs`) | `hickory-proto` (and possibly its server primitives) | Correct handling of compression, EDNS, truncation, TCP framing and record types is protocol-heavy. Preserve current no-leak ACL/fault semantics and benchmark allocations before replacing. The TC fast path still needs a deliberately bounded kernel parser. |
| Multiple duration parsers (`src/relish/fault.rs:16`, `src/smoker/scenario.rs:34`, `src/meat/autoscaler.rs:330`, `src/meat/deploy_types.rs:434`) | `humantime` or one typed internal parser | Current grammars can drift. Define one accepted syntax and serde/clap adapters, then delete duplicates. |
| Multiple percentage parsers with different units | Typed `Percentage`/`Ratio` newtypes, possibly using a small parsing crate | `50`, `50%` and `0.5` currently mean different things in different modules. The type should carry the unit semantics. |
| Hand-built API route concentration | Axum nested routers plus per-domain state | Axum is already present; use it more fully. This is a restructuring, not a new dependency. |
| Ad hoc task logging | Existing tracing ecosystem (`tracing`, already transitive) with structured subsystem health | Structured spans/errors make `wtf` evidence and shutdown diagnosis possible. Avoid adding a second logging facade. |

Do **not** replace the following merely to reduce line count:

- OpenRaft integration and the deterministic state machine are core product logic.
- Pickle's server-side distribution/auth/quota behaviour isn't replaced by the client-focused `oci-distribution` crate, which the project already uses for upstream access.
- Smoker fault semantics and safety rails are product behaviour, not generic command wrappers.
- The custom scheduler is small enough, well tested, and central to what the book teaches.

### 4.5 Dependency and build shape

The default crate pulls DataFusion/Arrow, Kubernetes API types, both object-store generations (DataFusion uses 0.11 while the project uses 0.12), and several duplicated transitive versions. `cargo tree -d` shows duplicate `object_store`, `rand`, `thiserror`, `base64`, `dashmap`, `itertools` and related stacks. Some duplication is unavoidable, but the single default feature set makes every developer pay for every subsystem. On this review host the worktree's build/coverage artefacts reached roughly 8.9 GiB, and macOS linked a 335 MiB debug Bun plus a 293 MiB debug Relish with an `__eh_frame section too large` warning.

Feature-gate the Kubernetes migration adapter and heavyweight analytical query engine more deliberately, or split them into internal crates while keeping them in release builds. Align direct dependency versions where upstream compatibility permits. Add `cargo-deny` or `cargo-audit` in CI so licence/advisory/duplicate policy is explicit. The goal isn't the smallest binary at all costs; it is a predictable edit-build-test loop and a reviewable supply chain.

## 5. Design-divergence matrix

| Design claim | Current implementation | Verdict |
|---|---|---|
| “Single binary” (`docs/whitepaper.md:90`; CLI design `:20`) | Cargo defines `bun`, `relish` and `testapp` binaries (`Cargo.toml:8-20`) | Diverged. This may be a sensible packaging decision, but docs must stop using it as a guarantee. |
| Three commands from bare metal to TLS app (`docs/whitepaper.md:340-356`) | Init doesn't start Bun; join lacks required node ID; apply syntax invalid; ACME absent | Materially false. Replace with tested current instructions. |
| Fresh cluster has mTLS with no configuration (`docs/design/security-sesame.md:22`) | `require_mtls` defaults false and init leaves it false | Security divergence, SEC-2. |
| Egress is deny-by-default (`docs/design/security-sesame.md:22,893-902`) | Declared policy is unenforced when eBPF isn't loaded | Security divergence, SEC-3. |
| Wrapper runs on every node by default (`docs/design/ingress-wrapper.md:16-18`) | Ingress defaults disabled (`src/config/node.rs:167-199`) | Diverged. |
| TLS defaults to automatic ACME/cluster mode (`docs/whitepaper.md:141`; ingress design `:117-129`) | `auto` and `acme` are rejected | Missing feature presented as current. |
| `.internal` names never leak and containers use the responder (`docs/design/discovery-onion.md:14`) | Resolver itself is fail-closed for matched names, but default runc address is unreachable | Partial; core behaviour good, deployment wiring defective. |
| Onion never touches TC/XDP (`docs/design/discovery-onion.md:983-990`) | True today; TC DNS is now proven feasible but remains only a review PoC | Current state accurate, architectural choice should be described as a choice rather than impossibility. |
| Registry redundancy/P2P built in | Implementation exists; cluster default binds loopback and disables it | Partial/default divergence, FUNC-1. |
| Read commands can target any node/API port 9443 (`docs/design/cli-relish.md:28-35`) | CLI defaults to local `127.0.0.1:9117`; no endpoint-selection global option | Diverged operational model. |
| Every TUI capability has a CLI equivalent (`docs/design/cli-relish.md:12`) | Events/alerts/firewall/metrics etc. have APIs/TUI pieces but no equivalent top-level commands | Diverged. |
| Relish and Bun cannot version-skew because they are one binary (`docs/design/cli-relish.md:20`) | Separate binaries; `/v1/version` exists for upgrade checks but no general client compatibility handshake | Diverged; add protocol negotiation or change the claim. |
| Process apps/jobs receive the same scheduling as containers (`docs/whitepaper.md:726`) | Process runtime works; jobs are still node-local (`src/bun/api.rs:1155-1161`) | Partial. |
| GPU whole-device allocation is supported (`docs/whitepaper.md:992`) | Placement counts devices but OCI passthrough is missing | Half-finished. |
| `docs/progress.md` completed items are current truth | Most revalidated fixes exist, but DNS fail-closed/container reachability and rootless adoption wording overstate reality | Checklist needs evidence links and occasional re-audit; don't edit it in this review-only change. |

## 6. Command compatibility appendix

### 6.1 Method

The whitepaper and design documents contain roughly 170 explicit `relish` invocations across about 60 top-level names, plus repeated `cargo`, `make`, `curl`, `dig`, `git`, `nft`, `tc`, `bpftool`, `systemctl`, `journalctl`, `runc`, `buildah`, `kubectl` and other platform examples. The table below normalises repeated Relish invocations by top-level family. Classification means:

- **working:** the current parser exposes the command and the example's main syntax matches;
- **partial:** command exists, but a documented option/behaviour is absent or the backend is incomplete;
- **stale:** current equivalent exists under a different name or syntax;
- **platform:** valid external/operator command requiring the named platform/tool;
- **conceptual:** illustrative output/pseudocode, not a runnable current product command;
- **unimplemented:** no current equivalent;
- **Phase 15:** explicitly planned, not a regression.

Current parser evidence came from `target/debug/relish --help` and group help. The actual top-level commands at the reviewed SHA are: `tui`, `apply`, `status`, `logs`, `logs-export`, `logs-search`, `top`, `exec`, `inspect`, `stop`, `init`, `nodes`, `council`, `join`, `resolve`, `routes`, `chaos`, `fault`, `snapshot`, `deploy`, `history`, `rollback`, `lint`, `compile`, `diff`, `fmt`, `import`, `export`, `images`, `build`, `batch`, `batch-status`, `secret`, `token`, `sign`, `dev`, and `upgrade` (`src/bin/relish.rs`).

### 6.2 Relish command families

| Documented family | Class | Current compatibility |
|---|---|---|
| `apply` | Partial/stale examples | Exists with positional path and `--dry-run`; `apply -f` is invalid. Live apply needs the local agent because there is no global remote endpoint flag. |
| `status` | Working | Exists; remote/topology promises are broader than the default client configuration. |
| `logs`, `logs-export` | Working | Exist. `logs-search` also exists but is under-documented. |
| `top` | Working | Exists. |
| `exec` | Partial | Exists; detailed debug-container/host targeting claims must be checked per option rather than assumed from the design. |
| `inspect` | Working/partial | Exists for current targets; not every design field is surfaced. |
| `init` | Partial | Generates config, PKI and token. Does not start Bun/dashboard. |
| `join` | Partial/stale examples | Exists; `--node-id` is required and API examples should use port 9117. |
| `nodes` | Working | Exists. The whitepaper's global `--agent` example is stale. |
| `council` | Working | Status exists; recovery is `council recover`. |
| `recover` | Stale | Use `relish council recover`; one gossip-design passage still says `relish recover`. |
| `resolve` | Working | Exists and reads the userspace service map; design text calling it a kernel-map query is stale when eBPF is off. |
| `route` | Stale | Current command is plural `routes`; documented hostname-detail form is absent. |
| `chaos` | Partial | Fixed scenario actions exist; per-node targeting is incomplete. |
| `fault` | Partial/stale | Delay/drop/DNS/partition/bandwidth/CPU/memory/disk/kill/pause/list/clear exist. `fault run` is now `fault scenario`; node drain/kill parse but reject. |
| `volume` | Stale | Current command is `snapshot create/list/restore/delete`; whitepaper and CLI design use `volume snapshot(s)/restore`. |
| `deploy`, `history`, `rollback` | Working/partial | Exist. Active operation state is missing. |
| `lint`, `compile`, `diff`, `fmt` | Working | Exist and are locally testable. |
| `import`, `export` | Working | Kubernetes feature must be compiled; examples are platform/conversion specific. |
| `images`, `build`, `sign` | Working/partial | Exist; cluster registry default/reachability and trust gaps apply. Design `pickle` administrative family does not exist. |
| `batch`, `batch-status` | Working/partial | Exist; jobs are not fully cluster-scheduled. |
| `secret` | Working | `pubkey`, `encrypt`, and `rotate` exist. One security-design status note saying rotate is deferred is stale (`docs/design/security-sesame.md:825`). |
| `token` | Partial/stale | `create`, `list`, `revoke` exist; documented `token rotate` does not. |
| `dev` | Working/platform | `create/status/shell/stop/start/destroy/test/disk/clean/keygen/sign-binary`; requires Lima. |
| `upgrade` | Working | `check/start/plan/status/rollback/resume` exist and real binary suites pass. |
| `tui` | Working | Explicit `tui` and bare no-command path exist. `serve-tui` is only an open design question, not a feature. |
| `test`, `bench`, `wtf`, `trace` | Phase 15 | Not implemented at the reviewed SHA. Existing `make bench*` is a developer Criterion suite, not `relish bench`. |
| `scale`, `plan`, `drain` | Unimplemented CLI | Scale is achieved by applying changed desired replicas/autoscaler state; apply prints a plan but there is no standalone `plan`; drain needs a real cluster primitive. |
| `events`, `alerts`, `metrics`, `firewall` | API/TUI only | Backing data/routes exist to varying degrees; no named top-level CLI command. |
| `ca`, `cert`, `identity` | Unimplemented CLI | Security backend pieces exist; the documented operator UX does not. |
| `ingress`, `gitops`, `namespace`, `pickle` | Unimplemented CLI family | Configuration/API pieces exist; no top-level administrative family. |
| `config`, `context`, `login`, `completions` | Unimplemented local UX | The client defaults to local state/env flags rather than the designed context/keychain model. |
| `franchise` | Unimplemented/future | Federation architecture only. |

### 6.3 Whitepaper sequences and repository examples

| Sequence | Result at reviewed SHA |
|---|---|
| Whitepaper `init → join → apply -f` (`docs/whitepaper.md:340-356`) | **Stale/non-working.** Init only writes files; join omits `--node-id`; apply has no `-f`; automatic public TLS is absent. |
| Whitepaper dev cluster (`docs/whitepaper.md:366-385`) | `dev create/destroy` and `chaos council-partition` parse; global `--agent` doesn't. The real cluster suites prove the cluster machinery, not this exact published sequence. |
| All 20 `examples/**/*.toml` through Makefile | **Fail 20/20** because `make examples` omits `--dry-run` and suppresses the useful agent error. |
| All 20 through the intended `relish apply --dry-run` | **18 pass, 2 fail.** `phase-8/build-job.toml` lacks namespace `production`; `phase-8/proc-exec-app.toml` uses a string health interval where the schema expects `u64`. |
| Phase 15 acceptance commands in the July plan | **Not runnable yet by definition.** Retain as future acceptance after the corrected prerequisites. |

### 6.4 Non-Relish examples

The `cargo` and `make` commands in `docs/design/test-harness.md` match the Makefile, with two qualifications: macOS `make lint` currently fails as described above, and `make examples` is defective. `nft`, `tc`, `bpftool`, `runc`, `buildah`, `systemctl`, `journalctl`, `dig`, `curl`, `openssl`, `git`, `kubectl` and Lima commands are **platform-specific** operator/development examples. They weren't run indiscriminately on the review host. Relevant Linux commands were exercised inside the isolated VM by the privileged suites and DNS PoC. Code blocks containing BPF C, Rust, TOML, packet flows or example command output are **conceptual/teaching material**, not shell acceptance cases. Documents should label them accordingly.

## 7. DNS decision record: userspace versus eBPF

### 7.1 Question and precise answer

The old decision needs one qualifier.

**Can the current `cgroup/sendmsg4` and `cgroup/recvmsg4` programs parse and synthesise a DNS packet? No.** They are `BPF_PROG_TYPE_CGROUP_SOCK_ADDR` programs with a `struct bpf_sock_addr` context. That context contains socket family, protocol, source/destination addresses and ports, but no payload pointer or mutable receive buffer. `bpf_msg_pull_data()` and `bpf_msg_push_data()` belong to `BPF_PROG_TYPE_SK_MSG`, whose context is `struct sk_msg_buff`; they don't turn a cgroup socket-address hook into a message hook. The comment in `ebpf/onion_dns.bpf.c:148-160` suggesting newer kernels make `bpf_msg_pull_data()` usable there is therefore wrong. The in-tree programs confirm the limitation by returning pass at both hooks (`ebpf/onion_dns.bpf.c:141-191`).

Primary kernel references:

- [Linux cgroup socket-address program context](https://docs.ebpf.io/linux/program-type/BPF_PROG_TYPE_CGROUP_SOCK_ADDR/)
- [Linux sockmap/SK_MSG documentation](https://docs.kernel.org/bpf/map_sockmap.html)

**Can eBPF implement DNS synthesis elsewhere? Yes.** TC and XDP receive packet bytes. At TC, `bpf_skb_change_tail()` can grow a control reply, packet stores can rewrite Ethernet/IP/UDP/DNS fields, checksum helpers can repair the headers, and ingress `bpf_redirect_peer()` can send the reply into a veth peer's network namespace. These helpers have existed long enough for the project's stated Linux floor except `bpf_redirect_peer`, which requires Linux 5.10. The current Onion loader claims only 5.7+ (`src/onion/ebpf/loader.rs:199-241`), so a production TC implementation either raises the floor to 5.10 or uses a different redirect path.

Primary references:

- [`bpf_skb_change_tail`](https://docs.ebpf.io/linux/helper-function/bpf_skb_change_tail/)
- [`bpf_l4_csum_replace`](https://docs.ebpf.io/linux/helper-function/bpf_l4_csum_replace/)
- [`bpf_redirect_peer`](https://docs.ebpf.io/linux/helper-function/bpf_redirect_peer/)
- [Linux XDP redirect documentation](https://docs.kernel.org/bpf/redirect.html)

SK_MSG/sockmap is another eBPF family that can see messages, and modern kernels contain UDP sockmap support. It doesn't rescue the current design: sockets must be enrolled in a sockmap and a message-verdict programme is a different attach/ownership model. It is operationally closer to inserting an L7 socket framework than to the existing transparent cgroup hooks. TC is the simpler proof for Reliaburger's veth-based runc networking.

### 7.2 TC-ingress proof of concept

The review built a disposable PoC and checked its source, isolated runner and dated verification transcript into `poc/dns-tc/`. Generated BPF objects and packet captures remain temporary artefacts rather than source. The topology was:

```text
client network namespace
  10.203.N.2/24, veth peer
          │ DNS query to 10.203.N.53:53
          ▼
host-side veth TC ingress
  bounded Ethernet/IPv4/UDP/DNS parser
  service_v4 hash map: redis.internal → 127.128.1.10
      ├── match: grow/rewrite packet as a DNS response, checksum, redirect_peer
      └── miss/malformed/external: TC_ACT_OK across the isolated bridge
          to the upstream network namespace and userspace DNS server
```

Environment and loader evidence:

```text
Linux 6.8.0-124-generic aarch64
clang 18
BPF program: id 479, name dns_tc_ingress, type sched_cls, JITed
BPF map: id 660, service_v4, type hash, key 4, value 4, max_entries 1024
TC hook: clsact ingress on the host side of the temporary veth
```

The parser was deliberately bounded for verifier safety. It accepted one-question, unfragmented IPv4/UDP A queries, lower-cased and packed at most the supported `.internal` QNAME, looked up a four-byte VIP, resized the skb, copied the original question, appended a compressed A answer, swapped MAC/IP/UDP endpoints, recalculated IP and UDP checksums, and redirected to the container peer. Misses returned `TC_ACT_OK` unchanged.

### 7.3 Results and packet evidence

| Case | Observed result |
|---|---|
| `redis.internal A` mapped | One authoritative answer, 48-byte response, `127.128.1.10`. |
| Missing `.internal` | Passed to upstream and returned upstream NXDOMAIN. |
| `example.com` | Passed to upstream unchanged and returned the test upstream's NXDOMAIN. |
| EDNS query | Synthesised the mapped A answer; intentionally stripped the OPT record and returned `ARCOUNT=0`. |
| Malformed three-byte UDP body | Passed upstream; upstream-side tcpdump reported `domain [length 3 < 12] (invalid)`. |
| Checksum | Client-side capture reported the synthesised response `[udp sum ok]`. The outbound query appeared checksum-bad before offload, which is normal capture behaviour on the transmitting veth. |
| Concurrency | 64 concurrent mapped queries: 64 answers, 0 timeouts, one consistent address. |
| Upstream leakage | Upstream capture/server saw only the missing internal name and `example.com`; it saw no mapped `redis.internal` query. |
| Detach/attach | After filter detach, `redis.internal` reached upstream and returned NXDOMAIN. Reattach restored the mapped answer. Final cleanup removed the filter, programme, maps, namespaces, veths and bridge. |

Representative client capture:

```text
10.203.N.53.53 > 10.203.N.2.<ephemeral>: [udp sum ok]
    redis.internal. A 127.128.1.10
```

Representative upstream evidence:

```text
queries observed upstream: missing.internal, example.com
queries not observed upstream: redis.internal (mapped normal, EDNS, 64-way run)
```

This establishes feasibility, payload correctness for the bounded case, checksum correctness, non-match pass-through, concurrency and lifecycle. It does **not** establish a production-ready resolver.

The reproducible runner and source live in `poc/dns-tc/`. The checked-in transcript at `poc/dns-tc/evidence/verified-2026-07-18.txt` records the exact command, environment, results, decoded packet excerpts, capture hashes and post-run cleanup checks.

### 7.4 Options

| Property | Userspace responder (current) | TC internal fast path + fallback | XDP responder |
|---|---|---|---|
| Hook/context | UDP/TCP sockets in Bun | skb on container-veth ingress | raw frame at driver/generic XDP ingress |
| Kernel floor | No BPF requirement | TC BPF; `redirect_peer` Linux 5.10+ for the demonstrated path | XDP support varies by device/driver; generic mode slower |
| Protocol coverage | IPv4/IPv6 types supported by codec; UDP and TCP listener; explicit forwarding/ACL/fault logic | PoC: IPv4, UDP, one A question, no fragments/VLAN; EDNS stripped; TCP/IPv6 need separate implementation | Same parser burden, plus harder return routing/headroom/driver differences |
| Service map | Userspace `ServiceMap` snapshot | New pinned DNS-name→answer map, kept atomic with service state | Same map/ownership problem |
| Namespace awareness | Current default namespace; source ACL, but no reliable querying-cgroup identity after forwarding | Interface/netns attachment can supply workload/node context; per-veth policy is possible | Interface context possible, less natural for veth return on this architecture |
| Failure mode | Process/bind/task failure | Loader, verifier, attach, map and per-packet failure; fallback can preserve external DNS | Driver/attach/redirect failure; harder portable fallback |
| Observability | Normal tracing, metrics, packet capture | BPF counters/ring buffer plus userspace map/loader telemetry | Same, with more driver-specific diagnosis |
| Complexity | One mature Rust responder, but deployment wiring needs repair | Two resolution paths and parity tests; bounded kernel implementation must be deliberately narrower | Highest operational complexity for negligible DNS-rate benefit |
| Likely performance | A local userspace hop per query; semaphore bounded at 64 | Mapped internal query answered without userspace/upstream context switch | Fastest theoretical path, but DNS isn't the steady-state application data path |

Map ownership is the key architectural issue. The current service ID/VIP map is keyed for connect-time routing, not by canonical DNS question. A TC path needs a pinned, versioned DNS answer map, updated atomically with userspace state and cleaned across Bun restart. The loader must discover veths, attach/detach filters for workload lifecycle, expose programme/link/map health, and coexist with rootless runc and Apple Container, neither of which presents the same host-veth model. TCP DNS needs either a userspace fallback or stream-aware implementation. IPv6 needs a separate parser/checksum/answer path. Fragmented IP should pass or drop by explicit policy. EDNS response-size, DNSSEC flags and more record types require clear semantics.

### 7.5 Decision

Keep userspace DNS as the production design for now.

The PoC disproves the broad claim “eBPF cannot synthesise DNS”; it confirms the narrower claim “the chosen cgroup socket-address hooks cannot synthesise DNS”. TC is feasible and a reasonable future optimisation or isolation mechanism. It doesn't currently produce a simpler system. Reliaburger would have to maintain the mature userspace path for Apple, rootless, TCP, IPv6 and fallbacks anyway, while adding a second parser, map, loader lifecycle and observability surface.

Fix NET-1 first and measure real internal DNS load/latency. Revisit a TC fast path only if measurements show material benefit or per-workload namespace resolution requires interface-local policy that userspace cannot supply cleanly. If revisited, ship TC only for a narrow, explicitly advertised cacheable A/AAAA fast path, pass every unsupported query unchanged to the userspace responder, and use one conformance corpus against both implementations. XDP offers no compelling advantage for this veth-local control exchange.

Also remove or quarantine the dead `onion_dns.bpf.c` build product. Preserving historical code for the book is fine; compiling an object the loader never uses creates false confidence and wastes build time. The design document should keep the teaching listing under a clearly historical appendix and correct the helper claim.

## 8. Phase 15 plan assessment

### 8.1 Status and framing

`docs/plans/2026-07-06-plan-chaos.md` is a useful historical proposal, not an executable plan for the reviewed SHA. Its opening baseline says the catalogue, command wiring and Chapter 15 foundations are absent (`:41-84`). Since then the repository has completed the suite taxonomy, truthful gating, deterministic async cleanup, required CI, Criterion/scale foundations, combined coverage and the first 303 lines of Chapter 15 (`docs/progress.md:1291-1314`). Keep that work. Do not re-embed testapp in Bun merely because the old plan says to; a standalone, deterministic OCI workload is needed for real runtimes, and `src/bun/testapp.rs` plus `src/bin/testapp.rs` already provide a reusable core/binary.

Phase 15 omissions are planned work. Bugs above that prevent trustworthy implementation are labelled prerequisites rather than Phase 15 regressions.

### 8.2 Cross-cutting corrections

#### Outcomes and acceptance

Every observation must end in exactly one of:

- `Pass`: the required behaviour was directly observed;
- `Fail`: contradictory evidence was observed or a required operation failed;
- `Skipped`: the selected profile declares the case optional and a known capability says it cannot run;
- `Unknown`: evidence was missing, stale, timed out, ambiguous, or a collector failed.

`Skipped` and `Unknown` aren't synonyms. The full acceptance profile requires a declared capability set and treats a missing required capability, timeout, `Unknown`, unsupported mandatory scenario and cleanup uncertainty as failure. A portable/development profile may allow named skips, but the report and exit policy must list them. The July plan's proposal to turn unavailable trace/firewall evidence into `Pass` (`docs/plans/2026-07-06-plan-chaos.md:53-62`) must be deleted.

Use a versioned JSON schema and snapshot it. Include profile, run ID, cluster identity, binary/build SHA, topology, runtime, kernel, capability evidence, start/end/deadline, outcome, observations and cleanup status. Human output is a rendering of the same report.

#### Safety and production guards

The old guard compares an optional free-text environment to the exact string `production` and then permits destructive work with `--override` (`:501-507`). That fails open for `prod`, missing metadata and future names. Introduce a typed environment/safety class, default unknown clusters to protected, and separate permissions:

- read diagnostics;
- provision isolated test workloads;
- inject workload faults;
- alter node state;
- saturate capacity;
- probe an external destination.

Authenticate every cross-node call with the initiating principal and propagate an operation/run ID. Require an explicit cluster allow-policy for node chaos and capacity tests, not just a client flag. Record who authorised it. A non-interactive `--yes` acknowledges a prompt; it does not grant permissions.

#### Workload and runtime portability

The proposed `bun testapp` executable path (`:74-82,668-673`) assumes the host Bun binary is directly runnable as a workload. That works for ProcessGrill but not for runc or Apple Container. Build and publish a deterministic multi-architecture OCI test image containing a small `testapp`, pin it by digest, and make the release/CI pipeline publish or load it into the test cluster. It should expose health modes, payload, environment echo, controlled memory/CPU, file read/write, exit codes and an observed probe endpoint. Record its digest in the report.

Keep ProcessGrill cases separate. They validate host-process policy and lifecycle, not container networking, mount or image behaviour. A full profile runs the OCI catalogue on runc and Apple Container plus a distinct ProcessGrill profile where supported.

#### Ownership and cleanup

Namespace deletion cannot clean images, global tokens, node faults, partitions, snapshots, mounts or partially-created resources. The old plan both says “clear all faults” and later says clear only the suite's IDs (`:499-507,812-821`). The latter is correct.

Use the resource lease described in the architecture section. Cleanup belongs to an outer runner supervisor, not the case task. If a case panics inside the same task, the old sequence cannot run its teardown despite `JoinSet` reporting the panic (`:682-687,812-820`). Server-side TTL/reaping is the only credible answer to Relish process death. The final result is `Unknown`/`Fail` until cleanup is confirmed.

#### Capabilities and evidence

Replace the proposed boolean capabilities struct (`:144-190`) with the per-node live state described above. Required capabilities include runtime, root/rootless mode, runc/Apple/ProcessGrill, eBPF hook and map state, firewall mode, DNS readiness/reachability, ingress/TLS modes, identity/secret state, volume backend/quota/snapshot semantics, registry reachability/redundancy/signing, metrics/log/event freshness, council/quorum, fault primitives and node-chaos authorisation.

### 8.3 Audit of all 39 catalogue cases

Statuses mean **delivered foundation** (equivalent lower-level/real-cluster coverage exists, but keep public acceptance), **valid**, **rewrite**, or **blocked prerequisite**. None of these are regressions merely because the Phase 15 command isn't present.

| # | Historical case | Assessment at reviewed SHA |
|---:|---|---|
| 1 | `schedule_fixed_replicas_across_nodes` | **Delivered foundation; rewrite assertion.** Real placement suite covers spread. Public acceptance should consume explicit node placement evidence rather than assume status shape. |
| 2 | `schedule_respects_required_placement_label` | **Valid.** Scheduler units exist; require node labels in live capability/status evidence. |
| 3 | `schedule_rejects_app_exceeding_namespace_quota` | **Delivered foundation/valid acceptance.** Keep public API failure and first-app-unchanged assertions. |
| 4 | `resolve_returns_vip_and_healthy_backends` | **Delivered foundation/valid.** Real cluster resolve coverage exists; also assert DNS from the workload namespace in full profile. |
| 5 | `resolve_reflects_scale_up` | **Rewrite.** There is no `relish scale`; apply a changed replica count or drive autoscaling, then observe service state. |
| 6 | `stopped_instance_leaves_the_backend_list` | **Delivered foundation/valid.** Keep bounded eventual assertion. |
| 7 | `rolling_deploy_replaces_image_without_losing_backends` | **Valid.** Use pinned OCI v1/v2 images and sample the real data plane, not just `/resolve`. |
| 8 | `failed_deploy_rolls_back_automatically` | **Valid.** Existing deploy/health units provide foundations; assert terminal operation contract. |
| 9 | `deploy_history_records_each_version` | **Valid.** Include operation IDs/outcomes; don't depend on the empty active-deploy endpoint. |
| 10 | `unhealthy_app_is_restarted` | **Valid.** Existing health/restart coverage; use instance ID and restart timeline evidence. |
| 11 | `hanging_health_check_marks_instance_unhealthy` | **Valid.** Preserve concurrent agent-readiness assertion. |
| 12 | `slow_but_within_timeout_app_stays_running` | **Valid.** Thirty seconds belongs in full/slow profile, not portable unit CI. |
| 13 | `encrypted_env_value_is_decrypted_in_workload` | **Blocked/rewrite.** Requires secret decryption specifically, not coarse `Identity`; inspect from the hermetic workload and ensure APIs never expose plaintext. |
| 14 | `config_file_is_mounted_with_contents` | **Valid; runtime matrix.** Test runc and Apple separately and define ownership/mode semantics. |
| 15 | `cluster_pubkey_encrypt_roundtrip` | **Blocked/rewrite.** Requires live secret/pubkey state and authorised exec/probe; `Identity` isn't the right capability. |
| 16 | `allow_from_permits_listed_app` | **Blocked prerequisite.** First fix fail-open eBPF admission. Probe from A's namespace and record live hook/map evidence. |
| 17 | `unlisted_app_is_denied` | **Blocked prerequisite.** Same; a declared-policy evaluator is insufficient. |
| 18 | `firewall_reflects_allow_from_change` | **Blocked prerequisite.** Assert atomic live map transition without an unenforced window. |
| 19 | `workload_receives_spiffe_certificate` | **Delivered foundation/valid.** Add non-default cluster name to catch FUNC-4 and verify expiry/chain/SPIFFE URI. |
| 20 | `jwks_endpoint_serves_signing_keys` | **Delivered foundation/valid.** Verify key use/rotation, not just non-empty JSON. |
| 21 | `namespace_scoped_token_is_rejected_elsewhere` | **Delivered foundation/valid.** Existing auth tests cover the contract; retain end-to-end cross-node acceptance. |
| 22 | `ingress_routes_host_header_to_app` | **Delivered foundation/valid.** Run against the hermetic OCI app and current enabled ingress mode. |
| 23 | `ingress_returns_502_when_no_backends` | **Delivered foundation/valid.** Define whether 502 is stable API behaviour. |
| 24 | `ingress_route_appears_in_routing_table` | **Delivered foundation/valid.** Compare route state with an actual request. |
| 25 | `volume_data_survives_instance_restart` | **Valid; runtime/backend-specific.** Distinguish process restart, container replacement and node restart. |
| 26 | `volume_is_isolated_per_app` | **Valid.** Include path/mount permission evidence. |
| 27 | `volume_size_limit_is_enforced` | **Rewrite/profile.** Btrfs quota and loop-file semantics differ; Apple support needs a defined backend. No vague green skip in full profile. |
| 28 | `process_workload_reports_running` | **Delivered foundation; separate profile.** ProcessGrill only. |
| 29 | `process_job_with_exit_zero_succeeds` | **Delivered foundation; separate profile.** Use an allowlisted deterministic executable, not shell if shell isn't what is under test. |
| 30 | `failing_job_retries_then_fails` | **Rewrite.** Current dispatch budget is fixed at three attempts (`src/bun/batch.rs:49-59,858-884`); `retries = 2` isn't a current `JobSpec` field. Test current semantics and add configurable retry only as separate product work. |
| 31 | `job_runs_to_completion_and_reports_exit` | **Delivered foundation/valid.** Run OCI job on real runtime as well as ProcessGrill. |
| 32 | `scheduled_job_fires_on_its_schedule` | **Valid but slow.** Current parser stores cron text; ensure the production scheduler actually owns firing before accepting the case. |
| 33 | `job_logs_are_retrievable_after_completion` | **Valid.** Include cross-node and retention evidence where logs capability promises it. |
| 34 | `push_and_pull_image_roundtrip` | **Delivered foundation/valid.** Existing Pickle digest/integrity tests are strong; public acceptance uses the hermetic image. |
| 35 | `manifest_catalog_lists_pushed_image` | **Delivered foundation/valid.** Include persistence across registry/Bun restart. |
| 36 | `deploy_from_cluster_registry` | **Blocked prerequisite.** Requires peer-reachable authenticated registry, hermetic signed OCI image, runtime and trust-policy matrix. |
| 37 | `all_nodes_report_alive` | **Delivered foundation/valid.** Real cluster tests pass; include node API/capability freshness. |
| 38 | `council_has_leader_and_quorum` | **Delivered foundation/valid.** Real failover/recovery tests pass; assert one leader at one term from consistent evidence. |
| 39 | `every_node_answers_health` | **Delivered foundation/rewrite.** Liveness alone is too weak; add authenticated readiness/capabilities on every node. |

### 8.4 Chaos catalogue and prerequisites

The five proposed scenarios (`docs/plans/2026-07-06-plan-chaos.md:499-517`) should not be implemented as written:

- C1 leader failure, C2 worker death and C5 death during deploy require a real node-failure primitive. `NodeKill` currently rejects (`src/bun/agent.rs:3000-3012`). A process-local fault record is not node failure.
- C4 asks for pressure on one worker, but current CPU/memory faults target workload cgroups. Implement node-scoped, bounded, automatically expiring pressure with reserved headroom for Bun/SSH/recovery.
- C3 partition is closest to viable, but the current Relish scenario may inject through the wrong node (`src/relish/chaos.rs:109-114`). It needs per-node authenticated routing, partition ownership and independent majority/minority observation.
- Node drain needs cordon, eviction, placement convergence and uncordon semantics. It isn't interchangeable with kill.

Every scenario must state the steady-state hypothesis, exact blast radius, abort condition, maximum duration, recovery mechanism and cleanup evidence. Unsupported mandatory chaos is `Unknown/Fail` in the full profile, never a green skip. Run these only after contracts/safety, leases and the ordinary catalogue are reliable.

### 8.5 Benchmark plan corrections

The existing developer benchmarks are good deterministic gossip foundations. They measure transport, setup and simulated convergence from 5 to 1,000 nodes, with a separate deterministic 10,000-member scale acceptance. Keep them.

The proposed in-cluster suites use the wrong paths in two places (`docs/plans/2026-07-06-plan-chaos.md:521-535`):

- 200 `GET /v1/resolve/{name}` calls measure the control-plane HTTP handler and serialisation, not service discovery as experienced by a workload. Measure an actual DNS query plus connect from a workload namespace, and report DNS and connect separately.
- Fetching a backend's host port directly bypasses Onion and Wrapper. Define direct-backend, Onion service and ingress throughput as distinct metrics.
- “Reconstruction time” currently measures leader election only. State reconstruction should include the replacement/restarted node loading snapshot/log, rejoining gossip, serving consistent state, rebuilding maps/routes and becoming ready.
- A timeout is a failed benchmark sample/run, not “skipped because timed out”. Skip only before execution for an allowed missing capability.

Every report must include binary SHA/version/profile, Rust build mode, target architecture, runtime/version/rootless mode, kernel, CPU/memory, node count/topology, network placement, eBPF programmes/hooks, workload image digest and relevant configuration. Comparison should refuse incompatible fingerprints unless the user explicitly requests informational comparison. Hosted noisy measurements remain informational; regression gates need controlled runners and repeated baselines.

### 8.6 `wtf` data audit

The pure `diagnose(WtfInputs) -> WtfReport` idea is worth keeping. It makes correlation deterministic and testable. The proposed patterns cannot all be honest with current data (`docs/plans/2026-07-06-plan-chaos.md:539-565`):

| Pattern | Data available now | Required addition |
|---|---|---|
| unreachable/dead nodes, leader/quorum | Membership and council endpoints exist | Per-node authenticated address, freshness and readiness evidence. |
| crashloop | Status/restart counts exist | Restart timestamps/window and previous instance IDs; count alone cannot prove “three in 15 min”. |
| crashloop after deploy | Deploy history exists | Stable deploy operation/version timestamp and cross-node event correlation. |
| no backends | Resolve/service state exists | Desired replica evidence and live map/DNS readiness; avoid duplicate findings. |
| deploy stuck | Active endpoint always empty | Real active operation state (`src/bun/api.rs:3345-3350`). |
| active faults | Fault list exists | Owner/run ID, node scope, expiry and cleanup state. |
| alerts | Alert API exists | Cluster-wide freshness/source evidence. |
| disk high | Some metrics/disk pressure state exists | Per-node disk capacity/usage/pressure with timestamp and storage-domain attribution. |
| CPU throttling | Metrics exist | Actual throttled time/cgroup limit evidence, not merely usage near limit. |
| cert expiring | Certificates exist internally | Safe certificate metadata API: subject/SPIFFE, issuer, serial, not-after, rotation status; no private material. |
| registry redundancy | Catalogue/replication exists | Per-node layer ownership, desired/actual redundancy, registry reachability and last heal error. |
| cluster events | Bun has a process-local 1,024-event ring (`src/bun/events.rs`) | Cluster-wide durable/bounded aggregation, cursor and freshness semantics. |

Never emit an OK row when the source is absent. Emit `Unknown` with source/error, and let the selected acceptance/diagnostic policy determine exit status.

### 8.7 Trace redesign

The old trace proposal admits that its firewall verdict is inferred from desired config and its TCP probe originates from Bun (`docs/plans/2026-07-06-plan-chaos.md:569-598`). Labelling those caveats doesn't make the overall trace an end-to-end observation.

The redesigned trace should be a protected `POST` operation (it has side effects on the network and may carry probe parameters), authorised for the source workload and destination. For an internal destination:

1. Select a concrete running source instance and identify its node/runtime/network namespace.
2. Execute an actual DNS A/AAAA query using that workload's resolver path. Record nameserver, response, latency and whether the result came from userspace or a live TC fast path.
3. Read the live Onion service/eBPF map state on that node and compare the returned VIP/backends.
4. Read live firewall/egress programme and map state, not only declared `allow_from`. If only policy can be evaluated, label the step `Inferred`, not `Pass`.
5. Run a bounded TCP/HTTP probe from the source workload namespace using a controlled probe helper. Record resolved destination, selected backend where observable, latency and error.
6. Correlate Wrapper only when the requested path uses ingress.

Each evidence item needs `Observed`, `Inferred` or `Unknown`, alongside the outcome. Do not allow arbitrary external DNS/TCP/HTTP probes by default; that turns an authenticated cluster API into an SSRF/scanning primitive. External trace needs a destination allowlist/egress policy, explicit permission, safe port/protocol limits, DNS rebinding checks, private/link-local/metadata-address protections, deadline/byte limits and audit event.

### 8.8 Audit of the 15 historical steps

| Old step | Disposition |
|---:|---|
| 1 capabilities endpoint | **Rewrite and move after contracts.** Per-node live evidence, not booleans/config. |
| 2 embed testapp in Bun | **Delete/rewrite.** Reuse the library, build a pinned multi-arch OCI image; keep ProcessGrill helper separate. |
| 3 testkit core | **Rewrite first.** Add Pass/Fail/Skipped/Unknown, profile policy, evidence and stable schema. |
| 4 runner | **Rewrite.** Runner supervisor plus server-side resource leases/TTL/reaper; panic/process-death cleanup. |
| 5 `relish test` | **Keep after core.** Render one report and implement strict full-profile exit semantics. |
| 6 catalogue A | **Rebase.** Reuse delivered foundations and current APIs; no invalid retry/scale assumptions. |
| 7 catalogue B | **Rebase/reorder.** Block firewall/registry cases on real prerequisites; runtime matrix. |
| 8 chaos | **Move after missing primitives.** Implement node failure/drain/pressure first. |
| 9 bench core | **Keep schemas/comparison, add fingerprints.** Refuse incompatible comparisons. |
| 10 bench suites A | **Rewrite data paths.** Real DNS/connect and workload observations. |
| 11 bench suites B | **Rewrite/reorder.** Direct/Onion/ingress throughput separate; reconstruction broader; timeout fails. |
| 12 `wtf` pure engine | **Keep after telemetry contracts.** Unknown is first-class; no OK without data. |
| 13 `wtf` collection/CLI | **Keep after engine/APIs.** Authenticated cross-node collection with freshness. |
| 14 trace endpoint | **Rewrite.** Observed source-netns probe, real DNS/live maps, POST/auth/SSRF controls. |
| 15 trace CLI/close-out | **Keep last.** Only close progress/book after real-cluster and Apple/runc acceptance evidence. |

### 8.9 Corrected implementation order

1. **Contracts and safety:** outcome/evidence/profile/JSON schemas, typed environment, permission model, strict deadlines and full-profile exit rules.
2. **Capability and evidence API:** per-node live subsystem/runtime/kernel/readiness state, authenticated cross-node collection.
3. **Resource leases and hermetic workload:** server-side TTL/reaper, owned cleanup, pinned multi-arch OCI test image, separate ProcessGrill helper.
4. **Ordinary test catalogue and `relish test`:** rebase all 39 cases on current APIs; runc and Apple runtime profiles; no unsupported green skips.
5. **Missing chaos primitives:** real drain, kill and node-scoped pressure with recovery/ownership, then the five scenarios.
6. **Benchmarks:** fingerprinted report/comparison, real DNS/network data paths, reconstruction and opt-in capacity.
7. **`wtf`:** add missing telemetry first, then pure diagnosis and collection/CLI.
8. **Trace:** observed source-namespace path, live policy state and external-probe safety.
9. **Documentation and acceptance:** update design/whitepaper/book/progress only after a fresh real-cluster run on runc and a defined Apple Container profile.

## 9. Reproducibility appendix

### 9.1 Host and reviewed tree

```text
review worktree: /Users/miko/github/reliaburger-review-2026-07-17
branch:          codex/codebase-review-2026-07-17
SHA:             8ba727f9332c35a1f6603a0f78f76a424513485c
host:            Apple silicon, macOS 26.3.1 / Darwin 25.3
Rust:            rustc/cargo 1.97
Linux VM:        Ubuntu arm64, kernel 6.8.0-124, 4 CPU, 8 GiB
Linux tools:     runc, Buildah, bpftool, clang 18, cgroup v2, Btrfs tools
```

The worktree started clean at the reviewed SHA. The checked-in PoC is reproducible with `sudo EVIDENCE_DIR=/tmp/reliaburger-dns-tc-evidence poc/dns-tc/run.sh`; its source and text evidence are tracked, while the generated object and packet captures remain outside Git. The runner removed its temporary network namespaces, veths, bridge, TC filter and loaded BPF objects after the run.

### 9.2 Check matrix

| Check | Result | Evidence/notes |
|---|---|---|
| `cargo fmt --all -- --check` | Pass | No formatting diff. |
| `cargo clippy --all-targets --all-features -- -D warnings` on macOS | **Fail** | 54 errors from target-Linux Aya types/modules referenced by all-feature macOS compilation. This is the portability-contract defect described in §1.3. |
| Same clippy command on Linux | Pass | Clean all-target/all-feature lint in the Lima VM. |
| `make test` | Pass | 2,628 passed, 39 skipped, 29.872 s. Required a sandbox exception for loopback sockets; the first sandbox denial was environmental. |
| `make test-doc` | Pass/empty | 0 doctests discovered. |
| `make pdf` | Pass | Quarto rendered the 19-part book, design collection, whitepaper and roadmap through LuaLaTeX. Generated PDFs stayed ignored under `docs/_book/`. |
| `make test-no-default` | Pass | 2,610 passed, 39 skipped, 30.488 s. |
| `make test-slow` | Pass | 4 passed, 29 skipped, 16.257 s. |
| `make test-linux` equivalent, privileged Linux | Pass | 33/33: 20 eBPF, 2 Buildah, 7 runc/netns/Btrfs, 1 identity tmpfs, 3 cgroup resource tests. Executed already-built test binaries as root in the isolated VM with the Makefile's environment gates. |
| `make test-cluster` | Pass | 20/20: chaos 2, failover 2, gossip 4, disaster recovery 4, self-healing 3, placement 5. |
| `make test-upgrade-node` | Pass | 5/5 real single-node binary-replacement tests. |
| `make test-upgrade-cluster` | Pass | 3/3 real rolling cluster binary-replacement tests. |
| `make test-apple` | Pass | 2 passed, 2,665 skipped: create and adopt/retrack on Apple Container. |
| `make coverage` | Pass | Default 2,628/2,628 and no-default 2,610/2,610; 81.24% lines, 82.57% regions, 80.50% functions. |
| `make examples` | **Fail** | 20/20 because target omits `--dry-run`; see FUNC-2. |
| Manual intended example dry run | **Partial** | 18/20 pass; two schema/config failures listed in FUNC-2. |
| `make bench` | Pass | Criterion transport and 5–250-node seeded gossip simulation. |
| `make bench-large` | Pass | 500/1,000-node setup and convergence. |
| `make bench-10k` | Pass | One-node 10,000-member state acceptance, 0.07 s test time. |
| Dependency advisory scan | Not available | `cargo-audit` and `cargo-deny` absent; no claim of advisory cleanliness. |

The privileged Linux suite initially failed to find its BPF objects when the manually executed binary had neither the Cargo working directory nor `CARGO_TARGET_DIR`. Re-running with `CARGO_TARGET_DIR=/Users/miko/github/reliaburger/review-linux/target` passed all 20 eBPF cases. That first result was an invocation error, not a product failure. The first concurrent Linux link was killed by the 8 GiB VM; a `-j1` build completed in 8m48s. Both are recorded to keep platform/build limitations separate from test results.

The real-cluster tests emitted cleanup warnings:

```text
umount: /var/lib/reliaburger/volumes/.identity/default__web-0:
       must be superuser to unmount
```

Assertions passed, but the warning shows that some test-cluster identity mount cleanup depends on privilege not available to that process. The upgrade tests also emitted the expected registry warning:

```text
[images] registry_bind is 127.0.0.1 in cluster mode —
image replication and P2P pulls between nodes will not work
```

### 9.3 Coverage detail

The combined report's useful headline rows were:

| Component | Line coverage |
|---|---:|
| Total | 81.24% |
| `src/bin/bun.rs` | 14.60% |
| `src/bin/relish.rs` | 61.51% |
| `src/bun/agent.rs` | 73.82% |
| `src/bun/api.rs` | 55.99% |
| `src/bun/supervisor.rs` | 97.36% |
| `src/smoker/safety.rs` | 99.16% |
| `src/wrapper/proxy.rs` | 93.24% |
| `src/wrapper/routing.rs` | 97.50% |

This supports the qualitative conclusion: pure/state-machine modules are usually very well covered, while process composition and the widest HTTP/agent integration surfaces remain the risk.

### 9.4 Benchmark evidence

The standard Criterion run reported, on this host:

```text
transport_send_recv                    152–158 ns
single_gossip_round                    658–670 ns
gossip_setup/25                         15.2–15.9 us
gossip_setup/100                        65.3–68.4 us
gossip_setup/250                         160–167 us
gossip_convergence_end_to_end/5           33–36 us
gossip_convergence_end_to_end/10         128.4–128.8 us
gossip_convergence_end_to_end/25          1.70–1.73 ms
gossip_convergence_end_to_end/50          5.26–5.32 ms
gossip_convergence_end_to_end/100         35.7–37.0 ms
gossip_convergence_end_to_end/250          333–360 ms
dissemination_enqueue_select                36–36.7 us
```

The large run reported setup at roughly 297–308 us for 500 members and 616–636 us for 1,000; end-to-end convergence was 1.80–1.85 s for 500 and 10.78–11.43 s for 1,000. Criterion warned that the 1,000-member sample required a longer target time; it completed successfully. The deterministic 10,000-member acceptance reported ingest in 6.3 ms and first dissemination of every update in 61.3 ms across 39,437 batches.

These are reproducibility data, not release thresholds. The report lacks the full machine/build fingerprint proposed for Phase 15 and was run on a busy development host.

### 9.5 Commands used

The principal commands were:

```bash
git rev-parse HEAD
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
make test
make test-doc
make pdf
make test-no-default
make test-slow
make coverage
make examples
make bench
make bench-large
make bench-10k
target/debug/relish --help
target/debug/relish <group> --help
target/debug/relish apply --dry-run examples/.../*.toml
```

Linux variants used the same source SHA in the existing Lima VM. The final privileged selection matched `Makefile:31-32`; real-cluster, upgrade and Apple selections matched `Makefile:34-47`.

## 10. Starting point for future planning

The shortest credible route forward isn't to finish every whitepaper feature. It is:

1. close the four P1 security/DNS/onboarding gaps;
2. make runtime capabilities and readiness observable and enforce them at scheduling time;
3. fix the current cluster-mode defaults for registry/ingress/auth rather than relying on warnings;
4. split the agent/API ownership seams before adding Phase 15;
5. implement Phase 15 in the corrected order from §8.9;
6. then reconcile the whitepaper/design documents to shipped, planned and deferred states with mechanically tested command examples.

Reliaburger already has enough working machinery to justify hardening rather than rewriting. The test foundations are a real asset. The next phase should use them to make the system refuse unsafe ambiguity, report degraded truth, and ensure the documentation describes the binary an operator can actually run. After all, a warning that the security policy isn't enforced is honest, but it still isn't a security policy.
