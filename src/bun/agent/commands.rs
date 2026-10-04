//! The commands callers send the agent, and the loop branch that
//! dispatches them.

use super::*;

/// A progress event emitted during a deploy operation.
///
/// Sent over an `mpsc` channel so the API layer can stream events
/// to the client via SSE. The client displays `Progress` messages
/// in real time and collects the final `Complete` or `Error` event.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ApplyEvent {
    /// The agent accepted the deploy and assigned its queryable operation ID.
    Accepted { operation_id: String },
    /// Informational progress update.
    Progress { message: String },
    /// A single instance was created and started.
    InstanceCreated { id: String, app: String },
    /// The deploy finished successfully.
    Complete {
        created: usize,
        instances: Vec<String>,
    },
    /// The deploy failed.
    Error { message: String },
}

/// The outcome of clearing one fault on this node.
#[derive(Debug)]
pub struct FaultClearance {
    /// Human-readable result for the API response.
    pub message: String,
    /// The committed node-fault reservation the fault held, until the leader
    /// has fenced it. The council releases that reservation asynchronously,
    /// so the API waits for it before reporting the clear as complete.
    pub reservation: Option<u64>,
}

/// Trusted local resolution used before public status/log authorisation.
#[derive(Debug)]
pub struct LogExecutionSelection {
    pub logical_name: String,
    pub instances: Vec<String>,
    pub selected_instance: Option<String>,
}

/// Commands sent to the agent over the command channel.
pub enum AgentCommand {
    ResolveExecutionLogs {
        app_name: String,
        namespace: String,
        instance: Option<String>,
        response: oneshot::Sender<Result<LogExecutionSelection, BunError>>,
    },
    LogCaptures {
        instances: Vec<String>,
        tail: Option<usize>,
        response: oneshot::Sender<Result<String, BunError>>,
    },
    /// Internal authenticated dispatch; admission must be durable before acknowledgement.
    RunJobsWithLabels {
        batch_id: u64,
        config: Config,
        execution_labels: BTreeMap<String, crate::bun::batch::BatchExecutionLabel>,
        events: mpsc::Sender<ApplyEvent>,
        response: oneshot::Sender<Result<BTreeMap<String, i32>, BunError>>,
    },
    /// Query explicit local ownership before enforcing first-launch allocation.
    BatchOwnedExecutions {
        identities: Vec<(String, String)>,
        response: oneshot::Sender<std::collections::BTreeSet<String>>,
    },
    /// Deploy workloads from a parsed Config.
    ///
    /// Progress events are streamed over the `events` channel so the
    /// API can relay them to the client in real time.
    Deploy {
        config: Config,
        events: mpsc::Sender<ApplyEvent>,
    },
    /// Admit and prepare migration images without launching any workload.
    PreparePrerequisites {
        config: Config,
        response: oneshot::Sender<
            Result<(Config, crate::bun::deploy_operations::DeployOperationHandle), String>,
        >,
    },
    /// Execute the prepared migrations under the operation's existing ownership.
    RunPrerequisites {
        config: Config,
        operation: crate::bun::deploy_operations::DeployOperationHandle,
        response: oneshot::Sender<Result<(), super::launch::PrerequisiteFailure>>,
    },
    /// Bind startup acknowledgement to the actual ordinary job generations.
    CaptureClusterJobs {
        config: Config,
        response: oneshot::Sender<Result<Arc<super::cluster_jobs::ClusterJobReceipt>, String>>,
    },
    /// Read positive completion or retirement evidence for that exact receipt.
    ClusterJobsSettlement {
        receipt: Arc<super::cluster_jobs::ClusterJobReceipt>,
        response: oneshot::Sender<super::cluster_jobs::ClusterJobSettlement>,
    },
    /// Explicit operator authorisation to rerun unknown node-local jobs.
    RerunJobs {
        config: Config,
        events: mpsc::Sender<ApplyEvent>,
    },
    /// Stop all instances of an app in a namespace.
    Stop {
        app_name: String,
        namespace: String,
        response: oneshot::Sender<Result<(), BunError>>,
    },
    /// Stop and retire an app removed from desired state or a resource lease.
    /// Successful retirement also releases its status and port ownership.
    Retire {
        app_name: String,
        namespace: String,
        response: oneshot::Sender<Result<(), BunError>>,
    },
    /// Retire a cleaning lease's runtime and its disposable managed storage.
    RetireTestResources {
        app_name: String,
        namespace: String,
        response: oneshot::Sender<Result<(), BunError>>,
    },
    /// Get status of all instances.
    Status {
        response: oneshot::Sender<Vec<InstanceStatus>>,
    },
    /// Whether instances adopted after a restart or self-upgrade already run
    /// exactly `spec` (replica count included), so the placement reconciler
    /// can record a still-pending placement as applied instead of rolling it.
    AdoptedPlacementMatches {
        app_name: String,
        namespace: String,
        spec: Box<AppSpec>,
        response: oneshot::Sender<bool>,
    },
    /// Get the local desired application specs for standalone diagnostics.
    DesiredApps {
        response: oneshot::Sender<Vec<crate::bun::diagnostics::DesiredAppEvidence>>,
    },
    /// Get the metrics endpoint of every live local instance whose app
    /// declares `metrics`, for the node's scrape loop.
    ScrapeTargets {
        response: oneshot::Sender<Vec<crate::mayo::scrape::AppScrapeTarget>>,
    },
    /// Get the currently deployed resources in plan format ("app.{name}",
    /// "job.{name}") with their images, for `relish --dry-run` diffing.
    CurrentResources {
        response: oneshot::Sender<Vec<CurrentResourceStatus>>,
    },
    /// Get status of run-to-completion workload instances.
    JobStatus {
        response: oneshot::Sender<Vec<JobStatus>>,
    },
    /// Get the image references of all current instances (for GC
    /// protection: actively deployed images must not be collected).
    ActiveImages {
        response: oneshot::Sender<std::collections::HashSet<String>>,
    },
    /// Snapshot active and recent real deploy operations.
    DeployOperations {
        response: oneshot::Sender<crate::bun::deploy_operations::DeployOperationSnapshot>,
    },
    /// Request cancellation of an operation owned by this node.
    CancelDeploy {
        operation_id: crate::bun::deploy_operations::DeployOperationId,
        response: oneshot::Sender<Option<crate::bun::deploy_operations::DeployOperation>>,
    },
    /// Get logs for an app.
    Logs {
        app_name: String,
        namespace: String,
        tail: Option<usize>,
        response: oneshot::Sender<Result<String, BunError>>,
    },
    /// Follow logs for an app (streaming).
    FollowLogs {
        app_name: String,
        namespace: String,
        tail: Option<usize>,
        /// Follow only this instance (`default__web-0`), not the whole app.
        instance: Option<String>,
        /// `Some(node)` prefixes every line with `[node instance]`, so lines
        /// from several nodes stay attributable once they're merged.
        label: Option<String>,
        lines: mpsc::Sender<String>,
    },
    /// Execute a command inside a running instance.
    Exec {
        app_name: String,
        namespace: String,
        command: Vec<String>,
        response: oneshot::Sender<Result<String, BunError>>,
    },
    /// Run the fixed Phase 15 connectivity probe from a local workload.
    Trace {
        request: crate::onion::trace::TraceRequest,
        internal_destination: bool,
        source_node: String,
        response: oneshot::Sender<Result<crate::onion::trace::TraceResult, BunError>>,
    },
    /// Get cluster node membership from the gossip layer.
    Nodes {
        /// Members the API remembers as down. Gossip's live view no longer
        /// lists them, but the listing must show them as dead rather than
        /// drop them. The agent adds their council flags.
        down: Vec<NodeStatus>,
        response: oneshot::Sender<Vec<NodeStatus>>,
    },
    /// Get council (Raft) status.
    Council {
        response: oneshot::Sender<CouncilStatus>,
    },
    /// Issue a node certificate for a joining node (issuer side).
    ///
    /// An existing cluster member receives this when a new node presents a
    /// join token. It validates the token against the replicated security
    /// state, consumes it via Raft, and returns the certificate bundle for
    /// the joiner to persist. `node_id` is supplied by the joiner.
    JoinIssue {
        token: String,
        node_id: String,
        /// DER PKCS#10 CSR the joiner generated (PKI4). The joiner keeps its
        /// private key; the issuer only signs this request.
        csr_der: Vec<u8>,
        response: oneshot::Sender<Result<crate::sesame::join::JoinBundle, BunError>>,
    },
    /// Snapshot an app's managed volumes (one volume, or all of them).
    SnapshotCreate {
        namespace: String,
        app_name: String,
        /// Container mount path to snapshot; `None` = every
        /// provisioned volume of the app.
        volume: Option<String>,
        name: Option<String>,
        response: oneshot::Sender<Result<Vec<crate::grill::snapshot::SnapshotMeta>, BunError>>,
    },
    /// List an app's snapshots, newest first.
    SnapshotList {
        namespace: String,
        app_name: String,
        response: oneshot::Sender<Result<Vec<crate::grill::snapshot::SnapshotMeta>, BunError>>,
    },
    /// Restore a snapshot over its live volume. Refused while the app
    /// has running instances.
    SnapshotRestore {
        namespace: String,
        app_name: String,
        name: String,
        /// Container mount path; required when several volumes share
        /// the snapshot name.
        volume: Option<String>,
        response: oneshot::Sender<Result<(), BunError>>,
    },
    /// Delete a snapshot.
    SnapshotDelete {
        namespace: String,
        app_name: String,
        name: String,
        /// Container mount path; required when several volumes share
        /// the snapshot name.
        volume: Option<String>,
        response: oneshot::Sender<Result<(), BunError>>,
    },
    /// Resolve a service name to its VIP and backends.
    Resolve {
        app_name: String,
        response: oneshot::Sender<Option<crate::onion::types::ResolveResponse>>,
    },
    /// List all registered services.
    ResolveAll {
        response: oneshot::Sender<Vec<crate::onion::types::ResolveResponse>>,
    },
    /// Install the latest cluster-wide endpoint catalogue (12b.4), replicated
    /// from the leader. The agent overlays it onto its local service map so
    /// DNS and ingress resolve services running on other nodes.
    SyncClusterCatalog {
        generation: u64,
        catalog: Box<crate::onion::catalog::EndpointCatalog>,
        ingress: Vec<crate::cluster::orchestrate::IngressAssignment>,
        response: oneshot::Sender<Result<(), BunError>>,
    },
    /// Reconcile enrolled durable consumer views and exact withdrawal obligations.
    SyncClusterConsumer {
        generation: u64,
        catalog: Box<crate::onion::catalog::EndpointCatalog>,
        ingress: Vec<crate::cluster::orchestrate::IngressAssignment>,
        withdrawals: Vec<crate::onion::withdrawal::EndpointWithdrawalInstruction>,
        /// When the placement request that carried this answer was sent, on
        /// [`crate::onion::lease::boot_clock_ns`]. The view lease runs from here.
        requested_at_ns: u64,
        response: oneshot::Sender<Result<ConsumerUpdate, BunError>>,
    },
    /// Confirm the leader acknowledged one original, locally proven receipt.
    ConfirmConsumerReceipt {
        generation: u64,
        response: oneshot::Sender<Result<(), BunError>>,
    },
    /// List all ingress routes.
    Routes {
        response: oneshot::Sender<Vec<crate::wrapper::types::RouteInfo>>,
    },
    /// Prepare the canonical request and identify this target process.
    PrepareNodeFault {
        request: crate::smoker::types::FaultRequest,
        response: oneshot::Sender<Result<(String, crate::smoker::types::FaultRequest), BunError>>,
    },
    /// Fence delayed activation and confirm reversal before releasing capacity.
    FenceNodeFault {
        only_if_finished: bool,
        reservation: crate::smoker::reservation::NodeFaultReservation,
        response: oneshot::Sender<Result<(), BunError>>,
    },
    /// Apply a workload fault, or a node fault carrying a committed grant.
    InjectFault {
        /// Boxed: a reservation embeds a whole fault request, and keeping it
        /// inline would make every other command as large as this one.
        reservation: Option<Box<crate::smoker::reservation::NodeFaultReservation>>,
        request: crate::smoker::types::FaultRequest,
        /// Cluster-wide replica counts for a workload fault, gathered by the
        /// API from every node. `None` falls back to this node's own view.
        replica_evidence: Option<crate::smoker::types::ReplicaEvidence>,
        response: oneshot::Sender<Result<crate::smoker::types::FaultSummary, BunError>>,
    },
    /// Clear a specific fault by ID.
    ClearFault {
        fault_id: u64,
        /// Whether the authenticated API caller may reverse a workload fault.
        allow_workload_fault: bool,
        /// Whether the authenticated API caller may reverse node state.
        allow_node_fault: bool,
        /// Whether the authenticated API caller may remove node pressure.
        allow_node_pressure: bool,
        response: oneshot::Sender<Result<FaultClearance, BunError>>,
    },
    /// Clear all active faults.
    ClearAllFaults {
        response: oneshot::Sender<Result<String, BunError>>,
    },
    /// Clear every active fault targeting a given service. `namespace`
    /// confines the clear to one tenant (`None` clears the service in every
    /// namespace, which the API allows only for unscoped tokens).
    ClearFaultsByService {
        service: String,
        namespace: Option<String>,
        response: oneshot::Sender<Result<String, BunError>>,
    },
    /// List all active faults.
    ListFaults {
        response: oneshot::Sender<Vec<crate::smoker::types::FaultSummary>>,
    },
    /// Verify an operator's detached image signature (made by `relish sign`
    /// with a key the cluster never sees) and attach it via Raft.
    SignImage {
        submission: crate::pickle::signing::SignatureSubmission,
        response: oneshot::Sender<Result<String, BunError>>,
    },
    /// Get the deployed AppSpec for a specific app (for safe env display).
    AppConfig {
        app_name: String,
        namespace: String,
        response: oneshot::Sender<Option<AppSpec>>,
    },
    /// Apply a node-level upgrade directive (Phase 14). Responds Ok once
    /// the upgrade is verified + staged; the exec happens just after.
    UpgradeApply {
        directive: crate::upgrade::types::UpgradeDirective,
        response: oneshot::Sender<Result<(), BunError>>,
        /// Resolves once the answer has reached the caller; the exec waits
        /// for it (bounded). `None` when no connection carries the answer.
        answer_delivered: Option<crate::sesame::connection::ConnectionClosed>,
    },
    /// Node-level upgrade status.
    UpgradeStatus {
        response: oneshot::Sender<Result<crate::upgrade::types::NodeUpgradeStatus, BunError>>,
    },
    /// Revert this node to a previous binary version.
    UpgradeRollback {
        version: Option<crate::upgrade::BinaryVersion>,
        response: oneshot::Sender<Result<(), BunError>>,
        /// As for [`AgentCommand::UpgradeApply`].
        answer_delivered: Option<crate::sesame::connection::ConnectionClosed>,
    },
    /// Post-boot self-verification of a freshly swapped-in version.
    /// Commits on success; flags revert and exits on failure.
    UpgradeVerify {
        marker: crate::upgrade::marker::UpgradeMarker,
        rejoin: Result<(), String>,
        response: oneshot::Sender<Result<bool, BunError>>,
    },
}

impl AgentCommand {
    /// The variant's name, for the loop meter's slow-turn log.
    pub(super) fn name(&self) -> &'static str {
        match self {
            AgentCommand::RunJobsWithLabels { .. } => "run_jobs_with_labels",
            AgentCommand::BatchOwnedExecutions { .. } => "batch_owned_executions",
            AgentCommand::ResolveExecutionLogs { .. } => "resolve_execution_logs",
            AgentCommand::LogCaptures { .. } => "log_captures",
            AgentCommand::Deploy { .. } => "deploy",
            AgentCommand::PreparePrerequisites { .. } => "prepare_prerequisites",
            AgentCommand::CaptureClusterJobs { .. } => "capture_cluster_jobs",
            AgentCommand::ClusterJobsSettlement { .. } => "cluster_jobs_settlement",
            AgentCommand::RunPrerequisites { .. } => "run_prerequisites",
            AgentCommand::RerunJobs { .. } => "rerun_jobs",
            AgentCommand::Stop { .. } => "stop",
            AgentCommand::Retire { .. } => "retire",
            AgentCommand::RetireTestResources { .. } => "retire_test_resources",
            AgentCommand::Status { .. } => "status",
            AgentCommand::AdoptedPlacementMatches { .. } => "adopted_placement_matches",
            AgentCommand::DesiredApps { .. } => "desired_apps",
            AgentCommand::ScrapeTargets { .. } => "scrape_targets",
            AgentCommand::CurrentResources { .. } => "current_resources",
            AgentCommand::JobStatus { .. } => "job_status",
            AgentCommand::ActiveImages { .. } => "active_images",
            AgentCommand::DeployOperations { .. } => "deploy_operations",
            AgentCommand::CancelDeploy { .. } => "cancel_deploy",
            AgentCommand::Logs { .. } => "logs",
            AgentCommand::FollowLogs { .. } => "follow_logs",
            AgentCommand::Exec { .. } => "exec",
            AgentCommand::Trace { .. } => "trace",
            AgentCommand::Nodes { .. } => "nodes",
            AgentCommand::Council { .. } => "council",
            AgentCommand::JoinIssue { .. } => "join_issue",
            AgentCommand::SnapshotCreate { .. } => "snapshot_create",
            AgentCommand::SnapshotList { .. } => "snapshot_list",
            AgentCommand::SnapshotRestore { .. } => "snapshot_restore",
            AgentCommand::SnapshotDelete { .. } => "snapshot_delete",
            AgentCommand::Resolve { .. } => "resolve",
            AgentCommand::ResolveAll { .. } => "resolve_all",
            AgentCommand::SyncClusterCatalog { .. } => "sync_cluster_catalog",
            AgentCommand::SyncClusterConsumer { .. } => "sync_cluster_consumer",
            AgentCommand::ConfirmConsumerReceipt { .. } => "confirm_consumer_receipt",
            AgentCommand::Routes { .. } => "routes",
            AgentCommand::PrepareNodeFault { .. } => "prepare_node_fault",
            AgentCommand::FenceNodeFault { .. } => "fence_node_fault",
            AgentCommand::InjectFault { .. } => "inject_fault",
            AgentCommand::ClearFault { .. } => "clear_fault",
            AgentCommand::ClearAllFaults { .. } => "clear_all_faults",
            AgentCommand::ClearFaultsByService { .. } => "clear_faults_by_service",
            AgentCommand::ListFaults { .. } => "list_faults",
            AgentCommand::SignImage { .. } => "sign_image",
            AgentCommand::AppConfig { .. } => "app_config",
            AgentCommand::UpgradeApply { .. } => "upgrade_apply",
            AgentCommand::UpgradeStatus { .. } => "upgrade_status",
            AgentCommand::UpgradeRollback { .. } => "upgrade_rollback",
            AgentCommand::UpgradeVerify { .. } => "upgrade_verify",
        }
    }
}

impl<G: Grill + Clone + 'static> BunAgent<G> {
    /// Check that every pre-upgrade workload survived the swap.
    pub(super) async fn verify_upgrade_inventory(
        &self,
        marker: &crate::upgrade::marker::UpgradeMarker,
    ) -> Result<(), String> {
        for item in &marker.pre_upgrade_instances {
            let id = InstanceId(item.full_id.clone());
            match self.supervisor.get_instance(&id) {
                Some(instance) if instance.state == ContainerState::Running => {}
                Some(instance) => {
                    return Err(format!(
                        "instance {id} is {} (was running before the upgrade)",
                        instance.state
                    ));
                }
                None => {
                    return Err(format!("instance {id} was not adopted after the upgrade"));
                }
            }
        }
        Ok(())
    }

    /// Handle a single command.
    pub(super) async fn handle_command(&mut self, cmd: AgentCommand) {
        match cmd {
            AgentCommand::ResolveExecutionLogs {
                app_name,
                namespace,
                instance,
                response,
            } => {
                let _ = response.send(self.resolve_execution_logs(
                    &app_name,
                    &namespace,
                    instance.as_deref(),
                ));
            }
            AgentCommand::LogCaptures {
                instances,
                tail,
                response,
            } => {
                self.spawn_selected_logs_read(
                    instances.into_iter().map(InstanceId).collect(),
                    tail,
                    response,
                );
            }
            AgentCommand::RunJobsWithLabels {
                batch_id,
                config,
                execution_labels,
                events,
                response,
            } => {
                let result = self
                    .begin_owned_batch(batch_id, config, execution_labels, events)
                    .await;
                let _ = response.send(result);
            }
            AgentCommand::BatchOwnedExecutions {
                identities,
                response,
            } => {
                let _ = response.send(self.batch_owned_identities(&identities));
            }
            AgentCommand::CaptureClusterJobs { config, response } => {
                let _ = response.send(self.capture_cluster_jobs(&config));
            }
            AgentCommand::ClusterJobsSettlement { receipt, response } => {
                let _ = response.send(self.cluster_jobs_settlement(&receipt));
            }
            AgentCommand::PreparePrerequisites { config, response } => {
                self.prepare_prerequisites(config, response).await;
            }
            AgentCommand::RunPrerequisites {
                config,
                operation,
                response,
            } => {
                self.run_prepared_prerequisites(config, operation, response);
            }
            AgentCommand::Deploy { config, events } => {
                self.begin_deploy(config, events, true, false).await;
            }
            AgentCommand::RerunJobs { config, events } => {
                self.begin_deploy(config, events, true, true).await;
            }
            AgentCommand::Stop {
                app_name,
                namespace,
                response,
            } => {
                self.request_app_stop(app_name, namespace, StopPurpose::Stop, response)
                    .await;
            }
            AgentCommand::Retire {
                app_name,
                namespace,
                response,
            } => {
                self.request_app_stop(app_name, namespace, StopPurpose::Retire, response)
                    .await;
            }
            AgentCommand::RetireTestResources {
                app_name,
                namespace,
                response,
            } => {
                if let Err(error) = Self::require_test_namespace(&app_name, &namespace) {
                    let _ = response.send(Err(error));
                } else {
                    self.request_app_stop(
                        app_name,
                        namespace,
                        StopPurpose::RetireTestResources,
                        response,
                    )
                    .await;
                }
            }
            AgentCommand::Status { response } => {
                // Publish now so the answer reflects every earlier command,
                // and read the runtime's evidence off the loop.
                let snapshot = self.publish_status();
                let grill = self.supervisor.grill().clone();
                tokio::spawn(async move {
                    let statuses = status_snapshot::read_status(&grill, &snapshot).await;
                    let _ = response.send(statuses);
                });
            }
            AgentCommand::ScrapeTargets { response } => {
                let targets = self
                    .supervisor
                    .list_instances()
                    .iter()
                    .filter(|instance| {
                        !instance.is_job
                            && matches!(
                                instance.state,
                                ContainerState::HealthWait
                                    | ContainerState::Running
                                    | ContainerState::Unhealthy
                            )
                    })
                    .filter_map(|instance| {
                        let spec = self
                            .deployed_specs
                            .get(&(instance.app_name.clone(), instance.namespace.clone()))?;
                        crate::mayo::scrape::AppScrapeTarget::for_instance(
                            &instance.id.0,
                            &instance.app_name,
                            &instance.namespace,
                            instance.container_ip,
                            spec,
                        )
                    })
                    .collect();
                let _ = response.send(targets);
            }
            AgentCommand::AdoptedPlacementMatches {
                app_name,
                namespace,
                spec,
                response,
            } => {
                let _ = response.send(self.adopted_instances_match(&app_name, &namespace, &spec));
            }
            AgentCommand::DesiredApps { response } => {
                let mut apps = self
                    .deployed_specs
                    .iter()
                    .map(
                        |((app, namespace), spec)| crate::bun::diagnostics::DesiredAppEvidence {
                            app: app.clone(),
                            namespace: namespace.clone(),
                            desired_replicas: crate::bun::diagnostics::desired_replica_count(
                                spec.replicas,
                                1,
                            ),
                            scheduled_replicas: self
                                .supervisor
                                .list_instances()
                                .iter()
                                .filter(|instance| {
                                    instance.app_name == *app && instance.namespace == *namespace
                                })
                                .count()
                                .try_into()
                                .unwrap_or(u32::MAX),
                            placements: Default::default(),
                            service_port: spec.port,
                            blocked: None,
                            volume_home_away: None,
                            volume_homes: Vec::new(),
                        },
                    )
                    .collect::<Vec<_>>();
                apps.sort_by(|left, right| {
                    (&left.namespace, &left.app).cmp(&(&right.namespace, &right.app))
                });
                let _ = response.send(apps);
            }
            AgentCommand::CurrentResources { response } => {
                let mut resources: Vec<CurrentResourceStatus> = self
                    .deployed_specs
                    .iter()
                    .map(|((app, namespace), spec)| CurrentResourceStatus {
                        resource: crate::config::fingerprint::app_resource_key(app, namespace),
                        image: spec.image.clone(),
                        fingerprint: crate::config::fingerprint::app_fingerprint_in(
                            spec, namespace,
                        ),
                    })
                    .collect();
                for job in self.get_job_status() {
                    resources.push(CurrentResourceStatus {
                        resource: crate::config::fingerprint::job_resource_key(
                            &job.name,
                            &job.namespace,
                        ),
                        image: Some(job.image),
                        fingerprint: self.recorded_jobs.get(&job.instance_id).and_then(|record| {
                            crate::config::fingerprint::job_fingerprint(&record.spec)
                        }),
                    });
                }
                resources.sort_by(|a, b| a.resource.cmp(&b.resource));
                resources.dedup_by(|a, b| {
                    if a.resource != b.resource {
                        return false;
                    }
                    if a.fingerprint != b.fingerprint || a.image != b.image {
                        // Different executions of one logical job cannot prove
                        // a single current specification.
                        b.fingerprint = None;
                        b.image = None;
                    }
                    true
                });
                let _ = response.send(resources);
            }
            AgentCommand::JobStatus { response } => {
                let statuses = self.get_job_status();
                let _ = response.send(statuses);
            }
            AgentCommand::ActiveImages { response } => {
                let images: std::collections::HashSet<String> = self
                    .supervisor
                    .list_instances()
                    .iter()
                    .map(|i| i.image.clone())
                    .filter(|image| !image.is_empty())
                    .collect();
                let _ = response.send(images);
            }
            AgentCommand::CancelDeploy {
                operation_id,
                response,
            } => {
                // LOOP-INLINE: in-memory lock, no I/O
                let operation = self
                    .deploy_operations
                    .request_cancellation(&operation_id)
                    .await;
                let _ = response.send(operation);
            }
            AgentCommand::DeployOperations { response } => {
                // LOOP-INLINE: in-memory lock, no I/O
                let _ = response.send(self.deploy_operations.snapshot().await);
            }
            AgentCommand::Logs {
                app_name,
                namespace,
                tail,
                response,
            } => {
                self.spawn_logs_read(&app_name, &namespace, tail, response);
            }
            AgentCommand::FollowLogs {
                app_name,
                namespace,
                tail,
                instance,
                label,
                lines,
            } => {
                self.spawn_logs_follow(&app_name, &namespace, tail, instance, label, lines);
            }
            AgentCommand::Exec {
                app_name,
                namespace,
                command,
                response,
            } => {
                // Resolve the target instance on the loop (cheap), then run the
                // exec off-loop under a deadline (H3). Running it inline let a
                // long command (`relish exec app -- sleep 3600`) stall health
                // checks, restarts and every other command — the exact reason
                // Trace was moved off the loop.
                match self.resolve_running_instance(&app_name, &namespace) {
                    Ok(instance_id) => {
                        let grill = self.supervisor.grill().clone();
                        tokio::spawn(async move {
                            let result = match tokio::time::timeout(
                                EXEC_TIMEOUT,
                                grill.exec(&instance_id, &command),
                            )
                            .await
                            {
                                Ok(inner) => inner.map_err(BunError::from),
                                Err(_) => Err(BunError::ExecTimeout {
                                    seconds: EXEC_TIMEOUT.as_secs(),
                                }),
                            };
                            let _ = response.send(result);
                        });
                    }
                    Err(error) => {
                        let _ = response.send(Err(error));
                    }
                }
            }
            AgentCommand::Trace {
                request,
                internal_destination,
                source_node,
                response,
            } => match self.prepare_trace(request, internal_destination, source_node) {
                Ok(trace) => {
                    tokio::spawn(async move {
                        let _ = response.send(trace.run().await);
                    });
                }
                Err(error) => {
                    let _ = response.send(Err(error));
                }
            },
            AgentCommand::Nodes { down, response } => {
                let nodes = self.get_cluster_nodes(down);
                let _ = response.send(nodes);
            }
            AgentCommand::Council { response } => {
                let status = self.get_council_status().await;
                let _ = response.send(status);
            }
            AgentCommand::JoinIssue {
                token,
                node_id,
                csr_der,
                response,
            } => {
                self.spawn_join_issue(token, node_id, csr_der, response);
            }
            AgentCommand::SnapshotCreate {
                namespace,
                app_name,
                volume,
                name,
                response,
            } => {
                let Some(lease) = self.reserve_volumes(
                    &namespace,
                    &app_name,
                    crate::bun::volume_maintenance::VolumeOperation::Snapshot,
                ) else {
                    let _ = response.send(Err(Self::volumes_busy(&namespace, &app_name)));
                    return;
                };
                // btrfs subprocess + fs walks off the command loop (M7).
                let volumes_dir = self.volumes_dir.clone();
                #[cfg(test)]
                let hold = self.snapshot_answered_hold.clone();
                tokio::task::spawn_blocking(move || {
                    let result = crate::grill::snapshot::SnapshotManager::new(&volumes_dir)
                        .create_for_app(
                            &namespace,
                            &app_name,
                            volume.as_deref(),
                            name.as_deref(),
                            std::time::SystemTime::now(),
                        )
                        .map_err(BunError::from);
                    Self::release_then_answer(lease, response, result);
                    #[cfg(test)]
                    Self::hold_after_answer(hold.as_deref());
                });
            }
            AgentCommand::SnapshotList {
                namespace,
                app_name,
                response,
            } => {
                let volumes_dir = self.volumes_dir.clone();
                tokio::task::spawn_blocking(move || {
                    let manager = crate::grill::snapshot::SnapshotManager::new(&volumes_dir);
                    let _ =
                        response.send(manager.list(&namespace, &app_name).map_err(BunError::from));
                });
            }
            AgentCommand::SnapshotRestore {
                namespace,
                app_name,
                name,
                volume,
                response,
            } => {
                // The running-instance check needs supervisor state, so it stays
                // on the loop; the btrfs restore itself runs off it (M7). An
                // instance waiting to be restarted counts as running.
                let running = self.supervisor.list_instances().into_iter().any(|i| {
                    i.app_name == app_name
                        && i.namespace == namespace
                        && (i.retry_pending
                            || !matches!(i.state, ContainerState::Stopped | ContainerState::Failed))
                });
                if running {
                    let _ = response.send(Err(crate::grill::snapshot::SnapshotError::AppRunning {
                        namespace: namespace.clone(),
                        app: app_name.clone(),
                    }
                    .into()));
                    return;
                }
                // A deploy still creating the volumes off the loop would
                // race the restore's swap.
                if self.off_loop_work.provisioning(&namespace, &app_name) {
                    let _ = response.send(Err(Self::volumes_busy(&namespace, &app_name)));
                    return;
                }
                // Reserve before dispatching, with no await in between: from
                // here until the task drops the lease, deploys, restarts and
                // other snapshot operations on this app are refused (B03).
                let Some(lease) = self.reserve_volumes(
                    &namespace,
                    &app_name,
                    crate::bun::volume_maintenance::VolumeOperation::Restore,
                ) else {
                    let _ = response.send(Err(Self::volumes_busy(&namespace, &app_name)));
                    return;
                };
                let volumes_dir = self.volumes_dir.clone();
                #[cfg(test)]
                let pause = self.restore_pause.clone();
                #[cfg(test)]
                let hold = self.snapshot_answered_hold.clone();
                tokio::task::spawn_blocking(move || {
                    #[cfg(test)]
                    if let Some(pause) = pause {
                        pause.wait();
                    }
                    let result = crate::grill::snapshot::SnapshotManager::new(&volumes_dir)
                        .restore(&namespace, &app_name, &name, volume.as_deref())
                        .map_err(BunError::from);
                    Self::release_then_answer(lease, response, result);
                    #[cfg(test)]
                    Self::hold_after_answer(hold.as_deref());
                });
            }
            AgentCommand::SnapshotDelete {
                namespace,
                app_name,
                name,
                volume,
                response,
            } => {
                let Some(lease) = self.reserve_volumes(
                    &namespace,
                    &app_name,
                    crate::bun::volume_maintenance::VolumeOperation::Snapshot,
                ) else {
                    let _ = response.send(Err(Self::volumes_busy(&namespace, &app_name)));
                    return;
                };
                let volumes_dir = self.volumes_dir.clone();
                #[cfg(test)]
                let hold = self.snapshot_answered_hold.clone();
                tokio::task::spawn_blocking(move || {
                    let result = crate::grill::snapshot::SnapshotManager::new(&volumes_dir)
                        .delete(&namespace, &app_name, &name, volume.as_deref())
                        .map_err(BunError::from);
                    Self::release_then_answer(lease, response, result);
                    #[cfg(test)]
                    Self::hold_after_answer(hold.as_deref());
                });
            }
            AgentCommand::PrepareNodeFault {
                mut request,
                response,
            } => {
                let result = crate::smoker::config::effective_duration(
                    request.duration,
                    false,
                    &self.smoker_config,
                )
                .map(|duration| {
                    request.duration = duration;
                    (self.node_fault_fence.boot_id.clone(), request)
                })
                .map_err(|reason| BunError::FaultRejected { reason });
                let _ = response.send(result);
            }
            AgentCommand::FenceNodeFault {
                only_if_finished,
                reservation,
                response,
            } => {
                match self
                    .fence_node_fault_up_to_pressure(&reservation, only_if_finished)
                    .await
                {
                    Ok(NodeFaultFence::Done) => {
                        let _ = response.send(Ok(()));
                    }
                    Ok(NodeFaultFence::Pressure { fenced }) => {
                        self.spawn_node_pressure_fence(reservation, fenced, response);
                    }
                    Err(reason) => {
                        let _ = response.send(Err(BunError::FaultRejected { reason }));
                    }
                }
            }
            AgentCommand::InjectFault {
                reservation,
                mut request,
                replica_evidence,
                response,
            } => {
                // Duration bounds first (server-side, so a direct API call
                // can't slip past the CLI's defaulting): apply the configured
                // default when none was given, reject anything over the max.
                match crate::smoker::config::effective_duration(
                    request.duration,
                    request.fault_type.is_instantaneous(),
                    &self.smoker_config,
                ) {
                    Ok(effective) => request.duration = effective,
                    Err(reason) => {
                        let _ = response.send(Err(BunError::FaultRejected { reason }));
                        return;
                    }
                }

                if !request.fault_type.is_node_targeted() && request.namespace.is_none() {
                    let _ = response.send(Err(BunError::FaultRejected {
                        reason: "workload faults require a namespace".into(),
                    }));
                    return;
                }

                if request.fault_type.is_node_targeted() {
                    let result = reservation
                        .as_deref()
                        .ok_or_else(|| {
                            "node faults require a committed cluster reservation".to_string()
                        })
                        .and_then(|grant| self.node_fault_fence.activate(grant, &request));
                    if let Err(reason) = result {
                        let _ = response.send(Err(BunError::FaultRejected { reason }));
                        return;
                    }
                }

                // Safety rails next (L14): reject faults that risk
                // quorum, kill a service's last replica, target the
                // leader, or exceed the node-percentage cap — unless
                // explicitly overridden. The context is built even with no
                // cluster handle so the replica-minimum rail still runs (M1).
                let context = self.build_safety_context(&request, replica_evidence).await;
                let check = crate::smoker::safety::evaluate_safety(&request, &context);
                if !check.approved {
                    let reason = check
                        .violation
                        .map(|v| v.to_string())
                        .unwrap_or_else(|| "safety check failed".into());
                    let _ = response.send(Err(BunError::FaultRejected { reason }));
                    return;
                }

                // Actually apply the fault. Only record it in the
                // registry if injection succeeded — a fault that can't
                // be applied must not report success (the old code
                // recorded everything, injecting nothing).
                let rule = self.fault_registry.insert(&request);
                if let Some(grant) = &reservation {
                    self.node_fault_fence.active = Some((grant.sequence, rule.id));
                }
                // A kill, pause or resume reads its targets' pids and signals
                // them from a task; the caller hears once they're signalled.
                if let Some(signal) = signal_faults::Signal::of(&rule.fault_type) {
                    self.spawn_signal_fault(&rule, signal, response);
                    return;
                }
                // A pressure helper takes seconds to start; it starts in a
                // task, and the caller hears once it runs (#351, stage 3).
                if let crate::smoker::types::FaultType::NodePressure {
                    cpu_percentage,
                    memory_percentage,
                } = rule.fault_type
                {
                    match self.check_node_pressure(&rule, cpu_percentage, memory_percentage) {
                        Ok(()) => self.spawn_node_pressure_start(
                            rule.id,
                            cpu_percentage,
                            memory_percentage,
                            response,
                        ),
                        Err(reason) => {
                            self.fault_registry.remove(rule.id);
                            let _ = response.send(Err(BunError::FaultRejected { reason }));
                        }
                    }
                    return;
                }
                match self.apply_fault(&rule).await {
                    Ok(()) => {
                        let summary = crate::smoker::types::FaultSummary::from(&rule);
                        // A partition is applied once the connections it cut
                        // are gone, not when its map key is written (#450).
                        // Cuts that outlasted the turn finish in a task, and
                        // the caller hears after them.
                        let late = std::mem::take(&mut self.network_faults.late_cuts);
                        if late.is_empty() {
                            let _ = response.send(Ok(summary));
                        } else {
                            tokio::spawn(async move {
                                faults::finish_late_cuts(late).await;
                                let _ = response.send(Ok(summary));
                            });
                        }
                    }
                    Err(reason) => {
                        self.fault_registry.remove(rule.id);
                        // Take back anything a partial network install wrote.
                        self.reconcile_network_faults().await;
                        let _ = response.send(Err(BunError::FaultRejected { reason }));
                    }
                }
            }
            AgentCommand::ClearFault {
                fault_id,
                allow_workload_fault,
                allow_node_fault,
                allow_node_pressure,
                response,
            } => {
                let fault_id = crate::smoker::types::FaultId(fault_id);
                // The fence keeps the grant after the effect is reversed, until
                // the leader fences it, so a retried clear still reports it.
                let reservation = self
                    .node_fault_fence
                    .active
                    .and_then(|(sequence, id)| (id == fault_id).then_some(sequence));
                if let Some(rule) = self.fault_registry.get(fault_id) {
                    let denied = if rule.fault_type.is_node_operation() {
                        (!allow_node_fault).then_some(
                            "node fault reversal requires alter_node_state authorisation",
                        )
                    } else if matches!(
                        rule.fault_type,
                        crate::smoker::types::FaultType::NodePressure { .. }
                    ) {
                        (!allow_node_pressure).then_some(
                            "node pressure reversal requires saturate_capacity authorisation",
                        )
                    } else {
                        (!allow_workload_fault).then_some(
                            "workload fault reversal requires inject_workload_faults authorisation",
                        )
                    };
                    if let Some(reason) = denied {
                        let _ = response.send(Err(BunError::FaultRejected {
                            reason: reason.to_string(),
                        }));
                        return;
                    }
                }
                let msg = match self.fault_registry.get(fault_id).cloned() {
                    Some(rule) => {
                        let node_pressure = matches!(
                            &rule.fault_type,
                            crate::smoker::types::FaultType::NodePressure { .. }
                        );
                        if node_pressure {
                            // The helper stops in a task, and the caller hears
                            // once it's gone (#351, stage 3).
                            self.spawn_node_pressure_clearance(rule, reservation, response);
                            return;
                        }
                        self.reverse_fault(&rule).await;
                        self.fault_registry.remove(fault_id);
                        // Network faults are converged from the registry, so
                        // reconciling without the rule takes its kernel state
                        // back. A DnsNxdomain fault lives in the published set,
                        // so republish so the responder stops faulting the
                        // target.
                        self.reconcile_network_faults().await;
                        self.publish_dns_faults();
                        format!("cleared fault {} ({})", rule.id, rule.fault_type)
                    }
                    None => format!("fault {} not found", fault_id.0),
                };
                let _ = response.send(Ok(FaultClearance {
                    message: msg,
                    reservation,
                }));
            }
            AgentCommand::ClearAllFaults { response } => {
                let removed = self.fault_registry.clear_workload_faults();
                for rule in &removed {
                    self.reverse_fault(rule).await;
                }
                self.reconcile_network_faults().await;
                // Republish the (now empty) DnsNxdomain set for the responder.
                self.publish_dns_faults();
                let msg = format!("cleared {} fault(s)", removed.len());
                let _ = response.send(Ok(msg));
            }
            AgentCommand::ClearFaultsByService {
                service,
                namespace,
                response,
            } => {
                let removed = self
                    .fault_registry
                    .clear_by_service(&service, namespace.as_deref());
                for rule in &removed {
                    self.reverse_fault(rule).await;
                }
                self.reconcile_network_faults().await;
                self.publish_dns_faults();
                let msg = format!("cleared {} fault(s) for {service}", removed.len());
                let _ = response.send(Ok(msg));
            }
            AgentCommand::ListFaults { response } => {
                let summaries = self.fault_registry.list();
                let _ = response.send(summaries);
            }
            AgentCommand::Resolve { app_name, response } => {
                // The CLI targets a service by bare name; resolve the first
                // match in any namespace, against the merged cluster view so a
                // service running only on other nodes still resolves (12b.4).
                let merged = self.merged_service_map();
                let result = merged
                    .resolve_by_name(&app_name)
                    .map(|e| e.to_resolve_response());
                let _ = response.send(result);
            }
            AgentCommand::ResolveAll { response } => {
                let merged = self.merged_service_map();
                let results = merged
                    .resolve_all()
                    .iter()
                    .map(|e| e.to_resolve_response())
                    .collect();
                let _ = response.send(results);
            }
            AgentCommand::SyncClusterCatalog {
                generation,
                catalog,
                ingress,
                response,
            } => {
                let result = self
                    .publish_cluster_catalogue(generation, *catalog, ingress)
                    .await;
                let _ = response.send(result);
            }
            AgentCommand::SyncClusterConsumer {
                generation,
                catalog,
                ingress,
                withdrawals,
                requested_at_ns,
                response,
            } => {
                // Each journal write is a step of its own turn (#505); the
                // answer, and the lease renewal, come after the last one.
                self.request_consumer_sync(super::consumer::ConsumerRequest {
                    generation,
                    catalog: *catalog,
                    ingress,
                    withdrawals,
                    answer: Some(super::consumer::ConsumerAnswer {
                        requested_at_ns,
                        response,
                    }),
                })
                .await;
            }
            AgentCommand::ConfirmConsumerReceipt {
                generation,
                response,
            } => {
                let result = self.confirm_consumer_receipt(generation).await;
                let _ = response.send(result);
            }
            AgentCommand::Routes { response } => {
                let table = self.routing_table.read().await;
                let _ = response.send(table.list_routes());
            }
            AgentCommand::SignImage {
                submission,
                response,
            } => {
                self.spawn_sign_image(submission, response);
            }
            AgentCommand::AppConfig {
                app_name,
                namespace,
                response,
            } => {
                let spec = self.deployed_specs.get(&(app_name, namespace)).cloned();
                let _ = response.send(spec);
            }
            AgentCommand::UpgradeApply {
                directive,
                response,
                answer_delivered,
            } => {
                self.begin_upgrade_apply(directive, response, answer_delivered);
            }
            AgentCommand::UpgradeStatus { response } => {
                let result = match &self.upgrade {
                    Some(manager) => Ok(manager.status()),
                    None => Err(BunError::UpgradesUnavailable),
                };
                let _ = response.send(result);
            }
            AgentCommand::UpgradeRollback {
                version,
                response,
                answer_delivered,
            } => {
                self.begin_upgrade_rollback(version, response, answer_delivered);
            }
            AgentCommand::UpgradeVerify {
                marker,
                rejoin,
                response,
            } => {
                self.handle_upgrade_verify(marker, rejoin, response).await;
            }
        }
    }

    /// Post-boot verification of a freshly swapped-in version: all
    /// pre-upgrade workloads must have been adopted and still be Running.
    /// Commit on success; flag revert and exit on failure (the supervisor
    /// restarts us, and startup recovery swaps the old binary back).
    pub(super) async fn handle_upgrade_verify(
        &mut self,
        marker: crate::upgrade::marker::UpgradeMarker,
        rejoin: Result<(), String>,
        response: oneshot::Sender<Result<bool, BunError>>,
    ) {
        let Some(manager) = self.upgrade.clone() else {
            let _ = response.send(Err(BunError::UpgradesUnavailable));
            return;
        };

        if let Err(reason) = rejoin {
            match manager.mark_revert_pending(&marker, &reason) {
                Ok(()) => {
                    let _ = response.send(Ok(false));
                    eprintln!("bun: {reason}; restarting into the previous binary");
                    std::process::exit(1);
                }
                Err(error) => {
                    let _ = response.send(Err(BunError::Upgrade(error)));
                }
            }
            return;
        }

        // In cluster mode, workload placement is the cluster's decision:
        // the scheduler may legitimately move an app off this node while it
        // bounces, so a missing pre-upgrade instance is NOT an upgrade
        // failure. Boot grace and fresh gossip acknowledgement provide
        // separate local and cluster liveness proofs; boot failures are
        // caught by the crash-loop budget, which reverts before we ever get
        // here. Single-node keeps the strict check as a local safety net —
        // there is no cluster to reschedule, so a vanished workload really
        // is a failed swap.
        if self.cluster.is_some() {
            if let Err(reason) = self.verify_upgrade_inventory(&marker).await {
                eprintln!("bun: note: {reason} — not reverting (cluster reschedules placements)");
            }
            match manager.commit(&marker) {
                Ok(()) => {
                    println!(
                        "bun: upgrade to {} verified and committed",
                        marker.target_version
                    );
                    let _ = response.send(Ok(true));
                }
                Err(e) => {
                    let _ = response.send(Err(BunError::Upgrade(e)));
                }
            }
            return;
        }

        match self.verify_upgrade_inventory(&marker).await {
            Ok(()) => match manager.commit(&marker) {
                Ok(()) => {
                    println!(
                        "bun: upgrade to {} verified and committed",
                        marker.target_version
                    );
                    let _ = response.send(Ok(true));
                }
                Err(e) => {
                    let _ = response.send(Err(BunError::Upgrade(e)));
                }
            },
            Err(reason) => {
                let _ = manager.mark_revert_pending(&marker, &reason);
                let _ = response.send(Ok(false));
                eprintln!("bun: exiting so the supervisor can restart into the revert");
                std::process::exit(1);
            }
        }
    }
}
