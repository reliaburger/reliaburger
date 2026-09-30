# Reliaburger Roadmap

This page is the high-level plan and status. It stays short on purpose: each
release gets a line or two, its GitHub milestone and the issues or pull
requests that carry the detail. The work itself is tracked in the
[milestones](https://github.com/reliaburger/reliaburger/milestones) and
[issues](https://github.com/reliaburger/reliaburger/issues). Each piece of
work starts with a dated plan in [plans/](plans/), and releases follow the
[release runbook](releasing.md). For the architecture, see the
[whitepaper](whitepaper.md) and the [design documents](design/).

The phase-by-phase roadmap that got us to 0.1.0, with every phase's test-first
detail, is [frozen in the archive](plans/archive/roadmap-to-0.1.0.md), and so
is the [implementation checklist](plans/archive/progress-to-0.1.0.md) that went
with it.

## Releases after 0.1.0

Patch releases (0.1.x) fix and harden without a headline. Each minor release
after that ships one headline feature, with a single exit test that proves it.
Until 1.0.0 every release is a development release: an incompatible format
change bumps the compatibility generation and needs a fresh cluster, with no
migration and no feature gate
([compatibility before 1.0.0](releasing.md#compatibility-before-100)).

- [x] **0.1.0: the first release.** Released on 29 September 2026: signed
  binaries, a managed three-node laptop cluster from one `curl | sh`, and the
  platform built in phases 1–16 below. The
  [release closure record](qualification/2026-09-27-v0.1.0-release-closure.md)
  lists every candidate, soak run and fix from #196 to the tag, and what was
  carried past it. The limits it ships with are in the
  [documentation](README.md#010-scope-and-limits).
- [ ] **0.1.1: every known bug fix**
  ([milestone](https://github.com/reliaburger/reliaburger/milestone/1)). A full
  release is expensive, so 0.1.1 takes every bug we know about:
  - [x] the soak follow-ups in [#276](https://github.com/reliaburger/reliaburger/pull/276)
    (departed nodes forgotten, dead nodes listed as dead, the commit in
    versions, the candidate digest in the build log);
  - [x] early-user fixes from [#241](https://github.com/reliaburger/reliaburger/issues/241):
    [#281](https://github.com/reliaburger/reliaburger/issues/281)–[#284](https://github.com/reliaburger/reliaburger/issues/284);
  - [x] UI polish ([#280](https://github.com/reliaburger/reliaburger/issues/280))
    and the two flakes from #277's CI ([#285](https://github.com/reliaburger/reliaburger/issues/285));
  - [ ] the static review's defects ([#258](https://github.com/reliaburger/reliaburger/pull/258)):
    snapshots ([#291](https://github.com/reliaburger/reliaburger/issues/291)–[#294](https://github.com/reliaburger/reliaburger/issues/294)),
    GitOps ([#295](https://github.com/reliaburger/reliaburger/issues/295)–[#297](https://github.com/reliaburger/reliaburger/issues/297),
    [#305](https://github.com/reliaburger/reliaburger/issues/305)), the
    permission matrix for read routes ([#298](https://github.com/reliaburger/reliaburger/issues/298)),
    autoscaler `min = 0` ([#299](https://github.com/reliaburger/reliaburger/issues/299))
    and docs that contradict 0.1.0 ([#300](https://github.com/reliaburger/reliaburger/issues/300));
  - [ ] tests that can't fail ([#301](https://github.com/reliaburger/reliaburger/issues/301),
    [#302](https://github.com/reliaburger/reliaburger/issues/302)) and the
    open flake ([#318](https://github.com/reliaburger/reliaburger/issues/318));
  - [ ] further known bugs: a changed ingress host ignored on redeploy
    ([#307](https://github.com/reliaburger/reliaburger/issues/307)), log
    forwarders not resuming from the checkpoint
    ([#308](https://github.com/reliaburger/reliaburger/issues/308)), the
    allocation error after a starved deploy
    ([#309](https://github.com/reliaburger/reliaburger/issues/309)), Mayo
    memory growth over a long soak ([#310](https://github.com/reliaburger/reliaburger/issues/310)),
    and the rest of the milestone;
  - [ ] housekeeping: the plans that led to 0.1.0 archived, and this page
    replacing the old implementation checklist.
- [ ] **0.1.2: images**
  ([milestone](https://github.com/reliaburger/reliaburger/milestone/2),
  [#248](https://github.com/reliaburger/reliaburger/pull/248)). The Go demo
  build in the tour, multi-arch builds and pulls (which removes 0.1.0's
  multi-platform limitation), platforms in `relish images`, a build cache cap,
  Buildah storage pruning and Buildah in the guest image.
- [ ] **0.1.3: observability and test quality**
  ([milestone](https://github.com/reliaburger/reliaburger/milestone/4)).
  Streamed metrics queries, so Mayo and rollup sessions no longer load every
  Parquet file; an owner for every ignored test and tested CI job selection
  ([#303](https://github.com/reliaburger/reliaburger/issues/303)); JUnit
  evidence for every suite ([#304](https://github.com/reliaburger/reliaburger/issues/304)).
- [ ] **0.2.0: "A million jobs"**
  ([milestone](https://github.com/reliaburger/reliaburger/milestone/3),
  [#266](https://github.com/reliaburger/reliaburger/pull/266)). Task arrays
  keep compact state in Raft and expand on each node, and finish a million
  tasks in minutes on three nodes.
- [ ] **0.3.0: "Bare metal in an hour"**
  ([milestone](https://github.com/reliaburger/reliaburger/milestone/6),
  research and spikes in [#218](https://github.com/reliaburger/reliaburger/pull/218)
  and [#259](https://github.com/reliaburger/reliaburger/pull/259)). The
  appliance OS (our own mkosi image on Ubuntu 26.04), `relish netboot` and
  claiming machines over the LAN, weekly signed OS builds with A/B updates,
  and the quickstart guest on Ubuntu 26.04. The claim flow is designed so the
  fleet control plane can extend it later. Exit test: ten Dell Wyse 3040s from
  power-on to a cluster.
- [ ] **0.4.0: "Full container migration"**
  ([milestone](https://github.com/reliaburger/reliaburger/milestone/7),
  [#268](https://github.com/reliaburger/reliaburger/pull/268)). Drain and
  uncordon, cold moves that carry volumes, CRIU checkpoint and restore,
  pre-dump iterations, lazy pages and TCP handoff, for apps and jobs.
- [ ] **After 0.4.0: the book edit and the full CLI tour.** Distil "Building
  Reliaburger" so it reads properly from beginning to end: cut the fluff, keep
  the design narrative, the first-use Rust explanations and the lessons, and
  fold material that was bolted onto earlier chapters later back into place
  ([#311](https://github.com/reliaburger/reliaburger/issues/311)). Alongside
  it, the website gets a full CLI walkthrough (a second recorded tour that
  exercises every `relish` command) and a searchable command reference on the
  landing page that ctrl-F can find, generated from the CLI
  ([#315](https://github.com/reliaburger/reliaburger/issues/315)).
- [ ] **Later.** The fleet control plane
  ([#265](https://github.com/reliaburger/reliaburger/pull/265)), and GPU
  scheduling and warm starts (F01 in [#320](https://github.com/reliaburger/reliaburger/issues/320)).
  The rest of the known gaps below aren't headline material; they land
  alongside whichever release they fit.

## Known gaps

What 0.1.0 shipped without, each with an issue:

- **Acceptance gates 0.1.0 didn't close.** V01, the live catalogue on three
  independent runc nodes ([#286](https://github.com/reliaburger/reliaburger/issues/286));
  the V02 leftovers, the snapshot uploader under power cuts and the
  `v02-loops` bounds ([#287](https://github.com/reliaburger/reliaburger/issues/287));
  V04, cold installs on Intel macOS, Linux x86_64 and Linux arm64
  ([#288](https://github.com/reliaburger/reliaburger/issues/288)).
- **Missing capabilities F01–F12** ([#320](https://github.com/reliaburger/reliaburger/issues/320)):
  GPU and cached-image placement evidence, rootless Runc clusters, upstream
  image trust, CA recovery and rotation, namespace-scoped identity, the full
  metrics and query architecture, cross-node views, WebSocket ingress parity
  and ACME, bandwidth faults, managed-volume and Apple Container parity, and
  lossless Kubernetes translation.
- **Module splits** (H05, [#319](https://github.com/reliaburger/reliaburger/issues/319)).
- **Flakes.** The open ones are in the [known flakes register](flakes.md)
  ([#318](https://github.com/reliaburger/reliaburger/issues/318)).

## How we got to 0.1.0

Every phase below is done. Each produced (or updated) a book chapter; the
[frozen roadmap](plans/archive/roadmap-to-0.1.0.md) has each phase's tests and
milestone, and the [frozen checklist](plans/archive/progress-to-0.1.0.md) has
the item-by-item history.

- [x] **Phase 1: Foundation.** Single-node container lifecycle, health checks, the Relish CLI. [Chapter 1](book/01-hello-container.md).
- [x] **Phase 2: Cluster formation.** SWIM gossip, Raft council, the Meat scheduler, reporting tree. [Chapter 2](book/02-finding-friends.md).
- [x] **Phase 2.1: Dev cluster.** `relish dev` multi-node clusters for testing. [Chapter 2](book/02-finding-friends.md).
- [x] **Phase 3: Networking.** eBPF service discovery, DNS, Wrapper ingress. [Chapter 3](book/03-talking-to-each-other.md).
- [x] **Phase 4: Security.** PKI, mTLS, secrets, join tokens. [Chapter 4](book/04-trust-no-one.md).
- [x] **Phase 5: Storage and registry.** Pickle OCI registry, volumes, snapshots. [Chapter 5](book/05-where-the-images-live.md).
- [x] **Phase 6: Observability.** Mayo metrics, Ketchup logs, Brioche dashboard, alerts. [Chapter 6](book/06-watching-everything.md).
- [x] **Phase 7: GitOps and deployments.** Lettuce, rolling deploys, Kubernetes import. [Chapter 7](book/07-ship-it.md).
- [x] **Phase 8: Advanced.** Smoker chaos, process workloads, batch jobs. [Chapter 8](book/08-breaking-things-on-purpose.md).
- [x] **Phase 9: User experience.** Blue-green deploys, autoscaling, migration. [Chapter 9](book/09-the-full-package.md).
- [x] **Phase 10: Advanced security.** Workload identity, image signing, token management. [Chapter 10](book/10-locking-it-down.md).
- [x] **Phase 11: Advanced observability.** Hierarchical aggregation, full Brioche, log export. [Chapter 11](book/11-eyes-everywhere.md).
- [x] **Phase 11b: Review and tying the loose ends.** Wiring the library-only subsystems into the binaries.
- [x] **Phase 12: Optimisations.** nftables maps, P2P downloads, compression, caching. [Chapter 12](book/12-squeezing-every-drop.md).
- [x] **Phase 12b: Correctness, security and convergence.** Six tiers (12b.1–12b.6) from the post-Phase-12 review; plans in the [archive](plans/archive/).
- [x] **Phase 13: Relish TUI.** [Chapter 13](book/13-a-room-with-a-view.md).
- [x] **Phase 14: Self-upgrade.** Rolling, signed binary replacement with rollback. [Chapter 14](book/14-changing-the-tyres.md).
- [x] **Phase 15: Testing, benchmarking and diagnostics.** `relish test`, `bench`, `wtf`, `trace`; hardening passes 15a and 15b. [Chapter 15](book/15-ready-for-production.md).
- [x] **UX track.** `relish setup`, `manual`, `source` and the README.
- [x] **Phase 16: Post-Phase-15 audit.** Truthfulness and hardening across code, manual and book.
- [x] **0.1.0 release work.** The codebase completion plan (C01–C60), static review fixes, the V02 soak and the signed release ([closure record](qualification/2026-09-27-v0.1.0-release-closure.md)).

## Future (v2)

Deferred beyond the 0.x releases; the whitepaper (§22) has the reasoning and
the [frozen roadmap](plans/archive/roadmap-to-0.1.0.md#future-v2) the detail:
TPM sealing of the master secret, external secret managers, multi-cluster
federation (Franchise), IPv6, sidecars, fractional GPU scheduling and a
PromQL-to-SQL compatibility layer.
