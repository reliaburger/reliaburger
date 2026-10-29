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
The sections near the end that show the Rust and the tests were
placeholders when we started. Each issue on the 0.2.4 train filled in its
part as it landed, and the exit test, which uses all of them, came last.

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

### Jobs, and rules you only watch

Apps aren't the only thing a platform team wants rules for. A nightly report
job pulling `:latest` is the same mistake as an app doing it
([#1119](https://github.com/reliaburger/reliaburger/issues/1119)). So a policy
can say what it judges:

```toml
[policy.no-latest-anywhere]
applies_to = "both"                      # apps (the default), jobs or both
deny = "workload.image.endsWith(':latest')"
```

The default is `apps`, so every policy written before jobs could be judged
still means exactly what it meant. A jobs policy sees the job as `job`. A
policy for both sees `workload`, which holds only the fields apps and jobs
share, so `workload.port` is refused when the policy is committed rather
than failing on every job at apply time. We could have bound one variable
to whatever the workload was, but then an expression that reads `app.port`
would be fine on apps and broken on jobs, and you'd find out from a refused
cron job at three in the morning. Making the variable name carry the kind
keeps the commit check honest.

The second addition is a third action. `action = "audit"` lets the apply go
ahead without telling the caller and records the finding in the audit log.
It's how you roll out a strict rule: commit it as `audit`, watch what it
would have refused, and change it to `deny` when nothing breaks it any more.
Gatekeeper calls this dry run.

And there's the question every new policy raises. It judges workloads as
they're applied, so it never touches what's already running, which is the
promise that keeps a policy commit from turning into a redeploy. But then
what *is* running that the new rule would refuse? `relish policy audit`
answers that, and a `relish wtf` row shows the same. It judges every
committed app and job the way its next apply would be judged, and changes
nothing. Gatekeeper runs a controller for this; we run the same pure
function when someone asks.

## Defaults only fill gaps

The second job admission does in Kubernetes is mutation: fill in a label, add
a limit, inject a sidecar. Platform teams want this badly. "Fill in our team
label" is as common a request as "deny `:latest`". So a policy can carry a
`default` fragment:

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

Two details came out of building it
([#1118](https://github.com/reliaburger/reliaburger/issues/1118)). First, each
policy's `match` sees the app as the policies before it left it, so a
`b-health` policy can say `match = "has(app.port)"` and pick up a port that
`a-port` filled in. That's what "in name order" has to mean if the order is to
matter at all. It also bounds the idempotence promise: filling twice gives the
same app unless an earlier policy's `match` tests a field a later one fills.
We never fill twice in practice, though, because the cluster always fills the
spec as you declared it, never the filled one, as the next section explains.
Second, "a field you left out" needs a precise meaning. We serialise the app
to JSON, and a field is absent when the JSON doesn't hold it or holds `null`.
That covers every optional field and every empty list or map. It doesn't
cover `replicas`, which always has a value (1 when you leave it out), so a
policy that tries to default `replicas` is refused when it's committed.
Nor can a default set `exec`, `script`, host volumes, `init`, `runtime` or
`sidecar`. The apply path checks those against the caller's token, on the
spec as declared, before any policy runs. A default filled in afterwards
would hand every matching app host access its deployer was never granted,
and the token check would never see it. The same goes for `owner`
([#1125](https://github.com/reliaburger/reliaburger/issues/1125)): only the
controller's own token or an admin may claim an app, and that's checked on the
declared spec too, so a default that filled it in would claim apps for a
controller their deployer can't speak for.

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

We store the whole declared spec, not the list of filled fields. Taking
fields back out of a spec would need a second, inverse operation that has to
agree with filling for ever, and a stored spec needs nothing but an equality
check. An app gets an entry, in a new `declared_apps` map in desired
state, whenever the spec the cluster stores differs from the one you wrote.
A default is one reason. The other was already there: the leader binds each
image tag to a digest before it writes the app, so `web:v1` is stored as
`web:v1@sha256:…`. Lettuce had a special case that read the digest back off
before comparing. With the declared spec stored, that special case only
matters for apps written before it, and `relish apply --dry-run` stops
calling an app with a bound image "updated" when nothing changed.

This has a consequence worth stating plainly: a new or changed default doesn't
reach a GitOps app until git changes that app. That's the same promise a new
`deny` makes, that a new policy never touches what already runs, and it's the
only behaviour that doesn't turn one policy commit into a redeploy of
everything it matches.

`relish apply --dry-run` shows each filled field and the policy that filled
it, so a default is never a surprise:

```text
Policies:
  FILL  policy team-label fills deploy.max_unavailable = 1 on app payments/web
  FILL  policy team-label fills env.TEAM = "payments" on app payments/web
```

## Templates, not new kinds

A CRD plus a controller exists to turn one high-level object ("a Postgres
cluster") into many low-level ones, and then keep them right. Those are two
different jobs, and splitting them is the whole trick.

**Expansion is a template.** A template has typed parameters and a body of
ordinary app, job and volume declarations, with `${param}` substitution and
nothing else. No conditionals, no loops, no functions.

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
ordinary apps, plus a note of which template they came from. `relish apply
--dry-run`, our plan, shows the expanded result:

```text
  + app.orders-db
      image     postgres:17
      replicas  1
      from      use.orders-db (template postgres)
```

A few decisions came out of building it
([#1124](https://github.com/reliaburger/reliaburger/issues/1124)).

**Substitution knows where it is.** The body is a string, and the obvious
implementation is a find-and-replace followed by a TOML parse. Think about
what that does with a value like `x"` followed by a newline and
`[app.evil]`. The quote closes the string, and the value has just declared
an app of its own. It's SQL injection with square brackets. So the
substitution reads the body the way a TOML parser would, enough to know
whether each placeholder sits outside quotes, in a basic `"..."` string, in
a literal `'...'` string or in a comment. Inside a basic string the value is
escaped, so it stays one string whatever it holds. Outside quotes, where
`[app.${name}]` lives, a string must be a plain key of letters, digits, `-`
and `_`. A literal string can't escape anything, so a value with a `'` in it
is refused there. Integers and booleans can't break anything and go in as
written. No value can change the shape of the body, the same promise a bound
parameter makes for an SQL statement.

**Expanded apps are ordinary apps of the file that uses them.** In a
repository, a template in `templates.toml` can serve a `use` in
`payments/db.toml`, and the app it makes lands in the `payments` namespace
with `payments/_defaults.toml` filled in, exactly as if you'd written it
there. Policies judge it like any other app, and they can tell where it came
from: `app.provenance.template == 'postgres'` is a fine `match`.

**Provenance is two strings.** The template's name and the `use`'s name, and
nothing else. No template version, no parameter values, no copy of the
body. The values are already in the spec they produced, and the body lives
in git. Because provenance is part of the spec, the declared-spec diff from
the last section needs no special case: what git declares after expansion
is exactly what the cluster stores, so a re-sync writes nothing. Like a
controller's `owner`, it isn't part of the app's version, so moving a
hand-written app into a template restarts nothing when the expansion comes
out the same. And because expansion runs before everything else, a body
that declares `owner = "${controller}"` gets no special treatment: the
owner rules see an ordinary app declaring an owner, and only that token or
an admin may apply it.

**No nesting, and no fighting.** A body holds apps and jobs and nothing
else, so a template can't use another template. Nesting is how a template
language grows into a programming language, and refusing it was the
simplest guard rail we could find. An expanded name that collides with an
app you declared, or with another `use`'s output, is refused. `relish
compile` normally lets a later file override an app with a warning, but not
when a template made either one. Two sources of truth fighting over one
name is a bug, not a feature.

**A placeholder no parameter fills is refused, even in a template nobody
uses.** Otherwise a typo like `${imgae}` would sit there quietly until the
day somebody tried the template.

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
It reads the API and the event stream, and writes ordinary apps. Each app it
manages carries an `owner`, the name of its token, like Lettuce's ownership
set, so `relish apply`, GitOps and the controller don't fight over the same
app: an apply that doesn't name the owner skips the app with a note, GitOps
leaves it out of its diff, and only the owner or an admin can stop, delete,
roll back or release it. That's Fly's "expose the
primitives" model, with the guarantees Reliaburger already has: scoped tokens,
audit, and the one apply path. No SDK, no framework, no new kinds. The 0.2.4
exit test ([#1126](https://github.com/reliaburger/reliaburger/issues/1126)) is
a Postgres template plus a small controller that fails over a replica, written
against the public API only. It's at the end of the chapter, with what it
honestly proves.

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
without them:

```toml
[app.vpn]
image = "ghcr.io/example/vpn:2.0"

[app.vpn.runtime]
capabilities = ["NET_ADMIN"]
sysctls = { "net.ipv4.ip_forward" = "1" }
devices = [{ path = "/dev/net/tun", permissions = "rw" }]
```

The node's half lives in its own `node.toml`:

```toml
[runtime]
allowed_capabilities = ["NET_ADMIN"]
allowed_sysctls = ["net.ipv4.ip_forward"]
allowed_devices = ["/dev/net/tun"]
```

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

### Fill-only defaults

([#1118](https://github.com/reliaburger/reliaburger/issues/1118)) A policy's
`default` arrives as a TOML table, but we keep it as JSON:

```rust
#[serde(default, skip_serializing_if = "Option::is_none")]
pub default: Option<serde_json::Map<String, serde_json::Value>>,
```

`serde_json::Value` is serde's "any JSON value" enum: `Null`, `Bool`,
`Number`, `String`, `Array` or `Object`. Because serde separates the data
model from the format, the TOML parser can fill a `serde_json::Map` directly:
it walks the TOML table and hands serde maps, strings and numbers, and serde
doesn't care which format they came from. Raft carries desired state as JSON
anyway, so the table crosses the wire as itself. `deny` became an
`Option<String>` at the same time, because a policy that only fills defaults
has nothing to deny. `check_policy` refuses a policy with neither.

Filling is one recursive function in `src/policy/defaults.rs`:

```rust
pub(super) fn fill(
    target: &mut Map<String, Value>,
    fragment: &Map<String, Value>,
    prefix: &str,
    filled: &mut Vec<(String, Value)>,
) {
    for (key, value) in fragment {
        let path = if prefix.is_empty() {
            key.clone()
        } else {
            format!("{prefix}.{key}")
        };
        let absent = matches!(target.get(key), None | Some(Value::Null));
        match (target.get_mut(key), value) {
            (Some(Value::Object(existing)), Value::Object(inner)) => {
                fill(existing, inner, &path, filled);
            }
            (_, Value::Object(inner)) if absent && !inner.is_empty() => {
                let mut table = Map::new();
                fill(&mut table, inner, &path, filled);
                target.insert(key.clone(), Value::Object(table));
            }
            _ if absent => {
                target.insert(key.clone(), value.clone());
                filled.push((path, value.clone()));
            }
            _ => {}
        }
    }
}
```

The parameters show who may change what. `target` is `&mut`, the app being
filled; `fragment` is `&`, the policy's table, which nothing here can alter;
and `filled` is `&mut` so each call can append the paths it filled. In C you'd
pass pointers and promise in a comment; here the compiler holds every caller
to it.

The `match` is on a *tuple* of two things at once: what the app holds at this
key, and what the default holds. Rust checks the arms top to bottom. Both are
tables: recurse, which is how `env.TEAM` joins an app's own `env` without
disturbing its other variables. The default is a table and the app has
nothing: build the table leaf by leaf, so `relish apply` can report
`deploy.max_unavailable` rather than all of `deploy`. The app has nothing and
the default is a plain value: copy it. Anything else, `_ => {}`, means the app
holds a value, and the app wins. There's no arm that writes over a value, so
"never overwrites" isn't a rule the code follows. It's a rule the code can't
break.

The `if absent` after a pattern is a match guard, and `absent` comes from
`matches!`, both of which we've met before. What's worth a closer look is
why `absent` is worked out *before* the `match`. `target.get_mut(key)`
borrows `target` mutably for the whole `match`, and while a mutable borrow is
alive the compiler won't let anything else read `target`, not even a guard.
This is the borrow checker's one-writer-or-many-readers rule at small scale.
Asking the question first, while nothing holds a mutable borrow, is the usual
way round it.

`apply_default` serialises the app to JSON, fills it, and deserialises the
result back into an `AppSpec`. The round trip is what keeps the types honest:
if a default gave `port` a string, serde refuses to turn it back into a `u16`.
The commit check uses the same trick. It fills the default into an app that
sets nothing, reads it back, and refuses the policy if serde complains or if
a key the default named has vanished on the way, which is what happens to a
key no field keeps.

`judge_app` runs the three stages in order. For each policy with a default,
in name order, it evaluates `match` against the spec so far and fills;
then, if anything was filled, it runs the same `validate_app_spec` that
`relish apply` runs on a declared app; then every `deny`. `admit` returns the
filled specs. `fence` compares each app write with the app as declared, and
when they differ it hands the fence the declared one:

```rust
PolicyFenced {
    generation: u64,
    change: Box<RaftRequest>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    declared: Option<Box<AppSpec>>,
},
```

The state machine applies `change` first, and only if that wasn't refused
does it record `declared` in `DesiredState::declared_apps`, or remove the
entry when `declared` is `None`. Any other write that changes an app's spec,
a rollback say, clears the entry, because the new spec is its own
declaration. A write of the *same* spec keeps it. That's what a GitOps
adoption sends: the spec the cluster already runs, filled fields and all,
to record that GitOps owns it.

On the GitOps side the fix is one line in `diff_apps`, with a comment that
names the trap:

```rust
let current_spec = current.declared_apps.get(app_id).unwrap_or(effective_spec);
```

`get` returns an `Option<&AppSpec>`, and `unwrap_or` gives the effective spec
when there's no declared one.

### Jobs, the audit action and `relish policy audit`

([#1119](https://github.com/reliaburger/reliaburger/issues/1119)) Which
workloads a policy judges is an enum with a default:

```rust
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PolicyTarget {
    #[default]
    Apps,
    Jobs,
    Both,
}
```

and the field that holds it skips itself when it's the default:

```rust
#[serde(default, skip_serializing_if = "PolicyTarget::is_apps")]
pub applies_to: PolicyTarget,
```

`#[default]` on a variant tells `#[derive(Default)]` which one
`PolicyTarget::default()` returns, and `#[serde(default)]` calls that when
the TOML leaves the key out. The two together are the whole compatibility
story for old policies: they parse as `Apps`, and they serialise without
the key, exactly as before.

Which variables an expression may name now depends on the target, so the
name check takes it as a parameter:

```rust
pub fn workload_variables(target: PolicyTarget) -> &'static [&'static str] {
    match target {
        PolicyTarget::Apps => &["app", "workload"],
        PolicyTarget::Jobs => &["job", "workload"],
        PolicyTarget::Both => &["workload"],
    }
}
```

The return type, `&'static [&'static str]`, is a slice that lives for the
whole program: each arm returns a reference to an array the compiler bakes
into the binary, so there's nothing to allocate and nothing to free. It's
the Rust spelling of a `static const char *[]` in C. `WORKLOAD_FIELDS` is
the intersection of `APP_FIELDS` and the new `JOB_FIELDS`, and a test checks
it is exactly that, the same way `APP_FIELDS` is checked against serde.

When the leader judges a workload, the context binds the same value twice,
once as `app` or `job` and once as `workload`. The policy loop skips a
policy whose target doesn't cover the workload's kind, using a `matches!`
over a tuple, `(kind, target)`, with `(_, PolicyTarget::Both)` as the first
pattern: `_` matches anything, so "both" covers either kind without a
separate arm per combination.

`PolicyAction` grew an `Audit` variant, and adding it broke the build in
every `match` that turned an action into words or a list. That's the
exhaustive `match` from Chapter 1 earning its keep: the compiler listed
each place that needed a decision about audit findings, and `Verdict` got a
third list, `audits`, beside `denials` and `warnings`. A `Finding` now also
carries a `WorkloadKind`, so a refusal says `policy pinned denies job
default/nightly` rather than calling a job an app.

The audit itself, in `src/policy/audit.rs`, is two functions. `audit` reads
desired state and gathers what to judge: each app's declared spec where
the cluster kept one (#1118), otherwise the stored spec with its image read
back as the tag it was bound from, and each job definition with its
schedule put back. `audit_workloads` takes plain slices and runs
`judge_app` and `judge_job` over them, so the unit tests can hand it apps
and jobs without building a cluster. The route runs it on a copy of
desired state inside `tokio::task::spawn_blocking`, because judging every
workload is CPU work that would otherwise hold up an async thread.

Who is the caller, when nobody is applying? The audit asks what the next
write would get, so it judges each app as its next writer: GitOps for an app
GitOps owns, the controller's token for an app a controller owns
([#1125](https://github.com/reliaburger/reliaburger/issues/1125)), with the
role read from that token, and the cluster's own `audit` caller for the rest.

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
Apple Container gets `container kill --signal`. Why not `container stop
--signal`, which looks made for this? Because it waits for the exit itself
and kills the workload after its own `--time`, 5 s unless told otherwise, so
`stop_grace = "2m"` bought two minutes on Linux and five seconds on a Mac
(#1435). `Grill::stop` only signals, on every runtime, and Bun owns the
grace. `container kill` refuses a workload that's already gone, so Apple's
`stop` checks the state before it calls that a failure. Because the spec is in the
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

### Runtime knobs

([#1122](https://github.com/reliaburger/reliaburger/issues/1122)) Runtime
knobs are the escape hatch with the sharpest edges. A capability, a sysctl or
a device reaches past the container in a way a stop signal never could, so
the work here is mostly about saying no, clearly, in the right place.

#### Two keys, in two places

Host paths already had the shape we wanted. A volume `source` needs the
token's `host-exec` permission at apply, and the node's own
`[storage] allowed_host_paths` when the instance is admitted. Neither key
opens the door alone: a cluster admin can't push a host path onto a node
that never agreed to it, and a node that allows a path doesn't hand it to
any deployer who asks. Runtime knobs reuse both halves.

The token half is two lines of defence in `apply.rs`. A knob counts in
`AppSpec::needs_host_access`, so the existing workload check demands
`host-exec` for it, and a CI guard script insists every app goes through
that check. But its refusal says only that the token lacks `host-exec`,
which isn't enough to act on. So a few lines just before it ask the same
question for each app with a knob, and refuse with a message that names the
knob and the gate:

```text
app.vpn: runtime.capabilities CAP_NET_ADMIN refused by the token gate: a runtime knob needs the host-exec permission, and this token's permission doesn't grant it
```

The node half is `WorkloadSupervisor::admit_runtime_knobs`, which `admit_app`
calls beside `admit_host_paths`, and which a rolling replacement calls before
it prepares anything, because a replacement never goes through `admit_app`.
It refuses in one of two ways: the runtime can't apply knobs at all, or the
allowlist doesn't list one. The check itself is a walk over the three kinds:

```rust
pub fn check(&self, knobs: &RuntimeKnobs) -> Result<(), KnobRefusal> {
    let refuse = |kind, value| {
        Err(KnobRefusal::NotAllowed {
            knob: KnobUse { kind, value },
        })
    };
    if let Some(capability) = knobs
        .capabilities
        .iter()
        .find(|capability| !self.capabilities.contains(capability))
    {
        return refuse(KnobKind::Capability, capability.to_string());
    }
    // ... the same for sysctls (by pattern) and devices (by path)
    Ok(())
}
```

`KnobKind` is a three-variant enum, not a `&str` like `"capabilities"`.
The refusal's message needs both the app-side field name and the node-side
key (`allowed_capabilities`), and two `match`es on the enum keep those in
step. A typo in a string would compile.

#### Placement asks the same question

The exec secret provider showed the pattern: a node advertises what it can
do, and `node_can_run` asks before placing. Here the node advertises its
whole allowlist as `NodeCapabilities::runtime_knobs`, and placement calls the
same `admits` the node will:

```rust
&& node
    .capabilities
    .runtime_knobs
    .admits(&requirements.runtime_knobs)
```

So an app that needs `/dev/fuse` lands on the two nodes that list it, and
stays unplaced with a reason if none do, instead of bouncing between nodes
that refuse it. One detail matters: a node advertises its allowlist only if
its runtime applies knobs (`Grill::applies_runtime_knobs`, true only for
rootful runc). A Mac on Apple Container with a generous `[runtime]` section
advertises nothing, so the scheduler never sends it an app it would have to
refuse.

#### Parsing is the validation, again

Some values must never reach any node, whatever its `node.toml` says.
`kernel.panic` is a sysctl, but it isn't namespaced: writing it from one
container changes the whole machine. So, as with stop hooks, the fields parse
into types that can't hold a bad value. `SysctlName` accepts only the
families the kernel keeps per namespace, the same list Docker, runc and
Kubernetes use:

```rust
fn namespaced_sysctl(name: &str) -> bool {
    name.starts_with("net.")
        || name.starts_with("fs.mqueue.")
        || name.starts_with("kernel.shm")
        || name.starts_with("kernel.msg")
        || name == "kernel.sem"
}
```

The node's list uses `SysctlPattern`, which goes through the same check, so
an operator who writes `allowed_sysctls = ["kernel.*"]` gets a startup error
saying why, not a node that quietly allows `kernel.panic`. A pattern may end
in `.*` only after `net.` or `fs.mqueue.`, where everything underneath is
namespaced.

`Capability` was the more interesting type. There are exactly 41 Linux
capabilities, so it holds a `&'static str` pointing into a table of them.
`'static` is a lifetime: it says the borrowed string lives for the whole
program, which a string literal does. That makes `Capability` `Copy`, as
cheap to pass around as an integer, and unable to hold a name the kernel
doesn't know. But serde's derive balked at it. A derived
`Deserialize<'de>` may borrow from its input, whose lifetime is `'de`, and a
`&'static str` field made it demand that the input live forever. No config
file does. So we wrote the impl by hand, reading an owned `String` and
looking it up:

```rust
impl<'de> Deserialize<'de> for Capability {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer)?
            .parse()
            .map_err(serde::de::Error::custom)
    }
}
```

Six lines, and the borrow question goes away: the `String` is dropped once
`parse` has found the table entry.

#### What about CAP_SYS_ADMIN?

We had to decide what to do with the capabilities that amount to root
inside the container: `CAP_SYS_ADMIN`, `CAP_SYS_MODULE`, `CAP_SYS_PTRACE` and
friends. Refusing them outright was tempting. But a FUSE filesystem needs
`CAP_SYS_ADMIN`, and "use a different orchestrator" is exactly the Swarm
answer this issue exists to avoid. So we allow them, on three conditions.
There's no wildcard anywhere: `ALL` is a parse error on both sides, so no
node allows `CAP_SYS_ADMIN` by accident. The node must name each one. And
Bun prints a warning at startup for every escape-hatch capability its
`[runtime]` lists, so it shows up in the node's log rather than hiding in a
config file. It helps that a rootful container runs in its own user
namespace (chapter 1): `CAP_SYS_ADMIN` there acts on the container's own
mounts and namespaces, not the host's. It's still the capability behind most
container escapes, which is why it needs naming.

#### The OCI half, and a namespace that isn't ours

The knobs travel to the runtime in the instance's `OciSpec`, like the stop
policy, and only rootful runc acts on them. The process runtime and Apple
Container refuse a spec that carries them with a `LaunchProblem`, rather than
running the app without the capability it asked for. A missing capability
fails in confusing ways much later; a refusal fails now.

runc's part is deliberately thin. Everything that can be tested on a laptop
lives in one portable function, `grill::knobs::apply_to_spec`:

```rust
pub fn apply_to_spec<F>(
    spec: &mut OciSpec,
    knobs: &RuntimeKnobs,
    lookup: F,
) -> Result<(), DeviceLookupError>
where
    F: Fn(&Path) -> Result<HostDevice, DeviceLookupError>,
```

The `where F: Fn(...)` clause says `lookup` can be any function or closure
that takes a path and returns a device, the way a Go function might take a
`func(string) (Device, error)`. runc passes `host_device`, which `stat`s the
real node. The tests pass a closure over a table, so they can describe a
block device with major 8 on a Mac that has no such thing. Because the
compiler generates a copy of `apply_to_spec` for each `F` it's called with,
there's no indirect call, as with the `Grill` generics in chapter 1.

The function joins the added capabilities to the set the user namespace
already granted (runc's code calls it after `userns::apply`), writes the
sysctls into `linux.sysctl`, and gives each device both a node in
`linux.devices` and an allow rule in `linux.resources.devices`. Without the
rule, the device cgroup would deny the device the container can see.

Then the surprise. runc writes `linux.sysctl` from inside the container, as
container root. For `kernel.shmmax` that works: the IPC namespace is the
container's own. For `net.core.somaxconn` it doesn't. Bun creates each
container's network namespace before runc starts, so the namespace belongs
to the node's user namespace, and container root has no say over it. (That's
why chapter 1's networking code already writes two sysctls itself, to let
container users bind low ports.) So `take_network_sysctls` lifts the `net.*`
entries out of the spec, and runc's prepare step writes them with
`ip netns exec`, like those two:

```rust
let network_sysctls = super::knobs::take_network_sysctls(&mut spec);
netns::write_sysctls_with_commands(commands, instance, &network_sysctls)
    .await
    .map_err(|e| failed(e.to_string()))?;
```

The knobs are new fields in Raft desired state, OCI specs and the capability
report, so they get a line under the 0.2.4 heading in `src/compatibility.rs`.
The bump itself came with stop hooks.

#### Kubernetes

`relish import` maps the parts that have a home. A container's
`securityContext.capabilities.add` becomes `capabilities`, the pod's
`securityContext.sysctls` becomes `sysctls`, and a `hostPath` volume of a
device becomes a device. Everything else is a warning: an unknown capability,
a sysctl that isn't namespaced, `capabilities.drop` (we only add) and
`privileged: true`, which is the very flag this design replaces with a list.
Export goes back the same way, but a device can only leave as a `hostPath`
mount, which Kubernetes won't let a pod open without a device plugin or a
privileged container, so the export report says so.

### Templates

([#1124](https://github.com/reliaburger/reliaburger/issues/1124)) The code is
in `src/config/template.rs`. `Config` gains two maps, and the second one
can't use its TOML name:

```rust
/// Template definitions keyed by name (#1124).
#[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
pub template: BTreeMap<String, TemplateSpec>,
/// Template instances keyed by name. `use` is a Rust keyword, so the
/// field has another name.
#[serde(rename = "use", default, skip_serializing_if = "BTreeMap::is_empty")]
pub uses: BTreeMap<String, UseSpec>,
```

`use` is Rust's `import`, so like `match` in the policy struct it gets a
Rust name and a serde rename.

#### Two shapes for one parameter

A parameter can be written two ways, `name = "string"` or `replicas = { type
= "int", default = 1 }`. serde reads that with an *untagged* enum:

```rust
#[derive(Serialize, Deserialize)]
#[serde(untagged)]
enum RawParam {
    Short(String),
    Full(FullParam),
}
```

A normal Rust enum in serde is *tagged*: the data says which variant it is,
the way a JSON `{"Short": "string"}` would. Untagged means there's no tag, so
serde tries each variant in order and keeps the first that fits. A string
fits `Short`; a table fits `Full`. It's the closest thing Rust has to
Python's "is it a str or a dict?", except the compiler makes us handle both
answers.

`RawParam` is private. What the rest of the code sees is `ParamSpec`, with
the type parsed into an enum and the default checked against it. The
`try_from` attribute we used for stop signals joins the two:

```rust
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(try_from = "RawParam", into = "RawParam")]
pub struct ParamSpec {
    pub kind: ParamType,
    pub default: Option<ParamValue>,
}
```

So `{ type = "int", default = "x" }` fails while the file parses, with
TOML's line number on it, rather than later when somebody uses the template.
A `[use.*]` table works the same way: it's a map of values, and `try_from`
takes the `template` key out of it and keeps the rest as parameter values.

#### Reading the body like a parser

The scanner splits a body into pieces without copying it:

```rust
enum Piece<'a> {
    Text(&'a str),
    Placeholder { name: &'a str, slot: Slot },
}

fn scan(body: &str) -> Result<Vec<Piece<'_>>, String>
```

Each `&'a str` is a slice of `body`, a pointer and a length into memory
somebody else owns. In C you'd hand out pointers into the buffer and hope
nobody freed it. Here the lifetime `'a` ties every piece to `body`, and `'_`
in the return type tells the compiler "the same lifetime as the one
borrowed argument". If the caller dropped the body while it still held the
pieces, the program wouldn't compile.

The scanner itself is a small state machine. `Context` says where we are,
and one function says what the next few bytes do to it:

```rust
fn advance(context: Context, rest: &str) -> (Context, usize) {
    match context {
        Context::Bare if rest.starts_with('#') => (Context::Comment, 0),
        Context::Bare if rest.starts_with("\"\"\"") => (Context::MultiBasic, 3),
        Context::Bare if rest.starts_with('"') => (Context::Basic, 0),
        // ...
        Context::Basic | Context::MultiBasic if rest.starts_with('\\') => {
            let escaped = rest[1..].chars().next().map_or(0, char::len_utf8);
            (context, 1 + escaped)
        }
        Context::Basic if rest.starts_with('"') => (Context::Bare, 0),
        // ...
        _ => (context, 0),
    }
}
```

The match guards are checked top to bottom, so `"""` is tried before `"`.
The backslash arm is there so `"say \"hi\""` doesn't end at the escaped
quote. The fiddly part is `char::len_utf8`. A Rust `&str` is always valid
UTF-8, and slicing one in the middle of a multi-byte character panics. So the
scanner steps over whole characters, never single bytes, and a template full
of emoji still scans.

A placeholder records its `Slot`, which is the context with the comment case
taken away: placeholders in comments never become pieces at all. `render`
then decides, per slot, what the value may be:

```rust
let text = match value {
    ParamValue::Int(number) => return Ok(number.to_string()),
    ParamValue::Bool(flag) => return Ok(flag.to_string()),
    ParamValue::String(text) => text,
};
match slot {
    Slot::Basic => Ok(escape_basic(text)),
    Slot::Bare => { /* a plain key, or refused */ }
    Slot::Literal | Slot::MultiLiteral => { /* no quote or control character */ }
}
```

The first `match` returns early for the two types that are safe anywhere,
and binds `text` for the one that isn't. After it, the compiler knows
`text` is a string, so the second `match` only has to think about where it
goes. Because `match` must be exhaustive, adding a fourth slot one day
won't compile until somebody decides what a string may hold there.

#### Expanding, and taking the maps

`expand` moves the `use` tables out of the config before it expands them:

```rust
let uses = std::mem::take(&mut config.uses);
```

`std::mem::take` swaps a value out and leaves its type's default behind,
here an empty map. We need it because we're about to insert apps into
`config` while walking the uses. If we iterated `config.uses` directly, we'd
hold a shared borrow of `config` while trying to change it, and the borrow
checker refuses that. Taking the map out first gives us our own copy to
walk, and leaves the config without uses, which is what we want at the end
anyway. In Go you'd copy the map, or not notice the aliasing.

Each expanded app gets its provenance, and a clash check runs first: was
this name made by an earlier `use` (named in the error), or declared by the
config itself? Uses expand in name order, because a `BTreeMap` iterates in
key order, so the same files always give the same apps and the same first
error.

#### Three callers, one function

`expand` has three callers, all before validation and policy. The compiler
shared by `relish compile` and Lettuce collects every file's templates
first, then expands each file's uses before that file's defaults and
namespace apply. `relish::manifest::parse`, which `apply`, `lint`, `diff`
and `policy test` read through, expands a single file's own templates. And
the apply endpoint expands a manifest that arrives unexpanded, say from
`curl`. Finally, `Config::validate` refuses a `use` that's still there, so a
future fourth caller that forgets to expand fails loudly instead of quietly
dropping apps.

The only wire and state change is `provenance` on `AppSpec` and `JobSpec`,
so it's a line under the 0.2.4 heading in `src/compatibility.rs`, not a
bump.

### The controller contract

([#1125](https://github.com/reliaburger/reliaburger/issues/1125)) The contract
has three parts, and only one of them needed much new code.

**The scoped token already existed.** `relish token create --namespaces pg`
has been there since chapter 10, and `authorize_scoped` checks it on every
route that names an app. A namespace-scoped `deployer` can't write
namespaces, permissions or policies either; those need an unscoped admin. So a
controller's token was already confined. What it couldn't do was read events.
`/v1/events` refused every scoped token, because events span every tenant,
and filtering looked like work nobody needed. A controller needs it, so now
the handler filters instead of refusing:

```rust
pub(super) fn event_visible(
    auth: Option<&crate::sesame::auth::AuthContext>,
    event: &crate::bun::events::ClusterEvent,
) -> bool {
    if !is_scoped(auth) {
        return true;
    }
    let Some(namespace) = event.namespace.as_deref() else {
        return false;
    };
    match event.app.as_deref() {
        Some(app) => crate::sesame::auth::authorize_scoped(auth, app, namespace).is_ok(),
        None => auth.is_some_and(|context| {
            context.scoped_apps.is_none()
                && context
                    .scoped_namespaces
                    .as_ref()
                    .is_some_and(|namespaces| namespaces.iter().any(|name| name == namespace))
        }),
    }
}
```

An event that names no namespace, a token being minted say, is for unscoped
tokens only. The same predicate filters the list and the live feed, and a
test reads the handler's source to make sure both call it. While we were
there, `?follow=true` learned to stream every node's events, as the
dashboard's WebSocket already did; it used to stream only the node you asked.

**The owner is one field.** `AppSpec` gains `owner: Option<String>`, with
`#[serde(default, skip_serializing_if = "Option::is_none")]` so an app without
one serialises exactly as before. Policies see the spec as JSON, so they got
`app.owner` for free. The `APP_FIELDS` test from #1117 noticed the new key and
failed until we added it to the list, which is the test doing its job.

**The rules are pure functions.** `src/council/owner.rs` has one function per
kind of change. Here's the one an apply calls for each app:

```rust
pub fn judge_apply(
    app: &AppId,
    declared: Option<&str>,
    current: Option<&str>,
    caller: &OwnerCaller,
) -> Result<ApplyVerdict, OwnerError> {
    if let Some(owner) = declared
        && !caller.speaks_for(owner)
    {
        return Err(OwnerError::NotTheCaller { /* … */ });
    }
    match (current, declared) {
        (None, _) => Ok(ApplyVerdict::Write),
        (Some(current), Some(declared)) if current == declared => Ok(ApplyVerdict::Write),
        (Some(current), Some(declared)) => Err(OwnerError::Claimed { /* … */ }),
        (Some(current), None) => Ok(ApplyVerdict::Skip {
            owner: current.to_string(),
        }),
    }
}
```

`match (current, declared)` builds a tuple of the two `Option`s and matches
both at once. Each arm is a pattern over the pair; `_` matches anything, and
`if current == declared` is a *guard*, an extra condition the arm needs before
it's chosen. Arms are tried top to bottom, so the guarded arm gets first go at
two owners, and the next arm only sees two that differ. The compiler checks
that the four arms cover every combination of `Some` and `None`. Delete the
last one and the build fails, naming the missing case. In C or Go this would be
a ladder of `if`s, and nothing would tell you if you'd forgotten a rung.

Two choices in that table deserve their reasons.

*Why skip, not refuse?* Because of how people use apply. A template expands to
`db-a`; a controller later claims it. If re-applying the directory that holds
the template refused the whole apply, every re-apply would fail, and GitOps
would fail every sync and never advance its commit. Skipping with a note is
what Lettuce already does with what it didn't write, and the note means it
isn't silent.

*Why tie the owner to a token's name?* The first sketch let any string be an
owner, and treated it as a label. Then anyone allowed to deploy in `pg` could
write `owner = "db-failover"` by copying a file, and silently take part in the
controller's writes. Requiring the token named, or an admin, makes the owner
mean something, and it's what makes `relish release` safe to offer: the
release endpoint knows who's asking.

**The state machine is the backstop.** The leader checks owners in
`cluster_apply` before policy runs, so a skipped app is neither judged nor
written. But the leader reads desired state and then proposes, and a
controller and a human can both read "unowned" in the same moment. So the
state machine refuses an app write that would change a stored owner:

```rust
if let Some(reason) = self.owner_conflict(app_id, spec) {
    return Some(CouncilResponse::Refused { reason });
}
```

It can't check *who* is writing: a Raft entry carries no caller, and the state
machine must apply the same log the same way on every node. So it compares two
strings, the stored owner and the incoming one, and that's all. It's the
policy fence's principle again: the state machine compares, the leader judges.
That leaves exactly one way to clear an owner, a dedicated entry:

```rust
RaftRequest::AppRelease { app_id } => {
    let Some(spec) = self.state.apps.get_mut(app_id) else { /* refuse */ };
    if spec.owner.take().is_none() {
        return Some(CouncilResponse::Refused { /* "has no owner to release" */ });
    }
}
```

`Option::take` moves the value out of the `Option` and leaves `None` behind,
returning what was there. One call clears the owner and tells us whether
there was one to clear. `get_mut` hands out a mutable borrow of the stored
spec, so the change happens in place, and the borrow ends with the block.

**Claiming isn't a new version.** A rollout compares versions by
`rollout_fingerprint`, a hash of the spec with the replica count set aside.
The owner is set aside too, and so is it in each node's placement
fingerprint. Without that, a controller claiming an app would roll every
replica of it, which is the kind of surprise that makes people distrust a
contract.

**Check before you forward.** A follower can't write to Raft, so a `relish
delete` that lands on one is forwarded to the leader, under the node's
service token. By then the caller's identity is gone. So `stop`, `delete`,
`release` and `rollback` check the owner on whichever node receives them,
before forwarding, and the leader lets the service principal through.
`relish apply` doesn't have this problem: its forward carries the caller's
own token.

**The JSON is pinned.** `src/bun/api/contract_tests.rs` builds one example of
every shape a controller reads or writes, and checks its JSON against a
snapshot with `insta`, the snapshot library the CLI's output tests use. A
snapshot is a file committed next to the test. Change a field's name, and the
test fails and shows the difference. The manual's "Writing a controller"
chapter documents the same shapes field by field, and the test's comment says
to update both.

**The example runs as a test.** `examples/controller/main.rs` imports nothing
from `reliaburger`: it's `reqwest`, `serde_json` and `toml`, the way anyone's
controller would be. The portable suite compiles that same file into itself:

```rust
#[allow(dead_code)]
#[path = "../../examples/controller/main.rs"]
mod controller;
```

`#[path]` tells the compiler where a module's file is, instead of the usual
`controller.rs` next to the parent. The example's `main` is never called from
the test, hence `allow(dead_code)`. Its functions are `pub` for a reason that
surprises people coming from Python: Rust's privacy works downwards. A module
sees everything in its parents, but a parent sees only what a child marks
`pub`. The test is the parent here.

The controller decides with one pure function, `decide`, and part of it
uses a *slice pattern*:

```rust
let existing: Vec<usize> = (0..2).filter(|&index| views[index].exists).collect();
let current = match existing[..] {
    [] => return Decision::Write { active: 0, /* … */ },
    [only] => only,
    _ => { /* both exist: keep the one that runs */ }
};
```

`existing[..]` is the vector viewed as a slice, and `[]`, `[only]` and `_`
match a slice with no elements, exactly one (binding it to `only`), or any
other length. It reads like the rule it implements: none, one, or more.

### Everything together

([#1126](https://github.com/reliaburger/reliaburger/issues/1126),
[#1311](https://github.com/reliaburger/reliaburger/issues/1311)) The exit
test adds almost no production code, and that's the point of it. It takes
what the rest of the train built and puts it together the way a platform
team would: one file of policies, a template, a controller, a cluster. What
it adds is that file, a few lines in the example controller, three cluster
tests, a benchmark and a check in the soak.

#### Four policies in one file

`examples/policies/platform.toml` is the four rules platform teams ask for
most, and the cluster test applies it unchanged. Two of them are worth
reading closely, because the obvious version is wrong:

```toml
[policy.no-latest]
applies_to = "both"
match = "has(workload.image)"
deny = "workload.image.matches(':latest(@|$)') || !workload.image.matches(':[^/:@]+(@|$)')"

[policy.trusted-registries]
applies_to = "both"
match = "has(workload.image)"
deny = "!['registry.example.com/', 'ghcr.io/example/'].exists(prefix, workload.image.startsWith(prefix))"
```

The chapter's first example, `app.image.endsWith(':latest')`, misses
`web` with no tag at all, which pulls `:latest` too, and `web:latest@sha256:…`.
`matches` is CEL's regular expression test (the crate's `regex` feature
provides it). The second pattern asks for a tag: a colon followed by
characters that aren't `/`, `:` or `@`, at the end or before a digest. That
also stops `registry.example.com:5000/web` passing, since its only colon is
a port. A digest on its own, `web@sha256:…`, passes: a digest is a pin. The
`match` keeps both rules off host processes, which have no image.

The registry rule uses `exists`, a CEL macro that's true when its expression
holds for some element of the list, binding each one to `prefix` in turn.
The trailing `/` on each registry is the important character. Without it,
`registry.example.com.evil.net/web` passes. `src/policy/check.rs` gives
the loop variable its own scope, so `prefix` can't leak out and be mistaken
for a field.

#### A controller that drives a template

The example controller took two `[app.*]` tables. To drive a template it
needed two small changes, and neither touches the crate. Given a manifest
with `[use.*]` tables, `manifest` keeps the active `use` and fills in two of
its parameters, `owner` and `namespace`, and the cluster expands it. And
`app_names` reads each `use`'s `name` parameter, the app it makes:

```rust
let names = uses
    .iter()
    .map(|(key, value)| {
        value
            .get("name")
            .and_then(toml::Value::as_str)
            .map(str::to_string)
            .ok_or(format!("use.{key} has no `name` parameter naming its app"))
    })
    .collect::<Result<Vec<String>, String>>()?;
```

The closure gives back a `Result` for each `use`, so the iterator yields
`Result<String, String>` items. `collect` can build many things, and the
type in its turbofish (`::<...>`) picks which one. Asking for
`Result<Vec<String>, String>`, rather than `Vec<Result<...>>`, turns the
whole list inside out: `Ok` holding every name if each item was `Ok`, or the
first `Err`, at which point it stops reading. The `?` after it returns that
error. In Go this is the loop that appends and returns on the first error;
here the error handling is in the type you collect into.

`.and_then(toml::Value::as_str)` passes a method as a function. `as_str`
takes `&Value` and returns `Option<&str>`, which is just the shape
`and_then` wants, so there's no need to write `|value| value.as_str()`.

#### Waiting on a cluster with an async closure

Every cluster test polls: wait until a leader answers, until GitOps reports,
until the standby runs. The fixture's `wait_until` takes a description, a
deadline and a condition, and the condition is an *async closure*:

```rust
wait_until(
    "the controller fails over to db-b, running on rack b",
    Duration::from_secs(150),
    async || {
        check_once(&mut db_failover, &api).await;
        let state = nodes[1].council.desired_state().await;
        !state.apps.contains_key(&db_a)
            && state.apps.contains_key(&db_b)
            && running_on(&nodes, &[1, 2], "db-b").await.as_deref()
                == Some(nodes[1].name.as_str())
    },
)
.await;
```

`async || { ... }` arrived in Rust 1.85. Calling it gives a future, and
`wait_until` awaits one per poll; its parameter is `impl AsyncFnMut() ->
bool`. The older spelling, `|| async { ... }`, is a plain closure that
returns a future, and it can't do what this one does. The future would need
to borrow `db_failover` mutably, but a plain closure's result can't hold a
borrow of the closure's own captures, because nothing stops you calling the
closure again while the first future is still alive. An async closure's
call ties the borrow to that one future, so each poll gets the controller
mutably, in turn. The C equivalent is a callback with a context pointer and
a promise in a comment that nobody calls it twice at once; here the
signature makes the promise.

#### The benchmark

`benches/policy.rs` builds a set of 10, 100 or 1,000 policies: the four
platform ones, which match every app, and copies of the same four shapes
scoped to other teams' namespaces. Those don't match the app being judged,
but each one's `match` still runs, and a quarter of them carry a default.
That's the shape a cluster grows into with many teams, rather than a best
case where nothing is evaluated. It measures two things per size:

```rust
group.bench_with_input(BenchmarkId::new("compile", count), &specs, |b, specs| {
    b.iter(|| PolicySet::compile(specs).expect("the set compiles"));
});
// ...
group.bench_with_input(BenchmarkId::new("judge_app", count), &set, |b, set| {
    b.iter(|| judge_app(set, &app_id, &spec, &caller, Source::Apply));
});
```

`bench_with_input` is Criterion's way of running one benchmark per
parameter, naming each `policy/compile/100` and so on. Compiling happens
once per apply; judging happens once per app. Before timing anything the
bench asserts the app passes and exactly one field is filled, so a change
that made every policy fail closed couldn't quietly report a fast number.

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

Defaults ([#1118](https://github.com/reliaburger/reliaburger/issues/1118))
added their own:

- **The default table** (`the_default_table`): a default fills an absent
  field key by key; `match` decides; the app's own value wins; an existing
  table gains only the keys it lacks; the first policy by name wins a field
  two of them fill; and a `match` sees what earlier policies filled.
- **Validation and deny see the filled spec**: a `deny` on `!has(app.memory)`
  passes once a default fills `memory`, and a default of `port = 0`, which has
  the right type and so commits, refuses the app on the built-in check,
  naming the policy.
- **Every kind of bad default** is refused at commit: no `deny` and no
  `default`, an empty default, an unknown field, `namespace` or `replicas`, a
  misspelt nested key, a value of the wrong type, and a table over the cap.
- **Property tests.** `defaulting_is_idempotent` fills a generated app and
  fills the result again, and expects nothing more to fill.
  `a_default_never_changes_a_field_the_user_set` checks that every value of
  the declared spec is in the filled one, unchanged, and that no field the
  user set is reported as filled. `default_order_does_not_depend_on_map_order`
  builds the policy set from shuffled and hashed maps and expects the same
  judgement, with the first name winning each contested field. The four
  generated policies overlap on purpose, two on `memory` and two on
  `env.TEAM`, because a property test of non-overlapping defaults would prove
  very little.
- **Snapshots**: the `relish policy test` report and the `relish apply
  --dry-run` plan, each listing the filled fields. The plan test also checks
  that a defaulted app reads as unchanged.
- **The state machine** (`a_declared_spec_lives_beside_the_effective_one_until_the_spec_changes`):
  a fenced write keeps the declared spec, the same spec again keeps it, a
  different spec clears it, and a refused write or a delete leaves no entry.
- **The diff** (`a_field_a_policy_filled_in_is_not_drift_from_git`) checks
  the trap both ways: with the declared spec there's no change, and without
  it there's an update on every sync.
- **The portable suite** (`repeated_syncs_of_a_defaulted_app_do_not_reapply_it`)
  syncs a repository whose policy fills `env.TEAM`, then commits an unrelated
  app and then a comment. The sync history must show one add and no
  modification, then nothing at all. We checked it the honest way: with the
  diff line reverted, it fails with `modified: 1`.
- **Through the router**: an apply streams `policy team-label fills env.TEAM
  = "payments"`, stores the filled spec and the declared one, and keeps the
  app's own `memory`.

Jobs and the audit action
([#1119](https://github.com/reliaburger/reliaburger/issues/1119)):

- **What a policy applies to** (`what_a_policy_applies_to_decides_what_it_judges`)
  judges one app and one scheduled job, both on `:latest`, by a policy of
  each kind: no `applies_to`, `jobs`, `both`, an apps policy reading
  `workload`, and an `audit` policy for both. The first row is the
  compatibility promise: an old policy still judges the app and only the
  app.
- **Every kind of bad job policy** is refused at commit: `job` in an apps
  policy, `app` in a jobs policy or one for both, `workload.port`, a field
  jobs don't have, and a default on a jobs policy. One case checks the full
  message, because it tells the reader which variables they do have.
- `job_fields_match_the_job_spec` and
  `workload_fields_are_the_fields_apps_and_jobs_share` keep the two new
  hand-written lists honest.
- **The audit** (`the_audit_lists_what_the_policies_would_now_say`) judges
  two apps and a job and checks each finding, including that the
  GitOps-owned app is judged as a GitOps sync. Its report is an `insta`
  snapshot. Another test covers the three quiet answers: every workload
  passes, there are no policies, and a policy no longer compiles.
- **Through the router**: a jobs policy refuses a job manifest with 422 and a
  `policy.denied` record, and writes no job definition; an audit policy lets
  an apply through, says nothing to the caller, and leaves one
  `policy.audited` record; and the audit route lists an app committed before
  a policy that would now refuse it, without changing the app.
- **`relish wtf`** (`warns_about_committed_workloads_the_policies_would_now_refuse`):
  denials and audit findings become warnings named by workload, warnings are
  left to the audit report, a clean audit is an OK, and no policies says
  nothing.
- **The portable suite** (`a_new_policy_leaves_running_work_alone_and_the_audit_names_it`)
  does the same on a real council through the public router: the app's
  stored spec is unchanged after the policy commits, and the audit names it.

The two cluster tests the plan still wanted, an admin removing a policy
that denies everything on more than one node and the exit test itself,
arrived together at the end of the train. They're described in
[The exit test](#the-exit-test) below.

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

Two more came out of the review of the 0.2.4 train. On Apple silicon,
`make test-apple` runs
`a_stop_only_signals_so_a_slow_workload_finishes_within_bun_s_grace`: a
workload traps SIGTERM, sleeps 10 s and prints `finished-cleanly`. The stop
must return within 3 s, the workload must print its line, and a second stop
of the exited workload must succeed. Against `container stop` it failed
before it got that far: the CLI held the call for its own 5 s, then gave up
with an XPC timeout. And `the_wait_for_remote_withdrawal_gives_up_at_its_patience`
covers the wait before a rollout stops an old instance, the one that holds
it serving until every node has dropped its backend (#1355). A node that
never confirms mustn't hold the stop for ever, so the test answers
"pending" to every ask, on tokio's paused clock, and checks the wait gives
up at its 5 s patience, not before and not after.

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

### Testing runtime knobs

Each gate is tested alone first, then both together, because a double gate
whose halves are never tested apart can quietly become a single gate.

- **The token gate alone**: `a_runtime_knob_needs_host_exec_and_the_refusal_names_the_token_gate`
  applies an app with `CAP_NET_ADMIN` as a deployer whose permission block
  grants only `deploy`. It must get a 403 whose body names
  `runtime.capabilities CAP_NET_ADMIN` and "token gate", and the council
  must hold no app. The same manifest under `deploy` and `host-exec` goes
  through.
- **The node gate alone**: `runtime_knobs_pass_only_the_node_gate_of_a_runtime_that_applies_them`
  admits the app in a supervisor whose runtime applies knobs but whose
  allowlist is empty, and reads the refusal for the knob, "node gate" and
  `allowed_capabilities`. The agent test
  `a_deploy_the_node_gate_refuses_never_reaches_the_runtime` runs a whole
  deploy and checks the mock runtime was never asked to create anything.
- **Both keys**: `a_deploy_both_gates_allow_hands_the_knobs_to_the_runtime`
  checks the spec the runtime received carries exactly the app's knobs.
- **The wrong runtime**: the process runtime and Apple Container each refuse
  a spec with knobs, with a message naming the knob and the runtime, and a
  node advertises its allowlist only when its runtime applies knobs.
- **What no node may allow**: `a_host_wide_sysctl_is_refused_and_says_why`
  tries `kernel.panic`, `vm.overcommit_memory`, `fs.file-max` and the
  near miss `kernel.semx`. `a_node_cannot_allowlist_a_host_wide_sysctl` does
  the same to `node.toml`, with `kernel.*` and a bare `*` as well. And
  `a_host_wide_sysctl_fails_the_whole_config_at_parse` shows the error at
  `relish lint`, before any gate is asked.
- **Escape hatches**: `an_escape_hatch_capability_needs_the_node_to_name_it`
  shows that a node allowing `CAP_NET_ADMIN` doesn't admit `CAP_SYS_ADMIN`,
  and that naming it does, and appears in the startup warning's list.
  `a_capability_wildcard_is_refused_with_its_reason` covers `ALL`, `all` and
  `CAP_*`.
- **Placement**: `an_app_with_runtime_knobs_lands_only_where_the_node_allows_them`
  builds three nodes (one allowing FUSE, one allowing network tuning, one
  allowing nothing) and checks each app lands only where every knob is
  listed, and that one asking for a mix no node allows is unplaced with
  `NoCapableNode`.
- **OCI generation**: the tests in `src/grill/knobs/tests.rs` compare the
  serialised spec against the JSON runc expects: added capabilities joined
  to the user namespace defaults with no duplicates, `linux.sysctl`, and
  each device as a node plus a matching cgroup rule, with the resource
  limits already in the spec left alone. `network_sysctls_are_taken_out_for_the_node_to_write`
  checks the split. `linux_device_numbers_split_like_glibc` round-trips
  device numbers through a copy of glibc's `makedev`, including a major
  above 4095 that needs the high bits.
- **Kubernetes**: one import test carries every case at once, mapped and
  unmapped, and a round trip sends an app's knobs through `relish export`
  and back in.

None of these run runc. The runc half is two calls and a `stat`, and it's
Linux-only, so it's exercised by `make test-linux` rather than by the
portable suite.

### Testing templates

([#1124](https://github.com/reliaburger/reliaburger/issues/1124)) Templates
are pure functions from text to config, so nearly all of their tests are
unit tests in `src/config/template/tests.rs`, with no node in sight:

- **Parsing**: both shapes of parameter, a `use` with typed values, a TOML
  and JSON round trip, and a table of malformed ones (an unknown type, a
  default of the wrong type, a misspelt key, no `body`, a `use` without
  `template`, a float value), each failing with its reason.
- **Expansion**: a `use` makes an ordinary app with its provenance and the
  default filled in; two uses make two apps; a body's jobs carry provenance
  too; a config with no templates comes out unchanged.
- **Injection**: `a_value_inside_quotes_is_escaped_so_it_cannot_add_tables`
  passes a value that closes its string and declares `[app.evil]`, and
  checks there's still one app, with the value intact in its environment.
  Its siblings check the other slots: a string outside quotes must be a
  plain key, a literal string refuses a quote, integers and booleans go
  anywhere, `$${` survives as `${` for a shell, comments are left alone, and
  a value that looks like a placeholder isn't expanded again.
- **Every refusal has a test**: an unknown template, an unknown parameter, a
  missing one, the wrong type, a clash with a declared app, two uses making
  one name, a nested template or `use`, a body with a policy in it, a
  placeholder no parameter resolves, bad names, and a body that isn't valid
  TOML. The ones whose wording matters compare the whole message.
- **A property test**, `expansion_is_deterministic_and_leaves_no_placeholders`,
  generates names, counts, flags and strings full of quotes, backslashes,
  newlines and Unicode, puts them in a basic string, a multi-line string, a
  literal string, a command array and a bare position, and checks three
  things: expanding twice gives the same config, the rendered result holds no
  `${`, and every value comes back out of the parsed app exactly as it went
  in. That last one is the injection promise, stated as a property.

Around it:

- **The compiler** (`relish::compile::tests`): a template in one file serves
  a `use` in another, and the app takes the `use`'s directory namespace and
  `_defaults.toml`, under both duplicate policies; a template declared twice
  is refused, naming both files; an expanded app that collides with one in
  another file is refused even when `relish compile` would only warn.
- **Policies** (`policy::tests`): a `deny` matching `app.provenance.template`
  refuses a one-replica Postgres and passes a two-replica one; a `default`
  fills a field on an expanded app and keeps its provenance; and a default
  that tries to set `provenance` is refused at commit. A job policy matching
  `job.provenance.template` refuses a templated migration job pinned to
  `:latest`. `app_fields_match_the_app_spec`
  failed the moment `provenance` joined `AppSpec`, which is the job it was
  written for.
- **Owners** (`bun::api::owner_tests`): a template whose body declares
  `owner = "${controller}"` is refused with a 403 for a human's token and
  committed for the controller's, and the stored app keeps its owner, its
  provenance and the field a policy default filled, with the declared spec
  holding the owner and provenance but not the filled field.
  `the_provenance_is_not_part_of_the_version` checks the rollout
  fingerprint, beside the test that does the same for `owner`.
- **Plan and diff**: an insta snapshot of `relish apply --dry-run` for two
  uses and a hand-written app, with a policy denying one of them, plus a
  check that the same apps planned against their own fingerprints are all
  unchanged. A `relish diff` test changes one parameter and expects a change
  to the app, not to a `use`.
- **The portable suite** (`tests/suite/templates.rs`): the config `relish
  apply` sends and the raw manifest both store one ordinary app, the same
  one, with its provenance; a policy refuses an expanded app with a 422 and
  writes nothing; a `use` that can't expand answers 400. And
  `repeated_syncs_of_a_templated_app_do_not_reapply_it` is #1118's diff-trap
  test for templates: a GitOps repository with the template in one file and
  the `use` in another syncs one app, then an unrelated app is added without
  touching it, then a comment-only commit changes nothing, and finally a
  changed parameter is exactly one modification.
- **Published manifests**: `documented_manifests` and `doc_snippets` now
  expand templates before they validate, the way `relish apply` does, so the
  manual's template and `examples/templates/container-postgres.toml` are
  checked like any other manifest. Before that change, the example's
  `use`-only shape would have been skipped as "not a workload" and never
  checked at all.

### Testing the controller contract

([#1125](https://github.com/reliaburger/reliaburger/issues/1125)) Each rule in
the owner table has a test at the level where it lives.

- **The pure rules** (`council::owner::tests`) walk the table: an unowned app
  is written, only the named token or an admin may declare an owner, an
  apply without the owner skips an owned app whoever sends it (even the owner:
  only a release clears it), a different owner is a 409 even for an admin,
  and the imperative changes need the owner or an admin.
- **The state machine** (`state_machine::tests`): an owned app refuses a write
  that drops or changes its owner and keeps the owner's spec; a release clears
  it and then an ordinary write applies; claiming doesn't clear a failed
  rollout; git can't declare an owner, and a claim takes the app out of the
  GitOps ownership set, so GitOps can't prune it afterwards.
- **Through the router** (`bun::api::owner_tests`), with a real council, a
  controller token scoped to `pg`, a human deployer and an admin: a re-apply
  skips the owned app with the note and still writes the rest of the file;
  the owner's writes go through; declaring someone else's name is a 403; a
  rival controller gets a 409; stop, delete (with or without `force`) and
  release are 409 for the human and allowed for the admin and the owner; a
  release hands the app back; the controller's token can't touch another
  namespace; and the desired-apps listing names the owner within the token's
  scope.
- **GitOps** (`lettuce::diff::tests::diff_leaves_controller_owned_apps_alone`):
  git declaring a newer image doesn't update an owned app, and git dropping
  one GitOps wrote before the claim doesn't prune it.
- **Events** (`a_scoped_token_reads_only_its_own_namespaces_events`): a token
  scoped to `team-a` gets its own app's event and its namespace's, not
  `team-b`'s and not the cluster's.
- **Not a version** (`the_owner_is_not_part_of_the_version`): claiming leaves
  the rollout fingerprint alone.
- **Policies** (`a_policy_sees_the_owner`): `app.owner` is there to match on,
  and `every_kind_of_bad_default_is_refused_at_commit` refuses a default that
  sets it.
- **The audit** (`the_audit_judges_a_controller_owned_app_as_its_controller`,
  `the_audit_reads_the_controllers_role_from_its_token`): `relish policy
  audit` judges an owned app as the controller's token, with that token's
  role, and falls back to the cluster's `audit` caller when the token is gone.
- **The JSON** (`bun::api::contract_tests`): one snapshot per shape. A failure
  here means the wire format changed; the diff `insta` prints is the change.
- **The example** (`tests/suite/controller_example.rs`): the decision rules on
  their own, then the controller against a real council over HTTP. It starts
  and claims `db-a`, an admin's re-apply of `db-a` is skipped, and since
  nothing runs in the test, the second missed check fails over: `db-b` is
  applied and `db-a` deleted. A second test points the controller at another
  namespace and gets a 403.

### The exit test

([#1126](https://github.com/reliaburger/reliaburger/issues/1126),
[#1311](https://github.com/reliaburger/reliaburger/issues/1311)) The exit
test runs on the `make test-cluster` fixture in `tests/cluster_failover.rs`:
three nodes in one process, each a full node with gossip, Raft, the HTTP API,
the scheduler and a real agent, talking to each other over TCP on
localhost. It isn't three machines, and it doesn't run containers, which
matters for the third test.

- **The four policies through both front doors and a leader kill**
  (`the_platform_policies_hold_through_apply_gitops_and_a_leader_kill`).
  An admin applies `examples/policies/platform.toml` as published. Through
  `relish apply`, sent to a follower, which forwards it with the caller's
  token, each rule refuses its case with a 422 naming it, and a passing
  `prod` app is stored with `TEAM = "platform"` filled in and its declared
  spec kept beside it. Through GitOps, running on all three nodes as Bun
  runs it, one commit with three apps lands the one that passes and names
  `trusted-registries` and `no-latest` for the two that don't. Then the
  leader is killed twenty milliseconds into an apply of twenty apps. The new
  leader holds the same four policies at the same generation, refuses
  `:latest` through `relish apply`, refuses it again in the next GitOps
  commit, which its own loop picks up, and accepts the interrupted apply
  when it's sent again, every app with its team filled in.
- **Break-glass on three nodes**
  (`an_admin_removes_a_policy_that_denies_everything_through_any_node`,
  #1311's third item). A policy with `deny = "true"` for apps and jobs
  refuses a deployer's app, an admin's app and an admin's job. It doesn't
  refuse a policy write, because policies never judge policies. With the
  leader killed, a deployer's delete through a follower of the new leader
  gets a 403; an admin's succeeds, forwarded to the leader, and both
  survivors then hold the other policy and the app that now applies.
- **A template and a controller failing over**
  (`a_controller_fails_a_templated_database_pair_over_when_its_node_dies`).
  The nodes carry `rack=a`, `rack=b` and `rack=c`. The example controller,
  compiled into the test with `#[path]` and using nothing but HTTP and JSON,
  drives `examples/controller/postgres-pair.toml` with a token scoped to
  `pg`, with the platform policies in force. It starts `db-a`, pinned to rack
  `a`. When the node on rack `a` is killed, `db-a` has nowhere to run; after
  enough missed checks the controller applies `db-b`, which runs on rack `b`,
  and deletes `db-a`. The stored `db-b` carries the controller as owner, the
  template in its provenance and the team a policy filled in, because a
  controller's writes go through the same apply path as everyone's.

Two things here are stand-ins, and it's better to say so plainly. **Postgres
is a stand-in.** The fixture's nodes run each container's command as a host
process (`tests/support/host_containers.rs`) and pull nothing, and a host
process can't enforce a memory limit. So the test takes the published
template and changes two lines: its image becomes one from a trusted
registry with `command = ["/bin/sleep", "600"]`, and its memory limit goes.
Everything else (the parameters, the port, the rack placement, the owner)
is the published file. **The failover has no data.** The controller fails
over a cold standby, and the standby starts empty. A real database failover
streams the data ahead of time, promotes the standby and fences the old
primary, and that is the database's job and the controller author's, not the
contract's. What the test proves is the contract: a controller written
against the public API, scoped to one namespace, can see a member die and
bring up its replacement on another rack, with the cluster's policies
judging what it writes.

All three pass. The first run of the third failed, and taught us something.
The test checked every half second, with a threshold of three misses, and a
fresh app takes several seconds to be placed and started. So `db-a` counted
as failed before it ever ran, and the controller swapped the pair back and
forth every few seconds without either one running. The fix was to check
every two seconds with a threshold of seven, about the five-second checks
the example makes. The manual now says to keep the threshold times the
interval longer than an app takes to start.

The soak (#1311's first item): the V02 workload,
`scripts/release/sustained/soak-workloads.toml`, now carries
`soak-pinned-images` (a deny), `soak-memory-limits` (a warn) and
`soak-team` (a default). The harness applies the file at the start and at
every generation, including the rolling deploy it kills a node under, so
evaluation runs under faults. Each policy matches only workloads named
`soak-*`. A first draft denied any unpinned image anywhere, which would have
refused the catalogue pulse's `relish test` apps in their leased
namespaces: policies are cluster-wide, so a soak's policies need a `match`.
`tests/suite/platform_policies.rs` checks offline that the three admit every
soak app and job, as the harness writes the file, and that the warn and the
default fire on every soak app, so every apply exercises them. The checker,
`sustained_check.py`, reads `relish policy list` after the first apply and in
every snapshot, and fails on a policy that's gone or changed, or a policy
generation that went backwards. The agent-loop turn budget check is
untouched: it already judges every turn, whatever caused it, and
`test_the_turn_budget_is_still_judged_with_the_policies_in_force` checks it
still fails a slow turn in a snapshot where the policies are in force. No
soak has run with the policies yet; the next V02 tier is the first.

The bench (#1311's second item), run once on the release laptop
([record](../qualification/2026-10-24-policy-evaluation-bench.md)):

| Policies in force | Compile the set (per apply) | Judge one app |
|------------------:|----------------------------:|--------------:|
| 10 | 1.21 ms | 69 µs |
| 100 | 11.5 ms | 248 µs |
| 1,000 | 115 ms | 2.06 ms |

Both grow linearly: about 115 µs per policy to compile, and about 2 µs
per policy for each app judged. At the tens of policies we expect, policy
adds well under a millisecond per app. At 1,000, one app's apply pays about
117 ms, nearly all of it compiling, because compiling repeats every commit
check. GitOps already keeps its compiled set while the generation holds;
`relish apply` could do the same if anyone ever runs a thousand policies.
The laptop was shared with other builds during the run, so these are upper
bounds for it, not a quiet baseline.

## Lessons

We wrote one lesson before the code, because Kubernetes paid for it so we
don't have to: the cost of an extension point isn't in what it lets people
do. It's in what it can stop the cluster from doing when it breaks. Keep
extensions in-process, bounded and off the state machine, and most of that
cost goes away. The exit test is where we checked it: a policy that denies
everything was removed through a follower of a leader that had just taken
over, and a leader killed mid-apply handed over the same policies at the
same generation.

A few more came from building it.

- **The obvious rule is often the wrong rule.** `endsWith(':latest')` reads
  like "no `:latest`" and lets an untagged image through. A registry prefix
  without its trailing `/` lets a look-alike host through. Publishing a
  tested file of policies is worth more than a page of advice, which is why
  `examples/policies/platform.toml` has a table of cases behind it.
- **Policies are cluster-wide, so scope them.** The first draft of the soak's
  deny would have refused the apps the soak's own catalogue pulse deploys.
  A `match` costs one line.
- **A controller's patience must outlast placement.** Our first cluster run
  failed a healthy pair back and forth because three quick checks were
  shorter than an app takes to start. The contract can't know how long your
  apps take; the controller has to.
- **Measure before optimising.** We expected judging to dominate. It's
  compiling, and only at sizes nobody has asked for yet, so it stays a note
  rather than a cache.
