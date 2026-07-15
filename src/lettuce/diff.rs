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

    // Apps diff on their full identity (namespace + name), not the bare
    // name. Two apps called `web` in `prod` and `staging` are different
    // resources; keying on the name alone let a `prod/web` deletion target
    // `default/web`, so GitOps and manual apply diverged (GIT2). The git
    // side derives the same `AppId` `config_to_desired_writes` does:
    // `namespace` field, defaulting to `default`.
    let git_apps: BTreeMap<AppId, &AppSpec> = git_config
        .app
        .iter()
        .map(|(name, spec)| (app_id_for(name, spec), spec))
        .collect();

    // Check for added and modified apps.
    for (app_id, git_spec) in &git_apps {
        match current.apps.get(app_id) {
            None => {
                changes.push(ResourceChange::Add {
                    resource_id: app_resource_id(app_id),
                    spec: ChangePayload::App(Box::new((*git_spec).clone())),
                });
                added += 1;
            }
            Some(current_spec) => {
                let replicas_changed = replicas_differ(git_spec, current_spec);
                let other_fields_changed = non_replica_fields_differ(git_spec, current_spec);

                if replicas_changed || other_fields_changed {
                    changes.push(ResourceChange::Update {
                        resource_id: app_resource_id(app_id),
                        spec: ChangePayload::App(Box::new((*git_spec).clone())),
                        replicas_changed,
                    });
                    modified += 1;
                }
            }
        }
    }

    // Check for removed apps (in current but not in git), keyed by full
    // identity so a removal deletes exactly the app git dropped.
    for app_id in current.apps.keys() {
        if !git_apps.contains_key(app_id) {
            changes.push(ResourceChange::Remove {
                resource_id: app_resource_id(app_id),
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

    // Jobs are deliberately absent from the diff. A job runs to
    // completion; it isn't reconciled desired state, so there's no job
    // map in Raft to compare against (see `config_to_desired_writes`).
    // The old code compared every git job against an always-empty set,
    // so it emitted an `Add` for every job on *every* sync — a change the
    // applier then silently dropped (`ChangePayload::Generic` maps to no
    // write) while inflating `summary.added`. That's the GIT2b bug: a job
    // "removed" from git was never in the desired state to begin with, so
    // it can't be re-added, and a job present in git is dispatched by the
    // one-shot deploy path, not by reconciliation. Emitting nothing here
    // keeps the summary honest and the applier free of no-op changes.

    let summary = DiffSummary {
        added,
        modified,
        removed,
    };

    (changes, summary)
}

/// The `AppId` a git-declared app resolves to.
///
/// Mirrors `config_to_desired_writes`: the app's own `namespace` field,
/// defaulting to `default`. Keeping the two derivations identical is what
/// makes GitOps and manual apply converge on the same identity.
fn app_id_for(name: &str, spec: &AppSpec) -> AppId {
    let namespace = spec.namespace.clone().unwrap_or_else(|| "default".into());
    AppId::new(name, namespace)
}

/// Encode an `AppId` into a diff `resource_id`.
///
/// The form is `app.<namespace>/<name>` so a removal carries the full
/// identity, not just the bare name. `parse_app_resource_id` is the
/// inverse.
pub fn app_resource_id(app_id: &AppId) -> String {
    format!("app.{}/{}", app_id.namespace, app_id.name)
}

/// Parse an `app.<namespace>/<name>` resource id back into an `AppId`.
///
/// Returns `None` for a resource id that isn't an app or doesn't carry a
/// namespace segment.
pub fn parse_app_resource_id(resource_id: &str) -> Option<AppId> {
    let rest = resource_id.strip_prefix("app.")?;
    let (namespace, name) = rest.split_once('/')?;
    if namespace.is_empty() || name.is_empty() {
        return None;
    }
    Some(AppId::new(name, namespace))
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
            matches!(&changes[0], ResourceChange::Add { resource_id, .. } if resource_id == "app.default/web")
        );
    }

    #[test]
    fn detects_removed_app() {
        let git = parse_config("");
        let current = make_current_apps(&[("old", r#"image = "old:v1""#)]);
        let (changes, summary) = compute_diff(&git, &apps_state(&current), &[]);
        assert_eq!(summary.removed, 1);
        assert!(
            matches!(&changes[0], ResourceChange::Remove { resource_id } if resource_id == "app.default/old")
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

    /// GIT2: a `prod/web` removed from git deletes `prod/web`, not
    /// `default/web`. Keying the diff on the bare name used to collapse the
    /// two, so a manual `default/web` was collateral damage of a `prod`
    /// change (or the wrong app survived).
    #[test]
    fn removing_a_namespaced_app_targets_that_namespace() {
        // Git declares nothing; Raft holds prod/web and default/web.
        let git = parse_config("");
        let mut current = HashMap::new();
        let prod_web: AppSpec = toml::from_str(r#"image = "web:v1""#).unwrap();
        let default_web: AppSpec = toml::from_str(r#"image = "web:v1""#).unwrap();
        current.insert(AppId::new("web", "prod"), prod_web);
        current.insert(AppId::new("web", "default"), default_web);

        let (changes, summary) = compute_diff(&git, &apps_state(&current), &[]);
        assert_eq!(summary.removed, 2, "both apps are absent from git");
        let removed: std::collections::BTreeSet<&str> = changes
            .iter()
            .filter_map(|c| match c {
                ResourceChange::Remove { resource_id } => Some(resource_id.as_str()),
                _ => None,
            })
            .collect();
        assert!(removed.contains("app.prod/web"));
        assert!(removed.contains("app.default/web"));
    }

    /// GIT2: two same-named apps in different namespaces converge
    /// independently. Git keeps `prod/web`, drops `staging/web`; only the
    /// staging one is removed and the prod one is untouched.
    #[test]
    fn same_named_apps_in_different_namespaces_converge_independently() {
        let git = parse_config(
            r#"
            [app.web]
            image = "web:v1"
            namespace = "prod"
            "#,
        );
        let mut current = HashMap::new();
        // The prod/web spec carries the same `namespace = "prod"` git
        // declares, so it's an exact match and produces no change.
        current.insert(
            AppId::new("web", "prod"),
            toml::from_str::<AppSpec>("image = \"web:v1\"\nnamespace = \"prod\"").unwrap(),
        );
        current.insert(
            AppId::new("web", "staging"),
            toml::from_str::<AppSpec>(r#"image = "web:v1""#).unwrap(),
        );

        let (changes, summary) = compute_diff(&git, &apps_state(&current), &[]);
        assert_eq!(summary.added, 0, "prod/web already matches");
        assert_eq!(summary.modified, 0, "prod/web is unchanged");
        assert_eq!(summary.removed, 1, "only staging/web is dropped");
        assert!(matches!(
            &changes[..],
            [ResourceChange::Remove { resource_id }] if resource_id == "app.staging/web"
        ));
    }

    /// GIT2b: a job in git produces no reconciled change. Jobs run to
    /// completion; the diff must not manufacture an `Add` the applier then
    /// silently drops (which also mis-inflated `summary.added`).
    #[test]
    fn jobs_produce_no_reconciled_change() {
        let git = parse_config(
            r#"
            [job.migrate]
            image = "migrate:v1"
            "#,
        );
        let current = HashMap::new();
        let (changes, summary) = compute_diff(&git, &apps_state(&current), &[]);
        assert!(changes.is_empty(), "jobs are not reconciled desired state");
        assert_eq!(summary.added, 0, "a job must not inflate the add count");
    }

    #[test]
    fn app_resource_id_round_trips_through_the_parser() {
        let id = AppId::new("web", "prod");
        let encoded = app_resource_id(&id);
        assert_eq!(encoded, "app.prod/web");
        assert_eq!(parse_app_resource_id(&encoded), Some(id));
        assert!(parse_app_resource_id("namespace.prod").is_none());
        assert!(
            parse_app_resource_id("app.web").is_none(),
            "needs a namespace segment"
        );
    }
}
