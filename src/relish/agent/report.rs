//! `relish agent report`: a redacted bug report, drafted for a human to post.
//!
//! The command writes three files into a new directory and makes no network
//! call at all:
//!
//! - `issue.md`, the issue body, with one `### ` section per field of the
//!   repository's agent-drafted issue form, in the form's order, so a draft
//!   posted with `gh issue create --body-file` reads like one filed through
//!   the form;
//! - `bundle.json`, the redacted evidence the agent passed with
//!   `--evidence`, and the patch;
//! - `MANIFEST.json`, every item that went in (or was left out), its size,
//!   and which redaction rules fired how often.
//!
//! It then prints the `gh` commands to search for a duplicate and to post,
//! as `next` steps. Posting is never `read_only`, so an agent asks a human
//! first. Every piece of text, whoever wrote it, passes through [`scrub`]:
//! the shared secret rules from [`crate::redact`], then address aliasing.

mod issue;
mod scrub;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use schemars::JsonSchema;
use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};

use super::budget::{self, Rendered};
use super::envelope::{AgentError, BuildInfo, CommandName, Envelope, ErrorCode, NextStep};
use super::{Formats, shell_quote};
use scrub::{ItemCounts, Scrubber};

/// The repository reports go to.
pub const REPOSITORY: &str = "reliaburger/reliaburger";

/// The longest a text field may be once read, in bytes.
pub const MAX_FIELD_BYTES: usize = 8 * 1024;

/// The longest an evidence file may be; a bigger one is left out.
pub const MAX_EVIDENCE_BYTES: usize = 1024 * 1024;

/// The longest a patch may be.
pub const MAX_PATCH_BYTES: usize = 256 * 1024;

/// The largest bundle or patch `issue.md` quotes inline; a bigger one stays
/// in its file for the human to attach.
pub const INLINE_MAX_BYTES: usize = 32 * 1024;

/// The longest issue body GitHub accepts, in characters.
pub const GITHUB_BODY_MAX: usize = 65_536;

/// What `issue.md`, `bundle.json` and `MANIFEST.json` are called.
pub const ISSUE_FILE: &str = "issue.md";
/// See [`ISSUE_FILE`].
pub const BUNDLE_FILE: &str = "bundle.json";
/// See [`ISSUE_FILE`].
pub const MANIFEST_FILE: &str = "MANIFEST.json";

/// `relish agent report`'s arguments. A text argument starting with `@`
/// is read from that file, as `curl` does.
#[derive(Debug, Clone, clap::Args)]
pub struct ReportArgs {
    /// The issue title, one line.
    #[arg(long)]
    pub title: String,
    /// What happened (text, or @file).
    #[arg(long)]
    pub what_happened: String,
    /// What should have happened instead (text, or @file).
    #[arg(long)]
    pub expected: String,
    /// What wasn't tried or checked (text, or @file).
    #[arg(long)]
    pub not_tested: String,
    /// What should have caught this before a release.
    #[arg(long, value_enum)]
    pub caught_by: CaughtBy,
    /// How: the test, the gate or the fault, in a sentence (text, or @file).
    #[arg(long)]
    pub caught_how: Option<String>,
    /// Steps or a script that make it happen on purpose (text, or @file).
    /// Without one the report says "unverified" in its title.
    #[arg(long)]
    pub reproducer: Option<String>,
    /// A file of evidence: a `relish agent` answer, JSON Lines, or text.
    /// Repeat for several. A `.log` file goes in only with --include-logs.
    #[arg(long)]
    pub evidence: Vec<PathBuf>,
    /// A diff of the fix, such as `git diff` in an extracted source tree.
    #[arg(long)]
    pub patch: Option<PathBuf>,
    /// For the fingerprint: the error code or variant the failure raised.
    #[arg(long)]
    pub error_code: Option<String>,
    /// For the fingerprint: the finding id, such as `node-dead`.
    #[arg(long)]
    pub finding: Option<String>,
    /// For the fingerprint: where the bug lives, `path::function`.
    #[arg(long)]
    pub site: Option<String>,
    /// An existing issue this is the same bug as: draft a comment for it.
    #[arg(long)]
    pub duplicate_of: Option<u64>,
    /// The release that fixed it, when the agent found one.
    #[arg(long)]
    pub fixed_in: Option<String>,
    /// Keep log lines in the evidence (still redacted). Left out by default.
    #[arg(long)]
    pub include_logs: bool,
    /// Keep host names and IP addresses. They're aliased by default.
    #[arg(long)]
    pub keep_addresses: bool,
    /// The directory to write; it must not exist or be empty.
    #[arg(long)]
    pub out: PathBuf,
}

/// What should have caught the bug: the "how this should have been
/// caught" field of the issue form.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema, clap::ValueEnum)]
#[serde(rename_all = "snake_case")]
pub enum CaughtBy {
    /// A unit or integration test that doesn't exist yet.
    Test,
    /// A release gate or qualification run.
    Gate,
    /// A Smoker fault we don't inject yet.
    SmokerFault,
    /// Code review.
    Review,
    /// Nobody can say yet.
    Unknown,
}

impl CaughtBy {
    /// The words `issue.md` uses for it.
    fn describe(self) -> &'static str {
        match self {
            CaughtBy::Test => "A test",
            CaughtBy::Gate => "A release gate",
            CaughtBy::SmokerFault => "A Smoker fault we don't inject yet",
            CaughtBy::Review => "Code review",
            CaughtBy::Unknown => "Not known yet",
        }
    }
}

/// `data` for `relish agent report`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub struct ReportData {
    /// The directory written, as given.
    pub dir: String,
    /// The issue title, redacted, after the form's `agent: ` prefix, with
    /// `[unverified]` when there's no reproducer.
    pub title: String,
    /// Whether a reproducer came with the report.
    pub verified: bool,
    /// The fingerprint to search for duplicates with, when there's
    /// something to make one from.
    pub fingerprint: Option<Fingerprint>,
    /// The labels the agent-drafted issue form applies.
    pub labels: Vec<String>,
    /// The issue this adds evidence to, when it's a duplicate.
    pub duplicate_of: Option<u64>,
    /// Whether a release already has the fix.
    pub fix: FixStatus,
    /// The files written, with their sizes.
    pub files: Vec<WrittenFile>,
    /// How many times each redaction rule fired, over everything.
    pub redactions: BTreeMap<String, usize>,
    /// What was left out and why.
    pub left_out: Vec<String>,
    /// The command a human runs to post it. Never run without them.
    pub post: String,
    /// The agent-drafted form, pre-filled, when the URL is short enough to
    /// open: another way for the human to post, in the browser.
    pub new_issue_url: Option<String>,
}

/// A failure's fingerprint, for finding the same bug in other reports.
///
/// `id` hashes the error code, the finding and the site, and not the
/// version, so the same bug reported from two releases has the same id.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub struct Fingerprint {
    /// `rbfp-` and 12 hex digits.
    pub id: String,
    /// The version that saw it.
    pub version: String,
    /// The error code or variant, when there was one.
    pub error_code: Option<String>,
    /// The finding id, when a diagnosis produced one.
    pub finding: Option<String>,
    /// `path::function`, when the agent found where the bug lives.
    pub site: Option<String>,
}

/// Whether the fix has shipped, as far as the agent found out.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum FixStatus {
    /// Nobody said: the duplicate search and the release notes would tell.
    Unknown,
    /// A newer release than this binary has the fix: upgrading may be all
    /// that's needed.
    ReleasedLater {
        /// The release with the fix.
        fixed_in: String,
    },
    /// This binary's release, or an older one, was meant to have the fix:
    /// a regression, or a different bug.
    InThisVersion {
        /// The release that was meant to fix it.
        fixed_in: String,
    },
}

/// One file the command wrote.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub struct WrittenFile {
    /// The file's path, inside `dir`.
    pub path: String,
    /// Its size in bytes.
    pub bytes: usize,
}

/// What the build knows about itself, for the version section and the
/// bundle.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReportBuild {
    /// The Cargo package version.
    pub version: String,
    /// The commit the binary was built from, when the build knew it.
    pub commit: Option<String>,
    /// Whether the bundled source had uncommitted changes, when git could say.
    pub dirty: Option<bool>,
    /// The Rust target triple.
    pub target: Option<String>,
    /// The cluster formats this binary speaks.
    pub formats: Formats,
}

impl ReportBuild {
    /// This binary's build.
    pub fn current() -> Self {
        let version = super::version_data();
        ReportBuild {
            version: version.version,
            commit: version.commit,
            dirty: crate::relish::source_bundle::dirty(),
            target: version.target,
            formats: version.formats,
        }
    }
}

/// Run `relish agent report`.
pub fn run(args: &ReportArgs, max_bytes: usize, rerun: &str) -> Rendered {
    // The command line carries the agent's own text, which may quote a
    // secret; the budget's hint repeats it, so it's redacted too.
    let rerun = crate::redact::redact_secrets(rerun);
    let build = BuildInfo::current();
    let result = report(args, &ReportBuild::current());
    render(result, build, max_bytes, &rerun)
}

/// Put the answer (or the failure) in an envelope.
fn render(
    result: Result<Answer, AgentError>,
    build: BuildInfo,
    max_bytes: usize,
    rerun: &str,
) -> Rendered {
    match result {
        Ok(answer) => {
            let mut envelope = Envelope::answered(CommandName::Report, build, answer.data);
            for warning in answer.warnings {
                envelope = envelope.with_warning(warning);
            }
            for step in answer.next {
                envelope = envelope.with_next(step);
            }
            budget::render(&envelope, max_bytes, rerun)
        }
        Err(error) => {
            let envelope: Envelope<()> = Envelope::failed(CommandName::Report, build, error);
            budget::render(&envelope, max_bytes, rerun)
        }
    }
}

/// The answer, before it goes into an envelope.
#[derive(Debug, Clone)]
pub struct Answer {
    /// The command's `data`.
    pub data: ReportData,
    /// The search, the read-through and the post.
    pub next: Vec<NextStep>,
    /// Warnings about the draft.
    pub warnings: Vec<String>,
}

fn usage(message: impl Into<String>) -> AgentError {
    AgentError::new(ErrorCode::Usage, message)
}

/// Draft the report for `build` and write it into `args.out`.
pub fn report(args: &ReportArgs, build: &ReportBuild) -> Result<Answer, AgentError> {
    let fingerprint = fingerprint(args, &build.version)?;
    let fix = fix_status(args.fixed_in.as_deref(), &build.version)?;
    let fields = Fields::read(args)?;
    let evidence = args
        .evidence
        .iter()
        .map(|path| Evidence::read(path, args.include_logs))
        .collect::<Result<Vec<_>, _>>()?;
    let patch = read_patch(args.patch.as_deref())?;
    check_out_dir(&args.out)?;

    let parsed: Vec<&Value> = evidence.iter().filter_map(Evidence::json).collect();
    let mut scrubber = Scrubber::new(args.keep_addresses, args.include_logs, &parsed);
    let mut manifest = Manifest::default();

    let title_text = field(&mut scrubber, &fields.title, "title", &mut manifest);
    let verified = fields.reproducer.is_some();
    // The form's title prefix, then the verdict the kernel's rule asks for.
    let title = if verified {
        format!("agent: {title_text}")
    } else {
        format!("agent: [unverified] {title_text}")
    };
    let scrubbed = issue::Sections {
        what_happened: field(
            &mut scrubber,
            &fields.what_happened,
            "what_happened",
            &mut manifest,
        ),
        expected: field(&mut scrubber, &fields.expected, "expected", &mut manifest),
        reproducer: fields
            .reproducer
            .as_deref()
            .map(|text| field(&mut scrubber, text, "reproducer", &mut manifest)),
        not_tested: field(
            &mut scrubber,
            &fields.not_tested,
            "not_tested",
            &mut manifest,
        ),
        caught_by: args.caught_by.describe(),
        caught_how: fields
            .caught_how
            .as_deref()
            .map(|text| field(&mut scrubber, text, "caught_how", &mut manifest)),
    };
    let bundled: Vec<Value> = evidence
        .into_iter()
        .filter_map(|item| item.scrub(&mut scrubber, &mut manifest))
        .collect();
    let patch = patch.map(|(name, text)| {
        let mut counts = ItemCounts::default();
        let text = scrubber.patch(&text, &mut counts);
        manifest.push(Item::included("patch", &name, text.len(), counts));
        text
    });

    let bundle = serde_json::json!({
        "schema": "relish.agent.report/v1",
        "relish": build,
        "fingerprint": fingerprint,
        "evidence": bundled,
        "patch": patch,
    });
    let bundle_json = pretty(&bundle)?;
    let draft = issue::Draft {
        build,
        fingerprint: fingerprint.as_ref(),
        sections: &scrubbed,
        fix: &fix,
        patch: patch.as_deref(),
        bundle: &bundle_json,
        manifest: &manifest,
        duplicate_of: args.duplicate_of,
        include_logs: args.include_logs,
        keep_addresses: args.keep_addresses,
    };
    let issue_md = draft.render();
    let new_issue_url = draft.prefilled_url(&title);

    let manifest_json = pretty(&manifest.document(
        &[(ISSUE_FILE, &issue_md), (BUNDLE_FILE, &bundle_json)],
        args.include_logs,
        args.keep_addresses,
    ))?;
    write_files(
        &args.out,
        &[
            (ISSUE_FILE, &issue_md),
            (BUNDLE_FILE, &bundle_json),
            (MANIFEST_FILE, &manifest_json),
        ],
    )?;

    let dir = args.out.display().to_string();
    let post = post_command(&dir, &title, args.duplicate_of);
    let mut labels = vec!["bug".to_string(), "agent-drafted".to_string()];
    labels.push(
        if verified {
            "has-reproducer"
        } else {
            "unverified"
        }
        .to_string(),
    );
    let data = ReportData {
        dir: dir.clone(),
        title,
        verified,
        fingerprint: fingerprint.clone(),
        labels,
        duplicate_of: args.duplicate_of,
        fix: fix.clone(),
        files: [
            (ISSUE_FILE, issue_md.len()),
            (BUNDLE_FILE, bundle_json.len()),
            (MANIFEST_FILE, manifest_json.len()),
        ]
        .into_iter()
        .map(|(path, bytes)| WrittenFile {
            path: path.to_string(),
            bytes,
        })
        .collect(),
        redactions: manifest.totals(),
        left_out: manifest.left_out(),
        post: post.clone(),
        new_issue_url,
    };
    let mut warnings = Vec::new();
    if !verified {
        warnings.push(
            "unverified: no --reproducer, so the title says so; a maintainer reads reproduced reports first"
                .to_string(),
        );
    }
    if fingerprint.is_none() {
        warnings.push(
            "no fingerprint: pass --error-code, --finding or --site so duplicates can be found"
                .to_string(),
        );
    }
    if let FixStatus::ReleasedLater { fixed_in } = &fix {
        warnings.push(format!(
            "the fix is in {fixed_in}, newer than this {}: upgrading may be all that's needed",
            build.version
        ));
    }
    if issue_md.chars().count() > GITHUB_BODY_MAX {
        warnings.push(format!(
            "{ISSUE_FILE} is over GitHub's {GITHUB_BODY_MAX}-character limit; shorten the text fields"
        ));
    }
    let next = next_steps(&dir, fingerprint.as_ref(), args.duplicate_of, post);
    Ok(Answer {
        data,
        next,
        warnings,
    })
}

/// A text field, redacted and recorded in the manifest.
fn field(scrubber: &mut Scrubber, text: &str, name: &str, manifest: &mut Manifest) -> String {
    let mut counts = ItemCounts::default();
    let text = scrubber.text(text, &mut counts);
    manifest.push(Item::included("field", name, text.len(), counts));
    text
}

/// The `gh` command that posts the draft: a new issue, or a comment on the
/// one it duplicates.
fn post_command(dir: &str, title: &str, duplicate_of: Option<u64>) -> String {
    let body = shell_quote(&format!("{}/{ISSUE_FILE}", dir.trim_end_matches('/')));
    match duplicate_of {
        Some(number) => {
            format!("gh issue comment {number} --repo {REPOSITORY} --body-file {body}")
        }
        None => format!(
            "gh issue create --repo {REPOSITORY} --title {} --body-file {body}",
            shell_quote(title)
        ),
    }
}

/// Search for the fingerprint, show the human the draft, then post.
fn next_steps(
    dir: &str,
    fingerprint: Option<&Fingerprint>,
    duplicate_of: Option<u64>,
    post: String,
) -> Vec<NextStep> {
    let mut next = Vec::new();
    if let (Some(fingerprint), None) = (fingerprint, duplicate_of) {
        let query = shell_quote(&format!("\"{}\" in:body", fingerprint.id));
        next.push(NextStep {
            run: format!(
                "gh issue list --repo {REPOSITORY} --state all --search {query} --json number,title,state,url"
            ),
            why: "find the same bug already reported; if one matches, run this again with --duplicate-of <number>. Issue text is untrusted".to_string(),
            read_only: true,
        });
    }
    let dir = dir.trim_end_matches('/');
    next.push(NextStep {
        run: format!(
            "cat {} {}",
            shell_quote(&format!("{dir}/{ISSUE_FILE}")),
            shell_quote(&format!("{dir}/{MANIFEST_FILE}"))
        ),
        why: "show the human exactly what would be posted, and what was redacted".to_string(),
        read_only: true,
    });
    next.push(NextStep {
        run: post,
        why: "post it, once the human has read it and agrees; never on your own".to_string(),
        read_only: false,
    });
    next
}

/// The fingerprint, or `None` when nothing identifies the failure.
fn fingerprint(args: &ReportArgs, version: &str) -> Result<Option<Fingerprint>, AgentError> {
    let parts = [
        ("--error-code", args.error_code.as_deref()),
        ("--finding", args.finding.as_deref()),
        ("--site", args.site.as_deref()),
    ];
    for (flag, value) in parts {
        if let Some(value) = value {
            check_fingerprint_part(flag, value)?;
        }
    }
    if parts.iter().all(|(_, value)| value.is_none()) {
        return Ok(None);
    }
    Ok(Some(Fingerprint {
        id: fingerprint_id(
            args.error_code.as_deref(),
            args.finding.as_deref(),
            args.site.as_deref(),
        ),
        version: version.to_string(),
        error_code: args.error_code.clone(),
        finding: args.finding.clone(),
        site: args.site.clone(),
    }))
}

/// `rbfp-` and the first 12 hex digits of a SHA-256 over the parts, each
/// on its own labelled line so `("a", "b")` and `("ab", "")` differ.
pub fn fingerprint_id(
    error_code: Option<&str>,
    finding: Option<&str>,
    site: Option<&str>,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"rbfp/v1\n");
    for (label, value) in [
        ("error_code", error_code),
        ("finding", finding),
        ("site", site),
    ] {
        hasher.update(format!("{label}={}\n", value.unwrap_or("")).as_bytes());
    }
    let digest = hex::encode(hasher.finalize());
    format!("rbfp-{}", &digest[..12])
}

/// A fingerprint part is an identifier, not prose: it can't carry a secret
/// or an address, because it isn't redacted (that would change the hash).
fn check_fingerprint_part(flag: &str, value: &str) -> Result<(), AgentError> {
    let fits = !value.is_empty()
        && value.len() <= 128
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "_.:/-".contains(c));
    if fits {
        Ok(())
    } else {
        Err(usage(format!(
            "{flag} must be an identifier (letters, digits and _.:/-, at most 128), not {value:?}"
        ))
        .with_hint(
            "--error-code unreachable, --finding node-dead, --site src/relish/commands.rs::logs",
        ))
    }
}

/// Compare `--fixed-in` with this binary's version.
fn fix_status(fixed_in: Option<&str>, version: &str) -> Result<FixStatus, AgentError> {
    let Some(fixed_in) = fixed_in else {
        return Ok(FixStatus::Unknown);
    };
    let parse = |text: &str| semver::Version::parse(text.trim_start_matches('v'));
    let fixed = parse(fixed_in).map_err(|_| {
        usage(format!(
            "--fixed-in {fixed_in:?} isn't a version like 0.2.8"
        ))
    })?;
    let fixed_in = fixed.to_string();
    match parse(version) {
        Ok(running) if fixed > running => Ok(FixStatus::ReleasedLater { fixed_in }),
        Ok(_) => Ok(FixStatus::InThisVersion { fixed_in }),
        Err(_) => Ok(FixStatus::Unknown),
    }
}

/// The text fields, read from `@file` where asked.
struct Fields {
    title: String,
    what_happened: String,
    expected: String,
    not_tested: String,
    caught_how: Option<String>,
    reproducer: Option<String>,
}

impl Fields {
    fn read(args: &ReportArgs) -> Result<Self, AgentError> {
        let title = read_text("--title", &args.title)?;
        if title.contains('\n') {
            return Err(usage("--title must be one line"));
        }
        let optional = |flag: &str, value: &Option<String>| {
            value
                .as_deref()
                .map(|value| read_text(flag, value))
                .transpose()
        };
        Ok(Fields {
            title,
            what_happened: read_text("--what-happened", &args.what_happened)?,
            expected: read_text("--expected", &args.expected)?,
            not_tested: read_text("--not-tested", &args.not_tested)?,
            caught_how: optional("--caught-how", &args.caught_how)?,
            reproducer: optional("--reproducer", &args.reproducer)?,
        })
    }
}

/// `value`, or the file it names after `@`, trimmed; empty is refused.
fn read_text(flag: &str, value: &str) -> Result<String, AgentError> {
    let text = match value.strip_prefix('@') {
        Some(path) => std::fs::read_to_string(path)
            .map_err(|error| usage(format!("{flag}: couldn't read {path}: {error}")))?,
        None => value.to_string(),
    };
    let text = text.trim().to_string();
    if text.is_empty() {
        return Err(usage(format!("{flag} is empty")));
    }
    if text.len() > MAX_FIELD_BYTES {
        return Err(usage(format!(
            "{flag} is {} bytes; keep it under {MAX_FIELD_BYTES} and put the rest in --evidence",
            text.len()
        )));
    }
    Ok(text)
}

/// The patch file's name and text.
fn read_patch(path: Option<&Path>) -> Result<Option<(String, String)>, AgentError> {
    let Some(path) = path else {
        return Ok(None);
    };
    let text = std::fs::read_to_string(path).map_err(|error| {
        usage(format!(
            "--patch: couldn't read {}: {error}",
            path.display()
        ))
    })?;
    if text.len() > MAX_PATCH_BYTES {
        return Err(usage(format!(
            "--patch is {} bytes; at most {MAX_PATCH_BYTES}",
            text.len()
        )));
    }
    Ok(Some((file_name(path), text)))
}

/// The last part of `path`: enough to tell items apart, without the
/// directories, which can name the user.
fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

/// One `--evidence` file, read and classified.
enum Evidence {
    /// One JSON document, such as a `relish agent` answer.
    Json { name: String, value: Value },
    /// JSON Lines: one document per line.
    JsonLines { name: String, value: Value },
    /// Anything else that's text.
    Text { name: String, text: String },
    /// Read, but not going in.
    LeftOut {
        name: String,
        bytes: usize,
        why: String,
    },
}

impl Evidence {
    fn read(path: &Path, include_logs: bool) -> Result<Self, AgentError> {
        let bytes = std::fs::read(path).map_err(|error| {
            usage(format!(
                "--evidence: couldn't read {}: {error}",
                path.display()
            ))
        })?;
        let name = file_name(path);
        let size = bytes.len();
        let left_out = |why: &str| Evidence::LeftOut {
            name: name.clone(),
            bytes: size,
            why: why.to_string(),
        };
        if size > MAX_EVIDENCE_BYTES {
            return Ok(left_out(&format!("over {MAX_EVIDENCE_BYTES} bytes")));
        }
        let Ok(text) = String::from_utf8(bytes) else {
            return Ok(left_out("not UTF-8 text"));
        };
        if let Ok(value) = serde_json::from_str::<Value>(&text) {
            return Ok(Evidence::Json { name, value });
        }
        let lines: Result<Vec<Value>, _> = text
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(serde_json::from_str::<Value>)
            .collect();
        if let Ok(lines) = lines.as_ref()
            && !lines.is_empty()
        {
            return Ok(Evidence::JsonLines {
                name,
                value: Value::Array(lines.clone()),
            });
        }
        let is_log = path.extension().is_some_and(|extension| extension == "log");
        if is_log && !include_logs {
            return Ok(left_out("a log file; pass --include-logs to add it"));
        }
        Ok(Evidence::Text { name, text })
    }

    fn json(&self) -> Option<&Value> {
        match self {
            Evidence::Json { value, .. } | Evidence::JsonLines { value, .. } => Some(value),
            _ => None,
        }
    }

    /// The bundle entry for this evidence, redacted, recording it in the
    /// manifest either way.
    fn scrub(self, scrubber: &mut Scrubber, manifest: &mut Manifest) -> Option<Value> {
        let mut counts = ItemCounts::default();
        let (name, format, content) = match self {
            Evidence::LeftOut { name, bytes, why } => {
                manifest.push(Item::left_out("evidence", &name, bytes, why));
                return None;
            }
            Evidence::Json { name, value } => (name, "json", scrubber.json(value, &mut counts)),
            Evidence::JsonLines { name, value } => {
                (name, "json_lines", scrubber.json(value, &mut counts))
            }
            Evidence::Text { name, text } => (
                name,
                "text",
                Value::String(scrubber.text(&text, &mut counts)),
            ),
        };
        let bytes = content.to_string().len();
        let lines = counts.log_lines_left_out;
        let mut item = Item::included("evidence", &name, bytes, counts);
        if lines > 0 {
            item.note = Some(format!(
                "{lines} log lines left out; pass --include-logs to keep them"
            ));
        }
        manifest.push(item);
        Some(serde_json::json!({ "name": name, "format": format, "content": content }))
    }
}

/// The out directory must be new or empty: a report never overwrites.
fn check_out_dir(dir: &Path) -> Result<(), AgentError> {
    match std::fs::read_dir(dir) {
        Ok(mut entries) => {
            if entries.next().is_some() {
                return Err(AgentError::new(
                    ErrorCode::Conflict,
                    format!("{} isn't empty", dir.display()),
                )
                .with_hint("name a directory that doesn't exist yet"));
            }
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(AgentError::new(
            ErrorCode::Internal,
            format!("couldn't read {}: {error}", dir.display()),
        )),
    }
}

fn write_files(dir: &Path, files: &[(&str, &String)]) -> Result<(), AgentError> {
    let failed = |error: std::io::Error| {
        AgentError::new(
            ErrorCode::Internal,
            format!("couldn't write {}: {error}", dir.display()),
        )
    };
    std::fs::create_dir_all(dir).map_err(failed)?;
    for (name, contents) in files {
        std::fs::write(dir.join(name), contents.as_bytes()).map_err(failed)?;
    }
    Ok(())
}

/// Pretty JSON with a final newline: these files are for a human to read.
fn pretty<T: Serialize>(value: &T) -> Result<String, AgentError> {
    serde_json::to_string_pretty(value)
        .map(|json| format!("{json}\n"))
        .map_err(|error| AgentError::new(ErrorCode::Internal, error.to_string()))
}

/// One thing that went into the report, or was left out.
#[derive(Debug, Clone, Serialize)]
struct Item {
    /// `field`, `evidence` or `patch`.
    kind: &'static str,
    /// The field's name, or the file's name without its directories.
    name: String,
    /// Bytes as written (after redaction), or as read when left out.
    bytes: usize,
    /// Whether it's in the report.
    included: bool,
    /// Rule name to how often it fired.
    redactions: BTreeMap<String, usize>,
    /// Why it was left out, or what was trimmed.
    note: Option<String>,
}

impl Item {
    fn included(kind: &'static str, name: &str, bytes: usize, counts: ItemCounts) -> Self {
        Item {
            kind,
            name: name.to_string(),
            bytes,
            included: true,
            redactions: counts.into_map(),
            note: None,
        }
    }

    fn left_out(kind: &'static str, name: &str, bytes: usize, why: String) -> Self {
        Item {
            kind,
            name: name.to_string(),
            bytes,
            included: false,
            redactions: BTreeMap::new(),
            note: Some(why),
        }
    }
}

/// Every item, in the order it was added.
#[derive(Debug, Clone, Default)]
struct Manifest {
    items: Vec<Item>,
}

impl Manifest {
    fn push(&mut self, item: Item) {
        self.items.push(item);
    }

    /// Every rule's count over every item.
    fn totals(&self) -> BTreeMap<String, usize> {
        let mut totals = BTreeMap::new();
        for item in &self.items {
            for (rule, count) in &item.redactions {
                *totals.entry(rule.clone()).or_insert(0) += count;
            }
        }
        totals
    }

    /// One line per item left out or trimmed.
    fn left_out(&self) -> Vec<String> {
        self.items
            .iter()
            .filter_map(|item| {
                let note = item.note.as_deref()?;
                Some(format!("{} {}: {note}", item.kind, item.name))
            })
            .collect()
    }

    /// The items as `issue.md` lists them: name, bytes, what was redacted.
    /// `In` is `yes` or `no`, with the note when there is one.
    fn rows(&self) -> impl Iterator<Item = (&str, &str, usize, String, String)> {
        self.items.iter().map(|item| {
            let redactions = if item.redactions.is_empty() {
                "none".to_string()
            } else {
                item.redactions
                    .iter()
                    .map(|(rule, count)| format!("{rule} {count}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            let included = if item.included { "yes" } else { "no" };
            let included = match &item.note {
                Some(note) => format!("{included} ({note})"),
                None => included.to_string(),
            };
            (
                item.kind,
                item.name.as_str(),
                item.bytes,
                included,
                redactions,
            )
        })
    }

    /// `MANIFEST.json`: the files with their hashes, then every item.
    fn document(
        &self,
        files: &[(&str, &String)],
        include_logs: bool,
        keep_addresses: bool,
    ) -> Value {
        let files: Vec<Value> = files
            .iter()
            .map(|(path, contents)| {
                serde_json::json!({
                    "path": path,
                    "bytes": contents.len(),
                    "sha256": hex::encode(Sha256::digest(contents.as_bytes())),
                })
            })
            .collect();
        serde_json::json!({
            "schema": "relish.agent.report-manifest/v1",
            "network": "none: nothing was sent anywhere",
            "addresses": if keep_addresses { "kept" } else { "aliased" },
            "logs": if include_logs { "included" } else { "left_out" },
            "files": files,
            "items": self.items,
            "redactions": self.totals(),
        })
    }
}

#[cfg(test)]
mod tests;
