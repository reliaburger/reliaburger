//! Cluster routing for workload faults.
//!
//! A workload fault acts on processes, and processes live on one node. The
//! node that receives `POST /v1/fault` therefore has to work out which nodes
//! own the target instances and send each of them its share. This module is
//! the pure half of that: given the cluster's live instance list, it plans
//! one request per owning node, and it counts replicas cluster-wide so the
//! replica-minimum rail judges the whole service rather than one node's
//! slice of it. The API layer gathers the evidence and does the sending.
//!
//! Network faults are the exception. They act where a connection *starts*
//! (the eBPF connect hook, the traffic-control qdisc and the DNS responder
//! all run on the caller's node), so [`plan_network_fault`] sends them to the
//! nodes that run the callers rather than the target.

use std::collections::BTreeMap;

use super::types::{FaultRequest, FaultSummary, FaultType, ReplicaEvidence};

/// One instance of the target service, as a node reported it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkloadInstance {
    /// Node that runs (and can signal) the instance.
    pub node: String,
    /// Node-local instance id, e.g. `default/web-0`.
    pub instance_id: String,
    /// Whether the instance is running and so can be faulted.
    pub running: bool,
}

/// A fault request addressed to the node that owns its targets.
#[derive(Debug, Clone, PartialEq)]
pub struct RoutedFault {
    /// Owning node.
    pub node: String,
    /// The request that node applies, with `target_node` set to it.
    pub request: FaultRequest,
}

/// Why a workload fault could not be routed.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RoutingError {
    #[error("no running instances of {namespace}/{service} match the fault's target")]
    NoRunningInstances { namespace: String, service: String },
    #[error("no running instances of source app {namespace}/{app} to fault the calls from")]
    NoRunningSources { namespace: String, app: String },
    #[error("node {node} is not a live cluster member that could host a caller")]
    NoSuchCallerNode { node: String },
}

/// Split a workload fault into one request per node that owns a target.
///
/// `instances` must already be restricted to the request's service and
/// namespace. The request's own `target_instance` and `target_node` narrow the
/// candidates further. A kill of `count` replicas picks that many running
/// instances in a stable order (node, then instance id) and sends each owner a
/// kill of its share; every other fault goes to every owner of a candidate.
pub fn plan_workload_fault(
    request: &FaultRequest,
    instances: &[WorkloadInstance],
) -> Result<Vec<RoutedFault>, RoutingError> {
    let mut candidates: Vec<&WorkloadInstance> = instances
        .iter()
        .filter(|instance| instance.running)
        .filter(|instance| {
            request
                .target_instance
                .as_deref()
                .is_none_or(|target| target == instance.instance_id)
        })
        .filter(|instance| {
            request
                .target_node
                .as_deref()
                .is_none_or(|target| target == instance.node)
        })
        .collect();
    if candidates.is_empty() {
        return Err(RoutingError::NoRunningInstances {
            namespace: request
                .namespace
                .clone()
                .unwrap_or_else(|| "default".to_string()),
            service: request.target_service.clone(),
        });
    }
    candidates.sort_by(|left, right| {
        (&left.node, &left.instance_id).cmp(&(&right.node, &right.instance_id))
    });

    // Per node, how many of its instances this request should touch. `None`
    // means "all candidates there", which is what every fault but a counted
    // kill does.
    let mut shares: BTreeMap<&str, Option<u32>> = BTreeMap::new();
    match request.fault_type {
        FaultType::Kill { count } if count > 0 => {
            for instance in candidates.iter().take(count as usize) {
                let share = shares.entry(instance.node.as_str()).or_insert(Some(0));
                *share = share.map(|taken| taken + 1);
            }
        }
        _ => {
            for instance in &candidates {
                shares.insert(instance.node.as_str(), None);
            }
        }
    }

    Ok(shares
        .into_iter()
        .map(|(node, share)| {
            let mut routed = request.clone();
            routed.target_node = Some(node.to_string());
            if let (FaultType::Kill { .. }, Some(count)) = (&request.fault_type, share) {
                routed.fault_type = FaultType::Kill { count };
            }
            RoutedFault {
                node: node.to_string(),
                request: routed,
            }
        })
        .collect())
}

/// Split a network fault into one request per node that runs a caller.
///
/// A fault limited to one source app (`--from`) goes to every node running
/// that app; `sources` must already be restricted to that app and the fault's
/// namespace. A fault on every caller goes to every live node in `nodes`,
/// because any of them may run, now or later, something that calls the
/// target. The request's `target_node` narrows either set to one node.
pub fn plan_network_fault(
    request: &FaultRequest,
    nodes: &[String],
    sources: &[WorkloadInstance],
) -> Result<Vec<RoutedFault>, RoutingError> {
    let namespace = request
        .namespace
        .clone()
        .unwrap_or_else(|| "default".to_string());
    let mut callers: Vec<&str> = match request.fault_type.source_app() {
        Some(source) => {
            let running: Vec<&str> = sources
                .iter()
                .filter(|instance| instance.running)
                .map(|instance| instance.node.as_str())
                .collect();
            if running.is_empty() {
                return Err(RoutingError::NoRunningSources {
                    namespace,
                    app: source.to_string(),
                });
            }
            running
        }
        None => nodes.iter().map(String::as_str).collect(),
    };
    callers.sort_unstable();
    callers.dedup();
    if let Some(node) = request.target_node.as_deref() {
        if !callers.contains(&node) {
            return Err(match request.fault_type.source_app() {
                Some(source) => RoutingError::NoRunningSources {
                    namespace,
                    app: format!("{source} on {node}"),
                },
                None => RoutingError::NoSuchCallerNode {
                    node: node.to_string(),
                },
            });
        }
        callers.retain(|caller| *caller == node);
    }
    Ok(callers
        .into_iter()
        .map(|node| {
            let mut routed = request.clone();
            routed.target_node = Some(node.to_string());
            RoutedFault {
                node: node.to_string(),
                request: routed,
            }
        })
        .collect())
}

/// Count the target service's replicas and already-faulted replicas across
/// the whole cluster, for the replica-minimum rail.
///
/// `instances` is the same service-restricted list the planner takes;
/// `faults` is every node's active fault list. Only pauses count: a paused
/// replica is out of service for the fault's lifetime, while a kill is
/// instantaneous and a network or resource fault leaves the replica running.
/// A network fault is also installed on every caller's node, so counting it
/// would count one fault several times.
pub fn replica_evidence(
    request: &FaultRequest,
    instances: &[WorkloadInstance],
    faults: &[FaultSummary],
) -> ReplicaEvidence {
    let replicas = instances.iter().filter(|instance| instance.running).count() as u32;
    let paused = FaultType::Pause.to_string();
    let faulted_replicas = faults
        .iter()
        .filter(|fault| fault.target_service == request.target_service)
        .filter(|fault| fault.fault_type == paused)
        .count() as u32;
    ReplicaEvidence {
        replicas,
        faulted_replicas,
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn request(fault_type: FaultType) -> FaultRequest {
        FaultRequest {
            fault_type,
            target_service: "web".to_string(),
            namespace: Some("default".to_string()),
            target_instance: None,
            target_node: None,
            duration: Duration::from_secs(0),
            injected_by: String::new(),
            reason: None,
            include_leader: false,
            override_safety: false,
            acknowledged: true,
        }
    }

    fn instance(node: &str, id: &str) -> WorkloadInstance {
        WorkloadInstance {
            node: node.to_string(),
            instance_id: id.to_string(),
            running: true,
        }
    }

    fn spread() -> Vec<WorkloadInstance> {
        vec![
            instance("node-3", "default/web-0"),
            instance("node-1", "default/web-0"),
            instance("node-2", "default/web-0"),
        ]
    }

    #[test]
    fn a_single_kill_goes_to_exactly_one_owner() {
        let plan = plan_workload_fault(&request(FaultType::Kill { count: 1 }), &spread()).unwrap();
        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].node, "node-1");
        assert_eq!(plan[0].request.target_node.as_deref(), Some("node-1"));
        assert_eq!(plan[0].request.fault_type, FaultType::Kill { count: 1 });
    }

    #[test]
    fn a_counted_kill_is_split_by_how_many_each_owner_holds() {
        let mut instances = spread();
        instances.push(instance("node-1", "default/web-1"));
        let plan = plan_workload_fault(&request(FaultType::Kill { count: 3 }), &instances).unwrap();
        let shares: Vec<_> = plan
            .iter()
            .map(|routed| (routed.node.as_str(), routed.request.fault_type.clone()))
            .collect();
        assert_eq!(
            shares,
            vec![
                ("node-1", FaultType::Kill { count: 2 }),
                ("node-2", FaultType::Kill { count: 1 }),
            ]
        );
    }

    #[test]
    fn uncounted_faults_reach_every_owner() {
        let plan = plan_workload_fault(&request(FaultType::Pause), &spread()).unwrap();
        let nodes: Vec<_> = plan.iter().map(|routed| routed.node.as_str()).collect();
        assert_eq!(nodes, vec!["node-1", "node-2", "node-3"]);
        assert!(
            plan.iter()
                .all(|routed| routed.request.fault_type == FaultType::Pause)
        );
    }

    #[test]
    fn an_instance_and_node_target_narrow_to_that_owner() {
        let mut targeted = request(FaultType::Kill { count: 0 });
        targeted.target_instance = Some("default/web-0".to_string());
        targeted.target_node = Some("node-2".to_string());
        let plan = plan_workload_fault(&targeted, &spread()).unwrap();
        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].node, "node-2");
        assert_eq!(
            plan[0].request.target_instance.as_deref(),
            Some("default/web-0")
        );
    }

    #[test]
    fn stopped_instances_are_neither_targets_nor_counted_replicas() {
        let mut instances = spread();
        instances[1].running = false;
        let plan = plan_workload_fault(&request(FaultType::Kill { count: 1 }), &instances).unwrap();
        assert_eq!(plan[0].node, "node-2");
        let evidence = replica_evidence(&request(FaultType::Kill { count: 1 }), &instances, &[]);
        assert_eq!(evidence.replicas, 2);
    }

    #[test]
    fn nothing_to_target_is_an_error_not_an_empty_plan() {
        let mut missing = request(FaultType::Kill { count: 1 });
        missing.target_node = Some("node-9".to_string());
        assert_eq!(
            plan_workload_fault(&missing, &spread()),
            Err(RoutingError::NoRunningInstances {
                namespace: "default".to_string(),
                service: "web".to_string(),
            })
        );
    }

    fn nodes() -> Vec<String> {
        ["node-1", "node-2", "node-3"].map(str::to_string).to_vec()
    }

    #[test]
    fn a_fault_on_every_caller_goes_to_every_live_node() {
        let plan = plan_network_fault(&request(FaultType::DnsNxdomain), &nodes(), &[]).unwrap();
        let routed: Vec<_> = plan
            .iter()
            .map(|routed| (routed.node.as_str(), routed.request.target_node.as_deref()))
            .collect();
        assert_eq!(
            routed,
            vec![
                ("node-1", Some("node-1")),
                ("node-2", Some("node-2")),
                ("node-3", Some("node-3")),
            ]
        );
    }

    #[test]
    fn a_fault_from_one_source_goes_only_where_the_source_runs() {
        let partition = request(FaultType::Partition {
            source_app: Some("frontend".to_string()),
        });
        let mut stopped = instance("node-1", "default/frontend-1");
        stopped.running = false;
        let sources = vec![
            instance("node-3", "default/frontend-0"),
            instance("node-3", "default/frontend-2"),
            stopped,
        ];
        let plan = plan_network_fault(&partition, &nodes(), &sources).unwrap();
        let routed: Vec<_> = plan.iter().map(|routed| routed.node.as_str()).collect();
        assert_eq!(routed, vec!["node-3"]);
    }

    #[test]
    fn a_source_that_runs_nowhere_is_an_error() {
        let delay = request(FaultType::Delay {
            delay_ns: 1,
            jitter_ns: 0,
            source_app: Some("frontend".to_string()),
        });
        assert_eq!(
            plan_network_fault(&delay, &nodes(), &[]),
            Err(RoutingError::NoRunningSources {
                namespace: "default".to_string(),
                app: "frontend".to_string(),
            })
        );
    }

    #[test]
    fn a_named_node_narrows_a_network_fault_to_that_node() {
        let mut drop = request(FaultType::Drop { probability: 50 });
        drop.target_node = Some("node-2".to_string());
        let plan = plan_network_fault(&drop, &nodes(), &[]).unwrap();
        assert_eq!(plan.len(), 1);
        assert_eq!(plan[0].node, "node-2");

        drop.target_node = Some("node-9".to_string());
        assert_eq!(
            plan_network_fault(&drop, &nodes(), &[]),
            Err(RoutingError::NoSuchCallerNode {
                node: "node-9".to_string()
            })
        );
    }

    #[test]
    fn network_faults_do_not_count_as_unavailable_replicas() {
        let fault = |kind: &str| FaultSummary {
            id: 1,
            fault_type: kind.to_string(),
            target_service: "web".to_string(),
            target_instance: None,
            target_node: None,
            remaining_secs: 30,
            injected_by: "ops".to_string(),
            node: None,
            routed: Vec::new(),
        };
        let evidence = replica_evidence(
            &request(FaultType::Kill { count: 1 }),
            &spread(),
            &[fault("delay 300ms"), fault("dns nxdomain"), fault("pause")],
        );
        assert_eq!(evidence.faulted_replicas, 1);
    }

    #[test]
    fn replica_evidence_counts_faults_from_every_node() {
        let fault = |service: &str| FaultSummary {
            id: 1,
            fault_type: "pause".to_string(),
            target_service: service.to_string(),
            target_instance: None,
            target_node: None,
            remaining_secs: 30,
            injected_by: "ops".to_string(),
            node: None,
            routed: Vec::new(),
        };
        let evidence = replica_evidence(
            &request(FaultType::Kill { count: 1 }),
            &spread(),
            &[fault("web"), fault("web"), fault("api")],
        );
        assert_eq!(
            evidence,
            ReplicaEvidence {
                replicas: 3,
                faulted_replicas: 2,
            }
        );
    }
}
