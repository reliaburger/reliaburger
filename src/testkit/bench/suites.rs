//! Public-data-plane benchmark suites.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio::sync::Mutex;

use crate::bun::capabilities::{Capability, CapabilityState, ClusterCapabilities};
use crate::config::Config;
use crate::relish::client::BunClient;
use crate::smoker::types::{FaultRequest, FaultType};
use crate::testkit::TestContext;
use crate::testkit::chaos::ChaosGuard;
use crate::testkit::context::PINNED_TEST_WORKLOAD_IMAGE;
use crate::testkit::deadline::Deadline;
use crate::testkit::safety::OperationPermission;

use super::report::BenchMetric;

const CAPACITY_APPS_PER_LEASE: usize = 120;
const CAPACITY_HARD_LIMIT: usize = 8_000;

/// One benchmark selected by the runner.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SuiteKind {
    DeploySpeed,
    SchedulerThroughput,
    DiscoveryLatency,
    NetworkThroughput,
    ReconstructionTime,
    ImageDistribution,
    ClusterCapacity,
}

impl SuiteKind {
    pub(crate) const ALL: [Self; 7] = [
        Self::DeploySpeed,
        Self::SchedulerThroughput,
        Self::DiscoveryLatency,
        Self::NetworkThroughput,
        Self::ReconstructionTime,
        Self::ImageDistribution,
        Self::ClusterCapacity,
    ];

    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::DeploySpeed => "deploy_speed",
            Self::SchedulerThroughput => "scheduler_throughput",
            Self::DiscoveryLatency => "discovery_latency_p99",
            Self::NetworkThroughput => "network_throughput",
            Self::ReconstructionTime => "reconstruction_time",
            Self::ImageDistribution => "image_distribution",
            Self::ClusterCapacity => "cluster_capacity",
        }
    }

    pub(crate) fn requirements(self) -> &'static [Capability] {
        match self {
            Self::DeploySpeed | Self::SchedulerThroughput => &[Capability::ContainerRuntime],
            Self::DiscoveryLatency | Self::NetworkThroughput => {
                &[Capability::ContainerRuntime, Capability::DnsUdp]
            }
            Self::ReconstructionTime => &[
                Capability::MultiNode,
                Capability::CouncilQuorum,
                Capability::NodeKill,
            ],
            Self::ImageDistribution => &[Capability::ContainerRuntime, Capability::MultiNode],
            Self::ClusterCapacity => &[Capability::ContainerRuntime],
        }
    }
}

/// CLI switches which deliberately include or exclude disruptive probes.
#[derive(Debug, Clone, Copy)]
pub(crate) struct SuiteOptions {
    pub quick: bool,
    pub capacity: bool,
    pub disruptive: bool,
    pub yes: bool,
}

/// Why a suite may be omitted before it creates any resource.
pub(crate) fn preflight(
    suite: SuiteKind,
    options: SuiteOptions,
    capabilities: &ClusterCapabilities,
) -> Result<(), Preflight> {
    if suite == SuiteKind::ClusterCapacity && !options.capacity {
        return Err(Preflight::Skip("requires --capacity".to_string()));
    }
    if suite == SuiteKind::ClusterCapacity && !options.yes {
        return Err(Preflight::Skip(
            "cluster saturation requires --capacity --yes".to_string(),
        ));
    }
    if suite == SuiteKind::ReconstructionTime && options.quick {
        return Err(Preflight::Skip("omitted by --quick".to_string()));
    }
    if suite == SuiteKind::ImageDistribution && options.quick {
        return Err(Preflight::Skip("omitted by --quick".to_string()));
    }
    if suite == SuiteKind::ReconstructionTime && !(options.disruptive && options.yes) {
        return Err(Preflight::Skip(
            "leader failure requires --disruptive --yes".to_string(),
        ));
    }
    let operation = match suite {
        SuiteKind::ReconstructionTime => Some(OperationPermission::AlterNodeState),
        SuiteKind::ClusterCapacity => Some(OperationPermission::SaturateCapacity),
        _ => None,
    };
    if let Some(operation) = operation {
        if !capabilities
            .test_policy
            .allowed_operations
            .contains(&operation)
        {
            return Err(Preflight::Skip(format!(
                "server policy does not allow {operation}"
            )));
        }
        if capabilities.test_policy.safety_class.is_protected()
            && !capabilities.test_policy.allow_protected_mutation
        {
            return Err(Preflight::Skip(
                "server policy forbids benchmark mutation on this protected cluster".to_string(),
            ));
        }
    }

    for capability in suite.requirements() {
        match capabilities.state(*capability) {
            CapabilityState::Available => {}
            CapabilityState::Unavailable => {
                return Err(Preflight::Skip(format!("requires {capability}")));
            }
            CapabilityState::Unknown => {
                return Err(Preflight::Fail(format!(
                    "capability evidence for {capability} is unknown or stale"
                )));
            }
        }
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Preflight {
    Skip(String),
    Fail(String),
}

/// Server-owned leases retained outside the suite task.
///
/// A timed-out capacity probe may have allocated several namespaces. The
/// runner keeps this guard and can release every one even after aborting the
/// task which created them.
#[derive(Clone)]
pub(crate) struct BenchLeaseOwner {
    client: BunClient,
    ttl_seconds: u64,
    lease_ids: Arc<Mutex<Vec<String>>>,
}

impl BenchLeaseOwner {
    pub(crate) fn new(client: BunClient, ttl_seconds: u64) -> Self {
        Self {
            client,
            ttl_seconds,
            lease_ids: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub(crate) async fn create(
        &self,
        namespace: &str,
    ) -> Result<crate::testkit::lease::TestLease, String> {
        let lease = self
            .client
            .create_test_lease(self.ttl_seconds, Some(namespace))
            .await
            .map_err(|error| format!("server did not grant a resource lease: {error}"))?;
        self.lease_ids.lock().await.push(lease.lease_id.clone());
        Ok(lease)
    }

    pub(crate) async fn cleanup(&self, deadline: Deadline) -> Result<(), String> {
        let mut ids = {
            let mut ids = self.lease_ids.lock().await;
            std::mem::take(&mut *ids)
        };
        ids.reverse();
        let mut failed = Vec::new();
        for id in ids {
            match deadline
                .run(
                    "benchmark lease cleanup",
                    self.client.release_test_lease(&id),
                )
                .await
            {
                Ok(Ok(())) => {}
                Ok(Err(error)) => failed.push(format!("{id}: {error}")),
                Err(error) => failed.push(format!("{id}: {error}")),
            }
        }
        if failed.is_empty() {
            Ok(())
        } else {
            Err(format!(
                "server did not confirm benchmark cleanup: {}",
                failed.join("; ")
            ))
        }
    }
}

/// Shared input for one bounded suite task.
#[derive(Clone)]
pub(crate) struct SuiteContext {
    pub client: BunClient,
    pub capabilities: ClusterCapabilities,
    pub namespace: String,
    pub lease_id: String,
    pub leases: BenchLeaseOwner,
    pub chaos: ChaosGuard,
    pub timeout: Duration,
    pub deadline: Deadline,
    pub options: SuiteOptions,
}

impl SuiteContext {
    fn test_context(&self) -> TestContext {
        TestContext {
            client: self.client.clone(),
            namespace: self.namespace.clone(),
            lease_id: Some(self.lease_id.clone()),
            chaos_guard: self.chaos.clone(),
            capabilities: self.capabilities.clone(),
            timeout: self.timeout,
            deadline: self.deadline,
        }
    }
}

pub(crate) async fn execute(
    suite: SuiteKind,
    context: SuiteContext,
) -> Result<BenchMetric, String> {
    match suite {
        SuiteKind::DeploySpeed => deploy_speed(&context).await,
        SuiteKind::SchedulerThroughput => scheduler_throughput(&context).await,
        SuiteKind::DiscoveryLatency => discovery_latency(&context).await,
        SuiteKind::NetworkThroughput => network_throughput(&context).await,
        SuiteKind::ReconstructionTime => reconstruction_time(&context).await,
        SuiteKind::ImageDistribution => image_distribution(&context).await,
        SuiteKind::ClusterCapacity => cluster_capacity(&context).await,
    }
}

async fn deploy_speed(context: &SuiteContext) -> Result<BenchMetric, String> {
    let test = context.test_context();
    let replicas = if context.options.quick { 3 } else { 10 };
    let sample_count = if context.options.quick { 1 } else { 3 };
    let mut samples = Vec::with_capacity(sample_count);
    for sample in 0..sample_count {
        let app = format!("deploy-{sample}");
        let start = Instant::now();
        test.apply(&test.container_http_spec(&app, replicas))
            .await?;
        test.wait_running_cluster(&app, replicas).await?;
        samples.push(start.elapsed().as_secs_f64());
    }
    metric(
        "deploy_speed",
        median(&samples).ok_or_else(|| "deploy suite produced no samples".to_string())?,
        "s",
        false,
        samples.len(),
        [
            ("method", "leased-apply-until-all-running".to_string()),
            ("replicas", replicas.to_string()),
            ("image", PINNED_TEST_WORKLOAD_IMAGE.to_string()),
        ],
    )
}

async fn scheduler_throughput(context: &SuiteContext) -> Result<BenchMetric, String> {
    let test = context.test_context();
    let apps = if context.options.quick { 10 } else { 30 };
    let mut manifest = String::new();
    for index in 0..apps {
        manifest.push_str(&test.container_idle_spec(&format!("schedule-{index}")));
        manifest.push('\n');
    }
    let config = Config::parse(&manifest).map_err(|error| format!("benchmark config: {error}"))?;
    let start = Instant::now();
    let result = context
        .client
        .apply_with_lease(&config, &context.lease_id)
        .await
        .map_err(|error| format!("scheduler batch apply failed: {error}"))?;
    if result.created < apps {
        return Err(format!(
            "scheduler created {} of {apps} requested instances",
            result.created
        ));
    }
    wait_namespace_assignments(&test, apps).await?;
    let elapsed = start.elapsed().as_secs_f64();
    metric(
        "scheduler_throughput",
        apps as f64 / elapsed.max(f64::MIN_POSITIVE),
        "apps/s",
        true,
        apps,
        [
            (
                "method",
                "single-public-apply-until-node-assignment".to_string(),
            ),
            ("apps", apps.to_string()),
            ("replicas_per_app", "1".to_string()),
            ("image", PINNED_TEST_WORKLOAD_IMAGE.to_string()),
        ],
    )
}

async fn discovery_latency(context: &SuiteContext) -> Result<BenchMetric, String> {
    let test = context.test_context();
    let target = "dns-target";
    let source = "dns-source";
    let manifest = format!(
        "{}\n{}",
        test.container_http_spec(target, 1),
        test.container_idle_spec(source)
    );
    test.apply(&manifest).await?;
    test.wait_running_cluster(target, 1).await?;
    test.wait_running_cluster(source, 1).await?;
    let source_client = client_hosting(&test, source).await?;
    let requests = if context.options.quick { 50 } else { 200 };
    let command = discovery_command(target, &context.namespace);
    let mut samples = Vec::with_capacity(requests);
    for _ in 0..requests {
        let start = Instant::now();
        let output = source_client
            .exec(source, &context.namespace, &command)
            .await
            .map_err(|error| format!("workload DNS query failed: {error}"))?;
        if !output.contains("Address") && !output.contains("address") {
            return Err(format!(
                "workload DNS query returned no address: {output:?}"
            ));
        }
        samples.push(start.elapsed().as_secs_f64() * 1_000.0);
    }
    metric(
        "discovery_latency_p99",
        nearest_rank(&samples, 99)
            .ok_or_else(|| "discovery suite produced no samples".to_string())?,
        "ms",
        false,
        samples.len(),
        [
            ("method", "source-workload-exec-nslookup".to_string()),
            ("requests", requests.to_string()),
            ("record", "A".to_string()),
            ("name_shape", "app.namespace.internal".to_string()),
        ],
    )
}

async fn network_throughput(context: &SuiteContext) -> Result<BenchMetric, String> {
    let test = context.test_context();
    let source = "network-source";
    let target = "network-target";
    let payload_mib = if context.options.quick { 8 } else { 64 };
    let sample_count = if context.options.quick { 1 } else { 3 };
    let port = test.container_port(target);
    let target_spec = format!(
        "[app.{target}]\n\
         image = \"{PINNED_TEST_WORKLOAD_IMAGE}\"\n\
         command = [\"sh\", \"-c\", \"mkdir -p /tmp/payload && echo ok > /tmp/payload/health && dd if=/dev/zero of=/tmp/payload/payload.bin bs=1048576 count={payload_mib} 2>/dev/null && exec httpd -f -p {port} -h /tmp/payload\"]\n\
         port = {port}\n\
         namespace = \"{}\"\n\n\
         [app.{target}.health]\n\
         path = \"/health\"\n\
         interval = 2\n\
         timeout = 2\n\
         threshold_unhealthy = 3\n\
         threshold_healthy = 1\n",
        context.namespace
    );
    test.apply(&format!(
        "{target_spec}\n{}",
        test.container_idle_spec(source)
    ))
    .await?;
    test.wait_running_cluster(target, 1).await?;
    test.wait_running_cluster(source, 1).await?;
    let source_client = client_hosting(&test, source).await?;
    let command = network_command(target, &context.namespace, port);
    let mut samples = Vec::with_capacity(sample_count);
    for _ in 0..sample_count {
        let start = Instant::now();
        source_client
            .exec(source, &context.namespace, &command)
            .await
            .map_err(|error| format!("service data-plane fetch failed: {error}"))?;
        samples.push(payload_mib as f64 / start.elapsed().as_secs_f64().max(f64::MIN_POSITIVE));
    }
    metric(
        "network_throughput",
        median(&samples).ok_or_else(|| "network suite produced no samples".to_string())?,
        "MiB/s",
        true,
        samples.len(),
        [
            (
                "method",
                "source-workload-wget-via-dns-and-service-vip".to_string(),
            ),
            ("payload_mib", payload_mib.to_string()),
            ("protocol", "http/1.1".to_string()),
        ],
    )
}

async fn reconstruction_time(context: &SuiteContext) -> Result<BenchMetric, String> {
    let test = context.test_context();
    let council = context
        .client
        .council()
        .await
        .map_err(|error| format!("could not inspect council: {error}"))?;
    let leader = council
        .leader
        .ok_or_else(|| "council has no observed leader".to_string())?;
    let clients: BTreeMap<_, _> = test.node_clients().await?.into_iter().collect();
    let leader_client = clients
        .get(&leader)
        .cloned()
        .ok_or_else(|| format!("no public API client for leader {leader}"))?;
    let observer = clients
        .iter()
        .find(|(node, _)| node.as_str() != leader)
        .map(|(_, client)| client.clone())
        .ok_or_else(|| "no non-leader observer is available".to_string())?;
    let request = FaultRequest {
        fault_type: FaultType::NodeKill {
            kill_containers: false,
        },
        target_service: String::new(),
        target_instance: None,
        target_node: Some(leader.clone()),
        duration: context.timeout.saturating_add(Duration::from_secs(30)),
        injected_by: String::new(),
        reason: Some("relish bench reconstruction_time".to_string()),
        include_leader: true,
        override_safety: false,
        acknowledged: true,
    };
    let start = Instant::now();
    context.chaos.inject_fault(leader_client, &request).await?;
    loop {
        if let Ok(status) = observer.council().await
            && status.leader.as_deref().is_some_and(|new| new != leader)
        {
            break;
        }
        if context.deadline.remaining().is_zero() {
            return Err(format!(
                "no leader different from {leader} was observed before the suite deadline"
            ));
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    metric(
        "reconstruction_time",
        start.elapsed().as_secs_f64(),
        "s",
        false,
        1,
        [
            (
                "method",
                "authenticated-leader-node-kill-until-new-leader".to_string(),
            ),
            ("kill_containers", "false".to_string()),
        ],
    )
}

async fn image_distribution(context: &SuiteContext) -> Result<BenchMetric, String> {
    let test = context.test_context();
    let app = "image-distribution";
    let spec = format!(
        "[app.{app}]\n\
         image = \"{PINNED_TEST_WORKLOAD_IMAGE}\"\n\
         command = [\"sleep\", \"infinity\"]\n\
         replicas = \"*\"\n\
         namespace = \"{}\"\n",
        context.namespace
    );
    let start = Instant::now();
    test.apply(&spec).await?;
    test.wait_running_cluster(app, context.capabilities.node_count)
        .await?;
    metric(
        "image_distribution",
        start.elapsed().as_secs_f64(),
        "s",
        false,
        1,
        [
            (
                "method",
                "pinned-public-oci-daemonset-until-running".to_string(),
            ),
            ("nodes", context.capabilities.node_count.to_string()),
            ("image", PINNED_TEST_WORKLOAD_IMAGE.to_string()),
            ("cache_state", "uncontrolled".to_string()),
        ],
    )
}

async fn cluster_capacity(context: &SuiteContext) -> Result<BenchMetric, String> {
    let mut lease_id = context.lease_id.clone();
    let mut namespace = context.namespace.clone();
    let mut placed = 0usize;
    while placed < CAPACITY_HARD_LIMIT {
        if placed > 0 && placed.is_multiple_of(CAPACITY_APPS_PER_LEASE) {
            namespace = format!("{}-{}", context.namespace, placed / CAPACITY_APPS_PER_LEASE);
            let lease = context.leases.create(&namespace).await?;
            lease_id = lease.lease_id;
        }
        let app = format!("capacity-{}", placed % CAPACITY_APPS_PER_LEASE);
        let config = Config::parse(&format!(
            "[app.{app}]\n\
             image = \"{PINNED_TEST_WORKLOAD_IMAGE}\"\n\
             command = [\"sleep\", \"infinity\"]\n\
             cpu = \"1m\"\n\
             memory = \"1Mi\"\n\
             namespace = \"{namespace}\"\n"
        ))
        .map_err(|error| format!("capacity config: {error}"))?;
        match context
            .client
            .apply_capacity_with_lease(&config, &lease_id)
            .await
        {
            Ok(_) => placed += 1,
            Err(error) if error.to_string().contains("no eligible nodes") => {
                return metric(
                    "cluster_capacity",
                    placed as f64,
                    "apps",
                    true,
                    1,
                    [
                        (
                            "method",
                            "leased-minimal-apps-until-no-eligible-nodes".to_string(),
                        ),
                        ("cpu_request", "1m".to_string()),
                        ("memory_request", "1Mi".to_string()),
                        ("image", PINNED_TEST_WORKLOAD_IMAGE.to_string()),
                    ],
                );
            }
            Err(error) => return Err(format!("capacity probe failed before saturation: {error}")),
        }
    }
    Err(format!(
        "capacity exceeded the hard safety limit of {CAPACITY_HARD_LIMIT} apps"
    ))
}

async fn client_hosting(test: &TestContext, app: &str) -> Result<BunClient, String> {
    for (_node, client) in test.node_clients().await? {
        let statuses = client
            .status()
            .await
            .map_err(|error| format!("could not locate {app}: {error}"))?;
        if statuses.iter().any(|instance| {
            instance.app_name == app
                && instance.namespace == test.namespace
                && instance.state == "running"
        }) {
            return Ok(client);
        }
    }
    Err(format!(
        "no running instance of {app} has a public node API"
    ))
}

async fn wait_namespace_assignments(test: &TestContext, expected: usize) -> Result<(), String> {
    loop {
        let mut assigned = BTreeSet::new();
        for (_node, client) in test.node_clients().await? {
            let statuses = client
                .status()
                .await
                .map_err(|error| format!("could not inspect scheduler assignments: {error}"))?;
            assigned.extend(
                statuses
                    .into_iter()
                    .filter(|instance| instance.namespace == test.namespace)
                    .map(|instance| instance.app_name),
            );
        }
        if assigned.len() >= expected {
            return Ok(());
        }
        if test.deadline.remaining().is_zero() {
            return Err(format!(
                "timed out waiting for {expected} scheduler assignments; observed {}",
                assigned.len()
            ));
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

fn metric<const N: usize>(
    name: &str,
    value: f64,
    unit: &str,
    higher_is_better: bool,
    samples: usize,
    parameters: [(&str, String); N],
) -> Result<BenchMetric, String> {
    if !value.is_finite() || value < 0.0 || samples == 0 {
        return Err(format!("{name} produced an invalid aggregate"));
    }
    Ok(BenchMetric {
        name: name.to_string(),
        value,
        unit: unit.to_string(),
        higher_is_better,
        samples: samples.try_into().unwrap_or(u32::MAX),
        parameters: parameters
            .into_iter()
            .map(|(key, value)| (key.to_string(), value))
            .collect(),
    })
}

/// Median of a non-empty sample set.
pub fn median(samples: &[f64]) -> Option<f64> {
    let sorted = finite_sorted(samples)?;
    let middle = sorted.len() / 2;
    if sorted.len() % 2 == 0 {
        Some((sorted[middle - 1] + sorted[middle]) / 2.0)
    } else {
        Some(sorted[middle])
    }
}

/// Nearest-rank percentile of a non-empty sample set.
pub fn nearest_rank(samples: &[f64], percentile: u8) -> Option<f64> {
    if percentile == 0 || percentile > 100 {
        return None;
    }
    let sorted = finite_sorted(samples)?;
    let rank = (usize::from(percentile) * sorted.len()).div_ceil(100);
    sorted.get(rank.saturating_sub(1)).copied()
}

fn finite_sorted(samples: &[f64]) -> Option<Vec<f64>> {
    if samples.is_empty() || samples.iter().any(|sample| !sample.is_finite()) {
        return None;
    }
    let mut sorted = samples.to_vec();
    sorted.sort_by(f64::total_cmp);
    Some(sorted)
}

fn discovery_command(service: &str, namespace: &str) -> Vec<String> {
    vec![
        "nslookup".to_string(),
        format!("{service}.{namespace}.internal"),
    ]
}

fn network_command(service: &str, namespace: &str, port: u16) -> Vec<String> {
    vec![
        "wget".to_string(),
        "-q".to_string(),
        "-O".to_string(),
        "/dev/null".to_string(),
        format!("http://{service}.{namespace}.internal:{port}/payload.bin"),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn median_sorts_and_averages_the_middle_pair() {
        assert_eq!(median(&[9.0, 1.0, 5.0]), Some(5.0));
        assert_eq!(median(&[9.0, 1.0, 5.0, 3.0]), Some(4.0));
        assert_eq!(median(&[]), None);
    }

    #[test]
    fn p99_uses_nearest_rank_without_interpolation() {
        let samples: Vec<f64> = (1..=200).map(f64::from).collect();
        assert_eq!(nearest_rank(&samples, 99), Some(198.0));
        assert_eq!(nearest_rank(&[3.0], 99), Some(3.0));
        assert_eq!(nearest_rank(&[], 99), None);
    }

    #[test]
    fn discovery_and_network_commands_use_the_workload_dns_path() {
        assert_eq!(
            discovery_command("redis", "rbtest-bench-01"),
            ["nslookup", "redis.rbtest-bench-01.internal"]
        );
        assert_eq!(
            network_command("payload", "rbtest-bench-01", 8080),
            [
                "wget",
                "-q",
                "-O",
                "/dev/null",
                "http://payload.rbtest-bench-01.internal:8080/payload.bin",
            ]
        );
    }

    #[test]
    fn destructive_suites_need_explicit_consent() {
        let capabilities = ClusterCapabilities::default();
        let options = SuiteOptions {
            quick: false,
            capacity: false,
            disruptive: false,
            yes: false,
        };

        assert_eq!(
            preflight(SuiteKind::ReconstructionTime, options, &capabilities),
            Err(Preflight::Skip(
                "leader failure requires --disruptive --yes".to_string()
            ))
        );
        assert_eq!(
            preflight(SuiteKind::ClusterCapacity, options, &capabilities),
            Err(Preflight::Skip("requires --capacity".to_string()))
        );
    }
}
