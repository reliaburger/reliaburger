# Pickle: Built-In Distributed Image Registry

**Component:** Pickle (image registry)
**Whitepaper Section:** 12
**Status:** Design

---

## 1. Overview

Pickle is Reliaburger's built-in, distributed, OCI-compatible container image registry. Rather than requiring an external registry service (Docker Hub, Harbor, ECR), Pickle embeds image storage directly into every cluster node. Every node has a local image store on disk. When an image is pushed to any node, Pickle stores it locally, immediately replicates it to N peer nodes for redundancy, and makes it available for peer-to-peer distribution across the cluster.

Core capabilities:

- **Synchronous replication on push.** A successful `docker push` guarantees the image survives the failure of any single node. The push doesn't return success until the image has been replicated to N peers (default N=2).
- **P2P layer distribution.** OCI images are composed of content-addressed layers. Pickle downloads different layers from different peer nodes simultaneously (BitTorrent-like fan-out), bounding load on any single node and decreasing total deployment time as cluster size increases.
- **Pull-through cache.** For images from external registries (Docker Hub, GHCR, ECR), Pickle acts as a transparent pull-through cache. The first node to need an external image pulls it from upstream; every subsequent node pulls from the peer cache.
- **OCI Distribution API.** Any OCI-compatible tool works: `docker push`, `crane push`, `buildah push`, etc.
- **Integrated image signing.** Keyless signing via workload identity (Sigstore/cosign compatible), with optional enforcement that unsigned images are unschedulable.
- **Build job integration.** Build jobs push directly to Pickle via the `pickle://` URI scheme through a scoped Unix socket, eliminating the need for Docker-in-Docker or external CI registries.

---

## 2. Dependencies

| Dependency | Role in Pickle |
|-----------|---------------|
| **Bun** (node agent) | Runs the Pickle image store on each node. Manages local layer storage, executes garbage collection, tracks under-replicated images, handles replication to peers, and mounts the scoped Unix socket for build jobs. |
| **Raft** (council consensus) | Stores image manifests (the metadata describing which layers compose an image) for consistency. The Raft state machine is the authoritative source for manifest data, tag-to-digest mappings, and the peer location map (which nodes hold which layers). |
| **Mustard** (gossip protocol) | Provides cluster membership and peer discovery. Pickle uses Mustard to discover which nodes are alive and their network addresses, enabling peer selection for replication and parallel downloads. Mustard also disseminates node resource summaries that inform peer selection (e.g., least-loaded node). |
| **Sesame** (security / mTLS / identity) | Provides the mTLS certificates for secure inter-node layer transfers, the workload identity JWTs used for keyless image signing, and the OIDC issuer infrastructure for Sigstore-compatible verification. |
| **Meat** (scheduler) | Consumes image availability from Raft state to make scheduling decisions. Meat considers an image schedulable once its manifest exists in Raft with sufficient replication. Meat refuses to schedule unsigned images when `require_signatures = true`. |

---

## 3. Architecture

### 3.1 Node-Local Store

Every node maintains a local content-addressed store on disk under a configurable root directory (governed by `[images] max_storage`). The store contains:

- **Layers** (blobs): stored by their content digest (SHA-256), deduplicated across all images on the node.
- **Manifests**: cached locally for fast resolution, but the authoritative copy lives in Raft.
- **Tags**: local index mapping `repository:tag` to a manifest digest, kept in sync with Raft state.

```
/var/lib/reliaburger/pickle/
  blobs/
    sha256/
      aabbccdd.../data          # layer blob (gzip-compressed tar)
      eeff0011.../data          # config blob
  manifests/
    myapp/
      v1.4.2                    # symlink or file containing manifest digest
      sha256:abc123.../manifest.json
  tmp/
    upload-<uuid>/              # in-progress uploads (atomic rename on completion)
```

### 3.2 Push Flow

```
Client (docker push / crane push / build job)
  │
  ▼
[1] OCI Distribution API endpoint on receiving node (Bun HTTP server)
    │
    ├── Receive layer blobs via chunked upload
    │   └── Stream to /tmp/upload-<uuid>/, verify SHA-256 on completion
    │       └── Atomic rename to /blobs/sha256/<digest>/data
    │
    ├── Receive manifest
    │   └── Validate manifest references (all layers present locally)
    │
    ▼
[2] Store locally on disk
    │
    ▼
[3] Replicate to N peer nodes (default N=2)
    │
    ├── Select peers for diversity:
    │   - Different racks / zones / failure domains (from node labels)
    │   - Prefer least-loaded peers (from Mustard resource summaries)
    │   - Avoid peers that already hold the layers
    │
    ├── For each peer:
    │   └── Stream layers over mTLS (Sesame node certificates)
    │       └── Peer verifies SHA-256 on receipt, stores locally
    │
    ├── Wait for all N peers to acknowledge (synchronous)
    │   ├── Timeout: configurable, default 30s
    │   └── If timeout or insufficient peers: return error to client
    │
    ▼
[4] Commit manifest to Raft
    │
    ├── Propose ManifestCommit { repository, tag, digest, layers, holders }
    │   └── holders = [receiving_node, peer_1, peer_2, ...]
    │
    ├── Raft commits → manifest is now the authoritative record
    │
    ▼
[5] Return success to client
    │
    ├── Node reports to its council aggregator via the hierarchical
    │   reporting tree: "myapp:v1.4.2 available, layers [...], held by [...]"
    │
    ▼
[6] Meat considers the image schedulable
```

When `push_sync = false`, step [3] returns immediately after local storage (step [2]) and replication proceeds in the background. The client gets a faster response but loses the single-node-failure durability guarantee until background replication completes.

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

Layer blobs are the bulk of image data (megabytes to gigabytes) and are too large for Raft. They live on each node's local filesystem in the content-addressed blob store. Layer transfer between nodes uses a direct node-to-node gRPC streaming protocol over mTLS, outside the Raft consensus path.

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
    /// Keyless signing using workload identity OIDC token.
    /// The SPIFFE identity of the build job serves as the signing credential.
    Keyless {
        issuer: String,           // cluster OIDC issuer URL
        identity: String,         // e.g. "spiffe://prod/ns/default/job/build-api"
    },
    /// External key-based signing (cosign with a pre-registered public key).
    ExternalKey {
        key_id: String,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum VerificationMaterial {
    /// Fulcio certificate chain (for keyless).
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

    /// Reference to a cluster secret containing the password/token.
    /// Decrypted by Bun at runtime (see Section 5.3).
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

```
/var/lib/reliaburger/pickle/
├── blobs/
│   └── sha256/
│       ├── <digest_hex>/
│       │   ├── data              # the blob content (gzip tar for layers)
│       │   ├── size              # file containing size in bytes (for fast stat)
│       │   └── refcount          # local reference count (number of manifests referencing)
│       └── .../
├── manifests/
│   └── <repository>/
│       ├── tags/
│       │   ├── v1.4.2 → sha256:<digest>   # tag-to-digest symlink
│       │   └── latest → sha256:<digest>
│       └── digests/
│           └── sha256:<digest>/
│               └── manifest.json           # cached manifest (source of truth is Raft)
├── tmp/
│   └── upload-<uuid>/                     # in-progress uploads
│       ├── data                            # partial blob data
│       └── meta.json                       # upload session metadata
├── cache/
│   └── external/                           # pull-through cache entries
│       └── docker.io/
│           └── library/
│               └── redis/
│                   └── 7-alpine/
│                       └── manifest.json
└── pickle.db                               # embedded KV store (sled/redb) for indices
```

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
   - Validates the manifest (schema version, media type, all referenced layers exist locally).
   - Initiates replication (step 5).

5. **Synchronous replication.** For each of N selected peers:
   - Open gRPC stream over mTLS to peer's Pickle replication endpoint.
   - Send layers the peer doesn't already hold (deduplicate by querying peer's blob inventory).
   - Peer stores layers, verifies digests, and sends acknowledgement.
   - If any peer is unreachable, select an alternate peer. If Pickle can't meet the redundancy target within the timeout (default 30s), it returns an error to the client.

6. **Raft commit.** Propose `ManifestCommit` to the Raft state machine with the manifest, tag, initial holders. Wait for commit confirmation.

7. **Return success.** `201 Created` to the client.

```
Sequence (synchronous push, redundancy=2):

Client          Node A (receiver)       Node B (peer)       Node C (peer)       Raft
  │                  │                      │                    │                │
  ├─POST uploads/───►│                      │                    │                │
  │◄──upload UUID────┤                      │                    │                │
  ├─PATCH chunks────►│                      │                    │                │
  ├─PUT  digest─────►│                      │                    │                │
  │                  ├──stream layers──────►│                    │                │
  │                  ├──stream layers───────────────────────────►│                │
  │                  │                      │                    │                │
  │                  │◄─────ack─────────────┤                    │                │
  │                  │◄─────ack──────────────────────────────────┤                │
  │                  │                      │                    │                │
  │                  ├──ManifestCommit──────────────────────────────────────────►│
  │                  │◄─────────────────────────────────────────────────commit───┤
  │◄──201 Created────┤                      │                    │                │
```

### 5.2 OCI Pull

Pickle implements the OCI Distribution Spec pull flow:

1. **Resolve tag.** Client calls `GET /v2/<name>/manifests/<reference>`. Bun resolves the tag to a manifest digest from its local cache (refreshed from Raft state).

2. **Return manifest.** The manifest JSON is returned with `Docker-Content-Digest` header.

3. **Pull layers.** For each layer in the manifest, client calls `GET /v2/<name>/blobs/<digest>`. Bun:
   - **Local hit:** Stream directly from local blob store.
   - **Local miss:** Look up `PeerLocation` for this digest from Raft-synchronised state. Select the best source (closest, least loaded). Fetch the layer from the peer via gRPC streaming, store locally, and stream to the client simultaneously (tee).

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
   - Authenticate to the external registry using credentials from `[images] external_registries` (the `password_secret` references a cluster secret decrypted by Bun at runtime).
   - Pull the manifest and layers from the upstream registry.
   - Store locally and replicate to N peers (same as a regular push).
   - Commit manifest to Raft.

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

[3] Bun requests a workload identity JWT from the council
    (via the CSR flow described in Section 12.1).
    Identity: spiffe://cluster/ns/<namespace>/job/<job-name>

[4] Bun uses the JWT as an OIDC token to obtain a short-lived
    signing certificate from the cluster's built-in Fulcio-compatible
    certificate authority (part of Sesame).

[5] Bun signs the image manifest digest with the ephemeral private key.

[6] The signature + certificate chain are attached to the manifest
    in Raft via AttachSignature command.

[7] The ephemeral private key is discarded. The certificate chain
    provides the verification path back to the cluster's OIDC issuer.
```

No separate signing key management is needed. The workload identity that Reliaburger already provides serves as the signing credential.

**External key signing:**

For images pushed from external CI systems:

```
[1] Developer signs the image with their own cosign key:
    cosign sign --key <private-key> mycluster:5000/myapp:v1.4.2

[2] Pickle receives the signature as an OCI artifact alongside the image.

[3] On schedule, Meat verifies the signature against public keys
    registered in cluster configuration:
    [images.trust_policy]
    keys = ["cosign-key:abc123..."]

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

Nodes can optionally pre-pull images that appear in cluster registry announcements, even before being scheduled to run them:

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

# Number of peer nodes to replicate pushed images to (synchronous).
# A successful push guarantees the image is held by (1 + redundancy) nodes.
# Default: 2
redundancy = 2

# Number of most recent tags per repository to retain during GC.
# Older tags are candidates for collection after gc_retain_days.
# Default: 10
gc_retain_tags = 10

# Number of days to retain unreferenced images before GC eligibility.
# Images referenced by running deployments are never collected regardless.
# Default: 30
gc_retain_days = 30

# Enable proactive pre-pulling of newly pushed images.
# When true, this node fetches new images as they appear in Raft,
# even before being scheduled to run them.
# Default: true
pre_pull = true

# Whether pushes wait for replication to complete before returning success.
# true  = synchronous push (default, guarantees durability)
# false = return success after local storage, replicate in background
# Default: true
push_sync = true

# External registries for pull-through caching.
# Pickle authenticates to these registries when pulling external images.
# password_secret references a cluster secret (Section 5.3) decrypted by Bun.
external_registries = [
  { host = "ghcr.io", username = "bot", password_secret = "ghcr-token" },
  { host = "docker.io", username = "myorg", password_secret = "dockerhub-token" },
]

# Require all images to be signed before Meat will schedule them.
# Unsigned images are accepted into Pickle but remain unschedulable.
# Default: false
require_signatures = false
```

**Corresponding Rust config struct:**

```rust
#[derive(Clone, Debug, Deserialize)]
pub struct PickleConfig {
    /// Maximum blob store size on this node.
    pub max_storage: ByteSize,

    /// Number of peers to replicate to on push.
    #[serde(default = "default_redundancy")]
    pub redundancy: u32,

    /// GC: retain this many recent tags per repository.
    #[serde(default = "default_gc_retain_tags")]
    pub gc_retain_tags: u32,

    /// GC: retain unreferenced images for this many days.
    #[serde(default = "default_gc_retain_days")]
    pub gc_retain_days: u32,

    /// Pre-pull newly pushed images proactively.
    #[serde(default = "default_true")]
    pub pre_pull: bool,

    /// Synchronous push (wait for replication before returning success).
    #[serde(default = "default_true")]
    pub push_sync: bool,

    /// External registries for pull-through caching.
    #[serde(default)]
    pub external_registries: Vec<ExternalRegistry>,

    /// Require image signatures for scheduling.
    #[serde(default)]
    pub require_signatures: bool,
}

fn default_redundancy() -> u32 { 2 }
fn default_gc_retain_tags() -> u32 { 10 }
fn default_gc_retain_days() -> u32 { 30 }
fn default_true() -> bool { true }
```

---

## 7. Failure Modes

### 7.1 Replication Failure During Push

**Scenario:** A push arrives at Node A, which stores the layers locally but can't reach enough peers to meet the redundancy target within the timeout (default 30s).

**Behaviour:** The push returns an error to the client. The image is stored locally on Node A but isn't committed to Raft (no manifest entry, no tag). The client must retry.

**Recovery:** Bun on Node A tracks the locally-stored-but-uncommitted layers. On the next successful replication attempt (either a client retry or Bun's background replication sweep), the layers are available locally and don't need to be re-uploaded. If no retry occurs within a configurable cleanup period (default 24h), the orphaned layers are removed.

**Mitigation for `push_sync = false`:** When async push is configured, the push returns success after local storage. Background replication failures are tracked and surfaced:

- `relish status` shows the under-replicated image count.
- Bun retries replication to new peers as nodes become available.
- An alert fires if an image remains under-replicated for more than 1 hour.

### 7.2 Under-Replicated Images

**Scenario:** A node holding one of the N replicas goes down permanently (e.g., hardware failure). The image's replica count drops below the desired redundancy.

**Behaviour:** The Raft state machine updates the peer location map when a node is removed from the cluster (detected via Mustard gossip failure detection). The image's `ReplicationState` transitions to `UnderReplicated`.

**Recovery:** Bun on the remaining holder nodes detects under-replicated images during its periodic replication audit (default every 5 minutes). It selects new peer targets and replicates the missing layers to restore the desired redundancy. The replication audit prioritizes images with the fewest remaining copies.

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
- **Keyless signing eliminates key management.** Build jobs sign automatically using their workload identity JWT. No signing keys to rotate, distribute, or protect.
- **External signatures use cosign-compatible verification.** Teams pushing from external CI register their public keys in the cluster configuration. Pickle verifies signatures against these keys using the standard cosign verification flow.
- **Signature verification is cached.** Once a manifest's signature is verified and recorded in Raft, subsequent scheduling decisions don't re-verify. The signature status is part of the `ManifestMetadata`.

### 8.3 Pull-Through Cache Credential Handling

Credentials for external registries are stored as cluster secrets (Section 5.3), encrypted at rest in Raft with the cluster's age key. Bun decrypts them at runtime when needed for pull-through cache authentication. Credentials are:

- **Never written to disk in plaintext.** They exist only in memory during the authentication flow.
- **Never exposed to workload containers.** The pull-through cache operates at the Bun agent level, not inside any container.
- **Scoped per registry.** Each external registry entry specifies credentials only for that host. A credential for `ghcr.io` can't be used to authenticate to `docker.io`.
- **Rotatable via cluster secret updates.** Updating the cluster secret referenced by `password_secret` takes effect on the next pull-through cache authentication attempt without restarting Bun.

### 8.4 Inter-Node Layer Transfer

All layer transfers between nodes occur over the cluster's mTLS connections (Sesame node certificates). No layer data travels in plaintext. Node identity is verified by the certificate common name, preventing a rogue node from intercepting layer transfers.

---

## 9. Performance

### 9.1 Push Latency

For the default configuration (`push_sync = true`, `redundancy = 2`):

| Image size | Expected push latency | Notes |
|-----------|----------------------|-------|
| 5 MB (small app layer) | < 1s | Dominated by Raft commit latency |
| 50 MB (typical app) | 2-3s | Layer transfer is the bottleneck |
| 200 MB (large app with deps) | 3-5s | Parallel replication to 2 peers |
| 1 GB+ (ML model, large base) | 10-30s | Bounded by network bandwidth |

Push latency is dominated by layer transfer time. Layers are streamed to peers in parallel (both peers receive data concurrently, not sequentially). The Raft commit for the manifest is small (< 10KB) and adds negligible latency (typically < 50ms).

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
Test: image replicates to N peers within 30s
  - Push image to Node A with redundancy=2.
  - Verify layers appear on exactly 2 additional nodes.
  - Verify Raft peer location map reflects all 3 holders.
  Expected: 2-8s

Test: replication failure returns error
  - Configure redundancy=2, take 2 of 3 nodes offline.
  - Push image to remaining node.
  - Verify push returns error (insufficient peers).
  - Bring nodes back online, retry push.
  - Verify push succeeds.
  Expected: 30s (timeout) + 2-5s (retry)

Test: under-replicated image auto-heals
  - Push image with redundancy=2 (holders: A, B, C).
  - Remove Node C from cluster.
  - Wait for replication audit (default 5 min, shortened in tests).
  - Verify a new node (D) now holds the layers.
  - Verify Raft peer location map updated.
  Expected: <30s with shortened audit interval

Test: push_sync=false returns immediately
  - Configure push_sync = false.
  - Push image, measure time (should be < 1s for small image).
  - Wait for background replication.
  - Verify replication completed asynchronously.
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
| `oci-spec` | OCI image and runtime spec types | Canonical Rust types for OCI image manifests, image configs, and index manifests. |
| `hyper` | HTTP server for the OCI Distribution API endpoint | The registry API is an HTTP endpoint on each node. `hyper` provides the low-level server. |
| `axum` or `warp` | HTTP framework layered on `hyper` | Routing for the `/v2/` API endpoints (blob uploads, manifest push/pull, tag listing). |
| `reqwest` | HTTP client for external registry pull-through | Used to pull manifests and blobs from upstream registries (Docker Hub, GHCR, ECR). |
| `tokio` | Async runtime | All Pickle I/O (disk, network, replication) is async. tokio provides the task scheduler, timers, and I/O primitives. |
| `ring` or `sha2` | SHA-256 hashing for content addressing | Every blob is verified by its SHA-256 digest. `ring` for performance-critical paths; `sha2` as a pure-Rust fallback. |
| `tonic` | gRPC framework for inter-node layer transfer | Layer replication and P2P downloads use gRPC streaming over mTLS. |
| `rustls` | TLS implementation for mTLS connections | Used by both `tonic` (gRPC) and `hyper` (HTTPS) for Sesame certificate-based authentication. |
| `sled` or `redb` | Embedded key-value store for local indices | Tag-to-digest mappings, blob reference counts, upload session state. Must be crash-safe (atomic writes). |
| `serde` + `serde_json` | Serialisation for manifests and Raft commands | OCI manifests are JSON. Raft commands are serialised for consensus. |
| `sigstore` | Sigstore/cosign signature creation and verification | Keyless signing via OIDC, certificate chain verification, signature format compatibility. |
| `flate2` or `zstd` | Compression for layer blobs | OCI layers are typically gzip-compressed tars. `zstd` may be used for inter-node transfer optimisation. |
| `tempfile` | Temporary file handling for upload sessions | Atomic file creation in the `tmp/` upload directory. |
| `tracing` | Structured logging and diagnostics | Push/pull/GC operations emit structured events for observability. |

### 12.2 OCI Spec Compliance

Pickle targets compliance with:

- **OCI Distribution Spec v1.1** -- the HTTP API for push, pull, and content discovery.
- **OCI Image Spec v1.1** -- the manifest and layer format for container images.
- **OCI Artifacts** -- for storing signatures alongside image manifests.

Compliance is verified by running the [OCI Distribution Spec conformance tests](https://github.com/opencontainers/distribution-spec/tree/main/conformance) against the Pickle endpoint as part of the integration test suite.

---

## 13. Open Questions

### 13.1 Large Image Handling

Very large images (multi-gigabyte ML models, monolithic base images) stress the synchronous push model. With `redundancy = 2`, a 5GB image push could take several minutes. Options under consideration:

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
