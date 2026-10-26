# Plans

New work starts here, as a dated plan (`YYYY-MM-DD-<topic>.md`) that says what
we'll build, in what order, and which tests prove it. When a plan's work has
shipped or been superseded, it moves to [archive/](archive/) unchanged. The
status and the order of releases are in the [roadmap](../roadmap.md), and
the detail is in the GitHub milestones and issues.

## Live

| Plan | What it is | Still open |
|---|---|---|
| [0.2.3: Two billion a day](2026-10-20-plan-0.2.3.md) | The 0.2.3 merge train: 2B host-job tasks in one real day on three nodes (profile, speed up one node, prove scale-out, run the day, publish), plus the CI, soak and test follow-ups from the CI and test efficiency plan and the 0.2.2 agents, on `release-0-2-3`. | [Milestone 0.2.3](https://github.com/reliaburger/reliaburger/milestone/17) |
| [0.2.4: Extension points](2026-10-23-plan-0.2.4.md) | The 0.2.4 merge train on `release-0-2-4`, in five waves: the book chapter first, then policies in the council (deny and warn, defaults, audit), stop hooks, sidecars and gated runtime knobs, the exec secret provider, templates and the controller contract, ending with the exit test on a real cluster. | [Milestone 0.2.4](https://github.com/reliaburger/reliaburger/milestone/19) |
| [CI and test efficiency](2026-10-11-ci-and-test-efficiency.md) | Faster CI and stronger test and soak evidence: one archive extraction per job (#1050), integration policy, test consolidation and compilation costs, with an 18 October status of what's done, tracked and proposed. | Merge queue on the current train; the proposed test, fixture and soak-evidence issues |
| [Competitive review and comparison pages](2026-10-15-competitor-comparisons.md) | Source-linked comparisons and a wider review, matching the static homepage. | Review and merge into the 0.2.1 train |
| [Research: LLM inference and training](2026-10-11-research-llm-workloads.md) | What would make Reliaburger the obvious choice for serving and training LLMs on a team's own GPUs: the projects to beat or host (vLLM, llm-d, Dynamo, GPUStack, dstack, SkyPilot, Slurm, Kubeflow, Ray), ten lessons, today's blockers, and the revised plan (0.6.0 GPUs, 0.7.0 Models, 0.8.0 Groups, 0.9.0 Pipelines, 0.10.0 Fair share, decided 11 October) with configuration sketches, implementation outlines and a landing-page demo per release. | Three open questions, then milestones and issues |
| [Research: the batch scheduler landscape](2026-10-11-research-batch-schedulers.md) | How Reliaburger compares with Slurm, HTCondor, Kueue, Volcano, KAI, Armada, Nomad, Mesos, Ray, the cloud batch services and the workflow engines: features, adoption, health and complaints, what we lack, what we leave to incumbents, and the first release proposal, since revised by the LLM follow-up. Direction decided 11 October. | The maintainer's review, then milestones and issues |
| [VM runtime research](2026-10-19-research-vm-runtime.md) | A `vm` runtime on KVM: apps switch to `runtime = "vm"` and run as Linux VMs on Cloud Hypervisor beside containers, from the same OCI image, then from disk images and ISOs. Covers the VMM choice, guest kernel and agent, networking and mirror VIPs, scheduling, security, moving VMs through the container move plan, and a Windows sketch. Agreed on 19 October: three 0.4.x releases after the AI SRE. | [0.4.3](https://github.com/reliaburger/reliaburger/milestone/20), [0.4.4](https://github.com/reliaburger/reliaburger/milestone/21), [0.4.5](https://github.com/reliaburger/reliaburger/milestone/22): #1153–#1187, #367 |
| [Extension points research](2026-10-19-research-extension-points.md) | What a Reliaburger equivalent of CRDs and admission policies should be: what Kubernetes' webhooks and CRDs cost, the lighter models (Nomad, Swarm, Fly.io, systemd, Envoy, Wasm hosts), what actually makes teams switch, five design options and a phased plan. Agreed on 19 October: policies in the council, escape hatches, an exec secret provider and templates, before 0.3.0. | [0.2.4](https://github.com/reliaburger/reliaburger/milestone/19): #1116–#1126 |
| [AI SRE research](2026-10-18-research-ai-sre.md) | Making Reliaburger the easiest orchestrator for an AI agent to operate safely: why we build our own rather than depend on Rocketgraph (BSL 1.1), the AI SRE landscape in October 2026, twelve ranked ideas and the two-part 0.4.2 milestone (read-only first, then human-approved mutations). | [0.4.2](https://github.com/reliaburger/reliaburger/milestone/18): #1082–#1094 |
| [Container migration](2026-10-09-plan-container-migration.md) | The delivery plan for moving running workloads: who it's for, decisions, ordered spikes with go/no-go rules, and nine named milestones across 0.4.0, 0.4.1 and 0.5.0 (live moves). The [research](2026-09-28-research-container-migration.md) holds the CRIU findings, continuity contract and design reference; checked against main on 9 October. | Feasibility spikes, implementation and qualification |
| [V02: sustained qualification](2026-09-25-v02-sustained.md) | The design, invariants and pass/fail thresholds of the sustained soak. `scripts/release/qualify-sustained.sh`, `sustained_check.py` and the [release runbook](../releasing.md) run every release against it. | Snapshot uploader under power cuts and the `v02-loops` bounds ([#287](https://github.com/reliaburger/reliaburger/issues/287)) |
| [Review: the agent loop](2026-09-30-agent-loop-review.md) | A review brief on Bun's `run_loop`: how it works, every soak failure it caused, what's still inline, and three options for restructuring it. For discussion. | The maintainer's decision on the recommendation (keep the loop, add a turn meter and a starvation harness) and its six open questions |
| [F03c: keyless cosign and Sigstore bundles](2026-10-05-plan-f03c-keyless-cosign.md) | Fulcio certificates, identity rules and Rekor proofs for keyless signatures, in the classic `.sig` layout and as Sigstore bundles stored as OCI referrers, with a pinned Sigstore trusted root. Planned for a later release. | Everything, after the crate spike (K0) and the six open questions ([#619](https://github.com/reliaburger/reliaburger/issues/619)) |
| [F04: CA recovery and rotation](2026-10-02-plan-f04-ca-recovery-and-rotation.md) | Several CAs per role, trust bundles in every verifier, an operator-held root backup, intermediate then root rotation, restore into an isolated cluster, and a stated grace policy. R0–R4 shipped by 0.1.6. | R5–R7 ([#362](https://github.com/reliaburger/reliaburger/issues/362)) |
| [F05: identity and tokens](2026-10-02-plan-f05-identity-and-tokens.md) | Audit attribution, token visibility and safe expiry, rotation and a default lifetime, per-namespace secret keys, per-app JWT audiences, and a decryption audit. I1–I4 shipped by 0.1.6. | I5–I6 ([#363](https://github.com/reliaburger/reliaburger/issues/363)) |
| [CI feedback loop](2026-09-23-ci-feedback-loop.md) | The CI layout and flake work of 23 September. Everything but one item has shipped. | C6.4: whether entry-node fault pre-checks should read the leader's view instead of their own ([#334](https://github.com/reliaburger/reliaburger/issues/334)) |

Plans for later releases arrive with their own pull requests. The
[roadmap](../roadmap.md) lists what comes next.

## History

[archive/](archive/) holds the plans and reviews that led to 0.1.0, and the
finished plans since. The
[0.1.0 release closure record](../qualification/2026-09-27-v0.1.0-release-closure.md)
is the summary of how 0.1.0 ended.

- [0.2.2: UX patch](archive/2026-10-16-plan-0.2.2-ux-patch.md): the 0.2.2 merge train, the CLI review, Kubernetes fidelity, chaos for jobs, demos and doc fixes, shipped in 0.2.2
- [Soak charts](archive/2026-10-17-soak-charts.md): SVG charts in every soak record, and the released version's on the homepage (#979)
- [Equal-window job demonstrations and hourly saturation runs](archive/2026-10-09-plan-timed-job-scenarios.md)
- [A million jobs, the September design](archive/2026-09-28-plan-million-jobs.md): superseded by the October plans; its M8 targets are recorded, not gated, by every job qualification run (D18-C)
- [Cluster migration prerequisites](archive/2026-10-04-cluster-migration-prerequisites.md): superseded in 0.2.0 when leased applies moved to the common job lifecycle and the prerequisite claims were retired (#681)
- [Batch capacity reservations](archive/2026-10-04-batch-capacity-reservations.md): superseded in 0.2.0 when the leader's batch reservations were retired and app placement started counting job leases (#681)
- [Job retirement and record-read races](archive/2026-10-10-job-retirement-read-races.md): retained cleanup across timeouts and single-inspection record reads (#654)
- [Jobs in the release soak](archive/2026-10-10-plan-release-job-soak.md): all job runtimes in the V02 soak and the job qualification tiers of the 0.2.0 exit gate (#686, #668)
- [Persistent host job executors](archive/2026-10-09-plan-host-job-executors.md): bounded native executors and their measurements (#640)
- [Reusable containers and throughput](archive/2026-10-07-plan-reusable-executors-and-throughput.md): built-in command executors and the sustained throughput demo (#639, #640)
- [Common job lifecycle](archive/2026-10-07-plan-common-job-lifecycle.md): one definition/run/task/attempt path for singleton jobs, batches, cron and hooks, shipped in 0.2.0 (#638)
- [Resource-aware delegated jobs](archive/2026-10-04-plan-delegated-jobs.md): resource budgets, mixed profiles, durable outcomes and the homepage demo, shipped in 0.2.0
- [GitOps webhook admission](archive/2026-10-04-gitops-webhook-admission.md) (#553)
- [GitOps job refusal](archive/2026-10-04-gitops-job-refusal.md) (#549)
- [Shared configuration tree](archive/2026-10-04-shared-config-tree.md) (#548)
- [Desired-spec previews](archive/2026-10-04-desired-spec-preview.md) (#550)
- [Live namespace validation](archive/2026-10-04-live-namespace-validation.md) (#551)
- [Review 2: audit fixes](archive/2026-10-03-review2-audit-fixes.md): the 28-issue audit, #528–#555, shipped in 0.1.5
- [Autoscale target validation](archive/2026-10-03-autoscale-target-validation.md) (#554)
- [Batch execution identities](archive/2026-10-03-batch-execution-identities.md) (#535)
- [Physical image storage quota](archive/2026-10-03-plan-physical-image-quota.md) (#540)
- [Registry verification admission](archive/2026-10-03-plan-registry-verification.md) (#539)
- [Blob repository authority](archive/2026-10-03-plan-blob-repository-authority.md) (#531)
- [Build signing lifetime](archive/2026-10-03-plan-build-signing-lifetime.md) (#529)
- [Managed volume image identities](archive/2026-10-03-managed-volume-image-identities.md) (#528)
- [Batch duplicate identities](archive/2026-10-03-batch-duplicate-identities.md) (#542)
- [Batch workload admission](archive/2026-10-03-batch-workload-admission.md) (#530)
- [Bound cron arithmetic](archive/2026-10-03-cron-step-overflow.md) (#541)
- [F03a: upstream image trust](archive/2026-10-02-plan-f03-upstream-image-trust.md): digest binding, the `node.toml` image policy and key-based cosign, shipped by 0.1.6; keyless signatures continue as F03c
- [F07: cross-node views and log streams](archive/2026-10-01-plan-f07-cross-node-views.md): every view and stream covers the cluster (#365)
- [Demo image build](archive/2026-09-27-plan-demo-image-build.md): the Go demo app, multi-arch export and Buildah pruning (#248)
- [Task arrays and a 100,000-job tour step](archive/2026-09-25-plan-task-arrays.md): superseded by the September million-jobs design
