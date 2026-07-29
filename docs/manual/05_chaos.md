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
