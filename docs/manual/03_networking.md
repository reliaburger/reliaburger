# Networking and ingress

## Service discovery

Every app with a port gets a stable virtual IP. Resolution happens on the
node, in the kernel where eBPF is available:

```sh
relish resolve web        # VIP and healthy backends for an app
```

With rootful runc on Linux you can enable the `.internal` DNS zone and the
eBPF data path, and containers dial `web.internal` directly:

```toml
[dns]
enabled = true            # binds the runc gateway address, port 53

[ebpf]
enabled = true            # VIP -> healthy backend in the connect hook
```

Bun refuses combinations it can't serve truthfully (rootless runc,
ProcessGrill and Apple Container don't support `.internal` yet).

## Ingress

The Wrapper proxy routes external HTTP/HTTPS to apps by host name:

```toml
[ingress]
enabled = true            # binds 80/443 on this node

[app.web]
image = "nginx:alpine"
port = 80

[app.web.ingress]
host = "web.example.com"
```

```sh
relish routes             # the live routing table
```

TLS uses the configured certificate, or a self-signed one at startup if you
haven't provided any. WebSockets and streaming responses pass through;
draining connections finish before an instance stops.

## Fault injection lives nearby

Once traffic flows through Reliaburger, you can bend it on purpose — add
latency, drop connections, return NXDOMAIN. See the `chaos` chapter.
