# Images and volumes

## Where images come from

An app's `image` can name any public or private OCI registry. Every node
runs Pickle, the built-in registry, and by default external images go through
it as a pull-through cache: the first pull fetches from upstream, and later
pulls anywhere in the cluster come from peers.

```sh
relish images             # what the cluster's registry holds
```

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

Relish uploads the context to the registry at `localhost:5050`
(`--registry-port` changes the port), so run it on a node or through a forward
to one. A node then builds it with Buildah, which must be installed there, for
`linux/amd64` and `linux/arm64` by default. A build has
15 minutes (`[images] build_timeout_secs`) and a 256 MiB context. Refer to the
result by its bare name, as `api:v1.2.3`, and nodes find it in Pickle. Built
images are signed by the cluster, which matters when `require_signatures` is
on (see `security`).

## Volumes

```toml
[[app.db.volumes]]
path = "/var/lib/postgresql/data"   # managed: Bun creates and owns it
size = "10Gi"                       # optional

[[app.db.volumes]]
path = "/import"
source = "/srv/import"              # host path: mounted as it is
```

A managed volume lives under the node's `[storage] volumes` directory
(`/var/lib/reliaburger/volumes` by default), one per app and mount path, and
survives restarts and redeploys. Bun hands it to the container's user the first
time it's mounted. It stays on its node, so it's local storage, not a
network volume. A host-path volume is never chowned: make it readable (or
writable) by the container's mapped user yourself.

## Snapshots

When the volumes directory is on Btrfs, each managed volume is a subvolume,
`size` becomes a quota, and snapshots are instant copy-on-write:

```sh
relish snapshot create db --volume /var/lib/postgresql/data --name before-upgrade
relish snapshot list db
relish stop db
relish snapshot restore db before-upgrade
relish snapshot delete db before-upgrade
```

Restore overwrites the live volume, so stop the app first. On other
filesystems, snapshot commands fail with an error saying so. For scheduled
snapshots, optionally uploaded as archives to object storage:

```toml
[storage.snapshots]
interval_secs = 86400                 # 0 (the default) disables it
retain = 7                            # newest N per volume
upload_url = "s3://backups/volumes"   # optional; file:// and gs:// too
```

Object-storage credentials come from each backend's standard environment
variables.
