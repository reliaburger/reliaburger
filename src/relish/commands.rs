/// Command executors for the Relish CLI.
///
/// Each subcommand is a function that returns `Result<(), RelishError>`.
/// In Phase 1 only `apply` does real work — the others return a graceful
/// "agent required" error instead of panicking with `todo!()`.
use std::path::Path;

use crate::config::Config;

use super::RelishError;
use super::output::{OutputFormat, format_output};
use super::plan::generate_plan;

/// Parse, validate, and display the apply plan for a config file.
///
/// In Phase 1 this only shows what *would* happen — actual deployment
/// requires the Bun agent (Phase 2+).
pub fn apply(path: &Path, output: OutputFormat) -> Result<(), RelishError> {
    let config = Config::from_file(path)?;
    config.validate()?;
    let plan = generate_plan(&config);
    let formatted = format_output(&plan, output)?;
    println!("{formatted}");
    println!("\n(dry run — actual deployment requires a running Bun agent)");
    Ok(())
}

/// Show cluster and app status.
pub fn status() -> Result<(), RelishError> {
    Err(RelishError::AgentRequired {
        command: "status".to_string(),
    })
}

/// Stream logs from an app or job.
pub fn logs(_name: &str) -> Result<(), RelishError> {
    Err(RelishError::AgentRequired {
        command: "logs".to_string(),
    })
}

/// Execute a command inside a running container.
pub fn exec(_app: &str, _command: &[String]) -> Result<(), RelishError> {
    Err(RelishError::AgentRequired {
        command: "exec".to_string(),
    })
}

/// Show detailed info about an app, node, or job.
pub fn inspect(_name: &str) -> Result<(), RelishError> {
    Err(RelishError::AgentRequired {
        command: "inspect".to_string(),
    })
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

    #[test]
    fn apply_with_valid_config_succeeds() {
        let f = write_temp_config(
            r#"
            [app.web]
            image = "myapp:v1"
            port = 8080
        "#,
        );
        assert!(apply(f.path(), OutputFormat::Human).is_ok());
    }

    #[test]
    fn apply_with_missing_file_errors() {
        let result = apply(Path::new("/nonexistent/config.toml"), OutputFormat::Human);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            matches!(err, RelishError::Config(_)),
            "expected Config error, got: {err:?}"
        );
    }

    #[test]
    fn apply_with_invalid_toml_errors() {
        let f = write_temp_config("this is not valid toml [[[");
        let result = apply(f.path(), OutputFormat::Human);
        assert!(result.is_err());
    }

    #[test]
    fn apply_with_validation_error() {
        let f = write_temp_config(
            r#"
            [app.broken]
            replicas = 3
        "#,
        );
        let result = apply(f.path(), OutputFormat::Human);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            matches!(err, RelishError::Config(_)),
            "expected Config error, got: {err:?}"
        );
    }

    #[test]
    fn apply_json_output_produces_valid_json() {
        let f = write_temp_config(
            r#"
            [app.web]
            image = "myapp:v1"
        "#,
        );
        // Just verify it doesn't error — the actual JSON goes to stdout
        assert!(apply(f.path(), OutputFormat::Json).is_ok());
    }

    #[test]
    fn status_returns_agent_required() {
        let err = status().unwrap_err();
        assert!(
            matches!(err, RelishError::AgentRequired { ref command } if command == "status"),
            "got: {err:?}"
        );
    }

    #[test]
    fn logs_returns_agent_required() {
        let err = logs("web").unwrap_err();
        assert!(matches!(err, RelishError::AgentRequired { ref command } if command == "logs"));
    }

    #[test]
    fn exec_returns_agent_required() {
        let err = exec("web", &["sh".to_string()]).unwrap_err();
        assert!(matches!(err, RelishError::AgentRequired { ref command } if command == "exec"));
    }

    #[test]
    fn inspect_returns_agent_required() {
        let err = inspect("web").unwrap_err();
        assert!(matches!(err, RelishError::AgentRequired { ref command } if command == "inspect"));
    }
}
