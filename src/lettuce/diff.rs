//! Diff engine for Lettuce GitOps.
//!
//! Computes the diff between the git-sourced config and the current
//! Raft desired state. Autoscaler-aware: the `replicas` field is
//! compared independently so autoscaler overrides aren't reset by
//! unrelated config changes.
//!
//! GitOps owns only what it wrote (D9, #693). A resource in Raft but not
//! in git is removed only when the ownership set says GitOps wrote it, so
//! `relish apply` and GitOps coexist. A running test's `rbtest-*`
//! namespace never enters the diff at all.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use crate::config::app::AppSpec;
use crate::config::{Config, NamespaceSpec, PermissionSpec, PolicySpec};
use crate::meat::types::AppId;
use crate::testkit::lease::valid_test_namespace;

use super::types::{DiffSummary, GitOpsOwnership};

/// The current declarative desired state a diff compares git against.
///
/// Grouping the maps keeps `compute_diff`'s signature from growing a
/// parameter per resource kind.
#[derive(Debug)]
pub struct CurrentState<'a> {
    /// Apps currently in Raft, keyed by identity.
    pub apps: &'a HashMap<AppId, AppSpec>,
    /// Namespaces currently in Raft, keyed by name.
    pub namespaces: &'a BTreeMap<String, NamespaceSpec>,
    /// Permissions currently in Raft, keyed by name.
    pub permissions: &'a BTreeMap<String, PermissionSpec>,
    /// Policies currently in Raft, keyed by name.
    pub policies: &'a BTreeMap<String, PolicySpec>,
    /// The resources GitOps wrote: the only ones it may remove.
    pub owned: &'a GitOpsOwnership,
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
    /// A resource that already matches git but that GitOps didn't write.
    /// It's written once, with the spec the cluster already runs, so the
    /// write changes nothing but records that GitOps owns it (#693).
    Adopt {
        resource_id: String,
        spec: ChangePayload,
    },
    /// A resource GitOps wrote that git no longer declares.
    Remove { resource_id: String },
}

/// The payload for an add or update change.
#[derive(Debug, Clone, PartialEq)]
pub enum ChangePayload {
    App(Box<AppSpec>),
    Namespace(Box<NamespaceSpec>),
    Permission(Box<PermissionSpec>),
    Policy(Box<PolicySpec>),
    /// Jobs (run to completion) and builds (dispatched imperatively) are
    /// not reconciled desired state, so their diff carries no spec.
    Generic,
}

/// The changes for one resource kind, kept apart so `compute_diff` can
/// order them: writes go namespaces, permissions, policies, apps; removals
/// go the other way, except that policies are removed before apps are
/// written.
#[derive(Default)]
struct KindDiff {
    writes: Vec<ResourceChange>,
    removals: Vec<ResourceChange>,
}

/// Compute the diff between git config and current Raft state.
///
/// Adds and updates come out namespaces first, then permissions, then
/// policies, then apps, the order manual apply writes them in, so a
/// namespace's quota exists before an app schedules against it. Removed
/// policies go before the apps too, so every app is judged by the
/// policies the repository declares at this commit (#1117). The other
/// removals follow, in the reverse order (#693). Only resources in `current.owned` are removed,
/// and nothing in an `rbtest-*` namespace appears at all.
///
/// The `autoscale_overrides` map is consulted to avoid resetting
/// the autoscaler's replica count when only non-replica fields changed.
pub fn compute_diff(
    git_config: &Config,
    current: &CurrentState<'_>,
    _autoscale_overrides: &[(String, u32)],
) -> (Vec<ResourceChange>, DiffSummary) {
    let mut summary = DiffSummary {
        added: 0,
        modified: 0,
        removed: 0,
    };

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
        .filter(|(app_id, _)| !valid_test_namespace(&app_id.namespace))
        .collect();
    let apps = diff_apps(&git_apps, current, &mut summary);

    let git_namespaces: BTreeMap<&String, &NamespaceSpec> = git_config
        .namespace
        .iter()
        .filter(|(name, _)| !valid_test_namespace(name))
        .collect();
    let current_namespaces: BTreeMap<&String, &NamespaceSpec> = current
        .namespaces
        .iter()
        .filter(|(name, _)| !valid_test_namespace(name))
        .collect();
    let namespaces = diff_named(
        "namespace",
        &git_namespaces,
        &current_namespaces,
        &current.owned.namespaces,
        |spec| ChangePayload::Namespace(Box::new(spec.clone())),
        &mut summary,
    );

    let git_permissions: BTreeMap<&String, &PermissionSpec> =
        git_config.permission.iter().collect();
    let current_permissions: BTreeMap<&String, &PermissionSpec> =
        current.permissions.iter().collect();
    let permissions = diff_named(
        "permission",
        &git_permissions,
        &current_permissions,
        &current.owned.permissions,
        |spec| ChangePayload::Permission(Box::new(spec.clone())),
        &mut summary,
    );

    let declared_policies: BTreeMap<&String, &PolicySpec> = git_config.policy.iter().collect();
    let current_policies: BTreeMap<&String, &PolicySpec> = current.policies.iter().collect();
    let policies = diff_named(
        "policy",
        &declared_policies,
        &current_policies,
        &current.owned.policies,
        |spec| ChangePayload::Policy(Box::new(spec.clone())),
        &mut summary,
    );

    // Jobs are absent from desired-state diffs: no reconciled job map exists
    // in Raft. `execute_sync` explicitly refuses any tree containing jobs
    // before reaching this function, so dependent apps cannot be published
    // while a migration is omitted. Manual apply owns job execution.

    let changes = namespaces
        .writes
        .into_iter()
        .chain(permissions.writes)
        .chain(policies.writes)
        .chain(policies.removals)
        .chain(apps.writes)
        .chain(apps.removals)
        .chain(permissions.removals)
        .chain(namespaces.removals)
        .collect();
    (changes, summary)
}

/// Diff git's apps against Raft's, outside `rbtest-*` namespaces.
fn diff_apps(
    git_apps: &BTreeMap<AppId, &AppSpec>,
    current: &CurrentState<'_>,
    summary: &mut DiffSummary,
) -> KindDiff {
    let mut diff = KindDiff::default();
    for (app_id, git_spec) in git_apps {
        let resource_id = app_resource_id(app_id);
        let Some(current_spec) = current.apps.get(app_id) else {
            diff.writes.push(ResourceChange::Add {
                resource_id,
                spec: ChangePayload::App(Box::new((*git_spec).clone())),
            });
            summary.added += 1;
            continue;
        };
        let as_git = as_git_wrote_it(git_spec, current_spec);
        let replicas_changed = replicas_differ(git_spec, &as_git);
        if replicas_changed || non_replica_fields_differ(git_spec, &as_git) {
            diff.writes.push(ResourceChange::Update {
                resource_id,
                spec: ChangePayload::App(Box::new((*git_spec).clone())),
                replicas_changed,
            });
            summary.modified += 1;
        } else if !current.owned.apps.contains(app_id) {
            diff.writes.push(ResourceChange::Adopt {
                resource_id,
                spec: ChangePayload::App(Box::new(current_spec.clone())),
            });
        }
    }

    // Removed apps: in Raft, owned by GitOps, gone from git. Keyed by full
    // identity so a removal deletes exactly the app git dropped. Sorted so
    // the order doesn't depend on the hash map.
    let mut removed: Vec<&AppId> = current
        .apps
        .keys()
        .filter(|app_id| {
            current.owned.apps.contains(*app_id)
                && !git_apps.contains_key(*app_id)
                && !valid_test_namespace(&app_id.namespace)
        })
        .collect();
    removed.sort();
    for app_id in removed {
        diff.removals.push(ResourceChange::Remove {
            resource_id: app_resource_id(app_id),
        });
        summary.removed += 1;
    }
    diff
}

/// Diff a kind keyed by name (namespaces, permissions, policies): add what git has
/// and Raft doesn't, update what differs, adopt what matches but GitOps
/// didn't write, remove what GitOps wrote and git dropped.
fn diff_named<S: PartialEq>(
    kind: &str,
    git: &BTreeMap<&String, &S>,
    current: &BTreeMap<&String, &S>,
    owned: &BTreeSet<String>,
    payload: impl Fn(&S) -> ChangePayload,
    summary: &mut DiffSummary,
) -> KindDiff {
    let mut diff = KindDiff::default();
    for (name, git_spec) in git {
        let resource_id = format!("{kind}.{name}");
        match current.get(name) {
            None => {
                diff.writes.push(ResourceChange::Add {
                    resource_id,
                    spec: payload(git_spec),
                });
                summary.added += 1;
            }
            Some(current_spec) if current_spec != git_spec => {
                diff.writes.push(ResourceChange::Update {
                    resource_id,
                    spec: payload(git_spec),
                    replicas_changed: false,
                });
                summary.modified += 1;
            }
            Some(current_spec) if !owned.contains(*name) => {
                diff.writes.push(ResourceChange::Adopt {
                    resource_id,
                    spec: payload(current_spec),
                });
            }
            Some(_) => {}
        }
    }
    for name in current.keys() {
        if owned.contains(*name) && !git.contains_key(name) {
            diff.removals.push(ResourceChange::Remove {
                resource_id: format!("{kind}.{name}"),
            });
            summary.removed += 1;
        }
    }
    diff
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

/// `current` with each image bound at apply read back as the tag Git
/// names. Raft holds `nginx:1.27@sha256:…` for Git's `nginx:1.27` (F03
/// U1); that's the binding, not drift. A tag that moves upstream is picked
/// up when Git changes the app, as a manual apply picks it up when it runs.
fn as_git_wrote_it(git: &AppSpec, current: &AppSpec) -> AppSpec {
    let mut current = current.clone();
    unbind(&mut current.image, git.image.as_deref());
    for (current_init, git_init) in current.init.iter_mut().zip(&git.init) {
        unbind(&mut current_init.image, git_init.image.as_deref());
    }
    current
}

/// Replace `image` with `git` when `image` is `git` bound to a digest.
fn unbind(image: &mut Option<String>, git: Option<&str>) {
    let Some(git) = git.filter(|git| !git.contains('@')) else {
        return;
    };
    if image
        .as_deref()
        .and_then(|image| image.split_once('@'))
        .is_some_and(|(tag, _)| tag == git)
    {
        *image = Some(git.to_string());
    }
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

    /// A `CurrentState` over just apps; namespaces/permissions empty, and
    /// every current app owned by GitOps.
    fn apps_state(apps: &HashMap<AppId, AppSpec>) -> CurrentState<'_> {
        let owned = GitOpsOwnership {
            apps: apps.keys().cloned().collect(),
            ..Default::default()
        };
        CurrentState {
            apps,
            namespaces: EMPTY_NS.get_or_init(BTreeMap::new),
            permissions: EMPTY_PERM.get_or_init(BTreeMap::new),
            policies: EMPTY_POLICIES.get_or_init(BTreeMap::new),
            owned: Box::leak(Box::new(owned)),
        }
    }

    static EMPTY_NS: std::sync::OnceLock<BTreeMap<String, NamespaceSpec>> =
        std::sync::OnceLock::new();
    static EMPTY_PERM: std::sync::OnceLock<BTreeMap<String, PermissionSpec>> =
        std::sync::OnceLock::new();
    static EMPTY_POLICIES: std::sync::OnceLock<BTreeMap<String, PolicySpec>> =
        std::sync::OnceLock::new();

    fn ids(changes: &[ResourceChange]) -> Vec<(&'static str, &str)> {
        changes
            .iter()
            .map(|change| match change {
                ResourceChange::Add { resource_id, .. } => ("add", resource_id.as_str()),
                ResourceChange::Update { resource_id, .. } => ("update", resource_id.as_str()),
                ResourceChange::Adopt { resource_id, .. } => ("adopt", resource_id.as_str()),
                ResourceChange::Remove { resource_id } => ("remove", resource_id.as_str()),
            })
            .collect()
    }

    /// D9, #693: GitOps removes only what it wrote. A manually applied app,
    /// namespace and permission absent from git are left alone; the owned
    /// ones absent from git are pruned.
    #[test]
    fn diff_never_removes_resources_gitops_did_not_write() {
        let git = parse_config("");
        let apps = make_current_apps(&[
            ("manual", r#"image = "m:v1""#),
            ("fromgit", r#"image = "g:v1""#),
        ]);
        let namespace: NamespaceSpec = toml::from_str(r#"cpu = "1000m""#).unwrap();
        let namespaces = BTreeMap::from([
            ("manual-ns".to_string(), namespace.clone()),
            ("git-ns".to_string(), namespace),
        ]);
        let permission = PermissionSpec {
            actions: vec!["deploy".into()],
            apps: vec![],
            namespaces: None,
        };
        let permissions = BTreeMap::from([
            ("manual-perm".to_string(), permission.clone()),
            ("git-perm".to_string(), permission),
        ]);
        let owned = GitOpsOwnership {
            apps: [AppId::new("fromgit", "default")].into(),
            namespaces: ["git-ns".to_string()].into(),
            permissions: ["git-perm".to_string()].into(),
            ..Default::default()
        };
        let current = CurrentState {
            apps: &apps,
            namespaces: &namespaces,
            permissions: &permissions,
            policies: EMPTY_POLICIES.get_or_init(BTreeMap::new),
            owned: &owned,
        };
        let (changes, summary) = compute_diff(&git, &current, &[]);
        assert_eq!(
            ids(&changes),
            vec![
                ("remove", "app.default/fromgit"),
                ("remove", "permission.git-perm"),
                ("remove", "namespace.git-ns"),
            ]
        );
        assert_eq!(summary.removed, 3);
    }

    /// #693: a running test's `rbtest-*` namespace, and everything in it,
    /// never enters the diff, whatever git or the ownership set says.
    #[test]
    fn diff_skips_test_lease_namespaces() {
        let git = parse_config(
            r#"
            [namespace.rbtest-abc]
            cpu = "1000m"
            [app.web]
            image = "web:v1"
            namespace = "rbtest-abc"
            "#,
        );
        let mut apps = HashMap::new();
        apps.insert(
            AppId::new("leased", "rbtest-run1"),
            toml::from_str::<AppSpec>(r#"image = "t:v1""#).unwrap(),
        );
        let namespaces = BTreeMap::from([(
            "rbtest-run1".to_string(),
            toml::from_str::<NamespaceSpec>(r#"cpu = "1000m""#).unwrap(),
        )]);
        let owned = GitOpsOwnership {
            apps: [AppId::new("leased", "rbtest-run1")].into(),
            namespaces: ["rbtest-run1".to_string()].into(),
            ..Default::default()
        };
        let current = CurrentState {
            apps: &apps,
            namespaces: &namespaces,
            permissions: EMPTY_PERM.get_or_init(BTreeMap::new),
            policies: EMPTY_POLICIES.get_or_init(BTreeMap::new),
            owned: &owned,
        };
        let (changes, summary) = compute_diff(&git, &current, &[]);
        assert!(changes.is_empty(), "{changes:?}");
        assert_eq!(
            summary,
            DiffSummary {
                added: 0,
                modified: 0,
                removed: 0
            }
        );
    }

    /// #1117: policies are written, and removed, before any app, so every
    /// app in a commit is judged by the policies that commit declares. Only
    /// a policy GitOps wrote is removed.
    #[test]
    fn diff_settles_policies_before_apps() {
        let declared = parse_config(
            r#"
            [policy.new-rule]
            deny = "false"
            [app.web]
            image = "web:v1"
            "#,
        );
        let rule = PolicySpec {
            match_expression: None,
            deny: "true".into(),
            message: None,
            action: crate::config::PolicyAction::Deny,
        };
        let policies = BTreeMap::from([
            ("old-rule".to_string(), rule.clone()),
            ("manual-rule".to_string(), rule),
        ]);
        let owned = GitOpsOwnership {
            policies: ["old-rule".to_string()].into(),
            ..Default::default()
        };
        let apps = HashMap::new();
        let current = CurrentState {
            apps: &apps,
            namespaces: EMPTY_NS.get_or_init(BTreeMap::new),
            permissions: EMPTY_PERM.get_or_init(BTreeMap::new),
            policies: &policies,
            owned: &owned,
        };
        let (changes, _) = compute_diff(&declared, &current, &[]);
        assert_eq!(
            ids(&changes),
            vec![
                ("add", "policy.new-rule"),
                ("remove", "policy.old-rule"),
                ("add", "app.default/web"),
            ]
        );
    }

    /// #693 (finding 5): adds and updates go namespaces, permissions, apps,
    /// so a quota exists before the apps that schedule against it; removals
    /// go the other way, apps first.
    #[test]
    fn diff_orders_namespaces_before_apps_and_removes_in_reverse() {
        let git = parse_config(
            r#"
            [namespace.team]
            cpu = "1000m"
            [permission.deployers]
            actions = ["deploy"]
            [app.web]
            image = "web:v1"
            namespace = "team"
            "#,
        );
        let apps = make_current_apps(&[("old", r#"image = "o:v1""#)]);
        let namespaces = BTreeMap::from([(
            "old-ns".to_string(),
            toml::from_str::<NamespaceSpec>(r#"cpu = "1000m""#).unwrap(),
        )]);
        let permissions = BTreeMap::from([(
            "old-perm".to_string(),
            PermissionSpec {
                actions: vec!["deploy".into()],
                apps: vec![],
                namespaces: None,
            },
        )]);
        let owned = GitOpsOwnership {
            apps: [AppId::new("old", "default")].into(),
            namespaces: ["old-ns".to_string()].into(),
            permissions: ["old-perm".to_string()].into(),
            ..Default::default()
        };
        let current = CurrentState {
            apps: &apps,
            namespaces: &namespaces,
            permissions: &permissions,
            policies: EMPTY_POLICIES.get_or_init(BTreeMap::new),
            owned: &owned,
        };
        let (changes, _) = compute_diff(&git, &current, &[]);
        assert_eq!(
            ids(&changes),
            vec![
                ("add", "namespace.team"),
                ("add", "permission.deployers"),
                ("add", "app.team/web"),
                ("remove", "app.default/old"),
                ("remove", "permission.old-perm"),
                ("remove", "namespace.old-ns"),
            ]
        );
    }

    /// #693: a resource that already matches git but that GitOps didn't
    /// write is adopted: written once, with the spec the cluster already
    /// runs (its bound image included), so GitOps owns it from then on.
    #[test]
    fn an_unowned_resource_matching_git_is_adopted_with_its_current_spec() {
        let digest = format!("sha256:{}", "a".repeat(64));
        let git = parse_config("[app.web]\nimage = \"myapp:v1\"\n");
        let apps = make_current_apps(&[("web", &format!("image = \"myapp:v1@{digest}\""))]);
        let owned = GitOpsOwnership::default();
        let current = CurrentState {
            apps: &apps,
            namespaces: EMPTY_NS.get_or_init(BTreeMap::new),
            permissions: EMPTY_PERM.get_or_init(BTreeMap::new),
            policies: EMPTY_POLICIES.get_or_init(BTreeMap::new),
            owned: &owned,
        };
        let (changes, summary) = compute_diff(&git, &current, &[]);
        assert!(
            matches!(
                &changes[..],
                [ResourceChange::Adopt { resource_id, spec: ChangePayload::App(spec) }]
                    if resource_id == "app.default/web"
                        && spec.image.as_deref() == Some(format!("myapp:v1@{digest}").as_str())
            ),
            "{changes:?}"
        );
        assert_eq!(
            summary,
            DiffSummary {
                added: 0,
                modified: 0,
                removed: 0
            }
        );
    }

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

    /// F03 U1: Raft holds `myapp:v1@sha256:…` where Git says `myapp:v1`.
    /// That's the binding, not drift, so a quiet repository stays quiet
    /// instead of re-resolving every tag on every poll.
    #[test]
    fn a_bound_image_is_not_drift_from_its_git_tag() {
        let digest = format!("sha256:{}", "a".repeat(64));
        let git = parse_config(
            r#"
            [app.web]
            image = "myapp:v1"
            [[app.web.init]]
            image = "busybox:1.36"
            "#,
        );
        let current = make_current_apps(&[(
            "web",
            &format!(
                "image = \"myapp:v1@{digest}\"\n[[init]]\nimage = \"busybox:1.36@{digest}\"\n"
            ),
        )]);
        let (changes, summary) = compute_diff(&git, &apps_state(&current), &[]);
        assert!(changes.is_empty(), "{changes:?}");
        assert_eq!(summary.modified, 0);
    }

    #[test]
    fn a_new_tag_in_git_still_changes_a_bound_image() {
        let digest = format!("sha256:{}", "a".repeat(64));
        let git = parse_config(
            r#"
            [app.web]
            image = "myapp:v2"
            "#,
        );
        let current = make_current_apps(&[("web", &format!("image = \"myapp:v1@{digest}\""))]);
        let (changes, summary) = compute_diff(&git, &apps_state(&current), &[]);
        assert_eq!(summary.modified, 1);
        assert!(matches!(
            &changes[0],
            ResourceChange::Update { spec: ChangePayload::App(spec), .. }
                if spec.image.as_deref() == Some("myapp:v2")
        ));
    }

    /// Git pinning a different digest is a change, even under the same tag.
    #[test]
    fn a_different_digest_pinned_in_git_is_a_change() {
        let git = parse_config(&format!(
            "[app.web]\nimage = \"myapp:v1@sha256:{}\"\n",
            "b".repeat(64)
        ));
        let current = make_current_apps(&[(
            "web",
            &format!("image = \"myapp:v1@sha256:{}\"", "a".repeat(64)),
        )]);
        let (_, summary) = compute_diff(&git, &apps_state(&current), &[]);
        assert_eq!(summary.modified, 1);
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
        // Git declares nothing; Raft holds prod/web and default/web, both
        // written by GitOps.
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
