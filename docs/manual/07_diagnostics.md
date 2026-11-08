# Diagnostics

When something is wrong, you don't want to assemble a runbook from ten
commands. Four diagnostic tools ship in the binary. Each one reports through
its exit code, so you can wire them straight into CI or an alert.

## `relish wtf`: what's broken and why

`wtf` fans authenticated requests across the expected nodes, correlates the
evidence, and prints a categorised report: CRITICAL, WARNING, UNKNOWN and OK.
It doesn't just list problems. It links a crashloop to the recent deploy and
the error log line behind it. A crashloop is three `restart` events for one
app inside 15 minutes; Bun records one for every restart, so `relish events`
shows the same history `wtf` read. A high lifetime `restart_count` in
`relish status` on its own isn't one.

You only need to reach one node. `wtf`, `inspect` and `path` send every
per-node request through the node you're connected to (`/v1/nodes/<node>/relay/...`), carrying
your own credential, so they work from a laptop whose VMs sit behind Lima's
private network. The relay forwards only the handful of read-only diagnostic
calls these commands make, plus the path probe itself, and each target
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

`wtf` also asks every node which `bun` it runs: the version, the commit it was
built from and the binary's SHA-256. When they all match, the OK row names the
build. When they don't, a `version-skew` warning names the odd node(s) and
lists every node's build. Nodes count as the same build when their version and
commit match, so one commit built for arm64 and x86_64 isn't skew; the SHA-256
decides only for a build that doesn't know its commit.

A node that doesn't answer (a dead one, say) leaves its build unknown, so
there's no OK row. You get one UNKNOWN `[builds]` row instead, which says what
the nodes that answered run and names the one that didn't. If the nodes that
answered disagree, the `version-skew` warning carries the silent node in its
details.

When desired apps span more than one namespace, `wtf` checks that every node
can keep them apart. A node without the live eBPF hooks (rootless runc,
ProcessGrill, eBPF off) gets a `namespace-isolation` warning naming it: there,
any namespace's workloads reach any other's, and `allow_from` is refused.

When the cluster has a GitOps configuration, `wtf` asks every node how it
sees it. A secret-looking `env` value that's plaintext in the repository is a
`gitops-plaintext-secret` warning naming the app and the key (never the
value); GitOps still applies it, so encrypt it with `relish secret encrypt`.
A node whose own `[gitops]` in `node.toml` differs from the cluster's gets a
`gitops-local-config` warning listing the settings that differ: the cluster's
configuration in Raft is the one that runs, and the node's is ignored. A node
that can't unseal the cluster's GitOps credentials (no master key) gets a
`gitops-config-unreadable` warning, since as leader it couldn't sync. With none
of these, the `gitops` OK row says so. See
[GitOps](12_operations.md#gitops).

`wtf` also asks every node for its own view of the council, because a split
brain looks healthy from either half. It reports CRITICAL when a node is
fenced out of a council `relish council recover` replaced, when nodes serve
different recovery epochs, or when more than one node is named leader. Each
finding names the nodes and the way back (`relish council re-enrol`, see
`operations`). A voter that doesn't answer is a warning while quorum holds,
naming the missing voter, and CRITICAL once quorum is gone. A voter that
answers but has applied more than 100 log entries fewer than the others is a
warning too: it counts towards quorum but isn't applying the log, which is
what a voter whose disk filled up looks like. `relish council status` shows
the same comparison as a table.

For the image registry, `wtf` warns (`registry-redundancy`) when a node's
registry isn't listening, peers can't reach it, or some layers have fewer live
copies than `[images] redundancy` asks for. Heal fixes the last on its own
within a minute or two. A *lost* image is worse: some layer it needs has no
live copy anywhere, so nothing can pull it. That's a CRITICAL
`registry-lost-images` finding listing each `repository:tag`, and the only fix
is to push the image again (or bring back the node that held it). `relish
status` prints the same list on one line. See `images-and-volumes`.

`relish inspect <app>` uses the same relay to list every instance of an app on
every node, with the node it runs on, under a `desired` vs `running` replica
count. A node that doesn't answer, or that gossip already considers dead, gets
a warning line naming it instead of silently shrinking the list. An app its
namespace quota keeps unplaced gets a `Blocked:` line with the quota it would
break (see `deploy-an-app`), and `wtf` reports it as a `quota-blocked` warning
rather than a generic under-replication.

An app the scheduler could place only in part gets an `Unplaced:` line saying
how many of its replicas found a node and why the rest didn't: no ready node,
no node with its required labels, none that can enforce its needs or run its
host binary, or no room. `relish status` prints the same line, and `wtf`
reports it as an `unschedulable` warning with that reason (see
`deploy-an-app`).

An app whose current spec failed to roll out on a node is marked
`RolloutFailed`: the leader holds its other nodes on the previous version and
won't try the failed spec again. `wtf` reports it as a `rollout-failed`
warning naming the node and the reason, and `relish status` prints the same
line. Applying a different spec (a scale change counts), or `relish rollback`,
clears it; applying the same spec again doesn't (see `deploy-an-app`).

An instance whose binary isn't on its node, or isn't executable, is `failed`
with the reason, such as `binary not found: /usr/local/bin/ghost-server`.
`relish status` prints it under the table, and `wtf` reports a critical
`launch-failed` finding naming each instance, its node and the reason, in place
of a generic under-replication (see `deploy-an-app`).

### Every finding

Each finding has a stable id. `relish agent explain <id>` prints what it
means, the numbers that decide it and the line of code that does (see
`working-with-an-ai-agent`).

| Id | Severity | Means |
|----|----------|-------|
| `unreachable-nodes` | critical | A node's Bun API didn't answer |
| `node-dead` | critical | Gossip marks a node dead or suspect |
| `version-skew` | warning | Nodes run different `bun` builds |
| `council-recovery-split` | critical | Two recoveries claimed one epoch; both councils fenced themselves |
| `council-fence-unpersisted` | warning | A recovery fence couldn't be written to disk |
| `council-fenced` | critical | Nodes belong to a council `relish council recover` replaced |
| `council-epoch-split` | critical | Nodes serve different recovery epochs |
| `council-voter-lagging` | warning | A voter answers but has applied more than 100 fewer log entries |
| `council-multiple-leaders` | critical | More than one node is named leader |
| `no-leader` | critical | The council has no leader |
| `quorum-loss` | critical | Fewer than a majority of council members answer |
| `council-member-down` | warning | A council member is missing, quorum holds |
| `active-faults` | warning | Smoker faults are active |
| `disk-high` | warning, critical | A filesystem is 85% full (critical at 95%) |
| `cert-expired` | critical | A certificate has expired |
| `cert-expiring` | warning | A certificate expires within 14 days without healthy rotation |
| `registry-lost-images` | critical | No live node holds every layer of an image |
| `registry-needs-mtls` | warning | A plaintext cluster's registry can't replicate |
| `registry-redundancy` | warning | A node's registry is down, unreachable or under-replicated |
| `token-expired` | warning | An API token has expired |
| `token-expiring` | warning | An API token expires within 14 days |
| `admin-token-old` | warning | An admin token's secret is over 90 days old |
| `namespace-isolation` | warning | A node can't keep namespaces apart |
| `gitops-plaintext-secret` | warning | A secret is plaintext in the GitOps repository |
| `gitops-local-config` | warning | A node's own `[gitops]` is ignored |
| `gitops-config-unreadable` | warning | A node can't unseal the GitOps credentials |
| `policy-broken` | warning | A policy in force can't be evaluated |
| `policy-would-deny` | warning | A committed app or job breaks a deny policy |
| `policy-audit` | warning | A committed app or job breaks an audit policy |
| `crashloop` | critical | An app restarted 3 times in 15 minutes |
| `no-backends` | critical | A service has no healthy backend |
| `launch-failed` | critical | The runtime refused to launch an app |
| `quota-blocked` | warning | A namespace quota keeps an app unplaced |
| `volume-home-away` | critical | An app waits for the node holding its volume |
| `unschedulable` | warning | Some of an app's replicas have no node |
| `under-replicated` | warning | An app runs fewer replicas than it wants |
| `rollout-failed` | warning | An app's spec failed to roll out on a node |
| `deploy-stuck` | warning | A deploy has been active for over 15 minutes |
| `alerts-firing` | warning | An alert is firing |
| `cpu-throttling` | warning | An app was CPU throttled |

## `relish timeline`: what changed, and why did it do that?

A finding tells you what's wrong now. In an incident you also want to know
what changed just before, and why the cluster did what it did. `relish
timeline` answers the first:

```sh
relish timeline --app web --since 15m
relish timeline --since 10m --until 1762560000
```

It merges three sources into one list, newest first: each node's events
(deploys, restarts, health, placement, alerts, faults, audited operations),
the audit log (applies, policy changes, refused deploys, faults, tokens,
including the ones that have left the events ring) and, with `--app`, the
app's deploy history. `--since` and `--until` take a duration or epoch
seconds, and a window covers at most a day; `--limit` (100 by default, at most
500) keeps the newest. Set `--until` to when it broke to see the minutes
before.

Above the list, each source says how much of it was seen. Events live in
memory, the newest 1,024 per node, so on a busy node the start of a long
window may be gone, and the timeline says so instead of showing a quiet hour.
A node that didn't answer is named, and a token that may not read the audit
log gets the rest of the timeline with the audit source marked unknown. `-o
json` prints the same data `relish agent timeline` returns.

### Decisions and their rules

Some entries are decisions, with a `rule` and the evidence it compared. The
scheduler, a node's restart loop and the apply path record them at the moment
they decide, because a moment later the evidence is gone: the scores, the
failed probes, the policy that said no. `relish agent explain` reads them
back:

```sh
relish agent explain --restart web-0
relish agent explain --placement web-1 --namespace payments
relish agent explain --refusal web
relish agent explain --rules
```

Each answer gives the rule, what it means, the evidence, the line of code
that decides it and what happened on the same node in the two minutes before.
A decision the records no longer hold comes back with `not_found`, saying how
far back the records reach: unknown, not "it didn't happen".

| Rule | Means |
|------|-------|
| `placement.scored` | The scheduler put a new replica on the node running fewest of the app's replicas, then the highest score; the evidence has the score's parts, how many nodes passed the filter and the runner-up |
| `placement.daemon` | A daemon set runs on every node that passes the filter |
| `placement.volume_home` | An app with managed volumes went back to the nodes that hold them |
| `placement.short` | Some replicas found no node, and why: the first constraint none met |
| `placement.quota_blocked` | The namespace quota has no room for the app |
| `restart.process_exited` | The process stopped, so the node restarted it within the restart budget, after the backoff |
| `restart.start_failed` | The runtime couldn't start the instance, so the node cleaned up and tried again |
| `restart.health_failed` | The instance failed `threshold` health probes in a row, so the node restarted it |
| `restart.budget_exhausted` | The instance used its whole restart budget, so the node gave up and marked it failed |
| `restart.sidecar_exited` | A sidecar exited and was restarted on its own |
| `deploy.refused.policy` | An enforcing policy denied the apply |
| `deploy.refused.owner` | The apply named an owner the caller can't speak for |
| `deploy.refused.council` | The council refused the write: a lease, an owner elsewhere, a quota |

The leader records one placement decision per app per change; a pass that
changes nothing records nothing. Jobs and task arrays don't record
placements: at a million runs they'd flush every other event out of the
ring. Restart and placement records live in the events ring, so they don't
survive a node restart. Refused deploys also go to the audit log, which does.

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
namespace and cleans up after itself. While the council changes leader, the
server refuses new leases with a 503 "retry shortly"; the runner asks again
for up to 30 seconds before it gives up on the case. The image-registry cases
do the same with the registry: a push the registry refuses with 503, because
it can't confirm the write with the council leader just then, is made again
every half second for up to 30 seconds. Any other refusal fails the case at
once. Every case that runs a job holds a job lease, in the `jobs` group and
the `process-workloads` group alike, because an application lease refuses
jobs. Its jobs run through the cluster's common job lifecycle like any other
job, and releasing the lease cancels their runs and forgets their
definitions. With a council the lease itself lives in Raft, so any node can
check it.

```sh
relish test                                  # every group, four at a time
relish test --filter scheduling,firewall     # just these groups
relish test --filter process_job_with_exit_zero_succeeds   # just this case
relish test --profile full-runc              # stricter acceptance profile
relish --output json test                    # report for CI
```

Group names are `scheduling`, `service-discovery`, `deployments`,
`health-checks`, `secrets-config`, `firewall`, `workload-identity`, `ingress`,
`volumes`, `process-workloads`, `jobs`, `image-registry` and
`cluster-coordination`. A `--filter` entry can also be an exact case name,
as the report prints it, to run one case on its own; groups and case names
mix. The default `development` profile accepts a skip with
a known reason, such as a capability this node lacks. A node with
`[process_workloads] mount_isolation` on, the Linux default, doesn't advertise
host workloads, because it refuses them until #714 lands; its host
`process-workloads` and `jobs` cases skip with that reason rather than
waiting for work that never starts. The workload cases launch
`bun testapp` as an `exec` app of `/usr/local/bin/bun`, so on the process
runtime the node's `allowed_binaries` must name that path. Bun doesn't add it
for you: that would let anyone who can deploy run the node agent's own binary
as root.

```toml
[process_workloads]
allowed_binaries = ["/usr/local/bin/bun"]
mount_isolation = false
```

The full profiles
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

For job throughput, select a fixed-window scenario instead of the ordinary
suite. For example, `relish bench --scenario jobs-shared-containers` runs for
60 seconds with concurrency 27 by default. The `jobs-containers`,
`jobs-host-processes` and `jobs-vm-baseline` scenarios provide the other isolation
levels and a local Linux raw reference. See `relish manual batch` for the
resource profile, binary allowlist, matched-node requirements and cutoff/cleanup
semantics. Daily rates are explicitly extrapolated from the measured window.

## When the CLI can't reach anything

The error names the endpoint `relish` dialled and why the connection failed,
and each cause has its own fix:

| The error says | What to do |
|---|---|
| the host name doesn't resolve | check the host in `--endpoint`, `RELIABURGER_ENDPOINT` or the current context |
| connection refused | nothing listens on that port: check Bun runs on that machine and the port is right |
| TLS handshake failed | pass the cluster CA with `--ca-cert` or `RELIABURGER_CA_CERT`, and check the endpoint starts with `https://` |
| no answer before the connect timeout | the host is down, or a firewall drops packets to the port; `relish` gives up after 10 seconds |

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
