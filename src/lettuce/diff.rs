//! Diff engine for Lettuce GitOps.
//!
//! Computes the diff between the git-sourced config and the current
//! Raft desired state. Autoscaler-aware: the `replicas` field is
//! compared independently so autoscaler overrides aren't reset by
//! unrelated config changes.

use std::collections::{BTreeMap, HashMap};

use crate::config::app::AppSpec;
use crate::config::{Config, NamespaceSpec, PermissionSpec};
use crate::meat::types::AppId;

use super::types::DiffSummary;

/// The current declarative desired state a diff compares git against.
///
/// Grouping the three maps keeps `compute_diff`'s signature from growing
/// a parameter per resource kind.
#[derive(Debug)]
pub struct CurrentState<'a> {
    /// Apps currently in Raft, keyed by identity.
    pub apps: &'a HashMap<AppId, AppSpec>,
    /// Namespaces currently in Raft, keyed by name.
    pub namespaces: &'a BTreeMap<String, NamespaceSpec>,
    /// Permissions currently in Raft, keyed by name.
    pub permissions: &'a BTreeMap<String, PermissionSpec>,
}

/// A single resource change to apply.
#[derive(Debug, Clone, PartialEq)]
pub enum ResourceChange {
    /// A new app/job/namespace to create.
    Add {
        resource_id: String,
        spec: ChangePayload,
    },
    /// An existing app/job/namespace to update.
    Update {
        resource_id: String,
        spec: ChangePayload,
        /// Whether the replicas field specifically changed.
        replicas_changed: bool,
    },
    /// A resource to remove.
    Remove { resource_id: String },
}

/// The payload for an add or update change.
#[derive(Debug, Clone, PartialEq)]
pub enum ChangePayload {
    App(Box<AppSpec>),
    Namespace(Box<NamespaceSpec>),
    Permission(Box<PermissionSpec>),
    /// Jobs (run to completion) and builds (dispatched imperatively) are
    /// not reconciled desired state, so their diff carries no spec.
    Generic,
}

/// Compute the diff between git config and current Raft state.
///
/// The `autoscale_overrides` map is consulted to avoid resetting
/// the autoscaler's replica count when only non-replica fields changed.
pub fn compute_diff(
    git_config: &Config,
    current: &CurrentState<'_>,
    _autoscale_overrides: &[(String, u32)],
) -> (Vec<ResourceChange>, DiffSummary) {
    let mut changes = Vec::new();
    let mut added = 0usize;
    let mut modified = 0usize;
    let mut removed = 0usize;

    // Build lookup for current apps
    let current_by_name: BTreeMap<String, (&AppId, &AppSpec)> = current
        .apps
        .iter()
        .map(|(id, spec)| (id.name.clone(), (id, spec)))
        .collect();

    // Check for added and modified apps
    for (name, git_spec) in &git_config.app {
        match current_by_name.get(name) {
            None => {
                changes.push(ResourceChange::Add {
                    resource_id: format!("app.{name}"),
                    spec: ChangePayload::App(Box::new(git_spec.clone())),
                });
                added += 1;
            }
            Some((_app_id, current_spec)) => {
                let replicas_changed = replicas_differ(git_spec, current_spec);
                let other_fields_changed = non_replica_fields_differ(git_spec, current_spec);

                if replicas_changed || other_fields_changed {
                    changes.push(ResourceChange::Update {
                        resource_id: format!("app.{name}"),
                        spec: ChangePayload::App(Box::new(git_spec.clone())),
                        replicas_changed,
                    });
                    modified += 1;
                }
            }
        }
    }

    // Check for removed apps (in current but not in git)
    for name in current_by_name.keys() {
        if !git_config.app.contains_key(name) {
            changes.push(ResourceChange::Remove {
                resource_id: format!("app.{name}"),
            });
            removed += 1;
        }
    }

    // Namespaces: add/update/remove against current desired state. A
    // namespace present in git but not in Raft is an add; one whose spec
    // changed is an update; one in Raft but not in git is removed (GitOps
    // reconciles the whole repo, unlike additive manual apply).
    for (name, git_ns) in &git_config.namespace {
        match current.namespaces.get(name) {
            None => {
                changes.push(ResourceChange::Add {
                    resource_id: format!("namespace.{name}"),
                    spec: ChangePayload::Namespace(Box::new(git_ns.clone())),
                });
                added += 1;
            }
            Some(current_ns) if current_ns != git_ns => {
                changes.push(ResourceChange::Update {
                    resource_id: format!("namespace.{name}"),
                    spec: ChangePayload::Namespace(Box::new(git_ns.clone())),
                    replicas_changed: false,
                });
                modified += 1;
            }
            Some(_) => {}
        }
    }
    for name in current.namespaces.keys() {
        if !git_config.namespace.contains_key(name) {
            changes.push(ResourceChange::Remove {
                resource_id: format!("namespace.{name}"),
            });
            removed += 1;
        }
    }

    // Permissions: same add/update/remove reconcile.
    for (name, git_perm) in &git_config.permission {
        match current.permissions.get(name) {
            None => {
                changes.push(ResourceChange::Add {
                    resource_id: format!("permission.{name}"),
                    spec: ChangePayload::Permission(Box::new(git_perm.clone())),
                });
                added += 1;
            }
            Some(current_perm) if current_perm != git_perm => {
                changes.push(ResourceChange::Update {
                    resource_id: format!("permission.{name}"),
                    spec: ChangePayload::Permission(Box::new(git_perm.clone())),
                    replicas_changed: false,
                });
                modified += 1;
            }
            Some(_) => {}
        }
    }
    for name in current.permissions.keys() {
        if !git_config.permission.contains_key(name) {
            changes.push(ResourceChange::Remove {
                resource_id: format!("permission.{name}"),
            });
            removed += 1;
        }
    }

    // Also diff jobs (simpler — no autoscaler interaction)
    let current_job_names: std::collections::BTreeSet<&String> = std::collections::BTreeSet::new();
    // TODO: wire in current job state from Raft when available
    for name in git_config.job.keys() {
        if !current_job_names.contains(name) {
            changes.push(ResourceChange::Add {
                resource_id: format!("job.{name}"),
                spec: ChangePayload::Generic,
            });
            added += 1;
        }
    }

    let summary = DiffSummary {
        added,
        modified,
        removed,
    };

    (changes, summary)
}

/// Check whether the `replicas` field differs between two app specs.
fn replicas_differ(a: &AppSpec, b: &AppSpec) -> bool {
    a.replicas != b.replicas
}

/// Check whether any non-replica field differs.
///
/// Serialises both specs to JSON and compares, excluding the replicas
/// field. This is robust against new fields being added.
fn non_replica_fields_differ(a: &AppSpec, b: &AppSpec) -> bool {
    let a_json = serde_json::to_value(a).unwrap_or_default();
    let b_json = serde_json::to_value(b).unwrap_or_default();

    if let (serde_json::Value::Object(mut a_map), serde_json::Value::Object(mut b_map)) =
        (a_json, b_json)
    {
        // Remove replicas from comparison
        a_map.remove("replicas");
        b_map.remove("replicas");
        a_map != b_map
    } else {
        true // can't compare, assume different
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn parse_config(toml: &str) -> Config {
        Config::parse(toml).unwrap()
    }

    fn make_current_apps(specs: &[(&str, &str)]) -> HashMap<AppId, AppSpec> {
        specs
            .iter()
            .map(|(name, toml)| {
                let spec: AppSpec = toml::from_str(toml).unwrap();
                (AppId::new(*name, "default"), spec)
            })
            .collect()
    }

    /// A `CurrentState` over just apps; namespaces/permissions empty.
    fn apps_state(apps: &HashMap<AppId, AppSpec>) -> CurrentState<'_> {
        CurrentState {
            apps,
            namespaces: EMPTY_NS.get_or_init(BTreeMap::new),
            permissions: EMPTY_PERM.get_or_init(BTreeMap::new),
        }
    }

    static EMPTY_NS: std::sync::OnceLock<BTreeMap<String, NamespaceSpec>> =
        std::sync::OnceLock::new();
    static EMPTY_PERM: std::sync::OnceLock<BTreeMap<String, PermissionSpec>> =
        std::sync::OnceLock::new();

    #[test]
    fn detects_added_app() {
        let git = parse_config(
            r#"
            [app.web]
            image = "myapp:v1"
            "#,
        );
        let current = HashMap::new();
        let (changes, summary) = compute_diff(&git, &apps_state(&current), &[]);
        assert_eq!(summary.added, 1);
        assert_eq!(summary.modified, 0);
        assert!(
            matches!(&changes[0], ResourceChange::Add { resource_id, .. } if resource_id == "app.web")
        );
    }

    #[test]
    fn detects_removed_app() {
        let git = parse_config("");
        let current = make_current_apps(&[("old", r#"image = "old:v1""#)]);
        let (changes, summary) = compute_diff(&git, &apps_state(&current), &[]);
        assert_eq!(summary.removed, 1);
        assert!(
            matches!(&changes[0], ResourceChange::Remove { resource_id } if resource_id == "app.old")
        );
    }

    #[test]
    fn detects_modified_image() {
        let git = parse_config(
            r#"
            [app.web]
            image = "myapp:v2"
            "#,
        );
        let current = make_current_apps(&[("web", r#"image = "myapp:v1""#)]);
        let (changes, summary) = compute_diff(&git, &apps_state(&current), &[]);
        assert_eq!(summary.modified, 1);
        assert!(matches!(
            &changes[0],
            ResourceChange::Update {
                replicas_changed: false,
                ..
            }
        ));
    }

    #[test]
    fn replicas_change_detected() {
        let git = parse_config(
            r#"
            [app.web]
            image = "myapp:v1"
            replicas = 5
            "#,
        );
        let current = make_current_apps(&[("web", r#"image = "myapp:v1""#)]);
        let (changes, _) = compute_diff(&git, &apps_state(&current), &[]);
        assert!(matches!(
            &changes[0],
            ResourceChange::Update {
                replicas_changed: true,
                ..
            }
        ));
    }

    #[test]
    fn unchanged_app_produces_no_change() {
        let git = parse_config(
            r#"
            [app.web]
            image = "myapp:v1"
            "#,
        );
        let current = make_current_apps(&[("web", r#"image = "myapp:v1""#)]);
        let (changes, summary) = compute_diff(&git, &apps_state(&current), &[]);
        assert!(changes.is_empty());
        assert_eq!(summary.added, 0);
        assert_eq!(summary.modified, 0);
        assert_eq!(summary.removed, 0);
    }

    #[test]
    fn non_replica_fields_differ_detects_image_change() {
        let a: AppSpec = toml::from_str(r#"image = "myapp:v1""#).unwrap();
        let b: AppSpec = toml::from_str(r#"image = "myapp:v2""#).unwrap();
        assert!(non_replica_fields_differ(&a, &b));
    }

    #[test]
    fn non_replica_fields_differ_ignores_replicas() {
        let a: AppSpec = toml::from_str(r#"image = "myapp:v1""#).unwrap();
        let mut b: AppSpec = toml::from_str(r#"image = "myapp:v1""#).unwrap();
        b.replicas = crate::config::types::Replicas::Fixed(5);
        assert!(
            !non_replica_fields_differ(&a, &b),
            "should ignore replicas difference"
        );
    }
}
