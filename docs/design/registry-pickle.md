# Pickle: Built-In Distributed Image Registry

**Component:** Pickle (image registry)
**Whitepaper Section:** 12
**Status:** Design

---

## 1. Overview

Pickle is Reliaburger's built-in, distributed, OCI-compatible container image registry. Rather than requiring an external registry service (Docker Hub, Harbor, ECR), Pickle embeds image storage directly into every cluster node. Every node has a local image store on disk. When an image is pushed to any node, Pickle stores it locally, commits the manifest to Raft, and makes it available for peer-to-peer distribution across the cluster. A background heal loop replicates layers to peer nodes over time until the configured redundancy is met.

Core capabilities:

- **Asynchronous, eventual replication.** A push stores the blobs locally, commits the manifest to Raft, and returns `201 Created` with an `oci-replication: pending` header. It does **not** block on replication. A leader-only heal loop (running roughly every 60s) then copies layers to peer nodes until the configured `redundancy` is met. `redundancy` counts the pushing node itself, so the default of 2 means two total copies — the pusher plus one peer. A freshly pushed image is therefore not guaranteed to survive the immediate loss of the pushing node until the heal loop has run at least once.
- **P2P layer distribution.** OCI images are composed of content-addressed layers. Pickle downloads different layers from different peer nodes simultaneously (BitTorrent-like fan-out), bounding load on any single node and decreasing total deployment time as cluster size increases.
- **Pull-through cache.** For images from external registries (Docker Hub, GHCR, ECR), Pickle acts as a transparent pull-through cache. The first node to need an external image pulls it from upstream; every subsequent node pulls from the peer cache.
- **OCI Distribution API.** Any OCI-compatible tool works: `docker push`, `crane push`, `buildah push`, etc.
- **Integrated image signing.** Keyless signing via workload identity (Sigstore/cosign compatible), with optional enforcement that unsigned images are unschedulable.
- **Build job integration.** Build jobs push directly to Pickle via the `pickle://` URI scheme through a scoped Unix socket, eliminating the need for Docker-in-Docker or external CI registries.

### 1.1 Current listener and evidence contract

The implementation derives the registry listener from the cluster address contract. A
standalone node keeps the loopback default. A clustered node replaces that default with its
gossip-advertised IP; `0.0.0.0`/`::` covers the advertised address, and an explicit different
interface fails startup. Peers therefore never receive a registry URL whose address Bun did
not bind.

Cluster reads and writes require authentication even before an operator creates the first
user token. Normal nodes use the master-key-derived internal service token; a cluster missing
that token still refuses anonymous requests. With a node identity the listener and peer client
use cluster TLS. The tokenless bootstrap is limited to a standalone registry, which remains
on loopback by default.

`GET /v1/capabilities` exposes the actual listener, readiness, peer reachability, TLS/P2P
state, redundancy target, active membership count and under-replicated layer count. Phase
15 diagnostics must use those live fields. They must not infer redundancy from configuration
or turn an impossible target into a green skip.

---

## 2. Dependencies

| Dependency | Role in Pickle |
|-----------|---------------|
| **Bun** (node agent) | Runs the Pickle image store on each node. Manages local layer storage, executes garbage collection, tracks under-replicated images, handles replication to peers, and mounts the scoped Unix socket for build jobs. |
| **Raft** (council consensus) | Stores image manifests (the metadata describing which layers compose an image) for consistency. The Raft state machine is the authoritative source for manifest data, tag-to-digest mappings, and the peer location map (which nodes hold which layers). |
| **Mustard** (gossip protocol) | Provides cluster membership and peer discovery. Pickle uses Mustard to discover which nodes are alive and their network addresses, enabling peer selection for replication and parallel downloads. Mustard also disseminates node resource summaries that inform peer selection (e.g., least-loaded node). |
| **Sesame** (security / mTLS / identity) | Provides the mTLS certificates for secure inter-node layer transfers and the Workload CA that mints the persistent per-namespace build-signer identity used for keyless image signing (an X.509 leaf, not a JWT). Verification chains the signature back to the cluster root CA. |
| **Meat** (scheduler) | Consumes image availability from Raft state to make scheduling decisions. Meat considers an image schedulable once its manifest exists in Raft with sufficient replication. Meat refuses to schedule unsigned images when `require_signatures = true`. |

---

## 3. Architecture

### 3.1 Node-Local Store

Every node maintains a local content-addressed store on disk under a configurable root directory (governed by `[images] max_storage`). The store contains:

- **Layers** (blobs): stored by their content digest (SHA-256), deduplicated across all images on the node.
- **Manifests**: stored as ordinary content-addressed blobs (they are just more bytes under `blobs/sha256/`). The authoritative catalogue — which digest a `repository:tag` resolves to, and which nodes hold each layer — lives in Raft.
- **Tags**: mirrored into a local `pickle-catalog.json` file (under the node's data directory), kept in sync with the Raft catalogue.

```
<images root, e.g. /var/lib/reliaburger/images>/
  blobs/
    sha256/
      aabbccdd.../data          # layer, config, or manifest blob
      eeff0011.../data
  <tmp upload dir>/             # in-progress uploads (atomic rename on completion)

<data dir>/
  pickle-catalog.json           # local mirror of the Raft manifest/tag catalogue
```

The blob store shares its on-disk layout with the local image store (`grill::image::ImageStore`), so cached and pushed blobs interoperate. There is no separate `manifests/` symlink tree and no embedded key-value database in the Pickle store itself; the local catalogue is the single JSON file above, and Raft is authoritative.

### 3.2 Push Flow

Push is a local, synchronous commit followed by asynchronous replication. The
receiving node never blocks on peers.

```
Client (docker push / crane push / build job)
  │
  ▼
[1] OCI Distribution API endpoint on receiving node (Bun HTTP server)
    │
    ├── Receive layer blobs via chunked upload
    │   └── Stream to tmp upload dir, verify SHA-256 on completion
    │       └── Atomic rename to blobs/sha256/<digest>/data
    │
    ├── Receive manifest
    │   └── Validate manifest references (all layers present locally)
    │
    ▼
[2] Store manifest blob locally on disk
    │
    ▼
[3] Commit manifest to Raft
    │
    ├── Propose the catalogue entry (repository, tag, digest, layers,
    │   initial holder = receiving node)
    │
    ├── Raft commits → manifest is now the authoritative record
    │
    ▼
[4] Return to client
    │
    ├── 201 Created + `oci-replication: pending` (Raft committed), or
    │   202 Accepted + `oci-replication: raft-uncommitted` (stored and
    │   persisted locally, but the Raft proposal did not commit)
    │
    ▼
[5] Meat considers the image schedulable once the manifest exists in Raft
    │
    ▼
[6] Leader-only heal loop (≈ every 60s) later replicates layers to peers
    │
    ├── For each under-replicated manifest (rarest first, capped per tick):
    │   └── Pull any layers the leader lacks, then stream layers to peers
    │       that lack them, over the registry's HTTP transport (TLS when
    │       cluster TLS is configured); peers verify SHA-256 on receipt.
    │
    └── Update the Raft layer-location map as holders change, until every
        layer meets the configured redundancy.
```

Replication is never on the push critical path. There is no synchronous
replicate-then-commit mode and no configurable per-push durability toggle; the
heal loop is the sole replication driver. `redundancy` includes the pushing
node, so `redundancy = 2` targets one peer copy in addition to the pusher.

### 3.3 Pull Flow

```
Meat schedules app.api to Node 4 → Node 4 needs myapp:v1.4.2
  │
  ▼
[1] Check local store
    ├── Layer already cached? Use it immediately.
    │
    ▼ (cache miss)
[2] Resolve manifest from Raft state (or local cache)
    ├── Manifest contains list of layer digests + sizes
    │
    ▼
[3] Query peer location map (from Raft state)
    ├── For each layer: which nodes hold it?
    │
    ▼
[4] Parallel multi-source download
    │
    ├── layer sha256:aaa (50MB) ← Node 5 (closest / least loaded)
    ├── layer sha256:bbb (30MB) ← Node 3
    ├── layer sha256:ccc (5MB)  ← Node 1 (only source)
    │
    ├── All downloads happen concurrently (tokio tasks)
    ├── Each download verifies SHA-256 on completion
    ├── Large layers may be range-requested from multiple sources
    │
    ▼
[5] Store layers locally, update local manifest cache
    │
    ▼
[6] Report new layer holdings to council (async, via reporting tree)
    │
    ▼
[7] Start container
```

### 3.4 Manifest Storage in Raft

Image manifests are small (typically < 10KB) and must be consistent across the cluster. They are stored in the Raft state machine as key-value entries:

- **Key:** `images/<repository>/<tag>` maps to a manifest digest.
- **Key:** `images/<repository>/manifests/<digest>` contains the full OCI manifest JSON.
- **Key:** `images/locations/<layer_digest>` contains the set of node IDs holding the layer.

This ensures that tag resolution (which digest does `myapp:v1.4.2` point to?) is always consistent, even during concurrent pushes. Layer location data is updated asynchronously via the reporting tree but committed to Raft periodically to survive leader elections.

### 3.5 Layer Storage on Disk

Layer blobs are the bulk of image data (megabytes to gigabytes) and are too large for Raft. They live on each node's local filesystem in the content-addressed blob store. Layer transfer between nodes uses a direct node-to-node HTTP streaming protocol over mTLS, outside the Raft consensus path.

---

## 4. Data Structures

### 4.1 Core Structs

```rust
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

/// Unique identifier for a content-addressed object (layer or config blob).
#[derive(Clone, Debug, Eq, Hash, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
pub struct Digest(pub String); // e.g. "sha256:aabbccddee..."

/// Unique identifier for a node in the cluster.
#[derive(Clone, Debug, Eq, Hash, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
pub struct NodeId(pub u64);

/// OCI image manifest stored in Raft state.
/// Represents the metadata for a single image (one platform).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ImageManifest {
    /// OCI schema version (always 2).
    pub schema_version: u32,

    /// Media type of this manifest.
    /// e.g. "application/vnd.oci.image.manifest.v1+json"
    pub media_type: String,

    /// Digest of the image configuration blob.
    pub config: LayerDescriptor,

    /// Ordered list of layer descriptors composing this image.
    pub layers: Vec<LayerDescriptor>,

    /// Optional annotations (OCI spec).
    pub annotations: BTreeMap<String, String>,

    /// Pickle-specific metadata (not part of OCI spec, stored alongside).
    pub pickle_meta: ManifestMetadata,
}

/// Describes a single content-addressed blob (layer or config).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LayerDescriptor {
    /// Media type of the blob.
    /// e.g. "application/vnd.oci.image.layer.v1.tar+gzip"
    pub media_type: String,

    /// Content-addressable digest.
    pub digest: Digest,

    /// Size in bytes.
    pub size: u64,

    /// Optional annotations.
    pub annotations: BTreeMap<String, String>,
}

/// Pickle-internal metadata attached to each manifest in Raft.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ManifestMetadata {
    /// Repository name (e.g. "api", "frontend").
    pub repository: String,

    /// Tags pointing to this manifest.
    pub tags: BTreeSet<String>,

    /// When the manifest was first pushed.
    pub created_at: SystemTime,

    /// Node that originally received the push.
    pub pushed_by: NodeId,

    /// Signature status.
    pub signature: Option<ImageSignature>,

    /// Total uncompressed size of all layers (for display / quota).
    pub total_size: u64,
}

/// Tracks which nodes hold a given layer and its replication health.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PeerLocation {
    /// The layer digest this location record describes.
    pub digest: Digest,

    /// Set of node IDs currently holding a verified copy of this layer.
    pub holders: BTreeSet<NodeId>,

    /// Last time this location record was updated.
    pub last_updated: SystemTime,
}

/// Tracks the replication state of an image across the cluster.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReplicationState {
    /// Manifest digest this state tracks.
    pub manifest_digest: Digest,

    /// Per-layer replication status.
    pub layer_replicas: BTreeMap<Digest, LayerReplicationStatus>,

    /// Overall replication health.
    pub health: ReplicationHealth,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LayerReplicationStatus {
    /// Desired replica count (from [images] redundancy).
    pub desired: u32,

    /// Current verified replica count.
    pub actual: u32,

    /// Nodes holding this layer.
    pub holders: BTreeSet<NodeId>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum ReplicationHealth {
    /// All layers meet the desired redundancy level.
    Healthy,
    /// Some layers are under-replicated but at least 1 copy exists.
    UnderReplicated { layers: Vec<Digest> },
    /// At least one layer has zero known copies (image is lost).
    Lost { layers: Vec<Digest> },
}

/// Per-node garbage collection policy and state.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GcPolicy {
    /// Maximum number of tags to retain per repository.
    pub retain_tags: u32,

    /// Number of days to retain unreferenced images.
    pub retain_days: u32,

    /// Maximum total storage for the Pickle store on this node.
    pub max_storage: u64,

    /// Set of manifest digests currently referenced by running deployments.
    /// Populated from Raft state before each GC run.
    pub active_refs: BTreeSet<Digest>,
}

/// State maintained during a GC sweep on a single node.
#[derive(Clone, Debug)]
pub struct GcSweepState {
    /// Layers identified as candidates for deletion.
    pub candidates: Vec<GcCandidate>,

    /// Layers that passed all safety checks and were actually deleted.
    pub deleted: Vec<Digest>,

    /// Layers that were spared (sole copy or active reference).
    pub spared: Vec<(Digest, SpareReason)>,
}

#[derive(Clone, Debug)]
pub struct GcCandidate {
    pub digest: Digest,
    pub size: u64,
    pub last_referenced: SystemTime,
    pub reference_count: u32,
}

#[derive(Clone, Debug)]
pub enum SpareReason {
    /// This node is the only known holder of the layer.
    SoleCopy,
    /// Layer is referenced by an active manifest.
    ActiveReference { manifest: Digest },
    /// Layer is within the retention window.
    WithinRetention,
}

/// Cosign-compatible image signature.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ImageSignature {
    /// The signing method.
    pub method: SigningMethod,

    /// Base64-encoded signature payload.
    pub signature: String,

    /// The certificate or public key used to verify.
    pub verification_material: VerificationMaterial,

    /// When the signature was created.
    pub signed_at: SystemTime,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum SigningMethod {
    /// Keyless signing using the build node's persistent build-signer
    /// identity (a Workload-CA leaf, `spiffe://…/job/build-signer`).
    /// `issuer`/`identity` are descriptive labels recorded on the
    /// signature, not a live OIDC issuance path.
    Keyless {
        issuer: String,           // descriptive label
        identity: String,         // e.g. "spiffe://prod/ns/default/job/build-signer"
    },
    /// External key-based signing (cosign with a pre-registered public key).
    ExternalKey {
        key_id: String,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum VerificationMaterial {
    /// Certificate chain for keyless signatures: the build-signer leaf
    /// plus the cluster's Workload CA, verified back to the cluster root
    /// CA (no Fulcio involved).
    CertificateChain(Vec<Vec<u8>>),
    /// Public key bytes (for external key signing).
    PublicKey(Vec<u8>),
}

/// Configuration for an external registry used for pull-through caching.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ExternalRegistry {
    /// Registry hostname (e.g. "ghcr.io", "docker.io").
    pub host: String,

    /// Username for authentication (optional for public registries).
    pub username: Option<String>,

    /// Name of an environment variable holding the password/token.
    /// Resolved once at startup, not an age-encrypted cluster secret
    /// (see Section 8.3). Unset means anonymous access.
    pub password_secret: Option<String>,
}

/// A request to replicate a set of layers to a peer node.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ReplicationRequest {
    /// The manifest this replication is for (informational).
    pub manifest_digest: Digest,

    /// Layers to replicate.
    pub layers: Vec<LayerDescriptor>,

    /// Target node.
    pub target: NodeId,

    /// Whether to wait for completion (sync push) or fire-and-forget (async).
    pub synchronous: bool,
}

/// Outcome of a replication attempt.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum ReplicationResult {
    Success {
        target: NodeId,
        layers_transferred: u32,
        bytes_transferred: u64,
        duration: Duration,
    },
    PartialFailure {
        target: NodeId,
        succeeded: Vec<Digest>,
        failed: Vec<(Digest, String)>,
    },
    Failure {
        target: NodeId,
        reason: String,
    },
}
```

### 4.2 Raft State Machine Entries

```rust
/// Commands proposed to the Raft state machine for Pickle operations.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum PickleRaftCommand {
    /// Commit a new manifest (after push + replication).
    ManifestCommit {
        repository: String,
        tag: String,
        digest: Digest,
        manifest: ImageManifest,
        initial_holders: BTreeSet<NodeId>,
    },

    /// Update the peer location map for a layer.
    UpdateLayerLocations {
        digest: Digest,
        added: BTreeSet<NodeId>,
        removed: BTreeSet<NodeId>,
    },

    /// Record that a node has completed GC and removed layers.
    GcReport {
        node: NodeId,
        removed_layers: Vec<Digest>,
    },

    /// Attach a signature to an existing manifest.
    AttachSignature {
        manifest_digest: Digest,
        signature: ImageSignature,
    },

    /// Delete a tag (but not the manifest if other tags reference it).
    DeleteTag {
        repository: String,
        tag: String,
    },
}
```

### 4.3 On-Disk Layout

The layout below reflects what Bun actually writes. Blobs (layers, config
blobs, and manifests alike) live under the images root; the tag/manifest index
is a single JSON file in the data directory, mirroring the authoritative Raft
catalogue. There is no on-disk `manifests/` symlink tree and no embedded KV
database in the Pickle store.

```
<images root>/                       # e.g. /var/lib/reliaburger/images
└── blobs/
    └── sha256/
        ├── <digest_hex>/
        │   └── data                 # blob content: layer, config, or manifest bytes
        └── .../

<tmp upload dir>/                     # in-progress chunked uploads (atomic rename on completion)

<data dir>/
└── pickle-catalog.json              # local mirror of the Raft manifest/tag catalogue
```

Pull-through cache entries are not a separate on-disk namespace: an upstream
image is committed like any other push, under a `cache/<host>/<repo>`
repository name in the same blob store and catalogue.

---

## 5. Operations

### 5.1 OCI Push

Pickle implements the OCI Distribution Spec push flow:

1. **Initiate upload session.** Client calls `POST /v2/<name>/blobs/uploads/` to start a chunked upload. Bun returns an upload UUID and a `Location` header.

2. **Upload layer chunks.** Client sends `PATCH /v2/<name>/blobs/uploads/<uuid>` with chunk data. Bun streams chunks to `tmp/upload-<uuid>/data`, tracking byte offsets.

3. **Complete layer upload.** Client calls `PUT /v2/<name>/blobs/uploads/<uuid>?digest=sha256:...`. Bun:
   - Computes SHA-256 of the received data.
   - Verifies it matches the client-provided digest.
   - Atomic-renames from `tmp/` to `blobs/sha256/<digest>/data`.
   - Returns `201 Created` with `Docker-Content-Digest` header.

4. **Push manifest.** Client calls `PUT /v2/<name>/manifests/<reference>` with the manifest JSON. Bun:
   - Validates the manifest (media type, all referenced layers exist locally).
   - Stores the manifest blob locally.

5. **Raft commit.** Propose the catalogue entry (manifest, tag, initial holder = receiving node) to the Raft state machine.

6. **Return to client.** `201 Created` with `oci-replication: pending` when the Raft proposal committed, or `202 Accepted` with `oci-replication: raft-uncommitted` when it did not. The receiving node never waits on peers.

7. **Background replication.** The leader-only heal loop later replicates layers to peers until the configured redundancy is met (see §3.2). This is not part of the push response.

```
Sequence (push, redundancy=2):

Client          Node A (receiver)                         Raft
  │                  │                                       │
  ├─POST uploads/───►│                                       │
  │◄──upload UUID────┤                                       │
  ├─PATCH chunks────►│                                       │
  ├─PUT  digest─────►│                                       │
  ├─PUT  manifest───►│                                       │
  │                  ├──commit catalogue entry──────────────►│
  │                  │◄──────────────────────────────commit──┤
  │◄─201, oci-replication: pending─┤                         │

Later, independently:

Leader heal loop (≈60s)  ──stream missing layers──►  peer nodes
                         ──update layer-location map──►  Raft
```

### 5.2 OCI Pull

Pickle implements the OCI Distribution Spec pull flow:

1. **Resolve tag.** Client calls `GET /v2/<name>/manifests/<reference>`. Bun resolves the tag to a manifest digest from its local cache (refreshed from Raft state).

2. **Return manifest.** The manifest JSON is returned with `Docker-Content-Digest` header.

3. **Pull layers.** For each layer in the manifest, client calls `GET /v2/<name>/blobs/<digest>`. Bun:
   - **Local hit:** Stream directly from local blob store.
   - **Local miss:** Look up `PeerLocation` for this digest from Raft-synchronised state. Select the best source (closest, least loaded). Fetch the layer from the peer via HTTP streaming, store locally, and stream to the client simultaneously (tee).

### 5.3 Parallel Multi-Source Downloads

When a node needs to pull an image it doesn't have, it exploits the content-addressed nature of OCI layers to download from multiple peers concurrently:

```rust
/// Downloads all layers for a manifest in parallel from the best available peers.
async fn parallel_pull(
    manifest: &ImageManifest,
    locations: &HashMap<Digest, PeerLocation>,
    local_store: &BlobStore,
) -> Result<(), PullError> {
    let mut tasks = Vec::new();

    for layer in &manifest.layers {
        if local_store.has(&layer.digest).await? {
            continue; // already cached
        }

        let peers = locations.get(&layer.digest)
            .ok_or(PullError::NoKnownSource(layer.digest.clone()))?;

        // Select best peer: prefer closest (same rack > same zone > any),
        // then least loaded, then random tiebreak.
        let source = select_best_peer(&peers.holders).await?;

        let digest = layer.digest.clone();
        let store = local_store.clone();
        tasks.push(tokio::spawn(async move {
            fetch_layer_from_peer(&digest, &source, &store).await
        }));
    }

    // Await all downloads concurrently.
    for task in tasks {
        task.await??;
    }

    Ok(())
}
```

For very large layers, a single layer can be split into range requests served by multiple peers simultaneously (similar to HTTP range requests in download accelerators). The content-addressed digest verifies integrity of the reassembled layer.

As images fan out during a deployment, each node that completes a download becomes a new source. The first few nodes pull from the original holders, but subsequent nodes pull from peers that already have the layers. This creates exponential fan-out:

```
Time T0: Nodes [1, 5, 9] have layers (original push + 2 replicas)
Time T1: Nodes [2, 3, 4] pull in parallel from [1, 5, 9]
         → 6 nodes now have layers
Time T2: Nodes [6, 7, 8, 10, 11, 12] pull from any of the 6 holders
         → 12 nodes now have layers
...exponential fan-out continues
```

### 5.4 Pull-Through Cache for External Registries

When an image reference includes a registry hostname (e.g., `docker.io/redis:7-alpine`), Pickle operates as a pull-through cache:

1. **First request.** Bun checks the local store and the cluster peer location map. On a cluster-wide miss:
   - Authenticate to the external registry using credentials from `[images] external_registries` (the `password_secret` names an environment variable resolved once at startup — see §8.3).
   - Pull the manifest and layers from the upstream registry.
   - Store locally and commit the manifest to Raft (same as a regular push); the heal loop replicates to peers afterwards.

2. **Subsequent requests.** Other nodes resolve the image from Raft state and pull layers from peers. The upstream registry is never contacted again until the cached manifest expires or is explicitly refreshed.

3. **Tag re-resolution.** For mutable tags (e.g., `redis:7-alpine`), Pickle periodically re-checks the upstream registry for manifest changes (configurable interval, default 1 hour). If the upstream digest has changed, the new manifest and any new layers are pulled and cached.

```rust
/// Pull-through cache resolution for an external image reference.
async fn resolve_external(
    registry: &str,
    repository: &str,
    reference: &str,
    config: &ExternalRegistriesConfig,
    raft_state: &RaftState,
    local_store: &BlobStore,
) -> Result<ImageManifest, PullError> {
    // Check if we have a cached manifest in Raft.
    if let Some(cached) = raft_state.get_external_manifest(registry, repository, reference).await? {
        if !cached.is_stale() {
            return Ok(cached.manifest);
        }
    }

    // Authenticate to external registry.
    let creds = config.credentials_for(registry)?;
    let client = OciRegistryClient::new(registry, creds).await?;

    // Pull manifest from upstream.
    let manifest = client.pull_manifest(repository, reference).await?;

    // Pull any layers we don't have cluster-wide.
    for layer in &manifest.layers {
        if !raft_state.layer_exists(&layer.digest).await? {
            let blob = client.pull_blob(repository, &layer.digest).await?;
            local_store.store(&layer.digest, blob).await?;
        }
    }

    // Replicate and commit (same as regular push).
    replicate_and_commit(&manifest, local_store, raft_state).await?;

    Ok(manifest)
}
```

### 5.5 Garbage Collection

Pickle runs per-node garbage collection on a configurable schedule. The GC algorithm is designed to be safe against concurrent operations and globally aware via Raft.

**GC algorithm (per node):**

```
[1] Build local inventory: all blobs on this node, with sizes and reference counts.

[2] Build active reference set from Raft state:
    - All manifests referenced by running or recently-deployed apps.
    - The last N tags per repository (default N=10 from gc_retain_tags).

[3] For each local blob not in the active reference set:
    │
    ├─ Is it within the retention window (gc_retain_days)?
    │  └─ Yes → skip (within retention).
    │
    ├─ Is this node the sole known holder (from Raft peer location map)?
    │  └─ Yes → skip (sole copy protection).
    │
    ├─ Is the blob referenced by any active manifest in Raft?
    │  └─ Yes → skip (active reference).
    │
    └─ Safe to delete:
       ├─ Delete from local disk.
       ├─ Propose GcReport to Raft (update peer location map).
       └─ Log deletion.
```

**Sole-copy protection:** Before deleting any layer, Bun reads the `PeerLocation` for that digest from the Raft-synchronised state. If `holders.len() <= 1` and this node is in the holder set, the layer isn't deleted regardless of other GC criteria. This prevents the last copy of a layer from being garbage collected.

**Raft location map update:** After a GC run, Bun proposes a `GcReport` to Raft listing all removed layers. The Raft state machine removes this node from the holder sets, ensuring other nodes never attempt to fetch from stale locations.

**Reference counting:** Each blob on disk maintains a local reference count (number of manifests on this node that include the layer). Only blobs with a local reference count of zero are GC candidates. This is a local-only optimisation; the global safety check (Raft active manifests) provides the authoritative protection.

### 5.6 Image Signing

Pickle supports image signing compatible with the Sigstore/cosign ecosystem.

**Keyless signing (build jobs):**

When a build job with `build = true` pushes an image, the signing flow is:

```
[1] Build job completes, pushes image to Pickle via Unix socket.

[2] Bun intercepts the push completion.

[3] The node uses a persistent, per-namespace build-signer identity
    (`spiffe://<cluster>/ns/<namespace>/job/build-signer`), a leaf
    certificate the council mints once from the cluster's Workload CA
    and Bun caches — not a fresh ephemeral CSR per build.

[4] Bun signs the image manifest digest with the build-signer's
    ECDSA P-256 private key (cosign-compatible signature format).

[5] The signature plus the certificate chain (build-signer leaf +
    Workload CA) are attached to the manifest in the Raft catalogue via
    the AttachSignature command. Signatures are stored in the catalogue,
    not as OCI referrer artifacts.

[6] Verification checks the signature against the digest and validates
    the certificate chain back to the cluster's root CA (and against the
    CRL, so a revoked signer fails closed).
```

There is no Fulcio-style ephemeral-key/OIDC exchange and no external signing key to manage: the build-signer is tied to the cluster's own Workload CA, so the trust policy accepts it by construction. Because the identity is persistent, `issuer` and `identity` on the signature are descriptive labels, not a live OIDC issuance path.

**External key signing:**

For images pushed from external CI systems:

```
[1] Developer signs the image with their own cosign key:
    cosign sign --key <private-key> mycluster:5000/myapp:v1.4.2

[2] The signature is recorded against the manifest in the Raft
    catalogue (not stored as a separate OCI referrer artifact).

[3] On schedule, Meat verifies the signature against the ECDSA P-256
    public keys registered in cluster configuration (base64-encoded):
    [images.trust_policy]
    keys = ["<base64 ECDSA P-256 public key>"]

[4] If verification succeeds, the image is schedulable.
```

**Enforcement:**

When `require_signatures = true`:

- Unsigned images are accepted into Pickle (pushes don't fail).
- Unsigned images remain **unschedulable** -- Meat refuses to place them.
- `relish inspect <image>` shows signature status.

```bash
$ relish inspect api:v1.4.3
Repository:  api
Tag:         v1.4.3
Digest:      sha256:abc123...
Signed:      yes (keyless, identity: spiffe://prod/ns/default/job/build-api)
Replicas:    3/3 nodes
```

### 5.7 Proactive Distribution

> **Status: planned — not yet implemented.** The `pre_pull` config key and the
> pre-pull loop below are a design sketch. No proactive pre-pull runs today;
> nodes fetch an image's layers when the scheduler places a workload that needs
> them (§3.3). The following is the intended shape, not shipped code.

Nodes could optionally pre-pull images that appear in cluster registry announcements, even before being scheduled to run them:

```rust
/// Pre-pull handler: listens for new manifest commits in Raft
/// and proactively fetches layers if pre_pull is enabled.
async fn pre_pull_loop(
    raft_watcher: &mut RaftWatcher,
    config: &PickleConfig,
    local_store: &BlobStore,
) {
    if !config.pre_pull {
        return;
    }

    while let Some(event) = raft_watcher.next_manifest_commit().await {
        // Don't pre-pull if we're low on storage.
        if local_store.available_space().await < config.pre_pull_min_free {
            continue;
        }

        // Background fetch — do not block the event loop.
        let store = local_store.clone();
        tokio::spawn(async move {
            if let Err(e) = parallel_pull(&event.manifest, &event.locations, &store).await {
                tracing::warn!(repo=%event.repository, tag=%event.tag, err=%e,
                    "pre-pull failed, will retry on actual schedule");
            }
        });
    }
}
```

By the time Meat schedules a new replica on a node, the image may already be cached, reducing scheduling-to-running latency to near zero.

### 5.8 Build Job Integration

Build jobs interact with Pickle through a scoped access model:

1. **`pickle://` URI scheme.** Build jobs reference the Pickle registry using `pickle://` URIs (e.g., `pickle://api:v1.4.3`). Bun translates this to the local Pickle registry endpoint.

2. **Scoped Unix socket.** When a job has `build = true`, Bun mounts a Unix socket into the job container that provides write access to the local Pickle registry. This socket:
   - Is scoped to the repositories listed in `build_push_to` (e.g., `build_push_to = ["api"]` allows pushing only to `pickle://api:*`).
   - Doesn't provide read access to other images in the registry.
   - Isn't mounted into non-build containers.

3. **Automatic signing.** When the build job pushes an image through the Unix socket, Bun automatically signs it using the job's workload identity (see Section 5.6).

```toml
[job.build-api]
image = "gcr.io/kaniko-project/executor:latest"
command = [
  "--context", "/workspace",
  "--destination", "pickle://api:v1.4.3",
  "--cache=true"
]
build = true                     # grants Pickle registry write access
build_push_to = ["api"]          # scoped: can only push to pickle://api:*
source = "git::main::services/api"
trigger = "push"
run_before = ["app.api"]
```

The Unix socket approach avoids granting the build container network access to the registry API (which would allow pushing to any repository). The socket is intercepted by Bun, which enforces the `build_push_to` scope before forwarding the request to the local Pickle store.

---

## 6. Configuration

All Pickle configuration lives in the `[images]` section of `node.toml`:

```toml
[images]
# Maximum disk space for the Pickle blob store on this node.
# When exceeded, GC runs more aggressively (evicting beyond retain policy).
max_storage = "50Gi"

# Target number of layer copies across the cluster, INCLUDING the pushing
# node. The leader-only heal loop replicates toward this target in the
# background; push itself does not wait. Default 2 = pusher + one peer.
redundancy = 2

# Number of days to retain unreferenced images before GC eligibility.
# Images referenced by running deployments are never collected regardless.
# Default: 7
gc_retain_days = 7

# External registries for pull-through caching (see the pull_through flag).
# Pickle authenticates to these when pulling external images. password_secret
# names an environment variable resolved once at startup (Section 8.3), NOT an
# age-encrypted cluster secret. Unset means anonymous access.
external_registries = [
  { host = "ghcr.io", username = "bot", password_secret = "GHCR_TOKEN" },
  { host = "docker.io", username = "myorg", password_secret = "DOCKERHUB_TOKEN" },
]

# Image trust policy. Require all Pickle-hosted images to be signed before
# Meat will schedule them. Unsigned images are accepted into Pickle but
# remain unschedulable. Default: false.
[images.trust_policy]
require_signatures = false
```

> **Not shipped as config keys:** the `push_sync` toggle does not exist — there
> is no synchronous-push mode to switch off (§3.2). `gc_retain_tags`
> (retain-N-tags-per-repo) and `pre_pull` (§5.7 proactive distribution) are
> **planned — not yet implemented**, and are not parsed today.

**Corresponding Rust config struct:**

```rust
#[derive(Clone, Debug, Deserialize)]
pub struct PickleConfig {
    /// Maximum blob store size on this node.
    pub max_storage: ByteSize,

    /// Target layer copies across the cluster, including the pusher.
    #[serde(default = "default_redundancy")]
    pub redundancy: u32,

    /// GC: retain unreferenced images for this many days.
    #[serde(default = "default_gc_retain_days")]
    pub gc_retain_days: u32,

    /// External registries for pull-through caching.
    #[serde(default)]
    pub external_registries: Vec<ExternalRegistry>,

    /// Image trust policy (signature requirements) — nested under
    /// `[images.trust_policy]`.
    #[serde(default)]
    pub trust_policy: TrustPolicySection,
    // Note: the shipped `ImagesSection` also carries registry_port,
    // registry_bind, p2p_concurrency, pull_through, cache_recheck_secs,
    // build_timeout_secs, and max_context_bytes. There is no push_sync,
    // pre_pull, or gc_retain_tags field.
}

fn default_redundancy() -> u32 { 2 }
fn default_gc_retain_days() -> u32 { 7 }
```

---

## 7. Failure Modes

### 7.1 Peers Unreachable at Push Time

**Scenario:** A push arrives at Node A, which stores the layers locally and commits the manifest, but no peers are currently reachable to replicate to.

**Behaviour:** The push still succeeds — replication is not on the push path. Node A stores the blobs, commits the manifest to Raft, and returns `201 Created` with `oci-replication: pending`. The image is immediately schedulable but under-replicated (only Node A holds the layers). If the Raft proposal itself fails on a council member, the response is `202 Accepted` with `oci-replication: raft-uncommitted` instead.

**Recovery:** The leader-only heal loop (§3.2, ≈ every 60s) replicates the layers to peers once any are reachable, until `redundancy` is met. Capability and status reporting expose the under-replicated layer count so operators can see that replication is still pending.

### 7.2 Under-Replicated Images

**Scenario:** A node holding one of the copies goes down permanently (e.g., hardware failure). The image's replica count drops below the desired redundancy.

**Behaviour:** The Raft layer-location map updates as holders change. The image is now under-replicated but remains schedulable from surviving holders.

**Recovery:** The leader-only heal loop detects manifests below `redundancy`, pulls any layers the leader lacks, and replicates them onward to peers that lack them (rarest first, capped per tick), restoring the desired redundancy over successive ticks.

### 7.3 Node Holding Sole Copy Goes Down

**Scenario:** Through an unlikely sequence of failures (or misconfiguration with `redundancy = 0`), a node holding the only copy of an image's layers goes down.

**Behaviour:** The image's `ReplicationState` transitions to `Lost`. Meat can't schedule new instances of the image. Running instances on other nodes (which have the layers cached locally from prior pulls) continue operating.

**Recovery:** If the node recovers, its layers become available again. If the node is permanently lost, the image must be re-pushed. `relish status` and `relish wtf` prominently warn about lost images. No silent data loss occurs -- the system explicitly reports the situation.

### 7.4 GC Race Conditions

**Scenario:** Node A decides to GC a layer at the same moment Node B is trying to pull it from Node A.

**Mitigation:** GC operates in two phases:

1. **Mark phase:** Identify candidates and check Raft state (sole copy, active references). Layers being actively served are excluded via a reader lock on the blob file.
2. **Sweep phase:** Delete candidates. Each deletion is preceded by a final `holders.len() > 1` check against the latest Raft state. If the holder count has changed (e.g., another node GC'd its copy concurrently), the deletion is aborted.

Additionally, Raft commits for GC reports are serialised: only one `GcReport` is processed at a time. This prevents two nodes from simultaneously deciding they are not the sole copy and both deleting.

**Scenario:** A new deployment references an image whose layers are being GC'd.

**Mitigation:** GC reads the active reference set from Raft at the start of each sweep. A Raft commit for a new deployment (which adds the manifest to the active reference set) happens-before any GC decision based on a stale active set. The GC sweep rechecks active references before each individual deletion, catching deployments that were committed during the sweep.

---

## 8. Security Considerations

### 8.1 Scoped Registry Access (`build_push_to`)

Build jobs are granted write access to Pickle only through a Unix socket mounted by Bun. This socket is:

- **Repository-scoped:** The `build_push_to` field limits which repositories the job can push to. A job with `build_push_to = ["api"]` can't push to `pickle://admin-tools:*`. Bun enforces this at the socket interception layer before the request reaches the Pickle store.
- **Write-only for builds:** The socket allows OCI push operations (blob upload, manifest push) but doesn't expose pull operations for other images. A compromised build container can't exfiltrate images from the registry.
- **Not mounted into non-build containers:** Only containers with `build = true` receive the Unix socket. Regular app containers have no mechanism to push to Pickle.

### 8.2 Image Signing Enforcement

When `require_signatures = true`:

- **Unsigned images are accepted but unschedulable.** Pushes never fail due to missing signatures, which avoids breaking CI pipelines. However, Meat refuses to schedule unsigned images. This creates a clear separation: the registry accepts all valid OCI images; the scheduler enforces trust policy.
- **Keyless signing eliminates key management.** Build nodes sign automatically with the cluster's persistent per-namespace build-signer identity (a Workload-CA leaf). No operator-provisioned signing keys to rotate, distribute, or protect.
- **External signatures use cosign-compatible verification.** Teams pushing from external CI register their public keys in the cluster configuration. Pickle verifies signatures against these keys using the standard cosign verification flow.
- **Signature verification is cached.** Once a manifest's signature is verified and recorded in Raft, subsequent scheduling decisions don't re-verify. The signature status is part of the `ManifestMetadata`.

### 8.3 Pull-Through Cache Credential Handling

Credentials for external registries come from the node's startup environment,
not from the cluster secret machinery. `password_secret` on an
`[[images.external_registries]]` entry names an **environment variable** whose
value Bun reads **once, at startup**, into memory. Registry credentials are
needed before the age/secret subsystem is up, and env-injected credentials are
the registry-auth convention, so this is a deliberate exception rather than the
age-encrypted-in-Raft path used for app secrets. An unset variable falls back to
anonymous access for that host. Credentials are:

- **Never checked into git as `ENC[AGE:...]` values.** They are not encrypted in Raft with the cluster age key; they live only in the node's environment and in Bun's memory.
- **Never exposed to workload containers.** The pull-through cache operates at the Bun agent level, not inside any container.
- **Scoped per registry.** Each external registry entry specifies credentials only for that host. A credential for `ghcr.io` can't be used to authenticate to `docker.io`.
- **Not hot-rotatable.** Because the value is resolved once at startup, changing it requires restarting Bun. There is no runtime re-read.

> **Status: partially implemented.** The env-var credential path above is what ships today. Sourcing pull-through credentials from age-encrypted cluster secrets, with rotation that takes effect without a restart, remains planned.

### 8.4 Inter-Node Layer Transfer

All layer transfers between nodes occur over the cluster's mTLS connections (Sesame node certificates). No layer data travels in plaintext. Node identity is verified by the certificate common name, preventing a rogue node from intercepting layer transfers.

---

## 9. Performance

### 9.1 Push Latency

Because replication is off the push path, push latency is the time to receive
and store the blobs locally plus the Raft manifest commit. It does **not**
include peer replication (which the heal loop does afterwards).

| Image size | Expected push latency | Notes |
|-----------|----------------------|-------|
| 5 MB (small app layer) | < 1s | Local store + Raft commit |
| 50 MB (typical app) | ~1s | Upload/store bandwidth to the receiving node |
| 200 MB (large app with deps) | 1-2s | Upload/store bandwidth |
| 1 GB+ (ML model, large base) | seconds | Bounded by client→node upload bandwidth |

Push latency is dominated by the client→receiving-node upload and local disk write. The Raft commit for the manifest is small (< 10KB) and adds negligible latency (typically < 50ms). Replicating to peers happens later, on the heal loop, and does not count against the push.

### 9.2 Fan-Out Speed

During a rolling deployment to N nodes, total image distribution time grows logarithmically with N due to peer-to-peer fan-out:

| Cluster size | Time to distribute 100MB image | Effective throughput |
|-------------|-------------------------------|---------------------|
| 3 nodes | ~3s (direct from holders) | 100 MB/s |
| 10 nodes | ~5s (2 rounds of fan-out) | 200 MB/s aggregate |
| 100 nodes | ~8s (4 rounds of fan-out) | 1.2 GB/s aggregate |
| 1000 nodes | ~12s (6 rounds of fan-out) | 8+ GB/s aggregate |

Each node that completes a download becomes a source, creating exponential fan-out. The load on any individual node is bounded (it serves at most a few concurrent transfers), while aggregate cluster throughput scales with the number of nodes.

### 9.3 Parallel Download Throughput

Per-node pull throughput depends on the number of available sources:

- **Single source:** Limited by the source node's upload bandwidth (typically 1-10 Gbps).
- **Multiple sources (common case):** Each layer is fetched from a different peer concurrently. A 200MB image with 4 layers can be pulled in the time it takes to download the single largest layer.
- **Range-split large layers:** For layers > 100MB, Pickle can split the download across multiple peers using byte-range requests, further reducing pull time.

### 9.4 Storage Overhead

Layer deduplication across images on the same node means that the actual disk usage is often significantly less than the sum of all image sizes. Common base layers (OS, language runtime) are stored once regardless of how many images reference them.

---

## 10. Testing Strategy

### 10.1 Push/Pull Round-Trip

```
Test: push image, verify local storage
  - Push a multi-layer image via OCI API to Node A.
  - Verify all layers exist in Node A's blob store with correct digests.
  - Verify manifest is committed to Raft.
  - Pull the image from Node A via OCI API.
  - Verify pulled manifest and layer digests match the pushed image.
  Expected: 1-2s

Test: push image, pull from different node
  - Push image to Node A (redundancy=2, replicated to B and C).
  - Pull image from Node D (which has no local copy).
  - Verify Node D downloads layers from peers (A, B, or C).
  - Verify pulled image matches pushed image (digest comparison).
  Expected: 3-5s
```

### 10.2 Replication Verification

```
Test: push returns immediately with oci-replication: pending
  - Push image to Node A (redundancy=2).
  - Verify 201 Created with header oci-replication: pending.
  - Verify the manifest is in the Raft catalogue and schedulable at once,
    with Node A as the only initial holder.

Test: heal loop reaches redundancy in the background
  - Push image to Node A with redundancy=2.
  - Run heal ticks (leader-only, ≈60s in production; driven directly in tests).
  - Verify layers appear on one additional node (2 total copies = pusher + 1).
  - Verify the Raft layer-location map reflects both holders.

Test: peers unreachable at push time does not fail the push
  - redundancy=2, take the other nodes offline.
  - Push image to the remaining node.
  - Verify push still succeeds (201, oci-replication: pending); the image is
    under-replicated but committed and schedulable.
  - Bring nodes back online, run heal ticks.
  - Verify redundancy is restored.

Test: under-replicated image auto-heals
  - Push image with redundancy=2 (holders: A, B).
  - Remove a holder from the cluster.
  - Run heal ticks.
  - Verify a surviving/new node now holds the layers and the Raft
    layer-location map is updated.
```

### 10.3 GC Safety

```
Test: GC does not collect active images
  - Push image, deploy app referencing it.
  - Trigger GC manually.
  - Verify image is not collected.

Test: GC collects unreferenced images after retention
  - Push image, do not reference it in any deployment.
  - Advance time past gc_retain_days.
  - Trigger GC.
  - Verify image layers are removed.
  - Verify Raft peer location map updated (node removed from holders).

Test: GC does not delete sole copy
  - Push image with redundancy=0 (single holder).
  - Advance time past gc_retain_days.
  - Trigger GC.
  - Verify image is NOT collected (sole copy protection).

Test: concurrent GC across nodes does not cause total loss
  - Push image with redundancy=2 (holders: A, B, C).
  - Stop all deployments referencing the image.
  - Trigger GC on all three nodes simultaneously.
  - Verify at least one copy survives (serialised GcReport processing).

Test: build job pushes to pickle:// and app deploys from it
  - Define build job with pickle://api:test destination.
  - Run build job.
  - Verify image appears in Pickle.
  - Deploy app referencing pickle://api:test.
  - Verify app starts successfully.
  Expected: ~12s
```

### 10.4 Signing Verification

```
Test: build job auto-signs image
  - Enable require_signatures = true.
  - Run build job with build = true.
  - Verify image has keyless signature in Raft.
  - Verify signature identity matches build job's SPIFFE ID.
  - Deploy app — verify Meat schedules it (signed).

Test: unsigned image is unschedulable
  - Enable require_signatures = true.
  - Push image externally without cosign signature.
  - Attempt to deploy app referencing the image.
  - Verify Meat refuses to schedule (unsigned).
  - Verify relish inspect shows "unsigned".

Test: external cosign signature accepted
  - Register external public key in cluster config.
  - Push image with cosign signature from matching private key.
  - Verify image is schedulable.

Test: require_signatures=false allows all images
  - Leave require_signatures at default (false).
  - Push unsigned image.
  - Deploy app — verify Meat schedules it.
```

### 10.5 Pull-Through Cache

```
Test: external image is cached on first pull
  - Reference docker.io/redis:7-alpine in an app spec.
  - Deploy app — verify first node pulls from Docker Hub.
  - Deploy second replica on different node.
  - Verify second node pulls from peer cache (not Docker Hub).

Test: pull-through with authentication
  - Configure external_registries with ghcr.io credentials.
  - Reference a private GHCR image.
  - Verify pull succeeds with authentication.
  - Verify credentials are not exposed to the app container.
```

---

## 11. Prior Art

### 11.1 Docker Hub / Container Registries

Traditional container registries (Docker Hub, GitHub Container Registry, Amazon ECR, Google Artifact Registry, Azure Container Registry) are centralized services that store and serve images. They provide the OCI Distribution API, authentication, and image scanning, but they are external dependencies that must be provisioned, credentialed, and maintained. They introduce a network dependency for every deploy, and for on-premises, edge, or air-gapped environments, this dependency is especially painful.

**What Pickle borrows:** The OCI Distribution API (push/pull protocol), content-addressed blob storage model, manifest/layer separation.

**What Pickle does differently:** Pickle is built into every node -- no separate service to provision. Images are stored locally on cluster nodes with automatic replication.

### 11.2 Harbor

[Harbor](https://goharbor.io/) is an open-source container registry with enterprise features: RBAC, vulnerability scanning, replication across registries, and a web UI. Harbor is a multi-component system (core, database, Redis, registry, job service) that must be deployed and operated as a separate service.

**What Pickle borrows:** The concept of replication policies and garbage collection with safety checks.

**What Pickle does differently:** Harbor is an external service; Pickle is embedded. Harbor replicates between registry instances; Pickle replicates between cluster nodes at the layer level.

### 11.3 Dragonfly (P2P Image Distribution)

[Dragonfly](https://d7y.io/) is a CNCF project that provides P2P-based image distribution. It works as a proxy/cache layer in front of an existing registry, intercepting image pulls and distributing layers via a P2P network between nodes. Dragonfly uses a supernode/CDN architecture and supports intelligent scheduling of download tasks.

**Reference:** [Dragonfly Architecture](https://d7y.io/docs/concepts/terminology/architecture/)

**What Pickle borrows:** The P2P layer distribution model. The insight that content-addressed layers are naturally suited to multi-source parallel downloads. The fan-out pattern where each node that completes a download becomes a new source.

**What Pickle does differently:** Dragonfly is a separate system overlaid on existing registries. Pickle is the registry -- P2P distribution is native, not retrofitted. Pickle doesn't need a supernode or scheduler daemon; peer selection uses the existing Mustard gossip and Raft location map.

### 11.4 Uber Kraken (P2P Image Distribution)

[Kraken](https://github.com/uber/kraken) is Uber's P2P Docker registry. It uses a BitTorrent-like protocol for layer distribution with dedicated tracker and origin components. Kraken achieves very high throughput in large clusters (distributing a 1GB image to thousands of nodes in under 30 seconds).

**What Pickle borrows:** The BitTorrent-inspired parallel download strategy. The insight that image fan-out speed improves (rather than degrades) as cluster size increases.

**What Pickle does differently:** Kraken is a standalone registry with dedicated tracker/origin infrastructure. Pickle embeds the tracker function in Raft and the origin function in the pushing node, requiring no additional components.

### 11.5 containerd Content Store

[containerd](https://containerd.io/) stores image content in a local content-addressed store (`/var/lib/containerd/io.containerd.content.v1.content/`). Each node manages its own content independently -- there's no built-in replication or P2P distribution.

**What Pickle borrows:** The on-disk content-addressed storage model (blobs indexed by digest). The separation of manifests (metadata) from layers (data).

**What Pickle does differently:** containerd is a local-only store. Pickle adds distributed replication, P2P fan-out, and global manifest consistency via Raft.

### 11.6 OCI Distribution Specification

**Reference:** [OCI Distribution Spec](https://github.com/opencontainers/distribution-spec)

Pickle implements the OCI Distribution Specification for API compatibility. Any tool that speaks this protocol (docker, crane, buildah, podman, skopeo, oras) works with Pickle without modification.

---

## 12. Libraries and Dependencies

### 12.1 Rust Crates

| Crate | Purpose | Notes |
|-------|---------|-------|
| `oci-distribution` | OCI Distribution API client/server primitives | Provides types for manifests, layer descriptors, and registry protocol handling. May need forking/wrapping for the server-side (Pickle is both client and server). |
| `hyper` | HTTP server for the OCI Distribution API endpoint | The registry API is an HTTP endpoint on each node. `hyper` provides the low-level server. |
| `axum` | HTTP framework layered on `hyper` | Routing for the `/v2/` API endpoints (blob uploads, manifest push/pull, tag listing). |
| `reqwest` | HTTP client for external registry pull-through | Used to pull manifests and blobs from upstream registries (Docker Hub, GHCR, ECR). |
| `tokio` | Async runtime | All Pickle I/O (disk, network, replication) is async. tokio provides the task scheduler, timers, and I/O primitives. |
| `ring` or `sha2` | SHA-256 hashing for content addressing | Every blob is verified by its SHA-256 digest. `ring` for performance-critical paths; `sha2` as a pure-Rust fallback. |
| `rustls` | TLS implementation for mTLS connections | Used by `hyper` (HTTPS) and `reqwest` for Sesame certificate-based authentication. |
| `serde_json` (local catalogue) | Local mirror of the manifest/tag catalogue | The shipped Pickle store keeps its tag-to-digest index in a single `pickle-catalog.json` file mirroring the authoritative Raft catalogue; it does **not** embed a key-value database (redb) of its own. Blobs are plain files under `blobs/sha256/`. |
| `serde` + `serde_json` | Serialisation for manifests and Raft commands | OCI manifests are JSON. Raft commands are serialised for consensus. |
| `flate2` | Compression for layer blobs | OCI layers are typically gzip-compressed tars. |
| `tempfile` | Temporary file handling for upload sessions | Atomic file creation in the `tmp/` upload directory. |

### 12.2 OCI Spec Compliance

Pickle targets compliance with:

- **OCI Distribution Spec v1.1** -- the HTTP API for push, pull, and content discovery.
- **OCI Image Spec v1.1** -- the manifest and layer format for container images.
- **OCI Artifacts** -- for storing signatures alongside image manifests.

Compliance is verified by running the [OCI Distribution Spec conformance tests](https://github.com/opencontainers/distribution-spec/tree/main/conformance) against the Pickle endpoint as part of the integration test suite.

---

## 13. Open Questions

### 13.1 Large Image Handling

Very large images (multi-gigabyte ML models, monolithic base images) stress replication: the heal loop must copy a 5GB image's layers to a peer before redundancy is met, and the whole image is under-replicated until it does. (Push itself stays fast, since it does not wait on replication.) Options under consideration:

- **Streaming replication:** Begin replicating layers to peers as they are received (before the full image upload is complete), overlapping client upload with peer replication.
- **Tiered redundancy:** Allow per-image or per-repository redundancy overrides. Large ML model images might use `redundancy = 1` to trade durability for push speed.
- **Chunked layer replication:** Split large layers into chunks for parallel replication to the same peer, saturating network bandwidth.

### 13.2 Cross-Datacenter Replication

The current design assumes a single cluster within a single network (or at least low-latency connectivity between all nodes). For multi-datacenter deployments:

- **Inter-cluster replication:** Should Pickle support replicating images between independent Reliaburger clusters? This would require a federation protocol.
- **WAN-aware peer selection:** If nodes span datacenters, peer selection for replication should prefer intra-datacenter peers for latency, while ensuring at least one cross-datacenter copy for disaster recovery.
- **Bandwidth throttling:** Cross-datacenter replication should be bandwidth-limited to avoid saturating WAN links.

### 13.3 Manifest List / Multi-Architecture Support

OCI supports manifest lists (also called "fat manifests" or image indices) that reference multiple platform-specific manifests under a single tag. For example, `myapp:v1.4.2` might contain manifests for `linux/amd64` and `linux/arm64`.

- **Storage:** Each platform-specific manifest and its layers are stored independently. The manifest list itself is a small JSON document stored in Raft.
- **Replication:** Should all platform variants be replicated, or only the variants that match nodes in the cluster? Replicating all variants wastes storage on homogeneous clusters; replicating only matching variants risks missing a variant when a new architecture node joins.
- **Pull behaviour:** When a node pulls a multi-arch image, it should automatically select the manifest matching its architecture. This requires Bun to report its platform in the node metadata.

### 13.4 Registry Quotas

Should Pickle enforce per-repository or per-namespace storage quotas? The current `max_storage` is a per-node global limit. In multi-tenant environments, a single team pushing large images could exhaust the Pickle storage for the entire node.

### 13.5 Image Vulnerability Scanning

Harbor and commercial registries integrate vulnerability scanning (Trivy, Clair). Should Pickle offer built-in scanning, or delegate to an external scanner? Options:

- **Built-in scanning job:** A Reliaburger job that runs Trivy against newly pushed images and attaches scan results as OCI annotations.
- **Scan-before-schedule policy:** Similar to `require_signatures`, a `require_scan` policy that makes un-scanned or vulnerable images unschedulable.
- **External integration:** Expose a webhook or event stream for external scanners.

### 13.6 Layer-Level Deduplication Across Repositories

Pickle already deduplicates layers on-disk by content address. However, the Raft location map tracks layers per-manifest. If two repositories share a base layer (identical digest), should the location map track the layer independently of manifests? This is already the case in the current design (locations are keyed by layer digest, not by manifest), but the interaction with GC reference counting needs careful analysis to ensure a shared layer is not collected when one repository's images are removed while the other's still reference it.
