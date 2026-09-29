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

`docker push` and `docker pull` speak a specific HTTP protocol: the OCI Distribution Spec. Pickle implements the subset that matters for pushing and pulling (no deletes, no cross-repository mounts).

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

1. `docker login node-1:5050` once, with a Reliaburger API token as the password
2. `docker push node-1:5050/myapp:v1` (Pickle's OCI API on any node), or skip docker entirely with `relish build`
3. Done. Pickle replicates internally. Every node can pull from its peers.

One login, with a token the cluster already issues. No second account, no rate limits, no outbound internet from worker nodes. (How that login works, and why it only works over TLS, comes later in the chapter.)

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

A mount is kernel state, though, and kernel state doesn't survive a reboot. We only mounted the image when the volume was first created. The V02 soak stopped and started the whole cluster, and the writer app came back to an empty `/data`: its image sat unmounted beside a bare directory, the container wrote into that directory on the root filesystem (with no size limit at all), and 36,318 acknowledged lines were hidden rather than lost. Provisioning an existing loop volume now mounts its image again when it isn't mounted. If something already wrote into the bare mountpoint, it refuses and says so, because mounting over those files would hide them just as quietly.

The same soak found a smaller trap in a new loop volume. `mkfs.ext4` leaves `lost+found` behind, so the app never sees an empty directory. The official Redis entrypoint only takes over its data directory when it holds nothing but `*.rdb` files and `appendonlydir`; anything else and it prints a notice, drops to the `redis` user and fails with "Permission denied". Postgres's `initdb` flatly refuses a non-empty directory. We remove `lost+found` right after mounting, so a new volume looks exactly like a Docker volume does. `e2fsck` makes a fresh one if it ever needs it.

On macOS, there's no loop mount. Reliaburger creates a plain directory and logs a warning. Size limits are soft-only on macOS. This is a development convenience, not a production limitation — production clusters run Linux.

## Whose volume is it?

Chapter 1 put every Runc container in a user namespace: container uid 999 is host uid 2,000,000,999, and container root is nobody special on the node. That fixed a lot, and broke volumes on the spot. Bun creates `/var/lib/reliaburger/volumes/default/redis/data` as root, bind-mounts it at `/data`, and Redis's entrypoint tries to `chown` it to the `redis` user. Inside a user namespace, `CAP_CHOWN` only works on files whose owner the namespace maps. Host root isn't mapped. `chown: Operation not permitted`, and Redis never starts.

Docker's `userns-remap` answers this by chowning a new volume into the remapped range. Kubernetes answers with `fsGroup`, which makes a volume group-owned by a chosen gid and adds that gid to every container in the pod. We copied Docker. A Reliaburger app has one process user, and images that switch users (Redis, Postgres) start as root and hand their data directory over themselves, which works as soon as container root owns the directory. So there's no `fs_group` field, and `relish import` warns that it dropped one.

The interesting part is *when* to chown. Every start? Redis's entrypoint gives `/data` to uid 999; Bun giving it back to root on the next start would fight it, and a recursive `chown` of a 200 GB volume on every restart is a long way to wait. Only when the volume is empty? A loop-mounted ext4 volume is born with `lost+found` in it. So the provisioning sidecar next to the volume (the one that already records its backend) now also records who we handed it to, and a small pure function decides:

```rust
pub fn plan_ownership(
    recorded: Option<VolumeOwner>,
    root: VolumeOwner,
    wanted: VolumeOwner,
) -> OwnershipPlan {
    let root_mapped = super::userns::container_id(root.uid).is_some()
        && super::userns::container_id(root.gid).is_some();
    match recorded {
        _ if !root_mapped => OwnershipPlan::HandOver,
        None => OwnershipPlan::HandOver,
        Some(previous) if previous == wanted => OwnershipPlan::Keep,
        Some(previous) => OwnershipPlan::Rehome { from: previous },
    }
}
```

The `if` after a pattern is a *match guard*: the arm only matches when the condition holds too, and the arms are tried top to bottom. `_ if !root_mapped` matches any `recorded` value, so it catches a root that nothing in the container range owns (a snapshot restored from before the first mount, say) before the other arms get a look. Because `OwnershipPlan` is an enum, the caller's `match` on the result must handle `Keep`, `HandOver` and `Rehome`, and the compiler says so if a fourth variant ever turns up.

`Rehome` is the image-`USER`-changed case. It moves only what the previous user owned, using the same tree walk as `HandOver` with a different rule:

```rust
fn chown_tree(
    root: &Path,
    new_owner: &dyn Fn(VolumeOwner) -> Option<VolumeOwner>,
) -> Result<(), VolumeError>
```

`&dyn Fn(...)` is a borrowed *trait object*: any closure with that signature, called through a pointer, much like passing a function pointer plus a context in C. `HandOver` passes `&|_| Some(wanted)` (everything goes to the new user), `Rehome` passes a closure that returns `None` for files the old user never owned. The walk uses `symlink_metadata` and `lchown`, so a symlink the container planted pointing at `/etc/shadow` gets its own ownership changed, not its target's.

Host-path volumes (`source = "/srv/import"`) get none of this. They're the operator's directories, and silently chowning `/srv/import` to uid 2,000,000,999 is exactly the kind of surprise an orchestrator shouldn't spring. Bun checks the mode bits instead and, if the container's user plainly can't write, logs which host uid to `chown` the directory to. It still starts the container, because plenty of host mounts are only ever read.

The proof is a gated test that runs the real Redis image with `--appendonly yes` over a managed volume: first as uid 999 directly (so no entrypoint `chown` can paper over a missing hand-over), writes a key, restarts, reads it back, then restarts as image root and reads it again.

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

That first version had two problems, both hiding in `std::fs::read`. It loads the whole file into a `Vec<u8>`, and the heal loop and P2P resolver call it for every layer they consider. A 4 GB layer meant a 4 GB allocation, on every tick. And `let Ok(data) = ... else { return false }` turned *every* read error into "not cached". A permissions mistake or a dying disk looked like a cache miss, so we'd quietly download the layer again on top of a file we couldn't even read.

Now the hash streams the file through a 64 KiB buffer (`sha256_file`, shared with the copy-confirmation path), and the function returns `Result<bool, PickleError>`:

```rust
let actual = match sha256_file(&path) {
    Ok(actual) => actual,
    Err(PickleError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
        return Ok(false);
    }
    Err(error) => return Err(error),
};
```

The `if` after a pattern is a *match guard*: the arm only matches when the pattern fits *and* the condition holds. Only "no such file" means "not cached". Any other I/O error propagates to the caller, and a mismatch still deletes the corrupt bytes, now checking that the delete worked too. Tests cover a multi-chunk blob whose last byte flips and a blob path that can't be read, which must be an error and must not be deleted.

### One rootfs per content, not per tag

Here's a subtle one. The unpacked rootfs used to live at `rootfs/{registry}/{repo}/{tag}/`, and unpacking *cleared and recreated* that directory. Now picture a tag move — `web:v1` re-pointed at new content — while a container is running out of the old rootfs. The re-extract does `remove_dir_all` on the directory the running container is living in. Two concurrent pushes to the same tag race the same way.

The fix content-addresses the rootfs: each set of layers unpacks into `…/{tag}/gen-{hash}`, where the hash is derived from the ordered layer digests. Different content lands in a different generation directory. The same content needs one more rule: publish it once, write a completion marker, then reuse it. Re-extracting an “identical” tree still starts by deleting the old one, which removes commands underneath a running container. `ImageStore` serialises generation publication across its clones and treats only a marked generation as reusable. A running container holds the path it was started with, and a tag move simply produces a *new* generation beside it. Nobody deletes anybody's live filesystem.

That marker turned out to be a promise we hadn't kept. The V02 soak powered a node off a few seconds after it unpacked Redis. When the node came back, `gen-…/.complete` was there and `usr/local/bin/redis-server` was 0 bytes: ext4 had written the tiny marker and was still holding the unpacked data in the page cache when the power went. Every redeploy then failed with `exec format error`, the node never got its pinned workload back, and the agent retried every two seconds. The marker now waits for `syncfs` on the unpacked tree, which flushes the whole filesystem in one call rather than fsyncing thousands of files, and is itself written with the same synced temp-file-and-rename as everything else. Cached blobs got the same treatment: they used to be renamed into place without a sync.

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

The price shows up when there's no council to ask. The V02 soak powers off two of three nodes for five minutes, and the survivor's registry answered every manifest GET with 503 while blobs, which are content-addressed and need no catalogue, kept coming back byte for byte. Is that a bug? We decided it isn't. A tag is a mutable pointer, and only the committed catalogue knows where it points now: a lone node serving its last local view could hand out a tag that was deleted or moved on the majority side. So the registry fails closed, and the soak's judge learned the difference. It retries a 503 three times, five seconds apart (enough for an election), records a lasting 503 as *unavailable* rather than as a problem, and lets that pass only inside a fault window. A changed digest or a 404 still fails at once, fault or no fault, because that's data loss, not downtime.

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

There was a subtler memory hole behind that hard cap. The pull *enforced* the cap but still buffered the whole blob in a `Vec` before writing it — up to two gigabytes per layer, multiplied by every concurrent pull. The cap stopped a single hostile peer; it did nothing about a handful of honest large pulls landing at once. So the pull now streams: chunks go straight to the upload temp as they arrive, a running SHA-256 hashes them on the way past, and once the digest checks out the temp is committed into the blob store by an atomic rename — no re-read, no second copy. Peak memory per pull is one network chunk, whatever the layer's size. (The push handlers still buffered their request bodies at this point; the release hardening below fixed that too.)

### Honest push semantics

A push commits locally and to Raft, then the heal loop drives it up to
`[images] redundancy` copies afterwards. So what should a push *report*? The
manifest PUT returns `201 Created` with `OCI-Replication: pending` once Raft has
accepted the commit: authoritative, but not yet fully replicated. If the Raft
proposal fails or times out, the PUT returns `503 Service Unavailable` and the
client retries. An earlier version returned `202 Accepted` with a custom header,
and generic OCI clients happily treated that success-class status as a finished
push without ever reading our header. The status code is the one part of the
answer every client reads.


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

Both stores now use one path resolver and one layout, the registry's. Nothing
has shipped yet, so there's no flat-file cache in the wild to keep reading.
Enumeration ignores temporary filenames. A regression writes through the
registry and reads through the runtime.

That exposed a second assumption: rootfs generation IDs hashed each layer's
filename. Every registry layer's filename is `data`, so replacing a layer could
reuse the previous rootfs generation. We now take the digest from the parent
directory's name instead. Changed digests produce a new generation without
touching a running container's files. The tests exercise that property
directly.

Digest pins also contain a colon (`sha256:...`), as do registries with explicit
ports. That character separates lower layers in overlayfs mount options. We
encode it as `%3A` in rootfs directory components while preserving the original
OCI reference for registry requests. A path regression covers both a pinned
digest and a registry port; ordinary tag paths keep their existing layout.

## Trust nothing that comes over the wire

Pickle's pull-through cache and Grill's direct pulls both talk to registries we
don't control. Docker Hub is mostly well behaved. "Mostly" isn't a security
property, and a laptop test found just how many assumptions we'd made about the
other end.

Start with the header everyone trusts. Ask a registry for
`alpine@sha256:...`, and it can reply with different, perfectly valid JSON while
repeating the requested digest in `Docker-Content-Digest`. Parsing that JSON
proves nothing about its identity. Our first regression accepted a changed
configuration; the pull-through version accepted a changed index. Both came with
plausible headers.

So Grill and Pickle now share one verified fetch path. It hashes the exact bytes
of the root manifest before parsing them. If the root is an image index, it picks
the Linux entry for the node's architecture (explicitly Linux: the OCI library
used to pick the *client's* operating system, so a Mac asking for a Linux image
found nothing) and verifies that child manifest's bytes against the index's
descriptor. Then it checks the configuration bytes against the manifest. Every
check goes through one small function:

```rust
fn verify(bytes: &[u8], expected: &str, size: Option<i64>) -> Result<(), OciDistributionError> {
    let actual = format!("sha256:{:x}", Sha256::digest(bytes));
    if actual != expected {
        return Err(invalid(format!(
            "upstream content digest mismatch: expected {expected}, received {actual}"
        )));
    }
    if let Some(size) = size
        && u64::try_from(size).ok() != Some(bytes.len() as u64)
    {
        return Err(invalid(format!(
            "upstream descriptor size mismatch for {expected}: expected {size}, received {}",
            bytes.len()
        )));
    }
    Ok(())
}
```

`{:x}` formats the hash as lowercase hex. The second `if` is a *let chain*, new
in Rust 2024: `if let Some(size) = size && ...` runs the block only when the
pattern matches *and* the condition holds, with `size` already bound in the
condition. OCI stores sizes as signed integers, so `u64::try_from(size)`
refuses a negative one, and `.ok()` turns that `Result` into an `Option` we can
compare. A size of `-1` therefore can't equal any real length. The old code used
`size as u64`, which quietly turned `-1` into 18 quintillion and passed it to
`Vec::with_capacity`. It panicked before fetching a single byte.

We keep the verified bytes alongside the parsed manifest
(`VerifiedImageManifest`) and publish those exact bytes into Pickle. Serialising
the parsed struct again would change whitespace or field order, and with it the
digest. Layers are still hashed before they reach the cache.

Two more lessons from the same fixtures. First, a registry's upload `Location`
header isn't permission to follow it with credentials. The peer-replication
fix above constrained redirects; the catalogue client had the same hole and
would have sent an administrator's bearer to whatever server the header named.
Both now resolve the location with `url::Url` and refuse any change of scheme,
host or port. Second, public registries rate-limit and drop connections. A
privileged CI run lost its BusyBox pull to `Rate exceeded` after 42 other checks
passed. Reads now retry rate limits, temporary gateway errors and interrupted
streams, at most four attempts with jittered backoff, all inside *one* deadline,
so a stalled request can't reset the clock. Each attempt starts from an empty
buffer, so a half-received body never prefixes the next one. Authentication
failures, missing images and integrity failures don't retry: asking again won't
change the answer.

That first version had a blind spot, and a release candidate found it. The
deadline covered the whole read, so one stalled connection could spend all 30
seconds of a manifest budget and leave no time to retry. A slow CDN edge does
exactly that: one connection hangs while a fresh one would answer at once. The
run failed with `registry read deadline exceeded` after a single attempt. Now
each read has two limits, a ceiling per attempt and a total:

```rust
pub(crate) struct RegistryReadBudget {
    pub(crate) attempt: Duration,
    pub(crate) total: Duration,
}

pub(crate) const METADATA_READ: RegistryReadBudget = RegistryReadBudget {
    attempt: Duration::from_secs(30),
    total: Duration::from_secs(120),
};
```

A `const` is evaluated at compile time and inlined wherever it's used, which is
why `Duration::from_secs` has to be a `const fn` to appear here. The retry loop
gives each attempt `min(now + attempt, deadline)` through
`tokio::time::timeout_at`, and treats an expired attempt like any other
transient failure. A single attempt keeps the old ceiling (30 seconds for
metadata, 120 per layer); the totals (two and six minutes) leave room for the
retries. Server errors (500, 408) and refused connections joined the transient
list too. The unit tests drive it with `#[tokio::test(start_paused = true)]` and
`std::future::pending()`, a future that never completes, so "the registry hung
for 30 seconds" takes no real time at all.

Retries help when the internet is slow. They can't help when it's gone, and
some clusters never had it: air-gapped sites, or a CI job that shouldn't depend
on a CDN's mood. So digest-pinned images can come from a mirror:

```toml
[images]
mirrors = { "public.ecr.aws" = "mirror.internal:5000" }
```

Why only digest-pinned ones? Because a digest makes the mirror harmless. When a
deployment asks for `busybox@sha256:9532…`, Bun hashes the index, the platform
manifest, the config and every layer it receives, whoever sent them. A mirror
that serves anything else fails verification, and Bun falls back to the real
registry. A tag is different. `busybox:1.37` means whatever the registry says
today, so a mirror answering it would be choosing the image for us. Tags always
go home.

Validation happens while the config parses, not when the first pull fails:

```rust
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(try_from = "BTreeMap<String, String>", into = "BTreeMap<String, String>")]
pub struct ImageMirrors(BTreeMap<String, String>);

impl TryFrom<BTreeMap<String, String>> for ImageMirrors {
    type Error = ImageError;

    fn try_from(mirrors: BTreeMap<String, String>) -> Result<Self, Self::Error> {
        Self::new(mirrors)
    }
}
```

`TryFrom` is the standard library's trait for fallible conversions, and `type
Error = ImageError;` is an *associated type*: each implementation names its own
error type, where Go would return a bare `error` interface. The `try_from`
attribute tells serde to deserialise a plain map first, then run it through our
conversion. There's no way to build an `ImageMirrors` holding `https://…` or a
path, so the pull code never has to check again. Loopback mirrors use plain
HTTP, like the loopback registries the tests already use; everything else stays
on HTTPS.

## Who owns these bytes?

Content addressing answers "are these the right bytes?". It says nothing about
who may publish them, who may delete them, or whether a push really happened.
Those questions kept coming back, so let's take them in the order a push meets
them.

**The push has to be real.** Imagine `catalog.json` has become unwritable. The
original handler updated its in-memory catalogue, logged the error and returned
`201 Created`. The image existed until Bun restarted. Now a manifest commit takes
the catalogue's write lock, persists the next catalogue and only then publishes it
in memory. It takes the lock with `write_owned()` rather than `write()`: an owned
guard holds an `Arc` to the lock instead of borrowing it, so we can move it into
`spawn_blocking` and the file write keeps the lock even if the HTTP client hangs
up halfway through. A client can also reach a perfectly healthy follower.
Storing the blobs there doesn't make the manifest visible in Raft, so the
follower forwards a `RegistryMutation` to the leader over node TLS. It's an enum
of the few registry operations a node may ask for, not an arbitrary Raft command,
and the leader derives the node's identity from its certificate, so a request
body can't speak for another node's storage.

**Repositories own metadata; content is shared.** Push the same image to
`production/app` and `rbtest-run1/app` and the bytes have one digest. The
catalogue used to keep one row per digest, so deleting the test tag could strip
`latest` from production. Rows are now keyed by repository *and* digest, while
blob files stay shared and garbage collection counts references from every
repository. Uploads belong to the credential that started them too: Pickle
records a fingerprint of the token, so another deploy token with the upload URL
can't append to someone else's half-finished push.

**Late publications mustn't undo collection.** Here's the race that took longest
to see. A node checks that its blobs exist and proposes a manifest, but the
proposal times out. Garbage collection then deletes one of those blobs. The old
proposal is still in flight and can reach Raft later, advertising bytes that are
gone. A local mutex can't help, because the two events happen on different sides
of the network. So every storage node has a *GC generation*, a counter in the
replicated state that goes up whenever Raft approves a deletion on that node. A
publication carries the generation its author observed, and the state machine
checks it when the entry is applied:

```rust
fn registry_publication_is_current(
    &self,
    commit: &crate::pickle::types::ManifestCommit,
) -> bool {
    commit.holder_nodes.iter().all(|node| {
        self.state
            .registry_gc_generations
            .get(node)
            .copied()
            .unwrap_or(0)
            == commit.observed_gc_generation
    })
}
```

`iter().all(...)` returns `true` only if the closure is true for every holder,
like Python's `all()`. `get` returns an `Option<&u64>` (a reference into the
map), `.copied()` turns it into an `Option<u64>`, and `unwrap_or(0)` supplies a
default for a node that has never collected anything. Unlike `unwrap`,
`unwrap_or` can't panic. A stale publication gets `RegistryPublicationStale`,
which the client sees as a retryable 503; its next attempt reads a fresh
generation and checks its blobs again. The GC side increments the counter with
`checked_add(1)` and refuses the whole deletion on overflow rather than wrapping
round to an old value. And because the publisher persisted its local catalogue
*before* proposing, an explicit refusal now restores the previous local
catalogue. A timeout doesn't: the council may have committed after all, so the
local state waits for a retry to settle the question.

The replication healer had the same shape of bug. It read a holder list, copied
some bytes and wrote the whole list back, resurrecting any copy that GC had
removed in the meantime. Raft now refuses whole-list updates. Instead, the node
that *received* the copy hashes its local files and confirms only its own
holding (`ImageCopyConfirmation`), checked against the same GC generation. The
storage node speaks for itself.

**Test images have to disappear completely.** `relish test` (Chapter 15) pushes
throwaway images into repositories under a reserved `rbtest-<run>/` namespace,
held by a lease. Cleaning those up is harder than it sounds, because partial
uploads may sit on nodes that never published anything. So before a node accepts
a single byte for a leased repository, it records a *writer receipt* in Raft:
"node N may hold data for repository R under lease L". Cleanup then runs in two
stages. First, every workload using the lease must be confirmed retired. Then
each node with a receipt deletes its partial uploads and metadata and confirms,
and only when the last receipt is gone does the lease disappear. Until then the
API answers 202 `CleanupPending`, which is an ordinary state, not a failure. An
ordinary app can't reference an `rbtest-` image at all, so a finished test can't
pull the rug from under production. Test volumes follow the same pattern: an
ownership checkpoint on disk before the loop image or subvolume exists, and a
normal (never lazy) unmount before anything is deleted.

Whose clock decides that a lease has expired? At first, each storage node
stamped its own time on its requests, so a node running a minute slow could keep
writing for a minute after its lease ended. The state machine can't read the
clock itself: every council member replays `apply` and must reach the same
answer. So the leader stamps its own time as it turns a mutation into a Raft
entry. One clock decides for everyone.

Finally, uploads that die with Bun. A crashed Bun leaves partial files but no
session map. On startup Bun takes an exclusive file lock on the image store (so
two Buns can't sweep each other's uploads), removes regular files whose names
match our generated upload IDs, and refuses to start if it finds anything
unexpected. It never follows a symlink out of the upload directory.

The test that ties this together launches the real binary, pushes the same
content into an ordinary and a leased repository, leaves another upload half
finished, and sends SIGKILL. A replacement Bun must retire the lease on its own,
with no renewals, while the ordinary image still returns exactly its original
bytes. A three-node variant kills the storage-owning leader and checks that the
survivors carry its receipts forward until it returns and cleans up.

## Letting docker in

The design doc said `docker push` and `crane push` work against Pickle. Then a comment in `pickle/build.rs` said Pickle never accepts HTTP Basic auth. Both were in the repository at the same time, and only one of them could be true. Which one?

The comment was. Every credential check in the registry read `Authorization: Bearer …` and nothing else. That suits `relish` and peer replication, which send a bearer. It doesn't suit docker. `docker login` and `docker push` send `Authorization: Basic base64(user:password)`, and they only send it after the server asks. Docker's first request is an anonymous `GET /v2/`. If the answer is a 401 with `WWW-Authenticate: Basic realm="…"`, docker remembers the scheme and attaches its stored credential to everything after. If the 401 names no scheme, docker has nothing to go on. Pickle's 401s named nothing. So on any registry that wanted a principal, docker never offered its credential, and when crane offered one anyway, Pickle ignored it. The only stock push that worked was an anonymous one to a standalone node's loopback registry before anyone created an API token.

There were two ways out. We could correct the docs and make `relish build` the only door. Or we could let standard clients in properly. The second turned out to be small, so we did that. Before writing any code, we decided what we would *not* do:

- **No second credential.** The Basic password is an ordinary Reliaburger API token. The username is ignored: clients insist on sending one, but the token alone names the principal, and checking a field that carries no authority would only add a way to get it wrong.
- **No Basic in the clear.** Basic is refused on a plaintext connection, even with a valid token and even for a read the loopback listener would serve to anyone. Accepting it would teach clients that sending the token in the clear works.
- **No Docker token service.** The other challenge, `WWW-Authenticate: Bearer realm="https://…/token"`, has the client swap its credential for a short-lived token at a separate endpoint. Every client we care about speaks Basic, and a token service would mint a second kind of credential in exchange for the API token the client already holds. That's more moving parts and no extra security.

### One authorisation path, two envelopes

The tempting implementation threads "was this Basic?" through every handler. Blob upload, chunk, completion and manifest push each authorise separately, because they have different role requirements and quota checks. Five call sites means five chances to forget one. Instead, a single middleware sits at the router's edge and rewrites the envelope before any handler sees the request:

```rust
async fn standard_client_credentials(
    mut request: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let over_tls = request
        .extensions()
        .get::<crate::sesame::connection::TlsTransport>()
        .is_some();
    let credential = basic_credential(request.headers());
    if !over_tls && !matches!(credential, BasicCredential::Absent) {
        return oci_error(StatusCode::UNAUTHORIZED, "UNAUTHORIZED",
            "HTTP Basic credentials are only accepted over TLS".to_string());
    }
    match credential {
        BasicCredential::Absent => {}
        BasicCredential::Token(token) => { /* replace the header with `Bearer {token}` */ }
        BasicCredential::Malformed => return basic_challenge(malformed_basic()),
    }
    let response = next.run(request).await;
    if over_tls && response.status() == StatusCode::UNAUTHORIZED {
        return basic_challenge(response);
    }
    response
}
```

`axum::middleware::from_fn(standard_client_credentials)` turns this plain `async fn` into a layer. `next.run(request).await` hands the request to everything inside the layer (the routes and their handlers) and gives back their response, so the function can act on the way in and the way out. On the way in, a Basic credential becomes the equivalent bearer. On the way out, any 401 from a TLS connection gets the `Basic` challenge that makes docker offer its credential. Every role, repository and lease check below the layer is byte-for-byte the bearer path. A ReadOnly token can pull and can't push, whichever envelope it arrives in.

`BasicCredential` is an enum whose variants carry different data. `Token(String)` holds the password. `Absent` and `Malformed` hold nothing. A C programmer would reach for a struct with a status code and a nullable string. Here the string only exists in the variant where it means something, so the compiler won't let you read a token out of a malformed header. `matches!(value, Pattern)` is a macro that evaluates to `true` when the value fits the pattern, a one-line `match` for when all you want is a yes or no.

### Ask the wire, not the config

How does the middleware know the connection was TLS? The obvious answer is a `registry_over_tls: bool` from configuration, and it's wrong. Bun decides to serve TLS when the node has an identity. If building the TLS config then fails, it logs the error and serves plaintext. A flag set from configuration would still say "TLS" and would happily accept a password sent in the clear.

So the evidence comes from the listener. The TLS accept loop (moved out of the `bun` binary into `sesame::connection::serve_router_over_tls` so tests can drive the real thing) attaches a marker to every request it serves:

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TlsTransport;

let service = service.layer(axum::Extension(TlsTransport));
```

`TlsTransport` is a *unit struct*: a type with no fields and exactly one value, also written `TlsTransport`. It takes up zero bytes. It's useful because of its *type*. An HTTP request's extensions are a map keyed by type, and `request.extensions().get::<TlsTransport>()` asks "is there a value of this type in the map?". The `::<TlsTransport>` turbofish names the key. The plaintext listener, plain `axum::serve`, never inserts one, so the absence of the marker is the proof. A Go programmer would stash a sentinel in a `context.Context` under a private key type. This is the same idea, except the key *is* the type, so there's nothing to collide with.

### What the real client found

The unit tests drive the router with a fake TLS marker. `tests/suite/registry_standard_clients.rs` goes further: it serves the registry through the real `serve_router_over_tls` with a node certificate named `localhost`, then pushes an image with reqwest's `basic_auth`, verifying the certificate against the cluster root as docker would. It also has an ignored test that runs actual `crane`: `crane auth login`, `crane append` to push, `crane manifest` to pull back.

That test failed on its first run, and not on authentication. crane asks `HEAD /v2/{name}/manifests/{tag}` before it pushes a manifest, and Pickle routed manifests for `GET` and `PUT` but not `HEAD`. The answer was 405, which crane treats as fatal. So "crane push works" had been false twice over: once for credentials and once for a missing verb. containerd asks the same question. The fix is a `manifest_head` that returns the `GET`'s status and headers, with the exact `Content-Length`, and no body.

That's why the crane test exists at all. The spec says a registry "MUST" support manifest `HEAD`, and we could have read that sentence a dozen times and still missed that we didn't. A real client doesn't miss it.

The honest limits for 0.1.0 are in the manual and design doc. Basic-auth clients need a TLS listener, which a cluster node with an identity has and a plaintext node doesn't. Node certificates name the node rather than an address, so a hostname-verifying client has to reach the registry by the node's name and trust the cluster root CA. And a loopback-only TLS listener answers docker's anonymous probe with 200, so docker never learns to send credentials there. Clustered listeners are routable, so that last one only bites hand-built setups.

Opening the door to docker also showed us who else could walk through it. The Basic middleware makes every client take the bearer path, and the bearer path checked the token's role but never its scope, so a Deployer scoped to one namespace could push to any repository. Repositories now map to namespaces (`team-a/web` belongs to `team-a`), and a scoped token can only push and pull inside its own. Chapter 10 has the rule and the awkward cases, like what a bare `web` belongs to.

## Tests

Pickle is almost entirely testable in-process. A blob store is a directory, the OCI API is an axum router, and the catalog is a `Vec` — none of that needs the internet or another node. So the default suite spins up a Pickle server in the test, pushes a manifest and its blobs, then pulls them back, all without leaving the process.

### Unit tests — the registry without the network

The 104 tests in `src/pickle/` cover:

- **Digest and manifest** — `Digest::new` accepts well-formed digests and rejects everything else; manifests round-trip through serde unchanged.
- **Blob store** — write/read, upload sessions, and the digest-mismatch rejection path (`PickleError::DigestMismatch`).
- **OCI API** — `full_push_pull_round_trip` drives the real `/v2/` handlers end to end against an in-process server; plus the not-found paths (`blob_head_not_found`, `manifest_get_not_found`) that must return the right status codes. The manifest-validation contract gets a rejection matrix: invalid JSON, missing or unknown media type, size mismatch, malformed descriptor digest, missing referenced blob, and a happy path asserting the GET returns byte-identical bytes.
- **Standard clients** — a Deployer token as a Basic password over TLS pushes (`deployer_token_as_basic_password_over_tls_may_push`), a ReadOnly one is forbidden, an unknown one gets a 401 with a fresh `Basic` challenge, and a valid token over plaintext is still refused (`basic_credentials_over_plaintext_are_refused_even_with_a_valid_token`). `tests/suite/registry_standard_clients.rs` repeats the push through the real TLS listener, and its ignored `crane` test runs with `cargo nextest run --test suite --run-ignored=only -E 'test(crane)'` on a machine with crane installed.
- **Garbage collection** — the safety rails get a test each: `gc_protects_sole_copy`, `gc_protects_active_deployment_images`, `gc_protects_tagged_manifest_layers`, `gc_protects_within_retention_window`, and the positive case `gc_collects_unreferenced_blob`. These are the tests that let you trust GC won't eat your last copy of a layer. `gc_never_nominates_a_catalogued_manifests_own_blob` pins the REG1 fix, and `tests/suite/pickle_integrity.rs` runs the full push → GC → peer-pull acceptance sequence against real in-process registries.

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
