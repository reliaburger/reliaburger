//! `relish agent source`: the code this binary was built from, for an agent.
//!
//! Every command here reads the source bundle (`source_bundle`) in memory:
//! `version` says what it is and whether the cluster runs the same commit,
//! `tree` lists it, `grep` searches it, `show` prints part of one file, and
//! `extract` writes it to a directory an agent can build and test in. Paths
//! are always repository-relative (`src/relish/client.rs`), the same in the
//! bundle, on GitHub and in an extracted copy.
//!
//! The commands are plain functions over a [`Bundle`], so the tests run
//! them over a small bundle packed from a temporary tree. Only [`run`]
//! loads the real one and asks the cluster about its nodes.

mod extract;
mod grep;
mod nodes;
mod show;
mod tree;

use std::path::PathBuf;

use serde::Serialize;

use super::budget::{self, Rendered};
use super::envelope::{AgentError, BuildInfo, CommandName, Envelope, ErrorCode, NextStep};
use crate::relish::client::BunClient;
use crate::relish::source_bundle::{self, Origin, SourceBundleError, SourceFile};

pub(crate) use extract::extract;
pub use extract::{ExtractData, GitRecord, STAMP_FILE};
pub use grep::{GrepArgs, GrepData, GrepMatch};
pub use nodes::{ClusterSource, NodeSource};
pub use show::{EnclosingItem, ItemKind, MAX_SHOW_LINES, ShowData, ShownLine, enclosing_items};
pub use tree::{EntryKind, TreeData, TreeEntry};

/// Where permalinks and node commits point.
pub const REPOSITORY_URL: &str = "https://github.com/reliaburger/reliaburger";

/// `relish agent source`'s own flags and its subcommand.
#[derive(Debug, Clone, clap::Args)]
pub struct SourceOptions {
    /// Don't ask the cluster which commit its nodes run.
    #[arg(long, global = true)]
    pub offline: bool,

    #[command(subcommand)]
    pub command: SourceCommand,
}

/// The `relish agent source` commands.
#[derive(Debug, Clone, clap::Subcommand)]
pub enum SourceCommand {
    /// This binary's source: version, commit, digest, file count, and the
    /// commit every node runs.
    Version,
    /// Files with their size in bytes and their line count.
    Tree {
        /// Only paths under this one, e.g. `src/relish`.
        prefix: Option<String>,
        /// Roll up everything deeper than this many levels below the prefix
        /// into one entry per directory.
        #[arg(long, value_parser = clap::value_parser!(u32).range(1..))]
        depth: Option<u32>,
    },
    /// Search the source with a regular expression, line by line.
    Grep(GrepArgs),
    /// Print up to 400 numbered lines of one file: `path` or `path:from-to`.
    Show {
        /// A repository-relative path, optionally with a line range, e.g.
        /// `src/relish/client.rs:340-400`.
        target: String,
    },
    /// Write the source into a new or empty directory, as a git repository
    /// when git is installed.
    Extract {
        /// The directory to write; it must not exist or be empty.
        dir: PathBuf,
    },
}

impl SourceCommand {
    /// The name the envelope's `command` field carries.
    pub fn name(&self) -> CommandName {
        match self {
            SourceCommand::Version => CommandName::SourceVersion,
            SourceCommand::Tree { .. } => CommandName::SourceTree,
            SourceCommand::Grep(_) => CommandName::SourceGrep,
            SourceCommand::Show { .. } => CommandName::SourceShow,
            SourceCommand::Extract { .. } => CommandName::SourceExtract,
        }
    }
}

/// What the build knows about itself, apart from the bundle's contents.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceBuild {
    /// The Cargo package version.
    pub version: String,
    /// The git commit the binary was built from, when the build knew it.
    pub commit: Option<String>,
    /// Whether the bundled paths had uncommitted changes, when git could say.
    pub dirty: Option<bool>,
    /// The Rust target triple the binary was built for.
    pub target: Option<String>,
    /// Where the bundle came from.
    pub origin: Origin,
}

impl SourceBuild {
    /// This binary's build.
    pub fn current() -> Self {
        SourceBuild {
            version: env!("CARGO_PKG_VERSION").to_string(),
            commit: crate::upgrade::version::build_commit().map(str::to_string),
            dirty: source_bundle::dirty(),
            target: option_env!("RELIABURGER_TARGET").map(str::to_string),
            origin: source_bundle::origin(),
        }
    }
}

/// The source bundle, unpacked in memory, and the build it belongs to.
#[derive(Debug, Clone)]
pub struct Bundle {
    build: SourceBuild,
    /// Lower-case hex SHA-256 of the uncompressed tarball.
    digest: String,
    files: Vec<SourceFile>,
    tar: Vec<u8>,
}

impl Bundle {
    /// This binary's bundle.
    pub fn load() -> Result<Self, SourceBundleError> {
        Self::from_tar(source_bundle::tarball()?, SourceBuild::current())
    }

    /// A bundle from an uncompressed tarball, as `source_bundle` packs one.
    pub fn from_tar(tar: Vec<u8>, build: SourceBuild) -> Result<Self, SourceBundleError> {
        let files = source_bundle::files_in(&tar)?;
        Ok(Bundle {
            digest: source_bundle::digest_of(&tar),
            build,
            files,
            tar,
        })
    }

    /// The bundle's digest as the envelope carries it: `sha256:<hex>`.
    pub fn digest(&self) -> String {
        format!("sha256:{}", self.digest)
    }

    /// The `relish` block of every envelope this bundle answers.
    pub fn build_info(&self) -> BuildInfo {
        BuildInfo {
            version: self.build.version.clone(),
            commit: self.build.commit.clone(),
            dirty: self.build.dirty,
            source_digest: Some(self.digest()),
        }
    }

    /// A GitHub link to `path` at the build's commit, with `anchor` (such
    /// as `L12` or `L12-L40`), when the build is known to be clean: a dirty
    /// or unknown tree may not match what GitHub has at that commit.
    pub(super) fn permalink(&self, path: &str, anchor: &str) -> Option<String> {
        let commit = self.build.commit.as_deref()?;
        (self.build.dirty == Some(false))
            .then(|| format!("{REPOSITORY_URL}/blob/{commit}/{path}#{anchor}"))
    }

    fn file(&self, path: &str) -> Option<&SourceFile> {
        self.files.iter().find(|file| file.path == path)
    }

    /// The text of the file at `path`, or `None` when it isn't in the
    /// bundle or isn't text.
    pub(super) fn text(&self, path: &str) -> Option<&str> {
        self.file(path).and_then(text_of)
    }

    /// Every text file in the bundle, as `(path, text)`, in path order.
    pub(super) fn texts(&self) -> impl Iterator<Item = (&str, &str)> {
        self.files
            .iter()
            .filter_map(|file| text_of(file).map(|text| (file.path.as_str(), text)))
    }
}

/// A file's contents as text, or `None` for a binary file.
fn text_of(file: &SourceFile) -> Option<&str> {
    std::str::from_utf8(&file.contents)
        .ok()
        .filter(|text| !text.contains('\0'))
}

/// `path` with a leading `./` and any trailing `/` removed, as the bundle
/// writes paths.
fn normalise_path(path: &str) -> &str {
    path.trim_start_matches("./").trim_end_matches('/')
}

/// What a command answered, before it goes into an envelope.
#[derive(Debug, Clone)]
pub struct Answer<T> {
    /// The command's `data`.
    pub data: T,
    /// Commands worth running next.
    pub next: Vec<NextStep>,
    /// Warnings about this answer.
    pub warnings: Vec<String>,
}

impl<T> Answer<T> {
    fn new(data: T) -> Self {
        Answer {
            data,
            next: Vec::new(),
            warnings: Vec::new(),
        }
    }

    fn with_next(mut self, run: String, why: &str, read_only: bool) -> Self {
        self.next.push(NextStep {
            run,
            why: why.to_string(),
            read_only,
        });
        self
    }
}

/// The `not_found` failure for `path`, with the closest paths as a hint.
fn not_found(bundle: &Bundle, path: &str) -> AgentError {
    let close = close_paths(bundle, path);
    let hint = if close.is_empty() {
        "relish agent source tree lists every path".to_string()
    } else {
        format!(
            "did you mean {}? relish agent source tree lists every path",
            close.join(", ")
        )
    };
    AgentError::new(
        ErrorCode::NotFound,
        format!("{path} isn't in the source bundle"),
    )
    .with_hint(hint)
}

/// How many close matches a `not_found` hint names.
const CLOSE_PATHS: usize = 5;

/// Up to five paths in the bundle that look like `query`: the same file
/// name anywhere first, then fuzzy matches of the whole path, then of the
/// file name alone.
fn close_paths(bundle: &Bundle, query: &str) -> Vec<String> {
    let paths: Vec<String> = bundle.files.iter().map(|file| file.path.clone()).collect();
    let name = query.rsplit('/').next().unwrap_or(query);
    let mut close: Vec<String> = paths
        .iter()
        .filter(|path| path.rsplit('/').next() == Some(name))
        .cloned()
        .collect();
    let stem = name.split('.').next().unwrap_or(name);
    for query in [query, stem] {
        for index in crate::relish::reader::fuzzy_rank(query, &paths) {
            if !close.contains(&paths[index]) {
                close.push(paths[index].clone());
            }
        }
    }
    close.truncate(CLOSE_PATHS);
    close
}

/// Put a command's answer (or its failure) in an envelope and render it.
///
/// `cluster_warnings` come first: they're about every answer, such as a
/// node that runs a different commit.
pub fn render_answer<T: Serialize>(
    command: CommandName,
    build: BuildInfo,
    result: Result<Answer<T>, AgentError>,
    cluster_warnings: &[String],
    max_bytes: usize,
    rerun: &str,
) -> Rendered {
    match result {
        Ok(answer) => {
            let mut envelope = Envelope::answered(command, build, answer.data);
            for warning in cluster_warnings.iter().cloned().chain(answer.warnings) {
                envelope = envelope.with_warning(warning);
            }
            for step in answer.next {
                envelope = envelope.with_next(step);
            }
            budget::render(&envelope, max_bytes, rerun)
        }
        Err(error) => {
            let envelope: Envelope<()> = Envelope::failed(command, build, error);
            budget::render(&envelope, max_bytes, rerun)
        }
    }
}

/// Answer one `relish agent source` command.
///
/// `cluster` is what the nodes said about their commits; every command
/// carries the warnings it raises, and `version` reports it in full.
pub fn answer(
    bundle: &Bundle,
    command: &SourceCommand,
    cluster: &ClusterSource,
    max_bytes: usize,
    rerun: &str,
) -> Rendered {
    let warnings = nodes::warnings(bundle.build.commit.as_deref(), cluster);
    let build = bundle.build_info();
    let name = command.name();
    match command {
        SourceCommand::Version => {
            let result = Ok(version(bundle, cluster));
            render_answer(name, build, result, &warnings, max_bytes, rerun)
        }
        SourceCommand::Tree { prefix, depth } => {
            let result = tree::tree(bundle, prefix.as_deref(), *depth);
            render_answer(name, build, result, &warnings, max_bytes, rerun)
        }
        SourceCommand::Grep(args) => {
            let result = grep::grep(bundle, args);
            render_answer(name, build, result, &warnings, max_bytes, rerun)
        }
        SourceCommand::Show { target } => {
            let result = show::show(bundle, target);
            render_answer(name, build, result, &warnings, max_bytes, rerun)
        }
        SourceCommand::Extract { dir } => {
            let result = extract::extract(bundle, dir);
            render_answer(name, build, result, &warnings, max_bytes, rerun)
        }
    }
}

/// Run one `relish agent source` command against this binary's bundle,
/// asking the cluster about its nodes unless `--offline`.
pub async fn run(options: &SourceOptions, max_bytes: usize, rerun: &str) -> Rendered {
    let command = options.command.name();
    let bundle = match Bundle::load() {
        Ok(bundle) => bundle,
        Err(error) => {
            let error = AgentError::new(ErrorCode::Internal, error.to_string());
            return render_answer::<()>(
                command,
                BuildInfo::current(),
                Err(error),
                &[],
                max_bytes,
                rerun,
            );
        }
    };
    let cluster = if options.offline {
        ClusterSource::Offline
    } else {
        nodes::check(&BunClient::default_local(), bundle.build.commit.as_deref()).await
    };
    answer(&bundle, &options.command, &cluster, max_bytes, rerun)
}

/// `data` for `relish agent source version`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, schemars::JsonSchema)]
pub struct VersionData {
    /// The Cargo package version.
    pub version: String,
    /// The git commit the binary was built from, when the build knew it.
    pub commit: Option<String>,
    /// Whether the bundled paths had uncommitted changes, when git could
    /// say. Permalinks appear only when this is `false`.
    pub dirty: Option<bool>,
    /// `sha256:` digest of the uncompressed source tarball.
    pub digest: String,
    /// The Rust target triple the binary was built for.
    pub target: Option<String>,
    /// How many files the bundle carries.
    pub files: usize,
    /// Where the source comes from.
    pub origin: SourceOrigin,
    /// Which commit each node of the cluster runs, compared with this one.
    pub cluster: ClusterSource,
}

/// Where the source comes from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, schemars::JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SourceOrigin {
    /// Compiled into the binary.
    Embedded,
    /// Read from the checkout a development build was built in. It's the
    /// checkout as it is now, which may have moved on since the build.
    Checkout {
        /// The checkout's directory.
        path: String,
    },
}

fn version(bundle: &Bundle, cluster: &ClusterSource) -> Answer<VersionData> {
    let build = &bundle.build;
    let origin = match &build.origin {
        Origin::Embedded => SourceOrigin::Embedded,
        Origin::Checkout(path) => SourceOrigin::Checkout {
            path: path.display().to_string(),
        },
    };
    Answer::new(VersionData {
        version: build.version.clone(),
        commit: build.commit.clone(),
        dirty: build.dirty,
        digest: bundle.digest(),
        target: build.target.clone(),
        files: bundle.files.len(),
        origin,
        cluster: cluster.clone(),
    })
    .with_next(
        "relish agent source tree --depth 1".to_string(),
        "what the bundle holds, one level deep",
        true,
    )
    .with_next(
        "relish agent source grep 'fn main' --path 'src/bin/*'".to_string(),
        "where each binary starts",
        true,
    )
}

/// Quote `word` for a `next` step's command line.
fn quote(word: &str) -> String {
    super::shell_quote(word)
}

#[cfg(test)]
pub(crate) mod fixture {
    //! A small bundle packed from a temporary tree, so the snapshots don't
    //! change every time the real source does.

    use super::*;
    use crate::relish::source_bundle::pack;

    /// The commit the fixture build claims.
    pub const COMMIT: &str = "a1b2c3d4e5f60718293a4b5c6d7e8f9012345678";

    pub const CLIENT_RS: &str = "\
//! The client.

use std::time::Duration;

pub struct Client {
    timeout: Duration,
}

impl Client {
    pub fn new() -> Self {
        Client {
            timeout: Duration::from_secs(10),
        }
    }

    pub async fn fetch(
        &self,
        path: &str,
    ) -> Result<String, Error> {
        let url = format!(\"http://localhost{path}\");
        let body = get(&url, self.timeout).await?;
        Ok(body)
    }
}

fn classify_error(error: Error) -> Kind {
    if error.is_timeout() {
        Kind::Timeout
    } else {
        Kind::Unreachable
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_timeout_is_a_timeout() {
        assert_eq!(classify_error(Error::timeout()), Kind::Timeout);
    }
}
";

    fn write(root: &std::path::Path, path: &str, contents: &[u8]) {
        let full = root.join(path);
        std::fs::create_dir_all(full.parent().unwrap()).unwrap();
        std::fs::write(full, contents).unwrap();
    }

    /// The fixture's uncompressed tarball.
    pub fn tar() -> Vec<u8> {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        write(root, "Cargo.toml", b"[package]\nname = \"sample\"\n");
        write(root, "src/lib.rs", b"pub mod client;\n");
        write(root, "src/client.rs", CLIENT_RS.as_bytes());
        write(
            root,
            "src/bin/tool.rs",
            b"fn main() {\n    println!(\"hi\");\n}\n",
        );
        write(
            root,
            "docs/book/01-hello.md",
            b"# Hello\n\nThe client times out.\n",
        );
        write(root, "brioche/dist/logo.png", b"\x89PNG\r\n\x1a\n\0\0\0");
        let files = pack::list_files(root, &["Cargo.toml", "src", "docs", "brioche"]).unwrap();
        pack::write_tar(root, &files).unwrap()
    }

    /// A build of the fixture with this dirty flag.
    pub fn build(dirty: Option<bool>) -> SourceBuild {
        SourceBuild {
            version: "0.2.7".to_string(),
            commit: Some(COMMIT.to_string()),
            dirty,
            target: Some("aarch64-apple-darwin".to_string()),
            origin: Origin::Embedded,
        }
    }

    /// The fixture bundle, built clean.
    pub fn bundle() -> Bundle {
        Bundle::from_tar(tar(), build(Some(false))).unwrap()
    }

    /// Answer `command` over `bundle` and parse the envelope.
    pub fn answer_json(
        bundle: &Bundle,
        command: &SourceCommand,
        cluster: &ClusterSource,
        max_bytes: usize,
    ) -> (serde_json::Value, u8) {
        let rendered = answer(bundle, command, cluster, max_bytes, "relish agent source");
        assert!(rendered.json.len() <= max_bytes, "over budget");
        (
            serde_json::from_str(&rendered.json).unwrap(),
            rendered.exit.code(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::fixture::*;
    use super::*;
    use crate::relish::agent::DEFAULT_MAX_BYTES;

    #[test]
    fn version_reports_the_bundle_and_the_cluster() {
        let cluster = ClusterSource::Checked {
            nodes: vec![
                nodes::node_source("node-1", Some(COMMIT), Ok(("v0.2.7", Some(COMMIT)))),
                nodes::node_source("node-2", Some(COMMIT), Ok(("v0.2.6", Some("3fcb1fd")))),
            ],
        };
        let (value, exit) = answer_json(
            &bundle(),
            &SourceCommand::Version,
            &cluster,
            DEFAULT_MAX_BYTES,
        );
        assert_eq!(exit, 0);
        insta::assert_json_snapshot!(value);
    }

    #[test]
    fn version_with_no_cluster_says_so_in_data_and_still_answers() {
        let cluster = ClusterSource::Unreachable {
            reason: "bun agent not reachable at localhost:9117 (is it running?)".to_string(),
        };
        let (value, exit) = answer_json(
            &bundle(),
            &SourceCommand::Version,
            &cluster,
            DEFAULT_MAX_BYTES,
        );
        assert_eq!(exit, 0);
        assert_eq!(value["ok"], true);
        assert_eq!(value["data"]["cluster"]["state"], "unreachable");
        assert_eq!(value["warnings"], serde_json::json!([]));
        assert_eq!(value["data"]["files"], 6);
    }

    #[test]
    fn every_answer_carries_the_bundle_digest_and_dirty_flag() {
        let bundle = bundle();
        let (value, _) = answer_json(
            &bundle,
            &SourceCommand::Tree {
                prefix: None,
                depth: None,
            },
            &ClusterSource::Offline,
            DEFAULT_MAX_BYTES,
        );
        assert_eq!(value["relish"]["dirty"], false);
        assert_eq!(
            value["relish"]["source_digest"],
            format!("sha256:{}", source_bundle::digest_of(&tar()))
        );
    }

    #[test]
    fn a_mismatched_node_warns_on_every_source_command() {
        let cluster = ClusterSource::Checked {
            nodes: vec![nodes::node_source(
                "node-2",
                Some(COMMIT),
                Ok(("v0.2.6", Some("3fcb1fd"))),
            )],
        };
        let commands = [
            SourceCommand::Version,
            SourceCommand::Tree {
                prefix: None,
                depth: None,
            },
            SourceCommand::Grep(GrepArgs::new("fn ")),
            SourceCommand::Show {
                target: "src/lib.rs".to_string(),
            },
        ];
        for command in commands {
            let (value, _) = answer_json(&bundle(), &command, &cluster, DEFAULT_MAX_BYTES);
            let warnings = value["warnings"].to_string();
            assert!(
                warnings.contains("source_mismatch") && warnings.contains("node-2"),
                "{command:?}: {warnings}"
            );
        }
    }

    #[test]
    fn a_missing_path_suggests_close_ones() {
        let close = close_paths(&bundle(), "src/relish/client.rs");
        assert_eq!(close.first().map(String::as_str), Some("src/client.rs"));
        assert!(close.len() <= CLOSE_PATHS);
    }

    #[test]
    fn a_failure_still_names_the_bundle() {
        let (value, exit) = answer_json(
            &bundle(),
            &SourceCommand::Show {
                target: "src/nope.rs".to_string(),
            },
            &ClusterSource::Offline,
            DEFAULT_MAX_BYTES,
        );
        assert_eq!(exit, 5);
        assert!(value["relish"]["source_digest"].is_string());
    }

    #[test]
    fn permalinks_need_a_clean_build_with_a_commit() {
        let clean = bundle();
        assert_eq!(
            clean.permalink("src/lib.rs", "L1").as_deref(),
            Some(
                "https://github.com/reliaburger/reliaburger/blob/a1b2c3d4e5f60718293a4b5c6d7e8f9012345678/src/lib.rs#L1"
            )
        );
        for dirty in [Some(true), None] {
            let bundle = Bundle::from_tar(tar(), build(dirty)).unwrap();
            assert_eq!(bundle.permalink("src/lib.rs", "L1"), None, "{dirty:?}");
        }
        let mut no_commit = build(Some(false));
        no_commit.commit = None;
        let bundle = Bundle::from_tar(tar(), no_commit).unwrap();
        assert_eq!(bundle.permalink("src/lib.rs", "L1"), None);
    }
}
