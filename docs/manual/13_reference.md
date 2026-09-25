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
| `--runtime NAME` | `auto` | `auto`, `process`, or `runc` (Linux) |
| `--cluster` | off | form or join a cluster using `[cluster]` |
| `--compatibility` | | print the protocol and state formats this binary supports |

`auto` picks runc on Linux when it's on the `PATH`, and ProcessGrill otherwise.
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
cache and the Lima home, and leaves anything else under `~/.reliaburger`.

## Files on a node

`relish setup` puts `bun` in `~/.reliaburger/bin` as `bun-vX.Y.Z` plus a `bun`
symlink, ready for self-upgrade, and writes `reliaburger.toml` in the current
directory (`--dir` to change it). It doesn't install a service; run `bun`
under systemd or another supervisor yourself. Bun's own data goes where
`[storage]` says:

| Setting | Default |
|---------|---------|
| `[storage] data` | `/var/lib/reliaburger/data` (Raft, ownership records, identity) |
| `[storage] images` | `/var/lib/reliaburger/images` |
| `[storage] logs` | `/var/lib/reliaburger/logs` |
| `[storage] metrics` | `/var/lib/reliaburger/metrics` |
| `[storage] volumes` | `/var/lib/reliaburger/volumes` |

## Node config sections

Every section is optional, and an unknown key is an error, so a typo fails at
startup instead of being ignored.

| Section | For |
|---------|-----|
| `[node]` | `name`, `labels` (matched by `placement`) |
| `[cluster]` | `name`, `join`, ports, `[cluster.backup]` (see `operations`) |
| `[storage]` | data directories, `[storage.snapshots]` (see `images-and-volumes`) |
| `[resources]` | CPU and memory held back for the node itself (`500m` or `0.5` cores, `512Mi`) |
| `[network]` | `advertise_address`, host `port_range` |
| `[security]` | master key, bootstrap and identity paths, `require_mtls` |
| `[ebpf]`, `[dns]`, `[ingress]` | the data plane (see `networking`) |
| `[images]` | registry, pull-through cache, mirrors, trust policy |
| `[metrics]`, `[logs]`, `[alerts]` | observability (see `observability`) |
| `[gitops]`, `[upgrades]` | see `operations` |
| `[smoker]`, `[testing]` | fault durations and the fault and test policy (see `chaos`) |
| `[process_workloads]` | `allowed_binaries` for `exec` workloads (empty: none) |
| `[runtime]` | `stop_confirmation_timeout_secs` |

## Exit codes

Most commands exit `0` on success and `1` on error. `wtf`, `path`, `test` and
`bench` exit `1` when they find a failure, and `wtf` and `path` exit `2` for
warnings, degraded paths or incomplete evidence.
