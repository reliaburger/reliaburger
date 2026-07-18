# Breaking things on purpose

Chaos tooling (Smoker) is built in, because a resilience claim you haven't
tested is a guess.

## One-shot faults

```sh
relish fault delay redis 200ms --jitter 50ms   # latency
relish fault drop api 10%                      # failed connections
relish fault dns redis nxdomain                # DNS misery
relish fault partition web --from payment      # block traffic between apps
relish fault kill web --count 1                # SIGKILL an instance
relish fault pause web                         # SIGSTOP (freeze)
relish fault cpu web 50%                       # burn CPU in the cgroup
relish fault memory web 90%                    # push toward the limit
relish fault node-drain node-03                # graceful departure
relish fault node-kill node-03 --containers    # abrupt failure
```

Every fault has a duration (default 10 minutes) and cleans up after itself:

```sh
relish fault list
relish fault clear          # or: relish fault clear <id>
```

## Scripted scenarios

Describe a whole experiment in TOML — steps, durations, checks — and run it:

```sh
relish fault scenario examples/phase-8/chaos-scenario.toml --dry-run
relish fault scenario examples/phase-8/chaos-scenario.toml
```

## Cluster-level scenarios

```sh
relish chaos council-partition   # isolate the Raft council, watch re-election
relish chaos worker-isolation    # cut a worker off, watch rescheduling
relish chaos status
relish chaos heal
```

Start small: one fault, one app, a hypothesis about what should happen. If
the system surprises you, that's the experiment working.
