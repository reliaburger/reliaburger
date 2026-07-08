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

    let mut output = String::new();

    // Top-level scalars first: emitted after any section header they
    // would silently become part of that section, changing the config.
    let mut scalars = String::new();
    for (key, value) in &table {
        if !value.is_table() {
            scalars.push_str(&format!("{key} = {}\n", inline_value(value)));
        }
    }
    if !scalars.is_empty() {
        output.push_str(&scalars);
        output.push('\n');
    }

    // Sections in canonical order, then any others alphabetically
    for &section in SECTION_ORDER {
        if let Some(toml::Value::Table(sub)) = table.get(section) {
            append_table(&mut output, section, sub);
        }
    }
    for (key, value) in &table {
        if !SECTION_ORDER.contains(&key.as_str())
            && let toml::Value::Table(sub) = value
        {
            append_table(&mut output, key, sub);
        }
    }

    verify_roundtrip(&table, &output)?;
    Ok(output)
}

/// Refuse to hand back output that re-parses to a different value tree.
///
/// This is the guard against formatter bugs corrupting user configs: the
/// caller writes the formatted string over the original file, so a
/// mismatch here must fail loudly rather than silently lose data.
fn verify_roundtrip(
    original: &BTreeMap<String, toml::Value>,
    output: &str,
) -> Result<(), RelishError> {
    let reparsed: BTreeMap<String, toml::Value> = toml::from_str(output).map_err(|e| {
        RelishError::FormatFailed(format!(
            "formatter produced invalid TOML ({e}); file left untouched"
        ))
    })?;
    if &reparsed != original {
        return Err(RelishError::FormatFailed(
            "formatted output would change the config's meaning; file left untouched".to_string(),
        ));
    }
    Ok(())
}

/// Append a table as a `[path]` section: scalar and array fields first,
/// then each nested table recursively as its own dotted section.
fn append_table(output: &mut String, path: &str, table: &toml::map::Map<String, toml::Value>) {
    let has_scalars = table.values().any(|v| !v.is_table());

    if has_scalars || table.is_empty() {
        output.push_str(&format!("[{path}]\n"));
        for (k, v) in table {
            if !v.is_table() {
                output.push_str(&format!("{k} = {}\n", inline_value(v)));
            }
        }
        output.push('\n');
    }

    for (k, v) in table {
        if let toml::Value::Table(sub) = v {
            append_table(output, &format!("{path}.{k}"), sub);
        }
    }
}

/// Format a TOML value inline (strings quoted, arrays bracketed,
/// tables in `{ … }` form). `toml::Value`'s `Display` produces exactly
/// this; `toml::to_string` must NOT be used here — it serialises tables
/// in document form, which is invalid on the right of `=`.
fn inline_value(value: &toml::Value) -> String {
    value.to_string()
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

    /// H11 regression: nested tables like `[app.web.health]` used to be
    /// emitted in document form on the right of `=`, producing invalid
    /// TOML that was written back over the user's file.
    #[test]
    fn fmt_roundtrips_nested_health_ingress_autoscale_tables() {
        let input = r#"
[app.web]
image = "myapp:v1"
replicas = 3
port = 8080

[app.web.health]
path = "/health"
interval = 5
threshold_unhealthy = 3

[app.web.ingress]
host = "web.example.com"

[app.web.deploy]
strategy = "rolling"
max_unavailable = 1

[app.web.autoscale]
metric = "cpu"
target = "70%"
min = 1
max = 10
"#;
        let original: BTreeMap<String, toml::Value> = toml::from_str(input).unwrap();
        let formatted = format_toml(input).unwrap();
        let reparsed: BTreeMap<String, toml::Value> = toml::from_str(&formatted)
            .unwrap_or_else(|e| panic!("formatted output is invalid TOML: {e}\n{formatted}"));
        assert_eq!(original, reparsed, "value tree changed:\n{formatted}");
    }

    #[test]
    fn fmt_roundtrips_deeply_nested_and_array_values() {
        let input = r#"
[app.api]
image = "api:v2"
command = ["serve", "--port", "9000"]

[app.api.env]
LOG_LEVEL = "debug"

[app.api.placement]
required = ["zone=eu-west"]
preferred = []
"#;
        let original: BTreeMap<String, toml::Value> = toml::from_str(input).unwrap();
        let formatted = format_toml(input).unwrap();
        let reparsed: BTreeMap<String, toml::Value> = toml::from_str(&formatted).unwrap();
        assert_eq!(original, reparsed, "value tree changed:\n{formatted}");
    }

    #[test]
    fn fmt_nested_output_is_idempotent() {
        let input = r#"
[app.web]
image = "myapp:v1"

[app.web.health]
path = "/health"
"#;
        let first = format_toml(input).unwrap();
        let second = format_toml(&first).unwrap();
        assert_eq!(first, second);
    }

    #[test]
    fn roundtrip_guard_rejects_output_that_changes_meaning() {
        let original: BTreeMap<String, toml::Value> =
            toml::from_str("[app.web]\nport = 8080\n").unwrap();
        // Output that parses fine but carries a different value
        let err = verify_roundtrip(&original, "[app.web]\nport = 9999\n").unwrap_err();
        assert!(err.to_string().contains("meaning"), "got: {err}");
    }

    #[test]
    fn roundtrip_guard_rejects_invalid_output() {
        let original: BTreeMap<String, toml::Value> =
            toml::from_str("[app.web]\nport = 8080\n").unwrap();
        let err = verify_roundtrip(&original, "not [ valid").unwrap_err();
        assert!(err.to_string().contains("invalid TOML"), "got: {err}");
    }

    #[test]
    fn fmt_emits_top_level_scalars_before_sections() {
        // A top-level scalar emitted after a section header would be
        // swallowed into that section on re-parse; the guard catches it,
        // but the emission order must make it valid in the first place.
        let input = "title = \"cluster\"\n\n[app.web]\nimage = \"a:1\"\n";
        let formatted = format_toml(input).unwrap();
        let title_pos = formatted.find("title").unwrap();
        let section_pos = formatted.find("[app").unwrap();
        assert!(title_pos < section_pos, "got:\n{formatted}");
    }
}
