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
    Container, ContainerPort, EnvVar, Namespace, PodSpec, PodTemplateSpec, Service, ServicePort,
    ServiceSpec,
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

    // Container
    let mut container = Container {
        name: name.to_string(),
        image: app.image.clone(),
        ..Container::default()
    };

    if let Some(port) = app.port {
        container.ports = Some(vec![ContainerPort {
            container_port: port as i32,
            ..ContainerPort::default()
        }]);
    }

    // Env vars
    if !app.env.is_empty() {
        container.env = Some(
            app.env
                .iter()
                .map(|(k, v)| {
                    let value = match v {
                        crate::config::types::EnvValue::Plain(s) => s.clone(),
                        crate::config::types::EnvValue::Encrypted(_) => {
                            "ENCRYPTED_VALUE".to_string()
                        }
                    };
                    EnvVar {
                        name: k.clone(),
                        value: Some(value),
                        ..EnvVar::default()
                    }
                })
                .collect(),
        );
    }

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

    // HPA (if autoscale configured)
    if let Some(ref auto) = app.autoscale {
        let hpa = HorizontalPodAutoscaler {
            metadata: ObjectMeta {
                name: Some(name.to_string()),
                ..ObjectMeta::default()
            },
            spec: Some(HorizontalPodAutoscalerSpec {
                scale_target_ref: k8s_openapi::api::autoscaling::v2::CrossVersionObjectReference {
                    api_version: Some("apps/v1".to_string()),
                    kind: "Deployment".to_string(),
                    name: name.to_string(),
                },
                min_replicas: Some(auto.min as i32),
                max_replicas: auto.max as i32,
                metrics: Some(vec![MetricSpec {
                    type_: "Resource".to_string(),
                    resource: Some(ResourceMetricSource {
                        name: auto.metric.clone(),
                        target: MetricTarget {
                            type_: "Utilization".to_string(),
                            average_utilization: parse_target_pct(&auto.target),
                            ..MetricTarget::default()
                        },
                    }),
                    ..MetricSpec::default()
                }]),
                ..HorizontalPodAutoscalerSpec::default()
            }),
            ..HorizontalPodAutoscaler::default()
        };
        docs.push(to_yaml(&hpa)?);
        report
            .resources_created
            .push(format!("HorizontalPodAutoscaler/{name}"));
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
    let container = Container {
        name: name.to_string(),
        image: job.image.clone(),
        command: job.command.clone(),
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

    Ok(())
}

// ---------------------------------------------------------------------------
// Namespace export
// ---------------------------------------------------------------------------

fn export_namespace(
    name: &str,
    _ns: &crate::config::NamespaceSpec,
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

fn parse_target_pct(target: &str) -> Option<i32> {
    target
        .trim()
        .strip_suffix('%')
        .and_then(|s| s.parse::<i32>().ok())
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
}
