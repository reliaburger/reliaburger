# Research: a fleet control plane for Reliaburger appliances

*Research note, 28 September 2026. Docs only, no product code. Builds on the appliance OS research ([`2026-09-26-research-appliance-os.md`](2026-09-26-research-appliance-os.md), draft PR #218) and the S1 image work (`feat/appliance-image`, draft PR #259). Repo facts come from `main` at `087d882f`.*

> **Status: in progress. Awaiting maintainer review once complete.** Branch `research/fleet-control-plane`, based on `research/appliance-os`.

## Progress checklist (for whoever resumes this)

- [x] Read the appliance research, the spike plan, the S1 image plan and the current join/decommission code
- [ ] Prior art: Omni (machines, classes, templates, SideroLink, Image Factory)
- [ ] Prior art: Metal³/Ironic, MAAS, Tinkerbell, Foreman (ideas and anti-patterns)
- [ ] Machine lifecycle state machine
- [ ] Identity and trust model
- [ ] Discovery
- [ ] Control-plane state and HA
- [ ] Allocation policies
- [ ] Wipe semantics
- [ ] Moving a node between clusters
- [ ] OS and bun upgrades across clusters
- [ ] Networking (overlay or not)
- [ ] CLI and UI
- [ ] Packaging decision: separate repo, integrated, or hybrid
- [ ] Effort, risks, open questions, phased plan
- [ ] Draft PR opened
