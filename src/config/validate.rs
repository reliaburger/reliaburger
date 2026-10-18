use super::Config;
/// Validation for parsed configuration.
///
/// Validation runs as a separate pass after TOML parsing succeeds.
/// This separation gives TOML errors line numbers and domain errors
/// meaningful context.
use super::error::ConfigError;
use super::node::NodeConfig;
use super::types::{parse_byte_size, parse_cpu_millicores};

/// Whether a workload or namespace is a non-empty lowercase DNS label.
pub(crate) fn valid_workload_label(value: &str) -> bool {
    let bytes = value.as_bytes();
    let edge = |byte: u8| byte.is_ascii_lowercase() || byte.is_ascii_digit();
    !bytes.is_empty()
        && bytes.len() <= 63
        && edge(bytes[0])
        && edge(bytes[bytes.len() - 1])
        && bytes.iter().all(|byte| edge(*byte) || *byte == b'-')
}

fn validate_label(value: &str, context: &str, field: &str) -> Result<(), ConfigError> {
    if valid_workload_label(value) {
        return Ok(());
    }
    Err(ConfigError::Validation {
        field: field.into(),
        context: context.into(),
        reason: format!(
            "{value:?} must be a lowercase DNS label of 1–63 bytes, with letters or digits at both ends and only letters, digits or hyphens within"
        ),
    })
}

impl Config {
    /// Validate the parsed configuration.
    ///
    /// Returns the first error found. Call after `from_str` or `from_file`.
    pub fn validate(&self) -> Result<(), ConfigError> {
        self.validate_against(&[])
    }

    /// Check all local semantics before a request reaches live admission.
    /// Namespace existence is resolved from authoritative cluster state later.
    pub fn validate_intrinsic(&self) -> Result<(), ConfigError> {
        self.validate_with_namespace_context(None)
    }

    /// Validate identity labels and refuse app/job runtime identity collisions.
    pub(crate) fn validate_workload_names(&self) -> Result<(), ConfigError> {
        for name in self.namespace.keys() {
            validate_label(name, "namespace declaration", "name")?;
        }
        for (name, job) in &self.job {
            validate_label(name, "job", "name")?;
            validate_label(
                job.namespace.as_deref().unwrap_or("default"),
                name,
                "namespace",
            )?;
        }
        for (name, app) in &self.app {
            validate_label(name, "app", "name")?;
            let namespace = app.namespace.as_deref().unwrap_or("default");
            validate_label(namespace, name, "namespace")?;
            if let Some(job) = self.job.get(name)
                && job.namespace.as_deref().unwrap_or("default") == namespace
            {
                return Err(ConfigError::Validation {
                    field: format!("job.{name}"),
                    context: namespace.into(),
                    reason: "apps and jobs must use distinct names within a namespace".into(),
                });
            }
        }
        Ok(())
    }

    /// A prerequisite must gate an app in this same submission and namespace.
    fn validate_prerequisites(&self) -> Result<(), ConfigError> {
        for (name, job) in &self.job {
            for target in &job.run_before {
                let app = target
                    .strip_prefix("app.")
                    .and_then(|name| self.app.get(name));
                if job.schedule.is_some()
                    || !app.is_some_and(|app| {
                        app.namespace.as_deref().unwrap_or("default")
                            == job.namespace.as_deref().unwrap_or("default")
                    })
                {
                    return Err(ConfigError::Validation {
                        field: "run_before".into(), context: name.clone(),
                        reason: "prerequisites must name app.<name> in the same apply and namespace, and cannot have a schedule".into(),
                    });
                }
            }
        }
        Ok(())
    }

    /// Validate this config, treating `known_namespaces` (from committed
    /// desired state) as also declared. Permissions and builds may target
    /// a namespace created by an earlier apply, not just one in this file.
    pub fn validate_against(&self, known_namespaces: &[String]) -> Result<(), ConfigError> {
        let mut declared: Vec<String> = self.namespace.keys().cloned().collect();
        declared.extend(known_namespaces.iter().cloned());
        self.validate_with_namespace_context(Some(&declared))
    }

    /// Things in a valid config that will probably go wrong on a node, one
    /// line each. `relish lint` and `relish apply` print them and carry on.
    ///
    /// Today that's a host app's image-less init container whose command
    /// starts with a bare name such as `echo` (#1044). It runs on the host
    /// (#935), and `[process_workloads] allowed_binaries` names binaries by
    /// full path, so the node or the scheduler refuses it unless that bare
    /// name is allowlisted word for word. That's possible, so this warns
    /// rather than refuses.
    pub fn warnings(&self) -> Vec<String> {
        let mut warnings = Vec::new();
        for (name, app) in &self.app {
            let namespace = app.namespace.as_deref().unwrap_or("default");
            for binary in crate::meat::filter::host_init_binaries(app) {
                let binary = binary.to_string_lossy();
                if binary.contains('/') {
                    continue;
                }
                warnings.push(format!(
                    "app {namespace}/{name}: init container command {binary:?} is a bare name, \
                     but it runs on the host, where allowed_binaries names binaries by full \
                     path; use the full path (`command -v {binary}` on the node prints it)"
                ));
            }
        }
        warnings
    }

    fn validate_with_namespace_context(
        &self,
        namespaces: Option<&[String]>,
    ) -> Result<(), ConfigError> {
        self.validate_workload_names()?;
        for (name, app) in &self.app {
            validate_app(name, app)?;
        }
        for (name, job) in &self.job {
            validate_job(name, job)?;
        }
        self.validate_prerequisites()?;
        for (name, ns) in &self.namespace {
            validate_namespace(name, ns)?;
        }
        for (name, permission) in &self.permission {
            validate_permission(name, permission, namespaces)?;
        }
        for (name, build) in &self.build {
            validate_build(name, build, namespaces)?;
        }
        Ok(())
    }
}

/// Actions a `[[permission]]` may grant. Mirrors `permission.rs`'s doc.
const KNOWN_PERMISSION_ACTIONS: &[&str] = &[
    "deploy",
    "scale",
    "logs",
    "metrics",
    "exec",
    "host-exec",
    "fault",
    "admin",
    "secret-read",
    "secret-write",
];

fn validate_namespace(name: &str, ns: &super::namespace::NamespaceSpec) -> Result<(), ConfigError> {
    // Resource budgets must parse as non-negative resource values. A
    // negative or overflowing CPU/memory budget is a typo, not a quota.
    if let Some(cpu) = &ns.cpu {
        parse_cpu_millicores(cpu).map_err(|_| ConfigError::Validation {
            field: "cpu".to_string(),
            context: format!("namespace {name:?}"),
            reason: format!("invalid resource value {cpu:?}"),
        })?;
    }
    if let Some(memory) = &ns.memory {
        parse_byte_size(memory).map_err(|_| ConfigError::Validation {
            field: "memory".to_string(),
            context: format!("namespace {name:?}"),
            reason: format!("invalid resource value {memory:?}"),
        })?;
    }
    // A zero cap means "admit nothing", which is never what an operator
    // wants and almost always a mistake. Omit the field for "no limit".
    for (field, value) in [
        ("max_apps", ns.max_apps),
        ("max_replicas", ns.max_replicas),
        ("gpu", ns.gpu),
    ] {
        if value == Some(0) {
            return Err(ConfigError::Validation {
                field: field.to_string(),
                context: format!("namespace {name:?}"),
                reason: format!("{field} must be positive; omit it for no limit"),
            });
        }
    }
    Ok(())
}

fn validate_permission(
    name: &str,
    perm: &super::permission::PermissionSpec,
    known_namespaces: Option<&[String]>,
) -> Result<(), ConfigError> {
    for action in &perm.actions {
        if !KNOWN_PERMISSION_ACTIONS.contains(&action.as_str()) {
            return Err(ConfigError::Validation {
                field: "actions".to_string(),
                context: format!("permission {name:?}"),
                reason: format!(
                    "unknown action {action:?}; valid actions are {}",
                    KNOWN_PERMISSION_ACTIONS.join(", ")
                ),
            });
        }
    }
    // A permission scoped to a namespace that doesn't exist grants nothing
    // and hides a typo. `default` always exists implicitly.
    if let Some(namespaces) = &perm.namespaces {
        for ns in namespaces {
            validate_label(ns, name, "namespaces")?;
            if ns != "default"
                && known_namespaces.is_some_and(|known| !known.iter().any(|n| n == ns))
            {
                return Err(ConfigError::Validation {
                    field: "namespaces".to_string(),
                    context: format!("permission {name:?}"),
                    reason: format!("references unknown namespace {ns:?}"),
                });
            }
        }
    }
    Ok(())
}

fn validate_build(
    name: &str,
    build: &super::build::BuildSpec,
    known_namespaces: Option<&[String]>,
) -> Result<(), ConfigError> {
    // Destination syntax and context existence.
    super::build::validate_build_namespace(name, build)?;
    // A build's namespace must exist. `default` is implicit.
    if let Some(ns) = &build.namespace {
        validate_label(ns, name, "namespace")?;
    }
    if let Some(ns) = &build.namespace
        && ns != "default"
        && known_namespaces.is_some_and(|known| !known.iter().any(|n| n == ns))
    {
        return Err(ConfigError::Validation {
            field: "namespace".to_string(),
            context: format!("build {name:?}"),
            reason: format!("references unknown namespace {ns:?}"),
        });
    }
    Ok(())
}

fn validate_app(name: &str, app: &super::app::AppSpec) -> Result<(), ConfigError> {
    // An app needs at least one of: image, command, exec, or script
    if app.image.is_none() && app.command.is_empty() && app.exec.is_none() && app.script.is_none() {
        return Err(ConfigError::MissingImage {
            name: name.to_string(),
        });
    }
    validate_network_policy(
        &format!("app {name:?}"),
        app.firewall.as_ref(),
        app.egress.as_ref(),
    )?;
    validate_app_ingress(name, app)?;
    validate_app_honoured_fields(name, app)?;

    // Ordinals come from the leader's placements, never from a config file.
    if app.ordinals.is_some() {
        return Err(ConfigError::Validation {
            field: "ordinals".to_string(),
            context: format!("app {name:?}"),
            reason: "ordinals are assigned by the cluster's placements, not set in config"
                .to_string(),
        });
    }

    // exec and image are mutually exclusive
    if app.exec.is_some() && app.image.is_some() {
        return Err(ConfigError::Validation {
            field: "exec".to_string(),
            context: format!("app {name:?}"),
            reason: "exec and image are mutually exclusive".to_string(),
        });
    }

    // script and image are mutually exclusive
    if app.script.is_some() && app.image.is_some() {
        return Err(ConfigError::Validation {
            field: "script".to_string(),
            context: format!("app {name:?}"),
            reason: "script and image are mutually exclusive".to_string(),
        });
    }

    // exec and script are mutually exclusive
    if app.exec.is_some() && app.script.is_some() {
        return Err(ConfigError::Validation {
            field: "exec/script".to_string(),
            context: format!("app {name:?}"),
            reason: "exec and script are mutually exclusive".to_string(),
        });
    }

    // `args` and `working_dir` describe a container's process against its
    // image config; a process workload has no image to resolve them against.
    if (app.exec.is_some() || app.script.is_some())
        && (!app.args.is_empty() || app.working_dir.is_some())
    {
        return Err(ConfigError::Validation {
            field: "args/working_dir".to_string(),
            context: format!("app {name:?}"),
            reason: "args and working_dir apply to image workloads, not exec or script".to_string(),
        });
    }
    if let Some(dir) = &app.working_dir
        && !dir.is_absolute()
    {
        return Err(ConfigError::Validation {
            field: "working_dir".to_string(),
            context: format!("app {name:?}"),
            reason: format!("{:?} must be absolute", dir.display()),
        });
    }

    // Port range
    if let Some(port) = app.port
        && port == 0
    {
        return Err(ConfigError::InvalidPort {
            name: name.to_string(),
            port,
        });
    }

    // Metrics: the scrape needs a port (its own or the app's) and a path.
    if let Some(metrics) = &app.metrics {
        let metrics_error = |reason: String| ConfigError::Validation {
            field: "metrics".to_string(),
            context: format!("app {name:?}"),
            reason,
        };
        match metrics.port.or(app.port) {
            None => {
                return Err(metrics_error(
                    "no port to scrape: set metrics.port or the app's port".to_string(),
                ));
            }
            Some(0) => return Err(metrics_error("port must not be 0".to_string())),
            Some(_) => {}
        }
        if !metrics.path.starts_with('/') || metrics.path.contains(char::is_whitespace) {
            return Err(metrics_error(format!(
                "path {:?} must start with '/' and contain no whitespace",
                metrics.path
            )));
        }
    }

    // Config files: exactly one of content/source
    for cf in &app.config_file {
        match (&cf.content, &cf.source) {
            (Some(_), Some(_)) | (None, None) => {
                return Err(ConfigError::InvalidConfigFile {
                    name: name.to_string(),
                });
            }
            _ => {}
        }
    }

    // Volumes: paths must be absolute
    for vol in &app.volumes {
        if !vol.path.is_absolute() {
            return Err(ConfigError::InvalidVolume {
                name: name.to_string(),
                reason: format!("mount path {:?} must be absolute", vol.path.display()),
            });
        }
        // A managed-volume host path is derived by joining the mount path
        // under the node's volumes directory, so a `..` component escapes that
        // directory — an absolute `/../../etc/cron.d` passes the check above
        // yet lets a deploy create and read-write bind-mount an arbitrary host
        // directory as root. Reject traversal outright.
        if vol
            .path
            .components()
            .any(|c| c == std::path::Component::ParentDir)
        {
            return Err(ConfigError::InvalidVolume {
                name: name.to_string(),
                reason: format!(
                    "mount path {:?} must not contain '..' components",
                    vol.path.display()
                ),
            });
        }
        if let Some(ref source) = vol.source
            && !source.is_absolute()
        {
            return Err(ConfigError::InvalidVolume {
                name: name.to_string(),
                reason: format!("source path {:?} must be absolute", source.display()),
            });
        }
    }

    crate::grill::volume::validate_managed_volume_layout(&app.volumes).map_err(|reason| {
        ConfigError::InvalidVolume {
            name: name.to_string(),
            reason,
        }
    })?;

    // Deploy block: `max_surge = 0` with `max_unavailable = 0` leaves a rolling
    // deploy no legal move in either direction, so reject it here rather than
    // let the rollout wedge (M7). A timeout that doesn't parse is refused
    // too, rather than replaced by the default (#692).
    if let Some(deploy) = &app.deploy
        && let Err(reason) = crate::meat::deploy_types::DeployConfig::parse(deploy)
    {
        return Err(ConfigError::Validation {
            field: "deploy".to_string(),
            context: format!("app {name:?}"),
            reason,
        });
    }

    // Autoscale block: bounds, windows and threshold are validated up front
    // so a bad `[autoscale]` fails the deploy instead of silently clamping
    // at runtime (DEP8). An unsupported metric, or one the app has no request
    // for, is refused here too: it would otherwise never scale.
    if let Some(autoscale) = &app.autoscale
        && let Err(e) =
            crate::meat::autoscaler::AutoscaleConfig::from_spec(autoscale, app.cpu, app.memory)
    {
        return Err(ConfigError::Validation {
            field: "autoscale".to_string(),
            context: format!("app {name:?}"),
            reason: e.to_string(),
        });
    }

    Ok(())
}

/// Refuse an ingress route every node's routing rebuild would skip, so a
/// typo like `tls = "acme"` fails at apply and lint instead of silently
/// serving no route (#700).
fn validate_app_ingress(name: &str, app: &super::app::AppSpec) -> Result<(), ConfigError> {
    let Some(ingress) = &app.ingress else {
        return Ok(());
    };
    crate::wrapper::routing::validate_ingress(ingress).map_err(|reason| ConfigError::Validation {
        field: "ingress".to_string(),
        context: format!("app {name:?}"),
        reason,
    })
}

/// Refuse app fields that would parse and then be ignored or half-honoured,
/// so a valid-looking config can't quietly run something else (#701): a GPU
/// request no node can back, mounts and a user a process workload never
/// applies, and a health check that can't probe or can only fail.
fn validate_app_honoured_fields(name: &str, app: &super::app::AppSpec) -> Result<(), ConfigError> {
    let refuse = |field: &str, reason: String| ConfigError::Validation {
        field: field.to_string(),
        context: format!("app {name:?}"),
        reason,
    };

    // D41: no device passthrough or cluster GPU accounting yet.
    if app.gpu.is_some_and(|gpu| gpu > 0) {
        return Err(refuse(
            "gpu",
            "GPU workloads aren't supported yet (#359)".to_string(),
        ));
    }

    // ProcessGrill runs the host binary as it is: no mounts, no user switch.
    // Placement asks the same question through `host_binary`.
    if crate::meat::filter::host_binary(app).is_some() {
        let ignored = [
            ("volumes", !app.volumes.is_empty()),
            ("config_file", !app.config_file.is_empty()),
            ("run_as_user", app.run_as_user.is_some()),
            ("run_as_group", app.run_as_group.is_some()),
        ];
        if let Some((field, _)) = ignored.iter().find(|(_, set)| *set) {
            return Err(refuse(
                field,
                format!("{field} applies to image workloads; an exec or script app can't use it"),
            ));
        }
    }

    let Some(health) = &app.health else {
        return Ok(());
    };
    if health.port.or(app.port).is_none() {
        return Err(refuse(
            "health",
            "no port to probe: set health.port or the app's port".to_string(),
        ));
    }
    let zero = [
        ("health.interval", health.interval == Some(0)),
        ("health.timeout", health.timeout == Some(0)),
        (
            "health.threshold_unhealthy",
            health.threshold_unhealthy == Some(0),
        ),
        (
            "health.threshold_healthy",
            health.threshold_healthy == Some(0),
        ),
    ];
    if let Some((field, _)) = zero.iter().find(|(_, is_zero)| *is_zero) {
        return Err(refuse(field, format!("{field} must be at least 1")));
    }
    let interval = health
        .interval
        .unwrap_or(crate::bun::health::DEFAULT_HEALTH_INTERVAL_SECS);
    let timeout = health
        .timeout
        .unwrap_or(crate::bun::health::DEFAULT_HEALTH_TIMEOUT_SECS);
    // A probe that may outlast the interval overlaps the next one. Equal is
    // allowed: seconds are whole, so a 1 s interval needs a 1 s timeout.
    if timeout > interval {
        return Err(refuse(
            "health.timeout",
            format!("timeout ({timeout}s) must not be longer than interval ({interval}s)"),
        ));
    }
    Ok(())
}

/// Check the network policy apps and jobs share: every egress entry parses
/// (without resolving names), and every `allow_from` source is `app` or
/// `namespace/app` made of workload labels.
fn validate_network_policy(
    context: &str,
    firewall: Option<&super::app::FirewallSpec>,
    egress: Option<&super::app::EgressSpec>,
) -> Result<(), ConfigError> {
    // Cross-cluster egress (Franchise) doesn't exist: every node would refuse
    // the workload, so refuse it here, at apply and lint (#701).
    if egress.is_some_and(|egress| !egress.allow_franchise.is_empty()) {
        return Err(ConfigError::Validation {
            field: "egress.allow_franchise".to_string(),
            context: context.to_string(),
            reason: "cross-cluster egress isn't implemented".to_string(),
        });
    }
    for entry in egress.map_or(&[][..], |egress| egress.allow.as_slice()) {
        crate::sesame::egress::validate_egress_entry(entry).map_err(|error| {
            ConfigError::Validation {
                field: "egress".to_string(),
                context: context.to_string(),
                reason: error.to_string(),
            }
        })?;
    }
    for source in firewall.map_or(&[][..], |firewall| firewall.allow_from.as_slice()) {
        let valid = match source.split_once('/') {
            Some((namespace, app)) => valid_workload_label(namespace) && valid_workload_label(app),
            None => valid_workload_label(source),
        };
        if !valid {
            return Err(ConfigError::Validation {
                field: "firewall.allow_from".to_string(),
                context: context.to_string(),
                reason: format!(
                    "{source:?} must be an app name or namespace/app, each a lowercase DNS label"
                ),
            });
        }
    }
    Ok(())
}

pub(crate) fn validate_job(name: &str, job: &super::job::JobSpec) -> Result<(), ConfigError> {
    validate_label(name, "job", "name")?;
    validate_network_policy(
        &format!("job {name:?}"),
        job.firewall.as_ref(),
        job.egress.as_ref(),
    )?;
    validate_label(
        job.namespace.as_deref().unwrap_or("default"),
        name,
        "namespace",
    )?;
    if let Some(expression) = &job.schedule {
        crate::meat::cron::CronSchedule::parse(expression).map_err(|error| {
            ConfigError::Validation {
                field: "schedule".to_string(),
                context: format!("job {name:?}"),
                reason: error.to_string(),
            }
        })?;
    }
    // Must have at least one of image, exec, or script
    if job.image.is_none() && job.exec.is_none() && job.script.is_none() {
        return Err(ConfigError::MissingImage {
            name: name.to_string(),
        });
    }

    // exec and image are mutually exclusive
    if job.exec.is_some() && job.image.is_some() {
        return Err(ConfigError::Validation {
            field: "exec".to_string(),
            context: format!("job {name:?}"),
            reason: "exec and image are mutually exclusive".to_string(),
        });
    }

    // script and image are mutually exclusive
    if job.script.is_some() && job.image.is_some() {
        return Err(ConfigError::Validation {
            field: "script".to_string(),
            context: format!("job {name:?}"),
            reason: "script and image are mutually exclusive".to_string(),
        });
    }

    // exec and script are mutually exclusive
    if job.exec.is_some() && job.script.is_some() {
        return Err(ConfigError::Validation {
            field: "exec/script".to_string(),
            context: format!("job {name:?}"),
            reason: "exec and script are mutually exclusive".to_string(),
        });
    }

    job.validate_runtime()
        .map_err(|reason| ConfigError::Validation {
            field: "runtime".into(),
            context: format!("job {name:?}"),
            reason: reason.into(),
        })?;
    validate_job_policy(name, job)
}

/// Check a TOML job's run policy against the same limits a manifest has.
fn validate_job_policy(name: &str, job: &super::job::JobSpec) -> Result<(), ConfigError> {
    let refuse = |field: &str, reason: String| ConfigError::Validation {
        field: field.into(),
        context: format!("job {name:?}"),
        reason,
    };
    if job.overlap.is_some() && job.schedule.is_none() {
        return Err(refuse(
            "overlap",
            "only applies to a job with a schedule".into(),
        ));
    }
    if job.replay_unknown && !job.run_before.is_empty() {
        return Err(refuse(
            "replay_unknown",
            "a run_before hook can't replay an unknown outcome automatically; acknowledge it with relish batch replay".into(),
        ));
    }
    // The definition is where the policy lands, so its limits are the ones to check.
    let tasks = crate::meat::job::JobDefinition::from_spec(job.clone()).tasks;
    tasks.validate().map_err(|error| {
        let field = match error {
            crate::meat::task_array::TaskArraySpecError::AttemptsOutOfRange { .. } => {
                "max_attempts"
            }
            crate::meat::task_array::TaskArraySpecError::TimeoutOutOfRange { .. } => {
                "task_timeout_secs"
            }
            _ => "job",
        };
        refuse(field, error.to_string())
    })
}

impl NodeConfig {
    /// `[security] leaf_lifetime_override_secs` exists to qualify renewal in a
    /// soak run. It must never reach a cluster whose class is production or
    /// undeclared, and it may only shorten both leaf classes it touches.
    fn validate_leaf_lifetime_override(&self) -> Result<(), ConfigError> {
        use crate::sesame::ca::{INGRESS_LEAF_LIFETIME, MIN_LEAF_LIFETIME_OVERRIDE};
        use crate::testkit::safety::ClusterSafetyClass;
        let Some(seconds) = self.security.leaf_lifetime_override_secs else {
            return Ok(());
        };
        let refuse = |reason: String| ConfigError::Validation {
            field: "security.leaf_lifetime_override_secs".to_string(),
            context: "node config".to_string(),
            reason,
        };
        if self.testing.safety_class != ClusterSafetyClass::Development {
            return Err(refuse(
                "is a development-only soak setting; it requires \
                 [testing] safety_class = \"development\""
                    .to_string(),
            ));
        }
        let minimum = MIN_LEAF_LIFETIME_OVERRIDE.as_secs();
        let maximum = INGRESS_LEAF_LIFETIME.as_secs();
        if !(minimum..=maximum).contains(&seconds) {
            return Err(refuse(format!(
                "must be between {minimum} and {maximum} seconds, got {seconds}"
            )));
        }
        Ok(())
    }

    /// Validate the parsed node configuration.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if self.reporting_tree.max_events_per_report
            != crate::reporting::transport::MAX_EVENTS_PER_REPORT
        {
            return Err(ConfigError::Validation {
                field: "reporting_tree.max_events_per_report".into(),
                context: "node config".into(),
                reason: "0.1.0 supports a fixed 100-event admission limit; custom limits are not supported".into(),
            });
        }
        self.images.trust_policy.validate()?;
        // Storage paths must be absolute
        let paths = [
            ("storage.data", &self.storage.data),
            ("storage.images", &self.storage.images),
            ("storage.logs", &self.storage.logs),
            ("storage.metrics", &self.storage.metrics),
            ("storage.volumes", &self.storage.volumes),
        ];
        let allowed_host_paths = self
            .storage
            .allowed_host_paths
            .iter()
            .map(|path| ("storage.allowed_host_paths", path));
        for (field, path) in paths.into_iter().chain(allowed_host_paths) {
            if !path.is_absolute() {
                return Err(ConfigError::NonAbsolutePath {
                    field: field.to_string(),
                    path: path.clone(),
                });
            }
        }

        // Port range validation
        if self.network.port_range.start >= self.network.port_range.end {
            return Err(ConfigError::InvalidPortRange {
                value: format!(
                    "{}-{}",
                    self.network.port_range.start, self.network.port_range.end
                ),
                reason: "start must be less than end".to_string(),
            });
        }

        // Upgrade keys must parse; retention must keep at least one version
        let upgrade_validation = |field: &str, reason: String| ConfigError::Validation {
            field: field.to_string(),
            context: "node config".to_string(),
            reason,
        };
        if let Some(key) = &self.upgrades.external_signing_key {
            crate::upgrade::signing::parse_public_key(key)
                .map_err(|e| upgrade_validation("upgrades.external_signing_key", e.to_string()))?;
        }
        for key in self.upgrades.release_keys_override.iter().flatten() {
            crate::upgrade::signing::parse_public_key(key)
                .map_err(|e| upgrade_validation("upgrades.release_keys_override", e.to_string()))?;
        }
        if self.upgrades.retain_versions == 0 {
            return Err(upgrade_validation(
                "upgrades.retain_versions",
                "must retain at least one version".to_string(),
            ));
        }
        if self.upgrades.max_boot_attempts == 0 {
            // With a zero budget, startup reverts every upgrade on its first
            // boot (`boot_attempts + 1 > 0`), so no upgrade can ever commit.
            return Err(upgrade_validation(
                "upgrades.max_boot_attempts",
                "must allow at least one boot attempt".to_string(),
            ));
        }
        if let Some(dir) = &self.upgrades.binary_dir
            && !dir.is_absolute()
        {
            return Err(ConfigError::NonAbsolutePath {
                field: "upgrades.binary_dir".to_string(),
                path: dir.clone(),
            });
        }

        // A log smaller than a few records would drop each record as soon
        // as the next arrived, which is no audit trail at all (#685).
        if self.storage.audit_max_bytes < crate::bun::audit::MIN_AUDIT_MAX_BYTES {
            return Err(ConfigError::Validation {
                field: "storage.audit_max_bytes".into(),
                context: "node config".into(),
                reason: format!(
                    "must be at least {} bytes",
                    crate::bun::audit::MIN_AUDIT_MAX_BYTES
                ),
            });
        }

        // Retaining zero would delete every snapshot the moment it was
        // taken (or, with an upload destination, the moment it shipped).
        let snapshots = &self.storage.snapshots;
        if snapshots.interval_secs > 0 && snapshots.retain == 0 {
            return Err(ConfigError::Validation {
                field: "storage.snapshots.retain".into(),
                context: "node config".into(),
                reason: "must retain at least one snapshot when interval_secs is set".into(),
            });
        }
        // A zero deadline would abandon every export before it started.
        if snapshots.upload_url.is_some() && snapshots.upload_timeout_secs == 0 {
            return Err(ConfigError::Validation {
                field: "storage.snapshots.upload_timeout_secs".into(),
                context: "node config".into(),
                reason: "must be greater than zero when upload_url is set".into(),
            });
        }

        // A zero deadline would fail every stop before the runtime answered,
        // leaving every workload owned and unstoppable.
        if self.runtime.stop_confirmation_timeout_secs == 0 {
            return Err(ConfigError::Validation {
                field: "runtime.stop_confirmation_timeout_secs".into(),
                context: "node config".into(),
                reason: "must be greater than zero".into(),
            });
        }

        self.validate_leaf_lifetime_override()?;

        // A bad operator CIDR would otherwise surface only as a warning when
        // the firewall reconciles, leaving the operator locked out with no
        // clear cause.
        for value in &self.security.operator_cidrs {
            crate::firewall::rules::parse_cidr(value).map_err(|error| ConfigError::Validation {
                field: "security.operator_cidrs".to_string(),
                context: "node config".to_string(),
                reason: error.to_string(),
            })?;
        }

        self.testing
            .validate()
            .map_err(|error| ConfigError::Validation {
                field: "testing".to_string(),
                context: "node config".to_string(),
                reason: error.to_string(),
            })?;

        // Service discovery needs the eBPF data path. A VIP the DNS
        // responder hands out only routes because the eBPF connect hook
        // rewrites it to a live backend. With `[dns] enabled` but `[ebpf]`
        // off, every resolved VIP is a black hole: the deploy would look
        // fine and silently never route. Reject that up front rather than
        // failing opaquely at connect time (12b.4, non-eBPF fallback).
        if self.dns.enabled && !self.ebpf.enabled {
            return Err(ConfigError::Validation {
                field: "dns.enabled".to_string(),
                context: "node config".to_string(),
                reason: "service discovery needs the eBPF data path; \
                         set [ebpf] enabled = true or disable [dns]"
                    .to_string(),
            });
        }

        // Reserved resources must parse
        parse_cpu_millicores(&self.resources.reserved_cpu).map_err(|_| {
            ConfigError::Validation {
                field: "resources.reserved_cpu".to_string(),
                context: "node config".to_string(),
                reason: format!("invalid resource value {:?}", self.resources.reserved_cpu),
            }
        })?;
        parse_byte_size(&self.resources.reserved_memory).map_err(|_| ConfigError::Validation {
            field: "resources.reserved_memory".to_string(),
            context: "node config".to_string(),
            reason: format!(
                "invalid resource value {:?}",
                self.resources.reserved_memory
            ),
        })?;

        // The registry storage cap must parse (M23): the binary reads it with
        // `unwrap_or(0)`, and 0 means "unlimited", so a typo like "10GBB" would
        // silently disable the cap. Fail here instead. `0` (explicit unlimited)
        // parses fine.
        parse_byte_size(&self.images.max_storage).map_err(|_| ConfigError::Validation {
            field: "images.max_storage".to_string(),
            context: "node config".to_string(),
            reason: format!("invalid resource value {:?}", self.images.max_storage),
        })?;

        // Periodic intervals feed `tokio::time::interval`, which panics on a
        // zero period (OBS4). The runtime also clamps these defensively, but
        // reject zero here so the operator gets a clear error rather than a
        // silent 1-second fallback.
        for (field, value) in [
            (
                "metrics.collection_interval_secs",
                self.metrics.collection_interval_secs,
            ),
            (
                "metrics.rollup_interval_secs",
                self.metrics.rollup_interval_secs,
            ),
            (
                "metrics.app_scrape_interval_secs",
                self.metrics.app_scrape_interval_secs,
            ),
            ("logs.export_interval_secs", self.logs.export_interval_secs),
        ] {
            if value == 0 {
                return Err(ConfigError::Validation {
                    field: field.to_string(),
                    context: "node config".to_string(),
                    reason: "must be greater than zero".to_string(),
                });
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::app::AppSpec;
    use crate::config::types::ConfigFileSpec;
    use std::path::PathBuf;

    fn config_with_app(name: &str, app: AppSpec) -> Config {
        let mut config = Config::default();
        config.app.insert(name.to_string(), app);
        config
    }

    fn minimal_app() -> AppSpec {
        toml::from_str(r#"image = "test:v1""#).unwrap()
    }

    fn app_with_metrics(extra: &str) -> AppSpec {
        toml::from_str(&format!("image = \"test:v1\"\n{extra}")).unwrap()
    }

    /// #1044: a host app's image-less init runs its command on the host,
    /// where the allowlist names binaries by their full path.
    fn host_app_with_init(init: &str) -> Config {
        config_with_app(
            "worker",
            toml::from_str(&format!(
                "exec = \"/usr/bin/python3\"\ncommand = [\"worker.py\"]\n[[init]]\n{init}"
            ))
            .unwrap(),
        )
    }

    #[test]
    fn a_host_apps_init_with_a_bare_command_name_is_warned() {
        let config = host_app_with_init("command = [\"echo\", \"hi\"]");
        config.validate().unwrap();

        let warnings = config.warnings();

        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(
            warnings[0].contains("default/worker")
                && warnings[0].contains("\"echo\"")
                && warnings[0].contains("full path"),
            "{}",
            warnings[0]
        );
    }

    #[test]
    fn a_host_apps_init_with_a_full_path_is_not_warned() {
        let config = host_app_with_init("command = [\"/bin/echo\", \"hi\"]");

        assert!(config.warnings().is_empty(), "{:?}", config.warnings());
    }

    #[test]
    fn an_init_with_its_own_image_is_never_warned() {
        let host = host_app_with_init("image = \"busybox:1\"\ncommand = [\"echo\", \"hi\"]");
        let container = config_with_app(
            "web",
            toml::from_str("image = \"web:v1\"\n[[init]]\ncommand = [\"echo\", \"hi\"]").unwrap(),
        );

        assert!(host.warnings().is_empty(), "{:?}", host.warnings());
        assert!(
            container.warnings().is_empty(),
            "{:?}",
            container.warnings()
        );
    }

    #[test]
    fn a_config_cannot_choose_instance_ordinals() {
        let config = config_with_app("web", app_with_metrics("replicas = 2\nordinals = [4, 7]"));
        let err = config.validate().unwrap_err();
        assert!(
            matches!(err, ConfigError::Validation { ref field, .. } if field == "ordinals"),
            "{err:?}"
        );
    }

    #[test]
    fn metrics_on_the_app_port_is_valid() {
        let config = config_with_app("web", app_with_metrics("port = 8080\nmetrics = {}"));
        config.validate().unwrap();
    }

    #[test]
    fn metrics_without_any_port_is_rejected() {
        let config = config_with_app("web", app_with_metrics("metrics = {}"));
        let err = config.validate().unwrap_err();
        assert!(
            matches!(err, ConfigError::Validation { ref field, ref reason, .. }
                if field == "metrics" && reason.contains("no port")),
            "{err:?}"
        );
    }

    #[test]
    fn metrics_port_zero_is_rejected() {
        let config = config_with_app("web", app_with_metrics("metrics = { port = 0 }"));
        assert!(config.validate().is_err());
    }

    #[test]
    fn metrics_path_must_be_absolute() {
        let config = config_with_app(
            "web",
            app_with_metrics("metrics = { port = 9797, path = \"metrics\" }"),
        );
        let err = config.validate().unwrap_err();
        assert!(
            matches!(err, ConfigError::Validation { ref reason, .. }
                if reason.contains("start with '/'")),
            "{err:?}"
        );
    }

    #[test]
    fn node_config_defaults_are_valid() {
        // H1: the whole-config validator now runs at startup, so the shipped
        // defaults must pass it (absolute paths, sane ranges, non-zero
        // intervals, dns-off-by-default so the dns/ebpf rail is inert).
        crate::config::NodeConfig::default().validate().unwrap();
    }

    #[test]
    fn node_config_rejects_a_zero_metric_interval() {
        let mut node = crate::config::NodeConfig::default();
        node.metrics.collection_interval_secs = 0;
        let err = node.validate().unwrap_err();
        assert!(
            matches!(err, ConfigError::Validation { ref field, .. }
                if field == "metrics.collection_interval_secs"),
            "expected a collection-interval validation error, got {err:?}"
        );
    }

    #[test]
    fn node_config_rejects_a_zero_stop_confirmation_timeout() {
        let mut node = crate::config::NodeConfig::default();
        node.runtime.stop_confirmation_timeout_secs = 0;
        let err = node.validate().unwrap_err();
        assert!(
            matches!(err, ConfigError::Validation { ref field, .. }
                if field == "runtime.stop_confirmation_timeout_secs"),
            "expected a stop-confirmation validation error, got {err:?}"
        );
    }

    #[test]
    fn node_config_rejects_an_unparseable_max_storage() {
        let mut node = crate::config::NodeConfig::default();
        node.images.max_storage = "10GBB".to_string();
        let err = node.validate().unwrap_err();
        assert!(
            matches!(err, ConfigError::Validation { ref field, .. } if field == "images.max_storage"),
            "expected a max_storage validation error, got {err:?}"
        );
        // Explicit "0" (unlimited) still validates.
        node.images.max_storage = "0".to_string();
        node.validate().unwrap();
    }

    #[test]
    fn node_config_rejects_dns_without_ebpf() {
        let mut node = crate::config::NodeConfig::default();
        node.dns.enabled = true;
        node.ebpf.enabled = false;
        let err = node.validate().unwrap_err();
        assert!(
            matches!(err, ConfigError::Validation { ref field, .. } if field == "dns.enabled"),
            "expected a dns/ebpf validation error, got {err:?}"
        );
    }

    #[test]
    fn app_and_job_cannot_share_a_name_in_one_namespace() {
        let mut config =
            Config::parse("[app.web]\nimage = 'web:v1'\n[job.web]\nimage = 'job:v1'\n").unwrap();
        assert!(config.validate().is_err());
        assert!(config.validate_against(&[]).is_err());
        config.job.get_mut("web").unwrap().namespace = Some("batch".into());
        config.validate().unwrap();
        config.validate_against(&[]).unwrap();
    }

    #[test]
    fn workload_labels_reject_unsafe_or_ambiguous_names() {
        let oversized = "a".repeat(64);
        for label in [
            "",
            ".",
            "..",
            "../outside",
            "a/b",
            "a\\b",
            "a__b",
            "Bad",
            "a b",
            "-a",
            "a-",
            "é",
            oversized.as_str(),
        ] {
            for target in [
                "app",
                "job",
                "app namespace",
                "job namespace",
                "declared namespace",
            ] {
                let mut config = Config::default();
                match target {
                    "app" => {
                        config.app.insert(label.into(), minimal_app());
                    }
                    "job" => {
                        config
                            .job
                            .insert(label.into(), toml::from_str("image = 'job:v1'").unwrap());
                    }
                    "app namespace" => {
                        let mut spec = minimal_app();
                        spec.namespace = Some(label.into());
                        config.app.insert("web".into(), spec);
                    }
                    "job namespace" => {
                        let mut spec: super::super::job::JobSpec =
                            toml::from_str("image = 'job:v1'").unwrap();
                        spec.namespace = Some(label.into());
                        config.job.insert("batch".into(), spec);
                    }
                    _ => {
                        config
                            .namespace
                            .insert(label.into(), toml::from_str("").unwrap());
                    }
                }
                assert!(config.validate().is_err(), "accepted {target}: {label:?}");
                assert!(
                    config.validate_against(&[]).is_err(),
                    "accepted {target}: {label:?}"
                );
            }
        }
    }

    #[test]
    fn workload_labels_accept_dns_label_boundaries() {
        for label in [
            "a".to_owned(),
            "0".to_owned(),
            "web-g17".to_owned(),
            "a".repeat(63),
        ] {
            let mut config = config_with_app(&label, minimal_app());
            config.app.get_mut(&label).unwrap().namespace = Some(label.clone());
            config
                .namespace
                .insert(label.clone(), toml::from_str("").unwrap());
            config.validate().unwrap();
            config.validate_against(&[]).unwrap();
        }
    }

    #[test]
    fn validate_app_missing_image_rejected() {
        let app: AppSpec = toml::from_str("replicas = 1").unwrap();
        let config = config_with_app("test", app);
        let err = config.validate().unwrap_err();
        assert!(
            matches!(err, ConfigError::MissingImage { ref name } if name == "test"),
            "expected MissingImage, got {err:?}"
        );
    }

    #[test]
    fn validate_config_file_both_content_and_source_rejected() {
        let mut app = minimal_app();
        app.config_file.push(ConfigFileSpec {
            path: PathBuf::from("/etc/app.conf"),
            content: Some("data".to_string()),
            source: Some("configs/app.conf".to_string()),
        });
        let config = config_with_app("test", app);
        assert!(matches!(
            config.validate(),
            Err(ConfigError::InvalidConfigFile { .. })
        ));
    }

    #[test]
    fn validate_config_file_neither_content_nor_source_rejected() {
        let mut app = minimal_app();
        app.config_file.push(ConfigFileSpec {
            path: PathBuf::from("/etc/app.conf"),
            content: None,
            source: None,
        });
        let config = config_with_app("test", app);
        assert!(matches!(
            config.validate(),
            Err(ConfigError::InvalidConfigFile { .. })
        ));
    }

    #[test]
    fn validate_port_zero_rejected() {
        let mut app = minimal_app();
        app.port = Some(0);
        let config = config_with_app("test", app);
        assert!(matches!(
            config.validate(),
            Err(ConfigError::InvalidPort { .. })
        ));
    }

    #[test]
    fn validate_valid_app_passes() {
        let config = config_with_app("test", minimal_app());
        config.validate().unwrap();
    }

    #[test]
    fn image_process_fields_parse_and_validate() {
        let app: AppSpec = toml::from_str(
            r#"
            image = "ghcr.io/stefanprodan/podinfo:6.15.0"
            args = ["--port=9898"]
            working_dir = "/home/app"
            run_as_user = 100
            run_as_group = 101
            "#,
        )
        .unwrap();
        assert_eq!(app.args, ["--port=9898"]);
        assert_eq!(app.run_as_user, Some(100));
        config_with_app("podinfo", app).validate().unwrap();
    }

    #[test]
    fn relative_working_dir_rejected() {
        let mut app = minimal_app();
        app.working_dir = Some(PathBuf::from("home/app"));
        let err = config_with_app("test", app).validate().unwrap_err();
        assert!(
            matches!(err, ConfigError::Validation { ref field, .. } if field == "working_dir"),
            "{err:?}"
        );
    }

    #[test]
    fn args_on_a_process_workload_rejected() {
        let app: AppSpec = toml::from_str(
            r#"
            script = "echo hi"
            args = ["--verbose"]
            "#,
        )
        .unwrap();
        let err = config_with_app("test", app).validate().unwrap_err();
        assert!(
            matches!(err, ConfigError::Validation { ref field, .. } if field == "args/working_dir"),
            "{err:?}"
        );
    }

    #[test]
    fn unknown_deploy_strategy_is_rejected() {
        // "bluegreen" used to mean "rolling", silently.
        let error = toml::from_str::<AppSpec>(
            r#"
            image = "web:v1"
            [deploy]
            strategy = "bluegreen"
        "#,
        )
        .unwrap_err();
        assert!(
            error.to_string().contains("unknown variant"),
            "an unknown strategy must fail to parse: {error}"
        );
        for strategy in ["rolling", "blue-green"] {
            let app: AppSpec = toml::from_str(&format!(
                "image = \"web:v1\"\n[deploy]\nstrategy = \"{strategy}\"\n"
            ))
            .unwrap();
            config_with_app("web", app).validate().unwrap();
        }
    }

    #[test]
    fn unparseable_drain_timeout_is_rejected() {
        for (field, value) in [
            ("drain_timeout", "1m30s"),
            ("drain_timeout", "500ms"),
            ("health_timeout", "a minute"),
        ] {
            let app: AppSpec = toml::from_str(&format!(
                "image = \"web:v1\"\n[deploy]\n{field} = \"{value}\"\n"
            ))
            .unwrap();
            let err = config_with_app("web", app).validate().unwrap_err();
            assert!(
                matches!(err, ConfigError::Validation { field: ref f, ref reason, .. }
                    if f == "deploy" && reason.contains(field) && reason.contains(value)),
                "{field} = {value:?} must be refused, not replaced by the default: {err:?}"
            );
        }
        let app: AppSpec = toml::from_str(
            "image = \"web:v1\"\n[deploy]\ndrain_timeout = \"90s\"\nhealth_timeout = \"2m\"\n",
        )
        .unwrap();
        config_with_app("web", app).validate().unwrap();
    }

    #[test]
    fn validate_autoscale_min_greater_than_max_rejected() {
        let app: AppSpec = toml::from_str(
            r#"
            image = "web:v1"
            cpu = "100m-500m"
            [autoscale]
            metric = "cpu"
            target = "70%"
            min = 10
            max = 3
        "#,
        )
        .unwrap();
        let config = config_with_app("web", app);
        let err = config.validate().unwrap_err();
        assert!(
            matches!(err, ConfigError::Validation { ref field, ref reason, .. }
                if field == "autoscale" && reason.contains("must not exceed")),
            "min>max autoscale must be rejected at validation: {err:?}"
        );
    }

    #[test]
    fn validate_autoscale_min_zero_rejected() {
        let app: AppSpec = toml::from_str(
            r#"
            image = "web:v1"
            cpu = "100m-500m"
            [autoscale]
            metric = "cpu"
            target = "70%"
            min = 0
            max = 3
        "#,
        )
        .unwrap();
        let err = config_with_app("web", app).validate().unwrap_err();
        assert!(
            matches!(err, ConfigError::Validation { ref field, ref reason, .. }
                if field == "autoscale" && reason.contains("min must be at least 1")),
            "min = 0 must be rejected at validation: {err:?}"
        );
    }

    fn autoscaled_app(resources: &str, metric: &str) -> AppSpec {
        toml::from_str(&format!(
            r#"
            image = "web:v1"
            {resources}
            [autoscale]
            metric = "{metric}"
            target = "50%"
            min = 1
            max = 5
        "#
        ))
        .unwrap()
    }

    #[test]
    fn validate_autoscale_accepts_cpu_and_memory_with_requests() {
        let cpu = autoscaled_app(r#"cpu = "250m-1000m""#, "cpu");
        config_with_app("web", cpu).validate().unwrap();
        let memory = autoscaled_app(r#"memory = "128Mi-512Mi""#, "memory");
        config_with_app("web", memory).validate().unwrap();
    }

    #[test]
    fn validate_autoscale_rejects_an_unsupported_metric() {
        let app = autoscaled_app(r#"cpu = "250m""#, "requests_per_second");
        let err = config_with_app("web", app).validate().unwrap_err();
        assert!(
            matches!(err, ConfigError::Validation { ref field, ref reason, .. }
                if field == "autoscale" && reason.contains("not supported")),
            "an unknown metric must fail validation, not silently never scale: {err:?}"
        );
    }

    #[test]
    fn validate_autoscale_accepts_cpu_without_a_request() {
        // Measured against one core; ProcessGrill apps can't declare cpu.
        let app = autoscaled_app("", "cpu");
        config_with_app("web", app).validate().unwrap();
    }

    #[test]
    fn validate_autoscale_rejects_memory_without_a_memory_request() {
        // CPU is requested, but the block scales on memory.
        let app = autoscaled_app(r#"cpu = "250m""#, "memory");
        let err = config_with_app("web", app).validate().unwrap_err();
        assert!(
            matches!(err, ConfigError::Validation { ref field, ref reason, .. }
                if field == "autoscale" && reason.contains("needs a non-zero memory request")),
            "{err:?}"
        );
    }

    #[test]
    fn validate_node_config_non_absolute_path_rejected() {
        let mut nc = NodeConfig::default();
        nc.storage.data = PathBuf::from("relative/path");
        assert!(matches!(
            nc.validate(),
            Err(ConfigError::NonAbsolutePath { .. })
        ));
    }

    #[test]
    fn audit_log_bound_has_a_floor() {
        let mut nc = NodeConfig::default();
        assert_eq!(
            nc.storage.audit_max_bytes,
            crate::bun::audit::DEFAULT_AUDIT_MAX_BYTES
        );
        nc.storage.audit_max_bytes = 1024;
        let error = nc.validate().unwrap_err().to_string();
        assert!(error.contains("storage.audit_max_bytes"), "{error}");
        nc.storage.audit_max_bytes = crate::bun::audit::MIN_AUDIT_MAX_BYTES;
        assert!(nc.validate().is_ok());
        let parsed = NodeConfig::parse("[storage]\naudit_max_bytes = 1048576\n").unwrap();
        assert_eq!(parsed.storage.audit_max_bytes, 1 << 20);
    }

    #[test]
    fn scheduled_snapshots_must_retain_at_least_one() {
        let mut nc = NodeConfig::default();
        nc.storage.snapshots.interval_secs = 3600;
        nc.storage.snapshots.retain = 0;
        let error = nc.validate().unwrap_err().to_string();
        assert!(error.contains("storage.snapshots.retain"), "{error}");

        // Disabled schedules don't prune, so zero is harmless there.
        nc.storage.snapshots.interval_secs = 0;
        assert!(nc.validate().is_ok());
        nc.storage.snapshots.interval_secs = 3600;
        nc.storage.snapshots.retain = 1;
        assert!(nc.validate().is_ok());
    }

    #[test]
    fn snapshot_uploads_need_a_nonzero_deadline() {
        let mut nc = NodeConfig::default();
        nc.storage.snapshots.upload_url = Some("file:///backups".into());
        nc.storage.snapshots.upload_timeout_secs = 0;
        let error = nc.validate().unwrap_err().to_string();
        assert!(error.contains("upload_timeout_secs"), "{error}");
        nc.storage.snapshots.upload_timeout_secs = 60;
        assert!(nc.validate().is_ok());
    }

    #[test]
    fn validate_node_config_invalid_port_range_rejected() {
        let mut nc = NodeConfig::default();
        nc.network.port_range.start = 60000;
        nc.network.port_range.end = 10000;
        assert!(matches!(
            nc.validate(),
            Err(ConfigError::InvalidPortRange { .. })
        ));
    }

    #[test]
    fn custom_event_limit_is_refused_instead_of_ignored() {
        let mut config = NodeConfig::default();
        config.reporting_tree.max_events_per_report = 101;
        assert!(
            config
                .validate()
                .unwrap_err()
                .to_string()
                .contains("fixed 100-event")
        );
        config.reporting_tree.max_events_per_report = 100;
        assert!(config.validate().is_ok());
    }

    fn development_with_leaf_override(seconds: u64) -> NodeConfig {
        let mut config = NodeConfig::default();
        config.testing.safety_class = crate::testkit::safety::ClusterSafetyClass::Development;
        config.security.leaf_lifetime_override_secs = Some(seconds);
        config
    }

    fn assert_leaf_override_refused(config: &NodeConfig, reason_contains: &str) {
        let error = config.validate().unwrap_err();
        assert!(
            matches!(
                &error,
                ConfigError::Validation { field, reason, .. }
                    if field == "security.leaf_lifetime_override_secs"
                        && reason.contains(reason_contains)
            ),
            "{error:?}"
        );
        assert!(
            error
                .to_string()
                .contains("security.leaf_lifetime_override_secs")
        );
    }

    #[test]
    fn operator_cidrs_default_to_empty() {
        let config = NodeConfig::parse("").unwrap();
        assert!(config.security.operator_cidrs.is_empty());
        config.validate().unwrap();
    }

    #[test]
    fn operator_cidrs_accept_v4_v6_and_bare_addresses() {
        let config = NodeConfig::parse(
            "[security]\noperator_cidrs = [\"192.168.0.0/24\", \"10.1.2.3/32\", \"2001:db8::/32\", \"10.1.2.4\"]\n",
        )
        .unwrap();
        assert_eq!(config.security.operator_cidrs.len(), 4);
        config.validate().unwrap();
    }

    fn assert_operator_cidr_refused(value: &str, reason_contains: &str) {
        let mut config = NodeConfig::default();
        config.security.operator_cidrs = vec!["10.0.0.0/8".to_string(), value.to_string()];
        let error = config.validate().unwrap_err();
        assert!(
            matches!(
                &error,
                ConfigError::Validation { field, reason, .. }
                    if field == "security.operator_cidrs" && reason.contains(reason_contains)
            ),
            "{value}: {error:?}"
        );
        assert!(error.to_string().contains(value), "{error}");
    }

    #[test]
    fn malformed_operator_cidrs_are_refused_at_load() {
        assert_operator_cidr_refused("not-a-cidr", "expected address");
        assert_operator_cidr_refused("10.0.0.0/33", "out of range");
        assert_operator_cidr_refused("10.0.0.0/8; accept", "invalid prefix length");
        assert_operator_cidr_refused("192.168.0.17/24", "did you mean 192.168.0.0/24");
    }

    #[test]
    fn operator_cidrs_refuse_the_whole_internet() {
        assert_operator_cidr_refused("0.0.0.0/0", "/0 prefix");
        assert_operator_cidr_refused("::/0", "/0 prefix");
    }

    #[test]
    fn leaf_lifetime_override_is_accepted_on_a_development_node() {
        let config = development_with_leaf_override(3600);
        config.validate().unwrap();
        assert_eq!(
            config.security.node_leaf_lifetime(),
            std::time::Duration::from_secs(3600)
        );
        assert_eq!(
            config.security.ingress_leaf_lifetime(),
            std::time::Duration::from_secs(3600)
        );
        development_with_leaf_override(600).validate().unwrap();
        development_with_leaf_override(90 * 24 * 3600)
            .validate()
            .unwrap();
    }

    #[test]
    fn leaf_lifetime_override_below_ten_minutes_is_refused() {
        assert_leaf_override_refused(&development_with_leaf_override(599), "between 600");
        assert_leaf_override_refused(&development_with_leaf_override(0), "between 600");
    }

    #[test]
    fn leaf_lifetime_override_above_the_ingress_default_is_refused() {
        // The knob shortens both leaf classes, so it can't exceed the shorter
        // (90-day ingress) default: that would lengthen ingress leaves.
        assert_leaf_override_refused(
            &development_with_leaf_override(90 * 24 * 3600 + 1),
            "and 7776000 seconds",
        );
        assert_leaf_override_refused(
            &development_with_leaf_override(365 * 24 * 3600),
            "and 7776000 seconds",
        );
    }

    #[test]
    fn leaf_lifetime_override_is_refused_outside_development() {
        use crate::testkit::safety::ClusterSafetyClass;
        for class in [
            ClusterSafetyClass::Unknown,
            ClusterSafetyClass::Staging,
            ClusterSafetyClass::Production,
        ] {
            let mut config = development_with_leaf_override(3600);
            config.testing.safety_class = class;
            assert_leaf_override_refused(&config, "development-only");
        }
        // An absent `[testing]` section is Unknown, never development.
        let parsed = NodeConfig::parse("[security]\nleaf_lifetime_override_secs = 3600\n").unwrap();
        assert_leaf_override_refused(&parsed, "safety_class = \"development\"");
    }

    #[test]
    fn absent_leaf_lifetime_override_keeps_the_compiled_defaults() {
        let config = NodeConfig::parse("").unwrap();
        assert_eq!(config.security.leaf_lifetime_override_secs, None);
        config.validate().unwrap();
        assert_eq!(
            config.security.node_leaf_lifetime(),
            crate::sesame::ca::NODE_LEAF_LIFETIME
        );
        assert_eq!(
            config.security.ingress_leaf_lifetime(),
            crate::sesame::ca::INGRESS_LEAF_LIFETIME
        );
        assert_eq!(
            crate::sesame::ca::NODE_LEAF_LIFETIME,
            std::time::Duration::from_secs(365 * 24 * 3600)
        );
        assert_eq!(
            crate::sesame::ca::INGRESS_LEAF_LIFETIME,
            std::time::Duration::from_secs(90 * 24 * 3600)
        );
    }

    #[test]
    fn validate_valid_node_config_passes() {
        let nc = NodeConfig::default();
        nc.validate().unwrap();
    }

    #[test]
    fn dns_enabled_without_ebpf_rejected() {
        // A DNS-served VIP only routes when the eBPF connect hook is loaded;
        // enabling DNS without eBPF is a silent black hole, so it must fail
        // config validation up front.
        let mut nc = NodeConfig::default();
        nc.dns.enabled = true;
        nc.ebpf.enabled = false;
        let err = nc.validate().unwrap_err();
        assert!(
            matches!(err, ConfigError::Validation { ref field, .. } if field == "dns.enabled"),
            "dns without ebpf must be rejected: {err:?}"
        );
    }

    #[test]
    fn dns_enabled_with_ebpf_passes() {
        let mut nc = NodeConfig::default();
        nc.dns.enabled = true;
        nc.ebpf.enabled = true;
        nc.validate().unwrap();
    }

    #[test]
    fn upgrades_section_defaults() {
        let nc = NodeConfig::default();
        assert_eq!(nc.upgrades.retain_versions, 3);
        assert_eq!(nc.upgrades.max_boot_attempts, 2);
        assert!(nc.upgrades.external_signing_key.is_none());
        assert!(nc.upgrades.release_keys_override.is_none());
        nc.validate().unwrap();
    }

    #[test]
    fn rejects_invalid_external_signing_key() {
        let mut nc = NodeConfig::default();
        nc.upgrades.external_signing_key = Some("not-a-key".to_string());
        assert!(matches!(nc.validate(), Err(ConfigError::Validation { .. })));
    }

    #[test]
    fn release_keys_override_requires_valid_keys() {
        let mut nc = NodeConfig::default();
        nc.upgrades.release_keys_override = Some(vec!["ed25519:c2hvcnQ=".to_string()]); // valid base64, wrong length
        assert!(matches!(nc.validate(), Err(ConfigError::Validation { .. })));
    }

    #[test]
    fn upgrades_retain_versions_zero_rejected() {
        let mut nc = NodeConfig::default();
        nc.upgrades.retain_versions = 0;
        assert!(matches!(nc.validate(), Err(ConfigError::Validation { .. })));
    }

    #[test]
    fn upgrades_max_boot_attempts_zero_rejected() {
        // A zero boot budget reverts every upgrade on its first boot, so no
        // upgrade can ever commit — reject it at config load.
        let mut nc = NodeConfig::default();
        nc.upgrades.max_boot_attempts = 0;
        assert!(matches!(nc.validate(), Err(ConfigError::Validation { .. })));
    }

    #[test]
    fn upgrades_relative_binary_dir_rejected() {
        let mut nc = NodeConfig::default();
        nc.upgrades.binary_dir = Some(PathBuf::from("relative/bin"));
        assert!(matches!(
            nc.validate(),
            Err(ConfigError::NonAbsolutePath { .. })
        ));
    }

    #[test]
    fn managed_volume_artifact_overlaps_are_rejected() {
        for paths in [
            ["/data", "/data/child"],
            ["/data", "/data.img"],
            ["/data", "/data.volume.json"],
            ["/data", "/data.volume.json.tmp"],
            ["/data", "/data.restore-staged"],
            ["/data", "/data.restore-old"],
            ["/data", "/data.restore.json"],
            ["/data", "/data"],
            ["/", "/data"],
        ] {
            let mut app = minimal_app();
            app.volumes = paths
                .map(|path| crate::config::types::VolumeSpec {
                    path: PathBuf::from(path),
                    source: None,
                    size: Some("16Mi".into()),
                })
                .to_vec();
            assert!(
                matches!(
                    config_with_app("db", app).validate(),
                    Err(ConfigError::InvalidVolume { .. })
                ),
                "accepted {paths:?}"
            );
        }
    }

    #[test]
    fn dotted_managed_volume_paths_remain_distinct() {
        let mut app = minimal_app();
        app.volumes = ["/data.a", "/data.b"]
            .map(|path| crate::config::types::VolumeSpec {
                path: PathBuf::from(path),
                source: None,
                size: Some("16Mi".into()),
            })
            .to_vec();
        config_with_app("db", app).validate().unwrap();
    }

    #[test]
    fn validate_volume_relative_mount_path_rejected() {
        let mut app = minimal_app();
        app.volumes.push(crate::config::types::VolumeSpec {
            path: PathBuf::from("relative/data"),
            source: None,
            size: None,
        });
        let config = config_with_app("test", app);
        assert!(matches!(
            config.validate(),
            Err(ConfigError::InvalidVolume { .. })
        ));
    }

    #[test]
    fn validate_volume_relative_source_path_rejected() {
        let mut app = minimal_app();
        app.volumes.push(crate::config::types::VolumeSpec {
            path: PathBuf::from("/data"),
            source: Some(PathBuf::from("relative/host/path")),
            size: None,
        });
        let config = config_with_app("test", app);
        assert!(matches!(
            config.validate(),
            Err(ConfigError::InvalidVolume { .. })
        ));
    }

    #[test]
    fn validate_volume_traversal_mount_path_rejected() {
        // An absolute path with `..` passes the absolute check but escapes the
        // volumes directory once joined — reject it.
        let mut app = minimal_app();
        app.volumes.push(crate::config::types::VolumeSpec {
            path: PathBuf::from("/../../../../etc/cron.d"),
            source: None,
            size: None,
        });
        let config = config_with_app("test", app);
        assert!(matches!(
            config.validate(),
            Err(ConfigError::InvalidVolume { .. })
        ));
    }

    #[test]
    fn validate_volume_absolute_paths_passes() {
        let mut app = minimal_app();
        app.volumes.push(crate::config::types::VolumeSpec {
            path: PathBuf::from("/data"),
            source: Some(PathBuf::from("/host/data")),
            size: None,
        });
        let config = config_with_app("test", app);
        config.validate().unwrap();
    }

    // -----------------------------------------------------------------------
    // Process workload validation
    // -----------------------------------------------------------------------

    #[test]
    fn validate_exec_and_image_mutually_exclusive() {
        let app: AppSpec = toml::from_str(
            r#"
            image = "test:v1"
            exec = "/usr/bin/python3"
        "#,
        )
        .unwrap();
        let config = config_with_app("test", app);
        let err = config.validate().unwrap_err();
        assert!(
            matches!(err, ConfigError::Validation { ref reason, .. } if reason.contains("mutually exclusive")),
            "expected mutual exclusivity error, got {err:?}"
        );
    }

    #[test]
    fn validate_script_and_image_mutually_exclusive() {
        let app: AppSpec = toml::from_str(
            r#"
            image = "test:v1"
            script = "echo hello"
        "#,
        )
        .unwrap();
        let config = config_with_app("test", app);
        let err = config.validate().unwrap_err();
        assert!(
            matches!(err, ConfigError::Validation { ref reason, .. } if reason.contains("mutually exclusive")),
            "expected mutual exclusivity error, got {err:?}"
        );
    }

    #[test]
    fn validate_exec_and_script_mutually_exclusive() {
        let app: AppSpec = toml::from_str(
            r#"
            exec = "/usr/bin/python3"
            script = "echo hello"
        "#,
        )
        .unwrap();
        let config = config_with_app("test", app);
        let err = config.validate().unwrap_err();
        assert!(
            matches!(err, ConfigError::Validation { ref field, .. } if field == "exec/script"),
            "expected exec/script error, got {err:?}"
        );
    }

    #[test]
    fn validate_exec_only_passes() {
        let app: AppSpec = toml::from_str(r#"exec = "/usr/bin/python3""#).unwrap();
        let config = config_with_app("test", app);
        config.validate().unwrap();
    }

    #[test]
    fn validate_script_only_passes() {
        let app: AppSpec = toml::from_str(r#"script = "echo hello""#).unwrap();
        let config = config_with_app("test", app);
        config.validate().unwrap();
    }

    #[test]
    fn autoscale_admission_refuses_nonpositive_and_nonfinite_targets() {
        for target in ["0%", "-20%", "NaN", "NaN%", "inf", "inf%", "-inf"] {
            let config = Config::parse(&format!(
                "[app.web]\nimage='busybox'\ncpu='100m'\n[app.web.autoscale]\nmetric='cpu'\ntarget='{target}'\nmin=1\nmax=5\n"
            ))
            .unwrap();
            assert!(config.validate().is_err(), "accepted target {target}");
        }
        for target in ["70%", "0.7", "150%"] {
            let config = Config::parse(&format!(
                "[app.web]\nimage='busybox'\ncpu='100m'\n[app.web.autoscale]\nmetric='cpu'\ntarget='{target}'\nmin=1\nmax=5\n"
            ))
            .unwrap();
            config.validate().unwrap();
        }
    }

    #[test]
    fn invalid_cron_schedule_is_rejected_during_config_validation() {
        for expression in ["61 * * * *", "*/0 * * * *", "not a schedule"] {
            let raw = format!("[job.tick]\nimage=\"busybox\"\nschedule={expression:?}\n");
            let error = Config::parse(&raw)
                .unwrap()
                .validate()
                .expect_err("invalid schedule must fail admission");
            assert!(
                matches!(error, ConfigError::Validation { ref field, .. } if field == "schedule")
            );
        }
    }

    #[test]
    fn validate_job_exec_and_image_mutually_exclusive() {
        let job: crate::config::job::JobSpec = toml::from_str(
            r#"
            image = "test:v1"
            exec = "/usr/bin/python3"
        "#,
        )
        .unwrap();
        let mut config = Config::default();
        config.job.insert("test-job".to_string(), job);
        let err = config.validate().unwrap_err();
        assert!(matches!(
            err,
            ConfigError::Validation { ref reason, .. } if reason.contains("mutually exclusive")
        ));
    }

    #[test]
    fn job_egress_is_validated_like_an_app() {
        let invalid = [
            ("egress", "allow = [\"no-port.example.com\"]", "egress"),
            ("egress", "allow = [\"10.0.0.1/8:443\"]", "egress"),
            ("egress", "allow = [\"2001:db8::1:443\"]", "egress"),
            (
                "firewall",
                "allow_from = [\"Not_A_Label\"]",
                "firewall.allow_from",
            ),
            (
                "firewall",
                "allow_from = [\"a/b/c\"]",
                "firewall.allow_from",
            ),
        ];
        for (table, body, field) in invalid {
            for kind in ["app", "job"] {
                let source =
                    format!("[{kind}.work]\nimage = \"fixture:v1\"\n[{kind}.work.{table}]\n{body}");
                let error = Config::parse(&source).unwrap().validate().unwrap_err();
                assert!(
                    matches!(&error, ConfigError::Validation { field: actual, .. } if actual == field),
                    "{kind} accepted {table} {body:?}: {error}"
                );
            }
        }
        for kind in ["app", "job"] {
            let source = format!(
                "[{kind}.work]\nimage = \"fixture:v1\"\n\
                 [{kind}.work.egress]\nallow = [\"api.example.com:443\", \"10.0.0.0/8:5432\", \"[2001:db8::1]:443\"]\n\
                 [{kind}.work.firewall]\nallow_from = [\"web\", \"frontend/scraper\"]"
            );
            Config::parse(&source).unwrap().validate().unwrap();
        }
    }

    #[test]
    fn validate_job_exec_only_passes() {
        let job: crate::config::job::JobSpec = toml::from_str(
            r#"runtime = "process"
exec = "/usr/bin/python3""#,
        )
        .unwrap();
        let mut config = Config::default();
        config.job.insert("test-job".to_string(), job);
        config.validate().unwrap();
    }

    // -----------------------------------------------------------------------
    // Namespace / permission / build validation (12b.2 T6)
    // -----------------------------------------------------------------------

    #[test]
    fn validate_namespace_negative_quota_rejected() {
        let config = Config::parse(
            r#"
            [namespace.team]
            cpu = "-5"
        "#,
        )
        .unwrap();
        let err = config.validate().unwrap_err();
        assert!(
            matches!(err, ConfigError::Validation { ref field, .. } if field == "cpu"),
            "negative cpu budget must be rejected: {err:?}"
        );
    }

    #[test]
    fn validate_namespace_zero_max_apps_rejected() {
        let config = Config::parse(
            r#"
            [namespace.team]
            max_apps = 0
        "#,
        )
        .unwrap();
        assert!(matches!(
            config.validate(),
            Err(ConfigError::Validation { .. })
        ));
    }

    #[test]
    fn validate_namespace_valid_quota_passes() {
        let config = Config::parse(
            r#"
            [namespace.team]
            cpu = "8000m"
            memory = "16Gi"
            gpu = 2
            max_apps = 50
            max_replicas = 200
        "#,
        )
        .unwrap();
        config.validate().unwrap();
    }

    #[test]
    fn validate_permission_unknown_action_rejected() {
        let config = Config::parse(
            r#"
            [permission.p]
            actions = ["deploy", "teleport"]
        "#,
        )
        .unwrap();
        let err = config.validate().unwrap_err();
        assert!(
            matches!(err, ConfigError::Validation { ref field, .. } if field == "actions"),
            "unknown permission action must be rejected: {err:?}"
        );
    }

    #[test]
    fn validate_permission_accepts_the_fault_action() {
        let config = Config::parse(
            r#"
            [permission.chaos]
            actions = ["fault"]
            apps = ["web"]
        "#,
        )
        .unwrap();
        config.validate().unwrap();
    }

    #[test]
    fn validate_permission_unknown_namespace_rejected() {
        let config = Config::parse(
            r#"
            [permission.p]
            actions = ["deploy"]
            namespaces = ["ghost"]
        "#,
        )
        .unwrap();
        assert!(matches!(
            config.validate(),
            Err(ConfigError::Validation { .. })
        ));
    }

    #[test]
    fn validate_permission_namespace_in_same_config_passes() {
        let config = Config::parse(
            r#"
            [namespace.prod]
            cpu = "8000m"

            [permission.p]
            actions = ["deploy", "scale"]
            namespaces = ["prod"]
        "#,
        )
        .unwrap();
        config.validate().unwrap();
    }

    #[test]
    fn validate_permission_namespace_from_desired_state_passes() {
        // `prod` isn't in this file but was created by an earlier apply.
        let config = Config::parse(
            r#"
            [permission.p]
            actions = ["deploy"]
            namespaces = ["prod"]
        "#,
        )
        .unwrap();
        assert!(config.validate().is_err(), "bare validate can't see prod");
        config
            .validate_against(&["prod".to_string()])
            .expect("validate_against sees the committed namespace");
    }

    #[test]
    fn validate_build_unknown_namespace_rejected() {
        let config = Config::parse(
            r#"
            [build.img]
            context = "."
            destination = "pickle://img:v1"
            namespace = "ghost"
        "#,
        )
        .unwrap();
        assert!(matches!(
            config.validate(),
            Err(ConfigError::Validation { .. })
        ));
    }

    #[test]
    fn validate_build_existing_namespace_passes() {
        let config = Config::parse(
            r#"
            [namespace.prod]
            cpu = "8000m"

            [build.img]
            context = "."
            destination = "pickle://img:v1"
            namespace = "prod"
        "#,
        )
        .unwrap();
        config.validate().unwrap();
    }

    #[test]
    fn prerequisites_require_an_app_in_the_same_apply_and_namespace() {
        for body in [
            "[job.migrate]\nimage='migrate'\nrun_before=['app.absent']\n",
            "[app.web]\nimage='web'\nnamespace='production'\n[job.migrate]\nimage='migrate'\nnamespace='other'\nrun_before=['app.web']\n",
            "[app.web]\nimage='web'\n[job.migrate]\nimage='migrate'\nrun_before=['job.web']\n",
        ] {
            let error = Config::parse(body).unwrap().validate().expect_err(body);
            assert!(error.to_string().contains("run_before"), "{error}");
        }
    }

    #[test]
    fn same_namespace_prerequisites_are_valid() {
        Config::parse("[app.web]\nimage='web'\nnamespace='production'\n[job.migrate]\nimage='migrate'\nnamespace='production'\nrun_before=['app.web']\n")
            .unwrap().validate().unwrap();
    }

    /// Validate a one-app config whose `[app.web.ingress]` table is `ingress`.
    fn validate_ingress(ingress: &str) -> Result<(), ConfigError> {
        Config::parse(&format!(
            "[app.web]\nimage = \"web:v1\"\nport = 8080\n[app.web.ingress]\nhost = \"web.example.com\"\n{ingress}"
        ))
        .unwrap()
        .validate()
    }

    fn assert_ingress_refused(ingress: &str, reason_contains: &str) {
        let error = validate_ingress(ingress).expect_err(ingress);
        assert!(
            matches!(error, ConfigError::Validation { ref field, .. } if field == "ingress"),
            "{error:?}"
        );
        assert!(error.to_string().contains(reason_contains), "{error}");
    }

    #[test]
    fn ingress_with_unknown_tls_mode_is_rejected() {
        // ACME is deferred: the route would be refused on every node, so
        // apply and lint refuse it first (#700).
        for mode in ["acme", "auto", "letsencrypt"] {
            assert_ingress_refused(&format!("tls = \"{mode}\""), "tls");
        }
    }

    #[test]
    fn ingress_with_zero_rate_limit_is_rejected() {
        assert_ingress_refused("rate_limit_rps = 0", "rate_limit_rps");
        assert_ingress_refused(
            "rate_limit_rps = 10\nrate_limit_burst = 0",
            "rate_limit_burst",
        );
    }

    #[test]
    fn ingress_rate_limit_without_a_derivable_burst_is_rejected() {
        assert_ingress_refused(&format!("rate_limit_rps = {}", u32::MAX), "burst");
    }

    #[test]
    fn ingress_with_supported_settings_is_valid() {
        for ingress in [
            "",
            "tls = \"cluster\"",
            "tls = \"explicit\"\nrate_limit_rps = 50",
            "tls = \"none\"\nrate_limit_rps = 5\nrate_limit_burst = 20",
        ] {
            validate_ingress(ingress).unwrap();
        }
    }

    /// Validate a one-app config whose `[app.web]` table is `app`.
    fn validate_app_toml(app: &str) -> Result<(), ConfigError> {
        Config::parse(&format!("[app.web]\n{app}"))
            .unwrap()
            .validate()
    }

    /// Assert `app` is refused with an error naming `field`.
    fn assert_app_refused(app: &str, field: &str) {
        let error = validate_app_toml(app).expect_err(app);
        assert!(error.to_string().contains(field), "{app}: {error}");
    }

    #[test]
    fn process_app_with_volume_is_refused() {
        // ProcessGrill never mounts anything: the app would run without
        // the volume it declared (#701).
        for kind in ["exec = \"/usr/bin/python3\"", "script = \"echo hi\""] {
            assert_app_refused(
                &format!("{kind}\n[[app.web.volumes]]\npath = \"/data\""),
                "volumes",
            );
        }
    }

    #[test]
    fn process_app_with_config_file_is_refused() {
        for kind in ["exec = \"/usr/bin/python3\"", "script = \"echo hi\""] {
            assert_app_refused(
                &format!(
                    "{kind}\n[[app.web.config_file]]\npath = \"/etc/x.conf\"\ncontent = \"x\""
                ),
                "config_file",
            );
        }
    }

    #[test]
    fn process_app_with_run_as_user_is_refused() {
        assert_app_refused(
            "exec = \"/usr/bin/python3\"\nrun_as_user = 1000",
            "run_as_user",
        );
        assert_app_refused("script = \"echo hi\"\nrun_as_group = 1000", "run_as_group");
    }

    #[test]
    fn image_app_keeps_volumes_config_files_and_user() {
        validate_app_toml(
            "image = \"web:v1\"\nrun_as_user = 1000\n\
             [[app.web.volumes]]\npath = \"/data\"\n\
             [[app.web.config_file]]\npath = \"/etc/x.conf\"\ncontent = \"x\"",
        )
        .unwrap();
    }

    #[test]
    fn health_port_without_app_port_is_valid() {
        // `health.port` is enough on its own: the probe has a port (#701).
        validate_app_toml("image = \"web:v1\"\n[app.web.health]\npath = \"/healthz\"\nport = 8081")
            .unwrap();
    }

    #[test]
    fn health_without_any_port_is_refused() {
        assert_app_refused(
            "image = \"web:v1\"\n[app.web.health]\npath = \"/healthz\"",
            "health",
        );
    }

    #[test]
    fn zero_health_timeout_is_refused() {
        let health = "image = \"web:v1\"\nport = 8080\n[app.web.health]\npath = \"/healthz\"";
        for field in [
            "timeout",
            "interval",
            "threshold_unhealthy",
            "threshold_healthy",
        ] {
            assert_app_refused(&format!("{health}\n{field} = 0"), field);
        }
    }

    #[test]
    fn health_timeout_longer_than_interval_is_refused() {
        let health = "image = \"web:v1\"\nport = 8080\n[app.web.health]\npath = \"/healthz\"";
        assert_app_refused(&format!("{health}\ninterval = 5\ntimeout = 6"), "timeout");
        // The default interval is 10 s, so an 11 s timeout alone is refused too.
        assert_app_refused(&format!("{health}\ntimeout = 11"), "timeout");
        // Seconds are whole, so a 1 s interval must allow a 1 s timeout.
        validate_app_toml(&format!("{health}\ninterval = 1\ntimeout = 1")).unwrap();
    }

    #[test]
    fn gpu_request_is_refused() {
        // D41: no device passthrough or cluster accounting yet (#359).
        let error = validate_app_toml("image = \"trainer:v1\"\ngpu = 1").unwrap_err();
        assert!(error.to_string().contains("#359"), "{error}");
        validate_app_toml("image = \"trainer:v1\"\ngpu = 0").unwrap();
    }

    #[test]
    fn allow_franchise_is_refused() {
        assert_app_refused(
            "image = \"web:v1\"\n[app.web.egress]\nallow_franchise = [\"redis.prod-west\"]",
            "allow_franchise",
        );
    }
}
