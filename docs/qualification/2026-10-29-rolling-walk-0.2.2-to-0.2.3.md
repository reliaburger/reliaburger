# Rolling walk: published 0.2.2 to published 0.2.3

29 October 2026, 00:00 to 00:02 GMT. The first rolling upgrade between two
published releases
([#1444](https://github.com/reliaburger/reliaburger/issues/1444)). Every
release from 0.1.0 to 0.2.2 changed the cluster's formats; 0.2.3 is the first
that keeps its predecessor's pair (protocol 53, state format 70), so a 0.2.2
cluster can roll onto it in place.

> **The walk worked, and it found a bug.** Every node moved to 0.2.3
> (`bun 0.2.3 (d387d56)`), leader last, in 2 min 20 s, with the council at 3
> of 3, the cron job firing every minute and `relish wtf` clean afterwards.
> But the app's replicas didn't stay put. After each node's exec, the new Bun
> adopted its running replica, and about 15 s later the leader moved it to
> another node, so all three `hello` replicas were replaced. A plain Bun
> restart on the same build did the same, so it isn't the version change.
> That's [#1449](https://github.com/reliaburger/reliaburger/issues/1449),
> in 0.2.2 and 0.2.3 alike, fixed in 0.2.4
> ([#1459](https://github.com/reliaburger/reliaburger/pull/1459)). The ingress
> was unanswered for 0.7 s, once, while the leader's Bun replaced itself.

## The rig

| Item | Value |
|---|---|
| Host | macOS 26.3.1, Apple M2 Max, 32 GiB |
| Cluster | the published 0.2.2 installer, `install.sh --laptop`: three Lima VMs (Lima 2.2.1, 2 vCPU / 2 GiB, Ubuntu 24.04), ready in 236.1 s |
| From | `v0.2.2` (`43baee6`), protocol 53, state format 70 |
| To | `v0.2.3` (`d387d56`), protocol 53, state format 70, downloaded from the published release |
| Workload | `hello` (BusyBox `httpd`) at 3 replicas, one per node ([`hello3.toml`](2026-10-29-rolling-walk-0.2.2-to-0.2.3/hello3.toml)), and a minutely cron job ([`tick.toml`](2026-10-29-rolling-walk-0.2.2-to-0.2.3/tick.toml)) |
| Probe | `curl` against the ingress every 200 ms, logging each change of outcome ([`probe.sh`](2026-10-29-rolling-walk-0.2.2-to-0.2.3/probe.sh)) |

The other machines on the host were building Rust at the time. This walk is a
functional check, not a measurement.

## What we ran

```sh
RELIABURGER_HOME=~/.rbwalk sh install-v0.2.2.sh --laptop --no-modify-path
openssl genpkey -algorithm ed25519 -outform DER -out operator.key
# on each node: [upgrades] external_signing_key = "ed25519:…" in
# /etc/reliaburger/node.toml, then restart bun
relish apply hello3.toml; relish apply tick.toml
relish upgrade check          # running: v0.2.2, latest: v0.2.3, upgrade available
relish upgrade plan v0.2.3    # 2 council members one at a time, then the leader
relish upgrade start v0.2.3 --external-key operator.key
relish upgrade status         # until Completed
```

`relish upgrade check` called it an upgrade, not a fresh cluster: the first
time the tooling has said so for two published releases.

## Results

| Check | Result |
|---|---|
| Every node on 0.2.3 | yes: node 2 exec'd at 00:00:43, node 3 at 00:01:16, node 1 (the leader) at 00:01:55 |
| Upgrade run | `Completed` at 00:02:29, 2 min 20 s after `start` |
| Council | 3 of 3 voters throughout, leader unchanged |
| Cron job | fired every minute through the walk, every run succeeded |
| `relish wtf` after | 0 critical, 0 warning, 0 unknown |
| Ingress probe | 3 failures out of about 700 probes in the walk, all between 00:01:55.773 and 00:01:56.454 ([`probe.log`](2026-10-29-rolling-walk-0.2.2-to-0.2.3/probe.log)) |
| App replicas kept | **no**: every replica was replaced ([#1449](https://github.com/reliaburger/reliaburger/issues/1449)) |

### The replicas

Before the walk the three replicas ran as PIDs 2334, 2100 and 2056, one per
node. Node 2's journal shows the pattern every node followed
([journals](2026-10-29-rolling-walk-0.2.2-to-0.2.3/)):

```text
00:00:43.197 bun: upgrading to v0.2.3 (upgrade up-v0.2.3-1793232033788)
00:00:43.578 bun: adopted 1 running instance(s) from a previous process
00:00:58.015 orchestrator: retirement of hello/default failed, will retry: cannot retire artifacts for default__hello-1 yet: other nodes have not yet confirmed the endpoint's withdrawal
00:01:13.611 bun: note: instance default__hello-1 was not adopted after the upgrade — not reverting (cluster reschedules placements)
00:01:13.611 bun: upgrade to v0.2.3 verified and committed
```

After the walk, node 2 ran two replicas, node 3 one and node 1 none, all with
new PIDs. Killing node 3's Bun with `systemctl kill -s KILL
--kill-whom=main reliaburger`, on the same build and with no upgrade, moved
its replica to node 1 the same way.

The cause is on the leader. The restarted Bun reports in as soon as its
agent loop answers, while its API, DNS, ingress and registry are still
starting, so the report says the node isn't ready. The leader's next
scheduling pass treats a node that reported not ready as gone and moves its
replicas. When that Raft write was refused (`scheduler: placement pass
refused: admission revision changed`, logged 2 s and 4 s after every
restart) the replicas stayed, which is why it didn't happen every time.
0.2.4 counts a node whose subsystems are all still starting for the first
time, within the stale-node grace, as not yet heard from: it keeps its
placements and gets no new ones
([#1459](https://github.com/reliaburger/reliaburger/pull/1459)).

### The ingress gap

The laptop cluster's ingress forward goes to node 1. For 0.7 s while node 1's
Bun replaced itself with 0.2.3, nothing answered on port 80; the replicas
behind it were already elsewhere. An exec of the node that serves the ingress
can't avoid that gap, since the ingress proxy is part of Bun.

## What this record doesn't show

0.2.4 changes the cluster's formats, so the walk can't be repeated with the
#1449 fix until a later release keeps 0.2.4's pair. Until then the fix is
proven by its tests, not by a published walk. One walk on one laptop is one
sample: it says the path works, not how often it fails.

## Evidence

[`2026-10-29-rolling-walk-0.2.2-to-0.2.3/`](2026-10-29-rolling-walk-0.2.2-to-0.2.3/)
holds each node's journal from 23:56 GMT (`journalctl -o short-precise`), the
cluster's events, the probe's log and script, the two workload files and
`relish upgrade start`'s output. The operator key was a throwaway made for
this walk and isn't kept.
