//! `relish trace`: the probes the agent runs from a workload, and the
//! evidence it gathers about the path between two services.

use super::*;

/// A trace starts processes inside a workload and may remain in flight for two
/// eight-second probe bounds. Refuse excess work instead of building an
/// unbounded queue of authenticated diagnostic tasks.
pub(super) const MAX_CONCURRENT_TRACES: usize = 8;

/// An immutable, owned connectivity trace that can run outside the agent
/// command loop. Workload probes have explicit timeouts, but even a bounded
/// probe must not delay status, shutdown or another control-plane command.
pub(super) struct PreparedTrace<G> {
    pub(super) _permit: tokio::sync::OwnedSemaphorePermit,
    pub(super) shutdown: CancellationToken,
    pub(super) grill: G,
    pub(super) source_instance: InstanceId,
    pub(super) request: crate::onion::trace::TraceRequest,
    pub(super) internal_destination: bool,
    pub(super) source_node: String,
    pub(super) service: Option<crate::onion::types::ServiceEntry>,
    pub(super) destination_port: u16,
    pub(super) dns_name: String,
    pub(super) expected_vip: Option<String>,
    /// Active faults that act on this source's calls to the destination.
    pub(super) faults: Vec<crate::onion::trace::PathFault>,
    /// TCP connects to make.
    pub(super) count: u32,
    #[cfg(all(feature = "ebpf", target_os = "linux"))]
    pub(super) onion_ebpf:
        Option<std::sync::Arc<tokio::sync::Mutex<crate::onion::ebpf::loader::OnionEbpf>>>,
}

impl<G: Grill + Clone + 'static> PreparedTrace<G> {
    /// Trace DNS, live service/firewall state and a TCP connection from one
    /// running workload. The command strings are fixed; request values are
    /// positional shell arguments and can never become shell syntax.
    pub(super) async fn run(self) -> Result<crate::onion::trace::TraceResult, BunError> {
        use crate::onion::trace::TraceResult;

        let dns_probe = self
            .run_workload_trace_probe(
                &self.source_instance,
                trace_dns_command(&self.dns_name),
                "__RB_TRACE_DNS_STATUS__",
                std::time::Duration::from_secs(8),
            )
            .await;
        let dns_step = trace_dns_step(&self.dns_name, dns_probe, self.expected_vip.as_deref());

        let service_step = self
            .trace_service_state(self.service.as_ref(), self.internal_destination)
            .await;
        let firewall_step = self
            .trace_firewall_state(
                &self.source_instance,
                self.service.as_ref(),
                self.internal_destination,
            )
            .await;
        let faults_step = crate::onion::trace::path_faults_step(
            4,
            &self.faults,
            self.trace_fault_evidence().await,
        );

        let connect_host = self
            .expected_vip
            .as_deref()
            .unwrap_or(self.request.destination.as_str());
        // One connect keeps the old three-second patience; a series waits
        // two seconds per connect so the whole trace stays inside the API's
        // deadline even when every connect hangs.
        let wait_secs = if self.count > 1 { 2 } else { 3 };
        let tcp_probe = self
            .run_workload_trace_probe(
                &self.source_instance,
                trace_tcp_command(connect_host, self.destination_port, self.count, wait_secs),
                "__RB_TRACE_TCP_STATUS__",
                std::time::Duration::from_secs(u64::from(self.count * (wait_secs + 1)) + 5),
            )
            .await;
        let tcp_step = crate::onion::trace::tcp_probe_step(
            5,
            &format!("{connect_host}:{}", self.destination_port),
            tcp_probe.clone(),
        );
        let connects = tcp_probe
            .ok()
            .and_then(|probe| crate::onion::trace::summarise_connects(&probe.attempts));
        let latency_ms = connects.as_ref().and_then(|summary| summary.median_ms);

        let steps = vec![dns_step, service_step, firewall_step, faults_step, tcp_step];
        let overall_result = crate::onion::trace::overall_verdict(&steps);
        Ok(TraceResult {
            schema_version: crate::onion::trace::TRACE_SCHEMA_VERSION,
            source: format!("{}/{}", self.request.source_namespace, self.request.source),
            destination: if self.internal_destination {
                format!(
                    "{}/{}",
                    self.request.destination_namespace, self.request.destination
                )
            } else {
                self.request.destination.clone()
            },
            destination_port: self.destination_port,
            source_node: self.source_node,
            steps,
            overall_result,
            latency_ms,
            connects,
        })
    }

    /// Live evidence for the faults on this path: the `fault_connect_map`
    /// entries the connect hook would find for this source (its own cgroup
    /// first, then every caller), and the netem delays on its interface.
    pub(super) async fn trace_fault_evidence(&self) -> Vec<String> {
        if self.faults.is_empty() {
            return Vec::new();
        }
        #[cfg_attr(not(target_os = "linux"), allow(unused_mut))]
        let mut evidence = Vec::new();
        #[cfg(all(feature = "ebpf", target_os = "linux"))]
        if let (Some(handle), Some(service)) = (&self.onion_ebpf, &self.service) {
            let cgroup = self
                .grill
                .workload_cgroup(&self.source_instance)
                .await
                .ok()
                .flatten();
            let virtual_ip = service.vip.to_network_byte_order();
            let port = service.port.to_be();
            let mut ebpf = handle.lock().await;
            for (label, source_cgroup_id) in
                [("this source's cgroup", cgroup), ("every caller", Some(0))]
            {
                let Some(source_cgroup_id) = source_cgroup_id else {
                    continue;
                };
                let key = crate::smoker::bpf_types::partition_fault_key(
                    virtual_ip,
                    port,
                    source_cgroup_id,
                );
                match crate::smoker::bpf_maps::read_connect_fault(&mut ebpf.bpf, &key) {
                    Ok(Some(value)) => evidence.push(format!(
                        "live fault_connect_map entry for {label}: {}",
                        describe_connect_fault(&value)
                    )),
                    Ok(None) => {}
                    Err(error) => {
                        evidence.push(format!("fault_connect_map could not be read: {error}"))
                    }
                }
            }
        }
        #[cfg(target_os = "linux")]
        if self
            .faults
            .iter()
            .any(|fault| fault.kind == crate::onion::trace::PathFaultKind::Delay)
            && let Ok(shown) = crate::smoker::network::run_in_instance_netns(
                &self.source_instance.0,
                "tc",
                &crate::smoker::network::delay_show_args(),
            )
            .await
        {
            evidence.extend(
                crate::smoker::network::installed_delays(&shown)
                    .into_iter()
                    .map(|delay| format!("live netem on the source's eth0: {delay}")),
            );
        }
        evidence
    }

    pub(super) async fn run_workload_trace_probe(
        &self,
        source_instance: &InstanceId,
        command: Vec<String>,
        marker: &str,
        timeout: std::time::Duration,
    ) -> Result<crate::onion::trace::ProbeOutput, String> {
        let future = self.grill.exec(source_instance, &command);
        let result = tokio::select! {
            _ = self.shutdown.cancelled() => {
                return Err("workload probe cancelled because the agent is shutting down".to_string());
            }
            result = tokio::time::timeout(timeout, future) => result,
        };
        match result {
            Ok(Ok(output)) => crate::onion::trace::parse_probe_output(&output, marker)
                .ok_or_else(|| "source image lacks a usable POSIX shell or probe tool".to_string()),
            Ok(Err(error)) => Err(format!("workload probe could not start: {error}")),
            Err(_) => Err(format!(
                "workload probe timed out after {} seconds",
                timeout.as_secs()
            )),
        }
    }

    pub(super) async fn trace_service_state(
        &self,
        service: Option<&crate::onion::types::ServiceEntry>,
        internal_destination: bool,
    ) -> crate::onion::trace::TraceStep {
        use crate::onion::trace::{TraceEvidence, TraceStep, TraceVerdict};
        if !internal_destination {
            return TraceStep {
                step_number: 2,
                name: "Service and eBPF state".to_string(),
                evidence: TraceEvidence::Inferred,
                details: vec![
                    "external destinations bypass the internal service and backend maps"
                        .to_string(),
                ],
                verdict: TraceVerdict::Pass,
            };
        }
        let Some(service) = service else {
            return TraceStep {
                step_number: 2,
                name: "Service and eBPF state".to_string(),
                evidence: TraceEvidence::Observed,
                details: Vec::new(),
                verdict: TraceVerdict::Fail {
                    reason: "destination is absent from the live userspace service map".to_string(),
                },
            };
        };
        let healthy = service
            .backends
            .iter()
            .filter(|backend| backend.healthy)
            .count();
        let mut details = vec![format!(
            "userspace service map: VIP {}, {} of {} backends healthy",
            service.vip,
            healthy,
            service.backends.len()
        )];
        details.extend(describe_backends(service));
        if healthy == 0 {
            return TraceStep {
                step_number: 2,
                name: "Service and eBPF state".to_string(),
                evidence: TraceEvidence::Observed,
                details,
                verdict: TraceVerdict::Fail {
                    reason: "live service state has no healthy backend".to_string(),
                },
            };
        }

        #[cfg(all(feature = "ebpf", target_os = "linux"))]
        if let Some(handle) = &self.onion_ebpf {
            let bpf_map = crate::onion::ebpf::maps::BpfServiceMap::new();
            let mut ebpf = handle.lock().await;
            return match bpf_map.read_backends(&mut ebpf, service.vip, service.port) {
                Ok(Some(value)) => {
                    let kernel_healthy = value
                        .backends
                        .iter()
                        .take(value.count as usize)
                        .filter(|backend| backend.healthy == 1)
                        .count();
                    details.push(format!(
                        "live backend_map: {} entries, {kernel_healthy} healthy",
                        value.count
                    ));
                    details.extend(value.backends.iter().take(value.count.min(5) as usize).map(
                        |backend| {
                            format!(
                                "  kernel backend {}:{} ({})",
                                std::net::Ipv4Addr::from(u32::from_be(backend.host_ip)),
                                u16::from_be(backend.host_port),
                                if backend.healthy == 1 {
                                    "healthy"
                                } else {
                                    "unhealthy"
                                }
                            )
                        },
                    ));
                    let verdict = if value.count == 0 || kernel_healthy == 0 {
                        TraceVerdict::Fail {
                            reason: "live eBPF backend map has no healthy backend".to_string(),
                        }
                    } else {
                        TraceVerdict::Pass
                    };
                    TraceStep {
                        step_number: 2,
                        name: "Service and eBPF state".to_string(),
                        evidence: TraceEvidence::Observed,
                        details,
                        verdict,
                    }
                }
                Ok(None) => TraceStep {
                    step_number: 2,
                    name: "Service and eBPF state".to_string(),
                    evidence: TraceEvidence::Observed,
                    details,
                    verdict: TraceVerdict::Fail {
                        reason: "service exists in userspace but is absent from live backend_map"
                            .to_string(),
                    },
                },
                Err(error) => TraceStep {
                    step_number: 2,
                    name: "Service and eBPF state".to_string(),
                    evidence: TraceEvidence::Unavailable,
                    details,
                    verdict: TraceVerdict::Unknown {
                        reason: format!("live backend_map could not be read: {error}"),
                    },
                },
            };
        }

        details.push(
            "no live eBPF backend map is attached; this step is inferred from userspace state"
                .to_string(),
        );
        TraceStep {
            step_number: 2,
            name: "Service and eBPF state".to_string(),
            evidence: TraceEvidence::Inferred,
            details,
            verdict: TraceVerdict::Pass,
        }
    }

    pub(super) async fn trace_firewall_state(
        &self,
        source_instance: &InstanceId,
        service: Option<&crate::onion::types::ServiceEntry>,
        internal_destination: bool,
    ) -> crate::onion::trace::TraceStep {
        use crate::onion::trace::{TraceEvidence, TraceStep, TraceVerdict};
        let unknown = |reason: String| TraceStep {
            step_number: 3,
            name: "Firewall state".to_string(),
            evidence: TraceEvidence::Unavailable,
            details: Vec::new(),
            verdict: TraceVerdict::Unknown { reason },
        };

        #[cfg(all(feature = "ebpf", target_os = "linux"))]
        if let Some(handle) = &self.onion_ebpf {
            let cgroup_id = match self.grill.workload_cgroup(source_instance).await {
                Ok(Some(cgroup_id)) => cgroup_id,
                Ok(None) => {
                    return unknown("runtime does not expose a verified workload cgroup".into());
                }
                Err(error) => {
                    return unknown(format!("source workload identity is unavailable: {error}"));
                }
            };
            let mut ebpf = handle.lock().await;
            if !internal_destination {
                return match crate::sesame::egress::egress_enforced(&mut ebpf.bpf, cgroup_id) {
                    Ok(false) => TraceStep {
                        step_number: 3,
                        name: "Firewall state".to_string(),
                        evidence: TraceEvidence::Observed,
                        details: vec![
                            "live egress_enabled_map has no policy for the source cgroup; external traffic passes through".to_string(),
                        ],
                        verdict: TraceVerdict::Pass,
                    },
                    Ok(true) => unknown(
                        "live egress enforcement is active; the exact hostname decision is observed by the TCP probe but cannot yet be attributed to one exact/CIDR map entry"
                            .to_string(),
                    ),
                    Err(error) => unknown(format!("live egress map could not be read: {error}")),
                };
            }
            let Some(service) = service else {
                return unknown("destination service state is unavailable".to_string());
            };
            return match crate::sesame::firewall::read_firewall_state(
                &mut ebpf.bpf,
                cgroup_id,
                service.app_id,
            ) {
                Ok(state) => {
                    let verdict = crate::onion::trace::evaluate_firewall(
                        state.source_namespace_id,
                        service.namespace_id,
                        state.action,
                    );
                    TraceStep {
                        step_number: 3,
                        name: "Firewall state".to_string(),
                        evidence: TraceEvidence::Observed,
                        details: vec![format!(
                            "live maps: source cgroup {cgroup_id}, source namespace {:?}, destination namespace {}, action {:?}",
                            state.source_namespace_id, service.namespace_id, state.action
                        )],
                        verdict,
                    }
                }
                Err(error) => unknown(format!("live firewall maps could not be read: {error}")),
            };
        }

        let _ = (source_instance, service, internal_destination);
        unknown("no live eBPF firewall maps are attached on this node".to_string())
    }
}

/// The post-rewrite backend addresses behind a service's (virtual IP, port),
/// both in network byte order: what a caller's sockets are connected to once
/// the connect hook has picked a backend.
#[cfg(all(feature = "ebpf", target_os = "linux"))]
pub(super) fn backend_addresses(
    services: &crate::onion::service_map::ServiceMap,
    virtual_ip: u32,
    port: u16,
) -> Vec<std::net::SocketAddrV4> {
    services
        .resolve_all()
        .into_iter()
        .filter(|entry| {
            entry.vip.to_network_byte_order() == virtual_ip && entry.port.to_be() == port
        })
        .flat_map(|entry| entry.backends.iter())
        .map(|backend| std::net::SocketAddrV4::new(backend.node_ip, backend.host_port))
        .collect()
}

pub(super) const DNS_TRACE_SCRIPT: &str = r#"
output=$(nslookup "$1" 2>&1)
status=$?
printf '%s\n' "$output"
printf '__RB_TRACE_DNS_STATUS__=%s\n' "$status"
"#;

// Each connect is timed inside the container, so the figure excludes the
// cost of exec'ing the probe. `date +%s%N` gives nanoseconds where the image's
// `date` supports `%N`; BusyBox often doesn't, so `/proc/uptime` (10 ms) is
// read too, and the parser uses whichever is plausible. nc's own chatter (the
// OpenBSD "Connection ... succeeded!" line) is dropped on success.
pub(super) const TCP_TRACE_SCRIPT: &str = r#"
count=$3
i=0
status=1
while [ "$i" -lt "$count" ]; do
  up_start=
  up_end=
  read -r up_start _ < /proc/uptime 2>/dev/null
  start=$(date +%s%N 2>/dev/null)
  output=$(nc -z -w "$4" "$1" "$2" 2>&1)
  status=$?
  end=$(date +%s%N 2>/dev/null)
  read -r up_end _ < /proc/uptime 2>/dev/null
  [ "$status" -ne 0 ] && [ -n "$output" ] && printf '%s\n' "$output"
  printf '__RB_TRACE_TCP_ATTEMPT__=%s %s %s %s %s\n' "$status" "$start" "$end" "$up_start" "$up_end"
  i=$((i + 1))
done
printf '__RB_TRACE_TCP_STATUS__=%s\n' "$status"
"#;

pub(super) fn trace_dns_command(name: &str) -> Vec<String> {
    vec![
        "sh".to_string(),
        "-c".to_string(),
        DNS_TRACE_SCRIPT.to_string(),
        "reliaburger-path".to_string(),
        name.to_string(),
    ]
}

pub(super) fn trace_tcp_command(host: &str, port: u16, count: u32, wait_secs: u32) -> Vec<String> {
    vec![
        "sh".to_string(),
        "-c".to_string(),
        TCP_TRACE_SCRIPT.to_string(),
        "reliaburger-path".to_string(),
        host.to_string(),
        port.to_string(),
        count.to_string(),
        wait_secs.to_string(),
    ]
}

/// Name a service's backends, and which one the VIP picks, for the trace.
pub(super) fn describe_backends(service: &crate::onion::types::ServiceEntry) -> Vec<String> {
    let mut details: Vec<String> = service
        .backends
        .iter()
        .take(5)
        .map(|backend| {
            format!(
                "  backend {} at {}:{} ({})",
                backend.instance_id,
                backend.node_ip,
                backend.host_port,
                if backend.healthy {
                    "healthy"
                } else {
                    "unhealthy"
                }
            )
        })
        .collect();
    let healthy: Vec<_> = service
        .backends
        .iter()
        .filter(|backend| backend.healthy)
        .collect();
    match healthy.as_slice() {
        [] => {}
        [only] => details.push(format!(
            "the VIP sends every connect to {} at {}:{}",
            only.instance_id, only.node_ip, only.host_port
        )),
        several => details.push(format!(
            "the VIP spreads connects round-robin over {} healthy backends",
            several.len()
        )),
    }
    details
}

/// Describe a live `fault_connect_map` value.
#[cfg(all(feature = "ebpf", target_os = "linux"))]
pub(super) fn describe_connect_fault(
    value: &crate::smoker::bpf_types::BpfConnectFaultValue,
) -> String {
    let action = match value.action {
        crate::smoker::bpf_types::FAULT_ACTION_PARTITION => "partition".to_string(),
        crate::smoker::bpf_types::FAULT_ACTION_DROP => format!("drop {}%", value.probability),
        other => format!("action {other}"),
    };
    let now = crate::smoker::types::monotonic_now_ns();
    let left = value.expires_ns.saturating_sub(now) / 1_000_000_000;
    format!("{action}, expires in {left}s")
}

pub(super) fn trace_dns_step(
    name: &str,
    probe: Result<crate::onion::trace::ProbeOutput, String>,
    expected_value: Option<&str>,
) -> crate::onion::trace::TraceStep {
    use crate::onion::trace::{TraceEvidence, TraceStep, TraceVerdict};
    let step_name = "DNS query".to_string();
    match probe {
        Ok(probe) => {
            let expected_answer = expected_value.is_none_or(|expected| {
                expected
                    .parse::<std::net::IpAddr>()
                    .is_ok_and(|address| probe.dns_answers().contains(&address))
            });
            let details = crate::onion::trace::dns_details(name, &probe);
            let verdict = if probe.status == 0 {
                if let Some(expected) = expected_value
                    && !expected_answer
                {
                    TraceVerdict::Fail {
                        reason: format!(
                            "probe succeeded but its DNS answers did not include exact address {expected}"
                        ),
                    }
                } else {
                    TraceVerdict::Pass
                }
            } else if probe.status == 126 || probe.status == 127 {
                TraceVerdict::Unknown {
                    reason: "source image does not provide the fixed DNS query probe tool"
                        .to_string(),
                }
            } else {
                TraceVerdict::Fail {
                    reason: format!("DNS query exited with status {}", probe.status),
                }
            };
            TraceStep {
                step_number: 1,
                name: step_name,
                evidence: TraceEvidence::Observed,
                details,
                verdict,
            }
        }
        Err(reason) => TraceStep {
            step_number: 1,
            name: step_name,
            evidence: TraceEvidence::Unavailable,
            details: vec![format!("query {name}")],
            verdict: TraceVerdict::Unknown { reason },
        },
    }
}

impl<G: Grill + Clone + 'static> BunAgent<G> {
    /// Execute a command inside a running instance of an app.
    ///
    /// Finds the first running instance of the app in the given namespace
    /// and delegates to `grill.exec()`. In Phase 1 (ProcessGrill), this
    /// just spawns the command directly. Phase 3+ will add namespace entry.
    /// Resolve the id of a running instance of `app_name` in `namespace`, or
    /// `AppNotFound`. Cheap and synchronous, so it runs on the command loop
    /// before the actual exec is spawned off it (H3).
    pub(super) fn resolve_running_instance(
        &self,
        app_name: &str,
        namespace: &str,
    ) -> Result<InstanceId, BunError> {
        self.supervisor
            .list_instances()
            .into_iter()
            .find(|i| {
                i.app_name == app_name
                    && i.namespace == namespace
                    && i.state == ContainerState::Running
            })
            .map(|i| i.id.clone())
            .ok_or_else(|| BunError::AppNotFound {
                app_name: app_name.to_string(),
                namespace: namespace.to_string(),
            })
    }

    /// Capture the immutable state needed by a connectivity trace. The slow
    /// workload and kernel observations run later on a spawned task.
    pub(super) fn prepare_trace(
        &self,
        request: crate::onion::trace::TraceRequest,
        internal_destination: bool,
        source_node: String,
    ) -> Result<PreparedTrace<G>, BunError> {
        if request.port == Some(0) {
            return Err(BunError::SecurityError {
                reason: "path destination port must be between 1 and 65535".to_string(),
            });
        }
        let source_instance = self
            .supervisor
            .list_instances()
            .into_iter()
            .find(|instance| {
                instance.app_name == request.source
                    && instance.namespace == request.source_namespace
                    && instance.state == ContainerState::Running
            })
            .map(|instance| instance.id.clone())
            .ok_or_else(|| BunError::AppNotFound {
                app_name: request.source.clone(),
                namespace: request.source_namespace.clone(),
            })?;

        let service_id = crate::onion::service_id::ServiceId::new(
            &request.destination_namespace,
            &request.destination,
        );
        let merged_services = self.merged_service_map();
        let service = internal_destination
            .then(|| merged_services.resolve(&service_id).cloned())
            .flatten();
        let destination_port = request
            .port
            .or_else(|| service.as_ref().map(|entry| entry.port))
            .ok_or_else(|| BunError::SecurityError {
                reason: "external path destination requires an explicit port".to_string(),
            })?;
        let dns_name = if internal_destination {
            format!(
                "{}.{}.internal",
                request.destination, request.destination_namespace
            )
        } else {
            request.destination.clone()
        };
        let expected_vip = service.as_ref().map(|entry| entry.vip.to_string());
        let count = request.count.unwrap_or(1);
        if count == 0 || count > crate::onion::trace::MAX_TRACE_CONNECTS {
            return Err(BunError::SecurityError {
                reason: format!(
                    "path probe count must be between 1 and {}",
                    crate::onion::trace::MAX_TRACE_CONNECTS
                ),
            });
        }
        let faults = if internal_destination {
            self.path_faults(&request)
        } else {
            Vec::new()
        };
        let permit = self
            .trace_slots
            .clone()
            .try_acquire_owned()
            .map_err(|_| BunError::TraceBusy)?;

        Ok(PreparedTrace {
            _permit: permit,
            shutdown: self.shutdown.clone(),
            grill: self.supervisor.grill().clone(),
            source_instance,
            request,
            internal_destination,
            source_node,
            service,
            destination_port,
            dns_name,
            expected_vip,
            faults,
            count,
            #[cfg(all(feature = "ebpf", target_os = "linux"))]
            onion_ebpf: self.onion_ebpf.clone(),
        })
    }

    /// The active network faults on this node that act on calls from the
    /// trace's source to its destination: faults on the destination (in its
    /// namespace) that either name this source or apply to every caller.
    pub(super) fn path_faults(
        &self,
        request: &crate::onion::trace::TraceRequest,
    ) -> Vec<crate::onion::trace::PathFault> {
        use crate::onion::trace::{PathFault, PathFaultKind};
        use crate::smoker::types::FaultType;

        let mut faults: Vec<PathFault> = self
            .fault_registry
            .iter()
            .filter(|rule| rule.fault_type.acts_on_callers())
            .filter(|rule| {
                rule.target_service == request.destination
                    && rule.matches_namespace(&request.destination_namespace)
            })
            .filter(|rule| {
                crate::smoker::network::applies_to_caller(
                    rule,
                    &request.source,
                    &request.source_namespace,
                )
            })
            .map(|rule| PathFault {
                id: rule.id.0,
                kind: match rule.fault_type {
                    FaultType::Partition { .. } => PathFaultKind::Partition,
                    FaultType::Drop { probability } => PathFaultKind::Drop { probability },
                    FaultType::Delay { .. } => PathFaultKind::Delay,
                    FaultType::DnsNxdomain => PathFaultKind::DnsNxdomain,
                    _ => PathFaultKind::Other,
                },
                description: rule.fault_type.to_string(),
                remaining_secs: rule.remaining().as_secs(),
            })
            .collect();
        faults.sort_by_key(|fault| fault.id);
        faults
    }
}
