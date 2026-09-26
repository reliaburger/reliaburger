# Cluster basics

A Reliaburger cluster is the same `bun` binary on every node, started with
`--cluster`. Membership spreads by SWIM gossip. A Raft council, embedded in the
agent, holds the desired state and schedules work; it starts on the first node
and grows as nodes join, up to seven voters.

A container cluster needs Linux with rootful runc and eBPF (kernel 5.8+,
cgroup v2, bpffs at `/sys/fs/bpf`). Bun refuses `--cluster` under rootless
runc. On a laptop, `relish setup --quickstart` builds exactly this inside VMs;
the rest of this chapter is for servers.

## Initialise the first node

`relish init` generates the cluster PKI, the first node's identity, a sample
`app.toml` and an mTLS-required `reliaburger.toml`:

```sh
relish init cluster --cluster-name prod --node-id node-01
```

Open `cluster/reliaburger.toml` and set `enabled = true` under `[ebpf]` (and
under `[dns]` and `[ingress]` if you want them; see `networking`). Back up
`cluster/prod-master.key`: every node needs it, and it unlocks the cluster's
CA and secret keys. Then start the node:

```sh
sudo bun --cluster --runtime runc --config cluster/reliaburger.toml
```

While the token store is empty, the API is open on loopback only, so you can
mint the first admin token over the generated CA:

```sh
export RELIABURGER_TOKEN="$(relish --ca-cert cluster/identity/root-ca.crt \
  token create --name first-admin --role admin)"
relish --ca-cert cluster/identity/root-ca.crt status
```

Export `RELIABURGER_CA_CERT=cluster/identity/root-ca.crt` to drop the flag.
`security` covers roles and scoped tokens.

## Add nodes

Join tokens are single-use, bound to one node id and short-lived (15 minutes
by default, at most an hour). They're separate from API tokens:

```sh
relish --ca-cert cluster/identity/root-ca.crt \
  join-token create --node-id node-02 --ttl 15m
```

On the new node, enrol an identity against any member's API:

```sh
relish join --token <TOKEN> --node-id node-02 \
  --ca-fingerprint sha256:<ROOT_CA_FINGERPRINT> https://<LEADER>:9117
```

`relish init` printed the root CA fingerprint; pinning it means a member
offering a different CA is refused. `--token-file` reads the token from a
private file instead of the command line. `join` only enrols the identity (into
`./identity` unless you pass `--identity-dir`). Then give the node its own
config with `[cluster] name` matching the cluster and `join` listing an
existing member's gossip address (port 9443), and start `bun --cluster`.

## Watch it

```sh
relish nodes      # gossip membership and node state
relish council    # Raft voters and the current leader
```

The council heals itself: lose a voter and the reconciler promotes a caught-up
node in its place. If every voter is lost, `relish council recover` rebuilds
the council from a stopped survivor's snapshot or a sealed backup (see
`operations`). Read its `--help` first: writes after the last backup are lost.

## When a node is gone for good

Every node that reads the service catalogue promises to confirm when it stops
routing to a retired address. A node that dies never confirms, so its promises
pile up with every deploy. When they fill three quarters of the ledger, the
leader's `discovery:withdrawal-backlog` readiness check turns degraded and its
log names the nodes that owe confirmations:

```text
scheduler: endpoint withdrawal ledger is 78% full; catalogue updates stop at 100%.
Receipts owed by: node-03 (800 generations, not alive), ...
```

The leader also exports the reading as the metrics
`discovery_withdrawal_ledger_occupancy_ratio` (0 to 1) and
`discovery_withdrawal_pending_generations`, if you'd rather alert on a trend.

At 100% the cluster stops publishing catalogue changes: new instances and
scale-ups don't become reachable. If a node is permanently gone, stop or isolate
whatever it was running, then retire it:

```sh
relish decommission-node node-03 --workloads-stopped --reason "disk failed"
```

This discharges only that node's confirmations. The name can't rejoin; a
replacement machine enrols fresh with `relish join`. Don't decommission a node
that might still be running, such as one behind a network partition: it could
still be sending traffic to addresses the cluster would then hand out again.

## Contributors: clusters from a checkout

`relish dev create` builds `bun` and `relish` from your source tree inside a
Lima build VM and starts a cluster from them. It's for working on Reliaburger
itself; everyone else wants the quickstart.

```sh
relish dev create --nodes 3
relish dev shell reliaburger-1
relish dev destroy
```

## Run a cluster on your own Linux servers

You can also deploy reliaburger to pre-existing Linux VMs or bare-metal servers
if you already have that computing power available. Check this [procedure here](../linux-servers.md).
