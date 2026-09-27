# Plan: the Kairos alternative (Ubuntu base, netboot as the unified spike)

*27 September 2026. Companion to [the appliance OS research](2026-09-26-research-appliance-os.md) (draft PR #218). Docs only. **The spike itself awaits maintainer approval: do not start building.***

The maintainer asked for "an alternative recommendation that's using Kairos, with Ubuntu and netboot as the unified spike". This file tracks writing that alternative. The alternative itself goes into the research note as a new, clearly separated §7, so the comparison with mkosi and Talos stays in one place.

## Checklist

- [x] Read the whole research note and the PR (no review comments yet)
- [x] Re-check repo facts against current `main` (327 commits past the note's `0a5dfc6`; `operator_cidrs` now exists)
- [ ] Web research: Kairos framework images, `kairos-init`, supported Ubuntu versions, AuroraBoot netboot, cloud-config, persistence, upgrades, Trusted Boot, licence, community health
- [ ] Write §7 "Alternative: Kairos on Ubuntu, netboot first" in the research note
- [ ] Spike plan (marked awaiting approval) inside §7
- [ ] Recommendation: when to prefer Kairos, own mkosi, or Talos
- [ ] Update §0 summary, sources and the PR body
- [ ] Commit and push each step to `research/appliance-os`

## Constraints for whoever resumes this

- A release soak runs on the maintainer's Mac: no Lima VMs, no clusters, no local image builds.
- Don't touch `.claude/worktrees/{dl,soak,train,research}`.
- Never squash or amend; push to `research/appliance-os`; PR #218 stays a draft.
