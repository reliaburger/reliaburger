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
have a memory limit". Here's what it looks like:

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
policies, and every write lands in the audit log. The field names survived
the build ([#1117](https://github.com/reliaburger/reliaburger/issues/1117))
unchanged: `match`, `deny`, `message` and `action`.

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

The evaluation itself is one *pure function* in a new `policy` module. The
design sketched it like this:

```text
evaluate(&PolicySet, &Config, &Caller) -> Result<Evaluated, Denials>
```

The real one is close, and we'll see it in the walkthrough below. The sketch
is worth reading closely first, because it carries the whole design.

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

Compile-once matters to us. A policy is compiled when it's checked at apply
time, and each apply compiles the set it runs under once and reuses the
programs for every app in it. GitOps keeps its compiled set for a whole sync
and compiles again only when the policy generation moves.

There was a catch, and we said we'd check it before building: we couldn't
confirm how closely the crate follows the CEL conformance suite, or whether it
tracks evaluation cost the way Kubernetes does. If CEL disappointed, the
fallback was a much smaller predicate language (field, operator, value).

Here's what the check found, in `cel` 0.15:

- **Conformance.** The crate runs the CEL conformance suite in its own
  repository (the published package doesn't ship it, but its changelog and
  source refer to the runner). We wrote our own table of the features
  policies use, in `src/policy/tests.rs`: field access, `has()`, `in` on a
  map, `startsWith` and `endsWith`, the `exists` and `all` macros, `size()`,
  `type()`, and int comparisons. Every row passed on the first run.
- **Cost.** There's no cost model and no step limit. The parser caps nesting
  at 96 levels, and that's all. So our own cap does the bounding: an
  expression of at most 1,024 bytes, over one app's spec. CEL has no loops of
  its own, and a macro like `exists` iterates over a list that came from the
  spec, so the work is bounded by expression size times input size.
- **The calls.** `Env::stdlib()` plus the strings extension, `env.compile` to
  get a `Program`, `Context::with_env` and `add_variable_from_value` to bind
  `app`, `caller` and `source`, then `program.execute`. A spec becomes a CEL
  value through `serde_json`, which we then walk ourselves, so whole numbers
  become CEL `int` and `app.port == 8080` compares like with like.

So CEL stays, and the fallback isn't needed. One surprise: `namespace` is a
reserved word in CEL, so `namespace.cpu` doesn't even parse. That suits us,
because a policy can't name a namespace anyway; it reads `app.namespace`.

We also turn off the crate's default `chrono` feature. Policies see no clock,
so they don't need timestamps, and it saves a time-zone database in the
binary.

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
sidecar's resources count towards the app's requests when it's scheduled:

```toml
[app.api]
image = "registry.example.com/api:3.2"
port = 8080

[[app.api.sidecar]]
name = "proxy"
image = "envoyproxy/envoy:v1.31.0"
args = ["-c", "/etc/envoy/envoy.yaml"]
cpu = "50m-200m"
memory = "64Mi-128Mi"
```

A sidecar that crashes restarts on its own. An app that crashes takes its
sidecars down with it and brings them back up first.

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

Each issue on the 0.2.4 train adds its part here, with real listings from the
source.

### The policy kind and the pure function

([#1117](https://github.com/reliaburger/reliaburger/issues/1117)) A policy is
a plain struct in `src/config/policy.rs`:

```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PolicySpec {
    #[serde(rename = "match", default, skip_serializing_if = "Option::is_none")]
    pub match_expression: Option<String>,
    pub deny: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(default)]
    pub action: PolicyAction,
}
```

`match` is a keyword in Rust, the way `switch` is in C, so the field can't be
called that. `#[serde(rename = "match")]` keeps the Rust name readable and the
TOML name what people expect. `deny_unknown_fields` turns a typo like
`mesage` into an error rather than a policy without a message.

The function the leader runs lives in `src/policy/mod.rs`. Its signature:

```rust
pub fn evaluate_app(
    set: &PolicySet,
    app_id: &AppId,
    spec: &AppSpec,
    caller: &Caller,
    source: Source,
) -> Verdict
```

Every parameter is a shared reference except `source`, which is a small
`Copy` enum passed by value. `Verdict` holds two lists, `denials` and
`warnings`. We didn't end up with `Result<Evaluated, Denials>` here, because
GitOps needs both lists for every app, denied or not, and a `Result` gives you
one side or the other. The `Result` lives one level up instead, in `admit`:

```rust
pub fn admit(
    committed: &BTreeMap<String, PolicySpec>,
    generation: u64,
    config: &Config,
    caller: &Caller,
    source: Source,
    writes: Vec<RaftRequest>,
) -> Result<Admitted, AdmitError>
```

`writes` is a `Vec` passed by value, not a slice: `admit` *takes ownership* of
the writes and hands them back wrapped. In Go you'd pass the slice and hope
nobody kept using the old one. In Rust the caller's `writes` variable is moved
into the call and can't be touched again, so nobody can accidentally send the
unwrapped writes to Raft. A denied config gets `Err(AdmitError::Denied(..))`
and the writes are dropped with it. There's no way to get them back out.

Inside, one policy is two compiled programs and a decision:

```rust
fn decide(policy: &CompiledPolicy, context: &Context) -> Result<bool, String> {
    if let Some(program) = &policy.match_program
        && !run(PolicyField::Match, program, context)?
    {
        return Ok(false);
    }
    run(PolicyField::Deny, &policy.deny_program, context)
}
```

`if let ... && ...` is a *let chain*, new in the 2024 edition: it binds
`program` only when the policy has a `match`, and goes on to the condition in
the same `if`. The `?` after `run(...)` returns early with the error, so an
expression that fails to evaluate becomes an `Err`, which `evaluate_app` turns
into a finding: a broken deny policy refuses the app. Failing closed is the
point. A policy that can't decide must not wave things through.

`run` itself wraps the crate's `execute` in `std::panic::catch_unwind`. Rust
code shouldn't panic on bad input, and the crate is young. A panic inside the
GitOps loop would kill the loop, so we turn one into a refused app instead.
`catch_unwind` needs its closure to be *unwind safe*, a marker trait saying
nothing half-updated is visible after a panic. Ours only reads, so we tell the
compiler with `AssertUnwindSafe`.

### Checking a policy before it's committed

`check_policy` in `src/policy/check.rs` compiles each expression, then walks
its syntax tree. The tree is the crate's `Expr` enum, and the walk is one
`match` with an arm per variant:

```rust
Expr::Select(select) => {
    if let Expr::Ident(variable) = &select.operand.expr
        && !bound.contains(variable)
    {
        check_field(field, variable, &select.field)?;
    }
    check_names(field, &select.operand, bound)
}
```

A `Select` is `app.memory`; when its operand is the bare identifier `app`, the
field must be one of `APP_FIELDS`. A free identifier must be `app`, `caller`
or `source`, and that's how a policy is stopped from matching policies: it
can't name them. The `bound` vector holds the loop variables of the macros
(`app.command.all(word, ...)` binds `word`), pushed on the way into a
comprehension and truncated on the way out. Because `match` on an enum must
cover every variant, a future crate version that adds an expression kind
breaks our build, not our check. We'd rather find out that way.

`APP_FIELDS` is a hand-written list, and hand-written lists drift. A test
keeps it honest: it asks serde to parse an `AppSpec` with an unknown key, and
reads the list of expected keys out of the error message.

### The generation fence

The state machine's side is short, as promised:

```rust
RaftRequest::PolicyFenced { generation, change } => {
    // (checks that `change` is an app write)
    if *generation != self.state.policy_generation {
        return Some(CouncilResponse::PolicyGenerationMoved {
            current: self.state.policy_generation,
        });
    }
    return self.apply_request_at(change, position);
}
```

`PolicyFenced` *wraps* another `RaftRequest` in a `Box`. An enum variant can't
contain its own type directly, because the compiler needs to know the size of
every value, and a type containing itself would be infinitely big. A `Box` is
a pointer to the heap, a fixed size, so the recursion is fine. `GitOpsWrite`
uses the same trick, and the two nest: GitOps sends a `PolicyFenced` around a
`GitOpsWrite` around an `AppSpec`, and each layer checks one thing.

Why a new response variant rather than a `Refused { reason }` string? Because
the callers act on it. `cluster_apply` and the Lettuce runner `match` on
`PolicyGenerationMoved` and judge the remaining apps again; matching on the
text of a reason would break the first time someone rewords it.

A config can carry policies and apps together. Its policy writes come first,
and each moves the generation by one, so `admit` fences the apps on
`generation + config.policy.len()`. If anyone else writes a policy in
between, the numbers don't match, the fence trips, and the apply judges again,
now with the committed set, which includes its own policies.

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

### Sidecars

A sidecar is three promises: it shares its app's network, it's up before the
app, and it's still up after the app. Kubernetes shipped the first in 2015
and spent until 1.33 on the other two. We had a head start, because init
containers already had most of the machinery: an auxiliary identity per
instance, built from the app's finished spec, registered with the loop before
the runtime hears of it so retirement can always find it. A sidecar is the
long-running cousin.

#### The config is a list, like `init`

`[[app.api.sidecar]]` is an array of tables, the TOML spelling of a list of
structs, exactly as `[[app.api.init]]` is:

```rust
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SidecarSpec {
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub command: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, EnvValue>,
    // memory and cpu, with the same range parsers as the app's
}
```

`image` is an `Option` for two reasons. A sidecar can run the app's own
image with a different command, which an init container already allowed.
And Pickle's binder pins every image an apply names to a digest through a
list of `&mut Option<String>` slots. With the same type, a sidecar's image
is just one more slot.

Validation refuses what can't work: a name that isn't a DNS label or appears
twice, a sidecar on an `exec` or `script` app (whose network is the node's,
so there's nothing to share), an encrypted value in a sidecar's own `env`,
and an app that has both sidecars and an `egress` allowlist. That last one
is a security call, and we come back to it under cgroups below.

#### Joining a namespace, without a new field

The portable half is `generate_sidecar_oci_spec`. It builds on the app's
finished spec, as `generate_init_oci_spec` does, and changes one thing an
init doesn't: the network namespace gets a path.

```rust
let mut namespaces = app.linux.namespaces.clone();
let shared = network_namespace_path(parent);
match namespaces.iter_mut().find(|ns| ns.ns_type == "network") {
    Some(network) => network.path = Some(shared),
    None => namespaces.push(OciNamespace {
        ns_type: "network".to_string(),
        path: Some(shared),
    }),
}
```

An OCI namespace entry with a `path` means "join this one" rather than
"make a new one", so runc does the joining for us. The question was how the
runtime learns it's looking at a sidecar. The obvious answer is a new
`OciSpec` field. That spec has fifty struct literals across the runtimes,
half of them in Linux-only files this laptop can't compile, and the runtime
knobs branch (#1122) was editing the same struct at the same time. So the
spec already says it:

```rust
pub fn joined_network(&self) -> Option<&str> {
    self.linux
        .namespaces
        .iter()
        .find(|ns| ns.ns_type == "network")
        .and_then(|ns| ns.path.as_deref())
}
```

Nothing else in Bun sets that path today, so it's unambiguous, and the
runc side shrinks to two conditions. `owned_create` reserves no address
for a spec that joins a network, and `prepare_with_commands` skips the veth
setup and checks the parent's namespace is still there. Cleanup needed no
change at all: with no reservation, there's no network to tear down. To
keep both sides naming the same path, `netns::namespace_path` now calls the
portable `network_namespace_path` instead of formatting its own.

Apple Container and the process runtime can't do this. Each Apple container
is a VM with its own network stack, and the `container` CLI has no way to
put one inside another; a process workload runs in the node's own network.
Rather than pretend, each runtime answers a new trait method:

```rust
fn shares_network_namespaces(&self) -> bool {
    false
}
```

A trait method with a body is a default, which an implementation may
override and otherwise inherits. Rootful runc overrides it, and the mixed
runtime asks its container backend. Admission refuses an app with sidecars
on a node that says no, with the runtime's name in the reason, and the node
reports the answer as `shared_network` in its capability report, which the
scheduler reads. Apple's `create` refuses a joining spec too, in case one
ever gets past both.

#### Two containers, two cgroups

An init container borrows its app's cgroup, because it has exited before the
app starts. A sidecar can't: runc warns about a non-empty cgroup at best,
and `runc delete --force` on the app would kill everything in it, sidecar
included. So a sidecar gets a sibling, `/reliaburger/default/api/0-sidecar-proxy`,
with its own `cpu` and `memory` limits.

That has a consequence. Egress allowlists are keyed by cgroup in the eBPF
programs, so a sidecar in its own cgroup isn't held to its app's allowlist.
A proxy could reach what the app can't. Programming the sidecar's cgroup
too means another set of egress bindings to own and retire, so for now
validation refuses the combination with a reason that says why. Namespace
isolation still applies, because the kernel falls back to the cgroup's
`/reliaburger/<namespace>` ancestor.

The requests, meanwhile, are simple arithmetic in `app_requests`, the one
function placement, quotas and a node's own admission all call:

```rust
let cpu = spec
    .sidecar
    .iter()
    .map(|sidecar| request(sidecar.cpu.as_ref()))
    .fold(request(spec.cpu.as_ref()), u64::saturating_add);
```

`fold` starts from the app's request and combines each sidecar's into it.
The combining function is `u64::saturating_add`, named rather than written
as a closure: in Rust a method is also a plain function whose first
argument is the receiver, so `u64::saturating_add(a, b)` is `a.saturating_add(b)`
and the name can be passed wherever a two-argument function fits. Three
places used to compute the request inline from the app alone; they all call
this now.

#### Up first, down last

The order lives in portable agent code. A fresh launch runs create, the init
containers, then `drive_sidecars`, then the app's start. `drive_sidecars`
registers every sidecar identity with the loop before it creates the first,
so a launch that dies half-way leaves nothing retirement can't find, and it
fails the instance with the sidecar's reason if one won't start. The rolling
replacement path makes the same call in the same place.

Stopping went into `stop_runtime_instance`, which every graceful stop already
used. `InstanceStop` grew the instance's sidecars, and once the instance's
exit is confirmed, each sidecar gets its signal and grace in turn. The first
draft called `stop_runtime_instance` for each sidecar, and rustc refused:

```text
error[E0733]: recursion in an async fn requires boxing
```

An `async fn` compiles to a state machine: a struct that holds every local
the function keeps across an `.await`. If the function awaits itself, that
struct contains itself, and has no finite size. Go and Python don't care,
because their coroutines live on the heap. Rust wants you to say so, with
`Box::pin`. Here the honest fix was smaller. A sidecar has no sidecars, so
the four steps moved into `stop_instance_alone`, which both the instance and
each sidecar use, and the recursion disappeared.

The graceful sidecar stop is best effort. The guarantee comes from
retirement, which kills every registered sidecar and confirms its exit
before it forgets an instance, as it does for init containers. Even a stop
that failed half-way can't leave a sidecar behind.

#### Two crash policies

Kubernetes restarts a crashed sidecar alone, and so do we. The health tick's
state sweep already read each running instance's state off the loop; it now
reads each sidecar too, under its instance's incarnation so a stale answer
can't hit a successor. An exited sidecar becomes due, and the tick starts it
again in a follow-up task: kill what's left of it, create, start. The first
restart is immediate, then the backoff doubles:

```rust
pub(super) fn sidecar_backoff(restarts: u32) -> Duration {
    if restarts == 0 {
        return Duration::ZERO;
    }
    let seconds = 1u64.checked_shl(restarts - 1).unwrap_or(u64::MAX);
    Duration::from_secs(seconds).min(SIDECAR_MAX_BACKOFF)
}
```

`1 << n` in C is undefined once `n` reaches the width of the type. Rust's
plain `<<` panics in a debug build and wraps in a release one, and
`checked_shl` returns `None` instead, which turns into "a very long time"
and then the one-minute cap.

An app that crashes is different. On runc the network namespace belongs to
the app's launch. The restart's Clear step tears it down with the app and
the Create step makes a new one, so a sidecar left running would sit in a
namespace nobody else is in. So an app's crash restarts the whole instance:
Clear kills the app, then its sidecars, and Start creates and starts the
sidecars before it starts the app. The same ordering as a fresh launch,
just from the restart steps.

Two races needed fencing. A sidecar restart in flight could create a sidecar
after the app's Clear step killed them all, so an instance with one in
flight waits a tick before its own restart begins, and every stop cancels
it with a `CancellationToken` the task checks between steps. Retirement
answers `StillRunning` until the task has ended, which is how every
retirement step already says "ask me again".

#### What a Bun restart does

A restarted Bun adopts the instances it has records for and retires every
other launch. Sidecars have no records, because the record format names
replicas, so they're retired. Adoption then registers each adopted
instance's sidecars as due, and the next tick starts them. That's a second
or so without a sidecar on every Bun restart. Giving sidecars records of
their own would close the gap, and it's the obvious next step.

Still to come in this walkthrough:

- fill-only defaults and the declared-versus-effective split
  ([#1118](https://github.com/reliaburger/reliaburger/issues/1118));
- jobs, the audit action, `relish policy audit` and the `wtf` row
  ([#1119](https://github.com/reliaburger/reliaburger/issues/1119));
- runtime knobs
  ([#1122](https://github.com/reliaburger/reliaburger/issues/1122));
- templates and the controller contract
  ([#1124](https://github.com/reliaburger/reliaburger/issues/1124),
  [#1125](https://github.com/reliaburger/reliaburger/issues/1125)).

## Tests

The research set out the plan, and each issue fills in what its tests check
and how to read them.

### Policies

([#1117](https://github.com/reliaburger/reliaburger/issues/1117))

- **The decision table** (`policy::tests::the_decision_table`) is one `Vec`
  of rows: a policy, an app, and the decisions expected. A failure prints the
  row's policy and app, so you can see which kind of decision broke.
- **Every kind of bad policy** (`every_kind_of_bad_policy_is_refused_at_commit`)
  checks a bad name, an empty expression, invalid CEL, an expression or
  message over the cap, an unknown field, a variable that isn't `app`,
  `caller` or `source`, and a loop variable used outside its loop. Each case
  also goes through `Config::validate`, so neither front door commits it.
- **Property tests.** `a_denied_config_produces_no_writes` generates a handful
  of apps and checks that `admit` refuses exactly when one breaks the rule,
  and otherwise fences every app. `evaluation_order_does_not_depend_on_map_order`
  inserts the same policies in a shuffled order, and through a `HashMap`, and
  checks the verdict never changes.
- **Snapshots** with `insta`: the `relish policy test` report, the `relish
  policy list` table, and a `relish apply --dry-run` plan with its policy
  section.
- **The state machine**: every policy write moves the generation by one; a
  fenced write at an old generation answers `PolicyGenerationMoved` and
  leaves no trace; the fence carries nothing but app writes.
- **Through the router** (`bun::api::policy_tests`): only an admin writes a
  policy, a denial answers 422 and is audited, a warning streams and the apply
  goes ahead, and an admin deletes a policy that refuses everything.
- **The portable suite** (`tests/suite/policies.rs`): a denied apply leaves the
  Raft log index where it was but an audit record behind; a GitOps commit with
  one denied app applies the rest and names the policy; and a policy committed
  between judging and writing makes the write retry. That second test found a
  real bug on its first run: the config compiler that GitOps uses to merge a
  repository's files dropped `[policy.*]` tables on the floor, so a
  repository's policies never reached the cluster.
- **The cluster** (`make test-cluster`): a three-node cluster whose leader is
  killed during an apply under a policy. The new leader holds the same policy
  at the same generation, refuses a breaking app, and accepts the interrupted
  apply when it's sent again.

Still to come from the plan:

- property tests that defaulting is idempotent and never changes a field the
  user set;
- snapshot tests of `relish plan` showing filled fields;
- a portable test that repeated syncs of a defaulted app don't re-apply;
- a cluster test of an admin removing a policy that denies everything (the
  router test above covers it on one node);
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

### Testing sidecars

The ordering is the feature, so the agent tests in
`src/bun/agent/tests/sidecars.rs` read the mock runtime's call log and
compare positions in it. Each deploys one app with an init container and
two sidecars, `proxy` and `logs`:

- `sidecars_start_after_the_init_containers_and_before_their_app`: the app
  is created first (its namespace is what they join), then the init runs,
  then `proxy` is created and started, then `logs`, then the app starts. The
  spec the runtime got for `proxy` joins `/var/run/netns/rb-default__web-0`,
  and `logs`, which names no image, runs the app's.
- `an_app_stop_stops_its_sidecars_after_the_app_has_exited`: the app's stop
  signal, then a state read that sees it exit, and only then each sidecar's
  signal, in order. Retirement kills what's left and forgets them.
- `a_rolling_deploy_starts_new_sidecars_first_and_stops_old_ones_last`: the
  replacement's sidecars start before it does and join its namespace, not
  the old one's; the old instance stops before its sidecars do.
- `a_sidecar_that_crashes_restarts_alone_and_its_app_keeps_running`: the
  sweep sees `proxy` exit, and the only runtime calls that follow are kill,
  create and start on `proxy`. The app and `logs` are untouched, and the app
  is still Running.
- `a_sidecar_that_keeps_crashing_waits_out_its_backoff`: the first restart
  is immediate and the second isn't.
- `an_app_that_crashes_restarts_with_its_sidecars_stopped_after_and_started_before_it`:
  the restart's Clear kills the app before `proxy` and `proxy` before
  `logs`, and its Start starts both sidecars before the app.
- `a_sidecar_that_cannot_start_fails_its_instance_which_never_starts`: when
  `logs` can't be created, the deploy fails naming it, the app is never
  started, and `proxy`, which did start, is killed.
- `a_node_whose_runtime_cannot_share_a_network_refuses_sidecars_with_a_reason`
  and `the_capability_report_says_whether_the_runtime_can_run_sidecars`
  cover the Apple, rootless and process case: the deploy fails before the
  runtime sees a create, and the node reports `shared_network: false`.
- `an_adopted_instance_gets_its_sidecars_started_again`: an instance adopted
  from its record gets both sidecars back on the next tick, joined to its
  namespace, without the instance itself being touched.

Around them, unit tests pin the pieces. `grill::oci` checks the generated
spec: the joined namespace, the image and argv, the env with the sidecar's
own on top, the app's mounts and a sibling cgroup with the sidecar's limits.
`config::validate` covers each refusal. `meat::admission` and `meat::filter`
check that a sidecar's requests add to its app's, that an app which fits
alone doesn't fit with its sidecar on a node with room for only the app, and
that only a node reporting `shared_network` is a candidate.
`AppleContainerGrill::create_command_args` refuses a joining spec before the
CLI runs, `pickle::binding` pins a sidecar's image, and the Kubernetes
importer turns a native sidecar and a pod's second container into sidecars
and survives a round trip through `relish export`. The registry's backoff
and cancellation have their own tests in `bun::agent::sidecars`.

To check that the agent tests check something, we started the sidecars
after the app and stopped them before it. Four tests failed: the start
order, the stop order, the rolling deploy and the failed launch, whose app
now started before its sidecar could fail.

The runc half can't be compiled on a Mac, so CI's Linux jobs compile it, and
`make test-linux` runs `runc_sidecar_reaches_its_app_on_localhost`: Redis as
the app, a sidecar in the same image running `sleep`, and `redis-cli -h
127.0.0.1 ping` executed inside the sidecar. `PONG` means they share a
namespace, and the sidecar must have no address of its own.

## Lessons

We'll add the lessons from building it when it's built. One we can state now,
because Kubernetes paid for it so we don't have to: the cost of an extension
point isn't in what it lets people do. It's in what it can stop the cluster
from doing when it breaks. Keep extensions in-process, bounded and off the
state machine, and most of that cost goes away.
