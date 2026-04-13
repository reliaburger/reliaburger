/// Structural diff between two Reliaburger configs.
///
/// Compares two `Config` values field-by-field to show what has been
/// added, modified, or removed. Used by `relish diff` and by the
/// Lettuce GitOps engine to compute changes before applying.
use std::collections::BTreeMap;

use serde::Serialize;

use crate::config::Config;

/// The complete diff between two configs.
#[derive(Debug, Clone, Serialize)]
pub struct ConfigDiff {
    /// Resources that exist in `new` but not `old`.
    pub added: Vec<ResourceDiff>,
    /// Resources that exist in both but differ.
    pub modified: Vec<ResourceDiff>,
    /// Resources that exist in `old` but not `new`.
    pub removed: Vec<ResourceDiff>,
}

impl ConfigDiff {
    /// True if the two configs are identical.
    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.modified.is_empty() && self.removed.is_empty()
    }
}

/// A diff for a single resource (app, job, namespace, etc.).
#[derive(Debug, Clone, Serialize)]
pub struct ResourceDiff {
    /// Resource identifier, e.g. "app.web" or "job.migrate".
    pub resource: String,
    /// Per-field changes (only present for modified resources).
    pub field_changes: Vec<FieldDiff>,
}

/// A single field change within a resource.
#[derive(Debug, Clone, Serialize)]
pub struct FieldDiff {
    /// Dotted field path, e.g. "image", "replicas", "health.path".
    pub path: String,
    /// Old value (None for added fields).
    pub old: Option<String>,
    /// New value (None for removed fields).
    pub new: Option<String>,
}

/// Compute a structural diff between two configs.
pub fn diff_configs(old: &Config, new: &Config) -> ConfigDiff {
    let mut added = Vec::new();
    let mut modified = Vec::new();
    let mut removed = Vec::new();

    // Diff apps
    diff_resource_map(
        &old.app,
        &new.app,
        "app",
        &mut added,
        &mut modified,
        &mut removed,
    );

    // Diff jobs
    diff_resource_map(
        &old.job,
        &new.job,
        "job",
        &mut added,
        &mut modified,
        &mut removed,
    );

    // Diff namespaces
    diff_resource_map(
        &old.namespace,
        &new.namespace,
        "namespace",
        &mut added,
        &mut modified,
        &mut removed,
    );

    // Diff permissions
    diff_resource_map(
        &old.permission,
        &new.permission,
        "permission",
        &mut added,
        &mut modified,
        &mut removed,
    );

    // Diff builds
    diff_resource_map(
        &old.build,
        &new.build,
        "build",
        &mut added,
        &mut modified,
        &mut removed,
    );

    ConfigDiff {
        added,
        modified,
        removed,
    }
}

/// Diff two BTreeMaps of serialisable resources.
fn diff_resource_map<T: Serialize>(
    old: &BTreeMap<String, T>,
    new: &BTreeMap<String, T>,
    prefix: &str,
    added: &mut Vec<ResourceDiff>,
    modified: &mut Vec<ResourceDiff>,
    removed: &mut Vec<ResourceDiff>,
) {
    // Check for added and modified
    for (name, new_val) in new {
        let resource = format!("{prefix}.{name}");
        match old.get(name) {
            None => {
                added.push(ResourceDiff {
                    resource,
                    field_changes: Vec::new(),
                });
            }
            Some(old_val) => {
                let changes = diff_values(old_val, new_val);
                if !changes.is_empty() {
                    modified.push(ResourceDiff {
                        resource,
                        field_changes: changes,
                    });
                }
            }
        }
    }

    // Check for removed
    for name in old.keys() {
        if !new.contains_key(name) {
            removed.push(ResourceDiff {
                resource: format!("{prefix}.{name}"),
                field_changes: Vec::new(),
            });
        }
    }
}

/// Diff two serialisable values by comparing their TOML representations.
///
/// Serialises both to TOML tables and compares field by field. This is
/// intentionally simple: it catches all value changes without needing
/// to know the internal structure of each spec type.
fn diff_values<T: Serialize>(old: &T, new: &T) -> Vec<FieldDiff> {
    let old_map = to_flat_map(old);
    let new_map = to_flat_map(new);

    let mut changes = Vec::new();

    // Fields in new that differ from old
    for (key, new_val) in &new_map {
        match old_map.get(key) {
            None => {
                changes.push(FieldDiff {
                    path: key.clone(),
                    old: None,
                    new: Some(new_val.clone()),
                });
            }
            Some(old_val) if old_val != new_val => {
                changes.push(FieldDiff {
                    path: key.clone(),
                    old: Some(old_val.clone()),
                    new: Some(new_val.clone()),
                });
            }
            _ => {}
        }
    }

    // Fields removed in new
    for key in old_map.keys() {
        if !new_map.contains_key(key) {
            changes.push(FieldDiff {
                path: key.clone(),
                old: Some(old_map[key].clone()),
                new: None,
            });
        }
    }

    changes.sort_by(|a, b| a.path.cmp(&b.path));
    changes
}

/// Serialise a value to a flat map of dotted-path → string pairs.
///
/// Uses serde_json as an intermediate format to walk the structure,
/// since TOML doesn't have a convenient visitor API.
fn to_flat_map<T: Serialize>(value: &T) -> BTreeMap<String, String> {
    let json = match serde_json::to_value(value) {
        Ok(v) => v,
        Err(_) => return BTreeMap::new(),
    };
    let mut map = BTreeMap::new();
    flatten_json("", &json, &mut map);
    map
}

/// Recursively flatten a JSON value into dotted-path keys.
fn flatten_json(prefix: &str, value: &serde_json::Value, out: &mut BTreeMap<String, String>) {
    match value {
        serde_json::Value::Object(obj) => {
            for (k, v) in obj {
                let key = if prefix.is_empty() {
                    k.clone()
                } else {
                    format!("{prefix}.{k}")
                };
                flatten_json(&key, v, out);
            }
        }
        serde_json::Value::Array(arr) => {
            // Serialise arrays as their JSON string for comparison
            out.insert(
                prefix.to_string(),
                serde_json::to_string(arr).unwrap_or_default(),
            );
        }
        serde_json::Value::Null => {
            // Skip null values (Option::None)
        }
        other => {
            out.insert(prefix.to_string(), format_json_scalar(other));
        }
    }
}

/// Format a JSON scalar as a human-readable string.
fn format_json_scalar(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::Bool(b) => b.to_string(),
        _ => value.to_string(),
    }
}

/// Display a `ConfigDiff` in a human-readable format.
impl std::fmt::Display for ConfigDiff {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.is_empty() {
            return write!(f, "no changes");
        }

        for diff in &self.added {
            writeln!(f, "+ {}", diff.resource)?;
        }

        for diff in &self.modified {
            writeln!(f, "~ {}", diff.resource)?;
            for change in &diff.field_changes {
                match (&change.old, &change.new) {
                    (Some(old), Some(new)) => {
                        writeln!(f, "    {}: {} -> {}", change.path, old, new)?;
                    }
                    (None, Some(new)) => {
                        writeln!(f, "    + {}: {}", change.path, new)?;
                    }
                    (Some(old), None) => {
                        writeln!(f, "    - {}: {}", change.path, old)?;
                    }
                    (None, None) => {}
                }
            }
        }

        for diff in &self.removed {
            writeln!(f, "- {}", diff.resource)?;
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(toml: &str) -> Config {
        Config::parse(toml).unwrap()
    }

    #[test]
    fn diff_detects_added_app() {
        let old = parse("");
        let new = parse(
            r#"
            [app.web]
            image = "myapp:v1"
            "#,
        );
        let diff = diff_configs(&old, &new);
        assert_eq!(diff.added.len(), 1);
        assert_eq!(diff.added[0].resource, "app.web");
        assert!(diff.modified.is_empty());
        assert!(diff.removed.is_empty());
    }

    #[test]
    fn diff_detects_removed_job() {
        let old = parse(
            r#"
            [job.migrate]
            image = "migrate:v1"
            "#,
        );
        let new = parse("");
        let diff = diff_configs(&old, &new);
        assert!(diff.added.is_empty());
        assert!(diff.modified.is_empty());
        assert_eq!(diff.removed.len(), 1);
        assert_eq!(diff.removed[0].resource, "job.migrate");
    }

    #[test]
    fn diff_detects_modified_field() {
        let old = parse(
            r#"
            [app.web]
            image = "myapp:v1"
            replicas = 3
            "#,
        );
        let new = parse(
            r#"
            [app.web]
            image = "myapp:v2"
            replicas = 3
            "#,
        );
        let diff = diff_configs(&old, &new);
        assert!(diff.added.is_empty());
        assert_eq!(diff.modified.len(), 1);
        assert_eq!(diff.modified[0].resource, "app.web");

        let image_change = diff.modified[0]
            .field_changes
            .iter()
            .find(|c| c.path == "image")
            .expect("should detect image change");
        assert_eq!(image_change.old.as_deref(), Some("myapp:v1"));
        assert_eq!(image_change.new.as_deref(), Some("myapp:v2"));
    }

    #[test]
    fn diff_unchanged_returns_empty() {
        let config = parse(
            r#"
            [app.web]
            image = "myapp:v1"
            replicas = 3
            "#,
        );
        let diff = diff_configs(&config, &config);
        assert!(diff.is_empty());
    }

    #[test]
    fn diff_mixed_operations() {
        let old = parse(
            r#"
            [app.web]
            image = "myapp:v1"
            [app.old]
            image = "old:v1"
            "#,
        );
        let new = parse(
            r#"
            [app.web]
            image = "myapp:v2"
            [app.new]
            image = "new:v1"
            "#,
        );
        let diff = diff_configs(&old, &new);
        assert_eq!(diff.added.len(), 1, "new app should be added");
        assert_eq!(diff.modified.len(), 1, "web should be modified");
        assert_eq!(diff.removed.len(), 1, "old should be removed");
    }

    #[test]
    fn diff_display_format() {
        let old = parse(
            r#"
            [app.web]
            image = "myapp:v1"
            "#,
        );
        let new = parse(
            r#"
            [app.web]
            image = "myapp:v2"
            "#,
        );
        let diff = diff_configs(&old, &new);
        let output = diff.to_string();
        assert!(output.contains("~ app.web"), "got:\n{output}");
        assert!(output.contains("image:"), "got:\n{output}");
        assert!(output.contains("myapp:v1"), "got:\n{output}");
        assert!(output.contains("myapp:v2"), "got:\n{output}");
    }

    #[test]
    fn diff_serialises_to_json() {
        let old = parse("");
        let new = parse(
            r#"
            [app.web]
            image = "myapp:v1"
            "#,
        );
        let diff = diff_configs(&old, &new);
        let json = serde_json::to_string(&diff).unwrap();
        assert!(json.contains("app.web"));
    }
}
