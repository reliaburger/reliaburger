# Diagnostics

When something is wrong, you don't want to assemble a runbook from ten
commands. Four diagnostic tools ship in the binary. Each one reports through
its exit code, so you can wire them straight into CI or an alert.

## `relish wtf`: what's broken and why

`wtf` fans authenticated requests across the expected nodes, correlates the
evidence, and prints a categorised report: CRITICAL, WARNING, UNKNOWN and OK.
It doesn't just list problems. It links a crashloop to the recent deploy and
the error log line behind it.

You only need to reach one node. `wtf` and `path` send every per-node request
through the node you're connected to (`/v1/nodes/<node>/relay/...`), carrying
your own credential, so they work from a laptop whose VMs sit behind Lima's
private network. The relay forwards only the handful of read-only diagnostic
calls these two commands make, plus the path probe itself, and each target
repeats every authentication and authorisation check.

```sh
relish wtf                       # diagnose the whole cluster
relish wtf --app payments        # scope to one app for a faster, deeper look
relish wtf --watch               # re-run every 30s until Ctrl-C (human output only)
relish wtf --watch --interval 5  # re-run every 5s instead
relish --output json wtf         # one exact report for machines
```

An OK row always rests on an observed, timestamped fact; a missing or
inherently incomplete source becomes UNKNOWN, never a green pass. The exit code
is the contract:

- `0`: every selected check was observed and healthy.
- `1`: at least one CRITICAL finding.
- `2`: warnings or unknown evidence only, nothing critical.

## `relish path`: can A reach B?

`path` walks the network path between two apps hop by hop. It finds a running
instance of the source app and gathers real evidence at each hop to the
destination: a DNS query, the virtual IP and the eBPF service map (including
which backend the VIP sends connects to), the firewall verdict, the fault
experiments active on the path, and a TCP connect from inside the source's
network namespace. Each step is labelled `observed`, `inferred` or
`unavailable`; incomplete evidence can't turn green. It isn't request tracing:
it follows one connection's route, not a request through your services.

Right after `relish apply`, a destination may not have a healthy backend in
the source node's view yet. `path` waits up to 30 s for one and says so on
stderr, then reports whatever it finds. A destination that has no instances
anywhere after a few seconds is treated as a wrong name and fails at once.

```sh
relish path web --to redis                     # internal service, port derived
relish path frontend --to redis --count 10     # ten connects: success rate, timing
relish path api --namespace frontend \
  --to db --to-namespace storage --port 5432   # cross-namespace, explicit port
relish --output yaml path web --to redis       # machine-readable steps
```

Flags: `--to <dest>` (required: an app, hostname or IP), `--namespace` for the
source, `--to-namespace` for an internal destination, `--port` (internal
services derive it when omitted; external destinations require it) and
`--count N` (1-10, default 1) to repeat the TCP connect. Probing an external
destination needs an Admin credential, the `probe_external_destination` grant
and a matching `host:port` entry in the server's
`[testing] external_probe_allowlist`.

The **Active faults** step lists every `relish fault` that acts on this
source's calls to this destination (one on the destination for every caller,
or one `--from` this source), with its id, parameters and time left. Where the
node can read them it adds the live `fault_connect_map` entry and the netem
delay on the source's interface. A partition, `dns nxdomain` or 100% drop
fails the path and names the fault; a delay or a partial drop makes it
`DEGRADED`.

The **TCP probe** times each connect inside the source container, not the
time to start the probe. It uses `date +%s%N` where the image's `date` has
nanoseconds and `/proc/uptime` otherwise (BusyBox images such as podinfo's),
in which case the output says `10 ms clock`. With `--count`, it reports how
many connects succeeded (`7/10 connects ... succeeded`) and the minimum and
median connect time; some succeeding is `DEGRADED`, none is `FAIL`.

Under `relish fault delay redis 300ms --from frontend` on the podinfo demo:

```text
  4. Active faults [DEGRADED; observed]
     fault 2: delay 300ms from frontend (54s left)
     live netem on the source's eth0: delay 300ms
     reason: fault 2 (delay 300ms from frontend) is active on this path
  5. TCP probe [PASS; observed]
     5/5 connects to 127.128.202.174:6379 succeeded (connect time min 300 ms, median 300 ms, 10 ms clock)
Overall: DEGRADED (5/5 connects, median connect 300.0 ms)
  because fault 2 (delay 300ms from frontend) is active on this path
```

The exit code mirrors `wtf`: `0` every step passes, `1` a step failed, `2` the
evidence was incomplete (`UNKNOWN`) or the path is `DEGRADED`. `FAIL` wins the
overall verdict, then `DEGRADED`, then `UNKNOWN`.

## `relish test`: does the cluster do what it says?

`test` runs a catalogue of live checks against the cluster: scheduling,
service discovery, deployments, health checks, secrets and config, firewall,
workload identity, ingress, volumes, process workloads, jobs, the image
registry and cluster coordination. Each case runs in its own leased `rbtest-*`
namespace and cleans up after itself.

```sh
relish test                                  # every group, four at a time
relish test --filter scheduling,firewall     # just these groups
relish test --profile full-runc              # stricter acceptance profile
relish --output json test                    # report for CI
```

Group names are `scheduling`, `service-discovery`, `deployments`,
`health-checks`, `secrets-config`, `firewall`, `workload-identity`, `ingress`,
`volumes`, `process-workloads`, `jobs`, `image-registry` and
`cluster-coordination`. The default `development` profile accepts a skip with
a known reason, such as a capability this node lacks. The full profiles
(`full-runc`, `process-grill`) fail on required skips, unknown evidence,
timeouts and unconfirmed cleanup. The server must grant
`provision_isolated_workloads`. Any failure exits non-zero. `--chaos` runs the
destructive recovery catalogue instead; see `chaos`.

From a laptop, `relish test` works against a `relish local` cluster with or
without `--endpoint`. It reaches the other nodes through the node it's
connected to when it can't reach them directly, and it uses the registry and
ingress forwards from the local context whenever the connection trusts that
cluster's CA (`--ca-cert` pointing at the same certificate). `--chaos` works
the same way: node faults go through the connected node, which routes them to
their target and back.

## `relish bench`: is it fast enough?

`bench` deploys leased benchmark workloads, measures the real data plane
(scheduler throughput, service-discovery latency, network throughput, deploy
speed, image distribution), confirms teardown, and prints a schema-versioned
report.

```sh
relish bench --quick                     # abbreviated suite for CI
relish bench --compare baseline.json     # flag regressions vs a saved report
relish --output json bench --quick       # machine-readable report
relish bench --disruptive --yes          # include leader-failure reconstruction
relish bench --capacity --yes            # saturate the cluster with leased apps
```

`--compare` flags a direction-aware regression only when it exceeds 10%, and it
refuses to compare unlike topology, runtime or workload parameters rather than
inventing a number. The two risky suites gate behind an explicit `--yes`:
`--disruptive` kills the observed leader, and `--capacity` fills the cluster.
Capacity needs the live council scheduler. It counts workloads only after they
run, stops on a typed placement refusal, and rechecks all counted workloads.
An API failure, incomplete evidence or deadline expiry is a failed measurement.
Once a suite has started, a timeout, API error or uncertain cleanup fails the
run (non-zero exit) instead of becoming a green skip.

## When the CLI can't reach anything

- `relish` talks to `--endpoint`, then `RELIABURGER_ENDPOINT`, then the
  laptop cluster's `~/.reliaburger/context.json`, then
  `http://127.0.0.1:9117`. An explicit endpoint doesn't borrow the saved
  context's credentials: pass `--token` and `--ca-cert` too.
- Plain `http://` works only to a loopback address. Anything remote needs
  HTTPS and the cluster CA (`--ca-cert` or `RELIABURGER_CA_CERT`).
- On a laptop cluster, `relish local status` checks every VM and its API. Node
  1 carries the CLI's endpoint, so while it's stopped `relish` can't reach the
  cluster even though the other nodes keep running.
- A node's own logs are in its service manager's journal; on a laptop cluster,
  `relish local status` prints the VM names for `limactl shell`.
