//! Chaos faults on this node: partitions, cgroup and process faults, network
//! delays and connect faults, their expiry and reversal.

use super::*;

/// What fencing a node fault left for a task to finish.
pub(super) enum NodeFaultFence {
    /// Nothing: the grant is fenced and its slot free.
    Done,
    /// Stop the fenced pressure fault's helper, if there is one, and confirm
    /// no helper is left, then free the slot.
    Pressure {
        fenced: Option<crate::smoker::types::FaultId>,
    },
}

/// What this node has installed for its active network faults.
///
/// Network faults are reconciled rather than written once: every change to the
/// fault set or the local instances recomputes the desired state and applies
/// only the difference against what is recorded here.
#[derive(Debug, Default)]
pub(super) struct InstalledNetworkFaults {
    /// `fault_connect_map` entries this node wrote.
    pub(super) connect: std::collections::BTreeMap<
        crate::smoker::network::ConnectFaultKey,
        crate::smoker::network::ConnectFaultEntry,
    >,
    /// Proven workload cgroup per caller instance, with the restart count it
    /// was read at, so a restarted container is looked up again.
    pub(super) caller_cgroups: std::collections::HashMap<InstanceId, (u32, u64)>,
    /// netem delay bands installed per caller instance id, with the restart
    /// count they were installed at.
    pub(super) delays:
        std::collections::HashMap<String, (u32, Vec<crate::smoker::network::DelayBand>)>,
    /// Whether this Bun has swept delay trees a previous Bun left behind.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub(super) delays_swept: bool,
    /// Connection cuts a reconcile started but its turn couldn't wait for.
    /// Whoever answers for the fault that landed them finishes them first:
    /// an injection before it replies, any other reconcile from a task.
    pub(super) late_cuts: crate::smoker::network::LateCuts,
    /// Callers the last caller read couldn't name a cgroup for before its
    /// turn's deadline. A fault keyed by caller cgroup doesn't reach them
    /// yet, so an injection waits for them before it answers (#625).
    pub(super) pending_callers: Vec<super::fault_coverage::PendingCaller>,
}

/// Run the cuts a reconcile left in [`InstalledNetworkFaults::late_cuts`].
pub(super) async fn finish_late_cuts(late: crate::smoker::network::LateCuts) {
    #[cfg(all(feature = "ebpf", target_os = "linux"))]
    late.finish(cut_open_connections).await;
    // Only the eBPF connect hook lands cuts, so there are none to run.
    #[cfg(not(all(feature = "ebpf", target_os = "linux")))]
    drop(late);
}

/// Destroy one caller's established connections to the faulted backends.
///
/// `use<>` says the future borrows nothing from `cut`: it owns copies of what
/// it needs, so it can outlive the turn that started it.
#[cfg(all(feature = "ebpf", target_os = "linux"))]
fn cut_open_connections(
    cut: &crate::smoker::network::ConnectionCut,
) -> impl std::future::Future<Output = ()> + Send + use<> {
    let instance_id = cut.instance_id.clone();
    let args = crate::smoker::network::socket_destroy_args(&cut.backends);
    async move {
        match crate::smoker::network::run_in_instance_netns(&instance_id, "ss", &args).await {
            // Process and host-network workloads have no namespace of their
            // own; their sockets live in the host's, among every other
            // caller's, so they are left alone.
            Ok(_) | Err(crate::smoker::network::NetnsCommandError::NoNamespace { .. }) => {}
            Err(error) => eprintln!("smoker: cutting open connections: {error}"),
        }
    }
}

/// The network-byte-order VIP and port of a fault's target service, if this
/// node knows it. Resolved against the exact namespace-qualified identity, so
/// a fault on `web` in `team-a` never picks up `team-b`'s `web` VIP.
#[cfg(all(feature = "ebpf", target_os = "linux"))]
pub(super) fn fault_vip_port(
    services: &crate::onion::service_map::ServiceMap,
    rule: &crate::smoker::types::FaultRule,
) -> Option<(u32, u16)> {
    let entry = services.resolve(&crate::onion::service_id::ServiceId::new(
        rule.namespace.as_deref()?,
        rule.target_service.as_str(),
    ))?;
    Some((entry.vip.to_network_byte_order(), entry.port.to_be()))
}

/// The post-rewrite backend addresses of a fault's target service, as this
/// node's merged service map knows them.
#[cfg(target_os = "linux")]
pub(super) fn fault_backend_addresses(
    services: &crate::onion::service_map::ServiceMap,
    rule: &crate::smoker::types::FaultRule,
) -> Vec<std::net::SocketAddrV4> {
    let Some(namespace) = rule.namespace.as_deref() else {
        return Vec::new();
    };
    services
        .resolve(&crate::onion::service_id::ServiceId::new(
            namespace,
            rule.target_service.as_str(),
        ))
        .map(|entry| {
            entry
                .backends
                .iter()
                .map(|backend| std::net::SocketAddrV4::new(backend.node_ip, backend.host_port))
                .collect()
        })
        .unwrap_or_default()
}

/// Remove Smoker's delay tree from an instance's interface if it has one,
/// restoring the default qdisc. Returns whether there was one.
#[cfg(target_os = "linux")]
pub(super) async fn remove_delay_tree(
    instance: &str,
) -> Result<bool, crate::smoker::network::NetnsCommandError> {
    use crate::smoker::network::{
        delay_remove_args, delay_show_args, has_delay_root, run_in_instance_netns,
    };
    let shown = run_in_instance_netns(instance, "tc", &delay_show_args()).await?;
    if !has_delay_root(&shown) {
        return Ok(false);
    }
    run_in_instance_netns(instance, "tc", &delay_remove_args()).await?;
    Ok(true)
}

/// Replace an instance's delay tree with `bands` (none: just remove it).
///
/// Rebuilding the whole tree keeps this simple and idempotent: a qdisc that
/// someone else added at the root makes the `add` fail rather than be
/// overwritten, and a failure half-way takes our partial tree back out.
#[cfg(target_os = "linux")]
pub(super) async fn program_delay_tree(
    instance: &str,
    bands: &[crate::smoker::network::DelayBand],
) -> Result<(), crate::smoker::network::NetnsCommandError> {
    use crate::smoker::network::{delay_install_args, run_in_instance_netns};
    remove_delay_tree(instance).await?;
    if bands.is_empty() {
        return Ok(());
    }
    for args in delay_install_args(bands) {
        if let Err(error) = run_in_instance_netns(instance, "tc", &args).await {
            let _ = remove_delay_tree(instance).await;
            return Err(error);
        }
    }
    Ok(())
}

/// Say what to do when the kernel has no netem, rather than echo tc.
#[cfg(target_os = "linux")]
pub(super) fn delay_error_hint(error: &crate::smoker::network::NetnsCommandError) -> String {
    let text = error.to_string();
    if text.contains("netem") && (text.contains("Unknown") || text.contains("not found")) {
        format!(
            "{text} (the kernel has no sch_netem module; install the linux-modules package for this kernel)"
        )
    } else {
        text
    }
}

/// Delays shape each caller's own container `eth0`. A host process on a
/// mixed node shares the host's network, so it has nothing to shape: leave it
/// out instead of failing the whole injection on it.
#[cfg(any(target_os = "linux", test))]
pub(super) fn delay_callers(
    callers: Vec<crate::smoker::network::LocalCaller>,
    addressed: &std::collections::HashSet<String>,
) -> Vec<crate::smoker::network::LocalCaller> {
    callers
        .into_iter()
        .filter(|caller| addressed.contains(&caller.instance_id))
        .collect()
}

impl<G: Grill + Clone + 'static> BunAgent<G> {
    /// Populate the gossip + Raft blocklists to partition this node
    /// from the named peers. Returns how many addresses were blocked.
    ///
    /// A peer is identified by gossip node name; its gossip address
    /// comes from membership and its Raft address is derived by the
    /// fixed port offset. Both must be blocked, or SWIM keeps half the
    /// path alive and the partition doesn't take.
    pub(super) async fn apply_partition(&self, peers: &[String]) -> usize {
        let Some(handle) = &self.cluster else {
            return 0;
        };
        let blocklists = &handle.partition_blocklists;

        // Resolve peer names → gossip SocketAddrs.
        let targets: Vec<std::net::SocketAddr> = {
            let membership = handle.membership_rx.borrow();
            peers
                .iter()
                .filter_map(|name| {
                    membership
                        .iter()
                        .find(|m| &m.node_id.0 == name)
                        .map(|m| m.address)
                })
                .collect()
        };

        let mut blocked = 0;
        if let Some(gossip) = &blocklists.gossip {
            let mut set = gossip.write().await;
            for addr in &targets {
                if set.insert(*addr) {
                    blocked += 1;
                }
            }
        }
        if let Some(raft) = &blocklists.raft {
            let mut set = raft.write().await;
            for addr in &targets {
                let raft_addr = std::net::SocketAddr::new(
                    addr.ip(),
                    (addr.port() as i32 + blocklists.raft_port_offset) as u16,
                );
                set.insert(raft_addr);
            }
        }
        blocked
    }

    /// Clear both transport blocklists (heal all partitions).
    pub(super) async fn clear_partition(&self) {
        let Some(handle) = &self.cluster else {
            return;
        };
        if let Some(gossip) = &handle.partition_blocklists.gossip {
            gossip.write().await.clear();
        }
        if let Some(raft) = &handle.partition_blocklists.raft {
            raft.write().await.clear();
        }
    }

    /// Unblock a specific set of peers on both transports — the reversal of
    /// [`apply_partition`]. Only the addresses this fault added are removed, so
    /// healing one partition fault leaves any others still in force.
    pub(super) async fn remove_partition(&self, peers: &[String]) {
        let Some(handle) = &self.cluster else {
            return;
        };
        let blocklists = &handle.partition_blocklists;
        let targets: Vec<std::net::SocketAddr> = {
            let membership = handle.membership_rx.borrow();
            peers
                .iter()
                .filter_map(|name| {
                    membership
                        .iter()
                        .find(|m| &m.node_id.0 == name)
                        .map(|m| m.address)
                })
                .collect()
        };
        if let Some(gossip) = &blocklists.gossip {
            let mut set = gossip.write().await;
            for addr in &targets {
                set.remove(addr);
            }
        }
        if let Some(raft) = &blocklists.raft {
            let mut set = raft.write().await;
            for addr in &targets {
                let raft_addr = std::net::SocketAddr::new(
                    addr.ip(),
                    (addr.port() as i32 + blocklists.raft_port_offset) as u16,
                );
                set.remove(&raft_addr);
            }
        }
    }

    /// Build the safety context for a fault request from live cluster state.
    ///
    /// Always returns a context (M1): when there's no council — standalone
    /// mode, or a node that hasn't joined — the quorum, leader, and
    /// node-percentage rails have nothing to act on and neutralise themselves
    /// via zeroed fields, but the **replica-minimum** rail still fires from the
    /// locally-known replica count. That rail is what stops `fault kill
    /// --count 0` from taking out a service's last replica, so it must run even
    /// with no cluster handle; the old code returned `None` there and skipped
    /// safety entirely.
    ///
    /// `replica_evidence`, when the API supplies it, replaces the local
    /// replica counts with cluster-wide ones, so a routed kill of the one
    /// replica this node holds is judged against the whole service.
    pub(super) async fn build_safety_context(
        &self,
        request: &crate::smoker::types::FaultRequest,
        replica_evidence: Option<crate::smoker::types::ReplicaEvidence>,
    ) -> crate::smoker::types::SafetyContext {
        // Replicas of the target service running locally (an approximation —
        // the leader has the cluster-wide count, but this node protects at
        // least its own replicas). Available with or without a cluster.
        let target_service_replicas = self
            .supervisor
            .list_instances()
            .iter()
            .filter(|i| {
                i.app_name == request.target_service
                    && request.namespace.as_deref() == Some(i.namespace.as_str())
            })
            .count() as u32;

        // Node-level faults already active. We count NodeKill/NodeDrain/
        // Partition and, conservatively, treat each as if it could touch a
        // council member — protecting quorum against the worst case rather
        // than assuming the best.
        let active_node_faults = self
            .fault_registry
            .iter()
            .filter(|f| {
                matches!(
                    f.fault_type,
                    crate::smoker::types::FaultType::NodeKill { .. }
                        | crate::smoker::types::FaultType::NodeDrain
                        | crate::smoker::types::FaultType::NodePressure { .. }
                        | crate::smoker::types::FaultType::CouncilPartition { .. }
                )
            })
            .count() as u32;

        let target_service_faulted_replicas =
            self.fault_registry
                .count_by_service(&request.target_service) as u32;

        // Cluster-derived fields, or zeros when this node has no council. A
        // zero `council_size`/`total_nodes` makes the quorum, leader, and
        // node-percentage rails self-skip (see `smoker::safety`).
        let (council_size, leader_node_id, total_nodes) = match self
            .cluster
            .as_ref()
            .and_then(|handle| handle.raft_metrics_rx.as_ref().map(|rx| (handle, rx)))
        {
            Some((handle, metrics_rx)) => {
                let metrics = metrics_rx.borrow().clone();
                let council_size =
                    metrics.membership_config.membership().voter_ids().count() as u32;
                let leader_node_id = metrics
                    .current_leader
                    .and_then(|id| {
                        metrics
                            .membership_config
                            .membership()
                            .get_node(&id)
                            .map(|info| info.name.clone())
                    })
                    .unwrap_or_default();
                let total_nodes = handle
                    .membership_rx
                    .borrow()
                    .iter()
                    .filter(|m| m.state == crate::mustard::state::NodeState::Alive)
                    .count()
                    .max(1) as u32;
                (council_size, leader_node_id, total_nodes)
            }
            None => (0, String::new(), 0),
        };

        let (target_service_replicas, target_service_faulted_replicas) = match replica_evidence {
            Some(evidence) => (evidence.replicas, evidence.faulted_replicas),
            None => (target_service_replicas, target_service_faulted_replicas),
        };

        crate::smoker::types::SafetyContext {
            council_size,
            council_nodes_with_active_faults: active_node_faults,
            leader_node_id,
            total_nodes,
            nodes_with_active_faults: active_node_faults,
            target_service_replicas,
            target_service_faulted_replicas,
        }
    }

    /// Apply a fault for real (L14). Process faults (kill/pause/resume)
    /// and CPU stress work on every platform; network faults need eBPF
    /// and are rejected honestly when it isn't loaded, rather than
    /// recorded as active while injecting nothing.
    pub(super) async fn apply_fault(
        &mut self,
        rule: &crate::smoker::types::FaultRule,
    ) -> Result<(), String> {
        use crate::smoker::types::FaultType;

        match &rule.fault_type {
            // `InjectFault` signals from a task (`spawn_signal_fault`); this
            // is the same work under the turn's runtime budget, for callers
            // that apply a fault directly.
            FaultType::Kill { .. } | FaultType::Pause | FaultType::Resume => {
                let Some(signal) = signal_faults::Signal::of(&rule.fault_type) else {
                    return Err(format!("{} is not a signal fault", rule.fault_type));
                };
                let ids = self.fault_targets(rule);
                let deadline = self.turn_deadline();
                // LOOP-INLINE: each read inside waits at most until the turn's runtime deadline
                let pids =
                    signal_faults::read_pids(self.supervisor.grill(), &ids, signal.count(), deadline)
                        .await;
                // Remember which PIDs we froze so clear/expiry can SIGCONT
                // them. Without this a paused workload stayed frozen forever
                // once the fault expired (CHAOS1); Resume was a separate
                // manual fault the operator had to remember to send.
                if let Some(paused) = signal_faults::send(signal, &pids, &rule.target_service)? {
                    self.record_reversal(rule.id, crate::smoker::types::FaultReversal::Pause(paused));
                }
                Ok(())
            }
            FaultType::CpuStress { percentage, cores } => {
                // Cap the TARGET instance's `cpu.max` quota instead of
                // burning cycles in Bun's own cgroup (CHAOS1). The old code
                // spun blocking tasks that competed for whatever CPU the Bun
                // process could get, which starved Bun — not the workload —
                // and could not be lifted before the deadline. Now the
                // workload keeps only `100 - percentage` of a core, and clear
                // /expiry restores its original quota.
                self.apply_cgroup_fault(
                    rule,
                    |cgroup| {
                        let saved = crate::smoker::resource::read_cpu_max(cgroup)
                            .map_err(|e| e.to_string())?;
                        // O17: `cores` used to be parsed and thrown away while
                        // the quota maths assumed one core, so on a 4-core node
                        // "80% stress" actually took 95%.
                        crate::smoker::resource::apply_cpu_stress(cgroup, *percentage, *cores)
                            .map_err(|e| e.to_string())?;
                        Ok(saved)
                    },
                    |cgroup, saved| {
                        if let Err(e) = crate::smoker::resource::restore_cpu_max(cgroup, saved) {
                            eprintln!(
                                "smoker: rollback cpu.max on {} failed: {e}",
                                cgroup.display()
                            );
                        }
                    },
                )
                .await
                .map(|saved| {
                    self.record_reversal(
                        rule.id,
                        crate::smoker::types::FaultReversal::CpuMax(saved),
                    );
                })
            }
            FaultType::DnsNxdomain => {
                if rule.namespace.as_deref().is_none_or(str::is_empty) {
                    return Err("DNS faults require an explicit namespace".into());
                }
                if rule.target_instance.is_some() {
                    return Err("DNS faults target a namespace-qualified service, not an individual instance".into());
                }
                // DNS resolution lives in the userspace responder
                // (src/onion/dns.rs), so this fault does too. Republish the
                // faulted-service set and the responder starts returning
                // NXDOMAIN for the target. This used to write an eBPF
                // `fault_dns_map` entry into an object that was never loaded,
                // so the fault did nothing on any configuration (12b.6 gate).
                self.publish_dns_faults();
                Ok(())
            }
            FaultType::Drop { .. } | FaultType::Partition { .. } => {
                // Connect-time drop and partition faults have a real cgroup
                // eBPF implementation. The rule is already in the registry,
                // so reconciling installs it; a failure here makes the caller
                // remove the rule and reconcile again, which takes back any
                // key this attempt wrote.
                #[cfg(all(feature = "ebpf", target_os = "linux"))]
                {
                    if self.onion_ebpf.is_some() {
                        self.check_connect_fault(rule).await?;
                        return self.reconcile_connect_faults().await;
                    }
                }
                Err(format!(
                    "{} requires the eBPF data path, which is not loaded on this node",
                    rule.fault_type
                ))
            }
            FaultType::Delay { .. } => {
                // The connect hook decides whether a connection may start; it
                // can't hold packets back. A netem qdisc on the caller's own
                // interface can, for new and open connections alike.
                #[cfg(target_os = "linux")]
                {
                    self.apply_delay_fault(rule).await
                }
                #[cfg(not(target_os = "linux"))]
                {
                    Err("delay faults need Linux traffic control (tc netem) in each caller's network namespace".to_string())
                }
            }
            FaultType::Bandwidth { .. } => Err(
                "bandwidth faults are not implemented yet; delay traffic with `relish fault delay` instead"
                    .to_string(),
            ),
            FaultType::MemoryPressure { percentage } => {
                // Squeeze the TARGET instance's `memory.high` toward its hard
                // limit so the kernel forces reclaim/allocation stalls on the
                // workload (CHAOS1 — this used to be a genuine no-op that
                // reported success).
                self.apply_cgroup_fault(
                    rule,
                    |cgroup| {
                        let saved = crate::smoker::resource::read_memory_high(cgroup)
                            .map_err(|e| e.to_string())?;
                        crate::smoker::resource::apply_memory_pressure(cgroup, *percentage)
                            .map_err(|e| e.to_string())?;
                        Ok(saved)
                    },
                    |cgroup, saved| {
                        if let Err(e) = crate::smoker::resource::restore_memory_high(cgroup, saved)
                        {
                            eprintln!(
                                "smoker: rollback memory.high on {} failed: {e}",
                                cgroup.display()
                            );
                        }
                    },
                )
                .await
                .map(|saved| {
                    self.record_reversal(
                        rule.id,
                        crate::smoker::types::FaultReversal::MemoryHigh(saved),
                    );
                })
            }
            FaultType::DiskIoThrottle {
                bytes_per_sec,
                write_only,
            } => {
                // Throttle the TARGET instance's block-I/O via `io.max`
                // (CHAOS1). The device major:minor is read from the workload's
                // volumes dir so the throttle lands on the disk the workload
                // actually writes to; clear/expiry lifts it.
                let device = self.io_device_major_minor();
                let dev_for_reverse = device.clone();
                let dev_for_rollback = device.clone();
                self.apply_cgroup_fault(
                    rule,
                    |cgroup| {
                        crate::smoker::resource::apply_disk_io_throttle(
                            cgroup,
                            *bytes_per_sec,
                            *write_only,
                            &device,
                        )
                        .map_err(|e| e.to_string())?;
                        Ok(cgroup.to_string_lossy().into_owned())
                    },
                    |cgroup, _saved| {
                        if let Err(e) = crate::smoker::resource::remove_disk_io_throttle(
                            cgroup,
                            &dev_for_rollback,
                        ) {
                            eprintln!(
                                "smoker: rollback io.max on {} failed: {e}",
                                cgroup.display()
                            );
                        }
                    },
                )
                .await
                .map(|paths| {
                    let instances = paths
                        .into_iter()
                        .map(|(_, path)| (path, dev_for_reverse.clone()))
                        .collect();
                    self.record_reversal(
                        rule.id,
                        crate::smoker::types::FaultReversal::DiskIo { instances },
                    );
                })
            }
            FaultType::NodeDrain => {
                if rule.duration_ns == 0 {
                    return Err("node faults require a non-zero duration".to_string());
                }
                if self.cluster.is_none() {
                    return Err("node drain requires an active cluster runtime".to_string());
                }
                let Some(readiness) = self.readiness.clone() else {
                    return Err(
                        "node drain requires live readiness evidence for scheduler fencing"
                            .to_string(),
                    );
                };

                if self.node_drain_gate.begin() {
                    // LOOP-INLINE: in-memory lock, no I/O
                    readiness.register("node:chaos-drain", true).await;
                }
                // LOOP-INLINE: in-memory lock, no I/O
                readiness
                    .degraded("node:chaos-drain", "node drain fault is active")
                    .await;
                self.record_reversal(rule.id, crate::smoker::types::FaultReversal::NodeDrain);
                Ok(())
            }
            FaultType::NodeKill { kill_containers } => {
                if rule.duration_ns == 0 {
                    return Err("node faults require a non-zero duration".to_string());
                }
                let Some(cluster) = &self.cluster else {
                    return Err("node kill requires an active cluster runtime".to_string());
                };

                cluster.partition_blocklists.node_gate.quiesce();
                if *kill_containers {
                    let ids: Vec<_> = self
                        .supervisor
                        .list_instances()
                        .iter()
                        .map(|instance| instance.id.clone())
                        .collect();
                    // The node is meant to look dead, so nothing waits on
                    // the kills: they run in a task, and the health tick
                    // sees the exits as it would a real crash (#351).
                    let grill = self.supervisor.grill().clone();
                    tokio::spawn(async move {
                        for id in ids {
                            if let Err(error) = grill.kill(&id).await {
                                eprintln!("smoker: node-kill container {} failed: {error}", id.0);
                            }
                        }
                    });
                }
                self.record_reversal(rule.id, crate::smoker::types::FaultReversal::NodeQuiesce);
                Ok(())
            }
            FaultType::NodePressure {
                cpu_percentage,
                memory_percentage,
            } => {
                // `InjectFault` starts a pressure helper itself, off the loop
                // (`spawn_node_pressure_start`); here it can only be refused.
                self.check_node_pressure(rule, *cpu_percentage, *memory_percentage)?;
                Err("node pressure starts from InjectFault, which waits for its helper".to_string())
            }
            FaultType::CouncilPartition { peers } => {
                // Block both the gossip and Raft transports to each named
                // peer, and record exactly which peers so clear and expiry
                // unblock these and leave any other partition in force.
                self.apply_partition(peers).await;
                self.record_reversal(
                    rule.id,
                    crate::smoker::types::FaultReversal::Partition {
                        peers: peers.clone(),
                    },
                );
                Ok(())
            }
        }
    }

    /// Original `(instance id, cgroup path)` pairs for matching workloads.
    /// Rollout generations must never share their predecessor's target path.
    #[cfg(target_os = "linux")]
    pub(super) fn target_instance_cgroups(
        &self,
        rule: &crate::smoker::types::FaultRule,
    ) -> Vec<(InstanceId, std::path::PathBuf)> {
        self.supervisor
            .list_instances()
            .iter()
            .filter(|i| {
                i.app_name == rule.target_service
                    && rule.matches_namespace(&i.namespace)
                    && rule.target_instance.as_ref().is_none_or(|t| &i.id.0 == t)
            })
            .filter_map(|instance| {
                Some((
                    instance.id.clone(),
                    instance.oci_spec.as_ref()?.linux.host_cgroup_path()?,
                ))
            })
            .collect()
    }

    /// Apply a cgroup-writing fault to every target instance and collect the
    /// per-instance saved state the `apply` closure returns (for later
    /// reversal).
    ///
    /// Returns an honest error when there are no running instances to target,
    /// or on any platform without cgroup v2. The `apply` closure runs once per
    /// target cgroup. If one fails partway through, the instances already
    /// modified are rolled back with `restore` before the error is surfaced
    /// (M1) — without that, an earlier replica stayed throttled while the
    /// caller, seeing the error, dropped the registry entry that would have
    /// let a later clear undo it.
    #[cfg(target_os = "linux")]
    pub(super) async fn apply_cgroup_fault<F, R>(
        &self,
        rule: &crate::smoker::types::FaultRule,
        mut apply: F,
        restore: R,
    ) -> Result<Vec<(String, String)>, String>
    where
        F: FnMut(&std::path::Path) -> Result<String, String>,
        R: Fn(&std::path::Path, &str),
    {
        let targets = self.target_instance_cgroups(rule);
        if targets.is_empty() {
            return Err(format!("no running instances of {}", rule.target_service));
        }
        let mut saved = Vec::with_capacity(targets.len());
        let mut applied: Vec<(std::path::PathBuf, String)> = Vec::new();
        for (id, cgroup) in targets {
            match apply(&cgroup) {
                Ok(value) => {
                    applied.push((cgroup.clone(), value.clone()));
                    saved.push((id.0, value));
                }
                Err(e) => {
                    // Roll back the instances already modified, newest first,
                    // so a partial application never leaks a limit.
                    for (cgroup, value) in applied.iter().rev() {
                        restore(cgroup, value);
                    }
                    return Err(e);
                }
            }
        }
        Ok(saved)
    }

    #[cfg(not(target_os = "linux"))]
    pub(super) async fn apply_cgroup_fault<F, R>(
        &self,
        rule: &crate::smoker::types::FaultRule,
        _apply: F,
        _restore: R,
    ) -> Result<Vec<(String, String)>, String>
    where
        F: FnMut(&std::path::Path) -> Result<String, String>,
        R: Fn(&std::path::Path, &str),
    {
        Err(format!("{} requires Linux cgroups", rule.fault_type))
    }

    /// Record a fault's reversal state in the registry after it was applied,
    /// so a later clear/expiry can undo the persistent effect.
    pub(super) fn record_reversal(
        &mut self,
        id: crate::smoker::types::FaultId,
        reversal: crate::smoker::types::FaultReversal,
    ) {
        if let Some(rule) = self.fault_registry.get_mut(id) {
            rule.reversal = reversal;
        }
    }

    /// The block device (`major:minor`) backing this node's workload storage,
    /// used to key an `io.max` throttle. cgroup v2 `io.max` is per-device, so
    /// a throttle must name one. We resolve the device under the volumes dir
    /// where workloads write; if it can't be determined we fall back to the
    /// common `8:0` (first SCSI/SATA disk), which the operator can override by
    /// running on a host whose data disk is `8:0`.
    pub(super) fn io_device_major_minor(&self) -> String {
        #[cfg(target_os = "linux")]
        {
            use std::os::unix::fs::MetadataExt;
            if let Ok(meta) = std::fs::metadata(&self.volumes_dir) {
                let dev = meta.dev();
                // Linux encodes major:minor in st_dev; unpack per libc rules.
                let major = (dev >> 8) & 0xfff;
                let minor = (dev & 0xff) | ((dev >> 12) & 0xfff00);
                return format!("{major}:{minor}");
            }
        }
        "8:0".to_string()
    }

    /// Serialise the fence with activation and retain ownership if cleanup
    /// fails. A pressure grant's helper is stopped, and its absence
    /// confirmed, off the loop: that's what `Pressure` hands back.
    pub(super) async fn fence_node_fault_up_to_pressure(
        &mut self,
        grant: &crate::smoker::reservation::NodeFaultReservation,
        only_if_finished: bool,
    ) -> Result<NodeFaultFence, String> {
        if only_if_finished
            && (!self.node_fault_fence.consumed(grant)
                || (grant.boot_id == self.node_fault_fence.boot_id
                    && self.node_fault_fence.active.is_some_and(|(sequence, id)| {
                        sequence == grant.sequence && self.fault_registry.get(id).is_some()
                    })))
        {
            return Err("node fault activation or reversal is still pending".into());
        }
        let pressure = matches!(
            grant.request.fault_type,
            crate::smoker::types::FaultType::NodePressure { .. }
        );
        let mut fenced = None;
        if let Some(id) = self.node_fault_fence.fence(grant) {
            if matches!(
                grant.request.fault_type,
                crate::smoker::types::FaultType::CouncilPartition { .. }
            ) {
                // The single node-experiment slot owns these transport lists.
                // Peer addresses may have changed since activation; removing
                // only today's addresses cannot prove the old entries are gone.
                self.clear_partition().await;
            }
            if pressure {
                // The registry keeps the fault until its helper is stopped.
                fenced = Some(id);
            } else if let Some(rule) = self.fault_registry.get(id).cloned() {
                self.reverse_fault(&rule).await;
                self.fault_registry.remove(id);
            }
        }
        // A failed apply or expiry may have removed its registry entry, so a
        // pressure grant inspects the helpers whether or not it fenced one.
        if pressure {
            return Ok(NodeFaultFence::Pressure { fenced });
        }
        self.release_node_fault_slot(grant);
        Ok(NodeFaultFence::Done)
    }

    /// Free the node-experiment slot a fenced grant held.
    pub(super) fn release_node_fault_slot(
        &mut self,
        grant: &crate::smoker::reservation::NodeFaultReservation,
    ) {
        if self
            .node_fault_fence
            .active
            .is_some_and(|(sequence, _)| sequence <= grant.sequence)
            && self.node_fault_fence.boot_id == grant.boot_id
        {
            self.node_fault_fence.active = None;
        }
    }

    /// The whole fence, pressure included, waiting for the helper inline.
    /// For tests that drive the agent without its loop.
    #[cfg(test)]
    pub(super) async fn fence_node_fault(
        &mut self,
        grant: &crate::smoker::reservation::NodeFaultReservation,
        only_if_finished: bool,
    ) -> Result<(), String> {
        let NodeFaultFence::Pressure { fenced } = self
            .fence_node_fault_up_to_pressure(grant, only_if_finished)
            .await?
        else {
            return Ok(());
        };
        let pressure = Arc::clone(&self.node_pressure);
        let mut controller = pressure.lock().await;
        if let Some(id) = fenced {
            controller.clear(id).await?;
            self.fault_registry.remove(id);
        }
        controller.confirm_no_helpers().await?;
        drop(controller);
        self.release_node_fault_slot(grant);
        Ok(())
    }

    /// Reverse a cleared or expired fault's persistent effect.
    ///
    /// Network faults are undone by `reconcile_network_faults`; this handles
    /// everything else that leaves a durable change — a paused process (SIGCONT
    /// it), a capped `cpu.max`, a squeezed `memory.high` or an `io.max`
    /// throttle (restore the saved value). Best-effort: an instance that has
    /// since exited simply has nothing left to restore.
    pub(super) async fn reverse_fault(&mut self, rule: &crate::smoker::types::FaultRule) {
        use crate::smoker::types::FaultReversal;
        match &rule.reversal {
            FaultReversal::None => {}
            FaultReversal::Pause(pids) => {
                for pid in pids {
                    if let Err(e) = crate::smoker::process::resume_process(*pid) {
                        // A process that exited while paused is fine; anything
                        // else is worth a line so a stuck workload is visible.
                        eprintln!("smoker: resume (auto) pid {pid} failed: {e}");
                    }
                }
            }
            FaultReversal::CpuMax(saved) => {
                for (_id, cgroup, value) in self.rejoin_cgroups(rule, saved) {
                    if let Err(e) = crate::smoker::resource::restore_cpu_max(&cgroup, &value) {
                        eprintln!(
                            "smoker: restore cpu.max on {} failed: {e}",
                            cgroup.display()
                        );
                    }
                }
            }
            FaultReversal::MemoryHigh(saved) => {
                for (_id, cgroup, value) in self.rejoin_cgroups(rule, saved) {
                    if let Err(e) = crate::smoker::resource::restore_memory_high(&cgroup, &value) {
                        eprintln!(
                            "smoker: restore memory.high on {} failed: {e}",
                            cgroup.display()
                        );
                    }
                }
            }
            FaultReversal::DiskIo { instances } => {
                for (path, device) in instances {
                    let cgroup = std::path::PathBuf::from(path);
                    if let Err(e) =
                        crate::smoker::resource::remove_disk_io_throttle(&cgroup, device)
                    {
                        eprintln!("smoker: lift io.max on {path} failed: {e}");
                    }
                }
            }
            FaultReversal::Partition { peers } => {
                self.remove_partition(peers).await;
            }
            FaultReversal::NodeDrain => {
                if self.node_drain_gate.finish()
                    && let Some(readiness) = self.readiness.clone()
                {
                    // LOOP-INLINE: in-memory lock, no I/O
                    readiness.ready("node:chaos-drain").await;
                }
            }
            FaultReversal::NodeQuiesce => {
                if let Some(cluster) = &self.cluster {
                    cluster.partition_blocklists.node_gate.restore();
                    eprintln!(
                        "smoker: reversed node fault {} on {:?}; transports quiesced={}",
                        rule.id,
                        rule.target_node,
                        cluster.partition_blocklists.node_gate.is_quiesced()
                    );
                }
            }
            // Nobody waits on this answer, so the helper stops in a task.
            FaultReversal::NodePressure => self.spawn_node_pressure_clear(rule.id),
        }
    }

    /// Pair each saved `(instance id, value)` with the instance's cgroup path.
    ///
    /// The cgroup path comes from the current instance's original OCI specification so
    /// reversal writes to the same directory the fault wrote to. An instance
    /// that has since gone away is dropped (nothing to restore).
    pub(super) fn rejoin_cgroups(
        &self,
        rule: &crate::smoker::types::FaultRule,
        saved: &[(String, String)],
    ) -> Vec<(String, std::path::PathBuf, String)> {
        saved
            .iter()
            .filter_map(|(id, value)| {
                let instance = self.supervisor.get_instance(&InstanceId(id.clone()))?;
                if instance.app_name != rule.target_service
                    || !rule.matches_namespace(&instance.namespace)
                {
                    return None;
                }
                let path = instance.oci_spec.as_ref()?.linux.host_cgroup_path()?;
                // A specific instance target still restores only its own cgroup.
                if rule.target_instance.as_ref().is_some_and(|t| t != id) {
                    return None;
                }
                Some((id.clone(), path, value.clone()))
            })
            .collect()
    }

    /// Drain expired faults from the registry. Called on every health tick.
    ///
    /// When a fault expires, its BPF map entry must be deleted so the
    /// kernel stops applying it. The eBPF programs also check expiry
    /// independently (defense in depth), but userspace cleanup frees
    /// map slots and kills resource fault helper processes.
    pub(super) async fn expire_faults(&mut self) {
        let now = crate::smoker::types::monotonic_now_ns();
        let expired = self.fault_registry.drain_expired(now);
        let mut expired_dns = false;
        for rule in &expired {
            if !rule.target_service.is_empty() {
                eprintln!(
                    "smoker: fault {} expired ({}), cleaning up",
                    rule.id, rule.fault_type
                );
            }
            // Undo persistent non-eBPF effects too: SIGCONT a paused
            // workload, lift a cgroup cap. Without this an expired Pause left
            // the process frozen and an expired resource fault left its cap in
            // place (CHAOS1).
            self.reverse_fault(rule).await;
            expired_dns |= matches!(
                rule.fault_type,
                crate::smoker::types::FaultType::DnsNxdomain
            );
        }
        // Republish the DnsNxdomain set only if one actually expired, so the
        // responder drops the name (the resolver also self-corrects on
        // expiry, but publishing keeps the set honest).
        if expired_dns {
            self.publish_dns_faults();
        }
        // Converge network faults every tick, not only on expiry: a source
        // instance that started or restarted since the last tick needs the
        // faults already active against its targets.
        self.reconcile_network_faults().await;
        // Retry any node-pressure cgroup whose directory lingered after its
        // helper was killed, so a transient removal failure doesn't leave the
        // controller permanently refusing new pressure faults.
        self.retry_node_pressure_cleanup();
    }

    /// Local instances that may call a faulted service.
    ///
    /// Only instances of an app some active fault names as its source need a
    /// cgroup id (the connect hook keys source-scoped faults by cgroup), and
    /// those are cached per restart, so the reconcile that runs on every
    /// health tick doesn't ask the runtime again for an unchanged container.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub(super) async fn local_callers(&mut self) -> Vec<crate::smoker::network::LocalCaller> {
        use crate::smoker::network::{LocalCaller, applies_to_caller};

        let live: Vec<(InstanceId, String, String, u32)> = self
            .supervisor
            .list_instances()
            .into_iter()
            .filter(|instance| {
                matches!(
                    instance.state,
                    ContainerState::Starting
                        | ContainerState::HealthWait
                        | ContainerState::Running
                        | ContainerState::Unhealthy
                )
            })
            .map(|instance| {
                (
                    instance.id.clone(),
                    instance.app_name.clone(),
                    instance.namespace.clone(),
                    instance.restart_count,
                )
            })
            .collect();
        self.network_faults
            .caller_cgroups
            .retain(|id, _| live.iter().any(|(live_id, ..)| live_id == id));

        let mut callers = Vec::with_capacity(live.len());
        let mut pending = Vec::new();
        // A caller whose cgroup the runtime doesn't name within the turn is
        // left out until a later reconcile asks again; the tick reconciles
        // network faults every second. It's remembered as pending, so an
        // injection doesn't report a fault in place that skipped it (#625).
        let deadline = self.turn_deadline();
        for (id, app, namespace, restarts) in live {
            let named_as_source = self.fault_registry.iter().any(|rule| {
                rule.fault_type.source_app().is_some() && applies_to_caller(rule, &app, &namespace)
            });
            let cgroup_id = match self.network_faults.caller_cgroups.get(&id) {
                _ if !named_as_source => None,
                Some((seen_at, cgroup)) if *seen_at == restarts => Some(*cgroup),
                _ => match tokio::time::timeout_at(
                    deadline,
                    self.supervisor.grill().workload_cgroup(&id),
                )
                .await
                {
                    Ok(Ok(Some(cgroup))) => {
                        self.network_faults
                            .caller_cgroups
                            .insert(id.clone(), (restarts, cgroup));
                        Some(cgroup)
                    }
                    Ok(Ok(None)) => None,
                    Ok(Err(error)) => {
                        eprintln!("smoker: caller {id} has no provable cgroup: {error}");
                        None
                    }
                    Err(_) => {
                        pending.push(super::fault_coverage::PendingCaller {
                            id: id.clone(),
                            app: app.clone(),
                            namespace: namespace.clone(),
                            restarts,
                        });
                        None
                    }
                },
            };
            callers.push(LocalCaller {
                instance_id: id.0,
                app,
                namespace,
                cgroup_id,
            });
        }
        self.network_faults.pending_callers = pending;
        callers
    }

    /// Bring every network fault's kernel state on this node in line with
    /// the active faults and the instances running now.
    ///
    /// Called after a fault is injected, cleared or expires, when a local
    /// instance starts, and on every health tick while a network fault is
    /// active, so a source replica that restarts or is scheduled here picks
    /// the fault up. Failures are logged; the next tick retries.
    pub(super) async fn reconcile_network_faults(&mut self) {
        // Until it has finished, reconcile_delays programs nothing.
        #[cfg(target_os = "linux")]
        let _ = self.sweep_stale_delays().await;
        let active = self
            .fault_registry
            .iter()
            .any(|rule| rule.fault_type.acts_on_callers());
        if !active
            && self.network_faults.connect.is_empty()
            && self.network_faults.delays.is_empty()
        {
            self.spawn_late_cuts();
            return;
        }
        if let Err(error) = self.reconcile_connect_faults().await {
            eprintln!("smoker: network fault reconcile: {error}");
        }
        // Nobody is waiting on this reconcile's answer, so its late cuts (and
        // any a failed injection left) finish from a task.
        self.spawn_late_cuts();
        #[cfg(target_os = "linux")]
        for (instance, error) in self.reconcile_delays().await {
            eprintln!("smoker: delay on {instance}: {error}");
        }
    }

    /// Finish the connection cuts no caller is waiting for from a task, so
    /// they land late rather than never.
    pub(super) fn spawn_late_cuts(&mut self) {
        let late = std::mem::take(&mut self.network_faults.late_cuts);
        if !late.is_empty() {
            tokio::spawn(finish_late_cuts(late));
        }
    }

    /// Check and install a delay fault on this node (Linux only).
    ///
    /// A delay is a netem qdisc on each caller container's own `eth0`, so it
    /// needs runc's per-container network namespaces, a target with backends
    /// to steer towards, and (for `--from`) a local instance of the source.
    /// The rule is already in the registry: reconciling installs it, and any
    /// caller that couldn't be shaped fails the injection.
    #[cfg(target_os = "linux")]
    pub(super) async fn apply_delay_fault(
        &mut self,
        rule: &crate::smoker::types::FaultRule,
    ) -> Result<(), String> {
        let runtime = self.supervisor.grill().runtime_kind();
        if runtime != crate::grill::records::RuntimeKind::Runc {
            return Err(format!(
                "delay faults shape each caller container's own network interface, which needs the runc runtime; this node runs {runtime:?}"
            ));
        }
        let services = self.merged_service_map();
        if fault_backend_addresses(&services, rule).is_empty() {
            return Err(format!(
                "{}/{} has no backends to delay traffic to",
                rule.namespace.as_deref().unwrap_or("default"),
                rule.target_service
            ));
        }
        let callers: Vec<String> = delay_callers(self.local_callers().await, &self.addressed())
            .into_iter()
            .filter(|caller| {
                crate::smoker::network::applies_to_caller(rule, &caller.app, &caller.namespace)
            })
            .map(|caller| caller.instance_id)
            .collect();
        if let Some(source) = rule.fault_type.source_app()
            && callers.is_empty()
        {
            return Err(format!(
                "no running instance of source app {source} runs on this node"
            ));
        }
        let failures: Vec<String> = self
            .reconcile_delays()
            .await
            .into_iter()
            .filter(|(instance, _)| callers.contains(instance))
            .map(|(_, error)| error)
            .collect();
        match failures.first() {
            None => Ok(()),
            Some(error) => Err(format!("cannot delay traffic: {error}")),
        }
    }

    /// Instances with their own container address, the only ones a delay
    /// can shape.
    #[cfg(target_os = "linux")]
    fn addressed(&self) -> std::collections::HashSet<String> {
        self.supervisor
            .list_instances()
            .into_iter()
            .filter(|instance| instance.container_ip.is_some())
            .map(|instance| instance.id.0.clone())
            .collect()
    }

    /// Remove any delay tree a previous Bun left on this node's containers.
    ///
    /// Faults don't survive a restart, but a netem qdisc lives in the
    /// container's network namespace, not in Bun, so a crashed Bun would
    /// leave its callers slowed forever. Runs once, on the first reconcile.
    #[cfg(target_os = "linux")]
    ///
    /// Every instance is swept at once, under the turn's runtime budget
    /// (#351, stage 3). Until every one has been, this returns `false` and
    /// no delay is programmed, so a late sweep can't take a fresh delay away.
    pub(super) async fn sweep_stale_delays(&mut self) -> bool {
        if self.network_faults.delays_swept
            || self.supervisor.grill().runtime_kind() != crate::grill::records::RuntimeKind::Runc
        {
            return true;
        }
        let instances: Vec<String> = self
            .supervisor
            .list_instances()
            .into_iter()
            .map(|instance| instance.id.0.clone())
            .collect();
        let deadline = self.turn_deadline();
        let sweeps = instances.iter().map(|instance| async move {
            tokio::time::timeout_at(deadline, remove_delay_tree(instance)).await
        });
        // `timeout_at` polls the sweeps before its clock, so at the deadline
        // the ones that finished still count.
        let Ok(outcomes) =
            tokio::time::timeout_at(deadline, futures_util::future::join_all(sweeps)).await
        else {
            return false;
        };
        let mut swept = true;
        for (instance, outcome) in instances.iter().zip(outcomes) {
            match outcome {
                Ok(Ok(true)) => eprintln!("smoker: removed a stale delay from {instance}"),
                Ok(
                    Ok(false) | Err(crate::smoker::network::NetnsCommandError::NoNamespace { .. }),
                ) => {}
                Ok(Err(error)) => eprintln!("smoker: stale delay sweep: {error}"),
                Err(_) => swept = false,
            }
        }
        self.network_faults.delays_swept = swept;
        swept
    }

    /// Converge every local caller's netem delays on what the active delay
    /// faults ask for. Returns `(instance, error)` for each caller whose
    /// interface couldn't be programmed; those are retried next tick.
    ///
    /// The callers are programmed at once, under the turn's runtime budget
    /// (#351, stage 3). One that doesn't finish in time is a failure, and
    /// its interface is marked unknown so the next pass rebuilds it.
    #[cfg(target_os = "linux")]
    pub(super) async fn reconcile_delays(&mut self) -> Vec<(String, String)> {
        use crate::smoker::network::{NetnsCommandError, desired_delays};

        let delaying = self.fault_registry.iter().any(|rule| {
            matches!(
                rule.fault_type,
                crate::smoker::types::FaultType::Delay { .. }
            )
        });
        if !delaying && self.network_faults.delays.is_empty() {
            return Vec::new();
        }
        let callers = delay_callers(self.local_callers().await, &self.addressed());
        let services = self.merged_service_map();
        let desired = desired_delays(
            self.fault_registry.iter(),
            |rule| fault_backend_addresses(&services, rule),
            &callers,
        );
        let restarts: std::collections::HashMap<String, u32> = self
            .supervisor
            .list_instances()
            .into_iter()
            .map(|instance| (instance.id.0.clone(), instance.restart_count))
            .collect();
        // A caller that has gone took its network namespace, and its qdisc,
        // with it.
        self.network_faults
            .delays
            .retain(|id, _| restarts.contains_key(id));

        let mut instances: std::collections::BTreeSet<String> = desired.keys().cloned().collect();
        instances.extend(self.network_faults.delays.keys().cloned());
        let changed: Vec<String> = instances
            .into_iter()
            .filter(|instance| {
                let restart = restarts.get(instance).copied().unwrap_or_default();
                let unchanged = match (
                    desired.get(instance),
                    self.network_faults.delays.get(instance),
                ) {
                    (Some(wanted), Some((seen_at, installed))) => {
                        *seen_at == restart && installed == wanted
                    }
                    (None, None) => true,
                    _ => false,
                };
                !unchanged
            })
            .collect();
        if changed.is_empty() {
            return Vec::new();
        }
        // A delay programmed before the stale sweep finishes could be swept
        // away by it.
        if !self.sweep_stale_delays().await {
            return changed
                .into_iter()
                .map(|instance| {
                    (
                        instance,
                        "the sweep of delays an earlier Bun left is still running".to_string(),
                    )
                })
                .collect();
        }
        let deadline = self.turn_deadline();
        let programs =
            changed.iter().map(|instance| {
                let bands = desired.get(instance).cloned().unwrap_or_default();
                async move {
                    tokio::time::timeout_at(deadline, program_delay_tree(instance, &bands)).await
                }
            });
        // `timeout_at` polls the programs before its clock, so at the
        // deadline the ones that finished still count.
        let outcomes = tokio::time::timeout_at(deadline, futures_util::future::join_all(programs))
            .await
            .unwrap_or_default();
        let mut outcomes = outcomes.into_iter();
        let mut failures = Vec::new();
        for instance in changed {
            let wanted = desired.get(&instance);
            let restart = restarts.get(&instance).copied().unwrap_or_default();
            let Some(Ok(outcome)) = outcomes.next() else {
                // Cut short half-way, the interface is in an unknown state:
                // remember it as nothing we asked for, so it's rebuilt.
                self.network_faults
                    .delays
                    .insert(instance.clone(), (u32::MAX, Vec::new()));
                failures.push((
                    instance,
                    "programming the delay did not finish within the turn; the next tick retries"
                        .to_string(),
                ));
                continue;
            };
            match outcome {
                Ok(()) => match wanted {
                    Some(wanted) => {
                        self.network_faults
                            .delays
                            .insert(instance, (restart, wanted.clone()));
                    }
                    None => {
                        self.network_faults.delays.remove(&instance);
                    }
                },
                // A caller without its own namespace (host networking)
                // can't be shaped; remember that so we don't retry every
                // tick, and report it once.
                Err(error @ NetnsCommandError::NoNamespace { .. }) => {
                    if let Some(wanted) = wanted {
                        self.network_faults
                            .delays
                            .insert(instance.clone(), (restart, wanted.clone()));
                        failures.push((instance, error.to_string()));
                    } else {
                        self.network_faults.delays.remove(&instance);
                    }
                }
                Err(error) => {
                    self.network_faults.delays.remove(&instance);
                    failures.push((instance, delay_error_hint(&error)));
                }
            }
        }
        failures
    }

    /// Check that a drop or partition can take effect here before reporting
    /// it installed: the target's VIP is known, and a source-scoped fault has
    /// at least one local source instance with a provable cgroup.
    #[cfg(all(feature = "ebpf", target_os = "linux"))]
    pub(super) async fn check_connect_fault(
        &mut self,
        rule: &crate::smoker::types::FaultRule,
    ) -> Result<(), String> {
        let services = self.merged_service_map();
        if fault_vip_port(&services, rule).is_none() {
            return Err(format!(
                "no service VIP exists for {}/{}",
                rule.namespace.as_deref().unwrap_or("default"),
                rule.target_service
            ));
        }
        let Some(source) = rule.fault_type.source_app() else {
            return Ok(());
        };
        let callers = self.local_callers().await;
        let proven = callers.iter().any(|caller| {
            caller.cgroup_id.is_some()
                && crate::smoker::network::applies_to_caller(rule, &caller.app, &caller.namespace)
        });
        if proven {
            Ok(())
        } else {
            Err(format!(
                "no running instance of source app {source} on this node has a verified workload cgroup"
            ))
        }
    }

    /// Converge the eBPF `fault_connect_map` on what the active drop and
    /// partition faults ask for (see `smoker::network`).
    #[cfg(all(feature = "ebpf", target_os = "linux"))]
    pub(super) async fn reconcile_connect_faults(&mut self) -> Result<(), String> {
        use crate::smoker::bpf_maps;
        use crate::smoker::bpf_types::{
            BpfConnectFaultValue, FAULT_ACTION_DROP, FAULT_ACTION_PARTITION, partition_fault_key,
        };
        use crate::smoker::network::{
            ConnectFaultAction, connect_fault_changes, connections_to_cut, desired_connect_faults,
            lands,
        };

        let Some(handle) = self.onion_ebpf.clone() else {
            return Ok(());
        };
        let callers = self.local_callers().await;
        let services = self.merged_service_map();
        let desired = desired_connect_faults(
            self.fault_registry.iter(),
            |rule| fault_vip_port(&services, rule),
            &callers,
        );
        let changes = connect_fault_changes(&self.network_faults.connect, &desired);
        if changes.write.is_empty() && changes.delete.is_empty() {
            return Ok(());
        }

        let mut failures = Vec::new();
        let mut landed = Vec::new();
        let mut ebpf = handle.lock().await;
        for key in changes.delete {
            let bpf_key = partition_fault_key(key.virtual_ip, key.port, key.source_cgroup_id);
            match bpf_maps::delete_connect_fault(&mut ebpf.bpf, &bpf_key) {
                Ok(()) => {
                    self.network_faults.connect.remove(&key);
                }
                Err(error) => failures.push(format!("delete {key:?}: {error}")),
            }
        }
        for (key, entry) in changes.write {
            let (action, probability) = match entry.action {
                ConnectFaultAction::Drop { probability } => (FAULT_ACTION_DROP, probability),
                ConnectFaultAction::Partition => (FAULT_ACTION_PARTITION, 100),
            };
            let value = BpfConnectFaultValue {
                action,
                probability,
                _pad: [0; 6],
                delay_ns: 0,
                jitter_ns: 0,
                expires_ns: entry.expires_ns,
            };
            let bpf_key = partition_fault_key(key.virtual_ip, key.port, key.source_cgroup_id);
            match bpf_maps::write_connect_fault(&mut ebpf.bpf, bpf_key, value) {
                Ok(()) => {
                    if lands(self.network_faults.connect.get(&key), &entry) {
                        landed.push(key);
                    }
                    self.network_faults.connect.insert(key, entry);
                }
                Err(error) => failures.push(format!("write {key:?}: {error}")),
            }
        }
        drop(ebpf);

        // The hook only refuses new connections, so cut the ones already
        // open: a pooled client reconnects straight into the fault.
        let cuts = connections_to_cut(&landed, &callers, |virtual_ip, port| {
            backend_addresses(&services, virtual_ip, port)
        });
        // The cuts must land before the fault is reported installed, or a
        // pooled client's next request still goes through. Every caller's
        // `ss` runs at once under the turn's runtime budget. The ones still
        // running at the deadline wait in `late_cuts` for whoever answers for
        // the fault (#450): reporting it applied while they ran let a
        // frontend's pool reach redis through a partition.
        // LOOP-INLINE: every cut waits at most until the turn's runtime deadline
        let late =
            crate::smoker::network::cut_until(cuts, self.turn_deadline(), cut_open_connections)
                .await;
        self.network_faults.late_cuts.extend(late);
        if failures.is_empty() {
            Ok(())
        } else {
            Err(format!(
                "failed to program fault_connect_map: {}",
                failures.join("; ")
            ))
        }
    }

    /// Without the eBPF data path there is no connect map to converge.
    #[cfg(not(all(feature = "ebpf", target_os = "linux")))]
    pub(super) async fn reconcile_connect_faults(&mut self) -> Result<(), String> {
        Ok(())
    }
}
