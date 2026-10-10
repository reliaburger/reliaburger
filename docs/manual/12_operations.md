# Upgrades, GitOps and backups

The day-two jobs: moving to a new version, letting a Git repository drive the
cluster, and getting the council back when everything else has gone wrong.

## Upgrading bun

`relish upgrade` rolls a new `bun` across the cluster: workers first
(`--parallel` at a time), then council members one by one, the leader last.
The new binary adopts running workloads without restarting them, and one that
crash-loops on boot reverts to the previous version by itself.

Until 1.0 it works only between builds of the same format. Every build speaks
one cluster protocol and one state format (`bun --compatibility` prints them),
and a node takes only a binary with its own pair. Every release from 0.1.0 to
0.2.0 changed one or the other, so none of them can roll onto the release
before it: moving between them means a fresh cluster, as described below.
`relish upgrade` is for a later build with the same pair, such as a patch release that doesn't
change a format, or your own rebuild. `relish upgrade check` tells you which
case you're in.

```sh
relish upgrade check                  # is there a newer release, and can it roll?
relish upgrade plan v0.2.1            # the rolling order and an estimate
relish upgrade start v0.2.1 --external-key keys/release.key
                                      # download, countersign, verify, roll
relish upgrade start --binary ./bun-v0.2.1   # a local, countersigned binary
relish upgrade status
relish upgrade resume                 # continue a paused upgrade
relish upgrade abort                  # end a paused upgrade that moved no node
relish upgrade rollback v0.2.0        # also replaces a paused upgrade
```

To see what a node runs, ask it. `bun --version` and `relish --version` print
the version and the commit it was built from, such as `bun 0.1.0 (3fcb1fd)`,
and so does bun's first log line. Two builds of the same version can hold
different code; the commit tells them apart. `GET /v1/version` has the full
commit in `commit`, next to `version` and `binary_sha256`.

It needs three things on every node:

- **A supervisor that restarts bun whenever it exits**, such as systemd with
  `Restart=always`. Bun replaces itself with the new binary in place, and
  recovering from a bad one depends on being started again. `on-failure`
  covers every exit bun makes on purpose; `always` also covers a release that
  exits cleanly while it's being verified. The quickstart uses `always`.
- **A writable binary directory.** Bun keeps `bun-vX.Y.Z` files beside its
  own binary and makes `bun` a symlink to the active one, with the previous
  versions kept for rollback (`[upgrades] retain_versions`, default 3). A
  plain `bun` binary is fine: the first upgrade copies it to `bun-vX.Y.Z` and
  turns `bun` into the symlink.
- **Two signatures for network upgrades**: the release's, checked against the
  key compiled into the running binary, and your own, from the key you name in
  `[upgrades] external_signing_key`. Generate that keypair with
  `relish dev keygen --out keys/` (or
  `openssl genpkey -algorithm ed25519 -outform DER -out operator.key`) and
  countersign each release binary with
  `relish dev countersign-binary --external-key keys/release.key bun-v0.2.1`.
  That adds your signature to the release's `bun-v0.2.1.sig` and leaves the
  release signature as it is; it also prints the `ed25519:…` public key to
  put in node.toml.

Published release metadata carries only the release's signature, never yours,
so the version form needs your key or your envelope. With
`--external-key keys/release.key`, relish countersigns each binary it
downloads before pushing it. Or countersign the downloaded binary yourself and
pass the envelope with `--sig bun-v0.2.1.sig`, which works when every node
runs on the same platform. Without either, `start` stops before it downloads
anything and says so.

relish downloads the build for the platform each node reports
(`platform` in `GET /v1/version`, such as `linux-aarch64`), not for the
machine relish runs on: from a Mac it still fetches the Linux builds the nodes
need. A cluster that mixes architectures gets one build per platform, each
node fetches its own, and `start` refuses before recording anything if the
release has no build for one of them.

`relish upgrade start --binary ./bun-v0.2.1` rolls a local binary instead of
downloading one, with its signatures in `bun-v0.2.1.sig` beside it (or
`--sig`). On a single node that's an air-gapped upgrade and needs only the
release signature. In a cluster the other nodes fetch the binary from the
registry, which counts as the network, so every node wants both signatures:
countersign it first. One local file is one platform's build, so the cluster
form refuses `--binary` when the nodes run on more than one platform.

relish asks the node it's connected to whether it belongs to a cluster
(`GET /v1/upgrade/cluster`). Only the node's own answer that it has no
council makes `start`, `status` and `rollback` act on that single node. Any
other failure, such as a timeout or a 5xx, stops the command with that error:
an upgrade that silently fell back to the connected node alone would skip the
rolling order, the quorum checks and the run record. On a single node, a
downloaded binary is staged in a fresh private directory for the node to read,
and removed once the node has answered.

relish pushes the binary to the registry of the node it's connected to and
tells the other nodes to fetch it from that node's cluster address. From a
laptop running a `relish local` cluster, the push goes through the registry
forward and works without flags. `--registry host:port` names one address for
both the push and the fetch.

Only the leader records and walks an upgrade, but you don't have to find it.
A council node that isn't the leader passes `start`, `resume`, `abort` and a
cluster `rollback` on to the leader with your own credentials, so the leader
checks your permissions as usual. The binary still goes to the registry of
the node you're connected to. A worker outside the council passes the calls
on too, but relish builds the `start` and `rollback` plans from the node list
of the node it's connected to, and a worker's list doesn't say which node
leads, so the leader refuses the plan. Run those two against a council node.

`start` refuses a candidate with the version the nodes already run but
different bytes: build it with a new version instead. If the bytes are
identical it reports that there's nothing to do. Moving to an older version
needs `--allow-downgrade` (note that `v0.2.0-rc.1` is older than `v0.2.0`);
`relish upgrade rollback` goes back to a retained version without it.

`start` also asks every node whether it can verify a cluster upgrade. A node
without `[upgrades] external_signing_key` would refuse the binary, so `start`
fails there and names the node, and nothing is recorded.

A cluster whose council has exactly two voters can't roll at all: taking
either one down for its swap leaves one, short of the two a quorum needs.
`start` and a cluster `rollback` refuse it and ask for a third node. Once a run
has started, losing a voter makes it wait in the council phase instead, and
the leader logs `cluster upgrade ... waiting: N of M voters alive`.

Then the leader does what every node will do with the candidate: it fetches
the binary from the registry, checks both signatures and runs
`bun --compatibility` on it. A release with a different protocol or state
format can't join the cluster, so `start` fails with both pairs and nothing is
recorded:

```text
refusing to upgrade to v0.1.6: incompatible binary: found protocol 46, state 63; this cluster (reliaburger v0.1.5 (…)) needs protocol 40, state 58. …
```

From 0.2.0 the release metadata names each release's pair (`compatibility` in
`metadata.json`), so relish can tell sooner. `relish upgrade check` reports a
release with another pair as needing a fresh cluster instead of offering the
command, and `relish upgrade start <version>` refuses it before downloading:

```text
v0.2.1 changes the cluster formats (protocol 51 -> 52, state 68 -> 68), so it needs a fresh cluster, not `relish upgrade start`; see …
```

A cluster `rollback` never downloads anything: each node goes back to a binary
already in its binary directory. So the leader first asks every node which
versions it holds (`installed_versions` in `GET /v1/version`) and refuses a
version any of them lacks, naming those nodes:

```text
cannot roll back to v0.1.0: it is not installed in the binary store on node node-2, node node-3. …
```

A node that ran or was upgraded from a version keeps it, up to
`[upgrades] retain_versions`.

A node that refuses or reverts pauses the upgrade, and a paused upgrade blocks
every new `start`. There are three ways on: fix the cause and
`relish upgrade resume`; `relish upgrade abort`, which ends the upgrade when no
node has moved to the new version yet; or `relish upgrade rollback <version>`,
which replaces the paused upgrade and walks every node, moved or not, back to
that version. `abort` refuses once a node has moved, and says which, because
ending the upgrade then would leave the cluster on two versions.

Rolling upgrades need matching protocol and state formats; `bun --compatibility`
prints what a binary supports. Nothing is migrated before 1.0.0: a release
that changes either format needs a fresh cluster. 0.1.1 is one: it moved the
state format from 44 to 46, so a 0.1.0 cluster refuses it and stays on 0.1.0
([upgrading from 0.1.0](https://github.com/reliaburger/reliaburger/blob/main/docs/releasing.md#upgrading-from-010)).
0.1.2 is another: it moved the protocol to 28 and the state format to 47, so a
0.1.1 cluster refuses it too
([upgrading from 0.1.1](https://github.com/reliaburger/reliaburger/blob/main/docs/releasing.md#upgrading-from-011)).
So is 0.1.3: protocol 33 and state format 49. A 0.1.2 cluster's leader refuses
it before recording a run, with the message above
([upgrading from 0.1.2](https://github.com/reliaburger/reliaburger/blob/main/docs/releasing.md#upgrading-from-012)).
And 0.1.4: protocol 34, state format still 49. A 0.1.3 cluster's leader
refuses it the same way
([upgrading from 0.1.3](https://github.com/reliaburger/reliaburger/blob/main/docs/releasing.md#upgrading-from-013)).
And 0.1.5: protocol 40 and state format 58. A 0.1.4 cluster's leader refuses
it the same way
([upgrading from 0.1.4](https://github.com/reliaburger/reliaburger/blob/main/docs/releasing.md#upgrading-from-014)).
And 0.1.6: protocol 46 and state format 63. A 0.1.5 cluster's leader refuses
it the same way
([upgrading from 0.1.5](https://github.com/reliaburger/reliaburger/blob/main/docs/releasing.md#upgrading-from-015)).
And 0.2.0: protocol 51 and state format 68. A 0.1.6 cluster's leader refuses
it the same way, and moving means recreating the cluster, which loses
everything the council held
([upgrading from 0.1.6](https://github.com/reliaburger/reliaburger/blob/main/docs/releasing.md#upgrading-from-016)).
So no published release has yet rolled in place onto another; the first that
can will be a release that keeps 0.2.0's pair.

A laptop cluster says the same thing when you rerun the quickstart installer
from a newer release over it. Its saved record names the release that set it
up, so setup refuses and names both versions:

```text
cluster "laptop" was set up with v0.1.0, and this installer is v0.1.1. Before 1.0, a release that changes the cluster's protocol or state format can't take over an older cluster: run `relish local destroy --yes` and set it up again, then re-apply your apps. …
```

### When bun refuses its data directory

Swap in a binary with a different state format and bun won't start. The first
line says what it found and what it needs, so even a truncated journal line
carries it:

```text
incompatible state format: found 58; this binary (reliaburger v0.1.6 (465fdeb)) needs 63. Pre-1.0 builds don't migrate state: …
```

A join between mismatched binaries fails the same way, starting
`cannot join: incompatible cluster formats: found protocol 28, state 47; …`.
The refusal is deliberate and leaves the data untouched. You have two ways on:
run the release that wrote the data (`bun --compatibility` on a candidate
tells you its pair), or move the data directory aside and recreate the
cluster. Don't write a new `state-format.json` by hand; the stamp is the only
thing standing between the new binary and data it can't read. The policy is
in [docs/releasing.md](https://github.com/reliaburger/reliaburger/blob/main/docs/releasing.md#compatibility-before-100).

## GitOps

Point the config at a repository and the council leader keeps the cluster in
step with it. It merges the TOML files under `path` (an app declared in two
files is an error) and validates the result like `relish apply`. Its watched
tree uses the same inherited `_defaults.toml` values and directory namespaces
as `relish compile`. The configured `path` is the root: its own name adds no
namespace, and defaults outside it are not inherited. A workload's explicit
namespace wins. GitOps reconciles deletions too: delete an app from the
repository and it goes from the cluster.

GitOps reconciles apps, namespaces and permissions. Any `[job.*]` declaration,
including a cron registration or a `run_before = ["app.web"]` migration, refuses
the whole commit before desired-state writes. The failed sync names the jobs;
the applied SHA stays at the previous successful commit. Use `relish apply`
with the migration and dependent app in the same manifest to execute their
ordering, or `relish batch` for batch work. Lettuce has no durable job identity
or dispatch path tied to a Git revision yet.

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
everything. List full fingerprints: the 40-hex-digit GPG fingerprint of the
signing key or its primary key (case and spaces don't matter), or an SSH
key's `SHA256:…` fingerprint exactly as `ssh-keygen -lf` prints it. Git checks
the signature against the node's own GnuPG keyring or
`gpg.ssh.allowedSignersFile`, so install the public keys there. Even without
`require_signed_commits`, a commit that adds, edits or removes an app's or
job's `script` needs a signature from one of `trusted_signing_keys`, and so
does any script on the first sync.

Every poll reconciles, not just polls that find a new commit: change or delete
a GitOps-managed app by hand and the next poll puts it back. Autoscaler
replica overrides and `relish stop` are left alone. A `git` command that runs
longer than two minutes is killed and the sync retried later, so a stalled
remote can't wedge GitOps. There's no `relish gitops` command: the web
dashboard's GitOps page shows the last sync.

To sync on push rather than on the next poll, point a GitHub, Gitea or GitLab
webhook at `https://NODE:9117/v1/gitops/webhook` with the same secret. The
endpoint needs no token, only the HMAC signature (`X-Hub-Signature-256`) or
GitLab's `X-Gitlab-Token`. It refuses replays and is rate-limited
(`webhook_rate_limit`, 10 a minute by default). A delivery refused for the
rate limit isn't counted as seen, so the provider's retry gets through.

A follower forwards an authenticated webhook to the leader. A clustered
`202 Accepted` confirms that the cluster has durably accepted the delivery;
the sync finishes later and pending work survives a leader change. A push
arriving during a sync remains pending for a later run. If leadership or
replication cannot be confirmed, the endpoint returns `503`; retrying the
original delivery ID is safe. Admission has one five-second budget, including
waiting for the validator, resolving the leader and forwarding or replicating
the trigger. Timing out releases the local rate and replay reservation.
The cluster retains the most recent 1,000
committed delivery IDs across restarts and leader changes. A delivery still
in that inventory is refused as a replay; one that was not admitted can be
tried again. Older delivery IDs can be admitted again after eviction.

Durable webhook admission changes both protocol and state formats. Check
`bun --compatibility` before upgrading. Before 1.0, recreate the cluster
with matching new binaries and re-apply the repository; older-format logs
and snapshots are refused rather than migrated.

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
`images-and-volumes`). Nor is the root CA's private key, which never enters the
council: back that up yourself with `relish ca backup`, sealed to a passphrase
or your own age key, and check it with `relish ca verify` (see `security`).

If every voter is gone, stop a surviving node and recover it:

```sh
relish council recover --data-dir /var/lib/reliaburger/data \
  --from s3://backups/prod-council --master-key /etc/reliaburger/master.key
```

Without `--from`, it rebuilds the state from the node's own Raft directory:
its snapshot, if it has taken one, plus every committed log entry after it.
That only works on a node that was a voter, and the log is encrypted, so pass
`--master-key` (it defaults to `/etc/reliaburger/master.key` when that file
exists). Entries the node hadn't seen committed are left out. It moves the dead
council's Raft directory aside (to `.raft-recovery-*/previous` in the data
directory, where you can delete it once the cluster is healthy) and stamps a
new recovery epoch. It refuses while the node is still running, and the next
start finishes a recovery that crashed part-way. Starting the node brings up a
one-voter council that grows again as nodes rejoin, even if its config still
lists `cluster.join` seeds. New members receive the whole restored state.
Anything written after the backup is lost. The restored state keeps its API
tokens, join tokens and certificate revocations: the node's
`security.bootstrap_path` file seeds only a brand-new cluster, never a
recovered one. Nodes that saw the old council publish a newer service
catalogue than the backup holds still follow the recovered one, because each
recovery starts a new range of catalogue generations; a leader of the replaced
council stays refused.

Before it starts, it asks the local agent whether any voter is still alive
and refuses if one is; `--force` skips that check. The node is stopped by
then, though, so in practice nothing answers and the check passes. Make sure
yourself that the other voters are really gone: recovering a cluster that's
still alive splits the brain.

You need `--force` when a majority is gone but not every voter, say two of
three. The survivor can't regrow the council alone (changing membership needs
a quorum too), so you stop it and recover it as above.

### When the old voters come back

The voters you gave up on still hold the old council in their data
directories. Two of three old voters are a majority of it, so on their own
they would elect a leader and take writes: a second council. The recovery
epoch stops that, as long as they can reach the recovered side:

- A restarted voter waits a few seconds for gossip to show its peers'
  epochs before it serves Raft. If any peer holds a newer epoch, it fences
  itself.
- A voter that talks to the recovered council, or to an already fenced
  peer, is refused with the newer epoch and fences itself on the spot.
- A fenced node serves no Raft, refuses writes, claims no leader, and stays
  fenced across restarts. It keeps running its workloads as a worker.

`relish council status` shows such a node as `fenced`, `relish status` prints
a warning line, and `relish wtf` reports it as CRITICAL until you re-enrol it.
Stop the node, then:

```sh
relish council re-enrol --data-dir /var/lib/reliaburger/data
```

That removes the replaced council's Raft state (its log, snapshot and fence
record) and nothing else. Start the node with `cluster.join` pointing at the
current council: it joins as a fresh member, adopts the council's epoch, and
the reconciler can promote it to voter. `re-enrol` refuses on a node that
isn't fenced; `--force` overrides that for an old voter that never heard of the
recovery.

The fence has one gap. An old voter learns of the new epoch only from a node
that holds it. **After `council recover --force`, don't start the old voters
where they can reach each other but not the recovered side.** Wipe them with
`relish council re-enrol` first, or keep them stopped until they can see the
recovered council. If two of them do come back cut off from it, they form a
council of their own; `relish council status` and `relish wtf` show it as two
recovery epochs and two leaders.
