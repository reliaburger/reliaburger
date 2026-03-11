/// Command executors for the Relish CLI.
///
/// Each subcommand is an async function that returns `Result<(), RelishError>`.
/// Commands try to reach the live Bun agent first. If the agent is
/// unreachable, `apply` falls back to a dry-run plan.
use std::fs;
use std::path::Path;

use crate::config::Config;

use super::RelishError;
use super::client::BunClient;
use super::output::{OutputFormat, format_output};
use super::plan::generate_plan;

/// Parse, validate, and deploy a config file.
///
/// If a Bun agent is running, sends the config for deployment.
/// Progress events are streamed to stderr in real time.
/// If no agent is reachable, falls back to showing the dry-run plan.
pub async fn apply(path: &Path, output: OutputFormat) -> Result<(), RelishError> {
    apply_with_client(path, output, &BunClient::default_local()).await
}

async fn apply_with_client(
    path: &Path,
    output: OutputFormat,
    client: &BunClient,
) -> Result<(), RelishError> {
    let config = Config::from_file(path)?;
    config.validate()?;

    match client.health().await {
        Ok(()) => {
            // Agent is alive — send the config (progress streams to stderr)
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
    status_with_client(&BunClient::default_local()).await
}

async fn status_with_client(client: &BunClient) -> Result<(), RelishError> {
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
    logs_with_client(name, &BunClient::default_local()).await
}

async fn logs_with_client(name: &str, client: &BunClient) -> Result<(), RelishError> {
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
    inspect_with_client(name, &BunClient::default_local()).await
}

async fn inspect_with_client(name: &str, client: &BunClient) -> Result<(), RelishError> {
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

/// Initialise a new project with starter config files.
///
/// Creates `reliaburger.toml` (node config) and `app.toml` (sample app)
/// in the given directory. Refuses to overwrite existing files.
pub fn init(dir: &Path) -> Result<(), RelishError> {
    let node_path = dir.join("reliaburger.toml");
    let app_path = dir.join("app.toml");

    if node_path.exists() {
        return Err(RelishError::FileExists {
            path: node_path.display().to_string(),
        });
    }
    if app_path.exists() {
        return Err(RelishError::FileExists {
            path: app_path.display().to_string(),
        });
    }

    let node_config = crate::config::node::NodeConfig::default();
    let node_toml = format!(
        "# Reliaburger node configuration.\n\
         # See docs/README.md for full reference.\n\n{}",
        toml::to_string_pretty(&node_config).expect("failed to serialise default node config")
    );

    let app_toml = "\
# Sample Reliaburger app configuration.
# Deploy with: relish apply app.toml

[app.web]
image = \"nginx:latest\"
port = 8080

[app.web.health]
path = \"/\"
";

    fs::write(&node_path, node_toml)?;
    fs::write(&app_path, app_toml)?;

    println!("created {}", node_path.display());
    println!("created {}", app_path.display());

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    /// Port 1 on localhost — nothing listens there, so connections
    /// are refused immediately without waiting for a timeout.
    fn bogus_client() -> BunClient {
        BunClient::new("http://127.0.0.1:1")
    }

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
        assert!(
            apply_with_client(f.path(), OutputFormat::Human, &bogus_client())
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn apply_with_missing_file_errors() {
        let result = apply_with_client(
            Path::new("/nonexistent/config.toml"),
            OutputFormat::Human,
            &bogus_client(),
        )
        .await;
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
        let result = apply_with_client(f.path(), OutputFormat::Human, &bogus_client()).await;
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
        let result = apply_with_client(f.path(), OutputFormat::Human, &bogus_client()).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            matches!(err, RelishError::Config(_)),
            "expected Config error, got: {err:?}"
        );
    }

    #[tokio::test]
    async fn status_returns_agent_unreachable() {
        let err = status_with_client(&bogus_client()).await.unwrap_err();
        assert!(matches!(err, RelishError::AgentUnreachable), "got: {err:?}");
    }

    #[tokio::test]
    async fn logs_returns_agent_unreachable() {
        let err = logs_with_client("web", &bogus_client()).await.unwrap_err();
        assert!(matches!(err, RelishError::AgentUnreachable));
    }

    #[tokio::test]
    async fn exec_returns_agent_required() {
        let err = exec("web", &["sh".to_string()]).await.unwrap_err();
        assert!(matches!(err, RelishError::AgentRequired { ref command } if command == "exec"));
    }

    #[test]
    fn init_creates_files() {
        let dir = tempfile::tempdir().unwrap();
        init(dir.path()).unwrap();
        assert!(dir.path().join("reliaburger.toml").exists());
        assert!(dir.path().join("app.toml").exists());
    }

    #[test]
    fn init_refuses_overwrite() {
        let dir = tempfile::tempdir().unwrap();
        init(dir.path()).unwrap();
        let err = init(dir.path()).unwrap_err();
        assert!(matches!(err, RelishError::FileExists { .. }));
    }

    #[test]
    fn init_generated_config_parses() {
        let dir = tempfile::tempdir().unwrap();
        init(dir.path()).unwrap();

        let node_content = std::fs::read_to_string(dir.path().join("reliaburger.toml")).unwrap();
        let _: crate::config::node::NodeConfig = toml::from_str(&node_content).unwrap();

        let app_content = std::fs::read_to_string(dir.path().join("app.toml")).unwrap();
        let config = Config::parse(&app_content).unwrap();
        config.validate().unwrap();
    }

    #[tokio::test]
    async fn inspect_returns_agent_unreachable() {
        let err = inspect_with_client("web", &bogus_client())
            .await
            .unwrap_err();
        assert!(matches!(err, RelishError::AgentUnreachable));
    }
}
