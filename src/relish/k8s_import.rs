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
    AppSpec, AutoscaleSpec, DeploySpec, HealthSpec, IngressSpec, PlacementSpec,
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

    // Convert Deployments → Apps (with correlated Service, Ingress, HPA)
    for (name, deploy) in &deployments {
        let mut app = deployment_to_app(name, deploy, &mut report);

        // Correlate Service by name match
        if let Some(svc) = services.get(name) {
            apply_service(&mut app, svc);
        }

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
        if let Some(svc) = services.get(name) {
            apply_service(&mut app, svc);
        }
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
        if let Some(svc) = services.get(name) {
            apply_service(&mut app, svc);
        }
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

    // Report uncorrelated Ingresses/HPAs — their routing/autoscaling is lost.
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

    // K8s splits the entrypoint into `command` (argv prefix) and `args`;
    // Reliaburger has a single command vector — concatenate them.
    if let Some(c) = container {
        let mut command = c.command.clone().unwrap_or_default();
        command.extend(c.args.clone().unwrap_or_default());
        app.command = command;
    }

    app.port = container
        .and_then(|c| c.ports.as_ref())
        .and_then(|ports| ports.first())
        .map(|p| p.container_port as u16);

    // Health check from readinessProbe
    if let Some(probe) = container.and_then(|c| c.readiness_probe.as_ref()) {
        if let Some(http_get) = &probe.http_get {
            app.health = Some(HealthSpec {
                path: http_get.path.clone().unwrap_or_else(|| "/".to_string()),
                port: None,
                protocol: Default::default(),
                interval: probe.period_seconds.map(|s| s as u64),
                timeout: probe.timeout_seconds.map(|s| s as u64),
                threshold_unhealthy: probe.failure_threshold.map(|t| t as u32),
                threshold_healthy: probe.success_threshold.map(|t| t as u32),
                initial_delay: probe.initial_delay_seconds.map(|s| s as u64),
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

    // Init containers
    if let Some(inits) = pod_spec.and_then(|ps| ps.init_containers.as_ref()) {
        for ic in inits {
            app.init.push(crate::config::app::InitContainerSpec {
                image: ic.image.clone(),
                command: ic.command.clone().unwrap_or_default(),
            });
        }
    }

    app
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
        namespace: None,
    }
}

// ---------------------------------------------------------------------------
// Correlation helpers
// ---------------------------------------------------------------------------

fn apply_service(app: &mut AppSpec, svc: &Service) {
    // If the app doesn't have a port, try to get it from the service
    if app.port.is_none() {
        if let Some(spec) = &svc.spec {
            if let Some(ports) = &spec.ports {
                if let Some(p) = ports.first() {
                    if let Some(target) = p.target_port.as_ref() {
                        match target {
                            k8s_openapi::apimachinery::pkg::util::intstr::IntOrString::Int(i) => {
                                app.port = Some(*i as u16);
                            }
                            _ => {}
                        }
                    }
                }
            }
        }
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

    // command + args concatenate, same as the app path; K8s splits them.
    let command = container.and_then(|c| {
        let mut cmd = c.command.clone().unwrap_or_default();
        cmd.extend(c.args.clone().unwrap_or_default());
        if cmd.is_empty() { None } else { Some(cmd) }
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

        // command + args concatenated into the single command vector
        assert_eq!(app.command, vec!["python", "-m", "worker.main"]);

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
        assert_eq!(agent.command, vec!["./agent", "--verbose"]);
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
}
