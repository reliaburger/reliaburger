# Review 2: audit fixes for 0.1.5

The maintainer approved the [codebase audit](../qualification/2026-10-03-codebase-audit/README.md) for implementation. Its 28 issues are in [milestone 0.1.5](https://github.com/reliaburger/reliaburger/milestone/11). The main weakness is **consistency across paths**. Repair the concrete failures before consolidating their shared contracts.

## Integration order

Branch `release-0-1-5-review2` starts at remote `release-1-1-5` commit `4f841ede`. Each issue has one child PR into this branch, with its issue-closing keyword. Merge a child only after its required CI succeeds. The outer train stays open for the maintainer's combined review. Existing release qualification and flake work remain on the parent release train. No agent tags or promotes a release.

Work in parallel across runtime/scheduling, registry/signing, networking/observability, and configuration/CLI/GitOps. Dependencies within each subsystem are implemented in order. After issues #528–#554 are integrated, implement #555 as the last child PR: shared admission and configuration contracts, identity and lifecycle boundaries, deterministic seams, and checked qualification evidence. Track the concrete child PRs on the outer PR rather than duplicating a checklist here.

## Proof of correctness

For each defect, add a regression that asserts the expected behaviour and observe it fail before fixing the production path. Keep portable tests in the existing suite. Use controlled clocks, explicit completion acknowledgements and injected failures where the behaviour needs them. Update the subsystem's existing book chapter in the same PR.

Run `make ci` and the matching gated targets before committing; record host limitations explicitly. Verify Linux-only storage and eBPF behaviour in CI or applicable Linux qualification. Preserve the coverage floor and zero retry policy. Any incompatible wire or durable-state change advances the relevant compatibility counter together with its tests; there are no migrations before 1.0.

Recheck every child diff and its acceptance criteria before merging. Qualify the completed integration branch against the parent release's latest flake fixes and include final CI results and remaining platform qualification requirements in the handoff. GitHub closes referenced issues when the release ultimately reaches its default branch; merging a child into a non-default integration branch does not itself close them.
