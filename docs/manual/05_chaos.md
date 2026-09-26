# Breaking things on purpose

Chaos tooling (Smoker) is built in, because a resilience claim you haven't
tested is a guess.

## One-shot faults

```sh
relish fault delay redis 200ms --jitter 50ms --acknowledge # slow every caller down
relish fault drop api 10% --acknowledge                    # failed connections
relish fault dns redis nxdomain --acknowledge              # DNS misery
relish fault partition web --from payment --acknowledge    # block traffic between apps
relish fault kill web --count 1 --acknowledge              # SIGKILL an instance
relish fault pause web --instance ID --acknowledge         # SIGSTOP (freeze) one instance
relish fault cpu web 50% --acknowledge                     # burn CPU in the cgroup
relish fault memory web 90% --acknowledge                  # push toward the limit
relish fault disk-io web 10mbps --write-only --acknowledge # throttle disk I/O
relish fault resume web --acknowledge                      # SIGCONT a paused app
relish fault node-drain node-03 --acknowledge  # stop new scheduling
relish fault node-kill node-03 --acknowledge   # bounded cluster-plane failure
relish fault node-pressure node-03 --cpu 80% --memory 90% --acknowledge
```

Workload faults take `--namespace` (default `default`), `--instance` to hit
one instance, `--duration` and `--reason`, which is recorded with the fault.

Every fault has a duration (default 10 minutes) and cleans up after itself:

```sh
relish fault list
relish fault clear          # or: relish fault clear <id>
```

`drop` and `partition` need the Linux eBPF connect hook. `delay` needs runc
containers: it adds a `tc` netem qdisc to each caller container's own `eth0`,
matching only packets to the target's backends, so it slows open connections
as well as new ones (add `--from APP` to slow just one caller). `bandwidth` is
accepted by the parser as a forward-compatible contract, but Bun refuses it for
now. A rejected command hasn't injected a fault.

Injection needs explicit `--acknowledge`, a credential with the right role
and the operation in the server's `[testing].allowed_operations`:

| Faults | Role | Grant |
|--------|------|-------|
| workload faults (`delay` to `resume`) | Deployer | `inject_workload_faults` |
| `node-drain`, `node-kill` | Admin | `alter_node_state` |
| `node-pressure` | Admin | `saturate_capacity` |

Admin doesn't override a missing grant. `node-pressure` also needs
`max_node_pressure_cpu_percent` or `max_node_pressure_memory_percent` above
zero (both default to 0, and memory stops at 90%). Clearing needs the same role
and grant, but no acknowledgement. Bun takes the audit identity from the
authenticated credential, not `$USER` or the request body.

`[testing] safety_class` is `development`, `staging`, `production` or, when
unset, `unknown`. On `production` and `unknown` clusters every injection also
needs `allow_protected_mutation = true`, a second switch you flip on purpose.
A server install writes no `[testing]` section, so every fault is refused until
an operator opts in. A laptop cluster from
`relish setup --quickstart` is the exception: it's a throwaway development
cluster, so each node's config says

```toml
[testing]
safety_class = "development"
allowed_operations = ["inject_workload_faults", "alter_node_state"]
```

That admits workload faults and `node-kill`/`node-drain`, but not
`node-pressure` (which could starve a small VM's own control plane).
`relish local status` prints the policy the first node actually serves.

You don't need to know where a replica runs. The node you talk to looks up
which nodes run the target, checks the replica rail against every replica in
the cluster (so `kill web --count 3` on a three-replica app is refused even
though each node holds one, and a pause without `--instance` counts as
freezing every replica, so it's always refused), and forwards each owner its
share under your own credential, so the owner repeats every check. Add `--node NAME` to pick the
node. A fault spread over several nodes becomes one fault per node, and the
command prints each one. `relish fault list` shows every node's faults with a
`NODE` column, and `relish fault clear <id>` finds the node that holds that
id (pass `--node` if two nodes happen to use the same number). `relish fault
clear` with no id, or with a service name, clears on every node; clearing
everything needs an unscoped token.

Network faults (`delay`, `drop`, `dns`, `partition`) are the other way round.
They change what happens when something *calls* the target, and that happens
on the caller's node: the eBPF connect hook, the delay's netem qdisc and the
DNS responder all act there. So
`relish fault drop redis 20%` lands on every node, because any of them may run
something that calls redis, and `relish fault partition redis --from frontend`
lands only on the nodes that run `frontend` in redis's namespace. A frontend
replica that starts, restarts or moves while the fault is active picks it up
within a second, and when the fault expires or is cleared every node removes
it. `relish fault clear redis` clears all of them at once.

A drop or partition also cuts the connections the callers already hold open
to the target's backends (Bun runs `ss -K` in each affected container's
network namespace). Without that, a client with a connection pool, like
podinfo's redis pool, would keep using its old connections and never notice.
With it, the pool reconnects straight into the fault. In the podinfo demo the
frontend's log says `cache set failed ... connect: operation not permitted`
on the very next call. A `dns` fault doesn't cut anything: open connections
were resolved before the fault, and only new lookups fail. Cutting needs a
kernel built with `CONFIG_INET_DIAG_DESTROY` (stock Ubuntu has it), and only
applies to containers with their own network namespace (runc), not to process
workloads, whose sockets share the host's.

A delay needs no cutting: it holds back packets, not connections. In the
podinfo demo, `relish fault delay redis 300ms --from frontend --acknowledge`
took a cache read (a couple of redis commands over the pool's open
connections) from about 43 ms to about 945 ms, and `relish fault clear redis`
brought it straight back.

## Recovery catalogue

`relish test --chaos` runs five destructive recovery checks, one at a time:

1. fail the council leader, elect another leader, run a canary, then recover;
2. fail a worker with three live replicas, restore all three on survivors,
   then admit the worker again;
3. isolate a minority of the council, prove the majority still serves a
   canary, then heal the exact partition;
4. apply bounded whole-node CPU and memory pressure while the API and
   membership remain observable, then clear it; and
5. fail a node during an observed rolling deploy and require a terminal,
   non-`Unknown` deployment result plus restored replicas.

The full catalogue needs at least three nodes, a digest-pinned BusyBox
container workload, fresh node-kill and node-pressure evidence, and server
grants for `provision_isolated_workloads`, `alter_node_state` and
`saturate_capacity`. Missing destructive prerequisites refuse the suite. They
don't turn into green skips. Node pressure needs rootful Linux runc with
cgroup v2.

An interactive run asks you to type exactly `yes`; CI uses `--yes`. Pass
`--filter` with a scenario name to run just that one:

```sh
relish test --chaos --yes
relish test --chaos --yes --filter minority_partition_degrades_and_heals
```

`--yes` records consent. It doesn't grant permission and there is no
`--override`. The runner refreshes short-lived capability evidence before
each serial case, records every fault's exact id and owning node, and reverses
those exact faults after pass, failure, timeout or panic. If it can't prove
cleanup, the case is `Unknown`, not green.

## Scripted scenarios

A scenario file lists faults with their targets, values, start offsets and
durations, and Relish injects each one when its time comes:

```toml
name = "Payment cascade failure"

[[step]]
description = "Database latency spike"
fault = "delay"
target = "pg"
value = "500ms"
duration = "2m"

[[step]]
description = "Database starts dropping connections"
fault = "drop"
target = "pg"
value = "25%"
start_after = "2m"
duration = "3m"
```

```sh
relish fault scenario examples/phase-8/chaos-scenario.toml --dry-run
relish fault scenario examples/phase-8/chaos-scenario.toml --acknowledge
relish fault scenario examples/phase-8/chaos-scenario.toml --speed 2.0 --acknowledge
```

Start small: one fault, one app, a hypothesis about what should happen. If
the system surprises you, that's the experiment working.
