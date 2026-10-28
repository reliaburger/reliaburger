# Research: how Kubernetes tests its releases

28 October 2026. Part of [0.2.4](https://github.com/reliaburger/reliaburger/milestone/19).

What does the Kubernetes project do before it ships a release that we don't?
Is any of it worth copying at our size? This page answers both, and is the
source of the "How each release is tested" section on the
[Kubernetes comparison page](../website/compare/kubernetes.html#testing).
Every Kubernetes claim links to its source. Where we couldn't find a current
document, we say so.

## The short answer

Our release gate is already stricter than Kubernetes' in two places. We never
retry a failed test. And we soak the exact signed candidate, installed the way
users install it, under faults, for hours. Kubernetes shares the first (it has
a zero-flake policy with no automatic retries,
[flaky-tests.md](https://github.com/kubernetes/community/blob/master/contributors/devel/sig-testing/flaky-tests.md)),
but not the second.

Where Kubernetes is ahead, most of it is tooling that only pays off with
thousands of contributors and hundreds of CI jobs. Four things aren't, and
they're filed for 0.2.4:

| # | What | Issue | Effort |
|---|---|---|---|
| 1 | Fuzz every decoder that reads bytes from another machine | [#1398](https://github.com/reliaburger/reliaburger/issues/1398) | Low to start |
| 2 | Check the soak's council history for linearizability, not just invariants | [#1399](https://github.com/reliaburger/reliaburger/issues/1399) | Medium to high |
| 3 | Gate the soak on two or three latency SLOs users would feel | [#1400](https://github.com/reliaburger/reliaburger/issues/1400) | Medium |
| 4 | Hollow nodes: measure the council with hundreds of fake agents before 0.3.0 | [#1401](https://github.com/reliaburger/reliaburger/issues/1401) | Medium to high |

They're independent of each other, so they can be worked in parallel, and
none of them touches the 0.2.3 release.

## How Kubernetes decides a release is ready

Kubernetes ships about three minor releases a year
([releases](https://kubernetes.io/releases/release/)). Each goes through
alphas, a beta and release candidates, with a code freeze and a test freeze
([release phases](https://github.com/kubernetes/sig-release/blob/master/releases/release_phases.md),
[1.34 schedule](https://github.com/kubernetes/sig-release/blob/master/releases/release-1.34/README.md)).

The decision rests on CI dashboards, not on one build. A job is
release-blocking only if it runs at least every three hours and passed at
least 75% of its runs in the past week, and at go/no-go each blocking job
needs three green runs in a row
([release-blocking jobs](https://github.com/kubernetes/sig-release/blob/master/release-blocking-jobs.md)).
"Informing" jobs don't have to be green. A Release Signal team reads
[TestGrid](https://github.com/kubernetes/test-infra/blob/master/testgrid/README.md)
and gives the go/no-go
([Release Signal handbook](https://github.com/kubernetes/sig-release/blob/master/release-team/role-handbooks/release-signal/README.md)).

We decide on one signed candidate: built once, staged as a private
pre-release, installed with `curl | sh`, soaked, and promoted byte for byte
([releasing.md](../releasing.md)). Every check must be green on that build,
and one invariant breach fails it.

## Side by side

| Area | Kubernetes | Reliaburger |
|---|---|---|
| Unit and integration | `make test`, with a stress mode ([testing.md](https://github.com/kubernetes/community/blob/master/contributors/devel/sig-testing/testing.md)) | nextest unit tests, the `tests/suite` binary, proptest, insta, doctests, doc snippets checked against the real parser ([testing.md](../testing.md)) |
| End to end | Ginkgo suites labelled `[Slow]`, `[Serial]`, `[Disruptive]`, `[Flaky]`, `[Conformance]`, plus node e2e ([e2e-tests.md](https://github.com/kubernetes/community/blob/master/contributors/devel/sig-testing/e2e-tests.md)); clusters from [kubetest2](https://github.com/kubernetes-sigs/kubetest2/blob/master/README.md) deployers such as kind | Gated suites (cluster, Linux, rootless, upgrade, slow acceptance, standard registry clients) plus manual Apple and GPU runs; every `#[ignore]` names who runs it |
| Conformance | GA-only, portable tests, reliable for two weeks before promotion ([conformance-tests.md](https://github.com/kubernetes/community/blob/master/contributors/devel/sig-architecture/conformance-tests.md)); CNCF certifies distributions ([k8s-conformance](https://github.com/cncf/k8s-conformance/blob/master/README.md)) | `relish test` runs live checks on the user's own cluster; no frozen subset, no certification |
| Scale | Up to 5,000 nodes and 150,000 pods ([large clusters](https://kubernetes.io/docs/setup/best-practices/cluster-large/)), measured with [ClusterLoader2](https://github.com/kubernetes/perf-tests/blob/master/clusterloader2/README.md) and trended on [perf-dash](https://github.com/kubernetes/perf-tests/blob/master/perfdash/README.md); [Kubemark](https://github.com/kubernetes/community/blob/master/contributors/devel/sig-scalability/kubemark-guide.md) runs a real control plane against hollow nodes | Criterion benches (gossip to 1,000 nodes, scheduling over 10,000), a regression gate on task arrays, three real VMs, and the 24-hour day run for job throughput |
| SLOs | API call p99 ≤ 1 s, pod startup p99 ≤ 5 s ([SLOs](https://github.com/kubernetes/community/blob/master/sig-scalability/slos/slos.md)) | Internal health only: no agent-loop turn over 1 s, no leader gap over 30 s, the job rate |
| Upgrades and skew | A [version-skew policy](https://kubernetes.io/releases/version-skew-policy/), upgrade jobs, and compatibility-version jobs since [KEP-4330](https://www.kubernetes.dev/resources/keps/4330/) | Rolling self-upgrade and rollback between builds of one format; nothing across formats before 1.0, by the maintainer's decision |
| Soak | Only a passing mention of soak clusters in the e2e docs; we found no current release-gating soak | One-hour V02 and jobs tiers, an 8-hour jobs fault soak with a leak-check tail, power-cut and reboot lanes, and the 24-hour day run |
| Data under faults | `[Disruptive]` tests; data correctness is etcd's job, and its [robustness tests](https://github.com/etcd-io/etcd/blob/main/tests/robustness/README.md) record histories under faults and check linearizability | Faults every soak cycle and hard power-offs that check every acknowledged write survives; invariants, not a history model |
| Fuzzing | In [OSS-Fuzz](https://github.com/google/oss-fuzz/blob/master/projects/kubernetes/project.yaml) | None |
| Static analysis | golangci-lint through `hack/verify-*` ([verify-golangci-lint.sh](https://github.com/kubernetes/kubernetes/blob/master/hack/verify-golangci-lint.sh)) | clippy with `-D warnings`, rustfmt, a rule against inline awaits in the agent loop, a coverage floor |
| Dependency advisories | A scheduled scan reported on TestGrid ([community PR 5853](https://github.com/kubernetes/community/pull/5853), 2021; today's tool unverified) | `cargo audit` on every PR and weekly, dated exceptions, Dependabot |
| CI | [Prow and Tide](https://docs.prow.k8s.io/docs/overview/), TestGrid | GitHub Actions, merge trains and a merge queue, one evidence gate that requires every test owner |
| Flaky tests | Zero-flake policy, no automatic retries, `kind/flake` issues, `[Flaky]` quarantine ([flaky-tests.md](https://github.com/kubernetes/community/blob/master/contributors/devel/sig-testing/flaky-tests.md)) | Retries are 0, every flake goes in the [register](../flakes.md) the same day; no quarantine |

## What we should copy

### 1. Fuzzing ([#1398](https://github.com/reliaburger/reliaburger/issues/1398))

Nothing in Reliaburger is fuzzed. Gossip datagrams arrive over unauthenticated
UDP. DNS queries come from any workload. Raft frames, OCI manifests, backup
files and join CSRs all cross a trust boundary. Each decoder has only seen the
inputs we wrote for its unit tests.

[cargo-fuzz](https://rust-fuzz.github.io/book/cargo-fuzz.html) makes this
cheap: a target is a function that takes a byte slice, and libFuzzer does the
rest. The plan is one target per decoder (`mustard::message::decode_datagram`,
`council::network::decode_and_admit`, `onion::dns::decode_query`,
`council::backup::decode_backup`, OCI manifest parsing, join CSRs, config
TOML), seed corpora from the unit-test fixtures, a short run on PRs that
touch a decoder, a longer one nightly, and OSS-Fuzz once the targets are
stable. Every crash becomes a unit test before it's fixed.

### 2. A history check in the soak ([#1399](https://github.com/reliaburger/reliaburger/issues/1399))

Our soak asks questions like "did a counter go backwards?" and "did an
acknowledged write survive?". Those are invariants, and they catch a lot. They
don't catch a stale read that happens to look plausible, or two clients
holding the same lease for a few hundred milliseconds.

etcd's robustness tests record every client operation under faults, with its
start time, end time and result, then check the whole history against a model
of a linearizable store (one where every operation appears to take effect at a
single instant between its call and its return). That found lost writes and
revisions going backwards that their other tests missed. The council is a Raft
store with leases, the same shape as etcd, so the same check fits. Start with
council KV and leases in the fast tier, with
[Porcupine](https://github.com/anishathalye/porcupine) as the reference design.

### 3. SLOs users would feel ([#1400](https://github.com/reliaburger/reliaburger/issues/1400))

We gate what the cluster feels: agent-loop turns, leader gaps. We don't gate
what a user feels: how long `relish deploy` takes to answer, or how long an app
takes to start. Kubernetes publishes both and measures every release against
them. We'd pick two or three, set thresholds from the 0.2.2 and 0.2.3 soak
records with margin, fail the soak when one breaks, and list them across past
records instead of building a dashboard.

### 4. Hollow nodes ([#1401](https://github.com/reliaburger/reliaburger/issues/1401))

5,000 real nodes isn't something a project our size can afford. Kubernetes
has a cheaper tool too: [Kubemark](https://github.com/kubernetes/community/blob/master/contributors/devel/sig-scalability/kubemark-guide.md) runs a real control plane against
"hollow" nodes, kubelets with the container runtime mocked out. Our biggest real cluster is three
VMs, and our 1,000- and 10,000-node numbers come from benchmarks of single
pieces. 0.3.0 starts making claims about real fleets, so we should measure a
council serving hundreds of hollow Buns first.

## What we won't copy, and why

- **Upgrade and version-skew jobs.** No compatibility before 1.0 is a
  maintainer decision (28 September 2026). From 1.0, the published-to-published
  rolling walk in [releasing.md](../releasing.md) becomes a gate, and an n−1
  skew lane is worth a look then.
- **Public alphas, betas and RCs.** `stage.yml` already makes a pre-release;
  we'd only stop keeping it private. Worth doing when outside users would test
  one. Not yet.
- **A conformance programme.** There are no other distributions to certify. At
  1.0, a stable subset of `relish test` checks can become the API promise.
- **Release Signal, TestGrid, blocking and informing thresholds.** These exist
  because hundreds of jobs can't all be green at once. Ours can, and "all green
  on the candidate" is a stricter rule than "75% this week".
- **A `[Flaky]` quarantine.** A small team fixing flakes the same day doesn't
  need a place to park them.

## What we do that Kubernetes doesn't

As far as we could verify:

- **We soak the release files themselves.** The candidate is installed from
  its staged pre-release with the public installer, then put through hours of
  faults. Kubernetes' go/no-go reads CI dashboards for the release branch, and
  we found no soak of the staged release.
- **Power cuts.** Hard VM power-offs, 25 per lane, checking every acknowledged
  write. Kubernetes has unclean-reboot node tests, and etcd covers crash
  durability, so this is only partly ours alone.
- **A multi-hour fault soak as a release gate.** We found no current
  Kubernetes equivalent.

## What we couldn't verify

- The current schedule of Kubernetes' 5,000-node jobs. The
  [scalability validation](https://github.com/kubernetes/community/blob/master/sig-scalability/processes/scalability-validation.md)
  document is from 2017.
- Which dependency scanner Kubernetes runs today. The only document we found
  is a 2021 community PR.
- Whether any soak gates a Kubernetes release. The e2e docs mention soak
  clusters only in passing.

## Sources

Kubernetes and etcd, all read on 27 October 2026:

- SIG Testing: [testing.md](https://github.com/kubernetes/community/blob/master/contributors/devel/sig-testing/testing.md), [e2e-tests.md](https://github.com/kubernetes/community/blob/master/contributors/devel/sig-testing/e2e-tests.md), [flaky-tests.md](https://github.com/kubernetes/community/blob/master/contributors/devel/sig-testing/flaky-tests.md), [kubetest2](https://github.com/kubernetes-sigs/kubetest2/blob/master/README.md), [KEP-2464](https://www.kubernetes.dev/resources/keps/2464/)
- Conformance: [conformance-tests.md](https://github.com/kubernetes/community/blob/master/contributors/devel/sig-architecture/conformance-tests.md), [cncf/k8s-conformance](https://github.com/cncf/k8s-conformance/blob/master/README.md)
- SIG Scalability: [Kubemark](https://github.com/kubernetes/community/blob/master/contributors/devel/sig-scalability/kubemark-guide.md), [large clusters](https://kubernetes.io/docs/setup/best-practices/cluster-large/), [ClusterLoader2](https://github.com/kubernetes/perf-tests/blob/master/clusterloader2/README.md), [perf-dash](https://github.com/kubernetes/perf-tests/blob/master/perfdash/README.md), [scalability validation](https://github.com/kubernetes/community/blob/master/sig-scalability/processes/scalability-validation.md), [SLOs](https://github.com/kubernetes/community/blob/master/sig-scalability/slos/slos.md)
- Upgrades: [version-skew policy](https://kubernetes.io/releases/version-skew-policy/), [KEP-4330](https://www.kubernetes.dev/resources/keps/4330/), [compatibility-versions job history](https://prow.k8s.io/job-history/gs/kubernetes-ci-logs/logs/ci-kubernetes-e2e-kind-compatibility-versions-feature-gate-test)
- SIG Release: [releases](https://kubernetes.io/releases/release/), [release phases](https://github.com/kubernetes/sig-release/blob/master/releases/release_phases.md), [1.34 schedule](https://github.com/kubernetes/sig-release/blob/master/releases/release-1.34/README.md), [release-blocking jobs](https://github.com/kubernetes/sig-release/blob/master/release-blocking-jobs.md), [Release Signal handbook](https://github.com/kubernetes/sig-release/blob/master/release-team/role-handbooks/release-signal/README.md)
- Tooling: [Prow](https://docs.prow.k8s.io/docs/overview/), [TestGrid](https://github.com/kubernetes/test-infra/blob/master/testgrid/README.md), [verify-golangci-lint.sh](https://github.com/kubernetes/kubernetes/blob/master/hack/verify-golangci-lint.sh), [community PR 5853](https://github.com/kubernetes/community/pull/5853)
- Fuzzing: [OSS-Fuzz Kubernetes project](https://github.com/google/oss-fuzz/blob/master/projects/kubernetes/project.yaml), [OSS-Fuzz Rust guide](https://google.github.io/oss-fuzz/getting-started/new-project-guide/rust-lang/), [cargo-fuzz](https://rust-fuzz.github.io/book/cargo-fuzz.html)
- etcd: [robustness tests](https://github.com/etcd-io/etcd/blob/main/tests/robustness/README.md); [Porcupine](https://github.com/anishathalye/porcupine)

Reliaburger: [testing.md](../testing.md), [releasing.md](../releasing.md),
[flakes.md](../flakes.md), [test-harness.md](../design/test-harness.md),
[`.config/nextest.toml`](../../.config/nextest.toml),
[`security.yml`](../../.github/workflows/security.yml),
[`soak.yml`](../../.github/workflows/soak.yml),
[`v02-loops.yml`](../../.github/workflows/v02-loops.yml) and
[`scripts/release/`](../../scripts/release/).
