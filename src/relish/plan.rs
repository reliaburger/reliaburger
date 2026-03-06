/// Apply plan generation.
///
/// Takes a parsed `Config` and produces a plan showing what would be deployed.
/// In Phase 1 everything is `Create` because there's no cluster state to diff
/// against. Future phases add `Update`, `Destroy`, and `Unchanged`.
use std::fmt;

use serde::Serialize;

use crate::config::Config;

/// What action will be taken on a resource.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanAction {
    Create,
    // TODO(Phase 2): Update, Destroy, Unchanged
}

impl fmt::Display for PlanAction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PlanAction::Create => write!(f, "+"),
        }
    }
}

impl Serialize for PlanAction {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            PlanAction::Create => serializer.serialize_str("create"),
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

/// Generate an apply plan from a parsed config.
///
/// In Phase 1 every resource is new, so all entries have `PlanAction::Create`.
pub fn generate_plan(config: &Config) -> ApplyPlan {
    let mut entries = Vec::new();

    // Apps
    for (name, app) in &config.app {
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

        entries.push(PlanEntry {
            resource: format!("app.{name}"),
            action: PlanAction::Create,
            summary,
        });
    }

    // Jobs
    for (name, job) in &config.job {
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

        entries.push(PlanEntry {
            resource: format!("job.{name}"),
            action: PlanAction::Create,
            summary,
        });
    }

    // Namespaces
    for name in config.namespace.keys() {
        entries.push(PlanEntry {
            resource: format!("namespace.{name}"),
            action: PlanAction::Create,
            summary: Vec::new(),
        });
    }

    // Permissions
    for name in config.permission.keys() {
        entries.push(PlanEntry {
            resource: format!("permission.{name}"),
            action: PlanAction::Create,
            summary: Vec::new(),
        });
    }

    let to_create = entries.len();
    ApplyPlan {
        entries,
        to_create,
        to_update: 0,
        to_destroy: 0,
        unchanged: 0,
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
        let plan = generate_plan(&Config::default());
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
        let plan = generate_plan(&config);
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
        let plan = generate_plan(&config);
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
        let plan = generate_plan(&config);
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
        let plan = generate_plan(&config);
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
        let plan = generate_plan(&config);
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
        let plan = generate_plan(&config);
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
        let plan = generate_plan(&config);
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
        let plan = generate_plan(&config);
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
        let plan = generate_plan(&config);
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
        let plan = generate_plan(&config);
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
        let plan = generate_plan(&config);
        let output = plan.to_string();
        assert!(
            output.contains("Plan: 2 to create, 0 to update, 0 to destroy."),
            "got:\n{output}"
        );
    }

    #[test]
    fn empty_plan_display() {
        let plan = generate_plan(&Config::default());
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
        let plan = generate_plan(&config);
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
        let plan = generate_plan(&config);
        let summary = &plan.entries[0].summary;
        assert!(summary.iter().any(|(k, _)| k == "memory"));
        assert!(summary.iter().any(|(k, _)| k == "cpu"));
    }
}
