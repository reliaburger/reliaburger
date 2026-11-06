//! Adding and removing one named entry in another tool's config file.
//!
//! Each agent keeps its MCP servers in a map under one key: `mcpServers` in
//! a JSON file, `[mcp_servers]` in Codex's TOML, `extensions:` in Goose's
//! YAML. Setup touches exactly one entry in that map and leaves every other
//! byte of the file as it found it, as far as the format allows:
//!
//! - JSON keeps the order and the text of every other key. `serde_json`
//!   sorts an object's keys unless the whole crate turns on
//!   `preserve_order`, which would change every other JSON document we
//!   print, so the members are read as [`RawValue`]s instead: the exact text
//!   of each value, in file order. Only the top level and the server map are
//!   re-indented.
//! - TOML goes through `toml_edit`, which keeps comments and layout.
//! - YAML can't be round-tripped with its comments, so a YAML file with any
//!   comment is refused rather than rewritten without them.
//!
//! A file that doesn't parse is refused too. Nothing here writes files; it
//! only turns one text into another.

use std::fmt;

use serde::de::{Deserializer, MapAccess, Visitor};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use serde_json::value::RawValue;

/// A config file's format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, schemars::JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Format {
    /// Plain JSON, no comments.
    Json,
    /// TOML.
    Toml,
    /// YAML, without comments.
    Yaml,
}

/// Why a file can't be edited safely.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum EditError {
    /// The text doesn't parse in its format.
    #[error("isn't valid {format}: {reason}")]
    Unparsable {
        /// The format it should be in.
        format: &'static str,
        /// The parser's explanation.
        reason: String,
    },
    /// The top level, or the key the servers live under, isn't a map.
    #[error("{key} isn't a table of servers")]
    NotAMap {
        /// The key, `top level` for the document itself.
        key: String,
    },
    /// A YAML file with comments, which rewriting would drop.
    #[error("has comments, and rewriting YAML would drop them")]
    HasComments,
}

/// What a file holds under the entry's name.
#[derive(Debug, Clone, PartialEq)]
pub enum Existing {
    /// Nothing by that name.
    Absent,
    /// Exactly the entry setup would write.
    Same,
    /// Something else by that name, which setup didn't write.
    Different,
}

/// What the file holds at `parent.name`, compared with `entry`.
///
/// Empty text is a file that doesn't exist yet.
pub fn existing(
    format: Format,
    text: &str,
    parent: &str,
    name: &str,
    entry: &Value,
) -> Result<Existing, EditError> {
    let current = match format {
        Format::Json => json::get(text, parent, name)?,
        Format::Toml => toml_format::get(text, parent, name)?,
        Format::Yaml => yaml::get(text, parent, name)?,
    };
    Ok(match current {
        None => Existing::Absent,
        Some(value) if value == *entry => Existing::Same,
        Some(_) => Existing::Different,
    })
}

/// `text` with `entry` at `parent.name`, replacing whatever was there.
pub fn insert(
    format: Format,
    text: &str,
    parent: &str,
    name: &str,
    entry: &Value,
) -> Result<String, EditError> {
    match format {
        Format::Json => json::insert(text, parent, name, entry),
        Format::Toml => toml_format::insert(text, parent, name, entry),
        Format::Yaml => yaml::insert(text, parent, name, entry),
    }
}

/// `text` without `parent.name`. A map left empty goes too, so removing
/// what [`insert`] added gives back the original text.
pub fn remove(format: Format, text: &str, parent: &str, name: &str) -> Result<String, EditError> {
    match format {
        Format::Json => json::remove(text, parent, name),
        Format::Toml => toml_format::remove(text, parent, name),
        Format::Yaml => yaml::remove(text, parent, name),
    }
}

mod json {
    use super::*;

    /// A JSON object's members in file order, each value as its exact text.
    struct Members(Vec<(String, Box<RawValue>)>);

    // A hand-written `Deserialize`, because the derive can't say "a map, in
    // order, values untouched". serde calls `visit_map` with each key and
    // value as the parser meets them; `Box<RawValue>` captures the value's
    // text without parsing it into a `Value`.
    impl<'de> Deserialize<'de> for Members {
        fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
            struct MembersVisitor;

            impl<'de> Visitor<'de> for MembersVisitor {
                type Value = Members;

                fn expecting(&self, formatter: &mut fmt::Formatter) -> fmt::Result {
                    formatter.write_str("a JSON object")
                }

                fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Members, A::Error> {
                    let mut members = Vec::new();
                    while let Some(member) = map.next_entry::<String, Box<RawValue>>()? {
                        members.push(member);
                    }
                    Ok(Members(members))
                }
            }

            deserializer.deserialize_map(MembersVisitor)
        }
    }

    fn unparsable(error: serde_json::Error) -> EditError {
        EditError::Unparsable {
            format: "JSON",
            reason: error.to_string(),
        }
    }

    /// The members of `text`'s top-level object; empty text is `{}`.
    fn top_level(text: &str) -> Result<Vec<(String, String)>, EditError> {
        if text.trim().is_empty() {
            return Ok(Vec::new());
        }
        // Parse it whole first, so a value that isn't an object gets its own
        // error rather than "expected a JSON object" from the visitor.
        let value: Value = serde_json::from_str(text).map_err(unparsable)?;
        if !value.is_object() {
            return Err(EditError::NotAMap {
                key: "the top level".to_string(),
            });
        }
        members_of(text, "the top level")
    }

    fn members_of(text: &str, key: &str) -> Result<Vec<(String, String)>, EditError> {
        let members: Members = serde_json::from_str(text).map_err(|_| EditError::NotAMap {
            key: key.to_string(),
        })?;
        Ok(members
            .0
            .into_iter()
            .map(|(name, value)| (name, value.get().to_string()))
            .collect())
    }

    fn quoted(key: &str) -> String {
        // A `&str` always serialises.
        serde_json::to_string(key).unwrap_or_else(|_| format!("\"{key}\""))
    }

    /// `members` as an object whose own braces sit at `depth` (0 for the top
    /// level), two spaces per level, as `JSON.stringify(value, null, 2)` and
    /// so every Node-based agent writes them.
    fn render(members: &[(String, String)], depth: usize) -> String {
        if members.is_empty() {
            return "{}".to_string();
        }
        let pad = "  ".repeat(depth + 1);
        let close = "  ".repeat(depth);
        let body: Vec<String> = members
            .iter()
            .map(|(key, value)| format!("{pad}{}: {value}", quoted(key)))
            .collect();
        format!("{{\n{}\n{close}}}", body.join(",\n"))
    }

    /// `value` pretty-printed for a member at `depth + 1`.
    fn pretty(value: &Value, depth: usize) -> String {
        let pad = "  ".repeat(depth + 1);
        // A `Value` always serialises.
        let text = serde_json::to_string_pretty(value).unwrap_or_default();
        text.lines()
            .enumerate()
            .map(|(index, line)| {
                if index == 0 {
                    line.to_string()
                } else {
                    format!("{pad}{line}")
                }
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Keep the original's final newline, or add one to a new file.
    fn finish(original: &str, rendered: String) -> String {
        if original.is_empty() || original.ends_with('\n') {
            rendered + "\n"
        } else {
            rendered
        }
    }

    pub(super) fn get(text: &str, parent: &str, name: &str) -> Result<Option<Value>, EditError> {
        let top = top_level(text)?;
        let Some((_, servers)) = top.iter().find(|(key, _)| key == parent) else {
            return Ok(None);
        };
        let servers = members_of(servers, parent)?;
        let Some((_, entry)) = servers.iter().find(|(key, _)| key == name) else {
            return Ok(None);
        };
        serde_json::from_str(entry).map(Some).map_err(unparsable)
    }

    pub(super) fn insert(
        text: &str,
        parent: &str,
        name: &str,
        entry: &Value,
    ) -> Result<String, EditError> {
        let mut top = top_level(text)?;
        let entry = pretty(entry, 1);
        // New keys go first, so the lines around them (other servers, with
        // whatever secrets their `env` holds) don't change, and the diff
        // shows only what setup added.
        match top.iter_mut().find(|(key, _)| key == parent) {
            Some((_, servers)) => {
                let mut members = members_of(servers, parent)?;
                match members.iter_mut().find(|(key, _)| key == name) {
                    Some((_, value)) => *value = entry,
                    None => members.insert(0, (name.to_string(), entry)),
                }
                *servers = render(&members, 1);
            }
            None => {
                let servers = render(&[(name.to_string(), entry)], 1);
                top.insert(0, (parent.to_string(), servers));
            }
        }
        Ok(finish(text, render(&top, 0)))
    }

    pub(super) fn remove(text: &str, parent: &str, name: &str) -> Result<String, EditError> {
        let mut top = top_level(text)?;
        if let Some((_, servers)) = top.iter_mut().find(|(key, _)| key == parent) {
            let mut members = members_of(servers, parent)?;
            members.retain(|(key, _)| key != name);
            *servers = render(&members, 1);
        }
        top.retain(|(key, servers)| key != parent || servers != "{}");
        Ok(finish(text, render(&top, 0)))
    }
}

mod toml_format {
    use super::*;
    use toml_edit::{DocumentMut, Item, Table};

    fn unparsable(reason: String) -> EditError {
        EditError::Unparsable {
            format: "TOML",
            reason,
        }
    }

    fn document(text: &str) -> Result<DocumentMut, EditError> {
        text.parse::<DocumentMut>()
            .map_err(|error| unparsable(error.to_string()))
    }

    pub(super) fn get(text: &str, parent: &str, name: &str) -> Result<Option<Value>, EditError> {
        // `toml` (with `preserve_order`) reads; `toml_edit` writes.
        let table: toml::Table =
            toml::from_str(text).map_err(|error| unparsable(error.to_string()))?;
        let Some(servers) = table.get(parent) else {
            return Ok(None);
        };
        let Some(servers) = servers.as_table() else {
            return Err(EditError::NotAMap {
                key: parent.to_string(),
            });
        };
        servers
            .get(name)
            .map(|entry| serde_json::to_value(entry).map_err(|error| unparsable(error.to_string())))
            .transpose()
    }

    fn servers_table<'a>(
        document: &'a mut DocumentMut,
        parent: &str,
    ) -> Result<&'a mut Table, EditError> {
        if !document.contains_key(parent) {
            let mut table = Table::new();
            // Implicit: the file gets `[mcp_servers.reliaburger]`, not an
            // empty `[mcp_servers]` header above it.
            table.set_implicit(true);
            document.insert(parent, Item::Table(table));
        }
        document
            .get_mut(parent)
            .and_then(Item::as_table_mut)
            .ok_or_else(|| EditError::NotAMap {
                key: parent.to_string(),
            })
    }

    pub(super) fn insert(
        text: &str,
        parent: &str,
        name: &str,
        entry: &Value,
    ) -> Result<String, EditError> {
        let mut document = document(text)?;
        let entry: toml::Table =
            serde_json::from_value(entry.clone()).map_err(|error| unparsable(error.to_string()))?;
        let entry = toml::to_string(&entry).map_err(|error| unparsable(error.to_string()))?;
        let entry = document_table(&entry)?;
        servers_table(&mut document, parent)?.insert(name, Item::Table(entry));
        Ok(document.to_string())
    }

    fn document_table(text: &str) -> Result<Table, EditError> {
        Ok(document(text)?.as_table().clone())
    }

    pub(super) fn remove(text: &str, parent: &str, name: &str) -> Result<String, EditError> {
        let mut document = document(text)?;
        if document.contains_key(parent) {
            let servers = servers_table(&mut document, parent)?;
            servers.remove(name);
            if servers.is_empty() {
                document.remove(parent);
            }
        }
        Ok(document.to_string())
    }
}

mod yaml {
    use super::*;
    use yaml_serde::{Mapping, Value as Yaml};

    fn unparsable(error: yaml_serde::Error) -> EditError {
        EditError::Unparsable {
            format: "YAML",
            reason: error.to_string(),
        }
    }

    /// The document as a map; refuses comments and anything but a map.
    fn document(text: &str) -> Result<Mapping, EditError> {
        if text.lines().any(|line| line.trim_start().starts_with('#')) {
            return Err(EditError::HasComments);
        }
        if text.trim().is_empty() {
            return Ok(Mapping::new());
        }
        match yaml_serde::from_str::<Yaml>(text).map_err(unparsable)? {
            Yaml::Mapping(mapping) => Ok(mapping),
            Yaml::Null => Ok(Mapping::new()),
            _ => Err(EditError::NotAMap {
                key: "the top level".to_string(),
            }),
        }
    }

    fn servers<'a>(document: &'a mut Mapping, parent: &str) -> Result<&'a mut Mapping, EditError> {
        let key = Yaml::String(parent.to_string());
        if matches!(document.get(&key), None | Some(Yaml::Null)) {
            document.insert(key.clone(), Yaml::Mapping(Mapping::new()));
        }
        document
            .get_mut(&key)
            .and_then(Yaml::as_mapping_mut)
            .ok_or_else(|| EditError::NotAMap {
                key: parent.to_string(),
            })
    }

    pub(super) fn get(text: &str, parent: &str, name: &str) -> Result<Option<Value>, EditError> {
        let mut document = document(text)?;
        if !document.contains_key(parent) {
            return Ok(None);
        }
        let Some(entry) = servers(&mut document, parent)?.get(name) else {
            return Ok(None);
        };
        serde_json::to_value(entry)
            .map(Some)
            .map_err(|error| EditError::Unparsable {
                format: "YAML",
                reason: error.to_string(),
            })
    }

    fn render(document: &Mapping) -> Result<String, EditError> {
        yaml_serde::to_string(document).map_err(unparsable)
    }

    pub(super) fn insert(
        text: &str,
        parent: &str,
        name: &str,
        entry: &Value,
    ) -> Result<String, EditError> {
        let mut document = document(text)?;
        let entry = yaml_serde::to_value(entry).map_err(unparsable)?;
        servers(&mut document, parent)?.insert(Yaml::String(name.to_string()), entry);
        render(&document)
    }

    pub(super) fn remove(text: &str, parent: &str, name: &str) -> Result<String, EditError> {
        let mut document = document(text)?;
        if document.contains_key(parent) {
            let servers = servers(&mut document, parent)?;
            servers.remove(name);
            if servers.is_empty() {
                document.remove(parent);
            }
        }
        render(&document)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ours() -> Value {
        json!({"command": "relish", "args": ["mcp"]})
    }

    #[test]
    fn json_insert_into_a_missing_file_makes_a_small_object() {
        let text = insert(Format::Json, "", "mcpServers", "reliaburger", &ours()).unwrap();
        assert_eq!(
            text,
            "{\n  \"mcpServers\": {\n    \"reliaburger\": {\n      \"args\": [\n        \"mcp\"\n      ],\n      \"command\": \"relish\"\n    }\n  }\n}\n"
        );
        assert_eq!(
            existing(Format::Json, &text, "mcpServers", "reliaburger", &ours()).unwrap(),
            Existing::Same
        );
    }

    #[test]
    fn json_keeps_every_other_key_in_its_order_and_its_text() {
        // Keys out of alphabetical order, odd spacing inside a value, and a
        // number serde_json would print differently: all must survive.
        let original = "{\n  \"zebra\": 1.50,\n  \"mcpServers\": {\n    \"other\": {\"command\": \"x\",   \"env\": {\"KEY\": \"s3cret\"}}\n  },\n  \"apple\": [1,2]\n}\n";
        let added = insert(Format::Json, original, "mcpServers", "reliaburger", &ours()).unwrap();
        assert!(added.contains("\"zebra\": 1.50"), "{added}");
        assert!(
            added.contains("{\"command\": \"x\",   \"env\": {\"KEY\": \"s3cret\"}}"),
            "{added}"
        );
        assert!(added.find("zebra") < added.find("apple"), "{added}");
        let removed = remove(Format::Json, &added, "mcpServers", "reliaburger").unwrap();
        assert_eq!(removed, original, "remove undoes insert byte for byte");
    }

    #[test]
    fn json_with_comments_is_refused() {
        let jsonc = "{\n  // my servers\n  \"servers\": {}\n}\n";
        let error = insert(Format::Json, jsonc, "servers", "reliaburger", &ours()).unwrap_err();
        assert!(matches!(error, EditError::Unparsable { .. }), "{error:?}");
    }

    #[test]
    fn json_servers_that_arent_an_object_are_refused() {
        let text = "{\"mcpServers\": [1]}";
        assert_eq!(
            insert(Format::Json, text, "mcpServers", "reliaburger", &ours()).unwrap_err(),
            EditError::NotAMap {
                key: "mcpServers".to_string()
            }
        );
        assert!(matches!(
            insert(Format::Json, "[]", "mcpServers", "reliaburger", &ours()),
            Err(EditError::NotAMap { .. })
        ));
    }

    #[test]
    fn json_tells_a_foreign_entry_from_ours() {
        let text = "{\"mcpServers\": {\"reliaburger\": {\"command\": \"/opt/relish\"}}}";
        assert_eq!(
            existing(Format::Json, text, "mcpServers", "reliaburger", &ours()).unwrap(),
            Existing::Different
        );
        assert_eq!(
            existing(Format::Json, "{}", "mcpServers", "reliaburger", &ours()).unwrap(),
            Existing::Absent
        );
    }

    #[test]
    fn toml_keeps_comments_and_adds_one_table() {
        let original = "# my Codex settings\nmodel = \"o4\" # the default\n\n[mcp_servers.other]\ncommand = \"x\"\n";
        let added = insert(
            Format::Toml,
            original,
            "mcp_servers",
            "reliaburger",
            &ours(),
        )
        .unwrap();
        assert!(added.starts_with(original), "{added}");
        assert!(
            added.ends_with("[mcp_servers.reliaburger]\nargs = [\"mcp\"]\ncommand = \"relish\"\n"),
            "{added}"
        );
        assert_eq!(
            existing(Format::Toml, &added, "mcp_servers", "reliaburger", &ours()).unwrap(),
            Existing::Same
        );
        let removed = remove(Format::Toml, &added, "mcp_servers", "reliaburger").unwrap();
        assert_eq!(removed, original);
    }

    #[test]
    fn toml_into_an_empty_file_has_no_empty_parent_header() {
        let added = insert(Format::Toml, "", "mcp_servers", "reliaburger", &ours()).unwrap();
        assert_eq!(
            added,
            "[mcp_servers.reliaburger]\nargs = [\"mcp\"]\ncommand = \"relish\"\n"
        );
    }

    #[test]
    fn yaml_round_trips_a_file_without_comments() {
        let original = "GOOSE_PROVIDER: anthropic\nextensions:\n  developer:\n    enabled: true\n";
        let added = insert(Format::Yaml, original, "extensions", "reliaburger", &ours()).unwrap();
        assert!(added.contains("GOOSE_PROVIDER: anthropic"), "{added}");
        assert_eq!(
            existing(Format::Yaml, &added, "extensions", "reliaburger", &ours()).unwrap(),
            Existing::Same
        );
        let removed = remove(Format::Yaml, &added, "extensions", "reliaburger").unwrap();
        assert_eq!(removed, original);
    }

    #[test]
    fn yaml_with_comments_is_refused() {
        let text = "# keep me\nextensions: {}\n";
        assert_eq!(
            insert(Format::Yaml, text, "extensions", "reliaburger", &ours()).unwrap_err(),
            EditError::HasComments
        );
    }
}
