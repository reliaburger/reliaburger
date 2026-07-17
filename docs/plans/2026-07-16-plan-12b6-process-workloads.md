# Phase 12b.6 — Process workloads and platform capabilities (Theme PW)

Theme: `docs/progress.md` §12b.6. Findings: H8, D17, D15, old M22/M23.
Source: `docs/plans/2026-07-10-review-past-phase-12.md`,
`...2026-07-09-review-design-discrepancies.md`.

Harness contract (#106): green = `make ci` (nextest portable + doctest +
no-default); deterministic tests, observable sync, no sleeps; env tests
`#[ignore]` + named in the right `make test-*` suite; coverage floor 78.65
in CI — cover new code or extract-and-test; **do NOT run `make coverage`
without 40+ GiB free** (`df -h /`); **no headline test count.**

## Ground truth (verified 15 Jul)

- D17/H8: supervisor built `.new()` (default all-allowed) at
  `src/bun/agent.rs:1195,1260`; `WorkloadSupervisor::with_process_config`
  (`src/bun/supervisor.rs:102`) has no caller, so `[process_workloads]`
  (`src/config/node.rs:26`, `src/config/process_workloads.rs`) is ignored
  → a config deploy can run an arbitrary host executable through
  ProcessGrill. `ProcessManager::prepare_exec` (`src/grill/process_
  workload.rs:59`) DOES validate the allowlist (called from
  `supervisor.rs:138`) — it's just never given a non-empty policy; and an
  EMPTY allowlist currently means all-allowed (`process_workloads.rs:51`).
- D15: `src/bun/gpu.rs:28` `StubGpuDetector` always returns none;
  `gpu_enabled` (`node.rs:388`) + `src/meat/scheduler.rs:159` gpu request
  are inert.
- M22: `src/grill/rootless.rs:97` drops limits (`resources=None`),
  systemd-run never called; `setup_slirp4netns` (`rootless.rs:165`)
  unwired.

## Implementation steps (tests first, each asserting the refusal/effect)

### 1. D17/H8 — enforce process-workload policy (default-deny)
- Thread `[process_workloads]` from `NodeConfig` into the supervisor:
  replace the `.new()` sites (agent.rs:1195,1260) with `with_process_
  config(...)` fed the parsed config.
- Semantics: host exec/script (ProcessGrill running an arbitrary host
  binary) is **default-DENY** — an empty/absent allowlist denies host
  exec, not allows it. A binary runs only if explicitly allowlisted.
  (Container workloads via runc/apple are unaffected; this is about
  ProcessGrill host execution.) Enforce mount isolation before
  ProcessGrill creates the process.
- Tests: a deploy of a host exec/script NOT in the allowlist is refused; an
  allowlisted binary runs; the policy comes from node.toml not a default.

### 2. D15 — effective GPU detection or explicit rejection
- Implement a real detector (parse `nvidia-smi -L` / probe `/dev/nvidia*`;
  keep it feature/OS-guarded and fall back cleanly) behind the existing
  detector seam, and pass the device into the OCI spec for isolation when a
  workload requests a GPU. OR, if real detection/isolation is out of scope
  for this pass, make `gpu_enabled` **explicitly reject** GPU-requesting
  workloads with a clear "GPU not supported on this node" error instead of
  silently scheduling them onto a node that reports zero GPUs. Pick the
  smaller correct option; do not leave it a silent lie.
- Tests: a GPU-requesting workload on a no-GPU node is refused (or, if
  detection implemented, placed only where a device exists); `gpu_enabled`
  actually affects behaviour.

### 3. M22 — rootless: implement or explicitly reject
- Either wire limits (systemd-run --user --scope for cgroup limits) +
  `setup_slirp4netns` for networking, OR **reject up front**: if a node is
  rootless and a workload needs enforced resource limits or container
  networking that rootless can't provide, fail the deploy/startup with a
  clear error rather than silently dropping limits / leaving an empty
  netns. Prefer the reject path unless wiring is cheap and testable.
- Tests: rootless + a limit-requiring workload is either enforced or
  refused with a clear message — never silently unlimited.

### 4. Remove/wire remaining dead config
`reserved_cpu`/`reserved_memory` (`node.rs:384`) parse but the scheduler
ignores them — wire into the node capacity the scheduler reserves, or
remove. Name what you wired vs removed.

## Acceptance
- `make ci` green. Any Linux-only rootless/gpu behaviour → named `#[ignore]`
  in `make test-linux`; quote results or state unverified.
- `docs/progress.md`: nested `- [x]` under "Process workloads and platform
  capabilities"; check the theme box. Book: `docs/design/agent-bun.md` +
  chapter 15; the process-workload security story (default-deny host exec).
- No headline count.

## Constraints (seam ownership — sibling SM + SU run concurrently)
- YOURS: `src/bun/supervisor.rs`, **`src/bun/agent.rs` ONLY the supervisor-
  construction region ~1195-1300**, `src/config/{node,process_workloads}.rs`,
  `src/grill/{process_workload,rootless}.rs`, `src/bun/gpu.rs`,
  `src/meat/scheduler.rs` (gpu), and the config→agent wiring in
  `src/bin/bun.rs`.
- NOT YOURS: `src/bun/agent.rs` fault region ~2777-3137 (SM) and the
  upgrade/adoption region (SU); `src/grill/apple.rs` adoption (SU owns it);
  `src/smoker/*`, `src/upgrade/*`. Quote your agent.rs region; keep edits
  inside it.
- thiserror; no unwrap in production; lowercase errors, no trailing full
  stop; British English.
