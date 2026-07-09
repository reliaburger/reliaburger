# Phase 11b — wire the library-only subsystems, then finish the eBPF data path

Closes the July 2026 review's remediation plan. This branch takes Reliaburger's
"implemented but never wired" subsystems and drives them end to end from the
`bun`/`relish` binaries, then completes the eBPF data path so service discovery,
network fault injection and egress allowlists actually enforce in the kernel.

## Why

The [July 2026 review](plans/review-2026-07.md) ran six parallel subsystem audits and
found that the green test suite was misleading: a large fraction of
`progress.md`'s "done" items were **library-only** — implemented and unit-tested
in isolation, but with no production caller. The structural root cause was a
handful of hardcoded `None`s in `api::router()` and `bin/bun.rs` that silently
disabled auth, identity, rollups, GitOps, and cross-node queries even under
`bun --cluster`. The review laid out a five-stage remediation (Stages 0–4).
Stages 0–3/3b landed earlier; **this branch is Stage 4 plus the eBPF-enforcement
follow-ups (P0–P3) and three security Mediums pulled forward (P4).**

## What's in this branch

24 commits, ~8.5k lines. Each sub-stage lands `make ci`-green with an
integration test that drives the **binary path** (a real `BunAgent` + the API
router / cluster runtime), not the library.

### Stage 4 — wiring (W1–W12)

Every sub-stage flushed out a latent bug that the isolated unit tests never hit:

| Sub-stage | What got wired | Latent bug surfaced |
|---|---|---|
| W1 | `relish` fmt recursion, k8s-import fidelity, logs flags, dry-run exit codes (H11, M17, X4, X5) | `relish fmt` rewrote nested-table configs as invalid TOML over the user's file |
| W2 | Wrapper ingress listener behind `[ingress]` (L7) | — |
| W3 | DNS responder with full M8 hardening (L9, M8) | recv error killed the responder; unmatched `.internal` leaked upstream |
| W4 | Rollup workers + live `/v1/metrics/cluster` + real capacity reports (L6/L11) | DataFusion 45 overflow on `timestamp <= u64::MAX` range predicates |
| W5 | Raft-backed Pickle catalog, replication, two-phase GC (L10/M2, X1) | `council.write(AppSpec)` hung forever — bincode can't drive `deserialize_any`; switched the durable log + council RPC to serde_json |
| W6 | Cluster scheduling via Raft placements + per-node reconciler (L1, H8) | scheduler packed all replicas on one node (spread weight lost to bin-pack) |
| W7 | Production deploy orchestration + real rollback (L2, M16, X3) | rollback leaked the failed step's new instance |
| W8 | Leader autoscale loop (L3) | — |
| W9 | State reconstruction gates scheduling until Active (L4) | unknown-node exclusion churned; dropped it, kept the learning gate |
| W10 | Leader GitOps sync loop + webhook + real key trust (L13, H12) | a fresh clone has nothing to fetch but nothing applied — now syncs on HEAD ≠ last-applied; `is_key_trusted` no longer falls through to `true` |
| W11 | Real Smoker fault injection + chaos partitions via transport blocklists (L14/L15) | the quorum/replica safety rails read hardcoded-zero counts and could never fire; replaced a no-op "worker isolation" chaos test with a real partition |
| W12 | eBPF loader in production (L8) + egress allowlists (L16) | — |

Plus a flaky-test fix: the rolling-redeploy path derived instance IDs from
`now % 10000`, so two redeploys in the same second collided ("instance already
exists"); replaced with a monotonic counter.

### eBPF production enforcement (P0–P3)

W12 loaded the eBPF programs but two paths stayed loaded-but-inert. This branch
closes them:

- **P0** — `monotonic_now_ns()` used a process-relative `Instant` epoch, but the
  eBPF fault programs compare `expires_ns` against `bpf_ktime_get_ns()`
  (CLOCK_MONOTONIC). The values were billions of ns apart, so kernel faults
  would never expire. Now reads CLOCK_MONOTONIC on Linux.
- **P1** — the agent mirrors its live service map into the kernel `backend_map`
  at every mutation, so the connect-rewrite hook actually resolves VIPs to live
  backends in production (`agent_deploy_populates_backend_map`).
- **P2** — the network-fault writers translate a `FaultRule` into real
  `fault_connect_map`/`fault_dns_map`/`fault_bw_map` entries (Drop/Delay/DNS/
  Bandwidth/Partition), keyed by the target's VIP, with the P0-correct expiry
  (`agent_drop_fault_refuses_vip_with_eperm`).
- **P3** — a rate-limited task re-resolves DNS-based egress allowlists and
  reprograms only the delta as IPs rotate (`egress_diff` unit tests).

### Security Mediums pulled forward (P4)

Three never-staged, high-value review findings:

- **M25** — workload private keys and JWTs were written world-readable (0644);
  now 0600.
- **M1** — metric/log query params (`name`, `app`, `namespace`, `grep`) were
  interpolated raw into DataFusion SQL, so `x' OR '1'='1` broke tenant isolation.
  Now single-quote-escaped, with injection tests.
- **M18** — the perimeter firewall reconciled on node *count* (missing swaps,
  never applying standalone) and dropped cluster ports on TCP only (gossip UDP
  reachable). Now reconciles on the membership *set* and drops both protocols.

## Verification

- **Host** (`make ci`): fmt `--check`, clippy `-D warnings`, and the full suite —
  **1,586 lib + 117 integration = 1,703 tests green** on the default (no-eBPF)
  build.
- **Lima VM** (`reliaburger-test`, kernel 6.8): the eBPF data path is verified
  where it actually runs — **12 `tests/ebpf.rs` integration tests** load the real
  programs into the kernel (load/attach, backend-map read/write, connect→VIP
  rewrite, agent-driven backend sync, Drop-fault EPERM, egress allow/deny, DNS).
  `cargo clippy --features ebpf` is clean. These need root + a 5.7+ kernel +
  cgroup v2, so they are **not** part of `make ci`:
  ```
  sudo -E env PATH="$PATH" CARGO_TARGET_DIR=/tmp/rb-ebpf-target \
    RELIABURGER_EBPF_TESTS=1 cargo test --features ebpf --test ebpf -j1
  ```
- Book chapters updated alongside (ch 2, 3, 5, 7, 8, 9, 10, 11); `progress.md`
  and both READMEs refreshed.

## Explicitly out of scope (documented, not dropped)

The review's Stages 0–4 are fully done. Everything the review *didn't* stage is
recorded in `progress.md` → "Beyond Phase 11b" as the authoritative backlog:

- **Deferred by design:** `C5(b)` mTLS on Raft/API listeners, `L17` CRL
  enforcement (both Stage 3b deferrals), `X6` the `relish` TUI (→
  `docs/plans/tui-plan-2026-07.md`).
- **Never-staged Mediums:** M3 (blocking I/O), M4 (adjacent-only dedup), M5 (VIP
  collisions), M6 (namespace-blind service map), M7 (crash detection for
  no-health-check apps), M19 (webhook formatting), M20 (S3/GCS export), M21
  (volumes), M22 (rootless), M23 (process-workload allowlist), M24 (date math),
  X8 (logs-export checkpoint race).
- **All ~24 Low findings.**

🤖 Generated with [Claude Code](https://claude.com/claude-code)
