//! Deploy operations: what a deploy worker asks the loop to do, and the
//! loop branch that does it.

use super::*;

/// The fast, `&mut self` steps a deploy needs the command loop to perform on
/// its behalf.
///
/// A deploy runs on its own spawned task so a slow image pull or a rolling
/// health wait can't wedge the command loop (DEP4/codex-M3). The task owns the
/// blocking grill I/O (create, start, init and health polling), but the
/// supervisor state machine stays authoritative on the loop: every state
/// transition and every mutation of supervisor/service-map/networking travels
/// back as one of these ops. Each carries a `oneshot` the loop replies on, so
/// the task drives the sequence while the loop applies it.
pub(super) enum DeployOp {
    /// A bounded probe completes off-loop; only the agent mutates health state.
    HealthProbeResult {
        instance_id: InstanceId,
        created_at: Instant,
        status: Result<crate::bun::health::HealthStatus, crate::bun::probe::ProbeError>,
    },
    /// Enforce the image trust policy on one image reference (an app's or an
    /// init container's); returns the digest-pinned image, if any.
    EnforceImageSignature {
        image: Option<String>,
        reply: oneshot::Sender<Result<Option<String>, String>>,
    },
    /// Admit the app kind before recording the deployed spec for the Brioche UI.
    /// Answers the spec the app's instances ran before, if the node knows it.
    StoreDeployedSpec {
        app_name: String,
        namespace: String,
        spec: Box<AppSpec>,
        reply: oneshot::Sender<Result<Option<AppSpec>, BunError>>,
    },
    /// Every owned instance id, including terminal instances awaiting cleanup.
    ListExistingOwned {
        app_name: String,
        namespace: String,
        reply: oneshot::Sender<Vec<InstanceId>>,
    },
    /// What a deploy that changes only this node's share of the same
    /// version does to the running replicas (#1111). Asked before the spec
    /// is stored.
    ShareChangeInPlace {
        app_name: String,
        namespace: String,
        spec: Box<AppSpec>,
        reply: oneshot::Sender<Option<super::scale_in_place::InPlaceChange>>,
    },
    /// Create Pending instances for the replicas a scale-up adds.
    AddAppReplicas {
        app_name: String,
        namespace: String,
        spec: Box<AppSpec>,
        count: u32,
        reply: oneshot::Sender<Result<Vec<InstanceId>, BunError>>,
    },
    /// Reserve and return the next rolling-redeploy generation counter.
    NextDeployGen {
        app_name: String,
        reply: oneshot::Sender<Result<u64, BunError>>,
    },
    /// Create supervisor-tracked instances for a fresh app deploy.
    SupervisorDeployApp {
        app_name: String,
        namespace: String,
        spec: Box<AppSpec>,
        reply: oneshot::Sender<Result<Vec<InstanceId>, BunError>>,
    },
    /// Register an app + firewall in the service map and sync its eBPF maps.
    RegisterServiceApp {
        app_name: String,
        namespace: String,
        port: u16,
        firewall: Option<Vec<String>>,
        reply: oneshot::Sender<Result<(), BunError>>,
    },
    /// Re-register the service and ingress route a completed stop released,
    /// before a redeploy rolls over the stopped replicas it kept.
    RestoreStoppedRouting {
        app_name: String,
        namespace: String,
        spec: Box<AppSpec>,
        reply: oneshot::Sender<Result<(), BunError>>,
    },
    /// Forget fresh instances that never left Pending, so a deploy that failed
    /// before touching the runtime leaves nothing for its retry to replace.
    AbandonUnstartedInstances {
        service: crate::onion::service_id::ServiceId,
        instance_ids: Vec<InstanceId>,
        reply: oneshot::Sender<()>,
    },
    /// Store an app's ingress config for the routing table.
    StoreIngress {
        app_name: String,
        namespace: String,
        ingress: Box<crate::config::app::IngressSpec>,
        reply: oneshot::Sender<()>,
    },
    /// Do the fast pre-create bookkeeping for a fresh instance: transition to
    /// Preparing, prepare its identity dir, its managed volumes, and build the
    /// OCI spec (fail closed on undecryptable secrets). The task then calls
    /// `grill.create` itself, off the loop.
    PrepareFreshInstance {
        instance_id: InstanceId,
        app_name: String,
        namespace: String,
        spec: Box<AppSpec>,
        reply: oneshot::Sender<Result<PreparedInstance, BunError>>,
    },
    /// Store the built OCI spec on the tracked instance (for restart re-drive).
    StoreOciSpec {
        instance_id: InstanceId,
        oci_spec: Box<crate::grill::oci::OciSpec>,
        reply: oneshot::Sender<()>,
    },
    /// Reserve an auxiliary identity before its runtime can be created.
    RegisterInitialiser {
        instance_id: InstanceId,
        index: usize,
        reply: oneshot::Sender<Result<InstanceId, BunError>>,
    },
    /// Remember the runtime generation an initialiser launched, for its
    /// parent's retirement to forget (#850).
    RecordInitialiserGeneration {
        instance_id: InstanceId,
        initialiser: InstanceId,
        generation: crate::grill::RuntimeGeneration,
        reply: oneshot::Sender<()>,
    },
    /// Release an initialiser only after confirmed runtime retirement.
    ForgetInitialiser {
        instance_id: InstanceId,
        initialiser: InstanceId,
        reply: oneshot::Sender<Result<(), BunError>>,
    },
    /// Reserve an instance's sidecar identities before the first is
    /// created, so its retirement finds every one (#1120).
    RegisterSidecars {
        instance_id: InstanceId,
        names: Vec<String>,
        reply: oneshot::Sender<Result<Vec<InstanceId>, BunError>>,
    },
    /// Program source and egress policy before create → program → start. On
    /// failure the caller stops the created container and fails the deploy.
    /// The caller retains the network reference first, off the loop.
    ApplyNetworkPreStart {
        instance_id: InstanceId,
        app_name: String,
        spec: Option<Box<AppSpec>>,
        cgroup_path: PathBuf,
        retained: Result<Option<crate::grill::runc_intent::NetworkReference>, BunError>,
        /// The allowlist the worker resolved, so the loop never waits on DNS.
        egress: Box<launch_evidence::EgressResolution>,
        reply: oneshot::Sender<Result<(), BunError>>,
    },
    /// Transition an instance to a new lifecycle state through the supervisor.
    TransitionState {
        instance_id: InstanceId,
        to: ContainerState,
        reply: oneshot::Sender<Result<(), BunError>>,
    },
    /// The runtime refused to launch an instance (#881): fail it, keeping
    /// the reason for status.
    RecordLaunchFailure {
        instance_id: InstanceId,
        reason: String,
        reply: oneshot::Sender<()>,
    },
    /// Post-start bookkeeping for a fresh instance: log forwarder, on-disk
    /// record, container IP, HealthWait(→Running), service-map backend and
    /// kernel networking.
    FinishFreshInstance {
        instance_id: InstanceId,
        app_name: String,
        namespace: String,
        evidence: Box<launch_evidence::LaunchEvidence>,
        reply: oneshot::Sender<Result<(), BunError>>,
    },
    /// Provision a workload identity (SPIFFE cert + OIDC JWT).
    ProvisionIdentity {
        app_name: String,
        namespace: String,
        instance_id: InstanceId,
        reply: oneshot::Sender<()>,
    },
    /// Claim replacement ownership before allocating identity or runtime resources.
    ReserveRollingInstance {
        instance_id: InstanceId,
        app_name: String,
        namespace: String,
        spec: Box<AppSpec>,
        reply: oneshot::Sender<Result<Option<u16>, BunError>>,
    },
    /// Fast pre-create bookkeeping for a rolling-redeploy instance: fail closed
    /// on undecryptable secrets, prepare its identity dir, build the OCI spec.
    PrepareRollingInstance {
        instance_id: InstanceId,
        app_name: String,
        namespace: String,
        spec: Box<AppSpec>,
        host_port: Option<u16>,
        reply: oneshot::Sender<Result<crate::grill::oci::OciSpec, BunError>>,
    },
    /// Persist a started replacement before health wait or traffic publication.
    RegisterRollingInstance {
        instance: Box<RollingInstance>,
        reply: oneshot::Sender<Result<(), BunError>>,
    },
    /// Keep a started replacement reachable through ordinary Stop after a failed cut-over.
    RetainRollingInstance {
        instance: Box<RollingInstance>,
        reply: oneshot::Sender<Result<(), BunError>>,
    },
    /// Forget the already-stopped old instances and register the healthy new
    /// ones: service map, health config, backends, kernel networking, ingress,
    /// history. Bookkeeping only — the deploy worker drains and stops the old
    /// instances off the command loop before sending this (M7).
    FinaliseRollingDeploy {
        app_name: String,
        namespace: String,
        spec: Box<AppSpec>,
        existing: Vec<InstanceId>,
        new_ids: Vec<InstanceId>,
        new_ports: std::collections::HashMap<InstanceId, Option<u16>>,
        new_ips: std::collections::HashMap<InstanceId, Option<std::net::Ipv4Addr>>,
        new_specs: std::collections::HashMap<InstanceId, crate::grill::oci::OciSpec>,
        now: Instant,
        reply: oneshot::Sender<Result<(), BunError>>,
    },
    /// Publish one freshly-healthy replacement as a routable backend (M7).
    ///
    /// Split out of `FinaliseRollingDeploy` so a rolling deploy can move
    /// traffic onto a replacement *before* retiring an old instance, which is
    /// what makes `max_unavailable = 0` mean anything.
    PublishNewBackend {
        app_name: String,
        namespace: String,
        new_id: InstanceId,
        host_port: Option<u16>,
        container_ip: Option<std::net::Ipv4Addr>,
        has_port: bool,
        reply: oneshot::Sender<Result<(), BunError>>,
    },
    /// Finish retiring one old instance: the fast `&mut self` bookkeeping
    /// (lift egress, clean identity, drop the record + supervisor entry) after
    /// the deploy worker has already drained and stopped it off the command
    /// loop (M7). The drain/stop wait used to run here on the loop, stalling
    /// every command for its duration per retired instance.
    FinishRetire {
        old_id: InstanceId,
        reply: oneshot::Sender<Result<(), BunError>>,
    },
    /// Fence restarts before the worker starts draining or signalling an old
    /// instance. A restart already in flight comes back for the worker to
    /// settle before it signals the runtime.
    BeginRetire {
        old_id: InstanceId,
        reply: oneshot::Sender<Result<super::retirement::BegunRetirement, BunError>>,
    },
    /// Ask the leader whether every node has dropped a retiring old
    /// instance's backend (#1355). Sent before the stop signal, while the
    /// instance still serves; the reply is the producer release's answer.
    AwaitRemoteWithdrawal {
        old_id: InstanceId,
        reply: oneshot::Sender<Result<(), BunError>>,
    },
    /// Hand a stopped old instance whose addresses still await remote
    /// withdrawal confirmations to the agent loop, so the rollout can finish.
    DeferRetire {
        old_id: InstanceId,
        reply: oneshot::Sender<()>,
    },
    /// Append an entry to the deploy history.
    PushDeployHistory {
        entry: Box<crate::meat::deploy_types::DeployHistoryEntry>,
        reply: oneshot::Sender<()>,
    },
    /// Rebuild the Wrapper routing table after all instances started.
    RebuildRoutingTable { reply: oneshot::Sender<()> },
    /// Record a per-app "deployed" lifecycle event.
    RecordDeployedEvent {
        app_name: String,
        namespace: String,
        reply: oneshot::Sender<()>,
    },
}

impl DeployOp {
    /// The variant's name, for the loop meter's slow-turn log.
    pub(super) fn name(&self) -> &'static str {
        match self {
            DeployOp::HealthProbeResult { .. } => "health_probe_result",
            DeployOp::EnforceImageSignature { .. } => "enforce_image_signature",
            DeployOp::StoreDeployedSpec { .. } => "store_deployed_spec",
            DeployOp::ListExistingOwned { .. } => "list_existing_owned",
            DeployOp::ShareChangeInPlace { .. } => "share_change_in_place",
            DeployOp::AddAppReplicas { .. } => "add_app_replicas",
            DeployOp::NextDeployGen { .. } => "next_deploy_gen",
            DeployOp::SupervisorDeployApp { .. } => "supervisor_deploy_app",
            DeployOp::RegisterServiceApp { .. } => "register_service_app",
            DeployOp::RestoreStoppedRouting { .. } => "restore_stopped_routing",
            DeployOp::AbandonUnstartedInstances { .. } => "abandon_unstarted_instances",
            DeployOp::StoreIngress { .. } => "store_ingress",
            DeployOp::PrepareFreshInstance { .. } => "prepare_fresh_instance",
            DeployOp::StoreOciSpec { .. } => "store_oci_spec",
            DeployOp::RegisterInitialiser { .. } => "register_initialiser",
            DeployOp::RecordInitialiserGeneration { .. } => "record_initialiser_generation",
            DeployOp::ForgetInitialiser { .. } => "forget_initialiser",
            DeployOp::RegisterSidecars { .. } => "register_sidecars",
            DeployOp::ApplyNetworkPreStart { .. } => "apply_network_pre_start",
            DeployOp::TransitionState { .. } => "transition_state",
            DeployOp::RecordLaunchFailure { .. } => "record_launch_failure",
            DeployOp::FinishFreshInstance { .. } => "finish_fresh_instance",
            DeployOp::ProvisionIdentity { .. } => "provision_identity",
            DeployOp::ReserveRollingInstance { .. } => "reserve_rolling_instance",
            DeployOp::PrepareRollingInstance { .. } => "prepare_rolling_instance",
            DeployOp::RegisterRollingInstance { .. } => "register_rolling_instance",
            DeployOp::RetainRollingInstance { .. } => "retain_rolling_instance",
            DeployOp::FinaliseRollingDeploy { .. } => "finalise_rolling_deploy",
            DeployOp::PublishNewBackend { .. } => "publish_new_backend",
            DeployOp::FinishRetire { .. } => "finish_retire",
            DeployOp::BeginRetire { .. } => "begin_retire",
            DeployOp::AwaitRemoteWithdrawal { .. } => "await_remote_withdrawal",
            DeployOp::DeferRetire { .. } => "defer_retire",
            DeployOp::PushDeployHistory { .. } => "push_deploy_history",
            DeployOp::RebuildRoutingTable { .. } => "rebuild_routing_table",
            DeployOp::RecordDeployedEvent { .. } => "record_deployed_event",
        }
    }
}

/// Launch data owned by the deploy worker before supervisor registration.
pub(super) struct RollingInstance {
    pub(super) instance_id: InstanceId,
    pub(super) app_name: String,
    pub(super) namespace: String,
    pub(super) spec: AppSpec,
    pub(super) oci_spec: crate::grill::oci::OciSpec,
    pub(super) host_port: Option<u16>,
    /// What the runtime reported once the replacement started.
    pub(super) launch: launch_evidence::LaunchEvidence,
}

/// How many deploy history entries a node keeps per app.
pub(crate) const DEPLOY_HISTORY_PER_APP: usize = 50;

/// Append a deploy history entry, dropping the app's oldest entries beyond
/// [`DEPLOY_HISTORY_PER_APP`].
///
/// Every deploy, retry and abort records one, each with a full spec, so an
/// app the reconciler retried for a week used to hold thousands of specs in
/// memory. Fifty is plenty to read a story from; `relish rollback` in a
/// cluster reads the council's rollback targets, not this.
pub(crate) fn record_deploy_history(
    history: &mut Vec<crate::meat::deploy_types::DeployHistoryEntry>,
    entry: crate::meat::deploy_types::DeployHistoryEntry,
) {
    let app_id = entry.app_id.clone();
    history.push(entry);
    let kept = history
        .iter()
        .filter(|entry| entry.app_id == app_id)
        .count();
    let mut excess = kept.saturating_sub(DEPLOY_HISTORY_PER_APP);
    history.retain(|entry| {
        if excess > 0 && entry.app_id == app_id {
            excess -= 1;
            return false;
        }
        true
    });
}

/// The fast pre-create outputs the loop hands back for a fresh instance.
pub(super) struct PreparedInstance {
    pub(super) oci_spec: crate::grill::oci::OciSpec,
    pub(super) cgroup_path: PathBuf,
    pub(super) has_init: bool,
}

/// How long a deploy worker keeps asking the leader to release a retired
/// instance's addresses before it hands the release to the agent loop and
/// carries on. Consumers confirm withdrawals on their placement poll, every
/// couple of seconds, so a healthy cluster answers well within it; a lost
/// node holds it up until the leader discharges it (`onion::lease`).
pub(super) const PRODUCER_RELEASE_PATIENCE: std::time::Duration = std::time::Duration::from_secs(5);

/// Pause between two producer release attempts.
pub(super) const PRODUCER_RELEASE_RETRY: std::time::Duration = std::time::Duration::from_secs(1);

/// Pause between two asks, before an old instance's stop, whether every node
/// has dropped its backend (#1355). Shorter than [`PRODUCER_RELEASE_RETRY`]:
/// the rollout waits on this one, so the answer is collected sooner.
pub(super) const REMOTE_WITHDRAWAL_RETRY: std::time::Duration =
    std::time::Duration::from_millis(500);

/// Run `attempt` until it stops reporting a pending producer release, or
/// until `patience` runs out; returns the last outcome either way.
pub(super) async fn retry_while_release_pending<F, Fut>(
    patience: std::time::Duration,
    interval: std::time::Duration,
    mut attempt: F,
) -> Result<(), BunError>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<(), BunError>>,
{
    let deadline = tokio::time::Instant::now() + patience;
    loop {
        match attempt().await {
            Err(BunError::ProducerReleasePending { .. })
                if tokio::time::Instant::now() + interval < deadline =>
            {
                tokio::time::sleep(interval).await;
            }
            outcome => return outcome,
        }
    }
}

/// How often a deploy worker asks again for a step whose disk work is still
/// running off the agent loop.
pub(super) const STILL_RUNNING_RECHECK: std::time::Duration = std::time::Duration::from_millis(100);

/// How long a deploy worker keeps asking before it reports the disk work as
/// stuck. A provisioning or cleanup task that runs this long has hung.
pub(super) const STILL_RUNNING_PATIENCE: std::time::Duration = std::time::Duration::from_secs(120);

/// Ask the loop for a step until the disk work it waits on has finished
/// (#351, stage 3). Each attempt is a short turn; the worker sleeps between
/// them, off the loop.
pub(super) async fn retry_while_still_running<T, F, Fut>(mut attempt: F) -> Result<T, BunError>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T, BunError>>,
{
    let deadline = tokio::time::Instant::now() + STILL_RUNNING_PATIENCE;
    loop {
        match attempt().await {
            Err(BunError::StillRunning { .. })
                if tokio::time::Instant::now() + STILL_RUNNING_RECHECK < deadline =>
            {
                tokio::time::sleep(STILL_RUNNING_RECHECK).await;
            }
            outcome => return outcome,
        }
    }
}

/// Answer a trust-policy question once any cosign check it owes has run.
///
/// The loop has already judged everything it can see locally (`verdict`).
/// A cosign check reads the `.sig` image over the network, so it runs on its
/// own task with a deadline, and the loop moves on; the deploy worker waiting
/// on `reply` is the only one that waits for it.
pub(super) fn answer_after_cosign(
    verdict: Result<Option<String>, String>,
    check: Option<crate::pickle::trust::CosignCheck>,
    reply: oneshot::Sender<Result<Option<String>, String>>,
) {
    let (Ok(pinned), Some(check)) = (&verdict, check) else {
        let _ = reply.send(verdict);
        return;
    };
    let pinned = pinned.clone();
    let image = check.image().to_string();
    tokio::spawn(async move {
        let outcome = match tokio::time::timeout(COSIGN_CHECK_TIMEOUT, check.run()).await {
            Ok(Ok(())) => Ok(pinned),
            Ok(Err(reason)) => Err(reason),
            Err(_) => Err(format!(
                "image {image}: the cosign signature check did not finish within {}s",
                COSIGN_CHECK_TIMEOUT.as_secs()
            )),
        };
        let _ = reply.send(outcome);
    });
}

/// How long a deploy waits for an image's cosign signature to be fetched
/// and verified before it refuses the image.
pub(super) const COSIGN_CHECK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// A handle a deploy task uses to ask the command loop to perform its
/// authoritative `&mut self` steps. Each method sends a `DeployOp` and awaits
/// the reply, so the loop stays the single owner of supervisor state.
#[derive(Clone)]
pub(super) struct DeployOps {
    pub(super) tx: mpsc::Sender<DeployOp>,
}

impl DeployOps {
    /// Send an op built by `make` (given the reply sender) and await its
    /// reply, falling back to `on_gone` if the loop has shut down (the task is
    /// tearing down anyway, so the value is never observed).
    pub(super) async fn call<T, F>(&self, make: F, on_gone: T) -> T
    where
        F: FnOnce(oneshot::Sender<T>) -> DeployOp,
    {
        let (reply, rx) = oneshot::channel();
        if self.tx.send(make(reply)).await.is_err() {
            return on_gone;
        }
        rx.await.unwrap_or(on_gone)
    }

    pub(super) async fn enforce_image_signature(
        &self,
        image: Option<&str>,
    ) -> Result<Option<String>, String> {
        self.call(
            |reply| DeployOp::EnforceImageSignature {
                image: image.map(String::from),
                reply,
            },
            Err("agent shutting down".to_string()),
        )
        .await
    }

    pub(super) async fn store_deployed_spec(
        &self,
        app_name: &str,
        namespace: &str,
        spec: &AppSpec,
    ) -> Result<Option<AppSpec>, BunError> {
        self.call(
            |reply| DeployOp::StoreDeployedSpec {
                app_name: app_name.to_string(),
                namespace: namespace.to_string(),
                spec: Box::new(spec.clone()),
                reply,
            },
            Err(BunError::DeployFailed {
                app_name: app_name.into(),
                reason: "agent shutting down".into(),
            }),
        )
        .await
    }

    pub(super) async fn share_change_in_place(
        &self,
        app_name: &str,
        namespace: &str,
        spec: &AppSpec,
    ) -> Option<super::scale_in_place::InPlaceChange> {
        self.call(
            |reply| DeployOp::ShareChangeInPlace {
                app_name: app_name.to_string(),
                namespace: namespace.to_string(),
                spec: Box::new(spec.clone()),
                reply,
            },
            None,
        )
        .await
    }

    pub(super) async fn add_app_replicas(
        &self,
        app_name: &str,
        namespace: &str,
        spec: &AppSpec,
        count: u32,
    ) -> Result<Vec<InstanceId>, BunError> {
        self.call(
            |reply| DeployOp::AddAppReplicas {
                app_name: app_name.to_string(),
                namespace: namespace.to_string(),
                spec: Box::new(spec.clone()),
                count,
                reply,
            },
            Err(BunError::DeployFailed {
                app_name: app_name.into(),
                reason: "agent shutting down".into(),
            }),
        )
        .await
    }

    pub(super) async fn list_existing_owned(
        &self,
        app_name: &str,
        namespace: &str,
    ) -> Vec<InstanceId> {
        self.call(
            |reply| DeployOp::ListExistingOwned {
                app_name: app_name.to_string(),
                namespace: namespace.to_string(),
                reply,
            },
            Vec::new(),
        )
        .await
    }

    pub(super) async fn next_deploy_gen(&self, app_name: &str) -> Result<u64, BunError> {
        self.call(
            |reply| DeployOp::NextDeployGen {
                app_name: app_name.into(),
                reply,
            },
            Err(BunError::DeployFailed {
                app_name: app_name.into(),
                reason: "agent loop closed before reserving rollout identity".into(),
            }),
        )
        .await
    }

    pub(super) async fn supervisor_deploy_app(
        &self,
        app_name: &str,
        namespace: &str,
        spec: &AppSpec,
    ) -> Result<Vec<InstanceId>, BunError> {
        self.call(
            |reply| DeployOp::SupervisorDeployApp {
                app_name: app_name.to_string(),
                namespace: namespace.to_string(),
                spec: Box::new(spec.clone()),
                reply,
            },
            Ok(Vec::new()),
        )
        .await
    }

    pub(super) async fn register_service_app(
        &self,
        app_name: &str,
        namespace: &str,
        port: u16,
        firewall: Option<Vec<String>>,
    ) -> Result<(), BunError> {
        // The loop answers `StillRunning` while the service's journal write
        // is in flight (#1263); ask again until it has landed.
        retry_while_still_running(|| {
            self.call(
                |reply| DeployOp::RegisterServiceApp {
                    app_name: app_name.to_string(),
                    namespace: namespace.to_string(),
                    port,
                    firewall: firewall.clone(),
                    reply,
                },
                Err(BunError::BackendPublication {
                    service: crate::onion::service_id::ServiceId::new(namespace, app_name),
                    reason: "agent loop closed before service registration".into(),
                }),
            )
        })
        .await
    }

    pub(super) async fn restore_stopped_routing(
        &self,
        app_name: &str,
        namespace: &str,
        spec: &AppSpec,
    ) -> Result<(), BunError> {
        retry_while_still_running(|| {
            self.call(
                |reply| DeployOp::RestoreStoppedRouting {
                    app_name: app_name.to_string(),
                    namespace: namespace.to_string(),
                    spec: Box::new(spec.clone()),
                    reply,
                },
                Err(BunError::BackendPublication {
                    service: crate::onion::service_id::ServiceId::new(namespace, app_name),
                    reason: "agent loop closed before service registration".into(),
                }),
            )
        })
        .await
    }

    pub(super) async fn abandon_unstarted_instances(
        &self,
        app_name: &str,
        namespace: &str,
        instance_ids: &[InstanceId],
    ) {
        self.call(
            |reply| DeployOp::AbandonUnstartedInstances {
                service: crate::onion::service_id::ServiceId::new(namespace, app_name),
                instance_ids: instance_ids.to_vec(),
                reply,
            },
            (),
        )
        .await
    }

    pub(super) async fn store_ingress(
        &self,
        app_name: &str,
        namespace: &str,
        ingress: &crate::config::app::IngressSpec,
    ) {
        self.call(
            |reply| DeployOp::StoreIngress {
                app_name: app_name.to_string(),
                namespace: namespace.to_string(),
                ingress: Box::new(ingress.clone()),
                reply,
            },
            (),
        )
        .await
    }

    pub(super) async fn prepare_fresh_instance(
        &self,
        instance_id: &InstanceId,
        app_name: &str,
        namespace: &str,
        spec: &AppSpec,
    ) -> Result<PreparedInstance, BunError> {
        retry_while_still_running(|| {
            self.call(
                |reply| DeployOp::PrepareFreshInstance {
                    instance_id: instance_id.clone(),
                    app_name: app_name.to_string(),
                    namespace: namespace.to_string(),
                    spec: Box::new(spec.clone()),
                    reply,
                },
                Err(BunError::InstanceNotFound {
                    instance_id: instance_id.clone(),
                }),
            )
        })
        .await
    }

    pub(super) async fn store_oci_spec(
        &self,
        instance_id: &InstanceId,
        oci_spec: crate::grill::oci::OciSpec,
    ) {
        self.call(
            |reply| DeployOp::StoreOciSpec {
                instance_id: instance_id.clone(),
                oci_spec: Box::new(oci_spec),
                reply,
            },
            (),
        )
        .await
    }

    pub(super) async fn register_initialiser(
        &self,
        instance_id: &InstanceId,
        index: usize,
    ) -> Result<InstanceId, BunError> {
        self.call(
            |reply| DeployOp::RegisterInitialiser {
                instance_id: instance_id.clone(),
                index,
                reply,
            },
            Err(BunError::InstanceNotFound {
                instance_id: instance_id.clone(),
            }),
        )
        .await
    }

    pub(super) async fn register_sidecars(
        &self,
        instance_id: &InstanceId,
        names: Vec<String>,
    ) -> Result<Vec<InstanceId>, BunError> {
        self.call(
            |reply| DeployOp::RegisterSidecars {
                instance_id: instance_id.clone(),
                names,
                reply,
            },
            Err(BunError::InstanceNotFound {
                instance_id: instance_id.clone(),
            }),
        )
        .await
    }

    pub(super) async fn record_initialiser_generation(
        &self,
        instance_id: &InstanceId,
        initialiser: &InstanceId,
        generation: crate::grill::RuntimeGeneration,
    ) {
        self.call(
            |reply| DeployOp::RecordInitialiserGeneration {
                instance_id: instance_id.clone(),
                initialiser: initialiser.clone(),
                generation,
                reply,
            },
            (),
        )
        .await
    }

    pub(super) async fn forget_initialiser(
        &self,
        instance_id: &InstanceId,
        initialiser: &InstanceId,
    ) -> Result<(), BunError> {
        self.call(
            |reply| DeployOp::ForgetInitialiser {
                instance_id: instance_id.clone(),
                initialiser: initialiser.clone(),
                reply,
            },
            Err(BunError::InstanceNotFound {
                instance_id: instance_id.clone(),
            }),
        )
        .await
    }

    pub(super) async fn apply_network_pre_start(
        &self,
        instance_id: &InstanceId,
        app_name: &str,
        spec: Option<&AppSpec>,
        cgroup_path: &std::path::Path,
        retained: Result<Option<crate::grill::runc_intent::NetworkReference>, BunError>,
        egress: launch_evidence::EgressResolution,
    ) -> Result<(), BunError> {
        self.call(
            |reply| DeployOp::ApplyNetworkPreStart {
                instance_id: instance_id.clone(),
                app_name: app_name.to_string(),
                spec: spec.cloned().map(Box::new),
                cgroup_path: cgroup_path.to_path_buf(),
                retained,
                egress: Box::new(egress),
                reply,
            },
            Err(BunError::InstanceNotFound {
                instance_id: instance_id.clone(),
            }),
        )
        .await
    }

    pub(super) async fn transition_state(
        &self,
        instance_id: &InstanceId,
        to: ContainerState,
    ) -> Result<(), BunError> {
        self.call(
            |reply| DeployOp::TransitionState {
                instance_id: instance_id.clone(),
                to,
                reply,
            },
            Ok(()),
        )
        .await
    }

    /// Fail an instance the runtime refused to launch, with the reason.
    pub(super) async fn record_launch_failure(&self, instance_id: &InstanceId, reason: String) {
        self.call(
            |reply| DeployOp::RecordLaunchFailure {
                instance_id: instance_id.clone(),
                reason,
                reply,
            },
            (),
        )
        .await
    }

    pub(super) async fn finish_fresh_instance(
        &self,
        instance_id: &InstanceId,
        app_name: &str,
        namespace: &str,
        evidence: launch_evidence::LaunchEvidence,
    ) -> Result<(), BunError> {
        // The loop answers `StillRunning` while the backend's journal write
        // is in flight (#1263); ask again until it has landed.
        retry_while_still_running(|| {
            self.call(
                |reply| DeployOp::FinishFreshInstance {
                    instance_id: instance_id.clone(),
                    app_name: app_name.to_string(),
                    namespace: namespace.to_string(),
                    evidence: Box::new(evidence.clone()),
                    reply,
                },
                Ok(()),
            )
        })
        .await
    }

    pub(super) async fn provision_identity(
        &self,
        app_name: &str,
        namespace: &str,
        instance_id: &InstanceId,
    ) {
        self.call(
            |reply| DeployOp::ProvisionIdentity {
                app_name: app_name.to_string(),
                namespace: namespace.to_string(),
                instance_id: instance_id.clone(),
                reply,
            },
            (),
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) async fn reserve_rolling_instance(
        &self,
        instance_id: &InstanceId,
        app_name: &str,
        namespace: &str,
        spec: &AppSpec,
    ) -> Result<Option<u16>, BunError> {
        self.call(
            |reply| DeployOp::ReserveRollingInstance {
                instance_id: instance_id.clone(),
                app_name: app_name.into(),
                namespace: namespace.into(),
                spec: Box::new(spec.clone()),
                reply,
            },
            Err(BunError::InstanceNotFound {
                instance_id: instance_id.clone(),
            }),
        )
        .await
    }

    pub(super) async fn prepare_rolling_instance(
        &self,
        instance_id: &InstanceId,
        app_name: &str,
        namespace: &str,
        spec: &AppSpec,
        host_port: Option<u16>,
    ) -> Result<crate::grill::oci::OciSpec, BunError> {
        retry_while_still_running(|| {
            self.call(
                |reply| DeployOp::PrepareRollingInstance {
                    instance_id: instance_id.clone(),
                    app_name: app_name.to_string(),
                    namespace: namespace.to_string(),
                    spec: Box::new(spec.clone()),
                    host_port,
                    reply,
                },
                Err(BunError::InstanceNotFound {
                    instance_id: instance_id.clone(),
                }),
            )
        })
        .await
    }

    pub(super) async fn register_rolling_instance(
        &self,
        instance: RollingInstance,
    ) -> Result<(), BunError> {
        let missing = BunError::InstanceNotFound {
            instance_id: instance.instance_id.clone(),
        };
        self.call(
            |reply| DeployOp::RegisterRollingInstance {
                instance: Box::new(instance),
                reply,
            },
            Err(missing),
        )
        .await
    }

    pub(super) async fn retain_rolling_instance(
        &self,
        instance: RollingInstance,
    ) -> Result<(), BunError> {
        let missing = BunError::InstanceNotFound {
            instance_id: instance.instance_id.clone(),
        };
        self.call(
            |reply| DeployOp::RetainRollingInstance {
                instance: Box::new(instance),
                reply,
            },
            Err(missing),
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) async fn finalise_rolling_deploy(
        &self,
        app_name: &str,
        namespace: &str,
        spec: &AppSpec,
        existing: Vec<InstanceId>,
        new_ids: Vec<InstanceId>,
        new_ports: std::collections::HashMap<InstanceId, Option<u16>>,
        new_ips: std::collections::HashMap<InstanceId, Option<std::net::Ipv4Addr>>,
        new_specs: std::collections::HashMap<InstanceId, crate::grill::oci::OciSpec>,
        now: Instant,
    ) -> Result<(), BunError> {
        self.call(
            |reply| DeployOp::FinaliseRollingDeploy {
                app_name: app_name.to_string(),
                namespace: namespace.to_string(),
                spec: Box::new(spec.clone()),
                existing,
                new_ids,
                new_ports,
                new_ips,
                new_specs,
                now,
                reply,
            },
            Err(BunError::DeployFailed {
                app_name: app_name.into(),
                reason: "agent loop closed before finalisation".into(),
            }),
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) async fn publish_new_backend(
        &self,
        app_name: &str,
        namespace: &str,
        new_id: &InstanceId,
        host_port: Option<u16>,
        container_ip: Option<std::net::Ipv4Addr>,
        has_port: bool,
    ) -> Result<(), BunError> {
        // The loop answers `StillRunning` while the backend's journal write
        // is in flight (#1263); ask again until it has landed.
        retry_while_still_running(|| {
            self.call(
                |reply| DeployOp::PublishNewBackend {
                    app_name: app_name.to_string(),
                    namespace: namespace.to_string(),
                    new_id: new_id.clone(),
                    host_port,
                    container_ip,
                    has_port,
                    reply,
                },
                Err(BunError::BackendPublication {
                    service: crate::onion::service_id::ServiceId::new(namespace, app_name),
                    reason: "agent loop closed before backend publication".into(),
                }),
            )
        })
        .await
    }

    pub(super) async fn begin_retire(
        &self,
        old_id: &InstanceId,
    ) -> Result<super::retirement::BegunRetirement, BunError> {
        self.call(
            |reply| DeployOp::BeginRetire {
                old_id: old_id.clone(),
                reply,
            },
            Err(BunError::RetirementState {
                instance_id: old_id.clone(),
                reason: "agent loop closed before retirement began".into(),
            }),
        )
        .await
    }

    /// Bookkeeping-only op sent after the worker has already drained+stopped
    /// the instance off the loop (M7).
    ///
    /// On a multi-node cluster the leader answers the first producer release
    /// with "pending" until every node confirms the old endpoint's
    /// withdrawal, which takes a placement poll or two. That's the normal
    /// case, not a failure, so the worker asks again for a while instead of
    /// failing the deploy (which would start yet another generation of
    /// replacements). The loop stays free between attempts, so this node can
    /// deliver its own receipt meanwhile.
    pub(super) async fn finish_retire(&self, old_id: &InstanceId) -> Result<(), BunError> {
        retry_while_still_running(|| {
            retry_while_release_pending(PRODUCER_RELEASE_PATIENCE, PRODUCER_RELEASE_RETRY, || {
                self.call(
                    |reply| DeployOp::FinishRetire {
                        old_id: old_id.clone(),
                        reply,
                    },
                    Err(BunError::RetirementState {
                        instance_id: old_id.clone(),
                        reason: "agent loop closed before retirement".into(),
                    }),
                )
            })
        })
        .await
    }

    /// Wait, before an old instance's stop, until the leader confirms that
    /// every node has dropped its backend, for at most
    /// [`PRODUCER_RELEASE_PATIENCE`] (#1355).
    ///
    /// Other nodes connect to the backend directly and drop it only when
    /// their next view arrives. Stopping before then refuses their callers'
    /// connections. The question is the producer release's, asked early:
    /// the leader fences the execution, takes it out of the catalogue and
    /// answers "confirmed" once every consumer's withdrawal receipt is in.
    /// The wait is best effort. A node that never answers, an uncertain
    /// ownership or an unreachable leader costs at most the patience, and
    /// the stop goes ahead; the release after the stop still holds the
    /// addresses until the leader confirms.
    pub(super) async fn await_remote_withdrawal(&self, old_id: &InstanceId) {
        let outcome =
            retry_while_release_pending(PRODUCER_RELEASE_PATIENCE, REMOTE_WITHDRAWAL_RETRY, || {
                self.call(
                    |reply| DeployOp::AwaitRemoteWithdrawal {
                        old_id: old_id.clone(),
                        reply,
                    },
                    Ok(()),
                )
            })
            .await;
        if let Err(error) = outcome {
            eprintln!(
                "bun: stopping {} before every node confirmed it is withdrawn: {error}",
                old_id.0
            );
        }
    }

    /// Let the agent loop finish releasing a stopped old instance's addresses
    /// once every node has confirmed the withdrawal.
    pub(super) async fn defer_retire(&self, old_id: &InstanceId) {
        self.call(
            |reply| DeployOp::DeferRetire {
                old_id: old_id.clone(),
                reply,
            },
            (),
        )
        .await
    }

    pub(super) async fn push_deploy_history(
        &self,
        entry: crate::meat::deploy_types::DeployHistoryEntry,
    ) {
        self.call(
            |reply| DeployOp::PushDeployHistory {
                entry: Box::new(entry),
                reply,
            },
            (),
        )
        .await
    }

    pub(super) async fn rebuild_routing_table(&self) {
        self.call(|reply| DeployOp::RebuildRoutingTable { reply }, ())
            .await
    }

    pub(super) async fn record_deployed_event(&self, app_name: &str, namespace: &str) {
        self.call(
            |reply| DeployOp::RecordDeployedEvent {
                app_name: app_name.to_string(),
                namespace: namespace.to_string(),
                reply,
            },
            (),
        )
        .await
    }
}

impl<G: Grill + Clone + 'static> BunAgent<G> {
    /// Remember what an app admission on this node says about capacity, so
    /// the next state report tells the leader (#681, #967).
    ///
    /// Every path that leases app replicas from the execution budget comes
    /// through here: a fresh deploy, replicas added beside running ones and
    /// a rolling or blue-green replacement. `ordinals` are the replicas the
    /// admission tried to start: the leader moves those, and only those,
    /// off this node (#971). A refusal is kept until an admission of the
    /// same app succeeds; any other failure says nothing about capacity, so
    /// it leaves the record as it was.
    fn note_admission<T>(
        &mut self,
        app_name: &str,
        namespace: &str,
        mut ordinals: Vec<u32>,
        result: &Result<T, BunError>,
    ) {
        let key = (app_name.to_string(), namespace.to_string());
        match result {
            Err(BunError::Capacity { .. }) => {
                ordinals.sort_unstable();
                ordinals.dedup();
                self.capacity_refusals.insert(key, ordinals);
            }
            Ok(_) => {
                self.capacity_refusals.remove(&key);
            }
            Err(_) => {}
        }
    }

    /// Apply one deploy op from a spawned deploy task. This is where the
    /// supervisor state machine stays authoritative: the task owns the
    /// blocking grill I/O, but every state transition and every mutation of
    /// supervisor / service-map / networking state happens here, on the loop
    /// (DEP4/codex-M3).
    pub(super) async fn handle_deploy_op(&mut self, op: DeployOp) {
        match op {
            DeployOp::HealthProbeResult {
                instance_id,
                created_at,
                status,
            } => {
                self.complete_health_probe(instance_id, created_at, status)
                    .await;
            }
            DeployOp::EnforceImageSignature { image, reply } => {
                let verdict = self.enforce_image_signature(image.as_deref()).await;
                let check = self.cosign_check(image.as_deref()).await;
                answer_after_cosign(verdict, check, reply);
            }
            DeployOp::StoreDeployedSpec {
                app_name,
                namespace,
                spec,
                reply,
            } => {
                let adopted = self.adopted_spec(&app_name, &namespace);
                self.forget_adopted_app(&app_name, &namespace);
                let previous = self
                    .deployed_specs
                    .insert((app_name, namespace), *spec)
                    .or(adopted);
                let _ = reply.send(Ok(previous));
            }
            DeployOp::ListExistingOwned {
                app_name,
                namespace,
                reply,
            } => {
                let ids = self
                    .supervisor
                    .list_instances()
                    .iter()
                    .filter(|i| i.app_name == app_name && i.namespace == namespace)
                    // Retired by an earlier rollout; only its release remains.
                    .filter(|i| !self.deferred_retirements.contains(&i.id))
                    .map(|i| i.id.clone())
                    .collect();
                let _ = reply.send(ids);
            }
            DeployOp::ShareChangeInPlace {
                app_name,
                namespace,
                spec,
                reply,
            } => {
                let _ = reply.send(self.share_change_in_place(&app_name, &namespace, &spec));
            }
            DeployOp::AddAppReplicas {
                app_name,
                namespace,
                spec,
                count,
                reply,
            } => {
                let adding = self
                    .supervisor
                    .ordinals_to_add(&app_name, &namespace, &spec, count);
                // LOOP-INLINE: in-memory lock, no I/O
                let result = self
                    .supervisor
                    .add_app_replicas(&app_name, &namespace, &spec, count, Instant::now())
                    .await;
                self.note_admission(&app_name, &namespace, adding, &result);
                let _ = reply.send(result);
            }
            DeployOp::NextDeployGen { app_name, reply } => {
                // Adoption restores owners, not the previous process's counter.
                // Use the structured app name to distinguish an ordinary app
                // named `worker-g9` from generation 9 of an app named `worker`.
                let highest_owned = self
                    .supervisor
                    .list_instances()
                    .iter()
                    .filter_map(|instance| {
                        let prefix = format!("{}__{}-g", instance.namespace, instance.app_name);
                        let suffix = instance.id.0.strip_prefix(&prefix)?;
                        let (generation, _) = suffix.split_once('-')?;
                        generation.parse::<u64>().ok()
                    })
                    .max()
                    .unwrap_or(0);
                let next = highest_owned
                    .checked_add(1)
                    .and_then(|after_owned| self.next_deploy_gen.max(after_owned).checked_add(1));
                let result = match next {
                    Some(next) => {
                        self.next_deploy_gen = next;
                        Ok(next - 1)
                    }
                    None => Err(BunError::DeployFailed {
                        app_name,
                        reason: "rollout generation exhausted; ownership preserved".into(),
                    }),
                };
                let _ = reply.send(result);
            }
            DeployOp::SupervisorDeployApp {
                app_name,
                namespace,
                spec,
                reply,
            } => {
                let now = Instant::now();
                // LOOP-INLINE: in-memory lock, no I/O
                let result = self
                    .supervisor
                    .deploy_app(&app_name, &namespace, &spec, now)
                    .await;
                self.note_admission(&app_name, &namespace, spec.replica_ordinals(), &result);
                let _ = reply.send(result);
            }
            DeployOp::RegisterServiceApp {
                app_name,
                namespace,
                port,
                firewall,
                reply,
            } => {
                let service_id = crate::onion::service_id::ServiceId::new(&namespace, &app_name);
                let result = self
                    .register_and_publish_service(
                        &service_id,
                        port,
                        firewall,
                        "register_service_app/landed",
                    )
                    .await;
                let _ = reply.send(result);
            }
            DeployOp::RestoreStoppedRouting {
                app_name,
                namespace,
                spec,
                reply,
            } => {
                let result = self
                    .restore_stopped_routing(&app_name, &namespace, &spec)
                    .await;
                let _ = reply.send(result);
            }
            DeployOp::AbandonUnstartedInstances {
                service,
                instance_ids,
                reply,
            } => {
                // A reservation that outlived a later publication step can only
                // be re-registered by a rollout, which needs these owners.
                if self.service_map.resolve(&service).is_none() {
                    for id in &instance_ids {
                        // Anything past Pending may own runtime artifacts, which
                        // only the retirement path can prove released.
                        if self
                            .supervisor
                            .get_instance(id)
                            .is_some_and(|instance| instance.state == ContainerState::Pending)
                        {
                            // LOOP-INLINE: in-memory lock, no I/O
                            self.supervisor.retire_instance(id).await;
                        }
                    }
                }
                let _ = reply.send(());
            }
            DeployOp::StoreIngress {
                app_name,
                namespace,
                ingress,
                reply,
            } => {
                self.ingress_configs.insert((namespace, app_name), *ingress);
                let _ = reply.send(());
            }
            DeployOp::PrepareFreshInstance {
                instance_id,
                app_name,
                namespace,
                spec,
                reply,
            } => {
                let result = self
                    .prepare_fresh_instance(&instance_id, &app_name, &namespace, &spec)
                    .await;
                let _ = reply.send(result);
            }
            DeployOp::StoreOciSpec {
                instance_id,
                oci_spec,
                reply,
            } => {
                if let Some(instance) = self.supervisor.get_instance_mut(&instance_id) {
                    instance.oci_spec = Some(*oci_spec);
                }
                let _ = reply.send(());
            }
            DeployOp::RegisterInitialiser {
                instance_id,
                index,
                reply,
            } => {
                let result = match self.supervisor.get_instance(&instance_id) {
                    Some(instance) if instance.state == ContainerState::Initialising => {
                        // DNS workload labels cannot contain this auxiliary separator.
                        let initialiser = InstanceId(format!("{}__init-{index}", instance_id.0));
                        if self
                            .initialisers
                            .entry(instance_id.clone())
                            .or_default()
                            .insert(initialiser.clone())
                        {
                            Ok(initialiser)
                        } else {
                            Err(BunError::RetirementState {
                                instance_id,
                                reason: "initialiser still owns its previous runtime".into(),
                            })
                        }
                    }
                    _ => Err(BunError::InstanceNotFound { instance_id }),
                };
                let _ = reply.send(result);
            }
            DeployOp::RecordInitialiserGeneration {
                instance_id,
                initialiser,
                generation,
                reply,
            } => {
                // Only the parent that still owns the initialiser records it.
                if self
                    .initialisers
                    .get(&instance_id)
                    .is_some_and(|children| children.contains(&initialiser))
                {
                    self.initialiser_generations
                        .entry(instance_id)
                        .or_default()
                        .insert(initialiser, generation);
                }
                let _ = reply.send(());
            }
            DeployOp::ForgetInitialiser {
                instance_id,
                initialiser,
                reply,
            } => {
                let result = if self
                    .initialisers
                    .get_mut(&instance_id)
                    .is_some_and(|children| children.remove(&initialiser))
                {
                    if self
                        .initialisers
                        .get(&instance_id)
                        .is_some_and(|children| children.is_empty())
                    {
                        self.initialisers.remove(&instance_id);
                    }
                    Ok(())
                } else {
                    Err(BunError::RetirementState {
                        instance_id,
                        reason: "initialiser ownership changed before confirmation".into(),
                    })
                };
                let _ = reply.send(result);
            }
            DeployOp::RegisterSidecars {
                instance_id,
                names,
                reply,
            } => {
                let _ = reply.send(self.register_sidecars(&instance_id, &names));
            }
            DeployOp::ApplyNetworkPreStart {
                instance_id,
                app_name,
                spec,
                cgroup_path,
                retained,
                egress,
                reply,
            } => {
                let result = self
                    .apply_network_pre_start(
                        &instance_id,
                        &app_name,
                        spec.as_deref(),
                        &cgroup_path,
                        retained,
                        *egress,
                    )
                    .await;
                // On failure, mark the instance Failed. The worker stops the
                // created container, off the loop, so no half-started
                // workload lingers. A start whose network reference is still
                // being journalled isn't failed: the worker asks again (#1266).
                if result
                    .as_ref()
                    .is_err_and(|error| !matches!(error, BunError::StillRunning { .. }))
                    && let Some(instance) = self.supervisor.get_instance_mut(&instance_id)
                    && let Ok(state) = instance.state.transition_to(ContainerState::Failed)
                {
                    instance.state = state;
                }
                let _ = reply.send(result);
            }
            DeployOp::TransitionState {
                instance_id,
                to,
                reply,
            } => {
                let result = self.transition_deploy_state(&instance_id, to).await;
                let _ = reply.send(result);
            }
            DeployOp::RecordLaunchFailure {
                instance_id,
                reason,
                reply,
            } => {
                self.record_launch_failure(&instance_id, reason).await;
                let _ = reply.send(());
            }
            DeployOp::FinishFreshInstance {
                instance_id,
                app_name,
                namespace,
                evidence,
                reply,
            } => {
                let result = self
                    .finish_fresh_instance(&instance_id, &app_name, &namespace, &evidence)
                    .await;
                let _ = reply.send(result);
            }
            DeployOp::ProvisionIdentity {
                app_name,
                namespace,
                instance_id,
                reply,
            } => {
                // A no-op in standalone mode; a failure here is retried by the
                // rotation loop rather than failing the deploy. The CSR runs
                // off the loop, and the worker is answered when it finishes.
                self.begin_identity_provision(&app_name, &namespace, &instance_id, Some(reply));
            }
            DeployOp::ReserveRollingInstance {
                instance_id,
                app_name,
                namespace,
                spec,
                reply,
            } => {
                let result = self
                    .reserve_rolling_instance(&instance_id, &app_name, &namespace, &spec)
                    .await;
                // A rollout that can't start one replacement rolls back, so
                // none of the new spec's replicas can run here.
                self.note_admission(&app_name, &namespace, spec.replica_ordinals(), &result);
                let _ = reply.send(result);
            }
            DeployOp::PrepareRollingInstance {
                instance_id,
                app_name,
                namespace,
                spec,
                host_port,
                reply,
            } => {
                let result = self
                    .prepare_rolling_instance(&instance_id, &app_name, &namespace, &spec, host_port)
                    .await;
                let _ = reply.send(result);
            }
            DeployOp::RegisterRollingInstance { instance, reply } => {
                let result = match self
                    .record_launch_execution(&instance.instance_id, &instance.launch.execution)
                {
                    Ok(()) => self.persist_rolling_instance(&instance).await,
                    Err(error) => Err(error),
                };
                if result.is_ok() {
                    self.spawn_log_forwarder(
                        &instance.instance_id,
                        &instance.app_name,
                        &instance.namespace,
                    );
                }
                let _ = reply.send(result);
            }
            DeployOp::RetainRollingInstance { instance, reply } => {
                let id = instance.instance_id.clone();
                let container_ip = instance.launch.container_ip;
                let result = match self.supervisor.get_instance_mut(&id) {
                    Some(owner)
                        if owner.app_name == instance.app_name
                            && owner.namespace == instance.namespace =>
                    {
                        owner.state = ContainerState::Running;
                        owner.container_ip = container_ip;
                        owner.retry_pending = false;
                        owner.oci_spec = Some(instance.oci_spec);
                        let health_config =
                            crate::bun::health::HealthCheckConfig::for_app(&instance.spec);
                        owner.health_config = health_config.clone();
                        if let Some(config) = health_config {
                            self.supervisor
                                .register_health(id.clone(), config, Instant::now());
                        }
                        Ok(())
                    }
                    _ => Err(BunError::InstanceNotFound { instance_id: id }),
                };
                let _ = reply.send(result);
            }
            DeployOp::FinaliseRollingDeploy {
                app_name,
                namespace,
                spec,
                existing,
                new_ids,
                new_ports,
                new_ips,
                new_specs,
                now,
                reply,
            } => {
                let result = self
                    .finalise_rolling_deploy(
                        &app_name, &namespace, &spec, &existing, &new_ids, &new_ports, &new_ips,
                        new_specs, now,
                    )
                    .await;
                let _ = reply.send(result);
            }
            DeployOp::PublishNewBackend {
                app_name,
                namespace,
                new_id,
                host_port,
                container_ip,
                has_port,
                reply,
            } => {
                let result = self
                    .publish_new_backend(
                        &app_name,
                        &namespace,
                        &new_id,
                        host_port,
                        container_ip,
                        has_port,
                    )
                    .await;
                let _ = reply.send(result);
            }
            DeployOp::BeginRetire { old_id, reply } => {
                let result = self.begin_instance_retirement(&old_id).await;
                let _ = reply.send(result);
            }
            DeployOp::FinishRetire { old_id, reply } => {
                let result = self.finish_retire_bookkeeping(&old_id).await;
                let _ = reply.send(result);
            }
            DeployOp::AwaitRemoteWithdrawal { old_id, reply } => {
                let result = self.confirm_producer_release(&old_id).await.map(|_| ());
                let _ = reply.send(result);
            }
            DeployOp::DeferRetire { old_id, reply } => {
                self.defer_retirement(&old_id);
                let _ = reply.send(());
            }
            DeployOp::PushDeployHistory { entry, reply } => {
                record_deploy_history(&mut *self.deploy_history.write().await, *entry);
                let _ = reply.send(());
            }
            DeployOp::RebuildRoutingTable { reply } => {
                self.rebuild_routing_table().await;
                let _ = reply.send(());
            }
            DeployOp::RecordDeployedEvent {
                app_name,
                namespace,
                reply,
            } => {
                let count = self
                    .supervisor
                    .list_instances()
                    .iter()
                    .filter(|i| i.app_name == app_name && i.namespace == namespace)
                    .count();
                self.record_event(
                    crate::bun::events::EventKind::Deploy,
                    crate::bun::events::EventSeverity::Info,
                    Some(app_name.clone()),
                    Some(namespace),
                    format!("deployed app {app_name} ({count} instances)"),
                )
                .await;
                let _ = reply.send(());
            }
        }
    }
}

#[cfg(test)]
mod history_tests {
    use super::{DEPLOY_HISTORY_PER_APP, record_deploy_history};
    use crate::meat::deploy_types::{DeployHistoryEntry, DeployId, DeployResult};

    fn entry(app: &str, id: u64) -> DeployHistoryEntry {
        DeployHistoryEntry {
            id: DeployId(id),
            app_id: crate::meat::types::AppId::new(app, "default"),
            image: format!("{app}:{id}"),
            result: DeployResult::Completed,
            created_at: std::time::SystemTime::UNIX_EPOCH,
            completed_at: std::time::SystemTime::UNIX_EPOCH,
            steps_completed: 1,
            steps_total: 1,
            spec: None,
        }
    }

    #[test]
    fn deploy_history_keeps_the_newest_fifty_entries_per_app() {
        let mut history = Vec::new();
        record_deploy_history(&mut history, entry("api", 0));
        for id in 0..120 {
            record_deploy_history(&mut history, entry("web", id));
        }
        let web: Vec<u64> = history
            .iter()
            .filter(|entry| entry.app_id.name == "web")
            .map(|entry| entry.id.0)
            .collect();
        assert_eq!(web.len(), DEPLOY_HISTORY_PER_APP);
        assert_eq!(web.first(), Some(&70), "the oldest entries go first");
        assert_eq!(web.last(), Some(&119));
        assert!(
            history.iter().any(|entry| entry.app_id.name == "api"),
            "one app's deploys never push out another app's history"
        );
    }
}
