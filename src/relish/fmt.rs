//! TOML formatter for Reliaburger configs (`relish fmt`).
//!
//! Edits the document in place with `toml_edit`, which keeps every comment
//! and each value exactly as written. It orders sections canonically, sorts
//! keys within each section, puts one space either side of `=`, drops
//! indentation and leaves one blank line before each section header.
//! Idempotent: running the formatter twice produces the same output as
//! running it once.
use std::collections::BTreeMap;

use toml_edit::{DocumentMut, Item, RawString, Table};

use super::RelishError;

/// Canonical section ordering within a Reliaburger config file.
const SECTION_ORDER: &[&str] = &["namespace", "permission", "app", "job", "build"];

/// Format a TOML string with canonical ordering, keeping its comments.
///
/// Returns the formatted string. The output is deterministic and
/// idempotent.
pub fn format_toml(input: &str) -> Result<String, RelishError> {
    let original: BTreeMap<String, toml::Value> =
        toml::from_str(input).map_err(|e| RelishError::FormatFailed(e.to_string()))?;
    let mut document: DocumentMut = input
        .parse()
        .map_err(|e: toml_edit::TomlError| RelishError::FormatFailed(e.to_string()))?;

    let root = document.as_table_mut();
    root.sort_values_by(|left, _, right, _| {
        section_rank(left.get())
            .cmp(&section_rank(right.get()))
            .then_with(|| left.get().cmp(right.get()))
    });
    let mut layout = Layout {
        position: 0,
        header_written: false,
    };
    // The root's own keys print before any header.
    root.set_position(Some(layout.position));
    layout.position += 1;
    normalise_values(root);
    layout.header_written = root.iter().any(|(_, item)| item.is_value());
    for (_, item) in root.iter_mut() {
        layout.visit(item);
    }
    let trailing = comment_lines(document.trailing().as_str().unwrap_or(""));
    document.set_trailing(if trailing.is_empty() {
        String::new()
    } else {
        format!("\n{trailing}")
    });

    let output = document.to_string();
    verify_roundtrip(&original, &output)?;
    Ok(output)
}

/// Where a top-level section sorts: the canonical ones first, in order,
/// then everything else alphabetically.
fn section_rank(key: &str) -> usize {
    SECTION_ORDER
        .iter()
        .position(|section| *section == key)
        .unwrap_or(SECTION_ORDER.len())
}

/// Walks the tables in print order, numbering them and setting the
/// whitespace around their headers.
struct Layout {
    /// The next document position; `toml_edit` prints tables in this order.
    position: isize,
    /// Whether anything has printed yet, so the first header gets no blank
    /// line above it.
    header_written: bool,
}

impl Layout {
    fn visit(&mut self, item: &mut Item) {
        match item {
            Item::Table(table) => self.visit_table(table),
            // The elements of `[[x]]` keep their order: it's meaningful.
            Item::ArrayOfTables(array) => {
                for table in array.iter_mut() {
                    self.visit_table(table);
                }
            }
            Item::None | Item::Value(_) => {}
        }
    }

    fn visit_table(&mut self, table: &mut Table) {
        // Dotted keys (`a.b = 1`) print inside their parent, not as a header.
        if table.is_dotted() {
            return;
        }
        table.sort_values();
        table.set_position(Some(self.position));
        self.position += 1;
        normalise_values(table);
        let has_values = table.iter().any(|(_, item)| item.is_value());
        if !table.is_implicit() || has_values {
            let comments = comment_lines(raw(table.decor().prefix()));
            let blank = if self.header_written { "\n" } else { "" };
            let suffix = trailing_comment(raw(table.decor().suffix()));
            table.decor_mut().set_prefix(format!("{blank}{comments}"));
            table.decor_mut().set_suffix(suffix);
            self.header_written = true;
        }
        for (_, item) in table.iter_mut() {
            self.visit(item);
        }
    }
}

/// Give each `key = value` line in `table` canonical spacing, keeping the
/// comment lines above it and any comment after it.
fn normalise_values(table: &mut Table) {
    for (mut key, item) in table.iter_mut() {
        let Item::Value(value) = item else {
            continue;
        };
        let comments = comment_lines(raw(key.leaf_decor().prefix()));
        key.leaf_decor_mut().set_prefix(comments);
        key.leaf_decor_mut().set_suffix(" ");
        let suffix = trailing_comment(raw(value.decor().suffix()));
        value.decor_mut().set_prefix(" ");
        value.decor_mut().set_suffix(suffix);
    }
}

/// The text of a piece of decor. Parsed documents hold their decor as
/// text, so a missing value is simply none.
fn raw(decor: Option<&RawString>) -> &str {
    decor.and_then(RawString::as_str).unwrap_or("")
}

/// The comment lines in a run of whitespace and comments, unindented, each
/// ending in a newline. Blank lines go.
fn comment_lines(text: &str) -> String {
    text.lines()
        .map(str::trim)
        .filter(|line| line.starts_with('#'))
        .map(|line| format!("{line}\n"))
        .collect()
}

/// A comment after a value or header on the same line, as ` # …`, or
/// nothing.
fn trailing_comment(text: &str) -> String {
    let comment = text.trim();
    if comment.starts_with('#') {
        format!(" {comment}")
    } else {
        String::new()
    }
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

    /// D35: `relish fmt` used to round-trip through `toml::Value` and drop
    /// every comment. Full-line comments travel with the key or section they
    /// sit above, and a trailing comment stays on its line.
    #[test]
    fn fmt_keeps_every_comment() {
        let input = "# The web tier.\n\
                     [app.web]\n\
                     # Pinned until the cache fix ships.\n\
                     image = \"web:v1\"\n\
                     replicas = 3 # one per zone\n\
                     \n\
                     # Runs before every deploy.\n\
                     [job.migrate]\n\
                     image = \"web:v1\"\n\
                     # end of file\n";
        let formatted = format_toml(input).unwrap();
        for comment in [
            "# The web tier.",
            "# Pinned until the cache fix ships.",
            "replicas = 3 # one per zone",
            "# Runs before every deploy.",
            "# end of file",
        ] {
            assert!(
                formatted.contains(comment),
                "lost {comment:?}:\n{formatted}"
            );
        }
        let section_comment = formatted.find("# Runs before every deploy.").unwrap();
        assert!(section_comment < formatted.find("[job.migrate]").unwrap());
        let key_comment = formatted.find("# Pinned").unwrap();
        assert!(key_comment < formatted.find("image").unwrap());
    }

    #[test]
    fn fmt_moves_a_section_with_the_comment_above_it() {
        let input =
            "[app.web]\nimage = \"web:v1\"\n\n# Team budget.\n[namespace.team]\ncpu = \"8000m\"\n";
        let formatted = format_toml(input).unwrap();
        assert!(
            formatted.starts_with("# Team budget.\n[namespace.team]\n"),
            "got:\n{formatted}"
        );
    }

    #[test]
    fn fmt_normalises_spacing_and_blank_lines() {
        let input = "[app.web]\n   image=\"web:v1\"\n\n\n   port   =   8080\n[app.api]\nimage = \"api:v1\"\n";
        let formatted = format_toml(input).unwrap();
        assert_eq!(
            formatted,
            "[app.api]\nimage = \"api:v1\"\n\n[app.web]\nimage = \"web:v1\"\nport = 8080\n"
        );
    }

    #[test]
    fn fmt_with_comments_is_idempotent() {
        let input = "# top\n[job.a]\nimage = \"a:1\" # pinned\n\n# web\n[app.web]\n# replicas\nreplicas = 2\nimage = \"w:1\"\n";
        let first = format_toml(input).unwrap();
        assert_eq!(first, format_toml(&first).unwrap());
        assert!(is_formatted(&first).unwrap());
    }

    #[test]
    fn fmt_keeps_array_of_tables_in_order() {
        let input = "[[app.web.volume]]\npath = \"/b\"\n\n[[app.web.volume]]\npath = \"/a\"\n";
        let formatted = format_toml(input).unwrap();
        assert!(formatted.find("/b").unwrap() < formatted.find("/a").unwrap());
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
