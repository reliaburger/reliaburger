# Reference

The flags, variables, files and ports you'll reach for. `relish help COMMAND`
(or `--help` on any command) is the complete, current list of flags.

## relish global flags

| Flag | Environment | Meaning |
|------|-------------|---------|
| `--endpoint URL` | `RELIABURGER_ENDPOINT` | Bun API to talk to |
| `--token TOKEN` | `RELIABURGER_TOKEN` | API bearer token |
| `--ca-cert PATH` | `RELIABURGER_CA_CERT` | cluster root CA; switches the default to HTTPS |
| `--output FORMAT` | | `human` (default), `json` or `yaml` |

A flag beats its variable. With neither, `relish` uses the laptop cluster's
saved context, then `http://127.0.0.1:9117`.

## Environment variables

| Variable | Read by | Meaning |
|----------|---------|---------|
| `RELIABURGER_HOME` | relish, installer | local state directory (absolute; default `~/.reliaburger`) |
| `RELIABURGER_ENDPOINT`, `RELIABURGER_TOKEN`, `RELIABURGER_CA_CERT` | relish | see above |
| `RELIABURGER_VERSION`, `RELIABURGER_RELEASE_BASE_URL` | installer | release to install, and where from |
| `RELIABURGER_NO_MODIFY_PATH` | installer | `1` leaves your shell config alone |

Registry passwords come from whatever variables you name in
`[[images.external_registries]] password_secret`. Object-storage exports and
backups use each backend's standard credential variables.

## bun flags

| Flag | Default | Meaning |
|------|---------|---------|
| `--config PATH` | none (all defaults) | node config file |
| `--listen ADDR` | `127.0.0.1:9117` | API listen address |
| `--runtime NAME` | `auto` | `auto`, `process`, `runc`, or `mixed` (Linux) |
| `--cluster` | off | form or join a cluster using `[cluster]` |
| `--compatibility` | | print the protocol and state formats this binary supports |

Linux `--runtime mixed` enables both container and host backends. Auto may
select both; explicit `--runtime runc` selects containers only. Restarting a
node from `runc` to `mixed` (or `auto`) keeps its running containers: Bun
adopts each one through runc. A node with running image apps can't yet switch
from `process` to `mixed` (#827). Jobs choose
`runtime="runc"` (default), `runtime="process"` or `runtime="shared-runc"`.
Process jobs refuse images and container jobs refuse host exec/script fields.

Host workloads require an explicit allowlist and `mount_isolation = false`;
CPU/memory limits and container egress enforcement remain unavailable to them.
While no API token exists, Bun refuses any `--listen` address that isn't a
loopback IP.

## Ports

| Port | What | Config |
|------|------|--------|
| 9117 | Bun API, web dashboard | `bun --listen` |
| 9443 | gossip | `[cluster] gossip_port` |
| 9444 | Raft | `[cluster] raft_port` |
| 9445 | state reporting | `[cluster] reporting_port` |
| 5050 | Pickle registry | `[images] registry_port` (same on every node) |
| 80, 443 | ingress, when enabled | `[ingress] http_port`, `https_port` |
| 53 | `.internal` DNS, when enabled | `[dns] listen` |

On a clustered Linux node, Bun's perimeter firewall drops 9117, 9443–9445 and
the host port range for anyone but cluster members, `[security]
bootstrap_peers` (API and cluster ports) and `[security] operator_cidrs` (API
port only). Loopback is always allowed.

A laptop cluster forwards `127.0.0.1:19117`–`19119` to each node's API,
`localhost:18080` to node 1's ingress and `localhost:15050` to its registry.
`relish setup --quickstart --api-port --ingress-port --registry-port` moves
them.

## Files on your machine

Everything the installer and the quickstart write lives in `~/.reliaburger`
(or `$RELIABURGER_HOME`):

| Path | What |
|------|------|
| `bin/relish` | the CLI (`~/.local/bin/relish` links here if that's on your `PATH`) |
| `context.json` | the laptop cluster's endpoint, CA path and admin token (mode 0600) |
| `clusters/NAME/` | a laptop cluster's state, node configs and `timings.json` |
| `clusters/NAME/security/` | its master key, admin token and CA (`identity/root-ca.crt`) |
| `cache/` | downloaded guest images and Linux binaries |
| `tools/`, `lima/` | the private Lima install and its VMs |

`relish local destroy --yes` removes a cluster's VMs, credentials and state
and keeps the cache. `relish uninstall` then removes the CLI, the tools, the
cache and the Lima home (plus `context.lock` when no `context.json` is left),
and leaves anything else under `~/.reliaburger`.

## Job commands and identities

| Command | Purpose |
|---------|---------|
| `relish jobs` | Bounded run summaries, backlog and accepted rates |
| `relish jobs --definitions` | Definitions, revisions and durable UTC schedules |
| `relish run --batch NAME --image IMAGE -- COMMAND` | Indexed run, count one by default |
| `relish run --batch NAME --count N --schedule "0 3 * * *" --image IMAGE -- COMMAND` | Register a durable array schedule |
| `relish batch watch ID` | Counts, queue, accepted rates and duration buckets |
| `relish batch results ID --index N` | One accepted task outcome |
| `relish batch logs ID --index N` | Retained head/tail output |
| `relish logs NAME --namespace NS --instance run-ID` | Singleton log stream selected by run |
| `relish batch cancel ID` | Cancel one run or all profiles in its parent |
| `relish batch replay ID --node NODE --grant-digest DIGEST --acknowledge-side-effects` | Acknowledge repeated effects for exact unknown grants |
| `relish run --batch …`, `relish batch FILE`, `relish batch submit FILE` with `--idempotency-key KEY` | Retry an uncertain submission without admitting it twice |

Use returned IDs; physical executor slots can be reused.

## App fields

An `[app.NAME]` table takes these fields. Unknown fields are refused, and so
is a field the node can't honour; nothing is silently dropped.

| Field | Meaning |
|-------|---------|
| `image` | OCI image. Not with `exec` or `script` |
| `exec`, `script` | Host binary or inline script: a process workload (needs the node's `[process_workloads]` allowlist) |
| `command`, `args` | Replace the image's entrypoint and its default arguments (`args` only with an image) |
| `working_dir` | Absolute working directory inside the container; image apps only |
| `run_as_user`, `run_as_group` | Container uid and gid, inside its user namespace; refused on `exec` and `script` apps |
| `replicas` | A count, or `"*"` for one per node. Default 1 |
| `port` | Port the app listens on; published on the node and in service discovery |
| `cpu`, `memory` | Request–limit ranges, such as `"250m-1000m"` and `"256Mi-512Mi"` |
| `gpu` | Refused above 0 until GPU support lands ([#359](https://github.com/reliaburger/reliaburger/issues/359)) |
| `env` | Environment variables, plain or encrypted |
| `namespace` | Namespace; `default` when omitted |
| `health` | `path`, `port` (default: the app's `port`), `protocol` (`http` or `https`), `interval` (10 s), `timeout` (5 s, at most `interval`), `threshold_unhealthy` (3), `threshold_healthy` (1), `initial_delay` (0 s). No zeros |
| `[[init]]` | `image` (default: the app's) and `command`; runs before the app with its env and mounts |
| `[[config_file]]` | `path` and exactly one of `content` (a per-instance read-only copy) or `source` (a node file, allowlisted); not on `exec` or `script` apps |
| `[[volumes]]` | `path`, and `source` (a node directory, allowlisted) or a managed volume with optional `size`; not on `exec` or `script` apps. A `size` needs root, and can't change once the volume exists |
| `placement` | `required` and `preferred` node labels, `key=value` |
| `deploy` | `strategy` (`rolling` or `blue-green`), `max_surge`, `max_unavailable`, `drain_timeout`, `health_timeout`, `auto_rollback` |
| `autoscale` | `metric` (`cpu` or `memory`), `target`, `min` (at least 1), `max`, `evaluation_window`, `cooldown`, `scale_down_threshold`. Removing it returns the app to `replicas` |
| `ingress` | `host`, `path`, `tls` (`cluster`, `explicit` or `none`), `websocket`, `rate_limit_rps`, `rate_limit_burst` |
| `firewall` | `allow_from`: apps (or `namespace/app`) allowed to connect |
| `egress` | `allow`: outside destinations the app may reach. `allow_franchise` is refused |
| `metrics` | `port` (default: the app's) and `path` (default `/metrics`) to scrape |

## Job fields

A `[job.NAME]` table takes these fields. Unknown fields are refused.

| Field | Meaning |
|-------|---------|
| `runtime` | `runc` (default), `shared-runc` or `process` |
| `image` | OCI image, for `runc` and `shared-runc` |
| `exec`, `script` | Host binary or inline script, for `process` only |
| `command` | Command and arguments |
| `cpu`, `memory` | Request–limit ranges, as for apps |
| `env` | Environment variables, plain or encrypted |
| `namespace` | Namespace; `default` when omitted |
| `schedule` | Cron schedule in UTC: five fields or a macro such as `@daily` |
| `run_before` | `["app.NAME"]`: run as a hook before that app publishes |
| `firewall` | Apps in other namespaces allowed to connect to the job, as for an app's `[firewall]`; container jobs only |
| `egress` | Outside destinations the job may reach, as for an app's `[egress]`; container jobs only |
| `max_attempts` | 1–10. Default 4, or 1 for a hook |
| `task_timeout_secs` | 0–86400 per attempt; 0 means no deadline. Default none, or 600 for a hook |
| `overlap` | Cron only: `forbid` (default) skips an occurrence while a run is active; `allow` starts another |
| `replay_unknown` | `true` replays automatically after an unknown outcome and counts a timeout as an ordinary failure; hooks refuse it. Default `false` | Ordinary TOML jobs,
cron and hooks use the same executor as arrays. See [batch jobs](14_batch-jobs.md)
for retry, overlap, retention, supported runtimes and API shapes.

Job limits, all enforced by the council:

| Limit | Value |
|-------|-------|
| Active runs in the cluster | 128, of which 32 only cron fires and deployment jobs may use |
| Active arrays, batches, profiles and manual runs per namespace | 64 |
| Job definitions per namespace / in the cluster | 64 / 1,024 (one-off definitions expire with their last run) |
| Retained run receipts in the cluster | 2,048 |
| Named jobs in one finite batch | 64 |
| Leader sync call accepted by a node | 8 MiB |

Namespace `cpu`, `memory` and `gpu` quotas also bound the job attempts the
leader grants; a run waiting on its quota shows `quota_blocked_reason`.

## Files on a node

`relish setup` puts `bun` in `~/.reliaburger/bin` as `bun-vX.Y.Z` plus a `bun`
symlink, ready for self-upgrade, and writes `reliaburger.toml` in the current
directory (`--dir` to change it). It doesn't install a service; run `bun`
under systemd or another supervisor yourself. Bun's own data goes where
`[storage]` says:

| Setting | Default |
|---------|---------|
| `[storage] data` | `/var/lib/reliaburger/data` (Raft, standalone `job-state/jobs.json`, worker ledgers, ownership records, identity) |
| `[storage] images` | `/var/lib/reliaburger/images` |
| `[storage] logs` | `/var/lib/reliaburger/logs` |
| `[storage] metrics` | `/var/lib/reliaburger/metrics` |
| `[storage] volumes` | `/var/lib/reliaburger/volumes` |
| `[storage] allowed_host_paths` | `[]`: absolute prefixes a volume or `config_file` `source` may mount from; empty refuses every host path, and the directories above are always refused (see `images-and-volumes`) |
| `[storage] audit_max_bytes` | `67108864` (64 MiB): the bound on the audit log in `{data}/audit`, which `relish audit` reads; at least 64 KiB (see `security`) |

## Node config sections

Every section is optional, and an unknown key is an error, so a typo fails at
startup instead of being ignored.

| Section | For |
|---------|-----|
| `[node]` | `name`, `labels` (matched by `placement`) |
| `[cluster]` | `name`, `join`, ports, `council_size` (how many voters the council grows to, odd, 1 to 7; read when the node creates the cluster), `[cluster.backup]` (see `operations`) |
| `[storage]` | data directories, `allowed_host_paths`, `audit_max_bytes` (see `security`), `[storage.snapshots]` (see `images-and-volumes`) |
| `[resources]` | CPU and memory held back for the node itself (`500m` or `0.5` cores, `512Mi`) |
| `[network]` | `advertise_address`, host `port_range` (default `{ start = 10000, end = 32767 }`, end exclusive: below Linux's ephemeral range; ports something on the host already listens on are skipped) |
| `[security]` | master key, bootstrap and identity paths, `require_mtls` (default `true`: `bun --cluster` refuses to start without an identity from `relish init` or `relish join`; `false` is the explicit plaintext opt-in, and single-node `bun` ignores it), `bootstrap_peers` (joining nodes through the perimeter), `operator_cidrs` (your networks to the API port only, see `security`), `leaf_lifetime_override_secs` (development only, see below), `[security.tokens] default_ttl` (the lifetime of a new deployer or read-only token, `"90d"` unless set; see `security`) |
| `[ebpf]`, `[dns]`, `[ingress]` | the data plane (see `networking`); `[dns] upstream` defaults to the host's `/etc/resolv.conf` nameservers |
| `[images]` | registry, pull-through cache, mirrors, trust policy and upstream rules (see `security`), builds (`build_timeout_secs`, `max_context_bytes`, `build_cache_max_bytes`: 100 GiB, 2 GiB on a quickstart; see `images-and-volumes`) |
| `[metrics]`, `[logs]`, `[alerts]` | observability (see `observability`) |
| `[gitops]`, `[upgrades]` | see `operations` |
| `[smoker]`, `[testing]` | fault durations and the fault and test policy (see `chaos`) |
| `[process_workloads]` | `allowed_binaries` for `exec` workloads (empty: none; a `script` needs `/bin/sh`), which the scheduler also places by; host commands inherit only `PATH`, `HOME`, `LANG`, `LANGUAGE`, `TZ`, `USER`, `LOGNAME`, `SHELL`, `TMPDIR` and `LC_*` from Bun |
| `[reporting_tree]` | `report_interval_secs` (5), `stale_report_timeout_secs` (30), `stale_node_grace_secs` (300: how long a live node whose reports stopped keeps its replicas before they move; see `deploy-an-app`) |
| `[runtime]` | `stop_confirmation_timeout_secs` |

### Shorter certificates for soak runs

`[security] leaf_lifetime_override_secs` shortens two certificate lifetimes so
a long test run can watch renewal happen many times a day:

| Certificate | Default | With the override |
|-------------|---------|-------------------|
| Node leaf (mTLS between nodes) | 1 year, renewed at 6 months | the override, renewed at half of it |
| Cluster-issued ingress leaf (`tls = "cluster"`) | 90 days, renewed at 45 | the override, renewed at half of it |

Workload identity (1 hour) and the CA certificates don't change. Bun refuses
to start unless the node also sets `[testing] safety_class = "development"`,
and the value must be between `600` (10 minutes) and `7776000` (90 days):

```toml
[testing]
safety_class = "development"

[security]
leaf_lifetime_override_secs = 3600
```

The member that signs a certificate decides its lifetime: the council leader
for node renewals, the member that handles a join for a new node, and each
node for its own ingress leaves. Give every node the same value. A node with
the override also renews no later than half the override after its leaf was
issued, so a node holding a year-long leaf when you add the setting renews
within half the override rather than in six months, and a node whose leader
lacks the setting renews once per half-override instead of spinning.

## Exit codes

Most commands exit `0` on success and `1` on error. `wtf`, `path`, `test` and
`bench` exit `1` when they find a failure, and `wtf` and `path` exit `2` for
warnings, degraded paths or incomplete evidence.
