# Manifest publication bypasses repository isolation for globally stored blob references

Suggested priority: **P1**. Affects **v0.1.4**; verified at `f4757e7789d3672d21f15ca203031d6604d7e11a`.

Verification: Full authorization and publication source path.

### Problem / impact


Scoped reads correctly require the named repository to reference the requested blob. However, a scoped Deployer may publish a manifest into its own allowed repository whose descriptors reference any blobs present anywhere in that node's global blob store. Publication only checks digest and physical size, not whether this principal may use those bytes. After commit, the new manifest makes those blobs referenced by the caller's repository, so the scoped-read check now allows them.

This makes the repository read boundary ineffective for principals that may push. It affects private images, credentialed upstream cache blobs and scratch context blobs present on the receiving node, when their digests are known. Digest secrecy is not an access-control boundary. `HEAD` intentionally exposes existence and size for any global digest when routed through the caller's own repository.

### Verified evidence


- [src/pickle/api.rs:1482](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/pickle/api.rs#L1482): `manifest_put` authenticates scope against the destination repository only.
- [src/pickle/api.rs:1425](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/pickle/api.rs#L1425): `check_descriptor` receives only `BlobStore`, description and descriptor, and checks `has_blob` plus physical size. It receives no authenticated principal or source-repository ownership.
- [src/pickle/api.rs:1635](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/pickle/api.rs#L1635) and `:1647`: config and layer descriptors of an ordinary manifest use this global check; submanifests in indexes do likewise.
- [src/pickle/api.rs:312](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/pickle/api.rs#L312): publication rechecks existence and applies the new repository manifest; no source ownership check intervenes.
- [src/pickle/types.rs:516](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/pickle/types.rs#L516): `apply_manifest_commit` records every descriptor as a reference in the new repository.
- [src/pickle/api.rs:894](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/pickle/api.rs#L894): `authorise_repository_read` checks whether the requested digest is now in this repository's `referenced_digest_set`, so that newly granted reference satisfies access control.
- Existing `namespace_scoped_reader_pulls_only_its_namespace` ([src/pickle/api.rs:4283](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/pickle/api.rs#L4283)) tests a direct cross-repository GET, but not whether a publisher can introduce a reference it was not authorised to read.

### Proposed fix / acceptance


Track repository/principal-authorised blob association on completed uploads and require every config/layer/index descriptor used by a scoped publisher to be associated with the destination or readable by that principal. Shared physical CAS storage can remain, but physical presence alone must not establish authorisation. Preserve ordinary same-repository upload/publish and explicitly authorised cross-repository reuse.

Add a developer regression fixture with two namespaces and a layer physically shared in the node's blob store: a scoped publisher must be refused when its manifest references a blob accessible only to the other namespace, and a subsequent read must stay refused. Cover config blobs, image layers and OCI-index submanifest descriptors, while allowing references to its own successfully uploaded blobs and authorised shared content.

### Verification limit


No live demonstration was constructed. The flaw is established by the end-to-end source path. Outcome assumes the referenced bytes are present on the receiving storage node; replication does not need to be complete cluster-wide.

### Current implementation snippets

The excerpts below are verbatim from the audited checkout. Links are pinned to that commit.

[src/pickle/api.rs:1425–1446](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/pickle/api.rs#L1425-L1446)

```rust
fn check_descriptor(
    store: &BlobStore,
    what: &str,
    descriptor: &OciDescriptor,
) -> Result<LayerDescriptor, Box<Response>> {
    let digest = Digest::new(&descriptor.digest).map_err(|e| {
        Box::new(oci_error(
            StatusCode::BAD_REQUEST,
            "MANIFEST_INVALID",
            format!("{what} digest {:?} is invalid: {e}", descriptor.digest),
        ))
    })?;
    if !store.has_blob(&digest) {
        return Err(Box::new(oci_error(
            StatusCode::BAD_REQUEST,
            "MANIFEST_BLOB_UNKNOWN",
            format!("{what} blob {digest} is not present in the registry"),
        )));
    }
    let actual_size = store.blob_size(&digest).unwrap_or(0);
    if actual_size != descriptor.size {
        return Err(Box::new(oci_error(
```

[src/pickle/api.rs:1482–1497](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/pickle/api.rs#L1482-L1497)

```rust
async fn manifest_put(
    state: &PickleState,
    name: &str,
    reference: &str,
    headers: &HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let principal = match state
        .authorise_write(headers, name, RepositoryAccess::WriteManifest)
        .await
    {
        Ok(principal) => principal,
        Err(response) => return response,
    };
    if super::lease::is_test_repository(name)
        && principal
```

[src/pickle/api.rs:1629–1654](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/pickle/api.rs#L1629-L1654)

```rust
            return oci_error(
                StatusCode::BAD_REQUEST,
                "MANIFEST_INVALID",
                "manifest has no config descriptor".to_string(),
            );
        };
        let config = match check_descriptor(&state.store, "config", config) {
            Ok(layer) => layer,
            Err(response) => return *response,
        };
        let mut layers = Vec::new();
        for descriptor in &manifest_json.layers {
            match check_descriptor(&state.store, "layer", descriptor) {
                Ok(layer) => layers.push(layer),
                Err(response) => return *response,
            }
        }
        let total_size = config.size + layers.iter().map(|l| l.size).sum::<u64>();
        ImageManifest {
            digest: manifest_digest.clone(),
            config,
            layers,
            repository: name.to_string(),
            tags: std::collections::BTreeSet::new(),
            total_size,
            pushed_at: std::time::SystemTime::now(),
```

[src/pickle/api.rs:900–923](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/pickle/api.rs#L900-L923)

```rust
    let reader = state.authorise_read(headers).await?;
    scope_refusal(reader.as_ref(), route.repository(), RepositoryAccess::Read)?;
    let V2Route::Blob { name, digest } = route else {
        return Ok(());
    };
    if method != axum::http::Method::GET || !super::registry_auth::is_scoped(reader.as_ref()) {
        return Ok(());
    }
    let catalog = state
        .catalog_snapshot(name)
        .await
        .map_err(registry_write_error)?;
    if catalog.referenced_digest_set().contains(digest.as_str()) {
        Ok(())
    } else {
        Err(oci_error(
            StatusCode::NOT_FOUND,
            "BLOB_UNKNOWN",
            format!("repository {name} references no blob {digest}"),
        ))
    }
}

// ---------------------------------------------------------------------------
```
