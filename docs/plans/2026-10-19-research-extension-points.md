# Bend, don't fork: extension points for Reliaburger

Research note, 19 October 2026, written against main at `1e3f69d0`. The
maintainer agreed the recommendation the same day and scheduled it as
[0.2.4](https://github.com/reliaburger/reliaburger/milestone/19), before 0.3.0
(see [the 0.2.4 milestone](#the-024-milestone)). It answers the maintainer's question:
Kubernetes has CRDs and admission policies so that one missing feature doesn't
force you onto another platform. What would the Reliaburger equivalent be,
without giving up the idea?

## The short version

Kubernetes made everything extensible, and paid for it twice: once in
outages (an admission webhook that's down can stop the API server from
booting), and once in sprawl (a cluster of 2,000 CRDs needs its own capacity
planning). Nomad and Swarm went the other way and shipped almost no extension
points. Their users then built proxies in front of the API to get the hooks
they needed, or left.

The evidence on why people leave a simpler orchestrator is mostly anecdotal,
but it's consistent. The biggest reasons aren't features at all: ecosystem,
Helm charts, hiring, managed offerings and whether the vendor will be around.
Among real features, the ones that keep coming up are operators and stateful
apps, networking and ingress, autoscaling, RBAC and multi-tenancy, then
sidecars and admission policy. Reliaburger already has autoscaling, ingress,
RBAC-ish permissions and namespaces. What it lacks is a way for a platform
team to say "every app in `prod` must have a memory limit" and "fill in our
team label", sidecars, and a way to package a stateful app.

The recommendation is a small ladder, in this order:

1. **Policies in the council.** A new `[policy.*]` kind, stored in Raft,
   evaluated in-process on the leader in the one shared apply path. A policy
   can *deny* or *warn* with a CEL expression, and can *fill in* missing
   fields with a TOML fragment. It never overwrites what you wrote. No
   webhooks, so nothing outside the binary can stop the cluster from taking
   writes.
2. **A few built-in escape hatches** for the gaps the evidence names:
   sidecars (already planned for v2), stop hooks and grace periods, a gated
   allowlist of runtime knobs (capabilities, sysctls, devices), and an
   exec-based secret provider on each node.
3. **Templates instead of CRDs.** A `[template.*]` with typed parameters that
   expands into ordinary apps, jobs and volumes at apply time, plus a written
   contract for "bring your own controller": an API client with a scoped
   token that writes ordinary apps.
4. **Wasm only if 1–3 leave a documented gap.** A spike, not a commitment.

What we don't do: CRDs, an operator framework, synchronous admission
webhooks, dynamic libraries, scheduler plugins on the job hot path, or a
package manager with logic in it.

## 1. How Kubernetes does it, and what that costs

### The mechanisms

**CRDs and controllers.** A custom resource extends the Kubernetes API; pair
it with a controller and you get a declarative API for anything. The operator
pattern is exactly that, plus domain knowledge in the controller
([Kubernetes, custom resources](https://kubernetes.io/docs/concepts/extend-kubernetes/api-extension/custom-resources/)).
The same page warns that "creating too many custom resources may overload your
API server's storage space", and that CRD bundles usually ship a Deployment of
third-party code. Each CRD has one storage version. If schemas differ between
versions you need a conversion webhook, and a failing one "can disrupt read
and write access to the custom resources, including the ability to update or
delete the resources"
([CRD versioning](https://kubernetes.io/docs/tasks/extend-kubernetes/custom-resources/custom-resource-definition-versioning/)).
You can't drop an old version until every stored object has been migrated.

**Admission webhooks.** Mutating webhooks run first, in sequence; validating
webhooks run after schema validation, in parallel
([dynamic admission control](https://kubernetes.io/docs/reference/access-authn-authz/extensible-admission-controllers/),
[webhook good practices](https://kubernetes.io/docs/concepts/cluster-administration/admission-webhooks-good-practices/)).
The timeout is 1–30 s, 10 s by default, and the default `failurePolicy` is
`Fail`. `reinvocationPolicy: IfNeeded` calls a mutator again if a later one
changed the object, and "the number of additional invocations is not
guaranteed to be exactly one". Webhooks must declare `sideEffects`, and since
1.30 `matchConditions` lets you filter calls with up to 64 CEL conditions.
The 1.22 move from v1beta1 to v1 flipped the default failure policy from
`Ignore` to `Fail` and cut the default timeout from 30 s to 10 s
([deprecation guide](https://kubernetes.io/docs/reference/using-api/deprecation-guide/)).

**CEL policies in-process.** ValidatingAdmissionPolicy is "a declarative,
in-process alternative to validating admission webhooks", stable since 1.30
([VAP docs](https://kubernetes.io/docs/reference/access-authn-authz/validating-admission-policy/),
[KEP-3488](https://github.com/kubernetes/enhancements/blob/master/keps/sig-api-machinery/3488-cel-admission-control/kep.yaml)).
A policy only acts through a binding, with `Deny`, `Warn` or `Audit` actions.
MutatingAdmissionPolicy (KEP-3962) went alpha in 1.32, beta in 1.34 and stable
in 1.36; it mutates through server-side-apply configurations or JSON Patch
([KEP-3962](https://github.com/kubernetes/enhancements/tree/master/keps/sig-api-machinery/3962-mutating-admission-policies),
[MAP docs](https://kubernetes.io/docs/reference/access-authn-authz/mutating-admission-policy/)).
CEL expressions carry a cost model: deterministic cost units, a runtime
budget that halts evaluation, and an estimated worst-case cost that refuses
an expensive expression at write time
([CEL in Kubernetes](https://kubernetes.io/docs/reference/using-api/cel/)).
The project now says outright: "use CEL-based admission control when
possible"
([webhook good practices](https://kubernetes.io/docs/concepts/cluster-administration/admission-webhooks-good-practices/)).
That's the most useful single sentence in this note. Kubernetes itself
learned that out-of-process admission was the wrong default.

**Kyverno and Gatekeeper.** Both are admission webhooks plus controllers.
Kyverno "runs as a dynamic admission controller"
([how Kyverno works](https://kyverno.io/docs/introduction/how-kyverno-works/));
Gatekeeper runs OPA behind a validating and mutating webhook with
ConstraintTemplate CRDs
([Gatekeeper docs](https://open-policy-agent.github.io/gatekeeper/website/docs/)).
Admission control is OPA's top use: about half of the OPA survey's
respondents run it for cluster policy (a vendor-adjacent sample,
[OPA 2021 survey](https://blog.openpolicyagent.org/open-policy-agent-2021-survey-summary-e749bbd7b824)).

**Scheduler extensions.** The scheduling framework has thirteen extension
points, from PreEnqueue through Filter, Score, Reserve, Permit and Bind, and
plugins are "compiled directly into the scheduler"
([scheduling framework](https://kubernetes.io/docs/concepts/scheduling-eviction/scheduling-framework/)).
It replaced HTTP extenders, whose drawbacks KEP-624 lists: few extension
points, JSON marshalling on every call, no way to tell an extender the
scheduler aborted, and no access to the scheduler's cache
([KEP-624](https://github.com/kubernetes/enhancements/blob/master/keps/sig-scheduling/624-scheduling-framework/README.md),
[extender design](https://github.com/kubernetes/design-proposals-archive/blob/main/scheduling/scheduler_extender.md)).
You can also run a second scheduler, but a pod naming one that isn't running
"remains in 'Pending' state forever"
([multiple schedulers](https://kubernetes.io/docs/tasks/extend-kubernetes/configure-multiple-schedulers/)).

**Plugin interfaces at the node.** CNI for networks
([network plugins](https://kubernetes.io/docs/concepts/extend-kubernetes/compute-storage-net/network-plugins/)),
CRI for runtimes, where from 1.26 the kubelet won't register a node whose
runtime lacks CRI v1 ([CRI](https://kubernetes.io/docs/concepts/architecture/cri/)),
and CSI for storage, GA in 1.13
([CSI GA](https://kubernetes.io/blog/2019/01/15/container-storage-interface-ga/)).
Device plugins register over a gRPC socket and expose integer resources that
"cannot be overcommitted" and can't be shared between containers
([device plugins](https://kubernetes.io/docs/concepts/extend-kubernetes/compute-storage-net/device-plugins/)).
Dynamic Resource Allocation replaces much of that; the KEP and the 1.34 blog
say it went GA in 1.34, while the current concept page says stable since
1.35, so treat the exact version as uncertain
([KEP-4381](https://github.com/kubernetes/enhancements/blob/master/keps/sig-node/4381-dra-structured-parameters/kep.yaml),
[1.34 DRA blog](https://kubernetes.io/blog/2025/09/01/kubernetes-v1-34-dra-updates/),
[DRA concept page](https://kubernetes.io/docs/concepts/scheduling-eviction/dynamic-resource-allocation/)).

**Aggregated API servers.** The API server proxies a path to an extension
server, and discovery must round-trip within five seconds
([aggregation layer](https://kubernetes.io/docs/concepts/extend-kubernetes/api-extension/apiserver-aggregation/)).

### What it costs

**Webhooks put the control plane's availability in someone else's hands.**
GKE's documentation says it plainly: with fail-closed behaviour "the
availability of the Kubernetes control plane becomes dependent on the
availability of the webhook", and a webhook blocking ClusterRole updates can
stop the API server becoming healthy and block an upgrade
([GKE, optimise webhooks](https://docs.cloud.google.com/kubernetes-engine/docs/how-to/optimize-webhooks)).
Kubernetes' own guidance spends most of its length on avoiding deadlocks: a
webhook can block the pods it needs to start itself, so exclude its own
namespace and stay away from `kube-system`
([webhook good practices](https://kubernetes.io/docs/concepts/cluster-administration/admission-webhooks-good-practices/)).
The same page recommends that mutating webhooks fail *open*, because failing
closed rejects compliant requests during webhook downtime. So you choose
between an outage and unenforced policy.

The incidents bear this out:

- In February 2026 an upgrade from 1.32 to 1.34 put a kube-apiserver into a
  crash loop. Kyverno's fail-closed webhook rejected the default ServiceCIDR
  the API server creates at boot, and the usual fix (delete the webhook
  configuration) needs a running API server. Recovery meant restarting it
  with webhook admission disabled
  ([zwindler, "Kyverno killed my API server. Again."](https://blog.zwindler.fr/en/2026/02/26/kyverno-killed-my-api-server.-again.)).
- On GKE, losing every Gatekeeper pod at once while its webhook still
  intercepted `kube-system` left DaemonSet pods unable to start, nodes never
  Ready, and a pre-prod cluster unusable; finding the cause took over two
  weeks ([Heureka on DEV](https://dev.to/fourstepper/dealing-with-a-google-kubernetes-engine-cluster-outage-2kf1)).
- Jetstack's 2019 write-up describes a misconfigured OPA webhook that sent a
  GKE cluster's nodes into an endless destroy-and-recreate loop during a
  master upgrade. The original URL now redirects, so it's cited through the
  [Kubernetes failure stories index](https://k8s.af/) and a
  [mirror](https://rwxdash.notion.site/How-a-simple-admission-webhook-lead-to-a-cluster-outage-Jetstack-Blog-50a9f19912a94ad2b3b1d5e269c4dc81).
- Kyverno's troubleshooting page has a section called "API server is
  blocked", and its fix is to delete Kyverno's webhook configurations
  ([Kyverno troubleshooting](https://kyverno.io/docs/troubleshooting/)).
  Gatekeeper's emergency procedure is the same
  ([Gatekeeper emergency](https://open-policy-agent.github.io/gatekeeper/website/docs/emergency/)),
  which is why it defaults to `failurePolicy: Ignore`
  ([Gatekeeper, failing closed](https://open-policy-agent.github.io/gatekeeper/website/docs/failing-closed/)).
  A Trail of Bits audit rated that insecure default High severity
  ([Trail of Bits, OPA](https://trailofbits.com/library/opa/); seen in search
  results, the PDF wasn't opened).
- Reachability alone is a long list: cert-manager's webhook troubleshooting
  page covers private GKE clusters, custom CNIs on EKS, NetworkPolicies,
  Fargate port clashes and stale serving certificates
  ([cert-manager](https://cert-manager.io/docs/troubleshooting/webhook/));
  Istio's sidecar injector failing closed blocked pod creation in
  [istio#20890](https://github.com/istio/istio/issues/20890).

I didn't find a large company's published postmortem with a webhook as the
root cause. The stories above are smaller teams, vendor docs and the projects
themselves.

**Latency.** Every mutating call adds latency in series
([good practices](https://kubernetes.io/docs/concepts/cluster-administration/admission-webhooks-good-practices/)).
Kyverno's own load test measured p95 admission latency of about 161 ms at 25
virtual users and 783 ms at 100
([CNCF blog, Kyverno performance](https://www.cncf.io/blog/2024/02/09/the-need-for-speed-optimizing-kyvernos-performance/)).

**Sprawl.** Crossplane puts each CRD at about 3 MiB of API server memory, and
one AWS provider installs about 200 of them
([Crossplane, disabling unused resources](https://docs.crossplane.io/v2.1/guides/disabling-unused-managed-resources/)).
Upbound measured a little over 4 MiB per CRD; installing hundreds at once
spiked CPU because every CRD change rebuilt the aggregated OpenAPI spec, and
hosted control planes went unavailable for seconds to about an hour
([Upbound, thousands of CRDs](https://www.upbound.io/blog/scaling-kubernetes-to-thousands-of-crds)).
Installing about 680 CRDs "reliably" took a GKE control plane offline
([crossplane#2649](https://github.com/crossplane/crossplane/issues/2649)).
Fixing it took years of upstream work, ending in aggregated discovery, stable
in 1.30 ([KEP-3352](https://github.com/kubernetes/enhancements/blob/master/keps/sig-api-machinery/3352-aggregated-discovery/kep.yaml)).
And an aggregated API that's down can leave namespaces stuck in
`Terminating`, a thread that's been open since 2018
([kubernetes#60807](https://github.com/kubernetes/kubernetes/issues/60807)).

**Version skew.** Only three minors are maintained at once
([skew policy](https://kubernetes.io/releases/version-skew-policy/)). The 1.22
removal of v1beta1 CRDs and admission APIs, then PodSecurityPolicy in 1.25,
forced every add-on to move
([1.22 changes](https://kubernetes.io/blog/2021/07/14/upcoming-changes-in-kubernetes-1-22/),
[deprecation guide](https://kubernetes.io/docs/reference/using-api/deprecation-guide/)).
dockershim's removal in 1.24 did the same for runtimes
([1.24 release](https://kubernetes.io/blog/2022/05/03/kubernetes-1-24-release-announcement/#dockershim-removed-from-kubelet)).
Webhooks written against old API versions "might not be able to reconcile more
recent changes"
([good practices](https://kubernetes.io/docs/concepts/cluster-administration/admission-webhooks-good-practices/)).

**Security.** Whoever controls a webhook configuration "can control webhooks
that can read any object admitted to the cluster", and mutate it
([RBAC good practices](https://kubernetes.io/docs/concepts/security/rbac-good-practices/)).
CVE-2020-8561 let a webhook's responses redirect the API server into its
private network
([announcement](https://groups.google.com/g/kubernetes-security-announce/c/RV2IhwcrQsY)).

The lesson isn't "extensibility is bad". It's that the expensive parts are
*out-of-process synchronous calls on the write path* and *new reconciled
kinds in the shared store*. In-process, bounded policy (CEL) is the cheap
part, and Kubernetes is moving towards it.

## 2. Lighter models elsewhere

### Orchestrators

**Nomad** has real plugins, all out of process. Task drivers and device
plugins run under HashiCorp's go-plugin over gRPC, and tasks outlive their
driver: a crashed driver is relaunched and reattaches through `RecoverTask`
([Nomad plugins](https://developer.hashicorp.com/nomad/docs/concepts/plugins),
[task drivers](https://developer.hashicorp.com/nomad/docs/concepts/plugins/task-drivers),
[device plugins](https://developer.hashicorp.com/nomad/plugins/devices)).
Secret-provider and host-volume plugins use a simpler "common plugin
interface": a binary that does one thing and exits. CSI plugins run as
ordinary Nomad jobs
([Nomad CSI](https://developer.hashicorp.com/nomad/docs/architecture/storage/csi)),
and 1.10 added dynamic host volumes managed by a named plugin
([Nomad 1.10](https://hashicorp.com/blog/nomad-1-10-adds-dynamic-host-volumes-extended-oidc-support-and-more)).

Admission is where Nomad stopped. Internally there's a compiled-in chain of
`jobMutator` and `jobValidator` hooks that canonicalise jobs and add implied
constraints ([`job_endpoint_hooks.go`](https://github.com/hashicorp/nomad/blob/main/nomad/job_endpoint_hooks.go)),
but nothing external can join it. Sentinel policy is Enterprise only, with
advisory, soft-mandatory and hard-mandatory levels
([Nomad Sentinel](https://developer.hashicorp.com/nomad/docs/enterprise/sentinel),
[enforcement levels](https://developer.hashicorp.com/sentinel/docs/concepts/enforcement-levels)).
Issue [nomad#4735](https://github.com/hashicorp/nomad/issues/4735), "Admission
controllers", asked in 2018 for policy hooks and sidecar injection; its author
closed it in 2022 saying there wasn't an appetite to build it. The community
built NACP instead, a proxy in front of the Nomad API that mutates and
validates with OPA or webhooks
([NACP](https://github.com/mxab/nacp),
[announcement](https://discuss.hashicorp.com/t/nomad-admission-control-proxy/51896)).
That's what happens when the platform has no hook: someone puts one in front
of it, with none of the guarantees.

Nomad's engineering lead put the CRD question this way: "Nomad has chosen not
to implement CRDs/Controllers/Operators", while allowing that it could gain
"CRD-like capabilities someday" ([LWN](https://lwn.net/Articles/905803/)).
Its templating layer, Nomad Pack, is "like Helm" with Go templates
([nomad-pack](https://github.com/hashicorp/nomad-pack)).

**Docker Engine and Swarm.** Engine plugins (volume, network, authorization,
logging) are out-of-process daemons speaking JSON over HTTP
([plugin API](https://docs.docker.com/engine/extend/plugin_api/),
[legacy plugins](https://docs.docker.com/engine/extend/legacy_plugins/)).
The authorization plugin is a cautionary tale for any request-inspecting
hook. It sees only JSON bodies, only the first request of a stream, and
responses through a 64 KiB buffer, and it must "apply the same decoding
semantics as the daemon"
([authz plugins](https://docs.docker.com/engine/extend/plugins_authorization/)).
CVE-2024-41110 was a regression where a zero-length body reached the plugin
empty, so it approved what it would have denied; rated 9.9–10
([Docker advisory](https://www.docker.com/blog/docker-security-advisory-docker-engine-authz-plugin/)).
Swarm itself has no admission or scheduler hooks I could find, and its CSI
plugins must be installed by hand on every manager
([cluster volumes](https://github.com/moby/moby/blob/master/docs/cluster_volumes.md)).

**Fly.io** dropped Nomad's scheduler and exposed primitives instead: "the
basic primitives are directly exposed, and we just write code to configure
them the way we want", with failover and multi-region logic moved into the
client ([Fly, carving the scheduler out](https://fly.io/blog/carving-the-scheduler-out-of-our-orchestrator/)).
The Machines API is the extension point
([Machines API](https://docs.fly.io/machines/api),
[automation without Terraform](https://www.fly.io/docs/blueprints/infra-automation-without-terraform/)).
That's relevant: Reliaburger's API plus scoped tokens plus the event stream
can be the "operator" interface without any new kind.

**systemd** is the best model for overrides without forks. A drop-in
`foo.service.d/*.conf` merges over the vendor unit, with `/etc` beating
`/run` beating `/usr/lib`; generators translate foreign config into units at
boot and may not touch anything else
([systemd.unit](https://www.freedesktop.org/software/systemd/man/latest/systemd.unit.html),
[systemd.generator](https://www.freedesktop.org/software/systemd/man/latest/systemd.generator.html)).
Reliaburger's `_defaults.toml` is already this idea, client side.

### Proxies and plugin protocols

- **Caddy** modules are compiled in: a package registers itself in `init()`,
  and `xcaddy` builds a binary with the modules you choose
  ([extending Caddy](https://caddyserver.com/docs/extending-caddy),
  [build](https://caddyserver.com/docs/build)). Safe and fast, but every
  extension is a custom build. For us that's just "send a PR".
- **Envoy** offers every model at once. `ext_authz` and `ext_proc` call out
  over gRPC, failing closed by default with `failure_mode_allow` to flip it
  ([ext_authz](https://www.envoyproxy.io/docs/envoy/latest/api-v3/extensions/filters/http/ext_authz/v3/ext_authz.proto),
  [ext_proc](https://www.envoyproxy.io/docs/envoy/latest/configuration/http/http_filters/ext_proc_filter)).
  The Wasm filter is still "experimental", with only V8 in the official
  build ([Wasm filter](https://www.envoyproxy.io/docs/envoy/latest/configuration/http/http_filters/wasm_filter),
  [proxy-wasm spec](https://github.com/proxy-wasm/spec)). Its newer dynamic
  modules are native shared libraries behind a frozen C ABI with no sandbox
  ([dynamic modules](https://www.envoyproxy.io/docs/envoy/latest/intro/arch_overview/advanced/dynamic_modules)).
- **Traefik** interprets Go plugin source with Yaegi, or runs Wasm middleware
  over http-wasm, and calls the whole feature experimental
  ([Traefik extend](https://doc.traefik.io/traefik/v3.6/extend/extend-traefik/),
  [plugin catalogue](https://plugins.traefik.io/create),
  [Traefik v3 Wasm](https://traefik.io/blog/traefik-3-deep-dive-into-wasm-support-with-coraza-waf-plugin)).
- **go-plugin** runs plugins as subprocesses over gRPC; a plugin panic
  doesn't crash the host, and the handshake cookie "is not a security
  measure" ([go-plugin](https://github.com/hashicorp/go-plugin),
  [package docs](https://pkg.go.dev/github.com/hashicorp/go-plugin)).
  **Terraform's provider protocol** is the same idea with versioned protobuf:
  majors break compatibility, minors are additive
  ([plugin protocol](https://developer.hashicorp.com/terraform/plugin/terraform-plugin-protocol)).
  OpenTofu reuses it as far as I know, but I didn't confirm that from a
  primary page.

### Wasm hosts

The component model and WIT give typed interfaces between host and guest,
with WASI 0.2 stable since January 2024
([component model](https://component-model.bytecodealliance.org/)). WASI 0.3,
with native async, shipped on 11 June 2026, and Wasmtime 46+ enables it by
default ([WASI roadmap](https://wasi.dev/roadmap)). Wasmtime limits CPU by
fuel (deterministic, slower) or epochs (about 10% overhead,
non-deterministic) and memory through `ResourceLimiter`
([interrupting Wasm](https://docs.wasmtime.dev/examples-interrupting-wasm.html),
[ResourceLimiter](https://docs.rs/wasmtime/latest/wasmtime/trait.ResourceLimiter.html)).
Its security design and continuous OSS-Fuzz coverage are strong
([security](https://docs.wasmtime.dev/security.html),
[fuzzing](https://docs.wasmtime.dev/contributing-fuzzing.html)), but it isn't
free of serious bugs: CVE-2023-26489 was a Cranelift miscompile that let a
guest reach about 34 GB beyond its memory base
([GHSA-ff4p-7xrq-q5r8](https://github.com/bytecodealliance/wasmtime/security/advisories/GHSA-ff4p-7xrq-q5r8)),
and on 14 October 2026 the project published ten advisories at once, the
worst a High filesystem sandbox escape
([advisories](https://github.com/bytecodealliance/wasmtime/security/advisories)).
Embedding wasmtime means taking on its release cadence (version 48 at the
time of writing, [crate](https://docs.rs/crate/wasmtime/latest)) and its
advisories.

Real uses show Wasm works well for narrow, typed hooks:
[Kubewarden](https://docs.kubewarden.io/admission-controller/1.35/en/explanations/architecture.html)
ships admission policies as Wasm in OCI artifacts;
[Shopify Functions](https://shopify.dev/docs/apps/build/functions) run JSON
in, JSON out under an 11-million-instruction budget
([instruction counts](https://shopify.dev/docs/apps/build/functions/optimize-instruction-counts));
[Zellij](https://zellij.dev/documentation/plugins) has a permission model for
plugins ([permissions](https://zellij.dev/documentation/plugin-api-permissions))
and appears to have moved from wasmtime to the wasmi interpreter
([Cargo.toml](https://raw.githubusercontent.com/zellij-org/zellij/main/zellij-server/Cargo.toml));
[Spin](https://github.com/spinframework/spin) builds whole services on it.
[Extism](https://github.com/extism/extism) wraps wasmtime as a plugin system
with host SDKs in many languages.

### Rust plugin options

- **Dynamic libraries.** Rust's default layout makes no guarantees
  ([type layout](https://doc.rust-lang.org/reference/type-layout.html)), so a
  `dylib` only works with the same compiler and flags
  ([linkage](https://doc.rust-lang.org/reference/linkage.html)). A stable
  Rust ABI, crABI, is an open tracking issue since 2023
  ([rust#111423](https://github.com/rust-lang/rust/issues/111423)). Loading
  any library runs its init code with no type checking
  ([libloading](https://docs.rs/libloading/latest/libloading/struct.Library.html)).
  [abi_stable](https://github.com/rodrimati1992/abi_stable_crates) checks
  layouts at load time but hasn't released since October 2023
  ([crate](https://crates.io/crates/abi_stable));
  [stabby](https://github.com/ZettaScaleLabs/stabby) is active
  ([crate](https://crates.io/crates/stabby)). Either way, a native plugin
  shares Bun's address space and privileges: no sandbox at all. Bun runs as
  root on most nodes. That rules it out.
- **Out of process over gRPC** with [tonic](https://github.com/hyperium/tonic):
  crash isolation, any language, but a socket, a process to supervise, a
  version handshake and a new failure mode on every call.
- **Wasm** (above): sandboxed, typed with WIT, but a large dependency and an
  API we'd have to version.

### Policy engines

| Engine | Properties | In Rust | Fit |
|---|---|---|---|
| CEL | "linear time, mutation free, and not Turing-complete" ([cel-spec](https://github.com/google/cel-spec)); Kubernetes adds a cost model ([CEL in Kubernetes](https://kubernetes.io/docs/reference/using-api/cel/)) | [`cel`](https://crates.io/crates/cel) 0.15 ([cel-rust](https://github.com/cel-rust/cel-rust)), active; conformance level and cost tracking not stated | Expressions over a spec. Kubernetes users already know it from VAP |
| Rego | Datalog-inspired, recursion refused at compile time ([policy language](https://www.openpolicyagent.org/docs/policy-language), [recursion error](https://openpolicyagent.org/docs/errors/rego-recursion-error/rule-name-is-recursive)) | Microsoft's [regorus](https://github.com/microsoft/regorus), "mostly compliant" with OPA, 1.9–6.3 MB ([crate](https://crates.io/crates/regorus)) | Powerful, but a second language to learn and a big surface |
| Cedar | Built for automated reasoning with bounded latency; Lean proofs and differential testing ([cedar](https://github.com/cedar-policy/cedar), [cedar-spec](https://github.com/cedar-policy/cedar-spec), [paper](https://arxiv.org/abs/2407.01688)); CNCF sandbox since October 2025 ([CNCF](https://www.cncf.io/projects/cedar/)) | Native: [`cedar-policy`](https://crates.io/crates/cedar-policy) 4.14 | Authorisation (principal, action, resource), not spec validation or defaulting |
| CUE | The spec allows structural cycles that "can result in infinite structure or evaluation loops" ([CUE spec](https://cuelang.org/docs/reference/spec/)) | Go only; Rust through libcue, which warns of "constant churn and breakage" ([libcue](https://github.com/cue-lang/libcue), [cue-rs](https://docs.rs/cue-rs)) | No |

CEL wins for validation and conditions. Cedar is the better tool if we ever
replace `[permission.*]` with richer authorisation, and that's a separate
question.

## 3. What actually makes people pick something else

No survey asks "which missing feature made you switch orchestrator". The
evidence is migration write-ups and forum threads, so treat the ranking as
directional.

**Non-feature reasons dominate.** CNCF's 2025 survey puts Kubernetes in
production at 82% of container users and ranks culture as the top challenge
([CNCF 2025](https://www.cncf.io/announcements/2026/01/20/kubernetes-established-as-the-de-facto-operating-system-for-ai-as-production-use-hits-82-in-2025-cncf-annual-cloud-native-survey/)).
HashiCorp conceded that "it will be hard to replicate the ever-growing
ecosystem built around Kubernetes"
([HashiCorp, choosing orchestrators](https://www.hashicorp.com/blog/nomad-kubernetes-a-pragmatic-look-at-choosing-orchestrators)).
In the big Nomad thread on HN, the complaints are Helm charts, operators,
no managed Nomad, and policy and quotas behind Enterprise
([HN, Nomad vs Kubernetes](https://news.ycombinator.com/item?id=28875764)).
Figma left ECS partly so it could hire people with Kubernetes experience
([Figma](https://www.figma.com/blog/migrating-onto-kubernetes/)). Playtomic
left Swarm partly because Mirantis called Kubernetes its "primary orchestrator
going forward" ([Playtomic](https://dev.to/playtomic/time-to-say-goodbye-to-docker-swarm-2iej),
[The New Stack](https://thenewstack.io/mirantis-acquires-docker-enterprise/)),
though Mirantis later committed to Swarm until 2030
([Mirantis](https://www.mirantis.com/blog/mirantis-guarantees-long-term-support-for-swarm/)).
A homelab moved off Nomad purely over licensing: "There's no technical reason
to replace Nomad and Consul" ([mei-home](https://blog.mei-home.net/posts/k8s-migration-0-plan/),
after [HashiCorp's BSL change](https://www.hashicorp.com/blog/hashicorp-adopts-the-business-source-license)).
Extension points don't fix any of this. `relish import`/`export` and the
Apache licence do more.

**Feature reasons, roughly in order of how often they come up:**

1. **Operators and stateful apps.** Figma: "ECS doesn't support StatefulSets",
   and it couldn't run services defined by Helm charts
   ([Figma](https://www.figma.com/blog/migrating-onto-kubernetes/)). An AWX
   user couldn't port it to Nomad because it ships as a Kubernetes operator
   ([HashiCorp Discuss](https://discuss.hashicorp.com/t/operator-pattern-in-nomad/47556)).
   Data on Kubernetes found two thirds of its respondents use operators for
   data workloads ([Business Wire](https://www.businesswire.com/news/home/20211007005029/en/)).
2. **Networking, ingress, certificates.** Swarm's routing mesh hides the
   client IP, open since 2016 ([moby#25526](https://github.com/moby/moby/issues/25526));
   Nomad users wanted cert-manager and egress control
   ([HN](https://news.ycombinator.com/item?id=28875764)); Kodius found Swarm
   "isn't really made to be highly customizable"
   ([Kodius](https://kodius.com/blog/bare-metal-part-1-why-kubernetes-beat-docker-swarm-in-our-setup)).
3. **Autoscaling**, the top reason to leave Swarm
   ([Playtomic](https://dev.to/playtomic/time-to-say-goodbye-to-docker-swarm-2iej)).
   Reliaburger has it.
4. **RBAC and multi-tenancy**: "giving full access to the cluster to every
   team is risky" (Playtomic, above); Swarm's access control is "not even
   close" ([HN 2026](https://news.ycombinator.com/item?id=47039073)).
   Reliaburger has scoped tokens, permissions and namespaces.
5. **Sidecars.** Swarm has no pods, so projects like
   [Podlike](https://github.com/rycus86/podlike) emulate them; even Kubernetes
   took from 2019 to 1.33 to get native sidecar lifecycles right
   ([KEP-753](https://www.kubernetes.dev/resources/keps/753/)). Nomad already
   has `lifecycle { sidecar = true }`
   ([Nomad lifecycle](https://developer.hashicorp.com/nomad/docs/job-specification/lifecycle)).
6. **Admission and defaulting.** The Nomad request and proxy above
   ([nomad#4735](https://github.com/hashicorp/nomad/issues/4735),
   [NACP](https://github.com/mxab/nacp)), and Sentinel being paid-only
   ([Nomad Enterprise](https://developer.hashicorp.com/nomad/docs/enterprise)).
7. **Devices and runtime knobs.** Swarm's `--privileged` request is open
   since 2016 ([moby#24862](https://github.com/moby/moby/issues/24862)), as
   are devices and GPUs ([moby#24865](https://github.com/moby/moby/issues/24865),
   [swarmkit#1244](https://github.com/moby/swarmkit/issues/1244));
   `cap_add` took four years ([moby#25885](https://github.com/moby/moby/issues/25885)),
   and `--security-opt` still hadn't landed in 2021
   ([moby#25209](https://github.com/moby/moby/issues/25209)).

Thin evidence: secret backends (Nomad defers to Vault,
[HashiCorp](https://www.hashicorp.com/blog/a-kubernetes-user-s-guide-to-hashicorp-nomad-secret-management)),
storage drivers, and lifecycle hooks beyond probes and drain. Storage shows
up more as a reason to *leave* Kubernetes: Gitpod cites PVC attach timing,
noisy neighbours and startup latency, never a missing feature
([Gitpod/Ona](https://ona.com/stories/we-are-leaving-kubernetes)), and
Fly.io left Nomad over scheduling semantics
([Fly](https://fly.io/blog/carving-the-scheduler-out-of-our-orchestrator/)).
Koyeb went the other way, to Nomad, because a custom task driver made
Firecracker easy ([Koyeb](https://www.koyeb.com/blog/the-koyeb-serverless-engine-from-kubernetes-to-nomad-firecracker-and-kuma)).
trivago accepted Nomad's missing fine-grained rights "by design"
([Endler](https://endler.dev/2019/maybe-you-dont-need-kubernetes/)).

So the gaps that would actually rule Reliaburger out for a team are a policy
hook, sidecars, a way to package stateful apps, and a few runtime knobs. Not
custom schedulers, not CNI, not CSI.

## 4. Design options for Reliaburger

### Where things sit today

The code already has the right shape for in-process policy:

- **The apply path.** `apply_handler` in `src/bun/api/apply.rs` parses,
  validates (`Config::validate`, `validate_intrinsic`), authorises the token
  (`sesame::auth::authorize`, `authorize_workload`), binds images to digests
  against the trust policy (`bind_images`), then `cluster_apply` forwards to
  the leader. The leader validates against committed namespaces and turns the
  config into writes with `config_to_desired_writes` (`src/council/apply.rs`).
- **One write path for apply and GitOps.** `config_to_desired_writes` exists
  so that "the same config converges identically whether you apply it by hand
  or through git" (its module comment). Lettuce calls the same function from
  `src/lettuce/runner.rs`. A policy stage inserted just before it covers both
  by construction.
- **The council.** `RaftRequest` in `src/council/types.rs` has a variant per
  declarative kind (`AppSpec`, `NamespaceSpec`, `PermissionSpec`, …), and
  `apply_app_spec` in `src/council/state_machine.rs` stores the spec. The
  state machine also calls `config_to_desired_writes` itself when it records
  job applies and previews (lines 438 and 1347), which matters below.
- **Client-side defaulting.** `_defaults.toml` (`src/config/defaults.rs`)
  already merges defaults with "explicit workload fields win". That's a
  mutation policy, but only for whoever controls the directory, and anyone
  can bypass it with a different file.
- **Policy that already exists, scattered.** Image trust rules live per node
  in `node.toml` (`TrustPolicySection` in `src/config/node.rs`); host paths
  and host binaries are gated by node allowlists; namespaces carry quotas.
- **The scheduler.** `node_can_run` and `filter_nodes` in `src/meat/filter.rs`
  and `score_nodes` in `src/meat/score.rs` are plain functions over a
  `WorkloadRequirements` and the cluster cache.
- **The agent.** The `Grill` trait (`src/grill/mod.rs`) has runc, Apple and
  process implementations; secrets are decrypted at launch
  (`src/bun/agent/launch.rs`).
- **Outbound webhooks.** Alerts already post HMAC-signed webhooks
  (`src/mayo/webhook.rs`), and the 0.4.2 AI SRE work plans agent-ready ones.

### Option A: embedded policy (CEL) for validate and default at apply time

A new declarative kind, committed to Raft like a permission:

```toml
[policy.prod-needs-limits]
match = "app.namespace == 'prod'"          # CEL, optional
deny  = "!has(app.memory)"                 # CEL: true means refuse
message = "apps in prod need a memory limit"

[policy.no-latest]
deny = "app.image.endsWith(':latest')"
action = "warn"                            # deny (default) | warn | audit

[policy.team-label]
match = "app.namespace == 'payments'"
[policy.team-label.default]                # filled in only where absent
env.TEAM = "payments"
deploy.max_unavailable = 1
```

How it works:

- **One pass, fixed order.** The leader applies every matching `default`
  fragment in policy-name order, then runs built-in validation, then every
  `deny`. No reinvocation. Validation sees the final spec, as Kubernetes
  recommends.
- **Defaults never overwrite.** A fragment fills only absent fields, the same
  rule as `_defaults.toml`. Filling an absent field twice is a no-op, so the
  stage is idempotent by construction, which a property test can check. To
  forbid a value, deny it. That one restriction removes most of what makes
  mutation confusing: users always see their own values honoured.
- **Inputs.** CEL sees the app (or job) spec, its namespace, the caller's
  token name and role, and the source (`apply` or `gitops`). Nothing from the
  network, no clock, no other apps. The input is bounded and CEL has no
  unbounded loops, so evaluation is bounded by input size times expression
  size. We also cap the expression size when the policy is committed.
- **Visible.** `relish plan` and `apply --dry-run` show each filled field and
  the policy that filled it, and each denial and warning by name. A new
  `relish policy test -f policies.toml -f apps.toml` runs the same code
  offline in CI (the idea behind `kyverno test` and conftest).
- **Where it sits.** A new `src/policy/` module with one pure function,
  roughly `evaluate(&PolicySet, &Config, &Caller) -> Result<Evaluated, Denials>`.
  The leader calls it in `cluster_apply` and in the Lettuce runner, right
  before `config_to_desired_writes`. The state machine never evaluates
  policy. If it did, replaying the log under a newer policy, or a CEL crate
  upgrade, could change what a committed entry means. Policies change what
  the leader proposes, never how an entry applies.
- **Racing a policy change.** The leader records the policy generation it
  evaluated against in the write, and the state machine refuses the write if
  the generation has moved. That's the same fence Lettuce already uses with
  the Raft term. The apply retries.
- **GitOps.** A denied app becomes a refused change in the partial sync
  result, as quota refusals already are. One trap: Lettuce diffs git against
  desired state, so if we store only the defaulted spec, every sync sees a
  difference and re-applies. The fix is to store the declared spec beside the
  effective one (or the list of filled fields) and have Lettuce diff the
  declared one. The autoscaler override already sets the precedent of
  runtime values that git doesn't own.
- **Existing apps.** A new policy doesn't touch running apps. `relish policy
  audit` and a `relish wtf` row list the committed apps that would now be
  denied (Gatekeeper's audit, without a controller).
- **Break-glass.** Policies live in Raft and run in-process, so there's no
  external service whose absence blocks writes. A bad policy is removed with
  `relish policy delete`, which policies never match (they apply only to
  apps and jobs, never to policies, permissions or namespaces). Only `admin`
  can write policies, and every write is audited.

Trade-offs. This is the cheapest option and covers the most-cited admission
gap. CEL knowledge transfers from Kubernetes VAP. The risk is the Rust crate:
I couldn't confirm the `cel` crate's conformance level or whether it offers
cost tracking, so the first slice has to check its conformance suite and our
own size cap has to do the bounding. If CEL disappoints, a much smaller fixed
predicate language (field, operator, value) would cover the common policies,
less flexibly.

Mutation is limited on purpose. It can't rewrite an image registry or inject
a sidecar. Image rewriting belongs with the trust policy and the pull-through
cache, and sidecar injection waits for sidecars (option E).

Compatibility: new Raft variants bump `protocol` and `state` in
`src/compatibility.rs`, as every format change does before 1.0.

### Option B: Wasm extension modules

A module in Pickle, referenced by digest, implementing a WIT world such as
`admit(spec) -> result<spec, denial>` or `filter(workload, node) -> bool` and
`score(workload, node) -> u32`, with fuel limits and no WASI capabilities.

Where it would sit: admission next to option A's stage on the leader;
filter and score as one more predicate in `node_can_run` and a term in
`score_nodes`; lifecycle hooks in the agent.

For: any language, sandboxed, typed, Kubewarden proves it works for
admission. Against: wasmtime is a large dependency with its own advisory
stream (ten on one day this month), and binary size and compile time matter
to a project that soaks on a laptop. The WIT world becomes an API that
module authors depend on. Before 1.0 we'd break it freely, which makes
modules a poor deal for their authors. Scheduler hooks are worse. The leader
admits job chunks at 4,600 a second in the 0.2.0 qualification, and a module
call per candidate node per placement is a new cost on that path, plus a new
way for placement to differ between leaders if a module isn't deterministic.

Verdict: not now. Keep a spike in reserve, triggered only by a documented
gap that options A, C, D and E can't close.

### Option C: out-of-process hooks (webhook or exec)

A synchronous admission webhook is the Kubernetes failure mode in section 1,
and Reliaburger has a nastier variant: a webhook served by an app *inside*
the cluster can deny the deploy that would fix it, and the leader's apply
path would block on a network call. With fail-open the policy isn't a
policy. With fail-closed the cluster's writes depend on someone's service.

What's worth having instead:

- **Asynchronous hooks**: the existing signed alert webhooks, and the event
  stream and agent-ready webhooks planned for 0.4.2. A controller can react
  to "app applied" without being on the write path.
- **Node-local exec providers** with a tiny protocol: JSON on stdin, JSON on
  stdout, a timeout, an allowlist in `node.toml` like `allowed_binaries`.
  This is how Docker and git credential helpers and Nomad's "common plugin
  interface" work. The obvious first one is a secret provider: an env value
  such as `PROVIDER[vault:secret/db#password]` that the node resolves at
  launch through an allowlisted binary. The whitepaper defers external secret
  managers to v2; this would bring a narrow version forward without linking
  any vendor SDK. It fails closed (the instance doesn't start, with a clear
  reason in `relish wtf`), and affects only the apps that use it.

If a synchronous webhook is ever added, it should be namespace-scoped,
time out within a couple of seconds, fail closed, never match policies,
permissions or namespaces, and never be served by an app in a namespace it
matches. I'd rather not add it at all.

### Option D: templates instead of CRDs

A CRD plus a controller exists to turn one high-level object ("a Postgres
cluster") into many low-level ones and keep them right. Split those two jobs:

- **Expansion** is a template: typed parameters and a body of ordinary
  `[app.*]`, `[job.*]` and volume declarations, with `${param}` substitution
  and nothing else. No conditionals, no loops, no functions.

  ```toml
  [template.postgres]
  params = { name = "string", size = "string", replicas = { type = "int", default = 1 } }
  body = """
  [app.${name}]
  image = "postgres:17"
  replicas = ${replicas}
  [[app.${name}.volumes]]
  path = "/var/lib/postgresql/data"
  size = "${size}"
  """

  [use.orders-db]
  template = "postgres"
  name = "orders-db"
  size = "50Gi"
  ```

  The expansion happens at apply time, through the same compiler `relish`
  and Lettuce share, so what lands in Raft is ordinary apps. `relish plan`
  shows the expanded result. The council stores no new reconciled kind, and
  the scheduler and agent never learn templates exist.
- **Day-two logic** (failover, backups, version upgrades) is a controller
  you run yourself, as an app, with a token scoped to its namespace, that
  reads the API and event stream and writes ordinary apps. That's Fly's
  "expose primitives" model. What we owe controller authors is a written
  contract: stable-enough JSON, an `owner` on each resource like Lettuce's
  ownership set (so `relish apply` and GitOps don't fight a controller), and
  a scoped token. No SDK, no framework, no new kinds.

The risk is becoming Helm. The guard rails: no logic in templates, a
template is a TOML resource like any other, and no registry in the first
slice (templates come from files or git).

### Option E: a few built-in escape hatches

Some gaps are better closed directly than through an extension point:

- **Sidecars.** The whitepaper plans a `sidecar` field for v2. The evidence
  says this is one of the commonest reasons simple orchestrators lose. Bring
  it forward. Containers share the parent's network namespace and lifecycle,
  start before it and stop after it, which is the ordering Kubernetes needed
  KEP-753 to get right.
- **Stop hooks.** A `stop_signal`, a grace period and a `pre_stop` exec. Cheap,
  and they remove a class of "we need a custom image entrypoint" workarounds.
- **Runtime knobs.** A small `[app.x.runtime]` allowlist: added capabilities,
  namespaced sysctls, devices. Each node has to allow it in `node.toml`, and
  the token needs `host-exec`, the same double gate host paths use. This is
  the Swarm `--privileged`/`--device` lesson: years of open issues for flags
  a few teams need badly.
- **Placement expressions** only if label constraints prove too weak. A CEL
  predicate over node labels, compiled once per app, for apps only, never
  jobs.

## 5. Recommendation

Do options A, E, C (exec providers only) and D, in that order. Hold B as a
spike. Don't do synchronous webhooks.

**Explicitly not:**

- **No CRDs.** No user-defined kinds that the council stores and reconciles.
  Templates expand to existing kinds.
- **No operator framework or SDK.** Controllers are API clients.
- **No synchronous out-of-process call on the write path**, and none on the
  scheduler path.
- **No policy evaluation in the Raft state machine.** Policies shape
  proposals, not how entries apply.
- **No dynamic libraries.** No stable ABI, no sandbox, and Bun runs as root.
- **No mutation that overwrites user values**, and no reinvocation.
- **No CNI, CSI or CRI plug-ins.** The whitepaper's reasons hold, and the
  evidence doesn't show these as reasons to leave.
- **No Rego or CUE dependency**, no general scripting (Lua, Rhai, JavaScript)
  in Bun.
- **No template registry or logic in templates** in the first slices.
- **No new plugin binaries for `relish`.** The CLI design keeps "no plugins";
  `--output json` and the API are the extension point.

| Option | Apply path | Council | Scheduler | Agent |
|---|---|---|---|---|
| A. Policies | evaluated on the leader before `config_to_desired_writes`, in `cluster_apply` and the Lettuce runner | `[policy.*]` kind, policy generation fence, declared spec stored beside effective | none | none |
| C. Exec providers | none | none | node advertises providers as a capability, `node_can_run` checks it | resolved at launch in `launch.rs` |
| D. Templates | expanded in the shared compiler before policy | stores expanded apps plus provenance | none | none |
| E. Escape hatches | validated with the app | new spec fields | sidecars add to requests | `Grill`, supervisor, OCI spec |
| B. Wasm (held) | beside A | module digests | `node_can_run`, `score_nodes` | lifecycle hooks |

## 6. A phased proposal

The phases are sized so the first can stand alone. Phases 1–3 ship together
as 0.2.4 (below); phase 4 stays gated.

**Phase 1 (the headline): "Your rules, in the cluster."**

- Slice 1a: `[policy.*]` with `deny` and `warn`, CEL, apps only. Leader-side
  evaluation in both apply paths, the generation fence, `relish policy
  list/delete`, `relish policy test` offline, denials in `relish plan`, apply
  output and the GitOps partial result, an audit entry for every policy
  write. Protocol and state bump.
- Slice 1b: `default` fragments, the declared-versus-effective spec split and
  the Lettuce diff on the declared spec, filled fields shown in `relish plan`.
- Slice 1c: jobs, `action = "audit"`, `relish policy audit` and the `wtf` row.
- Exit test: the four policies platform teams ask for most (memory limits in
  a namespace, no `:latest`, images only from listed registries, a required
  team env var defaulted) expressed in TOML, enforced through both `relish
  apply` and GitOps, surviving a leader kill mid-apply, and removable by an
  admin when a broken policy denies everything.

**Phase 2: the escape hatches.** Sidecars, stop hooks, the gated runtime
allowlist. Each is a spec field with its own tests; sidecars probably deserve
their own minor release.

**Phase 3: providers and templates.** The exec secret provider and its
protocol; `[template.*]` and `[use.*]`; the controller contract (`owner` on
resources, scoped token, documented JSON). Exit test: a Postgres template
plus a small external controller that fails over a replica, written against
the public API only.

**Phase 4 (gated): the Wasm spike.** Only with a named gap from users that
phases 1–3 don't close. Measure binary size, cold-start and per-call cost on
the release laptop, and the advisory load, before any commitment.

### Test strategy

Tests first, as always:

- **Unit tests in `src/policy/`**: a table of specs and policies with the
  expected decision; every category of bad policy refused at commit
  (unparseable CEL, an unknown field, an expression over the size cap, a
  `default` fragment naming a field that doesn't exist, a policy that tries
  to match policies).
- **Property tests (`proptest`)**: defaulting is idempotent
  (`evaluate(evaluate(x)) == evaluate(x)`); a default never changes a field
  the user set; a denied config produces no writes; evaluation order doesn't
  depend on map iteration order.
- **Snapshot tests (`insta`)**: `relish plan` and `relish policy test` output
  with filled fields and denials.
- **Portable integration tests in `tests/suite/`**: a denied apply leaves no
  Raft write and an audit entry; a GitOps commit with one denied app applies
  the rest and names the policy; a policy changed between evaluation and
  commit makes the write retry rather than land under the old policy;
  repeated syncs of a defaulted app don't re-apply (the diff trap).
- **Gated cluster tests (`make test-cluster`)**: a leader kill during an
  apply under policy; a new leader enforcing the same policy set; a policy
  that denies everything removed by an admin.
- **Smoker**: for exec providers, delay, kill and fill the disk under the
  provider binary and check the instance fails closed with a reason in
  `wtf`, and that unrelated apps keep starting.
- **Soak**: add a handful of policies to the V02 workload so the soak covers
  evaluation under faults. The apply path runs off the agent loop, so the
  turn budget shouldn't move; the soak checks that it doesn't.
- **Bench**: evaluation time per app for 10, 100 and 1,000 policies, so the
  cost of policy on apply and GitOps sync is a number, not a guess.

The book gets a chapter for phase 1 (policies sit across chapter 7's apply
and GitOps path and chapter 10's security model), with the Rust angle being
a pure function inserted into a shared path, and why the state machine must
stay deterministic.

## The 0.2.4 milestone

Decision (maintainer, 19 October 2026): land phases 1–3 before 0.3.0, as
[0.2.4](https://github.com/reliaburger/reliaburger/milestone/19), so the
extension points exist before real fleets arrive with the appliance OS. The
book chapter comes first, because the design needs the context in this note
to follow.

| Issue | What | Option |
|---|---|---|
| [#1116](https://github.com/reliaburger/reliaburger/issues/1116) | A new book chapter on extension points | all |
| [#1117](https://github.com/reliaburger/reliaburger/issues/1117) | `[policy.*]` deny and warn with CEL, leader-side, generation fence | A, slice 1a |
| [#1118](https://github.com/reliaburger/reliaburger/issues/1118) | Fill-only defaults, declared spec beside effective | A, slice 1b |
| [#1119](https://github.com/reliaburger/reliaburger/issues/1119) | Jobs, the audit action, `relish policy audit`, a `wtf` row | A, slice 1c |
| [#1120](https://github.com/reliaburger/reliaburger/issues/1120) | Sidecars | E |
| [#1121](https://github.com/reliaburger/reliaburger/issues/1121) | Stop hooks | E |
| [#1122](https://github.com/reliaburger/reliaburger/issues/1122) | Gated runtime knobs | E |
| [#1123](https://github.com/reliaburger/reliaburger/issues/1123) | Exec secret provider | C |
| [#1124](https://github.com/reliaburger/reliaburger/issues/1124) | `[template.*]` and `[use.*]` | D |
| [#1125](https://github.com/reliaburger/reliaburger/issues/1125) | The controller contract | D |
| [#1126](https://github.com/reliaburger/reliaburger/issues/1126) | The 0.2.4 exit test | — |
| [#1127](https://github.com/reliaburger/reliaburger/issues/1127) | Wasm spike, gated, in Later | B |

## What I'm unsure about

- **The `cel` crate.** Active and widely downloaded, but I couldn't confirm
  its conformance level or any cost tracking. The first slice must check.
- **Demand.** The evidence for admission and defaulting as a reason to leave
  is real but medium-strength; operators and the Helm ecosystem are stronger,
  and templates only partly answer them.
- **Mutation scope.** Fill-only defaults may prove too weak (registry
  rewriting is a common Kyverno use). I'd rather start narrow and widen with
  evidence.
- **Several source facts came through summaries.** Quotes from Gitpod,
  Figma, Playtomic and the HN threads should be checked against the pages
  before they go into the book. A few items (37signals, Conductor, Internet
  Archive, ZenHub, Plaid) were seen only in search results and aren't relied
  on here.

## Sources

Kubernetes documentation and KEPs

- https://kubernetes.io/docs/concepts/extend-kubernetes/
- https://kubernetes.io/docs/concepts/extend-kubernetes/api-extension/custom-resources/
- https://kubernetes.io/docs/tasks/extend-kubernetes/custom-resources/custom-resource-definition-versioning/
- https://kubernetes.io/docs/reference/access-authn-authz/extensible-admission-controllers/
- https://kubernetes.io/docs/concepts/cluster-administration/admission-webhooks-good-practices/
- https://kubernetes.io/docs/reference/access-authn-authz/validating-admission-policy/
- https://github.com/kubernetes/enhancements/blob/master/keps/sig-api-machinery/3488-cel-admission-control/kep.yaml
- https://github.com/kubernetes/enhancements/tree/master/keps/sig-api-machinery/3962-mutating-admission-policies
- https://kubernetes.io/docs/reference/access-authn-authz/mutating-admission-policy/
- https://kubernetes.io/docs/reference/using-api/cel/
- https://kubernetes.io/docs/reference/using-api/deprecation-guide/
- https://kubernetes.io/docs/reference/instrumentation/metrics/
- https://kubernetes.io/docs/concepts/scheduling-eviction/scheduling-framework/
- https://github.com/kubernetes/enhancements/blob/master/keps/sig-scheduling/624-scheduling-framework/README.md
- https://github.com/kubernetes/design-proposals-archive/blob/main/scheduling/scheduler_extender.md
- https://kubernetes.io/docs/tasks/extend-kubernetes/configure-multiple-schedulers/
- https://kubernetes.io/docs/concepts/extend-kubernetes/compute-storage-net/network-plugins/
- https://kubernetes.io/docs/concepts/architecture/cri/
- https://kubernetes.io/blog/2022/05/03/kubernetes-1-24-release-announcement/#dockershim-removed-from-kubelet
- https://kubernetes.io/blog/2019/01/15/container-storage-interface-ga/
- https://kubernetes.io/docs/concepts/extend-kubernetes/compute-storage-net/device-plugins/
- https://github.com/kubernetes/enhancements/blob/master/keps/sig-node/4381-dra-structured-parameters/kep.yaml
- https://kubernetes.io/blog/2025/09/01/kubernetes-v1-34-dra-updates/
- https://kubernetes.io/docs/concepts/scheduling-eviction/dynamic-resource-allocation/
- https://kubernetes.io/docs/concepts/extend-kubernetes/api-extension/apiserver-aggregation/
- https://github.com/kubernetes/enhancements/blob/master/keps/sig-api-machinery/3352-aggregated-discovery/kep.yaml
- https://github.com/kubernetes/kubernetes/issues/60807
- https://kubernetes.io/releases/version-skew-policy/
- https://kubernetes.io/blog/2021/07/14/upcoming-changes-in-kubernetes-1-22/
- https://kubernetes.io/docs/concepts/security/rbac-good-practices/
- https://groups.google.com/g/kubernetes-security-announce/c/RV2IhwcrQsY
- https://www.kubernetes.dev/resources/keps/753/

Kubernetes policy engines, incidents and scale

- https://kyverno.io/docs/introduction/how-kyverno-works/
- https://kyverno.io/docs/troubleshooting/
- https://www.cncf.io/blog/2024/02/09/the-need-for-speed-optimizing-kyvernos-performance/
- https://open-policy-agent.github.io/gatekeeper/website/docs/
- https://open-policy-agent.github.io/gatekeeper/website/docs/failing-closed/
- https://open-policy-agent.github.io/gatekeeper/website/docs/emergency/
- https://trailofbits.com/library/opa/
- https://blog.openpolicyagent.org/open-policy-agent-2021-survey-summary-e749bbd7b824
- https://blog.zwindler.fr/en/2026/02/26/kyverno-killed-my-api-server.-again.
- https://dev.to/fourstepper/dealing-with-a-google-kubernetes-engine-cluster-outage-2kf1
- https://k8s.af/
- https://rwxdash.notion.site/How-a-simple-admission-webhook-lead-to-a-cluster-outage-Jetstack-Blog-50a9f19912a94ad2b3b1d5e269c4dc81
- https://docs.cloud.google.com/kubernetes-engine/docs/how-to/optimize-webhooks
- https://cert-manager.io/docs/troubleshooting/webhook/
- https://github.com/istio/istio/issues/20890
- https://docs.crossplane.io/v2.1/guides/disabling-unused-managed-resources/
- https://www.upbound.io/blog/scaling-kubernetes-to-thousands-of-crds
- https://github.com/crossplane/crossplane/issues/2649

Nomad, Docker, Fly.io, systemd

- https://developer.hashicorp.com/nomad/docs/concepts/plugins
- https://developer.hashicorp.com/nomad/docs/concepts/plugins/task-drivers
- https://developer.hashicorp.com/nomad/plugins/devices
- https://developer.hashicorp.com/nomad/docs/architecture/storage/csi
- https://hashicorp.com/blog/nomad-1-10-adds-dynamic-host-volumes-extended-oidc-support-and-more
- https://developer.hashicorp.com/nomad/docs/enterprise/sentinel
- https://developer.hashicorp.com/sentinel/docs/concepts/enforcement-levels
- https://developer.hashicorp.com/nomad/docs/enterprise
- https://github.com/hashicorp/nomad/blob/main/nomad/job_endpoint_hooks.go
- https://github.com/hashicorp/nomad/issues/4735
- https://github.com/mxab/nacp
- https://discuss.hashicorp.com/t/nomad-admission-control-proxy/51896
- https://github.com/hashicorp/nomad-pack
- https://developer.hashicorp.com/nomad/docs/job-specification/lifecycle
- https://lwn.net/Articles/905803/
- https://discuss.hashicorp.com/t/operator-pattern-in-nomad/47556
- https://www.hashicorp.com/blog/a-kubernetes-user-s-guide-to-hashicorp-nomad-secret-management
- https://docs.docker.com/engine/extend/plugin_api/
- https://docs.docker.com/engine/extend/legacy_plugins/
- https://docs.docker.com/engine/extend/plugins_authorization/
- https://www.docker.com/blog/docker-security-advisory-docker-engine-authz-plugin/
- https://github.com/moby/moby/blob/master/docs/cluster_volumes.md
- https://docs.fly.io/machines/api
- https://fly.io/blog/carving-the-scheduler-out-of-our-orchestrator/
- https://www.fly.io/docs/blueprints/infra-automation-without-terraform/
- https://www.freedesktop.org/software/systemd/man/latest/systemd.unit.html
- https://www.freedesktop.org/software/systemd/man/latest/systemd.generator.html

Proxies, plugin protocols, Wasm and Rust

- https://caddyserver.com/docs/extending-caddy
- https://caddyserver.com/docs/build
- https://www.envoyproxy.io/docs/envoy/latest/api-v3/extensions/filters/http/ext_authz/v3/ext_authz.proto
- https://www.envoyproxy.io/docs/envoy/latest/configuration/http/http_filters/ext_proc_filter
- https://www.envoyproxy.io/docs/envoy/latest/configuration/http/http_filters/wasm_filter
- https://www.envoyproxy.io/docs/envoy/latest/intro/arch_overview/advanced/dynamic_modules
- https://github.com/proxy-wasm/spec
- https://doc.traefik.io/traefik/v3.6/extend/extend-traefik/
- https://plugins.traefik.io/create
- https://traefik.io/blog/traefik-3-deep-dive-into-wasm-support-with-coraza-waf-plugin
- https://github.com/hashicorp/go-plugin
- https://pkg.go.dev/github.com/hashicorp/go-plugin
- https://developer.hashicorp.com/terraform/plugin/terraform-plugin-protocol
- https://component-model.bytecodealliance.org/
- https://wasi.dev/roadmap
- https://docs.wasmtime.dev/examples-interrupting-wasm.html
- https://docs.rs/wasmtime/latest/wasmtime/trait.ResourceLimiter.html
- https://docs.rs/crate/wasmtime/latest
- https://docs.wasmtime.dev/security.html
- https://docs.wasmtime.dev/contributing-fuzzing.html
- https://github.com/bytecodealliance/wasmtime/security/advisories/GHSA-ff4p-7xrq-q5r8
- https://github.com/bytecodealliance/wasmtime/security/advisories
- https://github.com/extism/extism
- https://docs.kubewarden.io/admission-controller/1.35/en/explanations/architecture.html
- https://zellij.dev/documentation/plugins
- https://zellij.dev/documentation/plugin-api-permissions
- https://raw.githubusercontent.com/zellij-org/zellij/main/zellij-server/Cargo.toml
- https://shopify.dev/docs/apps/build/functions
- https://shopify.dev/docs/apps/build/functions/optimize-instruction-counts
- https://github.com/spinframework/spin
- https://doc.rust-lang.org/reference/type-layout.html
- https://doc.rust-lang.org/reference/linkage.html
- https://github.com/rust-lang/rust/issues/111423
- https://docs.rs/libloading/latest/libloading/struct.Library.html
- https://github.com/rodrimati1992/abi_stable_crates
- https://crates.io/crates/abi_stable
- https://github.com/ZettaScaleLabs/stabby
- https://crates.io/crates/stabby
- https://github.com/hyperium/tonic

Policy languages

- https://github.com/google/cel-spec
- https://github.com/cel-rust/cel-rust
- https://crates.io/crates/cel
- https://www.openpolicyagent.org/docs/policy-language
- https://openpolicyagent.org/docs/errors/rego-recursion-error/rule-name-is-recursive
- https://github.com/microsoft/regorus
- https://crates.io/crates/regorus
- https://github.com/cedar-policy/cedar
- https://github.com/cedar-policy/cedar-spec
- https://arxiv.org/abs/2407.01688
- https://crates.io/crates/cedar-policy
- https://www.cncf.io/projects/cedar/
- https://cuelang.org/docs/reference/spec/
- https://github.com/cue-lang/libcue
- https://docs.rs/cue-rs

Adoption and migration evidence

- https://www.cncf.io/announcements/2026/01/20/kubernetes-established-as-the-de-facto-operating-system-for-ai-as-production-use-hits-82-in-2025-cncf-annual-cloud-native-survey/
- https://www.hashicorp.com/blog/nomad-kubernetes-a-pragmatic-look-at-choosing-orchestrators
- https://news.ycombinator.com/item?id=28875764
- https://www.figma.com/blog/migrating-onto-kubernetes/
- https://dev.to/playtomic/time-to-say-goodbye-to-docker-swarm-2iej
- https://thenewstack.io/mirantis-acquires-docker-enterprise/
- https://www.mirantis.com/blog/mirantis-guarantees-long-term-support-for-swarm/
- https://blog.mei-home.net/posts/k8s-migration-0-plan/
- https://www.hashicorp.com/blog/hashicorp-adopts-the-business-source-license
- https://www.businesswire.com/news/home/20211007005029/en/
- https://github.com/moby/moby/issues/25526
- https://kodius.com/blog/bare-metal-part-1-why-kubernetes-beat-docker-swarm-in-our-setup
- https://news.ycombinator.com/item?id=47039073
- https://github.com/rycus86/podlike
- https://github.com/moby/moby/issues/24862
- https://github.com/moby/moby/issues/24865
- https://github.com/moby/swarmkit/issues/1244
- https://github.com/moby/moby/issues/25885
- https://github.com/moby/moby/issues/25209
- https://ona.com/stories/we-are-leaving-kubernetes
- https://www.koyeb.com/blog/the-koyeb-serverless-engine-from-kubernetes-to-nomad-firecracker-and-kuma
- https://endler.dev/2019/maybe-you-dont-need-kubernetes/
