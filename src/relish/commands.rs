/// Command executors for the Relish CLI.
///
/// Each subcommand is an async function that returns `Result<(), RelishError>`.
/// Commands try to reach the live Bun agent first. If the agent is
/// unreachable, `apply` falls back to a dry-run plan.
use std::path::Path;

use crate::config::Config;

use super::RelishError;
use super::client::BunClient;
use super::output::{OutputFormat, format_output};
use super::plan::generate_plan;

/// Parse, validate, and deploy a config file.
///
/// If a Bun agent is running, sends the config for deployment.
/// If no agent is reachable, falls back to showing the dry-run plan.
pub async fn apply(path: &Path, output: OutputFormat) -> Result<(), RelishError> {
    let config = Config::from_file(path)?;
    config.validate()?;

    let client = BunClient::default_local();

    match client.health().await {
        Ok(()) => {
            // Agent is alive — send the config
            let result = client.apply(&config).await?;
            println!(
                "deployed {} instance(s): {}",
                result.created,
                result.instances.join(", ")
            );
            Ok(())
        }
        Err(_) => {
            // Agent unreachable — fall back to dry-run
            let plan = generate_plan(&config);
            let formatted = format_output(&plan, output)?;
            println!("{formatted}");
            println!("\n(dry run — bun agent not reachable, showing plan only)");
            Ok(())
        }
    }
}

/// Show cluster and app status.
pub async fn status() -> Result<(), RelishError> {
    let client = BunClient::default_local();
    let statuses = client.status().await?;

    if statuses.is_empty() {
        println!("no workloads running");
    } else {
        println!(
            "{:<20} {:<15} {:<12} {:<10} {:<10} {:<6}",
            "INSTANCE", "APP", "NAMESPACE", "STATE", "PID", "RESTARTS"
        );
        for s in &statuses {
            let pid = s
                .pid
                .map(|p| p.to_string())
                .unwrap_or_else(|| "-".to_string());
            println!(
                "{:<20} {:<15} {:<12} {:<10} {:<10} {:<6}",
                s.id, s.app_name, s.namespace, s.state, pid, s.restart_count
            );
        }
    }

    Ok(())
}

/// Stream logs from an app or job.
pub async fn logs(name: &str) -> Result<(), RelishError> {
    let client = BunClient::default_local();
    let log_output = client.logs(name, "default").await?;
    println!("{log_output}");
    Ok(())
}

/// Execute a command inside a running container.
pub async fn exec(_app: &str, _command: &[String]) -> Result<(), RelishError> {
    Err(RelishError::AgentRequired {
        command: "exec".to_string(),
    })
}

/// Show detailed info about an app, node, or job.
pub async fn inspect(name: &str) -> Result<(), RelishError> {
    let client = BunClient::default_local();
    let statuses = client.status().await?;
    let matching: Vec<_> = statuses.iter().filter(|s| s.app_name == name).collect();

    if matching.is_empty() {
        println!("no instances found for {name}");
    } else {
        for s in &matching {
            println!("Instance: {}", s.id);
            println!("  App:       {}", s.app_name);
            println!("  Namespace: {}", s.namespace);
            println!("  State:     {}", s.state);
            println!("  Restarts:  {}", s.restart_count);
            if let Some(pid) = s.pid {
                println!("  PID:       {pid}");
            }
            if let Some(port) = s.host_port {
                println!("  Port:      {port}");
            }
            println!();
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    fn write_temp_config(content: &str) -> tempfile::NamedTempFile {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(content.as_bytes()).unwrap();
        f
    }

    #[tokio::test]
    async fn apply_with_valid_config_falls_back_to_dry_run() {
        let f = write_temp_config(
            r#"
            [app.web]
            image = "myapp:v1"
            port = 8080
        "#,
        );
        // No agent running, so this falls back to dry-run
        assert!(apply(f.path(), OutputFormat::Human).await.is_ok());
    }

    #[tokio::test]
    async fn apply_with_missing_file_errors() {
        let result = apply(Path::new("/nonexistent/config.toml"), OutputFormat::Human).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            matches!(err, RelishError::Config(_)),
            "expected Config error, got: {err:?}"
        );
    }

    #[tokio::test]
    async fn apply_with_invalid_toml_errors() {
        let f = write_temp_config("this is not valid toml [[[");
        let result = apply(f.path(), OutputFormat::Human).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn apply_with_validation_error() {
        let f = write_temp_config(
            r#"
            [app.broken]
            replicas = 3
        "#,
        );
        let result = apply(f.path(), OutputFormat::Human).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            matches!(err, RelishError::Config(_)),
            "expected Config error, got: {err:?}"
        );
    }

    #[tokio::test]
    async fn status_returns_agent_unreachable() {
        let err = status().await.unwrap_err();
        assert!(matches!(err, RelishError::AgentUnreachable), "got: {err:?}");
    }

    #[tokio::test]
    async fn logs_returns_agent_unreachable() {
        let err = logs("web").await.unwrap_err();
        assert!(matches!(err, RelishError::AgentUnreachable));
    }

    #[tokio::test]
    async fn exec_returns_agent_required() {
        let err = exec("web", &["sh".to_string()]).await.unwrap_err();
        assert!(matches!(err, RelishError::AgentRequired { ref command } if command == "exec"));
    }

    #[tokio::test]
    async fn inspect_returns_agent_unreachable() {
        let err = inspect("web").await.unwrap_err();
        assert!(matches!(err, RelishError::AgentUnreachable));
    }
}
