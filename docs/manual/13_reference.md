# Reference

The flags, variables, files and ports you'll reach for. `relish help COMMAND`
(or `--help` on any command) is the complete, current list of flags.

## relish global flags

| Flag | Environment | Meaning |
|------|-------------|---------|
| `--endpoint URL` | `RELIABURGER_ENDPOINT` | Bun API to talk to |
| `--token TOKEN` | `RELIABURGER_TOKEN` | API bearer token |
| `--ca-cert PATH` | `RELIABURGER_CA_CERT` | cluster root CA; switches the default to HTTPS |
| `-o`, `--output FORMAT` | | `human` (default), `json` or `yaml`; every command takes it except the six interactive ones ([machine output](#machine-output)) |

A flag beats its variable. With neither, `relish` uses the laptop cluster's
saved context, then `http://127.0.0.1:9117`.

## Machine output

Every command takes `-o json` or `-o yaml` and prints the same facts the
table does, as data, so a script or an agent never parses prose. Warnings and
progress ("Building web...", "downloading ...") go to stderr, so stdout stays
parseable; where a command has warnings worth keeping (`lint`, `fault list`),
they're in the data too. A command that changes something reports what it
did, not just that it worked.

Reading the cluster:

| Command | What `-o json` prints |
|---|---|
| `logs` | one object per line (JSON Lines): `at`, `node`, `instance`, `stream`, `text` |
| `inspect` | `desired` (or `desired_error`), then each node's `answer`: its `instances`, or the `reason` it's missing |
| `routes` | the list of routes |
| `logs-search` | the rows, as one list |
| `batch watch` | one summary per line (JSON Lines), once a second until the run ends |
| `batch logs` | `run`, `index` and the task's kept `output` |
| `snapshot list`, `snapshot create` | the node's snapshot records |
| `upgrade check` | `running`, `latest`, each node's `platform`, a `verdict` (`up_to_date`, `available`, `needs_fresh_cluster`, `no_artefact_for_node`, `no_artefacts`), the `reason` behind a verdict that blocks, and the `command` to run when one is available |
| `upgrade plan` | the `stages` in order, with `estimated_secs` |
| `upgrade status` | `scope` (`cluster` or `node`) and the run record as the node keeps it, under `state` |
| `secret pubkey` | `public_key`, and the `namespace` it belongs to |
| `exec` | `exit_code` and the command's `output`; relish still exits with that code |

Changing it:

| Command | What `-o json` prints |
|---|---|
| `stop`, `delete`, `release` | `action` (`stop_requested`, `delete_requested`, `released`), `app`, `namespace`, `forced` |
| `rollback` | the `app`, its `namespace`, and whether it was `forced` |
| `apply --rerun-jobs`, `batch`, `run` | the node's answer: `created` and `instances`, or `batch_id` and its counts |
| `batch replay`, `batch cancel` | `run` and the `change` (`replay_acknowledged`, `cancelled`) |
| `fault <type>` | the fault, with any it created on other nodes under `routed` |
| `fault list`, `fault clear` | `faults` and `warnings`; the node's `message` |
| `fault scenario` | each step's `outcome` once the run ends; with `--dry-run`, the timeline |
| `snapshot restore`, `snapshot delete` | the `change`, `app`, `namespace` and `snapshot` |
| `images delete` | the `repository` and `tag` |
| `build` | each image built: its `name`, `build_id`, `context_digest` and `image` |
| `sign` | the `image`, the `digest` it signed and the cluster's `message` |
| `gitops set`, `gitops disable` | whether GitOps is `enabled`, and the `config` file |
| `policy delete` | the policy `deleted` |
| `secret rotate`, `token revoke` | the node's `message` |
| `token create`, `token rotate`, `join-token create` | the new `token`, with its name, role, scope and expiry |
| `ca rotate` | the `role`, the `stage` (`begun`, `finalised`) and the council's `message` |
| `upgrade start` | `scope`, `target_version`, `upgrade_id`, and `already_running` with its `detail` |
| `upgrade rollback`, `resume`, `abort` | the `change`, and the version or upgrade it was about |
| `council recover`, `council re-enrol` | what was restored or removed, and the epochs |
| `decommission-node` | the retirement record |

On your machine:

| Command | What `-o json` prints |
|---|---|
| `lint` | `apps`, `jobs` and `warnings` |
| `compile` | the `files` merged, the `warnings`, and the merged `config` |
| `diff` | `added`, `modified` and `removed`, each with its `field_changes` |
| `fmt` | the `path` and the `result` (`formatted`, or `ok` with `--check`) |
| `import`, `export` | the converted `config` or `manifests`, and the `report` of what was approximated or dropped |
| `secret encrypt` | the `encrypted` value |
| `init`, `join` | the files written and the root CA `fingerprint` |
| `ca backup`, `ca verify` | the backup's `cluster`, `fingerprint` and `expires_at` |
| `sign keygen`, `dev keygen` | where the private key is, and the `public_key` |
| `setup --quickstart`, `setup --ssh` | what it built: the `nodes`, `api`, `app`, the `context` file, `stages` with their `seconds`, `next`, `teardown` and `warnings`; progress goes to stderr |
| `local`, `dev create`, `dev status`, `dev stop`, `dev start`, `dev destroy` | each node and its state, or what the command did to it |
| `dev test`, `dev disk`, `dev clean` | whether the run `passed`; the build VM's report as `output` |
| `dev sign-binary`, `dev countersign-binary` | the `signature` file and the binary's `sha256` |
| `manual examples` | the files `written` |
| `uninstall` | what was `removed` and `kept`, and the `notes` |

`status`, `top`, `nodes`, `history`, `events`, `audit`, `wtf`, `path`, `test`
and the other read commands print the same records as their tables.

A secret never appears in the output unless the command exists to hand it
over. `token create`, `token rotate` and `join-token create` print the new
token once, in `token`, exactly as their plain output does: the cluster keeps
only its hash, so this is the only time anyone sees it. Treat the JSON as you
would the token. Everything else names where a secret is (`init`'s
`master_key`, `sign keygen`'s `path`), never the secret itself, and `secret
encrypt` never echoes the plaintext.

Six commands refuse `-o json`, exit `2` and say they're interactive: `tui`,
`dashboard`, `manual` (the reader and `--web`), `source`, the guided `setup`
and `dev shell`. They take over the terminal or a browser, or ask their questions as
they go, so there's nothing to print as data. Bare `relish -o json` is
`relish -o json tui`, so it refuses too. `setup --quickstart` and `setup
--ssh` aren't among them: they never ask, so they print what they built, and
an agent that ran them can quote it. `exec` isn't one of them either: it runs a
command once, with no stdin and no terminal, and prints what it said. `-f` on
`logs` refuses `-o json` until a followed line carries its time and stream.

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
| `env` | Environment variables: plain, encrypted (`ENC[AGE:...]`) or from a secret provider (`PROVIDER[name:path]`, see `security`) |
| `namespace` | Namespace; `default` when omitted |
| `owner` | The name of the API token of the controller that manages the app. Only that token or an admin may set it; `relish apply` and GitOps then leave the app alone, and `relish release` hands it back (see `writing-a-controller`) |
| `health` | `path`, `port` (default: the app's `port`), `protocol` (`http` or `https`), `interval` (10 s), `timeout` (5 s, at most `interval`), `threshold_unhealthy` (3), `threshold_healthy` (1), `initial_delay` (0 s). No zeros |
| `[[init]]` | `image` (default: the app's), `command` and `env` (on top of the app's; plain or `PROVIDER[name:path]`, no encrypted values); runs before the app with its env and mounts |
| `[[sidecar]]` | `name` (a DNS label), `image` (default: the app's), `command`, `args`, `env` (on top of the app's; no encrypted values), `cpu` and `memory`. Runs beside each instance in its network namespace, after the init containers and before the app, and stops after it. Restarts on its own with a backoff up to a minute. Its requests add to the app's. Needs rootful runc; refused on `exec` and `script` apps and alongside an `egress` allowlist |
| `[[config_file]]` | `path` and exactly one of `content` (a per-instance read-only copy) or `source` (a node file, allowlisted); not on `exec` or `script` apps |
| `[[volumes]]` | `path`, and `source` (a node directory, allowlisted) or a managed volume with optional `size`; not on `exec` or `script` apps. A `size` needs root, and can't change once the volume exists |
| `placement` | `required` and `preferred` node labels, `key=value` |
| `deploy` | `strategy` (`rolling` or `blue-green`), `max_surge`, `max_unavailable`, `drain_timeout`, `health_timeout`, `auto_rollback` |
| `stop_signal` | The signal a stop sends: `SIGTERM` (default), `SIGINT`, `SIGQUIT`, `SIGHUP`, `SIGUSR1`, `SIGUSR2` or `SIGWINCH` |
| `stop_grace` | Time to exit after that signal before the kill, `"1s"` to `"1h"`. Default: the deploy's `drain_timeout`, or 10 s for a plain stop |
| `runtime` | A `[app.NAME.runtime]` table: `capabilities` to add, namespaced `sysctls` and `devices` (`path`, `permissions`). Each needs the node's `[runtime]` allowlist and, from a token with a `[permission]` block, `host-exec`; not on `exec` or `script` apps (see `security`) |
| `pre_stop`, `pre_stop_timeout` | A command run inside each instance before its signal, and its bound (default 30 s, at most an hour). A failure is logged, never fatal. On `exec` and `script` apps it runs on the host and needs an absolute, allowlisted path |
| `autoscale` | `metric` (`cpu` or `memory`), `target`, `min` (at least 1), `max`, `evaluation_window`, `cooldown`, `scale_down_threshold`. Removing it returns the app to `replicas` |
| `ingress` | `host`, `path`, `tls` (`cluster`, `explicit` or `none`), `websocket`, `rate_limit_rps`, `rate_limit_burst` |
| `firewall` | `allow_from`: apps (or `namespace/app`) allowed to connect |
| `egress` | `allow`: outside destinations the app may reach. `allow_franchise` is refused |
| `metrics` | `port` (default: the app's) and `path` (default `/metrics`) to scrape |
| `provenance` | `template` and `use`: the template and `[use.*]` that made the app. Template expansion writes it, and `relish compile` keeps it in its output; changing it alone restarts nothing, and a policy `default` can't set it. A label, not a credential: anyone who can apply can set it, so a policy shouldn't exempt on it |

## Template fields

A `[template.NAME]` table takes `params` and `body`; a `[use.NAME]` table
takes `template` and one key per parameter. Both names are lowercase DNS
labels. See `deploy` for a worked example.

| Field | Meaning |
|-------|---------|
| `params` | Parameter names to types: `"string"`, `"int"` or `"bool"`, or `{ type = "...", default = ... }`. A parameter without a default is required. Names start with a letter or `_`, and `template` is taken |
| `body` | Ordinary `[app.*]` and `[job.*]` TOML with `${param}` placeholders, and nothing else: no other tables, templates or uses |
| `template` (in `[use.*]`) | The template to expand |
| any other key (in `[use.*]`) | A parameter's value, of the parameter's type |

## Job fields

A `[job.NAME]` table takes these fields. Unknown fields are refused.

| Field | Meaning |
|-------|---------|
| `runtime` | `runc` (default), `shared-runc` or `process` |
| `image` | OCI image, for `runc` and `shared-runc` |
| `exec`, `script` | Host binary or inline script, for `process` only |
| `command` | Command and arguments |
| `cpu`, `memory` | Request–limit ranges, as for apps |
| `pids` | Most processes and threads one command may hold (`pids.max`), 1–32768. Default 256 for `shared-runc` and host executor commands; no limit for a fresh `runc` container |
| `env` | Environment variables, plain or encrypted; `PROVIDER[...]` values are refused (apps only) |
| `namespace` | Namespace; `default` when omitted |
| `schedule` | Cron schedule in UTC: five fields or a macro such as `@daily` |
| `run_before` | `["app.NAME"]`: run as a hook before that app publishes |
| `firewall` | Apps in other namespaces allowed to connect to the job, as for an app's `[firewall]`; container jobs only |
| `egress` | Outside destinations the job may reach, as for an app's `[egress]`; container jobs only |
| `max_attempts` | 1–10. Default 4, or 1 for a hook |
| `task_timeout_secs` | 0–86400 per attempt; 0 means no deadline. Default none, or 600 for a hook |
| `overlap` | Cron only: `forbid` (default) skips an occurrence while a run is active; `allow` starts another |
| `replay_unknown` | `true` replays automatically after an unknown outcome and counts a timeout as an ordinary failure; hooks refuse it. Default `false` |
| `provenance` | `template` and `use`: the template and `[use.*]` that made the job, written by template expansion, as for an app |

Ordinary TOML jobs, cron and hooks use the same executor as arrays. See [batch jobs](14_batch-jobs.md)
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
| `[security]` | master key, bootstrap and identity paths, `require_mtls` (default `true`: `bun --cluster` refuses to start without an identity from `relish init` or `relish join`; `false` is the explicit plaintext opt-in, and single-node `bun` ignores it; a plaintext cluster with no master key and no token takes a node's placement poll and rollout-failure report without a service token, see `security`), `bootstrap_peers` (joining nodes through the perimeter), `operator_cidrs` (your networks to the API port only, see `security`), `leaf_lifetime_override_secs` (development only, see below), `[security.tokens] default_ttl` (the lifetime of a new deployer or read-only token, `"90d"` unless set; see `security`) |
| `[ebpf]`, `[dns]`, `[ingress]` | the data plane (see `networking`); `[dns] upstream` defaults to the host's `/etc/resolv.conf` nameservers |
| `[images]` | registry, pull-through cache, mirrors, trust policy and upstream rules (see `security`), builds (`build_timeout_secs`, `max_context_bytes`, `build_cache_max_bytes`: 100 GiB, 2 GiB on a quickstart; see `images-and-volumes`) |
| `[metrics]`, `[logs]`, `[alerts]` | observability (see `observability`) |
| `[gitops]`, `[upgrades]` | see `operations` |
| `[smoker]`, `[testing]` | fault durations and the fault and test policy (see `chaos`) |
| `[process_workloads]` | `allowed_binaries` for `exec` workloads (empty: none; a `script` needs `/bin/sh`), which the scheduler also places by; host commands inherit only `PATH`, `HOME`, `LANG`, `LANGUAGE`, `TZ`, `USER`, `LOGNAME`, `SHELL`, `TMPDIR` and `LC_*` from Bun |
| `[secret_providers.NAME]` | `binary` (absolute), `timeout_secs` (5, at most 60), `max_output_bytes` (64 KiB, at most 1 MiB), `env`: the exec secret providers this node runs for `PROVIDER[NAME:path]` values, which the scheduler also places by (see `security`) |
| `[reporting_tree]` | `report_interval_secs` (5), `stale_report_timeout_secs` (30), `stale_node_grace_secs` (300: how long a live node whose reports stopped keeps its replicas before they move; see `deploy-an-app`) |
| `[runtime]` | `stop_confirmation_timeout_secs`; `allowed_capabilities`, `allowed_sysctls` and `allowed_devices`, the runtime knobs apps may use here, which the scheduler also places by (empty: none; see `security`) |

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

Most commands exit `0` on success and `1` on error. A usage error (an
unknown flag, a missing argument, or `--output json` on an interactive
command) exits `2`.

`wtf`, `path`, `test` and `bench` exit `1` when they find a failure, and `wtf`
and `path` exit `3` for warnings, degraded paths or incomplete evidence. Before
0.2.2 that was `2`, which a script couldn't tell from a usage error.

`relish agent` commands use their own codes, which say only whether the
command answered: `0` answered, `2` usage, `3` couldn't reach or authenticate
to the cluster, `4` refused, `5` anything else. A usage error there is a JSON
envelope on stdout, not text on stderr. See
[Working with an AI agent](16_working-with-an-ai-agent.md#exit-codes).

`relish exec` exits with the remote command's own exit code, so
`relish exec web ls /ready` works in a shell `if`. It exits `1` when it
can't run the command at all, with an `error:` line on stderr.

## Every relish command

<!-- relish-commands:start -->
<!-- Generated from relish's own command definitions: run `make reference-commands` after changing them. -->

Run `relish` with no command for the terminal UI. `relish help COMMAND` (or `--help` anywhere) lists every flag; the sections above cover [global flags](#relish-global-flags), [environment variables](#environment-variables) and [exit codes](#exit-codes).

### Set up and run a cluster

See [getting started](00_getting-started.md) and [cluster basics](02_cluster-basics.md).

- `relish setup`: Guided setup: detect or install bun, then write a starter config
- `relish local <status|start|stop|destroy> [NODE]`: Manage a laptop cluster created by setup --quickstart
- `relish init [DIR]`: Initialise a new cluster (generates CAs, age keypair, node identity)
- `relish cluster`: Create a cluster of bare-metal appliances (docs/manual/15_appliance.md) *(0.3.0, not in 0.2.7)*
  - `relish cluster create --bare-metal --name <NAME> --operator <OPERATORS>... <DIRECTORY> <MACHINES>...`: Create a cluster for appliance machines: its PKI and admin token on this machine, and a seed per machine for an RBSEED stick *(0.3.0, not in 0.2.7)*
- `relish image`: Appliance OS images: download, write to a disk, and seed machines *(0.3.0, not in 0.2.7)*
  - `relish image download`: Download the newest OS build for every architecture and check it against the release key *(0.3.0, not in 0.2.7)*
  - `relish image write <IMAGE> <DEVICE>`: Write a disk image (.raw or .raw.zst) onto a device, erasing it *(0.3.0, not in 0.2.7)*
  - `relish image seed <DIRECTORY> <MACHINES>...`: Write seeds for machines joining a running bare-metal cluster *(0.3.0, not in 0.2.7)*
  - `relish image delete <REFERENCE>`: Delete a registry tag: the same as `relish images delete` *(0.3.0, not in 0.2.7)*
- `relish machines`: Unclaimed appliances on the LAN: list them, or claim them with seeds over the network *(0.3.0, not in 0.2.7)*
  - `relish machines claim <DIRECTORY> <MACHINES>...`: Claim machines: compare each one's claim key with its console, then send it a seed, joining it to the cluster in DIRECTORY (or, with --create, making a new cluster there from them) *(0.3.0, not in 0.2.7)*
- `relish netboot <DIR>`: Install appliances over the network: ProxyDHCP, TFTP and HTTP for the OS that `relish image download` saved *(0.3.0, not in 0.2.7)*
- `relish os`: The appliance OS across the cluster: what each node runs, and rolling a new version out node by node *(0.3.0, not in 0.2.7)*
  - `relish os list`: Each node's OS version, and the newest release *(0.3.0, not in 0.2.7)*
  - `relish os upgrade [VERSION]`: Roll an OS version across the cluster, one node at a time: workers, then the council, the leader last. Each node's workloads move off before it reboots *(0.3.0, not in 0.2.7)*
  - `relish os status`: The rollout in progress, or the last one *(0.3.0, not in 0.2.7)*
  - `relish os resume`: Carry on with a paused rollout, retrying the node that stopped it *(0.3.0, not in 0.2.7)*
  - `relish os abort`: Stop the rollout; nodes keep the version they're on *(0.3.0, not in 0.2.7)*
- `relish join --node-id <NODE_ID> <ADDR>`: Join an existing cluster
- `relish join-token`: Manage short-lived node-enrolment tokens
  - `relish join-token create --node-id <NODE_ID>`: Create a single-use token for enrolling one node
  - `relish join-token list`: List join tokens: node id, expiry and whether each has been used *(0.3.0, not in 0.2.7)*
  - `relish join-token revoke <NODE_ID>`: Revoke every unused join token for a node id *(0.3.0, not in 0.2.7)*
- `relish nodes`: List cluster nodes and their gossip state
- `relish council`: Show the council (Raft) on every node, or recover from full loss
  - `relish council status`: Show every node's view of the council (the default)
  - `relish council recover --data-dir <DATA_DIR>`: Recover a cluster whose entire council was lost
  - `relish council backups --from <FROM>`: List the sealed council backups at an object-store URL
  - `relish council re-enrol --data-dir <DATA_DIR>`: Re-enrol a voter fenced out by `council recover`
- `relish decommission-node --workloads-stopped --reason <REASON> <NODE_ID>`: Permanently retire a stopped or fenced node; return requires fresh enrolment
- `relish uninstall`: Remove the CLI, its PATH link, the managed Lima tools and the image cache

### Deploy and manage apps

See [deploy an app](01_deploy-an-app.md) and [writing a controller](15_writing-a-controller.md).

- `relish apply [PATH_OR_URL]`: Apply a Reliaburger TOML or Kubernetes YAML manifest
- `relish status`: Show cluster and app status
- `relish inspect <NAME>`: Show every instance of an app across the cluster, with its node
- `relish exec <APP> [COMMAND]...`: Run a command once inside a running instance and exit with its exit code. Not interactive: no stdin and no terminal
- `relish deploy <PATH>`: Trigger a rolling deploy for an app
- `relish cancel-deploy <OPERATION_ID>`: Cancel a node-local deploy and wait for its current work to finish
- `relish history <APP>`: Show deploy history for an app, from every node
- `relish rollback <APP>`: Rollback an app to the previous version
- `relish stop <APP>`: Scale an app to zero, keeping its configuration; `relish apply` starts it again
- `relish delete <APP>`: Remove an app from the cluster; each node then stops its instances
- `relish release <APP>`: Take an app back from the controller that owns it, so `relish apply` and GitOps manage it again (owner or admin)
- `relish batch [PATH]`: Submit a batch of jobs, or manage a task array
  - `relish batch submit <PATH>`: Submit a compact TOML manifest containing mixed resource profiles
  - `relish batch replay --node <NODE> --grant-digest <GRANT_DIGEST> --acknowledge-side-effects <ID>`: Acknowledge repeating side effects for the exact unknown owner grants
  - `relish batch watch <ID>`: Watch rates, bounded profile summaries and duration distributions
  - `relish batch cancel <ID>`: Cancel an array or mixed manifest and retire its active attempts
  - `relish batch results <ID>`: Read a bounded indexed page of accepted task outcomes
  - `relish batch logs --index <INDEX> <ID>`: Print a task's retained output (the first and last 2 KiB): any singleton's, or a failed task's in a larger array
- `relish run --batch <NAME> [ARGS]...`: Run a resource-aware task array from an image or allowlisted host binary
- `relish batch-status <ID>`: Show the progress of a submitted batch
- `relish jobs`: List bounded job run summaries and rates, or durable definitions with their last outcomes

### Work with config files

See [deploy an app](01_deploy-an-app.md) and [coming from Kubernetes](09_kubernetes.md).

- `relish lint <PATH>`: Validate a config file without deploying
- `relish fmt <PATH>`: Format a TOML config file with canonical ordering
- `relish compile <PATH>`: Compile configs into a single resolved output
- `relish diff <PATH_A> [PATH_B]`: Show structural diff between two configs
- `relish import --file <FILES>...`: Convert Kubernetes YAML manifests to Reliaburger TOML
- `relish export --file <FILE>`: Export Reliaburger TOML to Kubernetes YAML manifests

### Networking

See [networking and ingress](03_networking.md).

- `relish resolve <NAME>`: Resolve a service name to its VIP and backends
- `relish routes`: Show ingress routing table

### Watch what's running

See [observability](04_observability.md).

- `relish tui`: Launch the interactive terminal UI
- `relish dashboard`: Open a read-only web dashboard through the current authenticated context
- `relish top`: Show every workload on every node, with its latest CPU and memory
- `relish metrics <APP>`: Show an app's own Prometheus metrics, scraped by the nodes running it
- `relish logs <NAME>`: Stream logs from an app or job
- `relish logs-export --dest <DEST>`: Copy a node's Parquet log files here, or have it ship them to object storage
- `relish logs-search <SOURCE> <SQL>`: Search exported Parquet log archives with SQL
- `relish events`: Show the cluster's recent events: deploys, restarts, health, faults, tokens, secrets and audited operations, newest last

### Diagnose and test

See [diagnostics](07_diagnostics.md).

- `relish wtf`: Diagnose cluster health and correlate likely causes
- `relish path --to <TO> <SOURCE>`: Walk the network path from a workload to a destination, hop by hop
- `relish test`: Run the built-in integration test suite against the cluster
- `relish bench`: Run reproducible performance benchmarks against the real data plane

### Work with an AI agent

See [working with an AI agent](16_working-with-an-ai-agent.md).

- `relish agent`: Commands for LLM agents: one JSON envelope, bounded, never interactive
  - `relish agent guide`: How to use relish as an agent: the manual chapter, as Markdown sections
  - `relish agent version`: This binary's version, commit, target and cluster formats
  - `relish agent schema [guide|version|schema|source-version|source-tree|source-grep|source-show|source-extract|logs|manual-search|manual-show|context|diagnose|explain|upgrade-check|report|patch-init|patch-build|patch-test|patch-try|patch-diff|patch-destroy|setup]`: The JSON Schema of the envelope and of each command's data
  - `relish agent source`: The source this binary was built from: search it, read it, extract it
    - `relish agent source version`: This binary's source: version, commit, digest, file count, and the commit every node runs
    - `relish agent source tree [PREFIX]`: Files with their size in bytes and their line count
    - `relish agent source grep <PATTERN>`: Search the source with a regular expression, line by line
    - `relish agent source show <TARGET>`: Print up to 400 numbered lines of one file: `path` or `path:from-to`
    - `relish agent source extract <DIR>`: Write the source into a new or empty directory, as a git repository when git is installed
  - `relish agent logs <APP>`: An app's recent log lines, newest kept, each wrapped as untrusted
  - `relish agent manual`: The manual compiled into this binary: search it, show a section
    - `relish agent manual search <WORDS>...`: Up to ten sections that mention every word, best first, each with a short excerpt and the `show` command that prints it
    - `relish agent manual show <TARGET>`: One chapter, or one section of it, as Markdown: `diagnostics` or `diagnostics#relish-path-can-a-reach-b`
  - `relish agent context`: The first call: findings, unknowns, faults, alerts, one line per app and the newest events, in one bounded answer
  - `relish agent diagnose`: Every finding with its evidence, read-only next steps, and pointers into the manual and the source
  - `relish agent explain [ID]`: What a finding means, its thresholds and where it's decided; every finding when no id is given. Reads only this binary
  - `relish agent upgrade-check`: Whether the cluster can take the latest release: `ready`, `up_to_date`, `needs_fresh_cluster`, `blocked` or `unknown`
  - `relish agent report --title <TITLE> --what-happened <WHAT_HAPPENED> --expected <EXPECTED> --not-tested <NOT_TESTED> --caught-by <CAUGHT_BY> --out <OUT>`: Draft a redacted bug report for a human to post. Never posts
  - `relish agent patch`: Build, test and try a fix on a throwaway cluster
    - `relish agent patch init <DIR>`: Extract this binary's source into a new workspace, on a `patch` branch
    - `relish agent patch build <DIR>`: Build the workspace: `relish` for this machine, `bun` and a Linux `relish` for the nodes (in the build VM on macOS). Both by default
    - `relish agent patch test <DIR> [FILTER]`: Run the portable test suite in the workspace, optionally filtered
    - `relish agent patch try <DIR>`: Boot a throwaway cluster from the workspace's binaries, with its own `RELIABURGER_HOME`
    - `relish agent patch diff <DIR>`: The change against the extracted source, and the tests that ran
    - `relish agent patch destroy [DIR]`: Remove the workspace's throwaway cluster, or every one with `--all`. The workspace and its source stay
  - `relish agent setup [claude-code|codex|cursor|vscode|goose|gemini]...`: Wire Reliaburger into the AI agents on this machine. Shows the diff; writes only with --yes
- `relish mcp`: Serve the `relish agent` commands as read-only MCP tools over stdio

### Break things on purpose

See [chaos](05_chaos.md).

- `relish fault`: Inject faults for chaos testing (Smoker)
  - `relish fault delay <TARGET> <DELAY>`: Add latency to connections to a service
  - `relish fault drop <TARGET> <PERCENTAGE>`: Fail a percentage of connections
  - `relish fault dns <TARGET> <FAULT_TYPE>`: Return NXDOMAIN for DNS resolution
  - `relish fault partition <TARGET>`: Block traffic between services
  - `relish fault bandwidth <TARGET> <LIMIT>`: Throttle bandwidth to a service (not implemented yet: Bun refuses it, #366)
  - `relish fault cpu <TARGET> <PERCENTAGE>`: Consume CPU in a service's cgroup
  - `relish fault memory <TARGET> <VALUE>`: Push memory usage toward a service's limit
  - `relish fault disk-io <TARGET> <LIMIT>`: Throttle disk I/O for a service
  - `relish fault kill <TARGET>`: Kill instances of a service, or running attempts of a job (SIGKILL)
  - `relish fault pause <TARGET>`: Freeze instances of a service, or running attempts of a job (SIGSTOP)
  - `relish fault resume <TARGET>`: Resume (unfreeze) previously paused instances of a service
  - `relish fault node-drain <TARGET>`: Simulate graceful node departure
  - `relish fault node-kill <TARGET>`: Simulate abrupt node failure
  - `relish fault node-pressure <TARGET>`: Consume bounded CPU and memory capacity on one node
  - `relish fault list`: List all active faults
  - `relish fault clear [TARGET]`: Clear faults — all, by numeric id, or by service name
  - `relish fault scenario <PATH>`: Run a scripted chaos scenario from a TOML file

### Security and access

See [security and access](10_security.md).

- `relish token`: Manage API tokens
  - `relish token create --name <NAME>`: Create a new API token
  - `relish token list`: List all API tokens with their scope, expiry and last use
  - `relish token revoke <NAME>`: Revoke an API token by name
  - `relish token rotate <NAME>`: Give a token a new secret under the same name. The old secret keeps working for the grace period, then stops
- `relish secret`: Manage secrets (encrypt values for use in app configs)
  - `relish secret pubkey [DIR]`: Print the cluster's age public key (for `relish secret encrypt`)
  - `relish secret encrypt --pubkey <PUBKEY> <VALUE>`: Encrypt a plaintext value for use in app config ENC[AGE:...] fields
  - `relish secret rotate`: Rotate the secret encryption key (start or finalise)
- `relish ca`: Back up the cluster's root CA, check the backup, and rotate intermediates
  - `relish ca backup --from <FROM> --out <OUT>`: Re-seal a root CA backup to a new passphrase or age recipient *(0.3.0, not in 0.2.7)*
  - `relish ca verify --fingerprint <FINGERPRINT> <FILE>`: Check a root CA backup offline
  - `relish ca rotate --role <ROLE>`: Rotate an intermediate CA, signing the new one with your root backup
- `relish identity`: Rotate the key that signs workload identity tokens *(0.3.0, not in 0.2.7)*
  - `relish identity rotate-signing-key`: Sign workload identity tokens with a new key *(0.3.0, not in 0.2.7)*
- `relish sign --key <KEY> <IMAGE>`: Sign a Pickle-hosted image with your own key so `require_signatures` admits it
  - `relish sign keygen --out <OUT>`: Generate an image signing key and print the public key line for `[images.trust_policy] keys`
- `relish audit`: Show who did what: the durable audit log of applies, deletes, execs, upgrades, faults, tokens and the other changes to the cluster
- `relish policy`: List, audit or delete the cluster's policies, or test policies offline
  - `relish policy list`: List the cluster's policies, with their action and rule
  - `relish policy delete <NAME>`: Delete a policy (admin only; audited). Policies never judge policies, so this works even when one refuses every app
  - `relish policy audit`: List the committed apps and jobs the cluster's policies would now refuse, warn about or record. Changes nothing: a new policy never touches running work
  - `relish policy test --file <FILES>...`: Judge the apps and jobs in some files by the policies in them, offline, the way the cluster would. Exits non-zero when an apply would be refused

### Images and volumes

See [images and volumes](11_images-and-volumes.md).

- `relish images`: List images in the Pickle registry, or delete a tag
  - `relish images delete <REFERENCE>`: Delete a tag (`repository:tag`). The image stays pullable by digest until it has been untagged for `[images] gc_retain_days`; then it expires, unless an app or job pins it, and GC reclaims its layers
- `relish build <PATH>`: Build an OCI image and push to Pickle
- `relish snapshot`: Manage volume snapshots (Btrfs-backed volumes only)
  - `relish snapshot create <APP>`: Snapshot an app's managed volumes
  - `relish snapshot list <APP>`: List an app's snapshots, newest first
  - `relish snapshot restore <APP> <NAME>`: Restore a snapshot over its live volume (stop the app first)
  - `relish snapshot delete <APP> <NAME>`: Delete a snapshot

### Upgrades and GitOps

See [operations](12_operations.md).

- `relish upgrade`: Roll a new bun binary across the cluster, or back
  - `relish upgrade check`: Check for available updates
  - `relish upgrade start [VERSION]`: Start a rolling upgrade (network: pass a version; air-gapped: pass --binary)
  - `relish upgrade plan <VERSION>`: Preview the rolling order and estimated duration
  - `relish upgrade status`: Show upgrade progress
  - `relish upgrade rollback [VERSION]`: Roll back to a previous version (cluster: version required)
  - `relish upgrade resume`: Resume a paused upgrade
  - `relish upgrade abort`: End a paused upgrade in which no node has moved. When some nodes already swapped, use `rollback <version>` instead
- `relish gitops`: Show or change the cluster's GitOps configuration
  - `relish gitops status`: Show the cluster's GitOps configuration, sync status and what GitOps owns. Credentials are never shown
  - `relish gitops set <PATH>`: Replace the cluster's GitOps configuration from a TOML file holding a `[gitops]` section (admin only; audited). Credentials in it are sealed with the cluster master key before they reach Raft
  - `relish gitops disable`: Turn GitOps off for the whole cluster (admin only; audited). What GitOps wrote stays in place

### Learn

See [under the hood](06_under-the-hood.md).

- `relish manual [CHAPTER]`: Read the built-in manual (searchable TUI; --web for the browser)
  - `relish manual examples`: Write the embedded example configs into a directory
- `relish source [QUERY]`: Browse and fuzzy-search the source this binary was built from

### Contributor tools

See [dev cluster](../README.md#dev-cluster).

- `relish dev`: Manage a local dev cluster (Lima VMs)
  - `relish dev create [NAME]`: Create a new dev cluster
  - `relish dev status [NAME]`: Show dev cluster status
  - `relish dev shell <NODE>`: Open a shell on a node
  - `relish dev stop [NAME]`: Stop a dev cluster (VMs stay on disk)
  - `relish dev start [NAME]`: Start a stopped dev cluster
  - `relish dev destroy [NAME]`: Destroy a dev cluster (delete all VMs)
  - `relish dev test [FILTER]`: Run tests in a Linux VM (all Linux-gated tests enabled)
  - `relish dev disk`: Show disk usage in the test VM
  - `relish dev clean`: Clean cargo build artefacts in the test VM
  - `relish dev keygen --out <OUT>`: Generate an Ed25519 release signing keypair
  - `relish dev sign-binary --key <KEY> <BINARY>`: Sign a binary, producing a detached .sig envelope
  - `relish dev countersign-binary --external-key <EXTERNAL_KEY> <BINARY>`: Add your external signature to a release binary's .sig envelope, keeping the release signature as it is (no release key needed)
<!-- relish-commands:end -->
