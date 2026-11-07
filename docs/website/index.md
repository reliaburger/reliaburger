<!-- Generated from docs/website/index.html by `make agent-docs`: edit the page, not this file. -->

RUST · APACHE 2.0 · 0.2.7

# The container platform, served ready.

Built for humans and their agents.

Kubernetes hands you the ingredients. Reliaburger hands you the burger: app scheduling, batch jobs, metrics, logs, service discovery, ingress, mTLS, a Docker registry, GitOps and chaos testing, in one Rust agent and one CLI. And that CLI speaks to your AI agent too.

[Get a working cluster in under 5 minutes](https://reliaburger.com/#install) [Debug it with your agent](https://reliaburger.com/#debug)

[Read the source](https://github.com/reliaburger/reliaburger) [Read the whitepaper](https://github.com/reliaburger/reliaburger/blob/main/docs/whitepaper.md) [PDF](https://github.com/reliaburger/reliaburger/releases/latest/download/reliaburger-whitepaper.pdf) [Read the book](https://github.com/reliaburger/reliaburger/tree/main/docs/book) [PDF](https://github.com/reliaburger/reliaburger/releases/latest/download/building-reliaburger.pdf)

## Built differently

1.  ### One agent instead of a dozen add-ons.

    App scheduling, batch jobs, metrics, logs, service discovery, ingress, PKI, a Docker registry, consensus, GitOps and fault injection are compiled into `bun`, the one agent every node runs. There’s no Prometheus, CoreDNS, ingress controller, etcd or Argo CD to install, upgrade and keep in step with each other.

2.  ### Seven voters at most. Everyone else gossips.

    A Raft council inside the agent holds the cluster’s state, and the cluster picks its voters from your ordinary nodes, never more than seven. Every node finds the others and notices failures through SWIM gossip, so a bigger cluster doesn’t mean a bigger quorum or a separate etcd to run.

3.  ### Service discovery in the kernel, with no proxy in the path.

    Every app gets a stable virtual IP, and an eBPF hook on `connect()` sends each connection straight to a healthy backend before its first packet leaves. There’s no kube-proxy and no sidecar on the hop, and the same hook decides who may connect at all.

4.  ### A million tasks, stored as one.

    Batch jobs run beside your apps under the same CPU and memory admission, and a task array is stored as a template and a count, not a record per task. Each job picks its isolation: a fresh container, a shared container with a process per task, or an allowlisted host command.

5.  ### Chaos is a command, not an install.

    `relish fault` adds latency, partitions, DNS failures, kills and CPU, memory or disk pressure from inside the agent, so there’s no chaos operator to deploy. Every fault ends on its own, and the cluster refuses one that would take out every replica or put the council’s quorum at risk.

6.  ### Your agent can read the recipe.

    New in 0.2.7, `relish agent` answers an AI agent in one line of JSON within a byte budget, and carries the manual and the exact source its binary was built from. Your agent reads how a finding is decided instead of guessing, and with a read-only token it can read the cluster and change nothing.

## Install it

Kick the tyres With your agent

INSTALL sh

```
curl -fsSL https://reliaburger.com/install.sh | sh -s -- --laptop
```

One line installs `relish`, then builds a three-node cluster of Linux VMs on your laptop, running real containers. Every download is checked against a pinned digest or the release’s signing key. From 0.2.7, you can check any release file yourself, too: `gh attestation verify FILE --repo reliaburger/reliaburger`, with the flags in [the release guide](https://github.com/reliaburger/reliaburger/blob/main/docs/releasing.md#build-provenance-attestations). Got Linux machines already? Swap `--laptop` for `--install-only` to install just `relish`, then follow the [Linux servers guide](https://github.com/reliaburger/reliaburger/blob/main/docs/linux-servers.md).

You’ll need a Mac with Apple silicon, or Linux with QEMU and KVM, plus about 8 GiB of free memory and 15 GiB of disk. No Rust, no checkout.

[One node, CLI only, cleanup](https://github.com/reliaburger/reliaburger/blob/main/docs/quickstart.md) · [Read the bootstrap script](https://reliaburger.com/install.sh) · [Release notes](https://github.com/reliaburger/reliaburger/releases/tag/v0.2.7)

PROMPT your agent

```
Set up a Reliaburger cluster for me. Start at https://reliaburger.com/agents.
```

Paste that into Claude Code, Codex or any agent with a shell. The page it reads is a playbook: ask you where the cluster should run (this computer, machines you already have, or a cloud), check the provider’s current prices and free allowances, and create nothing that costs money until you say yes. Then it installs `relish`, builds the cluster, shows you around, and leaves a summary with the way to remove it.

You still make the decisions; it does the typing. Give it a read-only token afterwards, and it can help you run the cluster too.

[Read the playbook](https://reliaburger.com/agents.html) · [As Markdown](https://reliaburger.com/agents.md) · [Add Reliaburger to your agent](https://reliaburger.com/agents.html#add-to-your-agent)

**Where it stands**

Good enough for home labs and dev clusters. Not for production until 1.0.0, expected in January 2027. Until then, an upgrade can mean starting a fresh cluster.

[Roadmap](https://github.com/reliaburger/reliaburger/blob/main/docs/roadmap.md)

**0.2.7: Built for humans and their agents**

What that line installs, released on 7 November 2026. `relish agent` answers your AI agent in JSON, with the manual and the exact source its binary was built from; `relish mcp` serves it as read-only tools; and every release file carries a provenance attestation you can check. A 0.2.6 cluster rolls onto it in place. Setup over SSH arrived in 0.2.6.

[Release notes](https://github.com/reliaburger/reliaburger/releases/tag/v0.2.7)

## Try it in five minutes

Put the cluster through its paces with a burger built from eight apps: a batch of jobs, a layer cut off, a rolling deploy, a rollback, a scale-up, a house rule, a killed patty, a slow network and a lost node.

The whole tour on a real laptop cluster, run by [scripts/demo/tour.sh](https://github.com/reliaburger/reliaburger/blob/main/scripts/demo/tour.sh) against the published 0.2.7 install. Setup plays four times faster and idle time is cut to two seconds; every wait says how long it really took. Jump to a step with the buttons above, or the marks on the progress bar. speed

Show the commands every step, with what to look for

1.   ```
     curl -fsSL https://reliaburger.com/install.sh | sh -s -- --laptop
     ```

     The install line from above. It’s the only step that takes minutes.

2.   ```
     curl -fsSL https://reliaburger.com/demo/burger.tar.gz | tar xz
     ```
     ```
     relish build burger/burger.toml
     ```
     ```
     relish images
     ```

     An app of your own: one small Go program and its Dockerfile. A node builds it with Buildah, straight into the cluster’s own registry, signed, for both `linux/amd64` and `linux/arm64`: `images` lists `burger:v1` once, with its platforms. It’s the only image the tour needs.

3.   ```
     relish apply burger/burger.toml
     ```
     ```
     relish status
     ```
     ```
     relish council status
     ```

     Eight apps from that one image: a layer of the burger each (top bun, lettuce, tomato, cheese, patty, bottom bun), a counter in front and a diner who keeps ordering. Give it half a minute. Three counters, one on each node, named `counter-0`, `-1` and `-2`: the scheduler spread and numbered them, not you. `status` opens with a line on the council, the Raft group that holds the cluster’s state; `council status` asks every node for its own view of it.

4.   ```
     curl http://burger.localhost:18080
     ```

     The burger, through the built-in ingress. The counter asks each layer by its service name and stacks whatever answers: one coloured line per layer, with its version and the node and instance that served it. Open the same address in a browser and it’s drawn in SVG, redrawn every second, so keep it open for what follows.

5.   ```
     relish batch submit burger/jobs.toml
     ```
     ```
     relish batch watch 1
     ```
     ```
     relish jobs
     ```

     Now cook some patties beside it: 8 small and 2 larger hashing jobs from the same image, in two resource profiles. Each profile is stored as a template and a count, not a record per task, and its tasks are packed around the apps already running. `watch` takes the number `submit` printed and shows the rate and how long tasks take; `jobs` lists the finished run. Each task tells the counter its patty is done, and the burger’s footer counts them. It keeps serving throughout.

6.   ```
     relish fault partition lettuce --from counter --duration 30s --acknowledge
     ```
     ```
     curl http://burger.localhost:18080
     ```

     Cut the lettuce off from the counters, and there’s a gap where it was: the cluster refuses their connections. Nothing else notices, and after thirty seconds the fault lifts itself and the lettuce is back. No chaos tooling to install.

7.   ```
     relish apply burger/cheese-v2.toml
     ```
     ```
     curl http://burger.localhost:18080
     ```

     A rolling deploy: the same two cheese slices at `v2`. The node starts a blue cheese beside the cheddar, waits for its health check, then drains a cheddar, and goes again. The other slice keeps serving meanwhile, so the burger never loses its cheese; it turns blue a slice at a time.

8.   ```
     relish apply burger/patty-v3.toml
     ```
     ```
     relish status
     ```
     ```
     relish history patty
     ```

     A bad deploy: patty `v3` is raw in the middle and fails its health check. It never joins the service, so the burger keeps its beef patty. After twenty seconds the node rolls it back on its own, and `status` marks the patty `RolloutFailed` so nothing retries it. `history` shows what shipped when, on every node.

9.   ```
     relish apply burger/triple-patty.toml
     ```
     ```
     curl http://burger.localhost:18080
     ```

     Three patty replicas, and the counter finds all three: a triple patty, one line per instance. Any change to the spec clears the `RolloutFailed` mark, so this one goes out.

10.  ```
     curl -fsSLO https://raw.githubusercontent.com/reliaburger/reliaburger/v0.2.7/examples/policies/platform.toml
     ```
     ```
     relish policy test -f platform.toml -f burger/burger.toml
     ```

     House rules, without forking anything. `platform.toml` is the release’s example of the four policies platform teams ask for most, each a line or two of CEL that the leader judges every apply by. `policy test` tries the burger against them offline: none of its apps comes from a trusted registry, so each one is listed with the rule’s own message. Applied, the rules would refuse the next deploy before anything is written. New in 0.2.4.

11.  ```
     relish fault kill patty --acknowledge
     ```
     ```
     curl http://burger.localhost:18080
     ```
     ```
     relish events --app patty
     ```

     Kill a patty. For a few seconds it’s a double; then the node agent has restarted it and it’s a triple again, the new patty under a new instance number. The cluster won’t kill an app’s last replica, which is why this is a patty and not the lettuce. `events` asks every node what happened and merges the answers by time: the deploys, the failed `v3` and its rollback, the kill and the restart, each with its node.

12.  ```
     relish path counter --to patty
     ```

     Walk the path from a counter to the patties, hop by hop: DNS, the virtual IP, the eBPF service map in the kernel, the firewall, any active faults, then a real TCP connect from inside the counter. On Kubernetes you’d piece that together from five tools and a debug pod.

13.  ```
     relish metrics counter
     ```

     The counters’ own Prometheus metrics, from every replica. Each node scrapes the replicas it runs, because the app says `metrics = {}`. There’s no Prometheus to install.

14.  ```
     relish fault delay patty 300ms --from counter --duration 2m --acknowledge
     ```
     ```
     relish path counter --to patty --count 3
     ```

     Now make the patties slow, but only for the counters. Every packet they send a patty waits 300 ms; the other layers don’t notice, and it lifts itself after two minutes. The path is now DEGRADED: it names the fault, shows the delay on the counter’s network interface, and every connect takes 300 ms. Your browser shows the same thing, each patty line in the hundreds of milliseconds.

15.  ```
     relish dashboard
     ```

     The same thing as live charts in your browser: CPU, memory in MiB, requests per second and latency in milliseconds, one line per replica. Ctrl-C closes it; the cluster keeps running.

16.  ```
     relish local stop node-3
     ```
     ```
     relish inspect counter
     ```

     Now lose a whole machine. About a minute later there are three running counters again, spread over the two nodes that are left: each keeps the one it had, and one of them starts a replacement under the lost one’s name. Any layer that lived on node-3 comes back the same way. Nobody had to notice first. `inspect` asks every node, lists each instance, running or stopped, with the node it’s on, and names the node that didn’t answer.

17.  ```
     relish wtf
     ```

     One screen on what’s wrong and what to do about it. Here: a council member is missing, and one more failure would lose quorum. It also checks that every node runs the same build. It’s what you’d run first in a real incident.

Finished? Remove the cluster, then Relish itself:

```
relish local destroy --yes
```
```
relish uninstall
```

Also in the CLI: `relish manual tour` · [Read it on GitHub](https://github.com/reliaburger/reliaburger/blob/main/docs/manual/08_five-minute-tour.md) · [Prerequisites, retries and cleanup](https://github.com/reliaburger/reliaburger/blob/main/docs/quickstart.md)

## A million jobs, your choice of isolation

Jobs run on the same cluster as your apps, in the room the apps leave free, so there’s no second system to install. Run one task or an array of a million, on a schedule or on demand. Give each task a fresh container for full isolation, share a container across short tasks to start them faster, or run them straight on the host when speed matters more than the boundary.

**1,807,742,226 host jobs in one real day.** 0.2.3 ran host jobs for 24 hours on three 4-vCPU VMs on one laptop: 20,920 a second, counted as the council accepted them, not extrapolated from a minute. That’s 1.81 billion against the two billion we aimed for, with no failed tasks and every node within a point of its share. Short benchmarks of the same release ran 14,917 a second on one node and 29,350 on three; a whole day on a laptop whose cores the VMs fill runs slower. Bun’s memory grew all day, inside its budget, and that growth isn’t fully fixed yet ([#1417](https://github.com/reliaburger/reliaburger/issues/1417)). [Day-run record](https://github.com/reliaburger/reliaburger/blob/main/docs/qualification/2026-10-28-v0.2.3-day-run.md) · [Benchmark record](https://github.com/reliaburger/reliaburger/blob/main/docs/qualification/2026-10-27-v0.2.3-candidate-throughput/README.md)

[![Bun resident memory per node over the 24-hour day run, growing from about 200 to about 345 MiB, against the 512 MiB target for node 1 and 384 MiB for the others](https://reliaburger.com/assets/day-run/rss.svg)](https://reliaburger.com/assets/day-run/rss.svg)

Bun memory per node over the day run, with the 512 and 384 MiB targets. The ✕ marks are failed health checks, nearly all the leak check flagging this growth, not failed job tasks.

What does each kind of isolation cost? Here’s the same command, run four ways on the same VM with the same concurrency.

Recorded on a 0.2.0 development build. Four real 60-second windows: VM baseline, fresh containers, shared containers and host jobs. Chapter jumps and playback speed change only your view; preparation and cleanup stay visible. speed 

Show the commands run the four scenarios on your own setup

Each scenario defaults to 60 seconds and concurrency 27 per node. The public scenarios use your configured cluster endpoint, token and CA. They submit real work, so use a test cluster.

1.  ```
    relish bench --scenario jobs-vm-baseline
    ```

    Run this inside the Linux VM that runs your jobs. Relish prepares the pinned BusyBox executable before timing raw process exits. This is the local reference, without scheduler or durability costs.

2.  ```
    relish bench --scenario jobs-containers
    ```

    Fresh containers for fully isolated, independent jobs. Each job starts its own container. The node needs rootful Linux runc; image preparation and startup count inside the public window.

3.  ```
    relish bench --scenario jobs-shared-containers
    ```

    Shared containers for repeatable tasks where startup would dominate the work. Each command starts a new process in a reusable container; this doesn't keep an interpreter or AI model resident.

4.  ```
    relish bench --scenario jobs-host-processes
    ```

    Host jobs for maximum speed with no isolation. Bun must run in `auto` or `mixed` mode on supported rootful Linux, with `/bin/busybox` explicitly allowlisted and `mount_isolation = false`. Explicit `runc` mode refuses host jobs; the benchmark doesn't change these permissions.

For a matched comparison, use a single-node test cluster and run all four sequentially on the same VM with the same BusyBox binary. The defaults match the recording's duration and concurrency; available resources determine how many commands actually run. Image pulls count in the public windows unless you've warmed the image beforehand.

```
relish bench --scenario jobs-shared-containers --seconds 60 --concurrency 27 --report shared-jobs.json
```

Change the duration or concurrency to suit your rig. The report includes accepted successes, failures, retries, extrapolated runs/day and cleanup; choose a new report filename for each run. Relish cancels its own work and verifies drain after the timed window, including on Ctrl-C.

[Runtime setup, permissions and measurement details](https://github.com/reliaburger/reliaburger/blob/main/docs/manual/14_batch-jobs.md#measure-job-throughput-with-relish)

Show the results and setup use cases, rates and testing rig

All three job types get the same resource admission, durable ownership, retries and accepted outcomes. Host jobs also need an explicitly allowlisted binary.

Released v0.2.0: the median of five rounds of the four scenarios, 60 seconds each. Same VM, BusyBox command and concurrency cap of 27. Daily figures are extrapolated.
| Execution         | Best for                                         | Completed in 60 seconds | Rate       | Extrapolated runs/day | Retries          |
| ----------------- | ------------------------------------------------ | ----------------------- | ---------- | --------------------- | ---------------- |
| VM baseline       | Raw-process reference on this VM                 | 929,510 raw exits       | 15,491.8/s | 1.3B                  | Not applicable   |
| Fresh containers  | Fully isolated, independent jobs                 | 762 accepted jobs       | 12.7/s     | 1.1M                  | 1 in five rounds |
| Shared containers | Repeatable tasks; amortised container startup    | 175,000 accepted jobs   | 2,916.7/s  | 252M                  | 0                |
| Host jobs         | Maximum speed for trusted commands; no isolation | 280,000 accepted jobs   | 4,666.7/s  | 403.2M                | 0                |

Testing rig: a local Lima VM on an Apple M2 Max host (12 physical cores, 32 GiB host RAM), with 4 vCPU and 8 GiB allocated to Ubuntu 24.04 aarch64, Linux 6.8 and runc 1.4. These figures measure the per-job overhead of BusyBox `true`, a command that does no work; benchmark your real commands on your own hardware when sizing a cluster. Single windows vary: across the five rounds, raw and host-job rates varied by about a quarter and the container paths by up to 15%. With 13 ms of real work per task, host jobs reached three quarters of the raw rate in an earlier rerun.

The VM baseline is a raw-process reference for this VM and command, not a ceiling. It counts successful exits without resource enforcement, durable ownership or task ledgers. Public rows count unique accepted successes received within the window; their rate gap includes those guarantees. Runs/day multiplies the minute count by 1,440; it isn't an observed daily total.

The published v0.2.0 binaries, 4 vCPU / 8 GiB, Ubuntu 24.04 aarch64, beside a 500m BusyBox web app. Pinned BusyBox commands, warm images and cold executors. The recording above is an earlier development build's single run. All public paths request 25m CPU / 32 MiB and enforce a one-core limit; native/shared helpers reserve another 10m / 8 MiB. One active submission caps each public mode at 27. Fresh containers use one-job receipt chunks; fast modes use 1,000-job chunks to amortise reporting. The receipt size controls reporting, not resource admission. Counts conservatively exclude results after the last sample. Preparation, cancellation and verified cleanup remain visible outside the 60-second windows. All windows have zero failures; one fresh-container window accepted one retry.

[v0.2.0 measurements](https://github.com/reliaburger/reliaburger/tree/main/docs/qualification/2026-10-17-v0.2.0-job-throughput) · [Recording's report](https://reliaburger.com/assets/job-throughput-report.json) · [The recording's raw results](https://github.com/reliaburger/reliaburger/tree/main/docs/qualification/2026-10-09-timed-job-scenarios) · [Job manual](https://github.com/reliaburger/reliaburger/blob/main/docs/manual/14_batch-jobs.md)

Arrays suit data preparation, offline evaluation and independent inference commands. Each job still starts a process; keeping an AI model resident is a separate planned executor.

## How a release is tested

Green unit tests don't ship a release. Every candidate is built and signed once, installed the way you'd install it, then left running under faults for hours. Only the maintainer promotes it, and only the exact files that passed.

1.  **Build and sign.** CI builds every binary and guest image at one commit and signs them. Nothing is rebuilt after this.
2.  **Stage.** The signed files go up as a private pre-release.
3.  **Install it like you would.** `curl | sh` from empty caches, twice, then the five-minute tour and a full teardown.
4.  **Soak.** Three VMs run apps, a volume writer, Redis, a log spammer and exporters while the harness breaks things every few minutes, for about an hour on a compressed schedule.
5.  **Soak with jobs, fast then final.** The same, plus a job campaign at more than the rate of 100 million jobs a day and a node-level fault every cycle: about an hour compressed, then 8 hours on the full schedule, which checks everything the app soak does as well as the jobs.
6.  **Pull the plug.** A throwaway VM loses power mid-write, 10 to 25 times per storage path, and reboots as often with containers and network rules in place.
7.  **Promote.** The maintainer tags the candidate's commit, and the promotion copies the qualified files, byte for byte, into the release.

What the soak does to the cluster, every hour of the full schedule
| Fault              | How                                                                                                   |
| ------------------ | ----------------------------------------------------------------------------------------------------- |
| Agent killed       | SIGKILL Bun on a follower, then on the leader                                                         |
| Killed mid-deploy  | A rolling deploy, with SIGKILL 0–20 s later on a node receiving a replacement                         |
| Power cut          | A VM forced off for 60–180 s, then started again                                                      |
| Chaos              | `relish fault`: kill, pause, delay, drop, partition and DNS failure                                   |
| Upgrade walk       | Every node to a signed soak build and back, every other walk with a power-off or leader kill mid-walk |
| Node faults (jobs) | Inter-node delay, a full disk and slow fsync                                                          |
| Whole cluster      | Graceful stop and start, quorum loss with two nodes off, and every VM off at once                     |

One occurrence of any of these fails the release: a lost write or a counter that goes backwards, a certificate close to expiry, no leader for more than 30 seconds, a fault that doesn't settle in time, leaked containers, namespaces or memory, an upgrade on the wrong version, an agent panic or an unexplained restart, an agent loop stalled for over a second, a job that runs twice, goes missing or never drains, a council history that isn't linearizable, or job tasks slower than 256 ms to start at the 99th percentile. A failure gets a root cause before anything else happens. A product fix means a new candidate, and every soak starts again.

**The latest: 0.2.7, 7 November 2026.** One candidate in one day, on the full gate because it changes the agent loop, on the Mac Studio. It passed everything clean but one slow agent turn in the 8-hour soak, waiting on a slow disk write. We shipped with it as a known issue, and fix it in 0.2.8. [The release log](https://github.com/reliaburger/reliaburger/blob/main/docs/release-log.md) has every release’s story.

Show 0.2.7's results headline numbers and charts, each with its record

- [8-hour job soak](https://github.com/reliaburger/reliaburger/blob/main/docs/qualification/2026-11-07-v0.2.7-job-qualification.md): every fault and upgrade walk settled, no data lost, every job drained, agent memory within target, a linearizable council history, and a quiet tail with every leak check armed. 9,439 accepted jobs a second against the 1,390 target, and job tasks started within 32 ms at the 99th percentile, over 217 million of them. It failed as run on one agent turn of 1,215 ms against the 1 s budget, waiting for a slow journal write.
- [One-hour job soak](https://github.com/reliaburger/reliaburger/blob/main/docs/qualification/2026-11-07-v0.2.7-job-qualification-fast.md): 8,584 accepted jobs a second, and job tasks started within 32 ms at the 99th percentile. Clean.
- [One-hour app soak](https://github.com/reliaburger/reliaburger/blob/main/docs/qualification/2026-11-07-v0.2.7-sustained-v02-fast.md): every fault kind, special and upgrade walk on the compressed schedule. Clean.
- [Power cuts and reboots](https://github.com/reliaburger/reliaburger/blob/main/docs/qualification/2026-11-07-power-cut-reboot.md): 25 of 25 in each of five lanes.
- [Every gate and every candidate](https://github.com/reliaburger/reliaburger/blob/main/docs/qualification/2026-11-07-v0.2.7-release-closure.md): one candidate, and why we shipped it with that one turn.

Time runs from the start of each soak. The marks under a chart are its faults and upgrade walks, and the dashed lines its targets. Open a chart to see it full size, with every mark named when you point at it.

### [The 8-hour job soak](https://github.com/reliaburger/reliaburger/blob/main/docs/qualification/2026-11-07-v0.2.7-job-qualification.md)

[![Accepted jobs a second in 10-minute windows over the 8-hour job soak, against the target rate, with the faults marked along the time axis](https://reliaburger.com/assets/soak/2026-11-07-v0.2.7-job-qualification-charts/jobs.svg)](https://reliaburger.com/assets/soak/2026-11-07-v0.2.7-job-qualification-charts/jobs.svg)

Accepted jobs a second, in 10-minute windows, against the target.

[![Bun resident memory per node over the 8-hour job soak, against the 512 MiB target for node 1 and 384 MiB for the others, with the faults marked along the time axis](https://reliaburger.com/assets/soak/2026-11-07-v0.2.7-job-qualification-charts/rss.svg)](https://reliaburger.com/assets/soak/2026-11-07-v0.2.7-job-qualification-charts/rss.svg)

Bun memory per node, with the 512 and 384 MiB targets.

[![Open file descriptors per node over the 8-hour job soak, with the faults marked along the time axis](https://reliaburger.com/assets/soak/2026-11-07-v0.2.7-job-qualification-charts/fds.svg)](https://reliaburger.com/assets/soak/2026-11-07-v0.2.7-job-qualification-charts/fds.svg)

Open file descriptors per node.

[![Bun CPU per node over the 8-hour job soak, as a percentage of one core, with the faults marked along the time axis](https://reliaburger.com/assets/soak/2026-11-07-v0.2.7-job-qualification-charts/cpu.svg)](https://reliaburger.com/assets/soak/2026-11-07-v0.2.7-job-qualification-charts/cpu.svg)

Bun CPU per node, as a percentage of one core.

### [The one-hour app soak](https://github.com/reliaburger/reliaburger/blob/main/docs/qualification/2026-11-07-v0.2.7-sustained-v02-fast.md)

The gate has no separate 8-hour app soak: the job soak above runs the same apps and faults. These are from the compressed one.

[![Bun resident memory per node over the one-hour app soak, against the 512 MiB target for node 1 and 384 MiB for the others, with the faults marked along the time axis](https://reliaburger.com/assets/soak/2026-11-07-v0.2.7-sustained-v02-fast-charts/rss.svg)](https://reliaburger.com/assets/soak/2026-11-07-v0.2.7-sustained-v02-fast-charts/rss.svg)

Bun memory per node, with the 512 and 384 MiB targets.

[![Open file descriptors per node over the one-hour app soak, with the faults marked along the time axis](https://reliaburger.com/assets/soak/2026-11-07-v0.2.7-sustained-v02-fast-charts/fds.svg)](https://reliaburger.com/assets/soak/2026-11-07-v0.2.7-sustained-v02-fast-charts/fds.svg)

Open file descriptors per node.

[![Bun CPU per node over the one-hour app soak, as a percentage of one core, with the faults marked along the time axis](https://reliaburger.com/assets/soak/2026-11-07-v0.2.7-sustained-v02-fast-charts/cpu.svg)](https://reliaburger.com/assets/soak/2026-11-07-v0.2.7-sustained-v02-fast-charts/cpu.svg)

Bun CPU per node, as a percentage of one core.

What the soaks don't prove: they run three small VMs on one laptop, so they say nothing about large clusters, real networks between machines or hardware failing. A VM forced off isn't a real power loss, the fault rates are a schedule, not production traffic, and zero failures in 25 runs only rules out failure rates above about 12%. [How each gate works](https://github.com/reliaburger/reliaburger/blob/main/docs/testing.md#before-a-release), and [how it compares with Kubernetes' release testing](https://reliaburger.com/compare/kubernetes.html#testing).

## Netboot real machines

Old mini PCs, thin clients, anything x86\_64 or arm64 with UEFI network boot: plug them into your switch and power them on. Your laptop serves the OS, they install themselves in under a minute, and one command makes them a cluster. No USB sticks, no distro, nothing to configure on each machine. Your router keeps handing out the addresses; relish only answers the boot part.

Three machines with blank disks network-boot into Reliaburger and become one cluster. They’re QEMU VMs with UEFI firmware on a virtual switch, run by [scripts/demo/netboot.sh](https://github.com/reliaburger/reliaburger/blob/main/scripts/demo/netboot.sh) in CI, serving a lab build of the OS signed with that run’s own key. Idle time is cut to two seconds; every wait says how long it really took. speed 

Show the commands from blank disks to a cluster (0.3.0, not in 0.2.7)

1.  In each machine’s firmware setup, turn on UEFI network boot and turn Secure Boot off. A blank disk falls through to the network by itself.

2.  ```
    relish image download --dir os
    ```

    Fetches the newest OS release for every architecture into `os/`, checked against the release key `relish` carries.

3.  ```
    sudo relish netboot os --mac d8:9e:f3:12:34:56 --mac d8:9e:f3:12:34:57 --mac d8:9e:f3:12:34:58
    ```

    Serves the boot part, for the machines you list and no others: iPXE over TFTP, then the installer and the image over HTTP. Leave it running and power the machines on. Each one installs, reboots into Reliaburger and shows a claim key on its screen. A disk that isn’t blank waits for your yes here first.

4.  ```
    relish machines
    ```

    Lists the machines on your network that are waiting to be claimed: address, MAC and architecture.

5.  ```
    relish machines claim ~/lab --create --name lab --operator 192.168.1.10 d8:9e:f3:12:34:56 d8:9e:f3:12:34:57 d8:9e:f3:12:34:58
    ```

    Makes them one cluster. For each machine, it asks whether the claim key it got is the one on that machine’s screen, then asks you to back up the cluster’s master key. `--operator` is your laptop’s address.

6.  ```
    relish nodes
    ```

    Three nodes alive. From here it’s the same cluster as the tour’s: deploy with `relish apply`, and update the OS with `relish os upgrade`, one node at a time.

Serving from a Mac, an isolated switch, firmware quirks: [the appliance chapter of the manual](https://github.com/reliaburger/reliaburger/blob/main/docs/manual/15_appliance.md).

## Debug the platform, not just your app

When something breaks, the bug may be in your app or in the platform under it. Your AI agent can tell which: it reads the cluster, the manual and the exact source its `relish` was built from. When the bug is ours, it proves a fix on a throwaway cluster and drafts the report for you to send. It never fixes your cluster for you.

No recording yet. A real session, an agent with a read-only token on a real cluster with real build times, is being recorded for [#1579](https://github.com/reliaburger/reliaburger/issues/1579), and we won’t show a mock-up in its place. Until then, here are the steps it takes.

1.  ```
    relish token create --name agent --role read-only --ttl-days 1
    ```

    You make a read-only token and give it to the agent in `RELIABURGER_TOKEN`. The token’s role is what stops it changing anything, not its good manners.

2.  ```
    relish agent context
    ```
    ```
    relish agent diagnose
    ```

    The first calls: findings, faults, alerts, one line per app and the newest events in one bounded answer, then every finding with its evidence and the read-only commands to run next. Text a workload wrote arrives marked as untrusted.

3.  ```
    relish agent explain
    ```
    ```
    relish agent source grep classify_error
    ```

    What each finding means, its thresholds and where it’s decided, then the code itself, from the source this `relish` was built from rather than whatever is on GitHub today.

4.  ```
    relish agent patch init ./fix
    ```
    ```
    relish agent patch build ./fix
    ```
    ```
    relish agent patch test ./fix
    ```
    ```
    relish agent patch try ./fix
    ```

    If the bug is in Reliaburger: a workspace from that source, a build, the portable test suite, then a throwaway cluster of the patched binaries with its own `RELIABURGER_HOME`, never yours. The build takes minutes, and the binaries it makes stay unsigned.

Last, it drafts a redacted bug report with the reproducer, the patch and what it didn’t test, for you to read and post. Nothing goes to GitHub on its own.

[The agent’s own page](https://reliaburger.com/agents.html) · [The manual chapter](https://github.com/reliaburger/reliaburger/blob/main/docs/manual/16_working-with-an-ai-agent.md) · [How we test, for agents](https://reliaburger.com/agents.html#how-reliaburger-is-tested)

## What’s next

**0.3.0: Bare metal in an hour**

Next. An appliance OS, netbooting and claiming machines over the LAN, and signed A/B OS updates. The exit test: Dell Wyse thin clients from power-on to a cluster.

[Milestone](https://github.com/reliaburger/reliaburger/milestone/6)

**0.4.0: Full container migration**

Planned. Cordon, drain and uncordon, cold moves that carry volumes, and checkpoint moves that keep memory while connections reconnect.

[Milestone](https://github.com/reliaburger/reliaburger/milestone/7)

## Read the docs

Documentation lives with the code.

START HERE

### [User documentation](https://github.com/reliaburger/reliaburger/blob/main/docs/README.md)

Build requirements, runtimes, configuration, secure cluster setup and CLI commands.

DESIGN & INTENT

### [The whitepaper](https://github.com/reliaburger/reliaburger/blob/main/docs/whitepaper.md)

The architecture, its trade-offs and the longer-term vision. Includes features still planned.

IMPLEMENTATION

### [Component design docs](https://github.com/reliaburger/reliaburger/tree/main/docs/design)

Protocols, data structures, failure modes and decisions for each subsystem.

WALKTHROUGH

### [Building Reliaburger](https://github.com/reliaburger/reliaburger/tree/main/docs/book)

The companion book: build the orchestrator while learning Rust and distributed systems. For programmers coming from C, Python or Go.

REFERENCE

### [The manual & examples](https://github.com/reliaburger/reliaburger/tree/main/docs/manual)

The manual is also embedded in `relish manual`. Browse the [example configs](https://github.com/reliaburger/reliaburger/tree/main/examples) by phase and runtime.

CURRENT STATE

### [Roadmap & backlog](https://github.com/reliaburger/reliaburger/blob/main/docs/roadmap.md)

What’s implemented, what 0.2.7 leaves out and what’s next.

Every release carries PDFs, and these links always fetch the latest: [whitepaper](https://github.com/reliaburger/reliaburger/releases/latest/download/reliaburger-whitepaper.pdf), [design docs](https://github.com/reliaburger/reliaburger/releases/latest/download/reliaburger-design-docs.pdf), [the book](https://github.com/reliaburger/reliaburger/releases/latest/download/building-reliaburger.pdf) and [roadmap](https://github.com/reliaburger/reliaburger/releases/latest/download/reliaburger-roadmap.pdf). The Markdown on GitHub follows `main`, so it can be newer.

## Compare the alternatives

How Reliaburger compares, with commands and sources beside the claims. These pages were last re-checked on 21 October 2026, against 0.2.2; the policies, templates and sidecars 0.2.4 added aren’t in them yet.

THE PLATFORM API

### [Reliaburger vs Kubernetes](https://reliaburger.com/compare/kubernetes.html)

Pods, controllers, storage and extensibility; installation with kubeadm and the cost of operating the platform.

THE WORKLOAD SCHEDULER

### [Reliaburger vs Nomad](https://reliaburger.com/compare/nomad.html)

Jobs, groups, allocations and service discovery; what comes with the scheduler and what you integrate.

KUBERNETES, PACKAGED

### [Reliaburger vs k3s](https://reliaburger.com/compare/k3s.html)

What its bundled runtime, networking and ingress simplify, and which Kubernetes concepts remain.

THE SINGLE-HOST BASELINE

### [Reliaburger vs Docker Compose](https://reliaburger.com/compare/docker-compose.html)

Services, networks and volumes; where a single Docker host is enough and what changes when you need a cluster.

THE APPLICATION PLATFORM

### [Reliaburger vs Coolify](https://reliaburger.com/compare/coolify.html)

Git deployments, preview environments, domains and backups; the difference between deployment to servers and cluster reconciliation.

THE SMALL-CLUSTER ALTERNATIVE

### [Reliaburger vs Uncloud](https://reliaburger.com/compare/uncloud.html)

Compose, WireGuard and Caddy; straightforward deployment across machines and the limits of automatic recovery.

[All comparisons](https://reliaburger.com/compare/index.html) · [Evidence and comparison method](https://reliaburger.com/compare/methodology.html)

## Follow a subsystem

`bun` runs on each node. `relish` talks to its API. The names are burger ingredients; the links are the design documents.

[Bun / Grill](https://github.com/reliaburger/reliaburger/blob/main/docs/design/agent-bun.md)

Agent, container lifecycle, health checks

[Mustard / Council](https://github.com/reliaburger/reliaburger/blob/main/docs/design/gossip-mustard.md)

SWIM membership, Raft consensus

[Meat](https://github.com/reliaburger/reliaburger/blob/main/docs/design/scheduler-meat.md)

Placement, resources, scheduling

[Onion](https://github.com/reliaburger/reliaburger/blob/main/docs/design/discovery-onion.md)

DNS, service discovery, eBPF

[Wrapper](https://github.com/reliaburger/reliaburger/blob/main/docs/design/ingress-wrapper.md)

Ingress, TLS, connection draining

[Sesame](https://github.com/reliaburger/reliaburger/blob/main/docs/design/security-sesame.md)

PKI, mTLS, secrets, authorisation

[Pickle](https://github.com/reliaburger/reliaburger/blob/main/docs/design/registry-pickle.md)

OCI registry, image distribution

[Mayo](https://github.com/reliaburger/reliaburger/blob/main/docs/design/metrics-mayo.md) / [Ketchup](https://github.com/reliaburger/reliaburger/blob/main/docs/design/logs-ketchup.md)

Metrics, SQL queries, logs

[Lettuce](https://github.com/reliaburger/reliaburger/blob/main/docs/design/gitops-lettuce.md)

GitOps, reconciliation

[Deployments](https://github.com/reliaburger/reliaburger/blob/main/docs/design/deployments.md)

Rollouts, health gates, rollback

[Smoker](https://github.com/reliaburger/reliaburger/blob/main/docs/design/chaos-smoker.md)

Fault injection, recovery testing

[Relish](https://github.com/reliaburger/reliaburger/blob/main/docs/design/cli-relish.md) / [Brioche](https://github.com/reliaburger/reliaburger/blob/main/docs/design/ui-brioche.md)

CLI, terminal UI, web dashboard

## Build it from source

Hacking on Reliaburger itself? Start with a local process. No container runtime required.

You’ll need Rust and your platform’s build tools. This runs `hello` as a host process on one node: no images, no isolated container, only the binaries the node config allows.

For real OCI containers, use runc on Linux. On macOS, the managed laptop quickstart uses Linux VMs. Direct Apple Container is deferred beyond 0.2.7.

[Build requirements & runtime setup](https://github.com/reliaburger/reliaburger/blob/main/docs/README.md)

LOCAL DEVELOPMENT sh

```
# Terminal 1 · build and start the agent
git clone https://github.com/reliaburger/reliaburger
cd reliaburger
cargo build --bins
target/debug/bun --runtime process \
  --config examples/node/process-runtime.toml

# Terminal 2 · from the same checkout
target/debug/relish apply \
  examples/phase-1/proc-first-run.toml
target/debug/relish status
target/debug/relish
```

Web dashboard: `http://127.0.0.1:9117/`

CODE, TESTS, DOCUMENTATION

## Find something that doesn’t work?

Open an issue with your OS, runtime, config and the steps to reproduce it. Implementation changes come with tests and a chapter in the book.

[GitHub issues](https://github.com/reliaburger/reliaburger/issues)

---

The Markdown copy of <https://reliaburger.com/>.
