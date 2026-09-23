//! Kubernetes YAML → Reliaburger TOML importer.
//!
//! Parses Kubernetes manifests (multi-document YAML), correlates
//! related resources (Deployment+Service+Ingress → single App),
//! and produces a Reliaburger `Config` with a migration report.

use std::collections::BTreeMap;
use std::path::PathBuf;

use k8s_openapi::api::apps::v1::{DaemonSet, Deployment, StatefulSet};
use k8s_openapi::api::autoscaling::v2::HorizontalPodAutoscaler;
use k8s_openapi::api::batch::v1::{CronJob, Job};
use k8s_openapi::api::core::v1::{ConfigMap, Namespace, Secret, Service};
use k8s_openapi::api::networking::v1::Ingress;

use crate::config::app::{
    AppSpec, AutoscaleSpec, DeploySpec, HealthSpec, IngressSpec, MetricsSpec, PlacementSpec,
};
use crate::config::types::{EnvValue, Replicas};
use crate::config::{Config, JobSpec, NamespaceSpec};

use super::RelishError;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Result of a Kubernetes import.
#[derive(Debug)]
pub struct ImportResult {
    /// The converted Reliaburger config.
    pub config: Config,
    /// Migration report (warnings, dropped resources).
    pub report: MigrationReport,
}

/// Migration report detailing what happened during import.
#[derive(Debug, Default)]
pub struct MigrationReport {
    /// Successfully converted resources.
    pub converted: Vec<String>,
    /// Resources that were approximated (review recommended).
    pub warnings: Vec<MigrationWarning>,
    /// Resources with no Reliaburger equivalent (dropped).
    pub dropped: Vec<String>,
}

/// A warning about an approximated conversion.
#[derive(Debug)]
pub struct MigrationWarning {
    pub resource: String,
    pub message: String,
}

impl std::fmt::Display for MigrationReport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if !self.converted.is_empty() {
            writeln!(f, "Converted:")?;
            for c in &self.converted {
                writeln!(f, "  + {c}")?;
            }
        }
        if !self.warnings.is_empty() {
            writeln!(f, "\nApproximated (review recommended):")?;
            for w in &self.warnings {
                writeln!(f, "  ~ {} — {}", w.resource, w.message)?;
            }
        }
        if !self.dropped.is_empty() {
            writeln!(f, "\nDropped (no Reliaburger equivalent):")?;
            for d in &self.dropped {
                writeln!(f, "  - {d}")?;
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Parsed K8s resource wrapper
// ---------------------------------------------------------------------------

#[derive(Debug)]
#[allow(clippy::large_enum_variant)]
enum K8sResource {
    Deployment(String, Deployment),
    DaemonSet(String, DaemonSet),
    StatefulSet(String, StatefulSet),
    Service(String, Service),
    Ingress(String, Ingress),
    Hpa(String, HorizontalPodAutoscaler),
    ConfigMap(String, ConfigMap),
    Secret(String, Secret),
    Job(String, Job),
    CronJob(String, CronJob),
    Namespace(String, Namespace),
    Unknown(String, String), // (kind, name)
}

impl K8sResource {
    #[allow(dead_code)]
    fn name(&self) -> &str {
        match self {
            Self::Deployment(n, _)
            | Self::DaemonSet(n, _)
            | Self::StatefulSet(n, _)
            | Self::Service(n, _)
            | Self::Ingress(n, _)
            | Self::Hpa(n, _)
            | Self::ConfigMap(n, _)
            | Self::Secret(n, _)
            | Self::Job(n, _)
            | Self::CronJob(n, _)
            | Self::Namespace(n, _)
            | Self::Unknown(_, n) => n,
        }
    }
}

// ---------------------------------------------------------------------------
// Import entry point
// ---------------------------------------------------------------------------

/// Import Kubernetes YAML files into a Reliaburger Config.
pub fn import_kubernetes(files: &[PathBuf]) -> Result<ImportResult, RelishError> {
    let mut all_resources = Vec::new();

    for file in files {
        let content = std::fs::read_to_string(file)?;
        let resources = parse_multi_document_yaml(&content)?;
        all_resources.extend(resources);
    }

    let (config, report) = correlate_and_convert(all_resources);
    Ok(ImportResult { config, report })
}

/// Import from a YAML string (for testing).
pub fn import_from_yaml(yaml: &str) -> Result<ImportResult, RelishError> {
    let resources = parse_multi_document_yaml(yaml)?;
    let (config, report) = correlate_and_convert(resources);
    Ok(ImportResult { config, report })
}

// ---------------------------------------------------------------------------
// YAML parsing
// ---------------------------------------------------------------------------

/// Parse a multi-document YAML string into typed K8s resources.
///
/// Uses `serde_yaml::Deserializer` to split documents (M28) rather than a naive
/// `split("---")`: the latter shredded any document with an embedded `---` (a
/// PEM block or a `|`-scalar), and skipping documents that *start with* `#`
/// dropped every Helm-rendered manifest, since Helm prefixes each with a
/// `# Source: ...` comment — so `helm template ... | relish import` imported
/// nothing and exited 0. The deserializer respects real document boundaries and
/// leading comments.
fn parse_multi_document_yaml(yaml: &str) -> Result<Vec<K8sResource>, RelishError> {
    use serde::Deserialize as _;

    let mut resources = Vec::new();

    for document in serde_yaml::Deserializer::from_str(yaml) {
        let value = serde_yaml::Value::deserialize(document)
            .map_err(|e| RelishError::FormatFailed(format!("YAML parse error: {e}")))?;

        // A comment-only or empty document deserialises to null — skip it.
        if value.is_null() {
            continue;
        }

        let kind = value["kind"].as_str().unwrap_or("").to_string();
        let name = value["metadata"]["name"]
            .as_str()
            .unwrap_or("unknown")
            .to_string();

        let resource = match kind.as_str() {
            "Deployment" => {
                let d: Deployment = serde_yaml::from_value(value.clone())
                    .map_err(|e| RelishError::FormatFailed(e.to_string()))?;
                K8sResource::Deployment(name, d)
            }
            "DaemonSet" => {
                let d: DaemonSet = serde_yaml::from_value(value.clone())
                    .map_err(|e| RelishError::FormatFailed(e.to_string()))?;
                K8sResource::DaemonSet(name, d)
            }
            "StatefulSet" => {
                let d: StatefulSet = serde_yaml::from_value(value.clone())
                    .map_err(|e| RelishError::FormatFailed(e.to_string()))?;
                K8sResource::StatefulSet(name, d)
            }
            "Service" => {
                let s: Service = serde_yaml::from_value(value.clone())
                    .map_err(|e| RelishError::FormatFailed(e.to_string()))?;
                K8sResource::Service(name, s)
            }
            "Ingress" => {
                let i: Ingress = serde_yaml::from_value(value.clone())
                    .map_err(|e| RelishError::FormatFailed(e.to_string()))?;
                K8sResource::Ingress(name, i)
            }
            "HorizontalPodAutoscaler" => {
                let h: HorizontalPodAutoscaler = serde_yaml::from_value(value.clone())
                    .map_err(|e| RelishError::FormatFailed(e.to_string()))?;
                K8sResource::Hpa(name, h)
            }
            "ConfigMap" => {
                let c: ConfigMap = serde_yaml::from_value(value.clone())
                    .map_err(|e| RelishError::FormatFailed(e.to_string()))?;
                K8sResource::ConfigMap(name, c)
            }
            "Secret" => {
                let s: Secret = serde_yaml::from_value(value.clone())
                    .map_err(|e| RelishError::FormatFailed(e.to_string()))?;
                K8sResource::Secret(name, s)
            }
            "Job" => {
                let j: Job = serde_yaml::from_value(value.clone())
                    .map_err(|e| RelishError::FormatFailed(e.to_string()))?;
                K8sResource::Job(name, j)
            }
            "CronJob" => {
                let c: CronJob = serde_yaml::from_value(value.clone())
                    .map_err(|e| RelishError::FormatFailed(e.to_string()))?;
                K8sResource::CronJob(name, c)
            }
            "Namespace" => {
                let n: Namespace = serde_yaml::from_value(value.clone())
                    .map_err(|e| RelishError::FormatFailed(e.to_string()))?;
                K8sResource::Namespace(name, n)
            }
            _ => K8sResource::Unknown(kind, name),
        };

        resources.push(resource);
    }

    Ok(resources)
}

// ---------------------------------------------------------------------------
// Resource correlation and conversion
// ---------------------------------------------------------------------------

/// Correlate K8s resources and convert to a Reliaburger Config.
fn correlate_and_convert(resources: Vec<K8sResource>) -> (Config, MigrationReport) {
    let mut config = Config::default();
    let mut report = MigrationReport::default();

    // Separate by type for correlation
    let mut deployments: Vec<(String, Deployment)> = Vec::new();
    let mut daemonsets: Vec<(String, DaemonSet)> = Vec::new();
    let mut statefulsets: Vec<(String, StatefulSet)> = Vec::new();
    let mut services: BTreeMap<String, Service> = BTreeMap::new();
    let mut ingresses: BTreeMap<String, Ingress> = BTreeMap::new();
    let mut hpas: BTreeMap<String, HorizontalPodAutoscaler> = BTreeMap::new();
    let mut configmaps: BTreeMap<String, ConfigMap> = BTreeMap::new();
    let mut secrets: BTreeMap<String, Secret> = BTreeMap::new();
    let mut jobs: Vec<(String, Job)> = Vec::new();
    let mut cronjobs: Vec<(String, CronJob)> = Vec::new();

    for resource in resources {
        match resource {
            K8sResource::Deployment(n, d) => deployments.push((n, d)),
            K8sResource::DaemonSet(n, d) => daemonsets.push((n, d)),
            K8sResource::StatefulSet(n, d) => statefulsets.push((n, d)),
            K8sResource::Service(n, s) => {
                services.insert(n, s);
            }
            K8sResource::Ingress(n, i) => {
                ingresses.insert(n, i);
            }
            K8sResource::Hpa(n, h) => {
                hpas.insert(n, h);
            }
            K8sResource::ConfigMap(n, c) => {
                configmaps.insert(n, c);
            }
            K8sResource::Secret(n, s) => {
                secrets.insert(n, s);
            }
            K8sResource::Job(n, j) => jobs.push((n, j)),
            K8sResource::CronJob(n, c) => cronjobs.push((n, c)),
            K8sResource::Namespace(n, _ns) => {
                config.namespace.insert(
                    n.clone(),
                    NamespaceSpec {
                        cpu: None,
                        memory: None,
                        gpu: None,
                        max_apps: None,
                        max_replicas: None,
                    },
                );
                report.converted.push(format!("Namespace/{n}"));
            }
            K8sResource::Unknown(kind, name) => {
                report
                    .dropped
                    .push(format!("{kind}/{name} — no Reliaburger equivalent"));
            }
        }
    }

    // Track which Ingresses/HPAs correlate to a workload, so the leftovers
    // can be reported instead of silently vanishing (the ConfigMap/Secret
    // sweeps below always reported; these two didn't).
    let mut used_ingresses: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let mut used_hpas: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let mut used_services: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();

    // Convert Deployments → Apps (with correlated Service, Ingress, HPA)
    for (name, deploy) in &deployments {
        let mut app = deployment_to_app(name, deploy, &mut report);
        let pod_spec = deploy.spec.as_ref().and_then(|s| s.template.spec.as_ref());

        // Correlate Service by name match
        if let Some(svc) = services.get(name) {
            apply_service(&mut app, name, svc, pod_spec, &mut report);
            used_services.insert(name.clone());
        }
        warn_unimported_ports(
            &format!("Deployment/{name}"),
            pod_spec,
            &mut app,
            &mut report,
        );

        // Correlate Ingress by backend service name
        if let Some(ing_name) = find_ingress_for_service(&ingresses, name) {
            if let Some(ing) = ingresses.get(&ing_name) {
                apply_ingress(&mut app, &ing_name, ing, &mut report);
                used_ingresses.insert(ing_name);
            }
        }

        // Correlate HPA by its scaleTargetRef (the HPA's own metadata name is
        // conventionally `{app}-hpa`, which a name-keyed lookup never matched).
        if let Some(hpa_name) = find_hpa_for_workload(&hpas, name, "Deployment") {
            if let Some(hpa) = hpas.get(&hpa_name) {
                apply_hpa(&mut app, hpa);
                used_hpas.insert(hpa_name);
            }
        }

        let key = insert_app_without_overwrite(&mut config, &mut report, name, app, "Deployment");
        report
            .converted
            .push(format!("Deployment/{name} → [app.{key}]"));
    }

    // Convert DaemonSets → Apps with replicas = "*" (Service + Ingress
    // correlate the same way; an HPA cannot target a DaemonSet).
    for (name, ds) in &daemonsets {
        let mut app = daemonset_to_app(name, ds, &mut report);
        let pod_spec = ds.spec.as_ref().and_then(|s| s.template.spec.as_ref());
        if let Some(svc) = services.get(name) {
            apply_service(&mut app, name, svc, pod_spec, &mut report);
            used_services.insert(name.clone());
        }
        warn_unimported_ports(
            &format!("DaemonSet/{name}"),
            pod_spec,
            &mut app,
            &mut report,
        );
        if let Some(ing_name) = find_ingress_for_service(&ingresses, name) {
            if let Some(ing) = ingresses.get(&ing_name) {
                apply_ingress(&mut app, &ing_name, ing, &mut report);
                used_ingresses.insert(ing_name);
            }
        }
        let key = insert_app_without_overwrite(&mut config, &mut report, name, app, "DaemonSet");
        report
            .converted
            .push(format!("DaemonSet/{name} → [app.{key}] (replicas = \"*\")"));
    }

    // Convert StatefulSets → Apps with warning (Service/Ingress/HPA used to
    // be Deployment-only, so a StatefulSet's siblings were silently dropped).
    for (name, ss) in &statefulsets {
        let mut app = statefulset_to_app(name, ss, &mut report);
        let pod_spec = ss.spec.as_ref().and_then(|s| s.template.spec.as_ref());
        if let Some(svc) = services.get(name) {
            apply_service(&mut app, name, svc, pod_spec, &mut report);
            used_services.insert(name.clone());
        }
        warn_unimported_ports(
            &format!("StatefulSet/{name}"),
            pod_spec,
            &mut app,
            &mut report,
        );
        if let Some(ing_name) = find_ingress_for_service(&ingresses, name) {
            if let Some(ing) = ingresses.get(&ing_name) {
                apply_ingress(&mut app, &ing_name, ing, &mut report);
                used_ingresses.insert(ing_name);
            }
        }
        if let Some(hpa_name) = find_hpa_for_workload(&hpas, name, "StatefulSet") {
            if let Some(hpa) = hpas.get(&hpa_name) {
                apply_hpa(&mut app, hpa);
                used_hpas.insert(hpa_name);
            }
        }
        let key = insert_app_without_overwrite(&mut config, &mut report, name, app, "StatefulSet");
        report
            .converted
            .push(format!("StatefulSet/{name} → [app.{key}]"));
        report.warnings.push(MigrationWarning {
            resource: format!("StatefulSet/{name}"),
            message: "ordering guarantees and stable network IDs lost".to_string(),
        });
    }

    // Convert Jobs
    for (name, job) in &jobs {
        let job_spec = job_to_jobspec(name, job, &mut report);
        config.job.insert(name.clone(), job_spec);
        report.converted.push(format!("Job/{name} → [job.{name}]"));
    }

    // Convert CronJobs
    for (name, cj) in &cronjobs {
        let job_spec = cronjob_to_jobspec(name, cj, &mut report);
        config.job.insert(name.clone(), job_spec);
        report
            .converted
            .push(format!("CronJob/{name} → [job.{name}]"));
    }

    // Report uncorrelated Services, Ingresses and HPAs: their names,
    // routing or autoscaling are lost.
    for name in services.keys() {
        if !used_services.contains(name) {
            report.warnings.push(MigrationWarning {
                resource: format!("Service/{name}"),
                message: "no imported workload has this name, so nothing answers to it; an app \
                          is reachable only by its own name"
                    .to_string(),
            });
        }
    }
    for name in ingresses.keys() {
        if !used_ingresses.contains(name) {
            report.warnings.push(MigrationWarning {
                resource: format!("Ingress/{name}"),
                message: "no imported workload matches its backend service; routing dropped"
                    .to_string(),
            });
        }
    }
    for name in hpas.keys() {
        if !used_hpas.contains(name) {
            report.warnings.push(MigrationWarning {
                resource: format!("HorizontalPodAutoscaler/{name}"),
                message: "no imported workload matches its scaleTargetRef; autoscaling dropped"
                    .to_string(),
            });
        }
    }

    // Report uncorrelated ConfigMaps as warnings
    for name in configmaps.keys() {
        report.warnings.push(MigrationWarning {
            resource: format!("ConfigMap/{name}"),
            message: "not referenced by any workload; import manually if needed".to_string(),
        });
    }

    // Report Secrets
    for name in secrets.keys() {
        report.warnings.push(MigrationWarning {
            resource: format!("Secret/{name}"),
            message: "re-encrypt values with `relish secret encrypt`".to_string(),
        });
    }

    (config, report)
}

/// Find the HPA whose `scaleTargetRef` names this workload.
///
/// Matching by the HPA's own metadata name (the old behaviour) never
/// correlated a conventionally-named `api-hpa` with its `api` Deployment, so
/// its autoscaling was silently lost. An absent/empty `kind` matches any
/// workload — real manifests omit it and serde defaults it to `""`.
fn find_hpa_for_workload(
    hpas: &BTreeMap<String, HorizontalPodAutoscaler>,
    workload_name: &str,
    workload_kind: &str,
) -> Option<String> {
    for (hpa_name, hpa) in hpas {
        if let Some(spec) = &hpa.spec {
            let target = &spec.scale_target_ref;
            if target.name == workload_name
                && (target.kind.is_empty() || target.kind == workload_kind)
            {
                return Some(hpa_name.clone());
            }
        }
    }
    None
}

/// Insert an app under its own name, or under `{namespace}-{name}` when
/// the name is already taken by a resource from another namespace.
/// K8s scopes names per namespace; a flat TOML table does not, so a
/// silent `insert` would overwrite the earlier app.
fn insert_app_without_overwrite(
    config: &mut Config,
    report: &mut MigrationReport,
    name: &str,
    app: AppSpec,
    kind: &str,
) -> String {
    let key = if config.app.contains_key(name) {
        let namespace = app
            .namespace
            .clone()
            .unwrap_or_else(|| "default".to_string());
        let renamed = format!("{namespace}-{name}");
        report.warnings.push(MigrationWarning {
            resource: format!("{kind}/{name}"),
            message: format!(
                "name collides with an earlier resource from another namespace; imported as [app.{renamed}]"
            ),
        });
        renamed
    } else {
        name.to_string()
    };
    config.app.insert(key.clone(), app);
    key
}

// ---------------------------------------------------------------------------
// Field mapping: Deployment → AppSpec
// ---------------------------------------------------------------------------

/// Extract everything a workload's pod template can carry into an `AppSpec`.
///
/// Shared by Deployment, DaemonSet and StatefulSet conversion — the latter
/// two used to keep only image/namespace/replicas/port, silently dropping
/// command, env, resources, probes and initContainers on the floor. Callers
/// set `replicas` (kind-specific) and any kind-only fields (the Deployment's
/// rolling-update strategy) themselves. `resource` is the `{kind}/{name}`
/// label warnings are filed under.
fn pod_spec_to_app(
    resource: &str,
    metadata_namespace: Option<&String>,
    pod_spec: Option<&k8s_openapi::api::core::v1::PodSpec>,
    report: &mut MigrationReport,
) -> AppSpec {
    let container = pod_spec.and_then(|ps| ps.containers.first());

    let mut app = empty_app_spec();
    app.image = container.and_then(|c| c.image.clone());
    app.namespace = metadata_namespace.cloned();

    // Warn about fields we only partially import, so a silent drop becomes
    // visible in the migration report rather than a surprise in production (M28).
    if let Some(ps) = pod_spec {
        if ps.containers.len() > 1 {
            report.warnings.push(MigrationWarning {
                resource: resource.to_string(),
                message: format!(
                    "only the first of {} containers is imported; {} sidecar(s) dropped",
                    ps.containers.len(),
                    ps.containers.len() - 1
                ),
            });
        }
        if ps.volumes.as_ref().is_some_and(|v| !v.is_empty()) {
            report.warnings.push(MigrationWarning {
                resource: resource.to_string(),
                message: "pod volumes are not imported; declare Reliaburger volumes manually"
                    .to_string(),
            });
        }
    }
    if container.and_then(|c| c.liveness_probe.as_ref()).is_some() {
        report.warnings.push(MigrationWarning {
            resource: resource.to_string(),
            message: "livenessProbe is not imported (only readinessProbe maps to a health check)"
                .to_string(),
        });
    }

    // Same split as Kubernetes: `command` replaces the image's Entrypoint,
    // `args` replaces its Cmd. The runtime applies the rules (Z1.1).
    if let Some(c) = container {
        app.command = c.command.clone().unwrap_or_default();
        app.args = c.args.clone().unwrap_or_default();
        app.working_dir = c.working_dir.clone().map(PathBuf::from);
    }
    import_security_context(resource, pod_spec, container, &mut app, report);

    let ports = container_ports(pod_spec);
    app.port = ports.first().map(|p| p.container_port as u16);

    // Health check from readinessProbe. Only HTTP probes have an equivalent;
    // anything else is reported rather than dropped.
    if let Some(probe) = container.and_then(|c| c.readiness_probe.as_ref()) {
        if let Some(http_get) = &probe.http_get {
            let probe_port = resolve_port(&http_get.port, ports);
            app.health = Some(HealthSpec {
                path: http_get.path.clone().unwrap_or_else(|| "/".to_string()),
                port: probe_port.filter(|port| Some(*port) != app.port),
                protocol: match http_get.scheme.as_deref() {
                    Some("HTTPS") => crate::config::app::HealthProtocol::Https,
                    _ => crate::config::app::HealthProtocol::Http,
                },
                interval: probe.period_seconds.map(|s| s as u64),
                timeout: probe.timeout_seconds.map(|s| s as u64),
                threshold_unhealthy: probe.failure_threshold.map(|t| t as u32),
                threshold_healthy: probe.success_threshold.map(|t| t as u32),
                initial_delay: probe.initial_delay_seconds.map(|s| s as u64),
            });
        } else {
            let kind = if let Some(exec) = &probe.exec {
                format!(
                    "runs a command ({})",
                    exec.command.clone().unwrap_or_default().join(" ")
                )
            } else if probe.tcp_socket.is_some() {
                "is a tcpSocket check".to_string()
            } else if probe.grpc.is_some() {
                "is a gRPC check".to_string()
            } else {
                "has no handler".to_string()
            };
            report.warnings.push(MigrationWarning {
                resource: resource.to_string(),
                message: format!(
                    "readinessProbe {kind}; only httpGet probes import, so this app has no \
                     health check. Add a [health] block with an HTTP path"
                ),
            });
        }
    }

    // CPU and memory from requests AND limits, parsed as real K8s quantities.
    if let Some(resources) = container.and_then(|c| c.resources.as_ref()) {
        app.cpu = resource_range_from(resource, "cpu", resources, parse_k8s_cpu_millicores, report);
        app.memory = resource_range_from(
            resource,
            "memory",
            resources,
            parse_k8s_memory_bytes,
            report,
        );
    }

    // Env vars. Plain values convert directly; `valueFrom` references
    // (secret/configmap/field) have no automatic mapping — surface them
    // as warnings instead of dropping them silently.
    if let Some(env_list) = container.and_then(|c| c.env.as_ref()) {
        for env_var in env_list {
            if let Some(ref value) = env_var.value {
                app.env
                    .insert(env_var.name.clone(), EnvValue::Plain(value.clone()));
            } else if env_var.value_from.is_some() {
                report.warnings.push(MigrationWarning {
                    resource: resource.to_string(),
                    message: format!(
                        "env {} uses valueFrom (secret/configmap/field ref) — set the value manually or use `relish secret encrypt`",
                        env_var.name
                    ),
                });
            }
        }
    }

    // Node selector → placement.required
    if let Some(selector) = pod_spec.and_then(|ps| ps.node_selector.as_ref()) {
        let labels: Vec<String> = selector.iter().map(|(k, v)| format!("{k}={v}")).collect();
        if !labels.is_empty() {
            app.placement = Some(PlacementSpec {
                required: labels,
                preferred: Vec::new(),
            });
        }
    }

    // Init containers carry a single argv: command followed by args.
    if let Some(inits) = pod_spec.and_then(|ps| ps.init_containers.as_ref()) {
        for ic in inits {
            let command = ic.command.clone().unwrap_or_default();
            let args = ic.args.clone().unwrap_or_default();
            if command.is_empty() && !args.is_empty() {
                report.warnings.push(MigrationWarning {
                    resource: resource.to_string(),
                    message: format!(
                        "initContainer {} sets args without a command; its image \
                         entrypoint is dropped, so add it to the command",
                        ic.name
                    ),
                });
            }
            app.init.push(crate::config::app::InitContainerSpec {
                image: ic.image.clone(),
                command: command.into_iter().chain(args).collect(),
            });
        }
    }

    app
}

/// The first container's declared ports, in order.
fn container_ports(
    pod_spec: Option<&k8s_openapi::api::core::v1::PodSpec>,
) -> &[k8s_openapi::api::core::v1::ContainerPort] {
    pod_spec
        .and_then(|ps| ps.containers.first())
        .and_then(|c| c.ports.as_deref())
        .unwrap_or_default()
}

/// A numeric port, or a named one looked up in the container's ports.
fn resolve_port(
    port: &k8s_openapi::apimachinery::pkg::util::intstr::IntOrString,
    ports: &[k8s_openapi::api::core::v1::ContainerPort],
) -> Option<u16> {
    use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;
    match port {
        IntOrString::Int(number) => u16::try_from(*number).ok(),
        IntOrString::String(name) => ports
            .iter()
            .find(|p| p.name.as_deref() == Some(name.as_str()))
            .and_then(|p| u16::try_from(p.container_port).ok()),
    }
}

/// `runAsUser` and `runAsGroup`, container-level winning over pod-level
/// as in Kubernetes. Other security settings have no equivalent.
fn import_security_context(
    resource: &str,
    pod_spec: Option<&k8s_openapi::api::core::v1::PodSpec>,
    container: Option<&k8s_openapi::api::core::v1::Container>,
    app: &mut AppSpec,
    report: &mut MigrationReport,
) {
    let pod = pod_spec.and_then(|ps| ps.security_context.as_ref());
    let own = container.and_then(|c| c.security_context.as_ref());
    let user = own
        .and_then(|sc| sc.run_as_user)
        .or_else(|| pod.and_then(|sc| sc.run_as_user));
    let group = own
        .and_then(|sc| sc.run_as_group)
        .or_else(|| pod.and_then(|sc| sc.run_as_group));
    for (field, value, target) in [
        ("runAsUser", user, &mut app.run_as_user),
        ("runAsGroup", group, &mut app.run_as_group),
    ] {
        let Some(value) = value else { continue };
        match u32::try_from(value) {
            Ok(id) => *target = Some(id),
            Err(_) => report.warnings.push(MigrationWarning {
                resource: resource.to_string(),
                message: format!("securityContext.{field} {value} is not a valid id; dropped"),
            }),
        }
    }
}

/// Fill the app's `metrics` from the pod template's Prometheus annotations.
///
/// `prometheus.io/scrape: "true"` opts in; `prometheus.io/port` and
/// `prometheus.io/path` override the app port and `/metrics`, the same
/// defaults a Prometheus `kubernetes_sd` scrape config applies. Anything but
/// `"true"` leaves the app unscraped, as it would in Kubernetes.
fn import_scrape_annotations(
    resource: &str,
    template_metadata: Option<&k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta>,
    app: &mut AppSpec,
    report: &mut MigrationReport,
) {
    let Some(annotations) = template_metadata.and_then(|meta| meta.annotations.as_ref()) else {
        return;
    };
    if annotations.get("prometheus.io/scrape").map(String::as_str) != Some("true") {
        return;
    }
    let mut metrics = MetricsSpec::default();
    if let Some(port) = annotations.get("prometheus.io/port") {
        match port.parse::<u16>() {
            Ok(number) if number > 0 => metrics.port = Some(number),
            _ => report.warnings.push(MigrationWarning {
                resource: resource.to_string(),
                message: format!(
                    "prometheus.io/port {port:?} is not a port number; scraping the app's port"
                ),
            }),
        }
    }
    if let Some(path) = annotations.get("prometheus.io/path") {
        if path.starts_with('/') && !path.contains(char::is_whitespace) {
            metrics.path = path.clone();
        } else {
            report.warnings.push(MigrationWarning {
                resource: resource.to_string(),
                message: format!("prometheus.io/path {path:?} is not a path; scraping /metrics"),
            });
        }
    }
    app.metrics = Some(metrics);
}

/// Report container ports the app doesn't carry. An app exposes one port:
/// the one its Service targets, or the first declared. A port the node
/// scrapes for metrics is carried too, so it isn't reported.
///
/// Runs after the Service is folded in, because that can still change the
/// app's port: this is also where a scrape annotation that ends up with no
/// port to scrape is reported and dropped.
fn warn_unimported_ports(
    resource: &str,
    pod_spec: Option<&k8s_openapi::api::core::v1::PodSpec>,
    app: &mut AppSpec,
    report: &mut MigrationReport,
) {
    if app.metrics.is_some() && app.metrics_endpoint().is_none() {
        app.metrics = None;
        report.warnings.push(MigrationWarning {
            resource: resource.to_string(),
            message: "prometheus.io/scrape is set but the pod has no port to scrape \
                      (no prometheus.io/port and no container port); metrics are not scraped"
                .to_string(),
        });
    }
    let metrics_port = app.metrics_endpoint().map(|(port, _)| port);
    let dropped: Vec<String> = container_ports(pod_spec)
        .iter()
        .filter(|p| {
            let port = u16::try_from(p.container_port).ok();
            port != app.port && port != metrics_port
        })
        .map(|p| match &p.name {
            Some(name) => format!("{} ({name})", p.container_port),
            None => p.container_port.to_string(),
        })
        .collect();
    if dropped.is_empty() {
        return;
    }
    report.warnings.push(MigrationWarning {
        resource: resource.to_string(),
        message: format!(
            "an app exposes one port ({}); container port(s) {} are not published or \
             routed, though the process can still listen on them",
            app.port
                .map_or_else(|| "none".to_string(), |p| p.to_string()),
            dropped.join(", ")
        ),
    });
}

fn deployment_to_app(name: &str, deploy: &Deployment, report: &mut MigrationReport) -> AppSpec {
    let spec = deploy.spec.as_ref();
    let pod_spec = spec.and_then(|s| s.template.spec.as_ref());
    let mut app = pod_spec_to_app(
        &format!("Deployment/{name}"),
        deploy.metadata.namespace.as_ref(),
        pod_spec,
        report,
    );
    import_scrape_annotations(
        &format!("Deployment/{name}"),
        spec.and_then(|s| s.template.metadata.as_ref()),
        &mut app,
        report,
    );

    app.replicas = spec
        .and_then(|s| s.replicas)
        .map(|r| Replicas::Fixed(r as u32))
        .unwrap_or_default();

    // Deploy strategy (Deployment-only; DaemonSet/StatefulSet update
    // strategies are different types with no Reliaburger equivalent).
    if let Some(strategy) = spec.and_then(|s| s.strategy.as_ref()) {
        if let Some(rolling) = &strategy.rolling_update {
            app.deploy = Some(DeploySpec {
                strategy: Some("rolling".to_string()),
                max_surge: rolling.max_surge.as_ref().and_then(|v| match v {
                    k8s_openapi::apimachinery::pkg::util::intstr::IntOrString::Int(i) => {
                        Some(*i as u32)
                    }
                    _ => None,
                }),
                max_unavailable: rolling.max_unavailable.as_ref().and_then(|v| match v {
                    k8s_openapi::apimachinery::pkg::util::intstr::IntOrString::Int(i) => {
                        Some(*i as u32)
                    }
                    _ => None,
                }),
                drain_timeout: pod_spec
                    .and_then(|ps| ps.termination_grace_period_seconds)
                    .map(|s| format!("{s}s")),
                health_timeout: None,
                auto_rollback: None,
            });
        }
    }

    app
}

fn daemonset_to_app(name: &str, ds: &DaemonSet, report: &mut MigrationReport) -> AppSpec {
    let pod_spec = ds.spec.as_ref().and_then(|s| s.template.spec.as_ref());
    let mut app = pod_spec_to_app(
        &format!("DaemonSet/{name}"),
        ds.metadata.namespace.as_ref(),
        pod_spec,
        report,
    );
    import_scrape_annotations(
        &format!("DaemonSet/{name}"),
        ds.spec.as_ref().and_then(|s| s.template.metadata.as_ref()),
        &mut app,
        report,
    );
    app.replicas = Replicas::DaemonSet;
    app
}

fn statefulset_to_app(name: &str, ss: &StatefulSet, report: &mut MigrationReport) -> AppSpec {
    let spec = ss.spec.as_ref();
    let pod_spec = spec.and_then(|s| s.template.spec.as_ref());
    let mut app = pod_spec_to_app(
        &format!("StatefulSet/{name}"),
        ss.metadata.namespace.as_ref(),
        pod_spec,
        report,
    );
    import_scrape_annotations(
        &format!("StatefulSet/{name}"),
        spec.and_then(|s| s.template.metadata.as_ref()),
        &mut app,
        report,
    );
    app.replicas = spec
        .and_then(|s| s.replicas)
        .map(|r| Replicas::Fixed(r as u32))
        .unwrap_or_default();
    app
}

// ---------------------------------------------------------------------------
// Kubernetes quantity parsing
// ---------------------------------------------------------------------------

/// Parse a Kubernetes CPU quantity into millicores.
///
/// K8s CPU is denominated in *cores*: `"1"` is one core (1000m), `"0.5"` is
/// 500m, `"500m"` is 500 millicores. The old code fed these strings to
/// `ResourceRange::parse`, where a bare integer means *millicores* — so
/// `cpu: "1"` imported as one millicore, a 1000× under-read, and `"0.5"`
/// failed to parse and vanished.
fn parse_k8s_cpu_millicores(quantity: &str) -> Option<u64> {
    let s = quantity.trim();
    if let Some(millis) = s.strip_suffix('m') {
        return millis.parse::<u64>().ok();
    }
    let cores = s.parse::<f64>().ok()?;
    if !cores.is_finite() || cores < 0.0 {
        return None;
    }
    Some((cores * 1000.0).round() as u64)
}

/// Parse a Kubernetes memory quantity into bytes.
///
/// Accepts binary suffixes (`Ki`/`Mi`/`Gi`/`Ti`/`Pi`), decimal suffixes
/// (`k`/`M`/`G`/`T`/`P`), scientific notation (`1e9`) and bare bytes —
/// the forms K8s accepts that Reliaburger's own `Ki/Mi/Gi/Ti`-only parser
/// silently rejected.
fn parse_k8s_memory_bytes(quantity: &str) -> Option<u64> {
    let s = quantity.trim();
    let (number, multiplier) = if let Some(n) = s.strip_suffix("Ki") {
        (n, 1024f64)
    } else if let Some(n) = s.strip_suffix("Mi") {
        (n, 1024f64.powi(2))
    } else if let Some(n) = s.strip_suffix("Gi") {
        (n, 1024f64.powi(3))
    } else if let Some(n) = s.strip_suffix("Ti") {
        (n, 1024f64.powi(4))
    } else if let Some(n) = s.strip_suffix("Pi") {
        (n, 1024f64.powi(5))
    } else if let Some(n) = s.strip_suffix('k') {
        (n, 1e3)
    } else if let Some(n) = s.strip_suffix('M') {
        (n, 1e6)
    } else if let Some(n) = s.strip_suffix('G') {
        (n, 1e9)
    } else if let Some(n) = s.strip_suffix('T') {
        (n, 1e12)
    } else if let Some(n) = s.strip_suffix('P') {
        (n, 1e15)
    } else {
        (s, 1f64)
    };
    let value = number.trim().parse::<f64>().ok()?;
    if !value.is_finite() || value < 0.0 {
        return None;
    }
    let bytes = value * multiplier;
    if bytes > u64::MAX as f64 {
        return None;
    }
    Some(bytes.round() as u64)
}

/// Build a `ResourceRange` for one resource (`cpu` or `memory`) from a
/// container's requests **and** limits.
///
/// The old code read `limits` only, so a requests-only Deployment (the
/// common case) imported with no resources at all, and a container with
/// both lost its request half. One side missing → the other stands in for
/// both. An unparseable quantity gets a warning instead of the old `.ok()`
/// silent swallow.
fn resource_range_from(
    resource: &str,
    field: &str,
    resources: &k8s_openapi::api::core::v1::ResourceRequirements,
    parse: fn(&str) -> Option<u64>,
    report: &mut MigrationReport,
) -> Option<crate::config::types::ResourceRange> {
    let mut lookup = |side: &Option<
        std::collections::BTreeMap<String, k8s_openapi::apimachinery::pkg::api::resource::Quantity>,
    >| {
        let quantity = side.as_ref()?.get(field)?;
        match parse(&quantity.0) {
            Some(v) => Some(v),
            None => {
                report.warnings.push(MigrationWarning {
                    resource: resource.to_string(),
                    message: format!(
                        "could not parse {field} quantity {:?}; value dropped",
                        quantity.0
                    ),
                });
                None
            }
        }
    };
    let request = lookup(&resources.requests);
    let limit = lookup(&resources.limits);
    let (request, limit) = (request.or(limit)?, limit.or(request)?);
    Some(crate::config::types::ResourceRange {
        // A request above the limit is invalid in K8s too; clamp rather than
        // produce a range Reliaburger's own validation would reject.
        request: request.min(limit),
        limit,
    })
}

// ---------------------------------------------------------------------------
// Helper: empty AppSpec
// ---------------------------------------------------------------------------

/// Create an AppSpec with all fields set to None/default.
fn empty_app_spec() -> AppSpec {
    AppSpec {
        image: None,
        command: Vec::new(),
        exec: None,
        script: None,
        replicas: Replicas::default(),
        port: None,
        health: None,
        memory: None,
        cpu: None,
        gpu: None,
        env: BTreeMap::new(),
        config_file: Vec::new(),
        volumes: Vec::new(),
        init: Vec::new(),
        ingress: None,
        placement: None,
        deploy: None,
        firewall: None,
        egress: None,
        autoscale: None,
        metrics: None,
        namespace: None,
        args: Vec::new(),
        working_dir: None,
        run_as_user: None,
        run_as_group: None,
    }
}

// ---------------------------------------------------------------------------
// Correlation helpers
// ---------------------------------------------------------------------------

/// Fold a Service into its workload's app.
///
/// An app has one port, reachable as `<app>:<port>` on the same number
/// inside the container, so the Service's first port decides which
/// container port the app exposes (`targetPort`, named or numeric). A
/// Service port that differs from its target, and any further Service
/// ports, can't be kept; both are reported, because clients using them
/// will fail to connect.
fn apply_service(
    app: &mut AppSpec,
    svc_name: &str,
    svc: &Service,
    pod_spec: Option<&k8s_openapi::api::core::v1::PodSpec>,
    report: &mut MigrationReport,
) {
    let resource = format!("Service/{svc_name}");
    let service_ports = svc
        .spec
        .as_ref()
        .and_then(|spec| spec.ports.as_deref())
        .unwrap_or_default();
    let Some(first) = service_ports.first() else {
        return;
    };
    let target = match &first.target_port {
        Some(target) => resolve_port(target, container_ports(pod_spec)),
        None => u16::try_from(first.port).ok(),
    };
    let Some(target) = target else {
        report.warnings.push(MigrationWarning {
            resource,
            message: format!(
                "targetPort {:?} names no container port; the app keeps port {}",
                first.target_port,
                app.port
                    .map_or_else(|| "none".to_string(), |p| p.to_string())
            ),
        });
        return;
    };

    if app.port != Some(target) {
        // The probe followed the old port implicitly; pin it before moving.
        if let (Some(old), Some(health)) = (app.port, app.health.as_mut())
            && health.port.is_none()
        {
            health.port = Some(old);
        }
        app.port = Some(target);
    }
    if u16::try_from(first.port).ok() != Some(target) {
        report.warnings.push(MigrationWarning {
            resource: resource.clone(),
            message: format!(
                "port {} forwards to container port {target}; Reliaburger has no port \
                 mapping, so clients must connect to {svc_name}:{target} (ingress is \
                 unaffected)",
                first.port
            ),
        });
    }
    let metrics_port = app.metrics_endpoint().map(|(port, _)| port);
    for extra in &service_ports[1..] {
        let extra_target = match &extra.target_port {
            Some(target) => resolve_port(target, container_ports(pod_spec)),
            None => u16::try_from(extra.port).ok(),
        };
        // The node scrapes the metrics port on each instance itself; nothing
        // needs the Service to carry it.
        if extra_target.is_some() && extra_target == metrics_port {
            continue;
        }
        report.warnings.push(MigrationWarning {
            resource: resource.clone(),
            message: format!(
                "port {}{} dropped; an app exposes one port",
                extra.port,
                extra
                    .name
                    .as_deref()
                    .map(|name| format!(" ({name})"))
                    .unwrap_or_default()
            ),
        });
    }
}

fn find_ingress_for_service(
    ingresses: &BTreeMap<String, Ingress>,
    service_name: &str,
) -> Option<String> {
    for (ing_name, ing) in ingresses {
        if let Some(spec) = &ing.spec {
            if let Some(rules) = &spec.rules {
                for rule in rules {
                    if let Some(http) = &rule.http {
                        for path in &http.paths {
                            if let Some(backend) = &path.backend.service {
                                if backend.name == service_name {
                                    return Some(ing_name.clone());
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    None
}

fn apply_ingress(
    app: &mut AppSpec,
    ingress_name: &str,
    ing: &Ingress,
    report: &mut MigrationReport,
) {
    if let Some(spec) = &ing.spec {
        // Reliaburger's IngressSpec is one host + one path prefix per app, so
        // everything beyond the first rule's first path is unrepresentable —
        // which must be *said*, not silently kept-first (M28 class).
        if let Some(rules) = &spec.rules {
            if rules.len() > 1 {
                report.warnings.push(MigrationWarning {
                    resource: format!("Ingress/{ingress_name}"),
                    message: format!(
                        "only the first of {} rules is imported; Reliaburger ingress is one host per app",
                        rules.len()
                    ),
                });
            }
            if let Some(rule) = rules.first() {
                if let Some(http) = &rule.http {
                    if http.paths.len() > 1 {
                        report.warnings.push(MigrationWarning {
                            resource: format!("Ingress/{ingress_name}"),
                            message: format!(
                                "only the first of {} paths is imported; Reliaburger ingress is one path prefix per app",
                                http.paths.len()
                            ),
                        });
                    }
                    if let Some(path_type) = http.paths.first().map(|p| p.path_type.as_str()) {
                        if path_type != "Prefix" && !path_type.is_empty() {
                            report.warnings.push(MigrationWarning {
                                resource: format!("Ingress/{ingress_name}"),
                                message: format!(
                                    "pathType {path_type} is imported as prefix matching"
                                ),
                            });
                        }
                    }
                }
            }
        }
        if spec.default_backend.is_some() {
            report.warnings.push(MigrationWarning {
                resource: format!("Ingress/{ingress_name}"),
                message: "defaultBackend is not imported".to_string(),
            });
        }
        if let Some(class) = spec.ingress_class_name.as_deref() {
            report.warnings.push(MigrationWarning {
                resource: format!("Ingress/{ingress_name}"),
                message: format!(
                    "ingressClassName {class} is dropped; Reliaburger's Wrapper serves all routes"
                ),
            });
        }
        if let Some(rules) = &spec.rules {
            if let Some(rule) = rules.first() {
                let host = rule.host.clone().unwrap_or_default();
                let path = rule
                    .http
                    .as_ref()
                    .and_then(|h| h.paths.first())
                    .map(|p| p.path.clone().unwrap_or_else(|| "/".to_string()))
                    .unwrap_or_else(|| "/".to_string());

                let tls = spec.tls.as_ref().map(|entries| {
                    let secret_names: Vec<&str> = entries
                        .iter()
                        .filter_map(|entry| entry.secret_name.as_deref())
                        .collect();
                    let source = if secret_names.is_empty() {
                        "Kubernetes TLS Secret material".to_string()
                    } else {
                        format!(
                            "Kubernetes TLS Secret material ({})",
                            secret_names.join(", ")
                        )
                    };
                    report.warnings.push(MigrationWarning {
                        resource: format!("Ingress/{ingress_name}"),
                        message: format!(
                            "{source} is not imported; using tls = \"cluster\", so clients must trust the Reliaburger cluster root CA"
                        ),
                    });
                    "cluster".to_string()
                });

                app.ingress = Some(IngressSpec {
                    host,
                    path: Some(path),
                    tls,
                    websocket: None,
                    rate_limit_rps: None,
                    rate_limit_burst: None,
                });
            }
        }
    }
}

fn apply_hpa(app: &mut AppSpec, hpa: &HorizontalPodAutoscaler) {
    if let Some(spec) = &hpa.spec {
        let min = spec.min_replicas.unwrap_or(1) as u32;
        let max = spec.max_replicas as u32;

        // Try to extract metric and target from the first metric
        let (metric, target) = spec
            .metrics
            .as_ref()
            .and_then(|metrics| metrics.first())
            .and_then(|m| {
                m.resource.as_ref().map(|r| {
                    let name = r.name.clone();
                    let target_val = r
                        .target
                        .average_utilization
                        .map(|v| format!("{v}%"))
                        .unwrap_or_else(|| "70%".to_string());
                    (name, target_val)
                })
            })
            .unwrap_or_else(|| ("cpu".to_string(), "70%".to_string()));

        app.autoscale = Some(AutoscaleSpec {
            metric,
            target,
            min,
            max,
            evaluation_window: None,
            cooldown: None,
            scale_down_threshold: None,
        });
    }
}

// ---------------------------------------------------------------------------
// Job conversion
// ---------------------------------------------------------------------------

/// Shared pod→JobSpec extraction for Job and CronJob — both used to keep only
/// image + command, dropping env, resources and the namespace on the floor.
fn pod_to_jobspec(
    resource: &str,
    metadata_namespace: Option<&String>,
    pod_spec: Option<&k8s_openapi::api::core::v1::PodSpec>,
    report: &mut MigrationReport,
) -> JobSpec {
    let container = pod_spec.and_then(|ps| ps.containers.first());

    // A job carries a single argv: command followed by args.
    let command = container.and_then(|c| {
        let command = c.command.clone().unwrap_or_default();
        let args = c.args.clone().unwrap_or_default();
        if command.is_empty() && !args.is_empty() {
            report.warnings.push(MigrationWarning {
                resource: resource.to_string(),
                message: "args without a command: the job's command replaces the image \
                          entrypoint, so add the entrypoint to it"
                    .to_string(),
            });
        }
        let argv: Vec<String> = command.into_iter().chain(args).collect();
        (!argv.is_empty()).then_some(argv)
    });

    let mut env = BTreeMap::new();
    if let Some(env_list) = container.and_then(|c| c.env.as_ref()) {
        for env_var in env_list {
            if let Some(ref value) = env_var.value {
                env.insert(env_var.name.clone(), EnvValue::Plain(value.clone()));
            } else if env_var.value_from.is_some() {
                report.warnings.push(MigrationWarning {
                    resource: resource.to_string(),
                    message: format!(
                        "env {} uses valueFrom (secret/configmap/field ref) — set the value manually or use `relish secret encrypt`",
                        env_var.name
                    ),
                });
            }
        }
    }

    let (memory, cpu) = match container.and_then(|c| c.resources.as_ref()) {
        Some(resources) => (
            resource_range_from(
                resource,
                "memory",
                resources,
                parse_k8s_memory_bytes,
                report,
            ),
            resource_range_from(resource, "cpu", resources, parse_k8s_cpu_millicores, report),
        ),
        None => (None, None),
    };

    JobSpec {
        image: container.and_then(|c| c.image.clone()),
        command,
        schedule: None,
        run_before: Vec::new(),
        memory,
        cpu,
        env,
        namespace: metadata_namespace.cloned(),
        exec: None,
        script: None,
    }
}

fn job_to_jobspec(name: &str, job: &Job, report: &mut MigrationReport) -> JobSpec {
    let pod_spec = job.spec.as_ref().and_then(|s| s.template.spec.as_ref());
    pod_to_jobspec(
        &format!("Job/{name}"),
        job.metadata.namespace.as_ref(),
        pod_spec,
        report,
    )
}

fn cronjob_to_jobspec(name: &str, cj: &CronJob, report: &mut MigrationReport) -> JobSpec {
    let spec = cj.spec.as_ref();
    let job_template = spec.and_then(|s| s.job_template.spec.as_ref());
    let pod_spec = job_template.and_then(|jt| jt.template.spec.as_ref());

    let mut job_spec = pod_to_jobspec(
        &format!("CronJob/{name}"),
        cj.metadata.namespace.as_ref(),
        pod_spec,
        report,
    );
    job_spec.schedule = spec.map(|s| s.schedule.clone());

    // Scheduling behaviour Reliaburger doesn't model — say so.
    if spec.and_then(|s| s.suspend) == Some(true) {
        report.warnings.push(MigrationWarning {
            resource: format!("CronJob/{name}"),
            message: "suspend = true is not imported; the job will run on its schedule".to_string(),
        });
    }
    if let Some(policy) = spec.and_then(|s| s.concurrency_policy.as_ref()) {
        if policy != "Allow" {
            report.warnings.push(MigrationWarning {
                resource: format!("CronJob/{name}"),
                message: format!("concurrencyPolicy {policy} is not imported"),
            });
        }
    }
    job_spec
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn import_deployment_to_app() {
        let yaml = r#"
apiVersion: apps/v1
kind: Deployment
metadata:
  name: web
spec:
  replicas: 3
  template:
    spec:
      containers:
      - name: web
        image: myapp:v1
        ports:
        - containerPort: 8080
"#;
        let result = import_from_yaml(yaml).unwrap();
        let app = &result.config.app["web"];
        assert_eq!(app.image.as_deref(), Some("myapp:v1"));
        assert_eq!(app.replicas, Replicas::Fixed(3));
        assert_eq!(app.port, Some(8080));
    }

    /// M17 regression: `command`/`args`, `env.valueFrom` and the K8s
    /// namespace used to be silently dropped on import.
    #[test]
    fn k8s_import_preserves_command_args_valuefrom_namespace() {
        let yaml = r#"
apiVersion: apps/v1
kind: Deployment
metadata:
  name: worker
  namespace: staging
spec:
  replicas: 1
  template:
    spec:
      containers:
      - name: worker
        image: worker:v3
        command: ["python"]
        args: ["-m", "worker.main"]
        env:
        - name: MODE
          value: fast
        - name: DB_PASSWORD
          valueFrom:
            secretKeyRef:
              name: db-secret
              key: password
        - name: NODE_NAME
          valueFrom:
            fieldRef:
              fieldPath: spec.nodeName
"#;
        let result = import_from_yaml(yaml).unwrap();
        let app = &result.config.app["worker"];

        // command and args stay separate, so the runtime applies the
        // Kubernetes rules against the image (Z1.3)
        assert_eq!(app.command, vec!["python"]);
        assert_eq!(app.args, vec!["-m", "worker.main"]);

        // namespace preserved
        assert_eq!(app.namespace.as_deref(), Some("staging"));

        // plain env kept
        assert!(matches!(
            app.env.get("MODE"),
            Some(EnvValue::Plain(v)) if v == "fast"
        ));

        // valueFrom entries not silently dropped: absent from env but warned
        assert!(!app.env.contains_key("DB_PASSWORD"));
        let warnings: Vec<String> = result
            .report
            .warnings
            .iter()
            .map(|w| format!("{}: {}", w.resource, w.message))
            .collect();
        assert!(
            warnings.iter().any(|w| w.contains("DB_PASSWORD")),
            "expected a valueFrom warning for DB_PASSWORD, got: {warnings:?}"
        );
        assert!(
            warnings.iter().any(|w| w.contains("NODE_NAME")),
            "expected a valueFrom warning for NODE_NAME, got: {warnings:?}"
        );
    }

    #[test]
    fn k8s_import_same_name_in_two_namespaces_does_not_overwrite() {
        let yaml = r#"
apiVersion: apps/v1
kind: Deployment
metadata:
  name: api
  namespace: alpha
spec:
  template:
    spec:
      containers:
      - name: api
        image: api:alpha
---
apiVersion: apps/v1
kind: Deployment
metadata:
  name: api
  namespace: beta
spec:
  template:
    spec:
      containers:
      - name: api
        image: api:beta
"#;
        let result = import_from_yaml(yaml).unwrap();
        assert_eq!(result.config.app.len(), 2, "second app must not overwrite");
        assert_eq!(result.config.app["api"].image.as_deref(), Some("api:alpha"));
        assert_eq!(
            result.config.app["beta-api"].image.as_deref(),
            Some("api:beta")
        );
    }

    #[test]
    fn import_correlates_deployment_service_ingress() {
        let yaml = r#"
apiVersion: apps/v1
kind: Deployment
metadata:
  name: web
spec:
  replicas: 2
  template:
    spec:
      containers:
      - name: web
        image: myapp:v1
        ports:
        - containerPort: 8080
---
apiVersion: v1
kind: Service
metadata:
  name: web
spec:
  ports:
  - port: 80
    targetPort: 8080
---
apiVersion: networking.k8s.io/v1
kind: Ingress
metadata:
  name: web-ingress
spec:
  tls:
  - hosts:
    - myapp.com
    secretName: web-tls
  rules:
  - host: myapp.com
    http:
      paths:
      - path: /
        pathType: Prefix
        backend:
          service:
            name: web
            port:
              number: 80
"#;
        let result = import_from_yaml(yaml).unwrap();
        assert_eq!(result.config.app.len(), 1);
        let app = &result.config.app["web"];
        assert!(app.ingress.is_some());
        assert_eq!(app.ingress.as_ref().unwrap().host, "myapp.com");
        assert_eq!(
            app.ingress.as_ref().unwrap().tls.as_deref(),
            Some("cluster")
        );
        assert!(result.report.warnings.iter().any(|warning| {
            warning.resource == "Ingress/web-ingress"
                && warning.message.contains("web-tls")
                && warning.message.contains("not imported")
        }));
    }

    #[test]
    fn import_daemonset_uses_star_replicas() {
        let yaml = r#"
apiVersion: apps/v1
kind: DaemonSet
metadata:
  name: monitoring
spec:
  template:
    spec:
      containers:
      - name: agent
        image: monitor:v1
"#;
        let result = import_from_yaml(yaml).unwrap();
        let app = &result.config.app["monitoring"];
        assert_eq!(app.replicas, Replicas::DaemonSet);
    }

    #[test]
    fn import_hpa_to_autoscale() {
        let yaml = r#"
apiVersion: apps/v1
kind: Deployment
metadata:
  name: api
spec:
  replicas: 3
  template:
    spec:
      containers:
      - name: api
        image: api:v1
---
apiVersion: autoscaling/v2
kind: HorizontalPodAutoscaler
metadata:
  name: api
spec:
  scaleTargetRef:
    name: api
  minReplicas: 2
  maxReplicas: 10
  metrics:
  - type: Resource
    resource:
      name: cpu
      target:
        type: Utilization
        averageUtilization: 70
"#;
        let result = import_from_yaml(yaml).unwrap();
        let app = &result.config.app["api"];
        let auto = app.autoscale.as_ref().unwrap();
        assert_eq!(auto.metric, "cpu");
        assert_eq!(auto.target, "70%");
        assert_eq!(auto.min, 2);
        assert_eq!(auto.max, 10);
    }

    #[test]
    fn import_secret_produces_enc_placeholder() {
        let yaml = r#"
apiVersion: v1
kind: Secret
metadata:
  name: api-secrets
data:
  DB_PASSWORD: cGFzc3dvcmQ=
"#;
        let result = import_from_yaml(yaml).unwrap();
        // Secrets aren't auto-correlated to apps, they appear as warnings
        assert!(
            result
                .report
                .warnings
                .iter()
                .any(|w| w.resource.contains("Secret"))
        );
    }

    #[test]
    fn import_job_and_cronjob() {
        let yaml = r#"
apiVersion: batch/v1
kind: Job
metadata:
  name: db-migrate
spec:
  template:
    spec:
      containers:
      - name: migrate
        image: migrate:v1
        command: ["npm", "run", "migrate"]
      restartPolicy: Never
---
apiVersion: batch/v1
kind: CronJob
metadata:
  name: cleanup
spec:
  schedule: "0 3 * * *"
  jobTemplate:
    spec:
      template:
        spec:
          containers:
          - name: cleanup
            image: cleanup:latest
          restartPolicy: Never
"#;
        let result = import_from_yaml(yaml).unwrap();
        assert_eq!(result.config.job.len(), 2);
        assert!(result.config.job.contains_key("db-migrate"));
        assert!(result.config.job.contains_key("cleanup"));
        assert_eq!(
            result.config.job["cleanup"].schedule.as_deref(),
            Some("0 3 * * *")
        );
    }

    #[test]
    fn import_migration_report_warns_on_statefulset() {
        let yaml = r#"
apiVersion: apps/v1
kind: StatefulSet
metadata:
  name: redis
spec:
  replicas: 3
  template:
    spec:
      containers:
      - name: redis
        image: redis:7
"#;
        let result = import_from_yaml(yaml).unwrap();
        assert!(
            result
                .report
                .warnings
                .iter()
                .any(|w| w.resource.contains("StatefulSet") && w.message.contains("ordering"))
        );
    }

    #[test]
    fn import_multi_document_yaml() {
        let yaml = r#"
apiVersion: apps/v1
kind: Deployment
metadata:
  name: web
spec:
  replicas: 1
  template:
    spec:
      containers:
      - name: web
        image: web:v1
---
apiVersion: apps/v1
kind: Deployment
metadata:
  name: api
spec:
  replicas: 2
  template:
    spec:
      containers:
      - name: api
        image: api:v1
"#;
        let result = import_from_yaml(yaml).unwrap();
        assert_eq!(result.config.app.len(), 2);
        assert!(result.config.app.contains_key("web"));
        assert!(result.config.app.contains_key("api"));
    }

    /// M28: a Helm-rendered manifest prefixes each document with a `# Source:`
    /// comment. The old `split("---")` + `starts_with('#')` skip dropped every
    /// such document and exited 0. The Deserializer path imports them.
    #[test]
    fn import_helm_style_source_comments() {
        let yaml = r#"---
# Source: chart/templates/web.yaml
apiVersion: apps/v1
kind: Deployment
metadata:
  name: web
spec:
  replicas: 1
  template:
    spec:
      containers:
      - name: web
        image: web:v1
---
# Source: chart/templates/api.yaml
apiVersion: apps/v1
kind: Deployment
metadata:
  name: api
spec:
  replicas: 2
  template:
    spec:
      containers:
      - name: api
        image: api:v1
"#;
        let result = import_from_yaml(yaml).unwrap();
        assert_eq!(result.config.app.len(), 2, "Helm docs must not be skipped");
        assert!(result.config.app.contains_key("web"));
        assert!(result.config.app.contains_key("api"));
    }

    #[test]
    fn import_unknown_kind_in_report() {
        let yaml = r#"
apiVersion: custom.io/v1
kind: MyCustomResource
metadata:
  name: foo
spec:
  bar: baz
"#;
        let result = import_from_yaml(yaml).unwrap();
        assert!(
            result
                .report
                .dropped
                .iter()
                .any(|d| d.contains("MyCustomResource"))
        );
    }

    #[test]
    fn import_deployment_with_health_check() {
        let yaml = r#"
apiVersion: apps/v1
kind: Deployment
metadata:
  name: web
spec:
  replicas: 1
  template:
    spec:
      containers:
      - name: web
        image: web:v1
        readinessProbe:
          httpGet:
            path: /healthz
            port: 8080
          periodSeconds: 10
          failureThreshold: 3
"#;
        let result = import_from_yaml(yaml).unwrap();
        let health = result.config.app["web"].health.as_ref().unwrap();
        assert_eq!(health.path, "/healthz");
        assert_eq!(health.interval, Some(10));
        assert_eq!(health.threshold_unhealthy, Some(3));
    }

    // -- import fidelity (deep audit) -----------------------------------------

    /// The HPA lookup used to key on the HPA's own metadata name, so the
    /// conventional `api-hpa` → `api` pairing never correlated and the
    /// autoscaling silently vanished.
    #[test]
    fn hpa_correlates_by_scale_target_ref_not_its_own_name() {
        let yaml = r#"
apiVersion: apps/v1
kind: Deployment
metadata:
  name: api
spec:
  replicas: 2
  template:
    spec:
      containers:
      - name: api
        image: api:v1
---
apiVersion: autoscaling/v2
kind: HorizontalPodAutoscaler
metadata:
  name: api-hpa
spec:
  scaleTargetRef:
    apiVersion: apps/v1
    kind: Deployment
    name: api
  minReplicas: 2
  maxReplicas: 8
"#;
        let result = import_from_yaml(yaml).unwrap();
        let auto = result.config.app["api"]
            .autoscale
            .as_ref()
            .expect("api-hpa must correlate with api via scaleTargetRef");
        assert_eq!(auto.min, 2);
        assert_eq!(auto.max, 8);
    }

    /// An Ingress or HPA that matches no imported workload used to vanish
    /// with no trace; ConfigMaps and Secrets were always reported.
    #[test]
    fn uncorrelated_ingress_and_hpa_are_warned_not_dropped_silently() {
        let yaml = r#"
apiVersion: networking.k8s.io/v1
kind: Ingress
metadata:
  name: orphan-ing
spec:
  rules:
  - host: gone.example.com
    http:
      paths:
      - path: /
        pathType: Prefix
        backend:
          service:
            name: nonexistent
            port:
              number: 80
---
apiVersion: autoscaling/v2
kind: HorizontalPodAutoscaler
metadata:
  name: orphan-hpa
spec:
  scaleTargetRef:
    kind: Deployment
    name: nonexistent
  maxReplicas: 4
"#;
        let result = import_from_yaml(yaml).unwrap();
        let warnings: Vec<String> = result
            .report
            .warnings
            .iter()
            .map(|w| format!("{}: {}", w.resource, w.message))
            .collect();
        assert!(
            warnings
                .iter()
                .any(|w| w.contains("Ingress/orphan-ing") && w.contains("routing dropped")),
            "{warnings:?}"
        );
        assert!(
            warnings
                .iter()
                .any(|w| w.contains("HorizontalPodAutoscaler/orphan-hpa")
                    && w.contains("autoscaling dropped")),
            "{warnings:?}"
        );
    }

    /// DaemonSets and StatefulSets used to keep only image/namespace/
    /// replicas/port — command, env, resources and probes vanished.
    #[test]
    fn daemonset_and_statefulset_keep_the_full_pod_spec() {
        let yaml = r#"
apiVersion: apps/v1
kind: DaemonSet
metadata:
  name: agent
spec:
  template:
    spec:
      containers:
      - name: agent
        image: agent:v1
        command: ["./agent"]
        args: ["--verbose"]
        env:
        - name: LEVEL
          value: debug
        resources:
          limits:
            cpu: 500m
            memory: 256Mi
        readinessProbe:
          httpGet:
            path: /ready
            port: 9100
---
apiVersion: apps/v1
kind: StatefulSet
metadata:
  name: db
spec:
  replicas: 3
  template:
    spec:
      containers:
      - name: db
        image: db:v2
        env:
        - name: DATA_DIR
          value: /data
"#;
        let result = import_from_yaml(yaml).unwrap();
        let agent = &result.config.app["agent"];
        assert_eq!(agent.replicas, Replicas::DaemonSet);
        assert_eq!(agent.command, vec!["./agent"]);
        assert_eq!(agent.args, vec!["--verbose"]);
        assert!(matches!(
            agent.env.get("LEVEL"),
            Some(EnvValue::Plain(v)) if v == "debug"
        ));
        let cpu = agent.cpu.expect("daemonset cpu must import");
        assert_eq!(cpu.limit, 500);
        assert_eq!(agent.health.as_ref().unwrap().path, "/ready");

        let db = &result.config.app["db"];
        assert_eq!(db.replicas, Replicas::Fixed(3));
        assert!(matches!(
            db.env.get("DATA_DIR"),
            Some(EnvValue::Plain(v)) if v == "/data"
        ));
    }

    /// A StatefulSet's HPA correlates via scaleTargetRef too (it used to be
    /// Deployment-only).
    #[test]
    fn statefulset_hpa_correlates() {
        let yaml = r#"
apiVersion: apps/v1
kind: StatefulSet
metadata:
  name: queue
spec:
  replicas: 2
  template:
    spec:
      containers:
      - name: queue
        image: queue:v1
---
apiVersion: autoscaling/v2
kind: HorizontalPodAutoscaler
metadata:
  name: queue-hpa
spec:
  scaleTargetRef:
    kind: StatefulSet
    name: queue
  maxReplicas: 6
"#;
        let result = import_from_yaml(yaml).unwrap();
        assert!(
            result.config.app["queue"].autoscale.is_some(),
            "StatefulSet HPA must correlate"
        );
    }

    /// K8s CPU is denominated in cores; the old code read `cpu: "1"` through
    /// Reliaburger's own parser, where a bare integer means millicores — a
    /// 1000× under-read. And requests-only manifests (the common case)
    /// imported with no resources at all because only `limits` was read.
    #[test]
    fn requests_only_deployment_imports_real_k8s_quantities() {
        let yaml = r#"
apiVersion: apps/v1
kind: Deployment
metadata:
  name: web
spec:
  replicas: 1
  template:
    spec:
      containers:
      - name: web
        image: web:v1
        resources:
          requests:
            cpu: "1"
            memory: 512M
"#;
        let result = import_from_yaml(yaml).unwrap();
        let app = &result.config.app["web"];
        let cpu = app.cpu.expect("requests-only cpu must import");
        assert_eq!(
            cpu.request, 1000,
            "cpu: \"1\" is one core = 1000 millicores"
        );
        assert_eq!(cpu.limit, 1000, "a missing limit takes the request value");
        let memory = app.memory.expect("requests-only memory must import");
        assert_eq!(memory.request, 512_000_000, "512M is decimal megabytes");
    }

    #[test]
    fn requests_and_limits_both_import_as_the_range() {
        let yaml = r#"
apiVersion: apps/v1
kind: Deployment
metadata:
  name: web
spec:
  replicas: 1
  template:
    spec:
      containers:
      - name: web
        image: web:v1
        resources:
          requests:
            cpu: 250m
            memory: 128Mi
          limits:
            cpu: "0.5"
            memory: 256Mi
"#;
        let result = import_from_yaml(yaml).unwrap();
        let app = &result.config.app["web"];
        let cpu = app.cpu.unwrap();
        assert_eq!((cpu.request, cpu.limit), (250, 500));
        let memory = app.memory.unwrap();
        assert_eq!(
            (memory.request, memory.limit),
            (128 * 1024 * 1024, 256 * 1024 * 1024)
        );
    }

    #[test]
    fn unparseable_quantity_warns_instead_of_vanishing() {
        let yaml = r#"
apiVersion: apps/v1
kind: Deployment
metadata:
  name: web
spec:
  replicas: 1
  template:
    spec:
      containers:
      - name: web
        image: web:v1
        resources:
          limits:
            cpu: banana
"#;
        let result = import_from_yaml(yaml).unwrap();
        assert!(result.config.app["web"].cpu.is_none());
        assert!(
            result
                .report
                .warnings
                .iter()
                .any(|w| w.message.contains("could not parse cpu")),
            "{:?}",
            result.report.warnings
        );
    }

    #[test]
    fn k8s_quantity_parsers_cover_the_k8s_forms() {
        assert_eq!(parse_k8s_cpu_millicores("1"), Some(1000));
        assert_eq!(parse_k8s_cpu_millicores("0.5"), Some(500));
        assert_eq!(parse_k8s_cpu_millicores("500m"), Some(500));
        assert_eq!(parse_k8s_cpu_millicores("1.5"), Some(1500));
        assert_eq!(parse_k8s_cpu_millicores("banana"), None);
        assert_eq!(parse_k8s_cpu_millicores("-1"), None);

        assert_eq!(parse_k8s_memory_bytes("1Ki"), Some(1024));
        assert_eq!(parse_k8s_memory_bytes("256Mi"), Some(256 * 1024 * 1024));
        assert_eq!(parse_k8s_memory_bytes("1Gi"), Some(1024 * 1024 * 1024));
        assert_eq!(parse_k8s_memory_bytes("1k"), Some(1000));
        assert_eq!(parse_k8s_memory_bytes("512M"), Some(512_000_000));
        assert_eq!(parse_k8s_memory_bytes("1G"), Some(1_000_000_000));
        assert_eq!(parse_k8s_memory_bytes("1e9"), Some(1_000_000_000));
        assert_eq!(parse_k8s_memory_bytes("1048576"), Some(1_048_576));
        assert_eq!(parse_k8s_memory_bytes("banana"), None);
    }

    /// Reliaburger ingress is one host + one path prefix per app; everything
    /// beyond the first rule's first path used to be kept-first silently.
    #[test]
    fn multi_rule_ingress_warns_about_what_it_drops() {
        let yaml = r#"
apiVersion: apps/v1
kind: Deployment
metadata:
  name: web
spec:
  replicas: 1
  template:
    spec:
      containers:
      - name: web
        image: web:v1
---
apiVersion: networking.k8s.io/v1
kind: Ingress
metadata:
  name: web-ing
spec:
  ingressClassName: nginx
  rules:
  - host: a.example.com
    http:
      paths:
      - path: /
        pathType: Exact
        backend:
          service:
            name: web
            port:
              number: 80
      - path: /api
        pathType: Prefix
        backend:
          service:
            name: web
            port:
              number: 80
  - host: b.example.com
    http:
      paths:
      - path: /
        pathType: Prefix
        backend:
          service:
            name: web
            port:
              number: 80
"#;
        let result = import_from_yaml(yaml).unwrap();
        let app = &result.config.app["web"];
        assert_eq!(app.ingress.as_ref().unwrap().host, "a.example.com");
        let warnings: Vec<&str> = result
            .report
            .warnings
            .iter()
            .map(|w| w.message.as_str())
            .collect();
        assert!(
            warnings.iter().any(|w| w.contains("first of 2 rules")),
            "{warnings:?}"
        );
        assert!(
            warnings.iter().any(|w| w.contains("first of 2 paths")),
            "{warnings:?}"
        );
        assert!(
            warnings.iter().any(|w| w.contains("pathType Exact")),
            "{warnings:?}"
        );
        assert!(
            warnings.iter().any(|w| w.contains("ingressClassName")),
            "{warnings:?}"
        );
    }

    /// Jobs and CronJobs used to keep only image + command.
    #[test]
    fn job_and_cronjob_keep_env_resources_and_namespace() {
        let yaml = r#"
apiVersion: batch/v1
kind: Job
metadata:
  name: migrate
  namespace: staging
spec:
  template:
    spec:
      containers:
      - name: migrate
        image: migrate:v1
        command: ["npm"]
        args: ["run", "migrate"]
        env:
        - name: DATABASE_URL
          value: postgres://db/main
        resources:
          limits:
            cpu: 200m
            memory: 128Mi
---
apiVersion: batch/v1
kind: CronJob
metadata:
  name: cleanup
  namespace: staging
spec:
  schedule: "0 3 * * *"
  suspend: true
  concurrencyPolicy: Forbid
  jobTemplate:
    spec:
      template:
        spec:
          containers:
          - name: cleanup
            image: cleanup:v1
            env:
            - name: DRY_RUN
              value: "false"
"#;
        let result = import_from_yaml(yaml).unwrap();
        let migrate = &result.config.job["migrate"];
        assert_eq!(
            migrate.command.as_deref(),
            Some(["npm", "run", "migrate"].map(String::from).as_slice())
        );
        assert_eq!(migrate.namespace.as_deref(), Some("staging"));
        assert!(matches!(
            migrate.env.get("DATABASE_URL"),
            Some(EnvValue::Plain(v)) if v == "postgres://db/main"
        ));
        assert_eq!(migrate.cpu.unwrap().limit, 200);
        assert_eq!(migrate.memory.unwrap().limit, 128 * 1024 * 1024);

        let cleanup = &result.config.job["cleanup"];
        assert_eq!(cleanup.schedule.as_deref(), Some("0 3 * * *"));
        assert_eq!(cleanup.namespace.as_deref(), Some("staging"));
        assert!(matches!(
            cleanup.env.get("DRY_RUN"),
            Some(EnvValue::Plain(v)) if v == "false"
        ));
        let warnings: Vec<&str> = result
            .report
            .warnings
            .iter()
            .map(|w| w.message.as_str())
            .collect();
        assert!(
            warnings.iter().any(|w| w.contains("suspend")),
            "{warnings:?}"
        );
        assert!(
            warnings
                .iter()
                .any(|w| w.contains("concurrencyPolicy Forbid")),
            "{warnings:?}"
        );
    }

    fn warnings_of(result: &ImportResult) -> Vec<String> {
        result
            .report
            .warnings
            .iter()
            .map(|w| format!("{}: {}", w.resource, w.message))
            .collect()
    }

    /// The podinfo frontend's shape: named ports, a Service on port 80
    /// targeting the named `http` port, and exec readiness probes.
    const PODINFO_FRONTEND: &str = r#"
apiVersion: apps/v1
kind: Deployment
metadata:
  name: frontend
spec:
  template:
    spec:
      securityContext:
        runAsUser: 100
      containers:
      - name: frontend
        image: ghcr.io/stefanprodan/podinfo:6.15.0
        workingDir: /home/app
        ports:
        - name: http-metrics
          containerPort: 9797
        - name: http
          containerPort: 9898
        args: ["--port=9898", "--backend-url=http://backend:9898/echo"]
        securityContext:
          runAsGroup: 101
        readinessProbe:
          exec:
            command: ["podcli", "check", "http", "localhost:9898/readyz"]
---
apiVersion: v1
kind: Service
metadata:
  name: frontend
spec:
  ports:
  - name: http
    port: 80
    targetPort: http
  - name: metrics
    port: 9797
    targetPort: http-metrics
"#;

    /// A Deployment with the given pod-template annotations and container
    /// ports, plus a matching Service on the first port.
    fn annotated_deployment(annotations: &str, ports: &str) -> String {
        format!(
            r#"
apiVersion: apps/v1
kind: Deployment
metadata:
  name: web
spec:
  template:
    metadata:
      annotations:
{annotations}
    spec:
      containers:
      - name: web
        image: podinfo:6
{ports}
"#
        )
    }

    #[test]
    fn prometheus_annotations_become_the_apps_metrics() {
        let yaml = annotated_deployment(
            "        prometheus.io/scrape: \"true\"\n        prometheus.io/port: \"9797\"\n        prometheus.io/path: /prom",
            "        ports:\n        - name: http\n          containerPort: 9898\n        - name: http-metrics\n          containerPort: 9797",
        );
        let result = import_from_yaml(&yaml).unwrap();
        let app = &result.config.app["web"];
        assert_eq!(app.port, Some(9898));
        assert_eq!(app.metrics_endpoint(), Some((9797, "/prom")));
    }

    #[test]
    fn scrape_without_a_port_annotation_uses_the_app_port_and_slash_metrics() {
        let yaml = annotated_deployment(
            "        prometheus.io/scrape: \"true\"",
            "        ports:\n        - containerPort: 8080",
        );
        let result = import_from_yaml(&yaml).unwrap();
        let app = &result.config.app["web"];
        assert_eq!(app.metrics_endpoint(), Some((8080, "/metrics")));
        assert!(
            warnings_of(&result).is_empty(),
            "{:?}",
            warnings_of(&result)
        );
    }

    #[test]
    fn scrape_false_or_absent_leaves_metrics_unset() {
        let yaml = annotated_deployment(
            "        prometheus.io/scrape: \"false\"\n        prometheus.io/port: \"9797\"",
            "        ports:\n        - containerPort: 8080",
        );
        let result = import_from_yaml(&yaml).unwrap();
        assert!(result.config.app["web"].metrics.is_none());
    }

    #[test]
    fn scrape_with_no_port_anywhere_is_warned_and_skipped() {
        let yaml = annotated_deployment("        prometheus.io/scrape: \"true\"", "");
        let result = import_from_yaml(&yaml).unwrap();
        assert!(result.config.app["web"].metrics.is_none());
        let warnings = warnings_of(&result);
        assert!(
            warnings.iter().any(|w| w.starts_with("Deployment/web")
                && w.contains("prometheus.io/scrape")
                && w.contains("no port")),
            "{warnings:?}"
        );
        // The imported config must still validate.
        result.config.validate().unwrap();
    }

    #[test]
    fn an_unparseable_port_annotation_is_warned_and_falls_back_to_the_app_port() {
        let yaml = annotated_deployment(
            "        prometheus.io/scrape: \"true\"\n        prometheus.io/port: metrics",
            "        ports:\n        - containerPort: 8080",
        );
        let result = import_from_yaml(&yaml).unwrap();
        assert_eq!(
            result.config.app["web"].metrics_endpoint(),
            Some((8080, "/metrics"))
        );
        assert!(
            warnings_of(&result)
                .iter()
                .any(|w| w.contains("prometheus.io/port \"metrics\"")),
            "{:?}",
            warnings_of(&result)
        );
    }

    #[test]
    fn the_shipped_podinfo_demo_scrapes_its_metrics_port_without_dropping_it() {
        let manifest = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("examples/kubernetes/podinfo.yaml"),
        )
        .unwrap();
        let result = import_from_yaml(&manifest).unwrap();
        for name in ["frontend", "backend"] {
            assert_eq!(
                result.config.app[name].metrics_endpoint(),
                Some((9797, "/metrics")),
                "{name}"
            );
        }
        assert!(result.config.app["redis"].metrics.is_none());
        let warnings = warnings_of(&result);
        assert!(
            !warnings.iter().any(|w| w.contains("9797")),
            "the scraped metrics port is reported as dropped: {warnings:?}"
        );
        // The gRPC port is still honestly reported.
        assert!(warnings.iter().any(|w| w.contains("9999")), "{warnings:?}");
    }

    #[test]
    fn args_alone_keep_the_image_entrypoint() {
        let result = import_from_yaml(PODINFO_FRONTEND).unwrap();
        let app = &result.config.app["frontend"];
        assert!(app.command.is_empty());
        assert_eq!(
            app.args,
            vec!["--port=9898", "--backend-url=http://backend:9898/echo"]
        );
    }

    #[test]
    fn working_dir_and_run_as_ids_import() {
        let result = import_from_yaml(PODINFO_FRONTEND).unwrap();
        let app = &result.config.app["frontend"];
        assert_eq!(
            app.working_dir.as_deref(),
            Some(std::path::Path::new("/home/app"))
        );
        // Pod-level runAsUser, container-level runAsGroup.
        assert_eq!(app.run_as_user, Some(100));
        assert_eq!(app.run_as_group, Some(101));
    }

    #[test]
    fn a_named_service_target_port_picks_the_apps_port() {
        let result = import_from_yaml(PODINFO_FRONTEND).unwrap();
        assert_eq!(result.config.app["frontend"].port, Some(9898));
    }

    #[test]
    fn a_service_port_that_differs_from_its_target_is_reported() {
        let result = import_from_yaml(PODINFO_FRONTEND).unwrap();
        let warnings = warnings_of(&result);
        assert!(
            warnings.iter().any(|w| w.starts_with("Service/frontend")
                && w.contains("port 80 forwards to container port 9898")
                && w.contains("frontend:9898")),
            "{warnings:?}"
        );
        assert!(
            warnings
                .iter()
                .any(|w| w.starts_with("Service/frontend")
                    && w.contains("port 9797 (metrics) dropped")),
            "{warnings:?}"
        );
    }

    /// With `prometheus.io/scrape`, the metrics port is scraped directly on
    /// each instance, so neither the Service nor the container port that
    /// carries it is "dropped".
    #[test]
    fn a_scraped_metrics_port_is_not_reported_as_dropped() {
        let yaml = PODINFO_FRONTEND.replacen(
            "  template:\n    spec:",
            "  template:\n    metadata:\n      annotations:\n        prometheus.io/scrape: \"true\"\n        prometheus.io/port: \"9797\"\n    spec:",
            1,
        );
        let result = import_from_yaml(&yaml).unwrap();
        assert_eq!(
            result.config.app["frontend"].metrics_endpoint(),
            Some((9797, "/metrics"))
        );
        let warnings = warnings_of(&result);
        assert!(!warnings.iter().any(|w| w.contains("9797")), "{warnings:?}");
    }

    #[test]
    fn a_service_on_its_target_port_is_not_reported() {
        let yaml = r#"
apiVersion: apps/v1
kind: Deployment
metadata:
  name: backend
spec:
  template:
    spec:
      containers:
      - name: backend
        image: podinfo:6
        ports:
        - name: http
          containerPort: 9898
---
apiVersion: v1
kind: Service
metadata:
  name: backend
spec:
  ports:
  - port: 9898
    targetPort: http
"#;
        let result = import_from_yaml(yaml).unwrap();
        assert_eq!(result.config.app["backend"].port, Some(9898));
        assert!(
            warnings_of(&result).is_empty(),
            "{:?}",
            warnings_of(&result)
        );
    }

    #[test]
    fn extra_container_ports_are_reported() {
        let result = import_from_yaml(PODINFO_FRONTEND).unwrap();
        let warnings = warnings_of(&result);
        assert!(
            warnings.iter().any(|w| w.starts_with("Deployment/frontend")
                && w.contains("one port (9898)")
                && w.contains("9797 (http-metrics)")),
            "{warnings:?}"
        );
    }

    #[test]
    fn non_http_readiness_probes_are_reported_not_dropped() {
        let result = import_from_yaml(PODINFO_FRONTEND).unwrap();
        assert!(result.config.app["frontend"].health.is_none());
        let warnings = warnings_of(&result);
        assert!(
            warnings.iter().any(|w| w.contains(
                "readinessProbe runs a command (podcli check http localhost:9898/readyz)"
            )),
            "{warnings:?}"
        );

        let tcp = r#"
apiVersion: apps/v1
kind: Deployment
metadata:
  name: cache
spec:
  template:
    spec:
      containers:
      - name: redis
        image: redis:8
        ports:
        - containerPort: 6379
        readinessProbe:
          tcpSocket:
            port: 6379
"#;
        let result = import_from_yaml(tcp).unwrap();
        assert!(
            warnings_of(&result)
                .iter()
                .any(|w| w.contains("readinessProbe is a tcpSocket check")),
            "{:?}",
            warnings_of(&result)
        );
    }

    #[test]
    fn an_http_probe_on_a_named_port_resolves_it() {
        let yaml = r#"
apiVersion: apps/v1
kind: Deployment
metadata:
  name: web
spec:
  template:
    spec:
      containers:
      - name: web
        image: web:1
        ports:
        - name: http
          containerPort: 8080
        - name: admin
          containerPort: 9090
        readinessProbe:
          httpGet:
            path: /ready
            port: admin
"#;
        let result = import_from_yaml(yaml).unwrap();
        let health = result.config.app["web"].health.as_ref().unwrap();
        assert_eq!(health.path, "/ready");
        assert_eq!(health.port, Some(9090));
    }

    #[test]
    fn a_service_without_a_matching_workload_is_reported() {
        let yaml = r#"
apiVersion: v1
kind: Service
metadata:
  name: cache
spec:
  ports:
  - port: 6379
"#;
        let result = import_from_yaml(yaml).unwrap();
        assert!(
            warnings_of(&result)
                .iter()
                .any(|w| w.starts_with("Service/cache") && w.contains("no imported workload")),
            "{:?}",
            warnings_of(&result)
        );
    }

    /// Z1.5: the tutorial's demo manifest imports cleanly into the three
    /// apps it describes, and the result is a valid config.
    #[test]
    fn the_podinfo_demo_manifest_imports_into_three_apps() {
        let yaml = include_str!("../../examples/kubernetes/podinfo.yaml");
        let result = import_from_yaml(yaml).unwrap();
        result.config.validate().unwrap();
        assert!(result.report.dropped.is_empty(), "{}", result.report);
        let names: Vec<&str> = result.config.app.keys().map(String::as_str).collect();
        assert_eq!(names, ["backend", "frontend", "redis"]);

        let frontend = &result.config.app["frontend"];
        assert_eq!(frontend.replicas, Replicas::Fixed(3));
        assert_eq!(frontend.port, Some(9898));
        assert!(
            frontend
                .command
                .iter()
                .any(|arg| arg == "--backend-url=http://backend:9898/echo")
        );
        assert!(
            frontend
                .command
                .iter()
                .any(|arg| arg == "--cache-server=tcp://redis:6379")
        );
        assert_eq!(frontend.health.as_ref().unwrap().path, "/readyz");
        assert_eq!(
            frontend
                .ingress
                .as_ref()
                .map(|ingress| ingress.host.as_str()),
            Some("podinfo.localhost")
        );
        assert!(
            frontend
                .image
                .as_deref()
                .unwrap()
                .starts_with("ghcr.io/stefanprodan/podinfo@sha256:")
        );

        let backend = &result.config.app["backend"];
        assert_eq!(backend.port, Some(9898));
        assert_eq!(backend.health.as_ref().unwrap().path, "/readyz");

        // No command: the official image's entrypoint must run.
        let redis = &result.config.app["redis"];
        assert!(redis.command.is_empty());
        assert_eq!(redis.args[0], "redis-server");
        assert_eq!(redis.port, Some(6379));
    }
}
