# Plans

New work starts here, as a dated plan (`YYYY-MM-DD-<topic>.md`) that says what
we'll build, in what order, and which tests prove it. When a plan's work has
shipped or been superseded, it moves to [archive/](archive/) unchanged. The
status and the order of releases are in the [roadmap](../roadmap.md), and
the detail is in the GitHub milestones and issues.

## Live

| Plan | What it is | Still open |
|---|---|---|
| [Review 2: audit fixes](2026-10-03-review2-audit-fixes.md) | The approved 28-issue audit, implemented through one merge train with cross-path hardening last. | [#528–#555](https://github.com/reliaburger/reliaburger/milestone/11) |
| [V02: sustained qualification](2026-09-25-v02-sustained.md) | The design, invariants and pass/fail thresholds of the sustained soak. `scripts/release/qualify-sustained.sh`, `sustained_check.py` and the [release runbook](../releasing.md) run every release against it. | Snapshot uploader under power cuts and the `v02-loops` bounds ([#287](https://github.com/reliaburger/reliaburger/issues/287)) |
| [Review: the agent loop](2026-09-30-agent-loop-review.md) | A review brief on Bun's `run_loop`: how it works, every soak failure it caused, what's still inline, and three options for restructuring it. For discussion. | The maintainer's decision on the recommendation (keep the loop, add a turn meter and a starvation harness) and its six open questions |
| [F07: cross-node views and log streams](2026-10-01-plan-f07-cross-node-views.md) | Which views and streams still answered for one node, and the split of [#365](https://github.com/reliaburger/reliaburger/issues/365): part 1 (every view and stream covers the cluster) ships in 0.1.3. | Part 2: separate stderr capture, `--until`/`--instance`/regex filters, a merged live event stream and a live-metrics contract |
| [F03a: upstream image trust](2026-10-02-plan-f03-upstream-image-trust.md) | Binding every image to a digest at apply, a `node.toml` policy for images outside Pickle, and key-based cosign verification ([#361](https://github.com/reliaburger/reliaburger/issues/361)). The tag-and-digest parser fix and `bind_image` shipped in 0.1.4; the rest is 0.1.5. Worker key separation (F03b) follows in its own plan. | U1 wiring, U2–U4 (decisions recorded in the plan, 2 Oct) |
| [F04: CA recovery and rotation](2026-10-02-plan-f04-ca-recovery-and-rotation.md) | Several CAs per role, trust bundles in every verifier, an operator-held root backup, intermediate then root rotation, restore into an isolated cluster, and a stated grace policy ([#362](https://github.com/reliaburger/reliaburger/issues/362)). R0 shipped in 0.1.4 (#472). | R1–R4 for 0.1.5, R5–R7 later (decisions recorded in the plan, 2 Oct) |
| [F05: identity and tokens](2026-10-02-plan-f05-identity-and-tokens.md) | Audit attribution, token visibility and safe expiry, rotation and a default lifetime, per-namespace secret keys, per-app JWT audiences, and a decryption audit ([#363](https://github.com/reliaburger/reliaburger/issues/363)). I1 shipped in 0.1.4 (#473); I2 is on the 0.1.5 train (#483). | I3–I4 for 0.1.5, I5–I6 later (decisions recorded in the plan, 2 Oct) |
| [CI feedback loop](2026-09-23-ci-feedback-loop.md) | The CI layout and flake work of 23 September. Everything but one item has shipped. | C6.4: whether entry-node fault pre-checks should read the leader's view instead of their own |

Plans for later releases arrive with their pull requests: the Go demo build
for 0.1.2 ([#248](https://github.com/reliaburger/reliaburger/pull/248)), a
million jobs for 0.2.0 ([#266](https://github.com/reliaburger/reliaburger/pull/266)),
the appliance for 0.3.0 ([#218](https://github.com/reliaburger/reliaburger/pull/218),
[#259](https://github.com/reliaburger/reliaburger/pull/259)), container
migration for 0.4.0 ([#268](https://github.com/reliaburger/reliaburger/pull/268))
and the fleet control plane ([#265](https://github.com/reliaburger/reliaburger/pull/265)).

## History

[archive/](archive/) holds the plans and reviews that led to 0.1.0. The
[0.1.0 release closure record](../qualification/2026-09-27-v0.1.0-release-closure.md)
is the summary of how that ended.
