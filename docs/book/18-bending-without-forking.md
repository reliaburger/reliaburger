# Bending Without Forking

Picture a platform team that likes Reliaburger. One binary, no YAML sprawl, a
cluster up in minutes. Then the security review asks a simple question: can you
stop anyone deploying to `prod` without a memory limit? And the answer is no.
Not "no, but here's a flag". Just no. Somebody suggests a wrapper script around
`relish apply`, somebody else points out that GitOps doesn't go through the
wrapper, and a week later the team is back on Kubernetes, where a policy engine
is one Helm chart away.

That's the problem this chapter is about. Kubernetes has CRDs and admission
webhooks so that one missing feature doesn't force you onto another platform.
They work. They also cost a great deal, and much of what makes Reliaburger
simple comes from not paying that cost. So the question for 0.2.4 is narrow:
what's the smallest set of extension points that lets a team bend Reliaburger
to its rules, without forking it and without turning it into Kubernetes?

This chapter is unusual for the book: we wrote it before the code. The design
needs more context than any of the issues can carry, and the context is mostly
other people's mistakes. The research behind it is in the
[extension points note](../plans/2026-10-19-research-extension-points.md), and
the order of the work is in the [0.2.4 plan](../plans/2026-10-23-plan-0.2.4.md).
The sections near the end that show the Rust and the tests are placeholders
for now. Each issue on the 0.2.4 train fills in its part as it lands.

Here's the answer, up front, so you know where we're going:

1. **Policies in the council.** A new `[policy.*]` kind, stored in Raft and
   evaluated on the leader, in the one function both `relish apply` and GitOps
   already share. A policy can deny or warn with a CEL expression, and fill in
   fields you left out. It never overwrites what you wrote.
2. **A few built-in escape hatches** for the gaps that push teams away:
   sidecars, stop hooks and a gated list of runtime knobs, plus an exec-based
   secret provider on each node.
3. **Templates instead of CRDs.** A `[template.*]` that expands into ordinary
   apps at apply time, and a written contract for running your own controller.

And what we don't do: no webhooks on the write path, no CRDs, no dynamic
libraries, and no Wasm, for now.

## What extensibility cost Kubernetes

Kubernetes lets you extend almost everything. That's not an accident, and it's
not a mistake. But the bill came in two parts, and it's worth reading both
before we design anything.

### Webhooks put your control plane in someone else's hands

An admission webhook is an HTTP call the API server makes on every matching
write. Mutating webhooks run first, one after another; validating webhooks run
after schema validation, in parallel
([dynamic admission control](https://kubernetes.io/docs/reference/access-authn-authz/extensible-admission-controllers/)).
Each call has a timeout between 1 and 30 seconds, 10 by default, and the
default failure policy is `Fail`. So if the webhook doesn't answer, the write
doesn't happen.

Google's GKE documentation puts the consequence in one sentence: "When a
webhook is configured to fail closed, the availability of the Kubernetes
control plane becomes dependent on the availability of the webhook"
([GKE, optimise webhooks](https://docs.cloud.google.com/kubernetes-engine/docs/how-to/optimize-webhooks)).
The same page warns that a webhook which blocks ClusterRole updates can stop
the API server becoming healthy, and block an upgrade.

Can you see the trap? A webhook is usually served by pods inside the cluster.
If those pods die, the webhook can block the very writes that would restart
them. Kubernetes' own
[webhook good practices](https://kubernetes.io/docs/concepts/cluster-administration/admission-webhooks-good-practices/)
spends much of its length on avoiding exactly this deadlock: exclude the
webhook's own namespace, stay away from `kube-system`. And it recommends that
mutating webhooks fail *open*, because failing closed rejects compliant
requests while the webhook is down. So you get to pick. Either an outage, or a
policy that isn't enforced when it matters.

The incidents bear this out:

- In February 2026 an upgrade from 1.32 to 1.34 put a kube-apiserver into a
  crash loop. Kyverno's fail-closed webhook rejected the default ServiceCIDR
  that the API server creates at boot. The usual fix, deleting the webhook
  configuration, needs a running API server. Recovery meant restarting it with
  webhook admission switched off
  ([zwindler, "Kyverno killed my API server. Again."](https://blog.zwindler.fr/en/2026/02/26/kyverno-killed-my-api-server.-again.)).
- On GKE, losing every Gatekeeper pod at once, while its webhook still
  intercepted `kube-system`, left DaemonSet pods unable to start and nodes
  never Ready. Finding the cause took more than two weeks
  ([Heureka on DEV](https://dev.to/fourstepper/dealing-with-a-google-kubernetes-engine-cluster-outage-2kf1)).
- Kyverno's troubleshooting guide has a section called "API server is
  blocked", and the fix is to delete Kyverno's webhook configurations
  ([Kyverno troubleshooting](https://kyverno.io/docs/troubleshooting/)).
  Gatekeeper's emergency procedure is the same
  ([Gatekeeper emergency](https://open-policy-agent.github.io/gatekeeper/website/docs/emergency/)),
  which is why Gatekeeper ships failing open by default
  ([Gatekeeper, failing closed](https://open-policy-agent.github.io/gatekeeper/website/docs/failing-closed/)).

These are small teams and the projects' own documentation, not big-company
postmortems. We didn't find one of those with a webhook as the root cause. But
when two policy engines both document "how to get your API server back from
us", that tells you something.

Webhooks also cost time on every write. Each mutating call adds latency in
series, and Kyverno's own load test measured p95 admission latency of about
161 ms at 25 virtual users and 783 ms at 100
([CNCF blog, Kyverno performance](https://www.cncf.io/blog/2024/02/09/the-need-for-speed-optimizing-kyvernos-performance/)).
And whoever controls a webhook configuration can read, and change, every
object it admits
([RBAC good practices](https://kubernetes.io/docs/concepts/security/rbac-good-practices/)).

Now the punchline. Kubernetes itself moved away from this model.
ValidatingAdmissionPolicy is a declarative, in-process alternative to
validating webhooks, written in CEL and stable since 1.30
([VAP docs](https://kubernetes.io/docs/reference/access-authn-authz/validating-admission-policy/)),
and the good-practices page now says: "The Kubernetes project recommends that
you use CEL-based admission control when possible"
([webhook good practices](https://kubernetes.io/docs/concepts/cluster-administration/admission-webhooks-good-practices/)).
That's the most useful sentence in this whole chapter. The project that
invented out-of-process admission learned it was the wrong default.

### CRDs turn your API server into everyone's database

A custom resource definition adds a new kind to the Kubernetes API. Pair it
with a controller and you get a declarative API for anything: that's the
operator pattern
([custom resources](https://kubernetes.io/docs/concepts/extend-kubernetes/api-extension/custom-resources/)).
It's a lovely idea. Then you install a cloud provider.

Crossplane puts each CRD at roughly 3 MiB of API server memory, and one AWS
provider installs about 200 of them
([Crossplane, disabling unused resources](https://docs.crossplane.io/v2.1/guides/disabling-unused-managed-resources/)).
Upbound measured a little over 4 MiB per CRD, and found that installing
hundreds at once spiked CPU, because every CRD change rebuilt the aggregated
OpenAPI spec. Hosted control planes went unavailable for anything from seconds
to about an hour
([Upbound, thousands of CRDs](https://www.upbound.io/blog/scaling-kubernetes-to-thousands-of-crds)).
A Crossplane issue reports that installing around 680 CRDs took a GKE control
plane offline
([crossplane#2649](https://github.com/crossplane/crossplane/issues/2649)).
Fixing that took years of upstream work, ending in aggregated discovery, stable
in 1.30
([KEP-3352](https://github.com/kubernetes/enhancements/blob/master/keps/sig-api-machinery/3352-aggregated-discovery/kep.yaml)).

Versions are the other half. Each CRD has one storage version, and if schemas
differ between versions you need a conversion webhook. A failing one can cut
off reads and writes to every object of that kind, including the ability to
delete them
([CRD versioning](https://kubernetes.io/docs/tasks/extend-kubernetes/custom-resources/custom-resource-definition-versioning/)).
So the webhook problem comes back in through the side door.

### Version skew makes every add-on move with you

Kubernetes maintains three minor releases at a time
([skew policy](https://kubernetes.io/releases/version-skew-policy/)). When 1.22
removed the v1beta1 CRD and admission APIs, every add-on built on them had to
move
([1.22 changes](https://kubernetes.io/blog/2021/07/14/upcoming-changes-in-kubernetes-1-22/),
[deprecation guide](https://kubernetes.io/docs/reference/using-api/deprecation-guide/)).
dockershim's removal in 1.24 did the same for runtimes
([1.24 release](https://kubernetes.io/blog/2022/05/03/kubernetes-1-24-release-announcement/#dockershim-removed-from-kubelet)).
And even a webhook that keeps working can drift: "webhooks that worked as
expected with earlier versions of an API might not be able to reconcile more
recent changes to that API"
([webhook good practices](https://kubernetes.io/docs/concepts/cluster-administration/admission-webhooks-good-practices/)).

Reliaburger has its own version of this, and it's worse. Before 1.0.0 we don't
keep backwards compatibility at all (see
[cluster compatibility](../releasing.md#compatibility-before-100)). Any API we
hand to plugin authors would break on their heads several times a year.

### So what's the lesson?

It isn't "extensibility is bad". It's narrower than that. The expensive parts
are *synchronous out-of-process calls on the write path*, and *new reconciled
kinds in the shared store*. In-process, bounded policy is the cheap part, and
it's where Kubernetes is heading anyway.

## What the others chose

If Kubernetes is one end of the scale, the lighter orchestrators sit at the
other. Their choices teach as much as Kubernetes' do, mostly by what their
users built around them.

### Nomad: real plugins, but no admission hook

Nomad has proper plugins, all out of process. Task drivers and device plugins
run under HashiCorp's go-plugin over gRPC, and a task outlives its driver: a
crashed driver is relaunched and reattaches to its tasks
([Nomad plugins](https://developer.hashicorp.com/nomad/docs/concepts/plugins),
[task drivers](https://developer.hashicorp.com/nomad/docs/concepts/plugins/task-drivers)).
Secret providers and host-volume plugins use something simpler, a "common
plugin interface": a binary that does one thing and exits. Hold on to that
one. We'll borrow it.

Admission is where Nomad stopped. Inside the server there's a compiled-in chain
of job mutators and validators
([`job_endpoint_hooks.go`](https://github.com/hashicorp/nomad/blob/main/nomad/job_endpoint_hooks.go)),
but nothing outside can join it. Sentinel policy is Enterprise only
([Nomad Sentinel](https://developer.hashicorp.com/nomad/docs/enterprise/sentinel)).
In 2018 someone opened [nomad#4735](https://github.com/hashicorp/nomad/issues/4735),
"Admission controllers", asking for policy hooks and sidecar injection; it was
closed in 2022 without being built. So the community built
[NACP](https://github.com/mxab/nacp), a proxy that sits in front of the Nomad
API and validates and mutates jobs with OPA or webhooks.

That's the pattern to remember. When the platform has no hook, someone puts
one in front of it, with none of the platform's guarantees.

On CRDs, Nomad's engineering lead was blunt: "Nomad has chosen not to implement
CRDs/Controllers/Operators as seen in Kubernetes", while adding "I could
absolutely see Nomad gaining CRD-like capabilities someday"
([LWN](https://lwn.net/Articles/905803/)).

### Docker and Swarm: the authorisation plugin that approved too much

Docker Engine's plugins are daemons speaking JSON over HTTP
([plugin API](https://docs.docker.com/engine/extend/plugin_api/)). The
authorisation plugin is a cautionary tale for any hook that inspects requests.
It sees JSON bodies only, the first request of a stream only, and has to decode
them exactly as the daemon would
([authz plugins](https://docs.docker.com/engine/extend/plugins_authorization/)).
CVE-2024-41110 was a regression where a request reached the plugin with an
empty body, so the plugin approved what it would have denied
([Docker advisory](https://www.docker.com/blog/docker-security-advisory-docker-engine-authz-plugin/)).
Any hook that re-parses a request is a second parser, and two parsers disagree
eventually.

Swarm itself has no admission or scheduler hooks we could find. Its users'
requests for runtime knobs stayed open for years: `--privileged` since 2016
([moby#24862](https://github.com/moby/moby/issues/24862)), devices
([moby#24865](https://github.com/moby/moby/issues/24865)), and `cap_add`, which
took four years ([moby#25885](https://github.com/moby/moby/issues/25885)).

### Fly.io: expose the primitives

Fly.io went a different way. It carved Nomad's scheduler out of its platform
and exposed the pieces: "In the new design, the basic primitives are directly
exposed, and we just write code to configure them the way we want"
([Fly, carving the scheduler out](https://fly.io/blog/carving-the-scheduler-out-of-our-orchestrator/)).
The [Machines API](https://docs.fly.io/machines/api) is the extension point.
Failover and multi-region logic live in code that calls it.

That fits Reliaburger well. We already have an API, scoped tokens and an
event stream. A controller that uses them doesn't need a new kind of anything.

### systemd: overrides without forks

systemd is the best model we know for changing something without forking it.
A drop-in file in `foo.service.d/` merges over the vendor's unit, with `/etc`
beating `/run` beating `/usr/lib`. Generators translate foreign config into
units at boot, and may touch nothing else
([systemd.unit](https://www.freedesktop.org/software/systemd/man/latest/systemd.unit.html),
[systemd.generator](https://www.freedesktop.org/software/systemd/man/latest/systemd.generator.html)).

Reliaburger's `_defaults.toml` is already this idea, on the client side
([Chapter 7](07-ship-it.md#defaults-must-survive-compilation)). Defaults merge
in, and the workload's own fields win.

### Envoy and the Wasm hosts

Envoy offers every model at once, which makes it a useful catalogue.
`ext_authz` and `ext_proc` call out over gRPC, failing closed unless you flip
`failure_mode_allow`
([ext_authz](https://www.envoyproxy.io/docs/envoy/latest/api-v3/extensions/filters/http/ext_authz/v3/ext_authz.proto)).
Its Wasm filter is still marked experimental
([Wasm filter](https://www.envoyproxy.io/docs/envoy/latest/configuration/http/http_filters/wasm_filter)),
and its newer dynamic modules are native shared libraries behind a C ABI, with
no sandbox at all
([dynamic modules](https://www.envoyproxy.io/docs/envoy/latest/intro/arch_overview/advanced/dynamic_modules)).

WebAssembly is the interesting one. It gives you a sandbox and, with the
component model, typed interfaces between host and guest
([component model](https://component-model.bytecodealliance.org/)). Wasmtime
can bound a guest's CPU with fuel or epochs, and its memory with a resource
limiter
([interrupting Wasm](https://docs.wasmtime.dev/examples-interrupting-wasm.html)).
It works well for narrow hooks:
[Kubewarden](https://docs.kubewarden.io/admission-controller/1.35/en/explanations/architecture.html)
ships admission policies as Wasm, and
[Shopify Functions](https://shopify.dev/docs/apps/build/functions) run JSON
in, JSON out under an instruction budget.

But a sandbox is code too. CVE-2023-26489 was a Cranelift miscompile that let
a guest reach far outside its memory
([GHSA-ff4p-7xrq-q5r8](https://github.com/bytecodealliance/wasmtime/security/advisories/GHSA-ff4p-7xrq-q5r8)),
and on 14 October 2026 the project published ten advisories at once
([advisories](https://github.com/bytecodealliance/wasmtime/security/advisories)).
Embedding Wasmtime means taking on its release cadence and its advisory
stream. We'll come back to whether that's worth it.

### What makes people actually leave?

Before choosing extension points, it helps to know which gaps make teams pick
something else. No survey asks that question directly, so the evidence is
migration write-ups and forum threads. Treat it as directional.

The biggest reasons aren't features at all. Figma moved off ECS partly because
"we are able to hire people with a lot more prior experience running on
Kubernetes" ([Figma](https://www.figma.com/blog/migrating-onto-kubernetes/)).
Playtomic left Swarm partly because Mirantis wrote "The primary orchestrator
going forward is Kubernetes"
([Playtomic](https://dev.to/playtomic/time-to-say-goodbye-to-docker-swarm-2iej)),
though Mirantis later committed to Swarm until 2030
([Mirantis](https://www.mirantis.com/blog/mirantis-guarantees-long-term-support-for-swarm/)).
One homelab left Nomad over licensing alone: "There's no technical reason to
replace Nomad and Consul"
([mei-home](https://blog.mei-home.net/posts/k8s-migration-0-plan/)). In the big
Hacker News thread on Nomad and Kubernetes, the ecosystem comes up again and
again; one commenter's "main downside" was "a lack of plug and play pieces,
e.g. a helm chart for Grafana or Prometheus"
([HN, Nomad vs Kubernetes](https://news.ycombinator.com/item?id=28875764)).
Extension points don't fix any of that.

Among real features, a rough order emerges:

1. **Operators and stateful apps.** Figma again: "ECS doesn't support
   StatefulSets", and it couldn't run services packaged as Helm charts
   ([Figma](https://www.figma.com/blog/migrating-onto-kubernetes/)).
2. **Networking and ingress**, such as Swarm's routing mesh hiding the client
   IP, open since 2016 ([moby#25526](https://github.com/moby/moby/issues/25526)).
3. **Autoscaling**: Playtomic needed "tools that allow us to scale up and down
   automatically as required" ([Playtomic](https://dev.to/playtomic/time-to-say-goodbye-to-docker-swarm-2iej)).
4. **Access control**: "giving full access to the cluster to every team is
   risky" (Playtomic, same post).
5. **Sidecars.** Swarm has no pods, so projects like
   [Podlike](https://github.com/rycus86/podlike) emulate them. Even Kubernetes
   took until 1.33 to get native sidecar lifecycles right
   ([KEP-753](https://www.kubernetes.dev/resources/keps/753/)).
6. **Admission and defaulting**: the Nomad request and proxy above.
7. **Devices and runtime knobs**: the Swarm issues above.

Reliaburger already has autoscaling, ingress, scoped tokens and namespaces. What
it lacks is a policy hook, sidecars, a way to package a stateful app, and a few
runtime knobs. Not custom schedulers, not network or storage plug-ins. That's
the list this release answers.

## Policies in the council

Let's start with the gap at the top of the chapter: "every app in `prod` must
have a memory limit". Here's what it will look like:

<!-- snippet: planned #1117 -->
```toml
[policy.prod-needs-limits]
match = "app.namespace == 'prod'"
deny = "!has(app.memory)"
message = "apps in prod need a memory limit"

[policy.no-latest]
deny = "app.image.endsWith(':latest')"
action = "warn"
```

A policy is a declarative kind, like a permission or a namespace. You apply it
with `relish apply`, it's committed to Raft, and every council member has a
copy. `match` and `deny` are CEL expressions. When `match` is true (or absent)
and `deny` is true, the apply is refused with the message, or, for
`action = "warn"`, accepted with a warning. Only an admin token can write
policies, and every write lands in the audit log. The field names are the
design's; the issue that builds it
([#1117](https://github.com/reliaburger/reliaburger/issues/1117)) may adjust
them.

Three questions decide whether this is a good design: where does the check
run, what language is it in, and what happens when things race? Let's take
them in turn.

### One function, one shared path

[Chapter 7](07-ship-it.md#one-config-two-front-doors-one-path) built one path
for desired state. `config_to_desired_writes` in `src/council/apply.rs` turns a
parsed config into Raft writes, and both `relish apply` (through
`cluster_apply` in `src/bun/api/apply.rs`) and the GitOps runner in
`src/lettuce/runner.rs` call it. Its module comment says why: so the same
config "converges identically whether you apply it by hand or through git",
because there's no second code path that could drift.

That gives us the hook for free. If the leader evaluates policy just before
`config_to_desired_writes`, in both callers, then both front doors enforce the
same rules by construction. No proxy in front of the API, as Nomad's users had
to build, and no second parser to disagree with the first, as Docker's
authorisation plugin had.

The evaluation itself will be one *pure function* in a new `policy` module.
Roughly:

```text
evaluate(&PolicySet, &Config, &Caller) -> Result<Evaluated, Denials>
```

That's a sketch of a signature, not real code yet, but it's worth reading
closely because it carries the whole design.

A pure function is one whose result depends only on its arguments, and which
changes nothing else: no files, no network, no clock, no global state. You've
met the idea in Python or Go, where it's a convention you keep by discipline.
Rust lets the signature say a good deal of it out loud. The `&` in `&PolicySet`
is a *shared reference*: the function may read the policy set but can't change
it. To change it, the parameter would have to be `&mut PolicySet`, a *mutable
reference*, and the compiler refuses to let anyone else hold a reference to
the same value while that one exists. So a caller who sees only `&` parameters
knows the function leaves its inputs alone. That's a guarantee in Rust, not a
code-review hope.

Rust can't prove the rest. A function with only `&` parameters can still call
`std::time::SystemTime::now()`, or open a socket. We keep that half by
discipline: a small module, no `async`, and inputs that are all explicit: the app or job spec, its namespace, the caller's token name
and role, and whether the change came from `apply` or GitOps. Nothing from the
network, no clock, no other apps.

The return type does the rest. `Result<Evaluated, Denials>` is Rust's standard
way of returning either a success value or an error, and the caller has to
handle both before it can get at the evaluated config. You can't forget to
check a denial the way you can forget to check `err` in Go. And the shape
makes the property tests easy to write, as we'll see.

Why does purity matter here? Because the same function will run in three
places: on the leader for `relish apply`, on the leader for GitOps, and on
your laptop for `relish policy test -f policies.toml -f apps.toml`, offline,
in CI. If the result depended on anything outside its arguments, those three
could disagree. Kubernetes users will know this idea from `kyverno test` and
conftest.

### Why not in the state machine?

Here's a tempting alternative. Raft already has a state machine that applies
every committed entry on every council member
([Chapter 2](02-finding-friends.md#raft-consensus)). Why not check policies
there, as each `AppSpec` entry applies? Then there'd be no way round them.

Because the state machine has to be deterministic, and policy isn't.

Raft's guarantee is that every member applies the same entries in the same
order. That only gives every member the same state if applying an entry is a
pure function of the entry and the state before it. Replay the log on a new
node a month later, and it must reach exactly the state the others reached.
Now suppose the state machine evaluated policy. A new member replaying the log
would evaluate old entries against today's policies, not the ones in force
when the entries were written. Upgrade the CEL crate, and an expression that
used to be true might now be false. Either way, the same log produces a
different state on different nodes, and the cluster has quietly forked. That's
the worst kind of bug Raft can have, because nothing fails.

Determinism in Rust has a trap that catches C and Go programmers too. The
standard `HashMap` seeds its hash function randomly in each process, to resist
denial-of-service attacks, so iterating over the same map gives a different
order on every node. Go does the same with its maps on purpose. Anything in the
state machine that depends on iteration order has to use a `BTreeMap`, which
keeps its keys sorted. The endpoint catalogue in
[Chapter 3](03-talking-to-each-other.md) is a `BTreeMap` for this reason.
Policy has the same problem in a smaller form: if two `default` fragments both
filled the same field, the winner must not depend on map order, so policies
apply in name order.

So the rule is: **policies change what the leader proposes, never how an entry
applies.** The state machine stores policies and stores apps. It never runs
CEL. The same rule shows up in the job bookkeeping of
[Chapter 17](17-a-million-jobs.md#the-leaders-bookkeeping-and-a-fence), which
never reads a clock because it lives inside the state machine.

### The generation fence

Evaluating on the leader and committing later opens a gap. What if a policy
changes between the two?

Say an app is evaluated against policy set 41 and passes. Before its write
commits, an admin commits a stricter policy, making the set 42. If the app's
write then lands, it lands under a policy that would have refused it.

The fix is a fence, an idea this book keeps coming back to. The leader stamps
each write with the *policy generation* it was evaluated against. The state
machine compares that generation with its own; if the policy set has moved, it
refuses the write, and the apply retries against the new set. The state
machine doesn't evaluate anything. It compares two numbers, which is as
deterministic as it gets.

Lettuce already does exactly this with the Raft term. Each GitOps write is a
`GitOpsWrite` stamped with the term the sync read desired state in, and the
state machine refuses it if leadership has moved since
([Chapter 7](07-ship-it.md#gitops-owns-what-it-wrote-and-nothing-else)). The
policy generation is the same idea, keyed on a different thing that can
change under you. The distributed-systems name for both is a fencing token.

### Why CEL

The policy language had to be bounded. A policy runs on the leader, inside the
apply path, so an expression that loops forever would stop the cluster taking
writes, which is the webhook failure mode all over again.

[CEL](https://github.com/google/cel-spec), Google's Common Expression Language,
says of itself: "CEL evaluates in linear time, is mutation free, and not
Turing-complete." It's
also what Kubernetes chose for ValidatingAdmissionPolicy, so a platform team's
existing expressions mostly carry over. The other candidates were weaker fits:

- **Rego**, OPA's language, is powerful and has a Rust implementation in
  Microsoft's [regorus](https://github.com/microsoft/regorus), but it's a
  second language to learn and a much larger surface.
- **Cedar** ([cedar](https://github.com/cedar-policy/cedar)) is built for
  authorisation, with formal proofs behind it. It's the right tool if we ever
  replace `[permission.*]` with something richer, but it answers "may this
  principal do this action", not "is this spec acceptable".
- **CUE** allows structural cycles that can loop
  ([CUE spec](https://cuelang.org/docs/reference/spec/)), and only has a Go
  implementation.

CEL's guarantee bounds evaluation by the size of the input times the size of
the expression. The input is one spec, and we cap the expression's size when a
policy is committed. Kubernetes goes further, with a cost model that refuses
expensive expressions at write time and halts them at run time
([CEL in Kubernetes](https://kubernetes.io/docs/reference/using-api/cel/)).

### The `cel` crate

In Rust, CEL comes from the [`cel`](https://crates.io/crates/cel) crate
([cel-rust](https://github.com/cel-rust/cel-rust)), version 0.15 at the time
of writing. Its API follows the usual compile-then-run split. Here's the
example from its own documentation
([docs.rs](https://docs.rs/cel/latest/cel/struct.Program.html)), not our code:

```rust
use cel::{extensions, Context, Env, Value};
use std::sync::Arc;

let mut env = Env::stdlib();
env.add_extension(extensions::strings).unwrap();
let env = Arc::new(env);
let program = env.compile("'TacoCat'.lowerAscii()").unwrap();
let context = Context::with_env(env);
assert_eq!(program.execute(&context), Ok(Value::from("tacocat")));
```

Reading it line by line:

- `Env` is the environment: which functions and extensions exist.
  `Env::stdlib()` starts with CEL's standard library.
- `Arc::new(env)` wraps it in an `Arc`, an *atomically reference-counted*
  pointer. Several owners can share one `Arc`, and the value is freed when the
  last one goes. Think of a refcounted pointer in C, with the count kept
  correct for you, even across threads. The environment is shared because both
  compiling and running need it.
- `env.compile(...)` parses and checks the expression once and returns a
  `Result` holding a `Program`. A parse error is a value you handle, not an
  exception. This is where a policy with broken CEL gets refused, at commit
  time, before it can match anything.
- A `Context` holds the variables an expression can see. In a policy, that's
  `app` and friends.
- `program.execute(&context)` evaluates it and returns a `Result` holding a
  `Value`, CEL's runtime value type, which our code then checks is a boolean.

The `.unwrap()` calls are fine in a doc example. In Reliaburger they'd be
`?`, because a broken policy must never panic the leader.

Compile-once matters to us. A policy is compiled when it's committed, and the
compiled programs are reused for every apply until the policy set changes.

There's a catch, and we won't hide it. We couldn't confirm from the crate's
documentation how closely it follows the CEL conformance suite, or whether it
tracks evaluation cost the way Kubernetes does. The crate's documentation also
shows more than one way in (`Program::compile` as well as `Env::compile`), so
the exact calls may differ from the example above. So the first slice
([#1117](https://github.com/reliaburger/reliaburger/issues/1117)) checks
before it builds:

- run the crate against CEL's conformance tests for the features policies use
  (field access, `has()`, string functions, list functions);
- check whether it offers any cost or step limit, and if it doesn't, rely on
  our own expression size cap;
- pin the exact compile, bind and execute calls, and how a Rust struct becomes
  a CEL value.

If CEL disappoints, the fallback is a much smaller predicate language (field,
operator, value) that covers the common policies less flexibly. Either way the
decision gets recorded in the plan, and this section will say which way it
went.

### Break-glass

What if someone commits a policy that denies everything? With webhooks, that's
the "API server is blocked" page. Here, two rules keep it from happening.
Policies match only apps and jobs, never policies, permissions or namespaces,
so `relish policy delete` always works. And there's no external service whose
absence can block writes, because everything runs inside Bun, from state that
Raft already replicates. The worst a bad policy can do is refuse apps until an
admin removes it.

## Defaults only fill gaps

The second job admission does in Kubernetes is mutation: fill in a label, add
a limit, inject a sidecar. Platform teams want this badly. "Fill in our team
label" is as common a request as "deny `:latest`". So a policy can carry a
`default` fragment:

<!-- snippet: planned #1118 -->
```toml
[policy.team-label]
match = "app.namespace == 'payments'"

[policy.team-label.default]
env.TEAM = "payments"
deploy.max_unavailable = 1
```

Here's the rule: **a default fills only fields you left out. It never
overwrites a value you wrote.** It's the same rule `_defaults.toml` already
follows, with the workload's explicit fields winning.

Why so strict? Kubernetes mutating webhooks can rewrite anything, and that's
where the confusion comes from. A later mutator changes a field an earlier one
set, so Kubernetes added `reinvocationPolicy: IfNeeded` to call mutators again,
and documents that "the number of additional invocations is not guaranteed to
be exactly one"
([dynamic admission control](https://kubernetes.io/docs/reference/access-authn-authz/extensible-admission-controllers/)).
Your manifest says one thing and the cluster runs another, and you find out by
diffing.

Fill-only removes most of that. Filling an absent field twice is a no-op, so
defaulting is idempotent by construction: evaluating twice gives the same
result as evaluating once. There's no reinvocation, because nothing can undo
anyone else's work. Defaults apply in policy-name order, then built-in
validation runs, then every `deny`, so validation sees the final spec. And you
always see your own values honoured. If a team must not use a value, the
answer is a `deny`, not a silent rewrite.

That rules some things out on purpose. A default can't rewrite an image
registry; that belongs with the trust policy and the pull-through cache
([Chapter 10](10-locking-it-down.md#trust-the-image-or-dont-run-it)). It can't
inject a sidecar either. If fill-only turns out too weak, we'll widen it with
evidence, not in advance.

### The diff trap

Fill-only defaults have one sharp edge, and it's with GitOps.

Lettuce works by comparing what's in git with the cluster's desired state, and
applying the difference. Now suppose we store only the *effective* spec, the
one with defaults filled in. Git says the app has no `TEAM` variable. The
cluster says it has `TEAM = "payments"`. Lettuce sees a difference and
re-applies. The policy fills the field again. The next sync sees the same
difference, and re-applies again. Forever.

The fix is to store the *declared* spec beside the effective one (or the list
of filled fields), and have Lettuce diff git against the declared spec. Then
what git owns is exactly what you wrote, and what the policy added sits beside
it. There's a precedent: the autoscaler's replica override is already a runtime
value that git doesn't own.

`relish plan` and `apply --dry-run` show each filled field and the policy that
filled it, so a default is never a surprise.

## Templates, not new kinds

A CRD plus a controller exists to turn one high-level object ("a Postgres
cluster") into many low-level ones, and then keep them right. Those are two
different jobs, and splitting them is the whole trick.

**Expansion is a template.** A template has typed parameters and a body of
ordinary app, job and volume declarations, with `${param}` substitution and
nothing else. No conditionals, no loops, no functions.

<!-- snippet: planned #1124 -->
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

The expansion happens at apply time, in the compiler `relish` and Lettuce
share, and before policy runs. So what policy sees, and what lands in Raft, is
ordinary apps, plus a note of which template they came from. `relish plan`
shows the expanded result.

Why not a new kind? Because every new kind the council stores and reconciles is
something the scheduler, the agent, the snapshot format and every future
version have to understand. That's the CRD sprawl from the start of the
chapter, in miniature. With templates, the council stores no new reconciled
kind, and the scheduler and agent never learn templates exist.

The risk is turning into Helm, with logic creeping into templates until
they're programs. The guard rails are deliberate: no logic in templates, a
template is a TOML resource like any other, and no template registry in the
first slice. Templates come from files or git.

**Day-two logic is a controller you run yourself.** Failover, backups and
version upgrades need code. Instead of an operator framework, we write down a
contract. A controller is an ordinary app with a token scoped to its namespace.
It reads the API and the event stream, and writes ordinary apps. Each resource
carries an `owner`, like Lettuce's ownership set, so `relish apply`, GitOps and
the controller don't fight over the same app. That's Fly's "expose the
primitives" model, with the guarantees Reliaburger already has: scoped tokens,
audit, and the one apply path. No SDK, no framework, no new kinds. The 0.2.4
exit test ([#1126](https://github.com/reliaburger/reliaburger/issues/1126)) is
a Postgres template plus a small controller that fails over a replica, written
against the public API only.

## Escape hatches

Some gaps are better closed directly than through an extension point. The
evidence names a few things teams need badly and simple orchestrators refused
for years. Each of these is just a spec field, with its own tests.

**Sidecars** ([#1120](https://github.com/reliaburger/reliaburger/issues/1120)).
The whitepaper planned a `sidecar` field for v2
([what's deliberately not included](../whitepaper.md#22-whats-deliberately-not-included)).
The evidence says sidecars are one of the commonest reasons teams leave a
simple orchestrator, so we're bringing them forward. A sidecar shares the
parent's network namespace and lifecycle, starts before it and stops after it.
That ordering is precisely what Kubernetes needed KEP-753 to get right. A
sidecar's resources count towards the app's requests when it's scheduled.

**Stop hooks** ([#1121](https://github.com/reliaburger/reliaburger/issues/1121)).
A `stop_signal`, a grace period and a `pre_stop` command. They're cheap, and
they remove a whole class of "we had to write a custom entrypoint" workarounds.
A deploy's drain and a node drain both honour them.

**Gated runtime knobs** ([#1122](https://github.com/reliaburger/reliaburger/issues/1122)).
A small `[app.x.runtime]` allowlist: added capabilities, namespaced sysctls,
devices. Each use needs two keys, like host paths already do: the node must
allow it in `node.toml`, and the token must carry `host-exec`. Anything else is
refused at apply, with a reason. That's the Swarm `--privileged` and
`--device` lesson. Few teams need these flags, but those that do can't live
without them.

### The exec secret provider

The whitepaper also deferred external secret managers to v2. Linking a Vault
SDK, an AWS SDK and a GCP SDK into Bun is the opposite of a simple binary, and
each would bring its own release cadence. But there's a much older protocol
that solves this: the credential helper. Git and Docker both ask a separate
binary for secrets, over JSON on stdin and stdout. Nomad's "common plugin
interface" works the same way.

So an env value can name a provider
([#1123](https://github.com/reliaburger/reliaburger/issues/1123)):

<!-- snippet: planned #1123 -->
```toml
[app.api]
image = "registry.example.com/api:1.4"
env = { DB_PASSWORD = "PROVIDER[vault:secret/db#password]" }
```

When an instance launches, the node runs the provider binary, allowlisted in
`node.toml` like `allowed_binaries`, with a JSON request on stdin and a
timeout, and reads the secret from its stdout. No vendor SDK is linked into
Bun. A node advertises which providers it has, and the scheduler only places
the app on nodes that have them.

It fails closed. If the provider is missing, slow or broken, the instance
doesn't start, and `relish wtf` says why. Only the apps that use that provider
are affected. That's the difference from a webhook: the failure stays with the
app that chose the dependency, instead of spreading to the whole cluster's
write path.

## What we chose not to do

Every one of these was considered. Here's why each is out.

**No synchronous admission webhooks.** The first section is the argument.
Reliaburger would have a nastier variant too: a webhook served by an app
inside the cluster can deny the deploy that would fix it, and the leader's
apply path would block on a network call. Fail open, and the policy isn't a
policy. Fail closed, and the cluster's writes depend on someone's service. If
you want to react to changes, the existing signed alert webhooks and the
planned event stream let a controller act *after* a write, off the write path.

**No CRDs or operator framework.** No user-defined kinds that the council
stores and reconciles. Templates expand to existing kinds, and controllers are
API clients.

**No dynamic libraries.** Rust's default type layout makes no guarantees
([type layout](https://doc.rust-lang.org/reference/type-layout.html)), so a
Rust `dylib` only works with the same compiler and flags
([linkage](https://doc.rust-lang.org/reference/linkage.html)). A stable Rust
ABI has been an open tracking issue since 2023
([rust#111423](https://github.com/rust-lang/rust/issues/111423)). Even with a
C ABI, a native plugin shares Bun's address space and privileges, and Bun runs
as root on most nodes. There's no sandbox at all. That alone rules it out.

**No Wasm, for now.** Wasm is the one real contender, and Kubewarden shows it
works for admission. But Wasmtime is a large dependency with its own advisory
stream, and the interface we'd hand module authors would become an API. Before
1.0 we break APIs freely, which makes modules a poor deal for the people
writing them. Scheduler hooks would be worse: the leader admits job chunks at
thousands a second, and a module call per candidate node per placement is a
new cost on that path, plus a new way for two leaders to place differently.
So Wasm waits behind a gated spike
([#1127](https://github.com/reliaburger/reliaburger/issues/1127)), triggered
only by a documented gap that policies, escape hatches, providers and
templates can't close.

**No mutation that overwrites user values**, and no reinvocation.

**No network, storage or runtime plug-in interfaces** (CNI, CSI, CRI). The
whitepaper's reasons still hold, and the evidence doesn't show these as
reasons teams leave.

**No Rego, no CUE, no general scripting** (Lua, Rhai, JavaScript) inside Bun.

**No plugin binaries for `relish`.** The CLI's design keeps "no plugins".
`--output json` and the API are its extension points.

## Rust walkthrough

*To be written as the code lands.* Each issue on the 0.2.4 train adds its
part here, with real listings from the source:

- the policy kind, the pure evaluation function and the generation fence
  ([#1117](https://github.com/reliaburger/reliaburger/issues/1117));
- fill-only defaults and the declared-versus-effective split
  ([#1118](https://github.com/reliaburger/reliaburger/issues/1118));
- jobs, the audit action, `relish policy audit` and the `wtf` row
  ([#1119](https://github.com/reliaburger/reliaburger/issues/1119));
- sidecars, stop hooks and runtime knobs
  ([#1120](https://github.com/reliaburger/reliaburger/issues/1120),
  [#1121](https://github.com/reliaburger/reliaburger/issues/1121),
  [#1122](https://github.com/reliaburger/reliaburger/issues/1122));
- the exec secret provider
  ([#1123](https://github.com/reliaburger/reliaburger/issues/1123));
- templates and the controller contract
  ([#1124](https://github.com/reliaburger/reliaburger/issues/1124),
  [#1125](https://github.com/reliaburger/reliaburger/issues/1125)).

## Tests

*To be written as the code lands.* The research sets out the plan, and each
issue fills in what its tests check and how to read them:

- a unit table of specs and policies with the expected decision, and every
  kind of bad policy refused at commit;
- property tests that defaulting is idempotent, never changes a field the user
  set, and doesn't depend on map iteration order;
- snapshot tests of `relish plan` and `relish policy test` output;
- portable integration tests: a denied apply leaves no Raft write but an audit
  entry, a GitOps commit with one denied app applies the rest, a policy that
  changes mid-apply makes the write retry, and repeated syncs of a defaulted
  app don't re-apply;
- cluster tests for a leader kill during an apply under policy, and an admin
  removing a policy that denies everything;
- Smoker faults under the secret provider binary;
- the exit test on a real three-node cluster
  ([#1126](https://github.com/reliaburger/reliaburger/issues/1126)).

## Lessons

We'll add the lessons from building it when it's built. One we can state now,
because Kubernetes paid for it so we don't have to: the cost of an extension
point isn't in what it lets people do. It's in what it can stop the cluster
from doing when it breaks. Keep extensions in-process, bounded and off the
state machine, and most of that cost goes away.
