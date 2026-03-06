/// Output format selection for CLI commands.
///
/// Supports human-readable, JSON, and YAML output. The `ValueEnum` derive
/// lets clap parse `--output human` / `--output json` / `--output yaml`
/// directly from the command line.
use std::fmt;

use serde::Serialize;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, clap::ValueEnum)]
pub enum OutputFormat {
    #[default]
    Human,
    Json,
    Yaml,
}

/// Format a value for display in the chosen output format.
///
/// - `Human` calls the value's `Display` implementation.
/// - `Json` serialises with `serde_json` (pretty-printed).
/// - `Yaml` serialises with `serde_yaml`.
pub fn format_output<T: Serialize + fmt::Display>(
    value: &T,
    format: OutputFormat,
) -> Result<String, super::RelishError> {
    match format {
        OutputFormat::Human => Ok(value.to_string()),
        OutputFormat::Json => {
            serde_json::to_string_pretty(value).map_err(super::RelishError::SerialiseJson)
        }
        OutputFormat::Yaml => {
            serde_yaml::to_string(value).map_err(super::RelishError::SerialiseYaml)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A trivial type that implements both Display and Serialize,
    /// so we can test format_output without pulling in the full plan types.
    #[derive(Serialize)]
    struct Greeting {
        message: String,
    }

    impl fmt::Display for Greeting {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(f, "hello: {}", self.message)
        }
    }

    fn greeting() -> Greeting {
        Greeting {
            message: "world".to_string(),
        }
    }

    #[test]
    fn default_format_is_human() {
        assert_eq!(OutputFormat::default(), OutputFormat::Human);
    }

    #[test]
    fn human_format_uses_display() {
        let g = greeting();
        let output = format_output(&g, OutputFormat::Human).unwrap();
        assert_eq!(output, "hello: world");
    }

    #[test]
    fn json_format_uses_serialize() {
        let g = greeting();
        let output = format_output(&g, OutputFormat::Json).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&output).unwrap();
        assert_eq!(parsed["message"], "world");
    }

    #[test]
    fn yaml_format_uses_serde_yaml() {
        let g = greeting();
        let output = format_output(&g, OutputFormat::Yaml).unwrap();
        assert!(output.contains("message: world"), "got: {output}");
    }
}
