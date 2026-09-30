# Phase 12b.6 — Documentation and book truth pass (Wave B)

Theme: `docs/progress.md` §12b.6 "Documentation and book truth pass".
Findings: D6, D14, D15, D18, D21, D22 (design-discrepancies review);
D20 is **already reconciled** (by #114 — do not redo). Source:
[2026-07-09-review-design-discrepancies.md](2026-07-09-review-design-discrepancies.md)
§§ "D13/D19", "D14/D15-D18", "D21", "D22", and the D6/D22 rows in the table.

**Prose-only.** No `src/**` changes. Touch `docs/**` and `docs/book/**` only.
This wave runs LAST so the prose matches the final 12b.6 code — which means
several findings that the review recorded as *defects* are now *fixed*, and
the docs must describe the fixed reality, not the old lie.

## The one rule that governs this whole pass

**Verify the current code before you write a word.** 12b.1–12b.6 changed a
lot since the review was written (2026-07-09). For every finding, read the
live code first, then write prose that matches what the binary does today.
Do not copy the review's "it's broken" phrasing if the code now works.

Specifically, these were FIXED in 12b.6 and the docs must reflect that:
- **GPU (D15):** `src/bun/gpu.rs` now has a real `NvidiaGpuDetector` (probes
  `/dev/nvidia0`, parses `nvidia-smi --query-gpu`, OS-guarded). `gpu_enabled`
  is effective; the supervisor **refuses** a GPU-requesting workload when GPU
  is disabled or unbacked. So: GPU *placement* is honest now; only OCI
  `/dev/nvidia*` device passthrough is still deferred. Qualify to THAT line,
  not "GPU cannot work".
- **Process workloads (D17):** `WorkloadSupervisor` is now built with the
  operator's `[process_workloads]` config; an empty allowlist is default-DENY.
  Whitepaper §15 must not imply host exec is unconstrained.
- **Self-upgrade (D20):** already reconciled in `docs/design/agent-bun.md`
  §5.5 and book chapter 14 — **leave alone**, just confirm no *other* doc
  still describes the old reporting-tree/leadership-transfer sequence.

## Findings → prose targets

### D6 / D22.3 — Userspace DNS (not in-kernel)
The whitepaper §10 (and any design prose) still says eBPF answers DNS
in-kernel with no DNS process. The implementation uses a **userspace UDP
resolver** (`src/onion/dns.rs`) and rewrites container `resolv.conf`
(`src/bin/bun.rs`). Correct §10 and `docs/design/discovery-onion.md`;
remove the in-kernel-DNS claim; keep one honest description. This is a
deliberate design choice, so explain WHY userspace (book audience learns
from the decision, not just the fact).

### D14 — Relish is a command CLI, not a no-arg TUI (X6 stays Phase 13)
Whitepaper / `docs/design/cli-relish.md` demonstrate a no-argument Ratatui
TUI + event/trace views as if shipped. Relish parses a required subcommand
today. Qualify the TUI as a Phase 13 deliverable; don't show it as a
present-tense available interface. **X6 stays in Phase 13** — do not try to
close it here.

### D15 — GPU (qualify to the FIXED state, see rule above)
Whitepaper §2 "GPU-first"/§ Bun design §5.4. Update to: real detector,
effective `gpu_enabled`, honest placement/refusal; OCI device passthrough
deferred. Not "the detector is a stub".

### D18 — Batch and distributed build are library/CLI shapes, not a pipeline
Whitepaper §6 (jobs) + `docs/design/scheduler-meat.md`. The build handler is
synchronous/local (buildah when present); full distributed batch dispatch +
completion lifecycle is a Phase 12 item. Keep the Phase-12 wording; don't
present distributed batch/build as operable.

### D21 — Recovery promises: mark as architecture proposal
Whitepaper §§8.2–8.3 promise leader reconstruction (95%/15s learning),
pre-seeded catastrophic recovery, backup/restore, disk-pressure council
step-down. Some of this landed (disk-pressure council resignation shipped in
12b.2 — VERIFY in `src/cluster/*` before you write). For what is genuinely
NOT implemented, mark §§8.2–8.3 as an **architecture proposal with a phase
reference**, not present-tense availability. Don't over-correct the parts
that now exist.

### D22 — Prose is versioned implementation docs
Three drifts to fix, plus the pattern:
1. whitepaper quick-start still names **containerd** → project uses the
   **Grill** runtime abstraction (runc/apple/process). Correct it.
2. agent design describes **bincode** Raft/reporting compatibility → payloads
   are **self-describing JSON** now (progress.md:302-310). Correct it.
3. in-kernel DNS (same as D6) — one honest description.
Add a short **"implemented in release / phase X"** status marker to design
chapters that carry superseded alternatives, and move obsolete designs into
a decision-log note rather than leaving them beside live code unlabelled
(this is a teaching book — an unlabelled old design teaches the wrong thing).

### Reconcile stale progress annotations + test counts
`docs/progress.md` and `docs/README.md` / top-level `README.md`: sweep for
stale "stub/not-wired/silently-dropped" annotations that 12b.1–12b.6 have
since resolved, and for any lingering **headline single test count** — the
harness reports a **suite taxonomy** now (#106), not one number. Don't invent
counts; describe suites.

## Constraints

- **Docs-only.** No `src/**`. If you find a code bug, note it in the PR body
  as a follow-up; do not fix it here.
- British English in prose; keep the book's voice (see CLAUDE.md style guide
  — vary sentence length, active voice, no "Notably,"/"Crucially,", sparse
  em dashes, explain Rust-vs-other-languages for first-appearance syntax).
- Check the **"Documentation and book truth pass"** box in progress.md
  (§12b.6) when done. Do NOT touch the acceptance-gate box (Wave C, the
  orchestrator runs that).
- Green gate = `make ci`. Docs-only means no Rust changes, so fmt/clippy/
  tests should pass untouched; run `make ci` anyway to confirm nothing
  regressed and the doctest/build-pdf paths are clean. Do NOT run
  `make coverage` (needs 40+ GiB free; unchanged code = unchanged coverage).
- ONE commit, publish authority, open the PR. British-English PR body ending
  with the standard trailer.
