//! Reliaburger TOML → Kubernetes YAML exporter.
//!
//! Converts a Reliaburger Config into multi-document Kubernetes YAML.
//! Each app produces a Deployment (or DaemonSet) and Service, optionally
//! with Ingress and HPA. Jobs produce Job or CronJob. Namespaces produce
//! Namespace and ResourceQuota.

use std::collections::BTreeMap;

use k8s_openapi::api::apps::v1::{
    DaemonSet, DaemonSetSpec, Deployment, DeploymentSpec, DeploymentStrategy,
    RollingUpdateDeployment,
};
use k8s_openapi::api::autoscaling::v2::{
    HorizontalPodAutoscaler, HorizontalPodAutoscalerSpec, MetricSpec, MetricTarget,
    ResourceMetricSource,
};
use k8s_openapi::api::batch::v1::{CronJob, CronJobSpec, Job, JobSpec, JobTemplateSpec};
use k8s_openapi::api::core::v1::{
    Container, ContainerPort, EnvVar, Namespace, PodSpec, PodTemplateSpec, ResourceQuota,
    ResourceQuotaSpec, ResourceRequirements, Service, ServicePort, ServiceSpec,
};
use k8s_openapi::api::networking::v1::{
    HTTPIngressPath, HTTPIngressRuleValue, Ingress, IngressBackend, IngressRule,
    IngressServiceBackend, IngressSpec, ServiceBackendPort,
};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta;
use k8s_openapi::apimachinery::pkg::util::intstr::IntOrString;

use crate::config::Config;
use crate::config::app::AppSpec;
use crate::config::types::Replicas;

use super::RelishError;

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// Result of a Kubernetes export.
#[derive(Debug)]
pub struct ExportResult {
    /// Multi-document YAML (separated by `---`).
    pub yaml: String,
    /// Report of what was created and what's unsupported.
    pub report: ExportReport,
}

/// Export report.
#[derive(Debug, Default)]
pub struct ExportReport {
    /// Resources created.
    pub resources_created: Vec<String>,
    /// Features with no K8s equivalent.
    pub unsupported: Vec<String>,
    /// Fields that *do* have a Kubernetes equivalent but this exporter
    /// doesn't translate yet (M28).
    ///
    /// Kept separate from `unsupported` because the two mean different things
    /// to whoever reads the report: "Kubernetes cannot express this" versus
    /// "we haven't written this bit yet". Conflating them tells an operator to
    /// stop looking for a solution that exists.
    pub dropped: Vec<String>,
}

impl std::fmt::Display for ExportReport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if !self.resources_created.is_empty() {
            writeln!(f, "Created:")?;
            for r in &self.resources_created {
                writeln!(f, "  + {r}")?;
            }
        }
        if !self.unsupported.is_empty() {
            writeln!(f, "\nUnsupported (no K8s equivalent):")?;
            for u in &self.unsupported {
                writeln!(f, "  - {u}")?;
            }
        }
        if !self.dropped.is_empty() {
            writeln!(f, "\nNot exported yet (a K8s equivalent exists):")?;
            for d in &self.dropped {
                writeln!(f, "  ! {d}")?;
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Export entry point
// ---------------------------------------------------------------------------

/// Export a Reliaburger Config to Kubernetes YAML.
pub fn export_kubernetes(config: &Config) -> Result<ExportResult, RelishError> {
    let mut docs = Vec::new();
    let mut report = ExportReport::default();

    for (name, app) in &config.app {
        export_app(name, app, &mut docs, &mut report)?;
    }

    for (name, job) in &config.job {
        export_job(name, job, &mut docs, &mut report)?;
    }

    for (name, ns) in &config.namespace {
        export_namespace(name, ns, &mut docs, &mut report)?;
    }

    let yaml = docs.join("---\n");

    Ok(ExportResult { yaml, report })
}

// ---------------------------------------------------------------------------
// App export
// ---------------------------------------------------------------------------

fn export_app(
    name: &str,
    app: &AppSpec,
    docs: &mut Vec<String>,
    report: &mut ExportReport,
) -> Result<(), RelishError> {
    let labels = BTreeMap::from([("app".to_string(), name.to_string())]);
    let app_namespace = app.namespace.clone();

    // Container
    let mut container = Container {
        name: name.to_string(),
        image: app.image.clone(),
        ..Container::default()
    };

    // M28: `command` was dropped, so an exported app silently ran its image's
    // default entrypoint instead of the command the config specified — a
    // different workload wearing the same name.
    if !app.command.is_empty() {
        container.command = Some(app.command.clone());
    }

    if let Some(port) = app.port {
        container.ports = Some(vec![ContainerPort {
            container_port: port as i32,
            ..ContainerPort::default()
        }]);
    }

    // Env vars — shared with the job path, which used to drop them (M28).
    container.env = env_vars(&app.env);

    // M28: resource requests were dropped entirely, so an app with a 512Mi
    // memory limit exported as an unlimited pod — the scheduler saw a
    // different workload from the one that was configured.
    container.resources = resource_requirements(app.memory.as_ref(), app.cpu.as_ref(), app.gpu);

    let pod_spec = PodSpec {
        containers: vec![container],
        ..PodSpec::default()
    };

    let template = PodTemplateSpec {
        metadata: Some(ObjectMeta {
            labels: Some(labels.clone()),
            ..ObjectMeta::default()
        }),
        spec: Some(pod_spec),
    };

    // Deployment or DaemonSet
    if app.replicas == Replicas::DaemonSet {
        let ds = DaemonSet {
            metadata: ObjectMeta {
                name: Some(name.to_string()),
                // M28: without this every exported resource lands in
                // `default`, silently collapsing two namespaces' apps of the
                // same name into one — the same class of bug as DEP1.
                namespace: app_namespace.clone(),
                ..ObjectMeta::default()
            },
            spec: Some(DaemonSetSpec {
                selector: label_selector(&labels),
                template,
                ..DaemonSetSpec::default()
            }),
            ..DaemonSet::default()
        };
        docs.push(to_yaml(&ds)?);
        report.resources_created.push(format!("DaemonSet/{name}"));
    } else {
        let replicas = match app.replicas {
            Replicas::Fixed(n) => Some(n as i32),
            Replicas::DaemonSet => None,
        };

        let mut strategy = None;
        if let Some(ref deploy) = app.deploy {
            if deploy.strategy.as_deref() == Some("rolling") {
                let mut rolling = RollingUpdateDeployment::default();
                if let Some(surge) = deploy.max_surge {
                    rolling.max_surge = Some(IntOrString::Int(surge as i32));
                }
                if let Some(unavail) = deploy.max_unavailable {
                    rolling.max_unavailable = Some(IntOrString::Int(unavail as i32));
                }
                strategy = Some(DeploymentStrategy {
                    type_: Some("RollingUpdate".to_string()),
                    rolling_update: Some(rolling),
                });
            }
        }

        let deploy = Deployment {
            metadata: ObjectMeta {
                name: Some(name.to_string()),
                // M28: without this every exported resource lands in
                // `default`, silently collapsing two namespaces' apps of the
                // same name into one — the same class of bug as DEP1.
                namespace: app_namespace.clone(),
                ..ObjectMeta::default()
            },
            spec: Some(DeploymentSpec {
                replicas,
                selector: label_selector(&labels),
                template,
                strategy,
                ..DeploymentSpec::default()
            }),
            ..Deployment::default()
        };
        docs.push(to_yaml(&deploy)?);
        report.resources_created.push(format!("Deployment/{name}"));
    }

    // Service (if the app has a port)
    if let Some(port) = app.port {
        let svc = Service {
            metadata: ObjectMeta {
                name: Some(name.to_string()),
                // M28: without this every exported resource lands in
                // `default`, silently collapsing two namespaces' apps of the
                // same name into one — the same class of bug as DEP1.
                namespace: app_namespace.clone(),
                ..ObjectMeta::default()
            },
            spec: Some(ServiceSpec {
                selector: Some(labels.clone()),
                ports: Some(vec![ServicePort {
                    port: port as i32,
                    target_port: Some(IntOrString::Int(port as i32)),
                    ..ServicePort::default()
                }]),
                ..ServiceSpec::default()
            }),
            ..Service::default()
        };
        docs.push(to_yaml(&svc)?);
        report.resources_created.push(format!("Service/{name}"));
    }

    // Ingress (if configured)
    if let Some(ref ingress) = app.ingress {
        let ing = Ingress {
            metadata: ObjectMeta {
                name: Some(format!("{name}-ingress")),
                // M28: the Ingress was the one resource that omitted the
                // namespace, so a namespaced app's Ingress landed in
                // `default` where it could not reach its Service.
                namespace: app_namespace.clone(),
                ..ObjectMeta::default()
            },
            spec: Some(IngressSpec {
                rules: Some(vec![IngressRule {
                    host: Some(ingress.host.clone()),
                    http: Some(HTTPIngressRuleValue {
                        paths: vec![HTTPIngressPath {
                            path: ingress.path.clone(),
                            path_type: "Prefix".to_string(),
                            backend: IngressBackend {
                                service: Some(IngressServiceBackend {
                                    name: name.to_string(),
                                    port: Some(ServiceBackendPort {
                                        number: app.port.map(|p| p as i32),
                                        ..ServiceBackendPort::default()
                                    }),
                                }),
                                ..IngressBackend::default()
                            },
                        }],
                    }),
                }]),
                ..IngressSpec::default()
            }),
            ..Ingress::default()
        };
        docs.push(to_yaml(&ing)?);
        report
            .resources_created
            .push(format!("Ingress/{name}-ingress"));
    }

    // HPA (if autoscale configured). A DaemonSet has no scale subresource, so
    // an HPA cannot target it — the old code emitted one anyway, with a
    // scaleTargetRef pointing at a Deployment that doesn't exist.
    if let Some(ref auto) = app.autoscale {
        if app.replicas == Replicas::DaemonSet {
            report.unsupported.push(format!(
                "[app.{name}.autoscale] — a DaemonSet has no scale subresource, no HPA emitted"
            ));
        } else {
            // A target that doesn't convert to a whole percentage must not
            // become `type: Utilization` with no `averageUtilization` — the
            // API server rejects that. Emit the HPA without the metric (the
            // K8s default applies) and say so.
            let average_utilization = parse_target_pct(&auto.target);
            let metrics = average_utilization.map(|pct| {
                vec![MetricSpec {
                    type_: "Resource".to_string(),
                    resource: Some(ResourceMetricSource {
                        name: auto.metric.clone(),
                        target: MetricTarget {
                            type_: "Utilization".to_string(),
                            average_utilization: Some(pct),
                            ..MetricTarget::default()
                        },
                    }),
                    ..MetricSpec::default()
                }]
            });
            if metrics.is_none() {
                report.unsupported.push(format!(
                    "[app.{name}.autoscale] target {:?} — could not convert to a percentage; \
                     HPA emitted without a metric (the K8s default applies)",
                    auto.target
                ));
            }
            let hpa = HorizontalPodAutoscaler {
                metadata: ObjectMeta {
                    name: Some(name.to_string()),
                    // M28: without this every exported resource lands in
                    // `default`, silently collapsing two namespaces' apps of
                    // the same name into one — the same class of bug as DEP1.
                    namespace: app_namespace.clone(),
                    ..ObjectMeta::default()
                },
                spec: Some(HorizontalPodAutoscalerSpec {
                    scale_target_ref:
                        k8s_openapi::api::autoscaling::v2::CrossVersionObjectReference {
                            api_version: Some("apps/v1".to_string()),
                            kind: "Deployment".to_string(),
                            name: name.to_string(),
                        },
                    min_replicas: Some(auto.min as i32),
                    max_replicas: auto.max as i32,
                    metrics,
                    ..HorizontalPodAutoscalerSpec::default()
                }),
                ..HorizontalPodAutoscaler::default()
            };
            docs.push(to_yaml(&hpa)?);
            report
                .resources_created
                .push(format!("HorizontalPodAutoscaler/{name}"));
        }
    }

    // Report unsupported features
    if app.firewall.is_some() {
        report.unsupported.push(format!(
            "[app.{name}.firewall] — use NetworkPolicy manually"
        ));
    }
    if app.egress.is_some() {
        report
            .unsupported
            .push(format!("[app.{name}.egress] — use NetworkPolicy manually"));
    }
    if app.exec.is_some() || app.script.is_some() {
        report.unsupported.push(format!(
            "[app.{name}] exec/script process workloads — no K8s equivalent"
        ));
    }

    // M28: everything below has a perfectly good Kubernetes equivalent and is
    // simply not translated yet. The distinction matters, so these are reported
    // as *dropped* rather than "unsupported": an operator reading "unsupported"
    // concludes Kubernetes can't express it, when in fact this exporter can't.
    // Either way, silence was the real bug — an export that looks complete and
    // has quietly discarded your probes is worse than one that says so.
    for (present, field, equivalent) in [
        (
            app.health.is_some(),
            "health",
            "livenessProbe/readinessProbe",
        ),
        (!app.volumes.is_empty(), "volumes", "volumes + volumeMounts"),
        (!app.init.is_empty(), "init", "initContainers"),
        (!app.config_file.is_empty(), "config_file", "ConfigMap"),
        (
            app.placement.is_some(),
            "placement",
            "nodeSelector/affinity",
        ),
    ] {
        if present {
            report.dropped.push(format!(
                "[app.{name}] {field} — has a K8s equivalent ({equivalent}) but is not exported yet"
            ));
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Job export
// ---------------------------------------------------------------------------

fn export_job(
    name: &str,
    job: &crate::config::JobSpec,
    docs: &mut Vec<String>,
    report: &mut ExportReport,
) -> Result<(), RelishError> {
    let app_namespace = job.namespace.clone();
    // Env and resources go through the same helpers as the app path — the job
    // exporter used to drop both, so a job with a memory limit and database
    // credentials exported as an unlimited pod with no environment.
    let container = Container {
        name: name.to_string(),
        image: job.image.clone(),
        command: job.command.clone(),
        env: env_vars(&job.env),
        resources: resource_requirements(job.memory.as_ref(), job.cpu.as_ref(), None),
        ..Container::default()
    };

    let pod_spec = PodSpec {
        containers: vec![container],
        restart_policy: Some("Never".to_string()),
        ..PodSpec::default()
    };

    let template = PodTemplateSpec {
        spec: Some(pod_spec),
        ..PodTemplateSpec::default()
    };

    if let Some(ref schedule) = job.schedule {
        // CronJob
        let cj = CronJob {
            metadata: ObjectMeta {
                name: Some(name.to_string()),
                // M28: without this every exported resource lands in
                // `default`, silently collapsing two namespaces' apps of the
                // same name into one — the same class of bug as DEP1.
                namespace: app_namespace.clone(),
                ..ObjectMeta::default()
            },
            spec: Some(CronJobSpec {
                schedule: schedule.clone(),
                job_template: JobTemplateSpec {
                    spec: Some(JobSpec {
                        template,
                        ..JobSpec::default()
                    }),
                    ..JobTemplateSpec::default()
                },
                ..CronJobSpec::default()
            }),
            ..CronJob::default()
        };
        docs.push(to_yaml(&cj)?);
        report.resources_created.push(format!("CronJob/{name}"));
    } else {
        // Job
        let j = Job {
            metadata: ObjectMeta {
                name: Some(name.to_string()),
                // M28: without this every exported resource lands in
                // `default`, silently collapsing two namespaces' apps of the
                // same name into one — the same class of bug as DEP1.
                namespace: app_namespace.clone(),
                ..ObjectMeta::default()
            },
            spec: Some(JobSpec {
                template,
                ..JobSpec::default()
            }),
            ..Job::default()
        };
        docs.push(to_yaml(&j)?);
        report.resources_created.push(format!("Job/{name}"));
    }

    if !job.run_before.is_empty() {
        report.unsupported.push(format!(
            "[job.{name}] run_before — use Argo Workflows or init containers"
        ));
    }
    if job.exec.is_some() || job.script.is_some() {
        report.unsupported.push(format!(
            "[job.{name}] exec/script process workloads — no K8s equivalent"
        ));
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Namespace export
// ---------------------------------------------------------------------------

fn export_namespace(
    name: &str,
    ns_spec: &crate::config::NamespaceSpec,
    docs: &mut Vec<String>,
    report: &mut ExportReport,
) -> Result<(), RelishError> {
    let ns = Namespace {
        metadata: ObjectMeta {
            name: Some(name.to_string()),
            ..ObjectMeta::default()
        },
        ..Namespace::default()
    };
    docs.push(to_yaml(&ns)?);
    report.resources_created.push(format!("Namespace/{name}"));

    // ResourceQuota for the budget fields (the module doc always promised
    // one; until M28 every quota field was silently dropped). Reliaburger's
    // TOML forms (`8000m`, `16Gi`, bare integers) are already valid K8s
    // quantities, so they pass through verbatim.
    let quantity = |value: String| k8s_openapi::apimachinery::pkg::api::resource::Quantity(value);
    let mut hard: BTreeMap<String, _> = BTreeMap::new();
    if let Some(ref cpu) = ns_spec.cpu {
        hard.insert("limits.cpu".to_string(), quantity(cpu.clone()));
    }
    if let Some(ref memory) = ns_spec.memory {
        hard.insert("limits.memory".to_string(), quantity(memory.clone()));
    }
    if let Some(gpu) = ns_spec.gpu {
        hard.insert(
            "requests.nvidia.com/gpu".to_string(),
            quantity(gpu.to_string()),
        );
    }
    if !hard.is_empty() {
        let quota = ResourceQuota {
            metadata: ObjectMeta {
                name: Some(format!("{name}-quota")),
                namespace: Some(name.to_string()),
                ..ObjectMeta::default()
            },
            spec: Some(ResourceQuotaSpec {
                hard: Some(hard),
                ..ResourceQuotaSpec::default()
            }),
            ..ResourceQuota::default()
        };
        docs.push(to_yaml(&quota)?);
        report
            .resources_created
            .push(format!("ResourceQuota/{name}-quota"));
    }

    // K8s ResourceQuota counts objects per API kind (`count/deployments.apps`),
    // not "distinct apps", and has no total-replica cap at all.
    if ns_spec.max_apps.is_some() {
        report.unsupported.push(format!(
            "[namespace.{name}] max_apps — K8s quotas count objects per kind, not apps"
        ));
    }
    if ns_spec.max_replicas.is_some() {
        report.unsupported.push(format!(
            "[namespace.{name}] max_replicas — no K8s equivalent for a total-replica cap"
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn label_selector(
    labels: &BTreeMap<String, String>,
) -> k8s_openapi::apimachinery::pkg::apis::meta::v1::LabelSelector {
    k8s_openapi::apimachinery::pkg::apis::meta::v1::LabelSelector {
        match_labels: Some(labels.clone()),
        ..Default::default()
    }
}

fn to_yaml<T: serde::Serialize>(resource: &T) -> Result<String, RelishError> {
    serde_yaml::to_string(resource).map_err(|e| RelishError::FormatFailed(e.to_string()))
}

/// Map Reliaburger env vars to Kubernetes `EnvVar`s.
///
/// Encrypted values export as the placeholder `ENCRYPTED_VALUE` — the sealed
/// ciphertext is useless outside the cluster that holds the key, and leaking
/// it would be worse than a placeholder that fails loudly.
fn env_vars(env: &BTreeMap<String, crate::config::types::EnvValue>) -> Option<Vec<EnvVar>> {
    if env.is_empty() {
        return None;
    }
    Some(
        env.iter()
            .map(|(k, v)| {
                let value = match v {
                    crate::config::types::EnvValue::Plain(s) => s.clone(),
                    crate::config::types::EnvValue::Encrypted(_) => "ENCRYPTED_VALUE".to_string(),
                };
                EnvVar {
                    name: k.clone(),
                    value: Some(value),
                    ..EnvVar::default()
                }
            })
            .collect(),
    )
}

/// Build Kubernetes resource requests/limits from Reliaburger's ranges.
///
/// `ResourceRange` carries a request and a limit, which map exactly onto
/// Kubernetes' two fields — so unlike most of this exporter the translation
/// is lossless. Reliaburger stores CPU in millicores, which is also
/// Kubernetes' `m` suffix, so the numbers carry across unchanged.
fn resource_requirements(
    memory: Option<&crate::config::types::ResourceRange>,
    cpu: Option<&crate::config::types::ResourceRange>,
    gpu: Option<u32>,
) -> Option<ResourceRequirements> {
    let quantity = |value: String| k8s_openapi::apimachinery::pkg::api::resource::Quantity(value);
    let mut requests: BTreeMap<String, _> = BTreeMap::new();
    let mut limits: BTreeMap<String, _> = BTreeMap::new();
    if let Some(memory) = memory {
        requests.insert("memory".to_string(), quantity(memory.request.to_string()));
        limits.insert("memory".to_string(), quantity(memory.limit.to_string()));
    }
    if let Some(cpu) = cpu {
        requests.insert("cpu".to_string(), quantity(format!("{}m", cpu.request)));
        limits.insert("cpu".to_string(), quantity(format!("{}m", cpu.limit)));
    }
    if let Some(gpu) = gpu.filter(|count| *count > 0) {
        requests.insert("nvidia.com/gpu".to_string(), quantity(gpu.to_string()));
        limits.insert("nvidia.com/gpu".to_string(), quantity(gpu.to_string()));
    }
    if limits.is_empty() {
        return None;
    }
    Some(ResourceRequirements {
        limits: Some(limits),
        requests: Some(requests),
        ..ResourceRequirements::default()
    })
}

/// Parse an autoscale target into a whole percentage for the HPA's
/// `averageUtilization`.
///
/// Accepts the same forms config validation does (`parse_percentage` in
/// `meat::autoscaler`): `"70%"` and the raw fraction `"0.7"` both mean 70.
/// Anything that doesn't land in 1-100 after conversion returns `None` — the
/// caller reports it instead of emitting a `Utilization` metric with no
/// `averageUtilization`, which the API server rejects.
fn parse_target_pct(target: &str) -> Option<i32> {
    let s = target.trim();
    let fraction = if let Some(pct) = s.strip_suffix('%') {
        pct.trim().parse::<f64>().ok().map(|v| v / 100.0)
    } else {
        s.parse::<f64>().ok()
    }?;
    let pct = (fraction * 100.0).round() as i32;
    (1..=100).contains(&pct).then_some(pct)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn parse_config(toml: &str) -> Config {
        Config::parse(toml).unwrap()
    }

    #[test]
    fn export_app_to_deployment_service() {
        let config = parse_config(
            r#"
            [app.web]
            image = "myapp:v1"
            replicas = 3
            port = 8080
            "#,
        );
        let result = export_kubernetes(&config).unwrap();
        assert!(result.yaml.contains("kind: Deployment"));
        assert!(result.yaml.contains("kind: Service"));
        assert!(result.yaml.contains("myapp:v1"));
        assert!(result.yaml.contains("replicas: 3"));
        assert!(
            result
                .report
                .resources_created
                .iter()
                .any(|r| r.contains("Deployment/web"))
        );
        assert!(
            result
                .report
                .resources_created
                .iter()
                .any(|r| r.contains("Service/web"))
        );
    }

    #[test]
    fn export_daemonset_from_star_replicas() {
        let config = parse_config(
            r#"
            [app.monitoring]
            image = "monitor:v1"
            replicas = "*"
            "#,
        );
        let result = export_kubernetes(&config).unwrap();
        assert!(result.yaml.contains("kind: DaemonSet"));
        assert!(!result.yaml.contains("kind: Deployment"));
    }

    #[test]
    fn export_app_with_ingress() {
        let config = parse_config(
            r#"
            [app.web]
            image = "myapp:v1"
            port = 8080

            [app.web.ingress]
            host = "myapp.com"
            "#,
        );
        let result = export_kubernetes(&config).unwrap();
        assert!(result.yaml.contains("kind: Ingress"));
        assert!(result.yaml.contains("myapp.com"));
    }

    #[test]
    fn export_app_with_autoscale_produces_hpa() {
        let config = parse_config(
            r#"
            [app.web]
            image = "myapp:v1"
            replicas = 3

            [app.web.autoscale]
            metric = "cpu"
            target = "70%"
            min = 2
            max = 10
            "#,
        );
        let result = export_kubernetes(&config).unwrap();
        assert!(result.yaml.contains("kind: HorizontalPodAutoscaler"));
        assert!(
            result
                .report
                .resources_created
                .iter()
                .any(|r| r.contains("HorizontalPodAutoscaler"))
        );
    }

    #[test]
    fn export_job_and_cronjob() {
        let config = parse_config(
            r#"
            [job.migrate]
            image = "migrate:v1"
            command = ["npm", "run", "migrate"]

            [job.cleanup]
            image = "cleanup:latest"
            schedule = "0 3 * * *"
            "#,
        );
        let result = export_kubernetes(&config).unwrap();
        assert!(result.yaml.contains("kind: Job"));
        assert!(result.yaml.contains("kind: CronJob"));
    }

    #[test]
    fn export_reports_unsupported_features() {
        let config = parse_config(
            r#"
            [app.web]
            image = "myapp:v1"
            exec = "/usr/bin/myapp"
            "#,
        );
        let result = export_kubernetes(&config).unwrap();
        assert!(
            result
                .report
                .unsupported
                .iter()
                .any(|u| u.contains("exec/script"))
        );
    }

    // -- export fidelity (M28) ------------------------------------------------

    /// M28: `namespace` was dropped, so every exported resource landed in
    /// `default`. Two namespaces' apps of the same name collapsed into one —
    /// the same collision DEP1 fixed inside Reliaburger, reintroduced on the
    /// way out.
    #[test]
    fn export_carries_the_namespace_onto_every_resource() {
        let config = parse_config(
            "[app.web]\nimage = \"web:v1\"\nport = 8080\nnamespace = \"team-a\"\n\n\
             [app.web.ingress]\nhost = \"web.example.com\"\n",
        );
        let result = export_kubernetes(&config).unwrap();
        assert!(
            result.yaml.contains("namespace: team-a"),
            "no namespace in the exported YAML:\n{}",
            result.yaml
        );
        // Deployment, Service AND Ingress must carry it — the Ingress was the
        // one resource that didn't, so it landed in `default` where it could
        // not reach its Service.
        assert_eq!(
            result.yaml.matches("namespace: team-a").count(),
            3,
            "expected the namespace on Deployment, Service and Ingress:\n{}",
            result.yaml
        );
    }

    /// M28: `command` was dropped, so the exported pod ran the image's default
    /// entrypoint — a different workload under the same name.
    #[test]
    fn export_carries_the_command() {
        let config = parse_config(
            "[app.web]\nimage = \"web:v1\"\ncommand = [\"./server\", \"--port=8080\"]\n",
        );
        let result = export_kubernetes(&config).unwrap();
        assert!(
            result.yaml.contains("./server") && result.yaml.contains("--port=8080"),
            "command missing from the export:\n{}",
            result.yaml
        );
    }

    /// M28: resource requests were dropped, so a memory-limited app exported as
    /// an unlimited pod and the K8s scheduler saw a different workload.
    #[test]
    fn export_carries_resource_requests_and_limits() {
        let config = parse_config(
            "[app.web]\nimage = \"web:v1\"\nmemory = \"256Mi-512Mi\"\ncpu = \"100m-500m\"\n",
        );
        let result = export_kubernetes(&config).unwrap();
        assert!(
            result.yaml.contains("resources:"),
            "no resources block:\n{}",
            result.yaml
        );
        assert!(
            result.yaml.contains("limits:") && result.yaml.contains("requests:"),
            "a range must export both bounds, not one:\n{}",
            result.yaml
        );
        assert!(
            result.yaml.contains("500m"),
            "cpu limit missing:\n{}",
            result.yaml
        );
    }

    /// What the exporter still can't do must be *said*, not silently omitted —
    /// and said as "not written yet" rather than "Kubernetes can't", which
    /// would send an operator looking for a workaround they don't need.
    #[test]
    fn fields_with_a_k8s_equivalent_are_reported_as_dropped() {
        let config = parse_config(
            "[app.web]\nimage = \"web:v1\"\nport = 8080\n\n[app.web.health]\npath = \"/healthz\"\n",
        );
        let result = export_kubernetes(&config).unwrap();
        assert!(
            result.report.dropped.iter().any(|d| d.contains("health")),
            "a dropped health check went unreported: {:?}",
            result.report.dropped
        );
        assert!(
            !result
                .report
                .unsupported
                .iter()
                .any(|u| u.contains("health")),
            "health has a K8s equivalent, so it must not be called unsupported"
        );
        // The report renders both sections distinctly.
        let rendered = result.report.to_string();
        assert!(rendered.contains("Not exported yet"), "{rendered}");
    }

    #[test]
    fn a_fully_exportable_app_reports_nothing_dropped() {
        let config = parse_config("[app.web]\nimage = \"web:v1\"\nport = 8080\n");
        let result = export_kubernetes(&config).unwrap();
        assert!(
            result.report.dropped.is_empty(),
            "reported drops for an app that exports cleanly: {:?}",
            result.report.dropped
        );
    }

    /// The module doc always promised "Namespaces produce Namespace and
    /// ResourceQuota"; until this test every quota field was silently dropped.
    #[test]
    fn namespace_quota_fields_export_as_a_resource_quota() {
        let config = parse_config(
            r#"
            [namespace.team-a]
            cpu = "8000m"
            memory = "16Gi"
            gpu = 2
            max_apps = 5
            max_replicas = 20
            "#,
        );
        let result = export_kubernetes(&config).unwrap();
        assert!(
            result.yaml.contains("kind: ResourceQuota"),
            "no ResourceQuota emitted:\n{}",
            result.yaml
        );
        assert!(result.yaml.contains("limits.cpu"), "{}", result.yaml);
        assert!(result.yaml.contains("8000m"), "{}", result.yaml);
        assert!(result.yaml.contains("limits.memory"), "{}", result.yaml);
        assert!(result.yaml.contains("16Gi"), "{}", result.yaml);
        assert!(
            result.yaml.contains("requests.nvidia.com/gpu"),
            "{}",
            result.yaml
        );
        // The quota itself must live inside the namespace it constrains.
        assert!(
            result.yaml.contains("namespace: team-a"),
            "the ResourceQuota must be namespaced:\n{}",
            result.yaml
        );
        // The two caps K8s genuinely cannot express are said, not dropped.
        assert!(
            result
                .report
                .unsupported
                .iter()
                .any(|u| u.contains("max_apps")),
            "{:?}",
            result.report.unsupported
        );
        assert!(
            result
                .report
                .unsupported
                .iter()
                .any(|u| u.contains("max_replicas")),
            "{:?}",
            result.report.unsupported
        );
    }

    #[test]
    fn namespace_without_quota_fields_emits_no_resource_quota() {
        let config = parse_config("[namespace.team-b]\n");
        let result = export_kubernetes(&config).unwrap();
        assert!(result.yaml.contains("kind: Namespace"));
        assert!(
            !result.yaml.contains("kind: ResourceQuota"),
            "an empty namespace must not emit a quota:\n{}",
            result.yaml
        );
    }

    /// The job exporter used to keep only image + command: a job with a memory
    /// limit and database credentials exported as an unlimited pod with no
    /// environment.
    #[test]
    fn job_export_carries_env_and_resources() {
        let config = parse_config(
            r#"
            [job.migrate]
            image = "migrate:v1"
            command = ["npm", "run", "migrate"]
            memory = "128Mi-256Mi"
            cpu = "100m-200m"

            [job.migrate.env]
            DATABASE_URL = "postgres://db/main"
            "#,
        );
        let result = export_kubernetes(&config).unwrap();
        assert!(result.yaml.contains("DATABASE_URL"), "{}", result.yaml);
        assert!(
            result.yaml.contains("postgres://db/main"),
            "{}",
            result.yaml
        );
        // ResourceRange stores memory in bytes; 256Mi = 268435456 (a valid
        // K8s quantity, matching what the app path exports).
        assert!(result.yaml.contains("268435456"), "{}", result.yaml);
        assert!(result.yaml.contains("200m"), "{}", result.yaml);
    }

    /// A DaemonSet has no scale subresource — the old export emitted an HPA
    /// whose scaleTargetRef pointed at a Deployment that doesn't exist.
    #[test]
    fn daemonset_autoscale_reports_instead_of_emitting_an_hpa() {
        let config = parse_config(
            r#"
            [app.monitor]
            image = "monitor:v1"
            replicas = "*"

            [app.monitor.autoscale]
            metric = "cpu"
            target = "70%"
            min = 1
            max = 5
            "#,
        );
        let result = export_kubernetes(&config).unwrap();
        assert!(
            !result.yaml.contains("kind: HorizontalPodAutoscaler"),
            "an HPA cannot target a DaemonSet:\n{}",
            result.yaml
        );
        assert!(
            result
                .report
                .unsupported
                .iter()
                .any(|u| u.contains("scale subresource")),
            "{:?}",
            result.report.unsupported
        );
    }

    /// Config validation accepts both "70%" and the raw fraction "0.7"
    /// (`parse_percentage`); the exporter used to emit `type: Utilization`
    /// with no `averageUtilization` for the fraction form, which the API
    /// server rejects.
    #[test]
    fn autoscale_target_forms_both_export_average_utilization() {
        for target in ["\"70%\"", "\"0.7\""] {
            let config = parse_config(&format!(
                "[app.web]\nimage = \"web:v1\"\nreplicas = 3\n\n\
                 [app.web.autoscale]\nmetric = \"cpu\"\ntarget = {target}\nmin = 2\nmax = 10\n",
            ));
            let result = export_kubernetes(&config).unwrap();
            assert!(
                result.yaml.contains("averageUtilization: 70"),
                "target {target} must export averageUtilization 70:\n{}",
                result.yaml
            );
        }
    }

    #[test]
    fn unparseable_autoscale_target_omits_the_metric_and_reports() {
        let config = parse_config(
            "[app.web]\nimage = \"web:v1\"\nreplicas = 3\n\n\
             [app.web.autoscale]\nmetric = \"cpu\"\ntarget = \"banana\"\nmin = 2\nmax = 10\n",
        );
        let result = export_kubernetes(&config).unwrap();
        assert!(result.yaml.contains("kind: HorizontalPodAutoscaler"));
        assert!(
            !result.yaml.contains("Utilization"),
            "no metric may be emitted for an unconvertible target:\n{}",
            result.yaml
        );
        assert!(
            result
                .report
                .unsupported
                .iter()
                .any(|u| u.contains("could not convert")),
            "{:?}",
            result.report.unsupported
        );
    }
}
