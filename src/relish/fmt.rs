/// TOML formatter for Reliaburger configs.
///
/// Parses TOML, validates it, and re-serialises with canonical section
/// ordering and consistent key ordering. Idempotent: running the
/// formatter twice produces the same output as running it once.
///
/// Note: comments are not preserved — the formatter round-trips through
/// the `toml` crate's typed representation, which discards comments.
/// This is acceptable for machine-generated configs (GitOps, compile
/// output). For hand-edited configs, use `relish lint` to validate
/// without reformatting.
use std::collections::BTreeMap;

use super::RelishError;

/// Canonical section ordering within a Reliaburger config file.
const SECTION_ORDER: &[&str] = &["namespace", "permission", "app", "job", "build"];

/// Format a TOML string with canonical ordering.
///
/// Returns the formatted string. The output is deterministic and
/// idempotent.
pub fn format_toml(input: &str) -> Result<String, RelishError> {
    // Parse into a generic TOML table (BTreeMap, so keys are alphabetical)
    let table: BTreeMap<String, toml::Value> =
        toml::from_str(input).map_err(|e| RelishError::FormatFailed(e.to_string()))?;

    // Rebuild with canonical section ordering
    let mut output = String::new();

    // First emit sections in canonical order
    for &section in SECTION_ORDER {
        if let Some(value) = table.get(section) {
            append_section(&mut output, section, value);
        }
    }

    // Then emit any sections not in the canonical list (alphabetically)
    for (key, value) in &table {
        if !SECTION_ORDER.contains(&key.as_str()) {
            append_section(&mut output, key, value);
        }
    }

    Ok(output)
}

/// Append a top-level section to the output string.
fn append_section(output: &mut String, key: &str, value: &toml::Value) {
    match value {
        toml::Value::Table(sub) => {
            // Each sub-key becomes a [key.subkey] section
            for (sub_key, sub_val) in sub {
                output.push_str(&format!("[{key}.{sub_key}]\n"));
                if let toml::Value::Table(fields) = sub_val {
                    append_fields(output, fields);
                } else {
                    output.push_str(&format!("{sub_key} = {}\n", format_value(sub_val)));
                }
                output.push('\n');
            }
        }
        _ => {
            output.push_str(&format!("{key} = {}\n\n", format_value(value)));
        }
    }
}

/// Append table fields (key = value pairs) sorted alphabetically.
fn append_fields(output: &mut String, fields: &toml::map::Map<String, toml::Value>) {
    for (k, v) in fields {
        output.push_str(&format!("{k} = {}\n", format_value(v)));
    }
}

/// Format a TOML value for output.
fn format_value(value: &toml::Value) -> String {
    // Use toml's serialiser for correct escaping
    toml::to_string(value).unwrap_or_else(|_| value.to_string())
}

/// Check whether a TOML string is already formatted.
///
/// Returns `true` if formatting would produce no changes.
pub fn is_formatted(input: &str) -> Result<bool, RelishError> {
    let formatted = format_toml(input)?;
    Ok(formatted == input)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fmt_is_idempotent() {
        let input = r#"
[app.web]
image = "myapp:v1"
replicas = 3
port = 8080

[job.migrate]
image = "myapp:v1"
command = ["npm", "run", "migrate"]
"#;
        let first = format_toml(input).unwrap();
        let second = format_toml(&first).unwrap();
        assert_eq!(first, second, "formatting should be idempotent");
    }

    #[test]
    fn fmt_sorts_sections_canonically() {
        let input = r#"
[job.migrate]
image = "myapp:v1"

[build.myapp]
context = "."

[app.web]
image = "myapp:v1"

[namespace.backend]
cpu = "8000m"

[permission.deployer]
actions = ["deploy"]
"#;
        let output = format_toml(input).unwrap();

        // Extract section order from output
        let sections: Vec<&str> = output
            .lines()
            .filter(|l| l.starts_with('[') && !l.starts_with("[["))
            .collect();

        // namespace < permission < app < job < build
        let ns_pos = sections.iter().position(|s| s.contains("namespace"));
        let perm_pos = sections.iter().position(|s| s.contains("permission"));
        let app_pos = sections.iter().position(|s| s.contains("app"));
        let job_pos = sections.iter().position(|s| s.contains("job"));
        let build_pos = sections.iter().position(|s| s.contains("build"));

        assert!(ns_pos < perm_pos, "namespace should come before permission");
        assert!(perm_pos < app_pos, "permission should come before app");
        assert!(app_pos < job_pos, "app should come before job");
        assert!(job_pos < build_pos, "job should come before build");
    }

    #[test]
    fn fmt_round_trips_values() {
        let input = r#"
[app.web]
image = "myapp:v1"
port = 8080
"#;
        let output = format_toml(input).unwrap();
        assert!(output.contains("image = \"myapp:v1\""), "got:\n{output}");
        assert!(output.contains("port = 8080"), "got:\n{output}");
    }

    #[test]
    fn fmt_invalid_toml_returns_error() {
        let input = "this is not valid toml [[[";
        let result = format_toml(input);
        assert!(result.is_err());
    }

    #[test]
    fn is_formatted_returns_true_for_already_formatted() {
        let input = "[app.web]\nimage = \"myapp:v1\"\n";
        let formatted = format_toml(input).unwrap();
        assert!(is_formatted(&formatted).unwrap());
    }
}
