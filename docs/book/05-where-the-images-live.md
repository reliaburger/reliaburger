# Where the Images Live

Up to now, every node in the cluster pulls images directly from Docker Hub. That works, but it's slow (every node downloads the same layers), fragile (Docker Hub rate limits and outages), and leaks information (your internal image names are visible to the registry).

This chapter builds Pickle, Reliaburger's built-in OCI image registry.

## Why not just use Docker Hub?

Three reasons.

First, speed. A 500MB image pulled from Docker Hub takes seconds over a good connection. Pulled from a node two racks away? Milliseconds. With Pickle, you push once, and the cluster replicates internally. Subsequent nodes never touch the internet.

Second, reliability. Docker Hub has rate limits (100 pulls per 6 hours for anonymous users) and goes down from time to time. When it does, nobody can deploy. With Pickle, your images are stored on cluster nodes. The registry is the cluster.

Third, simplicity. No external registry to manage, no credentials to rotate, no network policies to allow outbound HTTPS to Docker Hub from every node. One less thing to break.

## Content-addressed storage

Every OCI image is a stack of layers. Each layer is a tar.gz file containing filesystem changes. A manifest ties them together: it lists every layer by its SHA-256 digest, plus a config blob that holds metadata (entrypoint, env vars, labels).

Pickle stores blobs by their digest:

```
/blobs/sha256/{hex}/data
```

This layout is the same one our Phase 1 `ImageStore` already uses for Docker Hub pulls. Pickle inherits it. A blob pulled from Docker Hub is immediately visible to Pickle, and vice versa. No copying, no conversion.

The `Digest` type enforces this invariant:

```rust
pub struct Digest(pub String);  // "sha256:abcdef..."

impl Digest {
    pub fn new(s: &str) -> Result<Self, PickleError> {
        // Must be sha256:{64 hex chars}
        Self::validate(s)?;
        Ok(Self(s.to_string()))
    }
}
```

If you try to construct a `Digest` with the wrong format, you get an error at the point of creation, not somewhere deep in a filesystem operation.

## The OCI Distribution API

`docker push` and `docker pull` speak a specific HTTP protocol: the OCI Distribution Spec. Pickle implements the subset that matters.

**Pushing an image** takes three steps:

1. Upload each layer blob (POST to initiate, PATCH to send data, PUT to complete with digest verification)
2. Upload the config blob (same flow)
3. Push the manifest (PUT with the full manifest JSON, server verifies all referenced blobs exist)

**Pulling an image** is simpler:

1. GET the manifest by tag or digest
2. GET each layer blob by digest

The handlers are axum routes mounted under `/v2/`. They share the same server as the agent API (`/v1/`), which means authentication, TLS, and connection handling are already in place from Phase 4.

## Upload sessions

Blob uploads happen in chunks. The client initiates a session, sends data in one or more PATCH requests, then finalises with a PUT that includes the expected digest. If the SHA-256 of the received data doesn't match, the upload is rejected.

```rust
pub async fn complete_upload(
    &self,
    upload_id: &str,
    expected_digest: &Digest,
) -> Result<(), PickleError> {
    let data = tokio::fs::read(&upload_path).await?;
    let actual = compute_sha256(&data);
    if actual.as_str() != expected_digest.as_str() {
        return Err(PickleError::DigestMismatch { expected, actual });
    }
    tokio::fs::rename(&upload_path, &blob_path).await?;
    Ok(())
}
```

The rename is atomic on the same filesystem. No partial reads, no corruption.

## Say no before you say Created

Step 3 of the push flow claims the server "verifies all referenced blobs exist". For a long time that was a lie. The first version of `manifest_put` parsed the body on a best-effort basis and returned 201 Created for almost anything: invalid JSON, made-up media types, descriptors pointing at blobs nobody had ever uploaded. Worse, we had a test called `push_manifest_with_missing_layer_returns_400` that asserted *Created* — the test name described the contract we wanted, and the assertion pinned the bug in place. When the Phase 12b review re-read the registry (finding REG3), the fix started by flipping that assertion. Tests first cuts both ways: a wrong test is a bug with a seatbelt on.

The validated contract is short. Before storing or committing anything, a manifest PUT must:

1. Parse as JSON.
2. Carry a known media type — an OCI image manifest, a Docker schema 2 manifest, or an image index / manifest list. The media type can be embedded in the body or arrive in the `Content-Type` header (the spec allows either; buildah tends to use the header).
3. Reference only blobs the registry already holds, with sizes matching what's actually on disk. The OCI push order guarantees blobs land before the manifest, so a missing blob means a broken or malicious client, not bad timing.
4. If pushed by digest (docker pushes the sub-manifests of a multi-arch image as `PUT …/manifests/sha256:…`), the digest must match the bytes.

Each rejection returns an OCI Distribution error body, `{"errors": [{"code": …, "message": …}]}`, because that's the shape docker and podman know how to print. `MANIFEST_BLOB_UNKNOWN` for a missing blob, `MANIFEST_INVALID` for everything malformed. A rejected manifest leaves no trace: no blob written, no tag created — there's a test asserting exactly that, because "validate, then store" is easy to get backwards and the original code did (it wrote the blob first, then looked at the body).

One subtlety worth keeping: the registry stores the manifest's *raw bytes*, not a re-serialisation of what it parsed. Content addressing demands it. If you parse JSON and print it back, key order and whitespace change, the SHA-256 changes, and every client that pulls by digest gets a mismatch. The `manifest_get_returns_byte_identical_body` test pushes a manifest with deliberately quirky formatting and asserts the GET returns it byte for byte.

## Replication

When you push an image, Pickle doesn't just store it locally. It replicates the layers to N peer nodes (default: 2 total copies) before returning success. If a node dies, the image is still available elsewhere.

Replication uses the same OCI Distribution API that clients use. Each peer already runs the `/v2/` handlers, so the replicating node simply acts as a push client to its peers. No custom protocol, no new code paths to test.

Peer selection prefers nodes that don't already hold the layers. Before uploading, the replicator sends a HEAD request to check — if the peer already has the layer (from a previous push or pull-through cache), it's skipped. This makes re-pushing an updated image fast: only the changed layers transfer.

## The manifest catalog

Which images exist? Which tags point where? Which nodes hold which layers? All of this is Raft state.

When a push completes, Pickle proposes a `ManifestCommit` to Raft:

```rust
pub struct ManifestCommit {
    pub manifest: ImageManifest,
    pub tag: String,
    pub holder_nodes: BTreeSet<u64>,
}
```

The state machine applies it: stores the manifest, creates the tag→digest mapping, and records which nodes hold each layer. Every council member has the same view. When a worker needs an image, it reads the Raft state to find a peer that holds it.

## Garbage collection

Disk space isn't infinite. Pickle runs a periodic GC sweep that deletes unreferenced layers, with three safety rails:

1. **Active reference protection.** If an app in `DesiredState` uses an image, none of its layers are touched.
2. **Sole-copy protection.** If this node is the only one holding a layer, it's never deleted, even if unreferenced. You can't accidentally destroy the last copy.
3. **Retention window.** Recently pushed images are kept for `gc_retain_days` (default 7) even if no tags reference them. This gives you time to notice and re-tag.

After deletion, the node proposes a `GcReport` to Raft, which removes it from the layer holder sets. Because Raft proposals are serialised, two nodes can't simultaneously believe they're "not the sole copy" and both delete.

## Reachability is the whole game

In a content-addressed store, garbage collection has exactly one job: compute the set of blobs reachable from the roots, and delete the rest. That's it. There's no reference counting, no ownership, no "who allocated this". If a digest is reachable from something that matters, it stays; if not, it goes. Which means the entire correctness of GC hangs on one question: did you enumerate the roots completely?

We didn't. For over six phases, `ImageManifest::all_digests()` returned the config digest and the layer digests — the blobs you need to *run* the image. But `manifest_put` also stores the manifest's own raw bytes as a content-addressed blob, because `docker pull` fetches the manifest by digest and content addressing wants the exact bytes back. That blob was in GC's swept set (it's on disk, `list_blobs` finds it) but never in the protected set. Holder tracking skipped it too, so the replication loop never copied it anywhere, and to the arbiter it looked like an untracked orphan. The one-hour orphan grace window kept it alive between sweeps on a busy registry, which is why nobody noticed. Wait past the grace window, run GC, and the tagged manifest's own bytes vanish. The catalogue still lists the tag; the GET returns 404. Every layer perfectly preserved, image unpullable. (Finding REG1 in the Phase 12b review — the only P0 the re-validation confirmed at full strength.)

The fix is one authoritative definition of "everything this tag pins":

```rust
/// Every digest this catalogue entry pins in the blob store: the
/// manifest's own blob, then the config and layers.
pub fn referenced_digests(&self) -> Vec<&Digest> {
    let mut digests = vec![&self.digest];
    for digest in self.all_digests() {
        if !digests.contains(&digest) {
            digests.push(digest);
        }
    }
    digests
}
```

Then an audit of every `all_digests()` call site, asking each one: do you mean "blobs to unpack" or "blobs this tag keeps alive"? GC protection, holder commits, the heal loop, peer pulls and "is this image fully local" all mean the latter and moved over; unpacking a rootfs still means the former and didn't. The audit is the real lesson. The bug wasn't a clever race — it was a set with one missing element, duplicated informally across five call sites. When one notion ("what does a tag pin?") lives in many places, they *will* drift; give it a name and a single function, and the compiler keeps the call sites honest.

There's an upgrade wrinkle. Catalogues persisted before the fix have no holder entry for manifest blobs — on disk they still look like orphans. Two properties make the old data safe without a migration. GC protection is computed from the catalogue's manifests, not from holder entries, so the manifest digest is protected the moment the new code loads an old catalogue. And the heal loop treats "no recorded holders" as "zero copies", the most urgent rarest-first case, so the next tick replicates the manifest blob and records real holders. Heal, don't collect: when old state is ambiguous, converge it towards safety rather than assuming the worst interpretation. A fixture test pins this — it rewrites a freshly persisted catalogue into the old shape, reloads it, and asserts GC keeps the blob while one heal tick restores redundancy.

The acceptance test for the whole story reads like the incident report we never had to write: push an image, run GC with the grace window at zero, assert the manifest GET still returns the exact pushed bytes, then have a second node pull the image from the first — manifest blob included — and serve the manifest itself.

## Peer pull, and a note on the pull-through cache

Once an image is in the catalog, a worker that doesn't hold it locally fetches the layers from a peer that does. `pull.rs` reads the Raft layer-holder set, picks a peer, and downloads each missing blob over the same `GET /v2/{repository}/blobs/{digest}` endpoint, verifying the digest before storing. That's live in Phase 5 — push once, and every other node pulls internally.

The tempting next step is a *pull-through cache*: your apps reference `alpine:latest` or `nginx:1.25`, and the first node to need one transparently pulls it from Docker Hub (via the `oci-distribution` client from Phase 1), stores the layers, and commits the manifest to Raft so the next node gets it from a peer. The plumbing is sketched in `pull.rs`, but wiring it end to end — intercepting the miss, caching upstream, committing to Raft — is deferred to Phase 12. For now, public base images are still pulled from Docker Hub per node; only images you've explicitly pushed to Pickle replicate across the cluster. We'll come back to it in Chapter 12.

## How it compares to Docker Hub

Let's walk through what deploying an image looks like with Docker Hub versus Pickle.

**Docker Hub workflow:**

1. Build your image locally
2. `docker login` (hope your credentials haven't expired)
3. `docker tag myapp:v1 myorg/myapp:v1`
4. `docker push myorg/myapp:v1`
5. On every cluster node, `docker pull myorg/myapp:v1` (hope Docker Hub is up, hope you haven't hit the rate limit)
6. If you're on a private repo, configure registry credentials on every node
7. Set up network policies to allow outbound HTTPS to `registry-1.docker.io` from every node

**Pickle workflow:**

1. Build your image locally
2. `docker push localhost:5000/myapp:v1` (Pickle's OCI API on the cluster)
3. Done. Pickle replicates internally. Every node can pull from its peers.

No login. No credentials to rotate. No rate limits. No outbound internet from worker nodes.

Now, Docker Hub does things Pickle doesn't try to do. It's a public registry with millions of images. You can browse, search, read READMEs, check vulnerability scans. Pickle is a private cluster registry, not a community marketplace. For public base images like `alpine` or `nginx`, you still reference Docker Hub in your config. The pull-through cache handles the rest.

The real comparison isn't features. It's operational burden. Docker Hub is a dependency you manage. Pickle is infrastructure you already have.

## What happens when Docker Hub goes down

It's happened before. In November 2020, Docker Hub had a major outage that broke CI/CD pipelines across the industry. In 2023, rate limiting changes caught teams off guard when their automated builds suddenly started failing with 429 responses. These aren't hypothetical risks.

When your registry is external, your deploy pipeline inherits its uptime. Docker Hub goes down? You can't deploy. Your cloud provider's container registry has a bad day? Same story. You're at the mercy of someone else's infrastructure.

With Pickle, the cluster *is* the registry. If the cluster is up, the registry is up. There's no separate SLA to track, no status page to monitor, no fallback to configure — for the images you've pushed. Build and push your own apps to Pickle and a Docker Hub outage can't stop you redeploying them; they live on cluster nodes and replicate between peers.

Public base images are the caveat until Phase 12. Today a node still pulls `nginx:1.25` from Docker Hub the first time it needs it. Once the pull-through cache lands, that first pull caches into Pickle and every subsequent deploy on any node comes from a peer — at which point Docker Hub could vanish and your existing deployments wouldn't notice. For now, the honest story is: your own images are outage-proof, public base images aren't yet.

## Volume size enforcement

Phase 1 added volume support with `VolumeSpec.size`, but the size field was ignored. Phase 5 enforces it.

On Linux, managed volumes with a size limit get a loop-mounted ext4 filesystem. The node creates a sparse file of the specified size, formats it with ext4, and mounts it. Writes that exceed the quota fail with ENOSPC — the kernel enforces it, not us.

On macOS, there's no loop mount. Reliaburger creates a plain directory and logs a warning. Size limits are soft-only on macOS. This is a development convenience, not a production limitation — production clusters run Linux.

## Under the hood: key patterns

### Validate at construction, not at use

The `Digest` type is a newtype around `String`, but you can't create one without going through `Digest::new()`, which validates the format. Every function that takes a `Digest` knows it's well-formed without checking again.

```rust
pub fn write_blob(&self, data: &[u8], expected_digest: &Digest) -> Result<(), PickleError> {
    let actual = compute_sha256(data);
    if actual.as_str() != expected_digest.as_str() {
        return Err(PickleError::DigestMismatch {
            expected: expected_digest.clone(),
            actual,
        });
    }
    let path = self.blob_path(expected_digest);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, data)?;
    Ok(())
}
```

Validate the digest *before* writing. The data hits disk only after verification passes. If we wrote first and checked after, a crash between write and check would leave a corrupt blob. Failure-first validation is a pattern worth internalising.

### Upsert with Vec, not HashMap

The `ManifestCatalog` stores manifests as `Vec<(String, ImageManifest)>` instead of `HashMap`. Why? Raft state must serialise deterministically. `HashMap` iterates in an undefined order — serialise it twice and you might get different bytes, which breaks Raft's log comparison. `Vec` preserves insertion order and serialises identically every time.

The trade-off is O(n) lookups instead of O(1). With thousands of images, you'd want a `BTreeMap` (deterministic order). With dozens — which is the realistic case for a single cluster's registry — a linear scan is faster because it avoids the overhead of tree rebalancing and hashing.

```rust
pub fn apply_manifest_commit(&mut self, commit: &ManifestCommit) {
    let digest_str = commit.manifest.digest.0.clone();
    let tag_key = format!("{}:{}", commit.manifest.repository, commit.tag);

    // Remove old tag pointing to a different digest
    self.tags.retain(|(k, _)| k != &tag_key);
    self.tags.push((tag_key, digest_str.clone()));

    // Upsert: add tag to existing manifest, or insert new
    if let Some((_, existing)) = self.manifests.iter_mut().find(|(d, _)| d == &digest_str) {
        existing.tags.insert(commit.tag.clone());
    } else {
        let mut manifest = commit.manifest.clone();
        manifest.tags.insert(commit.tag.clone());
        self.manifests.push((digest_str, manifest));
    }
}
```

The `retain` + `push` pattern for updating the tag list is idiomatic Rust for "replace if exists, insert if not" on a `Vec`. It's not the most efficient approach, but it's clear and correct. At registry scale (hundreds of tags, not millions), clarity wins.

### Axum extractors: parse, don't validate

The OCI API handlers show a pattern that axum encourages: let the framework extract and parse, then validate the domain logic yourself.

```rust
async fn blob_head(
    State(state): State<PickleState>,
    Path((_name, digest_str)): Path<(String, String)>,
) -> Response {
    let Ok(digest) = Digest::new(&digest_str) else {
        return StatusCode::BAD_REQUEST.into_response();
    };
    // ...
}
```

That `let Ok(digest) = Digest::new(&digest_str) else { ... }` is a *let-else*, a fairly recent Rust addition. It reads "bind `digest` if construction succeeded, otherwise run the `else` block — which must diverge" (here, by returning early). It's the clean way to peel a value out of a `Result` or `Option` and bail on failure without nesting the happy path inside an `if let`. A Go programmer would write `if err != nil { return ... }`; let-else gives you the same early-return shape while keeping `digest` in scope for the rest of the function.

Axum handles URL routing and parameter extraction. `Digest::new` handles domain validation. The handler glues them together. This separation means the `Digest` type works the same way whether it came from an HTTP path, a manifest JSON document, or a test fixture.

## What we learned

### Atomic rename is your friend

The upload session design is simple: temp file for in-progress data, atomic rename to the blob store when verified. No journal, no WAL, no transaction log. The filesystem is the state machine.

This works because rename on the same filesystem is atomic on Linux (and macOS). The blob is either fully present or absent, never half-written. A crash during upload leaves an orphan temp file that the next GC sweep cleans up. A crash during rename either completes or doesn't. No corruption either way.

### Don't invent a protocol when HTTP exists

Peer replication uses the same OCI Distribution API that Docker uses. The replicating node is literally a push client. This means: zero new code for the receiving side, the same error codes and retry semantics as a client push, and a protocol that every container tool already understands.

We considered a custom binary protocol (gRPC, or raw TCP with length-prefixed frames). It would have been faster for large layers. But "slightly faster" doesn't beat "zero new code to test" when you're moving blobs between nodes on a local network.

### Sole-copy protection prevents cascading deletion

Without sole-copy protection, GC on two nodes can race: both check the holder set, both see "two holders", both delete. Now nobody holds the layer.

An earlier edition of this section claimed Raft serialisation already fixed this. It didn't — and the gap between the claim and the code is instructive. The old flow was: check holders, *delete the blob*, then propose a `GcReport` to Raft. The proposal was serialised, sure, but the deletion had already happened before anyone arbitrated it. Two nodes could still both pass the local check and both delete; Raft just tidily recorded the data loss afterwards.

The real fix inverts the order. GC is now two-phase: `gc_candidates` *nominates* layers (deleting nothing), the node proposes the nominations, and the state machine — applying entries one at a time — decides which deletions still leave at least one holder. Its verdict travels back in the applied entry's response (`CouncilResponse::GcApproved`), the same pattern serial allocation uses, and only then does `delete_approved` touch the disk. The second node in the race gets an empty approval list for the contested layer. In single-node mode the same arbitration rule runs against the local catalogue, so the invariant holds everywhere: no deletion before a verdict.

There's one more race hiding in "orphaned" blobs: a layer being pushed right now has no holder entry yet, because its manifest hasn't committed. The old sweep classified those as orphans and deleted them mid-upload. Nominations now skip untracked blobs younger than an hour.

### Wiring the registry into the cluster

The July 2026 review found most of this chapter's machinery had no production caller: the catalogue was rebuilt empty on every boot (all image metadata lost on restart), pushes recorded a hardcoded holder set of `{0}`, and replication, pull, and GC were never scheduled. The wiring pass connected them:

- **Real holders.** Pushes record the pushing node's actual raft id — derived from the node name even in single-node mode. No more made-up constants.
- **Persistence.** The catalogue writes itself to `pickle-catalog.json` (temp-file-and-rename, as ever) after each commit and loads at boot. A corrupt file aborts startup: silently starting empty would orphan every blob on disk.
- **Raft.** Council members also propose each commit to Raft, making the replicated catalogue the cluster's source of truth. Worker nodes outside the council can't write to Raft yet — proposal forwarding arrives with the scheduler wiring — so their pushes stay locally persisted until then, and the commit message says so out loud rather than pretending.
- **Replication.** A leader-only loop compares each manifest's full-holder count against `[images] redundancy`, copies missing layers to gossip-selected peers over the ordinary OCI endpoints, and proposes the updated holder sets.
- **GC on a schedule**, per the two-phase protocol above.

The same pass fixed `relish build` (X1), whose context upload had been pointed at port 9117 — the Bun *API* port, which has no `/v2` routes — since the day it was written. It now uploads to the actual registry port, and `/v1/build` genuinely runs `buildah bud` and pushes the result back through the registry (or says plainly that it needs `buildah`, instead of returning an unconditional 501).

## Making the registry durable (and safe to expose)

The wiring pass connected the registry to the cluster, but a later review pointed at a harder question: is any of it actually *durable*, and is it safe to run outside a trusted network? The answer, honestly, was no on both counts. A push could tear on a crash, two nodes could serve different views of the same catalogue, and the listener spoke plain HTTP with no authentication at all. Six fixes closed the gap.

### fsync, then rename, then fsync again

"Atomic rename is your friend" is true, but incomplete. A rename is only atomic once the bytes it points at have actually reached the disk. The old `write_blob` did `std::fs::write` straight to the final path — no temp file, no fsync — so a crash mid-write left a half-written blob at exactly the name a reader trusts. And the catalogue's temp-and-rename used a *predictable* temp name (`catalog.json.tmp`), so two concurrent writers could stamp on each other's temp file.

The durable write is a fixed little dance:

```rust
fn write_file_durably(path: &Path, data: &[u8]) -> std::io::Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)?;
    let tmp = parent.join(format!(".{}.{:032x}.tmp", /* file name */, rand::random::<u128>()));
    { let mut file = std::fs::File::create(&tmp)?; file.write_all(data)?; file.sync_all()?; }
    std::fs::rename(&tmp, path)?;
    if let Ok(dir) = std::fs::File::open(parent) { let _ = dir.sync_all(); }
    Ok(())
}
```

Four steps, in order: write to a *unique* temp (the random suffix means two writers never collide), `sync_all` the file so its bytes are on disk, rename over the target, then `sync_all` the *directory* so the rename itself survives a crash. That last fsync is the one everyone forgets. A rename is a change to the directory's metadata; without syncing the directory, the OS is free to lose the rename even though the file data is safe. You'd reboot to find the temp file present and the final name missing.

Rust makes the "sync the directory" step feel odd — you `File::open` a *directory* and call `sync_all` on it. On Unix a directory is just another file descriptor, and syncing it flushes the directory entry. (On platforms where that isn't meaningful, the call is harmless.)

### Don't trust a cached blob — re-verify it

A blob on disk isn't automatically a *correct* blob. It could have been truncated by the very crash we just protected against, or rotted on a failing disk. The old peer-pull code short-circuited on `store.has_blob(digest)` — existence, not correctness. So a truncated cache entry would be served forever as if it were the real layer.

`revalidate_blob` re-hashes the bytes and, if they no longer match, deletes them so the next pull refetches clean:

```rust
pub fn revalidate_blob(&self, digest: &Digest) -> bool {
    let Ok(data) = std::fs::read(self.blob_path(digest)) else { return false };
    if compute_sha256(&data).as_str() == digest.as_str() {
        true
    } else {
        let _ = std::fs::remove_file(self.blob_path(digest)); // corrupt: drop it
        false
    }
}
```

The deploy path (`image_available_locally`) and the peer pull both call this instead of `has_blob`. Re-hashing every blob on every read would be wasteful, so we do it where it matters: before trusting a cache for a deploy, and before short-circuiting a peer pull.

### One rootfs per content, not per tag

Here's a subtle one. The unpacked rootfs used to live at `rootfs/{registry}/{repo}/{tag}/`, and unpacking *cleared and recreated* that directory. Now picture a tag move — `web:v1` re-pointed at new content — while a container is running out of the old rootfs. The re-extract does `remove_dir_all` on the directory the running container is living in. Two concurrent pushes to the same tag race the same way.

The fix content-addresses the rootfs: each set of layers unpacks into `…/{tag}/gen-{hash}`, where the hash is derived from the ordered layer digests. Different content lands in a different generation directory. The same content needs one more rule: publish it once, write a completion marker, then reuse it. Re-extracting an “identical” tree still starts by deleting the old one, which removes commands underneath a running container. `ImageStore` serialises generation publication across its clones and treats only a marked generation as reusable. A running container holds the path it was started with, and a tag move simply produces a *new* generation beside it. Nobody deletes anybody's live filesystem.

### One writable rootfs per workload

That fixed image publication, but not workload isolation. Two replicas still
received the same `gen-*` path with `root.readonly = false`. Replica A could
write `/etc/example`; replica B would immediately read A's file. No container
escape required. We had handed both containers the same ordinary directory.

The Linux answer is OverlayFS (a kernel filesystem that combines read-only and
writable directory layers). The shared generation becomes the lower layer and
each instance gets its own upper and work directories:

```text
images/.../gen-57d492d21ee15a2b       shared lower
bundles/default__web-0/rootfs-upper   replica 0 writes
bundles/default__web-0/rootfs-work
bundles/default__web-0/rootfs         mounted view passed to runc
bundles/default__web-1/rootfs-upper   replica 1 writes
bundles/default__web-1/rootfs-work
bundles/default__web-1/rootfs         a different mounted view
```

The acceptance test makes the race concrete. Two Alpine containers start from
one generation. One writes `alpha`, the other writes `beta`, and both sleep long
enough for the writes to overlap. Each later reads its own value. The test also
asserts that their OCI specs name different rootfs mountpoints. It failed on the
old code before either process check ran, because both paths were the same.

Now, who owns the upper? The workload instance does. A restart of
`default__web-0` on the same image remounts its old upper, so files survive the
restart. Bun can also die and adopt the still-running runc process without
disturbing its mount. A small marker records the canonical lower generation; if
the image changes, Grill clears the old upper instead of smuggling changes into
the new image.

Cleanup needs the same care as creation. `MountedRootfs` starts armed and its
`Drop` implementation unmounts the overlay if any later preparation step fails
or unwinds. Only a fully recorded bundle disarms it. Normal exit, timeout/kill
and failed adoption all use one cleanup function, which releases the mount but
doesn't touch the shared lower. The privileged tests force a `config.json`
write failure after mounting and check `/proc/self/mountinfo`; there is no mount
left behind. They repeat that check after natural exit and after killing an
adopted workload.

What about rootless runc? Mounting a host OverlayFS needs privilege, and we don't
yet ship a FUSE snapshotter. Read-only image roots can safely share the lower.
Writable ones fail before pull with an explicit error. That's less convenient,
but it doesn't quietly turn “rootless” into “every local workload shares one
writable filesystem”. We can add an unprivileged snapshotter later and keep the
same ownership contract.

### One authoritative catalogue

Pickle has two catalogues: the council's Raft-replicated `manifest_catalog`, and each node's local `PickleState::catalog`. The push path proposes to Raft; the read path — `manifest_get` and `tags_list` — read the *local* one. So a manifest a peer committed to Raft was invisible on a node that hadn't received the original PUT, until a heal tick happened to reconcile it. Push on node A, `docker pull` from node B, get a 404.

The fix is a one-liner in spirit: read the authoritative catalogue. Both handlers now call `catalog_snapshot()`, which returns the council's Raft catalogue when clustered and the local one otherwise — exactly what the P2P pull path already used. The moment Raft applies a peer's commit, every node's tag list and manifest lookup see it.

### Authenticate the writes, serve over TLS

The registry listener was plain HTTP with no auth — fine behind a firewall, a liability anywhere else. Rather than invent a registry-specific credential, it reuses the cluster's existing `sesame::auth`: the same bearer tokens and internal service token that guard the agent API. Loopback reads stay open, but a peer-reachable listener requires a valid user or service token. Writes require at least the `Deployer` role, or the service token that node-to-node replication presents. A tokenless *standalone* registry keeps a loopback-only bootstrap window so a first local push works. Clustered Bun normally derives a service token from its master key, so its peer-reachable registry requires that token from its first request. If the key is missing, reads and writes still fail closed instead of silently becoming anonymous.

The listener follows the same address other nodes already know. Standalone Bun keeps the
`127.0.0.1` default. In cluster mode that default becomes the gossip-advertised IP; a wildcard
covers it, while an explicit different interface is rejected. Warning that P2P won't work
wasn't enough. It left the cluster running with a feature the configuration claimed to
provide.

The authenticated capability response carries the selected socket, TLS and P2P state,
configured redundancy, current member count and the number of under-replicated catalogue
layers. Those are deliberately separate facts. Three live nodes make two copies *possible*;
only the holder sets prove whether the catalogue has achieved it.

TLS comes for free from the same PKI: when the node has an mTLS identity, the registry serves with `build_api_server_config` — the very config the agent API listener uses — and peers address each other as `https://`. The scheme is threaded through one function (`pickle_peers_scheme`) so the server and every peer-URL derivation always agree.

Two resource limits ride along. Storage **quotas** cap bytes per repository and across the whole registry (`0` means unlimited, the default). And upload **sessions now expire**: a chunked upload that goes quiet past its TTL is refused on its next chunk and swept, so an abandoned `docker push` can't leak a temp file forever. Both were review findings — a registry with no quota and no session expiry is a disk-exhaustion waiting to happen.

One more thing moved off the hot path: whole-blob hashing. Verifying a digest re-hashes the entire blob, which for a 500 MB layer is real CPU work. Running it on a Tokio worker would stall every other request on that thread, so it now runs under `spawn_blocking`:

```rust
tokio::task::spawn_blocking(move || store.write_blob(&data, &digest)).await?
```

The `move` closure takes ownership of the bytes, so nothing is borrowed across the `.await` — the borrow checker's way of proving the data outlives the blocking task.

### Refusing an attacker's redirect

Peer replication follows the OCI upload dance: POST to start an upload, read the `Location` header, PUT the blob there. The old code followed whatever absolute URL the peer returned. A compromised peer could therefore hand back `Location: http://attacker.example/collect` and make *this* node PUT the blob bytes — which may be a secret-bearing image — straight to the attacker. That's a textbook SSRF.

`resolve_same_origin_location` constrains the redirect to the peer's own origin: a relative path is resolved against the peer's base URL; an absolute URL is accepted only if its scheme, host, and port all match; a protocol-relative `//host/…` (which quietly swaps the host) is refused outright. The PUT never leaves for anywhere but the peer we were already talking to. Peer body reads are bounded too — a hard cap and the request timeout, so a hostile peer can't stream an unbounded body to exhaust memory or hold the connection open with a slow trickle.

There was a subtler memory hole behind that hard cap. The pull *enforced* the cap but still buffered the whole blob in a `Vec` before writing it — up to two gigabytes per layer, multiplied by every concurrent pull. The cap stopped a single hostile peer; it did nothing about a handful of honest large pulls landing at once. So the pull now streams: chunks go straight to the upload temp as they arrive, a running SHA-256 hashes them on the way past, and once the digest checks out the temp is committed into the blob store by an atomic rename — no re-read, no second copy. Peak memory per pull is one network chunk, whatever the layer's size. (The push handlers still buffer their request body; streaming those the same way is a Phase 15 job, flagged in `MAX_REQUEST_BYTES`.)

### Honest push semantics

A push commits locally and to Raft, then the heal loop drives it up to
`[images] redundancy` copies afterwards. The manifest PUT returns `201 Created`
with `OCI-Replication: pending` only after authoritative acceptance. A failed or
timed-out Raft proposal returns `503 Service Unavailable`; the client must retry.
An earlier version returned 202 with a custom `raft-uncommitted` header. Generic
OCI clients could treat that success-class response as a completed push without
reading our header. The status itself now communicates the missing authority.

The regression uses a real Raft node: an uninitialised council refuses the push,
then election followed by the same push succeeds and exposes the tag in cluster
state. It also checks the actual `CouncilResponse::Applied { log_index }` response
from the state machine. Accepting only its generic `Ok` variant would reject
successful commits. Rust's `|` pattern lets the match accept both success forms;
other response variants remain explicit failures.

The GC arbiter got stricter too. It used to recheck only sole-copy protection at deletion time. Now it rechecks against the *full catalogue reference set* immediately before approving: a blob any manifest still references — its config, a layer, or the manifest blob itself — is never approved for deletion, even if the nominating node saw it as an orphan when it built the report. A fresh push can re-reference a blob between nomination and approval; this serialised recheck is the last chance to refuse, and it takes it.

### A scheme is a decision, not a default

Once the registry can serve TLS, `http://` stops being a neutral default and becomes an assertion — one that three separate code paths were making on their own.

The build-context URLs hardcoded it. So did the `buildah push`, via `--tls-verify=false`. So did the self-upgrade binary fetch. None of them worked against a TLS registry, and where plaintext did reach a listener they moved a build context (the caller's whole source tree) in the clear and pushed the result without checking the certificate they were pushing to.

The fix is dull, which is the point: derive the scheme once, from the same condition that decides whether the registry gets a TLS identity, and thread it. `ApiState` carries `registry_scheme` beside `registry_port`, both server-owned, so a caller can't smuggle either. `bun` computes it next to `cluster_http` rather than in two places — deriving the same fact twice is how the two answers drift apart, and this fact is already used by the P2P and heal peer URLs.

Two details worth stealing:

**Which client, not just which scheme.** The build runner fetched its context with a bare `reqwest::get`. Point that at `https://` and it fails, because a default reqwest client trusts the public CA roots and our registry's certificate is signed by the cluster CA. So the scheme change is only half a fix; the request has to move onto the client that holds the trust anchors too. Any time you make something TLS-aware, check whether the *client* knows about your PKI.

**`--tls-verify` mirrors reality.** It's now `--tls-verify={registry_over_tls}` rather than a constant. Against a plaintext registry the flag is still false — that's not a compromise, it's accurate, and there's no certificate to verify. What changed is that the value now describes the world instead of assuming it.

The upgrade path is the interesting counter-example, because it's the one where none of this touched integrity. Binaries are content-addressed and dual-signed; `verify_binary` checks the sha256 and the embedded release signature on every path, plaintext or not. Nobody was going to slip you a modified binary. What plaintext actually cost was *working at all* against a TLS-only registry, plus telling anyone on the path which build you were rolling out. Worth fixing, worth being precise about why — "we added TLS so nobody can tamper with the binary" would have been a nice story and a false one.

The registry's read side moved too, though for a different reason. Reads were open on the assumption of a loopback bind, which is right: a local `docker pull` shouldn't need a token. Published on a routable address, that same openness hands every image in the cluster to anyone who can reach the port — including the `cache/` copies of private upstream registries, pulled with the operator's credentials. So reads now need a principal when the bind isn't loopback. The classifier is deliberately strict: only an IP literal that *is* loopback counts. A hostname could resolve anywhere, and could resolve somewhere else tomorrow; `0.0.0.0` reads like a local default while being the most exposed bind there is. Both count as routable, because the failure we'd rather have is "you needed a token and didn't expect to".

## Tests

Pickle is almost entirely testable in-process. A blob store is a directory, the OCI API is an axum router, and the catalog is a `Vec` — none of that needs the internet or another node. So the default suite spins up a Pickle server in the test, pushes a manifest and its blobs, then pulls them back, all without leaving the process.

### Unit tests — the registry without the network

The 104 tests in `src/pickle/` cover:

- **Digest and manifest** — `Digest::new` accepts well-formed digests and rejects everything else; manifests round-trip through serde unchanged.
- **Blob store** — write/read, upload sessions, and the digest-mismatch rejection path (`PickleError::DigestMismatch`).
- **OCI API** — `full_push_pull_round_trip` drives the real `/v2/` handlers end to end against an in-process server; plus the not-found paths (`blob_head_not_found`, `manifest_get_not_found`) that must return the right status codes. The manifest-validation contract gets a rejection matrix: invalid JSON, missing or unknown media type, size mismatch, malformed descriptor digest, missing referenced blob, and a happy path asserting the GET returns byte-identical bytes.
- **Garbage collection** — the safety rails get a test each: `gc_protects_sole_copy`, `gc_protects_active_deployment_images`, `gc_protects_tagged_manifest_layers`, `gc_protects_within_retention_window`, and the positive case `gc_collects_unreferenced_blob`. These are the tests that let you trust GC won't eat your last copy of a layer. `gc_never_nominates_a_catalogued_manifests_own_blob` pins the REG1 fix, and `tests/pickle_integrity.rs` runs the full push → GC → peer-pull acceptance sequence against real in-process registries.

### Hermetic protocol tests, provisioned runtime tests

The image-pull protocol belongs in the portable suite. An in-process registry serves a
digest-pinned synthetic image over loopback, so manifest fetching, blob digest validation,
unpacking and cache reuse don't depend on Docker Hub or a mutable tag.

Real runtimes are different. runc needs Linux and kernel capabilities; Apple Container
needs Apple silicon and nested virtualisation. Those tests compile with a reasoned
`#[ignore]` and run through named targets:

```sh
sudo make test-linux  # runc plus the other provisioned Linux/kernel suites
make test-apple       # manual Apple Container check
```

An explicitly requested suite asserts its prerequisites and fails if they are missing. It
never returns early and appears green without running. Chapter 15 explains the distinction
between `#[cfg]`, `#[ignore]` and an executed test in detail.

For an end-to-end smoke test of a real push and pull through Pickle on macOS:

```sh
make pickle-test-macos    # push/pull a real Docker image through Pickle (needs Docker Desktop)
```

### Running them

The default path needs nothing special:

```sh
cargo test --lib pickle       # the whole registry, in-process
```

Reach for the gated commands only when you want to exercise real images or real runtimes. The full env-var table lives in `docs/README.md`.

Phase 5 adds 72 tests, bringing the total to 867.

## Release hardening: a push shouldn't need a layer's worth of RAM

Push a 400 MiB layer to Pickle. Previously, the HTTP handler collected the
request into memory before checking authentication, and completion read the
upload file back into another allocation. Four clients could exhaust a small
laptop VM without running a single container.

The handler now authenticates before reading the body and writes each incoming
chunk to the upload file before requesting the next one. That gives us
backpressure: a slow disk slows the sender. Completion hashes the file with a
64 KiB buffer on Tokio's blocking pool, syncs it, then renames it into the
content-addressed store. The digest must match before the blob becomes visible.
Manifests still need parsing in memory, so they have a separate 4 MiB limit.

A semaphore allows four simultaneous write requests. A fifth receives HTTP 429
with `Retry-After: 1`; it doesn't sit in a queue retaining its body. Each upload
also owns a one-permit semaphore, so a PATCH can't change a file while a PUT
verifies it. An `OwnedSemaphorePermit` holds its semaphore through an `Arc`
(shared ownership), rather than borrowing the request's stack. Moving it into
`spawn_blocking` keeps the upload locked even if the client disconnects while
verification is running. Dropping the permit releases the lock automatically.
The expiry sweep skips uploads with an active writer.

Each request has a five-minute deadline and a 512 MiB byte limit. Failed body
reads discard their partial upload; abandoned sessions remain subject to expiry.
Both PATCH and PUT reject expired sessions and repository mismatches. These
limits bound active request processing, not total temporary disk usage; storage
quotas and the expiry sweep still matter.

The regression test sends half a body, waits until those bytes reach the upload
file, and only then sends the rest. An implementation that buffers until EOF
cannot pass it. Other tests leave an unauthorised body unfinished, saturate the
writer limit, and try to complete an expired session. This tests the behaviour
clients depend on, without relying on process memory measurements. The separate
release acceptance still needs to measure memory under real concurrent pushes
in the laptop VM.

### One shared cache means one path convention

The laptop test found two implementations of the same promise. `ImageStore`
wrote a layer to `blobs/sha256/<digest>`, while Pickle wrote it to
`blobs/sha256/<digest>/data`. Both pointed at the same base directory. Once the
runtime created a flat file, the registry could no longer create its directory.
The fallback then pulled upstream again instead of using the bytes on disk.

Both stores now use one path resolver. New blobs use the registry layout, and
existing flat files remain readable and writable. Enumeration recognises both
forms and ignores temporary filenames. A regression writes through the registry
and reads through the runtime, then repeats with a legacy flat file.

That exposed a second assumption: rootfs generation IDs hashed each layer's
filename. Every registry layer's filename is `data`, so replacing a layer could
reuse the previous rootfs generation. We now extract the digest from its parent
for that layout. The same ordered digests produce the same generation in both
layouts, and changed digests produce a new generation without touching a running
container's files. The tests exercise those properties directly.

Digest pins also contain a colon (`sha256:...`), as do registries with explicit
ports. That character separates lower layers in overlayfs mount options. We
encode it as `%3A` in rootfs directory components while preserving the original
OCI reference for registry requests. A path regression covers both a pinned
digest and a registry port; ordinary tag paths keep their existing layout.

### An upload location is not permission to forward credentials

The registry starts an upload by returning a `Location` header. Our catalogue
client used to accept any absolute URL there, then reuse its authenticated HTTP
client for PATCH and PUT. A response naming a different server could therefore
send the administrator's bearer to that server. Disabling automatic redirects
doesn't fix an explicit request made by our own code.

We now resolve both relative and absolute upload locations with `url::Url`, then
compare their origins. An origin includes the scheme, host and effective port:
changing any of those refuses the next request. Credentials embedded in the URL
and fragments are refused too. Query strings remain valid because registries can
use them to identify an upload session. POST and PATCH error responses stop the
upload before any location from that response is used.

The regression runs two HTTP servers. The first supplies a location on the
second; the second records whether it received an Authorization header. Before
the fix it did. After the fix, the client reports an origin violation without
contacting it. Separate cases cover protocol-relative URLs, TLS downgrades,
changed ports, and valid relative and same-origin absolute locations.


### Give a busy registry room to recover

A privileged CI run reached the public registry successfully, then lost its
pinned BusyBox pull to a `Rate exceeded` response. The other 42 runtime checks
passed. A laptop making its first pull can hit the same path.

External manifest/config reads and layer downloads retry recognised rate-limit
and temporary gateway/service errors. They also retry interrupted requests and
response streams, as described below. Each operation makes at most
four attempts, with roughly one, two and four seconds between them and a small
random delay to spread simultaneous nodes. One deadline covers every attempt:
30 seconds for manifest/config retrieval and 120 seconds per layer. A stalled
request cannot reset that budget. Authentication failures, missing images,
malformed responses and digest mismatches still fail.

The retry helper accepts a closure which creates a fresh future for each
attempt. In Rust, `FnMut() -> F` means a callable that may update captured state
and returns a value of type `F`; the `Future` bound says that value represents
asynchronous work. Each layer attempt creates a new byte buffer, so a failed
transfer's prefix cannot contaminate the next attempt. Only a complete,
digest-verified layer reaches the atomic cache publication step.

The pinned OCI client exposes structured error codes but discards response
headers on this path. Our backoff therefore doesn't claim to honour a server's
`Retry-After` value. Hermetic registry tests exercise transient recovery,
permanent denial and attempt limits; a stalled-response test advances time
only after the real HTTP request reaches the fixture. The external registry
qualification remains a separate check.


### Keep failed upload cleanup on the list

An upload times out. The reaper forgets its session, tries to remove the
partial file, and ignores the filesystem error. Who retries tomorrow? Nobody.
The next sweep has no record of that file.

Upload sessions now have two explicit states: `Active` and `Retiring`. Expiry,
a failed request body or a finalisation attempt fences future writers before
cleanup starts. An existing writer retains its semaphore permit until its own
operation ends. The reaper selects only retired or expired sessions whose
writer has exited. It forgets each session after file removal and directory
sync succeed; errors keep the owner available for another pass.

The reaper visits every selected session, collecting failures instead of
stopping at the first one. The HTTP test replaces one upload file with a
directory, which makes deletion fail even when the test runs as root. That
upload stays fenced while another expired upload disappears. Restore the file
and the next pass finishes both physical cleanup and ownership retirement.
The unit regression demonstrates the original loss: the second sweep returns
nothing before the fix. Upload recovery after process death needs a separate
startup owner because these session records live in memory.


### Recover uploads only after acquiring their directory

Killing Bun destroys its upload-session map but leaves partial files on disk.
The real restart test proves the gap: an upload accepts a chunk, Bun receives
SIGKILL, and its replacement serves requests while the partial file remains.

Before starting any registry or replication writer, Bun now acquires an
exclusive file lock for the configured image store. The kernel releases it
when the process exits, including an ungraceful exit. A second Bun using that
same writable store refuses startup; it cannot sweep the first Bun's uploads.
Self-upgrade closes the old descriptor during `exec`, so the replacement can
acquire ownership again. The lock file itself remains in place.

The owner reclaims regular temporary files whose names match our generated
upload IDs, then syncs the upload directory before startup continues. Clients
must restart interrupted uploads; we don't pretend to recover their lost
session metadata. Unexpected names, non-regular entries, a redirected upload
directory or an I/O error refuse startup. Recovery never follows a directory
symlink to remove someone else's files.

`UploadDirectoryOwner` keeps the open lock file alive. Its `#[must_use]`
attribute asks the compiler to warn when a caller discards the guard; Bun holds
it until registry shutdown. The blocking pool handles directory traversal,
locking and sync operations, keeping those calls off the async executor.
Tests cover competing owners, replacement, unknown entries, directory symlinks,
and the actual Bun SIGKILL path. The separate live upgrade suite checks that
rolling replacement and rollback can reacquire ownership.


### A dropped connection has no HTTP status

The rootless runtime CI gate failed while fetching an Alpine configuration blob:
the connection failed before a complete response arrived. Our retry branch only
looked for an HTTP status, so this failure escaped the retry policy altogether.

Registry reads now also recognise request, timeout and response-stream errors.
Reqwest labels interrupted byte streams as decode errors; the OCI client parses
manifests separately, and ImageStore verifies layer digests. These errors are
different from a complete malformed manifest or corrupt layer. The same four-attempt limit and original deadline apply.
Retries preserve authentication and TLS verification and start layer buffers
from empty.

The local registry fixture sends part of a successful response, then breaks its
body stream. The failing-first regression repeats that interruption for the
manifest, configuration and layer paths and verifies the final unpacked bytes.
A separate case sends complete malformed manifests or corrupt layers and requires immediate refusal;
existing cases still check denied access, persistent rate limits and a stalled
response. Passing those fixtures doesn't establish Docker Hub availability,
so the real cold-image runtime gate remains part of qualification.


The configuration-content case exposed a separate integrity gap: this path
fetched but ignored the configuration bytes without verifying their descriptor
digest. Inspecting the upstream client also showed that pinned manifest bytes
need explicit verification, including each link through an image index. C56
tracks that work; the transport retry change does not close it.


### A digest header isn't proof

Ask a registry for `image@sha256:...`. It can reply with different, perfectly
valid JSON and repeat the requested digest in `Docker-Content-Digest`. Parsing
that JSON proves nothing about its identity. Our first regression accepted the
changed configuration; the pull-through regression accepted a changed index.
Both responses carried plausible headers.

ImageStore and Pickle now use one verified fetch path. It hashes the exact root
bytes before parsing a pinned reference. If the root is an image index, the
existing platform resolver selects a descriptor, and we verify the selected
manifest's raw bytes against that descriptor's digest and size. Finally, we
check the configuration bytes against the manifest's descriptor. Tag-based pulls
compute their root digest locally too; a mutable tag itself isn't an immutable
identity guarantee.

`VerifiedImageManifest` owns the parsed manifest and its original `Vec<u8>`
bytes. Keeping both avoids serialising JSON again, which can change whitespace
or field ordering and therefore the digest. Pickle publishes those verified
bytes directly instead of fetching the manifest a second time. There is no gap
where metadata from one response can be paired with another response's bytes.

Integrity failures use a terminal OCI error, so they don't enter the transient
transport retry branch. The complete manifest/index/config fetch stays inside
the caller's deadline. We still verify downloaded layers before publishing them.
This proves content identity; it doesn't establish who built the image or make a
mutable tag trustworthy.

The HTTP fixtures exercise both consumers with changed root manifests, changed
indices, changed selected manifests, wrong configuration bytes and incorrect
descriptor sizes. Another case resolves an intact index and checks both the
unpacked file and Pickle's exact manifest/configuration hashes. Complete corrupt
configuration responses now join malformed manifests and corrupt layers in the
no-retry regression. Actual upstream runtime tests remain a separate check of
registry interoperability.


### Valid bytes can still describe an invalid size

A manifest can have the correct digest and still declare a layer size of `-1`.
Our upstream adapter cast that signed number to `u64`, making it enormous, then
passed the value to `Vec::with_capacity`. The regression reaches a capacity
overflow panic before fetching the blob. Another manifest uses three large
positive sizes whose sum cannot fit the cache's accounting field.

We now validate every layer size and their sum before publishing upstream
metadata. `u64::try_from` returns an error for a negative value; the old `as`
cast silently changed its meaning. `checked_add` returns `None` if the sum would
overflow, which becomes an ordinary pull error. The public blob-fetch method
also checks that its unsigned descriptor fits OCI's signed size field.

The receive buffer starts with `Vec::new()`, so a descriptor cannot demand an
up-front allocation. It grows as bytes arrive; this change does not introduce a
new maximum image size or convert the download path into a streaming disk writer.
After transfer, the byte length must match the descriptor. Direct pulls make the
same check for an existing cached blob before reusing it. SHA-256 verification
remains a separate requirement.

The fixtures sign no content and trust no digest header. They compute valid
manifest digests over deliberately invalid size metadata, so identity checks
cannot hide the size defect. Cold and warm cache cases verify the actual layer
request count, and both direct and pull-through consumers refuse a length
mismatch.

### Give the cache the same deadline as a direct pull

A node could retry a throttled direct pull successfully, then fail on the same
response when Pickle fetched it for the cluster. The two consumers shared digest
verification but only ImageStore used the retry helper. Pickle's freshness HEAD
and layer fetch also lacked a deadline.

Both now call the same crate-private helper in `grill::oci_pull`. Each HEAD and
manifest/configuration read gets 30 seconds; each layer gets 120 seconds. Four
attempts fit inside that original budget, including backoff. Integrity and
authorisation errors remain terminal. A blob attempt owns a new empty buffer,
so bytes from an interrupted response cannot prefix the next complete response.

The tests exercise actual HTTP requests through the upstream adapter. They
count throttled attempts, interrupt a response body, and stall each read type
before advancing Tokio's clock past its budget. Separate denial and incorrect
length cases prove that retries don't turn permanent failures into repeated
downloads. This establishes the cache's read behaviour without depending on
a public registry's availability.

### One image, two repository owners

Push the same image to `production/app:latest` and `rbtest-copy/app:latest`.
The bytes have the same digest. The repositories still have different owners.
Our catalogue used to keep only one manifest row per digest, so the second push
inherited the first repository's metadata. Deleting the test tag could also
remove `latest` from the production row's tag set. That is a poor foundation
for automatic test cleanup.

The catalogue now identifies a metadata row by both repository and digest.
Tags point to content within that repository. Moving a tag updates its
repository's tag sets but preserves the previous digest for a pull that has
already verified and pinned it. Explicitly deleting the last tag removes only
that repository's row. Blob storage and holder locations remain keyed by content
digest; the registry does not write a second copy of identical bytes. Garbage
collection considers references from every remaining repository before allowing
a shared blob to be deleted.

The Rust lookup uses `.find(...)` with a closure that checks both fields. A
closure is an unnamed function; its `|...|` parameters receive each candidate.
The deletion path uses `.retain(...)`, which keeps entries for which its
predicate returns `true`. Checking the repository in that predicate is the
part that prevents one owner's cleanup from retiring somebody else's metadata.
Signatures attest the digest, so attaching one updates every repository copy
and later copies preserve it. Scheduler and peer pulls select the repository
when looking up a pinned digest too.

The first regression pushes identical metadata into two repositories and fails
because only one row survives. Further checks move a tag, preserve signatures
through disk reload, restore a Raft snapshot and push the actual same bytes over
HTTP. They then delete the test reference and verify that the ordinary reference
and its shared content remain available. Durable state advances to generation
11; fresh pre-release clusters avoid interpreting an older collapsed catalogue
as complete ownership evidence. Repository leases and upload retirement are the
next, separate boundary.

### A test volume needs an owner before it exists

Suppose a test creates a 32 MiB volume and Bun dies just after mounting its
backing image. The test client has disappeared too. Deleting an application row
won't unmount that filesystem. Deleting the directory underneath it is worse:
we might remove the data while the mount is still live.

For reserved test namespaces, `VolumeManager::prepare_test_storage` first writes
a private ownership checkpoint under `.test-storage`. Each managed path records
its backend and whether provisioning finished. We synchronise the checkpoint and
its directory before creating the subvolume or image. An interrupted preparation
stays incomplete on disk. Recovery refuses to format it again; lease retirement
can inspect and remove what that attempt actually created. Reopening a completed
volume preserves its data and verifies its backend. An existing directory without
an ownership checkpoint is an error, not an invitation to adopt someone else's data.

The checkpoint also owns the application's generated configuration directory.
Host-source volumes remain outside that ownership. We reject symlinked parents,
symlinked configuration files, overlapping volume/artifact paths, duplicate JSON
keys and invalid identities. The `unique_volumes` deserialiser implements Serde's
`Visitor<'de>` trait: `'de` is the lifetime of the input being decoded, and
`visit_map` consumes entries one at a time. This lets us reject a duplicate before
a `BTreeMap` silently replaces the earlier value. `#[serde(deserialize_with =
"unique_volumes")]` selects that function when reading the journal; normal
serialisation still produces a JSON object.

Stopping an application is not permission to delete its storage. Scale-down,
rescheduling and ordinary Stop all preserve it. The lease reaper sends a separate
`RetireTestResources` command, which first confirms runtime retirement and only
then retires storage. The checkpoint enters a retiring state before deletion.
A loop volume must match its recorded image and unmount normally; a busy mount
keeps the checkpoint and lease pending. Btrfs subvolumes use `btrfs subvolume
delete`. Plain directories are removed only after checking for unexpected mounts.
We delete the checkpoint last and synchronise the directory again. A retry can
finish an interrupted deletion without treating missing files as lost ownership.

Linux reports mounts through `/proc/self/mountinfo`. macOS uses
[`getfsstat`](https://developer.apple.com/library/archive/documentation/System/Conceptual/ManPages_iPhoneOS/man2/getfsstat.2.html).
The latter fills a caller-provided array. `MaybeUninit<statfs>` reserves correctly
aligned storage without pretending it contains valid Rust values. We inspect
only the records the syscall says it initialised. The `unsafe` blocks document
that boundary; the rest of the cleanup path remains ordinary checked Rust.
Filesystem commands run on `spawn_blocking` threads with deadlines. On Linux,
parent-death signalling also prevents a provisioning helper from continuing after
its Bun owner dies.

For 0.1.0, disposable test volumes do not support snapshots. Manual mutation
refuses and automatic snapshot discovery excludes their namespace. Unexpected
snapshot storage keeps cleanup pending so an operator can investigate. Ordinary
application snapshots keep their existing behaviour. Storage ownership doesn't
solve the separate problem of recovering a runtime that died before its first
adoption record; that remains part of the release's runtime recovery work.

### A successful push must survive a catalogue write failure

Imagine `catalog.json` has become unwritable. The original registry handler
updated its in-memory catalogue, printed the persistence error and still returned
`201 Created`. The image appeared until Bun restarted. That is not a successful
push.

Manifest commits now acquire an owned Tokio write guard, build the next
catalogue, persist it and only then publish it in memory. `write_owned()` differs
from borrowing a guard with `write()`: the guard owns an `Arc` reference to the
lock, so we can move it into `spawn_blocking`. If the HTTP caller disappears,
the blocking task still owns the guard until its filesystem transaction finishes.
Another request cannot persist an older snapshot over it. The catalogue uses a
private temporary file, atomic replacement and checked file/directory syncs.
Persistence errors reach the caller and the pull-through cache.

Garbage collection uses the same guard through persistence and physical blob
deletion. A manifest handler also rechecks its required blobs after acquiring
the guard. Why twice? Its initial HTTP validation might have happened before a
queued collector removed an unreferenced layer. Publishing after that deletion
would acknowledge an image we could no longer run. If a push commits first,
collection's fresh reference check preserves its blobs. If collection wins,
the push must refuse and the client must upload the missing bytes again.

The tests drive both orderings through the real handlers. One queues collection,
waits until a push has validated and stored its raw manifest, then lets collection
finish before publication. Another submits a stale collection report after the
manifest commit. Four simultaneous pushes must all survive reopening the on-disk
catalogue. A failed GC catalogue write must delete no bytes, and repair followed
by retry must finish collection. Authoritative cluster acceptance remains a
separate Raft decision; an unconfirmed cluster commit returns a retryable error.

### A collection decision must survive a failed deletion

Suppose two nodes hold an unreferenced layer. Raft approves node A's deletion
and removes A from the holder set, then its filesystem refuses the unlink.
On retry, the catalogue lists only B. That does not make A's leftover bytes the
last copy: B is still the protected holder. The old collector confused those
cases and retained A's extra copy indefinitely.

Nomination now protects a sole holder only when it names the local node.
Arbitration can approve A again while another advertised holder remains, even
if A was already removed from the set. It still checks every manifest reference
on every attempt. A push between the first approval and retry protects the layer,
and B cannot delete the last advertised copy. Three failing-first tests exercise
reloaded approval, nomination and an actual failed file deletion followed by
repair. All 243 Pickle and 82 Raft state-machine tests pass on macOS/Linux, with
strict Clippy on both. The ownership decision is unchanged in shape; no new
state format is needed.

### The node receiving a push might not lead the catalogue

A client can reach a perfectly healthy registry on a follower. Saving the blobs
there does not make its manifest visible in Raft. The receiving node now sends
its proposal directly to the advertised leader when the local council cannot
commit it. A worker without a council uses the same path. If the leader is
unavailable, the client gets a retryable error and the stored bytes remain
available for its next attempt.

The internal request contains a `RegistryMutation` enum with two alternatives:
a manifest commit or a garbage-collection proposal. It cannot carry an arbitrary
Raft command. The larger manifest lives in `Box<ManifestCommit>`: `Box` owns a
heap allocation, so the small GC alternative does not reserve space for every
manifest field. Serialisation still produces the same manifest fields on the
wire; this is a Rust memory-layout choice.

The leader requires both the internal service credential and the node certificate
presented on that TLS connection. It checks current revocations and derives the
allowed holder ID from the certificate. A request body cannot nominate another
node's holdings for deletion. If an operator retires the writer while a proposal
is in flight, the Raft state machine refuses it too. Checking only before the
write would leave an ordering gap.

The client refuses redirects, bounds the response and includes streaming in its
deadline. The server bounds the request body and places its deadline outside JSON
extraction, so a sender cannot hold the handler indefinitely by trickling JSON.
A timed-out proposal remains uncertain: it might have committed after the caller
stopped waiting. Retrying is how the client establishes acceptance.

The tests push through a worker and a follower using real TLS, then inspect the
leader's catalogue. A three-node Raft fixture isolates its old leader, elects a
replacement and updates the advertised route. Forwarding succeeds through the
new leader and refuses after the remaining quorum is lost. Separate tests cover
wrong credentials, forged holdings, certificate revocation and bounded streams.
Repository lease ownership and cleanup still need their own conditional commits;
forwarding alone cannot establish those obligations.

### An upload belongs to its creator

Two deploy tokens can publish into the same repository. That doesn't make them
interchangeable halfway through an upload. Previously Pickle checked the role
on every chunk but discarded the authenticated identity. Anyone with another
deploy token and the upload URL could append bytes or complete the upload.

Authentication now returns `Option<AuthContext>`. `Some(context)` retains the
exact credential fingerprint and its scope; `None` represents the explicitly
open standalone bootstrap mode. Upload metadata stores that fingerprint, never
the bearer secret or just its human-readable name. A replacement credential
with the same name doesn't inherit the old session. The internal service
principal doesn't inherit it either.

The session's existing writer guard checks repository, identity and lifecycle
before body consumption. Every request still authenticates against the current
token store, so revocation takes effect even if the session remains within its
TTL. A refused outsider cannot mutate or discard the creator's temporary file.
The TTL reaper remains responsible for abandoned sessions. The regression uses
two credentials with the same name, attempts PATCH and completion with the
wrong credential and the service principal, revokes the owner, then verifies
that the restored owner can finish its unchanged bytes.

### Keeping the writer receipt until cleanup finishes

A repository can receive uploads on two nodes before either publishes a
manifest. A catalogue of completed images can't tell us who owns those partial
files. The lease therefore records each repository and every node which may
have accepted a writer. The receipt comes before the bytes.

The record uses `BTreeMap<String, BTreeSet<u64>>`: an ordered map from repository
names to ordered sets of node identities. The type arguments inside `<...>`
select what each generic collection stores. Ordering makes serialisation
stable; a set makes a repeated writer claim idempotent. We retain a repository
entry even after its final node acknowledges retirement, because the global
catalogue still needs that repository name for its final cleanup.

Publication is a separate conditional Raft operation. It checks the active
lease, the repository namespace and the publishing node's receipt when the
entry is applied. Moving the lease to Cleaning fences a proposal admitted
before cleanup but committed afterwards. Ordinary manifest commits cannot
bypass the reserved `rbtest-.../` repository namespace.

The cleanup barrier has two stages. First, desired workloads disappear and
all former placement owners confirm runtime retirement. Only then does Raft
record `workloads_retired`. Registry acknowledgements before that point refuse.
The lease remains until every registered writer confirms local retirement.
Finishing removes all of the repository's metadata, including untagged rows,
while preserving ordinary repositories and shared digests. Only exclusive,
unreferenced digests lose their location records; normal GC can then reclaim
the orphan bytes instead of retaining a useless last copy forever.

Operator decommission records how many registry obligations it clears, alongside
the existing placement audit. It doesn't grant the old identity a way back in.
Snapshot restoration preserves the remaining owners and the original audit.
The standalone lease store uses the same durable receipts and refuses early
completion; its runtime reaper records the workload barrier only after the
agent confirms cleanup.

These are the durable state transitions. They don't, by themselves, wire OCI
request admission, replication and local upload/catalogue deletion into the
protocol. Those callers must acquire the receipt before writing and retain
transaction ownership until their mutation finishes. We track that integration
separately rather than counting the schema as a finished cleanup feature.

### Keeping repository names intact between peers

`rbtest-run1/team/web` must keep that name when a node copies its layers.
An old workaround replaced slashes with hyphens because the original HTTP
router accepted only one path segment. The router has supported nested
repositories for some time, but the workaround remained. That would move a
leased upload outside the namespace which owns it.

Peer HEAD, GET and upload requests now preserve the full repository path. The
regression serves only the exact nested path: upload used to receive 404, and
now upload, inventory and verified download all succeed. Content-addressed blob
storage stays shared; it doesn't excuse losing the request's repository identity.

### Asking the current owner before deleting

A worker knows which uploads it received. It cannot infer that the applications
using those images have stopped. It asks the leader for its own outstanding
repository receipts, and the leader returns them only after the committed
workload-retirement barrier. The response includes the lease generation as well
as the repository name; a cleanup retry must not target a later owner.

`RegistryQuery` is an enum with two variants: `Lease { repository }` and
`Retirements`. Matching the enum forces the server to handle both questions.
The result is another enum, so a caller expecting a receipt inventory cannot
silently reinterpret an owner lookup. A service bearer alone is insufficient:
the TLS leaf identifies the actual node, and a quorum-backed security read
checks revocation before answering. An isolated old leader refuses even if its
local catalogue looks plausible.

The integration test drives active, Cleaning, workload-retired and acknowledged
states, checking the returned inventory at each step. Real TLS tests replace
the leader and remove quorum. Request limits include a stalled body, since a
deadline around just the handler would start too late. These queries provide
the cleanup worker's evidence; they do not themselves remove any files.
