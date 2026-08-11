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

/// A leased apply declared a kind that has no lease-owned lifecycle.
///
/// Only apps and namespaces can be attached to a lease and reaped when it
/// ends. Jobs, builds and permissions have no owning record, so a leased
/// config that declares one is rejected rather than silently dropped — the
/// alternative let a leased test believe it had deployed a job that the
/// cluster never created and cleanup could never account for.
#[derive(Debug, thiserror::Error)]
pub enum LeasedConfigError {
    /// The config declared a kind other than an app or a namespace.
    #[error(
        "leased apply cannot declare {kind} {name:?}: only apps and namespaces are lease-owned"
    )]
    UnsupportedKind {
        /// The offending declarative kind (`job`, `build`, `permission`).
        kind: &'static str,
        /// The name of the first offending declaration, for a precise error.
        name: String,
    },
}

/// Convert apps and their namespace declaration into ownership-checked writes.
///
/// Each entry atomically inserts the desired object and its lease resource.
/// A client crash can therefore leave both or neither, never an unowned object.
///
/// Mirrors the "every declarative kind is accounted for" invariant of
/// [`config_to_desired_writes`]: this path owns only apps and namespaces, so
/// any other kind is a hard error instead of a silent drop.
pub fn config_to_leased_writes(
    config: &Config,
    lease_id: &str,
    observed_at_unix_ms: u64,
) -> Result<Vec<RaftRequest>, LeasedConfigError> {
    // Reject any kind this path cannot own before emitting a single write, so a
    // half-understood config never lands as a partial, unowned deployment.
    if let Some(name) = config.permission.keys().next() {
        return Err(LeasedConfigError::UnsupportedKind {
            kind: "permission",
            name: name.clone(),
        });
    }
    if let Some(name) = config.job.keys().next() {
        return Err(LeasedConfigError::UnsupportedKind {
            kind: "job",
            name: name.clone(),
        });
    }
    if let Some(name) = config.build.keys().next() {
        return Err(LeasedConfigError::UnsupportedKind {
            kind: "build",
            name: name.clone(),
        });
    }

    let mut writes = Vec::new();
    for (name, spec) in &config.namespace {
        writes.push(RaftRequest::TestLeaseNamespaceSpec {
            lease_id: lease_id.to_string(),
            observed_at_unix_ms,
            name: name.clone(),
            spec: Box::new(spec.clone()),
        });
    }
    for (name, spec) in &config.app {
        let namespace = spec.namespace.clone().unwrap_or_else(|| "default".into());
        writes.push(RaftRequest::TestLeaseAppSpec {
            lease_id: lease_id.to_string(),
            observed_at_unix_ms,
            app_id: AppId::new(name, namespace),
            spec: Box::new(spec.clone()),
        });
    }
    Ok(writes)
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
    fn leased_writes_pair_the_namespace_and_every_app_with_the_lease() {
        let config = parse(
            r#"
            [namespace."rbtest-run1"]
            max_apps = 2

            [app.web]
            image = "web:v1"
            namespace = "rbtest-run1"

            [app.api]
            image = "api:v1"
            namespace = "rbtest-run1"
        "#,
        );
        let writes = config_to_leased_writes(&config, "run1", 42).unwrap();
        assert_eq!(writes.len(), 3);
        assert!(matches!(
            &writes[0],
            RaftRequest::TestLeaseNamespaceSpec {
                lease_id,
                observed_at_unix_ms: 42,
                name,
                ..
            } if lease_id == "run1" && name == "rbtest-run1"
        ));
        assert!(writes[1..].iter().all(|write| matches!(
            write,
            RaftRequest::TestLeaseAppSpec {
                lease_id,
                observed_at_unix_ms: 42,
                ..
            } if lease_id == "run1"
        )));
    }

    #[test]
    fn leased_writes_reject_a_non_owned_kind() {
        // A leased apply owns only apps and namespaces. A job, build or
        // permission has no owning record, so cleanup could never reap it —
        // reject the whole config rather than silently dropping the kind and
        // letting the test believe it deployed something the cluster never did.
        for (toml, kind) in [
            (
                r#"
                [app.web]
                image = "web:v1"
                namespace = "rbtest-run1"

                [job.migrate]
                image = "migrate:v1"
                "#,
                "job",
            ),
            (
                r#"
                [build.img]
                context = "."
                destination = "pickle://img:v1"
                "#,
                "build",
            ),
            (
                r#"
                [permission.deployer]
                actions = ["deploy"]
                "#,
                "permission",
            ),
        ] {
            let config = parse(toml);
            let error = config_to_leased_writes(&config, "run1", 42).unwrap_err();
            assert!(
                matches!(&error, LeasedConfigError::UnsupportedKind { kind: k, .. } if *k == kind),
                "expected an {kind} rejection, got: {error}"
            );
        }
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
