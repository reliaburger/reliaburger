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

### One authoritative catalogue

Pickle has two catalogues: the council's Raft-replicated `manifest_catalog`, and each node's local `PickleState::catalog`. The push path proposes to Raft; the read path — `manifest_get` and `tags_list` — read the *local* one. So a manifest a peer committed to Raft was invisible on a node that hadn't received the original PUT, until a heal tick happened to reconcile it. Push on node A, `docker pull` from node B, get a 404.

The fix is a one-liner in spirit: read the authoritative catalogue. Both handlers now call `catalog_snapshot()`, which returns the council's Raft catalogue when clustered and the local one otherwise — exactly what the P2P pull path already used. The moment Raft applies a peer's commit, every node's tag list and manifest lookup see it.

### Authenticate the writes, serve over TLS

The registry listener was plain HTTP with no auth — fine behind a firewall, a liability anywhere else. Rather than invent a registry-specific credential, it reuses the cluster's existing `sesame::auth`: the same bearer tokens and internal service token that guard the agent API. Reads stay open (peers and clients pull freely); writes require a principal with at least `Deployer` role, or the service token that node-to-node replication presents. A fresh, tokenless cluster keeps the same fail-open bootstrap window the API uses, so single-node push works before an operator mints the first token.

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

### Honest push semantics

The last one is about telling the truth. A push commits locally and to Raft, then the heal loop drives it up to `[images] redundancy` copies afterwards. So what should a push *report*? Not "durable" — it isn't yet. The manifest PUT now returns `201 Created` with an `OCI-Replication: pending` header when the commit is authoritative but replication is still owed, and a distinct `202 Accepted` with `OCI-Replication: raft-uncommitted` when a council member's Raft proposal failed (the bytes are stored, but the cluster catalogue doesn't know yet). A caller reading headers can tell acceptance from durability.

The GC arbiter got stricter too. It used to recheck only sole-copy protection at deletion time. Now it rechecks against the *full catalogue reference set* immediately before approving: a blob any manifest still references — its config, a layer, or the manifest blob itself — is never approved for deletion, even if the nominating node saw it as an orphan when it built the report. A fresh push can re-reference a blob between nomination and approval; this serialised recheck is the last chance to refuse, and it takes it.

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
