//! The one path that turns a parsed `Config` into desired-state writes
//! (12b.2 T6).
//!
//! Manual `relish apply` (`bun::api`) and GitOps sync (`lettuce`) both
//! call [`config_to_desired_writes`]. Sharing one function is what makes
//! "the same config converges identically whether you apply it by hand
//! or through git" true *by construction*: there's no second code path
//! that could drift.
//!
//! Only the declarative kinds live here: apps, namespaces, permissions.
//! Jobs run to completion (not reconciled desired state) and builds are
//! dispatched imperatively; both are validated but not written by this
//! function. See chapter 7 for why builds aren't a reconciling resource.
//!
//! Deletion is *not* the concern of this function. Manual apply is
//! additive: it writes what's in the file and never prunes what isn't,
//! matching how app apply already behaves. GitOps reconciles a whole
//! repo against desired state, so it computes deletions separately (in
//! `lettuce`) and layers them on top of these writes.

use crate::config::Config;
use crate::meat::types::AppId;

use super::types::RaftRequest;

/// The ordered set of desired-state writes a `Config` implies.
///
/// Namespaces come first so a namespace's quota is committed before any
/// app that schedules against it, then permissions, then apps. Each app
/// keys on `AppId::new(name, namespace)`, defaulting to the `default`
/// namespace when the spec doesn't name one — the same rule
/// `cluster_apply` used before this function existed.
///
/// The caller writes these in order and must treat a failed write as a
/// hard stop: applying half the set leaves desired state inconsistent.
pub fn config_to_desired_writes(config: &Config) -> Vec<RaftRequest> {
    let mut writes = Vec::new();

    for (name, spec) in &config.namespace {
        writes.push(RaftRequest::NamespaceSpec {
            name: name.clone(),
            spec: Box::new(spec.clone()),
        });
    }

    for (name, spec) in &config.permission {
        writes.push(RaftRequest::PermissionSpec {
            name: name.clone(),
            spec: Box::new(spec.clone()),
        });
    }

    for (name, spec) in &config.app {
        let namespace = spec.namespace.clone().unwrap_or_else(|| "default".into());
        writes.push(RaftRequest::AppSpec {
            app_id: AppId::new(name, &namespace),
            spec: Box::new(spec.clone()),
        });
    }

    writes
}

/// Convert only the apps in a test manifest into ownership-checked writes.
///
/// The state machine inserts each app and its lease resource in one log entry.
/// A client crash can therefore leave both or neither, never an unowned app.
pub fn config_to_leased_app_writes(
    config: &Config,
    lease_id: &str,
    observed_at_unix_ms: u64,
) -> Vec<RaftRequest> {
    config
        .app
        .iter()
        .map(|(name, spec)| {
            let namespace = spec.namespace.clone().unwrap_or_else(|| "default".into());
            RaftRequest::TestLeaseAppSpec {
                lease_id: lease_id.to_string(),
                observed_at_unix_ms,
                app_id: AppId::new(name, namespace),
                spec: Box::new(spec.clone()),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(toml: &str) -> Config {
        Config::parse(toml).unwrap()
    }

    #[test]
    fn empty_config_produces_no_writes() {
        assert!(config_to_desired_writes(&Config::default()).is_empty());
    }

    #[test]
    fn namespaces_are_written_before_apps() {
        let config = parse(
            r#"
            [app.web]
            image = "web:v1"

            [namespace.prod]
            cpu = "8000m"
        "#,
        );
        let writes = config_to_desired_writes(&config);
        let ns_pos = writes
            .iter()
            .position(|w| matches!(w, RaftRequest::NamespaceSpec { .. }))
            .unwrap();
        let app_pos = writes
            .iter()
            .position(|w| matches!(w, RaftRequest::AppSpec { .. }))
            .unwrap();
        assert!(ns_pos < app_pos, "namespace quota must land before the app");
    }

    #[test]
    fn leased_writes_atomically_pair_every_app_with_the_lease() {
        let config = parse(
            r#"
            [app.web]
            image = "web:v1"
            namespace = "rbtest-run1"

            [app.api]
            image = "api:v1"
            namespace = "rbtest-run1"
        "#,
        );
        let writes = config_to_leased_app_writes(&config, "run1", 42);
        assert_eq!(writes.len(), 2);
        assert!(writes.iter().all(|write| matches!(
            write,
            RaftRequest::TestLeaseAppSpec {
                lease_id,
                observed_at_unix_ms: 42,
                ..
            } if lease_id == "run1"
        )));
    }

    #[test]
    fn every_declarative_kind_becomes_a_write() {
        let config = parse(
            r#"
            [app.web]
            image = "web:v1"

            [namespace.prod]
            cpu = "8000m"

            [permission.deployer]
            actions = ["deploy"]
        "#,
        );
        let writes = config_to_desired_writes(&config);
        assert_eq!(writes.len(), 3);
        assert!(
            writes
                .iter()
                .any(|w| matches!(w, RaftRequest::NamespaceSpec { .. }))
        );
        assert!(
            writes
                .iter()
                .any(|w| matches!(w, RaftRequest::PermissionSpec { .. }))
        );
        assert!(
            writes
                .iter()
                .any(|w| matches!(w, RaftRequest::AppSpec { .. }))
        );
    }

    #[test]
    fn app_namespace_flows_into_the_app_id() {
        let config = parse(
            r#"
            [app.web]
            image = "web:v1"
            namespace = "prod"
        "#,
        );
        let writes = config_to_desired_writes(&config);
        match &writes[0] {
            RaftRequest::AppSpec { app_id, .. } => {
                assert_eq!(app_id.namespace, "prod");
                assert_eq!(app_id.name, "web");
            }
            other => panic!("expected AppSpec, got {other:?}"),
        }
    }

    #[test]
    fn jobs_and_builds_are_not_desired_state_writes() {
        // Jobs run to completion and builds dispatch imperatively; neither
        // is reconciled desired state, so neither produces a write here.
        let config = parse(
            r#"
            [job.migrate]
            image = "migrate:v1"

            [build.img]
            context = "."
            destination = "pickle://img:v1"
        "#,
        );
        assert!(config_to_desired_writes(&config).is_empty());
    }
}
