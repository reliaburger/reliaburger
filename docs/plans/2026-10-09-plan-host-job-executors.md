# Persistent host job executors

Implement the host throughput plan in #640 after the executor foundation in #654.
The public choices remain `runc`, `shared-runc` and `process`; all jobs retain
the common definition/run/task/attempt path.

## Implementation order

1. Matched benchmarks and bounded phase histograms: identical executable, counts,
   concurrency, resource requests/limits, node budget and service; separate cold
   setup, warmed command execution, direct ownership and durable/public completion.
2. Extend the bounded executor pool to rootful Linux host commands. Keep a durable
   owner per executor slot, a fresh child per command and live namespace credentials.
   Retain the existing owner backend on platforms without the required capability.
3. Event-driven start/exit/output/cleanup through the existing private protocol.
   Host cleanup uses owned unreaped children and pidfds, never container kill-all.
4. Amortise ownership publications over the executor lifetime and batch outcomes
   through the existing group-commit ledger. The complete slot subtree remains
   owned across Bun loss; connection loss retires it before replacement. No result
   is acknowledged before its ledger flush and no uncertain command is replayed
   without the existing explicit replay policy. A separate per-command owner WAL
   is unnecessary when the durable slot already owns every command descendant.
5. Admit explicit CPU/memory ranges for supported host executors. Launch directly
   into a task cgroup, reserve helper overhead, and enforce limits before user code.
   Other platforms continue refusing unsupported limits. Preserve allowlisting.
6. Bound slots, private directories, active metrics and journals independently of
   completed job count; qualify cleanup, worker loss, Bun restart, cancellation,
   mixed profiles and durable faults. Publish fresh matched measurements and a
   continuous hour. The separate 24-hour cluster release qualification remains
   required before claiming 100m/day.

## Safety and tests

Tests precede each change. A slot can serve another command only after the exact
sequence's cleanup receipt and an empty task cgroup. Host children may create
sessions or grandchildren; the helper is a subreaper and signals only original
owned child identities. Avoid normal-path `cgroup.kill` on Linux 6.8 because of
its previously documented clone/kill generation bug. Destroy uncertain contexts
rather than weakening cleanup. Backend routing, slot identity and the durable
owner retain their existing recovery boundary. New wire/state fields bump the
compatibility generations; pre-1.0 clusters need recreation.

## Review evidence

Run portable CI, privileged Linux executor/recovery gates and fresh optimised
measurements at concurrency 1, 3 and 27. Keep failures, manifest/build hashes,
CPU/memory/storage data, cold/warm timings, accepted counts and service probes.
Compare old/new at identical concurrency where both support the same reservation;
label unsupported legacy host limits rather than calling them matched. Real
commands and indexed accepted outcomes are required; synthetic runner rates are
not throughput evidence. Add short-command, CPU, output and mixed-profile cases.

## Dispatch follow-through

The first matched native build completed durable worker work at 9,295/s, but
100,000 public jobs reached only 992/s. Two queued chunks and separate receipt
acceptance/grant-delivery ticks limited fast workers to about 1,000/s. Learn a
bounded grant lookahead from already verified duration histograms, covering the
two control rounds and capping extra prefetch at sixteen chunks. Unknown, slow
or overflow-duration work keeps the original small window. This adds no wire or
state fields and never increases execution concurrency or reserves resources for
queued tasks. Retain the first measurements, repeat public measurements and the
hour on a fresh frozen build, and document the larger ownership/replay window.
