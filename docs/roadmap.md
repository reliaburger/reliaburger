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
  [documentation](README.md#scope-and-limits).
- [x] **0.1.1: bug fixes.** [Released](https://github.com/reliaburger/reliaburger/releases/tag/v0.1.1)
  on 30 September 2026
  ([milestone](https://github.com/reliaburger/reliaburger/milestone/1)). A
  full release is expensive, so 0.1.1 took every bug we knew about:
  - [x] the soak follow-ups in [#276](https://github.com/reliaburger/reliaburger/pull/276)
    (departed nodes forgotten, dead nodes listed as dead, the commit in
    versions, the candidate digest in the build log);
  - [x] early-user fixes from [#241](https://github.com/reliaburger/reliaburger/issues/241):
    [#281](https://github.com/reliaburger/reliaburger/issues/281)–[#284](https://github.com/reliaburger/reliaburger/issues/284);
  - [x] UI polish ([#280](https://github.com/reliaburger/reliaburger/issues/280))
    and the two flakes from #277's CI ([#285](https://github.com/reliaburger/reliaburger/issues/285));
  - [x] from the static review ([#258](https://github.com/reliaburger/reliaburger/pull/258)): the
    permission matrix for read routes ([#298](https://github.com/reliaburger/reliaburger/issues/298)) and docs that contradicted
    0.1.0 ([#300](https://github.com/reliaburger/reliaburger/issues/300));
  - [x] a changed ingress host ignored on redeploy ([#307](https://github.com/reliaburger/reliaburger/issues/307)) and log
    forwarders not resuming from the checkpoint ([#308](https://github.com/reliaburger/reliaburger/issues/308));
  - [x] the rest of the static review's defects: snapshots
    ([#291](https://github.com/reliaburger/reliaburger/issues/291)–[#294](https://github.com/reliaburger/reliaburger/issues/294)), GitOps ([#295](https://github.com/reliaburger/reliaburger/issues/295)–[#297](https://github.com/reliaburger/reliaburger/issues/297), [#305](https://github.com/reliaburger/reliaburger/issues/305)) and
    autoscaler `min = 0` ([#299](https://github.com/reliaburger/reliaburger/issues/299));
  - [x] tests that can't fail ([#301](https://github.com/reliaburger/reliaburger/issues/301), [#302](https://github.com/reliaburger/reliaburger/issues/302)) and the open flake
    ([#318](https://github.com/reliaburger/reliaburger/issues/318));
  - [x] further known bugs: the allocation error after a starved deploy
    ([#309](https://github.com/reliaburger/reliaburger/issues/309)), Mayo memory growth over a long soak ([#310](https://github.com/reliaburger/reliaburger/issues/310)), and the rest
    of the milestone ([#314](https://github.com/reliaburger/reliaburger/issues/314), [#322](https://github.com/reliaburger/reliaburger/issues/322), [#331](https://github.com/reliaburger/reliaburger/issues/331), [#333](https://github.com/reliaburger/reliaburger/issues/333), [#335](https://github.com/reliaburger/reliaburger/issues/335));
  - [x] housekeeping: the plans that led to 0.1.0 archived, and this page
    replacing the old implementation checklist.

  The homepage tour was re-recorded against the published install
  ([#317](https://github.com/reliaburger/reliaburger/issues/317)). 0.1.1 changes the state format
  (44 in 0.1.0), so a 0.1.0 cluster can't roll to it: recreate the cluster
  ([upgrading from 0.1.0](releasing.md#upgrading-from-010)).
- [x] **0.1.2: images.** [Released](https://github.com/reliaburger/reliaburger/releases/tag/v0.1.2)
  on 1 October 2026
  ([milestone](https://github.com/reliaburger/reliaburger/milestone/2)):
  - [x] images ([#248](https://github.com/reliaburger/reliaburger/pull/248)): the Go
    demo build in the tour, multi-arch builds and pulls (which removes the
    multi-platform limitation), platforms in `relish images`, a build cache cap,
    Buildah storage pruning and Buildah in the guest image;
  - [x] upgrades and rollbacks that can't succeed refused before a run starts
    ([#339](https://github.com/reliaburger/reliaburger/issues/339), [#350](https://github.com/reliaburger/reliaburger/pull/350));
  - [x] replicas spread over the survivors when a node dies, with healthy ones
    left in place ([#346](https://github.com/reliaburger/reliaburger/issues/346), [#349](https://github.com/reliaburger/reliaburger/pull/349));
  - [x] the pull-through cache holding every platform of a multi-platform
    upstream image, not just the first puller's
    ([#353](https://github.com/reliaburger/reliaburger/issues/353), [#354](https://github.com/reliaburger/reliaburger/pull/354)).

  The homepage tour was re-recorded against the published install, with the
  build, its platforms in `relish images` and the spread after a lost node.
  0.1.2 changes the protocol and state formats (27 and 46 in 0.1.1; 28 and 47
  now), so a 0.1.1 cluster can't roll to it: recreate the cluster
  ([upgrading from 0.1.1](releasing.md#upgrading-from-011)).
- [ ] **0.1.3: ready for release, qualification in progress**
  ([milestone](https://github.com/reliaburger/reliaburger/milestone/4)). Every
  item has landed and the crate says 0.1.3; what's left is building, staging,
  soaking and promoting the candidate ([runbook](releasing.md)). 0.1.3 is about
  the agent loop, observability and test quality:
  - [x] an agent loop that's metered and never waits inline on slow work: a
    turn meter, a starvation harness and the inline-await rule, the status
    snapshot, parallel state reads and restarts off the loop, and the last
    long awaits moved out, with the soak failing any turn over 1 s
    ([#351](https://github.com/reliaburger/reliaburger/issues/351), [#352](https://github.com/reliaburger/reliaburger/pull/352), [#355](https://github.com/reliaburger/reliaburger/pull/355), [#357](https://github.com/reliaburger/reliaburger/pull/357));
  - [x] streamed metrics queries, so Mayo and rollup sessions no longer load
    every Parquet file ([#377](https://github.com/reliaburger/reliaburger/issues/377), [#382](https://github.com/reliaburger/reliaburger/pull/382));
  - [x] a "blocked by quota" status for apps the scheduler won't place
    ([#326](https://github.com/reliaburger/reliaburger/issues/326), [#383](https://github.com/reliaburger/reliaburger/pull/383));
  - [x] cross-node views and log streams, part 1 of F07: live logs, deploy
    history, events and jobs answer for the whole cluster, whichever node
    serves them ([#365](https://github.com/reliaburger/reliaburger/issues/365), [#385](https://github.com/reliaburger/reliaburger/pull/385)). Part 2 is [#392](https://github.com/reliaburger/reliaburger/issues/392), in 0.1.4;
  - [x] the agent and API modules split along their ownership boundaries
    (H05, [#319](https://github.com/reliaburger/reliaburger/issues/319), [#394](https://github.com/reliaburger/reliaburger/pull/394), [#396](https://github.com/reliaburger/reliaburger/pull/396));
  - [x] an owner for every ignored test, tested CI job selection and JUnit
    evidence for every suite ([#303](https://github.com/reliaburger/reliaburger/issues/303), [#304](https://github.com/reliaburger/reliaburger/issues/304), [#380](https://github.com/reliaburger/reliaburger/pull/380));
  - [x] a two-voter council's rolling upgrade refused before it starts,
    instead of waiting for good ([#371](https://github.com/reliaburger/reliaburger/issues/371), [#372](https://github.com/reliaburger/reliaburger/pull/372));
  - [x] flakes and bugs: a live instance's pid reported missing after an
    upgrade walk ([#358](https://github.com/reliaburger/reliaburger/issues/358), [#378](https://github.com/reliaburger/reliaburger/pull/378)), the self-upgrade cluster suite run on
    a real four-voter council ([#373](https://github.com/reliaburger/reliaburger/issues/373), [#384](https://github.com/reliaburger/reliaburger/pull/384)), `relish images` columns
    and a single `wtf` builds row ([#375](https://github.com/reliaburger/reliaburger/issues/375), [#379](https://github.com/reliaburger/reliaburger/pull/379)), no double prepare of
    one instance ([#386](https://github.com/reliaburger/reliaburger/issues/386), [#388](https://github.com/reliaburger/reliaburger/pull/388)), retirement waiting out a slow runtime
    and exit codes that say when they're unknown ([#387](https://github.com/reliaburger/reliaburger/issues/387), [#389](https://github.com/reliaburger/reliaburger/issues/389),
    [#391](https://github.com/reliaburger/reliaburger/pull/391)), and the execution fence waiting out a slow network-reference
    read ([#393](https://github.com/reliaburger/reliaburger/issues/393), [#400](https://github.com/reliaburger/reliaburger/pull/400));
  - [x] the installers showing what they're downloading instead of staying
    silent for minutes ([#395](https://github.com/reliaburger/reliaburger/issues/395), [#397](https://github.com/reliaburger/reliaburger/pull/397)).

  0.1.3 changes the protocol and state formats (28 and 47 in 0.1.2; 30 and 48
  now), so a 0.1.2 cluster can't roll to it: recreate the cluster
  ([upgrading from 0.1.2](releasing.md#upgrading-from-012)). 0.1.2's leader
  refuses the upgrade before it records a run.
- [ ] **0.1.4: operations security**
  ([milestone](https://github.com/reliaburger/reliaburger/milestone/8)).
  Upstream image trust: digest pinning, cosign and worker key separation (F03,
  [#361](https://github.com/reliaburger/reliaburger/issues/361)); CA recovery
  and rotation (F04, [#362](https://github.com/reliaburger/reliaburger/issues/362));
  namespace identity and the token lifecycle (F05,
  [#363](https://github.com/reliaburger/reliaburger/issues/363)); and part 2
  of F07: separate stderr capture, richer log filters, a merged live event
  stream and rollback over merged history
  ([#392](https://github.com/reliaburger/reliaburger/issues/392)). The
  security work comes before real fleets in 0.3.0. Also a data-loss fix:
  restarting Bun no longer moves a managed-volume app onto an empty volume,
  and `relish wtf` reports an app waiting for its volume's node
  ([#423](https://github.com/reliaburger/reliaburger/issues/423)).
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
  fleet control plane can extend it later. Alongside it: the acceptance gates
  0.1.0 didn't close (V01–V04, below), WebSocket drain parity and ACME (F08,
  [#369](https://github.com/reliaburger/reliaburger/issues/369)), and fault
  pre-checks that read the leader's gossip view
  ([#334](https://github.com/reliaburger/reliaburger/issues/334)). Exit test:
  ten Dell Wyse 3040s from power-on to a cluster.
- [ ] **0.4.0: "Full container migration"**
  ([milestone](https://github.com/reliaburger/reliaburger/milestone/7),
  [#268](https://github.com/reliaburger/reliaburger/pull/268)). Drain and
  uncordon, cold moves that carry volumes, CRIU checkpoint and restore,
  pre-dump iterations, lazy pages and TCP handoff, for apps and jobs. Also
  managed-volume retirement and the Apple runtime (F10,
  [#367](https://github.com/reliaburger/reliaburger/issues/367)).
- [ ] **After 0.4.0: book v2 and the full CLI tour.** Book v2: a distilled
  rewrite alongside v1; v1 keeps collecting until then. The rewrite reads
  properly from beginning to end: it cuts the fluff, keeps the design
  narrative, the first-use Rust explanations and the lessons, and folds
  material that was bolted onto earlier chapters later back into place
  ([#311](https://github.com/reliaburger/reliaburger/issues/311)). Alongside
  it, the website gets a full CLI walkthrough (a second recorded tour that
  exercises every `relish` command) and a searchable command reference on the
  landing page that ctrl-F can find, generated from the CLI
  ([#315](https://github.com/reliaburger/reliaburger/issues/315)).
- [ ] **Later**
  ([milestone](https://github.com/reliaburger/reliaburger/milestone/9)). The
  fleet control plane ([#265](https://github.com/reliaburger/reliaburger/pull/265));
  GPU capacity and cached-image placement (F01,
  [#359](https://github.com/reliaburger/reliaburger/issues/359)); rootless
  clusters (F02, [#360](https://github.com/reliaburger/reliaburger/issues/360));
  PromQL and the full metrics architecture (F06,
  [#364](https://github.com/reliaburger/reliaburger/issues/364)); bandwidth
  faults (F09, [#366](https://github.com/reliaburger/reliaburger/issues/366));
  and the remaining Kubernetes translations (F11,
  [#368](https://github.com/reliaburger/reliaburger/issues/368)).

## Known gaps

What 0.1.0 shipped without, each with an issue. The
[whitepaper](whitepaper.md) describes the full vision; this page tracks only
what's shipped and what's scheduled.

- **Acceptance gates 0.1.0 didn't close.** V01, the live catalogue on three
  independent runc nodes ([#286](https://github.com/reliaburger/reliaburger/issues/286));
  the V02 leftovers, the snapshot uploader under power cuts and the
  `v02-loops` bounds ([#287](https://github.com/reliaburger/reliaburger/issues/287));
  V04, cold installs on Intel macOS, Linux x86_64 and Linux arm64
  ([#288](https://github.com/reliaburger/reliaburger/issues/288)). All in 0.3.0.
- **Missing capabilities F01–F11**, one issue each, placed in
  [0.1.3](https://github.com/reliaburger/reliaburger/milestone/4) (F07 part 1, landed),
  [0.1.4](https://github.com/reliaburger/reliaburger/milestone/8) (F03–F05,
  F07 part 2),
  [0.3.0](https://github.com/reliaburger/reliaburger/milestone/6) (F08),
  [0.4.0](https://github.com/reliaburger/reliaburger/milestone/7) (F10) and
  [Later](https://github.com/reliaburger/reliaburger/milestone/9) (F01, F02,
  F06, F09, F11).
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
