# Five-minute tour

The same tour as the homepage's "Try it in five minutes": install, run a real
Kubernetes app on a three-node laptop cluster, watch it, break it and watch it
heal. Open it any time with `relish manual tour`.

It needs macOS, or Linux with QEMU and KVM, plus about 8 GiB of free memory and
15 GiB of disk. The one-line install arrives with 0.1.0; until the signed
release is published, it stops with a release-not-published message.

## Install and build the cluster

```sh
curl -fsSL https://reliaburger.com/install.sh | sh
```

This installs `relish`, then builds a three-node cluster in Linux VMs. Every
download is checked against a pinned digest. It's the only step that takes
minutes. If `relish` isn't on your `PATH` yet, the installer says how to add it
and setup prints the full path in its next steps.

## Run a real Kubernetes app

```sh
relish apply -f https://reliaburger.com/demo/podinfo.yaml
```

podinfo's frontend, backend and Redis cache. Relish converts the Kubernetes
YAML as it applies it and prints a report of anything it had to change. The
same manifest is `examples/kubernetes/podinfo.yaml` in `relish manual examples`.

```sh
relish status
```

Three replicas, spread over three nodes. The scheduler did that, not you.

Now open <http://podinfo.localhost:18080> in a browser. The request comes in
through the built-in ingress, and the page shows which replica answered.

## Watch it

```sh
relish logs podinfo --since 1m
```

Logs from every replica on every node, in one stream. No log shipper to
install.

```sh
relish dashboard
```

Live CPU and memory charts in your browser, over the CLI's own authenticated
connection. Ctrl-C closes it; the cluster keeps running.

## Break it

```sh
relish fault kill podinfo --count 1 --acknowledge
relish status
```

Fault injection is built in, and on a laptop cluster it's switched on. Run
`status` again and the killed replica is back, because the node agent restarts
what should be running.

```sh
relish local stop NODE
relish status
```

Now lose a whole machine: use the name of the third node from `relish status`.
The replicas that lived there come back on the two survivors.

```sh
relish wtf
```

One screen on what just happened and what still needs attention. It's what
you'd run first in a real incident.

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
