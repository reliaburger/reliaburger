# Cluster basics

A Reliaburger cluster is the same `bun` binary on every node, started with
`--cluster`. Membership spreads by SWIM gossip. A Raft council, embedded in the
agent, holds the desired state and schedules work; it starts on the first node
and grows as nodes join, up to seven voters, or to the council size the
cluster was created with (appliance clusters default to five).

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

`relish join-token list` shows the tokens the council holds (node id, and
whether each is used, expired or still valid, never the token itself), and
`relish join-token revoke node-02` makes node-02's unused tokens worthless.

`relish init` printed the root CA fingerprint; pinning it means a member
offering a different CA is refused. `--token-file` reads the token from a
private file instead of the command line. `join` only enrols the identity (into
`./identity` unless you pass `--identity-dir`). Then give the node its own
config with `[cluster] name` matching the cluster and `join` listing an
existing member's gossip address (port 9443), and start `bun --cluster`.

On Linux as root, Bun runs its own nftables perimeter: only cluster members
reach the API (9117) and the cluster ports. List a joining node's address in
the members' `[security] bootstrap_peers` so it can enrol before gossip knows
it, and list your own laptop's network in `[security] operator_cidrs` so
`relish` can reach the API from there (see `security`).

## Watch it

```sh
relish nodes            # gossip membership and node state
relish council status   # every node's view of the council
```

`relish nodes` shows every member's gossip state: `alive`, `suspect` (missed
its probes, not yet declared dead) or `dead`. A dead node stays listed for up
to a day after it was last heard from, so you can see what went missing. It
drops off sooner if it comes back, leaves the cluster on purpose, or you
retire it with `relish decommission-node`.

`relish council status` asks every node, through the one you're connected to,
for its own view: its role, recovery epoch, Raft term and log position. Above
the table it prints the epoch, the leader and quorum (`3/3 voters, quorum
ok`), and it lists any node that didn't answer. One node's answer can't show a
split, so it compares them all and calls out fenced nodes, two epochs or two
leaders. A cluster created with a council size shows it on a `Size:` line
(`up to 5 voters`). `--output json` gives the same report to scripts. `relish
status` opens with a one-line summary of it.

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

Already have Linux VMs or bare-metal servers? You can run Reliaburger on them
directly. The [Linux servers guide](https://github.com/reliaburger/reliaburger/blob/main/docs/linux-servers.md)
walks through a three-node cluster, from firewall rules to systemd units.
