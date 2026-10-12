# Networking and ingress

## Service discovery

Every app with a `port` gets a stable virtual IP (VIP). The eBPF connect hook
in each node's kernel turns a connection to that VIP into a connection to a
healthy backend, wherever it runs:

```sh
relish resolve web        # VIP and healthy backends for an app
```

With rootful runc on Linux you can also enable the `.internal` DNS zone, and
containers dial `web.internal` directly. DNS needs the eBPF data path, since
it answers with the VIP:

```toml
[dns]
enabled = true            # binds the runc gateway address, port 53
upstream = "8.8.8.8:53"   # default, for everything outside .internal

[ebpf]
enabled = true            # VIP -> healthy backend in the connect hook
```

A short name like `redis.internal` resolves in the caller's own namespace;
`redis.shop.internal` names another. Containers also get a Kubernetes-style
search list, so plain `redis:6379` and `redis.shop:6379` work as they would in
a pod. Bun refuses combinations it can't serve truthfully: rootless runc and
ProcessGrill don't get `.internal`.

## Who may connect

Apps in the same namespace can reach each other; other namespaces can't. That
holds for every address a workload has, not just its VIP: a backend's
container address (any port), and a published host port on any node, which is
what `relish resolve` prints. Open a specific cross-namespace path with
`allow_from`, naming apps as `namespace/app`:

```toml
[app.db]
namespace = "storage"
image = "postgres:17"
port = 5432

[app.db.firewall]
allow_from = ["shop/api"]
```

`allow_from` only opens cross-namespace paths; it doesn't restrict callers in
the same namespace. One grant covers the VIP and the real addresses alike.

The eBPF hooks enforce all of this, so isolation needs `[ebpf]`. `relish init`
turns `[ebpf]` and `[dns]` on when it runs as root on Linux with `runc`
installed, and Bun won't start a rootful runc node whose eBPF data path fails
to load. A node that can't enforce isolation (rootless runc, ProcessGrill,
macOS) refuses `allow_from`, and `relish wtf` warns when a cluster with more
than one namespace has such a node.

What isolation covers:

- containers on rootful runc, and their delegated job tasks;
- `exec` and `script` workloads on the owned process runtime as root, which
  start inside their own workload cgroup. They run as root, so a hostile one
  can leave its cgroup: isolation keeps honest host commands honest, it
  doesn't contain hostile ones;
- traffic between containers on one node that never meets a socket hook (raw
  sockets, a host port reached through DNAT), checked again on the forward
  path by the `reliaburger_isolation` nftables table.

What it doesn't cover: processes outside Reliaburger's cgroups, such as host
daemons, SSH sessions or Bun itself. They can reach any workload, as root on
the node always could.

Egress is open unless an app lists what it may reach. Every entry needs a port:

```toml
[app.api.egress]
allow = ["api.stripe.com:443", "10.0.0.0/8:5432", "[2001:db8::1]:443"]
```

An allowlist always lets the workload reach the node's DNS responder on port
53, over UDP and TCP, so it can resolve the names on its list (and
`.internal` names). Without `[dns]` there's no responder to allow: list your
resolver yourself. An egress allowlist only works on rootful runc with eBPF,
and Bun refuses to deploy an app with one anywhere else rather than run it
unguarded. `allow_franchise` (cross-cluster egress) parses but is refused.
Jobs take the same `[job.NAME.egress]` and `[job.NAME.firewall]` tables (see
[Batch jobs](14_batch-jobs.md)).

## Ingress

The Wrapper proxy routes external HTTP and HTTPS to apps by host name and path:

```toml
[ingress]
enabled = true            # binds 80 and 443 on this node

[app.web]
image = "nginx:alpine"
port = 80

[app.web.ingress]
host = "web.example.com"
path = "/"                # default
tls = "cluster"           # or "explicit"; omit for plain HTTP
websocket = true          # allow WebSocket upgrades (default false)
rate_limit_rps = 100      # optional, per client IP
rate_limit_burst = 200
```

```sh
relish routes             # the live routing table
```

Omitting `tls` makes the route deliberately plain HTTP. With `tls` set, plain
HTTP redirects to HTTPS. `cluster` serves certificates signed by the cluster's
own ingress CA, which renew themselves; your clients need to trust that CA.
`explicit` uses the `tls_cert` and `tls_key` PEM files from this node's
`[ingress]` section, reloaded when they change; Bun rejects a half-configured
pair. There's no ACME: `auto` and `acme` fail route validation. The listener's
self-signed fallback is for development, not a substitute for either mode.

Streaming responses pass through, and so do WebSockets on routes that allow
them. A stopping instance drains its connections before it goes.

## Fault injection lives nearby

Once traffic flows through Reliaburger you can bend it on purpose: add latency,
drop connections, return NXDOMAIN. See the `chaos` chapter.
