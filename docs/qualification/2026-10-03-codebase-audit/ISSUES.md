# Reliaburger v0.1.4 — proposed issue descriptions

28 drafts for maintainer review: 27 concrete findings and one final cross-cutting hardening issue. No GitHub issues have been created. Each numbered block is a separate proposed issue; its first heading is the suggested title. Priorities are suggestions, not milestone assignments. Code excerpts are pinned to the audited implementation.

---

<!-- Draft 01: 01-volume-backing-image-collision.md -->

# Distinct managed volume paths can share a loop image and reformat each other’s data

Suggested priority: **P1**. Affects **v0.1.4**; verified at `f4757e7789d3672d21f15ca203031d6604d7e11a`.

Verification: Validation/path reproduced; Linux formatting path verified.

### Problem


On rootful Linux with ext4/xfs managed volumes, `/data.a` and `/data.b` are valid distinct volume mount paths, but `path.with_extension("img")` maps both to the same host `data.img`. Creating the second volume runs `fallocate` and `mkfs.ext4 -F` on the first volume's backing image. This can corrupt/reformat its data, then either mount the same filesystem again or fail after damage has already occurred.

Even `/data` and `/data.backup` collide. Test-owned volume provisioning already detects overlapping artifact paths, but ordinary production app provisioning does not run that check.

### Reproduction / verification


`Config::validate` accepts an app with these two managed volumes:

```toml
[app.db]
image = "busybox"
[[app.db.volumes]]
path = "/data.a"
size = "128Mi"
[[app.db.volumes]]
path = "/data.b"
size = "128Mi"
```

Executed the current-library pure probe: `colliding_volumes_validate=true`, `/data.a -> /data.img`, `/data.b -> /data.img`. The filesystem formatting call chain was checked, but **no destructive Linux mount/format repro was run** on this macOS host.

### Evidence


- [src/config/validate.rs:335–370](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/config/validate.rs#L335-L370): volume paths validated independently; no artifact collision rejection.
- [src/bun/agent/volumes.rs:104–110](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/bun/agent/volumes.rs#L104-L110): ordinary namespace provisions every volume independently.
- [src/grill/volume.rs:311](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/grill/volume.rs#L311), `383`: backing image path replaces the final extension.
- [src/grill/volume.rs:314–350](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/grill/volume.rs#L314-L350): allocation, forced format and mount act on that path.
- [src/grill/volume/owned.rs:59–77](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/grill/volume/owned.rs#L59-L77): existing overlap defense limited to test-owned storage.

### Fix / acceptance


Derive backing artifact names injectively (append `.img`, or encode the complete path), and validate all volume/artifact overlaps before any provisioning. Include ordinary namespaces, parent/child volumes, dotted names, and an image-artifact path used as a mountpoint. Linux integration test: write a sentinel to one sized volume, provision another with a formerly-colliding name, and verify independent filesystems/data plus correct reboot remounts. A durable format change may require a compatibility generation bump under the repository's pre-1.0 rules.

### Current implementation snippets

The excerpts below are verbatim from the audited checkout. Links are pinned to that commit.

[src/grill/volume.rs:305–324](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/grill/volume.rs#L305-L324)

```rust
    /// Creates a sparse file, formats it with ext4, and loop-mounts it.
    /// Writes beyond the quota fail with ENOSPC.
    #[cfg(target_os = "linux")]
    fn setup_loop_mount(&self, path: &Path, size_bytes: u64) -> Result<(), VolumeError> {
        use std::process::Command;

        let img_path = path.with_extension("img");

        // Create sparse file
        let status = Command::new("fallocate")
            .args(["-l", &size_bytes.to_string()])
            .arg(&img_path)
            .status()
            .map_err(|e| VolumeError::CreateFailed {
                path: path.display().to_string(),
                reason: format!("fallocate: {e}"),
            })?;
        if !status.success() {
            return Err(VolumeError::CreateFailed {
                path: path.display().to_string(),
```

[src/grill/volume.rs:329–343](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/grill/volume.rs#L329-L343)

```rust
        // Format with ext4
        let status = Command::new("mkfs.ext4")
            .args(["-F", "-q"])
            .arg(&img_path)
            .status()
            .map_err(|e| VolumeError::CreateFailed {
                path: path.display().to_string(),
                reason: format!("mkfs.ext4: {e}"),
            })?;
        if !status.success() {
            return Err(VolumeError::CreateFailed {
                path: path.display().to_string(),
                reason: "mkfs.ext4 failed".to_string(),
            });
        }
```

[src/bun/agent/volumes.rs:85–101](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/bun/agent/volumes.rs#L85-L101)

```rust
        let provisioning = async move {
            tokio::task::spawn_blocking(move || {
                if crate::testkit::lease::valid_test_namespace(&namespace) {
                    manager.prepare_test_storage(&namespace, &app, &spec)?;
                } else {
                    for volume in spec.volumes.iter().filter(|volume| volume.source.is_none()) {
                        manager.create_managed_volume(
                            &namespace,
                            &app,
                            &volume.path,
                            volume.size.as_deref(),
                        )?;
                    }
                }
                Ok::<(), crate::grill::volume::VolumeError>(())
            })
            .await
```

---

<!-- Draft 02: 02-build-signature-one-hour-expiry.md -->

# Build signer expiry breaks later builds and redeployment of cluster-signed images after one hour

Suggested priority: **P1**. Affects **v0.1.4**; verified at `f4757e7789d3672d21f15ca203031d6604d7e11a`.

Verification: Full source path; existing clock-injected expiry test.

### Problem / impact


The built-in code-signing identity uses the same one-hour certificate lifetime as runtime workload identities. The leader caches that identity forever without checking expiry or rotating it. Every image signature is then checked against certificate validity at current wall-clock time.

Within one hour after the namespace's first build signer is provisioned, images built under it stop passing `require_signatures` enforcement. Later builds on the same leader use the expired cached signer and fail the local signature self-check. Already built images cannot be newly deployed, rescheduled to a replacement worker, or restarted through a fresh deploy after expiry. Restarting the leader may mint a new signer for later builds but does not repair signatures already embedded in existing image metadata. This affects cluster-generated keyless signatures; externally signed images have no leaf certificate and are unaffected.

### Verified evidence


- [src/bun/build_runner.rs:937](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/bun/build_runner.rs#L937): `get_or_provision_build_signer` returns any existing cached identity without expiry inspection. No refresh/removal path exists for this cache.
- [src/bun/build_runner.rs:946](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/bun/build_runner.rs#L946): provisioning calls `CouncilNode::sign_workload_csr` with `CertUsage::CodeSigning`.
- [src/council/node.rs:525](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/council/node.rs#L525): all usage types go through `identity::validate_and_sign_csr`.
- [src/sesame/identity.rs:185](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/sesame/identity.rs#L185): `params.not_after = now + WORKLOAD_CERT_LIFETIME`; `WORKLOAD_CERT_LIFETIME` at line 21 is 3,600 seconds and the function does not distinguish signing lifetime.
- [src/bun/build_runner.rs:994](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/bun/build_runner.rs#L994): `sign_pushed_image` verifies locally against current trust before attaching the signature, so expired cache reuse produces a signing failure.
- [src/pickle/signing.rs:280](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/pickle/signing.rs#L280): keyless verification passes `SystemTime::now()` to `verify_keyless_at`.
- [src/pickle/signing.rs:318](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/pickle/signing.rs#L318): verification validates every certificate's lifetime at that current time.
- [src/meat/scheduler.rs:432](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/meat/scheduler.rs#L432): enforcement calls that verifier, and [src/bun/agent/launch.rs:292](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/bun/agent/launch.rs#L292) repeats it before node launch.
- [src/pickle/signing.rs:689](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/pickle/signing.rs#L689): existing `verify_keyless_rejects_an_expired_chain` explicitly asserts expiration makes the previously valid signature invalid. It uses ten years, masking the production leaf's actual one-hour lifetime.
- Existing cache test `get_or_provision_build_signer` reuse is immediate only, with no expiry scenario ([src/bun/build_runner.rs:1903](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/bun/build_runner.rs#L1903)).

### Proposed fix / acceptance


Give artifact signing an explicit lifecycle and durable verification semantics separate from runtime mTLS leaves. Renew the cached signing identity before it expires, and choose a supported strategy that keeps previously signed image digests deployable after leaf expiry while respecting revocation and rejecting signatures made without valid authority. Simply checking the existing unsigned `signed_at` value is not sufficient evidence of historical signing time; use an authenticated timestamp/attestation or another well-defined trust mechanism. Alternatively, a longer-lived dedicated signing credential plus documented maintenance policy could be an interim solution, with expiry handling still required.

Inject clock/expiry in regression tests: provision signer, advance just beyond its one-hour validity, request another build/signature, and require fresh valid signing authority; sign an image before expiry, verify/deploy it after the old leaf expires, and require the supported retained-artifact behavior. Revoked and cryptographically invalid signatures must still be refused. Cover cache reuse, leader restart, build tracking terminal state, and image redeploy/reschedule.

### Verification limit


No one-hour live cluster soak was run. Exact expiry follows directly from the issued leaf's hard-coded lifetime, cached reuse and the current-time verifier. The existing injected-clock expired-chain test demonstrates the relevant refusal semantics.

### Current implementation snippets

The excerpts below are verbatim from the audited checkout. Links are pinned to that commit.

[src/bun/build_runner.rs:930–939](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/bun/build_runner.rs#L930-L939)

```rust
    cache: &tokio::sync::Mutex<HashMap<String, BuildSigner>>,
    council: &crate::council::CouncilNode,
    trust_domain: &str,
    namespace: &str,
    node_name: &str,
) -> Result<BuildSigner, String> {
    let mut guard = cache.lock().await;
    if let Some(existing) = guard.get(namespace) {
        return Ok(existing.clone());
    }
```

[src/sesame/identity.rs:19–22](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/sesame/identity.rs#L19-L22)

```rust

/// Workload certificate lifetime: 1 hour.
pub const WORKLOAD_CERT_LIFETIME: Duration = Duration::from_secs(3600);

```

[src/sesame/identity.rs:182–186](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/sesame/identity.rs#L182-L186)

```rust
    // Exact validity window: a one-hour certificate is valid for one hour
    // (plus the skew backdate), not until midnight.
    params.not_before = time::OffsetDateTime::from(now - CLOCK_SKEW_BACKDATE);
    params.not_after = time::OffsetDateTime::from(now + WORKLOAD_CERT_LIFETIME);

```

[src/pickle/signing.rs:270–282](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/pickle/signing.rs#L270-L282)

```rust
    sig: &ImageSignature,
    digest: &Digest,
    root_ca_cert_der: &[u8],
    crl: Option<&crate::sesame::types::Crl>,
) -> Result<(), SigningError> {
    verify_keyless_at(
        sig,
        digest,
        root_ca_cert_der,
        crl,
        std::time::SystemTime::now(),
    )
}
```

[src/pickle/signing.rs:315–319](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/pickle/signing.rs#L315-L319)

```rust
    // Full chain: every adjacent signature + issuer binding, chaining to the
    // trust anchor, and every cert valid at `at`.
    crate::sesame::cert::validate_chain_at(chain, root_ca_cert_der, at)
        .map_err(|e| SigningError::ChainVerifyFailed(e.to_string()))?;

```

---

<!-- Draft 03: 03-batch-workload-authorization.md -->

# Batch submission bypasses token workload scope and Deploy/HostExec grants

Suggested priority: **P1**. Affects **v0.1.4**; verified at `f4757e7789d3672d21f15ca203031d6604d7e11a`.

Verification: Authenticated router dispatch reproduced.

### Problem


`POST /v1/batch` checks only the caller's Deployer role and then dispatches every supplied job directly to the agent. A token scoped to app `allowed` in namespace `allowedns` can submit a job named `forbidden` in `forbiddenns`. The normal `/v1/apply` route confines every app/job to token scope and checks `Deploy`, plus `HostExec` for scripts/binaries. Batch does none of these checks.

An unscoped Deployer denied `HostExec` by namespace policy can also use batch to run an allowlisted host script. The node's binary allowlist still applies; this defect bypasses the per-principal authorization layer, not the allowlist. Follower submission forwards using the cluster service token, so merely adding a leader-side check without preserving the original authorization would still grant the user system authority.

### Evidence


- [src/bun/batch.rs:700–711](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/bun/batch.rs#L700-L711): role check and early forwarding, before job inspection.
- [src/bun/batch.rs:731–778](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/bun/batch.rs#L731-L778): namespace resolution followed by allocation; no scoped/grant admission.
- [src/bun/batch.rs:499–508](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/bun/batch.rs#L499-L508): `AgentCommand::Deploy` bypasses the HTTP apply admission checks.
- [src/bun/batch.rs:1068–1075](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/bun/batch.rs#L1068-L1075): user request is forwarded with the service token.
- [src/bun/api/apply.rs:267–307](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/bun/api/apply.rs#L267-L307): corresponding scope, Deploy and HostExec enforcement in the ordinary route.

Executed router probe `evidence/batch.rs` with a genuine Argon2-authenticated Deployer token confined to `allowed`/`allowedns`. Submission of the forbidden job returned `202 Accepted`, `assigned:1`, and the fake command consumer observed `AgentCommand::Deploy` containing `forbidden`. This verifies HTTP authentication, handler acceptance and dispatch, without executing host commands.

### Expected behavior / fix direction


Apply the normal scope, namespace permission, host-execution and test-lease/image admission checks to every job **before** registering or dispatching any batch. Preserve the user's credential when forwarding submission, just as `cluster_apply` does. Test submission through both leader and follower, mixed allowed/disallowed jobs, and denied HostExec; refusals must create no tracker record or agent command.

Existing F05 (#363) concerns identity lifecycle and audiences, not this already-supported scoped credential bypass. Closed #298 concerns read-route authorization, not batch job execution.

### Current implementation snippets

The excerpts below are verbatim from the audited checkout. Links are pinned to that commit.

[src/bun/batch.rs:700–712](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/bun/batch.rs#L700-L712)

```rust
    // Submitting work is a Deployer action (AUTH2 — it used to take no auth).
    if let Err(resp) =
        crate::sesame::auth::authorize(auth.as_deref(), crate::sesame::types::ApiRole::Deployer)
    {
        return resp;
    }
    // Followers forward the raw body to the leader (the tracker and
    // the aggregated capacity view live there).
    if let Some(council) = &state.council
        && !council.is_leader().await
    {
        return forward_to_leader(&state, council, "/v1/batch", body).await;
    }
```

[src/bun/batch.rs:731–745](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/bun/batch.rs#L731-L745)

```rust
    // One namespace per job, resolved here and used everywhere (JOB3).
    let mut jobs = match resolve_job_namespaces(request.jobs) {
        Ok(jobs) => jobs,
        Err(e) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": e })),
            )
                .into_response();
        }
    };
    // Stable input order: together with the scheduler's ordered
    // profile groups this pins the assignment plan (the old
    // allocation-order finding).
    jobs.sort_by(|a, b| a.name.cmp(&b.name));
```

[src/bun/batch.rs:491–508](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/bun/batch.rs#L491-L508)

```rust
        Err(e) => {
            eprintln!("bun: batch config synthesis failed: {e}");
            for job in &jobs {
                reporter.report(batch_id, &job.name, false).await;
            }
            return;
        }
    };
    for job in &jobs {
        config.job.insert(job.name.clone(), job.spec.clone());
    }

    let (event_tx, mut event_rx) = mpsc::channel(64);
    if cmd_tx
        .send(AgentCommand::Deploy {
            config,
            events: event_tx,
        })
```

[src/bun/batch.rs:1063–1078](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/bun/batch.rs#L1063-L1078)

```rust
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({ "error": "no cluster leader known yet; retry shortly" })),
        )
            .into_response();
    };
    let mut request = state
        .cluster_http
        .client()
        .post(format!("{leader_url}{path}"))
        .header("content-type", "application/json")
        .body(body);
    if let Some(token) = &state.service_token {
        request = request.bearer_auth(token);
    }
    proxy_response(request.send().await).await
}
```

[src/bun/api/apply.rs:273–299](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/bun/api/apply.rs#L273-L299)

```rust
    for (app_name, namespace, host_execution) in targets {
        if let Err(resp) =
            crate::sesame::auth::authorize_scoped(auth.as_deref(), app_name, namespace)
        {
            return resp;
        }
        if let Err(resp) = crate::sesame::auth::authorize_permission(
            auth.as_deref(),
            crate::config::PermissionAction::Deploy,
            app_name,
            namespace,
            &permissions,
        ) {
            return resp;
        }
        if host_execution
            && let Err(resp) = crate::sesame::auth::authorize_permission(
                auth.as_deref(),
                crate::config::PermissionAction::HostExec,
                app_name,
                namespace,
                &permissions,
            )
        {
            return resp;
        }
    }
```

---

<!-- Draft 04: 04-manifest-descriptor-authorization.md -->

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

---

<!-- Draft 05: 05-log-flush-data-loss.md -->

# Failed Ketchup flush discards rows and advances replay checkpoints past lost data

Suggested priority: **P1**. Affects **v0.1.4**; verified at `f4757e7789d3672d21f15ca203031d6604d7e11a`.

Verification: Failed flush, replay refusal and restart loss reproduced.

### Problem

Ketchup clears its buffered rows before a flush's filesystem operations succeed. A temporary write failure permanently removes those rows from the live store. The in-memory capture offsets already cover the lost rows, so the capture reader cannot replay them. A subsequent successful flush persists a newer checkpoint, making the loss survive restart.

### Evidence

- [src/ketchup/log_store.rs:600–613](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/ketchup/log_store.rs#L600-L613): `take_flush_batch` builds a batch, clears the buffer at line 606 and increments the counter before IO.
- [src/ketchup/log_store.rs:385–398](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/ketchup/log_store.rs#L385-L398): the production `flush_shared` hands the batch to `write_log_pending` and returns an error without restoring it.
- [src/ketchup/log_store.rs:622–626](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/ketchup/log_store.rs#L622-L626): direct `flush` has the same failure.
- [src/ketchup/log_store.rs:503–511](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/ketchup/log_store.rs#L503-L511): ingest advances capture offsets and rejects records at or below the stored offset.
- [src/ketchup/log_store.rs:333–375](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/ketchup/log_store.rs#L333-L375): a successful subsequent flush persists its capture checkpoint.

### Reproduction

The executable public API reproduction is in `evidence/state.rs`.

1. Create a capture file containing `lost\nkept\n`.
2. Make the configured Parquet directory a regular file, forcing a transient `create_dir_all` failure.
3. Ingest `lost` with capture end offset 5 and flush.
4. Repair the Parquet directory and replay `lost` at offset 5.
5. Ingest `kept` at offset 10, flush successfully, reopen the store and query `SELECT line FROM logs`.

Actual verified output:

```text
failed flush result=Err(Io(Os { code: 17, kind: AlreadyExists, message: "File exists" })) buffered=0
replay lost row after failed flush accepted=false
reopened log checkpoint offset=Some(10)
rows after recovery=[Object {"line": String("kept")}]
```
Expected: both records remain retryable and are eventually persisted exactly once. No committed checkpoint may advance past a missing batch.

### Suggested fix / acceptance

Retain ownership of pending batches until successful persistence, or restore failed batches and corresponding offset state without breaking ordering with concurrent ingestion/flushes. Cover both shared and direct flush callers and cancellation. Add failures at directory creation, Parquet write and checkpoint publication followed by recovery; confirm no missing/duplicate rows and correct restart replay. This is distinct from #308, which added successful-flush replay checkpoints, and #510's physical disk pressure handling.

### Current implementation snippets

The excerpts below are verbatim from the audited checkout. Links are pinned to that commit.

[src/ketchup/log_store.rs:600–615](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/ketchup/log_store.rs#L600-L615)

```rust
    pub fn take_flush_batch(&mut self) -> Result<Option<LogPendingFlush>, KetchupError> {
        let Some(batch) = self.buffer_to_batch()? else {
            return Ok(None);
        };
        let filename = format!("logs_{:06}.parquet", self.flush_counter);
        let path = self.data_dir.join(filename);
        self.buffer.clear();
        self.flush_counter += 1;
        Ok(Some(LogPendingFlush {
            data_dir: self.data_dir.clone(),
            path,
            batch,
            checkpoint: self.ingested.clone(),
        }))
    }

```

[src/ketchup/log_store.rs:385–399](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/ketchup/log_store.rs#L385-L399)

```rust
pub async fn flush_shared(
    store: &std::sync::Arc<tokio::sync::RwLock<LogStore>>,
) -> Result<bool, KetchupError> {
    let pending = {
        let mut guard = store.write().await;
        guard.take_flush_batch()?
    };
    match pending {
        Some(p) => {
            write_log_pending(p).await?;
            Ok(true)
        }
        None => Ok(false),
    }
}
```

[src/ketchup/log_store.rs:503–512](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/ketchup/log_store.rs#L503-L512)

```rust
    fn ingest_at_nanos(&mut self, nanos: u64, record: &super::types::LogRecord) -> bool {
        if let Some(position) = &record.position {
            let seen = self.ingested.offsets.get(&position.file).copied();
            if seen.is_some_and(|offset| position.end_offset <= offset) {
                return false;
            }
            self.ingested
                .offsets
                .insert(position.file.clone(), position.end_offset);
        }
```

---

<!-- Draft 06: 06-metrics-object-key-collision.md -->

# Metrics writers sharing an object-store prefix overwrite each other’s Parquet chunks

Suggested priority: **P1**. Affects **v0.1.4**; verified at `f4757e7789d3672d21f15ca203031d6604d7e11a`.

Verification: Two production stores using file:// reproduced.

### Problem and impact


Two Buns configured with the same `[metrics] object_store_url` independently choose the same numeric Parquet object keys. Opening both before either flushes seeds both counters to zero. Their first successful flushes both PUT `metrics_000000.parquet`; the second silently replaces the first node’s durable metrics. Later chunks continue colliding when the counters remain aligned. This is a storage identity defect, independent of the planned metrics query/reporting architecture.

The production Bun passes the configured URL straight to `MayoStore::open`, without adding its node identity. The backend uses unconditional object-store PUT, so an operator’s natural shared-bucket configuration can lose data despite both writers reporting success. A restart-only numeric counter scan prevents a single sequential writer from overwriting itself, but does not coordinate independent live writers.

### Reproduction and actual result


The retained `evidence/mayo_collision.rs` opens two production `MayoStore`s against the same empty `file://` prefix, inserts one labelled CPU sample per node, then flushes A followed by B. Parent independently reran it:

```text
after node a flush [(100, "cpu", "{\"node\":\"a\"}", 11.0)]
after node b flush [(100, "cpu", "{\"node\":\"b\"}", 22.0)]
remote files=["metrics_000000.parquet"]
```

Expected: both samples remain durably queryable after either order of flush and after reopen. The file backend demonstrates the common object-key/PUT path; no S3/GCS service was contacted.

### Evidence and fix direction


[src/bin/bun.rs:945](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/bin/bun.rs#L945) passes the raw configured URL to the store. [src/mayo/store.rs:315](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/mayo/store.rs#L315) scans the prefix once on open; [src/mayo/store.rs:430](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/mayo/store.rs#L430) generates names from the process-local counter; [src/mayo/store.rs:244](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/mayo/store.rs#L244) overwrites that key with an unconditional PUT.

Use globally unique immutable chunk identities, or a durable node/writer namespace plus collision-safe writes. Define query ownership too: if every node reads the whole shared prefix, cluster fan-out must avoid counting the same durable samples once per node. An undocumented requirement for manually unique URLs is insufficient while shared prefixes are accepted without a warning or refusal.

### Acceptance criteria


- Open two stores on one prefix before either writes; concurrent/sequential flushes preserve both labelled samples and produce distinct immutable chunks.
- Reopen writers and repeat with counter histories that overlap; no prior chunk is replaced.
- Verify shared-prefix local and cluster queries do not duplicate samples; exercise file:// and an object-store test double.

### Existing issue comparison


#364/F06 covers PromQL, remote read, extra tiers, reporting/event production, chunking and live-metrics contracts. It does not describe object-key collisions and acknowledged durable data replacement in the existing object-store backend. No matching issue or discussion was found in the 112-issue inventory.


### Current implementation snippets

The excerpts below are verbatim from the audited checkout. Links are pinned to that commit.

[src/bin/bun.rs:944–953](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/bin/bun.rs#L944-L953)

```rust
    // it must exist before the runtime starts.
    let metrics_dir = storage_directory(&config.storage.metrics, "metrics").await?;
    // With `[metrics] object_store_url` set, metrics are persisted to and
    // queried from an object store (s3://, gs://, file://) so they survive node
    // loss (H8); otherwise Parquet stays in the local metrics dir.
    let mayo_store = Arc::new(RwLock::new(
        MayoStore::open(metrics_dir, Some(config.metrics.object_store_url.as_str()))
            .await
            .map_err(|e| anyhow::anyhow!("failed to open metrics store: {e}"))?,
    ));
```

[src/mayo/store.rs:315–326](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/mayo/store.rs#L315-L326)

```rust
        let Some(url) = object_store_url.filter(|u| !u.is_empty()) else {
            return Ok(Self::new(data_dir));
        };
        let (store, prefix) = parse_object_store(url)?;
        let flush_counter = next_remote_flush_counter(&store, &prefix).await?;
        Ok(Self {
            buffer: Vec::new(),
            data_dir,
            backend: Backend::Remote { store, prefix },
            flush_counter,
        })
    }
```

[src/mayo/store.rs:427–448](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/mayo/store.rs#L427-L448)

```rust
        let Some(batch) = self.buffer_to_batch()? else {
            return Ok(None);
        };
        let filename = format!("metrics_{:06}.parquet", self.flush_counter);
        self.buffer.clear();
        self.flush_counter += 1;
        let target = match &self.backend {
            Backend::Local => FlushTarget::Local {
                data_dir: self.data_dir.clone(),
                path: self.data_dir.join(&filename),
            },
            Backend::Remote { store, prefix, .. } => FlushTarget::Remote {
                store: Arc::clone(store),
                key: prefix.clone().join(filename.as_str()),
            },
        };
        Ok(Some(PendingFlush { batch, target }))
    }

    /// Build a DataFusion session exposing a `metrics` table over all data:
    /// the Parquet files unioned with the unflushed buffer.
    async fn session(&self) -> Result<SessionContext, MayoError> {
```

[src/mayo/store.rs:240–249](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/mayo/store.rs#L240-L249)

```rust
            let bytes = tokio::task::spawn_blocking(move || batch_to_parquet_bytes(&batch))
                .await
                .map_err(|e| MayoError::Io(std::io::Error::other(e.to_string())))??;
            store
                .put(&key, object_store::PutPayload::from(bytes))
                .await
                .map_err(|e| MayoError::ObjectStore(e.to_string()))?;
            Ok(())
        }
    }
```

---

<!-- Draft 07: 07-cluster-migration-gate.md -->

# Cluster apply commits dependent app revisions before run_before migrations complete

Suggested priority: **P1**. Affects **v0.1.4**; verified at `f4757e7789d3672d21f15ca203031d6604d7e11a`.

Verification: Cluster and agent production paths verified; no live council run.

### Problem


The supported `run_before = ["app.api"]` migration gate is lost whenever apply uses a council. `cluster_apply` first commits app specs into Raft, making them available to the scheduler and node reconcilers. It then sends a config containing only the jobs to the agent. `DeployWorker::run_deploy` implements prerequisite waiting only inside its app loop; with an empty app map that loop never executes. The job is instead launched as an ordinary independent job, and apply can report completion once the job has started.

An arbitrarily slow migration cannot delay the new app, and an exit-1 migration cannot abort/roll back the already-committed app. This can expose an app to an incompatible database schema. This contradicts the current whitepaper §11 statement at [docs/whitepaper.md:652](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/docs/whitepaper.md#L652): jobs can declare `run_before` to ensure migrations complete before app instances start.

### Reproduction


Apply the following to a running process-mode test council with `/bin/sh` allowlisted (or use container commands on a rootful runc council):

```toml
[job.migrate]
script = "sleep 15; exit 1"
run_before = ["app.api"]

[app.api]
script = "sleep 300"
```

Watch `api` start before the migration exits. The migration fails but `api` remains desired/running. A standalone apply takes the prerequisite path and fails before deploying `api`.

### Evidence


- [src/bun/api/apply.rs:583–617](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/bun/api/apply.rs#L583-L617): desired-state app writes commit first.
- [src/bun/api/apply.rs:619–637](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/bun/api/apply.rs#L619-L637): jobs-only `Config` sent afterward.
- [src/council/apply.rs:68–77](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/council/apply.rs#L68-L77): app specs become schedulable desired state; jobs excluded.
- [src/bun/agent/deploy_worker.rs:155–191](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/bun/agent/deploy_worker.rs#L155-L191): prerequisite gates exist only inside `for (app_name, spec) in &config.app`.
- [src/bun/agent/deploy_worker.rs:436–485](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/bun/agent/deploy_worker.rs#L436-L485): job-only path launches without waiting for successful exit.

Verification: exact call path and data transformation checked; a live council repro was not run by this subagent. The parent independently rechecked these production paths; the proposed live-council reproduction remains an acceptance test, not an executed result.

### Fix / acceptance


Keep dependent app revisions unschedulable until the prerequisite run has positively exited zero. Failure/timeout must leave the old app revision untouched and return an apply error. Add clustered leader/follower tests with a blocked migration and a failed migration; checking only standalone agent behavior misses this defect.

### Current implementation snippets

The excerpts below are verbatim from the audited checkout. Links are pinned to that commit.

[src/bun/api/apply.rs:583–590](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/bun/api/apply.rs#L583-L590)

```rust
            None => crate::council::config_to_desired_writes(&config),
        };
        for request in writes {
            let describe = describe_write(&request);
            match council.write(request).await {
                // A state-machine refusal (lease expired, in cleanup, resource
                // owned elsewhere, quota) is NOT a commit — surfacing it as an
                // error stops the apply instead of streaming "committed" and
```

[src/bun/api/apply.rs:619–637](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/bun/api/apply.rs#L619-L637)

```rust
        // Jobs are not cluster-scheduled yet; run them here, as before.
        if !config.job.is_empty() {
            let _ = event_tx
                .send(ApplyEvent::Progress {
                    message: format!(
                        "{} job(s) deploying on this node (jobs are not cluster-scheduled yet)",
                        config.job.len()
                    ),
                })
                .await;
            let job_config = Config {
                job: config.job.clone(),
                ..Config::default()
            };
            let _ = cmd_tx
                .send(AgentCommand::Deploy {
                    config: job_config,
                    events: event_tx.clone(),
                })
```

[src/bun/agent/deploy_worker.rs:155–175](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/bun/agent/deploy_worker.rs#L155-L175)

```rust

        for (app_name, spec) in &config.app {
            if self.report_cancellation(&events).await {
                return;
            }
            let namespace = spec.namespace.as_deref().unwrap_or("default");

            // run_before (E): jobs declaring `run_before = ["app.<name>"]` must
            // run to completion before this app's deploy begins — migrations are
            // the classic case. A prerequisite failure aborts the whole deploy.
            let target = format!("app.{app_name}");
            for (job_name, job_spec) in &config.job {
                // Cron-scheduled jobs fire on their schedule, never as a
                // deploy-time prerequisite.
                if ran_prereqs.contains(job_name)
                    || job_spec.schedule.is_some()
                    || !job_spec.run_before.contains(&target)
                {
                    continue;
                }
                let job_ns = job_spec.namespace.as_deref().unwrap_or("default");
```

[src/bun/agent/deploy_worker.rs:436–445](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/bun/agent/deploy_worker.rs#L436-L445)

```rust
            }
            // Already run to completion as a run_before prerequisite above, or a
            // cron-scheduled job that fires on its schedule rather than now.
            if ran_prereqs.contains(job_name) || spec.schedule.is_some() {
                continue;
            }
            let namespace = spec.namespace.as_deref().unwrap_or("default");
            if let Some(operation) = &self.operation {
                operation
                    .advance(
```

---

<!-- Draft 08: 08-batch-stale-run-completion.md -->

# A previous job’s terminal status can falsely complete a new batch before launch

Suggested priority: **P1**. Affects **v0.1.4**; verified at `f4757e7789d3672d21f15ca203031d6604d7e11a`.

Verification: Real router and delayed fake agent acknowledgement reproduced.

### Problem


The leader's batch pull watcher starts immediately at submission and matches runner status solely by `(name, namespace)`. It has no batch/run/attempt generation. A completed job with the same name remains visible while the new job is queued or clearing its prior artifacts. The watcher immediately marks the **new** batch completed based on the **old** successful run. That terminal state rejects a later failure from the actual new run.

Similar misattribution occurs when separate overlapping batches reuse a job name: even a batch whose deploy is refused because another run is active can race with another run's success report. The job ledger already stores explicit run generations, but they aren't carried into `InstanceStatus`/`BatchJobRecord`.

### Verified reproduction


Current HTTP-router harness held the new `AgentCommand::Deploy`'s completion event for two seconds while Status returned a prior `duplicate/default` job's `stopped,exit_code=0`. Submitted a new batch whose script would fail. Within 100 ms, before the new deploy completed, `/v1/batch/3` returned `completed:1,done:true,elapsed_secs:0`. This demonstrates the exact watcher race without running scripts. Probe `evidence/batch.rs`.

### Evidence / fix


- [src/bun/batch.rs:575–585](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/bun/batch.rs#L575-L585): watcher starts independently of run acknowledgement.
- [src/bun/batch.rs:627–642](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/bun/batch.rs#L627-L642): polls every nonterminal assignment immediately.
- [src/bun/batch.rs:461–468](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/bun/batch.rs#L461-L468): name/namespace only outcome match.
- [src/meat/batch_tracker.rs:87–99](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/meat/batch_tracker.rs#L87-L99): assignment record has no run generation.
- [src/bun/jobs.rs:67](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/bun/jobs.rs#L67): node-local ledger already has a generation field.

Bind every batch job to a unique acknowledged run identity, expose that identity in status and callbacks, and ignore evidence from another generation. Add tests reusing terminal names, delayed dispatch/launch, overlapping batches, runner restart and delayed old callbacks. Until identities are supported, reject conflicting/reused job identities safely.

### Current implementation snippets

The excerpts below are verbatim from the audited checkout. Links are pinned to that commit.

[src/bun/batch.rs:461–475](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/bun/batch.rs#L461-L475)

```rust
fn job_outcome(statuses: &[InstanceStatus], name: &str, namespace: &str) -> Option<bool> {
    for status in statuses
        .iter()
        .filter(|s| s.app_name == name && s.namespace == namespace)
    {
        let outcome = match (status.state.as_str(), status.exit_code) {
            ("failed", _) => Some(false),
            ("stopped", Some(0) | None) => Some(true),
            _ => None,
        };
        if outcome.is_some() {
            return outcome;
        }
    }
    None
```

[src/bun/batch.rs:627–646](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/bun/batch.rs#L627-L646)

```rust
        for job in record.jobs.iter().filter(|j| !j.status.is_terminal()) {
            let Some(node) = &job.node else { continue };
            let outcome = if node.0 == self_name {
                local_statuses
                    .as_deref()
                    .and_then(|statuses| job_outcome(statuses, &job.name, &job.namespace))
            } else {
                fetch_remote_outcome(state, node, &job.name, &job.namespace).await
            };
            if let Some(completed) = outcome {
                let status = if completed {
                    JobStatus::Completed
                } else {
                    JobStatus::Failed
                };
                let _ = report_batch_job(state, batch_id, &job.name, status).await;
            }
        }

        tokio::time::sleep(std::time::Duration::from_millis(PULL_INTERVAL_MS)).await;
```

[src/meat/batch_tracker.rs:87–100](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/meat/batch_tracker.rs#L87-L100)

```rust
    pub namespace: String,
    /// Node the job was assigned to; `None` for unschedulable jobs.
    pub node: Option<NodeId>,
    /// Current status.
    pub status: JobStatus,
}

/// One tracked batch: its jobs and when it was submitted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BatchRecord {
    /// Per-job records, in allocation order.
    pub jobs: Vec<BatchJobRecord>,
    /// Submission time as seconds since the Unix epoch. Wall-clock (not
    /// `Instant`) because the record crosses the Raft wire and must
```

---

<!-- Draft 09: 09-ingress-health-rebuild.md -->

# Ingress table rebuilds resurrect failed backends and continued probes never exclude them

Suggested priority: **P1**. Affects **v0.1.4**; verified at `f4757e7789d3672d21f15ca203031d6604d7e11a`.

Verification: Production probe loop and table rebuild reproduced.

### Problem

Routing-table rebuilds reset every backend's local health to healthy. The long-running probe tracker reports updates only when its health state changes. If a backend was already marked unhealthy, an unrelated service catalogue update makes it routable again; continued failed probes never remove it because the tracker is already unhealthy. It can receive traffic indefinitely until it first recovers and fails again.

### Evidence

- [src/wrapper/routing.rs:425–432](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/wrapper/routing.rs#L425-L432): rebuilt backend starts with `locally_healthy: true` at line 431.
- [src/wrapper/routing.rs:566–583](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/wrapper/routing.rs#L566-L583): `ProbeTracker::record` only returns a verdict on state transitions.
- [src/wrapper/routing.rs:645–650](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/wrapper/routing.rs#L645-L650): probe loop only applies returned verdicts.
- [src/bun/agent/consumer.rs:484–507](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/bun/agent/consumer.rs#L484-L507): consumer publication constructs and installs a fresh routing table; rebuild line 500 and replacement line 505.

### Reproduction

`evidence/state.rs` creates a real table, starts production `run_health_probes` against closed loopback port 9 with unhealthy threshold 1, waits for exclusion, rebuilds from the identical ServiceMap and waits for several additional failed sweeps.

Actual verified output:

```text
probe before rebuild: routable=0
probe after rebuild and more failures: routable=1
```
Expected: an unchanged failed backend remains excluded after rebuilding. A catalogue change for another application must not resurrect it.

### Suggested fix / acceptance

Carry the verdict for unchanged endpoint/execution identity through rebuilds, or continuously reapply current tracker state. Ensure a probe of an old address cannot mark a replacement backend unhealthy. Test failed backend + unrelated catalogue update, recovery, endpoint replacement and continued failures after multiple rebuilds. Distinct from #431's expiry of stale node endpoint reports: this is the ingress-local active probe state.

### Current implementation snippets

The excerpts below are verbatim from the audited checkout. Links are pinned to that commit.

[src/wrapper/routing.rs:425–434](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/wrapper/routing.rs#L425-L434)

```rust
        .map(|b| Backend {
            instance_id: b.instance_id.clone(),
            addr: SocketAddr::new(b.node_ip.into(), b.host_port),
            healthy: b.healthy,
            // Trust the service map on a fresh rebuild; the active probe loop
            // re-evaluates local reachability from here.
            locally_healthy: true,
            local: b.local,
        })
        .collect();
```

[src/wrapper/routing.rs:574–586](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/wrapper/routing.rs#L574-L586)

```rust
            counters.consecutive_unhealthy = counters.consecutive_unhealthy.saturating_add(1);
            counters.consecutive_healthy = 0;
            if counters.locally_healthy
                && counters.consecutive_unhealthy >= self.threshold_unhealthy
            {
                counters.locally_healthy = false;
                return Some(false);
            }
        }
        None
    }

    /// Drop tracker state for instances no longer present in `live`.
```

[src/bun/agent/consumer.rs:498–507](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/bun/agent/consumer.rs#L498-L507)

```rust
            .collect();
        let mut table = crate::wrapper::routing::RoutingTable::new();
        table.rebuild(&map, &routes).map_err(failure)?;
        for entry in services {
            self.publish_backend_kernel(&ServiceId::new(&entry.namespace, &entry.app_name), &map)
                .await?;
        }
        *self.routing_table.write().await = table;
        self.service_map_tx.send_replace(map);
        Ok(())
```

---

<!-- Draft 10: 10-ingress-backend-redirects.md -->

# Ingress follows backend redirects and loses the original status, Location and cookies

Suggested priority: **P1**. Affects **v0.1.4**; verified at `f4757e7789d3672d21f15ca203031d6604d7e11a`.

Verification: Real production proxy with loopback backend reproduced.

### Problem

Wrapper uses reqwest's default redirect policy. A backend's HTTP redirect is followed inside the proxy instead of being returned to the client. The original status, Location and Set-Cookie are lost. Login/OAuth and other redirect-based applications therefore behave incorrectly. Relative redirect requests also bypass a fresh ingress routing decision, while absolute redirects cause the bun to make an outbound request to the redirected origin.

### Evidence

- [src/wrapper/proxy.rs:190–200](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/wrapper/proxy.rs#L190-L200): both the normal and fresh-connection clients use `Client::builder()` without `redirect(Policy::none())`.
- [src/wrapper/proxy.rs:653](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/wrapper/proxy.rs#L653): sends through that client.
- [src/wrapper/proxy.rs:680](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/wrapper/proxy.rs#L680): forwards the final response status, after reqwest has followed any redirects.

### Reproduction

`evidence/network.rs` starts a real production `bind_proxy` and a loopback backend. The backend `/redirect` responds `302`, `Location: /landing`, `Set-Cookie: login=nonce; Path=/`; `/landing` responds `200` and `landing`. The external test client disables redirects.

Actual verified output:

```text
redirect: status=200 OK location=None set-cookie=None body=landing
```
Expected: the ingress client receives the backend's 302, Location and cookie, and the backend receives no `/landing` request until the external client follows it.

### Suggested fix / acceptance

Set `reqwest::redirect::Policy::none()` on both proxy clients. Verify 301/302/303/307/308, relative/absolute locations, cookie preservation and method/body preservation for POST 307/308. An absolute redirect target must not be contacted by the ingress proxy.

### Current implementation snippets

The excerpts below are verbatim from the audited checkout. Links are pinned to that commit.

[src/wrapper/proxy.rs:188–201](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/wrapper/proxy.rs#L188-L201)

```rust
    shutdown: CancellationToken,
) -> Result<BoundProxy, WrapperError> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .pool_max_idle_per_host(32)
        .pool_idle_timeout(UPSTREAM_POOL_IDLE_TIMEOUT)
        .build()
        .map_err(|e| WrapperError::ProxyFailed(format!("failed to build http client: {e}")))?;
    let fresh_client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .pool_max_idle_per_host(0)
        .build()
        .map_err(|e| WrapperError::ProxyFailed(format!("failed to build http client: {e}")))?;

```

[src/wrapper/proxy.rs:649–662](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/wrapper/proxy.rs#L649-L662)

```rust
    // connection failure*. A backend that answered (even with a 5xx) may have
    // side effects, so its request is never replayed against another instance;
    // a connection that never opened is always safe to retry (§ retry).
    for (idx, (_cand_id, cand_addr)) in candidates.iter().enumerate() {
        let upstream_uri = match build_upstream_uri(cand_addr, &parts.uri) {
            Some(u) => u,
            None => continue,
        };

        let (upstream_req, body_sent) = upstream.build(&state.client, &upstream_uri);
        let mut sent = tokio::select! {
            biased;
            _ = super::draining::wait_for_termination(&terminate) => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
            result = upstream_req.send() => result,
```

[src/wrapper/proxy.rs:678–690](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/wrapper/proxy.rs#L678-L690)

```rust
            };
        }
        match sent {
            Ok(resp) => {
                let status =
                    StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
                let mut response = Response::builder().status(status);

                // Copy end-to-end response headers only. Drop hop-by-hop headers
                // and the upstream framing headers (`Content-Length` /
                // `Transfer-Encoding`): the response body streams below, so hyper
                // re-frames it and copying the upstream length would mismatch.
                let conn_tokens = connection_tokens(resp.headers());
```

---

<!-- Draft 11: 11-consumer-backend-capacity.md -->

# One service with more than 32 cluster backends blocks publication of the whole consumer view

Suggested priority: **P1**. Affects **v0.1.4**; verified at `f4757e7789d3672d21f15ca203031d6604d7e11a`.

Verification: 33-endpoint map failure reproduced; production publication traced.

### Problem

The cluster catalogue can contain more than 32 backends for one service, but consumer publication validates the complete merged inventory with a strict per-service maximum of 32. A normally configured service scaled to 33 replicas (or a daemon running on >32 nodes) therefore makes a bun reject the entire new consumer view. New endpoints, DNS and ingress updates for unrelated services included in that view can also stop publishing.

### Evidence

- [src/onion/types.rs:16](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/onion/types.rs#L16): `MAX_BACKENDS = 32`.
- [src/onion/service_map.rs:286–355](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/onion/service_map.rs#L286-L355): remote merge appends the entire cluster endpoint set without checking this limit.
- [src/onion/service_map.rs:72–73](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/onion/service_map.rs#L72-L73): `from_snapshot` rejects a service with more than 32 backends.
- [src/bun/agent/consumer.rs:304–345](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/bun/agent/consumer.rs#L304-L345): builds the merged candidate and validates consumer ownership.
- [src/bun/agent/consumer.rs:489](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/bun/agent/consumer.rs#L489): reconstructs the merged map with `from_snapshot` during publication.
- [src/onion/ebpf/maps.rs:236–252](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/onion/ebpf/maps.rs#L236-L252): kernel converter separately limits the fixed-size backend array to 32, so simply dropping validation is insufficient.

### Reproduction / limits

`evidence/state.rs` uses production `EndpointCatalog::rebuild`, `ServiceMap::with_cluster_catalog` and `ServiceMap::from_snapshot`: 33 healthy endpoints for one service, each on a distinct node.

Actual verified output:

```text
33 cluster backends: merged=33 validation=Some(InvalidSnapshot { service: "default__crowded", reason: "backend capacity exceeded" })
```
This is a verified public map/validation reproduction plus the production consumer call path, not a live 33-node cluster run. Parent review found no cluster-wide replica admission cap in configuration/scheduling. The discovery design documents a 32-backend dataplane limit and says excess backends are dropped with a warning ([docs/design/discovery-onion.md:785](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/docs/design/discovery-onion.md#L785)); the implemented merged-view validation instead refuses the whole publication. The defect is the accepted configuration and failure scope, even if 32 remains the supported dataplane maximum.

### Suggested fix / acceptance

Ensure admitted service sizes remain publishable. Support larger cluster endpoint sets in the dataplane, or reject unsupported cluster replica counts and rollout surge sizes at admission with clear documentation; do not permit one oversized service to poison unrelated updates. Test 33 distributed endpoints, daemon deployment on >32 nodes, rolling-surge boundaries and publication of an unrelated service.

### Current implementation snippets

The excerpts below are verbatim from the audited checkout. Links are pinned to that commit.

[src/onion/service_map.rs:69–76](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/onion/service_map.rs#L69-L76)

```rust
            if map.entries.contains_key(&key) || map.allocated_vips.contains(&entry.vip) {
                return Err(invalid("duplicate service or virtual IP owner"));
            }
            if entry.backends.len() > MAX_BACKENDS {
                return Err(invalid("backend capacity exceeded"));
            }
            let mut backend_ids = HashSet::new();
            for backend in &entry.backends {
```

[src/onion/service_map.rs:335–350](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/onion/service_map.rs#L335-L350)

```rust
                        port: service.port,
                        backends: service
                            .backends
                            .iter()
                            .filter(|backend| Some(backend.node_id.as_str()) != local_node)
                            .map(|b| BackendInstance {
                                instance_id: catalog_instance_id(b),
                                node_ip: b.node_ip,
                                host_port: b.host_port,
                                healthy: b.healthy,
                                local: false,
                            })
                            .collect(),
                        firewall_allow_from: None,
                    };
                    merged.entries.insert(qualified.clone(), entry);
```

[src/bun/agent/consumer.rs:303–308](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/bun/agent/consumer.rs#L303-L308)

```rust
            .collect();
        let local = ServiceMap::from_snapshot(&local).map_err(failure)?;
        let merged =
            local.with_cluster_catalog_excluding_node(&catalog, Some(&owner.identity.node_id.0));
        let mut routes = self.ingress_configs.clone();
        let mut seen = std::collections::HashSet::new();
```

[src/bun/agent/consumer.rs:483–494](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/bun/agent/consumer.rs#L483-L494)

```rust
    /// service's entry in place.
    async fn install_consumer_view(
        &mut self,
        services: &[ServiceEntry],
        ingress: &[IngressAssignment],
    ) -> Result<(), BunError> {
        let map = ServiceMap::from_snapshot(services).map_err(failure)?;
        let routes = ingress
            .iter()
            .map(|route| {
                (
                    (route.namespace.clone(), route.name.clone()),
```

---

<!-- Draft 12: 12-registry-blocking-token-verification.md -->

# Registry token verification bypasses bounded Argon2 admission and blocks Tokio workers

Suggested priority: **P1**. Affects **v0.1.4**; verified at `f4757e7789d3672d21f15ca203031d6604d7e11a`.

Verification: Complete synchronous verification call path.

### Problem / impact


Pickle uses synchronous token hashing on Tokio runtime threads for both reads and writes. The API auth path already addresses exactly this resource exhaustion hazard with a cheap shape check, a process-wide four-permit semaphore and `spawn_blocking`. Registry authentication bypasses all three. Even ordinary authenticated OCI requests can block runtime workers; repeated invalid credentials cost one Argon2 verification per stored token and lack the API concurrency bound. Bun hosts scheduling, health, networking and registry on the same Tokio runtime, so registry traffic can interfere with the whole node.

### Verified evidence


- [src/pickle/registry_auth.rs:171](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/pickle/registry_auth.rs#L171): `authenticate_writer` calls synchronous `sesame::auth::authenticate(bearer, &tokens)` inside an async function.
- [src/pickle/registry_auth.rs:214](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/pickle/registry_auth.rs#L214): `authorise_read` does the same. This path is reachable by `GET /v2/` with no upload admission limit.
- [src/sesame/auth.rs:144](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/sesame/auth.rs#L144): `authenticate` calls `token::find_valid_token`; [src/sesame/token.rs:139](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/sesame/token.rs#L139) loops across stored tokens; [src/sesame/token.rs:106](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/sesame/token.rs#L106) performs Argon2 verification synchronously.
- [src/sesame/auth.rs:236](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/sesame/auth.rs#L236): `authenticate_off_lock` is the existing bounded asynchronous verifier. It checks shape, acquires `VERIFY_PERMITS` and uses `spawn_blocking`.
- [src/pickle/api.rs:551](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/pickle/api.rs#L551): four-write admission applies only POST/PATCH/PUT, not GET/HEAD; it is separate from shared token verification anyway.

### Proposed fix / acceptance


Route registry read and write verification through `authenticate_off_lock` with the cloned token snapshot. Keep constant-time internal service-token recognition and existing role/scope checks. Extend the existing shared-permit auth test approach to registry reads and writes: holding all process-wide verification permits must make registry verification wait while unrelated async work proceeds; malformed credentials must be rejected without hash work. Test both Bearer and TLS Basic envelopes and assert unchanged 401/403 decisions.

### Verification limit


No live CPU-exhaustion test was performed. Blocking/concurrency behavior follows directly from the synchronous call chain. Existing registry auth tests confirm the paths but do not assert bounded hashing or timer progress.

### Current implementation snippets

The excerpts below are verbatim from the audited checkout. Links are pinned to that commit.

[src/pickle/registry_auth.rs:167–175](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/pickle/registry_auth.rs#L167-L175)

```rust

    let Some(bearer) = bearer else {
        return Err(WriteDenied::Unauthenticated);
    };
    match crate::sesame::auth::authenticate(bearer, &tokens) {
        Ok(ctx) => {
            // A registry push is a deploy-class mutation.
            if crate::sesame::token::check_role(ctx.role, ApiRole::Deployer).is_ok() {
                Ok(Some(ctx))
```

[src/pickle/registry_auth.rs:208–215](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/pickle/registry_auth.rs#L208-L215)

```rust
    }

    let Some(bearer) = bearer else {
        return Err(WriteDenied::Unauthenticated);
    };
    let tokens = { auth.tokens.read().await.clone() };
    crate::sesame::auth::authenticate(bearer, &tokens).map_err(|_| WriteDenied::Unauthenticated)
}
```

[src/sesame/auth.rs:255–266](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/sesame/auth.rs#L255-L266)

```rust
                "authentication temporarily unavailable".to_string(),
            ));
        }
    };

    let candidate = plaintext.to_string();
    let result = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        authenticate(&candidate, &tokens)
    })
    .await;

```

---

<!-- Draft 13: 13-registry-physical-storage-quota.md -->

# images.max_storage does not bound bare blobs or temporary uploads on disk

Suggested priority: **P1**. Affects **v0.1.4**; verified at `f4757e7789d3672d21f15ca203031d6604d7e11a`.

Verification: Quota accounting and all normal upload paths verified.

### Problem / impact


The configured registry storage ceiling is checked against the sum of published manifest logical sizes. Completed blob uploads without manifests never increase that sum, and active chunked upload bytes are not charged before writing to disk. Consequently a series of individually below-limit uploads can exceed `images.max_storage` by any amount. Normal `relish build` context uploads use the blob-only `_buildcontext` repository, so even legitimate repeated builds create uncharged storage until periodic GC. Chunked requests can also grow temporary files above the total ceiling and are only checked when completed.

Published-manifest accounting has the inverse mismatch: duplicated physical content in different repositories counts more than once, while the configured cap is per-node physical storage. Concurrent admission has no reservation, so more than one accepted upload/publication can observe the same remaining budget.

### Verified evidence


- [src/bin/bun.rs:2677](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/bin/bun.rs#L2677): production `QuotaConfig.total_bytes` is wired from `[images] max_storage`.
- [src/pickle/api.rs:179](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/pickle/api.rs#L179): quota usage queries the catalogue, not storage and outstanding writers.
- [src/pickle/types.rs:456](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/pickle/types.rs#L456): `stored_sizes` iterates only `self.manifests` and sums `manifest.total_size`.
- [src/pickle/api.rs:1283](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/pickle/api.rs#L1283): completed upload checks quota once against incoming size then `complete_upload_guarded` installs the blob; it does not publish manifest metadata or increase usage.
- [src/pickle/api.rs:1175](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/pickle/api.rs#L1175): `stream_upload` appends bytes to disk with only a 512 MiB request ceiling, not a session aggregate ceiling or quota reservation.
- [src/pickle/build.rs:24](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/pickle/build.rs#L24): `_buildcontext` is expressly a blob-only scratch repository.
- [src/bin/bun.rs:2941](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/bin/bun.rs#L2941): normal GC interval is at least one hour, so cleanup is not an admission bound.
- [src/pickle/p2p.rs:661](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/pickle/p2p.rs#L661)–`:729`: pull-through cache fills write upstream manifest/config/layers and publish through `record_commit` without any quota check; peer download path at [src/pickle/pull.rs:153](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/pickle/pull.rs#L153) likewise commits bytes without quota admission.
- Existing `push_over_repository_quota_is_refused` and `chunked_upload_over_repository_quota_is_refused` exercise one single assembled blob larger than the limit, not cumulative accepted storage.

### Proposed fix / acceptance


Enforce max_storage against per-node physical CAS and temporary upload usage, reserving capacity atomically before accepting/writing bytes; release reservations only when replacement/deduplication or confirmed cleanup justifies it. Keep logical repository accounting as a distinct policy if desired. Define whether downloaded/replicated images share the same cap and apply the policy consistently.

Ordinary regression cases: sequential unique blob-only uploads whose cumulative bytes exceed a small configured total must refuse further bytes; simultaneous uploads must not oversubscribe remaining capacity; PATCH growth must be limited before the temporary file crosses the cap; same-digest retries must not double-charge physical storage; restart must reconstruct accounting from durable files and pending uploads. Check build contexts as well as manifests.

### Verification limit


Parent verified the production quota and upload paths. No disk-filling workload or live overload was run. Existing quota tests cover a single oversized assembled blob and pass; cumulative storage and reservations are not covered by those tests.

### Current implementation snippets

The excerpts below are verbatim from the audited checkout. Links are pinned to that commit.

[src/pickle/types.rs:455–466](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/pickle/types.rs#L455-L466)

```rust
    /// Logical stored image sizes used by repository and aggregate quota admission.
    pub fn stored_sizes(&self, repository: &str) -> (u64, u64) {
        let mut repository_bytes = 0u64;
        let mut total_bytes = 0u64;
        for (_, manifest) in &self.manifests {
            total_bytes = total_bytes.saturating_add(manifest.total_size);
            if manifest.repository == repository {
                repository_bytes = repository_bytes.saturating_add(manifest.total_size);
            }
        }
        (repository_bytes, total_bytes)
    }
```

[src/pickle/api.rs:1280–1304](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/pickle/api.rs#L1280-L1304)

```rust

    // Enforce the storage quota against the fully-assembled blob before it is
    // committed (M10). Every upload path only knows its total size here — the on-disk
    // temp file is authoritative. Over quota: drop the temp and the session so
    // nothing lands in the blob store.
    match state.store.upload_size(upload_id).await {
        Ok(incoming) => {
            if let Err(response) = state.enforce_quota(name, incoming).await {
                discard_upload(state, upload_id).await;
                return response;
            }
        }
        Err(super::types::PickleError::InvalidUploadId(_)) => {
            return StatusCode::BAD_REQUEST.into_response();
        }
        Err(_) => return StatusCode::NOT_FOUND.into_response(),
    }

    state.sessions.retire(upload_id).await;
    let result = state
        .store
        .complete_upload_guarded(upload_id, &digest, Some(writer), access.guard.clone())
        .await;
    if result.is_ok() {
        state.sessions.complete(upload_id).await;
```

[src/pickle/api.rs:1180–1194](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/pickle/api.rs#L1180-L1194)

```rust
    let write = async {
        let mut stream = body.into_data_stream();
        let mut received = 0usize;
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|_| StatusCode::BAD_REQUEST.into_response())?;
            received = received.saturating_add(chunk.len());
            if received > MAX_REQUEST_BYTES {
                return Err(StatusCode::PAYLOAD_TOO_LARGE.into_response());
            }
            state
                .store
                .write_upload_chunk(upload_id, &chunk)
                .await
                .map_err(|_| StatusCode::BAD_REQUEST.into_response())?;
        }
```

[src/pickle/build.rs:20–30](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/pickle/build.rs#L20-L30)

```rust
/// entry-bomb archive cannot exhaust inodes or directory entries.
pub const MAX_CONTEXT_ENTRIES: usize = 65_536;

/// The scratch repository `relish build` uploads its context tarball to.
/// It only ever holds bare blobs: nothing tags or publishes a manifest here.
pub const BUILD_CONTEXT_REPOSITORY: &str = "_buildcontext";

/// Default Pickle registry port.
///
/// X1 regression note: this used to be 9117 — the *Bun API* port,
/// which has no `/v2` routes — so context uploads always 404ed.
```

---

<!-- Draft 14: 14-cron-step-overflow.md -->

# Large cron steps panic in debug and schedule every minute in release

Suggested priority: **P2**. Affects **v0.1.4**; verified at `f4757e7789d3672d21f15ca203031d6604d7e11a`.

Verification: Current library/debug and exact-source/release reproduced.

### Problem


`CronSchedule::parse` parses a positive step as `u8` and increments a `u8` without checking overflow. `schedule = "59/255 * * * *"` is accepted syntax and should match minute 59 once per hour. Debug builds panic. Release builds wrap and parse all minutes 0–59, so an expensive or destructive hourly task runs every minute.

The cron expression is parsed inside `begin_deploy -> register_scheduled_jobs`, on the agent command loop. A debug-build apply can panic the long-lived agent task. Registered schedules are parsed again at startup, so persisted bad schedules can also affect restart.

### Verified reproduction


- Exact current-library call, caught with `catch_unwind`: `CronSchedule::parse("59/255 * * * *")` panics at [src/meat/cron.rs:180](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/meat/cron.rs#L180), `attempt to add with overflow`.
- Compiled exact source with `-C opt-level=3 -C overflow-checks=off`: it matched all 60 minutes. Probe `evidence/cron_release.rs`.

### Evidence / fix


- [src/meat/cron.rs:153](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/meat/cron.rs#L153): step `u8`.
- [src/meat/cron.rs:177–180](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/meat/cron.rs#L177-L180): unchecked increment.
- [src/bun/agent/job_runs.rs:322](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/bun/agent/job_runs.rs#L322): parsing on deploy loop.
- [src/bun/agent/records.rs:414](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/bun/agent/records.rs#L414): parsing persisted schedule on recovery.

Use wider/checking arithmetic and stop once the next step is beyond the bounded field. Validate schedules before admitting apply. Test maximal positive steps, boundary starts, lists/ranges, both build profiles, and prove the exact intended firing times.

### Current implementation snippets

The excerpts below are verbatim from the audited checkout. Links are pinned to that commit.

[src/meat/cron.rs:150–160](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/meat/cron.rs#L150-L160)

```rust
    for term in token.split(',') {
        let (base, step) = match term.split_once('/') {
            Some((base, step)) => {
                let step: u8 = step.parse().map_err(|_| malformed())?;
                if step == 0 {
                    return Err(malformed());
                }
                (base, step)
            }
            None => (term, 1),
        };
```

[src/meat/cron.rs:173–184](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/meat/cron.rs#L173-L184)

```rust

        if start > end {
            return Err(malformed());
        }
        let mut v = start;
        while v <= end {
            values.insert(normalise(field, v));
            v += step;
        }
    }

    Ok(CronField { any: false, values })
```

[src/bun/agent/job_runs.rs:313–328](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/bun/agent/job_runs.rs#L313-L328)

```rust
            let namespace = spec
                .namespace
                .clone()
                .unwrap_or_else(|| "default".to_string());
            let key = (name.clone(), namespace.clone());
            let Some(expression) = spec.schedule.as_deref() else {
                next.remove(&key);
                continue;
            };
            let schedule = crate::meat::cron::CronSchedule::parse(expression)
                .map_err(|error| BunError::ScheduleState(error.to_string()))?;
            let last_fired_minute = next
                .get(&key)
                .and_then(|existing| existing.last_fired_minute);
            next.insert(
                key,
```

---

<!-- Draft 15: 15-batch-duplicate-identities.md -->

# Duplicate batch job names silently drop work and leave completion tracking stuck

Suggested priority: **P2**. Affects **v0.1.4**; verified at `f4757e7789d3672d21f15ca203031d6604d7e11a`.

Verification: Real router dispatch and tracker state reproduced.

### Problem


Batch jobs are described as unique by name, but submit never checks uniqueness. Two entries called `duplicate` return `assigned:2`, yet per-node config synthesis is a `BTreeMap` keyed by the bare name, so only one job is deployed. Assignment-to-spec matching also selects the first matching name. Completion tracking finds only the first record; every later report is a duplicate/conflict on that first record. The second record stays pending permanently, including after the watch deadline, because timeout reports again update the first record.

Same bare names in different namespaces are also broken: the tracker/report wire uses only `job_name` and config synthesis/dispatch matching omit namespace. Either reject duplicate bare names clearly, or carry namespace-qualified identities throughout.

### Verified reproduction


Current router probe submitted two same-name job entries with different scripts. Response: `202`, `assigned:2`. Fake agent received `jobs ["duplicate"]` (one map entry). Supplied a terminal status for that name; `/v1/batch/2` returned `total:2,pending:1,completed:1,done:false`. `BatchRecord::report` always finds the first matching name, so retries/deadline cannot settle the second.

### Evidence / fix


- [src/bun/batch.rs:724–745](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/bun/batch.rs#L724-L745): no uniqueness admission.
- [src/bun/batch.rs:784–788](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/bun/batch.rs#L784-L788), `820-825`: `.find` by name.
- [src/bun/batch.rs:499–500](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/bun/batch.rs#L499-L500): silent map replacement.
- [src/meat/batch_tracker.rs:127–130](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/meat/batch_tracker.rs#L127-L130): reports only first same-name job.
- [src/bun/batch.rs:605–607](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/bun/batch.rs#L605-L607): timeout cannot repair duplicates.

Reject ambiguous duplicates before registering a batch, or add stable per-submission identities to assignment, dispatch and reports. Test duplicates in one namespace and same names across namespaces; never report more assigned work than can execute and every admitted record must reach a terminal state.

### Current implementation snippets

The excerpts below are verbatim from the audited checkout. Links are pinned to that commit.

[src/bun/batch.rs:493–505](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/bun/batch.rs#L493-L505)

```rust
            for job in &jobs {
                reporter.report(batch_id, &job.name, false).await;
            }
            return;
        }
    };
    for job in &jobs {
        config.job.insert(job.name.clone(), job.spec.clone());
    }

    let (event_tx, mut event_rx) = mpsc::channel(64);
    if cmd_tx
        .send(AgentCommand::Deploy {
```

[src/bun/batch.rs:820–830](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/bun/batch.rs#L820-L830)

```rust
    for (job_name, node_id) in &allocation.assignments {
        if let Some(submission) = jobs.iter().find(|j| &j.name == job_name) {
            by_node
                .entry(node_id.clone())
                .or_default()
                .push(submission.clone());
        }
    }

    let callback_base_url = self_callback_url(&state, &self_name).await;
    for (node_id, node_jobs) in by_node {
```

[src/meat/batch_tracker.rs:121–137](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/meat/batch_tracker.rs#L121-L137)

```rust
        if !matches!(
            status,
            JobStatus::Running | JobStatus::Completed | JobStatus::Failed
        ) {
            return Err(ReportError::NotReportable { status });
        }
        let job = self
            .jobs
            .iter_mut()
            .find(|j| j.name == job_name)
            .ok_or_else(|| ReportError::UnknownJob {
                job: job_name.to_string(),
            })?;
        if job.status == status {
            return Ok(ReportOutcome::Duplicate);
        }
        let legal = matches!(
```

---

<!-- Draft 16: 16-batch-capacity-admission.md -->

# Cluster batches use stale or unlimited fallback capacity and do not retain admission reservations

Suggested priority: **P2**. Affects **v0.1.4**; verified at `f4757e7789d3672d21f15ca203031d6604d7e11a`.

Verification: Capacity translation, fallback and durable tracker source path.

### Problem


The batch capacity path bypasses the cluster scheduler's fail-closed placement policy:

1. `capacities_from_reports` never filters `aggregated.stale_nodes`; expired resource commitments still look current.
2. If every report is missing/pre-capacity, even in a cluster it substitutes a local capacity of `u64::MAX / 2` CPU and memory and assigns everything to the leader.
3. Allocations reserve only a request-local `Vec<NodeCapacity>` which is dropped after submit. Durable batch records do not include resource requests/specs. Another batch submitted before reports reflect the first sees the same free capacity and can double-book the node. Cluster app pending-placement reservation fix #432 does not apply to batch's independent dispatch path.

This can dispatch resource-limited jobs to an already-overcommitted node, and startup/leader transition is exactly when capacity evidence is least trustworthy.

### Evidence / reproduction


- [src/bun/batch.rs:152–182](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/bun/batch.rs#L152-L182): reports mapped without freshness filtering.
- [src/bun/batch.rs:185–195](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/bun/batch.rs#L185-L195), `752-764`: unlimited fallback in clustered branch.
- [src/bun/batch.rs:766–778](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/bun/batch.rs#L766-L778): reserves only local capacity vector.
- [src/meat/batch_tracker.rs:87–109](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/meat/batch_tracker.rs#L87-L109): durable assignment does not retain resource footprint.

Repro: on a one-node cluster with no fresh reports, submit more CPU/memory than the configured node owns; it reports assigned instead of waiting/refusing. Or submit two batches each consuming the full same fresh capacity before the first runner report updates; both are accepted/assigned. Verification here is source call-path; no live overload repro run. The parent independently rechecked the capacity path, unlimited fallback and tracker schema; the live overload case remains unexecuted.

### Fix / acceptance


Require fresh capacity in cluster mode, fail retryably when unavailable, and retain admitted resource commitments atomically until they are represented in runtime reports or terminal cleanup. Add missing/stale-capacity and concurrent-submission tests; verify totals never exceed allocatable resources. Keep the standalone fallback policy explicit.

### Current implementation snippets

The excerpts below are verbatim from the audited checkout. Links are pinned to that commit.

[src/bun/batch.rs:158–170](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/bun/batch.rs#L158-L170)

```rust
        let Some(report) = aggregated.reports.get(&member.node_id) else {
            continue;
        };
        let usage = &report.resource_usage;
        if usage.cpu_total_millicores == 0 {
            continue; // pre-capacity node
        }
        capacities.push(NodeCapacity {
            node_id: member.node_id.clone(),
            address: member.address,
            total: Resources::new(
                u64::from(usage.cpu_total_millicores),
                u64::from(usage.memory_total_mb) * 1024 * 1024,
```

[src/bun/batch.rs:185–197](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/bun/batch.rs#L185-L197)

```rust
/// Standalone fallback: one self node with effectively unlimited
/// capacity, so single-node clusters (and tests) schedule locally.
pub fn local_only_capacity(node_name: &str) -> Vec<NodeCapacity> {
    vec![NodeCapacity {
        node_id: NodeId(node_name.to_string()),
        address: std::net::SocketAddr::from(([127, 0, 0, 1], 0)),
        total: Resources::new(u64::MAX / 2, u64::MAX / 2, 0),
        reserved: Resources::new(0, 0, 0),
        allocated: Resources::new(0, 0, 0),
        labels: Default::default(),
    }]
}

```

[src/bun/batch.rs:751–764](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/bun/batch.rs#L751-L764)

```rust

    // Capacity: the aggregated reports when clustered, self otherwise.
    let mut capacities = match (&state.aggregated_rx, &state.membership) {
        (Some(aggregated_rx), Some(membership)) => {
            let members = membership.read().await.clone();
            let capacities = capacities_from_reports(&members, &aggregated_rx.borrow());
            if capacities.is_empty() {
                local_only_capacity(&self_name)
            } else {
                capacities
            }
        }
        _ => local_only_capacity(&self_name),
    };
```

[src/bun/batch.rs:766–778](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/bun/batch.rs#L766-L778)

```rust
    let batch_jobs: Vec<BatchJob> = jobs
        .iter()
        .map(|job| BatchJob {
            name: job.name.clone(),
            resources: Resources::new(
                job.spec.cpu.as_ref().map(|r| r.request).unwrap_or(0),
                job.spec.memory.as_ref().map(|r| r.request).unwrap_or(0),
                0,
            ),
        })
        .collect();

    let allocation = schedule_batch(&batch_jobs, &mut capacities);
```

---

<!-- Draft 17: 17-ingress-stream-timeout.md -->

# Ingress aborts healthy SSE and download streams after 30 seconds

Suggested priority: **P2**. Affects **v0.1.4**; verified at `f4757e7789d3672d21f15ca203031d6604d7e11a`.

Verification: Active 40-second backend stream aborted at 30 seconds.

### Problem

Both Wrapper reqwest clients impose a 30-second total request timeout, which also applies while consuming the response body. A healthy SSE feed or long download is aborted at 30 seconds even if it continues producing data. The streamed-body path explicitly claims support for SSE and large downloads.

### Evidence

- [src/wrapper/proxy.rs:191](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/wrapper/proxy.rs#L191) and `:197`: `.timeout(Duration::from_secs(30))` on both clients.
- [src/wrapper/proxy.rs:702–717](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/wrapper/proxy.rs#L702-L717): SSE/large-download comment and the response-body streaming path.

### Reproduction

`evidence/network.rs` uses real `bind_proxy` with a backend sending one valid `text/event-stream` chunk per second for 40 seconds. The test client reads until EOF/error.

Actual verified output:

```text
stream: error after 30.002430958s, chunks=30, error=error decoding response body
```
Expected: all 40 chunks are delivered and an actively streaming response can remain connected beyond 30 seconds.

### Suggested fix / acceptance

Separate connection/header deadlines from an active body's lifetime; apply any desired stream idle deadline to inactivity, while respecting drain/cancellation. Cover both normal and fresh clients, a stream active beyond 30 seconds, stalled upstreams and bounded cancellation. This is distinct from #369's deferred WebSocket close-handshake/ACME work.

### Current implementation snippets

The excerpts below are verbatim from the audited checkout. Links are pinned to that commit.

[src/wrapper/proxy.rs:188–201](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/wrapper/proxy.rs#L188-L201)

```rust
    shutdown: CancellationToken,
) -> Result<BoundProxy, WrapperError> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .pool_max_idle_per_host(32)
        .pool_idle_timeout(UPSTREAM_POOL_IDLE_TIMEOUT)
        .build()
        .map_err(|e| WrapperError::ProxyFailed(format!("failed to build http client: {e}")))?;
    let fresh_client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .pool_max_idle_per_host(0)
        .build()
        .map_err(|e| WrapperError::ProxyFailed(format!("failed to build http client: {e}")))?;

```

[src/wrapper/proxy.rs:700–717](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/wrapper/proxy.rs#L700-L717)

```rust
                }

                // Stream the backend's body instead of buffering it whole, so
                // SSE, gRPC and large downloads flow with backpressure and don't
                // pin the whole response in memory (ING3).
                //
                // The response owns its permit; a bounded upstream pump owns
                // the drain guard and observes cancellation even when the
                // client stops polling its body (ING2/DEP5/§5.5). Only this
                // backend's drain may hold or cancel the stream (T1.7).
                let mut drain_guard = drain_guard;
                if let Some(guard) = &mut drain_guard {
                    guard.keep_only(idx);
                }
                let terminate: Vec<_> = terminate.into_iter().nth(idx).into_iter().collect();
                let stream =
                    guarded_body_stream(resp.bytes_stream(), permit, drain_guard, terminate);
                return response
```

---

<!-- Draft 18: 18-external-dns-tcp.md -->

# External DNS queries fail over TCP, including retries after truncated UDP answers

Suggested priority: **P2**. Affects **v0.1.4**; verified at `f4757e7789d3672d21f15ca203031d6604d7e11a`.

Verification: Real UDP/TCP responder and mock upstream reproduced.

### Problem

The node DNS listener accepts TCP but only resolves internal names over that transport. Every external name receives SERVFAIL. The UDP external resolver can return an upstream response with the truncated flag intact; a normal resolver's TCP retry then fails. External responses requiring TCP (including large/DNSSEC responses) cannot resolve through the configured container resolver.

### Evidence

- [src/onion/dns.rs:516–523](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/onion/dns.rs#L516-L523): TCP listener comments explicitly limit it to internal answers.
- [src/onion/dns.rs:592](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/onion/dns.rs#L592): an external query in `answer_tcp_query` returns SERVFAIL.
- [src/onion/dns.rs:712–756](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/onion/dns.rs#L712-L756): UDP external upstream relay returns the received packet, including TC.
- Whitepaper line 562 promises that non-internal names forward to the host's configured resolvers.

### Reproduction

`evidence/network.rs` starts `BoundDnsResponder` on real loopback UDP/TCP sockets, with a mock upstream returning a valid external `example.com` response with NOERROR and TC. It queries UDP, then performs the ordinary framed TCP retry against the same node resolver.

Actual verified output:

```text
external UDP: bytes=29 rcode=0 tc=true
external TCP retry: rcode=2
```
Expected: TCP external queries relay to an upstream resolver and return the complete answer.

### Suggested fix / acceptance

Implement bounded upstream TCP DNS with correct length framing and reply transaction/question validation; retain namespace ACL behavior and avoid leaking internal names upstream. Cover truncated UDP followed by successful TCP retry, direct external TCP queries, timeout/cancellation and internal-name resolution over both transports.

### Current implementation snippets

The excerpts below are verbatim from the audited checkout. Links are pinned to that commit.

[src/onion/dns.rs:579–596](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/onion/dns.rs#L579-L596)

```rust
    let response = if !config.source_acl.allows(peer.ip()) {
        build_status_response(&query, RCODE_REFUSED)
    } else {
        match name.strip_suffix(".internal") {
            Some(stripped) => answer_internal(
                &config,
                &service_map,
                &dns_faults,
                &query,
                stripped,
                qtype,
                peer.ip(),
            ),
            None => build_status_response(&query, RCODE_SERVFAIL),
        }
    };
    let mut framed = Vec::with_capacity(response.len() + 2);
    framed.extend_from_slice(&(response.len() as u16).to_be_bytes());
```

[src/onion/dns.rs:729–748](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/onion/dns.rs#L729-L748)

```rust
            .await
            .ok()?
            .ok()?;
        if n >= 2 && reply_buf[..2] == query[..2] {
            let mut reply = reply_buf[..n].to_vec();
            // A reply that fills our whole buffer was probably cut off
            // mid-packet; set TC so the client retries over TCP.
            if n == UPSTREAM_BUFFER && reply.len() > 2 {
                reply[2] |= 0x02;
            }
            return Some(reply);
        }
    }
}

/// Parse the query name and QTYPE from a DNS packet.
///
/// Returns the name as a lowercase dotted string, or `None` if the
/// packet is malformed.
/// Decode the complete packet before admitting its one Internet-class question.
```

---

<!-- Draft 19: 19-compile-namespace-collision.md -->

# Directory compile silently drops same-named workloads from distinct namespaces

Suggested priority: **P1**. Affects **v0.1.4**; verified at `f4757e7789d3672d21f15ca203031d6604d7e11a`.

Verification: Two-file directory compilation reproduced.

### Problem and impact


`relish compile` keys merged apps and jobs by bare name. Two legitimate namespace-qualified workloads are inserted under the same key, so the later definition replaces the earlier one. The collision warning is explicitly conditional on the namespaces being equal; cross-namespace loss produces no warning. Operators can compile and apply an apparently complete production/staging tree while one workload is absent.

### Reproduction


Create these files, then run `relish compile config/`:

```toml
# config/prod/web.toml
[app.web]
image = "prod:v1"
```

```toml
# config/staging/web.toml
[app.web]
image = "staging:v1"
```

The current-library test compiled two files successfully with zero warnings, but returned one app, `staging/web`. The app map length was 1; the same `insert` implementation affects jobs. The manual explicitly derives each subdirectory’s namespace ([docs/manual/01_deploy-an-app.md:162](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/docs/manual/01_deploy-an-app.md#L162)).

Expected: preserve both distinct workload identities. If the serialized single-config representation cannot express both yet, compilation must fail clearly instead of silently claiming success.

### Evidence and fix direction


[src/relish/compile.rs:247](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/relish/compile.rs#L247) checks for a same-namespace collision, then unconditionally inserts by bare name. Both the app and job loops have this shape. `Config` itself uses bare-name maps, so only changing the warning cannot deliver namespace-preserving output.

Carry namespace-qualified resource identity through directory compilation, serialization and apply. Ensure the fix also covers jobs and builds with namespace ownership rather than introducing a second silent collision elsewhere. If a format change is needed, follow the repository’s pre-1.0 compatibility policy.

### Acceptance criteria


- A prod/staging tree containing two `web` apps emits and applies both, retaining images and namespace identities.
- Cover same-named jobs in distinct namespaces and explicit namespace overrides.
- Ambiguous duplicate definitions within one namespace follow an explicit deterministic policy with visible diagnostics.
- Round-trip the resolved output through parsing and apply; checking only the compiler’s intermediate map is insufficient.

### Existing issue comparison


No matching issue was found. #398 fixed instance ordinals across nodes, not directory compilation’s loss of namespace-qualified desired resources.

### Current implementation snippets

The excerpts below are verbatim from the audited checkout. Links are pinned to that commit.

[src/relish/compile.rs:247–280](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/relish/compile.rs#L247-L280)

```rust

    for (name, spec) in source.app {
        let namespace = spec.namespace.clone();
        if let Some(existing) = target.app.get(&name)
            && existing.namespace == namespace
        {
            collisions.push(format!(
                "duplicate app {:?} in namespace {:?}: the later definition wins",
                name,
                namespace.as_deref().unwrap_or("default")
            ));
        }
        target.app.insert(name, spec);
    }

    for (name, spec) in source.job {
        let namespace = spec.namespace.clone();
        if let Some(existing) = target.job.get(&name)
            && existing.namespace == namespace
        {
            collisions.push(format!(
                "duplicate job {:?} in namespace {:?}: the later definition wins",
                name,
                namespace.as_deref().unwrap_or("default")
            ));
        }
        target.job.insert(name, spec);
    }

    target.namespace.extend(source.namespace);
    target.permission.extend(source.permission);
    target.build.extend(source.build);
    collisions
}
```

[src/relish/compile.rs:219–231](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/relish/compile.rs#L219-L231)

```rust
}

/// Apply a namespace to all apps and jobs in a config that don't
/// already have one set.
fn apply_namespace(config: &mut Config, namespace: &str) {
    for app in config.app.values_mut() {
        if app.namespace.is_none() {
            app.namespace = Some(namespace.to_string());
        }
    }
    for job in config.job.values_mut() {
        if job.namespace.is_none() {
            job.namespace = Some(namespace.to_string());
```

---

<!-- Draft 20: 20-compile-ignored-defaults.md -->

# _defaults.toml silently ignores shared environment, memory and deployment settings

Suggested priority: **P2**. Affects **v0.1.4**; verified at `f4757e7789d3672d21f15ca203031d6604d7e11a`.

Verification: Documented defaults fixture compiled; fields absent.

### Problem and unsupported claim


The whitepaper promises `_defaults.toml` values for common environment variables, memory limits and deployment strategy inherited by apps unless overridden ([docs/whitepaper.md:1050](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/docs/whitepaper.md#L1050)). The manual says defaults fill fields an app leaves unset ([docs/manual/01_deploy-an-app.md:163](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/docs/manual/01_deploy-an-app.md#L163)). The compiler parses the defaults as an arbitrary TOML map but implements only the `image` key. Other valid-looking keys are silently discarded.

This can remove expected memory budgets or required environment settings from all apps in a directory while compilation exits successfully and prints no warning.

### Reproduction


```toml
# _defaults.toml
image = "web:v1"
memory = "256Mi"
[env]
MODE = "prod"
[deploy]
max_unavailable = 0
```

```toml
# web.toml
[app.web]
replicas = 2
```

The current compiler returned `image = "web:v1"`, no memory setting, an empty environment and default deployment settings, with no warnings. The retained test directly asserts image/memory/environment behavior; the deployment omission is also established by the only-field implementation below.

### Fix direction and acceptance


Implement typed defaults for the advertised supported fields and define inheritance/override semantics for scalar fields and nested env/deploy tables. Reject unsupported/misspelled defaults keys instead of accepting and ignoring them. Alternatively, explicitly refuse unsupported defaults until implementation and narrow the advertised contract.

- Image, memory, CPU, env and deployment defaults survive resolved output where supported.
- Explicit per-app values win; test partial nested tables, per-directory inheritance and malformed values.
- Unsupported keys fail with the defaults file and key named.
- Round-trip compiled output and verify resource enforcement receives the intended memory/deployment settings.

### Existing issue comparison


No matching implementation issue was found. #300’s five documentation corrections do not cover shared-defaults omission.

### Current implementation snippets

The excerpts below are verbatim from the audited checkout. Links are pinned to that commit.

[src/relish/compile.rs:182–192](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/relish/compile.rs#L182-L192)

```rust
    match toml::from_str(&content) {
        Ok(parsed) => (Some(parsed), None),
        Err(e) => (
            None,
            Some(format!(
                "{}: invalid TOML, defaults not applied: {e}",
                defaults_path.display()
            )),
        ),
    }
}
```

[src/relish/compile.rs:194–209](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/relish/compile.rs#L194-L209)

```rust
/// Apply defaults to a config. For each app, if a field from defaults
/// is missing, inject it. Currently supports the `image` default.
fn apply_defaults(config: &mut Config, defaults: &BTreeMap<String, toml::Value>) {
    let default_image = defaults
        .get("image")
        .and_then(|v| v.as_str())
        .map(String::from);

    for app in config.app.values_mut() {
        if app.image.is_none()
            && let Some(ref img) = default_image
        {
            app.image = Some(img.clone());
        }
    }
}
```

---

<!-- Draft 21: 21-gitops-directory-semantics.md -->

# GitOps rejects _defaults.toml and ignores directory-derived namespaces

Suggested priority: **P2**. Affects **v0.1.4**; verified at `f4757e7789d3672d21f15ca203031d6604d7e11a`.

Verification: Production execute_sync on local Git repository reproduced.

### Problem and unsupported claim


Lettuce’s directory loader differs from `relish compile`. It parses every TOML file as a complete `Config`, including `_defaults.toml`, and never performs defaults inheritance or derives namespaces from directory names. A tree documented for ordinary deployment therefore fails GitOps sync when it contains defaults, or targets `default` instead of the intended directory namespace when it does not.

The whitepaper says Lettuce works with directory trees natively immediately after describing shared defaults ([docs/whitepaper.md:1050](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/docs/whitepaper.md#L1050)). This is a concrete deployment incompatibility, not a request for a new configuration format.

### Reproduction and verified result


Commit a local test repository with `_defaults.toml` containing `image = "web:v1"` and `web.toml` containing `[app.web]` and `replicas = 2`. Execute the production `execute_sync` with empty current state. It returns `SyncResult::Failure`; the `_defaults.toml` file error names unknown field `image`. Ordinary `relish compile` accepts this tree and supplies the image.

For the namespace path, place `[app.web] image = "web:v1"` in `prod/web.toml`, with no explicit namespace. The source path leaves namespace unset and the diff resolves it to `default`; ordinary directory compilation derives `prod`. This second case was source-verified rather than separately executed.

### Evidence and fix direction


[src/lettuce/sync.rs:257](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/lettuce/sync.rs#L257) loops over all files with `Config::parse`; it only merges resource maps. [src/lettuce/diff.rs:212](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/lettuce/diff.rs#L212) resolves unset namespace to `default`. The compiler’s defaults and namespace operations live separately in [src/relish/compile.rs:90](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/relish/compile.rs#L90).

Share a deterministic directory compilation contract between CLI and GitOps, operating on the checked-out commit’s in-memory contents without bypassing signed-commit checks. Treat parsing/inheritance errors as sync refusal before desired-state mutation. Correct existing wrongly targeted resources through explicit reviewable namespace changes.

### Acceptance criteria


- The same tree yields equivalent namespace-qualified resolved specs in manual and GitOps paths, including defaults and overrides.
- `_defaults.toml` is treated as defaults, not as an invalid app config.
- Distinct namespaces containing same-named resources remain distinct or are explicitly refused until representable.
- Signed-script policy still inspects the effective configuration; invalid defaults never cause partial application.

### Existing issue comparison


No matching issue was found. #305 repairs unchanged-commit drift; it does not reconcile these two loaders’ semantics.

### Current implementation snippets

The excerpts below are verbatim from the audited checkout. Links are pinned to that commit.

[src/lettuce/sync.rs:257–281](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/lettuce/sync.rs#L257-L281)

```rust
    for (path, content) in ordered {
        let file_config = match Config::parse(content) {
            Ok(config) => config,
            Err(e) => {
                errors.insert(path.clone(), e.to_string());
                continue;
            }
        };

        // A resource named in two files is ambiguous: report it against
        // this later-sorted file and let the earlier definition stand,
        // rather than silently letting hash order pick a winner.
        if let Some(duplicate) = first_duplicate(&merged, &file_config) {
            errors.insert(
                path.clone(),
                format!("duplicate resource {duplicate} already declared in an earlier file"),
            );
            continue;
        }

        merged.app.extend(file_config.app);
        merged.job.extend(file_config.job);
        merged.namespace.extend(file_config.namespace);
        merged.permission.extend(file_config.permission);
        merged.build.extend(file_config.build);
```

[src/lettuce/diff.rs:207–215](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/lettuce/diff.rs#L207-L215)

```rust
/// The `AppId` a git-declared app resolves to.
///
/// Mirrors `config_to_desired_writes`: the app's own `namespace` field,
/// defaulting to `default`. Keeping the two derivations identical is what
/// makes GitOps and manual apply converge on the same identity.
fn app_id_for(name: &str, spec: &AppSpec) -> AppId {
    let namespace = spec.namespace.clone().unwrap_or_else(|| "default".into());
    AppId::new(name, namespace)
}
```

[src/relish/compile.rs:90–105](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/relish/compile.rs#L90-L105)

```rust
                // Apply defaults: merge default fields into apps/jobs
                // that don't have them set
                if let Some(defaults_toml) = defaults {
                    apply_defaults(&mut file_config, defaults_toml);
                }

                // Derive namespace from subdirectory name relative to root
                let namespace = derive_namespace(dir, entry_path);
                if let Some(ref ns) = namespace {
                    apply_namespace(&mut file_config, ns);
                }

                for collision in merge_into(&mut merged, file_config) {
                    warnings.push(format!("{}: {collision}", entry_path.display()));
                }
                merged_from.push(entry_path.clone());
```

---

<!-- Draft 22: 22-gitops-silently-ignored-jobs.md -->

# GitOps reports successful sync while dropping jobs and migration prerequisites

Suggested priority: **P1**. Affects **v0.1.4**; verified at `f4757e7789d3672d21f15ca203031d6604d7e11a`.

Verification: Production execute_sync with app and migration job reproduced.

### Problem and impact


Lettuce accepts job declarations but omits them entirely from its change set and never dispatches them. An app and its `run_before` migration in the same signed commit can be reported successfully applied while only the app is committed. Cron jobs are likewise never registered by this path.

The diff comment says jobs use a one-shot deploy path, but the production sync runner only calls `apply_changes` on the diff. It has no subsequent job dispatch. Avoiding repeated reconciliation of one-shot jobs is reasonable; silently accepting and discarding the declarations is not a complete one-shot execution policy.

### Reproduction


Commit this config to a local test Git repository:

```toml
[app.web]
image = "web:v1"
[job.migrate]
image = "migrate:v1"
run_before = ["app.web"]
```

Production `execute_sync` returns `Success`, one added resource and exactly one change, for the app. No job change exists for the runner to apply. The parent’s executable test confirms that result; the runner’s lack of any additional dispatch was source-verified.

Expected: the prerequisite positively completes before the app revision becomes schedulable, or the entire unsupported configuration is refused with a clear job-specific error. A successful GitOps sync must not imply that ignored declarations ran.

### Fix direction and acceptance


Define a durable commit/run identity for one-shot jobs so retries and leader failover do not rerun a completed migration accidentally. Register supported scheduled jobs through the appropriate durable execution path. While that is unavailable, reject GitOps job declarations, especially `run_before`, before writing dependent apps.

- A commit with an app and blocked/failed migration cannot publish the new app early.
- Job-only and scheduled-job commits either perform the promised work or fail explicitly.
- Repeated polls of the same commit and leader changes respect the chosen once-per-revision policy.
- Sync summaries/history name job outcomes; unsupported work never produces full success.

### Existing issue comparison


No matching issue was found. #305 concerns drift repair. This is separate from the manual cluster-apply migration gate defect: Lettuce drops jobs before it ever reaches agent deployment, so fixing cluster_apply alone does not repair GitOps.

### Current implementation snippets

The excerpts below are verbatim from the audited checkout. Links are pinned to that commit.

[src/lettuce/diff.rs:186–204](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/lettuce/diff.rs#L186-L204)

```rust
    // Jobs are deliberately absent from the diff. A job runs to
    // completion; it isn't reconciled desired state, so there's no job
    // map in Raft to compare against (see `config_to_desired_writes`).
    // The old code compared every git job against an always-empty set,
    // so it emitted an `Add` for every job on *every* sync — a change the
    // applier then silently dropped (`ChangePayload::Generic` maps to no
    // write) while inflating `summary.added`. That's the GIT2b bug: a job
    // "removed" from git was never in the desired state to begin with, so
    // it can't be re-added, and a job present in git is dispatched by the
    // one-shot deploy path, not by reconciliation. Emitting nothing here
    // keeps the summary honest and the applier free of no-op changes.

    let summary = DiffSummary {
        added,
        modified,
        removed,
    };

    (changes, summary)
```

[src/lettuce/runner.rs:178–191](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/lettuce/runner.rs#L178-L191)

```rust
            // silently skipped the way a `None` used to drop jobs and
            // namespaces on the floor.
            //
            // Atomicity (D12): `last_applied_commit` advances only if
            // EVERY write in the sync succeeds. The old code advanced the
            // commit regardless of per-change failures, so a failed write
            // was marked "applied" and never retried — the resource just
            // vanished until the next unrelated commit. Now a failure
            // leaves the commit unadvanced, and the next tick re-applies
            // the whole set. Writes are idempotent (spec upsert / delete),
            // so re-applying an already-committed change is a harmless
            // no-op.
            let applied = match apply_changes(&council, &outcome.changes).await {
                Ok(applied) => applied,
```

[src/lettuce/sync.rs:277–284](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/lettuce/sync.rs#L277-L284)

```rust
        merged.app.extend(file_config.app);
        merged.job.extend(file_config.job);
        merged.namespace.extend(file_config.namespace);
        merged.permission.extend(file_config.permission);
        merged.build.extend(file_config.build);
    }

    (merged, errors)
```

---

<!-- Draft 23: 23-dry-run-incomplete-diff.md -->

# apply --dry-run calls materially changed workloads unchanged when the image is unchanged

Suggested priority: **P2**. Affects **v0.1.4**; verified at `f4757e7789d3672d21f15ca203031d6604d7e11a`.

Verification: Plan generated with changed replicas, port and env reproduced.

### Problem and impact


The live dry-run preview compares only each app/job’s image string. It reports unchanged for updates to replicas, resources, environment, port, health, command/script or deployment settings when the image stays the same. Namespace and permission resources are also considered unchanged merely because their names exist. This hides operational and authorization changes from an operator using the preview to review an apply.

The input/output contract cannot compute a full diff: `CurrentResource` carries only a bare `resource` key and `image`. The `/v1/apps` cluster aggregation additionally collapses same-named apps from different namespaces under `app.<name>`.

### Reproduction and verified result


Generate a production plan with current resource `{resource: "app.web", image: "web:v1"}` and this desired config:

```toml
[app.web]
image = "web:v1"
replicas = 9
port = 9999
[app.web.env]
MODE = "changed"
```

The executable test observes `PlanAction::Unchanged` and `to_update = 0`. The same-image comparator is identical for jobs. The bare-name status merge is source-verified.

### Fix direction and acceptance


Fetch namespace-qualified desired specs or stable canonical fingerprints and compare every field that apply may change. Use desired-state evidence rather than treating running image identity as the whole configuration. If complete comparison is unavailable, explicitly label it unknown/partial rather than unchanged. Retain the documented offline preview behavior while making unavailable comparison visible.

- Preview detects replica/resource/env/port/health/command/deployment changes without an image change.
- Preview detects permission grants and namespace quota changes.
- Same-named apps in different namespaces do not overwrite each other’s evidence.
- Unchanged is reserved for equivalent effective desired specs, and rendered totals agree with the actions.

### Existing issue comparison


No matching issue was found in the inventory. This concerns the existing `apply --dry-run` preview, not a deferred separate plan command.

### Current implementation snippets

The excerpts below are verbatim from the audited checkout. Links are pinned to that commit.

[src/relish/plan.rs:130–141](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/relish/plan.rs#L130-L141)

```rust
                    PlanAction::Update
                } else {
                    PlanAction::Unchanged
                }
            }
        };

        entries.push(PlanEntry {
            resource: resource_key,
            action,
            summary,
        });
```

[src/relish/client.rs:1093–1104](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/relish/client.rs#L1093-L1104)

```rust
        Ok(rows
            .into_iter()
            .map(|row| crate::relish::plan::CurrentResource {
                resource: row.resource,
                image: row.image,
            })
            .collect())
    }

    /// Trigger an immediate log export on the agent (`POST /v1/logs/export`).
    ///
    /// The destination is resolved agent-side — a path on the agent host,
```

[src/bun/api/status.rs:29–41](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/bun/api/status.rs#L29-L41)

```rust
    }

    if let Some(council) = &state.council {
        let desired = council.desired_state().await;
        for (app_id, spec) in &desired.apps {
            resources.insert(format!("app.{}", app_id.name), spec.image.clone());
        }
        for name in desired.namespaces.keys() {
            resources.insert(format!("namespace.{name}"), None);
        }
        for name in desired.permissions.keys() {
            resources.insert(format!("permission.{name}"), None);
        }
```

---

<!-- Draft 24: 24-cli-existing-namespace-validation.md -->

# CLI rejects permission and build manifests that refer to an already-created cluster namespace

Suggested priority: **P2**. Affects **v0.1.4**; verified at `f4757e7789d3672d21f15ca203031d6604d7e11a`.

Verification: Same config refused locally and accepted by validate_against reproduced.

### Problem and impact


The server supports a permission/build config targeting a namespace created by an earlier apply, but `relish apply` validates the manifest locally before contacting the server. Its bare `Config::validate` sees only namespace declarations in that one manifest and rejects the existing cluster namespace as unknown. This prevents the normal split-file workflow from reaching the server’s correct cluster-aware validator.

### Reproduction


First create namespace `prod` in the cluster. Then apply this separate file:

```toml
[permission.reader]
actions = ["logs"]
namespaces = ["prod"]
```

The retained current-library test shows `config.validate()` refuses this with an unknown-namespace error, while `config.validate_against(&["prod"])` accepts exactly the same config. `load_manifest` invokes the former before `apply_with_client` can send any request. Build blocks follow the same namespace validation logic; this report does not claim every separate build command uses this loader.

Expected: local syntax/intrinsic validation followed by the server’s authoritative namespace check, or a client check against fetched known namespaces.

### Evidence and fix direction


[src/relish/commands.rs:39](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/relish/commands.rs#L39) calls bare validation in `load_manifest`; [src/config/validate.rs:52](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/config/validate.rs#L52) validates permissions and builds against only the current config. The cluster route deliberately calls `validate_against` with committed namespaces at [src/bun/api/apply.rs:502](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/bun/api/apply.rs#L502).

Separate intrinsic validation from checks requiring server context, without weakening server-side rejection of nonexistent namespaces. Dry-run should explain when namespace existence could not be verified offline. Do not require redeclaring an existing namespace in every file; that can accidentally replace its quota spec.

### Acceptance criteria


- CLI apply reaches the cluster and accepts the separate permission/build file when its namespace exists.
- A truly missing namespace is still refused clearly.
- Existing namespace quotas are not reset by the workaround or fix.
- Test the real client-to-server path, plus offline lint/preview behavior and leader forwarding.

### Existing issue comparison


No matching issue was found. The server’s explicit separate-file support is already implemented; this is a remaining client-side obstruction.

### Current implementation snippets

The excerpts below are verbatim from the audited checkout. Links are pinned to that commit.

[src/relish/commands.rs:39–47](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/relish/commands.rs#L39-L47)

```rust
async fn load_manifest(source: &super::manifest::ManifestSource) -> Result<Config, RelishError> {
    let loaded = super::manifest::load(source).await?;
    if let Some(report) = &loaded.migration_report {
        eprint!("{report}");
        eprintln!();
    }
    loaded.config.validate()?;
    Ok(loaded.config)
}
```

[src/config/validate.rs:50–61](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/config/validate.rs#L50-L61)

```rust
        // Permissions and builds may reference namespaces declared in the
        // same file. Apply passes the already-committed desired-state
        // namespaces through `validate_against`; a bare `validate` only
        // knows about namespaces in this config.
        let declared: Vec<String> = self.namespace.keys().cloned().collect();
        for (name, perm) in &self.permission {
            validate_permission(name, perm, &declared)?;
        }
        for (name, build) in &self.build {
            validate_build(name, build, &declared)?;
        }
        Ok(())
```

[src/bun/api/apply.rs:502–516](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/bun/api/apply.rs#L502-L516)

```rust
    // namespace is rejected before any write lands.
    let known_namespaces: Vec<String> = council
        .desired_state()
        .await
        .namespaces
        .keys()
        .cloned()
        .collect();
    if let Err(e) = config.validate_against(&known_namespaces) {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
            .into_response();
    }
```

---

<!-- Draft 25: 25-compile-partial-success.md -->

# Directory compile exits successfully and emits an incomplete manifest after workload parse failures

Suggested priority: **P2**. Affects **v0.1.4**; verified at `f4757e7789d3672d21f15ca203031d6604d7e11a`.

Verification: Good plus malformed file returns successful partial config.

### Problem and impact


`relish compile` downgrades an invalid workload file to a warning and still emits the remaining config with a successful exit. Unreadable subdirectories are also skipped in one path. The documented `relish compile config/ > all.toml` followed by `relish apply all.toml` can therefore publish an incomplete application set while CI sees a successful compilation.

A syntax error in one required app is not equivalent to a non-fatal formatting suggestion. Printing a warning to stderr does not let ordinary command pipelines distinguish a complete artifact from a partial one.

### Reproduction and verified result


Create `good.toml` with `[app.good] image = "good:v1"` and `broken.toml` with an unterminated `image = [` under `[app.broken]`. The current public compile function returns `Ok`, one merged file, one app and one warning. The CLI prints that partial TOML and returns `Ok(())`. The retained executable test verifies the successful partial compile; the exit behavior is source-verified.

Expected: fail compilation and avoid emitting a deployable success artifact when an input workload cannot be parsed/read. If partial compilation is useful, require an explicit option and make the incomplete state machine-readable.

### Fix direction and acceptance


Classify parse/read failures as hard errors, aggregate file-specific diagnostics, and validate completeness before writing stdout. Keep warnings for genuinely non-fatal conditions. Treat defaults parsing failures consistently: continuing without required defaults should not masquerade as fully resolved configuration.

- One malformed or unreadable required input makes the CLI exit nonzero and prevents successful partial artifact publication.
- A corrected tree compiles completely with deterministic output.
- A pipeline test asserts the exit code as well as the compiler’s return type.
- Explicit partial mode, if introduced, cannot accidentally look like the normal complete mode.

### Existing issue comparison


No matching issue was found. The code’s existing O10 warnings improve visibility but leave the successful incomplete artifact contract unchanged.

### Current implementation snippets

The excerpts below are verbatim from the audited checkout. Links are pinned to that commit.

[src/relish/compile.rs:106–115](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/relish/compile.rs#L106-L115)

```rust
            }
            Err(e) => {
                warnings.push(format!("{}: {e}", entry_path.display()));
            }
        }
    }

    // Recurse into subdirectories — directory name becomes the namespace
    if let Ok(read_dir) = std::fs::read_dir(dir) {
        let mut subdirs: Vec<PathBuf> = read_dir
```

[src/relish/compile.rs:136–145](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/relish/compile.rs#L136-L145)

```rust
                    // Skip unreadable directories
                }
                Err(e) => return Err(e),
            }
        }
    }

    Ok(CompileResult {
        config: merged,
        merged_from,
```

[src/relish/commands.rs:1365–1388](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/relish/commands.rs#L1365-L1388)

```rust
pub fn compile(path: &Path) -> Result<(), RelishError> {
    let result = super::compile::compile(path)?;

    if !result.warnings.is_empty() {
        for w in &result.warnings {
            eprintln!("warning: {w}");
        }
    }

    let app_count = result.config.app.len();
    let job_count = result.config.job.len();
    let file_count = result.merged_from.len();

    // Serialise the merged config as TOML
    let toml = toml::to_string_pretty(&result.config)
        .map_err(|e| RelishError::FormatFailed(e.to_string()))?;
    print!("{toml}");

    eprintln!("compiled {file_count} file(s): {app_count} app(s), {job_count} job(s)");
    Ok(())
}

/// Show structural diff between two configs.
pub fn diff(path_a: &Path, path_b: Option<&Path>) -> Result<(), RelishError> {
```

---

<!-- Draft 26: 26-follower-webhook-lost-trigger.md -->

# GitOps webhook returns 202 on followers but discards the trigger before leader sync

Suggested priority: **P2**. Affects **v0.1.4**; verified at `f4757e7789d3672d21f15ca203031d6604d7e11a`.

Verification: API queue, per-node startup and leader-gated runner traced.

### Problem and unsupported claim


Each council member configured for GitOps exposes a webhook queue and validates a signed delivery locally. The handler enqueues a local nudge and returns `202` saying sync is queued. A follower’s local sync loop consumes that nudge, sees it is not leader and discards it. It never forwards the trigger to the leader.

A stable webhook endpoint can therefore stop providing instant deployment after leadership changes while continuing to acknowledge deliveries successfully. Periodic polling can eventually notice the commit, but accepted webhook delivery does not trigger the promised immediate leader sync ([docs/whitepaper.md:679](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/docs/whitepaper.md#L679)).

### Reproduction / verification


On a configured council with a long poll interval, deliver a correctly signed fresh webhook to a follower. Observe `202` from that follower and no leader fetch before its next poll. Repeat after leader handover with the same fixed endpoint. This is a proposed multi-node regression; no live council was launched for this case.

The parent independently checked all three production stages: [src/bin/bun.rs:2233](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/bin/bun.rs#L2233) creates a queue/runner on every configured council member; [src/bun/api/gitops.rs:20](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/bun/api/gitops.rs#L20) admits only into that local queue; [src/lettuce/runner.rs:64](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/lettuce/runner.rs#L64) receives the signal and then immediately continues on a nonleader. No forwarding path intervenes.

### Fix direction and acceptance


Route accepted deliveries to the current leader or persist a cluster-visible pending sync trigger. Preserve the raw signed payload/header verification and replay/rate guarantees when forwarding; avoid consuming a delivery ID permanently before leader admission succeeds. Return a retryable failure when no coordinator can accept it.

- A valid follower delivery causes prompt leader sync with polling intentionally delayed.
- Leader handover does not silently discard acknowledged deliveries.
- No leader/unreachable leader produces a retryable result unless the nudge is durably retained.
- Replay and rate-limit behavior remain correct across retries and forwarding.

### Existing issue comparison


No matching issue was found. #297 fixes replay registration before local rate admission, not leader routing and acknowledged-but-discarded triggers.

### Current implementation snippets

The excerpts below are verbatim from the audited checkout. Links are pinned to that commit.

[src/bin/bun.rs:2233–2253](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/bin/bun.rs#L2233-L2253)

```rust
    let gitops_webhook_tx =
        if let (Some(gitops), Some(council)) = (config.gitops.clone(), api_council.clone()) {
            let (webhook_tx, webhook_rx) = mpsc::channel::<()>(16);
            if let Some(secret) = gitops.webhook_secret.as_deref() {
                gitops_webhook_validator = Some(std::sync::Arc::new(tokio::sync::Mutex::new(
                    reliaburger::lettuce::webhook::WebhookValidator::new(
                        secret,
                        gitops.webhook_rate_limit,
                    ),
                )));
            }
            reliaburger::lettuce::runner::spawn_gitops_sync(
                council,
                gitops,
                webhook_rx,
                config.storage.data.clone(),
                shutdown.clone(),
            );
            println!("bun: gitops sync loop started");
            Some(webhook_tx)
        } else {
```

[src/bun/api/gitops.rs:67–80](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/bun/api/gitops.rs#L67-L80)

```rust
        // GitHub/Gitea sign the body: `X-Hub-Signature-256: sha256=<hex>`.
        guard.validate(&body, signature, delivery_id.as_deref(), &branch)
    };
    drop(guard);

    match result {
        Ok(_) => {
            permit.send(());
            (
                StatusCode::ACCEPTED,
                Json(serde_json::json!({ "message": "sync queued" })),
            )
                .into_response()
        }
```

[src/lettuce/runner.rs:60–74](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/lettuce/runner.rs#L60-L74)

```rust
                _ = shutdown.cancelled() => break,
                _ = ticker.tick() => {}
                signal = webhook_rx.recv() => {
                    if signal.is_none() {
                        break; // sender dropped
                    }
                    // Drain any queued webhook signals so a burst
                    // collapses into a single sync.
                    while webhook_rx.try_recv().is_ok() {}
                }
            }

            if !council.is_leader().await {
                continue;
            }
```

---

<!-- Draft 27: 27-autoscale-invalid-targets.md -->

# Autoscaling accepts nonpositive and nonfinite targets, disabling or corrupting scaling decisions

Suggested priority: **P2**. Affects **v0.1.4**; verified at `f4757e7789d3672d21f15ca203031d6604d7e11a`.

Verification: Current Config validation accepts all four invalid values.

### Problem


Autoscale target parsing only checks whether Rust can parse an `f64`. `target="0%"`, `"-20%"`, `"NaN"`, and `"inf"` all pass `Config::validate` and apply validation.

Zero/negative targets make `compute_desired` return the current count forever. NaN produces NaN ratios/casts, then fails the hysteresis comparisons, normally preserving the current count. An infinite target yields a zero finite-load ratio and can scale down to the minimum regardless of load. These invalid control parameters look like successfully enabled autoscaling but either disable it or produce incorrect control decisions. Do not reject finite targets above 100% automatically: utilization is measured against a request, so a target above a request can be legitimate.

### Verified reproduction / evidence


Current-library `Config::validate` probe printed `true` for each listed target.

- [src/meat/autoscaler.rs:159–163](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/meat/autoscaler.rs#L159-L163): only parse failure rejected.
- [src/meat/autoscaler.rs:385–392](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/meat/autoscaler.rs#L385-L392): returns any parsed float.
- [src/meat/autoscaler.rs:307–326](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/meat/autoscaler.rs#L307-L326): zero/negative short circuit; unchecked nonfinite ratio/hysteresis.
- [src/config/validate.rs:387–392](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/config/validate.rs#L387-L392): delegates to that parser for validation.

Reject nonfinite or nonpositive targets with the existing clear config error. Defensively refuse nonfinite collected metrics too. Test invalid target values through lint/apply and valid fraction/percentage targets, including a legitimate >100% utilization target. This is separate from closed #299's `min=0` problem.

### Current implementation snippets

The excerpts below are verbatim from the audited checkout. Links are pinned to that commit.

[src/meat/autoscaler.rs:159–166](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/meat/autoscaler.rs#L159-L166)

```rust
        let target =
            parse_percentage(&spec.target).ok_or_else(|| AutoscaleConfigError::InvalidTarget {
                target: spec.target.clone(),
            })?;
        if spec.max == 0 {
            return Err(AutoscaleConfigError::ZeroMax);
        }
        // Scale-to-zero would need a wake-up signal that exists without a
```

[src/meat/autoscaler.rs:385–394](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/meat/autoscaler.rs#L385-L394)

```rust
fn parse_percentage(s: &str) -> Option<f64> {
    let s = s.trim();
    if let Some(pct) = s.strip_suffix('%') {
        pct.trim().parse::<f64>().ok().map(|v| v / 100.0)
    } else {
        // Try as a raw fraction
        s.parse::<f64>().ok()
    }
}

```

[src/meat/autoscaler.rs:307–330](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/meat/autoscaler.rs#L307-L330)

```rust
fn compute_desired(current: u32, metric: f64, config: &AutoscaleConfig) -> u32 {
    if config.target <= 0.0 || current == 0 {
        return current;
    }

    let ratio = metric / config.target;
    let raw_desired = (current as f64 * ratio).ceil() as u32;

    // Hysteresis: only scale down if metric is well below target
    let desired = if raw_desired < current {
        if metric < config.target * config.scale_down_threshold {
            raw_desired
        } else {
            current // not low enough to scale down
        }
    } else {
        raw_desired
    };

    desired.clamp(config.min, config.max)
}

/// Manage autoscale state for all apps.
#[derive(Debug, Default)]
```

---

<!-- Draft 28: 28-cross-path-contracts-and-lifecycle-hardening.md -->

# Unify behavioral contracts across entry points and add lifecycle regression coverage

Suggested priority: **P2 — cross-cutting hardening, implemented last after the accepted individual fixes.** Baseline: v0.1.4 at `f4757e7789d3672d21f15ca203031d6604d7e11a`.

This is a follow-up implementation issue derived from the audit, rather than a 28th independently reproduced runtime defect. It consolidates the remaining architectural and test-harness work after the concrete regressions have been fixed.

### Problem

The main weakness is **consistency across paths**. The same policy or behavior is implemented separately in manual apply, clustered apply, follower forwarding, GitOps, batch dispatch, registry publication and node launch. Individual components have substantial tests, but the complete paths do not consistently preserve authorization, resource identity, configuration meaning or completion guarantees.

Examples from the audit:

- Ordinary apply enforces token scope and Deploy/HostExec grants; batch dispatch bypasses them (draft 03).
- Standalone deploy waits for `run_before`; clustered apply publishes apps first, and GitOps drops the job (07, 22).
- CLI directory compilation applies image defaults and path namespaces; GitOps uses a separate loader (19–21).
- The direct scoped registry read check works, while publication can create the reference that defeats it (04).
- API authentication bounds/offloads Argon2; registry authentication uses the synchronous verifier (12).

The other recurring weaknesses are lifecycle coverage and the meaning of success. Tests often establish an immediate result without verifying expiry, reuse, concurrent writers, failed persistence or reopening. An accepted request, completed run, unchanged preview or successful flush must correspond to the promised observable outcome. A configured line-coverage floor cannot establish those guarantees on its own.

### Scheduling and scope

Execute this as the **final hardening step** after the accepted audit fixes, using their expected-behavior regressions as the baseline. Do not defer urgent authorization/data-loss fixes until this umbrella work lands. The earlier fixes should already add their own failing-first tests; this issue connects those tests, eliminates remaining rule drift and makes future gaps visible in CI.

Before implementation, read the approved issue list and current source. References below identify the audited implementation; upstream fixes may have moved or replaced it. Replace draft numbers with actual GitHub issue numbers when the maintainer publishes them.

Implement the approved audit work through **one merge train PR**, under one dated plan in `docs/plans/`, following `CLAUDE.md`. All smaller implementation PRs merge into its integration branch. The train receives the consolidated maintainer review and final qualification, then merges into the maintainer-selected destination once.

### Merge train workflow

1. Create one integration branch from the maintainer-selected base, using a name such as `codex/audit-hardening-train`, and one train PR targeting the selected release branch or `main`. Keep its description current with the final scope, child-PR checklist, dependency order and qualification evidence. Its initial description must state that qualification is incomplete.
2. Target every smaller implementation PR at that integration branch. Merge the accepted individual fixes (drafts 01–27) in dependency order, then implement draft 28 as the final phase on the same train. Child PRs are implementation units; the maintainer reviews the combined result through the train rather than separately approving every child.
3. Run failing-first regressions and focused local checks for each child, plus any required checks on its target. Do not wait for optional full-cluster/release qualification between child merges. Rebase or resolve integration conflicts against the current train before merging each child; preserve dependency order and the regressions from earlier children.
4. Use a `codex/` integration target rather than a `release-*` child target. The current CI selector enables heavy suites for PRs targeting `main` or `release-*`; other stacked targets use the lighter selection unless labelled `full-ci`. Reuse this existing policy. Do not add `full-ci` to every child or weaken required checks. See `scripts/ci/select-jobs.sh` and its fixtures.
5. The outer train PR can still start full CI on intermediate updates. Existing PR concurrency cancels superseded runs. Integrate ready children without waiting for each obsolete outer run; wait for full qualification of the **final integrated commit** before requesting consolidated approval and merging the train. If a required-check or CI-selection policy prevents this workflow, resolve that explicit policy rather than silently bypassing it.
6. Finish all child integration, run the complete applicable CI/runtime/qualification matrix, attach its evidence to the train and request one consolidated maintainer review. Any changes after that qualification invalidate affected evidence; rerun the applicable checks on the new final commit. Merge the train only after its required checks and maintainer approval.

Preserve intentional semantics: manual apply is additive, GitOps also computes removals, imperative jobs have run identities, and webhook admission authenticates a signed delivery rather than a user bearer. Equivalent guarantees do not require identical transport or execution behavior.

### Required contract matrix

Create a machine-readable inventory, suggested location `tests/contracts/manifest.json`, with stable contract IDs, applicable entry points, exact Rust test names, owning CI/manual gates, and any explicitly unsupported combinations. Generate a human-readable matrix for `docs/testing.md` from that inventory. Each supported combination must be exercised; an unsupported combination must have an explicit-refusal test rather than silent success.

| Contract family | Entry points / dimensions to cover | Observable invariant | Audit drafts |
| --- | --- | --- | --- |
| Authorization and admission | Direct leader, follower forwarding, local/remote batch, registry reads/publication; scoped Deployer, denied grants, allowed and forbidden namespaces | Original principal is preserved; refusals create no desired resource, run, catalogue reference or storage side effect; token hashing uses bounded async admission | 03, 04, 12 |
| Effective configuration and preview | CLI file/directory, manual API, GitOps commit, online/offline preview; defaults, existing namespaces, same-name resources | Equivalent supported inputs produce equivalent namespace-qualified effective specs; invalid/incomplete input cannot claim complete success; material changes are visible | 19–25 |
| Deployment ordering and triggers | Standalone, cluster leader/follower, GitOps; blocked/failed migration, leader handover | Dependent app revision becomes schedulable only after prerequisite success; an accepted webhook reaches a coordinator or is retained for retry | 07, 22, 26 |
| Run identity and completion | Batch dispatch/callback/pull watcher, repeated names, concurrent batches, restart | Only the acknowledged execution generation can settle its record; every admitted job is represented and reaches the correct terminal state | 08, 15 |
| Persistence and artifact identity | Log flush/replay/reopen, metrics multiwriter storage, managed volume creation/remount | Acknowledged durable content is retained; no checkpoint passes missing data; distinct owners/paths cannot overwrite one another’s artifacts | 01, 05, 06 |
| Capacity and storage admission | Cluster apps/batches, stale/missing reports, simultaneous submissions/uploads, consumer publication | Reservations survive the admission/reporting gap; physical usage is bounded; unsupported service size is refused explicitly and does not block unrelated updates | 11, 13, 16 |
| Identity/time and network lifecycle | Signer expiry, routing replacement, active stream lifetime, redirects, DNS UDP-to-TCP retry | Earlier success remains valid for the documented lifetime; current endpoint health persists appropriately; protocol behavior reaches the client intact | 02, 09, 10, 17, 18 |
| Numeric boundary behavior | Cron parser in debug/release, autoscale targets | Invalid inputs are refused; accepted values have identical defined semantics across profiles and finite valid control parameters | 14, 27 |

Keep the matrix finite. For each contract, name the specific supported combinations and why they differ; do not generate every possible cross-product of unrelated runtime, role and transport settings.

### Phase 1 — shared fixtures and assertions

1. Extend existing `tests/support/cluster.rs`, `cluster_harness.rs`, `bun_process.rs` and `task_harness.rs`. Reuse process cleanup, bounded readiness and real API/Raft fixtures. Put portable cases under `tests/suite/` and register them in `tests/suite/main.rs`; keep gated/process-isolated cases in the existing heavy binaries.
2. Add reusable authenticated fixtures. Mint actual scoped credentials and persist realistic permission/namespace state. Exercise at least one refusal and one allowed control for each principal-facing mutating path. Default tokenless/system-token fixtures cannot substitute for a scoped-principal case.
3. Represent entry-point adapters in test support: CLI subprocess, direct HTTP apply, follower HTTP apply, GitOps repository sync and batch submission. Adapters drive the public path and collect observable evidence; they must not reproduce the production policy logic themselves.
4. Add assertions over canonical effective specs, qualified resource identities, execution generations, persisted rows/checkpoints and admitted footprints. Compare exact expected fixtures or independently stated invariants. Reusing the same production helper for both the tested transformation and the expected answer would conceal a shared bug.
5. Retain a small real-binary/real-council check for each important contract family. Fake agent commands are useful for deterministic race scheduling, but at least one real run must prove that admission, forwarding and runtime wiring invoke the tested code.

### Phase 2 — consolidate rules where the matrix finds drift

Inventory remaining duplicated decisions after the individual fixes. Extract only the common domain rule and keep transport-specific orchestration local.

- **Workload admission:** one clearly named admission function for principal scope, namespace grants, host execution and applicable lease/resource restrictions. Call it before writes or dispatch in every supported mutating workload path. Forwarding must retain the original authority or a verified delegation carrying its limits. Internal service credentials must not accidentally replace the user’s scope.
- **Configuration resolution:** one typed directory resolver operating on a deterministic set of path/content inputs, usable from filesystem compilation and a verified Git commit. Separate intrinsic validation from checks requiring committed namespaces. Use namespace-qualified identity and effective specs consistently in apply, diff and preview. Do not interpret every TOML file as a workload declaration when some are defaults.
- **Execution/storage identity:** use existing typed identity/generation structures where available. Carry them across tracker records, status, callbacks and durable artifact naming; avoid a parallel string convention in each subsystem. If a persisted/wire shape must change, apply the repository’s compatibility-generation rules and update restart fixtures.
- **Persistence/admission ownership:** make pending bytes, batches and reservations have a clear owner until durable success or confirmed release. Shared helpers may support this, but log checkpoints, registry quota and volume provisioning have different commit boundaries and need explicit adapters.

Preserve the regression tests while extracting helpers. Introduce a typed admission/resolution result only where it makes bypassing an important rule harder; avoid a generic policy framework or broad code reorganization unrelated to a failing contract. Each extraction child PR must demonstrate the behavior of all callers it changes before joining the train; its results contribute to the consolidated review.

### Phase 3 — deterministic lifecycle and boundary tests

Add narrow clock and storage fault seams where the existing interfaces cannot drive the required transition. Production defaults must retain real time and real storage. Do not add operator-facing fault injection switches.

- Distinguish wall-clock certificate/token validity from Tokio’s monotonic deadlines. Advancing a paused Tokio clock does not advance `SystemTime::now()`. Pass an explicit clock/time into lifecycle decisions or use a narrowly injectable clock; test expiry just before, at and after the boundary, renewal and reopen/restart. Do not require a one-hour sleep to detect signer expiry.
- Fail log persistence at directory creation, Parquet write and checkpoint publication; then ingest more rows, retry and reopen. Assert all promised rows appear exactly once and checkpoints never advance beyond the committed data boundary. Exercise concurrent ingestion and cancellation where the contract permits them.
- Open two metrics writers before either flushes. Test interleaved/concurrent writes and restart using the same prefix. Assert immutable chunk identity, retention of both writers’ samples and correct query ownership.
- Delay new batch acknowledgement while publishing an old terminal status; then deliver old/new callbacks out of order. Assert only the new acknowledged run settles the new batch and its actual failure cannot be overwritten by old success.
- Hold a prerequisite migration behind an explicit barrier. Read committed app state and observe launch activity before releasing it. Cover success, failure, timeout and leader handover without a race-prone sleep.
- Test missing/stale reports and two admissions before runtime reports catch up. Assert reservations account for both pending and reported work without duplication.
- Rebuild routing while a backend remains failed; replace its address/generation and deliver a delayed old probe result. Assert the verdict belongs to the correct endpoint identity.
- Generate parser/control inputs around numeric extremes: cron field boundaries and maximal steps; zero, negative, NaN and infinite autoscale targets; backend capacity at 31/32/33 and rollout surge boundaries. Add a small release-profile parser test lane because debug overflow checks can hide release-only behavior.
- Keep a real ingress test active beyond the former 30-second limit, and real redirect/DNS transport tests. Use virtual time only where all timing consumers are injectable; retain the bounded wall-clock protocol test in its appropriate gate.

Assertions must state expected fixed behavior. The retained audit observation tests assert existing faults and must be inverted or replaced when adopted. A passing test that merely confirms an expired image is refused does not establish the promised retained-image deployment contract.

### Phase 4 — CI evidence, sensitivity and documentation

1. Extend the existing ignored-test/JUnit evidence checks to the new contract inventory. Verify exact required test names were discovered and executed by the assigned lane. Detect removed/renamed cases, empty applicable sets and filters that silently stop selecting a required combination. Preserve current job-selection tests and leak/timeout checks.
2. Run portable contract cases through the ordinary `make test`/coverage lane. Route privileged and real-council cases through their existing gates. Keep the small release-profile boundary lane separate and retain its named JUnit evidence. Manual-host cases must have a documented owner, command and qualification receipt.
3. Demonstrate test sensitivity with bounded, deliberate mutations in disposable checkouts: omit one scope check, compare a bare name instead of a run ID, drop a failed flush batch, or skip directory defaults. At least one named regression must fail for each representative mutation. Revert every mutation; none is part of the shipped implementation. Do not add a permanent expensive whole-repository mutation campaign to every PR.
4. Record per-family contract execution and relevant changed-code coverage alongside the existing aggregate coverage result. Keep the current aggregate floor; raising that percentage alone is not this issue’s completion criterion.
5. Update `docs/testing.md`, `docs/design/test-harness.md` and the affected existing book chapters, explaining the rules, test boundaries and why paired paths share domain logic. Link user-visible claims to their contract IDs and state unsupported behavior explicitly. Record exact commands, commit, host/runtime, failures and sensitivity checks in a dated qualification report.

### Completion criteria

- Every applicable matrix entry has an expected-behavior test and an execution owner; explicit-refusal cases cover unsupported paths.
- Equivalent inputs retain authorization, qualified identity and effective configuration meaning across the named entry points. Intentional differences are documented and tested.
- Lifecycle tests cover expiry, repeat execution, failed persistence, concurrent writers/admissions and restart with observable outcomes; no current-fault observation is mistaken for a passing regression.
- New shared rules are exercised through each production caller, including at least one real transport/runtime check per important family.
- Representative negative controls prove the regressions fail when the relevant rule is bypassed; all temporary mutations are removed.
- CI evidence fails when a required contract case disappears or is not executed. Existing portable, privileged, cluster and release gates remain effective, with new cases assigned appropriately.
- Required formatting, Clippy, doctest and applicable runtime checks pass on the final integrated train commit; documentation and a qualification receipt explain exactly what was exercised.
- All implementation children target and merge into one integration branch. The single train PR contains the combined result, current checklist and final qualification evidence for consolidated maintainer approval.

### Existing issue comparison

Drafts 01–27 own the concrete defects and their immediate fixes. This issue owns the final shared-contract architecture, fixtures and cross-path/lifecycle evidence. Existing #303/#304 cover ignored-test ownership and CI/JUnit evidence; build on those mechanisms rather than reopening their completed work. #351 and #505/#508 own agent-loop timing defects; retain their existing starvation harness as a model for deterministic adverse-condition tests.

### Current implementation snippets

These excerpts are verbatim from the audited baseline and illustrate why paired-path contracts are needed. Re-read the current implementation after the individual fixes; they may already have consolidated some of this logic.

Ordinary apply enforces scope and permission grants.

[src/bun/api/apply.rs:273–299](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/bun/api/apply.rs#L273-L299)

```rust
    for (app_name, namespace, host_execution) in targets {
        if let Err(resp) =
            crate::sesame::auth::authorize_scoped(auth.as_deref(), app_name, namespace)
        {
            return resp;
        }
        if let Err(resp) = crate::sesame::auth::authorize_permission(
            auth.as_deref(),
            crate::config::PermissionAction::Deploy,
            app_name,
            namespace,
            &permissions,
        ) {
            return resp;
        }
        if host_execution
            && let Err(resp) = crate::sesame::auth::authorize_permission(
                auth.as_deref(),
                crate::config::PermissionAction::HostExec,
                app_name,
                namespace,
                &permissions,
            )
        {
            return resp;
        }
    }
```

Batch checks the role and forwards before inspecting workload admission.

[src/bun/batch.rs:700–712](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/bun/batch.rs#L700-L712)

```rust
    // Submitting work is a Deployer action (AUTH2 — it used to take no auth).
    if let Err(resp) =
        crate::sesame::auth::authorize(auth.as_deref(), crate::sesame::types::ApiRole::Deployer)
    {
        return resp;
    }
    // Followers forward the raw body to the leader (the tracker and
    // the aggregated capacity view live there).
    if let Some(council) = &state.council
        && !council.is_leader().await
    {
        return forward_to_leader(&state, council, "/v1/batch", body).await;
    }
```

CLI defaults have their own field-specific resolver.

[src/relish/compile.rs:194–209](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/relish/compile.rs#L194-L209)

```rust
/// Apply defaults to a config. For each app, if a field from defaults
/// is missing, inject it. Currently supports the `image` default.
fn apply_defaults(config: &mut Config, defaults: &BTreeMap<String, toml::Value>) {
    let default_image = defaults
        .get("image")
        .and_then(|v| v.as_str())
        .map(String::from);

    for app in config.app.values_mut() {
        if app.image.is_none()
            && let Some(ref img) = default_image
        {
            app.image = Some(img.clone());
        }
    }
}
```

GitOps parses and merges its directory contents separately.

[src/lettuce/sync.rs:257–281](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/lettuce/sync.rs#L257-L281)

```rust
    for (path, content) in ordered {
        let file_config = match Config::parse(content) {
            Ok(config) => config,
            Err(e) => {
                errors.insert(path.clone(), e.to_string());
                continue;
            }
        };

        // A resource named in two files is ambiguous: report it against
        // this later-sorted file and let the earlier definition stand,
        // rather than silently letting hash order pick a winner.
        if let Some(duplicate) = first_duplicate(&merged, &file_config) {
            errors.insert(
                path.clone(),
                format!("duplicate resource {duplicate} already declared in an earlier file"),
            );
            continue;
        }

        merged.app.extend(file_config.app);
        merged.job.extend(file_config.job);
        merged.namespace.extend(file_config.namespace);
        merged.permission.extend(file_config.permission);
        merged.build.extend(file_config.build);
```

Shared desired-state writes alone do not cover parsing, imperative jobs or admission.

[src/council/apply.rs:1–19](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/council/apply.rs#L1-L19)

```rust
//! The one path that turns a parsed `Config` into desired-state writes
//! (12b.2 T6).
//!
//! Manual `relish apply` (`bun::api`) and GitOps sync (`lettuce`) both
//! call [`config_to_desired_writes`]. Sharing one function is what makes
//! "the same config converges identically whether you apply it by hand
//! or through git" true *by construction*: there's no second code path
//! that could drift.
//!
//! Only the declarative kinds live here: apps, namespaces, permissions.
//! Jobs run to completion (not reconciled desired state) and builds are
//! dispatched imperatively; both are validated but not written by this
//! function. See chapter 7 for why builds aren't a reconciling resource.
//!
//! Deletion is *not* the concern of this function. Manual apply is
//! additive: it writes what's in the file and never prunes what isn't,
//! matching how app apply already behaves. GitOps reconciles a whole
//! repo against desired state, so it computes deletions separately (in
//! `lettuce`) and layers them on top of these writes.
```

Completion matching lacks the execution generation needed by lifecycle tests.

[src/bun/batch.rs:461–475](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/bun/batch.rs#L461-L475)

```rust
fn job_outcome(statuses: &[InstanceStatus], name: &str, namespace: &str) -> Option<bool> {
    for status in statuses
        .iter()
        .filter(|s| s.app_name == name && s.namespace == namespace)
    {
        let outcome = match (status.state.as_str(), status.exit_code) {
            ("failed", _) => Some(false),
            ("stopped", Some(0) | None) => Some(true),
            _ => None,
        };
        if outcome.is_some() {
            return outcome;
        }
    }
    None
```

Persistence tests must exercise the drained batch and checkpoint together.

[src/ketchup/log_store.rs:600–613](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/ketchup/log_store.rs#L600-L613)

```rust
    pub fn take_flush_batch(&mut self) -> Result<Option<LogPendingFlush>, KetchupError> {
        let Some(batch) = self.buffer_to_batch()? else {
            return Ok(None);
        };
        let filename = format!("logs_{:06}.parquet", self.flush_counter);
        let path = self.data_dir.join(filename);
        self.buffer.clear();
        self.flush_counter += 1;
        Ok(Some(LogPendingFlush {
            data_dir: self.data_dir.clone(),
            path,
            batch,
            checkpoint: self.ingested.clone(),
        }))
```

Reuse absolute deadlines in adapters and assertions instead of resetting budgets.

[src/testkit/deadline.rs:77–90](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/testkit/deadline.rs#L77-L90)

```rust

    /// Run a future without allowing it to exceed this deadline.
    pub async fn run<T, F>(&self, operation: &str, future: F) -> Result<T, DeadlineError>
    where
        F: Future<Output = T>,
    {
        tokio::time::timeout_at(self.expires_at, future)
            .await
            .map_err(|_| DeadlineError::Exceeded {
                operation: operation.to_string(),
                budget_ms: self.budget_ms,
            })
    }
}
```

### Suggested first child PR into the merge train

Record the dated plan when setting up the train. In its first draft-28 child PR, implement the contract inventory and reusable authenticated fixture. Implement one paired-path regression covering direct leader apply and follower-forwarded batch with the same scoped principal, including a permitted control and a refusal with zero side effects. Register it in the portable suite, retain its CI evidence, and demonstrate that a temporary scope-check bypass makes it fail. Then expand by family in the phase order above. Merge this child into the train after its focused and required checks, then continue the remaining children without separate full-qualification waits. This gives the next agent a bounded first deliverable and exercises the hardest fixture requirements early.

