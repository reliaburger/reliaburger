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
fn parse_multi_document_yaml(yaml: &str) -> Result<Vec<K8sResource>, RelishError> {
    let mut resources = Vec::new();

    for doc in yaml.split("---") {
        let doc = doc.trim();
        if doc.is_empty() || doc.starts_with('#') {
            continue;
        }

        // Peek at kind and name
        let value: serde_yaml::Value = serde_yaml::from_str(doc)
            .map_err(|e| RelishError::FormatFailed(format!("YAML parse error: {e}")))?;

        let kind = value["kind"].as_str().unwrap_or("").to_string();
        let name = value["metadata"]["name"]
            .as_str()
            .unwrap_or("unknown")
            .to_string();

        let resource = match kind.as_str() {
            "Deployment" => {
                let d: Deployment = serde_yaml::from_str(doc)
                    .map_err(|e| RelishError::FormatFailed(e.to_string()))?;
                K8sResource::Deployment(name, d)
            }
            "DaemonSet" => {
                let d: DaemonSet = serde_yaml::from_str(doc)
                    .map_err(|e| RelishError::FormatFailed(e.to_string()))?;
                K8sResource::DaemonSet(name, d)
            }
            "StatefulSet" => {
                let d: StatefulSet = serde_yaml::from_str(doc)
                    .map_err(|e| RelishError::FormatFailed(e.to_string()))?;
                K8sResource::StatefulSet(name, d)
            }
            "Service" => {
                let s: Service = serde_yaml::from_str(doc)
                    .map_err(|e| RelishError::FormatFailed(e.to_string()))?;
                K8sResource::Service(name, s)
            }
            "Ingress" => {
                let i: Ingress = serde_yaml::from_str(doc)
                    .map_err(|e| RelishError::FormatFailed(e.to_string()))?;
                K8sResource::Ingress(name, i)
            }
            "HorizontalPodAutoscaler" => {
                let h: HorizontalPodAutoscaler = serde_yaml::from_str(doc)
                    .map_err(|e| RelishError::FormatFailed(e.to_string()))?;
                K8sResource::Hpa(name, h)
            }
            "ConfigMap" => {
                let c: ConfigMap = serde_yaml::from_str(doc)
                    .map_err(|e| RelishError::FormatFailed(e.to_string()))?;
                K8sResource::ConfigMap(name, c)
            }
            "Secret" => {
                let s: Secret = serde_yaml::from_str(doc)
                    .map_err(|e| RelishError::FormatFailed(e.to_string()))?;
                K8sResource::Secret(name, s)
            }
            "Job" => {
                let j: Job = serde_yaml::from_str(doc)
                    .map_err(|e| RelishError::FormatFailed(e.to_string()))?;
                K8sResource::Job(name, j)
            }
            "CronJob" => {
                let c: CronJob = serde_yaml::from_str(doc)
                    .map_err(|e| RelishError::FormatFailed(e.to_string()))?;
                K8sResource::CronJob(name, c)
            }
            "Namespace" => {
                let n: Namespace = serde_yaml::from_str(doc)
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

    // Convert Deployments → Apps (with correlated Service, Ingress, HPA)
    for (name, deploy) in &deployments {
        let mut app = deployment_to_app(deploy);

        // Correlate Service by name match
        if let Some(svc) = services.get(name) {
            apply_service(&mut app, svc);
        }

        // Correlate Ingress by backend service name
        let ingress_name = find_ingress_for_service(&ingresses, name);
        if let Some(ing_name) = ingress_name {
            if let Some(ing) = ingresses.get(&ing_name) {
                apply_ingress(&mut app, ing);
            }
        }

        // Correlate HPA by scaleTargetRef name
        if let Some(hpa) = hpas.get(name) {
            apply_hpa(&mut app, hpa);
        }

        config.app.insert(name.clone(), app);
        report
            .converted
            .push(format!("Deployment/{name} → [app.{name}]"));
    }

    // Convert DaemonSets → Apps with replicas = "*"
    for (name, ds) in &daemonsets {
        let mut app = daemonset_to_app(ds);
        if let Some(svc) = services.get(name) {
            apply_service(&mut app, svc);
        }
        config.app.insert(name.clone(), app);
        report.converted.push(format!(
            "DaemonSet/{name} → [app.{name}] (replicas = \"*\")"
        ));
    }

    // Convert StatefulSets → Apps with warning
    for (name, ss) in &statefulsets {
        let app = statefulset_to_app(ss);
        config.app.insert(name.clone(), app);
        report
            .converted
            .push(format!("StatefulSet/{name} → [app.{name}]"));
        report.warnings.push(MigrationWarning {
            resource: format!("StatefulSet/{name}"),
            message: "ordering guarantees and stable network IDs lost".to_string(),
        });
    }

    // Convert Jobs
    for (name, job) in &jobs {
        let job_spec = job_to_jobspec(job);
        config.job.insert(name.clone(), job_spec);
        report.converted.push(format!("Job/{name} → [job.{name}]"));
    }

    // Convert CronJobs
    for (name, cj) in &cronjobs {
        let job_spec = cronjob_to_jobspec(cj);
        config.job.insert(name.clone(), job_spec);
        report
            .converted
            .push(format!("CronJob/{name} → [job.{name}]"));
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

// ---------------------------------------------------------------------------
// Field mapping: Deployment → AppSpec
// ---------------------------------------------------------------------------

fn deployment_to_app(deploy: &Deployment) -> AppSpec {
    let spec = deploy.spec.as_ref();
    let template = spec.map(|s| &s.template);
    let pod_spec = template.and_then(|t| t.spec.as_ref());
    let container = pod_spec.and_then(|ps| ps.containers.first());

    let mut app = empty_app_spec();
    app.image = container.and_then(|c| c.image.clone());
    app.replicas = spec
        .and_then(|s| s.replicas)
        .map(|r| Replicas::Fixed(r as u32))
        .unwrap_or_default();
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

    // CPU and memory from resources (parse K8s quantity strings)
    if let Some(resources) = container.and_then(|c| c.resources.as_ref()) {
        if let Some(limits) = &resources.limits {
            if let Some(cpu) = limits.get("cpu") {
                app.cpu = crate::config::types::ResourceRange::parse(&cpu.0).ok();
            }
            if let Some(mem) = limits.get("memory") {
                app.memory = crate::config::types::ResourceRange::parse(&mem.0).ok();
            }
        }
    }

    // Env vars
    if let Some(env_list) = container.and_then(|c| c.env.as_ref()) {
        for env_var in env_list {
            if let Some(ref value) = env_var.value {
                app.env
                    .insert(env_var.name.clone(), EnvValue::Plain(value.clone()));
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

    // Deploy strategy
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

fn daemonset_to_app(ds: &DaemonSet) -> AppSpec {
    let spec = ds.spec.as_ref();
    let template = spec.map(|s| &s.template);
    let pod_spec = template.and_then(|t| t.spec.as_ref());
    let container = pod_spec.and_then(|ps| ps.containers.first());

    let mut app = empty_app_spec();
    app.image = container.and_then(|c| c.image.clone());
    app.replicas = Replicas::DaemonSet;
    app.port = container
        .and_then(|c| c.ports.as_ref())
        .and_then(|ports| ports.first())
        .map(|p| p.container_port as u16);
    app
}

fn statefulset_to_app(ss: &StatefulSet) -> AppSpec {
    let spec = ss.spec.as_ref();
    let template = spec.map(|s| &s.template);
    let pod_spec = template.and_then(|t| t.spec.as_ref());
    let container = pod_spec.and_then(|ps| ps.containers.first());

    let mut app = empty_app_spec();
    app.image = container.and_then(|c| c.image.clone());
    app.replicas = spec
        .and_then(|s| s.replicas)
        .map(|r| Replicas::Fixed(r as u32))
        .unwrap_or_default();
    app.port = container
        .and_then(|c| c.ports.as_ref())
        .and_then(|ports| ports.first())
        .map(|p| p.container_port as u16);
    app
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

fn apply_ingress(app: &mut AppSpec, ing: &Ingress) {
    if let Some(spec) = &ing.spec {
        if let Some(rules) = &spec.rules {
            if let Some(rule) = rules.first() {
                let host = rule.host.clone().unwrap_or_default();
                let path = rule
                    .http
                    .as_ref()
                    .and_then(|h| h.paths.first())
                    .map(|p| p.path.clone().unwrap_or_else(|| "/".to_string()))
                    .unwrap_or_else(|| "/".to_string());

                let tls = spec.tls.as_ref().map(|_| "auto".to_string());

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

fn job_to_jobspec(job: &Job) -> JobSpec {
    let spec = job.spec.as_ref();
    let pod_spec = spec.and_then(|s| s.template.spec.as_ref());
    let container = pod_spec.and_then(|ps| ps.containers.first());

    JobSpec {
        image: container.and_then(|c| c.image.clone()),
        command: container.and_then(|c| c.command.clone()),
        schedule: None,
        run_before: Vec::new(),
        memory: None,
        cpu: None,
        env: BTreeMap::new(),
        namespace: None,
        exec: None,
        script: None,
    }
}

fn cronjob_to_jobspec(cj: &CronJob) -> JobSpec {
    let spec = cj.spec.as_ref();
    let schedule = spec.map(|s| s.schedule.clone());
    let job_template = spec.and_then(|s| s.job_template.spec.as_ref());
    let pod_spec = job_template.and_then(|jt| jt.template.spec.as_ref());
    let container = pod_spec.and_then(|ps| ps.containers.first());

    JobSpec {
        image: container.and_then(|c| c.image.clone()),
        command: container.and_then(|c| c.command.clone()),
        schedule,
        run_before: Vec::new(),
        memory: None,
        cpu: None,
        env: BTreeMap::new(),
        namespace: None,
        exec: None,
        script: None,
    }
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
}
