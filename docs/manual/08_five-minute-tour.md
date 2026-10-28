# Five-minute tour

The same tour as the homepage's "Try it in five minutes": install, build a
burger on a three-node laptop cluster and watch it, layer by layer, while you
cook a batch of patties beside it, cut a layer off, roll one forward, roll a
bad one back, scale one up, kill one, slow it down and lose a whole node. Open it any time with
`relish manual tour`.

It needs a Mac with Apple silicon, or Linux with QEMU and KVM, plus about 8 GiB
of free memory and 15 GiB of disk. The one-line install fetches the signed 0.2.3 release.

## Install and build the cluster

```sh
curl -fsSL https://reliaburger.com/install.sh | sh -s -- --laptop
```

This installs `relish`, then builds a three-node cluster in Linux VMs on this
computer. Without `--laptop` the installer only explains the choices. Every
download is checked against a pinned digest. It's the only step that takes
minutes: about a minute and a half once the downloads are cached, closer to
four the first time. If `relish` isn't on your `PATH` yet, the installer says how to
add it and setup prints the full path in its next steps.

## Build the burger

```sh
curl -fsSL https://reliaburger.com/demo/burger.tar.gz | tar xz
relish build burger/burger.toml
```

That's one small Go program, its tests and a Dockerfile, in `./burger`. It's
`examples/demo/burger` in `relish manual examples` too. `relish build` reads
the `[build.burger]` section, tars the directory and uploads it to the
cluster's registry, Pickle, through the quickstart's forward. A node then
builds the Dockerfile with Buildah, pushes the result into Pickle as
`burger:v1` and signs it with the cluster's build key. No Docker on your
laptop, no registry account, no `docker login`. The first build pulls the Go
toolchain image, so give it a minute; after that it takes seconds.

Builds target `linux/amd64` and `linux/arm64` by default. The Dockerfile
cross-compiles: its Go stage runs on the builder's own architecture and tells
the compiler which one to target, so neither platform needs an emulator.

```sh
relish images
```

What the cluster's registry holds. `burger:v1` is one row with both
platforms: an x86 node and an Arm one each pull their own. It's the only image
the rest of the tour needs.

## Run it

```sh
relish apply burger/burger.toml
relish status
relish council status
```

Eight apps from that one image. Each layer of the burger is its own app,
named after it: `top-bun`, `lettuce`, `tomato`, `cheese`, `patty` and
`bottom-bun`, each the same image with `LAYER` and `VERSION` set. In front of
them is the `counter`, and a `diner` orders a burger twice a second so there's
always traffic to look at. `burger:v1` is a bare name: nodes pull it from
Pickle, and because it's signed it would deploy even on a cluster that
requires signatures.

Give it half a minute. Three counter replicas, one on each node, named
`default__counter-0`, `-1` and `-2`. The scheduler spread them and numbered
them across the cluster, not you. `status` opens with a line on the council,
the Raft group that holds the cluster's state, and `council status` asks every
node for its own view of it: its role, term and log position, with the leader
and quorum above the table.

```sh
curl http://burger.localhost:18080
```

The burger, through the built-in ingress:

```text
  ▟  '  .  '  .  '  .  '  .▙   top bun     v1  sesame         rb-bab43735aab6-1 #f56d    4.0 ms
  ~^~^~~^~^~~^~^~~^~^~~^~^~~   lettuce     v1  iceberg        rb-bab43735aab6-1 #d42b    3.0 ms
  ▐  o    o    o    o    o ▌   tomato      v1  beef tomato    rb-bab43735aab6-1 #721d    2.3 ms
  ◢   ▾       ▾       ▾    ◣   cheese      v1  cheddar        rb-bab43735aab6-1 #6114    4.1 ms
  ◢   ▾       ▾       ▾    ◣   cheese      v1  cheddar        rb-bab43735aab6-2 #4c4a    4.0 ms
  ▐░▒░▒░▒░▒░▒░▒░▒░▒░▒░▒░▒░▒▌   patty       v1  beef           rb-bab43735aab6-1 #3299    4.1 ms
  ▜                        ▛   bottom bun  v1  sesame         rb-bab43735aab6-1 #1c15    3.4 ms

  patties cooked: 0 · served by counter rb-bab43735aab6-1 #b11d
```

In a terminal each layer is in colour. The counter asks every layer for its
slice by service name (`http://cheese:8080/layer`), exactly as it would in a
Kubernetes pod, and stacks whatever answers. Each line names the layer's
version and the node and instance that served it. An instance is a process: a
restarted or replaced one gets a new number. A layer with no healthy instance
is a gap in the burger, with the reason. Cheese runs two replicas, so it's
two slices; every other layer runs one.

Open <http://burger.localhost:18080> in a browser for the same burger in SVG,
redrawn every second, and keep it open for the rest of the tour. A script can
ask for JSON with `Accept: application/json`.

## Cook a batch of patties

```sh
relish batch submit burger/jobs.toml
relish batch watch 1
relish jobs
```

`burger/jobs.toml` asks for 10 tasks from the same `burger:v1` image in
two resource profiles: 8 small ones that ask for 100m to 250m of CPU and
32 to 64 MiB, and 2 larger ones that ask for a whole core and 128 MiB. Each
task cooks a patty: it hashes a block of memory and prints a digest, so the
run takes a few seconds, then tells the counter by its service name. The
cluster stores each profile as a task array, a template and a count rather
than a record per task, and packs the tasks onto the nodes around the apps
already running there.

`submit` prints the manifest's number; on a fresh cluster it's 1. `watch`
takes that number and prints a summary a second: how many tasks succeeded,
the rate, how long the final attempts took, and where they ran. It returns
when the run finishes. Meanwhile the burger keeps serving, and its footer
counts the patties: `patties cooked: 10`. `relish jobs` then lists the run
with its outcome and rate. The `batch` chapter covers cron, deployment hooks,
retries and choosing how each job is isolated.

## Cut a layer off

```sh
relish fault partition lettuce --from counter --duration 30s --acknowledge
curl http://burger.localhost:18080
```

The cluster now refuses every connection from a counter to the lettuce, so
there's a gap where it was:

```text
  ▟  '  .  '  .  '  .  '  .▙   top bun     v1  sesame         rb-bab43735aab6-1 #f56d    3.0 ms
   · · · · · · · · · · · ·     lettuce     missing: blocked by the cluster
  ▐  o    o    o    o    o ▌   tomato      v1  beef tomato    rb-bab43735aab6-1 #721d    3.8 ms
```

Nothing else notices: the lettuce is running and healthy, and only the
counters can't reach it. After thirty seconds the fault lifts itself and the
gap fills. Fault injection is built in, and on a laptop cluster it's switched
on.

## Roll a layer forward

```sh
relish apply burger/cheese-v2.toml
curl http://burger.localhost:18080
```

`cheese-v2.toml` is `burger.toml`'s cheese app at `VERSION = "v2"`, nothing
else changed. Cheese runs two replicas, so the burger has two slices, and the
rolling deploy replaces them one at a time: the node starts a v2 instance
beside the v1 ones, waits for it to pass its health check, then drains a v1,
and goes again. The other slice keeps serving meanwhile, so the burger never
loses its cheese. It turns from cheddar to blue cheese a slice at a time.

## Roll a bad one back

```sh
relish apply burger/patty-v3.toml
relish status
relish history patty
```

Patty `v3` is raw in the middle: it starts, but its health check always
fails. It never joins the service, so the burger keeps its beef patty
throughout. After the app's 20-second `health_timeout` the node gives up and
puts v1 back by itself, and the cluster marks the patty `RolloutFailed` so
nothing retries the same spec. `status` prints the node and the reason, and
`history` asks every node what shipped when.

## Scale one up

```sh
relish apply burger/triple-patty.toml
curl http://burger.localhost:18080
```

The patty at `v1` with three replicas. Any change to the spec clears the
`RolloutFailed` mark, so this one goes out. The counter asks each layer six
times over fresh connections, and the cluster balances connections
round-robin, so it finds all three patties and stacks one line per instance: a
triple patty.

## Kill a patty

```sh
relish fault kill patty --acknowledge
curl http://burger.localhost:18080
relish events --app patty
```

For a few seconds the burger is a double. Then the node agent has restarted
the killed patty, it passes its health check, and the burger is a triple
again, the new patty under a new instance number. The cluster won't kill an
app's last replica (`relish fault kill lettuce` is refused), which is why this
is a patty and not a single layer. `events` asks every node what happened and
merges the answers by time: the deploys, the failed v3 and its rollback, the
kill and the restart, each with its node.

## See the whole path

```sh
relish path counter --to patty
```

Every hop from a counter replica to the patties, walked in order: the DNS
answer, the virtual IP, the eBPF service map in the kernel, the firewall, any
active faults, and finally a real TCP connect from inside the counter's
network namespace. On Kubernetes you'd piece that together from `nslookup`,
`kubectl`, `bpftool`, `iptables` and a debug pod.

```sh
relish metrics counter
```

The counters' own Prometheus metrics, from all three replicas. The app says
`metrics = {}`, so each node scrapes `/metrics` on the replicas it runs.
There's no Prometheus to install.

## Slow it down

```sh
relish fault delay patty 300ms --from counter --duration 2m --acknowledge
relish path counter --to patty --count 3
```

Every packet the counters send a patty now waits 300 ms. Only the patties,
and only from the counters: the other layers don't notice. The fault lifts
itself after two minutes. The same path is now DEGRADED: `relish path` names
the fault, shows the delay it found on the counter's network interface, and
every connect takes 300 ms. In your browser, each patty line's latency is in
the hundreds of milliseconds.

```sh
relish dashboard
```

The same numbers as live charts in your browser: CPU, memory in MiB, requests
per second and mean latency in milliseconds, one line per replica. It runs over
the CLI's own authenticated connection. Ctrl-C closes it; the cluster keeps
running.

## Lose a node

```sh
relish local stop node-3
relish inspect counter
```

Now lose a whole machine. About a minute later there are three running
counters again, spread over the two nodes that are left: each keeps the one it
had, and one of them starts a replacement for node-3's, under the name node-3's
had. A layer that lived on node-3 comes back the same way, and its gap in the
burger fills. Nobody had to notice first. `relish inspect` asks every node for
its instances, lists each one, running or stopped, with the node it's on, and
names the node that didn't answer.

```sh
relish wtf
```

One screen on what's wrong and what to do about it. Here it warns that a
council member is missing and one more failure would lose quorum. Its `builds`
row checks that every node runs the same build, and warns when one doesn't.
It's what you'd run first in a real incident.

## Clean up

```sh
relish local destroy --yes
relish uninstall
```

The first removes the cluster's VMs and data; the second removes Relish, its
tools and the image cache.

## Where to next

- Write your own app config: `deploy-an-app`
- How the cluster fits together: `cluster-basics`
- More ways to break things: `chaos`
- Jobs, cron, hooks and task arrays: `batch`
