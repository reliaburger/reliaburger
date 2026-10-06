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
    PreparedBatchJob {
        name: String,
        namespace: String,
        generation: u64,
        spec: Box<JobSpec>,
        reply: oneshot::Sender<Result<Vec<InstanceId>, BunError>>,
    },
    /// A prerequisite's observed success must be durable before its dependent app runs.
    EnforceImageReference {
        image: Option<String>,
        reply: oneshot::Sender<Result<Option<String>, String>>,
    },
    ConfirmJobSuccess {
        instance_id: InstanceId,
        code: i32,
        reply: oneshot::Sender<Result<(), BunError>>,
    },
    /// A bounded probe completes off-loop; only the agent mutates health state.
    HealthProbeResult {
        instance_id: InstanceId,
        created_at: Instant,
        status: Result<crate::bun::health::HealthStatus, crate::bun::probe::ProbeError>,
    },
    /// Enforce the image trust policy; returns the digest-pinned image, if any.
    EnforceImageSignature {
        spec: Box<AppSpec>,
        reply: oneshot::Sender<Result<Option<String>, String>>,
    },
    /// Admit the app kind before recording the deployed spec for the Brioche UI.
    StoreDeployedSpec {
        app_name: String,
        namespace: String,
        spec: Box<AppSpec>,
        reply: oneshot::Sender<Result<(), BunError>>,
    },
    /// Every owned instance id, including terminal instances awaiting cleanup.
    ListExistingOwned {
        app_name: String,
        namespace: String,
        reply: oneshot::Sender<Vec<InstanceId>>,
    },
    /// How many replicas a deploy adds beside the running ones, when it
    /// only raises the replica count. Asked before the spec is stored.
    ReplicasToAddInPlace {
        app_name: String,
        namespace: String,
        spec: Box<AppSpec>,
        reply: oneshot::Sender<Option<u32>>,
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
    /// Create supervisor-tracked instances for a job deploy.
    SupervisorDeployJob {
        rerun_unknown: bool,
        job_name: String,
        namespace: String,
        spec: Box<JobSpec>,
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
    /// Release an initialiser only after confirmed runtime retirement.
    ForgetInitialiser {
        instance_id: InstanceId,
        initialiser: InstanceId,
        reply: oneshot::Sender<Result<(), BunError>>,
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
        is_job: bool,
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
        reply: oneshot::Sender<Result<Option<restarts::TakenRestart>, BunError>>,
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
    /// Post-start bookkeeping for a job instance: log forwarder, on-disk
    /// record, transitions to Running.
    FinishJobInstance {
        instance_id: InstanceId,
        job_name: String,
        namespace: String,
        oci_spec: Box<crate::grill::oci::OciSpec>,
        evidence: Box<launch_evidence::LaunchEvidence>,
        reply: oneshot::Sender<Result<(), BunError>>,
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
            DeployOp::ConfirmJobSuccess { .. } => "confirm_job_success",
            DeployOp::EnforceImageReference { .. } => "enforce_image_reference",
            DeployOp::HealthProbeResult { .. } => "health_probe_result",
            DeployOp::EnforceImageSignature { .. } => "enforce_image_signature",
            DeployOp::StoreDeployedSpec { .. } => "store_deployed_spec",
            DeployOp::ListExistingOwned { .. } => "list_existing_owned",
            DeployOp::ReplicasToAddInPlace { .. } => "replicas_to_add_in_place",
            DeployOp::AddAppReplicas { .. } => "add_app_replicas",
            DeployOp::NextDeployGen { .. } => "next_deploy_gen",
            DeployOp::SupervisorDeployApp { .. } => "supervisor_deploy_app",
            DeployOp::PreparedBatchJob { .. } => "prepared_batch_job",
            DeployOp::SupervisorDeployJob { .. } => "supervisor_deploy_job",
            DeployOp::RegisterServiceApp { .. } => "register_service_app",
            DeployOp::RestoreStoppedRouting { .. } => "restore_stopped_routing",
            DeployOp::AbandonUnstartedInstances { .. } => "abandon_unstarted_instances",
            DeployOp::StoreIngress { .. } => "store_ingress",
            DeployOp::PrepareFreshInstance { .. } => "prepare_fresh_instance",
            DeployOp::StoreOciSpec { .. } => "store_oci_spec",
            DeployOp::RegisterInitialiser { .. } => "register_initialiser",
            DeployOp::ForgetInitialiser { .. } => "forget_initialiser",
            DeployOp::ApplyNetworkPreStart { .. } => "apply_network_pre_start",
            DeployOp::TransitionState { .. } => "transition_state",
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
            DeployOp::DeferRetire { .. } => "defer_retire",
            DeployOp::PushDeployHistory { .. } => "push_deploy_history",
            DeployOp::FinishJobInstance { .. } => "finish_job_instance",
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
        spec: &AppSpec,
    ) -> Result<Option<String>, String> {
        self.call(
            |reply| DeployOp::EnforceImageSignature {
                spec: Box::new(spec.clone()),
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
    ) -> Result<(), BunError> {
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

    pub(super) async fn replicas_to_add_in_place(
        &self,
        app_name: &str,
        namespace: &str,
        spec: &AppSpec,
    ) -> Option<u32> {
        self.call(
            |reply| DeployOp::ReplicasToAddInPlace {
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

    pub(super) async fn enforce_image_reference(
        &self,
        image: Option<&str>,
    ) -> Result<Option<String>, String> {
        self.call(
            |reply| DeployOp::EnforceImageReference {
                image: image.map(str::to_owned),
                reply,
            },
            Err("agent shutting down during prerequisite trust admission".into()),
        )
        .await
    }

    pub(super) async fn confirm_job_success(
        &self,
        instance_id: &InstanceId,
    ) -> Result<(), BunError> {
        self.confirm_job_exit(instance_id, 0).await
    }

    pub(super) async fn confirm_job_exit(
        &self,
        instance_id: &InstanceId,
        code: i32,
    ) -> Result<(), BunError> {
        self.call(
            |reply| DeployOp::ConfirmJobSuccess {
                instance_id: instance_id.clone(),
                code,
                reply,
            },
            Err(BunError::JobState(
                "agent unavailable before job exit was persisted".into(),
            )),
        )
        .await
    }

    pub(super) async fn prepared_batch_job(
        &self,
        name: &str,
        namespace: &str,
        generation: u64,
        spec: &JobSpec,
    ) -> Result<Vec<InstanceId>, BunError> {
        self.call(
            |reply| DeployOp::PreparedBatchJob {
                name: name.into(),
                namespace: namespace.into(),
                generation,
                spec: Box::new(spec.clone()),
                reply,
            },
            Err(BunError::JobState(
                "agent unavailable before the prepared generation was claimed".into(),
            )),
        )
        .await
    }

    pub(super) async fn supervisor_deploy_job(
        &self,
        job_name: &str,
        namespace: &str,
        spec: &JobSpec,
        rerun_unknown: bool,
    ) -> Result<Vec<InstanceId>, BunError> {
        retry_while_still_running(|| {
            self.call(
                |reply| DeployOp::SupervisorDeployJob {
                    rerun_unknown,
                    job_name: job_name.to_string(),
                    namespace: namespace.to_string(),
                    spec: Box::new(spec.clone()),
                    reply,
                },
                Ok(Vec::new()),
            )
        })
        .await
    }

    pub(super) async fn register_service_app(
        &self,
        app_name: &str,
        namespace: &str,
        port: u16,
        firewall: Option<Vec<String>>,
    ) -> Result<(), BunError> {
        self.call(
            |reply| DeployOp::RegisterServiceApp {
                app_name: app_name.to_string(),
                namespace: namespace.to_string(),
                port,
                firewall,
                reply,
            },
            Err(BunError::BackendPublication {
                service: crate::onion::service_id::ServiceId::new(namespace, app_name),
                reason: "agent loop closed before service registration".into(),
            }),
        )
        .await
    }

    pub(super) async fn restore_stopped_routing(
        &self,
        app_name: &str,
        namespace: &str,
        spec: &AppSpec,
    ) -> Result<(), BunError> {
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

    pub(super) async fn finish_fresh_instance(
        &self,
        instance_id: &InstanceId,
        app_name: &str,
        namespace: &str,
        evidence: launch_evidence::LaunchEvidence,
    ) -> Result<(), BunError> {
        self.call(
            |reply| DeployOp::FinishFreshInstance {
                instance_id: instance_id.clone(),
                app_name: app_name.to_string(),
                namespace: namespace.to_string(),
                evidence: Box::new(evidence),
                reply,
            },
            Ok(()),
        )
        .await
    }

    pub(super) async fn provision_identity(
        &self,
        app_name: &str,
        namespace: &str,
        instance_id: &InstanceId,
        is_job: bool,
    ) {
        self.call(
            |reply| DeployOp::ProvisionIdentity {
                app_name: app_name.to_string(),
                namespace: namespace.to_string(),
                instance_id: instance_id.clone(),
                is_job,
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
        .await
    }

    pub(super) async fn begin_retire(
        &self,
        old_id: &InstanceId,
    ) -> Result<Option<restarts::TakenRestart>, BunError> {
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

    pub(super) async fn finish_job_instance(
        &self,
        instance_id: &InstanceId,
        job_name: &str,
        namespace: &str,
        oci_spec: crate::grill::oci::OciSpec,
        evidence: launch_evidence::LaunchEvidence,
    ) -> Result<(), BunError> {
        self.call(
            |reply| DeployOp::FinishJobInstance {
                instance_id: instance_id.clone(),
                job_name: job_name.to_string(),
                namespace: namespace.to_string(),
                oci_spec: Box::new(oci_spec),
                evidence: Box::new(evidence),
                reply,
            },
            Ok(()),
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
            DeployOp::EnforceImageSignature { spec, reply } => {
                let verdict = self.enforce_image_signature(&spec).await;
                let check = self.cosign_check(spec.image.as_deref()).await;
                answer_after_cosign(verdict, check, reply);
            }
            DeployOp::StoreDeployedSpec {
                app_name,
                namespace,
                spec,
                reply,
            } => {
                let result = self.supervisor.admit_workload_kind(
                    &app_name,
                    &namespace,
                    crate::bun::deploy_operations::DeployTargetKind::App,
                );
                if result.is_ok() {
                    self.forget_adopted_app(&app_name, &namespace);
                    self.deployed_specs.insert((app_name, namespace), *spec);
                }
                let _ = reply.send(result);
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
                    .filter(|i| !i.is_job && i.app_name == app_name && i.namespace == namespace)
                    // Retired by an earlier rollout; only its release remains.
                    .filter(|i| !self.deferred_retirements.contains(&i.id))
                    .map(|i| i.id.clone())
                    .collect();
                let _ = reply.send(ids);
            }
            DeployOp::ReplicasToAddInPlace {
                app_name,
                namespace,
                spec,
                reply,
            } => {
                let _ = reply.send(self.replicas_to_add_in_place(&app_name, &namespace, &spec));
            }
            DeployOp::AddAppReplicas {
                app_name,
                namespace,
                spec,
                count,
                reply,
            } => {
                // LOOP-INLINE: in-memory lock, no I/O
                let result = self
                    .supervisor
                    .add_app_replicas(&app_name, &namespace, &spec, count, Instant::now())
                    .await;
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
                let _ = reply.send(result);
            }
            DeployOp::EnforceImageReference { image, reply } => {
                let verdict = self
                    .enforce_image_reference_signature(image.as_deref())
                    .await;
                let check = self.cosign_check(image.as_deref()).await;
                answer_after_cosign(verdict, check, reply);
            }
            DeployOp::ConfirmJobSuccess {
                instance_id,
                code,
                reply,
            } => {
                let result = self
                    .record_observed_job_exit(
                        &instance_id,
                        crate::bun::jobs::JobPhase::Exited { code },
                    )
                    .await;
                if result.is_ok()
                    && let Some(instance) = self.supervisor.get_instance_mut(&instance_id)
                {
                    instance.retry_pending = false;
                    if instance.state.can_transition_to(ContainerState::Stopping) {
                        instance.state = ContainerState::Stopping;
                    }
                    if instance.state.can_transition_to(ContainerState::Stopped) {
                        instance.state = ContainerState::Stopped;
                    }
                }
                let _ = reply.send(result);
            }
            DeployOp::PreparedBatchJob {
                name,
                namespace,
                generation,
                spec,
                reply,
            } => {
                let id = crate::grill::InstanceIdentity::new(&namespace, &name, 0).instance_id();
                let result =
                    self.recorded_jobs
                        .get(&id.0)
                        .filter(|job| {
                            job.generation == generation
                                && job.restart_count == 0
                                && job.phase == crate::bun::jobs::JobPhase::Preparing
                                && job.batch_execution.is_some()
                                && job.spec == *spec
                        })
                        .filter(|_| {
                            !self.job_store_uncertain
                                && self.supervisor.get_instance(&id).is_some_and(|instance| {
                                    instance.state == ContainerState::Pending
                                })
                        })
                        .map(|_| vec![id.clone()])
                        .ok_or_else(|| {
                            BunError::JobState(
                                "prepared batch generation no longer owns this launch".into(),
                            )
                        });
                let _ = reply.send(result);
            }
            DeployOp::SupervisorDeployJob {
                rerun_unknown,
                job_name,
                namespace,
                spec,
                reply,
            } => {
                let result = self
                    .prepare_job_run(&job_name, &namespace, &spec, rerun_unknown)
                    .await;
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
                let result = async {
                    self.register_local_service(&service_id, port, firewall)?;
                    self.publish_backend_ebpf(&service_id).await?;
                    self.sync_firewall_ebpf().await;
                    Ok(())
                }
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
                // workload lingers.
                if result.is_err()
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
                is_job,
                reply,
            } => {
                // A no-op in standalone mode; a failure here is retried by the
                // rotation loop rather than failing the deploy. The CSR runs
                // off the loop, and the worker is answered when it finishes.
                self.begin_identity_provision(
                    &app_name,
                    &namespace,
                    &instance_id,
                    is_job,
                    Some(reply),
                );
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
                            instance.spec.health.as_ref().zip(instance.spec.port).map(
                                |(health, port)| {
                                    crate::bun::health::HealthCheckConfig::from_spec(health, port)
                                },
                            );
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
            DeployOp::DeferRetire { old_id, reply } => {
                self.defer_retirement(&old_id);
                let _ = reply.send(());
            }
            DeployOp::PushDeployHistory { entry, reply } => {
                self.deploy_history.write().await.push(*entry);
                let _ = reply.send(());
            }
            DeployOp::FinishJobInstance {
                instance_id,
                job_name,
                namespace,
                oci_spec,
                evidence,
                reply,
            } => {
                let result = self
                    .finish_job_instance(&instance_id, &job_name, &namespace, *oci_spec, &evidence)
                    .await;
                let _ = reply.send(result);
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
