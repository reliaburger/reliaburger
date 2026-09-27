# Plan: appliance spike (mkosi on Ubuntu, relish netboot, Wyse 3040 finale)

*27 September 2026. Companion to [the appliance OS research](2026-09-26-research-appliance-os.md) (draft PR #218). Docs only. **Awaiting maintainer approval: do not start building.***

This file replaces `2026-09-27-plan-kairos-spike.md`. The maintainer chose the own mkosi image on Ubuntu with a netboot server built into `relish`, so the Kairos spike is gone and Kairos becomes an alternative that was considered.

## Write-up checklist (docs only, in progress)

- [x] Rename this plan from the Kairos spike
- [ ] Research note: new recommendation (mkosi + Ubuntu kernel + `relish` netboot)
- [ ] Research note: shrink Kairos to "considered, not chosen"; drop Talos spike and plan items, keep a one-paragraph note
- [ ] Research note: weekly appliance builds in GitHub Actions and the node update path
- [ ] Research note: netbooting VMs on a Mac (Lima vs raw QEMU, L2 networks, aarch64 vs x86_64)
- [ ] Research note: Dell Wyse 3040 constraints
- [ ] This file: the spike stages
- [ ] PR #218 title and body

## Constraints for whoever resumes this

- A release soak runs on the maintainer's Mac: no Lima VMs, no clusters, no local image builds.
- Don't touch `.claude/worktrees/{dl,soak,train,research}`.
- Never squash or amend; push to `research/appliance-os`; PR #218 stays a draft.
