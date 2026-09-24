# Diagnostics

When something is wrong, you don't want to assemble a runbook from ten
commands. Three diagnostic tools ship in the binary. Each one reports through
its exit code, so you can wire them straight into CI or an alert.

## `relish wtf` — what's broken and why

`wtf` fans authenticated requests across the expected nodes, correlates the
evidence, and prints a categorised report: CRITICAL, WARNING, UNKNOWN and OK.
It doesn't just list problems — it links a crashloop to the recent deploy and
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

- `0` — every selected check was observed and healthy.
- `1` — at least one CRITICAL finding.
- `2` — warnings or unknown evidence only, nothing critical.

## `relish path` — can A reach B?

`path` walks the network path from a source app to a destination, hop by hop.
It locates a running instance of the source and gathers real evidence at each
hop: a DNS query, the service-map and eBPF backend state (including which
backend the VIP sends connects to), the firewall verdict, the fault experiments
active on the path, and a TCP probe. Each step is labelled `observed`,
`inferred` or `unavailable`; incomplete evidence can't turn green.

```sh
relish path web --to redis                    # internal service, port derived
relish path frontend --to redis --count 10    # ten connects: success rate, timing
relish path api --namespace frontend \
  --to db --to-namespace storage --port 5432   # cross-namespace, explicit port
relish --output yaml path web --to redis      # machine-readable steps
```

Flags: `--to <dest>` (required — an app, hostname or IP), `--namespace` for the
source, `--to-namespace` for an internal destination, `--port` (internal
services derive it when omitted; external destinations require it) and
`--count N` (1-10, default 1) to repeat the TCP connect.

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

## `relish bench` — is it fast enough?

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
