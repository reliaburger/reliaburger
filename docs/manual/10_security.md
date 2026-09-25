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

## API tokens

```sh
relish token create --name ci-deploy --role deployer --namespaces shop --ttl-days 90
relish token list
relish token revoke ci-deploy
```

| Role | May |
|------|-----|
| `admin` | everything, including tokens, join tokens, secret rotation and node faults |
| `deployer` | apply, deploy, stop and roll back workloads, inject workload faults |
| `read-only` | status, logs, metrics and diagnostics (the default role) |

`--apps` and `--namespaces` narrow a token to those apps and namespaces. Some
operations need cluster-wide authority, so only an *unscoped* admin can manage
tokens and join tokens, rotate secrets, sign images, decommission nodes, clear
every fault, or apply `[namespace]` and `[permission]` declarations.

`create` prints the plaintext token once, on stdout, and the cluster keeps
only a hash, so `TOKEN="$(relish token create ...)"` captures it. Tokens don't
expire unless you give `--ttl-days`. `revoke` refuses to remove the last admin
token; create its replacement first.

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
through.

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
pull-through cache aren't checked, so pin those by digest. `relish sign`
attaches a signature to an image already in the registry, but only takes a
`sha256:` digest, and it signs with a key `keys` can't list, so it won't get an
image past `require_signatures`.

## Between nodes

`relish init` generates the root, node, workload and ingress CAs. Nodes renew
their certificates at the midpoint of their validity without restarting, and a
new node joins with a single-use token and a certificate signing request, so
its private key never leaves it. Every node needs the cluster's master key
(`*-master.key` from `init`): it unwraps the CA keys and the secret keys, and
seals council backups. Keep it safe and backed up.
