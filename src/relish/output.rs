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

/// A one-line chart of `values` in block characters, scaled from the
/// smallest value (`▁`) to the largest (`█`). Shared by the TUI and
/// `relish metrics`.
pub fn sparkline(values: &[f64]) -> String {
    const BARS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    let min = values.iter().copied().fold(f64::INFINITY, f64::min);
    let max = values.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    let range = (max - min).max(f64::EPSILON);
    values
        .iter()
        .map(|value| {
            let index = (((value - min) / range) * (BARS.len() - 1) as f64).round() as usize;
            BARS[index.min(BARS.len() - 1)]
        })
        .collect()
}

/// Format a value for display in the chosen output format.
///
/// - `Human` calls the value's `Display` implementation.
/// - `Json` serialises with `serde_json` (pretty-printed).
/// - `Yaml` serialises with `yaml_serde`.
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
            yaml_serde::to_string(value).map_err(super::RelishError::SerialiseYaml)
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
    fn yaml_format_uses_serialize() {
        let g = greeting();
        let output = format_output(&g, OutputFormat::Yaml).unwrap();
        assert!(output.contains("message: world"), "got: {output}");
    }

    /// Pins the exact YAML layout, so swapping the YAML crate (serde_yaml to
    /// yaml_serde, #817) can't silently change what scripts parse.
    #[derive(Serialize)]
    struct Layout {
        name: String,
        replicas: u32,
        ratio: f64,
        enabled: bool,
        missing: Option<String>,
        looks_like_bool: String,
        looks_like_number: String,
        multi_line: String,
        empty: Vec<String>,
        ports: Vec<u16>,
        labels: std::collections::BTreeMap<String, String>,
        state: Mode,
    }

    #[derive(Serialize)]
    enum Mode {
        Running,
        Failed { code: i32 },
    }

    #[test]
    fn yaml_output_layout_is_stable() {
        let layout = Layout {
            name: "web".to_string(),
            replicas: 3,
            ratio: 0.5,
            enabled: true,
            missing: None,
            looks_like_bool: "yes".to_string(),
            looks_like_number: "8080".to_string(),
            multi_line: "line one\nline two\n".to_string(),
            empty: Vec::new(),
            ports: vec![80, 443],
            labels: [("tier".to_string(), "front".to_string())].into(),
            state: Mode::Running,
        };
        let output = yaml_serde::to_string(&layout).unwrap();
        assert_eq!(
            output,
            "name: web\n\
             replicas: 3\n\
             ratio: 0.5\n\
             enabled: true\n\
             missing: null\n\
             looks_like_bool: yes\n\
             looks_like_number: '8080'\n\
             multi_line: |\n  line one\n  line two\n\
             empty: []\n\
             ports:\n- 80\n- 443\n\
             labels:\n  tier: front\n\
             state: Running\n"
        );
        let failed = yaml_serde::to_string(&Mode::Failed { code: 2 }).unwrap();
        assert_eq!(failed, "!Failed\ncode: 2\n");
    }
}
