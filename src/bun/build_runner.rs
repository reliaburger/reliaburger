//! Asynchronous image builds (Phase 12, F2).
//!
//! The Stage 4 build handler worked but ran synchronously — a
//! minutes-long `buildah` build held the HTTP request open, and the
//! CLI's 300-second client timeout made anything real strand mid-
//! response. This module lifts that handler body into a spawned
//! runner behind a small tracker: `POST /v1/build` answers `202` with
//! a build id immediately, `GET /v1/build/{id}` reports progress, and
//! nodes without `buildah` delegate to a peer that reported the
//! capability in its state reports.
//!
//! Since 12b.2 (JOB4/JOB7/D18) the tracker is Raft-durable when a
//! council exists: ids come from a replicated counter (never reused
//! across restarts), state transitions are Raft entries, delegation
//! transfers the context blob to the builder and retries across
//! capable peers, and — when the trust policy requires signatures — a
//! build only counts as `Completed` once the pushed digest carries a
//! signature that chains to the cluster CA.

use std::collections::HashMap;

use axum::Json;
use axum::extract::{Path as AxumPath, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};

use crate::meat::batch_tracker::epoch_now_secs;

use super::api::ApiState;

/// How long a terminal build record is kept before registration-time
/// pruning removes it.
pub const TERMINAL_BUILD_RETENTION_SECS: u64 = 3600;

/// Ceiling on retained terminal build records (the deploy-history
/// cap-at-50 precedent).
pub const MAX_TERMINAL_BUILDS: usize = 50;

/// Request body for `/v1/build` and `/v1/build/run`.
///
/// `deny_unknown_fields` (JOB2): the registry destination is now
/// server-owned — a caller that smuggles a `registry_port` (or any
/// other unexpected field) to point a privileged Bun at an arbitrary
/// localhost service gets a 400, not a silently-ignored field.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BuildSubmitRequest {
    pub name: String,
    pub context_digest: String,
    pub spec: crate::config::build::BuildSpec,
}

/// Lifecycle of one tracked build.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case", tag = "status")]
pub enum BuildState {
    /// The runner is fetching context / building / pushing.
    Running,
    /// Built, pushed, and (when the policy requires it) signed.
    Completed { image: String },
    /// Any stage failed; the reason is the last useful stderr.
    Failed { reason: String },
    /// Delegated to a peer builder; status reads proxy through.
    Delegated { url: String, remote_id: u64 },
}

impl BuildState {
    /// Short name of the state, for transition error messages.
    pub fn kind(&self) -> &'static str {
        match self {
            BuildState::Running => "running",
            BuildState::Completed { .. } => "completed",
            BuildState::Failed { .. } => "failed",
            BuildState::Delegated { .. } => "delegated",
        }
    }

    /// Whether the state can never change again on this node.
    /// `Delegated` counts: the entry node's record is a pointer, and
    /// the real lifecycle lives on the builder.
    pub fn is_terminal(&self) -> bool {
        !matches!(self, BuildState::Running)
    }
}

/// One tracked build: what it builds, where it runs, and its state.
/// Serialised into the Raft state machine, so every field is data.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuildRecord {
    /// The `[build.*]` section name.
    pub name: String,
    /// Gossip name of the node running the build; `None` for records
    /// created before the runner was known (delegated pointers).
    pub runner_node: Option<String>,
    /// Current lifecycle state.
    pub state: BuildState,
    /// Registration time as seconds since the Unix epoch.
    pub created_at_epoch_secs: u64,
}

/// Why a build state update was rejected.
#[derive(Debug, Clone, thiserror::Error, PartialEq, Eq)]
pub enum BuildUpdateError {
    #[error("build {build_id} is not tracked")]
    UnknownBuild { build_id: u64 },
    #[error("illegal build transition from {from} to {to}")]
    IllegalTransition {
        from: &'static str,
        to: &'static str,
    },
}

/// The durable half of build tracking, mirrored into the Raft
/// `DesiredState` when a council exists (`#[serde(default)]` there, so
/// pre-12b.2 snapshots load cleanly).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BuildDurableState {
    /// Next build id to allocate. Monotonic; never reset or reused.
    pub next_build_id: u64,
    /// Tracked builds as `(id, record)` pairs, oldest first.
    pub builds: Vec<(u64, BuildRecord)>,
}

impl Default for BuildDurableState {
    fn default() -> Self {
        Self {
            next_build_id: 1,
            builds: Vec::new(),
        }
    }
}

impl BuildDurableState {
    /// Register a build: allocate the next id, prune stale terminal
    /// records (using the new record's clock — deterministic across
    /// Raft replicas), and store the record.
    pub fn register(&mut self, record: BuildRecord) -> u64 {
        let id = self.next_build_id.max(1);
        self.next_build_id = id + 1;
        self.prune_terminal(record.created_at_epoch_secs);
        self.builds.push((id, record));
        id
    }

    fn prune_terminal(&mut self, now_epoch_secs: u64) {
        self.builds.retain(|(_, record)| {
            !(record.state.is_terminal()
                && now_epoch_secs.saturating_sub(record.created_at_epoch_secs)
                    > TERMINAL_BUILD_RETENTION_SECS)
        });
        let terminal = self
            .builds
            .iter()
            .filter(|(_, r)| r.state.is_terminal())
            .count();
        if terminal > MAX_TERMINAL_BUILDS {
            let mut to_drop = terminal - MAX_TERMINAL_BUILDS;
            self.builds.retain(|(_, record)| {
                if to_drop > 0 && record.state.is_terminal() {
                    to_drop -= 1;
                    false
                } else {
                    true
                }
            });
        }
    }

    /// Look up a build by id.
    pub fn get(&self, build_id: u64) -> Option<&BuildRecord> {
        self.builds
            .iter()
            .find(|(id, _)| *id == build_id)
            .map(|(_, record)| record)
    }

    /// Apply a state transition, validating it. A repeat of the current
    /// state kind is idempotent; anything else from a terminal state is
    /// rejected, as is any transition of a `Delegated` pointer.
    pub fn update(&mut self, build_id: u64, state: BuildState) -> Result<(), BuildUpdateError> {
        let record = self
            .builds
            .iter_mut()
            .find(|(id, _)| *id == build_id)
            .map(|(_, record)| record)
            .ok_or(BuildUpdateError::UnknownBuild { build_id })?;
        if record.state.kind() == state.kind() {
            return Ok(()); // idempotent duplicate
        }
        if record.state.is_terminal() {
            return Err(BuildUpdateError::IllegalTransition {
                from: record.state.kind(),
                to: state.kind(),
            });
        }
        record.state = state;
        Ok(())
    }
}

/// Node-local build registry. Standalone it is the only tracker; in a
/// cluster it mirrors the Raft records this node cares about, so
/// status reads don't wait on replication.
#[derive(Debug, Default)]
pub struct BuildRegistry {
    next_id: u64,
    builds: HashMap<u64, BuildRecord>,
}

impl BuildRegistry {
    /// Standalone registration with a process-local id.
    pub fn register(&mut self, record: BuildRecord) -> u64 {
        self.next_id += 1;
        self.builds.insert(self.next_id, record);
        self.next_id
    }

    /// Insert or overwrite a record under a known (Raft-allocated) id.
    pub fn set(&mut self, id: u64, record: BuildRecord) {
        self.builds.insert(id, record);
    }

    /// Update the state of a tracked record; a no-op for unknown ids.
    pub fn set_state(&mut self, id: u64, state: BuildState) {
        if let Some(record) = self.builds.get_mut(&id) {
            record.state = state;
        }
    }

    pub fn get(&self, id: u64) -> Option<BuildRecord> {
        self.builds.get(&id).cloned()
    }
}

// ---------------------------------------------------------------------------
// Durable tracking plumbing
// ---------------------------------------------------------------------------

/// Cross-node build tracking request, POSTed to the leader's
/// `/v1/build/track` by nodes whose council handle is a follower.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum BuildTrackRequest {
    /// Allocate an id and store the record; the reply carries `build_id`.
    Register { record: BuildRecord },
    /// Apply a state transition to a tracked build.
    Update { build_id: u64, state: BuildState },
}

/// Propose a tracking operation to Raft from this node, forwarding to
/// the leader over HTTP when this node's council handle is a follower.
/// Returns the allocated id for `Register`, `None` for `Update`.
async fn propose_track(
    state: &ApiState,
    request: &BuildTrackRequest,
) -> Result<Option<u64>, String> {
    let Some(council) = &state.council else {
        return Err("no council available".to_string());
    };
    let raft_request = match request {
        BuildTrackRequest::Register { record } => {
            crate::council::types::RaftRequest::BuildRegister {
                build: record.clone(),
            }
        }
        BuildTrackRequest::Update { build_id, state } => {
            crate::council::types::RaftRequest::BuildUpdate {
                build_id: *build_id,
                state: state.clone(),
            }
        }
    };
    match council.write(raft_request).await {
        Ok(crate::council::types::CouncilResponse::BuildRegistered { build_id }) => {
            Ok(Some(build_id))
        }
        Ok(crate::council::types::CouncilResponse::Refused { reason }) => Err(reason),
        Ok(_) => Ok(None),
        Err(crate::council::CouncilError::ForwardToLeader { .. }) => {
            forward_track_to_leader(state, council, request).await
        }
        Err(e) => Err(format!("raft write failed: {e}")),
    }
}

/// POST the tracking operation to the leader's `/v1/build/track`.
async fn forward_track_to_leader(
    state: &ApiState,
    council: &crate::council::CouncilNode,
    request: &BuildTrackRequest,
) -> Result<Option<u64>, String> {
    let Some(leader_url) = super::api::leader_api_url(state, council).await else {
        return Err("no cluster leader known yet".to_string());
    };
    let mut proxied = state
        .cluster_http
        .client()
        .post(format!("{leader_url}/v1/build/track"))
        .json(request);
    if let Some(token) = &state.service_token {
        proxied = proxied.bearer_auth(token);
    }
    let response = proxied
        .send()
        .await
        .map_err(|e| format!("leader forward failed: {e}"))?;
    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        return Err(format!("leader refused the tracking op ({status}): {body}"));
    }
    let value: serde_json::Value = response
        .json()
        .await
        .map_err(|e| format!("leader track response unreadable: {e}"))?;
    Ok(value["build_id"].as_u64())
}

/// Register a build record durably (Raft when a council exists, the
/// local registry standalone), mirroring it locally either way.
pub(crate) async fn register_build_record(
    state: &ApiState,
    record: BuildRecord,
) -> Result<u64, String> {
    let build_id = match &state.council {
        Some(_) => propose_track(
            state,
            &BuildTrackRequest::Register {
                record: record.clone(),
            },
        )
        .await?
        .ok_or_else(|| "raft register returned no build id".to_string())?,
        None => state.build_registry.lock().await.register(record.clone()),
    };
    if state.council.is_some() {
        state.build_registry.lock().await.set(build_id, record);
    }
    Ok(build_id)
}

/// Record a build state transition durably, mirroring it locally.
/// Never fails the caller: the mirror always updates, and a Raft write
/// failure is logged (the honest-failure path on status reads catches
/// records that stayed `Running`).
pub(crate) async fn update_build_state(state: &ApiState, build_id: u64, new_state: BuildState) {
    state
        .build_registry
        .lock()
        .await
        .set_state(build_id, new_state.clone());
    if state.council.is_none() {
        return;
    }
    let request = BuildTrackRequest::Update {
        build_id,
        state: new_state,
    };
    for attempt in 0..3u32 {
        match propose_track(state, &request).await {
            Ok(_) => return,
            Err(e) if attempt == 2 => {
                eprintln!("bun: build {build_id}: state update not written to raft: {e}");
            }
            Err(_) => {
                tokio::time::sleep(std::time::Duration::from_millis(200 * (attempt as u64 + 1)))
                    .await;
            }
        }
    }
}

/// Look up a build record: the local mirror first, then the replicated
/// Raft state (which also serves reads for builds registered elsewhere).
async fn lookup_build(state: &ApiState, build_id: u64) -> Option<BuildRecord> {
    if let Some(record) = state.build_registry.lock().await.get(build_id) {
        return Some(record);
    }
    let council = state.council.as_ref()?;
    council
        .desired_state()
        .await
        .build_state
        .get(build_id)
        .cloned()
}

// ---------------------------------------------------------------------------
// Filesystem and subprocess plumbing
// ---------------------------------------------------------------------------

/// An owned build directory that deletes itself on drop (JOB6).
///
/// Rust has no destructors you call by hand; instead the `Drop` trait
/// runs code the moment a value goes out of scope, on *every* exit path
/// — normal return, early `return`, `?`, or a panic unwinding the
/// stack. Wrapping the temp dir in one of these means a build that
/// fails, times out, or panics still leaves nothing behind. This is the
/// RAII pattern (Resource Acquisition Is Initialisation) that C++ calls
/// the same name; Rust makes it the default way to manage resources.
struct ScopedDir {
    path: std::path::PathBuf,
}

impl ScopedDir {
    /// Create a unique per-build directory under `base`. The random
    /// suffix means two concurrent builds of the *same* context digest
    /// never share a directory (the old digest-derived path did).
    fn new(base: &std::path::Path, build_id: u64) -> std::io::Result<Self> {
        use rand::Rng as _;
        let suffix: u64 = rand::thread_rng().r#gen();
        let path = base.join(format!("{build_id}-{suffix:016x}"));
        std::fs::create_dir_all(&path)?;
        Ok(Self { path })
    }

    fn path(&self) -> &std::path::Path {
        &self.path
    }
}

impl Drop for ScopedDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

/// Why a capped context download stopped short.
enum DownloadError {
    /// The body exceeded the configured byte cap.
    TooLarge,
    /// A transport or disk error.
    Io(String),
}

/// Stream a context download to `path`, aborting once more than
/// `max_bytes` have arrived (JOB6). Counting bytes as they stream keeps
/// peak memory bounded — the whole body is never buffered.
async fn download_to_file_capped(
    response: reqwest::Response,
    path: &std::path::Path,
    max_bytes: u64,
) -> Result<(), DownloadError> {
    use futures_util::StreamExt as _;
    use tokio::io::AsyncWriteExt as _;

    let mut file = tokio::fs::File::create(path)
        .await
        .map_err(|e| DownloadError::Io(e.to_string()))?;
    let mut stream = response.bytes_stream();
    let mut written = 0u64;
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| DownloadError::Io(e.to_string()))?;
        written += chunk.len() as u64;
        if written > max_bytes {
            return Err(DownloadError::TooLarge);
        }
        file.write_all(&chunk)
            .await
            .map_err(|e| DownloadError::Io(e.to_string()))?;
    }
    file.flush()
        .await
        .map_err(|e| DownloadError::Io(e.to_string()))?;
    Ok(())
}

/// Read a response body into memory, aborting past `max_bytes`.
async fn read_body_capped(response: reqwest::Response, max_bytes: u64) -> Result<Vec<u8>, String> {
    use futures_util::StreamExt as _;
    let mut stream = response.bytes_stream();
    let mut body = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| e.to_string())?;
        if body.len() as u64 + chunk.len() as u64 > max_bytes {
            return Err(format!("body exceeds the {max_bytes} byte limit"));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

/// Why a bounded subprocess run failed to produce output.
enum BoundedError {
    /// The process outlived its timeout and was killed.
    Timeout,
    /// The process could not be spawned or waited on.
    Spawn(std::io::Error),
}

/// SIGKILL an entire process group by its leader pid (JOB6). Buildah
/// spawns children; killing only the parent orphans them. We put the
/// build in its own process group and signal the whole group.
#[cfg(unix)]
fn kill_process_group(pid: u32) {
    use nix::sys::signal::{Signal, kill};
    use nix::unistd::Pid;
    // A negative pid signals the process group whose id is `pid`.
    let _ = kill(Pid::from_raw(-(pid as i32)), Signal::SIGKILL);
}

/// Run a subprocess in `dir`, bounded by `timeout`. On Unix the child
/// leads its own process group and `kill_on_drop` is a backstop, so a
/// timeout kills Buildah *and its children*, not just the parent (JOB6).
async fn run_bounded(
    program: &str,
    args: &[String],
    dir: &std::path::Path,
    timeout: std::time::Duration,
) -> Result<std::process::Output, BoundedError> {
    let mut command = tokio::process::Command::new(program);
    command
        .args(args)
        .current_dir(dir)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    command.process_group(0);

    let child = command.spawn().map_err(BoundedError::Spawn)?;
    #[cfg(unix)]
    let pid = child.id();

    // Take stdout/stderr so we can collect them while waiting.
    match tokio::time::timeout(timeout, child.wait_with_output()).await {
        Ok(Ok(output)) => Ok(output),
        Ok(Err(e)) => Err(BoundedError::Spawn(e)),
        Err(_) => {
            // Dropping the wait future here also drops `child`, so
            // `kill_on_drop` SIGKILLs the direct child; the group kill
            // reaches its grandchildren too.
            #[cfg(unix)]
            if let Some(pid) = pid {
                kill_process_group(pid);
            }
            Err(BoundedError::Timeout)
        }
    }
}

/// Whether this node can build (probed per call — cheap, and the
/// submit path is rare).
pub async fn local_buildah_available() -> bool {
    tokio::process::Command::new("buildah")
        .arg("--version")
        .output()
        .await
        .map(|out| out.status.success())
        .unwrap_or(false)
}

// ---------------------------------------------------------------------------
// Peer builder selection and context transfer
// ---------------------------------------------------------------------------

/// A peer that can run a build: where its API lives and where its
/// registry listens. Both derive from the membership table and this
/// node's own config — never from request data (JOB2/D18).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PeerBuilder {
    /// The peer's Bun API base URL.
    pub api_url: String,
    /// The peer's Pickle registry as `host:port` (cluster registry
    /// ports are uniform, like API ports).
    pub registry_address: String,
}

/// All capable peer builders, in membership order: any member whose
/// latest state report says it has buildah, excluding this node.
/// Multiple candidates make builder failure retryable (D18).
pub fn find_peer_builders(
    members: &[super::api::NodeMembershipInfo],
    aggregated: &crate::reporting::aggregator::AggregatedState,
    self_name: Option<&str>,
    cluster_http: &crate::cluster::ClusterHttp,
    registry_port: u16,
) -> Vec<PeerBuilder> {
    members
        .iter()
        .filter(|m| Some(m.node_id.0.as_str()) != self_name)
        .filter(|m| {
            aggregated
                .reports
                .get(&m.node_id)
                .is_some_and(|report| report.has_buildah)
        })
        .map(|m| PeerBuilder {
            api_url: cluster_http.url(&m.address.to_string(), ""),
            registry_address: format!("{}:{registry_port}", m.address.ip()),
        })
        .collect()
}

/// Copy the build-context blob from this node's registry to a peer
/// builder's registry (12b.2, the JOB5 residual). Locally-uploaded
/// `_buildcontext` blobs are bare blobs — no manifest, so no holder
/// record, no replication — which means a delegated builder's own
/// registry has never heard of them. Transferring the blob at
/// delegation time closes that gap without inventing request-supplied
/// registry endpoints: both addresses derive from config + membership.
pub async fn transfer_context_to_builder(
    client: &reqwest::Client,
    local_registry_port: u16,
    builder_registry_address: &str,
    digest: &str,
    max_bytes: u64,
) -> Result<(), String> {
    // Skip the copy if the builder already holds the blob.
    let peer_blob_url =
        crate::pickle::build::context_download_url_at(builder_registry_address, digest);
    if let Ok(response) = client.head(&peer_blob_url).send().await
        && response.status().is_success()
    {
        return Ok(());
    }

    let local_url = crate::pickle::build::context_download_url(local_registry_port, digest);
    let response = client
        .get(&local_url)
        .send()
        .await
        .map_err(|e| format!("local registry unreachable: {e}"))?;
    if !response.status().is_success() {
        return Err(format!(
            "context blob {digest} not found in the local registry"
        ));
    }
    let body = read_body_capped(response, max_bytes)
        .await
        .map_err(|e| format!("context read failed: {e}"))?;

    let upload_url = crate::pickle::build::context_upload_url_at(builder_registry_address, digest);
    let response = client
        .post(&upload_url)
        .body(body)
        .send()
        .await
        .map_err(|e| format!("builder registry unreachable: {e}"))?;
    if !response.status().is_success() {
        return Err(format!(
            "builder registry refused the context blob: {}",
            response.status()
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Signing
// ---------------------------------------------------------------------------

/// Sign a pushed manifest digest with a certificate issued by the
/// cluster's Workload CA and attach the signature via Raft (JOB7).
///
/// The old path generated an ephemeral external key that no trust
/// policy could ever list, so the signature was decorative. A keyless
/// signature over a CA-issued certificate verifies against the
/// cluster's root CA — the same check `require_signatures` deploys
/// run. The signature is verified locally before it is attached, and
/// an `AttachSignature` for a digest the catalogue doesn't know is
/// refused by the state machine, not silently dropped.
pub async fn sign_pushed_image(
    council: &crate::council::CouncilNode,
    digest: &crate::pickle::types::Digest,
    namespace: &str,
    build_name: &str,
    node_name: &str,
) -> Result<(), String> {
    let uri = crate::sesame::types::SpiffeUri {
        trust_domain: "default".to_string(),
        namespace: namespace.to_string(),
        workload_type: crate::sesame::types::WorkloadType::Job,
        name: build_name.to_string(),
    };
    let (csr_der, key_der) = crate::sesame::identity::create_workload_csr(&uri)
        .map_err(|e| format!("csr generation failed: {e}"))?;
    let signed = council
        .sign_workload_csr(
            &csr_der,
            &uri,
            "default",
            node_name,
            &format!("build-{build_name}"),
        )
        .await
        .map_err(|e| format!("csr signing failed: {e}"))?;
    let signature = crate::pickle::signing::create_keyless_signature(
        digest,
        &signed.cert_der,
        &key_der,
        std::slice::from_ref(&signed.workload_ca_cert_der),
        "reliaburger-council",
        &uri.to_uri(),
    )
    .map_err(|e| format!("signature creation failed: {e}"))?;

    // Self-check before attaching: the signature must verify against
    // the cluster root CA — the exact check enforcement runs.
    crate::pickle::signing::verify_signature(
        &signature,
        digest,
        &crate::config::node::TrustPolicySection::default(),
        Some(&signed.root_ca_cert_der),
        None,
    )
    .map_err(|e| format!("signature failed verification against the cluster ca: {e}"))?;

    let attach = crate::pickle::types::AttachSignature {
        manifest_digest: digest.clone(),
        signature,
    };
    match council
        .write(crate::council::types::RaftRequest::AttachSignature(attach))
        .await
    {
        Ok(crate::council::types::CouncilResponse::Refused { reason }) => {
            Err(format!("signature attach refused: {reason}"))
        }
        Ok(_) => Ok(()),
        Err(e) => Err(format!("signature attach failed: {e}")),
    }
}

// ---------------------------------------------------------------------------
// The build runner
// ---------------------------------------------------------------------------

/// Everything `run_build` needs, bundled so the spawned task owns its
/// dependencies. Built from `ApiState` on the handler path; tests can
/// construct it directly.
#[derive(Clone)]
pub struct BuildRunnerContext {
    /// The handler state (durable tracking, council, catalog, config).
    pub api: ApiState,
}

impl BuildRunnerContext {
    async fn set_state(&self, build_id: u64, state: BuildState) {
        update_build_state(&self.api, build_id, state).await;
    }
}

/// The lifted build body: fetch the context blob from the local
/// registry, extract, run `buildah bud` + `buildah push` (bounded by
/// `[images] build_timeout_secs`), then sign the pushed manifest.
/// When `[images.trust_policy] require_signatures` is set, signing is
/// part of the terminal-state definition: no trusted signature, no
/// `Completed` (JOB7). Every failure lands in the tracker with a
/// reason; nothing is reported over HTTP from here.
pub async fn run_build(request: BuildSubmitRequest, build_id: u64, ctx: BuildRunnerContext) {
    let outcome = run_build_inner(&request, build_id, &ctx).await;
    match outcome {
        Ok(image) => {
            ctx.set_state(build_id, BuildState::Completed { image })
                .await;
        }
        Err(reason) => {
            eprintln!("bun: build {build_id} ({}) failed: {reason}", request.name);
            ctx.set_state(build_id, BuildState::Failed { reason }).await;
        }
    }
    ctx.api.active_builds.lock().await.remove(&build_id);
}

async fn run_build_inner(
    request: &BuildSubmitRequest,
    build_id: u64,
    ctx: &BuildRunnerContext,
) -> Result<String, String> {
    let state = &ctx.api;
    let timeout = std::time::Duration::from_secs(state.build_timeout_secs);

    // The registry destination is the node's own config, never the
    // request (JOB2): a caller cannot point this privileged process at
    // an arbitrary localhost service.
    let job = crate::pickle::build::execute_build(
        &request.spec,
        &request.context_digest,
        Some(state.registry_port),
    )
    .map_err(|e| format!("invalid build spec: {e}"))?;

    // A unique, self-cleaning build directory. `ScopedDir::drop` removes
    // it on every exit path below, so no failure leaves a stray context.
    let base = std::env::temp_dir().join("reliaburger-build");
    let build_dir = ScopedDir::new(&base, build_id)
        .map_err(|e| format!("failed to create build directory: {e}"))?;
    // The tar lands beside the context, not inside it, so `context.tar`
    // never becomes part of the build.
    let tar_path = build_dir.path().join("context.tar");
    let ctx_dir = build_dir.path().join("ctx");

    // Stream the context blob to disk with a hard byte cap (no whole-body
    // buffer), then extract through the hardened unpacker (bounds
    // size/entries, rejects traversal/symlinks, strips setuid) off the
    // async runtime.
    let context_url =
        crate::pickle::build::context_download_url(state.registry_port, &request.context_digest);
    let response = match reqwest::get(&context_url).await {
        Ok(response) if response.status().is_success() => response,
        _ => {
            return Err(format!(
                "context blob {} not found in the registry",
                request.context_digest
            ));
        }
    };
    match download_to_file_capped(response, &tar_path, state.max_context_bytes).await {
        Ok(()) => {}
        Err(DownloadError::TooLarge) => {
            return Err(format!(
                "build context exceeds the {} byte limit",
                state.max_context_bytes
            ));
        }
        Err(DownloadError::Io(e)) => return Err(format!("context read failed: {e}")),
    }

    let extract_path = ctx_dir.clone();
    let dockerfile = request.spec.dockerfile.clone();
    let max_context_bytes = state.max_context_bytes;
    let extracted = tokio::task::spawn_blocking(move || {
        let file = std::fs::File::open(&tar_path)?;
        crate::pickle::build::unpack_context(
            std::io::BufReader::new(file),
            &extract_path,
            max_context_bytes,
        )?;
        // Prove the Dockerfile resolves inside the extracted context
        // before Buildah ever reads it (JOB6).
        crate::pickle::build::confine_dockerfile(&extract_path, &dockerfile)?;
        Ok::<_, crate::pickle::build::BuildError>(())
    })
    .await;
    match extracted {
        Ok(Ok(())) => {}
        Ok(Err(e)) => return Err(format!("failed to prepare build context: {e}")),
        Err(e) => return Err(format!("context extraction task failed: {e}")),
    }

    // Build, then push. The push targets the local registry, so the
    // image lands in Pickle through the standard handlers (real
    // holders, catalog persistence, Raft propose — all for free).
    for (label, cmd) in [("build", &job.build_cmd), ("push", &job.push_cmd)] {
        let (program, args) = match cmd.split_first() {
            Some(pair) => pair,
            None => continue,
        };
        match run_bounded(program, args, &ctx_dir, timeout).await {
            Ok(out) if out.status.success() => {}
            Ok(out) => {
                let stderr = String::from_utf8_lossy(&out.stderr);
                let tail: String = stderr.lines().rev().take(10).collect::<Vec<_>>().join("\n");
                return Err(format!("buildah {label} failed: {tail}"));
            }
            Err(BoundedError::Timeout) => {
                return Err(format!(
                    "buildah {label} exceeded build_timeout_secs ({}s)",
                    timeout.as_secs()
                ));
            }
            Err(BoundedError::Spawn(e)) => {
                return Err(format!("failed to run buildah {label}: {e}"));
            }
        }
    }

    // Sign the pushed manifest so it deploys under require_signatures.
    // With the policy set, signing is part of success (JOB7); without
    // it, a failure is a warning and the unsigned image stays useful.
    let digest = pushed_manifest_digest(state, &job.destination.name, &job.destination.tag).await;
    let sign_outcome: Result<(), String> = match (&state.council, digest) {
        (Some(council), Some(digest)) => {
            let namespace = request.spec.namespace.as_deref().unwrap_or("default");
            let node_name = state.node_name.as_deref().unwrap_or("local");
            sign_pushed_image(council, &digest, namespace, &request.name, node_name).await
        }
        (Some(_), None) => Err("pushed image not found in the catalogue".to_string()),
        (None, _) => Err("no council available for signing".to_string()),
    };
    match sign_outcome {
        Ok(()) => {}
        Err(reason) if state.require_signatures => {
            return Err(format!(
                "image pushed but the trust policy requires signatures and signing failed: {reason}"
            ));
        }
        Err(reason) => {
            eprintln!(
                "bun: build {build_id}: image pushed but unsigned ({reason}); \
                 deploys need require_signatures = false"
            );
        }
    }

    Ok(format!("{}:{}", job.destination.name, job.destination.tag))
}

/// The manifest digest the push landed under, from the local catalog
/// (shared with the registry) or the replicated council catalog.
async fn pushed_manifest_digest(
    state: &ApiState,
    name: &str,
    tag: &str,
) -> Option<crate::pickle::types::Digest> {
    if let Some(catalog) = &state.pickle_catalog {
        let found = catalog
            .read()
            .await
            .get_manifest_by_tag(name, tag)
            .map(|manifest| manifest.digest.clone());
        if found.is_some() {
            return found;
        }
    }
    let council = state.council.as_ref()?;
    council
        .manifest_catalog()
        .await
        .get_manifest_by_tag(name, tag)
        .map(|manifest| manifest.digest.clone())
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

/// Parse a build request body, returning a 400 response on malformed
/// JSON or any unexpected field. `deny_unknown_fields` on the struct
/// means a smuggled `registry_port` (JOB2) is a 400, not a silent
/// no-op.
#[allow(clippy::result_large_err)]
fn parse_build_request(body: &str) -> Result<BuildSubmitRequest, Response> {
    serde_json::from_str(body).map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": format!("invalid build request: {e}") })),
        )
            .into_response()
    })
}

/// `POST /v1/build`: run locally when buildah is present, else
/// delegate to a peer that reported the capability (transferring the
/// context blob and retrying across candidates, D18/JOB5), else 503.
pub async fn build_submit_handler(
    State(state): State<ApiState>,
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    body: String,
) -> Response {
    // Submitting a build is a Deployer action (AUTH2).
    if let Err(resp) =
        crate::sesame::auth::authorize(auth.as_deref(), crate::sesame::types::ApiRole::Deployer)
    {
        return resp;
    }
    let request = match parse_build_request(&body) {
        Ok(request) => request,
        Err(resp) => return resp,
    };

    // A build's `pickle://ns/name` destination must target an existing
    // namespace (12b.2 T6). Desired-state namespaces are the source of
    // truth; `default` is always available. Without a council this node
    // is single-node, so only the destination syntax is enforced.
    if let Some(council) = &state.council {
        let mut existing: Vec<String> = council
            .desired_state()
            .await
            .namespaces
            .keys()
            .cloned()
            .collect();
        existing.push("default".to_string());
        if let Err(e) = crate::pickle::build::check_namespace_scope(&request.spec, &existing) {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({ "error": e.to_string() })),
            )
                .into_response();
        }
    }

    if local_buildah_available().await {
        return accept_build(&state, request).await;
    }

    // Delegate to a capable peer, trying each candidate in turn (D18).
    let candidates = match (&state.membership, &state.aggregated_rx) {
        (Some(membership), Some(aggregated_rx)) => {
            let members = membership.read().await.clone();
            find_peer_builders(
                &members,
                &aggregated_rx.borrow(),
                state.node_name.as_deref(),
                &state.cluster_http,
                state.registry_port,
            )
        }
        _ => Vec::new(),
    };
    if candidates.is_empty() {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({
                "error": "no builder available: buildah is not on this node's PATH \
                          and no peer reports the capability"
            })),
        )
            .into_response();
    }

    let mut last_error = String::new();
    for candidate in candidates {
        // The builder fetches the context from its *own* registry, so
        // the blob must be there before the run request lands (JOB5).
        if let Err(e) = transfer_context_to_builder(
            state.cluster_http.client(),
            state.registry_port,
            &candidate.registry_address,
            &request.context_digest,
            state.max_context_bytes,
        )
        .await
        {
            last_error = format!("context transfer to {} failed: {e}", candidate.api_url);
            eprintln!("bun: build delegation: {last_error}");
            continue;
        }

        let mut proxied = state
            .cluster_http
            .client()
            .post(format!("{}/v1/build/run", candidate.api_url))
            .json(&request);
        if let Some(token) = &state.service_token {
            proxied = proxied.bearer_auth(token);
        }
        let response = match proxied.send().await {
            Ok(response) if response.status().is_success() => response,
            Ok(response) => {
                last_error = format!(
                    "builder {} refused the build: {}",
                    candidate.api_url,
                    response.status()
                );
                eprintln!("bun: build delegation: {last_error}");
                continue;
            }
            Err(e) => {
                last_error = format!("builder {} unreachable: {e}", candidate.api_url);
                eprintln!("bun: build delegation: {last_error}");
                continue;
            }
        };
        let remote: serde_json::Value = match response.json().await {
            Ok(value) => value,
            Err(e) => {
                last_error = format!("builder {} response unreadable: {e}", candidate.api_url);
                continue;
            }
        };
        let Some(remote_id) = remote["build_id"].as_u64() else {
            last_error = format!("builder {} rejected the build: {remote}", candidate.api_url);
            continue;
        };

        // Track locally as delegated, so the client polls one node.
        let record = BuildRecord {
            name: request.name.clone(),
            runner_node: None,
            state: BuildState::Delegated {
                url: candidate.api_url.clone(),
                remote_id,
            },
            created_at_epoch_secs: epoch_now_secs(),
        };
        return match register_build_record(&state, record).await {
            Ok(build_id) => (
                StatusCode::ACCEPTED,
                Json(serde_json::json!({ "build_id": build_id })),
            )
                .into_response(),
            Err(e) => (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({
                    "error": format!("build accepted by {} (remote id {remote_id}) but \
                                      tracking it failed: {e}", candidate.api_url)
                })),
            )
                .into_response(),
        };
    }

    (
        StatusCode::BAD_GATEWAY,
        Json(serde_json::json!({
            "error": format!("all capable builders failed; last error: {last_error}")
        })),
    )
        .into_response()
}

/// `POST /v1/build/run`: the builder-node endpoint — always local.
pub async fn build_run_handler(
    State(state): State<ApiState>,
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    body: String,
) -> Response {
    let request = match parse_build_request(&body) {
        Ok(request) => request,
        Err(resp) => return resp,
    };
    // Reject a malformed context digest first (JOB2) — an unconditional shape
    // check with no side effects. This endpoint is reached by a delegating
    // peer, and the digest is later used to build a temp path; `Digest::new`
    // allows only `sha256:` + 64 hex, which cannot contain `.`/`/`.
    if let Err(e) = crate::pickle::types::Digest::new(&request.context_digest) {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": format!("invalid context digest: {e}") })),
        )
            .into_response();
    }
    // Node-to-node delegation only (JOB1): a build runs an arbitrary Buildah
    // context as a privileged process, so only a cluster node (system
    // principal) may drive it.
    if let Err(resp) = crate::sesame::auth::require_system(auth.as_deref()) {
        return resp;
    }
    if !local_buildah_available().await {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({
                "error": "buildah is not on this node's PATH"
            })),
        )
            .into_response();
    }
    accept_build(&state, request).await
}

/// `POST /v1/build/track`: leader-side durable tracking for builds
/// running on nodes whose council handle is a follower (node-to-node
/// only). Proposes the operation to Raft and answers with the outcome.
pub async fn build_track_handler(
    State(state): State<ApiState>,
    auth: Option<axum::Extension<crate::sesame::auth::AuthContext>>,
    Json(request): Json<BuildTrackRequest>,
) -> Response {
    if let Err(resp) = crate::sesame::auth::require_system(auth.as_deref()) {
        return resp;
    }
    let Some(council) = &state.council else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({ "error": "no council on this node" })),
        )
            .into_response();
    };
    let raft_request = match &request {
        BuildTrackRequest::Register { record } => {
            crate::council::types::RaftRequest::BuildRegister {
                build: record.clone(),
            }
        }
        BuildTrackRequest::Update { build_id, state } => {
            crate::council::types::RaftRequest::BuildUpdate {
                build_id: *build_id,
                state: state.clone(),
            }
        }
    };
    match council.write(raft_request).await {
        Ok(crate::council::types::CouncilResponse::BuildRegistered { build_id }) => {
            Json(serde_json::json!({ "build_id": build_id })).into_response()
        }
        Ok(crate::council::types::CouncilResponse::Refused { reason }) => (
            StatusCode::CONFLICT,
            Json(serde_json::json!({ "error": reason })),
        )
            .into_response(),
        Ok(_) => Json(serde_json::json!({ "recorded": true })).into_response(),
        Err(e) => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(serde_json::json!({ "error": format!("raft write failed: {e}") })),
        )
            .into_response(),
    }
}

/// Register + spawn a local build; answer 202 with the id.
async fn accept_build(state: &ApiState, request: BuildSubmitRequest) -> Response {
    // Validate the context digest before it is ever used to build a path
    // (JOB2): `context_digest` reaches `run_build` from a peer over
    // `/v1/build/run` and is interpolated into a temp directory. A digest
    // like `sha256:../../x` would escape the build sandbox and the tar
    // unpack would run as a privileged process. `Digest::new` enforces
    // `sha256:` + 64 hex, so `.`/`/` can never appear.
    if let Err(e) = crate::pickle::types::Digest::new(&request.context_digest) {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": format!("invalid context digest: {e}") })),
        )
            .into_response();
    }

    // Validate the spec synchronously so a bad destination or an
    // escaping Dockerfile path (JOB6) is a 400 here, not a build that
    // 202s and then fails asynchronously.
    if let Err(e) = crate::pickle::build::validate_build(&request.spec) {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": format!("invalid build spec: {e}") })),
        )
            .into_response();
    }

    let record = BuildRecord {
        name: request.name.clone(),
        runner_node: state.node_name.clone(),
        state: BuildState::Running,
        created_at_epoch_secs: epoch_now_secs(),
    };
    let build_id = match register_build_record(state, record).await {
        Ok(id) => id,
        Err(e) => {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({
                    "error": format!("cluster build tracker unavailable: {e}")
                })),
            )
                .into_response();
        }
    };
    // Mark the runner live before answering, so a status read can never
    // see a Running record with no live runner on this node.
    state.active_builds.lock().await.insert(build_id);
    tokio::spawn(run_build(
        request,
        build_id,
        BuildRunnerContext { api: state.clone() },
    ));
    (
        StatusCode::ACCEPTED,
        Json(serde_json::json!({ "build_id": build_id })),
    )
        .into_response()
}

/// `GET /v1/build/{id}`: local state, or a proxy read for delegated
/// builds. A `Running` record whose runner is this node but has no
/// live runner task means the node restarted mid-build: the record is
/// terminated honestly rather than reading `Running` forever (JOB4).
pub async fn build_status_handler(
    State(state): State<ApiState>,
    AxumPath(build_id): AxumPath<u64>,
) -> Response {
    let Some(record) = lookup_build(&state, build_id).await else {
        return (
            StatusCode::NOT_FOUND,
            Json(serde_json::json!({ "error": format!("build {build_id} not found") })),
        )
            .into_response();
    };

    match record.state {
        BuildState::Delegated { url, remote_id } => {
            let mut request = state
                .cluster_http
                .client()
                .get(format!("{url}/v1/build/{remote_id}"));
            if let Some(token) = &state.service_token {
                request = request.bearer_auth(token);
            }
            match request.send().await {
                Ok(response) => {
                    let status = StatusCode::from_u16(response.status().as_u16())
                        .unwrap_or(StatusCode::BAD_GATEWAY);
                    let body = response.bytes().await.unwrap_or_default();
                    (status, body).into_response()
                }
                Err(e) => (
                    StatusCode::BAD_GATEWAY,
                    Json(serde_json::json!({ "error": format!("builder unreachable: {e}") })),
                )
                    .into_response(),
            }
        }
        BuildState::Running
            if record.runner_node.is_some()
                && record.runner_node == state.node_name
                && !state.active_builds.lock().await.contains(&build_id) =>
        {
            let failed = BuildState::Failed {
                reason: "builder restarted mid-build".to_string(),
            };
            update_build_state(&state, build_id, failed.clone()).await;
            Json(serde_json::json!(failed)).into_response()
        }
        build_state => Json(serde_json::json!(build_state)).into_response(),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reporting::aggregator::AggregatedState;
    use crate::reporting::types::{ResourceUsage, StateReport};

    fn running_record(name: &str) -> BuildRecord {
        BuildRecord {
            name: name.to_string(),
            runner_node: Some("n1".to_string()),
            state: BuildState::Running,
            created_at_epoch_secs: 1_000_000,
        }
    }

    #[test]
    fn build_request_rejects_unknown_fields() {
        // A smuggled registry destination (JOB2) fails to deserialise.
        let body = r#"{
            "name": "app",
            "context_digest": "sha256:abc",
            "registry_port": 5050,
            "spec": { "context": ".", "destination": "pickle://app:v1" }
        }"#;
        assert!(serde_json::from_str::<BuildSubmitRequest>(body).is_err());
    }

    #[test]
    fn scoped_dir_removes_itself_on_drop() {
        let base = std::env::temp_dir().join("reliaburger-build-test");
        let path = {
            let dir = ScopedDir::new(&base, 42).unwrap();
            let path = dir.path().to_path_buf();
            assert!(path.is_dir());
            path
        };
        assert!(!path.exists(), "ScopedDir should delete itself on drop");
    }

    #[test]
    fn scoped_dir_gives_concurrent_builds_distinct_paths() {
        let base = std::env::temp_dir().join("reliaburger-build-test");
        let a = ScopedDir::new(&base, 7).unwrap();
        let b = ScopedDir::new(&base, 7).unwrap();
        assert_ne!(a.path(), b.path(), "same build id must not share a dir");
    }

    /// JOB6: a Buildah run that outlives its timeout is killed along with
    /// its children. A shell shim backgrounds a long `sleep` (a
    /// grandchild), records its pid, and waits; after `run_bounded`
    /// times out, that grandchild must be gone — proving the whole
    /// process group was signalled, not just the direct child.
    #[cfg(unix)]
    #[tokio::test]
    async fn run_bounded_kills_the_whole_process_group_on_timeout() {
        use std::time::Duration;

        let dir = tempfile::tempdir().unwrap();
        let pidfile = dir.path().join("grandchild.pid");
        let script = format!("sleep 300 & echo $! > {}; wait", pidfile.display());
        let args = vec!["-c".to_string(), script];

        let result = run_bounded("sh", &args, dir.path(), Duration::from_millis(300)).await;
        assert!(matches!(result, Err(BoundedError::Timeout)));

        // Read the grandchild pid the shim recorded.
        let mut child_pid = None;
        for _ in 0..50 {
            if let Ok(text) = std::fs::read_to_string(&pidfile)
                && let Ok(pid) = text.trim().parse::<i32>()
            {
                child_pid = Some(pid);
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let pid = child_pid.expect("shim never recorded its child pid");

        use nix::sys::signal::{Signal, kill};
        use nix::unistd::Pid;
        // Poll until the grandchild is gone (reaped after the group kill).
        // `kill` with `None` sends no signal, only an existence check.
        let mut gone = false;
        for _ in 0..100 {
            if kill(Pid::from_raw(pid), None).is_err() {
                gone = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        if !gone {
            let _ = kill(Pid::from_raw(-pid), Signal::SIGKILL);
        }
        assert!(gone, "grandchild sleep survived the process-group kill");
    }

    #[test]
    fn registry_tracks_lifecycle() {
        let mut registry = BuildRegistry::default();
        let id = registry.register(running_record("myapp"));
        assert!(matches!(
            registry.get(id).map(|r| r.state),
            Some(BuildState::Running)
        ));

        registry.set_state(
            id,
            BuildState::Completed {
                image: "myapp:v1".to_string(),
            },
        );
        assert!(matches!(
            registry.get(id).map(|r| r.state),
            Some(BuildState::Completed { .. })
        ));
        assert!(registry.get(id + 1).is_none());
    }

    #[test]
    fn build_state_serialises_with_status_tag() {
        let json = serde_json::to_value(BuildState::Completed {
            image: "myapp:v1".to_string(),
        })
        .unwrap();
        assert_eq!(json["status"], "completed");
        assert_eq!(json["image"], "myapp:v1");
    }

    #[test]
    fn durable_state_allocates_monotonic_ids() {
        let mut state = BuildDurableState::default();
        let a = state.register(running_record("a"));
        let b = state.register(running_record("b"));
        assert_eq!(a, 1);
        assert_eq!(b, 2);
        assert_eq!(state.next_build_id, 3);
    }

    #[test]
    fn durable_state_validates_transitions() {
        let mut state = BuildDurableState::default();
        let id = state.register(running_record("a"));

        // Running → Completed is fine.
        state
            .update(
                id,
                BuildState::Completed {
                    image: "a:v1".to_string(),
                },
            )
            .unwrap();
        // A duplicate terminal report of the same kind is idempotent.
        state
            .update(
                id,
                BuildState::Completed {
                    image: "a:v1".to_string(),
                },
            )
            .unwrap();
        // Completed → Failed is a forged/conflicting transition.
        let err = state
            .update(
                id,
                BuildState::Failed {
                    reason: "nope".to_string(),
                },
            )
            .unwrap_err();
        assert!(matches!(err, BuildUpdateError::IllegalTransition { .. }));
        // Unknown ids are an error, never a silent no-op.
        let err = state.update(999, BuildState::Running).unwrap_err();
        assert_eq!(err, BuildUpdateError::UnknownBuild { build_id: 999 });
    }

    #[test]
    fn delegated_records_reject_updates() {
        let mut state = BuildDurableState::default();
        let id = state.register(BuildRecord {
            name: "a".to_string(),
            runner_node: None,
            state: BuildState::Delegated {
                url: "http://10.0.0.2:9117".to_string(),
                remote_id: 4,
            },
            created_at_epoch_secs: 1_000_000,
        });
        let err = state
            .update(
                id,
                BuildState::Completed {
                    image: "a:v1".to_string(),
                },
            )
            .unwrap_err();
        assert!(matches!(err, BuildUpdateError::IllegalTransition { .. }));
    }

    #[test]
    fn registration_prunes_old_terminal_builds() {
        let mut state = BuildDurableState::default();
        let mut done = running_record("old");
        done.state = BuildState::Failed {
            reason: "old".to_string(),
        };
        done.created_at_epoch_secs = 1_000;
        let old_id = state.register(done);

        let mut in_flight = running_record("running");
        in_flight.created_at_epoch_secs = 1_000;
        let running_id = state.register(in_flight);

        let mut fresh = running_record("fresh");
        fresh.created_at_epoch_secs = 1_000 + TERMINAL_BUILD_RETENTION_SECS + 1;
        let fresh_id = state.register(fresh);

        assert!(state.get(old_id).is_none(), "terminal build pruned");
        assert!(state.get(running_id).is_some(), "in-flight build kept");
        assert!(state.get(fresh_id).is_some());
        assert_eq!(state.next_build_id, fresh_id + 1);
    }

    fn report(node: &crate::meat::NodeId, has_buildah: bool) -> StateReport {
        StateReport {
            node_id: node.clone(),
            timestamp: std::time::SystemTime::UNIX_EPOCH,
            running_apps: vec![],
            cached_specs: vec![],
            resource_usage: ResourceUsage::default(),
            event_log: vec![],
            has_buildah,
        }
    }

    #[test]
    fn peer_builder_selection_prefers_capable_non_self() {
        let builder = crate::meat::NodeId("builder".to_string());
        let plain = crate::meat::NodeId("plain".to_string());
        let mut aggregated = AggregatedState::default();
        aggregated
            .reports
            .insert(builder.clone(), report(&builder, true));
        aggregated
            .reports
            .insert(plain.clone(), report(&plain, false));

        let members = vec![
            super::super::api::NodeMembershipInfo {
                node_id: plain,
                address: std::net::SocketAddr::from(([10, 0, 0, 1], 9117)),
            },
            super::super::api::NodeMembershipInfo {
                node_id: builder.clone(),
                address: std::net::SocketAddr::from(([10, 0, 0, 2], 9117)),
            },
        ];

        let http = crate::cluster::ClusterHttp::plaintext();
        let builders = find_peer_builders(&members, &aggregated, Some("plain"), &http, 5050);
        assert_eq!(
            builders,
            vec![PeerBuilder {
                api_url: "http://10.0.0.2:9117".to_string(),
                // The registry address is membership host + this node's
                // configured registry port — never request data (D18).
                registry_address: "10.0.0.2:5050".to_string(),
            }]
        );

        // The capable node itself is excluded — "peer" means peer.
        let builders = find_peer_builders(&members, &aggregated, Some("builder"), &http, 5050);
        assert!(builders.is_empty());
    }
}
