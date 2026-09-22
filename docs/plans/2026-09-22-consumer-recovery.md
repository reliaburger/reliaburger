# Durable consumer recovery and receipt retry

Continue C34 on `codex/codebase-completion-fixes`, PR167. Keep production cluster
activation gated until this loop and its runtime integration pass qualification.

1. Persist original catalogue, effective service entries and ingress before
   publication. A generation fence survives compaction. Bound history and receipt
   inventory; capacity refusal must not discard evidence.
2. Use explicit Publishing, Active, Withdrawing and Withdrawn phases. Before a
   replacement view publishes, remove the previous kernel/userspace view and
   cancel captured requests. Cancellation alone is not completion: wait for
   actual request guards to release. During uncertainty keep publication fenced.
   This conservative 0.1.0 path can briefly interrupt routing during catalogue
   changes; it avoids an overlapping-generation cleanup protocol.
3. Record withdrawal instructions before acting. Only confirmed withdrawal permits
   a Ready receipt. Preserve the exact instruction through failed HTTP requests,
   lost acknowledgements and restart. Retry against the current authenticated
   leader; forget a receipt only after its positive response. Compaction requires
   a prior durable Withdrawn checkpoint, and retains the latest generation fence.
4. Recover under the original enrolled node/cluster identity, before adoption or
   publication. Inspect original local and consumer kernel ownership together,
   withdraw old exposures and preserve all unconfirmed receipts. Never replay old
   health into DNS or ingress. Refuse missing/corrupt authority or changed identity.
5. Carry instructions through the assignment reconciler, send bounded authenticated
   receipts outside the agent loop and confirm their journal removal explicitly.
   Failed/cancelled publication cannot become a receipt. Qualify captured HTTP and
   WebSocket requests, repeated generations, crash/reopen, leader replacement and
   idempotent retries. Keep progress, book and handoff current after each commit.

Production activation, rootless integration and upgrade/rollback acceptance remain
separate gates after the consumer loop is implemented.
