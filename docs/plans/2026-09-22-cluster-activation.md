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
