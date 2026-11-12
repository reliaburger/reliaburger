# Release log

One paragraph per release: when it shipped, how long qualifying it took, and
what the soaks found on the way. It's the track record behind the
[release gate](releasing.md#staging-a-candidate); the evidence is in the
[qualification records](qualification/) each paragraph links, and what each
release contains is in the [roadmap](roadmap.md#releases-after-010). A new
paragraph goes at the top when a release is promoted
([release gate, step 9](releasing.md#staging-a-candidate)).

## 0.2.8: agents you can trust, built faster

Promoted on 12 November 2026 as tag `v0.2.8` at `e423d469`
([promote run](https://github.com/reliaburger/reliaburger/actions/runs/43152695366)).
An agent no longer changes the cluster itself: it proposes a change, with
the before and after state, a blast radius the cluster computes and a
one-command undo, and a person, or a policy a person wrote, approves it
([#1682](https://github.com/reliaburger/reliaburger/issues/1682)). Short leases on apps and namespaces keep agents and people off
each other's work ([#1686](https://github.com/reliaburger/reliaburger/issues/1686)). `relish verify` checks every node's `bun`
against the published release ([#1689](https://github.com/reliaburger/reliaburger/issues/1689)), `relish timeline` says what
changed and why ([#1685](https://github.com/reliaburger/reliaburger/issues/1685)), `relish status` fits the whole cluster on one
screen with the lists moved to `relish apps` and `relish nodes`
([#1788](https://github.com/reliaburger/reliaburger/pull/1788)), and `relish gym run` tests your own agent on your own cluster
([#1683](https://github.com/reliaburger/reliaburger/issues/1683)). Our own loop got shorter too: macOS CI builds only `relish`,
qualification runs nightly on the Studio, and a candidate build reuses
`main`'s CI ([plan](plans/archive/2026-11-07-plan-0.2.8.md)).
`relish status -o json` changed shape: it no longer lists the instances,
and a script that read it that way should use `relish apps --instances -o
json`. Qualification ran from 10 to 12 November 2026 on the Mac Studio, on
three candidates, on the full gate, because 0.2.8 changes the cluster
formats and the agent loop
([closure record](qualification/2026-11-11-v0.2.8-release-closure.md)). The first
thing it found was our own harness: `qualify-sustained.sh` still read
`relish status -o json` as a list of instances, and failed in setup with
every app running ([#1850](https://github.com/reliaburger/reliaburger/pull/1850)). Candidate 1's V02 fast tier then found a
crash loop: a deploy-kill one second into a rollout, then a power cut of the
same node, left an adoption record out of step with its runtime generation,
and Bun refused every start after that ([#1849](https://github.com/reliaburger/reliaburger/issues/1849), fixed by [#1851](https://github.com/reliaburger/reliaburger/pull/1851) and
[#1854](https://github.com/reliaburger/reliaburger/pull/1854)). Its jobs fast tier found a 1,593 ms agent turn waiting on an
`fdatasync` ([#1853](https://github.com/reliaburger/reliaburger/issues/1853), fixed by [#1854](https://github.com/reliaburger/reliaburger/pull/1854)), and its final one ingress 502
after a power cut ([#1855](https://github.com/reliaburger/reliaburger/issues/1855), fixed by [#1856](https://github.com/reliaburger/reliaburger/pull/1856))
([candidate 1](qualification/2026-11-10-v0.2.8-candidate-1/README.md)). Candidate 2's
extra V02 run, on the seed that found the crash loop, found a 1,152 ms turn
with the loop's thread in `lseek` on a busy disk ([#1857](https://github.com/reliaburger/reliaburger/issues/1857), fixed by
[#1858](https://github.com/reliaburger/reliaburger/pull/1858)) ([candidate 2](qualification/2026-11-11-v0.2.8-candidate-2/README.md)).
Candidate 3 is that fix plus `rustix` 1.1.6 for RUSTSEC-2026-0486
([#1862](https://github.com/reliaburger/reliaburger/pull/1862)), and it passed everything clean with no turn over budget: both
staged installs, in 125 s and 118 s
([first](qualification/2026-11-11-v0.2.8-staged-install-apple-silicon.md),
[second](qualification/2026-11-11-v0.2.8-staged-install-apple-silicon-2.md)), the
power-cut and reboot lanes, 25 of 25 in every lane
([record](qualification/2026-11-11-power-cut-reboot.md)), the V02 fast tier
([record](qualification/2026-11-11-v0.2.8-sustained-v02-fast.md)) and its seeded rerun
([record](qualification/2026-11-11-v0.2.8-sustained-v02-fast-seed-3139726783.md)), and
the jobs fast tier at 8,630.0 accepted jobs a second
([record](qualification/2026-11-11-v0.2.8-job-qualification-fast.md)). The 8-hour jobs
final completed its whole schedule clean
([record](qualification/2026-11-11-v0.2.8-job-qualification.md)): 9,311.2 accepted
jobs a second against the 1,390 target, about 0.2.7's rate, job-start p99
32 ms over 213.5 million starts, a council history linearizable over
213,407 operations, and Bun's worst steady-state RSS 208.41 MiB against
384 MiB. It ships with no known issue the gate found. 0.2.8 moves the
cluster formats to protocol 58 and state format 74, so a 0.2.7 cluster
starts afresh ([upgrading from 0.2.7](releasing.md#upgrading-from-027)).

## 0.2.7: built for humans and their agents

Promoted on 7 November 2026 as tag `v0.2.7` at `ace072c7`
([promote run](https://github.com/reliaburger/reliaburger/actions/runs/42392039001)).
`relish agent` answers an AI agent in one JSON envelope within a byte budget
([#1565](https://github.com/reliaburger/reliaburger/issues/1565)): `context`
and `diagnose` read the cluster, `manual` and `source` read the manual and
the exact source the binary was built from
([#1570](https://github.com/reliaburger/reliaburger/issues/1570),
[#1571](https://github.com/reliaburger/reliaburger/issues/1571)), `patch`
builds and tries a fix on a throwaway cluster
([#1574](https://github.com/reliaburger/reliaburger/issues/1574)), and
`report` drafts a redacted bug report for a human to post
([#1575](https://github.com/reliaburger/reliaburger/issues/1575)).
`relish mcp` serves the same commands as read-only MCP tools
([#1094](https://github.com/reliaburger/reliaburger/issues/1094)), `relish
agent setup` wires them into the agents on a machine
([#1581](https://github.com/reliaburger/reliaburger/issues/1581)), every
command takes `-o json`
([#1566](https://github.com/reliaburger/reliaburger/issues/1566)), and every
release file carries a build provenance attestation
([#1583](https://github.com/reliaburger/reliaburger/issues/1583))
([plan](plans/archive/2026-11-04-plan-0.2.7.md)). Its exit test, Claude Code
with a read-only token in the agent gym, ran three times on 6 November and
passed: the right diagnosis in 5 of 5 runs of each fault, no planted secret in
any report, no mutating call in 30 runs, and the planted Reliaburger bug found
and patched in 5 of 5
([record](qualification/2026-11-06-v0.2.7-exit-test.md)). Qualification ran
on 7 November 2026 on the Mac Studio, on one candidate
([closure record](qualification/2026-11-07-v0.2.7-release-closure.md)), on
the full gate, because 0.2.7 changes the agent loop. Candidate 1 passed both
staged installs, in 73 s and 58 s
([first](qualification/2026-11-07-v0.2.7-staged-install-apple-silicon.md),
[second](qualification/2026-11-07-v0.2.7-staged-install-apple-silicon-2.md)),
the power-cut and reboot lanes, 25 of 25 in every lane
([record](qualification/2026-11-07-power-cut-reboot.md)), the V02 fast tier
([record](qualification/2026-11-07-v0.2.7-sustained-v02-fast.md)) and the
jobs fast tier at 8,584.4 accepted jobs a second
([record](qualification/2026-11-07-v0.2.7-job-qualification-fast.md)), all
clean with no turn over budget. The 8-hour jobs final completed its schedule
at 9,439.4 accepted jobs a second, about 0.2.6's rate, but it wasn't clean
([record](qualification/2026-11-07-v0.2.7-job-qualification.md)): one
journal turn on node 1 took 1,215 ms against the 1 s budget, the same family
as 0.2.6's #1606
([#1707](https://github.com/reliaburger/reliaburger/issues/1707)). The
maintainer accepted it as a known issue rather than build a second
candidate; 0.2.8 works on it. It ships with the `zstd` 0.13 advisories
under the exception that expires on 18 November 2026
([#1366](https://github.com/reliaburger/reliaburger/issues/1366)). 0.2.7
keeps 0.2.6's cluster formats, so a 0.2.6 cluster can roll onto it in place
([upgrading from 0.2.6](releasing.md#upgrading-from-026)).

## 0.2.6: the rest of the evaluation's bugs, and setup over SSH

Promoted on 6 November 2026 as tag `v0.2.6` at `50596cfe`
([promote run](https://github.com/reliaburger/reliaburger/actions/runs/42178473916)).
`relish setup --ssh` builds a cluster on Linux machines you already have,
past the seven-seat council cap too
([#1529](https://github.com/reliaburger/reliaburger/issues/1529)); a node
with no Raft seat reads the leader's view of the security state, so a secure
cluster's eighth node no longer restarts for ever
([#1477](https://github.com/reliaburger/reliaburger/issues/1477)); node
certificates carry an IP SAN
([#1508](https://github.com/reliaburger/reliaburger/issues/1508)); Bun with
eBPF starts on Linux 7.0
([#1557](https://github.com/reliaburger/reliaburger/issues/1557)); and CI
runs `make test-s3` and a privileged job on Ubuntu 26.04
([#1534](https://github.com/reliaburger/reliaburger/issues/1534),
[#1559](https://github.com/reliaburger/reliaburger/issues/1559))
([plan](plans/archive/2026-11-03-plan-0.2.6.md)).
[#1500](https://github.com/reliaburger/reliaburger/issues/1500) closed as not
reproduced in 60 cycles. Qualification ran on 5 and 6 November 2026 (UTC),
the first on the Mac Studio, and took two candidates
([closure record](qualification/2026-11-06-v0.2.6-release-closure.md)), on
the full gate, because 0.2.6 moves the protocol (56 to 57) and changes the
agent loop. Candidate 1 failed the dependency audit on
[RUSTSEC-2026-0442](https://rustsec.org/advisories/RUSTSEC-2026-0442.html),
published against arrow that morning, and ran no gate; arrow 59.3.2 fixed it
([#1599](https://github.com/reliaburger/reliaburger/pull/1599)). Candidate 2
passed both staged installs, in 72 s and 57 s
([first](qualification/2026-11-05-v0.2.6-staged-install-apple-silicon.md),
[second](qualification/2026-11-05-v0.2.6-staged-install-apple-silicon-2.md)),
the power-cut and reboot lanes, 25 of 25 in every lane
([record](qualification/2026-11-05-power-cut-reboot.md)), the V02 fast tier
([record](qualification/2026-11-05-v0.2.6-sustained-v02-fast.md)) and the
jobs fast tier at 8,647.9 accepted jobs a second
([record](qualification/2026-11-05-v0.2.6-job-qualification-fast.md)), all
clean with no turn over budget. The 8-hour jobs final completed its schedule,
10 h 15 min with every leak checker armed in the quiet tail, at 9,580.2
accepted jobs a second, six times 0.2.5's final on the hot laptop, but it
wasn't clean
([record](qualification/2026-11-06-v0.2.6-job-qualification.md)): a
partition fault was refused because its source's cgroup lookup missed the
inject turn's deadline
([#1601](https://github.com/reliaburger/reliaburger/issues/1601)), and one
compaction turn on node 1 took 1,232 ms against the 1 s budget
([#1606](https://github.com/reliaburger/reliaburger/issues/1606)). The
maintainer accepted both as known issues rather than build a third
candidate; both are fixed or tracked in 0.2.7. It ships with the `zstd` 0.13
advisories under the exception that expires on 18 November 2026
([#1366](https://github.com/reliaburger/reliaburger/issues/1366)). 0.2.6
moves the protocol, so a 0.2.5 cluster can't roll to it: start a fresh
cluster ([upgrading from 0.2.5](releasing.md#upgrading-from-025)).

## 0.2.5: bugs an outside evaluation found

Promoted on 3 November 2026 as tag `v0.2.5` at `954bb7b1`
([promote run](https://github.com/reliaburger/reliaburger/actions/runs/41605919475)).
A patch release for the product bugs that an external evaluation of 0.2.0
([#1080](https://github.com/reliaburger/reliaburger/pull/1080)) found and
that were still on `main`: `relish token create` and the other token
commands work on any node, issued certificates name their issuer's key, the
Kubernetes importer keeps an init container's `env`, `relish logs-export
--dest DIR` writes where relish runs, an app can be deleted and applied
straight back, the audit table names every app an apply touched, and
`relish logs --tail` stays current
([plan](plans/archive/2026-10-31-plan-0.2.5.md)). Spare nodes also join as
Raft learners up to the council cap
([#1494](https://github.com/reliaburger/reliaburger/pull/1494)).
Qualification ran from 1 to 3 November 2026 (UTC) and took one candidate
([closure record](qualification/2026-11-03-v0.2.5-release-closure.md)), on
the full gate, because 0.2.5 moves the formats (protocol 55 to 56, state
format 72 to 73) and changes the agent loop. A first build came out as 0.2.4
because the version bump hadn't merged
([#1522](https://github.com/reliaburger/reliaburger/pull/1522)). The gate was
split around a workshop: overnight on 1 to 2 November the candidate passed
both staged installs, in 134 s and 132 s
([first](qualification/2026-11-02-v0.2.5-staged-install-apple-silicon.md),
[second](qualification/2026-11-02-v0.2.5-staged-install-apple-silicon-2.md)),
the power-cut and reboot lanes, 25 of 25 in every lane
([record](qualification/2026-11-02-power-cut-reboot.md)), the V02 fast tier
([record](qualification/2026-11-02-v0.2.5-sustained-v02-fast.md)) and the
jobs fast tier at 6,307.0 accepted jobs a second
([record](qualification/2026-11-02-v0.2.5-job-qualification-fast.md)), all
clean. The 8-hour jobs final ran the next night and completed its schedule,
10 h 20 min with every leak checker armed in the quiet tail, but it wasn't
clean: one `health_tick` turn on node 3 took 1,077 ms against the 1 s
budget, four minutes after the soak killed that node's Bun
([record](qualification/2026-11-03-v0.2.5-job-qualification.md)). The Mac
had been plugged in at 10% charge two minutes before the final started and
logged three thermal emergencies in the night; the job rate fell to 1,552.4
a second with the first, a quarter of the fast tier's, though still above
the 1,390 target, and the slow turn came just after the second. The
maintainer accepted the turn as a host stall rather than run the final
again. It ships with the `zstd` 0.13 advisories under the exception that
expires on 18 November 2026
([#1366](https://github.com/reliaburger/reliaburger/issues/1366)), and with
[RUSTSEC-2026-0433](https://rustsec.org/advisories/RUSTSEC-2026-0433.html)
in `pulldown-cmark` 0.13.5, which it doesn't expose. `make test-s3` wasn't
run and `make test-apple` failed on the host's container networking; neither
change reached what they cover, and both get fixed in 0.2.6
([#1534](https://github.com/reliaburger/reliaburger/issues/1534),
[#1535](https://github.com/reliaburger/reliaburger/issues/1535)). 0.2.5
changes the cluster's formats, so a 0.2.4 cluster can't roll to it: start a
fresh cluster ([upgrading from 0.2.4](releasing.md#upgrading-from-024)).

## 0.2.4: extension points

Promoted on 30 October 2026 as tag `v0.2.4` at `964b56a2`
([promote run](https://github.com/reliaburger/reliaburger/actions/runs/41018110348)).
Its headline is a way to bend Reliaburger without forking it: `[policy.*]`
rules in CEL that deny, warn or audit on the leader, fill-only defaults and
`relish policy audit`; `[template.*]` and `[use.*]` in place of CRDs;
sidecars, stop hooks and gated runtime knobs; an exec secret provider; and a
contract for your own controllers
([#1302](https://github.com/reliaburger/reliaburger/pull/1302), [Chapter
18](book/18-bending-without-forking.md)). Qualification ran on 29 and 30
October 2026 (UTC) and took one candidate
([closure record](qualification/2026-10-30-v0.2.4-release-closure.md)), on
the full gate, because 0.2.4 moves the formats (protocol 53 to 55, state
format 70 to 72) and changes the agent loop. Its first build failed when
GitHub's hosted runner lost contact with it, and the second made the
candidate. It passed both staged installs, in 166 s and 165 s
([first](qualification/2026-10-29-v0.2.4-staged-install-apple-silicon.md),
[second](qualification/2026-10-29-v0.2.4-staged-install-apple-silicon-2.md)),
and the power-cut and reboot lanes, 25 of 25 in every lane
([record](qualification/2026-10-29-power-cut-reboot.md)). Before any soak
reached a fault, the harness stopped three times on itself: it needed Python
3.11 for `tomllib` and the qualification Mac has 3.9
([#1483](https://github.com/reliaburger/reliaburger/issues/1483)), its
manifest put policies beside jobs, which the API refuses
([#1484](https://github.com/reliaburger/reliaburger/pull/1484)), and a
stopped run left its VMs holding the cluster's ports. 0.2.4 is the first
release whose soaks gate on a latency SLO, a job task's start lag at p99 no
more than 256 ms ([#1400](https://github.com/reliaburger/reliaburger/issues/1400)),
and both fast tiers first failed on it: counting only the tasks no fault
overlapped left each of them 20 samples, and the p99 of 20 is their largest
([#1485](https://github.com/reliaburger/reliaburger/issues/1485)). The
checker now wants 1,000 quiet samples before it judges
([#1486](https://github.com/reliaburger/reliaburger/pull/1486)); replayed
on the same evidence, the V02 fast tier passed, 1 h 09 min and 6 cycles
([record](qualification/2026-10-29-v0.2.4-sustained-v02-fast.md)), and so
did the jobs fast tier, at 6,467.0 accepted jobs a second
([record](qualification/2026-10-29-v0.2.4-job-qualification-fast.md)). The
8-hour jobs final passed clean with the original checker: 10 h 16 min, 8
cycles and a 2¼-hour quiet tail with every leak checker armed, 6,899.6
accepted jobs a second against the 1,390 target, and a job-start p99 of
32 ms over 137,306,923 quiet tasks
([record](qualification/2026-10-30-v0.2.4-job-qualification.md)). It ships
with the `zstd` 0.13 advisories 0.2.3 shipped with, under the same exception,
which expires on 18 November 2026
([#1366](https://github.com/reliaburger/reliaburger/issues/1366), in 0.3.0).
The first release profile
([#1358](https://github.com/reliaburger/reliaburger/issues/1358)) ran on the
published binaries after the tag: in the same VM, 0.2.4 ran host jobs 4.5% and
shared containers 5.7% faster than 0.2.3, with the same start lag and 15 MiB
less idle memory ([record](qualification/2026-10-30-v0.2.4-profile/README.md)).
A 24-hour memory run for
[#1417](https://github.com/reliaburger/reliaburger/issues/1417),
[#1403](https://github.com/reliaburger/reliaburger/issues/1403) and
[#1418](https://github.com/reliaburger/reliaburger/issues/1418) follows it.
On 29 October the first rolling walk between two published releases, 0.2.2
to 0.2.3, moved every node, leader last, in 2 min 20 s, and
found that a restarting Bun's replicas moved to other nodes
([record](qualification/2026-10-29-rolling-walk-0.2.2-to-0.2.3.md)); 0.2.4
fixes that ([#1449](https://github.com/reliaburger/reliaburger/issues/1449)).
0.2.4 itself changes the cluster's formats, so a 0.2.3 cluster can't roll to
it: start a fresh cluster
([upgrading from 0.2.3](releasing.md#upgrading-from-023)).

## 0.2.3: two billion a day

Promoted on 28 October 2026 as tag `v0.2.3` at `d387d569`
([promote run](https://github.com/reliaburger/reliaburger/actions/runs/40779591166)).
Qualification ran from 26 to 28 October 2026 (UTC) and took one candidate
([closure record](qualification/2026-10-28-v0.2.3-release-closure.md)), on the same gate
as 0.2.2. Its build first stopped at the dependency audit: RustSec published
13 advisories against `zstd` 0.13 on 26 October, reached only through the
Parquet files Reliaburger writes itself, and the maintainer accepted them
under a dated exception
([#1367](https://github.com/reliaburger/reliaburger/pull/1367),
[record](qualification/2026-10-26-dependency-exceptions.md)). The staged candidate was
benchmarked before any soak: 14,916.7 host jobs a second on one node and
29,350.0 on three, every node within its share
([record](qualification/2026-10-27-v0.2.3-candidate-throughput/README.md)). It passed
both staged installs, in 153 s and 149 s
([first](qualification/2026-10-26-v0.2.3-staged-install-apple-silicon.md),
[second](qualification/2026-10-26-v0.2.3-staged-install-apple-silicon-2.md)). Its first
V02 fast tier failed on one 2,260 ms `upgrade_verify` turn, the upgrade's
commit writing to disk on the agent loop while another node's disk stalled
for 4.9 s ([record](qualification/2026-10-28-v0.2.3-release-closure/candidate-1-v02-fast-run1.md)).
The maintainer accepted it under 0.2.2's host-stall rule, with the write
moving off the loop in 0.2.4
([#1381](https://github.com/reliaburger/reliaburger/issues/1381)), and the
rerun on the same candidate passed
([record](qualification/2026-10-27-v0.2.3-sustained-v02-fast.md)). The jobs fast tier
passed at 6,701.5 accepted jobs a second
([record](qualification/2026-10-27-v0.2.3-job-qualification-fast.md)), and the 8-hour
jobs final passed clean at 7,300.5 a second, with every job drained, nothing
lost and every leak checker armed through its quiet tail
([record](qualification/2026-10-27-v0.2.3-job-qualification.md)). The power-cut and
reboot lanes passed 25 of 25 in every lane
([record](qualification/2026-10-27-power-cut-reboot.md)). Then the day run, outside the
gate. The first ran the image's BusyBox, which costs twice the CPU per
process of the `busybox-static` the benchmark used, so it was stopped after
two hours and the day tier pinned the benchmark's
([#1386](https://github.com/reliaburger/reliaburger/pull/1386)). The second
counted 1,807,742,226 host-job tasks in 24 hours on three 4-vCPU VMs, 20,920
a second: below the 2 billion target, above the 1 billion floor
([record](qualification/2026-10-28-v0.2.3-day-run.md)). The harness's verdict is FAIL,
published unchanged: Bun's memory grew all day on every node, still inside
its budget ([#1417](https://github.com/reliaburger/reliaburger/issues/1417)),
node 1's disk filled near the end
([#1418](https://github.com/reliaburger/reliaburger/issues/1418)), and one
turn on node 3 ran 1 to 2.5 s
([#1419](https://github.com/reliaburger/reliaburger/issues/1419)). The
maintainer accepted the measured number and shipped with all three fixed in
0.2.4. On the day of the promotion RustSec published RUSTSEC-2026-0414 against
`pulldown-cmark` 0.13.4, which needs an option Reliaburger never sets; the
maintainer kept the candidate
([record](qualification/2026-10-28-dependency-exceptions.md)), and `main` has the fix
([#1409](https://github.com/reliaburger/reliaburger/pull/1409)). 0.2.3 is the
first release to keep its predecessor's formats, so a 0.2.2 cluster can roll
onto it; that walk isn't qualified yet
([#1444](https://github.com/reliaburger/reliaburger/issues/1444)).

## 0.2.2: UX polish

Promoted on 25 October 2026 as tag `v0.2.2` at `43baee64`
([promote run](https://github.com/reliaburger/reliaburger/actions/runs/40244465831)).
Qualification ran from 21 to 25 October 2026 (UTC) and took four candidates
([closure record](qualification/2026-10-25-v0.2.2-release-closure.md)), on the
same gate as 0.2.1. Candidate 1's V02 fast tier was refused a network delay in
its first cycle: since chaos reached jobs
([#704](https://github.com/reliaburger/reliaburger/issues/704)), every runc job
attempt is a delay caller, and one that finished while `tc` programmed it
failed the whole fault
([#1291](https://github.com/reliaburger/reliaburger/issues/1291)). The same
run showed `relish wtf`'s GitOps check was always UNKNOWN
([#1286](https://github.com/reliaburger/reliaburger/issues/1286)). Candidate
2's jobs fast tier found Bun panicking after `shutdown complete`: a
process-owner request on a blocking thread registered a Tokio timer after the
runtime had stopped, so Bun now talks to process owners over a plain `std`
socket with its own deadline
([#1299](https://github.com/reliaburger/reliaburger/issues/1299)). Candidate
3 stopped at its build's dependency audit: RustSec published
RUSTSEC-2026-0387 on 23 October, for crypto-bigint 0.5, reached only through
`age` 0.12's P-256 code, which Reliaburger never calls. The maintainer
accepted it under a reviewed exception
([#1318](https://github.com/reliaburger/reliaburger/pull/1318),
[record](qualification/2026-10-24-dependency-exceptions.md)). Candidate 4
passed both staged installs, in 261 s and 143 s
([first](qualification/2026-10-24-v0.2.2-staged-install-apple-silicon.md),
[second](qualification/2026-10-24-v0.2.2-staged-install-apple-silicon-2.md)).
Its V02 fast tier failed twice and its jobs fast tier once, each on one slow
agent-loop turn, 1.4 to 5.6 s, waiting on a disk write while the soak host's
three VMs stalled on disk together
([V02 fast run 1](qualification/2026-10-24-v0.2.2-sustained-v02-fast.md),
[run 2](qualification/2026-10-25-v0.2.2-release-closure/candidate-4-v02-fast-run2.md),
[jobs fast](qualification/2026-10-24-v0.2.2-job-qualification-fast.md)). The
maintainer accepted all three as host stalls for 0.2.2, and set a rule for the
class: a slow turn whose slow part is a disk write, with a multi-second stall
on another node at the same time and every other check passing. Its first
8-hour jobs final passed everything but the job campaign's drain, which the
harness asked for while the hour-8 graceful whole-cluster stop was still
running ([#1338](https://github.com/reliaburger/reliaburger/issues/1338),
[record](qualification/2026-10-25-v0.2.2-release-closure/candidate-4-jobs-final-run1.md)).
With the harness fixed
([#1339](https://github.com/reliaburger/reliaburger/pull/1339)), run 2 on the
same candidate ran the full 8 hours at 3,877 accepted jobs a second, drained
the campaign with nothing missing, lost nothing and kept Bun's memory within
target ([record](qualification/2026-10-25-v0.2.2-job-qualification.md)). Its
one failure was a 3,634 ms receipt-confirmation turn in a disk stall node 3
saw in the same second, a host stall under the maintainer's rule. The
power-cut and reboot lanes passed 25 of 25 in every lane
([record](qualification/2026-10-25-power-cut-reboot.md)). 0.2.2 ships with
two writes that still wait on the agent loop, so a slow disk can hold a node's
loop for seconds: the egress-owners file
([#1263](https://github.com/reliaburger/reliaburger/issues/1263)) and the
consumer receipt confirmation
([#1340](https://github.com/reliaburger/reliaburger/issues/1340)), both fixed
in 0.2.3. It also ships with RUSTSEC-2026-0387 under the exception; 0.2.3
moves to `age` 0.11.5, which drops crypto-bigint 0.5
([#1333](https://github.com/reliaburger/reliaburger/pull/1333)). It's the
first release without an Intel macOS build
([#1018](https://github.com/reliaburger/reliaburger/issues/1018)).

## 0.2.1: cluster safety

Promoted on 21 October 2026 as tag `v0.2.1` at `abec2c45`
([promote run](https://github.com/reliaburger/reliaburger/actions/runs/39575597952)).
Qualification ran from 17 to 21 October 2026 (UTC) and took nine candidates
([closure record](qualification/2026-10-21-v0.2.1-release-closure.md)), the
first on the shorter gate: one 8-hour jobs final in place of 0.2.0's separate
V02 and job finals ([#991](https://github.com/reliaburger/reliaburger/issues/991)).
Candidate 1 never got past its staged install: 0.2.1 refuses a health timeout
longer than its interval, and the quickstart's own `hello` app had one
([#1019](https://github.com/reliaburger/reliaburger/issues/1019)). Candidate 2
couldn't launch a fresh runc job
([#1021](https://github.com/reliaburger/reliaburger/issues/1021)). Candidate 3
found that `relish local stop` power-cut every VM, because Lima kills a guest
that ignores its stop request after 30 s
([#1023](https://github.com/reliaburger/reliaburger/issues/1023)), and that
cut left a Raft store redb couldn't open
([#1024](https://github.com/reliaburger/reliaburger/issues/1024)). Candidates 3
and 4 both found a leader that couldn't prove its leadership forwarding API
calls to itself until it ran out of memory
([#1069](https://github.com/reliaburger/reliaburger/issues/1069)).
Candidate 5 found a health-probe publication holding the agent loop for 5 s
([#1100](https://github.com/reliaburger/reliaburger/issues/1100)). From
candidate 6 on, the V02 fast tier passed and the failures moved to the job
soaks: a failed redeploy that marked a released version failed for good
([#1110](https://github.com/reliaburger/reliaburger/issues/1110)), a stale
follower answering a batch cancel with 404
([#1192](https://github.com/reliaburger/reliaburger/issues/1192)), and Bun
exiting after a self-upgrade because a forking child held its upload lock
([#1198](https://github.com/reliaburger/reliaburger/issues/1198)), plus a
handful of harness bugs. Candidate 9 passed both staged installs, the V02 fast
tier and the jobs fast tier at 3,520 accepted jobs a second
([V02 fast](qualification/2026-10-20-v0.2.1-sustained-v02-fast.md),
[jobs fast](qualification/2026-10-20-v0.2.1-job-qualification-fast.md)). Its
first jobs final ran the full 8 hours at 3,840 accepted jobs a second and
failed on one 3.95 s agent-loop turn: a health-probe journal write caught in a
host disk stall ([#1255](https://github.com/reliaburger/reliaburger/issues/1255)).
The maintainer re-ran it on the same candidate, and accepted slow turns from
#1255 as a known issue for 0.2.1, provided everything else passed. Run 2 ran
the full 8 hours at 3,764 accepted jobs a second, with every fault settled,
nothing lost and Bun's memory within target
([record](qualification/2026-10-21-v0.2.1-job-qualification.md)). Its only
failures were slow agent-loop turns: #1255 again, which the checker counted
twice ([#1261](https://github.com/reliaburger/reliaburger/issues/1261)), and
the same fsync'd discovery journal write at a second call site
([#1266](https://github.com/reliaburger/reliaburger/issues/1266)), which the
maintainer accepted too. The power-cut and reboot lanes then passed 25 of 25 in
every lane ([record](qualification/2026-10-21-power-cut-reboot.md)).
0.2.1 ships with #1255 and #1266 (a slow disk can still hold a node's agent
loop for seconds; the fixes are in 0.2.2), and with RUSTSEC-2026-0381, -0382
and -0383, published after the gate: its rustls-webpki and webpki-roots trust
the distrusted emSign intermediates on outbound HTTPS, as 0.2.0's do. Cluster
mTLS isn't affected, `main` has the patched versions, and 0.2.2 is the first
release built with them
([#1269](https://github.com/reliaburger/reliaburger/issues/1269)). It's also
the last release with an Intel macOS CLI (`relish-macos-x86_64`); from
0.2.2 the macOS build is Apple silicon only
([#1018](https://github.com/reliaburger/reliaburger/issues/1018)).

## 0.2.0: A million jobs

Promoted on 16 October 2026 as tag `v0.2.0` at `cc5f6439`
([promote run](https://github.com/reliaburger/reliaburger/actions/runs/38944559201)).
Qualification ran from 12 to 16 October 2026 (UTC) and took eight
candidates ([closure record](qualification/2026-10-16-v0.2.0-release-closure.md)).
All eight installed cleanly; the soaks, now carrying a job campaign alongside
the apps, failed the first seven. Candidates 1 and 2 never got past setup:
Bun crash-looped when the soak switched it from runc to the mixed runtime
([#823](https://github.com/reliaburger/reliaburger/issues/823),
[#839](https://github.com/reliaburger/reliaburger/issues/839)). Candidates 3
to 7 found the API node's Bun at over 1 GiB
([#892](https://github.com/reliaburger/reliaburger/issues/892),
[#945](https://github.com/reliaburger/reliaburger/issues/945)), a launch
failure that was never retried, a killed shell reported as a clean exit,
stale executor slots after a Bun kill, and a replica refused for capacity and
never moved, plus a dozen harness bugs in the new job checks. Candidate 8
passed the 8-hour V02 final tier cleanly and the jobs fast tier at 5,001
accepted jobs a second. Its first 12-hour jobs final failed after six hours:
not Bun, but Lima 2.1's gRPC port forwarder leaking a socket per forwarded
request until the API node's guest agent held 1 GiB and the kernel killed Bun
([#982](https://github.com/reliaburger/reliaburger/issues/982),
[lima-vm/lima#5210](https://github.com/lima-vm/lima/issues/5210)). The rerun
forwarded ports over SSH instead
([#986](https://github.com/reliaburger/reliaburger/pull/986)) and ran the full
12 hours at 4,600 accepted jobs a second, with Bun's memory within target. Its
15 failures all came from the harness: SSH forwarding that Lima never retried
after a reboot, cutting a healthy node off from the host, and the job
controller running out of its run budget after 10.5 hours. The maintainer
counted both as known harness artefacts, and 0.2.1 removes them
([#983](https://github.com/reliaburger/reliaburger/issues/983),
[#987](https://github.com/reliaburger/reliaburger/issues/987),
[#989](https://github.com/reliaburger/reliaburger/issues/989);
[record](qualification/2026-10-17-v0.2.0-job-qualification.md)). Every
power-cut and reboot lane passed 25 of 25
([record](qualification/2026-10-16-power-cut-reboot.md)).
0.2.0 ships with the quickstart still pinning Lima 2.1.0, so with that leak
and with CVE-2026-53657 (root in a QEMU-driven VM through the guest agent's
socket); 0.2.1 bundles Lima 2.2.1
([#983](https://github.com/reliaburger/reliaburger/issues/983)).

## 0.1.6: operations security

Promoted on 7 October 2026, the day after 0.1.5. One candidate passed
everything first time: the
[staged install](qualification/2026-10-06-v0.1.6-staged-install-apple-silicon.md),
the [compressed](qualification/2026-10-06-v0.1.6-soak-compressed.md) and
[full](qualification/2026-10-06-v0.1.6-soak-full.md) CI soaks, and a clean
[8-hour final tier](qualification/2026-10-06-v0.1.6-sustained-v02-final.md).

## 0.1.5: bug fixes

Promoted on 6 October 2026 after two candidates. The first one's 8-hour final
tier found a node crash-looping after Lima's guest agent stepped the guest
clock, because a clock step made its adopted processes look like strangers
([#607](https://github.com/reliaburger/reliaburger/issues/607)). The fix
([#609](https://github.com/reliaburger/reliaburger/pull/609)) went into a
re-cut, which passed the
[staged install](qualification/2026-10-05-v0.1.5-staged-install-apple-silicon.md),
the [compressed](qualification/2026-10-05-v0.1.5-soak-compressed.md) and
[full](qualification/2026-10-05-v0.1.5-soak-full.md) CI soaks and the
[8-hour final tier](qualification/2026-10-05-v0.1.5-sustained-v02-final.md).
It also fixed the three bugs 0.1.4 shipped with.

## 0.1.4: cross-node logs and events, security fixes

Promoted on 3 October 2026, the same day as 0.1.3. The
[staged install](qualification/2026-10-03-v0.1.4-staged-install-apple-silicon.md)
passed, and the [compressed](qualification/2026-10-03-v0.1.4-soak-compressed.md)
and [full](qualification/2026-10-03-v0.1.4-soak-full.md) CI soaks found three
bugs that it shipped with and 0.1.5 fixed:
consumer syncs over the agent loop's turn budget
([#505](https://github.com/reliaburger/reliaburger/issues/505)), a pooled peer
connection presenting an expired leaf
([#509](https://github.com/reliaburger/reliaburger/issues/509)), and the
leader resigning for disk pressure over unexported logs
([#510](https://github.com/reliaburger/reliaburger/issues/510)).

## 0.1.3: agent loop, observability and test quality

Promoted on 3 October 2026, two days after 0.1.2. It went through the release
path twice: after the first candidates, every known bug fix found since was
folded in (data safety, council recovery, the scheduler and API audit) and the
release re-cut ([#470](https://github.com/reliaburger/reliaburger/pull/470)).
The flakes and turn-budget misses found qualifying the first candidates were
fixed on the way. The [release notes](https://github.com/reliaburger/reliaburger/releases/tag/v0.1.3)
record two cold installs (377 s and 379 s on Apple silicon), then the fast
tier and an 8-hour soak with no failures and, for the first time, no
agent-loop turn over 1 s.

## 0.1.2: images

Promoted on 1 October 2026, a day after 0.1.1. The
[release notes](https://github.com/reliaburger/reliaburger/releases/tag/v0.1.2)
record two cold installs (240 s and 250 s on Apple silicon), then the fast tier
and an 8-hour soak with no failures. Only the x86_64 half of a multi-platform
image ran under runc in CI; both architectures haven't run in one cluster yet.

## 0.1.1: bug fixes

Promoted on 30 September 2026, a day after 0.1.0, with every bug known from
0.1.0's soak, early users and the static review. The
[release notes](https://github.com/reliaburger/reliaburger/releases/tag/v0.1.1)
record two cold installs (191 s and 183 s on Apple silicon), then the fast tier
and an 8-hour soak, both with no failures.

## 0.1.0: the first release

Promoted on 29 September 2026 after five days and 13 candidates of
qualification, from 25 September
([closure record](qualification/2026-09-27-v0.1.0-release-closure.md)). The
soaks found most of what that push fixed: a
deploy that wedged every retry, Bun's memory climbing from 240 to 734 MB in an
hour because Lima 2.1's guest agent never closed forwarded connections, slow
health ticks starving the agent loop, and a stopped volume app restarting on
an empty volume. Candidate 13 passed the
[fast](qualification/2026-09-29-v02-fast-tier-7d7dfc0.md) and
[8-hour final](qualification/2026-09-29-sustained-v02.md) tiers cleanly.
