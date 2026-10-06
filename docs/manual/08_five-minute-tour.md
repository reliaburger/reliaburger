# Five-minute tour

The same tour as the homepage's "Try it in five minutes": install, run a real
Kubernetes app on a three-node laptop cluster, build and run an app of your
own next to it, walk its network path, measure it, break it and watch it
heal. Open it any time with `relish manual tour`.

It needs macOS, or Linux with QEMU and KVM, plus about 8 GiB of free memory and
15 GiB of disk. The one-line install fetches the signed 0.1.5 release.

## Install and build the cluster

```sh
curl -fsSL https://reliaburger.com/install.sh | sh
```

This installs `relish`, then builds a three-node cluster in Linux VMs. Every
download is checked against a pinned digest. It's the only step that takes
minutes: about a minute and a half once the downloads are cached, closer to
four the first time. If `relish` isn't on your `PATH` yet, the installer says how to
add it and setup prints the full path in its next steps.

## Run a real Kubernetes app

```sh
relish apply -f https://reliaburger.com/demo/podinfo.yaml
```

podinfo's frontend, backend and Redis cache, plus a small load generator that
calls the frontend a few times a second, so there's always traffic to look at.
Relish converts the Kubernetes YAML as it applies it and tells you what it had
to change. The same manifest is `examples/kubernetes/podinfo.yaml` in
`relish manual examples`.

```sh
relish status
relish council status
```

Give it half a minute for the images to arrive. Three frontend replicas, one on
each node, named `default__frontend-0`, `-1` and `-2`. The scheduler spread
them and numbered them across the cluster, not you. `status` opens with a line
on the council, the Raft group that holds the cluster's state, and `council
status` asks every node for its own view of it: its role, term and log
position, with the leader and quorum above the table.

Now open <http://podinfo.localhost:18080> in a browser. The request comes in
through the built-in ingress; reload and the hostname changes as each replica
answers.

## Build your own

```sh
curl -fsSL https://reliaburger.com/demo/burger.tar.gz | tar xz
relish build burger/burger.toml
```

That's a small Go web service, its test and a Dockerfile, in `./burger`. It's
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
platforms: an x86 node and an Arm one each pull their own. podinfo's images
are there too, cached on the way through.

```sh
relish apply burger/burger.toml
curl http://burger.localhost:18080/order
```

Two replicas of `burger:v1`. That's a bare name: nodes pull it from Pickle,
and because it's signed it would deploy even on a cluster that requires
signatures. Each order names the podinfo backend that "cooked" it: the burger
service calls `http://backend:9898/` by service name, exactly as it would in a
Kubernetes pod. Run the `curl` a few times and the cashier changes as the
ingress takes turns between the two replicas.

## See the whole path

```sh
relish path frontend --to redis
```

Every hop from a frontend replica to Redis, walked in order: the DNS answer,
the virtual IP, the eBPF service map in the kernel, the firewall, any active
faults, and finally a real TCP connect from inside the frontend's network
namespace. On Kubernetes
you'd piece that together from `nslookup`, `kubectl`, `bpftool`, `iptables` and
a debug pod.

```sh
relish metrics frontend
```

podinfo's own Prometheus metrics, from all three replicas. The manifest carries
the usual `prometheus.io/scrape` and `prometheus.io/port` annotations; each node
read them and scrapes the replicas it runs. There's no Prometheus to install.

## Slow it down

```sh
relish fault delay redis 300ms --from frontend --duration 2m --acknowledge
```

Every packet the frontends send to Redis now waits 300 ms. Only the frontends:
anything else calling Redis doesn't notice. The fault lifts itself after two
minutes. Fault injection is built in, and on a laptop cluster it's switched on.

```sh
relish path frontend --to redis --count 3
```

The same path, now DEGRADED. `relish path` names the fault, shows the delay it
found on the frontend's network interface, and every connect takes 300 ms.

```sh
relish metrics frontend --name http_request_duration_seconds
```

Give it twenty seconds to scrape, then look at latency per replica. It was
under a millisecond; now it's in the hundreds, and a cache read through the
ingress takes almost a second.

```sh
relish dashboard
```

The same numbers as live charts in your browser: CPU, memory in MiB, requests
per second and mean latency in milliseconds, one line per replica. It runs over
the CLI's own authenticated connection. Ctrl-C closes it; the cluster keeps
running.

## Break it

```sh
relish fault kill frontend --count 1 --acknowledge
relish status
```

Kill a frontend. Run `status` again a few seconds later and it's back, with a
restart counted: the node agent restarts what should be running.

```sh
relish local stop node-3
relish inspect frontend
```

Now lose a whole machine. About a minute later there are three running
frontends again, spread over the two nodes that are left: each keeps the one it
had, and one of them starts a replacement for node-3's, under the name node-3's
had. Nobody had to notice first.
`relish inspect` asks every node for its instances, lists each one, running or
stopped, with the node it's on, and names the node that didn't answer.

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
