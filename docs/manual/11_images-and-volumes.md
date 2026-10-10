# Images and volumes

## Where images come from

An app's `image` can name any public or private OCI registry. Every node
runs Pickle, the built-in registry, and by default external images go through
it as a pull-through cache: the first pull fetches from upstream, and later
pulls anywhere in the cluster come from peers.

```sh
relish images             # what the cluster's registry holds
```

A multi-platform image is one row, with the platforms it offers. Images
pulled through the cache live under `cache/<registry>/<repository>`, and the
columns widen to fit those long names:

```text
REPOSITORY                          TAG     PLATFORMS                 LAYERS     SIZE
burger                              v1      linux/amd64, linux/arm64       -   9.4 MB
cache/ghcr.io/stefanprodan/podinfo  <none>  linux/amd64, linux/arm64       -  33.0 MB
```

LAYERS is `-` because each platform has its own. `relish images --output json`
lists them under `platforms`, each with its own manifest `digest`, `layers`
and `total_size`; a single-platform image has no `platforms` field. The
image's `digest` is the index's, which is what a deploy of `burger:v1`
verifies and pins.

Private registries take credentials from environment variables that Bun reads
at startup, so the password never sits in the config:

```toml
[[images.external_registries]]
host = "ghcr.io"
username = "bot"
password_secret = "GHCR_TOKEN"   # the name of an environment variable
```

`mirrors` sends digest-pinned pulls to a mirror first, falling back to the
upstream; tag references never use a mirror. Bun verifies the digest chain
whichever registry answers, so a bad mirror can slow a pull but can't change
what runs:

```toml
[images]
mirrors = { "ghcr.io" = "mirror.internal:5000" }
```

## Tags bind to digests at apply

A tag moves. `nginx:1.27` today needn't be `nginx:1.27` next week, so the
node that handles your apply (the leader, on a cluster) asks which manifest
the tag names right now and stores both:

```text
$ relish apply web.toml
  web: nginx:1.27 → sha256:3f2a1b9c04d7... (from the registry)
  app web: committed to the cluster
```

The app's image is now `nginx:1.27@sha256:3f2a…`. The digest decides what
every pull fetches, so every node, every restart and every replacement runs
the same bytes, and you still read the tag. Because a pinned pull may use a
mirror, `mirrors` now covers every image you apply.

- **Apply again to move.** Applying the same file re-resolves the tag, which
  is how you pick up a new `nginx:1.27` on purpose. The binding line shows it.
- **Already pinned?** An image you write as `name@sha256:…` or
  `name:tag@sha256:…` is stored as written.
- **Registry down?** The apply fails with the registry's error and stores
  nothing, unless the pull-through cache holds the tag: then the apply binds
  the cached copy and says so (`from the pull-through cache; the registry did
  not answer`). The registry gets 20 seconds to answer.
- **Pickle images** bind to the digest the cluster's registry holds for the
  tag, without asking anyone else.
- **Where you see it:** `relish history` and `relish inspect` show the bound
  reference, as does each instance's image in `relish status`. `relish rollback`
  restores the bound reference of the version before, so it runs the bytes
  that ran then, even if the tag has moved since.
- **GitOps** binds the same way when it writes an app. Git keeps the tag, and
  a bound image isn't drift; the tag re-resolves when the app changes in Git.

Binding happens only where the leader's own runtime pulls images (runc, or
Apple containers on a Mac). Under the process runtime an app's `image` is a
placeholder nobody pulls, so it's stored as written. A cluster runs one
runtime kind, so the leader's speaks for every node.

## Storage ceiling

`[images] max_storage` bounds each node's compressed CAS blob, temporary upload
and repository authority receipt payload bytes. The store also allows at most
65,536 payload files, including empty uploads and receipts. Registry pushes, peer replication,
upstream cache fills and runtime image pulls share this ceiling. An upload chunk
that would exceed it is refused before writing, with HTTP 413 from the registry. Completed uploads remain charged
without a manifest; deleting confirmed payloads returns their space to the budget.
The accounting is rebuilt from disk at startup.

Unpacked root filesystems, catalogue files and filesystem overhead need
additional disk capacity. This cap measures payload bytes, not filesystem
block allocation. Reusing an existing verified blob does not charge its bytes
twice, but uploading another physical copy needs room for that temporary copy
until completion.

## Building images

`relish build` builds images from your config straight into Pickle:

```toml
[build.api]
context = "./api"               # relative to this file
dockerfile = "Dockerfile"       # default
destination = "pickle://api:v1.2.3"
args = { RUST_VERSION = "1.97" }

[app.api]
image = "api:v1.2.3"
port = 8080
```

```sh
relish build app.toml
relish apply app.toml
```

Relish uploads the context to the registry: through the quickstart's registry
forward (`127.0.0.1:15050`) on a laptop cluster, otherwise at `localhost:5050`,
so run it on a node or through a forward to one (`--registry-port` names the
port on this host). A node then builds it with Buildah, which must be installed
there (the quickstart's VMs have it). The five-minute tour builds
`examples/demo/burger` this way. A build has 15 minutes per stage
(`[images] build_timeout_secs`) and a 256 MiB context. Refer to the result by
its bare name, as `api:v1.2.3`, and nodes find it in Pickle. Built images are
signed by the cluster, which matters when `require_signatures` is on (see
`security`).

A build targets `linux/amd64` and `linux/arm64` unless `platform` says
otherwise (`platform = ["linux/arm64"]`). Pickle stores every platform under
the one tag, and each node pulls the one that matches its own architecture. A
build fails if a platform it asked for is missing from the result. Buildah
runs a `RUN` step for a foreign platform under emulation, which is slow or
missing, so a Dockerfile that cross-compiles (as the demo's does) builds both
platforms quickly.

A node builds one image at a time, in its own Buildah storage under
`<storage.data>/buildah`. After every build it removes the build's containers
and images and keeps base images for the next build, up to
`[images] build_cache_max_bytes` (100 GiB by default, 2 GiB on a quickstart
node; `0` keeps nothing). Past that, it removes every cached image.

A `RUN` step that uses the network gets Buildah's own bridge (`podman0`,
`10.88.0.0/16`), with Buildah's firewall rules next to Reliaburger's. The two
don't interfere: Reliaburger's firewall only drops traffic to its own ports
from outside the cluster, and that includes a build step trying to reach the
node's API or registry. Add `--network=none` to `RUN` steps that don't need
the network.

## Pushing with docker or crane

Pickle speaks the standard registry API, so `docker push`, `crane` and other
OCI clients work against a cluster whose nodes have mTLS identities (every
`relish init` cluster and the quickstart). Log in with an API token of role
`deployer` or above as the password; the username is ignored:

```sh
TOKEN="$(relish token create --name laptop-push --role deployer)"
crane auth login NODE:5050 -u push -p "$TOKEN"
crane push app.tar NODE:5050/api:v1
```

`docker login NODE:5050 -u push --password-stdin <<<"$TOKEN"` then
`docker push NODE:5050/api:v1` works the same way. Apps refer to the image
by its bare name, `api:v1`.

A token made with `--apps` or `--namespaces` is held to repositories named
`<namespace>/<app>`, for pushes and pulls alike: the first path segment is the
namespace, the rest is the app. A deployer scoped to `shop` may push
`shop/api:v1` and `shop/api/worker:v1`, but not `billing/api:v1`, and not a
bare `api:v1` or `library/redis` either, because those name no namespace it
owns. The registry answers such a request with 403 `DENIED`, and the same rule
applies to a `relish build` destination. From a routable registry a scoped
token pulls only its own namespaces' images (a loopback registry serves reads
to anyone on the node, token or not), and `relish images` lists only those.
Unscoped tokens push and pull anything, as before.

The registry serves the node's certificate, which names the node rather than
its address, so `NODE` has to be the node's name (`relish status` lists them)
and your machine has to resolve it and trust the cluster's root CA. For the
quickstart: add `127.0.0.1 NODE-1-NAME` to `/etc/hosts`, use port `15050`, and
trust `~/.reliaburger/clusters/NAME/security/identity/root-ca.crt` (for docker,
copy it to `/etc/docker/certs.d/NODE-1-NAME:15050/ca.crt`; on macOS, add it to
the keychain for crane).

Pickle only takes these credentials over TLS. A plaintext registry, which is
what a node without an identity serves, refuses them even with a good token,
because the password is your API token. There, use `relish build`, or push
anonymously to a standalone node's loopback registry before its first API
token exists. Pickle has no Docker token service, and it doesn't support
deleting images through the registry API.

## Multi-platform images

A multi-platform image (an OCI image index or Docker manifest list) in Pickle
runs on every node whose platform it offers. The node reads the index, picks
`linux/amd64` or `linux/arm64` to match its own architecture, and pulls only
that platform's layers. A node whose platform the index doesn't list refuses
the image and names the architecture it looked for, rather than running the
wrong one.

That covers `docker buildx build --platform linux/amd64,linux/arm64 --push`
and `relish build`, which builds both platforms by default and stores all of
them under one tag. To build just one, name it:

```toml
[build.api]
context = "./api"
destination = "pickle://api:v1.2.3"
platform = ["linux/arm64"]
```

Multi-platform images from an upstream registry work the same way through the
pull-through cache. The cache stores the upstream index under the tag, and
each platform's layers the first time a node of that architecture pulls the
image. So a cluster that mixes amd64 and arm64 nodes can run `redis:7`
straight from Docker Hub: every node gets its own platform, and each platform
comes from upstream only once.

## Volumes

```toml
[[app.db.volumes]]
path = "/var/lib/postgresql/data"   # managed: Bun creates and owns it
size = "10Gi"                       # optional

[[app.db.volumes]]
path = "/import"
source = "/srv/import"              # host path: needs a node allowlist entry
```

A managed volume lives under the node's `[storage] volumes` directory
(`/var/lib/reliaburger/volumes` by default), one per app and mount path, and
survives restarts and redeploys. Bun hands it to the container's user the first
time it's mounted. It stays on its node, so it's local storage, not a
network volume. An app with a managed volume stays with it too: after
`relish stop`, the next `relish apply` starts it again on the node that holds
its volume, and waits there if that node is short of room, not ready, or in the
middle of an upgrade. A running app stays put for the same reasons, and also
while its node is out of the cluster: restarting Bun, rebooting or a crash all
look the same to gossip, and the data is usually still there. While it waits,
`relish status` and `relish inspect` say which node it's waiting for, and
`relish wtf` reports it as critical. Only two things move it: decommissioning
the node with `relish decommission-node <node> --workloads-stopped --reason
<why>`, which writes its volumes off, or changing the app's
`placement.required` labels so that node no longer matches. Either way the app
starts elsewhere on a new, empty volume (restore a snapshot into it if you have
one). A bigger `cpu` or `memory` request doesn't move it, even when its node is
now short of room. A host-path volume is never chowned: make it readable (or writable) by
the container's mapped user yourself.

### Host paths

A `source` on a volume (mounted read-write) or on a `config_file` (read-only)
reaches straight into the node's filesystem, past the app's namespace. Every
rootful container on a node shares one user-namespace id range, so a host path
that pointed at another app's managed volume would hand its files to your
container. Two gates stand in the way:

- **Every node refuses host paths by default.** It mounts a `source` only when
  `[storage] allowed_host_paths` in its `node.toml` lists a prefix that covers
  it. Prefixes match by whole path component (so `/srv/import-evil` isn't
  under `/srv/import`), after symlinks are resolved, and a `source` with `..`
  in it is refused outright:

  ```toml
  [storage]
  allowed_host_paths = ["/srv/import", "/etc/ssl/certs"]
  ```

  The node's own directories are refused even under a listed prefix: the five
  `[storage]` directories, the identity directory, the script directory and
  the directories holding the master key and the security bootstrap. So is a
  path that contains one of them, such as `/var/lib`. A refused app fails its
  deploy on that node, and `relish apply` names the path and the reason.
- **A token with a `[permission]` block needs `host-exec`** as well as
  `deploy` for an app with any `source` (see `security`).

`relish lint` and `relish apply --dry-run` accept a `source`, since they can't
see any node's allowlist; `relish apply` prints a line for each one, reminding
you which nodes will mount it. Managed volumes and inline `config_file`
content need neither gate.

## Snapshots

When the volumes directory is on Btrfs, each managed volume is a subvolume,
`size` becomes a quota, and snapshots are instant copy-on-write:

```sh
relish snapshot create db --volume /var/lib/postgresql/data --name before-upgrade
relish snapshot list db
relish stop db
relish snapshot restore db before-upgrade
relish snapshot delete db before-upgrade
relish apply db.toml     # start it again
```

Restore overwrites the live volume, so stop the app first. On other
filesystems, snapshot commands fail with an error saying so.

A managed volume lives on the node that runs its app, and snapshot commands
act there whichever node you send them to: the node that receives one asks the
council where the app's volume lives and forwards the request, with your own
credential, to that node. A stopped app's volume is wherever it last ran. A
copy of the volume that an app left behind on another node is never
snapshotted or restored by mistake. When an app with several replicas keeps a
volume on each of several nodes, send the command to one of those nodes; any
other node refuses it with a 409 that names them.

While a restore runs it owns the app's volumes: `relish apply` for that app,
automatic restarts, and any other snapshot command for it get a "retry
shortly" refusal (a 409) until the restore finishes. The restored volume keeps
the original's `size` quota. If Bun dies mid-restore, it finishes or rolls back
the swap when it starts again, so the app sees either its old data or the
restored data, never a missing volume. When it can't tell which copy is right,
it keeps every copy (`<volume>.restore-staged`, `<volume>.restore-old`), logs
`needs manual recovery`, and refuses to mount that volume until you move the
right copy into place and delete the `<volume>.restore.json` journal.

`relish snapshot list` reports an error, rather than a shorter list, when
snapshot metadata can't be read.

`--volume` must be one of the app's own managed volumes, named by its
container mount path. A custom `--name` is 1 to 128 characters from
`A-Z a-z 0-9 . _ -` and can't start with a dot. Anything else is refused with
a 400. Without `--volume`, `create` snapshots every managed volume of the app
under one shared name. Restoring or deleting that name then needs `--volume`
to say which copy you mean:

```sh
relish snapshot restore db 1752000000 --volume /var/lib/postgresql/data
```

For scheduled snapshots, optionally uploaded as archives to object storage:

```toml
[storage.snapshots]
interval_secs = 86400                 # 0 (the default) disables it
retain = 7                            # newest N per volume; at least 1 when scheduled
upload_url = "s3://backups/volumes"   # optional; file:// and gs:// too
upload_timeout_secs = 3600            # deadline for archiving, then for uploading, each snapshot
```

Object-storage credentials come from each backend's standard environment
variables.

Each sweep takes new snapshots, uploads every snapshot the destination hasn't
confirmed, then prunes past `retain`. A snapshot that hasn't reached the
destination is never pruned: if the store is down for longer than the
retention window, snapshots pile up on the node (Bun logs how many it's
keeping) and ship once the store is back. Change `upload_url` and the new
destination receives every retained snapshot. `relish snapshot list` shows
which destinations hold each one.

Archives are streamed to a spool file in `<volumes>/.snapshot-spool` and
uploaded in 8 MiB parts, so a large volume doesn't need its size in memory.
The spool always leaves 5% of the volumes filesystem free, or 10 GiB on a
filesystem larger than 200 GiB; an archive that would cut into that reserve
fails and is retried on the next sweep. Objects land under
`<prefix>/<namespace>/<app>/<node>/<volume>/`: the archive is
`archives/sha256-<digest>.tar.gz`, and a JSON manifest in `manifests/` names
the snapshot, its volume, node and creation time. Two nodes running the same
app never overwrite each other's archives, and neither does reusing a
snapshot name.
