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
A deploy's drain and every other stop honour them:

```toml
[app.worker]
image = "registry.example.com/worker:2.1"
stop_signal = "SIGQUIT"
stop_grace = "2m"
pre_stop = ["/app/bin/worker", "hand-back-jobs"]
pre_stop_timeout = "20s"
```

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
- templates and the controller contract
  ([#1124](https://github.com/reliaburger/reliaburger/issues/1124),
  [#1125](https://github.com/reliaburger/reliaburger/issues/1125)).

### The exec secret provider

([#1123](https://github.com/reliaburger/reliaburger/issues/1123)) The code
lives in `src/sesame/secret_provider.rs`. It has three jobs: parse the
reference, run the provider safely, and hand the launch either a secret or a
reason.

**A reference is a type, not a string.** `EnvValue` already had two variants,
`Plain` and `Encrypted`. It gets a third:

```rust
pub enum EnvValue {
    Plain(String),
    Encrypted(String),
    /// `PROVIDER[name:path]`: resolved on the node when an instance launches.
    Provider(crate::sesame::secret_provider::SecretReference),
}
```

Adding a variant to an enum is the cheapest code review you'll ever get.
Every `match` on `EnvValue` that listed `Plain` and `Encrypted` stopped
compiling, so the compiler walked us through each place that has to decide
what a provider value means: the dashboard shows the reference (it names
where the secret lives, not the secret), the Kubernetes exporter writes the
reference and warns, the GitOps "this looks like a plaintext secret" check
leaves it alone, and the job paths never see one, because validation refuses
it. In Go you'd add a constant to a string-typed kind and grep for the
switches; here, missing one is a build error.

The deserialiser does the parsing, so a malformed reference never gets as far
as Raft:

```rust
} else if crate::sesame::secret_provider::SecretReference::is_reference(&s) {
    crate::sesame::secret_provider::SecretReference::parse(&s)
        .map(EnvValue::Provider)
        .map_err(serde::de::Error::custom)
}
```

`serde::de::Error::custom` turns our `ReferenceError` into whatever error type
the format in use (TOML, JSON, postcard) wants. The provider's name is a
newtype, `ProviderName`, with `#[serde(try_from = "String", into = "String")]`,
which tells serde to deserialise a `String` and then run our `TryFrom`, so a
name that isn't a lowercase DNS label can't exist, whether it came from an
env value or from `node.toml`.

**Running the provider.** This is `tokio::process::Command`, the async twin of
`std::process::Command`:

```rust
let mut command = tokio::process::Command::new(&config.binary);
command
    .env_clear()
    .env("PATH", PROVIDER_PATH)
    .envs(&config.env)
    .current_dir("/")
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .stderr(Stdio::null())
    .kill_on_drop(true);
#[cfg(unix)]
command.process_group(0);
```

`env_clear()` matters more than it looks. Bun runs as root with whatever its
service manager handed it, and none of that is the provider's business, so
the provider gets a default `PATH` and exactly the `env` the operator wrote.
`process_group(0)` makes it the leader of a new process group, which pays off
when it has to die. `kill_on_drop(true)` is a belt to go with those braces: if
the future holding the child is dropped, say because the deploy is cancelled,
tokio kills the process instead of leaking it.

The exchange itself is one `async` function, and the timeout wraps all of it:

```rust
let outcome = tokio::time::timeout(timeout, exchange(&mut child, &request, limit)).await;
```

`exchange` writes the request, reads stdout and waits for the exit. The read
is the interesting line:

```rust
stdout
    .take(limit as u64 + 1)
    .read_to_end(&mut output)
    .await?;
```

`AsyncReadExt::take` wraps the reader in one that reports end-of-file after
`limit + 1` bytes. If we get that extra byte, the provider wrote too much, and
we never buffered more than one byte past the cap. A provider stuck in a loop
printing `yes` costs Bun 64 KiB, not its memory.

Our first version read to end-of-file and only then waited for the exit. It
passed every test on its own, and timed out in a crowd. End-of-file on a pipe
means every copy of its write end is closed, and on macOS a copy can leak:
Rust's standard library creates the pipe and only then marks it close-on-exec,
so a process spawned by another thread in between inherits it. One test's
`sleep 30` was holding the stdout of every other test's provider. Linux
creates the pipe close-on-exec in one call, but a provider that leaves a
child running would do the same thing anywhere. So the read races the exit:

```rust
tokio::select! {
    result = &mut read => {
        result?;
        None
    }
    status = child.wait() => {
        if let Ok(result) = tokio::time::timeout(DRAIN_AFTER_EXIT, &mut read).await {
            result?;
        }
        Some(status?)
    }
}
```

`tokio::select!` polls both futures and runs the branch of whichever finishes
first, dropping the other. `read` is pinned with `tokio::pin!` and passed as
`&mut read`, so losing the race doesn't drop it: when the exit wins, we keep
reading for a tenth of a second, which is plenty to empty a pipe whose writer
has gone. The request got the matching change: it ends in a newline, so a
provider can answer after one line instead of waiting for end-of-file on
stdin.

`tokio::time::timeout` returns `Err(Elapsed)` when the deadline wins, and
drops the inner future. Dropping a future in Rust cancels it: there's no
thread to interrupt, the state machine just stops being polled. But the
*process* is still running, and so is anything it started. A shell script
that runs `sleep 30` has a child of its own, which `kill` on the script would
orphan, and that orphan would hold stdout open. That's why we put the provider
in its own group, and kill the group:

```rust
async fn kill_provider(child: &mut tokio::process::Child) {
    #[cfg(unix)]
    if let Some(group) = child.id().and_then(|pid| i32::try_from(pid).ok()) {
        // The child isn't reaped yet, so the group id can't have been reused.
        let _ = nix::sys::signal::killpg(
            nix::unistd::Pid::from_raw(group),
            nix::sys::signal::Signal::SIGKILL,
        );
    }
    let _ = child.kill().await;
}
```

The comment is the subtle part. A process group id is the leader's pid, and
pids get reused. But a child that has exited and hasn't been waited for is a
zombie, and a zombie still holds its pid. We only call `killpg` before
`child.kill()` reaps it, so we can't hit some unrelated process that inherited
the number.

**Errors that can't leak.** The rule is that no error carries a secret. That
sounds easy until you remember where errors come from. The provider's answer
is parsed with serde:

```rust
// A parse error's message can quote the input, which is the secret, so
// only whether it parsed is kept.
let response = serde_json::from_slice::<ProviderResponse>(output).ok();
```

`serde_json`'s errors are helpful: `invalid type: string "hunter2", expected
u32`. That's exactly what we can't print. `.ok()` turns the `Result` into an
`Option` and throws the message away, and a malformed answer gets a fixed
reason instead. `ProviderResponse` doesn't derive `Debug`, so nobody can
`{:?}` a secret into a log by accident. The tests feed the provider answers
designed to trip each of these, and check that the secret appears in no
message.

**Where it runs.** Building the OCI spec happens on the agent loop, which must
never wait (chapter 14's loop rule). A provider can take five seconds. So
`build_env_with_decryptor` passes a provider value straight through, and the
call happens later, on the deploy or restart task, just before the runtime
creates the instance:

```rust
pub(super) async fn create_with_secrets<'s, G: Grill>(
    grill: &G,
    providers: &crate::sesame::secret_provider::SecretProviders,
    target: LaunchTarget<'_>,
    spec: &'s crate::grill::oci::OciSpec,
) -> Result<std::borrow::Cow<'s, crate::grill::oci::OciSpec>, crate::grill::GrillError> {
```

Two Rust things are going on in that signature. `'s` is a named lifetime: it
says the returned value may borrow from `spec`, and so can't outlive it. And
`Cow` (clone on write, which we met in chapter 5) is either `Borrowed(&'s
OciSpec)` or `Owned(OciSpec)`. An app with no provider values gets the
caller's spec back, borrowed, with no copy. One with provider values gets an
owned copy with the secrets filled in. The caller uses either through `&*`
without caring which.

That copy is the only place the resolved environment lives. The supervisor
keeps the original, references and all, so a restart asks the provider again.
That means a secret rotated at the source reaches the next restart, and Bun's
memory holds no secrets it doesn't need.

**Failing closed.** A provider error becomes a new `LaunchProblem` variant:

```rust
/// A `PROVIDER[...]` env value couldn't be resolved (#1123). The launch
/// fails closed: the instance never runs without its secret.
#[error(transparent)]
SecretUnavailable(#[from] crate::sesame::secret_provider::SecretResolutionError),
```

`LaunchProblem` is what a missing binary already produced (#881), and
everything downstream already knew what to do with one: mark the instance
`Failed` with the reason, show it in `/v1/status` and `relish wtf`, and not
burn the restart budget on a launch that would fail the same way. The
`#[from]` attribute generates `impl From<SecretResolutionError> for
LaunchProblem`, which is why `create_with_secrets` can write
`problem: error.into()`. We wrote no new failure path, just a new reason to
take the existing one.

**Placement.** A node advertises its providers in its capability report,
`NodeCapabilities::secret_providers`, and `node_can_run` gains one line:

```rust
&& requirements
    .secret_providers
    .is_subset(&node.capabilities.secret_providers)
```

Both are `BTreeSet<ProviderName>`, and `is_subset` is the whole check. An app
that names no provider has an empty set, which is a subset of anything.

### Stop hooks

Stop hooks were the smallest escape hatch, so they landed first. Before
them, every stop did the same thing: withdraw the instance from routing,
drain it, send SIGTERM, wait, send SIGKILL. That's right for most programs.
It's wrong for nginx, which finishes its requests on SIGQUIT and drops them
on SIGTERM, and for a queue worker that needs a minute and a command to hand
its jobs back. Teams worked round it with wrapper entrypoints that trapped
SIGTERM and translated it. Now an app says what it wants, and Bun does it.

#### Parsing is the validation

The first question was where a bad value gets refused. A grace of `"0s"`
makes no sense, `"SIGKILL"` defeats the point of a grace, and `"15"` is
SIGTERM on both Linux and macOS but a guess in a config file. We could have
kept the fields as strings and checked them in `validate_app`. Instead
they're types that can't hold a bad value:

```rust
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub enum StopSignal {
    #[default]
    Term,
    Int,
    Quit,
    Hup,
    Usr1,
    Usr2,
    Winch,
}
```

`#[serde(try_from = "String", into = "String")]` tells serde to read the
field as a plain `String` first and then convert it with our own
`TryFrom<String>` implementation. If that returns an error, deserialising the
whole `AppSpec` fails, and the error carries our message. Writing goes the
other way, through `From<StopSignal> for String`. So the TOML, the Raft
entry and the API JSON all hold `"SIGQUIT"`, and the Rust code holds an enum
it can `match` on.

The conversion itself is `FromStr`, the trait behind `str::parse`. It's
Rust's version of Go's `strconv.Atoi` or Python's `int(s)`, except that you
implement it for your own types, and `"SIGINT".parse::<StopSignal>()` picks
yours from the type you ask for:

```rust
impl FromStr for StopSignal {
    type Err = StopSettingError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let upper = value.trim().to_ascii_uppercase();
        let bare = upper.strip_prefix("SIG").unwrap_or(&upper);
        StopSignal::ALL
            .into_iter()
            .find(|signal| &signal.name()[3..] == bare)
            .ok_or_else(|| StopSettingError::UnknownSignal(value.to_string()))
    }
}
```

`TryFrom<String>` then just calls `value.parse()`. Durations work the same
way: `StopDuration` wraps a `Duration` and refuses anything under a second or
over an hour. The payoff shows up in the error. A typo fails `relish lint`
with the file's line and column, and with the list of signals that would
have worked:

```text
failed to parse TOML: TOML parse error at line 3, column 15
  |
3 | stop_signal = "SIGFOO"
  |               ^^^^^^^^
unknown stop signal "SIGFOO": use one of SIGTERM, SIGINT, SIGQUIT, SIGHUP, SIGUSR1, SIGUSR2, SIGWINCH
```

What's left for `validate_app` is how the fields fit together: a
`pre_stop_timeout` with no `pre_stop`, an empty program name, and an `exec`
or `script` app whose pre-stop program isn't an absolute path. That last one
matters because such an app's pre-stop runs on the host, so it has to pass
the node's `allowed_binaries` like the app's own binary does. It joins the
app's host binaries for placement too, so the scheduler won't pick a node
that would refuse it.

#### The signal travels with the instance

The obvious design was a `signal` argument on `Grill::stop`. We didn't take
it. Some callers of `stop` know nothing about apps (a Bun shutdown stops
whatever is running), and threading a signal through every one of them is
churn that buys nothing. Instead, `generate_oci_spec` copies the app's
fields into the instance's spec as a `StopPolicy`:

```rust
pub struct StopPolicy {
    #[serde(default)]
    pub signal: StopSignal,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grace: Option<Duration>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pre_stop: Option<PreStop>,
}
```

Every runtime already keeps that spec. The process runtime signals the
workload's process group with `entry.spec.stop_signal()`, the process owner
reads the spec it was launched with, runc passes the name to `runc kill`, and
Apple Container gets `container stop --signal`. Because the spec is in the
instance record too, a Bun that restarts and adopts its instances still
knows how to stop them. And a deploy retires an old instance with *its own*
policy, not the new spec's. That matters when the new image has dropped the
program the old one's pre-stop runs.

The spec is durable state, so this is a format change: it's the change that
moved `CURRENT` in `src/compatibility.rs` to 55/72 for 0.2.4.

#### Four steps, four bounds

The sequence lives in `stop_runtime_instance`, which every graceful stop
already went through:

```rust
pub async fn stop_runtime_instance<G: Grill>(
    grill: &G,
    id: &InstanceId,
    stop: &InstanceStop,
    confirmation_timeout: std::time::Duration,
) -> Result<(), BunError> {
    if let Some(pre_stop) = &stop.pre_stop {
        run_pre_stop(grill, id, pre_stop).await;
    }
    tokio::time::timeout(confirmation_timeout, grill.stop(id))
        .await
        .map_err(|_| BunError::StopUnconfirmed {
            instance_id: id.clone(),
            reason: "graceful stop request timed out",
        })??;
    if observe_runtime_exit(grill, id, stop.grace).await? {
        return Ok(());
    }
    kill_runtime_instance(grill, id, confirmation_timeout).await
}
```

Notice that `run_pre_stop` returns nothing at all. It isn't allowed to fail
the stop, so its type says it can't: a pre-stop command that exits non-zero,
can't start or runs over its bound gets a line in `bun`'s log, and the
signal goes out regardless. A broken hook must never keep an instance
running.

The bound is a `tokio::time::timeout` around `Grill::exec`. As chapter 2
explained, a timeout cancels by dropping the future, so the exec has to
clean up when it's dropped. The process owner already did: the exec's
socket lives in the caller's future, and the owner retires the command when
it closes. The plain process runtime didn't. It spawned the command with
`tokio::process::Command`, whose child carries on after its handle is
dropped unless you ask otherwise, so `exec` now sets `kill_on_drop(true)`.

`InstanceStop::for_policy` turns a policy into the grace and pre-stop for
one stop, with the caller's own grace wherever the app names none. That
keeps the old behaviour exactly: a deploy still gives an old instance its
`drain_timeout`, and any other stop still gives it 10 s.

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
- the exit test on a real three-node cluster
  ([#1126](https://github.com/reliaburger/reliaburger/issues/1126)).

### Testing the exec secret provider

The provider tests don't mock the provider. Each one writes a real `/bin/sh`
script into a temporary directory, marks it executable, and points a
`SecretProviders` at it. That's the only honest way to test a timeout that
has to kill a process group, or a cap that has to stop reading a pipe.

The unit tests in `src/sesame/secret_provider/tests.rs` cover:

- **Parsing**: a good reference, a path holding `:` and `]`, and each kind of
  malformed reference (no `]`, no `:`, a bad provider name, an empty path, a
  control character, an over-long path), each with its own `ReferenceError`.
- **Node config**: the defaults, an unknown field or a bad name refused, and
  a relative binary or out-of-range bounds refused by `validate`.
- **The protocol**: `a_provider_gets_one_versioned_request_and_its_value_is_the_secret`
  has the script copy its stdin to a file, and compares it with the exact
  JSON object the manual documents. Another checks the provider sees only its
  configured environment, not Cargo's.
- **The bounds**: `a_slow_provider_is_killed_with_everything_it_started`
  starts `sleep 30 &`, records its pid, and asserts both that the call fails
  in about a second and that the `sleep` is dead afterwards. Without
  `process_group(0)` and `killpg`, that second assertion fails.
  `a_provider_that_writes_too_much_is_killed_at_the_cap` runs `yes`.
- **The faults the research asked Smoker for**: delay (the slow provider),
  kill (`kill -9 $$`, which must read "was killed by signal 9") and a full
  disk (a provider that reports `No space left on device` on stderr and exits
  1, which must read "exited with status 1").
- **No leaks**: answers in the wrong version, with the wrong types, or as
  bare text, each carrying a marker secret, and an assertion that the marker
  appears in neither the error's `Display` nor its `Debug`.

The agent tests in `src/bun/agent/tests/secret_providers.rs` run whole deploys
against `MockGrill`:

- the secret reaches the spec the runtime creates, and the spec the
  supervisor keeps still holds the reference;
- a slow provider fails the deploy, the instance is `failed` with the reason
  in its status, and the runtime is never asked to create it;
- a provider the node doesn't run fails that app, and a plain app deployed
  next starts normally;
- a provider that answers in the wrong version never puts its value in an
  event or a status;
- a restart asks the provider again, and when the provider has started
  failing, the restart fails closed at once with the reason;
- the node's capability report lists its providers.

Smoker's faults target workloads, nodes and the network, not a short-lived
host process Bun spawns, so the delay, kill and full-disk cases are scripts
rather than Smoker rules. A full disk is the one that only a script can fake
here. On Linux, the honest version mounts a tiny tmpfs, fills it, and points
the provider's cache at it.

### Testing stop hooks

The stop hook tests use real processes, because a trap in a shell script is
the only honest witness to which signal arrived. Each workload is a few
lines of `sh`: it sets its traps, touches a `ready` file so the test can't
signal it before the traps exist, and loops. A trap writes the signal's name
to a log and exits.

- `the_configured_signal_arrives_and_a_trapping_workload_exits_within_its_grace`
  sets `stop_signal = SIGINT` and traps TERM, INT and QUIT. The log must
  read `INT` alone, the exit code must be 0 (it left by itself), and the
  stop must finish well inside its 10 s grace.
- `a_workload_that_ignores_its_signal_is_killed_once_the_grace_runs_out`
  ignores SIGTERM with a 1 s grace. The stop must take at least the grace,
  and not much more, which also proves the app's grace beat the caller's
  30 s default.
- `pre_stop_runs_inside_the_workloads_environment_before_the_signal` has the
  pre-stop command append to the same log, which must read `pre-stop`, then
  `TERM`.
- `a_failing_pre_stop_never_prevents_the_stop` tries a command that exits 3
  and one that doesn't exist; both stops complete with the signal logged.
- `a_pre_stop_that_overruns_costs_only_its_own_bound` runs `sleep 30` with
  a 1 s bound and checks that the stop moved on after about a second.

Those drive the in-process runtime. Two more in the integration suite,
`an_owned_workload_gets_its_own_signal_after_its_pre_stop` and
`an_owned_workload_that_ignores_its_signal_is_killed_after_its_grace`, run
the same story through the production path, where every workload sits under
its own `bun` owner process. There it's the owner, not Bun, that picks the
signal, so only these tests prove the owner reads the spec.

The wiring tests use the mock runtime and read its call log: an operator
stop calls `exec` before `stop`, an app with no `pre_stop` never calls
`exec`, and a rolling deploy runs the *old* instance's pre-stop before it
signals it, even though the new spec has none. The parsing tests in
`grill::stop` and `config::validate` cover each refusal, and
`stop_hooks_survive_an_export_and_import_round_trip` sends an app through
`relish export` and back in. To check the tests check something, we put
SIGTERM back in both runtimes and skipped the pre-stop: eight of them failed,
each saying what it saw instead.

## Lessons

We'll add the lessons from building it when it's built. One we can state now,
because Kubernetes paid for it so we don't have to: the cost of an extension
point isn't in what it lets people do. It's in what it can stop the cluster
from doing when it breaks. Keep extensions in-process, bounded and off the
state machine, and most of that cost goes away.
