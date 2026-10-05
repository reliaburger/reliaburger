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
relish token list      # name, role, times (UTC), last use and scope
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

`relish token list` shows, for each token, its role, when it was created, when
it expires (`never`, `(in 30d)` or `(expired)`), when it was **last used** and
its **scope** (`all`, or `apps=… namespaces=…`). New columns are added on the
right, so a script that cuts the older ones out keeps working; `-o json` gives
the same fields (`scope`, `expires_at`, `last_used` in Unix seconds, `null` for
never) plus a `principal` id that matches the `principal` of that token's audit
events.

Last use is kept in memory on each node, not in the cluster's replicated state,
so it's cheap. The node you ask collects every node's answer and shows the
latest. Two things follow:

- a node that doesn't answer leaves a gap, and `token list` names it on stderr
  (`warning: last use incomplete: node-3 timed out`);
- a node forgets what it saw when it restarts, so a token can look *less*
  recently used than it was, never more. `never` means no node that's up has
  seen it since starting.

### Expired tokens

An expired token is refused at once (`401 token expired`). A day later (the
24-hour grace, so `token list` still shows why a client started failing) the
council leader removes it from the store. The sweep runs hourly and records a
`token.expired_swept` event per token, with principal `system`, in
`relish events`.

The sweep never removes the last admin token, and never empties the store. An
empty store is the bootstrap window: the API lets everyone in so the first
token can be created. If every admin token has expired, the one that expired
most recently stays: still refused, but present, so the API stays closed.
Expiry alone can lock you out of token management, sweep or no sweep, so keep
one admin token without `--ttl-days`, or mint the next admin token before the
current one lapses. With no admin token at all, the most recently expired
token stays. A store whose every token has expired is
not empty, so it keeps refusing anonymous requests.

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
| `secret-write` | `relish secret rotate`, for the cluster or one namespace (needs `apps = ["*"]` and no `namespaces`) |
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
that carries it is deleted when the instance stops. The runtime's record of a
stopped instance keeps the names of its environment variables, never their
values.

To rotate the key: `relish secret rotate` makes a new keypair and prints its
public key, while the old one keeps decrypting. Re-encrypt your values with the
new key, re-apply, then run `relish secret rotate --finalize`. It refuses, and
names the offenders, while any stored secret still needs the old key. Plain
`secret pubkey` follows the rotation; the offline form reads the file from
`init`, which still holds the original key.

Finalising retires the old keys, except the very first one: `relish init`
sealed the root CA's private key to it, in `<cluster>-root-ca.age`, so that key
stays (read-only, never used for new secrets) and the root backup keeps
opening. Keep that file with the master key; together they're how you'd
recover the root.

### A key per namespace

By default every namespace shares the cluster key, so a value encrypted for
one namespace decrypts in any other: anyone who can deploy to namespace B and
has a copy of namespace A's ciphertext can read it. Give a namespace its own
key to stop that:

```toml
[namespace.team-a]
secret_key = true
```

After you apply it, the leader creates team-a's key within a few seconds and
re-seals every encrypted value team-a's apps already have, in the same step,
so they keep starting. Each of those apps rolls once, because its stored
spec changed. From then on:

- team-a's values decrypt only with team-a's key. A value encrypted to the
  cluster key, or to another namespace's key, no longer decrypts in team-a,
  and the instance refuses to start.
- other namespaces can't decrypt team-a's values.

Re-encrypt the values in your own config (or GitOps repo) with the new key,
or the next apply puts the old cluster-sealed values back and those apps stop
starting:

```sh
relish secret pubkey --namespace team-a
relish secret encrypt --pubkey "$(relish secret pubkey --namespace team-a)" 'the plaintext'
```

The re-seal covers apps only. Encrypt a job's values in an opted-in
namespace with the namespace key from the start.

Rotate and finalise a namespace's key the same way as the cluster key, with
`--namespace`. It doesn't touch the cluster key or any other namespace's:

```sh
relish secret rotate --namespace team-a
relish secret rotate --finalize --namespace team-a
```

Only an Admin token with no scope can rotate a namespace's key, even one
scoped to that namespace can't. Rotating a namespace that hasn't set
`secret_key = true` fails; opt in first. Turning `secret_key` off again
doesn't remove the key, and its values keep needing it.

What this doesn't do:

- **It doesn't protect against a compromised node.** Until the master key is
  split (F03b), every node holds the master key, and the master key unwraps
  every namespace's key. The boundary is between tenants' tokens and
  workloads, not between a tenant and someone with root on a node.
- **It doesn't recall old copies.** A value encrypted to the cluster key still
  decrypts in every namespace that hasn't opted in. If team-a's ciphertext
  might have leaked, change the secret itself, or rotate and finalise the
  cluster key.

The leader records `secret.namespace_key_created` (with how many values it
re-sealed, never the values), and every rotation or finalise records
`secret.rotated` or `secret.rotation_finalised` with the namespace in its
details. See them with `relish events`.

## Backing up the root CA

`<cluster>-root-ca.age` only opens with the master key and the cluster's own
state, so it's a backup for the cluster rather than for you. Make your own
copy of the root, one that opens with something only you hold. Run this on
the node where `relish init` ran, since that's where the master key, the
security state and the sealed root are:

```sh
relish ca backup --out prod-root-backup.age --dir /etc/reliaburger
```

It asks for a passphrase twice (at least 12 characters) and writes the root's
private key and certificate, with the cluster, trust domain, fingerprint and
expiry, into an ASCII-armoured age file. The file is created owner-only, and
an existing file is never overwritten. If the directory holds more than one
cluster, add `--cluster-name`. To seal it to your own age key instead of a
passphrase:

```sh
relish ca backup --out prod-root-backup.age --recipient age1...
```

`--passphrase-file PATH` reads the passphrase from the first line of a file,
for scripts. The backup is never sealed to a cluster key: those rotate, and
this file has to outlive them. Store it off the cluster, away from the
passphrase or identity that opens it. The root's key never goes into the
council or onto another node; rotating an intermediate (coming in a later
release) will ask for this file rather than keep the root on the cluster.

Check a backup at any time, with no cluster running:

```sh
relish ca verify prod-root-backup.age --fingerprint sha256:...
relish ca verify prod-root-backup.age --fingerprint sha256:... --identity ~/.age/operator.key
```

`--fingerprint` is the root CA fingerprint `relish init` printed and every
joiner pinned. `verify` refuses a backup whose key doesn't match its
certificate, whose root has expired, whose recorded fingerprint isn't its
certificate's, or that belongs to another cluster. A wrong passphrase or
identity doesn't open it at all. Since the file is a standard age file, the
`age` tool opens it too, and inside is JSON with the certificate and key in
PEM.

## Workload identity

Every container gets a SPIFFE identity, `spiffe://CLUSTER/ns/NAMESPACE/app/NAME`
(or `/job/NAME`), and its credentials appear read-only in
`/run/reliaburger/identity/`: `cert.pem`, `key.pem`, `ca.pem`, `bundle.pem`,
and `token`, an OIDC JWT. Use the certificate for mTLS between your services.
`ca.pem` is a bundle: every Workload CA the cluster trusts, then the root.
Usually that's one of each, but while a Workload CA rotation is in progress it
holds the old CA and the new one, so verify peers against the whole file, not
just its first certificate.
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
which the policy trusts without a key. Signatures on images from external
registries and the pull-through cache aren't checked yet, but every apply binds
their tags to digests
([Tags bind to digests at apply](11_images-and-volumes.md#tags-bind-to-digests-at-apply)),
so what you applied is what runs on every node.

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

### Images from other registries

Images from outside the cluster's registry (Docker Hub, GHCR, your own
registry) follow `[[images.trust_policy.upstream]]` rules, also in each node's
`node.toml`:

```toml
[[images.trust_policy.upstream]]
match = "docker.io/library/*"      # Docker Hub's official images

[[images.trust_policy.upstream]]
match = "ghcr.io/acme/*"

[images.trust_policy.upstream_default]
allow = false                      # refuse anything no rule matches
```

`match` names a repository with its registry, `docker.io/library/nginx`, or a
prefix ending in `*`. Write Docker Hub shorthand out in full: `nginx` is
`docker.io/library/nginx`, and `acme/web` is `docker.io/acme/web`. When several
rules match, the most specific wins: an exact name beats any prefix, and a
longer prefix beats a shorter one. Tags and digests don't take part, so
`nginx:1.27` and `nginx@sha256:…` both match `docker.io/library/*`.

With no rules, or with `upstream_default.allow = true` (the default), every
upstream image is allowed, as before. With `allow = false` the rules are an
allow-list: an image that matches none is refused, by name, when you apply it
(HTTP 403, or an error in the apply's stream on a cluster) and again by Bun
before every deploy. The cluster's own images, and anything else Pickle holds,
answer to `require_signatures` instead. Pinning an image by digest doesn't
exempt it.

A rule can't ask for a signature yet: `require_signatures = true` on an
upstream rule stops the node at startup, until cosign verification lands.
Keep the rules the same on every node. A node with stricter rules refuses to
deploy what the leader admitted, and the rules only apply where the runtime
pulls images (not under the process runtime). On a single node without a
cluster, the apply is the check.

### Cosign signatures

`relish sign` signatures aren't cosign signatures. They sign the digest
string, not cosign's payload, and they live in the cluster's registry
catalogue rather than in a `.sig` tag, so `cosign verify` can't check them and
Reliaburger doesn't read them as cosign.

For images outside the cluster's registry, Reliaburger reads key-based cosign
signatures in cosign's classic layout: the `sha256-<hex>.sig` tag beside the
image, made by `cosign sign --key` with an ECDSA P-256 key (cosign's default).
A signature counts when it verifies under a trusted `cosign.pub` and names the
exact digest being deployed. With the pull-through cache on, the `.sig` image
is cached beside the image, so each signature is fetched from upstream once.

cosign 3 writes the newer Sigstore bundle by default, which Reliaburger doesn't
read yet. Ask for the classic layout when you sign:

```bash
cosign sign --key cosign.key --new-bundle-format=false ghcr.io/acme/web@sha256:…
```

Keyless signatures (a Fulcio certificate, as Chainguard and distroless images
carry) aren't checked: there's no key to trust. Turning the check on for an upstream rule (`require_signatures` with
`cosign_keys`) is still being built under
[#361](https://github.com/reliaburger/reliaburger/issues/361); until it ships, a rule
with `require_signatures = true` stops the node at startup, and upstream
images are bound to digests but not signature-checked.

## Between nodes

`relish init` generates the root, node, workload and ingress CAs. Node
certificates last a year and cluster-issued ingress certificates 90 days. Nodes
renew their certificates at the midpoint of their validity without restarting
(a development cluster can shorten both lifetimes for soak testing, see
`[security] leaf_lifetime_override_secs` in the reference). A node's API and
registry listeners close a connection, after letting in-flight requests finish,
before the certificate its client presented expires, so a peer reconnects with
its renewed certificate even over a connection that never goes idle. A
new node joins with a single-use token and a certificate signing request, so
its private key never leaves it. Every node needs the cluster's master key
(`*-master.key` from `init`): it unwraps the CA keys and the secret keys, and
seals council backups. Keep it safe and backed up.

### Cluster build signing authority

Cluster build signers receive code-signing certificates valid for at most five
years, bounded by the Workload CA's expiry. The build runner renews cached
signers before expiry. Workload mTLS certificates continue to last one hour.

An image signature is checked against its certificate chain at deployment time.
The leaf, every intermediate and the trusted root must be currently valid; expiry
or revocation of an authority can shorten the signature's usable lifetime.
Renewing a signer does not extend signatures already attached to images. Re-sign
retained images before their chain expires, and after retiring or revoking their
signing authority. Images signed under the previous one-hour certificate policy
also need re-signing. The signature timestamp does not extend certificate validity.
