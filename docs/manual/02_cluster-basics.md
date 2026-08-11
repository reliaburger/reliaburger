# Cluster basics

A Reliaburger cluster is the same `bun` binary on every node, started with
`--cluster`. Membership spreads by SWIM gossip; a small Raft council (up to
three voters) holds the desired state and schedules work.

## Initialise the first node

`relish init` generates the cluster PKI and an mTLS-required config:

```sh
relish init cluster --cluster-name prod --node-id node-01
sudo bun --cluster --runtime runc --config cluster/reliaburger.toml
```

Then mint the first admin token over the generated CA:

```sh
export RELIABURGER_TOKEN="$(relish --ca-cert cluster/identity/root-ca.crt \
  token create --name first-admin --role admin)"
relish --ca-cert cluster/identity/root-ca.crt status
```

## Add nodes

Join tokens are single-use and short-lived, separate from API tokens:

```sh
relish --ca-cert cluster/identity/root-ca.crt \
  join-token create --node-id node-02 --ttl 15m
```

On the new node, enrol an identity, point `[cluster].join` at an existing
member's gossip address (port 9443), then start `bun --cluster`:

```sh
relish join --token <TOKEN> --node-id node-02 \
  --ca-fingerprint sha256:<ROOT_CA_FINGERPRINT> https://<LEADER>:9117
```

## Watch it

```sh
relish nodes      # gossip membership and node state
relish council    # Raft voters and the current leader
```

The council self-heals: lose a voter and the reconciler promotes a worker.
For total council loss there is `relish council recover` (read its `--help`
before using it — it rewinds to a backup).

## Try it in VMs

`relish dev create` builds a real three-node cluster in Lima VMs:

```sh
relish dev create --nodes 3
limactl shell reliaburger-1 relish nodes
relish dev destroy
```
