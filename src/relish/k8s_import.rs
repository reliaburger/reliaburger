//! Kubernetes YAML → Reliaburger TOML importer.
//!
//! Parses Kubernetes manifests (multi-document YAML), correlates
//! related resources (Deployment+Service+Ingress → single App),
//! and produces a Reliaburger `Config` with a migration report.

use std::collections::{BTreeMap, BTreeSet};
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

/// A Kubernetes object's identity. Names are unique per namespace, not per
/// cluster, so every collected resource is keyed by both; an unset namespace
/// means `default`, as it does to the API server.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct ResourceKey {
    namespace: String,
    name: String,
}

impl ResourceKey {
    fn new(namespace: Option<&str>, name: &str) -> Self {
        Self {
            namespace: namespace.unwrap_or("default").to_string(),
            name: name.to_string(),
        }
    }

    /// The report label: `Kind/name` in `default`, `Kind/namespace/name`
    /// elsewhere, so two same-named resources never share a report line.
    fn label(&self, kind: &str) -> String {
        if self.namespace == "default" {
            format!("{kind}/{}", self.name)
        } else {
            format!("{kind}/{}/{}", self.namespace, self.name)
        }
    }
}

#[derive(Debug)]
#[allow(clippy::large_enum_variant)]
enum K8sResource {
    Deployment(ResourceKey, Deployment),
    DaemonSet(ResourceKey, DaemonSet),
    StatefulSet(ResourceKey, StatefulSet),
    Service(ResourceKey, Service),
    Ingress(ResourceKey, Ingress),
    Hpa(ResourceKey, HorizontalPodAutoscaler),
    ConfigMap(ResourceKey, ConfigMap),
    Secret(ResourceKey, Secret),
    Job(ResourceKey, Job),
    CronJob(ResourceKey, CronJob),
    /// Namespaces are cluster-scoped, so only the name counts.
    Namespace(String, Namespace),
    Unknown(String, ResourceKey), // (kind, key)
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
/// Uses `yaml_serde::Deserializer` to split documents (M28) rather than a naive
/// `split("---")`: the latter shredded any document with an embedded `---` (a
/// PEM block or a `|`-scalar), and skipping documents that *start with* `#`
/// dropped every Helm-rendered manifest, since Helm prefixes each with a
/// `# Source: ...` comment — so `helm template ... | relish import` imported
/// nothing and exited 0. The deserializer respects real document boundaries and
/// leading comments.
fn parse_multi_document_yaml(yaml: &str) -> Result<Vec<K8sResource>, RelishError> {
    use serde::Deserialize as _;

    fn typed<T: serde::de::DeserializeOwned>(value: yaml_serde::Value) -> Result<T, RelishError> {
        yaml_serde::from_value(value).map_err(|e| RelishError::FormatFailed(e.to_string()))
    }

    let mut resources = Vec::new();

    for document in yaml_serde::Deserializer::from_str(yaml) {
        let value = yaml_serde::Value::deserialize(document)
            .map_err(|e| RelishError::FormatFailed(format!("YAML parse error: {e}")))?;

        // A comment-only or empty document deserialises to null — skip it.
        if value.is_null() {
            continue;
        }

        let kind = value["kind"].as_str().unwrap_or("").to_string();
        let name = value["metadata"]["name"].as_str().unwrap_or("unknown");
        let key = ResourceKey::new(value["metadata"]["namespace"].as_str(), name);

        let resource = match kind.as_str() {
            "Deployment" => K8sResource::Deployment(key, typed(value)?),
            "DaemonSet" => K8sResource::DaemonSet(key, typed(value)?),
            "StatefulSet" => K8sResource::StatefulSet(key, typed(value)?),
            "Service" => K8sResource::Service(key, typed(value)?),
            "Ingress" => K8sResource::Ingress(key, typed(value)?),
            "HorizontalPodAutoscaler" => K8sResource::Hpa(key, typed(value)?),
            "ConfigMap" => K8sResource::ConfigMap(key, typed(value)?),
            "Secret" => K8sResource::Secret(key, typed(value)?),
            "Job" => K8sResource::Job(key, typed(value)?),
            "CronJob" => K8sResource::CronJob(key, typed(value)?),
            "Namespace" => K8sResource::Namespace(name.to_string(), typed(value)?),
            _ => K8sResource::Unknown(kind, key),
        };

        resources.push(resource);
    }

    Ok(resources)
}

// ---------------------------------------------------------------------------
// Resource correlation and conversion
// ---------------------------------------------------------------------------

/// The ConfigMaps in the input, and which of them a workload's `envFrom`
/// consumed (those aren't reported as unreferenced).
#[derive(Default)]
struct ConfigMaps {
    all: BTreeMap<ResourceKey, ConfigMap>,
    used: BTreeSet<ResourceKey>,
}

/// One Deployment, DaemonSet or StatefulSet on its way to an `[app.*]`.
struct Workload<'a> {
    kind: &'static str,
    key: ResourceKey,
    /// The pod template's labels, which Service selectors match.
    labels: BTreeMap<String, String>,
    pod_spec: Option<&'a k8s_openapi::api::core::v1::PodSpec>,
    app: AppSpec,
    /// The Service the app took its port from, if any.
    service: Option<ResourceKey>,
}

impl Workload<'_> {
    fn label(&self) -> String {
        self.key.label(self.kind)
    }
}

/// The pod template labels of a workload, empty when it sets none.
fn template_labels(
    metadata: Option<&k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta>,
) -> BTreeMap<String, String> {
    metadata
        .and_then(|meta| meta.labels.clone())
        .unwrap_or_default()
}

/// Correlate K8s resources and convert to a Reliaburger Config.
fn correlate_and_convert(resources: Vec<K8sResource>) -> (Config, MigrationReport) {
    let mut config = Config::default();
    let mut report = MigrationReport::default();

    // Separate by type for correlation
    let mut deployments: Vec<(ResourceKey, Deployment)> = Vec::new();
    let mut daemonsets: Vec<(ResourceKey, DaemonSet)> = Vec::new();
    let mut statefulsets: Vec<(ResourceKey, StatefulSet)> = Vec::new();
    let mut services: BTreeMap<ResourceKey, Service> = BTreeMap::new();
    let mut ingresses: BTreeMap<ResourceKey, Ingress> = BTreeMap::new();
    let mut hpas: BTreeMap<ResourceKey, HorizontalPodAutoscaler> = BTreeMap::new();
    let mut configmaps = ConfigMaps::default();
    let mut secrets: BTreeMap<ResourceKey, Secret> = BTreeMap::new();
    let mut jobs: Vec<(ResourceKey, Job)> = Vec::new();
    let mut cronjobs: Vec<(ResourceKey, CronJob)> = Vec::new();

    for resource in resources {
        match resource {
            K8sResource::Deployment(k, d) => deployments.push((k, d)),
            K8sResource::DaemonSet(k, d) => daemonsets.push((k, d)),
            K8sResource::StatefulSet(k, d) => statefulsets.push((k, d)),
            K8sResource::Service(k, s) => {
                services.insert(k, s);
            }
            K8sResource::Ingress(k, i) => {
                ingresses.insert(k, i);
            }
            K8sResource::Hpa(k, h) => {
                hpas.insert(k, h);
            }
            K8sResource::ConfigMap(k, c) => {
                configmaps.all.insert(k, c);
            }
            K8sResource::Secret(k, s) => {
                secrets.insert(k, s);
            }
            K8sResource::Job(k, j) => jobs.push((k, j)),
            K8sResource::CronJob(k, c) => cronjobs.push((k, c)),
            K8sResource::Namespace(n, _ns) => {
                config.namespace.insert(
                    n.clone(),
                    NamespaceSpec {
                        cpu: None,
                        memory: None,
                        gpu: None,
                        max_apps: None,
                        max_replicas: None,
                        secret_key: false,
                    },
                );
                report.converted.push(format!("Namespace/{n}"));
            }
            K8sResource::Unknown(kind, key) => {
                report
                    .dropped
                    .push(format!("{} — no Reliaburger equivalent", key.label(&kind)));
            }
        }
    }

    // Convert every workload's pod template first: Services, Ingresses and
    // HPAs then fold into the resulting apps.
    let mut workloads: Vec<Workload<'_>> = Vec::new();
    for (key, deploy) in &deployments {
        let spec = deploy.spec.as_ref();
        workloads.push(Workload {
            kind: "Deployment",
            app: deployment_to_app(
                &key.label("Deployment"),
                deploy,
                &mut configmaps,
                &mut report,
            ),
            labels: template_labels(spec.and_then(|s| s.template.metadata.as_ref())),
            pod_spec: spec.and_then(|s| s.template.spec.as_ref()),
            key: key.clone(),
            service: None,
        });
    }
    for (key, ds) in &daemonsets {
        let spec = ds.spec.as_ref();
        workloads.push(Workload {
            kind: "DaemonSet",
            app: daemonset_to_app(&key.label("DaemonSet"), ds, &mut configmaps, &mut report),
            labels: template_labels(spec.and_then(|s| s.template.metadata.as_ref())),
            pod_spec: spec.and_then(|s| s.template.spec.as_ref()),
            key: key.clone(),
            service: None,
        });
    }
    for (key, ss) in &statefulsets {
        let spec = ss.spec.as_ref();
        workloads.push(Workload {
            kind: "StatefulSet",
            app: statefulset_to_app(&key.label("StatefulSet"), ss, &mut configmaps, &mut report),
            labels: template_labels(spec.and_then(|s| s.template.metadata.as_ref())),
            pod_spec: spec.and_then(|s| s.template.spec.as_ref()),
            key: key.clone(),
            service: None,
        });
    }

    // Services attach by label selector (D33), within their namespace.
    let service_targets = correlate_services(&services, &mut workloads, &mut report);

    // Track which Ingresses/HPAs correlate to a workload, so the leftovers
    // can be reported instead of silently vanishing.
    let mut used_ingresses: BTreeSet<ResourceKey> = BTreeSet::new();
    let mut used_hpas: BTreeSet<ResourceKey> = BTreeSet::new();

    for (index, workload) in workloads.iter_mut().enumerate() {
        let label = workload.label();
        warn_unimported_ports(&label, workload.pod_spec, &mut workload.app, &mut report);

        // An Ingress follows its backend Service to the workloads that
        // Service selected.
        if let Some(ing_key) = find_ingress_for_workload(&ingresses, &service_targets, index) {
            if let Some(ing) = ingresses.get(&ing_key) {
                apply_ingress(
                    &mut workload.app,
                    &ing_key.label("Ingress"),
                    ing,
                    &mut report,
                );
                used_ingresses.insert(ing_key);
            }
        }

        // An HPA cannot target a DaemonSet: it has no scale subresource.
        if workload.kind != "DaemonSet" {
            if let Some(hpa_key) = find_hpa_for_workload(&hpas, &workload.key, workload.kind) {
                if let Some(hpa) = hpas.get(&hpa_key) {
                    apply_hpa(
                        &mut workload.app,
                        &hpa_key.label("HorizontalPodAutoscaler"),
                        hpa,
                        &mut report,
                    );
                    used_hpas.insert(hpa_key);
                }
            }
        }
    }

    for workload in workloads {
        let label = workload.label();
        let key = insert_without_overwrite(
            &mut config.app,
            "app",
            &mut report,
            &label,
            &workload.key,
            workload.app,
        );
        match workload.kind {
            "DaemonSet" => report
                .converted
                .push(format!("{label} → [app.{key}] (replicas = \"*\")")),
            "StatefulSet" => {
                report.converted.push(format!("{label} → [app.{key}]"));
                report.warnings.push(MigrationWarning {
                    resource: label,
                    message: "ordering guarantees and stable network IDs lost".to_string(),
                });
            }
            _ => report.converted.push(format!("{label} → [app.{key}]")),
        }
    }

    for (key, job) in &jobs {
        let label = key.label("Job");
        let job_spec = job_to_jobspec(&label, job, &mut configmaps, &mut report);
        let name =
            insert_without_overwrite(&mut config.job, "job", &mut report, &label, key, job_spec);
        report.converted.push(format!("{label} → [job.{name}]"));
    }

    for (key, cj) in &cronjobs {
        let label = key.label("CronJob");
        let job_spec = cronjob_to_jobspec(&label, cj, &mut configmaps, &mut report);
        let name =
            insert_without_overwrite(&mut config.job, "job", &mut report, &label, key, job_spec);
        report.converted.push(format!("{label} → [job.{name}]"));
    }

    // Report uncorrelated Ingresses and HPAs: their routing or autoscaling
    // is lost. (Services report themselves in `correlate_services`.)
    for key in ingresses.keys() {
        if !used_ingresses.contains(key) {
            report.warnings.push(MigrationWarning {
                resource: key.label("Ingress"),
                message: "no imported workload is selected by its backend Service; routing dropped"
                    .to_string(),
            });
        }
    }
    for key in hpas.keys() {
        if !used_hpas.contains(key) {
            report.warnings.push(MigrationWarning {
                resource: key.label("HorizontalPodAutoscaler"),
                message: "no imported workload matches its scaleTargetRef; autoscaling dropped"
                    .to_string(),
            });
        }
    }

    for key in configmaps.all.keys() {
        if !configmaps.used.contains(key) {
            report.warnings.push(MigrationWarning {
                resource: key.label("ConfigMap"),
                message: "not referenced by any workload's envFrom; import manually if needed"
                    .to_string(),
            });
        }
    }

    for key in secrets.keys() {
        report.warnings.push(MigrationWarning {
            resource: key.label("Secret"),
            message: "re-encrypt values with `relish secret encrypt`".to_string(),
        });
    }

    (config, report)
}

/// Fold each Service into the workloads it selects, and report every
/// Service that can't be represented exactly.
///
/// Kubernetes routes a Service to every pod whose labels include all of the
/// Service's `spec.selector`, in the Service's namespace; names play no part.
/// The importer does the same against each workload's pod template labels.
/// A Service with no selector routes to nothing in Kubernetes (its endpoints
/// are managed by hand), so for that one alone the importer falls back to the
/// workload of the same name, and says so.
///
/// Returns the workloads each Service ended up attached to, which Ingress
/// correlation follows.
fn correlate_services(
    services: &BTreeMap<ResourceKey, Service>,
    workloads: &mut [Workload<'_>],
    report: &mut MigrationReport,
) -> BTreeMap<ResourceKey, Vec<usize>> {
    let mut targets = BTreeMap::new();
    for (svc_key, svc) in services {
        let resource = svc_key.label("Service");
        let selector = svc
            .spec
            .as_ref()
            .and_then(|spec| spec.selector.as_ref())
            .filter(|selector| !selector.is_empty());

        let matched: Vec<usize> = workloads
            .iter()
            .enumerate()
            .filter(|(_, w)| w.key.namespace == svc_key.namespace)
            .filter(|(_, w)| match selector {
                Some(selector) => selector
                    .iter()
                    .all(|(label, value)| w.labels.get(label) == Some(value)),
                None => w.key.name == svc_key.name,
            })
            .map(|(index, _)| index)
            .collect();

        match (selector, matched.len()) {
            (Some(_), 0) => report.warnings.push(MigrationWarning {
                resource: resource.clone(),
                message: "its selector selects no imported workload, so nothing answers to it; \
                          an app is reachable only by its own name"
                    .to_string(),
            }),
            (None, 0) => report.warnings.push(MigrationWarning {
                resource: resource.clone(),
                message: "has no selector and no imported workload has its name, so nothing \
                          answers to it; an app is reachable only by its own name"
                    .to_string(),
            }),
            (None, _) => report.warnings.push(MigrationWarning {
                resource: resource.clone(),
                message: format!(
                    "has no selector, so it routes to nothing in Kubernetes; matched to {} by \
                     name instead",
                    workloads[matched[0]].label()
                ),
            }),
            (Some(_), 1) => {}
            (Some(_), count) => report.warnings.push(MigrationWarning {
                resource: resource.clone(),
                message: format!(
                    "selects {count} workloads ({}); one Service can't front several apps, so \
                     each takes its port and clients must call each app by its own name",
                    matched
                        .iter()
                        .map(|&index| workloads[index].label())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            }),
        }

        for &index in &matched {
            let workload = &mut workloads[index];
            if let Some(earlier) = &workload.service {
                report.warnings.push(MigrationWarning {
                    resource: resource.clone(),
                    message: format!(
                        "{} already takes its port from {}; this Service's ports are not imported",
                        workload.label(),
                        earlier.label("Service")
                    ),
                });
                continue;
            }
            let port = apply_service(
                &mut workload.app,
                &workload.key.name,
                &resource,
                svc,
                workload.pod_spec,
                report,
            );
            // The Service's own name isn't kept: an app answers only to its
            // name, so clients that dialled the Service have to change.
            if matched.len() == 1 && svc_key.name != workload.key.name {
                report.warnings.push(MigrationWarning {
                    resource: resource.clone(),
                    message: format!(
                        "attached to {} by its selector; the name {} is not kept, so clients \
                         must connect to {}:{}",
                        workload.label(),
                        svc_key.name,
                        workload.key.name,
                        port.map_or_else(|| "its port".to_string(), |p| p.to_string())
                    ),
                });
            }
            workload.service = Some(svc_key.clone());
        }
        targets.insert(svc_key.clone(), matched);
    }
    targets
}

/// Find the HPA whose `scaleTargetRef` names this workload, in its namespace.
///
/// Matching by the HPA's own metadata name (the old behaviour) never
/// correlated a conventionally-named `api-hpa` with its `api` Deployment, so
/// its autoscaling was silently lost. An absent/empty `kind` matches any
/// workload — real manifests omit it and serde defaults it to `""`.
fn find_hpa_for_workload(
    hpas: &BTreeMap<ResourceKey, HorizontalPodAutoscaler>,
    workload: &ResourceKey,
    workload_kind: &str,
) -> Option<ResourceKey> {
    hpas.iter()
        .filter(|(key, _)| key.namespace == workload.namespace)
        .find(|(_, hpa)| {
            hpa.spec.as_ref().is_some_and(|spec| {
                let target = &spec.scale_target_ref;
                target.name == workload.name
                    && (target.kind.is_empty() || target.kind == workload_kind)
            })
        })
        .map(|(key, _)| key.clone())
}

/// Insert a converted app or job under its own name, or under
/// `{namespace}-{name}` when an earlier resource already took the name.
/// Kubernetes scopes names per namespace; a flat TOML table does not, so a
/// plain `insert` would silently overwrite the earlier one.
fn insert_without_overwrite<T>(
    table: &mut BTreeMap<String, T>,
    table_name: &str,
    report: &mut MigrationReport,
    label: &str,
    key: &ResourceKey,
    value: T,
) -> String {
    let mut name = key.name.clone();
    if table.contains_key(&name) {
        name = format!("{}-{}", key.namespace, key.name);
        let mut suffix = 2;
        while table.contains_key(&name) {
            name = format!("{}-{}-{suffix}", key.namespace, key.name);
            suffix += 1;
        }
        report.warnings.push(MigrationWarning {
            resource: label.to_string(),
            message: format!(
                "name {:?} is already taken by an earlier imported resource; imported as \
                 [{table_name}.{name}]",
                key.name
            ),
        });
    }
    table.insert(name.clone(), value);
    name
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
    configmaps: &mut ConfigMaps,
    report: &mut MigrationReport,
) -> AppSpec {
    let container = pod_spec.and_then(|ps| ps.containers.first());

    let mut app = empty_app_spec();
    app.image = container.and_then(|c| c.image.clone());
    app.namespace = metadata_namespace.cloned();

    warn_unimported_pod_fields(resource, pod_spec, report);
    if container.and_then(|c| c.liveness_probe.as_ref()).is_some() {
        report.warnings.push(MigrationWarning {
            resource: resource.to_string(),
            message: "livenessProbe is not imported (only readinessProbe maps to a health check)"
                .to_string(),
        });
    }
    app.env = import_env(resource, metadata_namespace, container, configmaps, report);
    import_pod_template(resource, pod_spec, container, &mut app, report);
    app
}

/// Report what a pod spec carries that neither an app nor a job can, so a
/// silent drop becomes visible in the migration report rather than a
/// surprise in production (M28).
fn warn_unimported_pod_fields(
    resource: &str,
    pod_spec: Option<&k8s_openapi::api::core::v1::PodSpec>,
    report: &mut MigrationReport,
) {
    let mut warn = |message: String| {
        report.warnings.push(MigrationWarning {
            resource: resource.to_string(),
            message,
        })
    };
    let Some(ps) = pod_spec else { return };
    if ps.tolerations.as_ref().is_some_and(|t| !t.is_empty()) {
        warn(
            "tolerations are not imported; Reliaburger has no taints, so any node may run it"
                .to_string(),
        );
    }
    if ps.affinity.is_some() {
        warn("affinity is not imported; use placement with node labels instead".to_string());
    }
    if ps
        .topology_spread_constraints
        .as_ref()
        .is_some_and(|t| !t.is_empty())
    {
        warn("topologySpreadConstraints are not imported".to_string());
    }
    if ps.containers.len() > 1 {
        warn(format!(
            "only the first of {} containers is imported; {} sidecar(s) dropped",
            ps.containers.len(),
            ps.containers.len() - 1
        ));
    }
    if ps.volumes.as_ref().is_some_and(|v| !v.is_empty()) {
        warn("pod volumes are not imported; declare Reliaburger volumes manually".to_string());
    }
    // A hostPath volume is the one kind a hand-written replacement can't
    // just declare: a `source` needs the host-exec permission and an
    // allowlist entry on every node that runs it.
    for volume in ps.volumes.iter().flatten() {
        if let Some(host_path) = &volume.host_path {
            warn(format!(
                "hostPath volume {:?} ({}) is not imported; a hand-written `source` \
                 needs the host-exec permission and a [storage] allowed_host_paths \
                 entry in node.toml on every node that runs it",
                volume.name, host_path.path
            ));
        }
    }
}

/// A container's environment: `envFrom` sources first, in order, then its
/// explicit `env`, which wins on a clash, as in Kubernetes.
///
/// A ConfigMap in the input, in the same namespace, is flattened into plain
/// values. That's an approximation (a later change to the ConfigMap no
/// longer reaches the workload), so it's reported. A Secret, a ConfigMap
/// that isn't in the input, and any `valueFrom` reference have no automatic
/// mapping and are reported by name.
fn import_env(
    resource: &str,
    metadata_namespace: Option<&String>,
    container: Option<&k8s_openapi::api::core::v1::Container>,
    configmaps: &mut ConfigMaps,
    report: &mut MigrationReport,
) -> BTreeMap<String, EnvValue> {
    let mut env = BTreeMap::new();
    let Some(container) = container else {
        return env;
    };
    let mut warn = |message: String| {
        report.warnings.push(MigrationWarning {
            resource: resource.to_string(),
            message,
        })
    };
    for source in container.env_from.iter().flatten() {
        let prefix = source.prefix.as_deref().unwrap_or_default();
        if let Some(secret) = &source.secret_ref {
            warn(format!(
                "envFrom Secret/{} is not imported; encrypt each value with `relish secret \
                 encrypt` and add it to env",
                secret.name.as_deref().unwrap_or_default()
            ));
        }
        let Some(reference) = &source.config_map_ref else {
            continue;
        };
        let name = reference.name.as_deref().unwrap_or_default();
        let key = ResourceKey::new(metadata_namespace.map(String::as_str), name);
        let Some(configmap) = configmaps.all.get(&key) else {
            warn(format!(
                "envFrom ConfigMap/{name} is not in the input; its keys are not imported"
            ));
            continue;
        };
        let data = configmap.data.clone().unwrap_or_default();
        warn(format!(
            "envFrom ConfigMap/{name} flattened into {} env value(s); later changes to the \
             ConfigMap won't reach it",
            data.len()
        ));
        if configmap
            .binary_data
            .as_ref()
            .is_some_and(|b| !b.is_empty())
        {
            warn(format!(
                "envFrom ConfigMap/{name} binaryData is not imported"
            ));
        }
        for (name, value) in data {
            env.insert(format!("{prefix}{name}"), EnvValue::Plain(value));
        }
        configmaps.used.insert(key);
    }
    for env_var in container.env.iter().flatten() {
        if let Some(value) = &env_var.value {
            env.insert(env_var.name.clone(), EnvValue::Plain(value.clone()));
        } else if env_var.value_from.is_some() {
            warn(format!(
                "env {} uses valueFrom (secret/configmap/field ref) — set the value manually \
                 or use `relish secret encrypt`",
                env_var.name
            ));
        }
    }
    env
}

/// The parts of a pod template only an app carries: argv, working
/// directory and user, port, health check, resources, placement and init
/// containers.
fn import_pod_template(
    resource: &str,
    pod_spec: Option<&k8s_openapi::api::core::v1::PodSpec>,
    container: Option<&k8s_openapi::api::core::v1::Container>,
    app: &mut AppSpec,
    report: &mut MigrationReport,
) {
    // Same split as Kubernetes: `command` replaces the image's Entrypoint,
    // `args` replaces its Cmd. The runtime applies the rules (Z1.1).
    if let Some(c) = container {
        app.command = c.command.clone().unwrap_or_default();
        app.args = c.args.clone().unwrap_or_default();
        app.working_dir = c.working_dir.clone().map(PathBuf::from);
    }
    import_security_context(resource, pod_spec, container, app, report);

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
    if let Some(fs_group) = pod.and_then(|sc| sc.fs_group) {
        report.warnings.push(MigrationWarning {
            resource: resource.to_string(),
            message: format!(
                "securityContext.fsGroup {fs_group} dropped: managed volumes are handed to \
                 the container's user on first mount, and host path volumes keep their \
                 host ownership"
            ),
        });
    }
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

fn deployment_to_app(
    resource: &str,
    deploy: &Deployment,
    configmaps: &mut ConfigMaps,
    report: &mut MigrationReport,
) -> AppSpec {
    let spec = deploy.spec.as_ref();
    let pod_spec = spec.and_then(|s| s.template.spec.as_ref());
    let mut app = pod_spec_to_app(
        resource,
        deploy.metadata.namespace.as_ref(),
        pod_spec,
        configmaps,
        report,
    );
    import_scrape_annotations(
        resource,
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
                strategy: Some(crate::meat::deploy_types::DeployStrategy::Rolling),
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

fn daemonset_to_app(
    resource: &str,
    ds: &DaemonSet,
    configmaps: &mut ConfigMaps,
    report: &mut MigrationReport,
) -> AppSpec {
    let pod_spec = ds.spec.as_ref().and_then(|s| s.template.spec.as_ref());
    let mut app = pod_spec_to_app(
        resource,
        ds.metadata.namespace.as_ref(),
        pod_spec,
        configmaps,
        report,
    );
    import_scrape_annotations(
        resource,
        ds.spec.as_ref().and_then(|s| s.template.metadata.as_ref()),
        &mut app,
        report,
    );
    app.replicas = Replicas::DaemonSet;
    app
}

fn statefulset_to_app(
    resource: &str,
    ss: &StatefulSet,
    configmaps: &mut ConfigMaps,
    report: &mut MigrationReport,
) -> AppSpec {
    let spec = ss.spec.as_ref();
    let pod_spec = spec.and_then(|s| s.template.spec.as_ref());
    let mut app = pod_spec_to_app(
        resource,
        ss.metadata.namespace.as_ref(),
        pod_spec,
        configmaps,
        report,
    );
    import_scrape_annotations(
        resource,
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
/// 500m, `"500m"` is 500 millicores. Reliaburger's own CPU parser now
/// follows the same convention, but this one stays separate because K8s
/// quantities allow forms (scientific notation, sub-millicore values that
/// round) that an app TOML deliberately rejects. The result goes straight
/// into a `ResourceRange` in millicores, so nothing converts it twice.
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
        ordinals: None,
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
    app_name: &str,
    resource: &str,
    svc: &Service,
    pod_spec: Option<&k8s_openapi::api::core::v1::PodSpec>,
    report: &mut MigrationReport,
) -> Option<u16> {
    let resource = resource.to_string();
    let service_ports = svc
        .spec
        .as_ref()
        .and_then(|spec| spec.ports.as_deref())
        .unwrap_or_default();
    let Some(first) = service_ports.first() else {
        return app.port;
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
        return app.port;
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
                 mapping, so clients must connect to {app_name}:{target} (ingress is \
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
    app.port
}

/// Find the first Ingress with a backend Service that selected this
/// workload (`service_targets` maps each Service to the workloads it
/// attached to). An Ingress routes to a Service, never straight to a
/// workload, and only within its own namespace.
fn find_ingress_for_workload(
    ingresses: &BTreeMap<ResourceKey, Ingress>,
    service_targets: &BTreeMap<ResourceKey, Vec<usize>>,
    workload: usize,
) -> Option<ResourceKey> {
    ingresses
        .iter()
        .find(|(ing_key, ing)| {
            ingress_backend_services(ing).any(|service| {
                let key = ResourceKey {
                    namespace: ing_key.namespace.clone(),
                    name: service.to_string(),
                };
                service_targets
                    .get(&key)
                    .is_some_and(|targets| targets.contains(&workload))
            })
        })
        .map(|(key, _)| key.clone())
}

/// The Service names every path of an Ingress routes to.
fn ingress_backend_services(ing: &Ingress) -> impl Iterator<Item = &str> {
    ing.spec
        .iter()
        .flat_map(|spec| spec.rules.iter().flatten())
        .flat_map(|rule| rule.http.iter())
        .flat_map(|http| http.paths.iter())
        .filter_map(|path| path.backend.service.as_ref())
        .map(|backend| backend.name.as_str())
}

fn apply_ingress(app: &mut AppSpec, resource: &str, ing: &Ingress, report: &mut MigrationReport) {
    if let Some(spec) = &ing.spec {
        // Reliaburger's IngressSpec is one host + one path prefix per app, so
        // everything beyond the first rule's first path is unrepresentable —
        // which must be *said*, not silently kept-first (M28 class).
        if let Some(rules) = &spec.rules {
            if rules.len() > 1 {
                report.warnings.push(MigrationWarning {
                    resource: resource.to_string(),
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
                            resource: resource.to_string(),
                            message: format!(
                                "only the first of {} paths is imported; Reliaburger ingress is one path prefix per app",
                                http.paths.len()
                            ),
                        });
                    }
                    if let Some(path_type) = http.paths.first().map(|p| p.path_type.as_str()) {
                        if path_type != "Prefix" && !path_type.is_empty() {
                            report.warnings.push(MigrationWarning {
                                resource: resource.to_string(),
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
                resource: resource.to_string(),
                message: "defaultBackend is not imported".to_string(),
            });
        }
        if let Some(class) = spec.ingress_class_name.as_deref() {
            report.warnings.push(MigrationWarning {
                resource: resource.to_string(),
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
                        resource: resource.to_string(),
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

fn apply_hpa(
    app: &mut AppSpec,
    resource: &str,
    hpa: &HorizontalPodAutoscaler,
    report: &mut MigrationReport,
) {
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

        let autoscale = AutoscaleSpec {
            metric,
            target,
            min,
            max,
            evaluation_window: None,
            cooldown: None,
            scale_down_threshold: None,
        };
        // Reliaburger only scales on cpu/memory utilisation of a request. Say
        // so now rather than let the imported config fail on apply.
        if let Err(e) =
            crate::meat::autoscaler::AutoscaleConfig::from_spec(&autoscale, app.cpu, app.memory)
        {
            report.warnings.push(MigrationWarning {
                resource: resource.to_string(),
                message: format!(
                    "imported [autoscale] will not validate: {e}; fix it before applying"
                ),
            });
        }
        app.autoscale = Some(autoscale);
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
    configmaps: &mut ConfigMaps,
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

    let env = import_env(resource, metadata_namespace, container, configmaps, report);
    warn_unimported_pod_fields(resource, pod_spec, report);
    warn_unimported_job_pod_fields(resource, pod_spec, container, report);

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
        pids: None,
        runtime: Default::default(),
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
        firewall: None,
        egress: None,
        max_attempts: None,
        task_timeout_secs: None,
        overlap: None,
        replay_unknown: false,
    }
}

/// Report what a job's pod template carries that a `[job.*]` can't: a job
/// runs one command in its image, as the image's user, from the image's
/// working directory, anywhere in the cluster.
fn warn_unimported_job_pod_fields(
    resource: &str,
    pod_spec: Option<&k8s_openapi::api::core::v1::PodSpec>,
    container: Option<&k8s_openapi::api::core::v1::Container>,
    report: &mut MigrationReport,
) {
    let mut warn = |message: String| {
        report.warnings.push(MigrationWarning {
            resource: resource.to_string(),
            message,
        })
    };
    if pod_spec
        .and_then(|ps| ps.node_selector.as_ref())
        .is_some_and(|selector| !selector.is_empty())
    {
        warn("nodeSelector is not imported; a job has no placement".to_string());
    }
    if pod_spec
        .and_then(|ps| ps.init_containers.as_ref())
        .is_some_and(|inits| !inits.is_empty())
    {
        warn("initContainers are not imported; a job runs one container".to_string());
    }
    let Some(container) = container else { return };
    if let Some(dir) = &container.working_dir {
        warn(format!(
            "workingDir {dir} is not imported; a job runs in its image's working directory"
        ));
    }
    let pod_security = pod_spec.and_then(|ps| ps.security_context.as_ref());
    let own_security = container.security_context.as_ref();
    let user = own_security
        .and_then(|sc| sc.run_as_user)
        .or_else(|| pod_security.and_then(|sc| sc.run_as_user));
    let group = own_security
        .and_then(|sc| sc.run_as_group)
        .or_else(|| pod_security.and_then(|sc| sc.run_as_group));
    for (field, value) in [("runAsUser", user), ("runAsGroup", group)] {
        if let Some(value) = value {
            warn(format!(
                "securityContext.{field} {value} is not imported; a job runs as its image's user"
            ));
        }
    }
}

/// Map a Kubernetes Job spec's retry and deadline policy onto the job's
/// `max_attempts` and `task_timeout_secs`, and report every field that
/// changes how often or how many times the job runs.
fn import_job_policy(
    resource: &str,
    spec: Option<&k8s_openapi::api::batch::v1::JobSpec>,
    job_spec: &mut JobSpec,
    report: &mut MigrationReport,
) {
    let Some(spec) = spec else { return };
    let mut warn = |message: String| {
        report.warnings.push(MigrationWarning {
            resource: resource.to_string(),
            message,
        })
    };

    // backoffLimit counts retries after the first attempt; max_attempts
    // counts every attempt, and allows at most 10.
    if let Some(limit) = spec.backoff_limit.filter(|limit| *limit >= 0) {
        let attempts = i64::from(limit) + 1;
        let max = i64::from(crate::meat::task_array::MAX_ATTEMPTS);
        if attempts > max {
            warn(format!(
                "backoffLimit {limit} allows {attempts} attempts; a job allows at most {max}, \
                 so max_attempts = {max}"
            ));
        }
        job_spec.max_attempts = u8::try_from(attempts.min(max)).ok();
    }

    // The pod's deadline is per attempt (each retry is a new pod), which is
    // exactly task_timeout_secs. The Job's own deadline spans every attempt;
    // the nearest equivalent is the per-attempt one, so say it changed.
    let pod_deadline = spec
        .template
        .spec
        .as_ref()
        .and_then(|ps| ps.active_deadline_seconds);
    if let Some(seconds) = pod_deadline {
        job_spec.task_timeout_secs = u32::try_from(seconds).ok();
    }
    if let Some(seconds) = spec.active_deadline_seconds {
        if pod_deadline.is_some() {
            warn(format!(
                "activeDeadlineSeconds {seconds} on the Job is not imported; the pod's \
                 per-attempt deadline is kept as task_timeout_secs"
            ));
        } else {
            job_spec.task_timeout_secs = u32::try_from(seconds).ok();
            warn(format!(
                "activeDeadlineSeconds {seconds} bounds the whole Job; imported as \
                 task_timeout_secs, which applies per attempt"
            ));
        }
    }

    if let Some(completions) = spec.completions.filter(|c| *c != 1) {
        warn(format!(
            "completions {completions} is not imported; the job runs once per trigger"
        ));
    }
    if let Some(parallelism) = spec.parallelism.filter(|p| *p != 1) {
        warn(format!(
            "parallelism {parallelism} is not imported; one attempt runs at a time"
        ));
    }
    if let Some(mode) = spec
        .completion_mode
        .as_deref()
        .filter(|mode| *mode != "NonIndexed")
    {
        warn(format!("completionMode {mode} is not imported"));
    }
    if spec.pod_failure_policy.is_some() {
        warn("podFailurePolicy is not imported; every failure is retried".to_string());
    }
}

fn job_to_jobspec(
    resource: &str,
    job: &Job,
    configmaps: &mut ConfigMaps,
    report: &mut MigrationReport,
) -> JobSpec {
    let spec = job.spec.as_ref();
    let pod_spec = spec.and_then(|s| s.template.spec.as_ref());
    let mut job_spec = pod_to_jobspec(
        resource,
        job.metadata.namespace.as_ref(),
        pod_spec,
        configmaps,
        report,
    );
    import_job_policy(resource, spec, &mut job_spec, report);
    job_spec
}

fn cronjob_to_jobspec(
    resource: &str,
    cj: &CronJob,
    configmaps: &mut ConfigMaps,
    report: &mut MigrationReport,
) -> JobSpec {
    let spec = cj.spec.as_ref();
    let job_template = spec.and_then(|s| s.job_template.spec.as_ref());
    let pod_spec = job_template.and_then(|jt| jt.template.spec.as_ref());

    let mut job_spec = pod_to_jobspec(
        resource,
        cj.metadata.namespace.as_ref(),
        pod_spec,
        configmaps,
        report,
    );
    import_job_policy(resource, job_template, &mut job_spec, report);
    job_spec.schedule = spec.map(|s| s.schedule.clone());

    // Scheduling behaviour Reliaburger doesn't model — say so.
    let mut warn = |message: String| {
        report.warnings.push(MigrationWarning {
            resource: resource.to_string(),
            message,
        })
    };
    if spec.and_then(|s| s.suspend) == Some(true) {
        warn("suspend = true is not imported; the job will run on its schedule".to_string());
    }
    if let Some(policy) = spec.and_then(|s| s.concurrency_policy.as_ref()) {
        if policy != "Allow" {
            warn(format!("concurrencyPolicy {policy} is not imported"));
        }
    }
    if let Some(zone) = spec.and_then(|s| s.time_zone.as_ref()) {
        warn(format!(
            "timeZone {zone} is not imported; a job's schedule runs in UTC"
        ));
    }
    if let Some(seconds) = spec.and_then(|s| s.starting_deadline_seconds) {
        warn(format!(
            "startingDeadlineSeconds {seconds} is not imported; a missed firing is skipped"
        ));
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
        // CPU scaling without a CPU request measures against one core, so
        // the block validates and there's nothing to warn about.
        assert!(
            !result
                .report
                .warnings
                .iter()
                .any(|w| w.resource.starts_with("HorizontalPodAutoscaler/")),
            "{:?}",
            result.report.warnings
        );
        result.config.validate().unwrap();
    }

    #[test]
    fn import_memory_hpa_without_a_memory_request_warns() {
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
  name: api-hpa
spec:
  scaleTargetRef:
    name: api
  minReplicas: 2
  maxReplicas: 10
  metrics:
  - type: Resource
    resource:
      name: memory
      target:
        type: Utilization
        averageUtilization: 80
"#;
        let result = import_from_yaml(yaml).unwrap();
        assert_eq!(
            result.config.app["api"].autoscale.as_ref().unwrap().metric,
            "memory"
        );
        assert!(
            result
                .report
                .warnings
                .iter()
                .any(|w| w.resource == "HorizontalPodAutoscaler/api-hpa"
                    && w.message.contains("needs a non-zero memory request")),
            "{:?}",
            result.report.warnings
        );
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
    /// Reliaburger's own parser, where a bare integer then meant millicores — a
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

    /// `relish import` writes the imported config out as TOML. The CPU
    /// value must survive that trip: bare numbers in TOML are cores, so
    /// writing 1000 millicores as `"1000"` would re-read as 1000 cores.
    #[test]
    fn imported_cpu_survives_the_toml_round_trip() {
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
            cpu: "0.5"
          limits:
            cpu: "2"
"#;
        let result = import_from_yaml(yaml).unwrap();
        let toml = toml::to_string_pretty(&result.config).unwrap();
        assert!(toml.contains(r#"cpu = "500m-2000m""#), "{toml}");
        let reparsed: crate::config::Config = toml::from_str(&toml).unwrap();
        let cpu = reparsed.app["web"].cpu.unwrap();
        assert_eq!((cpu.request, cpu.limit), (500, 2000));
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
    metadata:
      labels:
        app: web
    spec:
      containers:
      - name: web
        image: web:v1
---
apiVersion: v1
kind: Service
metadata:
  name: web
spec:
  selector:
    app: web
  ports:
  - port: 80
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
    fn host_path_volume_is_named_in_the_import_warning() {
        let yaml = r#"
apiVersion: apps/v1
kind: Deployment
metadata:
  name: agent
spec:
  template:
    spec:
      containers:
      - name: agent
        image: fluent-bit:3
      volumes:
      - name: varlog
        hostPath:
          path: /var/log
      - name: scratch
        emptyDir: {}
"#;
        let result = import_from_yaml(yaml).unwrap();
        assert!(result.config.app["agent"].volumes.is_empty());
        let warnings = warnings_of(&result);
        let host_path: Vec<_> = warnings.iter().filter(|w| w.contains("hostPath")).collect();
        assert_eq!(host_path.len(), 1, "{warnings:?}");
        assert!(
            host_path[0].starts_with("Deployment/agent")
                && host_path[0].contains("\"varlog\" (/var/log)")
                && host_path[0].contains("host-exec")
                && host_path[0].contains("allowed_host_paths"),
            "{warnings:?}"
        );
    }

    #[test]
    fn fs_group_is_dropped_with_a_warning() {
        let yaml = r#"
apiVersion: apps/v1
kind: Deployment
metadata:
  name: db
spec:
  template:
    spec:
      securityContext:
        runAsUser: 999
        fsGroup: 999
      containers:
      - name: db
        image: postgres:17
"#;
        let result = import_from_yaml(yaml).unwrap();
        assert_eq!(result.config.app["db"].run_as_user, Some(999));
        let warnings = warnings_of(&result);
        assert!(
            warnings
                .iter()
                .any(|w| w.starts_with("Deployment/db") && w.contains("fsGroup 999 dropped")),
            "{warnings:?}"
        );
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
    metadata:
      labels:
        app: backend
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
  selector:
    app: backend
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

    /// Z1.5, Z6.7: the tutorial's demo manifest imports cleanly into the
    /// four apps it describes, and the result is a valid config.
    #[test]
    fn the_podinfo_demo_manifest_imports_into_four_apps() {
        let yaml = include_str!("../../examples/kubernetes/podinfo.yaml");
        let result = import_from_yaml(yaml).unwrap();
        result.config.validate().unwrap();
        assert!(result.report.dropped.is_empty(), "{}", result.report);
        let names: Vec<&str> = result.config.app.keys().map(String::as_str).collect();
        assert_eq!(names, ["backend", "frontend", "loadgen", "redis"]);

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

        // The load generator calls the frontend by service name, forever,
        // and serves nothing itself.
        let loadgen = &result.config.app["loadgen"];
        assert_eq!(loadgen.replicas, Replicas::Fixed(1));
        assert_eq!(loadgen.port, None);
        assert!(loadgen.ingress.is_none());
        assert_eq!(loadgen.command[..2], ["/bin/sh", "-c"]);
        assert!(loadgen.command[2].contains("http://frontend:9898/cache/loadgen"));
        assert!(
            loadgen
                .image
                .as_deref()
                .unwrap()
                .starts_with("public.ecr.aws/docker/library/busybox@sha256:")
        );
    }

    // -----------------------------------------------------------------------
    // #703: keying by (namespace, name), selector correlation, envFrom, Job
    // fields. Every finding there was a silent loss, so each test checks the
    // report as well as the config.
    // -----------------------------------------------------------------------

    fn report_lines(report: &MigrationReport) -> Vec<String> {
        report
            .warnings
            .iter()
            .map(|w| format!("{}: {}", w.resource, w.message))
            .collect()
    }

    #[test]
    fn same_named_jobs_in_two_namespaces_both_survive() {
        let yaml = r#"
apiVersion: batch/v1
kind: Job
metadata:
  name: migrate
  namespace: a
spec:
  template:
    spec:
      containers:
      - name: migrate
        image: migrate:a
---
apiVersion: batch/v1
kind: CronJob
metadata:
  name: migrate
  namespace: b
spec:
  schedule: "0 * * * *"
  jobTemplate:
    spec:
      template:
        spec:
          containers:
          - name: migrate
            image: migrate:b
"#;
        let result = import_from_yaml(yaml).unwrap();
        assert_eq!(result.config.job.len(), 2, "{:?}", result.config.job.keys());
        assert_eq!(
            result.config.job["migrate"].image.as_deref(),
            Some("migrate:a")
        );
        assert_eq!(
            result.config.job["b-migrate"].image.as_deref(),
            Some("migrate:b")
        );
        let warnings = report_lines(&result.report);
        assert!(
            warnings
                .iter()
                .any(|w| w.starts_with("CronJob/b/migrate") && w.contains("[job.b-migrate]")),
            "{warnings:?}"
        );
    }

    #[test]
    fn same_named_services_in_two_namespaces_attach_to_their_own_workload() {
        let yaml = r#"
apiVersion: apps/v1
kind: Deployment
metadata:
  name: api
  namespace: alpha
spec:
  template:
    metadata:
      labels:
        app: api
    spec:
      containers:
      - name: api
        image: api:alpha
        ports:
        - containerPort: 8080
---
apiVersion: apps/v1
kind: Deployment
metadata:
  name: api
  namespace: beta
spec:
  template:
    metadata:
      labels:
        app: api
    spec:
      containers:
      - name: api
        image: api:beta
        ports:
        - containerPort: 9090
---
apiVersion: v1
kind: Service
metadata:
  name: api
  namespace: alpha
spec:
  selector:
    app: api
  ports:
  - port: 8080
---
apiVersion: v1
kind: Service
metadata:
  name: api
  namespace: beta
spec:
  selector:
    app: api
  ports:
  - port: 9090
"#;
        let result = import_from_yaml(yaml).unwrap();
        assert_eq!(result.config.app["api"].port, Some(8080));
        assert_eq!(result.config.app["beta-api"].port, Some(9090));
        let warnings = report_lines(&result.report);
        assert!(
            !warnings.iter().any(|w| w.starts_with("Service/")),
            "each Service attaches to its own namespace's workload: {warnings:?}"
        );
    }

    #[test]
    fn service_attaches_by_label_selector_not_name() {
        let yaml = r#"
apiVersion: apps/v1
kind: Deployment
metadata:
  name: web
spec:
  template:
    metadata:
      labels:
        app.kubernetes.io/name: web
        tier: frontend
    spec:
      containers:
      - name: web
        image: web:v1
        ports:
        - containerPort: 8080
        - containerPort: 9000
---
apiVersion: v1
kind: Service
metadata:
  name: web-http
spec:
  selector:
    app.kubernetes.io/name: web
  ports:
  - port: 9000
---
apiVersion: networking.k8s.io/v1
kind: Ingress
metadata:
  name: web
spec:
  rules:
  - host: web.example.com
    http:
      paths:
      - path: /
        pathType: Prefix
        backend:
          service:
            name: web-http
            port:
              number: 9000
"#;
        let result = import_from_yaml(yaml).unwrap();
        let web = &result.config.app["web"];
        assert_eq!(web.port, Some(9000), "the Service's port wins");
        assert_eq!(
            web.ingress.as_ref().map(|i| i.host.as_str()),
            Some("web.example.com"),
            "the Ingress follows its Service to the workload"
        );
        let warnings = report_lines(&result.report);
        // The Service's own name isn't kept, so its clients must be told.
        assert!(
            warnings
                .iter()
                .any(|w| w.starts_with("Service/web-http") && w.contains("web:9000")),
            "{warnings:?}"
        );
        assert!(
            !warnings.iter().any(|w| w.contains("nothing answers")),
            "{warnings:?}"
        );
    }

    #[test]
    fn service_selector_must_be_a_subset_of_the_pod_labels() {
        let yaml = r#"
apiVersion: apps/v1
kind: Deployment
metadata:
  name: web
spec:
  template:
    metadata:
      labels:
        app: web
    spec:
      containers:
      - name: web
        image: web:v1
        ports:
        - containerPort: 8080
---
apiVersion: v1
kind: Service
metadata:
  name: web
spec:
  selector:
    app: web
    tier: frontend
  ports:
  - port: 9999
"#;
        let result = import_from_yaml(yaml).unwrap();
        // Same name, but the selector doesn't match: Kubernetes would route
        // nothing, so neither does the import.
        assert_eq!(result.config.app["web"].port, Some(8080));
        let warnings = report_lines(&result.report);
        assert!(
            warnings
                .iter()
                .any(|w| w.starts_with("Service/web") && w.contains("selects no")),
            "{warnings:?}"
        );
    }

    #[test]
    fn service_selecting_two_workloads_is_reported() {
        let yaml = r#"
apiVersion: apps/v1
kind: Deployment
metadata:
  name: blue
spec:
  template:
    metadata:
      labels:
        app: shop
        colour: blue
    spec:
      containers:
      - name: shop
        image: shop:blue
---
apiVersion: apps/v1
kind: Deployment
metadata:
  name: green
spec:
  template:
    metadata:
      labels:
        app: shop
        colour: green
    spec:
      containers:
      - name: shop
        image: shop:green
---
apiVersion: v1
kind: Service
metadata:
  name: shop
spec:
  selector:
    app: shop
  ports:
  - port: 8080
"#;
        let result = import_from_yaml(yaml).unwrap();
        // Each workload still takes the Service's port...
        assert_eq!(result.config.app["blue"].port, Some(8080));
        assert_eq!(result.config.app["green"].port, Some(8080));
        // ...but one Service name can't front two apps.
        let warnings = report_lines(&result.report);
        assert!(
            warnings.iter().any(|w| w.starts_with("Service/shop")
                && w.contains("selects 2 workloads")
                && w.contains("Deployment/blue")
                && w.contains("Deployment/green")),
            "{warnings:?}"
        );
    }

    #[test]
    fn service_without_a_selector_falls_back_to_its_name_with_a_warning() {
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
        image: web:v1
        ports:
        - containerPort: 8080
---
apiVersion: v1
kind: Service
metadata:
  name: web
spec:
  ports:
  - port: 8080
"#;
        let result = import_from_yaml(yaml).unwrap();
        assert_eq!(result.config.app["web"].port, Some(8080));
        let warnings = report_lines(&result.report);
        assert!(
            warnings
                .iter()
                .any(|w| w.starts_with("Service/web") && w.contains("no selector")),
            "{warnings:?}"
        );
    }

    #[test]
    fn a_matching_service_named_after_its_workload_is_not_reported() {
        let yaml = r#"
apiVersion: apps/v1
kind: Deployment
metadata:
  name: web
spec:
  template:
    metadata:
      labels:
        app: web
    spec:
      containers:
      - name: web
        image: web:v1
        ports:
        - containerPort: 8080
---
apiVersion: v1
kind: Service
metadata:
  name: web
spec:
  selector:
    app: web
  ports:
  - port: 8080
"#;
        let result = import_from_yaml(yaml).unwrap();
        assert!(
            result.report.warnings.is_empty(),
            "{:?}",
            report_lines(&result.report)
        );
        assert!(result.report.dropped.is_empty());
    }

    #[test]
    fn env_from_configmap_in_input_is_flattened() {
        let yaml = r#"
apiVersion: v1
kind: ConfigMap
metadata:
  name: settings
data:
  LOG_LEVEL: debug
  MODE: from-configmap
---
apiVersion: apps/v1
kind: Deployment
metadata:
  name: web
spec:
  template:
    spec:
      containers:
      - name: web
        image: web:v1
        env:
        - name: MODE
          value: explicit
        envFrom:
        - configMapRef:
            name: settings
        - prefix: APP_
          configMapRef:
            name: settings
"#;
        let result = import_from_yaml(yaml).unwrap();
        let env = &result.config.app["web"].env;
        let plain = |key: &str| match env.get(key) {
            Some(EnvValue::Plain(v)) => Some(v.clone()),
            _ => None,
        };
        assert_eq!(plain("LOG_LEVEL").as_deref(), Some("debug"));
        assert_eq!(
            plain("MODE").as_deref(),
            Some("explicit"),
            "explicit env wins"
        );
        assert_eq!(plain("APP_LOG_LEVEL").as_deref(), Some("debug"));
        let warnings = report_lines(&result.report);
        assert!(
            warnings.iter().any(
                |w| w.starts_with("Deployment/web") && w.contains("envFrom ConfigMap/settings")
            ),
            "flattening is an approximation: {warnings:?}"
        );
        // A ConfigMap that envFrom consumed isn't "not referenced".
        assert!(
            !warnings.iter().any(|w| w.contains("not referenced")),
            "{warnings:?}"
        );
    }

    #[test]
    fn env_from_secret_is_warned() {
        let yaml = r#"
apiVersion: batch/v1
kind: Job
metadata:
  name: backup
spec:
  template:
    spec:
      containers:
      - name: backup
        image: backup:v1
        envFrom:
        - secretRef:
            name: s3-credentials
        - configMapRef:
            name: not-in-the-input
"#;
        let result = import_from_yaml(yaml).unwrap();
        assert!(result.config.job["backup"].env.is_empty());
        let warnings = report_lines(&result.report);
        assert!(
            warnings
                .iter()
                .any(|w| w.starts_with("Job/backup") && w.contains("Secret/s3-credentials")),
            "{warnings:?}"
        );
        assert!(
            warnings.iter().any(|w| w.starts_with("Job/backup")
                && w.contains("ConfigMap/not-in-the-input")
                && w.contains("not in the input")),
            "{warnings:?}"
        );
    }

    #[test]
    fn env_from_configmap_in_another_namespace_is_not_used() {
        let yaml = r#"
apiVersion: v1
kind: ConfigMap
metadata:
  name: settings
  namespace: other
data:
  LOG_LEVEL: debug
---
apiVersion: apps/v1
kind: Deployment
metadata:
  name: web
spec:
  template:
    spec:
      containers:
      - name: web
        image: web:v1
        envFrom:
        - configMapRef:
            name: settings
"#;
        let result = import_from_yaml(yaml).unwrap();
        assert!(result.config.app["web"].env.is_empty());
        let warnings = report_lines(&result.report);
        assert!(
            warnings
                .iter()
                .any(|w| w.starts_with("Deployment/web") && w.contains("not in the input")),
            "{warnings:?}"
        );
    }

    #[test]
    fn job_backoff_limit_maps_to_attempts() {
        let yaml = r#"
apiVersion: batch/v1
kind: Job
metadata:
  name: migrate
spec:
  backoffLimit: 2
  template:
    spec:
      activeDeadlineSeconds: 300
      containers:
      - name: migrate
        image: migrate:v1
---
apiVersion: batch/v1
kind: Job
metadata:
  name: patient
spec:
  backoffLimit: 30
  activeDeadlineSeconds: 900
  template:
    spec:
      containers:
      - name: patient
        image: patient:v1
"#;
        let result = import_from_yaml(yaml).unwrap();
        // backoffLimit counts retries; max_attempts counts the first try too.
        let migrate = &result.config.job["migrate"];
        assert_eq!(migrate.max_attempts, Some(3));
        // A pod's deadline is per attempt, exactly like task_timeout_secs.
        assert_eq!(migrate.task_timeout_secs, Some(300));

        let patient = &result.config.job["patient"];
        assert_eq!(patient.max_attempts, Some(10), "clamped to the 1-10 range");
        assert_eq!(patient.task_timeout_secs, Some(900));
        let warnings = report_lines(&result.report);
        assert!(
            warnings
                .iter()
                .any(|w| w.starts_with("Job/patient") && w.contains("backoffLimit 30")),
            "{warnings:?}"
        );
        assert!(
            warnings.iter().any(|w| w.starts_with("Job/patient")
                && w.contains("activeDeadlineSeconds 900")
                && w.contains("per attempt")),
            "{warnings:?}"
        );
        assert!(
            !warnings.iter().any(|w| w.starts_with("Job/migrate")),
            "{warnings:?}"
        );
    }

    #[test]
    fn job_completions_is_warned() {
        let yaml = r#"
apiVersion: batch/v1
kind: Job
metadata:
  name: render
spec:
  completions: 50
  parallelism: 5
  completionMode: Indexed
  template:
    spec:
      tolerations:
      - key: gpu
        operator: Exists
      affinity:
        nodeAffinity: {}
      nodeSelector:
        disk: ssd
      volumes:
      - name: scratch
        emptyDir: {}
      containers:
      - name: render
        image: render:v1
        workingDir: /work
        securityContext:
          runAsUser: 1000
      - name: sidecar
        image: sidecar:v1
---
apiVersion: batch/v1
kind: CronJob
metadata:
  name: nightly
spec:
  schedule: "0 3 * * *"
  timeZone: Europe/London
  startingDeadlineSeconds: 60
  jobTemplate:
    spec:
      completions: 2
      template:
        spec:
          containers:
          - name: nightly
            image: nightly:v1
"#;
        let result = import_from_yaml(yaml).unwrap();
        let warnings = report_lines(&result.report);
        for field in [
            "completions 50",
            "parallelism 5",
            "completionMode Indexed",
            "tolerations",
            "affinity",
            "nodeSelector",
            "volumes",
            "workingDir",
            "runAsUser",
            "sidecar(s)",
        ] {
            assert!(
                warnings
                    .iter()
                    .any(|w| w.starts_with("Job/render") && w.contains(field)),
                "no warning for {field}: {warnings:?}"
            );
        }
        for field in [
            "timeZone Europe/London",
            "startingDeadlineSeconds",
            "completions 2",
        ] {
            assert!(
                warnings
                    .iter()
                    .any(|w| w.starts_with("CronJob/nightly") && w.contains(field)),
                "no warning for {field}: {warnings:?}"
            );
        }
    }

    #[test]
    fn deployment_tolerations_and_affinity_are_warned() {
        let yaml = r#"
apiVersion: apps/v1
kind: Deployment
metadata:
  name: web
spec:
  template:
    spec:
      tolerations:
      - key: dedicated
        operator: Exists
      affinity:
        podAntiAffinity: {}
      containers:
      - name: web
        image: web:v1
"#;
        let result = import_from_yaml(yaml).unwrap();
        let warnings = report_lines(&result.report);
        for field in ["tolerations", "affinity"] {
            assert!(
                warnings
                    .iter()
                    .any(|w| w.starts_with("Deployment/web") && w.contains(field)),
                "no warning for {field}: {warnings:?}"
            );
        }
    }
}
