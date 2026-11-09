<p align="center">
  <img src="assets/images/reliaburger_logo_v1.jpg" alt="Reliaburger" width="400">
</p>

# Reliaburger

<!-- hero:start -->
**The container platform, served ready. Built for humans and their agents.**

Kubernetes hands you the ingredients. Reliaburger hands you the burger:
app scheduling, batch jobs, metrics, logs, service discovery, ingress, mTLS,
a Docker registry, GitOps and chaos testing, in one Rust agent and one CLI.
And that CLI speaks to your AI agent too.
<!-- hero:end -->

**Status:** 0.2.7 was released on 7 November 2026. Until 1.0.0, Reliaburger is good enough for home labs and dev clusters, not for production.

## Five minutes, zero to cluster

You'll need a Mac with Apple silicon, or Linux with QEMU and KVM, plus about
8 GiB of free memory and 15 GiB of disk. The one-line installer fetches signed
binaries, which you can check with `gh attestation verify`
([how](docs/releasing.md#build-provenance-attestations)). `--laptop` asks for
a development cluster on this computer. Without a flag the installer only
explains the choices, including clusters on Linux machines you already have
(`--install-only`, then the [Linux servers guide](docs/linux-servers.md)).
With Homebrew, from 0.2.7: `brew install reliaburger/tap/relish`, then
`relish setup --quickstart` (or `relish setup --ssh user@host,...`).

```sh
# Install relish and build a three-node cluster in Linux VMs on this laptop
curl -fsSL https://reliaburger.com/install.sh | sh -s -- --laptop

# Build a burger on the cluster (Buildah on a node, into the built-in
# registry, signed): one Go program that plays every layer
curl -fsSL https://reliaburger.com/demo/burger.tar.gz | tar xz
relish build burger/burger.toml
relish images                        # burger:v1, built for amd64 and arm64
relish apply burger/burger.toml      # six layers, a counter and a diner
relish status                        # counter-0, -1 and -2, one on each node
relish council status                # every node's view of the council
curl http://burger.localhost:18080   # the burger, layer by layer; open it in a browser too

# Ten jobs from the same image cook patties beside the running apps
relish batch submit burger/jobs.toml
relish batch watch 1                 # the number submit printed
relish jobs                          # the finished run and its rate

# Cut a layer off, roll one forward, roll a bad one back, scale one up, kill one
relish fault partition lettuce --from counter --duration 30s --acknowledge  # a gap
relish apply burger/cheese-v2.toml        # a rolling deploy to blue cheese
relish apply burger/patty-v3.toml         # fails its health check, rolled back
relish history patty
relish apply burger/triple-patty.toml     # a triple patty
relish fault kill patty --acknowledge     # a double, then a triple again
relish events --app patty                 # what happened, from every node

# See every hop from counter to patty: DNS, VIP, eBPF map, firewall, TCP
relish path counter --to patty
relish metrics counter               # its own Prometheus metrics, scraped for you

# Make the patties slow, but only for the counters, for two minutes
relish fault delay patty 300ms --from counter --duration 2m --acknowledge
relish path counter --to patty --count 3
relish dashboard                     # live charts in the browser

# Lose a whole machine and watch the cluster recover
relish local stop node-3
relish inspect counter               # three counters again, over the survivors
relish wtf                           # what's wrong and what to do about it

# Clean up
relish local destroy --yes
relish uninstall
```

The [five-minute tour](docs/manual/08_five-minute-tour.md) explains each step,
and the [laptop quickstart](docs/quickstart.md) covers prerequisites, retries
and single-node setups.

Would you rather your agent did it? Tell it "Set up a Reliaburger cluster for
me. Start at https://reliaburger.com/agents." The playbook there
([working with an AI agent](docs/manual/16_working-with-an-ai-agent.md#setting-up-a-cluster-for-your-human))
asks where the cluster should run and creates nothing that costs money without
your yes. To give an agent you already use the read-only MCP server,
`relish mcp`, click
[Add to Cursor](https://cursor.com/en/install-mcp?name=reliaburger&config=eyJhcmdzIjpbIm1jcCJdLCJjb21tYW5kIjoicmVsaXNoIn0%3D) or
[Add to VS Code](https://insiders.vscode.dev/redirect?url=vscode%3Amcp%2Finstall%3F%257B%2522args%2522%253A%255B%2522mcp%2522%255D%252C%2522command%2522%253A%2522relish%2522%252C%2522name%2522%253A%2522reliaburger%2522%257D)
(Goose's button is on [the agents page](https://reliaburger.com/agents#add-to-your-agent)),
or run `claude mcp add reliaburger -- relish mcp` for Claude Code.
`relish agent setup` wires up every agent on this machine, and shows the diff
before it writes anything. From an admin context, `relish mcp` makes and sends
a read-only token of its own, so an agent never holds admin credentials.

## What's in it

- **Kubernetes YAML:** `relish apply` takes most manifests and tells you what it changed or dropped.
- **Clustering without etcd:** SWIM gossip finds nodes, and a Raft council inside the agent holds the state.
- **Service discovery in the kernel:** a virtual IP and `.internal` name per app, routed by an eBPF connect hook.
- **Ingress with TLS:** host-based routing, WebSockets, streaming and connection draining.
- **Security on by default:** mTLS between nodes, join tokens, encrypted secrets, scoped API tokens and signed images.
- **Tenants kept apart:** namespaces can't reach each other unless you let them, and egress has allowlists.
- **An audit log:** who applied, deleted, exec'd, upgraded or broke what, and from where.
- **A registry on every node:** Pickle stores, caches and builds OCI images inside the cluster.
- **Metrics, logs and alerts:** scraped, captured and exported by the agent, with nothing else to install. Alerts are five built-in node rules for now ([#1773](https://github.com/reliaburger/reliaburger/issues/1773)).
- **Batch jobs:** one-off jobs, cron, deploy hooks and task arrays of millions, beside your apps.
- **Deploys and GitOps:** health-gated rolling deploys with rollback, blue-green, autoscaling and Git sync.
- **Policies and templates:** CEL rules that refuse, warn or fill in defaults, and typed templates instead of CRDs.
- **Your own controllers:** an ordinary app with a scoped token, no operator SDK.
- **Chaos as a command:** `relish fault` injects latency, partitions, kills and pressure, and cleans up after itself.
- **Self-upgrade:** `relish upgrade` rolls a signed `bun` across the cluster without restarting workloads, and `relish verify` (0.2.8, not in 0.2.7) checks every node runs the published release, byte for byte.
- **Diagnostics for incidents:** `relish wtf` and `relish path` say what's wrong and where.
- **Built for agents:** `relish agent` answers in bounded JSON, and `relish mcp` serves it over MCP.
- **A terminal UI and a web dashboard:** run `relish` with no arguments, or `relish dashboard`.
- **The manual and the source in the binary:** `relish manual` and `relish source` work offline.

[Everything 0.2.7 does](docs/README.md#whats-in-the-binary), what you
don't have to install, and its [limits](docs/README.md#scope-and-limits) are
in the documentation. The
[feature status page](https://reliaburger.com/features.html) rates every
feature Ready, Beta, Alpha or Planned.

## Where to go next

- [reliaburger.com](https://reliaburger.com): the tour, recorded, plus job throughput and how a release is tested.
- [The user manual](docs/manual/00_getting-started.md), also built into `relish manual`. Its [reference](docs/manual/13_reference.md#every-relish-command) lists every `relish` command.
- [The documentation](docs/README.md): features, limits, and building and running from source.
- [*Building Reliaburger*](docs/book/00-preface.md), the book of how we built each subsystem, teaching Rust and distributed systems to programmers who know C, Python or Go.
- [The whitepaper](docs/whitepaper.md) for the architecture and its trade-offs, and the [design docs](docs/design/) for each subsystem.
- [The comparisons](https://reliaburger.com/compare/) with Kubernetes, Nomad, k3s, Docker Compose, Coolify and Uncloud.
- [The roadmap](docs/roadmap.md) for what's next and the known gaps.

## Contributing

Bug reports and pull requests are welcome. Read the
[contributing guide](CONTRIBUTING.md) first: it covers
[building from source](CONTRIBUTING.md#development-setup), and changes come
with tests and a matching update to the book. Report a bug through an
[issue form](https://github.com/reliaburger/reliaburger/issues/new/choose). You're
welcome to use AI tools if you say so and a human answers for every line;
agents start with [AGENTS.md](AGENTS.md). Vulnerabilities go privately, as
[SECURITY.md](SECURITY.md) describes, and there's no bounty.
[`.devcontainer/`](.devcontainer) gives you CI's toolchain in one step for
`make ci` (the portable suite only), and issues labelled `missed-mutant` are
self-checking first tasks.

## Licence

[Apache 2.0](LICENSE)
