# Upgrades, GitOps and backups

The day-two jobs: moving to a new version, letting a Git repository drive the
cluster, and getting the council back when everything else has gone wrong.

## Upgrading bun

`relish upgrade` rolls a new `bun` across the cluster: workers first
(`--parallel` at a time), then council members one by one, the leader last.
The new binary adopts running workloads without restarting them, and one that
crash-loops on boot reverts to the previous version by itself.

```sh
relish upgrade check                  # is there a newer release?
relish upgrade plan v0.2.0            # the rolling order and an estimate
relish upgrade start v0.2.0           # download, verify, roll
relish upgrade status
relish upgrade resume                 # continue a paused upgrade
relish upgrade abort                  # end a paused upgrade that moved no node
relish upgrade rollback v0.1.0        # also replaces a paused upgrade
```

It needs three things on every node:

- **A supervisor that restarts bun whenever it exits**, such as systemd with
  `Restart=always`. Bun replaces itself with the new binary, and recovering
  from a bad one depends on being started again.
- **A versioned binary directory.** `bun` is a symlink to `bun-vX.Y.Z`, and
  the previous versions stay beside it for rollback (`[upgrades]
  retain_versions`, default 3). `relish setup` installs it this way.
- **Two signatures for network upgrades**: the release's, checked against the
  key compiled into the running binary, and your own, from the key you name in
  `[upgrades] external_signing_key`. Generate that keypair with
  `relish dev keygen --out keys/` (or
  `openssl genpkey -algorithm ed25519 -outform DER -out operator.key`) and
  countersign each release binary with
  `relish dev countersign-binary --external-key keys/release.key bun-v0.2.0`.
  That adds your signature to the release's `bun-v0.2.0.sig` and leaves the
  release signature as it is; it also prints the `ed25519:…` public key to
  put in node.toml.

`relish upgrade start --binary ./bun-v0.2.0` rolls a local binary instead of
downloading one, with its signatures in `bun-v0.2.0.sig` beside it (or
`--sig`). On a single node that's an air-gapped upgrade and needs only the
release signature. In a cluster the other nodes fetch the binary from the
registry, which counts as the network, so every node wants both signatures:
countersign it first.

relish pushes the binary to the registry of the node it's connected to and
tells the other nodes to fetch it from that node's cluster address. From a
laptop running a `relish local` cluster, the push goes through the registry
forward and works without flags. `--registry host:port` names one address for
both the push and the fetch.

`start` refuses a candidate with the version the nodes already run but
different bytes: build it with a new version instead. If the bytes are
identical it reports that there's nothing to do. Moving to an older version
needs `--allow-downgrade` (note that `v0.2.0-rc.1` is older than `v0.2.0`);
`relish upgrade rollback` goes back to a retained version without it.

`start` also asks every node whether it can verify a cluster upgrade. A node
without `[upgrades] external_signing_key` would refuse the binary, so `start`
fails there and names the node, and nothing is recorded.

A node that refuses or reverts pauses the upgrade, and a paused upgrade blocks
every new `start`. There are three ways on: fix the cause and
`relish upgrade resume`; `relish upgrade abort`, which ends the upgrade when no
node has moved to the new version yet; or `relish upgrade rollback <version>`,
which replaces the paused upgrade and walks every node, moved or not, back to
that version. `abort` refuses once a node has moved, and says which, because
ending the upgrade then would leave the cluster on two versions.

Rolling upgrades need matching protocol and state formats; `bun --compatibility`
prints what a binary supports. Development builds' state isn't migrated.

## GitOps

Point the config at a repository and the council leader keeps the cluster in
step with it. It merges the TOML files under `path` (an app declared in two
files is an error) and validates the result like `relish apply`. Unlike
`relish compile`, it doesn't derive namespaces from directories or read
`_defaults.toml`, so set `namespace` in each app. And unlike `apply`, which
only adds and updates, GitOps reconciles: delete an app from the repository
and it goes from the cluster.

```toml
[gitops]
repo = "https://deploy-bot:TOKEN@git.example.com/org/infra.git"
branch = "main"                  # default
path = "/"                       # default
poll_interval_secs = 30          # default
require_signed_commits = true
trusted_signing_keys = ["<GPG or SSH key fingerprint>"]
webhook_secret = "a-long-random-string"
```

The sync always reads every `.toml` file under `path`, subdirectories
included. Credentials go in the URL; Bun strips them out of the process arguments and
Git's config. With `require_signed_commits`, a commit that doesn't verify
against `trusted_signing_keys` isn't applied, and an empty key list refuses
everything. There's no `relish gitops` command: the web dashboard's GitOps
page shows the last sync.

To sync on push rather than on the next poll, point a GitHub, Gitea or GitLab
webhook at `https://NODE:9117/v1/gitops/webhook` with the same secret. The
endpoint needs no token, only the HMAC signature (`X-Hub-Signature-256`) or
GitLab's `X-Gitlab-Token`. It refuses replays and is rate-limited
(`webhook_rate_limit`, 10 a minute by default).

## Backing up the council

The council holds the cluster's desired state: apps, jobs, tokens and the
rest. Turn on sealed backups to object storage, and you can rebuild a cluster
that has lost every council member:

```toml
[cluster.backup]
url = "s3://backups/prod-council"   # or file://, gs://
interval_secs = 300                 # default
retain = 24                         # default
```

The leader writes one every `interval_secs`, encrypted with a key derived from
the cluster's master key. Volumes aren't in it; snapshot those separately (see
`images-and-volumes`).

If every voter is gone, stop a surviving node and recover it:

```sh
relish council recover --data-dir /var/lib/reliaburger/data \
  --from s3://backups/prod-council --master-key /etc/reliaburger/master.key
```

Without `--from`, it uses the node's own latest snapshot. It wipes the dead
council's log and stamps a new recovery epoch; starting the node brings up a
one-voter council that grows again as nodes rejoin. Anything written after the
backup is lost. It refuses while it can still see a live council; `--force`
skips that check, and using it against a cluster that's still alive splits
the brain.
