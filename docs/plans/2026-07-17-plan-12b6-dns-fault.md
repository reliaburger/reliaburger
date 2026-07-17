# Phase 12b.6 (gate follow-up) — DnsNxdomain fault against the userspace resolver

Found by the Phase 12b acceptance gate: the Smoker `DnsNxdomain` chaos fault
is a **silent no-op**. It writes to `fault_dns_map`, which exists only in the
never-loaded `onion_dns.bpf.o` (the agent loads `onion_connect.bpf.o`, which
has no such map), and the live **userspace** resolver
(`src/onion/dns.rs::answer_internal`) resolves purely from the service
catalogue and never checks fault state. So an operator injecting `DnsNxdomain`
sees no effect on any configuration. This is exactly the "advertised fault
that does nothing" that 12b.6 CHAOS1 set out to eliminate; DNS slipped through
because CHAOS1 treated the eBPF network faults as "already live" while DNS had
moved to userspace.

## Goal

Make `DnsNxdomain` genuinely effective against the userspace resolver, with a
portable test proving both the effect and its reversal — matching the CHAOS1
reversal model the rest of the Smoker faults now use.

## Evidence (verified by the gate trace)

- Fault variant: `src/smoker/types.rs:58` (`FaultType::DnsNxdomain`), marked
  `requires_ebpf()` at `:206`.
- Apply path: `src/bun/agent.rs:3329-3342` writes `BpfDnsFaultValue` to
  `fault_dns_map` via `smoker::bpf_maps::write_dns_fault`
  (`src/smoker/bpf_maps.rs:64-89`).
- Loaded object is `onion_connect.bpf.o` (`src/onion/ebpf/loader.rs:66`);
  `fault_dns_map` is NOT in its required-maps list (`:14-24`) and NOT defined
  in `ebpf/onion_connect.bpf.c`. It lives only in `ebpf/onion_dns.bpf.c:39-45`,
  which is never loaded.
- Userspace resolver: `src/onion/dns.rs::answer_internal` (~:303-325) has zero
  fault awareness; `(None, _) => NXDOMAIN` is the only NXDOMAIN path.

## Approach (userspace, honest, deterministic)

1. **Fault state channel.** Mirror the existing service-map plumbing: give the
   DNS resolver a `watch::Receiver<DnsFaultState>` (or equivalent shared,
   cheaply-cloned read handle) carrying the set of currently-faulted service
   names with their expiry. The apply/clear/expire paths in `agent.rs` that
   own the Smoker fault registry publish to it. Reuse the CHAOS1 reversal hooks
   (`record_reversal` / expiry in `expire_faults` and both clear paths) so a
   `DnsNxdomain` fault is removed on clear/expiry exactly like the resource
   faults.
2. **Resolver checks it.** In `answer_internal`, after ACL and before/against
   the normal service lookup: if the queried service has an active
   (non-expired) `DnsNxdomain` fault, return `RCODE_NXDOMAIN` regardless of
   whether the service resolves. Keep it simple and honour the fault's
   `probability` only if trivial; a 100%-probability NXDOMAIN is the contract
   operators expect. Match the service-name/namespace keying the resolver
   already uses (`service_id_for` / `target_service`).
3. **`requires_ebpf()`.** Reconsider for `DnsNxdomain`: DNS is userspace now.
   Either flip it to false (it's a userspace-resolver fault) or keep it honest
   with the `dns.enabled ⇒ ebpf.enabled` config rule — but the fault MUST take
   effect whenever the resolver runs, not depend on the unloaded DNS eBPF
   object. Decide and document the choice.
4. **Retire the dead path** if it's cleanly removable: the write to the
   never-loaded `fault_dns_map` (`agent.rs:3329-3342`, `bpf_maps::write_dns_fault`,
   the `onion_dns.bpf.c` map) is dead. Remove it, or leave a clearly-labelled
   `// TODO(Phase N):` if a future in-kernel DNS path is intended. Do not leave
   two competing mechanisms unlabelled.

## Tests (write first, portable, no eBPF)

- `dns_nxdomain_fault_forces_nxdomain_for_the_targeted_service`: a service that
  normally resolves to a VIP returns `RCODE_NXDOMAIN` while the fault is
  active; a *different* service still resolves. Drive `answer_internal` (or the
  resolver's public entry) directly with a fault-state handle — no sleeps,
  observable state only.
- `dns_nxdomain_fault_is_reversed_on_clear_and_expiry`: after clear (and after
  expiry) the service resolves to its VIP again.
- Keep everything in the portable default suite. Any eBPF/Linux-only bits stay
  `#[ignore]` under the existing `make test-*` filters.

## Seams / constraints

- Own: `src/onion/dns.rs`, the DNS-fault apply/clear/expire region in
  `src/bun/agent.rs`, `src/smoker/{types,bpf_maps}.rs` as needed, and the
  resolver task wiring in `src/bin/bun.rs` (pass the fault-state handle in).
- Do NOT touch unrelated fault types or the resource-fault cgroup code from
  #113 beyond reusing its reversal hooks.
- Book: chapter 8 "Breaking Things on Purpose" — update the DNS-fault
  description to the userspace mechanism (explain WHY userspace, teaching
  voice). `docs/design/chaos-smoker.md` + the DnsNxdomain note in
  `docs/progress.md` (and remove any "eBPF DNS fault" claim that is now false).
- British English; thiserror; no unwrap in production; lowercase error
  messages; green gate = `make ci` (run the components; `clippy --all-features`
  is Linux-CI-only on macOS). Do NOT run `make coverage` (disk < 40 GiB).
- Cover the new code so the 78.65 coverage floor holds (the two portable tests
  above exercise the resolver path directly).
- ONE commit, publish authority, open the PR. Standard trailers.
