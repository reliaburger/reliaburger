//! Keep the repository's front door honest (#1580).
//!
//! The issue forms in `.github/ISSUE_TEMPLATE/` are the report contract:
//! every form demands a version, a reproducer, what wasn't tested and an
//! AI-disclosure answer, and the agent-drafted form's fields are the ones
//! `relish agent report` (#1575) fills in. GitHub only checks a form when
//! someone opens the new-issue page, so a typo would ship silently. These
//! tests parse each form into the subset of GitHub's issue-form schema we
//! use, with unknown keys refused, and check the policy files the forms
//! point at.

use serde::Deserialize;
use std::collections::BTreeSet;

use crate::website::{read, repository};

const FORMS: &[&str] = &[
    "bug.yml",
    "agent-bug.yml",
    "docs-error.yml",
    "test-request.yml",
];

/// Labels that existed before #1580 and that a form may apply.
const EXISTING_LABELS: &[&str] = &["bug", "documentation"];

/// The labels #1580 created to route triage; CONTRIBUTING.md explains each.
const TRIAGE_LABELS: &[&str] = &[
    "agent-drafted",
    "has-reproducer",
    "patch-tested",
    "unverified",
    "missed-mutant",
];

/// The agent-drafted form, field by field: the id a pre-filled form URL
/// uses, the label `relish agent report` writes as a `### ` heading, and
/// whether GitHub requires it. Changing this list changes the report's
/// contract, so change `relish agent report` with it.
const AGENT_REPORT_FIELDS: &[(&str, &str, bool)] = &[
    ("fingerprint", "Fingerprint", true),
    ("version", "Version", true),
    ("fix_status", "Fix status", true),
    ("summary", "What happened", true),
    ("reproducer", "Reproducer", true),
    ("evidence", "Evidence", true),
    ("source", "Suspected source", false),
    ("patch", "Patch", false),
    ("not_tested", "What was not tested", true),
    (
        "should_have_caught",
        "How this should have been caught",
        true,
    ),
    ("redaction", "Redaction", true),
    ("ai_disclosure", "AI assistance", true),
    ("human_review", "Human review", true),
];

const AI_DISCLOSURE_OPTIONS: &[&str] = &[
    "No AI assistance",
    "An AI tool helped, and a human checked every line",
    "An AI agent drafted it, and a human checked every line",
];

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Form {
    name: String,
    description: String,
    title: Option<String>,
    #[serde(default)]
    labels: Vec<String>,
    body: Vec<Element>,
}

/// One body element. `#[serde(tag = "type")]` reads the `type:` key to pick
/// the variant, the way GitHub does.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase", deny_unknown_fields)]
enum Element {
    Markdown {
        attributes: MarkdownAttributes,
    },
    Input {
        id: String,
        attributes: TextAttributes,
        validations: Option<Validations>,
    },
    Textarea {
        id: String,
        attributes: TextAttributes,
        validations: Option<Validations>,
    },
    Dropdown {
        id: String,
        attributes: DropdownAttributes,
        validations: Option<Validations>,
    },
    Checkboxes {
        id: String,
        attributes: CheckboxAttributes,
    },
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct MarkdownAttributes {
    value: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TextAttributes {
    label: String,
    description: Option<String>,
    placeholder: Option<String>,
    render: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct DropdownAttributes {
    label: String,
    description: Option<String>,
    options: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CheckboxAttributes {
    label: String,
    options: Vec<Checkbox>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Checkbox {
    label: String,
    #[serde(default)]
    required: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Validations {
    required: bool,
}

/// What the tests need from a field, whatever its type.
struct Field<'a> {
    id: &'a str,
    label: &'a str,
    required: bool,
    kind: &'static str,
}

impl Element {
    fn field(&self) -> Option<Field<'_>> {
        let required =
            |validations: &Option<Validations>| validations.as_ref().is_some_and(|v| v.required);
        match self {
            Element::Markdown { .. } => None,
            Element::Input {
                id,
                attributes,
                validations,
            } => Some(Field {
                id,
                label: &attributes.label,
                required: required(validations),
                kind: "input",
            }),
            Element::Textarea {
                id,
                attributes,
                validations,
            } => Some(Field {
                id,
                label: &attributes.label,
                required: required(validations),
                kind: "textarea",
            }),
            Element::Dropdown {
                id,
                attributes,
                validations,
            } => Some(Field {
                id,
                label: &attributes.label,
                required: required(validations),
                kind: "dropdown",
            }),
            Element::Checkboxes { id, attributes } => Some(Field {
                id,
                label: &attributes.label,
                required: attributes.options.iter().all(|option| option.required),
                kind: "checkboxes",
            }),
        }
    }
}

/// The code languages our textareas render as.
const RENDERERS: &[&str] = &["shell", "diff", "rust"];

fn assert_not_blank(file: &str, text: &Option<String>) {
    if let Some(text) = text {
        assert!(
            !text.trim().is_empty(),
            "{file}: a blank description or placeholder"
        );
    }
}

fn form(file: &str) -> Form {
    let text = read(&format!(".github/ISSUE_TEMPLATE/{file}"));
    yaml_serde::from_str(&text).unwrap_or_else(|error| panic!("{file}: {error}"))
}

#[test]
fn the_forms_directory_holds_exactly_the_listed_forms() {
    let directory = repository().join(".github/ISSUE_TEMPLATE");
    let found: BTreeSet<String> = std::fs::read_dir(directory)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .filter(|name| name != "config.yml")
        .collect();
    let listed: BTreeSet<String> = FORMS.iter().map(|form| form.to_string()).collect();
    assert_eq!(found, listed, "add a new form to FORMS so it gets checked");
}

#[test]
fn every_form_follows_githubs_issue_form_schema() {
    for file in FORMS {
        let form = form(file);
        assert!(!form.name.trim().is_empty(), "{file}: empty name");
        assert!(
            !form.description.trim().is_empty(),
            "{file}: empty description"
        );
        assert!(form.title.is_some(), "{file}: no title prefix");
        let fields: Vec<Field> = form.body.iter().filter_map(Element::field).collect();
        assert!(!fields.is_empty(), "{file}: no fields");

        let mut ids = BTreeSet::new();
        let mut labels = BTreeSet::new();
        for field in &fields {
            assert!(
                field
                    .id
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-'),
                "{file}: id {:?} may only use letters, digits, - and _",
                field.id
            );
            assert!(ids.insert(field.id), "{file}: duplicate id {:?}", field.id);
            assert!(
                labels.insert(field.label),
                "{file}: duplicate label {:?}",
                field.label
            );
        }
        for element in &form.body {
            match element {
                Element::Dropdown { attributes, .. } => {
                    assert!(!attributes.options.is_empty(), "{file}: empty dropdown");
                    assert_not_blank(file, &attributes.description);
                }
                Element::Checkboxes { attributes, .. } => {
                    assert!(!attributes.options.is_empty(), "{file}: empty checkboxes");
                    for option in &attributes.options {
                        assert!(!option.label.trim().is_empty(), "{file}: empty checkbox");
                    }
                }
                Element::Input { id, attributes, .. } => {
                    // GitHub accepts `render` only on a textarea.
                    assert!(attributes.render.is_none(), "{file}: input {id} has render");
                    assert_not_blank(file, &attributes.description);
                    assert_not_blank(file, &attributes.placeholder);
                }
                Element::Textarea { id, attributes, .. } => {
                    if let Some(render) = &attributes.render {
                        assert!(
                            RENDERERS.contains(&render.as_str()),
                            "{file}: {id} renders as unknown {render:?}"
                        );
                    }
                    assert_not_blank(file, &attributes.description);
                    assert_not_blank(file, &attributes.placeholder);
                }
                Element::Markdown { attributes } => {
                    assert!(
                        !attributes.value.trim().is_empty(),
                        "{file}: empty markdown"
                    )
                }
            }
        }
        for label in &form.labels {
            assert!(
                EXISTING_LABELS.contains(&label.as_str())
                    || TRIAGE_LABELS.contains(&label.as_str()),
                "{file}: label {label:?} isn't one the repository has"
            );
        }
    }
}

#[test]
fn every_form_requires_a_version_a_reproducer_what_was_not_tested_and_ai_disclosure() {
    for file in FORMS {
        let form = form(file);
        let fields: Vec<Field> = form.body.iter().filter_map(Element::field).collect();
        for (id, kind) in [
            ("version", "input"),
            ("reproducer", "textarea"),
            ("not_tested", "textarea"),
            ("ai_disclosure", "dropdown"),
        ] {
            let field = fields
                .iter()
                .find(|field| field.id == id)
                .unwrap_or_else(|| panic!("{file}: no {id} field"));
            assert!(field.required, "{file}: {id} must be required");
            assert_eq!(field.kind, kind, "{file}: {id} is the wrong type");
        }
        let disclosure = form
            .body
            .iter()
            .find_map(|element| match element {
                Element::Dropdown { id, attributes, .. } if id == "ai_disclosure" => {
                    Some(attributes.options.clone())
                }
                _ => None,
            })
            .unwrap();
        assert_eq!(
            disclosure, AI_DISCLOSURE_OPTIONS,
            "{file}: disclosure options differ"
        );
    }
}

#[test]
fn the_agent_form_fields_match_the_report_contract() {
    let form = form("agent-bug.yml");
    let fields: Vec<(&str, &str, bool)> = form
        .body
        .iter()
        .filter_map(Element::field)
        .map(|field| (field.id, field.label, field.required))
        .collect();
    assert_eq!(fields, AGENT_REPORT_FIELDS);
    assert!(form.labels.contains(&"agent-drafted".to_string()));
}

#[test]
fn blank_issues_are_off_and_vulnerabilities_go_to_private_reporting() {
    let config = read(".github/ISSUE_TEMPLATE/config.yml");
    let config: yaml_serde::Value = yaml_serde::from_str(&config).unwrap();
    assert_eq!(config["blank_issues_enabled"].as_bool(), Some(false));
    let links: Vec<&str> = config["contact_links"]
        .as_sequence()
        .unwrap()
        .iter()
        .map(|link| link["url"].as_str().unwrap())
        .collect();
    assert!(links.contains(&"https://github.com/reliaburger/reliaburger/security/advisories/new"));
    assert!(links.contains(&"https://github.com/reliaburger/reliaburger/discussions"));
}

#[test]
fn the_security_policy_reports_privately_and_offers_no_bounty() {
    let policy = read("SECURITY.md");
    assert!(policy.contains("https://github.com/reliaburger/reliaburger/security/advisories/new"));
    assert!(policy.contains("no bug bounty"));
    assert!(policy.contains("public issue"));
}

#[test]
fn the_ai_policy_is_in_the_contributing_guide_and_agents_md() {
    let contributing = read("CONTRIBUTING.md");
    let agents = read("AGENTS.md");
    for (file, text) in [("CONTRIBUTING.md", &contributing), ("AGENTS.md", &agents)] {
        for rule in [
            "Co-Authored-By",
            "never post",
            "failing",
            "no reproducer",
            "answers for every line",
        ] {
            assert!(
                text.contains(rule),
                "{file} doesn't state the rule {rule:?}"
            );
        }
    }
    for label in TRIAGE_LABELS {
        assert!(
            contributing.contains(&format!("`{label}`")),
            "CONTRIBUTING.md doesn't explain the {label} label"
        );
    }
}
