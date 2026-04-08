/// Apply plan generation.
///
/// Takes a parsed `Config` and produces a plan showing what would be deployed.
/// When current state is available, the plan diffs against it to show creates,
/// updates, destroys, and unchanged resources.
use std::collections::BTreeSet;
use std::fmt;

use serde::Serialize;

use crate::config::Config;

/// What action will be taken on a resource.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanAction {
    Create,
    Update,
    Destroy,
    Unchanged,
}

impl fmt::Display for PlanAction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PlanAction::Create => write!(f, "+"),
            PlanAction::Update => write!(f, "~"),
            PlanAction::Destroy => write!(f, "-"),
            PlanAction::Unchanged => write!(f, " "),
        }
    }
}

impl Serialize for PlanAction {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            PlanAction::Create => serializer.serialize_str("create"),
            PlanAction::Update => serializer.serialize_str("update"),
            PlanAction::Destroy => serializer.serialize_str("destroy"),
            PlanAction::Unchanged => serializer.serialize_str("unchanged"),
        }
    }
}

/// A single resource in the apply plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PlanEntry {
    /// Resource identifier, e.g. "app.web" or "job.db-migrate".
    pub resource: String,
    /// Action to take.
    pub action: PlanAction,
    /// Key-value summary fields describing the resource.
    pub summary: Vec<(String, String)>,
}

/// The complete apply plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ApplyPlan {
    pub entries: Vec<PlanEntry>,
    pub to_create: usize,
    pub to_update: usize,
    pub to_destroy: usize,
    pub unchanged: usize,
}

/// Snapshot of a currently deployed resource for diffing.
///
/// The `image` field is used to detect updates (image change).
#[derive(Debug, Clone)]
pub struct CurrentResource {
    /// Resource identifier matching the plan format, e.g. "app.web".
    pub resource: String,
    /// Current image, if applicable.
    pub image: Option<String>,
}

/// Generate an apply plan from a parsed config.
///
/// When `current` is `None`, all entries are `Create` (single-node, no prior
/// state). When `Some`, the plan diffs against the current state.
pub fn generate_plan(config: &Config, current: Option<&[CurrentResource]>) -> ApplyPlan {
    let mut entries = Vec::new();
    let mut desired_resources = BTreeSet::new();

    // Build a lookup of current resources
    let current_map: std::collections::HashMap<&str, &CurrentResource> = current
        .unwrap_or(&[])
        .iter()
        .map(|r| (r.resource.as_str(), r))
        .collect();

    // Apps
    for (name, app) in &config.app {
        let resource_key = format!("app.{name}");
        desired_resources.insert(resource_key.clone());

        let mut summary = Vec::new();
        if let Some(ref image) = app.image {
            summary.push(("image".to_string(), image.clone()));
        }
        summary.push(("replicas".to_string(), app.replicas.to_string()));
        if let Some(port) = app.port {
            summary.push(("port".to_string(), port.to_string()));
        }
        if let Some(ref health) = app.health {
            summary.push(("health".to_string(), health.path.clone()));
        }
        if let Some(ref memory) = app.memory {
            summary.push(("memory".to_string(), memory.to_string()));
        }
        if let Some(ref cpu) = app.cpu {
            summary.push(("cpu".to_string(), cpu.to_string()));
        }
        if let Some(ref namespace) = app.namespace {
            summary.push(("namespace".to_string(), namespace.clone()));
        }

        let action = match current_map.get(resource_key.as_str()) {
            None => PlanAction::Create,
            Some(existing) => {
                if existing.image.as_deref() != app.image.as_deref() {
                    PlanAction::Update
                } else {
                    PlanAction::Unchanged
                }
            }
        };

        entries.push(PlanEntry {
            resource: resource_key,
            action,
            summary,
        });
    }

    // Jobs
    for (name, job) in &config.job {
        let resource_key = format!("job.{name}");
        desired_resources.insert(resource_key.clone());

        let mut summary = Vec::new();
        if let Some(ref image) = job.image {
            summary.push(("image".to_string(), image.clone()));
        }
        if let Some(ref command) = job.command {
            summary.push(("command".to_string(), command.join(" ")));
        }
        if let Some(ref schedule) = job.schedule {
            summary.push(("schedule".to_string(), schedule.clone()));
        }

        let action = match current_map.get(resource_key.as_str()) {
            None => PlanAction::Create,
            Some(existing) => {
                if existing.image.as_deref() != job.image.as_deref() {
                    PlanAction::Update
                } else {
                    PlanAction::Unchanged
                }
            }
        };

        entries.push(PlanEntry {
            resource: resource_key,
            action,
            summary,
        });
    }

    // Namespaces
    for name in config.namespace.keys() {
        let resource_key = format!("namespace.{name}");
        desired_resources.insert(resource_key.clone());

        let action = if current_map.contains_key(resource_key.as_str()) {
            PlanAction::Unchanged
        } else {
            PlanAction::Create
        };

        entries.push(PlanEntry {
            resource: resource_key,
            action,
            summary: Vec::new(),
        });
    }

    // Permissions
    for name in config.permission.keys() {
        let resource_key = format!("permission.{name}");
        desired_resources.insert(resource_key.clone());

        let action = if current_map.contains_key(resource_key.as_str()) {
            PlanAction::Unchanged
        } else {
            PlanAction::Create
        };

        entries.push(PlanEntry {
            resource: resource_key,
            action,
            summary: Vec::new(),
        });
    }

    // Resources in current state but not in desired config → Destroy
    if let Some(current_list) = current {
        for existing in current_list {
            if !desired_resources.contains(&existing.resource) {
                entries.push(PlanEntry {
                    resource: existing.resource.clone(),
                    action: PlanAction::Destroy,
                    summary: Vec::new(),
                });
            }
        }
    }

    let to_create = entries
        .iter()
        .filter(|e| e.action == PlanAction::Create)
        .count();
    let to_update = entries
        .iter()
        .filter(|e| e.action == PlanAction::Update)
        .count();
    let to_destroy = entries
        .iter()
        .filter(|e| e.action == PlanAction::Destroy)
        .count();
    let unchanged = entries
        .iter()
        .filter(|e| e.action == PlanAction::Unchanged)
        .count();

    ApplyPlan {
        entries,
        to_create,
        to_update,
        to_destroy,
        unchanged,
    }
}

impl fmt::Display for ApplyPlan {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.entries.is_empty() {
            writeln!(f, "Relish apply plan:")?;
            writeln!(f)?;
            write!(f, "Plan: 0 to create, 0 to update, 0 to destroy.")?;
            return Ok(());
        }

        writeln!(f, "Relish apply plan:")?;

        for entry in &self.entries {
            // Skip unchanged entries in display to reduce noise
            if entry.action == PlanAction::Unchanged {
                continue;
            }
            writeln!(f)?;
            writeln!(f, "  {} {}", entry.action, entry.resource)?;
            for (key, value) in &entry.summary {
                writeln!(f, "      {key:9} {value}")?;
            }
        }

        write!(
            f,
            "Plan: {} to create, {} to update, {} to destroy.",
            self.to_create, self.to_update, self.to_destroy
        )?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_config(toml: &str) -> Config {
        Config::parse(toml).unwrap()
    }

    #[test]
    fn empty_config_produces_empty_plan() {
        let plan = generate_plan(&Config::default(), None);
        assert!(plan.entries.is_empty());
        assert_eq!(plan.to_create, 0);
        assert_eq!(plan.to_update, 0);
        assert_eq!(plan.to_destroy, 0);
        assert_eq!(plan.unchanged, 0);
    }

    #[test]
    fn single_app_produces_one_create_entry() {
        let config = parse_config(
            r#"
            [app.web]
            image = "myapp:v1"
        "#,
        );
        let plan = generate_plan(&config, None);
        assert_eq!(plan.entries.len(), 1);
        assert_eq!(plan.entries[0].resource, "app.web");
        assert_eq!(plan.entries[0].action, PlanAction::Create);
        assert_eq!(plan.to_create, 1);
    }

    #[test]
    fn app_summary_includes_image() {
        let config = parse_config(
            r#"
            [app.web]
            image = "myapp:v1.4.2"
        "#,
        );
        let plan = generate_plan(&config, None);
        let entry = &plan.entries[0];
        assert!(
            entry
                .summary
                .contains(&("image".to_string(), "myapp:v1.4.2".to_string())),
            "summary should contain image, got: {:?}",
            entry.summary
        );
    }

    #[test]
    fn app_summary_includes_replicas_port_health() {
        let config = parse_config(
            r#"
            [app.web]
            image = "myapp:v1"
            replicas = 3
            port = 8080

            [app.web.health]
            path = "/healthz"
        "#,
        );
        let plan = generate_plan(&config, None);
        let summary = &plan.entries[0].summary;
        assert!(summary.contains(&("replicas".to_string(), "3".to_string())));
        assert!(summary.contains(&("port".to_string(), "8080".to_string())));
        assert!(summary.contains(&("health".to_string(), "/healthz".to_string())));
    }

    #[test]
    fn multiple_apps_ordered_alphabetically() {
        let config = parse_config(
            r#"
            [app.zebra]
            image = "z:v1"
            [app.alpha]
            image = "a:v1"
        "#,
        );
        let plan = generate_plan(&config, None);
        assert_eq!(plan.entries.len(), 2);
        // BTreeMap orders alphabetically
        assert_eq!(plan.entries[0].resource, "app.alpha");
        assert_eq!(plan.entries[1].resource, "app.zebra");
    }

    #[test]
    fn job_included_with_command_and_schedule() {
        let config = parse_config(
            r#"
            [job.db-migrate]
            image = "myapp:v1"
            command = ["npm", "run", "migrate"]

            [job.cleanup]
            image = "cleanup:latest"
            schedule = "0 3 * * *"
        "#,
        );
        let plan = generate_plan(&config, None);
        assert_eq!(plan.entries.len(), 2);

        let migrate = plan
            .entries
            .iter()
            .find(|e| e.resource == "job.db-migrate")
            .unwrap();
        assert!(
            migrate
                .summary
                .contains(&("command".to_string(), "npm run migrate".to_string()))
        );

        let cleanup = plan
            .entries
            .iter()
            .find(|e| e.resource == "job.cleanup")
            .unwrap();
        assert!(
            cleanup
                .summary
                .contains(&("schedule".to_string(), "0 3 * * *".to_string()))
        );
    }

    #[test]
    fn namespace_included() {
        let config = parse_config(
            r#"
            [namespace.backend]
            cpu = "8000m"
        "#,
        );
        let plan = generate_plan(&config, None);
        assert_eq!(plan.entries.len(), 1);
        assert_eq!(plan.entries[0].resource, "namespace.backend");
        assert_eq!(plan.entries[0].action, PlanAction::Create);
    }

    #[test]
    fn permission_included() {
        let config = parse_config(
            r#"
            [permission.deployer]
            actions = ["deploy"]
            apps = ["web"]
        "#,
        );
        let plan = generate_plan(&config, None);
        assert_eq!(plan.entries.len(), 1);
        assert_eq!(plan.entries[0].resource, "permission.deployer");
    }

    #[test]
    fn whitepaper_example_correct_count() {
        let config = parse_config(
            r#"
            [app.web]
            image = "myapp:v1.4.2"
            replicas = 3
            port = 8080

            [app.web.health]
            path = "/healthz"

            [app.redis]
            image = "redis:7-alpine"
            port = 6379

            [job.db-migrate]
            image = "myapp:v1.4.2"
            command = ["npm", "run", "migrate"]

            [job.cleanup]
            image = "cleanup:latest"
            schedule = "0 3 * * *"

            [namespace.team-backend]
            cpu = "8000m"
            memory = "16Gi"

            [permission.deployer]
            actions = ["deploy", "scale"]
            apps = ["web", "api"]
        "#,
        );
        let plan = generate_plan(&config, None);
        // 2 apps + 2 jobs + 1 namespace + 1 permission = 6
        assert_eq!(plan.to_create, 6);
        assert_eq!(plan.entries.len(), 6);
    }

    #[test]
    fn display_uses_plus_prefix() {
        let config = parse_config(
            r#"
            [app.web]
            image = "myapp:v1"
        "#,
        );
        let plan = generate_plan(&config, None);
        let output = plan.to_string();
        assert!(output.contains("+ app.web"), "got:\n{output}");
    }

    #[test]
    fn display_shows_summary_fields() {
        let config = parse_config(
            r#"
            [app.web]
            image = "myapp:v1.4.2"
            replicas = 3
            port = 8080

            [app.web.health]
            path = "/healthz"
        "#,
        );
        let plan = generate_plan(&config, None);
        let output = plan.to_string();
        assert!(output.contains("image"), "got:\n{output}");
        assert!(output.contains("myapp:v1.4.2"), "got:\n{output}");
        assert!(output.contains("replicas"), "got:\n{output}");
        assert!(output.contains("/healthz"), "got:\n{output}");
    }

    #[test]
    fn display_shows_plan_summary_line() {
        let config = parse_config(
            r#"
            [app.web]
            image = "myapp:v1"
            [app.api]
            image = "api:v1"
        "#,
        );
        let plan = generate_plan(&config, None);
        let output = plan.to_string();
        assert!(
            output.contains("Plan: 2 to create, 0 to update, 0 to destroy."),
            "got:\n{output}"
        );
    }

    #[test]
    fn empty_plan_display() {
        let plan = generate_plan(&Config::default(), None);
        let output = plan.to_string();
        assert!(output.contains("Relish apply plan:"), "got:\n{output}");
        assert!(
            output.contains("Plan: 0 to create, 0 to update, 0 to destroy."),
            "got:\n{output}"
        );
    }

    #[test]
    fn plan_serialises_to_json() {
        let config = parse_config(
            r#"
            [app.web]
            image = "myapp:v1"
        "#,
        );
        let plan = generate_plan(&config, None);
        let json = serde_json::to_string(&plan).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed["entries"][0]["action"], "create");
        assert_eq!(parsed["entries"][0]["resource"], "app.web");
        assert_eq!(parsed["to_create"], 1);
    }

    #[test]
    fn app_summary_includes_memory_and_cpu() {
        let config = parse_config(
            r#"
            [app.web]
            image = "myapp:v1"
            memory = "128Mi-512Mi"
            cpu = "100m-500m"
        "#,
        );
        let plan = generate_plan(&config, None);
        let summary = &plan.entries[0].summary;
        assert!(summary.iter().any(|(k, _)| k == "memory"));
        assert!(summary.iter().any(|(k, _)| k == "cpu"));
    }

    // -- Diff-based plan tests ------------------------------------------------

    #[test]
    fn update_detected_when_image_changes() {
        let config = parse_config(
            r#"
            [app.web]
            image = "myapp:v2"
        "#,
        );
        let current = vec![CurrentResource {
            resource: "app.web".to_string(),
            image: Some("myapp:v1".to_string()),
        }];
        let plan = generate_plan(&config, Some(&current));
        assert_eq!(plan.entries[0].action, PlanAction::Update);
        assert_eq!(plan.to_update, 1);
        assert_eq!(plan.to_create, 0);
    }

    #[test]
    fn unchanged_when_spec_identical() {
        let config = parse_config(
            r#"
            [app.web]
            image = "myapp:v1"
        "#,
        );
        let current = vec![CurrentResource {
            resource: "app.web".to_string(),
            image: Some("myapp:v1".to_string()),
        }];
        let plan = generate_plan(&config, Some(&current));
        assert_eq!(plan.entries[0].action, PlanAction::Unchanged);
        assert_eq!(plan.unchanged, 1);
        assert_eq!(plan.to_create, 0);
    }

    #[test]
    fn destroy_detected_when_app_removed() {
        let config = parse_config(
            r#"
            [app.web]
            image = "myapp:v1"
        "#,
        );
        let current = vec![
            CurrentResource {
                resource: "app.web".to_string(),
                image: Some("myapp:v1".to_string()),
            },
            CurrentResource {
                resource: "app.old-service".to_string(),
                image: Some("old:v3".to_string()),
            },
        ];
        let plan = generate_plan(&config, Some(&current));
        let destroy = plan
            .entries
            .iter()
            .find(|e| e.action == PlanAction::Destroy);
        assert!(destroy.is_some());
        assert_eq!(destroy.unwrap().resource, "app.old-service");
        assert_eq!(plan.to_destroy, 1);
    }

    #[test]
    fn mixed_plan_with_create_update_destroy_unchanged() {
        let config = parse_config(
            r#"
            [app.web]
            image = "myapp:v2"
            [app.api]
            image = "api:v1"
            [app.new-service]
            image = "new:v1"
        "#,
        );
        let current = vec![
            CurrentResource {
                resource: "app.web".to_string(),
                image: Some("myapp:v1".to_string()),
            },
            CurrentResource {
                resource: "app.api".to_string(),
                image: Some("api:v1".to_string()),
            },
            CurrentResource {
                resource: "app.removed".to_string(),
                image: Some("old:v1".to_string()),
            },
        ];
        let plan = generate_plan(&config, Some(&current));

        assert_eq!(plan.to_create, 1); // new-service
        assert_eq!(plan.to_update, 1); // web (v1→v2)
        assert_eq!(plan.to_destroy, 1); // removed
        assert_eq!(plan.unchanged, 1); // api
    }

    #[test]
    fn display_uses_tilde_for_update() {
        let config = parse_config(
            r#"
            [app.web]
            image = "myapp:v2"
        "#,
        );
        let current = vec![CurrentResource {
            resource: "app.web".to_string(),
            image: Some("myapp:v1".to_string()),
        }];
        let plan = generate_plan(&config, Some(&current));
        let output = plan.to_string();
        assert!(output.contains("~ app.web"), "got:\n{output}");
    }

    #[test]
    fn display_uses_minus_for_destroy() {
        let config = Config::default();
        let current = vec![CurrentResource {
            resource: "app.old".to_string(),
            image: Some("old:v1".to_string()),
        }];
        let plan = generate_plan(&config, Some(&current));
        let output = plan.to_string();
        assert!(output.contains("- app.old"), "got:\n{output}");
    }

    #[test]
    fn display_hides_unchanged_entries() {
        let config = parse_config(
            r#"
            [app.web]
            image = "myapp:v1"
        "#,
        );
        let current = vec![CurrentResource {
            resource: "app.web".to_string(),
            image: Some("myapp:v1".to_string()),
        }];
        let plan = generate_plan(&config, Some(&current));
        let output = plan.to_string();
        // Unchanged entries should not appear in the display
        assert!(
            !output.contains("app.web"),
            "unchanged should be hidden, got:\n{output}"
        );
    }
}
