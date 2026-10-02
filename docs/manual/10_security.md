# Security and access

A generated cluster requires mTLS between nodes, and every API call carries a
bearer token. This chapter covers the parts you handle yourself: how the CLI
finds and proves itself to the cluster, API tokens, encrypted secrets,
workload identity and signed images.

## How relish connects

`relish` picks its endpoint in this order:

1. `--endpoint URL`
2. `RELIABURGER_ENDPOINT`
3. the laptop cluster's saved context, `~/.reliaburger/context.json`
4. `http://127.0.0.1:9117`, or `https://` when a CA certificate is set

The token comes from `--token` or `RELIABURGER_TOKEN`, and the cluster CA from
`--ca-cert` or `RELIABURGER_CA_CERT`. The saved context carries its own token
and CA, but an explicit endpoint doesn't borrow them. Plain `http://` is
allowed only to a loopback address, so a bearer token never crosses the network
in the clear.

### Reaching a node from another machine

A clustered Linux node runs its own nftables perimeter firewall. Out of the box
it admits only cluster members, the `bootstrap_peers` you listed, and loopback
to the API port. To use `relish` from your laptop, list your address or network
in the node's config and restart Bun:

```toml
[security]
operator_cidrs = ["192.168.0.0/24", "10.1.2.3/32", "2001:db8:1::/48"]
```

The Bun API also has to listen on a routable address (`bun --listen
0.0.0.0:9117`), which it allows only once an API token exists.

- It opens the API port (`--listen`, default 9117) and nothing else: gossip,
  Raft and reporting stay members-only.
- It changes the packet filter only. Every call still needs a token, and TLS
  still verifies against the cluster CA.
- IPv4 and IPv6 both work; a bare address means that one host.
- Bun refuses to start on a malformed entry, a `/0` (`0.0.0.0/0`, `::/0`: list
  the networks you actually use), or a CIDR with host bits set
  (`192.168.0.17/24`; the error names `192.168.0.0/24`).
- It's read at startup; there's no live reload.

Rootless Bun and macOS don't run the perimeter, so the setting has no effect
there. The laptop quickstart doesn't need it either: it reaches each node's API
through a port forward that arrives on the node's loopback.

## API tokens

```sh
relish token create --name ci-deploy --role deployer --namespaces shop --ttl-days 90
relish token list      # name, role, created and expiry times (UTC)
relish token revoke ci-deploy
```

| Role | May |
|------|-----|
| `admin` | everything, including tokens, join tokens, secret rotation and node faults |
| `deployer` | apply, deploy, stop and roll back workloads, inject workload faults |
| `read-only` | status, logs, metrics and diagnostics (the default role) |

`--apps` and `--namespaces` narrow a token to those apps and namespaces. In the
image registry that means repositories named `<namespace>/<app>` inside the
scope; a scoped token can't push or pull a bare name like `api` at all (see
`images-and-volumes`). Some
operations need cluster-wide authority, so only an *unscoped* admin can manage
tokens and join tokens, rotate secrets, sign images, decommission nodes, clear
every fault, or apply `[namespace]` and `[permission]` declarations.

`create` prints the plaintext token once, on stdout, and the cluster keeps
only a hash, so `TOKEN="$(relish token create ...)"` captures it. Tokens don't
expire unless you give `--ttl-days`. `revoke` refuses to remove the last admin
token; create its replacement first. Revoking a token, or letting it expire,
also ends every dashboard session that was logged in with it.

### Permissions

Roles and scopes are coarse. A `[permission.<token-name>]` block narrows one
token further, to named actions on named apps:

```toml
[permission.ci-deploy]
actions = ["deploy", "logs"]
apps = ["web"]
namespaces = ["shop"]   # omit for every namespace
```

A permission can only take away: the token still needs the role and scope for
anything it does. A token with no block is governed by its role and scope
alone, so permissions are opt-in per token. Once a token has a block, it may
do only what the block lists:

| Action | What it covers |
|--------|----------------|
| `deploy` | apply, delete and roll back the listed apps, cancel their deploys |
| `scale` | stop the listed apps |
| `exec` | `relish exec` into the listed apps |
| `host-exec` | jobs and process workloads that run host commands |
| `logs` | the listed apps' logs: `relish logs`, follow, WebSocket stream, entries |
| `metrics` | the listed apps' metrics and charts, and their rows in `relish top` |
| `secret-write` | `relish secret rotate` (needs `apps = ["*"]` and no `namespaces`) |
| `admin` | every action above, plus tokens, join tokens, upgrades, elections, node decommissioning, image signing, log export and `[permission]`/`[namespace]` declarations |
| `secret-read` | nothing yet: no API route returns a decrypted secret |

Some reads span every app: `/v1/logs/sql`, the raw metric store, the cluster
metric rollups and alerts. They need the action granted with `apps = ["*"]` and
no `namespaces` list. So do the admin routes. A block that grants `metrics` on
one app still shows that app's charts on its dashboard page, but the dashboard
leaves out the alert panel, and `relish top` shows only that app's rows.

A browser session keeps its token's permissions. Nodes talking to each other
use the cluster's internal identity, which no block can restrict, so
`relish logs` and `relish top` still gather every node's answer; the
node you asked filters it. `relish wtf` reads health, membership and
diagnostics, none of which a permission block gates.

Take care putting a block on an admin token. Leave out `admin` and that token
loses token management and upgrades. Keep at least one admin token with no
block.

A new cluster starts with no tokens, and until the first one exists the API is
open. Bun only allows that on a loopback listener, so mint the first admin
token on the node itself before exposing the API (see `cluster-basics`).

## Secrets

Secrets live in your config, encrypted to the cluster's age public key. Only
`env` values take them, in apps and jobs:

```toml
[app.api]
image = "ghcr.io/example/api:1.4.2"
env = { DB_PASSWORD = "ENC[AGE:YWdlLWVuY3J5cHRpb24...]" }
```

Encrypt a value with the cluster's public key. `relish secret pubkey` asks the
cluster for its current key, using the same endpoint, token and CA as every
other command, so it works straight after `relish setup --quickstart`. Give it
the directory `relish init` wrote to read the key from disk instead, with no
cluster running:

```sh
relish secret pubkey                  # ask the cluster
relish secret pubkey cluster          # offline, from `relish init` output
relish secret encrypt --pubkey "$(relish secret pubkey)" 'the plaintext'
```

Encryption is local: `secret encrypt` never contacts the cluster. The node
that starts the instance decrypts the value into its environment. If it can't
decrypt, it refuses to start the instance rather than pass the ciphertext
through. On the node, the plaintext goes only into files that root (or, for
rootless runc, the user running Bun) alone can read, and the container spec
that carries it is deleted when the instance stops.

To rotate the key: `relish secret rotate` makes a new keypair and prints its
public key, while the old one keeps decrypting. Re-encrypt your values with the
new key, re-apply, then run `relish secret rotate --finalize`. It refuses, and
names the offenders, while any stored secret still needs the old key. Plain
`secret pubkey` follows the rotation; the offline form reads the file from
`init`, which still holds the original key.

## Workload identity

Every container gets a SPIFFE identity, `spiffe://CLUSTER/ns/NAMESPACE/app/NAME`
(or `/job/NAME`), and its credentials appear read-only in
`/run/reliaburger/identity/`: `cert.pem`, `key.pem`, `ca.pem`, `bundle.pem`,
and `token`, an OIDC JWT. Use the certificate for mTLS between your services.
The JWT also works as a bearer token against the Reliaburger API, as a
read-only credential confined to the workload's own app and namespace.

## Signed images

Trust policy is node config. With it on, the scheduler refuses unsigned images
from the cluster's own registry, and a node that can't read the cluster's
trust state refuses the deploy rather than guess:

```toml
[images.trust_policy]
require_signatures = true
keys = []                 # extra trusted ECDSA P-256 public keys, base64
```

Images that `relish build` pushes are signed by the cluster's build signer,
which the policy trusts without a key. Images from external registries and the
pull-through cache aren't checked, so pin those by digest.

For images you build elsewhere and push to the registry, sign them with your
own key. Make one, and put the public key it prints in every node's `keys`:

```bash
relish sign keygen --out ci-signing.pem   # private key, mode 0600; keep it secret
```

Then sign each image you push, by tag, pinned reference or digest:

```bash
relish sign myapp:v1 --key ci-signing.pem
```

`relish sign` looks the tag up in the registry and signs the manifest digest it
points at, on your machine; only the signature and public key reach the
cluster, and signing needs an unscoped Admin token. Re-pushing the tag with new
content leaves the new digest unsigned, so sign again after every push. The
agent warns if its own `keys` doesn't list your key yet, since deploys there
will refuse the image until it does.

Already have key tooling? Any unencrypted PKCS#8 P-256 key works, and this
prints its public key in the form `keys` wants:

```bash
openssl genpkey -algorithm EC -pkeyopt ec_paramgen_curve:P-256 -out ci-signing.pem
openssl pkey -in ci-signing.pem -pubout -outform DER | tail -c 65 | base64
```

## Between nodes

`relish init` generates the root, node, workload and ingress CAs. Node
certificates last a year and cluster-issued ingress certificates 90 days. Nodes
renew their certificates at the midpoint of their validity without restarting
(a development cluster can shorten both lifetimes for soak testing, see
`[security] leaf_lifetime_override_secs` in the reference), and a
new node joins with a single-use token and a certificate signing request, so
its private key never leaves it. Every node needs the cluster's master key
(`*-master.key` from `init`): it unwraps the CA keys and the secret keys, and
seals council backups. Keep it safe and backed up.
