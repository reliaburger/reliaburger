//! `issue.md`: the draft, one section per field of the agent-drafted form.
//!
//! The form is `.github/ISSUE_TEMPLATE/agent-bug.yml` (#1580). GitHub
//! renders an issue filed through a form as `### <label>` and the answer
//! beneath it, with a `render: shell` or `render: diff` field in a code
//! block, so a body with the same headings in the same order reads like one
//! filed through the form, and the triage tooling that reads one reads the
//! other. A pre-filled form URL uses the fields' ids instead.

use std::fmt::Write as _;

use super::{
    BUNDLE_FILE, Fingerprint, FixStatus, INLINE_MAX_BYTES, MANIFEST_FILE, Manifest, REPOSITORY,
    ReportBuild,
};
use crate::relish::agent::source::REPOSITORY_URL;

/// The form's fields, in its order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Section {
    Fingerprint,
    Version,
    FixStatus,
    WhatHappened,
    Reproducer,
    Evidence,
    SuspectedSource,
    Patch,
    NotTested,
    ShouldHaveCaught,
    Redaction,
    AiAssistance,
    HumanReview,
}

impl Section {
    /// Every field, in the form's order.
    pub(super) const ALL: &'static [Section] = &[
        Section::Fingerprint,
        Section::Version,
        Section::FixStatus,
        Section::WhatHappened,
        Section::Reproducer,
        Section::Evidence,
        Section::SuspectedSource,
        Section::Patch,
        Section::NotTested,
        Section::ShouldHaveCaught,
        Section::Redaction,
        Section::AiAssistance,
        Section::HumanReview,
    ];

    /// The field's id, which a pre-filled form URL uses.
    pub(super) fn id(self) -> &'static str {
        match self {
            Section::Fingerprint => "fingerprint",
            Section::Version => "version",
            Section::FixStatus => "fix_status",
            Section::WhatHappened => "summary",
            Section::Reproducer => "reproducer",
            Section::Evidence => "evidence",
            Section::SuspectedSource => "source",
            Section::Patch => "patch",
            Section::NotTested => "not_tested",
            Section::ShouldHaveCaught => "should_have_caught",
            Section::Redaction => "redaction",
            Section::AiAssistance => "ai_disclosure",
            Section::HumanReview => "human_review",
        }
    }

    /// The field's label, which GitHub renders as the `### ` heading.
    pub(super) fn label(self) -> &'static str {
        match self {
            Section::Fingerprint => "Fingerprint",
            Section::Version => "Version",
            Section::FixStatus => "Fix status",
            Section::WhatHappened => "What happened",
            Section::Reproducer => "Reproducer",
            Section::Evidence => "Evidence",
            Section::SuspectedSource => "Suspected source",
            Section::Patch => "Patch",
            Section::NotTested => "What was not tested",
            Section::ShouldHaveCaught => "How this should have been caught",
            Section::Redaction => "Redaction",
            Section::AiAssistance => "AI assistance",
            Section::HumanReview => "Human review",
        }
    }

    /// The language GitHub fences the field's answer in, for the fields the
    /// form renders as code.
    fn code_language(self) -> Option<&'static str> {
        match self {
            Section::Reproducer | Section::Evidence => Some("shell"),
            Section::Patch => Some("diff"),
            _ => None,
        }
    }

    /// Whether a pre-filled URL can carry the field: GitHub fills in text
    /// fields from the query string, not dropdowns or checkboxes.
    fn prefillable(self) -> bool {
        !matches!(self, Section::AiAssistance | Section::HumanReview)
    }
}

/// The form's answer for a report an agent drafted.
pub(super) const AI_DISCLOSURE: &str = "An AI agent drafted it, and a human checked every line";

/// The form's human-review checkboxes, which only the human ticks.
pub(super) const HUMAN_REVIEW: &[&str] = &[
    "I'm a human, I read this whole report and I answer for it.",
    "I checked the redaction manifest and nothing private is left.",
    "I searched the issues for the fingerprint.",
];

/// The longest pre-filled form URL offered. Browsers and GitHub start
/// refusing long URLs ("414 URI Too Long") well before evidence fits.
pub(super) const PREFILL_URL_MAX: usize = 8 * 1024;

/// The text fields, already redacted.
pub(super) struct Sections {
    pub(super) what_happened: String,
    pub(super) expected: String,
    pub(super) reproducer: Option<String>,
    pub(super) not_tested: String,
    pub(super) caught_by: &'static str,
    pub(super) caught_how: Option<String>,
}

/// Everything `issue.md` is made from.
pub(super) struct Draft<'a> {
    pub(super) build: &'a ReportBuild,
    pub(super) fingerprint: Option<&'a Fingerprint>,
    pub(super) sections: &'a Sections,
    pub(super) fix: &'a FixStatus,
    pub(super) patch: Option<&'a str>,
    pub(super) bundle: &'a str,
    pub(super) manifest: &'a Manifest,
    pub(super) duplicate_of: Option<u64>,
    pub(super) include_logs: bool,
    pub(super) keep_addresses: bool,
}

/// One field's answer: prose, or text the form shows as code.
enum Answer {
    Text(String),
    Code(String),
}

impl Answer {
    fn text(&self) -> &str {
        match self {
            Answer::Text(text) | Answer::Code(text) => text,
        }
    }
}

impl Draft<'_> {
    /// The Markdown body.
    pub(super) fn render(&self) -> String {
        let mut out = String::new();
        // `writeln!` into a `String` can't fail; `fmt::Write` only returns a
        // `Result` because other writers can.
        let _ = writeln!(
            out,
            "<!-- drafted by relish agent report; a human reads it and posts it -->"
        );
        if let Some(number) = self.duplicate_of {
            let _ = writeln!(
                out,
                "\nMore evidence for #{number}, from another cluster. The fields below are this report's."
            );
        }
        for section in Section::ALL {
            let body = match (self.answer(*section), section.code_language()) {
                (Answer::Code(code), Some(language)) => fenced(&code, language),
                (answer, _) => answer.text().to_string(),
            };
            let _ = write!(out, "\n### {}\n\n{body}\n", section.label());
        }
        out
    }

    /// The agent-drafted form's URL with every text field filled in, or
    /// `None` when that would be too long to open (or the draft is a
    /// comment on another issue, which no form takes).
    pub(super) fn prefilled_url(&self, title: &str) -> Option<String> {
        if self.duplicate_of.is_some() {
            return None;
        }
        let mut query = url::form_urlencoded::Serializer::new(String::new());
        query.append_pair("template", "agent-bug.yml");
        query.append_pair("title", title);
        for section in Section::ALL.iter().filter(|section| section.prefillable()) {
            query.append_pair(section.id(), self.answer(*section).text());
        }
        let url = format!(
            "https://github.com/{REPOSITORY}/issues/new?{}",
            query.finish()
        );
        (url.len() <= PREFILL_URL_MAX).then_some(url)
    }

    fn answer(&self, section: Section) -> Answer {
        let sections = self.sections;
        match section {
            Section::Fingerprint => Answer::Text(self.fingerprint_answer()),
            Section::Version => Answer::Text(self.version_answer()),
            Section::FixStatus => Answer::Text(self.fix_answer()),
            Section::WhatHappened => Answer::Text(format!(
                "{}\n\nExpected: {}",
                sections.what_happened, sections.expected
            )),
            Section::Reproducer => match &sections.reproducer {
                Some(reproducer) => Answer::Code(reproducer.clone()),
                None => Answer::Text(
                    "None. This report is **unverified**: nobody has made it happen on purpose yet."
                        .to_string(),
                ),
            },
            Section::Evidence => self.evidence_answer(),
            Section::SuspectedSource => Answer::Text(self.source_answer()),
            Section::Patch => match self.patch {
                None => Answer::Text("None.".to_string()),
                Some(patch) if patch.len() <= INLINE_MAX_BYTES => Answer::Code(patch.to_string()),
                Some(patch) => Answer::Text(format!(
                    "{} bytes, too long to quote here; it's in `{BUNDLE_FILE}`.",
                    patch.len()
                )),
            },
            Section::NotTested => Answer::Text(sections.not_tested.clone()),
            Section::ShouldHaveCaught => Answer::Text(match &sections.caught_how {
                Some(how) => format!("{}: {how}", sections.caught_by),
                None => format!("{}.", sections.caught_by),
            }),
            Section::Redaction => Answer::Text(self.redaction_answer()),
            Section::AiAssistance => Answer::Text(AI_DISCLOSURE.to_string()),
            Section::HumanReview => Answer::Text(
                HUMAN_REVIEW
                    .iter()
                    .map(|label| format!("- [ ] {label}"))
                    .collect::<Vec<_>>()
                    .join("\n"),
            ),
        }
    }

    fn fingerprint_answer(&self) -> String {
        let Some(fingerprint) = self.fingerprint else {
            return format!(
                "None: no error code, finding or site was given (seen on {}).",
                self.build.version
            );
        };
        let part = |value: &Option<String>| match value {
            Some(value) => format!("`{value}`"),
            None => "none".to_string(),
        };
        format!(
            "`{}` (error code {}, finding {}, site {}; seen on {})",
            fingerprint.id,
            part(&fingerprint.error_code),
            part(&fingerprint.finding),
            part(&fingerprint.site),
            fingerprint.version,
        )
    }

    fn version_answer(&self) -> String {
        let build = self.build;
        let source = match build.dirty {
            Some(false) => "source as committed",
            Some(true) => "source changed from that commit",
            None => "source state unknown",
        };
        format!(
            "relish {}, commit {}, {source}, {}, formats protocol {} / state {}",
            build.version,
            build.commit.as_deref().unwrap_or("unknown"),
            build.target.as_deref().unwrap_or("target unknown"),
            build.formats.protocol,
            build.formats.state,
        )
    }

    fn fix_answer(&self) -> String {
        match self.fix {
            FixStatus::Unknown => {
                "Not known: the agent didn't find a release with the fix.".to_string()
            }
            FixStatus::ReleasedLater { fixed_in } => format!(
                "Fixed in {fixed_in}, newer than the {} this ran: upgrading may be all that's needed.",
                self.build.version
            ),
            FixStatus::InThisVersion { fixed_in } => format!(
                "{fixed_in} was meant to fix it, and this ran {}: a regression, or a different bug.",
                self.build.version
            ),
        }
    }

    /// The site, with a permalink when the build is clean and knows its
    /// commit: a dirty tree may not match what GitHub has.
    fn source_answer(&self) -> String {
        let Some(site) = self
            .fingerprint
            .and_then(|fingerprint| fingerprint.site.as_deref())
        else {
            return "None.".to_string();
        };
        let path = site.split("::").next().unwrap_or(site);
        match (&self.build.commit, self.build.dirty) {
            (Some(commit), Some(false)) => {
                format!("`{site}`, at {REPOSITORY_URL}/blob/{commit}/{path}")
            }
            _ => format!("`{site}`"),
        }
    }

    /// The redacted bundle, quoted when it's short enough.
    fn evidence_answer(&self) -> Answer {
        if self.bundle.len() <= INLINE_MAX_BYTES {
            Answer::Code(self.bundle.to_string())
        } else {
            Answer::Text(format!(
                "`{BUNDLE_FILE}` is {} bytes, too long to quote here: attach it to the issue.",
                self.bundle.len()
            ))
        }
    }

    /// What redaction did, from the manifest.
    fn redaction_answer(&self) -> String {
        let mut out = format!(
            "Addresses {}; log lines {}. From `{MANIFEST_FILE}`, what went in and what redaction took out:\n\n\
             | Item | Bytes | In | Redacted |\n|---|---:|---|---|\n",
            if self.keep_addresses {
                "kept"
            } else {
                "aliased"
            },
            if self.include_logs {
                "included, redacted"
            } else {
                "left out"
            },
        );
        for (kind, name, bytes, included, redactions) in self.manifest.rows() {
            let _ = writeln!(
                out,
                "| {kind} `{name}` | {bytes} | {included} | {redactions} |"
            );
        }
        out.trim_end().to_string()
    }
}

/// `text` in a code block whose fence is longer than any run of backticks
/// inside it, so a patch that adds a Markdown file can't end the block.
fn fenced(text: &str, language: &str) -> String {
    let mut longest = 0;
    let mut run = 0;
    for c in text.chars() {
        run = if c == '`' { run + 1 } else { 0 };
        longest = longest.max(run);
    }
    let fence = "`".repeat((longest + 1).max(3));
    format!("{fence}{language}\n{}\n{fence}", text.trim_end())
}
