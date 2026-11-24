# Changelog

What changed in each Reliaburger release, newest first, and what you have to do to move onto it. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/): each release starts with **Upgrading**, then what it added, changed, fixed and removed, and the known issues it shipped with.

Until 1.0.0 every release is a development release. We keep no backwards compatibility: a release that changes the wire or on-disk format needs a fresh cluster, with no migration ([compatibility before 1.0.0](docs/releasing.md#compatibility-before-100)). So read a release's Upgrading section before you install it.

This file is the short version. The longer records are:

- the [roadmap](docs/roadmap.md#releases-after-010), for everything each release contains and what comes next;
- the [release log](docs/release-log.md), for how each release was qualified and what the soaks found;
- [docs/releasing.md](docs/releasing.md#compatibility-before-100), for the full upgrade steps between each pair of releases;
- the [GitHub release pages](https://github.com/reliaburger/reliaburger/releases), for the binaries and every merged pull request.

## [Unreleased] - 0.3.3

Not released yet: this is what the train carries, and it can change before the tag.

### Added

- `relish os boot-logs` shows where each appliance sends its kernel and boot logs, and `--to <address>[:<port>]` or `--off` changes it on a claimed cluster, so turning them on or off no longer means editing a file on the node's EFI partition. It needs a cluster-wide admin token and is audited with the target ([#2149](https://github.com/reliaburger/reliaburger/issues/2149)).
- `relish apps NAME` lists each instance's sidecars under it, with their state (`running`, `backoff`, `restarting` or `stopped`) and how often each has restarted on its own, and `-o json` carries them on each instance as `sidecars`. They used to show up only as Restart events ([#1315](https://github.com/reliaburger/reliaburger/issues/1315)).
- `relish apply --wait` returns once every app in the file runs its replicas, passes its health checks and has finished rolling out, printing each step (placed, pulling, waiting for the health check, ready). `--timeout` bounds it (5 minutes by default); when it runs out, `apply` names each app that isn't ready and why, and exits 1, while a failed apply exits 3, 4 or 5. With `-o json` it answers in the `relish.agent/v1` envelope. A daemon set or an autoscaled app counts the replicas the scheduler placed ([#2240](https://github.com/reliaburger/reliaburger/issues/2240)).

### Changed

- The batch-jobs manual opens its host commands section with what a Linux node needs first: mount isolation is on by default, including on nodes `relish setup --ssh` installs, and no host workload runs under it yet, so set `mount_isolation = false` and allowlist the binary, or give the job an image. Running host workloads under mount isolation stays open as [#714](https://github.com/reliaburger/reliaburger/issues/714).
- The appliance image has Buildah, so `relish build` and the five-minute tour's burger build on a cluster of appliances. Its storage is on the data partition, where Btrfs reflinks the `vfs` driver's layer copies; the image adds `netavark` and `gpg` for it, drops Buildah's 97 MB of test helpers, and grows each `/usr` slot by 54 MB (667 MB to 721 MB of 1,100 MB on x86-64). A multi-platform build can't emulate the other architecture's `RUN` steps on an appliance, so cross-compile or name one `platform`. With no node able to build, the refusal no longer says appliances never can ([#2144](https://github.com/reliaburger/reliaburger/issues/2144)).
- Policy expressions nested more than 32 deep are refused before they're parsed, and `relish apply` checks the admin permission for policy tables before compiling them.

### Fixed

- A node checks an app's runtime knobs and host-path sources again each time one of its instances restarts. A restart builds the container from what the deploy approved, so a node restarted with a shorter `[runtime]` or `[storage] allowed_host_paths` list in `node.toml` brought crashed containers back with what it no longer allowed. Now the restart is refused, the instance is `Failed` with the refusal as its reason in `relish apps` and `relish wtf`, and it isn't retried. A `make test-linux` test starts a real runc container with a capability, a network and an IPC sysctl and `/dev/fuse`, and checks each from inside it ([#1316](https://github.com/reliaburger/reliaburger/issues/1316)).
- A Bun restart or self-upgrade adopts running sidecars with their app instead of restarting them. Sidecars had no adoption records, so every Bun restart killed them and started them again about a second later. Each now has a record of its own, checked against the runtime like an app's ([#1315](https://github.com/reliaburger/reliaburger/issues/1315)).

## [0.3.2] - 2026-11-23

Fixes from the Wyse 3040 lab that need a new appliance image. [Release page](https://github.com/reliaburger/reliaburger/releases/tag/v0.3.2), [release log](docs/release-log.md#032-wyse-lab-fixes).

### Upgrading

- 0.3.2 moves the wire protocol from 60 to 61, because an apply error now carries its real status instead of 500 ([#2157](https://github.com/reliaburger/reliaburger/issues/2157)). Start a fresh 0.3.2 cluster: 0.3.1 and 0.3.2 nodes refuse each other, so there's no rolling upgrade ([compatibility before 1.0.0](docs/releasing.md#compatibility-before-100)).

### Added

- `relish netboot --stream-logs` shows an appliance's kernel and boot logs as it installs and boots, so a machine with no serial port can be watched from the terminal, and `relish machines logs` listens for them on its own. Off by default, because the logs cross the LAN in cleartext ([#2149](https://github.com/reliaburger/reliaburger/issues/2149)).
- `relish tui` has a node page showing what runs on a node and what maintenance would affect ([#2170](https://github.com/reliaburger/reliaburger/issues/2170)), and an app can be scaled from the terminal UI and from `relish` ([#2176](https://github.com/reliaburger/reliaburger/issues/2176)).

### Fixed

- Containers on a node where nothing publishes a port can reach services on other nodes. The node's masquerade rule was created only with the first published port, so a node running only workers or jobs sent their calls out with container source addresses, and they timed out. Network setup now creates it for every container ([#2219](https://github.com/reliaburger/reliaburger/issues/2219)).
- `relish test`'s firewall cases test what `allow_from` does: callers from another namespace, named as `namespace/app`. They put every app in one namespace, where `allow_from` doesn't apply, so on a cluster with eBPF they failed although the firewall worked. A fourth case checks that callers in the target's own namespace aren't restricted, and the case that widens `allow_from` waits until the target's rollout brings the new rule ([#2206](https://github.com/reliaburger/reliaburger/issues/2206)).
- `allow_from` admits listed callers on every node, not only on the node running the target. Each caller's node enforces the list, but only the target's own node knew it, so a listed caller anywhere else was refused (`Operation not permitted`) for as long as it ran. The leader now puts each service's `allow_from` in the service catalogue, which every node fetches every 2 s, and `relish test` has a case with the callers on another node ([#2216](https://github.com/reliaburger/reliaburger/issues/2216)).
- `relish wtf` calls a crash loop only when one instance restarts three times in 15 minutes. Three replicas that restarted once each were a critical `crashloop` for 15 minutes, and the instance a restart named was never read ([#2197](https://github.com/reliaburger/reliaburger/issues/2197)).
- `relish nodes` no longer reports a healthy node as not answering every so often: the alert check releases the metrics store's lock before its query, so the node summary doesn't wait seconds behind it ([#2215](https://github.com/reliaburger/reliaburger/issues/2215)).
- `url` 2.5.9, for RUSTSEC-2026-0592 ([#2213](https://github.com/reliaburger/reliaburger/issues/2213)).

### Known issues

- [#2128](https://github.com/reliaburger/reliaburger/issues/2128): one registry deploy case stalled for five minutes after an injected disk fault in 0.3.0's gate. It didn't recur in 0.3.1's or 0.3.2's gates; 0.3.2 logs what the stalled helper is doing if it happens again, and the cause isn't proven.

## [0.3.1] - 2026-11-21

Fixes from 0.3.0's gate and the first Wyse lab, and a terminal UI you can act from. [Release page](https://github.com/reliaburger/reliaburger/releases/tag/v0.3.1), [release log](docs/release-log.md#031-gate-follow-ups).

### Upgrading

- 0.3.1 keeps 0.3.0's formats (protocol 60, state 76), so a 0.3.0 cluster rolls onto it node by node.
- `relish wtf` prints a compact table by default; `--short` gives only the counts and what isn't OK, and `--explain` says what each check tests. `-o json` is unchanged ([#2180](https://github.com/reliaburger/reliaburger/issues/2180)).
- The built-in `cpu_idle` alert is gone: it fired on idle nodes and marked a healthy cluster DEGRADED. Four node alerts remain, all for high load ([#2171](https://github.com/reliaburger/reliaburger/issues/2171)).

### Added

- `relish tui`: `/` searches the Logs and Metrics tabs and the logs view, highlighting what matches ([#2174](https://github.com/reliaburger/reliaburger/issues/2174)); `k` kills a selected instance after a y/N question, through Smoker's audited kill fault ([#2175](https://github.com/reliaburger/reliaburger/issues/2175)); the nodes view has `relish nodes`' VERSION, CPU, MEM, DISK and ALLOCATED columns ([#2168](https://github.com/reliaburger/reliaburger/issues/2168)); the app detail view shows each instance's image ([#2167](https://github.com/reliaburger/reliaburger/issues/2167)); and Left and Right switch its tabs ([#2169](https://github.com/reliaburger/reliaburger/issues/2169)).
- `relish netboot` puts the local time on every log line ([#2159](https://github.com/reliaburger/reliaburger/issues/2159)).

### Fixed

- `relish apply` says an image isn't in the cluster's registry and how to build or push it, instead of blaming Docker Hub ([#2145](https://github.com/reliaburger/reliaburger/issues/2145)).
- `relish machines claim` waits until the claimed nodes answer before it returns ([#2146](https://github.com/reliaburger/reliaburger/issues/2146)), and a relish context whose files are gone is named in the error and replaced by a new cluster ([#2162](https://github.com/reliaburger/reliaburger/issues/2162)).
- `relish wtf` no longer calls Pickle degraded on a one-node cluster ([#2147](https://github.com/reliaburger/reliaburger/issues/2147)), and `relish nodes` keeps listing a node that shut down cleanly as `left` ([#2164](https://github.com/reliaburger/reliaburger/issues/2164)).
- Node shutdown runs each app's `pre_stop` and stop grace ([#2118](https://github.com/reliaburger/reliaburger/issues/2118)); an OS rollout keeps host-path apps with their data and shows its cordons ([#2119](https://github.com/reliaburger/reliaburger/issues/2119), [#2120](https://github.com/reliaburger/reliaburger/issues/2120)).
- Bun collects a state read that outlived its 1 s sweep instead of queuing a new one behind it, so a crashed container is still seen and restarted on a slow disk ([#2085](https://github.com/reliaburger/reliaburger/issues/2085)).
- Smoker's pause on a runc app freezes the workload, not its launcher ([#2079](https://github.com/reliaburger/reliaburger/issues/2079)).
- An ingress Host header is routed only when its port is digits, and `relish netboot` escapes a TFTP client's error text in its log.
- An exec into a workload has its own deadline inside the client's, so a hung one fails naming the app and the node.

### Known issues

- [#2197](https://github.com/reliaburger/reliaburger/issues/2197): `relish wtf` counts a crash loop per app, so three replicas that each restarted once show as a critical `crashloop` for 15 minutes. Fixed on the 0.3.2 train.
- [#2128](https://github.com/reliaburger/reliaburger/issues/2128): one registry deploy case stalled for five minutes after an injected disk fault in 0.3.0's gate. Two possible causes are fixed in this release, but the cause isn't proven.

## [0.3.0] - 2026-11-20

"Bare metal in an hour": blank machines boot our appliance OS over the network, join a cluster when you claim them, and take signed OS updates one node at a time. [Release page](https://github.com/reliaburger/reliaburger/releases/tag/v0.3.0), [release log](docs/release-log.md#030-bare-metal-in-an-hour).

### Upgrading

- A 0.2.8 cluster can't roll onto 0.3.0: start a fresh cluster. The protocol moves from 58 to 60 and the state format from 74 to 76 ([upgrading from 0.2.8](docs/releasing.md#upgrading-from-028)).
- The root CA is offline. `relish init` writes the root's private key only into the backup you name with `--root-backup`, and without a terminal it needs `--passphrase-file` or `--recipient` too ([#709](https://github.com/reliaburger/reliaburger/issues/709)).
- `relish ca backup` now only re-seals such a backup (`--from`). Keep the backup off the cluster: it's the only copy of the root's key ([#709](https://github.com/reliaburger/reliaburger/issues/709)).

- Appliance clusters start with five council voters, and spare nodes join as Raft learners up to the council cap ([#496](https://github.com/reliaburger/reliaburger/pull/496), [#1469](https://github.com/reliaburger/reliaburger/pull/1469)).
- A memory request no longer sets `memory.high`, and runc refuses a memory limit below runc init's footprint (about 16 MiB on runc 1.4) with a message saying why ([#1955](https://github.com/reliaburger/reliaburger/issues/1955), [#1959](https://github.com/reliaburger/reliaburger/issues/1959)).

### Added

- The appliance OS: our own image on Ubuntu 26.04 with no SSH server, built weekly, signed, and published on an OS channel ([#401](https://github.com/reliaburger/reliaburger/issues/401)). Its A/B updates boot the new version on trial and fall back to the old one by themselves if it doesn't come up ([#406](https://github.com/reliaburger/reliaburger/issues/406)).
- `relish os` rolls an OS version across the cluster one node at a time, workers first and the leader last, moving each node's workloads off before it reboots. `relish os status`, `resume` and `abort` follow and steer it ([#406](https://github.com/reliaburger/reliaburger/issues/406)).
- `relish image download`, `write` and `seed`, and `relish cluster create --bare-metal`, which makes a cluster from blank appliances and confirms the root CA backup before it keeps anything ([#403](https://github.com/reliaburger/reliaburger/issues/403)).
- `relish netboot` installs appliances over the network: ProxyDHCP, TFTP and HTTP from a laptop, macOS included, asking before it wipes a disk that's in use. `--reinstall` reinstalls a machine that already runs Reliaburger ([#404](https://github.com/reliaburger/reliaburger/issues/404)).
- `relish machines` lists unclaimed appliances on the LAN, and `relish machines claim` compares each one's claim key with its console, then sends it a seed ([#405](https://github.com/reliaburger/reliaburger/issues/405)).
- Bun's appliance seed mode (`bun --appliance`) ([#402](https://github.com/reliaburger/reliaburger/issues/402)).
- `relish join-token list` and `revoke` ([#1918](https://github.com/reliaburger/reliaburger/pull/1918), [#1977](https://github.com/reliaburger/reliaburger/pull/1977)).
- JSON and YAML output for every appliance command, and `-o json` everywhere else too ([#1709](https://github.com/reliaburger/reliaburger/issues/1709), [#2025](https://github.com/reliaburger/reliaburger/pull/2025)).
- OIDC signing-key rotation, with an overlap window for workload tokens ([#709](https://github.com/reliaburger/reliaburger/issues/709)).
- Ingress: a route's TLS mode picks its certificate per SNI host, and a drained WebSocket closes with 1001 and carries the HTTP proxy's headers ([#2028](https://github.com/reliaburger/reliaburger/pull/2028), [#369](https://github.com/reliaburger/reliaburger/issues/369)).
- `relish wtf` reports the council's backups, and the last usable admin token can't be revoked ([#2030](https://github.com/reliaburger/reliaburger/pull/2030)).
- The soak gates its API read, API write, rolling-update and app-start SLOs, and checks that every task-array chunk completes exactly once ([#1405](https://github.com/reliaburger/reliaburger/issues/1405), [#1415](https://github.com/reliaburger/reliaburger/issues/1415)).

### Changed

- The quickstart guest moves to Ubuntu 26.04.
- `relish status` exits 3 when the cluster doesn't answer ([#2025](https://github.com/reliaburger/reliaburger/pull/2025)).
- The API takes tokens issued for its own audience only ([#2022](https://github.com/reliaburger/reliaburger/pull/2022)).
- Published ports are held to the cluster's perimeter, and the port map covers local destinations only ([#2026](https://github.com/reliaburger/reliaburger/pull/2026)).
- On boot, Bun waits up to 30 s for the clock before it starts ([#2037](https://github.com/reliaburger/reliaburger/pull/2037)).

### Fixed

- A disk-pressured voter on a full council is replaced, and a replaced voter coming back no longer forces an election ([#1523](https://github.com/reliaburger/reliaburger/issues/1523), [#1524](https://github.com/reliaburger/reliaburger/issues/1524)).
- A stopping Bun no longer launches new job attempts ([#1965](https://github.com/reliaburger/reliaburger/issues/1965)).
- Shared-runc executors start on Ubuntu 26.04: runc 1.4's init was OOM-killed in the helper's 8 MiB ([#1957](https://github.com/reliaburger/reliaburger/issues/1957)).
- A half-written instance record from a power cut no longer fails the node's startup ([#1852](https://github.com/reliaburger/reliaburger/issues/1852)).
- Secret providers are asked again after a brief outage, and only typed references are resolved ([#1308](https://github.com/reliaburger/reliaburger/issues/1308)).
- A join seed sent again gets a fresh join token ([#2002](https://github.com/reliaburger/reliaburger/issues/2002)).
- About 60 fixes from the 16 and 17 November codebase review: council and Raft durability, gossip freshness, council membership on the Raft port, leader forwards with deadlines, layer whiteouts and volume paths resolved by descriptor, ingress HTTP/2 authority, and relish's handling of untrusted text. Each is in the [release's pull requests](https://github.com/reliaburger/reliaburger/pull/490).

### Known issues

- The Raft snapshot, `snapshot.redb`, is stored in plaintext. Only the Raft log is encrypted at rest ([#1775](https://github.com/reliaburger/reliaburger/issues/1775)).
- Node shutdown, which every OS update goes through, skips each app's `pre_stop` hook and gives it 5 s instead of its `stop_grace` ([#2118](https://github.com/reliaburger/reliaburger/issues/2118)).
- An OS rollout moves apps that keep data in a host path away from it, and `relish status` and `relish nodes` don't show its cordons ([#2120](https://github.com/reliaburger/reliaburger/issues/2120), [#2119](https://github.com/reliaburger/reliaburger/issues/2119)).
- A Smoker `pause` of a runc app stops its launcher, not the app ([#2079](https://github.com/reliaburger/reliaburger/issues/2079)).
- In the release gate's 8-hour final, one catalogue deploy from the cluster registry stalled for its whole 5-minute deadline right after an injected disk fault, then recovered by itself with no data lost. The cause isn't verified yet ([#2128](https://github.com/reliaburger/reliaburger/issues/2128)).

## [0.2.8] - 2026-11-12

"Agents you can trust, built faster": an agent proposes a change, and a person or a policy approves it and can undo it. [Release page](https://github.com/reliaburger/reliaburger/releases/tag/v0.2.8), [release log](docs/release-log.md#028-agents-you-can-trust-built-faster).

### Upgrading

- A 0.2.7 cluster can't roll onto 0.2.8: start a fresh cluster. The protocol moved from 57 to 58 and the state format from 73 to 74 ([upgrading from 0.2.7](docs/releasing.md#upgrading-from-027)).
- `relish status -o json` is now the cluster summary, not a list of instances. A script that read the list should use `relish apps --instances -o json` ([#1788](https://github.com/reliaburger/reliaburger/issues/1788)).
- `relish agent context` has a `data.status` section instead of `data.faults`, `data.alerts` and `data.apps`.
- `relish agent guide`'s default budget is 80 KiB and `relish agent schema`'s is 40 KiB; every other agent command keeps 24 KiB.

### Added

- Proposals: `relish agent propose` asks for an `apply`, `scale`, `rollback`, `stop` or `delete`, the cluster works out the diff, the blast radius and the undo command, and `relish proposal approve` applies it. `relish undo` puts an applied proposal back for 24 hours ([#1682](https://github.com/reliaburger/reliaburger/issues/1682)).
- Agent tokens: `relish token create --agent` makes a token that can propose changes but can't make any directly.
- Policies with `applies_to = "proposals"` can refuse a proposal or approve one without a person, and `relish policy test --proposal` tries them offline.
- Leases: an agent takes a short lease on an app or a namespace, other changes are refused as `lease_held`, and `--break-lease` takes one over. `relish lease` lists, takes, renews, releases and breaks them ([#1686](https://github.com/reliaburger/reliaburger/issues/1686)).
- `relish mcp` makes and renews a read-only token of its own from an admin context, and `relish mcp --propose` adds the proposing and lease tools ([#1730](https://github.com/reliaburger/reliaburger/issues/1730)).
- `relish verify` checks every node's `bun` and your `relish` against the published release, byte for byte, and works on an air-gapped cluster with `--metadata` and `--artefacts` ([#1689](https://github.com/reliaburger/reliaburger/issues/1689)).
- `relish timeline` merges events, the audit log and deploys over a window, and `relish agent explain --placement`, `--restart` and `--refusal` say which rule decided ([#1685](https://github.com/reliaburger/reliaburger/issues/1685)).
- `relish apps` lists apps, problems first, and `relish nodes` gains version, usage and allocation ([#1777](https://github.com/reliaburger/reliaburger/issues/1777)).
- `relish status --check` sets the exit code from the verdict ([#1777](https://github.com/reliaburger/reliaburger/issues/1777)).
- `relish gym run` tests your own agent against known faults in a staging namespace, scores it and cleans up ([#1683](https://github.com/reliaburger/reliaburger/issues/1683)).
- `relish agent patch apply` applies a unified diff to a patch workspace, all or nothing ([#1691](https://github.com/reliaburger/reliaburger/issues/1691)).
- Every agent envelope and `candidate.json` carry the source digest of the build ([#1694](https://github.com/reliaburger/reliaburger/issues/1694)).
- Every release carries a signed `.mcpb` bundle of `relish mcp` per platform ([#1695](https://github.com/reliaburger/reliaburger/issues/1695)).

### Changed

- `relish status` fits the whole cluster on one screen: a verdict (`HEALTHY`, `DEGRADED` or `DOWN`), then one line per part of the cluster. The instance list moved to `relish apps --instances` ([#1777](https://github.com/reliaburger/reliaburger/issues/1777), [#1788](https://github.com/reliaburger/reliaburger/issues/1788)).

### Fixed

- After a SIGKILL during a rollout and then a power cut, a node could exit at every start and never rejoin ([#1849](https://github.com/reliaburger/reliaburger/issues/1849)).
- An agent-loop turn could wait on an fsync of an instance record, or on a blocking file seek in the log reader ([#1853](https://github.com/reliaburger/reliaburger/issues/1853), [#1857](https://github.com/reliaburger/reliaburger/issues/1857)).
- One slow inventory read on a busy node could drop its backends from discovery for a few seconds, so ingress answered 502 ([#1855](https://github.com/reliaburger/reliaburger/issues/1855)).
- A `council_partition` fault cuts Raft in both directions, and healing removes exactly the addresses it blocked ([#1810](https://github.com/reliaburger/reliaburger/issues/1810), [#1820](https://github.com/reliaburger/reliaburger/issues/1820)).
- A new or revoked API token takes effect on every node at once, not after up to 5 s on followers ([#1809](https://github.com/reliaburger/reliaburger/issues/1809)).
- A decrypted secret whose plaintext starts with `PROVIDER[` is no longer sent to a secret provider ([#1819](https://github.com/reliaburger/reliaburger/issues/1819)).
- An app that sets only a short health `interval` gets a matching default timeout instead of a refusal.
- A Smoker `pause` of a process-runtime instance on macOS no longer leaves it `runtime_unknown` for good ([#1772](https://github.com/reliaburger/reliaburger/issues/1772)).
- `brew install reliaburger/tap/relish` works on older Command Line Tools: the formula has bottles ([#1712](https://github.com/reliaburger/reliaburger/issues/1712)).
- Redacting a report twice can no longer expose URL credentials the first pass had hidden ([#1717](https://github.com/reliaburger/reliaburger/issues/1717)).
- `rustix` 1.1.6, for RUSTSEC-2026-0486 ([#1862](https://github.com/reliaburger/reliaburger/issues/1862)).

### Known issues

- Bun's memory grows for the whole of a 24-hour run at 20,000 jobs a second ([#1417](https://github.com/reliaburger/reliaburger/issues/1417)). A later run on 0.2.8 found it's glibc keeping freed memory after log queries, and it levels off.
- The slow agent-loop turn from 0.2.7's final isn't proven fixed ([#1707](https://github.com/reliaburger/reliaburger/issues/1707)).
- `relish mcp` isn't in the MCP Registry yet, and MCP clients show the bundle as unsigned ([#1695](https://github.com/reliaburger/reliaburger/issues/1695), [#1838](https://github.com/reliaburger/reliaburger/issues/1838)).
- The macOS `relish` isn't notarised, so macOS may ask before running it the first time.
- It ships with the `zstd` 0.13 advisories under a reviewed exception ([#1366](https://github.com/reliaburger/reliaburger/issues/1366)).

## [0.2.7] - 2026-11-07

"Built for humans and their agents": a read-only, JSON-only way for an AI agent to read your cluster, with no LLM inside `bun`. [Release page](https://github.com/reliaburger/reliaburger/releases/tag/v0.2.7), [release log](docs/release-log.md#027-built-for-humans-and-their-agents).

### Upgrading

- A 0.2.6 cluster can roll onto 0.2.7 in place with `relish upgrade start v0.2.7`: it keeps protocol 57 and state format 73 ([upgrading from 0.2.6](docs/releasing.md#upgrading-from-026)).
- Nobody had walked a cluster from the published 0.2.6 to the published 0.2.7 when it shipped. If the walk fails, a fresh cluster still works.

### Added

- `relish agent`, a command family that answers an agent in one JSON envelope within a byte budget ([#1565](https://github.com/reliaburger/reliaburger/issues/1565)).
- `relish agent context` and `diagnose` read the cluster; `manual` and `source` read the manual and the exact source the binary was built from ([#1570](https://github.com/reliaburger/reliaburger/issues/1570), [#1571](https://github.com/reliaburger/reliaburger/issues/1571)).
- `relish agent patch` builds and tries a fix on a throwaway cluster ([#1574](https://github.com/reliaburger/reliaburger/issues/1574)).
- `relish agent report` drafts a redacted bug report for a person to post ([#1575](https://github.com/reliaburger/reliaburger/issues/1575)).
- `relish mcp` serves the same commands as read-only MCP tools ([#1094](https://github.com/reliaburger/reliaburger/issues/1094)), and `relish agent setup` wires them into the agents on a machine ([#1581](https://github.com/reliaburger/reliaburger/issues/1581)).
- Every command takes `-o json` ([#1566](https://github.com/reliaburger/reliaburger/issues/1566)).
- Every release file carries a build provenance attestation ([#1583](https://github.com/reliaburger/reliaburger/issues/1583)).

### Known issues

- One journal turn in the 8-hour final took 1,215 ms against the 1 s budget ([#1707](https://github.com/reliaburger/reliaburger/issues/1707)).
- It ships with the `zstd` 0.13 advisories under a reviewed exception ([#1366](https://github.com/reliaburger/reliaburger/issues/1366)).

## [0.2.6] - 2026-11-06

The rest of the evaluation's bugs, and setup over SSH. [Release page](https://github.com/reliaburger/reliaburger/releases/tag/v0.2.6), [release log](docs/release-log.md#026-the-rest-of-the-evaluations-bugs-and-setup-over-ssh).

### Upgrading

- A 0.2.5 cluster can't roll onto 0.2.6: start a fresh cluster. The protocol moved from 56 to 57; the state format stays at 73 ([upgrading from 0.2.5](docs/releasing.md#upgrading-from-025)).

### Added

- `relish setup --ssh` builds a cluster on Linux machines you already have, past the seven-seat council cap too ([#1529](https://github.com/reliaburger/reliaburger/issues/1529)).
- Node certificates name the node's address as an IP SAN, so a strict client can connect by address ([#1508](https://github.com/reliaburger/reliaburger/issues/1508)).

### Fixed

- A secure cluster's eighth node no longer restarts for ever: a node with no Raft seat reads the leader's view of the security state ([#1477](https://github.com/reliaburger/reliaburger/issues/1477)).
- Bun with eBPF starts on Linux 7.0 ([#1557](https://github.com/reliaburger/reliaburger/issues/1557)).
- `relish logs --since` stays inside its window for a quiet app ([#1515](https://github.com/reliaburger/reliaburger/issues/1515)).

### Known issues

- A network partition fault can be refused when its source's cgroup lookup is slow ([#1601](https://github.com/reliaburger/reliaburger/issues/1601)).
- One compaction turn in the 8-hour final took 1,232 ms against the 1 s budget ([#1606](https://github.com/reliaburger/reliaburger/issues/1606)).
- It ships with the `zstd` 0.13 advisories under a reviewed exception ([#1366](https://github.com/reliaburger/reliaburger/issues/1366)).

## [0.2.5] - 2026-11-03

Bugs an outside evaluation found: a patch release for product bugs an external evaluation of 0.2.0 ([#1080](https://github.com/reliaburger/reliaburger/issues/1080)) found. [Release page](https://github.com/reliaburger/reliaburger/releases/tag/v0.2.5), [release log](docs/release-log.md#025-bugs-an-outside-evaluation-found).

### Upgrading

- A 0.2.4 cluster can't roll onto 0.2.5: start a fresh cluster. The protocol moved from 55 to 56 and the state format from 72 to 73 ([upgrading from 0.2.4](docs/releasing.md#upgrading-from-024)).
- `relish logs-export --dest DIR` now writes where `relish` runs, not on the node ([#1499](https://github.com/reliaburger/reliaburger/issues/1499)).

### Changed

- Spare nodes join as Raft learners up to the council cap ([#1494](https://github.com/reliaburger/reliaburger/issues/1494)).

### Fixed

- `relish token create` and the other token commands work on any node, and a new token works at once ([#1496](https://github.com/reliaburger/reliaburger/issues/1496)).
- Issued certificates name their issuer's key, so strict verifiers accept the API ([#1497](https://github.com/reliaburger/reliaburger/issues/1497)).
- The Kubernetes importer keeps an init container's `env` ([#1498](https://github.com/reliaburger/reliaburger/issues/1498)).
- An app can be deleted and applied straight back ([#1500](https://github.com/reliaburger/reliaburger/issues/1500), fixed in part).
- The `relish audit` table names every app an apply touched ([#1501](https://github.com/reliaburger/reliaburger/issues/1501)).
- `relish logs --tail` stays current once an app has logged for a while ([#1493](https://github.com/reliaburger/reliaburger/issues/1493)).

### Known issues

- Delete then re-apply wasn't reproduced on real multi-node runc, so [#1500](https://github.com/reliaburger/reliaburger/issues/1500) stayed open into 0.2.6.
- Secure clusters with more nodes than the council cap still restart the extra nodes ([#1477](https://github.com/reliaburger/reliaburger/issues/1477), fixed in 0.2.6).
- It ships with the `zstd` 0.13 advisories under a reviewed exception ([#1366](https://github.com/reliaburger/reliaburger/issues/1366)), and with RUSTSEC-2026-0433 in `pulldown-cmark`, which it doesn't expose.

## [0.2.4] - 2026-10-30

Extension points: ways to bend Reliaburger without forking it. [Release page](https://github.com/reliaburger/reliaburger/releases/tag/v0.2.4), [release log](docs/release-log.md#024-extension-points).

### Upgrading

- A 0.2.3 cluster can't roll onto 0.2.4: start a fresh cluster. The protocol moved from 53 to 55 and the state format from 70 to 72 ([upgrading from 0.2.3](docs/releasing.md#upgrading-from-023)).

### Added

- `[policy.*]` rules in CEL that deny, warn or audit, with fill-only defaults and `relish policy audit`, on the one apply path that `relish apply` and GitOps share ([#1302](https://github.com/reliaburger/reliaburger/issues/1302)).
- `[template.*]` and `[use.*]`: templates that expand to ordinary apps ([#1302](https://github.com/reliaburger/reliaburger/issues/1302)).
- Sidecars, stop hooks and gated runtime knobs ([#1302](https://github.com/reliaburger/reliaburger/issues/1302)).
- An exec secret provider ([#1302](https://github.com/reliaburger/reliaburger/issues/1302)).
- A contract for running your own controller ([#1302](https://github.com/reliaburger/reliaburger/issues/1302)).

### Fixed

- A restarting node keeps its replicas instead of the leader moving them ([#1449](https://github.com/reliaburger/reliaburger/issues/1449)).
- The slow agent-loop turn 0.2.3's day run found ([#1419](https://github.com/reliaburger/reliaburger/issues/1419)).
- Capture files the log store has already saved are reclaimed, so they no longer fill a node's disk ([#1418](https://github.com/reliaburger/reliaburger/issues/1418), [#1479](https://github.com/reliaburger/reliaburger/issues/1479)).

### Known issues

- It ships with the `zstd` 0.13 advisories under a reviewed exception ([#1366](https://github.com/reliaburger/reliaburger/issues/1366)).
- `relish logs --tail` can keep returning the same old lines once an app has logged for a while ([#1493](https://github.com/reliaburger/reliaburger/issues/1493), fixed in 0.2.5).

## [0.2.3] - 2026-10-28

Two billion a day: one node runs 14,917 host-job tasks a second and three run 29,350, and a real 24-hour day on three nodes counted 1.81 billion. [Release page](https://github.com/reliaburger/reliaburger/releases/tag/v0.2.3), [release log](docs/release-log.md#023-two-billion-a-day).

### Upgrading

- A 0.2.2 cluster can roll onto 0.2.3 in place with `relish upgrade start v0.2.3`: it keeps protocol 53 and state format 70, the first release to keep its predecessor's formats ([upgrading from 0.2.2](docs/releasing.md#upgrading-from-022)).
- Expect your apps to restart, and sometimes move, once per node during the walk ([#1449](https://github.com/reliaburger/reliaburger/issues/1449), fixed in 0.2.4).

### Changed

- Host jobs run faster: 14,917 tasks a second on one node and 29,350 on three ([#1030](https://github.com/reliaburger/reliaburger/issues/1030), [#1031](https://github.com/reliaburger/reliaburger/issues/1031)).
- `relish bench` reports per-node successes and fails a node more than 20% over its fair share ([#1362](https://github.com/reliaburger/reliaburger/issues/1362)).

### Fixed

- The two disk writes that could hold a node's agent loop for seconds in 0.2.2 moved off the loop ([#1263](https://github.com/reliaburger/reliaburger/issues/1263), [#1340](https://github.com/reliaburger/reliaburger/issues/1340)).
- `age` 0.11.5 drops the dependency behind RUSTSEC-2026-0387 ([#1333](https://github.com/reliaburger/reliaburger/issues/1333)).

### Known issues

- A restarting node's replicas are replaced, and sometimes moved ([#1449](https://github.com/reliaburger/reliaburger/issues/1449)).
- An upgrade's commit can hold the agent loop while a disk stalls ([#1381](https://github.com/reliaburger/reliaburger/issues/1381)).
- Over a 24-hour run at full rate, Bun's memory grew all day ([#1417](https://github.com/reliaburger/reliaburger/issues/1417)), a node's disk filled ([#1418](https://github.com/reliaburger/reliaburger/issues/1418)) and one turn ran 1 to 2.5 s ([#1419](https://github.com/reliaburger/reliaburger/issues/1419)).
- It ships with the `zstd` 0.13 advisories ([#1367](https://github.com/reliaburger/reliaburger/issues/1367)) and RUSTSEC-2026-0414 in `pulldown-cmark`, which needs an option Reliaburger never sets.

## [0.2.2] - 2026-10-25

UX polish: the CLI review, Kubernetes import and export fidelity, and chaos for jobs. [Release page](https://github.com/reliaburger/reliaburger/releases/tag/v0.2.2), [release log](docs/release-log.md#022-ux-polish).

### Upgrading

- A 0.2.1 cluster can't roll onto 0.2.2: start a fresh cluster. The protocol moved from 52 to 53 and the state format from 69 to 70 ([upgrading from 0.2.1](docs/releasing.md#upgrading-from-021)).
- There's no Intel macOS build any more, and the installer refuses Intel Macs. 0.2.1 is the last release with `relish-macos-x86_64` ([#1018](https://github.com/reliaburger/reliaburger/issues/1018)).
- The installer asks for a mode; pass `--laptop` for the laptop cluster ([#1051](https://github.com/reliaburger/reliaburger/issues/1051)).
- A one-shot `relish exec` exits with the command's exit code ([#522](https://github.com/reliaburger/reliaburger/issues/522)).

### Added

- `relish events` ([#522](https://github.com/reliaburger/reliaburger/issues/522)).
- Smoker can fault jobs and task arrays ([#704](https://github.com/reliaburger/reliaburger/issues/704)).
- A configurable per-command process limit for jobs ([#741](https://github.com/reliaburger/reliaburger/issues/741)).
- Runnable demos and examples for rollouts, GitOps, secrets, shared runc, egress and TLS, showing CPU and memory as they run ([#705](https://github.com/reliaburger/reliaburger/issues/705), [#671](https://github.com/reliaburger/reliaburger/issues/671)).

### Changed

- The CLI review: `relish fmt` keeps comments, with fixes to `-o`, exit codes and `resolve --namespace` ([#522](https://github.com/reliaburger/reliaburger/issues/522)).
- Kubernetes import and export no longer lose data silently, and correlate Services by label selector ([#703](https://github.com/reliaburger/reliaburger/issues/703)).
- `relish lint` and `relish apply` warn when a host app's init container uses a bare command name ([#1047](https://github.com/reliaburger/reliaburger/issues/1047)).

### Fixed

- Host apps' init containers run as host specs and are checked against the allowlist ([#935](https://github.com/reliaburger/reliaburger/issues/935)).
- A namespaced build whose destination is outside its namespace is refused ([#1076](https://github.com/reliaburger/reliaburger/issues/1076)).
- A process job that exits non-zero on macOS is retried ([#934](https://github.com/reliaburger/reliaburger/issues/934)).
- The slow health-probe and discovery journal writes 0.2.1 shipped with ([#1255](https://github.com/reliaburger/reliaburger/issues/1255), [#1266](https://github.com/reliaburger/reliaburger/issues/1266)).
- Outbound HTTPS no longer trusts the distrusted emSign intermediates, RUSTSEC-2026-0381 to -0383 ([#1269](https://github.com/reliaburger/reliaburger/issues/1269)).

### Removed

- The Intel macOS build ([#1018](https://github.com/reliaburger/reliaburger/issues/1018)).

### Known issues

- Two writes still wait on the agent loop, so a slow disk can hold a node's loop for seconds ([#1263](https://github.com/reliaburger/reliaburger/issues/1263), [#1340](https://github.com/reliaburger/reliaburger/issues/1340), fixed in 0.2.3).
- It ships with RUSTSEC-2026-0387 under a reviewed exception ([#1318](https://github.com/reliaburger/reliaburger/issues/1318)), which 0.2.3 removes.

## [0.2.1] - 2026-10-21

Cluster safety: the data-loss and availability bugs the 10 October audit ([#674](https://github.com/reliaburger/reliaburger/issues/674)) found that predate jobs. [Release page](https://github.com/reliaburger/reliaburger/releases/tag/v0.2.1), [release log](docs/release-log.md#021-cluster-safety).

### Upgrading

- A 0.2.0 cluster can't roll onto 0.2.1: start a fresh cluster. The protocol moved from 51 to 52 and the state format from 68 to 69 ([upgrading from 0.2.0](docs/releasing.md#upgrading-from-020)).
- Tokens have to be reissued anyway: 0.2.1 stores their hashes differently ([#918](https://github.com/reliaburger/reliaburger/issues/918)).
- Clusters are mTLS by default ([#915](https://github.com/reliaburger/reliaburger/issues/915)).
- An app whose health timeout is longer than its interval is refused ([#1019](https://github.com/reliaburger/reliaburger/issues/1019)).
- An image app is refused when the node has no container runtime ([#926](https://github.com/reliaburger/reliaburger/issues/926)).

### Changed

- Rollouts go node by node, keep capacity on an auto-rollback, and mark a failed rollout `RolloutFailed` ([#830](https://github.com/reliaburger/reliaburger/issues/830)).
- The scheduler holds a stale node's placements for a grace period and places what fits, with a reason for the rest ([#869](https://github.com/reliaburger/reliaburger/issues/869)).
- GitOps prunes only what it wrote, and its configuration lives in Raft ([#824](https://github.com/reliaburger/reliaburger/issues/824)).
- The quickstart bundles Lima 2.2.1 ([#983](https://github.com/reliaburger/reliaburger/issues/983)).
- `relish local stop` powers VMs off from inside instead of power-cutting them ([#1023](https://github.com/reliaburger/reliaburger/issues/1023)).

### Fixed

- Registry images stay durable after a node is lost, and untagged images expire ([#826](https://github.com/reliaburger/reliaburger/issues/826)).
- Council recoveries are fenced by epoch and recovery id, and councils stay odd ([#825](https://github.com/reliaburger/reliaburger/issues/825)).
- Gossip is hardened against oversized datagrams and replays ([#854](https://github.com/reliaburger/reliaburger/issues/854)).
- Every chaos fault is reverted when Bun restarts ([#828](https://github.com/reliaburger/reliaburger/issues/828)).
- A power cut while the Raft store grows can't leave a store that won't open ([#1024](https://github.com/reliaburger/reliaburger/issues/1024)).
- A leader that couldn't prove its leadership no longer forwards API calls to itself until it runs out of memory ([#1069](https://github.com/reliaburger/reliaburger/issues/1069)).
- Batch cancel, results and logs are forwarded before a follower answers not found ([#1192](https://github.com/reliaburger/reliaburger/issues/1192)).

### Known issues

- A slow disk can hold a node's agent loop for seconds ([#1255](https://github.com/reliaburger/reliaburger/issues/1255), [#1266](https://github.com/reliaburger/reliaburger/issues/1266), fixed in 0.2.2).
- Outbound HTTPS trusts the distrusted emSign intermediates, RUSTSEC-2026-0381 to -0383, as 0.2.0's does; cluster mTLS isn't affected ([#1269](https://github.com/reliaburger/reliaburger/issues/1269), fixed in 0.2.2).

## [0.2.0] - 2026-10-16

"A million jobs": one-off jobs, cron, deployment hooks and task arrays share one durable lifecycle, and accepted outcomes survive a leader change. [Release page](https://github.com/reliaburger/reliaburger/releases/tag/v0.2.0), [release log](docs/release-log.md#020-a-million-jobs).

### Upgrading

- A 0.1.6 cluster can't roll onto 0.2.0: start a fresh cluster. The protocol moved from 46 to 51 and the state format from 63 to 68, and a council backup from 0.1.6 can't seed a 0.2.0 cluster ([upgrading from 0.1.6](docs/releasing.md#upgrading-from-016)).
- Nothing the old council held comes across: re-encrypt secrets, create tokens again, trust the new CA, and export job history and snapshot volumes first.
- Every job names its runtime explicitly instead of having it inferred.
- Namespaces are isolated by default, with egress allowlists ([#675](https://github.com/reliaburger/reliaburger/issues/675)–[#691](https://github.com/reliaburger/reliaburger/issues/691)).

### Added

- Compact task arrays: a template and a count, not a record per task ([#266](https://github.com/reliaburger/reliaburger/issues/266), [#588](https://github.com/reliaburger/reliaburger/issues/588)).
- One job lifecycle for one-off jobs, cron, deployment hooks and task arrays ([#638](https://github.com/reliaburger/reliaburger/issues/638), [#642](https://github.com/reliaburger/reliaburger/issues/642)).
- Fresh, shared and host executors, and timed throughput scenarios in `relish bench` ([#654](https://github.com/reliaburger/reliaburger/issues/654), [#639](https://github.com/reliaburger/reliaburger/issues/639), [#640](https://github.com/reliaburger/reliaburger/issues/640)).
- Fair shares across namespaces, bounded job storage, per-definition job history, job metrics and failure alerts ([#675](https://github.com/reliaburger/reliaburger/issues/675)–[#691](https://github.com/reliaburger/reliaburger/issues/691)).
- A durable audit log, read with `relish audit` ([#675](https://github.com/reliaburger/reliaburger/issues/675)–[#691](https://github.com/reliaburger/reliaburger/issues/691)).
- A signed upgrade path ([#675](https://github.com/reliaburger/reliaburger/issues/675)–[#691](https://github.com/reliaburger/reliaburger/issues/691)).
- `relish upgrade check` says when a release needs a fresh cluster instead of a rolling upgrade.

### Fixed

- API authorisation holes closed and host-path sources gated ([#675](https://github.com/reliaburger/reliaburger/issues/675)–[#691](https://github.com/reliaburger/reliaburger/issues/691)).
- Job leases count in app placement ([#675](https://github.com/reliaburger/reliaburger/issues/675)–[#691](https://github.com/reliaburger/reliaburger/issues/691)).

### Known issues

- The quickstart still pins Lima 2.1.0, with its port-forwarder leak ([#982](https://github.com/reliaburger/reliaburger/issues/982)) and CVE-2026-53657; 0.2.1 bundles Lima 2.2.1 ([#983](https://github.com/reliaburger/reliaburger/issues/983)).

## [0.1.6] - 2026-10-07

Operations security: image trust, CA rotation and token lifecycle. [Release page](https://github.com/reliaburger/reliaburger/releases/tag/v0.1.6), [release log](docs/release-log.md#016-operations-security).

### Upgrading

- A 0.1.5 cluster can't roll onto 0.1.6: start a fresh cluster. The protocol moved from 40 to 46 and the state format from 58 to 63 ([upgrading from 0.1.5](docs/releasing.md#upgrading-from-015)).
- Images bind to a digest at apply. Apply again to pick up a tag that has moved; an apply fails if the registry doesn't answer and the pull-through cache doesn't hold the tag ([#361](https://github.com/reliaburger/reliaburger/issues/361)).
- `deployer` and `read-only` tokens expire after 90 days unless you pass `--ttl-days` or `--no-expiry`, so check what your CI holds ([#363](https://github.com/reliaburger/reliaburger/issues/363)).
- Back up the root CA yourself with `relish ca backup`: `relish ca rotate` asks for the backup ([#362](https://github.com/reliaburger/reliaburger/issues/362)).

### Added

- Upstream trust rules, and cosign signatures checked on upstream images before deploy ([#361](https://github.com/reliaburger/reliaburger/issues/361)).
- Several CAs per role rotated through Raft, `relish ca backup` and `relish ca verify`, and intermediate rotation signed with the root backup ([#362](https://github.com/reliaburger/reliaburger/issues/362)).
- Token rotation with a grace period ([#363](https://github.com/reliaburger/reliaburger/issues/363)).
- A secret key per namespace, opt-in with `secret_key = true` ([#363](https://github.com/reliaburger/reliaburger/issues/363)).

### Fixed

- A consumer sync's inventory read gets an agent-loop turn of its own ([#603](https://github.com/reliaburger/reliaburger/issues/603)).
- Every file lock, the Raft recovery lock included, unlocks before it closes ([#606](https://github.com/reliaburger/reliaburger/issues/606), [#613](https://github.com/reliaburger/reliaburger/issues/613)).
- The quickstart guest leaves its clock to Lima's guest agent ([#608](https://github.com/reliaburger/reliaburger/issues/608)).
- A network partition fault waits until every caller is cut ([#625](https://github.com/reliaburger/reliaburger/issues/625)).

## [0.1.5] - 2026-10-06

Bug fixes: the 0.1.4 soak findings and a codebase audit. [Release page](https://github.com/reliaburger/reliaburger/releases/tag/v0.1.5), [release log](docs/release-log.md#015-bug-fixes).

### Upgrading

- A 0.1.4 cluster can't roll onto 0.1.5: start a fresh cluster. The protocol moved from 34 to 40 and the state format from 49 to 58 ([upgrading from 0.1.4](docs/releasing.md#upgrading-from-014)).

### Added

- The token list shows scope, expiry and last use, and an hourly sweep removes expired tokens but never the last Admin ([#483](https://github.com/reliaburger/reliaburger/issues/483)).

### Fixed

- Council recovery keeps its restored tokens and recovers from a log that was never snapshotted ([#477](https://github.com/reliaburger/reliaburger/issues/477)–[#479](https://github.com/reliaburger/reliaburger/issues/479)).
- Disk pressure is measured on the real disk, and a voter whose Raft core stopped on a full disk restarts ([#480](https://github.com/reliaburger/reliaburger/issues/480), [#510](https://github.com/reliaburger/reliaburger/issues/510)).
- Remote backends stay routed when a node stops or rolls its last local replica ([#481](https://github.com/reliaburger/reliaburger/issues/481)).
- Snapshot commands go to the node holding the app's volume ([#482](https://github.com/reliaburger/reliaburger/issues/482)).
- A retired runc intent is scrubbed of decrypted environment values ([#476](https://github.com/reliaburger/reliaburger/issues/476)).
- A consumer sync no longer runs over the agent loop's turn budget ([#505](https://github.com/reliaburger/reliaburger/issues/505)).
- A peer TLS connection retires when its client leaf expires ([#509](https://github.com/reliaburger/reliaburger/issues/509)).
- A guest clock step can't crash-loop a node ([#607](https://github.com/reliaburger/reliaburger/issues/607)).
- A codebase audit's 28 fixes, across volumes, logs, metrics, the registry, token scope, ingress, DNS, batch, cron, GitOps and `apply --dry-run` ([#528](https://github.com/reliaburger/reliaburger/issues/528)–[#555](https://github.com/reliaburger/reliaburger/issues/555)).

## [0.1.4] - 2026-10-03

Cross-node logs and events, and security fixes. [Release page](https://github.com/reliaburger/reliaburger/releases/tag/v0.1.4), [release log](docs/release-log.md#014-cross-node-logs-and-events-security-fixes).

### Upgrading

- A 0.1.3 cluster can't roll onto 0.1.4: start a fresh cluster. The protocol moved from 33 to 34; the state format stays at 49 ([upgrading from 0.1.3](docs/releasing.md#upgrading-from-013)).
- A namespace-scoped token can no longer read the cluster-wide event stream ([#468](https://github.com/reliaburger/reliaburger/issues/468)).

### Added

- `relish logs --until`, `--instance` and a regular-expression `--grep` ([#458](https://github.com/reliaburger/reliaburger/issues/458)).
- Separate stderr capture for process workloads ([#466](https://github.com/reliaburger/reliaburger/issues/466)) and a `--stream` filter ([#467](https://github.com/reliaburger/reliaburger/issues/467)).
- A cluster-wide live event stream ([#469](https://github.com/reliaburger/reliaburger/issues/469)).
- Audit events name who minted or revoked a token or rotated the secret key ([#473](https://github.com/reliaburger/reliaburger/issues/473)).
- Image references with both a tag and a digest ([#455](https://github.com/reliaburger/reliaburger/issues/455)).

### Fixed

- Rollback works over the merged history, at the app's scale ([#459](https://github.com/reliaburger/reliaburger/issues/459), [#489](https://github.com/reliaburger/reliaburger/issues/489)).
- The TUI's job detail shows the selected node's run ([#460](https://github.com/reliaburger/reliaburger/issues/460)).
- Decrypted secrets stay out of world-readable runc bundles and are deleted with the instance ([#475](https://github.com/reliaburger/reliaburger/issues/475)).
- Finalising a secret rotation no longer strands the root CA backup ([#472](https://github.com/reliaburger/reliaburger/issues/472)).

### Known issues

- Consumer syncs can run over the agent loop's turn budget ([#505](https://github.com/reliaburger/reliaburger/issues/505)).
- A pooled peer connection can present an expired leaf ([#509](https://github.com/reliaburger/reliaburger/issues/509)).
- The leader can resign for disk pressure over unexported logs ([#510](https://github.com/reliaburger/reliaburger/issues/510)).

## [0.1.3] - 2026-10-03

Agent loop, observability and test quality: a node agent that never waits inline on slow work, plus cluster-wide views. [Release page](https://github.com/reliaburger/reliaburger/releases/tag/v0.1.3), [release log](docs/release-log.md#013-agent-loop-observability-and-test-quality).

### Upgrading

- A 0.1.2 cluster can't roll onto 0.1.3: start a fresh cluster. The protocol moved from 28 to 33 and the state format from 47 to 49 ([upgrading from 0.1.2](docs/releasing.md#upgrading-from-012)).
- Instance names are numbered across the cluster: three `frontend` replicas on three nodes are `frontend-0`, `-1` and `-2`, not three `frontend-0`s ([#398](https://github.com/reliaburger/reliaburger/issues/398)).
- `/v1/jobs` respects a token's namespace scope ([#385](https://github.com/reliaburger/reliaburger/issues/385)).
- A rolling upgrade or rollback on a council of exactly two voters is refused before it starts ([#371](https://github.com/reliaburger/reliaburger/issues/371)).

### Added

- Live logs, deploy history, events and jobs answer for the whole cluster, whichever node serves them ([#365](https://github.com/reliaburger/reliaburger/issues/365)).
- A "blocked by quota" status for apps the scheduler won't place ([#326](https://github.com/reliaburger/reliaburger/issues/326)).
- `relish council status`, a council line in `relish status`, and `relish wtf` findings for a fenced node, an epoch split or two leaders ([#424](https://github.com/reliaburger/reliaburger/issues/424)).
- A turn meter, `bun_agent_loop_turn_seconds`, for every agent-loop turn ([#351](https://github.com/reliaburger/reliaburger/issues/351)).
- The installers show what they're downloading ([#395](https://github.com/reliaburger/reliaburger/issues/395)).

### Changed

- Status reads a published snapshot, so `relish status`, the dashboard and the API never queue behind the agent loop ([#351](https://github.com/reliaburger/reliaburger/issues/351)).
- Metrics queries are streamed, so memory no longer grows with history ([#377](https://github.com/reliaburger/reliaburger/issues/377)).

### Fixed

- Restarting Bun no longer moves a managed-volume app onto an empty volume; `relish wtf` reports an app waiting for its volume's node ([#423](https://github.com/reliaburger/reliaburger/issues/423)).
- No split brain after `relish council recover --force`: old voters are fenced on the old epoch ([#424](https://github.com/reliaburger/reliaburger/issues/424)).
- Council snapshots install every entry ([#426](https://github.com/reliaburger/reliaburger/issues/426), [#427](https://github.com/reliaburger/reliaburger/issues/427)).
- The scheduler holds capacity reservations between ticks, and a daemon's own copy no longer counts against it ([#432](https://github.com/reliaburger/reliaburger/issues/432)–[#434](https://github.com/reliaburger/reliaburger/issues/434)).
- Stale healthy endpoints expire, and `cluster stop` waits for workers to confirm ([#431](https://github.com/reliaburger/reliaburger/issues/431), [#435](https://github.com/reliaburger/reliaburger/issues/435), [#436](https://github.com/reliaburger/reliaburger/issues/436)).
- Status never reports a live instance's pid or exit code as missing ([#358](https://github.com/reliaburger/reliaburger/issues/358), [#389](https://github.com/reliaburger/reliaburger/issues/389)).
- A stop no longer fails when the runtime is slow ([#387](https://github.com/reliaburger/reliaburger/issues/387), [#393](https://github.com/reliaburger/reliaburger/issues/393)).

## [0.1.2] - 2026-10-01

Images: multi-platform builds and pulls. [Release page](https://github.com/reliaburger/reliaburger/releases/tag/v0.1.2), [release log](docs/release-log.md#012-images).

### Upgrading

- A 0.1.1 cluster can't roll onto 0.1.2: start a fresh cluster. The protocol moved from 27 to 28 and the state format from 46 to 47 ([upgrading from 0.1.1](docs/releasing.md#upgrading-from-011)).

### Added

- Multi-platform images: `relish build` builds `linux/amd64` and `linux/arm64` by default, and every node runs the platform that matches it ([#248](https://github.com/reliaburger/reliaburger/issues/248)).
- `relish images` shows each image's platforms ([#248](https://github.com/reliaburger/reliaburger/issues/248)).
- A build cache cap and Buildah storage pruning ([#248](https://github.com/reliaburger/reliaburger/issues/248)).

### Changed

- An upgrade to a release with other formats, or a rollback to a version some node lacks, is refused before a run is recorded ([#339](https://github.com/reliaburger/reliaburger/issues/339), [#350](https://github.com/reliaburger/reliaburger/issues/350)).
- When a node dies its replicas spread over the survivors, and healthy ones stay in place ([#346](https://github.com/reliaburger/reliaburger/issues/346), [#349](https://github.com/reliaburger/reliaburger/issues/349)).

### Fixed

- The pull-through cache holds every platform of a multi-platform upstream image, not just the first puller's ([#353](https://github.com/reliaburger/reliaburger/issues/353), [#354](https://github.com/reliaburger/reliaburger/issues/354)).

## [0.1.1] - 2026-09-30

Bug fixes: every bug known from 0.1.0's soak, early users and a static review. [Release page](https://github.com/reliaburger/reliaburger/releases/tag/v0.1.1), [release log](docs/release-log.md#011-bug-fixes).

### Upgrading

- A 0.1.0 cluster can't roll onto 0.1.1: start a fresh cluster. The protocol is still 27, but the state format moved from 44 to 46 ([upgrading from 0.1.0](docs/releasing.md#upgrading-from-010)).
- The `logs`, `metrics`, `admin` and `secret-write` permissions now gate every route that needs them, and rollback needs `deploy` ([#298](https://github.com/reliaburger/reliaburger/issues/298), [#325](https://github.com/reliaburger/reliaburger/issues/325)).
- The autoscaler refuses `min = 0` ([#299](https://github.com/reliaburger/reliaburger/issues/299)).

### Added

- `relish inspect` lists every instance across the cluster, with its node ([#306](https://github.com/reliaburger/reliaburger/issues/306)).
- `relish wtf` reports each node's build and warns when nodes run different ones ([#306](https://github.com/reliaburger/reliaburger/issues/306)).

### Fixed

- A redeploy updates the app's ingress host ([#307](https://github.com/reliaburger/reliaburger/issues/307)).
- Log forwarders resume from their checkpoint after a restart ([#308](https://github.com/reliaburger/reliaburger/issues/308)).
- Departed nodes are forgotten, `relish nodes` shows dead nodes as dead, and `bun --version` includes the commit ([#276](https://github.com/reliaburger/reliaburger/issues/276)).
- Snapshot defects from the static review: restores, exports and uploads ([#291](https://github.com/reliaburger/reliaburger/issues/291)–[#294](https://github.com/reliaburger/reliaburger/issues/294)).
- GitOps defects from the static review, including signing that could be bypassed ([#295](https://github.com/reliaburger/reliaburger/issues/295)–[#297](https://github.com/reliaburger/reliaburger/issues/297), [#305](https://github.com/reliaburger/reliaburger/issues/305), [#328](https://github.com/reliaburger/reliaburger/issues/328)).
- The allocation error after a starved deploy ([#309](https://github.com/reliaburger/reliaburger/issues/309)).
- Mayo's memory growth over a long soak ([#310](https://github.com/reliaburger/reliaburger/issues/310)).
- `relish build` works on a laptop cluster ([#327](https://github.com/reliaburger/reliaburger/issues/327)).

### Known issues

- Multi-platform images don't run from Pickle; 0.1.2 fixes it.
- Rootless runc is standalone only.

## [0.1.0] - 2026-09-29

The first release: signed binaries, and a managed three-node laptop cluster from one `curl | sh`. [Release page](https://github.com/reliaburger/reliaburger/releases/tag/v0.1.0), [release log](docs/release-log.md#010-the-first-release).

### Upgrading

- Nothing to upgrade from. 0.1.0 speaks protocol 27 and state format 44.

### Added

- Two binaries: `bun`, the node agent, and `relish`, the CLI.
- Clustering: gossip membership, a Raft council, a scheduler, automatic rescheduling and rolling self-upgrades.
- Networking: eBPF service discovery with cluster DNS, and an ingress proxy with TLS, streaming and connection draining.
- Security on by default: mTLS between nodes, single-use joins, workload identity, secrets encrypted at rest and signature-gated images.
- A built-in OCI registry (Pickle), metrics you can query with SQL, indexed logs, GitOps, a web dashboard, a terminal UI and a fault injector.
- `relish manual` for offline docs and `relish wtf` to diagnose a cluster.

### Known issues

- Multi-platform images don't run from Pickle.
- Rootless runc is standalone only.

[Unreleased]: https://github.com/reliaburger/reliaburger/pull/490
[0.2.8]: https://github.com/reliaburger/reliaburger/releases/tag/v0.2.8
[0.2.7]: https://github.com/reliaburger/reliaburger/releases/tag/v0.2.7
[0.2.6]: https://github.com/reliaburger/reliaburger/releases/tag/v0.2.6
[0.2.5]: https://github.com/reliaburger/reliaburger/releases/tag/v0.2.5
[0.2.4]: https://github.com/reliaburger/reliaburger/releases/tag/v0.2.4
[0.2.3]: https://github.com/reliaburger/reliaburger/releases/tag/v0.2.3
[0.2.2]: https://github.com/reliaburger/reliaburger/releases/tag/v0.2.2
[0.2.1]: https://github.com/reliaburger/reliaburger/releases/tag/v0.2.1
[0.2.0]: https://github.com/reliaburger/reliaburger/releases/tag/v0.2.0
[0.1.6]: https://github.com/reliaburger/reliaburger/releases/tag/v0.1.6
[0.1.5]: https://github.com/reliaburger/reliaburger/releases/tag/v0.1.5
[0.1.4]: https://github.com/reliaburger/reliaburger/releases/tag/v0.1.4
[0.1.3]: https://github.com/reliaburger/reliaburger/releases/tag/v0.1.3
[0.1.2]: https://github.com/reliaburger/reliaburger/releases/tag/v0.1.2
[0.1.1]: https://github.com/reliaburger/reliaburger/releases/tag/v0.1.1
[0.1.0]: https://github.com/reliaburger/reliaburger/releases/tag/v0.1.0
