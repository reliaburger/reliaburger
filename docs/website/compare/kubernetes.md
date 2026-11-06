<!-- Generated from docs/website/compare/kubernetes.html by `make agent-docs`: edit the page, not this file. -->

PROJECT COMPARISON · 15 OCTOBER 2026

# Reliaburger and Kubernetes

Reliaburger brings deployment, service discovery, metrics, logs and fault injection into one platform. Define an app, operate it through Relish, and use the same app name when investigating a failure. Kubernetes remains the choice when your application needs its APIs and extensions.

Basis: [Integrated platform](https://github.com/reliaburger/reliaburger/blob/v0.2.2/README.md) · [App diagnostics](https://github.com/reliaburger/reliaburger/blob/v0.2.2/docs/manual/07_diagnostics.md) · [Kubernetes extension model](https://kubernetes.io/docs/concepts/extend-kubernetes/operator/). The recommendation is our judgement.

**Versions matter.** Reliaburger statements below describe the [0.2.2 release](https://github.com/reliaburger/reliaburger/releases/tag/v0.2.2), which the public installer installs, re-checked on 21 October 2026. Kubernetes reference: [v1.37.1](https://github.com/kubernetes/kubernetes/releases/tag/v1.37.1). This is a source and documentation review, not a deployment benchmark. [Method and limits](https://reliaburger.com/compare/methodology.html#method).

## Where each fits

### Choose Reliaburger for an integrated workflow

Reliaburger puts scheduling, service discovery, ingress, registry, metrics, logs, GitOps and fault injection behind Bun and Relish. For a team whose applications fit its app model, this gives deployment and operations a shared interface. That is the reason to consider Reliaburger; availability and operating cost still need testing for your workload.

Sources: [Implemented platform and release boundary](https://github.com/reliaburger/reliaburger/blob/v0.2.2/README.md) · [Fault controls](https://github.com/reliaburger/reliaburger/blob/v0.2.2/docs/manual/05_chaos.md).

### Choose Kubernetes for its API contract

Our recommendation: prefer Kubernetes when an application needs operators, multi-container Pods, persistent-volume integrations or StatefulSet identity. Those are explicit Kubernetes concepts. Reliaburger imports only the first container, drops PVCs and custom resources, and converts StatefulSets without their ordering or stable network identities.

Sources: [Operator pattern](https://kubernetes.io/docs/concepts/extend-kubernetes/operator/) · [Pods](https://kubernetes.io/docs/concepts/workloads/pods/) · [StatefulSets](https://kubernetes.io/docs/concepts/workloads/controllers/statefulset/) · [Import contract](https://github.com/reliaburger/reliaburger/blob/v0.2.2/docs/manual/09_kubernetes.md).

## Translate concepts, then check semantics

Related concepts are not interchangeable guarantees
| Concern           | Kubernetes                                                                                                                                                                        | Reliaburger                                                                                                                                                                                                               |
| ----------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| A running service | A Deployment manages replicas of a Pod template. [Deployment contract](https://kubernetes.io/docs/concepts/workloads/controllers/deployment/)                                     | An `[app.web]` describes a service and its replicas. [App contract](https://github.com/reliaburger/reliaburger/blob/v0.2.2/docs/manual/01_deploy-an-app.md)                                                               |
| Discovery         | A Service selects backends and exposes a stable endpoint. [Services](https://kubernetes.io/docs/concepts/services-networking/service/)                                            | An app with a port gets a VIP; configured DNS gives it an `.internal` name. The service data path is TCP-only. [Networking contract](https://github.com/reliaburger/reliaburger/blob/v0.2.2/docs/manual/03_networking.md) |
| Durable data      | PVCs request storage from the volume system; the selected provisioner determines behaviour. [Persistent volumes](https://kubernetes.io/docs/concepts/storage/persistent-volumes/) | Managed volumes stay on their node. Btrfs-backed volumes support snapshots. [Volume contract](https://github.com/reliaburger/reliaburger/blob/v0.2.2/docs/manual/11_images-and-volumes.md#volumes)                        |
| Control plane     | API server, scheduler, controllers and etcd are distinct architectural components. [Architecture](https://kubernetes.io/docs/concepts/architecture/)                              | Bun embeds a Raft council alongside the node agent; SWIM gossip handles membership. [Architecture](https://github.com/reliaburger/reliaburger/blob/v0.2.2/docs/manual/06_under-the-hood.md)                               |

Existing YAML is a starting point, not proof of compatibility. Review the conversion report; `relish import -f web.yaml --strict` exits 1 if anything was approximated or dropped, so CI can refuse a lossy migration. Since 0.2.2 the importer attaches a Service to the workloads its selector matches, as Kubernetes routes it, and copies a ConfigMap read through `envFrom` into the app's `env`.

Sources: [Supported kinds, losses and strict import](https://github.com/reliaburger/reliaburger/blob/v0.2.2/docs/manual/09_kubernetes.md).

## Install the platform

**Reliaburger laptop trial:** macOS on Apple silicon, or Linux (x86\_64 or arm64) with QEMU/KVM, roughly 8 GiB free memory and 15 GiB disk. The installer below creates Linux VMs and installs released 0.2.2, the version this page describes. `--laptop` picks the laptop cluster; with no flag the installer only explains its choices.

Sources: [Laptop prerequisites](https://github.com/reliaburger/reliaburger/blob/v0.2.2/docs/quickstart.md) · [Installer version](https://github.com/reliaburger/reliaburger/blob/v0.2.2/docs/website/install.sh).

RELIABURGER · RELEASED LAPTOP TRIAL sh

```
curl -fsSL https://reliaburger.com/install.sh | RELIABURGER_VERSION=v0.2.2 sh -s -- --laptop
relish local status
relish nodes
```

**Kubernetes on Linux servers:** the following is the bootstrap stage after installing a compatible container runtime, kubeadm, kubelet and kubectl and completing host/network prerequisites. It is not a complete installation script. Configure the chosen CNI network, then run the generated join command on prepared workers.

Sources: [Host and package preparation](https://kubernetes.io/docs/setup/production-environment/tools/kubeadm/install-kubeadm/) · [Bootstrap, CNI and joining](https://kubernetes.io/docs/setup/production-environment/tools/kubeadm/create-cluster-kubeadm/).

KUBERNETES · PREPARED CONTROL-PLANE HOST sh

```
sudo kubeadm init --kubernetes-version=v1.37.1
mkdir -p "$HOME/.kube"
sudo cp /etc/kubernetes/admin.conf "$HOME/.kube/config"
sudo chown "$(id -u):$(id -g)" "$HOME/.kube/config"
# Install your selected CNI before joining workers.
kubectl get nodes
```

This is the self-managed kubeadm path. Kubernetes also documents hosted control planes; responsibilities depend on the provider. [Production environment options](https://kubernetes.io/docs/setup/production-environment/).

These starting points differ. For a laptop Kubernetes trial, upstream documents kind and minikube; for existing Linux servers, Reliaburger has a separate rootful-runc/eBPF setup guide. Counting the displayed commands would not be a fair installation-speed comparison.

Sources: [Local Kubernetes tools](https://kubernetes.io/docs/tasks/tools/) · [Reliaburger server installation](https://github.com/reliaburger/reliaburger/blob/v0.2.2/docs/linux-servers.md).

## Deploy, inspect and change an application

Assume authenticated clients, a working container cluster, namespace `default` and permission to deploy. These examples use a disposable, stateless nginx service. The image tag illustrates configuration; use your reviewed image digest for a reproducible deployment. Kubernetes commands below generate a Deployment manifest; Relish consumes the TOML file.

Sources: [kubectl examples](https://kubernetes.io/docs/reference/kubectl/quick-reference/) · [App configuration](https://github.com/reliaburger/reliaburger/blob/v0.2.2/docs/manual/01_deploy-an-app.md) · [Image pinning](https://github.com/reliaburger/reliaburger/blob/v0.2.2/docs/manual/11_images-and-volumes.md#tags-bind-to-digests-at-apply).

The Relish file adds an HTTP health check. Add a Kubernetes `readinessProbe` to the generated workload for a comparable readiness gate. [Kubernetes probes](https://kubernetes.io/docs/tasks/configure-pod-container/configure-liveness-readiness-startup-probes/) · [Relish health checks](https://github.com/reliaburger/reliaburger/blob/v0.2.2/docs/manual/01_deploy-an-app.md#health-checks).

KUBERNETES · CREATE AND APPLY sh

```
kubectl create deployment web --image=nginx:alpine \
  --replicas=2 --dry-run=client -o yaml > web.yaml
kubectl apply --dry-run=server -f web.yaml
kubectl apply -f web.yaml
kubectl rollout status deployment/web
```

RELIABURGER · SAVE AS web.toml toml

```
[app.web]
image = "nginx:alpine"
port = 80
replicas = 2

[app.web.health]
path = "/"
```

RELIABURGER · VALIDATE AND APPLY sh

```
relish lint web.toml
relish apply web.toml
relish inspect web
```

Equivalent intentions, different command interfaces
| Task               | Kubernetes                                                            | Reliaburger                                                     |
| ------------------ | --------------------------------------------------------------------- | --------------------------------------------------------------- |
| Inspect / logs     | `kubectl get pods`\ `kubectl logs deployment/web`                     | `relish inspect web`\ `relish logs web`                         |
| Execute a command  | `kubectl exec deployment/web -- nginx -t`                             | `relish exec web nginx -t`                                      |
| Scale to three     | `kubectl scale deployment/web --replicas=3`; update the manifest too. | Set `replicas = 3` in `web.toml`, then `relish apply web.toml`. |
| Delete the example | `kubectl delete deployment web`                                       | `relish delete web`                                             |

Sources: [Kubernetes command syntax](https://kubernetes.io/docs/reference/kubectl/quick-reference/) · [Relish lifecycle commands](https://github.com/reliaburger/reliaburger/blob/v0.2.2/docs/manual/01_deploy-an-app.md) · [Relish reference](https://github.com/reliaburger/reliaburger/blob/v0.2.2/docs/manual/13_reference.md).

The Kubernetes example creates no Service. Use `kubectl expose deployment web --port=80` for an internal Service; delete that Service separately during cleanup. Reliaburger creates discovery from the app's port. Neither example configures public ingress.

Sources: [Service exposure](https://kubernetes.io/docs/concepts/services-networking/service/) · [App discovery and ingress](https://github.com/reliaburger/reliaburger/blob/v0.2.2/docs/manual/03_networking.md).

## Application rollouts and platform upgrades

### Changing the application

Kubernetes Deployment rollouts expose `maxSurge`, `maxUnavailable`, history and `kubectl rollout undo deployment/web`. Exceeding a progress deadline reports failure; the Deployment controller does not automatically roll back. Since 0.2.1, Reliaburger gates the new specification one node at a time; each node applies its own surge/unavailable budget. Automatic rollback is configurable, and `relish rollback web` uses council-recorded healthy history. These are different budget scopes.

Sources: [Deployment rollout and failure semantics](https://kubernetes.io/docs/concepts/workloads/controllers/deployment/) · [Rollout coordination and rollback](https://github.com/reliaburger/reliaburger/blob/v0.2.2/docs/manual/01_deploy-an-app.md).

### Changing the orchestrator

Kubeadm has a supported upgrade procedure: prepare the target packages, inspect `sudo kubeadm upgrade plan`, upgrade control-plane nodes sequentially, handle the CNI, then drain and update worker components. Follow the version-skew policy; do not skip minor versions. This compatibility process is a concrete advantage over Reliaburger's current pre-1.0 policy.

Sources: [Upgrade procedure](https://kubernetes.io/docs/tasks/administer-cluster/kubeadm/kubeadm-upgrade/) · [Version skew](https://kubernetes.io/releases/version-skew-policy/) · [Reliaburger compatibility](https://github.com/reliaburger/reliaburger/blob/v0.2.2/docs/releasing.md#compatibility-before-100).

Reliaburger's `relish upgrade check` and `relish upgrade plan VERSION` check a candidate. Same-format upgrades roll workers before council members, with signatures and workload adoption. Format changes require a fresh cluster: there are no state migrations before 1.0, and 0.2.2 is such a change, so a 0.2.1 cluster is recreated rather than upgraded. A qualified published-to-published rolling upgrade is not established by this review.

Sources: [Upgrade mechanics](https://github.com/reliaburger/reliaburger/blob/v0.2.2/docs/manual/12_operations.md) · [Qualification and format limits](https://github.com/reliaburger/reliaburger/blob/v0.2.2/docs/releasing.md#compatibility-before-100).

## How each release is tested

Kubernetes decides a release from its CI dashboards. Each release-blocking job runs at least every three hours, passes at least 75% of its runs in a week, and needs three green runs in a row at the go/no-go meeting. Reliaburger decides from one candidate: CI builds and signs the files once, and the gate installs those exact files the way you would, then breaks the cluster for hours. One lost write, missed drain or stalled agent loop fails it. Each fits its size. Kubernetes' gate covers thousands of contributors' changes across hundreds of jobs; ours tests one small project's binary very hard, on one laptop.

Sources: [Release-blocking criteria](https://github.com/kubernetes/sig-release/blob/master/release-blocking-jobs.md) · [Release Signal handbook](https://github.com/kubernetes/sig-release/blob/master/release-team/role-handbooks/release-signal/README.md) · [Reliaburger release gate](https://github.com/reliaburger/reliaburger/blob/v0.2.2/docs/testing.md#before-a-release) · [0.2.2 gate record](https://github.com/reliaburger/reliaburger/blob/v0.2.2/docs/qualification/2026-10-25-v0.2.2-release-closure.md).

How each project tests a release
| Area                      | Kubernetes                                                                                                                                                                                                                                                                                                                       | Reliaburger                                                                                                                                                                                                                                                                                                                   |
| ------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| Before merge              | Unit and integration tests, then Ginkgo end-to-end tests labelled `[Slow]`, `[Serial]`, `[Disruptive]` and so on, on clusters that kubetest2 brings up, often with kind. [End-to-end tests](https://github.com/kubernetes/community/blob/master/contributors/devel/sig-testing/e2e-tests.md)                                     | Unit tests and one portable integration binary, property tests with proptest, then gated suites on real clusters, Linux, rootless runc and upgrades. Every skipped test names who runs it. [Testing guide](https://github.com/reliaburger/reliaburger/blob/v0.2.2/docs/testing.md)                                            |
| Flaky tests               | A zero-flake policy: jobs must not retry a failed test. A flake that can't be fixed quickly gets a `[Flaky]` label, which most jobs skip. [Flaky tests](https://github.com/kubernetes/community/blob/master/contributors/devel/sig-testing/flaky-tests.md)                                                                       | No retries either. A failure that passes on re-run goes in a register the same day, with an issue if it isn't fixed that day. There is no quarantine. [Flake register](https://github.com/reliaburger/reliaburger/blob/v0.2.2/docs/flakes.md)                                                                                 |
| What the release rests on | The release branch's blocking jobs, read by the release team's Release Signal role, through alphas, a beta and release candidates. [Release phases](https://github.com/kubernetes/sig-release/blob/master/releases/release_phases.md)                                                                                            | The staged, signed release files: two cold `curl &#124; sh` installs, a one-hour app soak, one-hour and 8-hour job soaks with faults every cycle, and 25 power cuts and reboots per lane. Promotion copies those files byte for byte. [Release runbook](https://github.com/reliaburger/reliaburger/blob/v0.2.2/docs/releasing.md) |
| Scale                     | Supports clusters of up to 5,000 nodes. ClusterLoader2 checks published SLOs, such as API calls at p99 ≤ 1 s and stateless pod startup at p99 ≤ 5 s. [Large clusters](https://kubernetes.io/docs/setup/best-practices/cluster-large/) · [SLOs](https://github.com/kubernetes/community/blob/master/sig-scalability/slos/slos.md) | Three VMs on one laptop. Benchmarks simulate gossip to 1,000 nodes and a scheduling pass over 10,000. No user-facing latency SLO yet. [Benchmarks](https://github.com/reliaburger/reliaburger/blob/v0.2.2/docs/testing.md#benchmarks-in-ci)                                                                                   |
| Data under faults         | Correctness lives in etcd, whose robustness tests record every operation under injected faults and check the history for linearizability. They have found more than a dozen bugs, lost writes among them. [etcd robustness tests](https://github.com/etcd-io/etcd/blob/main/tests/robustness/README.md)                          | The soak checks invariants all the way through: no lost write, no counter going backwards, no job run twice. Power-cut lanes check every acknowledged write survives a VM forced off. There's no history checker. [Pulling the plug](https://github.com/reliaburger/reliaburger/blob/v0.2.2/docs/testing.md#pulling-the-plug) |
| Upgrades                  | A version-skew policy, upgrade and skew jobs, and since KEP-4330 jobs that run a binary emulating an older version. [Version skew](https://kubernetes.io/releases/version-skew-policy/) · [KEP-4330](https://www.kubernetes.dev/resources/keps/4330/)                                                                            | Every soak walks each node to a signed build and back, with a power-off or leader kill mid-walk on every other walk. Before 1.0 there is no upgrade between published releases. [Compatibility before 1.0](https://github.com/reliaburger/reliaburger/blob/v0.2.2/docs/releasing.md#compatibility-before-100)                 |
| Fuzzing and dependencies  | Enrolled in OSS-Fuzz. [OSS-Fuzz project](https://github.com/google/oss-fuzz/blob/master/projects/kubernetes/project.yaml)                                                                                                                                                                                                        | No fuzzing yet. `cargo audit` fails the build on a new advisory unless a dated, reviewed exception covers it. [Exception review](https://github.com/reliaburger/reliaburger/blob/v0.2.2/docs/qualification/2026-10-24-dependency-exceptions.md)                                                                               |
| On your cluster           | Conformance tests (GA features only) let the CNCF certify distributions, run with Sonobuoy or Hydrophone. [Conformance tests](https://github.com/kubernetes/community/blob/master/contributors/devel/sig-architecture/conformance-tests.md) · [Certification](https://github.com/cncf/k8s-conformance/blob/master/README.md)     | `relish test` runs a catalogue of live checks in leased namespaces, and `--chaos` kills the leader and partitions nodes. Nothing is certified. [Inside a live cluster](https://github.com/reliaburger/reliaburger/blob/v0.2.2/docs/testing.md#inside-a-live-cluster)                                                          |

**Where Kubernetes is ahead:** fuzzing the parsers that face the network, checking full histories rather than invariants, and latency SLOs a user would feel. None of that needs Kubernetes' size, so these are the gaps worth closing. Its 5,000-node runs and dashboard tooling do need that size. **Where Reliaburger goes further:** we couldn't find a Kubernetes gate that soaks the staged release files under faults for hours, or pulls the power on them mid-write. The closest are its `[Disruptive]` end-to-end tests and etcd's crash tests.

Comparison checked against primary documentation on 27 October 2026. Some Kubernetes schedules, like how often the 5,000-node jobs run, aren't documented in a current source, so we've left them out.

## Where Reliaburger fits

**Learning curve, qualitative:** Kubernetes asks an application operator to distinguish Pods, Deployments, Services and storage claims. Reliaburger collects several of those settings into one app. That is fewer configuration boundaries for this nginx example, not a measured reduction in training time. An experienced Kubernetes team must instead learn Reliaburger's conversion losses, volume locality and upgrade restrictions.

Sources: [Pods](https://kubernetes.io/docs/concepts/workloads/pods/) · [Services](https://kubernetes.io/docs/concepts/services-networking/service/) · [Claims](https://kubernetes.io/docs/concepts/storage/persistent-volumes/) · [Migration limits](https://github.com/reliaburger/reliaburger/blob/v0.2.2/docs/manual/09_kubernetes.md) · [Upgrade limits](https://github.com/reliaburger/reliaburger/blob/v0.2.2/docs/releasing.md).

**Follow an incident through the same interface.** `relish wtf --app web` correlates deployment and runtime findings; `relish path web --to redis` checks the network path between two apps. Missing evidence is reported as unknown or unavailable. These built-in tools make diagnosis part of the deployment platform.

Source: [Diagnostic commands and evidence labels](https://github.com/reliaburger/reliaburger/blob/v0.2.2/docs/manual/07_diagnostics.md).

## What was checked

The Kubernetes release reference is [v1.37.1](https://github.com/kubernetes/kubernetes/releases/tag/v1.37.1). Official documentation was checked on 15 October 2026 and may subsequently change. Reliaburger references pin the [v0.2.2](https://github.com/reliaburger/reliaburger/tree/v0.2.2) tag, the release the installer installs, re-checked on 21 October 2026. We checked command contracts and architectural limits, not comparative uptime, resource use, throughput or migration outcomes. The testing section was checked against Kubernetes' SIG Testing, SIG Release and SIG Scalability documents, etcd's robustness tests and OSS-Fuzz on 27 October 2026.

For a distribution that bundles Kubernetes installation choices, read the [k3s comparison](https://reliaburger.com/compare/k3s.html). For evidence grades and correction criteria, see the [comparison methodology](https://reliaburger.com/compare/methodology.html#method).

---

The Markdown copy of <https://reliaburger.com/compare/kubernetes.html>.
