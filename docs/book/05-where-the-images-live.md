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

## Pull-through cache

Not every image is pushed explicitly. Your apps might reference `alpine:latest` or `nginx:1.25`. The first time any node needs an image that's not in Pickle, it pulls from Docker Hub (using the existing `oci-distribution` client from Phase 1), stores the layers locally, and commits the manifest to Raft. The next node to need the same image gets it from a peer — Docker Hub is never contacted again.

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

With Pickle, the cluster *is* the registry. If the cluster is up, the registry is up. There's no separate SLA to track, no status page to monitor, no fallback to configure.

And the pull-through cache makes this even better. The first time you deploy `nginx:1.25`, Pickle pulls it from Docker Hub and caches it. Every subsequent deploy of that image, on any node, comes from a cluster peer. Docker Hub could vanish entirely and your existing deployments wouldn't notice.

Can you deploy a *brand new* Docker Hub image during an outage? No. But how often do you deploy something you've never deployed before versus something you've deployed a hundred times? In production, most deploys are updates to images you already have. Pickle keeps them all.

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

Axum handles URL routing and parameter extraction. `Digest::new` handles domain validation. The handler glues them together. This separation means the `Digest` type works the same way whether it came from an HTTP path, a manifest JSON document, or a test fixture.

## What we learned

### Atomic rename is your friend

The upload session design is simple: temp file for in-progress data, atomic rename to the blob store when verified. No journal, no WAL, no transaction log. The filesystem is the state machine.

This works because rename on the same filesystem is atomic on Linux (and macOS). The blob is either fully present or absent, never half-written. A crash during upload leaves an orphan temp file that the next GC sweep cleans up. A crash during rename either completes or doesn't. No corruption either way.

### Don't invent a protocol when HTTP exists

Peer replication uses the same OCI Distribution API that Docker uses. The replicating node is literally a push client. This means: zero new code for the receiving side, the same error codes and retry semantics as a client push, and a protocol that every container tool already understands.

We considered a custom binary protocol (gRPC, or raw TCP with length-prefixed frames). It would have been faster for large layers. But "slightly faster" doesn't beat "zero new code to test" when you're moving blobs between nodes on a local network.

### Sole-copy protection prevents cascading deletion

Without sole-copy protection, GC on two nodes can race: both check the holder set, both see "two holders", both delete. Now nobody holds the layer. The fix is in Raft: after GC, the node proposes a `GcReport` that removes itself from the holder set. Because Raft proposals are serialised, the second node's GC will see only one holder remaining and skip the deletion.

## Test count

Phase 5 adds 72 tests, bringing the total to 867. The new tests cover digest parsing, manifest serde, Raft state machine extensions (commit, update, GC, delete), blob store operations (write, read, upload sessions, digest verification), OCI API endpoints (full push/pull round-trip), peer selection, image availability checks, GC safety (sole-copy, active reference, retention), and volume management.
