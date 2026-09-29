/// App specification — a long-running containerised workload.
///
/// An App replaces seven Kubernetes resource types (Deployment, Service,
/// Ingress, HPA, ConfigMap, Secret, PersistentVolumeClaim) with a single
/// flat definition.
use std::collections::BTreeMap;
use std::fmt;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use super::types::{ConfigFileSpec, EnvValue, Replicas, ResourceRange, VolumeSpec};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppSpec {
    /// OCI image reference. Required for container workloads.
    /// Ignored by ProcessGrill (which runs `command` as an OS process).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image: Option<String>,
    /// Command and arguments to run inside the container.
    /// For ProcessGrill, this is the process that gets spawned.
    /// For runc/Apple Container, this overrides the image entrypoint
    /// (and, like Kubernetes, drops the image's `Cmd`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub command: Vec<String>,
    /// Arguments that replace the image's `Cmd`, keeping its `Entrypoint`
    /// unless `command` is also set (Kubernetes `args`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    /// Working directory inside the container. Defaults to the image's
    /// `WorkingDir`, or `/`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub working_dir: Option<PathBuf>,
    /// User id to run as inside the container, replacing the image's
    /// `User` (Kubernetes `runAsUser`). Container ids live in a user
    /// namespace, so 0 is never root on the host.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_as_user: Option<u32>,
    /// Group id to run as inside the container (Kubernetes `runAsGroup`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub run_as_group: Option<u32>,
    /// Host binary path (Phase 8: process workloads).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exec: Option<PathBuf>,
    /// Inline script content (Phase 8: process workloads).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub script: Option<String>,
    /// Replica count or daemon mode.
    #[serde(default)]
    pub replicas: Replicas,
    /// Container-internal port.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    /// Health check configuration.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub health: Option<HealthSpec>,
    /// Memory request-limit range, e.g. "128Mi-512Mi".
    #[serde(
        default,
        with = "super::types::memory_range",
        skip_serializing_if = "Option::is_none"
    )]
    pub memory: Option<ResourceRange>,
    /// CPU request-limit range, e.g. "100m-500m" or "0.5-2" (bare numbers are cores).
    #[serde(
        default,
        with = "super::types::cpu_range",
        skip_serializing_if = "Option::is_none"
    )]
    pub cpu: Option<ResourceRange>,
    /// Number of GPUs required.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gpu: Option<u32>,
    /// Environment variables (plain or encrypted).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, EnvValue>,
    /// Configuration files injected into the container.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub config_file: Vec<ConfigFileSpec>,
    /// Persistent volumes mounted into the container.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub volumes: Vec<VolumeSpec>,
    /// Init containers run before the main container starts.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub init: Vec<InitContainerSpec>,
    /// Ingress configuration for external traffic.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ingress: Option<IngressSpec>,
    /// Placement constraints.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub placement: Option<PlacementSpec>,
    /// Deploy strategy configuration.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub deploy: Option<DeploySpec>,
    /// Ingress firewall rules.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub firewall: Option<FirewallSpec>,
    /// Egress allowlist.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub egress: Option<EgressSpec>,
    /// Autoscaling configuration.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub autoscale: Option<AutoscaleSpec>,
    /// Prometheus metrics the node should scrape from each instance.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub metrics: Option<MetricsSpec>,
    /// Namespace this app belongs to.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub namespace: Option<String>,
}

impl AppSpec {
    /// Every explicitly declared main or init image; omitted init images inherit the main image.
    pub fn image_references(&self) -> impl Iterator<Item = &str> {
        self.image
            .iter()
            .map(String::as_str)
            .chain(self.init.iter().filter_map(|init| init.image.as_deref()))
    }

    /// Where to scrape this app's Prometheus metrics inside each instance:
    /// `(port, path)`. `None` when the app declares no `metrics` block, or
    /// declares one with no port of its own and no app port to fall back on
    /// (validation rejects that case).
    pub fn metrics_endpoint(&self) -> Option<(u16, &str)> {
        let metrics = self.metrics.as_ref()?;
        let port = metrics.port.or(self.port)?;
        Some((port, metrics.path.as_str()))
    }
}

// ---------------------------------------------------------------------------
// Sub-specs
// ---------------------------------------------------------------------------

/// A Prometheus `/metrics` endpoint every instance serves.
///
/// `metrics = {}` scrapes `/metrics` on the app's own port; both fields
/// can be overridden, e.g. `metrics = { port = 9797 }` for an app with a
/// separate metrics listener. The node running each instance scrapes it
/// directly, so the port needn't be published or routed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MetricsSpec {
    /// Port the metrics endpoint listens on. Defaults to the app's `port`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    /// HTTP path of the endpoint. Defaults to `/metrics`.
    #[serde(default = "default_metrics_path")]
    pub path: String,
}

impl Default for MetricsSpec {
    fn default() -> Self {
        Self {
            port: None,
            path: default_metrics_path(),
        }
    }
}

/// The conventional Prometheus exposition path.
pub fn default_metrics_path() -> String {
    "/metrics".to_string()
}

/// Health check configuration.
///
/// Probes the app's HTTP endpoint to determine health status.
/// Only `path` is required; all other fields have sensible defaults.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HealthSpec {
    /// HTTP path to probe, e.g. "/healthz".
    pub path: String,
    /// Port to probe. Defaults to the app's declared port.
    pub port: Option<u16>,
    /// Protocol for the probe.
    #[serde(default)]
    pub protocol: HealthProtocol,
    /// Probe interval in seconds. Default: 10.
    pub interval: Option<u64>,
    /// Probe timeout in seconds. Default: 5.
    pub timeout: Option<u64>,
    /// Consecutive failures before marking unhealthy. Default: 3.
    pub threshold_unhealthy: Option<u32>,
    /// Consecutive successes before marking healthy. Default: 1.
    pub threshold_healthy: Option<u32>,
    /// Seconds to wait before first probe. Default: 0.
    pub initial_delay: Option<u64>,
}

/// Protocol for health check probes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum HealthProtocol {
    #[default]
    Http,
    Https,
}

impl fmt::Display for HealthProtocol {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HealthProtocol::Http => write!(f, "http"),
            HealthProtocol::Https => write!(f, "https"),
        }
    }
}

impl Serialize for HealthProtocol {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for HealthProtocol {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        match s.as_str() {
            "http" => Ok(HealthProtocol::Http),
            "https" => Ok(HealthProtocol::Https),
            other => Err(serde::de::Error::custom(format!(
                "invalid health protocol {other:?}: expected \"http\" or \"https\""
            ))),
        }
    }
}

/// Init container that runs before the main container.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InitContainerSpec {
    /// Image to use. If omitted, inherits the parent app's image.
    pub image: Option<String>,
    /// Command and arguments.
    #[serde(default)]
    pub command: Vec<String>,
}

/// Ingress configuration for external traffic routing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IngressSpec {
    /// Hostname for routing, e.g. "myapp.com".
    pub host: String,
    /// URL path prefix. Default: "/".
    pub path: Option<String>,
    /// TLS mode: `"cluster"`, `"explicit"`, or `"none"`.
    ///
    /// Automatic ACME (`"auto"`/`"acme"`) is deferred and rejected during
    /// ingress route validation.
    pub tls: Option<String>,
    /// Enable WebSocket upgrade support.
    pub websocket: Option<bool>,
    /// Rate limit: requests per second.
    pub rate_limit_rps: Option<u32>,
    /// Rate limit: burst size.
    pub rate_limit_burst: Option<u32>,
}

/// Placement constraints for scheduling.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlacementSpec {
    /// Hard constraints — app only schedules on nodes matching all labels.
    /// Format: "key=value" strings.
    #[serde(default)]
    pub required: Vec<String>,
    /// Soft constraints — scheduler prefers matching nodes.
    #[serde(default)]
    pub preferred: Vec<String>,
}

/// Deploy strategy configuration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeploySpec {
    /// Strategy: "rolling" or "blue-green".
    pub strategy: Option<String>,
    /// Max instances above target during rollout.
    pub max_surge: Option<u32>,
    /// Max instances below target during rollout.
    pub max_unavailable: Option<u32>,
    /// Connection drain timeout, e.g. "30s".
    pub drain_timeout: Option<String>,
    /// Time to wait for health checks after start, e.g. "60s".
    pub health_timeout: Option<String>,
    /// Revert on health check failure.
    pub auto_rollback: Option<bool>,
}

/// Ingress firewall rules.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FirewallSpec {
    /// App names allowed to connect to this app.
    #[serde(default)]
    pub allow_from: Vec<String>,
}

/// Egress allowlist.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EgressSpec {
    /// External destinations allowed, e.g. "api.stripe.com:443".
    #[serde(default)]
    pub allow: Vec<String>,
    /// Cross-cluster destinations, e.g. "redis.prod-west".
    #[serde(default)]
    pub allow_franchise: Vec<String>,
}

/// Autoscaling configuration.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AutoscaleSpec {
    /// Metric to scale on, e.g. "cpu" or "memory".
    pub metric: String,
    /// Target utilisation, e.g. "70%".
    pub target: String,
    /// Minimum replica count.
    pub min: u32,
    /// Maximum replica count.
    pub max: u32,
    /// Window over which to average the metric (default "5m").
    #[serde(default)]
    pub evaluation_window: Option<String>,
    /// Minimum time between scale events (default "3m").
    #[serde(default)]
    pub cooldown: Option<String>,
    /// Scale-down hysteresis factor (default 0.8).
    /// Only scale down when metric < target * this value.
    #[serde(default)]
    pub scale_down_threshold: Option<f64>,
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_app(toml_str: &str) -> AppSpec {
        // Wrap in [app.test] table for top-level parsing,
        // or parse directly as an AppSpec
        toml::from_str(toml_str).unwrap()
    }

    #[test]
    fn empty_metrics_block_scrapes_the_app_port_at_slash_metrics() {
        let app = parse_app(
            r#"
            image = "myapp:v1"
            port = 8080
            metrics = {}
            "#,
        );
        assert_eq!(app.metrics, Some(MetricsSpec::default()));
        assert_eq!(app.metrics_endpoint(), Some((8080, "/metrics")));
    }

    #[test]
    fn metrics_port_and_path_override_the_defaults() {
        let app = parse_app(
            r#"
            image = "myapp:v1"
            port = 9898
            metrics = { port = 9797, path = "/prom" }
            "#,
        );
        assert_eq!(app.metrics_endpoint(), Some((9797, "/prom")));
    }

    #[test]
    fn an_app_without_a_metrics_block_is_not_scraped() {
        let app = parse_app(
            r#"
            image = "myapp:v1"
            port = 8080
            "#,
        );
        assert_eq!(app.metrics_endpoint(), None);
    }

    #[test]
    fn metrics_block_rejects_unknown_fields() {
        let result: Result<AppSpec, _> = toml::from_str(
            r#"
            image = "myapp:v1"
            metrics = { port = 9797, interval = 5 }
            "#,
        );
        assert!(result.is_err());
    }

    #[test]
    fn parse_minimal_app() {
        let app = parse_app(r#"image = "myapp:v1""#);
        assert_eq!(app.image.as_deref(), Some("myapp:v1"));
        assert_eq!(app.replicas, Replicas::default());
        assert!(app.port.is_none());
        assert!(app.health.is_none());
        assert!(app.memory.is_none());
        assert!(app.cpu.is_none());
        assert!(app.env.is_empty());
        assert!(app.config_file.is_empty());
        assert!(app.init.is_empty());
    }

    #[test]
    fn parse_app_with_all_fields() {
        let toml_str = r#"
            image = "myapp:v1.4.2"
            replicas = 3
            port = 8080
            memory = "128Mi-512Mi"
            cpu = "100m-500m"
            gpu = 1
            namespace = "backend"

            [health]
            path = "/healthz"
            port = 8081
            protocol = "https"
            interval = 15
            timeout = 3
            threshold_unhealthy = 5
            threshold_healthy = 2
            initial_delay = 10

            [env]
            NODE_ENV = "production"
            SECRET = "ENC[AGE:abc123]"

            [[config_file]]
            path = "/etc/app/config.yaml"
            content = "key: value"

            [[volumes]]
            path = "/data"
            size = "10Gi"

            [[init]]
            image = "migration:v1"
            command = ["npm", "run", "migrate"]

            [ingress]
            host = "myapp.com"
            tls = "auto"

            [placement]
            required = ["region=us-east"]
            preferred = ["ssd=true"]

            [deploy]
            strategy = "rolling"
            max_surge = 1
            max_unavailable = 1
            auto_rollback = true

            [firewall]
            allow_from = ["api", "worker"]

            [egress]
            allow = ["api.stripe.com:443"]

            [autoscale]
            metric = "cpu"
            target = "70%"
            min = 2
            max = 10
        "#;
        let app = parse_app(toml_str);
        assert_eq!(app.image.as_deref(), Some("myapp:v1.4.2"));
        assert_eq!(app.replicas, Replicas::Fixed(3));
        assert_eq!(app.port, Some(8080));
        assert_eq!(app.gpu, Some(1));
        assert_eq!(app.namespace.as_deref(), Some("backend"));

        // Health
        let h = app.health.as_ref().unwrap();
        assert_eq!(h.path, "/healthz");
        assert_eq!(h.port, Some(8081));
        assert_eq!(h.protocol, HealthProtocol::Https);
        assert_eq!(h.interval, Some(15));
        assert_eq!(h.timeout, Some(3));
        assert_eq!(h.threshold_unhealthy, Some(5));
        assert_eq!(h.threshold_healthy, Some(2));
        assert_eq!(h.initial_delay, Some(10));

        // Env
        assert_eq!(app.env.len(), 2);
        assert_eq!(
            app.env.get("NODE_ENV"),
            Some(&EnvValue::Plain("production".to_string()))
        );
        assert!(app.env.get("SECRET").unwrap().is_encrypted());

        // Config file
        assert_eq!(app.config_file.len(), 1);
        assert_eq!(app.config_file[0].content.as_deref(), Some("key: value"));

        // Volumes
        assert_eq!(app.volumes.len(), 1);
        assert_eq!(app.volumes[0].size.as_deref(), Some("10Gi"));

        // Init containers
        assert_eq!(app.init.len(), 1);
        assert_eq!(app.init[0].image.as_deref(), Some("migration:v1"));

        // Ingress
        assert_eq!(app.ingress.as_ref().unwrap().host, "myapp.com");

        // Placement
        assert_eq!(
            app.placement.as_ref().unwrap().required,
            vec!["region=us-east"]
        );

        // Deploy
        assert_eq!(app.deploy.as_ref().unwrap().auto_rollback, Some(true));

        // Firewall
        assert_eq!(
            app.firewall.as_ref().unwrap().allow_from,
            vec!["api", "worker"]
        );

        // Egress
        assert_eq!(
            app.egress.as_ref().unwrap().allow,
            vec!["api.stripe.com:443"]
        );

        // Autoscale
        let auto = app.autoscale.as_ref().unwrap();
        assert_eq!(auto.metric, "cpu");
        assert_eq!(auto.min, 2);
        assert_eq!(auto.max, 10);
    }

    #[test]
    fn parse_app_with_daemon_replicas() {
        let app = parse_app(
            r#"
            image = "monitoring:v1"
            replicas = "*"
        "#,
        );
        assert_eq!(app.replicas, Replicas::DaemonSet);
    }

    #[test]
    fn parse_app_with_resource_ranges() {
        let app = parse_app(
            r#"
            image = "myapp:v1"
            cpu = "100m-500m"
            memory = "128Mi-512Mi"
        "#,
        );
        let cpu = app.cpu.unwrap();
        assert_eq!(cpu.request, 100);
        assert_eq!(cpu.limit, 500);
        let mem = app.memory.unwrap();
        assert_eq!(mem.request, 128 * 1024 * 1024);
        assert_eq!(mem.limit, 512 * 1024 * 1024);
    }

    #[test]
    fn parse_app_with_env_mixed() {
        let app = parse_app(
            r#"
            image = "myapp:v1"

            [env]
            PLAIN = "hello"
            ENCRYPTED = "ENC[AGE:secret]"
        "#,
        );
        assert_eq!(
            app.env.get("PLAIN"),
            Some(&EnvValue::Plain("hello".to_string()))
        );
        assert!(app.env.get("ENCRYPTED").unwrap().is_encrypted());
    }

    #[test]
    fn parse_app_with_config_file_inline() {
        let app = parse_app(
            r#"
            image = "nginx:latest"

            [[config_file]]
            path = "/etc/nginx/nginx.conf"
            content = "worker_processes auto;"
        "#,
        );
        assert_eq!(app.config_file.len(), 1);
        assert_eq!(
            app.config_file[0].content.as_deref(),
            Some("worker_processes auto;")
        );
        assert!(app.config_file[0].source.is_none());
    }

    #[test]
    fn parse_app_with_config_file_source() {
        let app = parse_app(
            r#"
            image = "myapp:v1"

            [[config_file]]
            path = "/etc/app/config.yaml"
            source = "configs/production.yaml"
        "#,
        );
        assert_eq!(app.config_file.len(), 1);
        assert!(app.config_file[0].content.is_none());
        assert_eq!(
            app.config_file[0].source.as_deref(),
            Some("configs/production.yaml")
        );
    }

    #[test]
    fn parse_app_with_init_containers() {
        let app = parse_app(
            r#"
            image = "myapp:v1"

            [[init]]
            image = "migration:v1"
            command = ["npm", "run", "migrate"]

            [[init]]
            command = ["echo", "ready"]
        "#,
        );
        assert_eq!(app.init.len(), 2);
        assert_eq!(app.init[0].image.as_deref(), Some("migration:v1"));
        assert!(app.init[1].image.is_none());
    }

    #[test]
    fn parse_app_with_health_check() {
        let app = parse_app(
            r#"
            image = "myapp:v1"
            port = 8080

            [health]
            path = "/healthz"
            port = 8081
            protocol = "https"
            interval = 15
            timeout = 3
            threshold_unhealthy = 5
            threshold_healthy = 2
            initial_delay = 10
        "#,
        );
        let h = app.health.unwrap();
        assert_eq!(h.path, "/healthz");
        assert_eq!(h.port, Some(8081));
        assert_eq!(h.protocol, HealthProtocol::Https);
        assert_eq!(h.interval, Some(15));
        assert_eq!(h.timeout, Some(3));
        assert_eq!(h.threshold_unhealthy, Some(5));
        assert_eq!(h.threshold_healthy, Some(2));
        assert_eq!(h.initial_delay, Some(10));
    }

    #[test]
    fn parse_app_with_health_check_minimal() {
        let app = parse_app(
            r#"
            image = "myapp:v1"
            port = 8080

            [health]
            path = "/healthz"
        "#,
        );
        let h = app.health.unwrap();
        assert_eq!(h.path, "/healthz");
        assert_eq!(h.port, None);
        assert_eq!(h.protocol, HealthProtocol::Http);
        assert_eq!(h.interval, None);
        assert_eq!(h.timeout, None);
        assert_eq!(h.threshold_unhealthy, None);
        assert_eq!(h.threshold_healthy, None);
        assert_eq!(h.initial_delay, None);
    }

    #[test]
    fn parse_app_with_ingress() {
        let app = parse_app(
            r#"
            image = "myapp:v1"
            port = 8080

            [ingress]
            host = "myapp.com"
            tls = "acme"
            websocket = true
        "#,
        );
        let ing = app.ingress.unwrap();
        assert_eq!(ing.host, "myapp.com");
        assert_eq!(ing.tls.as_deref(), Some("acme"));
        assert_eq!(ing.websocket, Some(true));
    }

    #[test]
    fn parse_app_with_placement() {
        let app = parse_app(
            r#"
            image = "myapp:v1"

            [placement]
            required = ["region=us-east", "zone=a"]
            preferred = ["ssd=true"]
        "#,
        );
        let p = app.placement.unwrap();
        assert_eq!(p.required, vec!["region=us-east", "zone=a"]);
        assert_eq!(p.preferred, vec!["ssd=true"]);
    }

    #[test]
    fn parse_app_with_deploy() {
        let app = parse_app(
            r#"
            image = "myapp:v1"

            [deploy]
            strategy = "rolling"
            max_surge = 1
            max_unavailable = 1
            drain_timeout = "30s"
            health_timeout = "60s"
            auto_rollback = true
        "#,
        );
        let d = app.deploy.unwrap();
        assert_eq!(d.strategy.as_deref(), Some("rolling"));
        assert_eq!(d.max_surge, Some(1));
        assert_eq!(d.auto_rollback, Some(true));
    }

    #[test]
    fn parse_app_with_volume_hostpath() {
        let app = parse_app(
            r#"
            image = "redis:7-alpine"
            port = 6379

            [[volumes]]
            source = "/host/data"
            path = "/data"
        "#,
        );
        assert_eq!(app.volumes.len(), 1);
        assert_eq!(app.volumes[0].path, PathBuf::from("/data"));
        assert_eq!(app.volumes[0].source, Some(PathBuf::from("/host/data")));
        assert!(app.volumes[0].size.is_none());
    }

    #[test]
    fn parse_app_with_volume_managed() {
        let app = parse_app(
            r#"
            image = "redis:7-alpine"
            port = 6379

            [[volumes]]
            path = "/data"
            size = "10Gi"
        "#,
        );
        assert_eq!(app.volumes.len(), 1);
        assert_eq!(app.volumes[0].path, PathBuf::from("/data"));
        assert!(app.volumes[0].source.is_none());
        assert_eq!(app.volumes[0].size.as_deref(), Some("10Gi"));
    }

    #[test]
    fn parse_app_with_multiple_volumes() {
        let app = parse_app(
            r#"
            image = "myapp:v1"

            [[volumes]]
            source = "/host/logs"
            path = "/var/log/app"

            [[volumes]]
            path = "/data"
            size = "5Gi"
        "#,
        );
        assert_eq!(app.volumes.len(), 2);
        assert_eq!(app.volumes[0].source, Some(PathBuf::from("/host/logs")));
        assert_eq!(app.volumes[1].path, PathBuf::from("/data"));
    }
}
