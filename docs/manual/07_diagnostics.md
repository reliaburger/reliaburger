# Diagnostics

When something is wrong, you don't want to assemble a runbook from ten
commands. Three diagnostic tools ship in the binary. Each one reports through
its exit code, so you can wire them straight into CI or an alert.

## `relish wtf` — what's broken and why

`wtf` fans authenticated requests across the expected nodes, correlates the
evidence, and prints a categorised report: CRITICAL, WARNING, UNKNOWN and OK.
It doesn't just list problems — it links a crashloop to the recent deploy and
the error log line behind it.

```sh
relish wtf                       # diagnose the whole cluster
relish wtf --app payments        # scope to one app for a faster, deeper look
relish wtf --watch               # re-run every 30s until Ctrl-C (human output only)
relish --output json wtf         # one exact report for machines
```

An OK row always rests on an observed, timestamped fact; a missing or
inherently incomplete source becomes UNKNOWN, never a green pass. The exit code
is the contract:

- `0` — every selected check was observed and healthy.
- `1` — at least one CRITICAL finding.
- `2` — warnings or unknown evidence only, nothing critical.

## `relish trace` — can A reach B?

`trace` locates a running instance of the source app and gathers real evidence
along the path to a destination: a DNS query, the service-map and eBPF backend
state, the firewall verdict, and a TCP probe. Each step is labelled `observed`,
`inferred` or `unavailable`; incomplete evidence can't turn green.

```sh
relish trace web --to redis                    # internal service, port derived
relish trace api --namespace frontend \
  --to db --to-namespace storage --port 5432   # cross-namespace, explicit port
relish --output yaml trace web --to redis      # machine-readable steps
```

Flags: `--to <dest>` (required — an app, hostname or IP), `--namespace` for the
source, `--to-namespace` for an internal destination, and `--port` (internal
services derive it when omitted; external destinations require it). The exit
code mirrors `wtf`: `0` all steps pass, `1` a step failed, `2` the evidence was
incomplete (`Unknown`). A `Fail` wins the overall verdict.

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
Once a suite has started, a timeout, API error or uncertain cleanup fails the
run (non-zero exit) instead of becoming a green skip.
