# Phase 12b.6 — Smoker effects and cleanup (Theme SM)

Theme: `docs/progress.md` §12b.6. Finding: CHAOS1.
Source: `docs/plans/2026-07-10-review-past-phase-12.md`.

Harness contract (#106): green = `make ci`; deterministic, observable sync,
no sleeps; env tests `#[ignore]` + named; coverage floor 78.65 — cover new
code; **do NOT run `make coverage` without 40+ GiB free**; no headline
count. CHAOS1 acceptance: **measure each advertised effect AND its removal.**

## Ground truth (verified 15 Jul)

- **Already live — do NOT touch:** eBPF network faults (Delay/Drop/
  Partition/Bandwidth/DnsNxdomain) write/clear/expire fully
  (`agent.rs:2996-3137`, maps in `smoker/bpf_types.rs`); Kill (SIGKILL,
  `smoker/process.rs:11`).
- Memory pressure + disk I/O throttle: **genuine no-ops** returning Ok
  (`agent.rs:2853`); the cgroup apply/remove fns exist
  (`smoker/resource.rs:25-109`) but are never called.
- CPU stress: burns **Bun's own cgroup** via spin tasks (`agent.rs:2810`),
  not the target; no early-stop on clear.
- Pause (SIGSTOP, `smoker/process.rs:28`) never auto-resumes: `expire_
  faults` (`agent.rs:2938`) doesn't SIGCONT; Resume is a separate manual
  fault (`process.rs:42`).
- Node drain/kill: return Ok without applying (`agent.rs:2864`).
- Target cgroup path is reachable: `grill/cgroup.rs:35`
  `cgroup_path(ns,app,ordinal)`; instance ns/app from
  `supervisor.list_instances()` (near `agent.rs:2878 target_pids`). The
  fault apply currently gets only a service name — thread instance metadata
  in.

## Implementation steps (tests first, each measuring effect + removal)

### 1. Memory pressure + disk I/O throttle — wire + reverse
Call the existing `smoker::resource` apply fns against the TARGET
instance's cgroup (compute via `cgroup_path` from instance ns/app), and the
matching remove fn on clear/expiry. Tests: applying memory/disk fault
writes the cgroup limit for the target instance; clear/expiry removes it
(assert the cgroup file value before/after). Linux-cgroup-gated →
`#[ignore]` + `make test-linux`; a pure unit test can cover the argv/path
builder deterministically in the portable suite.

### 2. CPU stress — target cgroup, reversible
Write `cpu.max` (a quota) to the TARGET instance cgroup instead of burning
Bun's CPU; remove it on clear/expiry (early stop, not just deadline). Test:
CPU fault sets the target cgroup's cpu.max; clear restores it. Portable
unit test for the quota computation + path; effect test gated.

### 3. Pause auto-resume
Track pause state in the registry; on clear/expiry of a pause fault, send
SIGCONT (auto-resume) rather than leaving the process frozen. Test: a pause
fault that expires (or is cleared) leaves the process running again
(observe via process state, not a sleep — poll a bounded loop).

### 4. Node drain / node kill — real effect or honest rejection
CHAOS1: stop returning success for a no-op. Either apply a real effect
(drain: cordon + evict local replicas; kill: terminate the node's
workloads) via the existing cluster machinery, OR reject the fault as
not-locally-applicable with a clear error (an honest 4xx/error, not a fake
Ok). Pick the smaller correct option; document. Test: the chosen behaviour
is asserted (effect measured, or the honest rejection returned).

### 5. Thread instance metadata into fault apply
`apply_fault` receives the target instances (ns/app/ordinal → cgroup), so
resource faults can target correctly. Keep the existing safety checks
(quorum/replica/leader/node-%) intact.

## Acceptance
- `make ci` green. cgroup-effect tests → `make test-linux` `#[ignore]`;
  portable tests cover path/quota/argv + registry pause-state + the
  drain/kill decision deterministically. Quote gated results or state
  unverified.
- `docs/progress.md`: nested `- [x]` under "Smoker effects and cleanup";
  check the theme box. Book: chapter 8 "Breaking Things on Purpose" — each
  fault's real effect + guaranteed reversal. No headline count.

## Constraints (seam ownership — sibling PW + SU run concurrently)
- YOURS: `src/smoker/{resource,node,process,registry}.rs`, **`src/bun/
  agent.rs` ONLY the fault region ~2777-3137**, `src/grill/cgroup.rs`
  (reuse/extend), the fault handlers in `src/bun/api.rs` (~2375-2524,
  minimal).
- NOT YOURS: `src/bun/agent.rs` supervisor-construction ~1195-1300 (PW) and
  the upgrade/adoption region (SU); `src/grill/{process_workload,rootless,
  apple}.rs`; `src/upgrade/*`; the upgrade handlers in api.rs (~580).
- Do NOT touch the already-live eBPF network faults / Kill paths beyond
  what threading instance metadata requires.
- thiserror; no unwrap in production; lowercase errors, no trailing full
  stop; `// SAFETY:` on any unsafe; British English.
