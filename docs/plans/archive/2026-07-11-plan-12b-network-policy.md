# Phase 12b.1 — Network policy enforcement (remainder)

Theme: `docs/progress.md` §12b.1 "Network policy enforcement".
Findings: NET7, NET8, the network slice of D5, and the old Lows "Missing BPF
maps can panic" + "nftables CIDR interpolation". Also the two theme-text items
NET6 left open: egress programmed *before* process start, and reconciling
kernel truth against live instances.
Source review: [2026-07-10-review-past-phase-12.md](2026-07-10-review-past-phase-12.md).

Already landed (do not redo): NET5 (namespace-firewall maps written and
reconciled on deploy/redeploy/restart/stop) and NET6 (egress on rolling
redeploy and crash-restart via the shared `finish_instance_networking` helper
in `src/bun/agent.rs`, failing closed, with stop/redeploy deleting allow
entries).

Out of scope (12b.4 "Global namespaced service catalogue and DNS"): ServiceId
namespacing, VIP collision handling, DNS bind address/TCP/ACLs, the portable
non-eBPF VIP path (H3/codex-M1 and the rest of D5).

## Ground truth

- NET7: the policy hook is `cgroup/connect4` only. `parse_egress_entry`
  (`src/sesame/egress.rs:62`) resolves to `(Ipv4Addr, u16)` and explicitly
  errors on CIDR (`:83` "CIDR notation not yet supported"); IPv6 addresses are
  discarded at parse. A dual-stack workload bypasses the entire allowlist by
  connecting over IPv6.
- NET8 (BPF half): `src/onion/ebpf/maps.rs` calls
  `ebpf.bpf.map_mut("backend_map").unwrap()` (lines 31/83/108/132/274 and the
  same pattern for other maps) — a `.bpf.o` missing a map panics Bun; several
  update/remove results are discarded.
- NET8 (nftables half): `src/firewall/rules.rs` interpolates administrator
  CIDR strings into the `nft -f` script without validation; the perimeter
  table is IPv4 (`ip`) only, so every drop rule is void over IPv6; node-IP
  trust applies to all host services; `nft` is invoked with no timeout.
- NET6 residue: `finish_instance_networking` runs after the process starts
  (it needs the PID to find the cgroup id), so there is a window where a
  freshly started workload has no egress entries and the connect hook, seeing
  no `egress_enforced` flag, allows everything.
- eBPF C programs live under the `ebpf` feature (built via `build.rs` into
  `RELIABURGER_BPF_DIR`); locate the connect4 program and mirror it. The
  9-test `tests/ebpf.rs` suite runs in the `reliaburger-test` Lima VM.

## Implementation steps (tests first for each)

### 1. NET7 — connect6 + IPv6 egress

- Add a `cgroup/connect6` eBPF program mirroring connect4: same
  `egress_enforced` gate, an IPv6 allow map (key: cgroup id + 16-byte address
  + port; `#[repr(C)]` with explicit padding per CLAUDE.md), same deny
  semantics, and the connect-rewrite/backend behaviour left untouched unless
  the service map already promises it for v6 (it does not — VIPs stay v4;
  only *policy* goes dual-stack here).
- `parse_egress_entry`/`resolve_egress_entries` return both v4 and v6
  targets (DNS resolution keeps AAAA records instead of discarding them);
  `egress_diff`/`egress_to_bpf_entries`/`re_resolve_egress_async` and the
  agent's programming path carry both families.
- Attach connect6 wherever connect4 attaches; if the kernel refuses (old
  kernel), the existing "log and continue without enforcement" load policy
  applies — but when an app *has* an egress allowlist and enforcement is
  active, missing v6 coverage must fail the deploy closed (an allowlist you
  can trivially bypass over v6 is not enforced).

### 2. NET7 — CIDR enforcement

- Accept `cidr:port` entries (v4 and v6) in `parse_egress_entry`.
- Exact-match hash maps cannot answer CIDR membership: add an
  `LPM_TRIE`-backed allow map per family (aya `LpmTrie`), keyed by
  cgroup id + prefix; the connect hook checks exact map first, then LPM.
- Userspace: validate prefix lengths (0–32 / 0–128), normalise the network
  address (reject `10.1.2.3/8` or normalise it — pick one, document, test).

### 3. NET8 — no panics, no discarded errors, required-map validation

- Replace every `map_mut(...).unwrap()` in `src/onion/ebpf/maps.rs` (and any
  sibling) with a typed `MissingMap` error; propagate update/remove results.
- At load time (`OnionEbpf` loader), validate every required map and program
  exists in the loaded object and fail the load with a clear list of what is
  missing — one place, instead of scattered panics later.
- Callers (agent networking helper) already fail closed on programming
  errors thanks to NET6 — make sure the new error paths flow into that.

### 4. NET8 — safe nftables input, dual-stack perimeter, timeouts

- Parse administrator-supplied CIDRs/addresses into `ipnet`-style types (or
  hand-rolled parse onto `IpAddr`+prefix — avoid a new dependency if in-tree
  parsing exists) *before* rendering the ruleset; rendering only ever
  interpolates re-serialised, validated values. A config value like
  `10.0.0.0/8; drop` must be a parse error, pinned by test.
- Perimeter rules rendered for both `ip` and `ip6` tables (same policy,
  family-appropriate addresses; a v6 management CIDR lands only in `ip6`).
- Every `nft` invocation gets a `tokio::time::timeout` (bounded, config or
  constant ~10s) and a clear error naming the command on expiry.
- Keep the C4 lesson: perimeter stays in its own `reliaburger_fw` table;
  flush-before-apply semantics unchanged.

### 5. Close the start-window: egress before process start

For runc (the enforcement runtime), split create/start: `runc create` gives a
created-but-not-running container whose cgroup exists → resolve the cgroup id,
program egress + namespace maps, then `runc start`. On any programming error,
delete the created container (fail closed, no half-started workload). For
ProcessGrill/Apple (no eBPF enforcement there anyway), document the ordering
gap explicitly in the book/design doc instead of pretending. Wire this through
`finish_instance_networking`'s call sites in `src/bun/agent.rs` — this theme
owns that file now (the sibling wave-1 themes are done), but keep the diff
tight around deploy/redeploy/restart.

### 6. Reconcile kernel truth

A periodic (and post-restart) sweep: enumerate live instances' cgroup ids,
list `egress_enforced`/allow/namespace map keys, delete entries whose cgroup
id no longer maps to a live instance, and (re)install entries a live instance
should have but lost (e.g. after a Bun restart with adopted workloads). Reuse
the existing adoption inventory. Log what was added/removed; a no-op sweep is
silent. Interval config under `[ebpf]` with a sane default (60s).

## Tests

- Pure/userspace (default suite, macOS-safe): parse v6 + CIDR entries
  (valid, invalid prefix, non-normalised, whitespace); diff over mixed
  families; nft ruleset rendering rejects malformed CIDRs and renders both
  families (snapshot/insta if already used for rulesets); loader required-map
  validation errors against a stub object; sweep planning as a pure function
  (live set vs kernel set → add/remove plan).
- Lima-gated (`tests/ebpf.rs`, run in the `reliaburger-test` VM if
  `limactl list` shows the rig; otherwise report unverified): connect6 deny
  without entry / allow with entry; v4 CIDR allow via LPM; v6 bypass test —
  the old behaviour (v4 allowlist, v6 connect succeeds) must now deny;
  runc create→program→start ordering (no allow-window: assert a connect
  attempted before `start` completes is denied); sweep removes an orphaned
  cgroup entry.
- Fail-closed integration (default suite with the mock grill): programming
  error during the pre-start phase fails the deploy and leaves no running
  process.

## Acceptance

- `make ci` green (fmt, clippy `-D warnings`, full default suite).
- Lima eBPF suite run if the rig is available; every skipped Linux-only test
  named in the final report.
- `docs/progress.md`: nested `- [x]` items added under the theme, theme box
  checked (this completes it).
- Book: chapter 3 (`docs/book/03-talking-to-each-other.md`) eBPF/firewall
  sections + chapter 10 where egress policy is described: the v6-bypass
  lesson (a v4-only allowlist is a suggestion, not a policy), LPM tries for
  CIDR, and the create→program→start ordering. Update
  `docs/design/security-sesame.md` if it still describes the old ordering.
  British English, style guide in CLAUDE.md, explain new Rust syntax on
  first use (e.g. `LpmTrie`, `#[repr(C)]` padding).

## Constraints

- This theme owns `src/bun/agent.rs` for wave 2 — but the secrets/identity
  theme lands right after it, so keep agent.rs changes localised to the
  networking helper and grill create/start call sites.
- New dependencies: avoid; aya is in-tree, prefer std `Ipv6Addr` parsing.
  If LPM support genuinely needs an aya version bump, flag it in the report
  before committing to it.
- Error messages lowercase, no trailing full stop; thiserror in library code;
  `// SAFETY:` on any unsafe map casts.
