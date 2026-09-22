# Clustered activation and final runtime qualification

Continue PR167 on the current branch, one commit per fix or feature.

1. Preserve already-durable producer release permission through restart. An exact
   ReleaseAuthorised reference must replay without a second network grant; Held
   references still require the original authenticated confirmation.
2. Use council-allocated VIPs for local clustered services. Confirm local consumer
   withdrawal and release every original runtime reference before forgetting the
   local allocation. Global VIP reuse remains governed by the replicated withdrawal
   ledger; local retirement must not delete a successor's published kernel entry.
3. Select owned runtime/discovery on supported normal clustered and rootless startup.
   Bind consumer recovery to the enrolled node and root trust fingerprint, before
   adoption, DNS/ingress and readiness. Preserve mode-change and missing-authority
   refusal. Rootful eBPF recovery and userspace-only rootless recovery have distinct
   enforcement capabilities; never claim kernel policy without its hooks.
4. Qualify real clustered startup/publication/removal/recovery, rootless Bun startup
   and helper recovery, then actual upgrade and rollback with ownership retained.
   Run focused failing-first regressions, affected platform checks and relevant
   real runtime/kernel tests. Update progress, README, book and handoff as each
   piece is committed. Keep V01–V04 open until their separate acceptance evidence.

## Checkpoint

Steps 1–4 are implemented and physically qualified for enrolled rootful Runc/eBPF
clusters and standalone rootless Runc. See `docs/progress.md` for individual commits
and evidence. Three enrolled nodes preserve workload and kernel ownership through
six controlled binary swaps and final remote cleanup. The operator has deferred
rootless clusters beyond 0.1.0. Bun now explicitly refuses them for both explicit and automatic Runc selection, including the
experimental flag. Standalone rootless forwarding remains supported; future
cluster support requires generation-bound host-port release permission. Platform
validation, managed bpffs startup and final hosted CI/builds pass at `6c64fed`; the [closure record](2026-09-22-c34-closure.md) closes C34. V01–V04
remain independent release gates.
