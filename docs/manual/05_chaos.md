# Breaking things on purpose

Chaos tooling (Smoker) is built in, because a resilience claim you haven't
tested is a guess.

## One-shot faults

```sh
relish fault delay redis 200ms --jitter 50ms --acknowledge # reserved; refused (TC pending)
relish fault drop api 10% --acknowledge                    # failed connections
relish fault dns redis nxdomain --acknowledge              # DNS misery
relish fault partition web --from payment --acknowledge    # block traffic between apps
relish fault kill web --count 1 --acknowledge              # SIGKILL an instance
relish fault pause web --acknowledge                       # SIGSTOP (freeze)
relish fault cpu web 50% --acknowledge                     # burn CPU in the cgroup
relish fault memory web 90% --acknowledge                  # push toward the limit
relish fault node-drain node-03 --acknowledge  # stop new scheduling
relish fault node-kill node-03 --acknowledge   # bounded cluster-plane failure
```

Every fault has a duration (default 10 minutes) and cleans up after itself:

```sh
relish fault list
relish fault clear          # or: relish fault clear <id>
```

`drop` and `partition` need the Linux eBPF connect hook. `delay` and
`bandwidth` are accepted by the parser as forward-compatible contracts, but Bun
refuses to activate them until a TC packet program can provide real delay and
pacing. A rejected command hasn't injected a fault.

Injection needs at least a Deployer credential, explicit `--acknowledge`, and
`"inject_workload_faults"` in the server's
`[testing].allowed_operations`. Admin doesn't override a disabled operation.
Clearing remains possible with the same role and grant without a destructive
acknowledgement. Bun gets audit identity from the authenticated credential,
not `$USER` or the request body.

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
don't turn into green skips. Rootless Linux and Apple Container can exercise
the workload, but the full catalogue currently needs rootful Linux cgroup v2
for node pressure. ProcessGrill remains a separate profile.

An interactive run asks you to type exactly `yes`; CI uses:

```sh
relish test --chaos --yes
```

`--yes` records consent. It doesn't grant permission and there is no
`--override`. The runner refreshes short-lived capability evidence before
each serial case, records every fault's exact id and owning node, and reverses
those exact faults after pass, failure, timeout or panic. If it can't prove
cleanup, the case is `Unknown`, not green.

## Scripted scenarios

Describe a whole experiment in TOML — steps, durations, checks — and run it:

```sh
relish fault scenario examples/phase-8/chaos-scenario.toml --dry-run
relish fault scenario examples/phase-8/chaos-scenario.toml --acknowledge
```

## Cluster-level scenarios

```sh
relish chaos council-partition --acknowledge # isolate the council
relish chaos worker-isolation --acknowledge  # cut off a worker
relish chaos status
relish chaos heal
```

Start small: one fault, one app, a hypothesis about what should happen. If
the system surprises you, that's the experiment working.
