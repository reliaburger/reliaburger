/// Command executors for the Relish CLI.
///
/// Each subcommand is an async function that returns `Result<(), RelishError>`.
/// Commands try to reach the live Bun agent first. If the agent is
/// unreachable, `apply` falls back to a dry-run plan.
use std::fs;
use std::path::{Path, PathBuf};

use crate::config::Config;

use super::RelishError;
use super::client::BunClient;
use super::output::{OutputFormat, format_output, print_output};
use super::plan::generate_plan;

use crate::bun::agent::CouncilStatus;

/// Parse, validate, and deploy a config file.
///
/// If a Bun agent is running, sends the config for deployment.
/// Progress events are streamed to stderr in real time.
/// With `--dry-run`, prints the plan and exits 0 without deploying.
/// Without `--dry-run`, an unreachable agent is an error: the plan is
/// still printed for reference, but the exit code is non-zero so
/// scripts and CI cannot mistake "nothing happened" for a deploy.
///
/// The manifest is Reliaburger TOML or Kubernetes YAML, from a file or an
/// `https://` URL. Kubernetes YAML is imported in memory and its migration
/// report printed to stderr before anything is applied.
pub async fn apply(
    source: &super::manifest::ManifestSource,
    output: OutputFormat,
    dry_run: bool,
    break_lease: bool,
) -> Result<(), RelishError> {
    let client = BunClient::default_local().breaking_leases(break_lease);
    apply_with_client(source, output, dry_run, &client).await
}

/// Read a local config file for `lint`, `deploy`, `diff` and `compile`:
/// Reliaburger TOML, or Kubernetes YAML or JSON imported the way `apply`
/// imports it, with the migration report on stderr.
pub(crate) fn load_config_file(path: &Path) -> Result<Config, RelishError> {
    let loaded = super::manifest::load_file(path)?;
    if let Some(report) = &loaded.migration_report {
        eprintln!("{report}");
    }
    Ok(loaded.config)
}

/// Read a manifest for `apply`, printing any migration report to stderr.
async fn load_manifest(source: &super::manifest::ManifestSource) -> Result<Config, RelishError> {
    let loaded = super::manifest::load(source).await?;
    if let Some(report) = &loaded.migration_report {
        eprint!("{report}");
        eprintln!();
    }
    loaded.config.validate_intrinsic()?;
    for warning in loaded.config.warnings() {
        eprintln!("warning: {warning}");
    }
    Ok(loaded.config)
}

/// Request another common run; unresolved ownership still requires grant-specific replay.
/// `-o json` prints the node's answer, `created` and `instances`.
pub async fn rerun_jobs(
    source: &super::manifest::ManifestSource,
    output: OutputFormat,
) -> Result<(), RelishError> {
    let config = load_manifest(source).await?;
    let result = BunClient::default_local()
        .apply_rerunning_jobs(&config)
        .await?;
    if output != OutputFormat::Human {
        let answer = serde_json::to_value(&result).map_err(RelishError::SerialiseJson)?;
        println!("{}", format_output(&answer, output)?);
        return Ok(());
    }
    println!(
        "started {} job instance(s): {}",
        result.created,
        result.instances.join(", ")
    );
    Ok(())
}

/// The last line of `relish apply`. A single node starts the instances
/// before it answers and names them; a cluster commits the apps and lets
/// the scheduler place them, so there are no instances to name yet.
fn apply_summary(created: usize, instances: &[String]) -> String {
    if instances.is_empty() {
        format!(
            "applied {created} app(s); the scheduler places them now (watch with `relish apps`)"
        )
    } else {
        format!("deployed {created} instance(s): {}", instances.join(", "))
    }
}

/// What the policies say about `config`'s apps, for `relish apply
/// --dry-run`: the cluster's policies with the config's own replacing any
/// of the same name, judged as a deployer applying by hand (#1117).
pub(crate) fn plan_policies(
    committed: &std::collections::BTreeMap<String, crate::config::PolicySpec>,
    config: &Config,
) -> super::plan::PolicyPlan {
    let effective = crate::policy::effective_policies(committed, config);
    match crate::policy::PolicySet::compile(&effective) {
        Ok(set) => super::plan::PolicyPlan::from_evaluation(&crate::policy::evaluate(
            &set,
            config,
            &crate::policy::Caller::token("relish", crate::sesame::types::ApiRole::Deployer),
            crate::policy::Source::Apply,
        )),
        Err(error) => super::plan::PolicyPlan {
            denials: vec![error.to_string()],
            ..Default::default()
        },
    }
}

async fn apply_with_client(
    source: &super::manifest::ManifestSource,
    output: OutputFormat,
    dry_run: bool,
    client: &BunClient,
) -> Result<(), RelishError> {
    let config = load_manifest(source).await?;

    if dry_run {
        // Diff against the live agent's current state when one answers, so
        // updates and unchanged resources render as such instead of every
        // resource claiming to be a create. No agent → the all-create plan.
        let (current, committed_policies) = match client.health().await {
            Ok(()) => (
                Some(client.current_resources().await?),
                // A standalone node has no council and so no policies.
                client
                    .policies()
                    .await
                    .map(|listing| listing.policies)
                    .unwrap_or_default(),
            ),
            Err(_) => (None, Default::default()),
        };
        let mut plan = generate_plan(&config, current.as_deref());
        plan.policy = plan_policies(&committed_policies, &config);
        let formatted = format_output(&plan, output)?;
        println!("{formatted}");
        // Human-only trailer: appending it to --output json|yaml would
        // corrupt the document (deploy already guarded this; apply didn't).
        if matches!(output, OutputFormat::Human) {
            println!("\n(dry run — nothing deployed)");
        }
        return Ok(());
    }

    match client.health().await {
        Ok(()) => {
            // Agent is alive — send the config (progress streams to stderr)
            let result = client.apply(&config).await?;
            println!("{}", apply_summary(result.created, &result.instances));
            Ok(())
        }
        Err(error) => {
            // X5: show the plan for reference, but fail — a dead agent
            // must not make a deploy look successful.
            let plan = generate_plan(&config, None);
            let formatted = format_output(&plan, output)?;
            println!("{formatted}");
            eprintln!("\nnothing was deployed (use --dry-run to preview a plan without an agent)");
            Err(error)
        }
    }
}

/// The `relish logs` flags, as typed.
#[derive(Debug, Clone, Default)]
pub struct LogFlags {
    pub tail: Option<usize>,
    pub follow: bool,
    /// A regular expression.
    pub grep: Option<String>,
    pub since: Option<String>,
    pub until: Option<String>,
    pub instance: Option<String>,
    /// `stdout` or `stderr`.
    pub stream: Option<String>,
    pub json_field: Option<String>,
}

/// Stream logs from an app or job.
///
/// With `-o json`, each line is one JSON object on its own line (`at`,
/// `node`, `instance`, `stream`, `text`); with `-o yaml`, one list of them.
pub async fn logs(
    name: &str,
    namespace: &str,
    flags: LogFlags,
    output: OutputFormat,
) -> Result<(), RelishError> {
    if flags.follow && output != OutputFormat::Human {
        return Err(RelishError::InvalidFlag {
            flag: "follow".to_string(),
            reason: "can't be combined with --output json or yaml yet: a followed stream \
                     doesn't say when, or on which stream, each line was written"
                .to_string(),
        });
    }
    let options = build_log_options(flags, unix_now())?;
    logs_with_client(
        name,
        namespace,
        &options,
        output,
        &BunClient::default_local(),
    )
    .await
}

/// One log line as `relish logs -o json` prints it. A field the source
/// didn't record is `null`: the agent's own capture, the fallback when the
/// log store has nothing, records only the text.
#[derive(Debug, serde::Serialize)]
pub struct LogRecord<'a> {
    /// When the line was stored, RFC 3339 in UTC, to the second.
    pub at: Option<String>,
    /// The node that stored the line.
    pub node: Option<&'a str>,
    /// The instance that wrote it.
    pub instance: Option<&'a str>,
    /// `stdout` or `stderr`.
    pub stream: Option<&'static str>,
    /// The line itself, without its newline.
    pub text: &'a str,
}

/// The records behind `lines`, oldest first.
pub fn log_records(lines: &super::client::LogLines) -> Vec<LogRecord<'_>> {
    match lines {
        super::client::LogLines::Stored(entries) => entries
            .iter()
            .map(|entry| LogRecord {
                at: rfc3339(entry.timestamp),
                node: entry.node.as_deref(),
                instance: entry.instance.as_deref(),
                stream: Some(entry.stream.as_str()),
                text: &entry.line,
            })
            .collect(),
        super::client::LogLines::Captured(lines) => lines
            .iter()
            .map(|line| LogRecord {
                at: None,
                node: None,
                instance: None,
                stream: None,
                text: line,
            })
            .collect(),
    }
}

/// `seconds` since the Unix epoch as RFC 3339 in UTC, if it's a real time.
pub(crate) fn rfc3339(seconds: u64) -> Option<String> {
    let seconds = i64::try_from(seconds).ok()?;
    time::OffsetDateTime::from_unix_timestamp(seconds)
        .ok()?
        .format(&time::format_description::well_known::Rfc3339)
        .ok()
}

/// `lines` as `relish logs` prints them in `output`: the plain lines for
/// people, JSON Lines for `json`, one YAML list for `yaml`. Empty when there
/// are no lines.
pub fn render_logs(
    lines: &super::client::LogLines,
    output: OutputFormat,
) -> Result<String, RelishError> {
    match output {
        OutputFormat::Human => Ok(lines.render()),
        OutputFormat::Json => {
            let mut out = Vec::new();
            for record in log_records(lines) {
                out.push(serde_json::to_string(&record).map_err(RelishError::SerialiseJson)?);
            }
            Ok(out.join("\n"))
        }
        OutputFormat::Yaml => {
            let records = log_records(lines);
            if records.is_empty() {
                return Ok(String::new());
            }
            Ok(yaml_serde::to_string(&records)
                .map_err(RelishError::SerialiseYaml)?
                .trim_end()
                .to_string())
        }
    }
}

/// Translate the CLI flags into [`LogOptions`], validating as we go.
fn build_log_options(
    flags: LogFlags,
    now_epoch: u64,
) -> Result<super::client::LogOptions, RelishError> {
    let start = match &flags.since {
        Some(s) => Some(parse_since(s, now_epoch)?),
        None => None,
    };
    let end = match &flags.until {
        Some(s) => Some(parse_time("until", s, now_epoch)?),
        None => None,
    };
    if end.is_some() && flags.follow {
        return Err(RelishError::InvalidFlag {
            flag: "until".to_string(),
            reason: "a followed stream has no end; drop --until or --follow".to_string(),
        });
    }
    if let (Some(start), Some(end)) = (start, end)
        && end < start
    {
        return Err(RelishError::InvalidFlag {
            flag: "until".to_string(),
            reason: "the window ends before it starts; --until must be later than --since"
                .to_string(),
        });
    }
    let stream = match flags.stream.as_deref() {
        None => None,
        Some(name) => Some(
            crate::ketchup::types::LogStream::parse(name).ok_or_else(|| {
                RelishError::InvalidFlag {
                    flag: "stream".to_string(),
                    reason: format!("{name:?} — use stdout or stderr"),
                }
            })?,
        ),
    };
    if stream.is_some() && flags.follow {
        return Err(RelishError::InvalidFlag {
            flag: "stream".to_string(),
            reason: "can't be combined with --follow yet: a followed tail doesn't say which \
                     stream each line came from"
                .to_string(),
        });
    }
    if let Some(grep) = &flags.grep {
        crate::ketchup::log_store::validate_grep(grep).map_err(|error| {
            RelishError::InvalidFlag {
                flag: "grep".to_string(),
                reason: error.to_string(),
            }
        })?;
    }
    let json_field = match &flags.json_field {
        Some(s) => Some(parse_json_field(s)?),
        None => None,
    };
    Ok(super::client::LogOptions {
        tail: flags.tail,
        follow: flags.follow,
        grep: flags.grep,
        start,
        end,
        instance: flags.instance,
        stream,
        json_field,
    })
}

/// Parse a `--since` value: raw epoch seconds, or a duration like
/// `30s`, `5m`, `2h`, `1d` subtracted from now.
fn parse_since(value: &str, now_epoch: u64) -> Result<u64, RelishError> {
    parse_time("since", value, now_epoch)
}

/// Parse a point in time for `--flag`: raw epoch seconds, or a duration like
/// `30s`, `5m`, `2h`, `1d` before now.
pub(crate) fn parse_time(flag: &str, value: &str, now_epoch: u64) -> Result<u64, RelishError> {
    if value.chars().all(|c| c.is_ascii_digit()) && !value.is_empty() {
        return value.parse::<u64>().map_err(|e| RelishError::InvalidFlag {
            flag: flag.to_string(),
            reason: e.to_string(),
        });
    }

    let (number, unit) = value
        .char_indices()
        .next_back()
        .map(|(index, _)| value.split_at(index))
        .unwrap_or(("", ""));
    let multiplier = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 3600,
        "d" => 86_400,
        _ => {
            return Err(RelishError::InvalidFlag {
                flag: flag.to_string(),
                reason: format!("{value:?} — use epoch seconds or a duration like 30s, 5m, 2h, 1d"),
            });
        }
    };
    let amount: u64 = number.parse().map_err(|_| RelishError::InvalidFlag {
        flag: flag.to_string(),
        reason: format!("{value:?} — use epoch seconds or a duration like 30s, 5m, 2h, 1d"),
    })?;
    let seconds = amount
        .checked_mul(multiplier)
        .ok_or_else(|| RelishError::InvalidFlag {
            flag: flag.to_string(),
            reason: format!("duration {value:?} exceeds the supported seconds range"),
        })?;
    Ok(now_epoch.saturating_sub(seconds))
}

/// Parse a `--json-field` value of the form `key=value`.
fn parse_json_field(value: &str) -> Result<(String, String), RelishError> {
    match value.split_once('=') {
        Some((key, val)) if !key.is_empty() => Ok((key.to_string(), val.to_string())),
        _ => Err(RelishError::InvalidFlag {
            flag: "json-field".to_string(),
            reason: format!("{value:?} — expected key=value"),
        }),
    }
}

/// Current unix time in seconds.
pub(crate) fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

async fn logs_with_client(
    name: &str,
    namespace: &str,
    options: &super::client::LogOptions,
    output: OutputFormat,
    client: &BunClient,
) -> Result<(), RelishError> {
    let log_output = if output == OutputFormat::Human {
        client.logs(name, namespace, options).await?
    } else {
        render_logs(&client.log_lines(name, namespace, options).await?, output)?
    };
    if !log_output.is_empty() {
        println!("{log_output}");
    }
    Ok(())
}

/// Where `relish logs-export --dest` puts the files.
#[derive(Debug, PartialEq, Eq)]
enum ExportLanding {
    /// A directory on the machine running relish: a bare path or `file://`.
    ThisMachine { directory: PathBuf },
    /// An object store, written with the exporter's own credentials.
    ObjectStore { url: String },
}

impl ExportLanding {
    fn parse(destination: &str) -> Result<Self, RelishError> {
        if !destination.contains("://") {
            return Ok(Self::ThisMachine {
                directory: PathBuf::from(destination),
            });
        }
        let invalid = |reason: String| RelishError::InvalidFlag {
            flag: "dest".into(),
            reason,
        };
        let url = url::Url::parse(destination).map_err(|error| invalid(error.to_string()))?;
        if url.scheme() != "file" {
            return Ok(Self::ObjectStore {
                url: destination.trim_end_matches('/').to_string(),
            });
        }
        let directory = url
            .to_file_path()
            .map_err(|()| invalid("a file:// URL must name a path on this machine".into()))?;
        Ok(Self::ThisMachine { directory })
    }

    /// Where `node_id`'s files land, in words that can't be taken for the
    /// wrong machine.
    fn place_for(&self, node_id: &str) -> String {
        match self {
            Self::ThisMachine { directory } => {
                format!("{} on this machine", directory.join(node_id).display())
            }
            Self::ObjectStore { url } => format!("{url}/{node_id}"),
        }
    }
}

/// What `relish logs-export` copied or shipped.
#[derive(Debug, serde::Serialize)]
pub struct ExportReport {
    /// The node whose logs these are.
    pub node: String,
    /// Files written this time; ones already there are skipped.
    pub files: u64,
    pub bytes: u64,
    /// Where the files landed: a directory on this machine, or a URL.
    pub destination: String,
    #[serde(skip)]
    summary: String,
}

impl ExportReport {
    fn new(landing: &ExportLanding, node_id: &str, files: u64, bytes: u64) -> Self {
        Self {
            node: node_id.to_string(),
            files,
            bytes,
            destination: landing.place_for(node_id),
            summary: render_export_summary(landing, node_id, files, bytes),
        }
    }
}

impl std::fmt::Display for ExportReport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "{}", self.summary)
    }
}

/// The one line `relish logs-export` prints when it succeeds.
fn render_export_summary(landing: &ExportLanding, node_id: &str, files: u64, bytes: u64) -> String {
    let place = landing.place_for(node_id);
    if files == 0 {
        format!("no new files to export from {node_id} to {place}")
    } else {
        format!("exported {files} file(s) ({bytes} bytes) of {node_id}'s logs to {place}")
    }
}

/// Export a node's Parquet log files.
///
/// A directory (a bare path or `file://`) is on the machine running relish:
/// relish copies the files of the node it's talking to over the API. An
/// `s3://` or `gs://` URL is asked of the agent (`POST /v1/logs/export`),
/// which writes with its own credentials. Either way the node's name comes
/// from the agent; `--node-id` only affects a local store.
///
/// With `source`, or when no agent answers the health probe, relish reads a
/// local Parquet store directly, preferring the agent's default
/// `/var/lib/reliaburger/logs/parquet` and then the per-user data dir (the
/// same order bun itself uses); a custom `[storage] logs` path is only
/// reachable through the agent or an explicit `source` directory.
pub async fn logs_export(
    dest: &Path,
    node_id: &str,
    source: Option<&Path>,
    output: OutputFormat,
) -> Result<(), RelishError> {
    let dest_str = dest.to_str().ok_or_else(|| RelishError::InvalidFlag {
        flag: "dest".into(),
        reason: "destination must be valid UTF-8".into(),
    })?;
    let landing = ExportLanding::parse(dest_str)?;
    if let Some(source) = source {
        return logs_export_from(source, dest_str, &landing, node_id, output).await;
    }
    let client = BunClient::default_local();
    if client.health().await.is_ok() {
        match &landing {
            ExportLanding::ThisMachine { directory } => {
                let copied = copy_node_archive(&client, directory).await?;
                print_output(
                    &ExportReport::new(&landing, &copied.node_id, copied.files, copied.bytes),
                    output,
                )?;
            }
            ExportLanding::ObjectStore { .. } => {
                let outcome = client.logs_export(dest_str).await?;
                if !outcome.checkpoint_saved {
                    eprintln!(
                        "warning: the agent could not persist its export checkpoint; \
                         a later export may re-ship these files"
                    );
                }
                print_output(
                    &ExportReport::new(
                        &landing,
                        &outcome.node_id,
                        outcome.files_exported,
                        outcome.bytes_written,
                    ),
                    output,
                )?;
            }
        }
        return Ok(());
    }

    // No agent: read the local store directly, in the agent's own path
    // preference order (bin/bun.rs) — the old code tried them reversed, so a
    // stock install with both dirs present exported the stale one.
    let primary = std::path::PathBuf::from("/var/lib/reliaburger/logs/parquet");
    let fallback = dirs::data_local_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("/tmp/reliaburger"))
        .join("reliaburger")
        .join("logs")
        .join("parquet");
    let source = if primary.exists() {
        primary
    } else if fallback.exists() {
        fallback
    } else {
        return Err(RelishError::ApiError {
            status: 0,
            body: format!(
                "no agent reachable and no log store found at {} or {}",
                primary.display(),
                fallback.display()
            ),
        });
    };
    logs_export_from(&source, dest_str, &landing, node_id, output).await
}

/// What a copy of a node's archive brought over.
#[derive(Debug, PartialEq, Eq)]
struct CopiedArchive {
    node_id: String,
    files: u64,
    bytes: u64,
}

fn archive_error(body: String) -> RelishError {
    RelishError::ApiError { status: 0, body }
}

/// Copy the node's flushed log files to `directory/<node>/` on this machine.
///
/// Files land under the same `{sha256}-{filename}` names an export gives
/// them, so one already there is complete and the right bytes: that's all
/// the memory a repeat run needs. The node's export checkpoint isn't involved,
/// because a copy on someone's laptop must not let the node prune its logs.
async fn copy_node_archive(
    client: &BunClient,
    directory: &Path,
) -> Result<CopiedArchive, RelishError> {
    use crate::ketchup::export::{archive_object_name, is_archive_file_name};
    use std::path::Component;

    let listing = client.logs_archive().await?;
    // The agent's answer becomes local paths, so a wrong one must not be able
    // to write anywhere but under `directory`.
    let mut components = Path::new(&listing.node_id).components();
    let one_name = matches!(components.next(), Some(Component::Normal(_)));
    if !one_name || components.next().is_some() {
        return Err(archive_error(format!(
            "the agent gave an unusable node name: {:?}",
            listing.node_id
        )));
    }
    let target = directory.join(&listing.node_id);
    tokio::fs::create_dir_all(&target)
        .await
        .map_err(|error| archive_error(format!("cannot create {}: {error}", target.display())))?;

    let mut copied = CopiedArchive {
        node_id: listing.node_id.clone(),
        files: 0,
        bytes: 0,
    };
    for file in &listing.files {
        let is_sha256 =
            file.sha256.len() == 64 && file.sha256.bytes().all(|byte| byte.is_ascii_hexdigit());
        if !is_archive_file_name(&file.name) || !is_sha256 {
            return Err(archive_error(format!(
                "the agent listed an unusable log file: {:?}",
                file.name
            )));
        }
        let path = target.join(archive_object_name(&file.sha256, &file.name));
        let exists = tokio::fs::try_exists(&path).await.map_err(|error| {
            archive_error(format!("cannot inspect {}: {error}", path.display()))
        })?;
        if exists {
            continue;
        }
        // Retention may remove a file between the listing and its fetch.
        let Some(response) = client.logs_archive_file(&file.name).await? else {
            continue;
        };
        receive_archive_file(response, file, &path).await?;
        copied.files += 1;
        copied.bytes += file.size;
    }
    Ok(copied)
}

/// Stream one file to `path`. The name appears only once the bytes are
/// verified and on disk, so a file an interrupted run leaves behind is a
/// hidden `.partial` that neither a repeat run nor `logs-search` mistakes for
/// a finished one.
async fn receive_archive_file(
    response: reqwest::Response,
    file: &crate::ketchup::export::ArchiveFile,
    path: &Path,
) -> Result<(), RelishError> {
    let partial = path.with_file_name(format!(".{}.{}.partial", file.name, std::process::id()));
    let written = write_archive_file(response, file, &partial).await;
    let renamed = match written {
        Ok(()) => tokio::fs::rename(&partial, path)
            .await
            .map_err(|error| archive_error(format!("cannot write {}: {error}", path.display()))),
        Err(error) => Err(error),
    };
    if renamed.is_err() {
        let _ = tokio::fs::remove_file(&partial).await;
    }
    renamed
}

async fn write_archive_file(
    mut response: reqwest::Response,
    file: &crate::ketchup::export::ArchiveFile,
    partial: &Path,
) -> Result<(), RelishError> {
    use sha2::{Digest, Sha256};
    use tokio::io::AsyncWriteExt;

    let write_error = |error: std::io::Error| {
        archive_error(format!("cannot write {}: {error}", partial.display()))
    };
    let changed = || {
        archive_error(format!(
            "{} changed on the node during the copy; run the export again",
            file.name
        ))
    };
    // Logs can hold anything a workload printed, so only their owner reads them.
    let mut out = tokio::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(partial)
        .await
        .map_err(write_error)?;
    let mut hash = Sha256::new();
    let mut size = 0_u64;
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|error| archive_error(format!("fetching {}: {error}", file.name)))?
    {
        size += chunk.len() as u64;
        if size > file.size {
            return Err(changed());
        }
        hash.update(&chunk);
        out.write_all(&chunk).await.map_err(write_error)?;
    }
    if size != file.size || hex::encode(hash.finalize()) != file.sha256 {
        return Err(changed());
    }
    out.flush().await.map_err(write_error)?;
    out.sync_all().await.map_err(write_error)
}

async fn logs_export_from(
    source: &Path,
    dest_str: &str,
    landing: &ExportLanding,
    node_id: &str,
    output: OutputFormat,
) -> Result<(), RelishError> {
    use crate::ketchup::export::{ExportCheckpoint, export_logs};

    // The exporter owns cross-process locking, reload and durable persistence.
    let _entries = tokio::fs::read_dir(source)
        .await
        .map_err(|error| RelishError::ApiError {
            status: 0,
            body: format!(
                "cannot read log export source {}: {error}",
                source.display()
            ),
        })?;
    let mut checkpoint = ExportCheckpoint::default();

    match export_logs(source, dest_str, node_id, &mut checkpoint).await {
        Ok(result) => print_output(
            &ExportReport::new(
                landing,
                node_id,
                result.files_exported as u64,
                result.bytes_written,
            ),
            output,
        ),
        Err(e) => Err(RelishError::ApiError {
            status: 0,
            body: format!("export failed: {e}"),
        }),
    }
}

/// Search exported Parquet log archives with SQL.
///
/// Runs a DataFusion SQL query against Parquet files at the given
/// source path. No running agent needed — reads files directly.
///
/// For people each row is one line of JSON; `-o json` prints the rows as one
/// JSON list.
pub async fn logs_search(source: &str, sql: &str, output: OutputFormat) -> Result<(), RelishError> {
    use crate::ketchup::remote_query::query_remote_json;

    match query_remote_json(source, sql).await {
        Ok(rows) => print_output(&SearchRows(rows), output),
        Err(e) => Err(RelishError::ApiError {
            status: 0,
            body: format!("search failed: {e}"),
        }),
    }
}

/// The rows `relish logs-search` found.
#[derive(Debug, serde::Serialize)]
#[serde(transparent)]
pub struct SearchRows(pub Vec<serde_json::Value>);

impl std::fmt::Display for SearchRows {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.0.is_empty() {
            return writeln!(f, "(no results)");
        }
        for row in &self.0 {
            writeln!(f, "{row}")?;
        }
        Ok(())
    }
}

/// Execute a command inside a running container.
///
/// One-shot: no stdin and no terminal, so an interactive shell exits at
/// once. Relish exits with the command's own exit code (D34).
///
/// `-o json` prints `{app, namespace, command, exit_code, output}`; relish
/// still exits with the command's exit code.
pub async fn exec(
    app: &str,
    command: &[String],
    namespace: &str,
    output: OutputFormat,
) -> Result<super::CommandOutcome, RelishError> {
    exec_with_client(app, namespace, command, output, &BunClient::default_local()).await
}

async fn exec_with_client(
    app: &str,
    namespace: &str,
    command: &[String],
    output: OutputFormat,
    client: &BunClient,
) -> Result<super::CommandOutcome, RelishError> {
    // The entry node only runs its own instances; exec on the node that
    // runs this app (through the entry node's relay on a laptop cluster).
    let target = super::path_cmd::find_source_client(client, app, namespace).await?;
    let answer = target.exec(app, namespace, command).await?;
    print_output(
        &ExecReport {
            app,
            namespace,
            command,
            exit_code: answer.exit_code,
            output: &answer.output,
        },
        output,
    )?;
    Ok(exec_outcome(answer.exit_code))
}

/// What a `relish exec` ran and what it printed.
#[derive(Debug, serde::Serialize)]
struct ExecReport<'a> {
    app: &'a str,
    namespace: &'a str,
    command: &'a [String],
    exit_code: i32,
    /// stdout and stderr, as the instance wrote them.
    output: &'a str,
}

impl std::fmt::Display for ExecReport<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.output)
    }
}

/// The outcome that makes relish exit with a remote command's exit code.
/// A code no process could exit with still fails rather than reading as 0.
fn exec_outcome(exit_code: i32) -> super::CommandOutcome {
    match u8::try_from(exit_code) {
        Ok(0) => super::CommandOutcome::Clean,
        Ok(code) => super::CommandOutcome::Exited(code),
        Err(_) => super::CommandOutcome::Problems,
    }
}

/// Stop all instances of an app.
pub async fn stop(
    app: &str,
    namespace: &str,
    break_lease: bool,
    output: OutputFormat,
) -> Result<(), RelishError> {
    let client = BunClient::default_local().breaking_leases(break_lease);
    stop_with_client(app, namespace, output, &client).await
}

async fn stop_with_client(
    app: &str,
    namespace: &str,
    output: OutputFormat,
    client: &BunClient,
) -> Result<(), RelishError> {
    client.stop(app, namespace).await?;
    print_output(
        &AppChange::new(AppAction::StopRequested, app, namespace),
        output,
    )
}

/// Remove an app from the cluster. A GitOps-managed app needs `force`.
pub async fn delete(
    app: &str,
    namespace: &str,
    force: bool,
    break_lease: bool,
    output: OutputFormat,
) -> Result<(), RelishError> {
    let client = BunClient::default_local().breaking_leases(break_lease);
    delete_with_client(app, namespace, force, output, &client).await
}

async fn delete_with_client(
    app: &str,
    namespace: &str,
    force: bool,
    output: OutputFormat,
    client: &BunClient,
) -> Result<(), RelishError> {
    client.delete_with_force(app, namespace, force).await?;
    let mut change = AppChange::new(AppAction::DeleteRequested, app, namespace);
    change.forced = force;
    print_output(&change, output)
}

/// Hand an app a controller owns back to `relish apply` and GitOps (#1125).
pub async fn release(
    app: &str,
    namespace: &str,
    break_lease: bool,
    output: OutputFormat,
) -> Result<(), RelishError> {
    BunClient::default_local()
        .breaking_leases(break_lease)
        .release(app, namespace)
        .await?;
    print_output(&AppChange::new(AppAction::Released, app, namespace), output)
}

/// What `relish stop`, `delete` or `release` asked of the cluster.
#[derive(Debug, serde::Serialize)]
pub struct AppChange<'a> {
    pub action: AppAction,
    pub app: &'a str,
    pub namespace: &'a str,
    /// `--force` overrode GitOps ownership.
    pub forced: bool,
}

/// The change an [`AppChange`] asked for. Stopping and deleting are
/// requests: the nodes act on them, and `relish apps` shows when they have.
#[derive(Debug, Clone, Copy, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AppAction {
    StopRequested,
    DeleteRequested,
    Released,
}

impl<'a> AppChange<'a> {
    fn new(action: AppAction, app: &'a str, namespace: &'a str) -> Self {
        Self {
            action,
            app,
            namespace,
            forced: false,
        }
    }
}

impl std::fmt::Display for AppChange<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (app, namespace) = (self.app, self.namespace);
        match self.action {
            AppAction::StopRequested => writeln!(
                f,
                "stop requested for {app}; check `relish apps {app}` for completion; `relish apply` starts it again"
            ),
            AppAction::DeleteRequested => writeln!(
                f,
                "delete requested for {app}; check `relish apps {app}` for remaining instances"
            ),
            AppAction::Released => writeln!(
                f,
                "released {app} in {namespace}; relish apply and GitOps manage it again"
            ),
        }
    }
}

/// Show every instance of an app across the cluster, with its node.
pub async fn inspect(name: &str, output: OutputFormat) -> Result<(), RelishError> {
    inspect_with_client(name, output, &BunClient::default_local()).await
}

async fn inspect_with_client(
    name: &str,
    output: OutputFormat,
    client: &BunClient,
) -> Result<(), RelishError> {
    let inspection = super::inspect::collect(client, name).await?;
    print_output(&inspection, output)
}

/// Security mode written by [`init_with_security`] into the generated node
/// configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InitSecurityMode {
    /// Encrypt and mutually authenticate the internal cluster transports.
    MutualTls,
    /// Leave internal transports plaintext for isolated development only.
    DevelopmentPlaintext,
}

/// What `relish init` needs to know about the host it writes a node config
/// for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InitHost {
    /// The host runs Linux, where the eBPF data path exists.
    pub linux: bool,
    /// `relish init` runs as root, as Bun will on a rootful runc node.
    pub root: bool,
    /// `runc` is on `PATH`.
    pub runc: bool,
}

impl InitHost {
    /// Inspect the current host.
    pub fn detect() -> Self {
        let runc = std::env::var_os("PATH").is_some_and(|path| {
            std::env::split_paths(&path).any(|directory| directory.join("runc").is_file())
        });
        Self {
            linux: cfg!(target_os = "linux"),
            root: nix::unistd::geteuid().is_root(),
            runc,
        }
    }

    /// Whether a node here runs rootful runc and so can load the eBPF data
    /// path.
    fn runs_rootful_runc(self) -> bool {
        self.linux && self.root && self.runc
    }
}

/// Turn on the eBPF data path and the DNS responder for a rootful runc
/// node. Namespace isolation is enforced only with eBPF, and Bun refuses to
/// start such a node when eBPF fails to load, so isolation is on by default
/// rather than silently absent. An egress allowlist resolves names through
/// the DNS responder. Elsewhere both stay off, and Bun refuses `allow_from`
/// and egress allowlists instead of running them unenforced.
pub fn enable_node_data_path(config: &mut crate::config::node::NodeConfig, host: InitHost) {
    if host.runs_rootful_runc() {
        config.ebpf.enabled = true;
        config.dns.enabled = true;
    }
}

/// `relish init` for tests and harnesses that need a cluster and never its
/// root: the backup and the identity that opens it go into `dir` itself, as
/// `throwaway-root-backup.age` and `throwaway-root-backup.age-identity`.
/// Never use it for a real cluster, whose root mustn't stay on the node
/// (#709).
#[doc(hidden)]
pub fn init_with_throwaway_root(
    dir: &Path,
    cluster_name: &str,
    node_id: &str,
) -> Result<(), RelishError> {
    fs::create_dir_all(dir)?;
    let identity = age::x25519::Identity::generate();
    let identity_path = dir.join("throwaway-root-backup.age-identity");
    {
        use age::secrecy::ExposeSecret as _;
        crate::sesame::identity::atomic_write_mode(
            &identity_path,
            format!("{}\n", identity.to_string().expose_secret()).as_bytes(),
            Some(0o600),
        )?;
    }
    init_with_security(
        dir,
        cluster_name,
        node_id,
        InitSecurityMode::MutualTls,
        &super::ca_cmd::InitRootBackup {
            path: dir.join("throwaway-root-backup.age"),
            target: super::ca_cmd::BackupTarget::Recipient(identity.to_public().to_string()),
        },
        OutputFormat::Human,
    )
}

/// What `relish init` wrote. The summary for people goes to stderr whatever
/// the format; no secret is in either, only where the secrets are.
#[derive(Debug, serde::Serialize)]
pub struct InitReport {
    pub cluster: String,
    pub node: String,
    /// The node config to start Bun with.
    pub node_config: PathBuf,
    /// The sample app.
    pub app_config: PathBuf,
    /// The master secret's file (mode 0600).
    pub master_key: PathBuf,
    pub security_bootstrap: PathBuf,
    pub identity_dir: PathBuf,
    /// The root CA backup, the only copy of the root key (#709).
    pub root_backup: PathBuf,
    /// What a joiner must see from `relish join`.
    pub root_ca_fingerprint: String,
    /// The internal transports run in the clear (`--development-plaintext`).
    pub development_plaintext: bool,
}

impl std::fmt::Display for InitReport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "created {}", self.node_config.display())?;
        writeln!(f, "created {}", self.app_config.display())
    }
}

/// Initialise a new cluster with starter config files, PKI and an explicit
/// transport-security mode.
///
/// Creates `reliaburger.toml` (node config) and `app.toml` (sample app)
/// in the given directory. Generates the CA hierarchy, age keypair and first
/// node certificate, and writes the root CA's key only into `root_backup`,
/// sealed to the operator, before anything else (#709).
pub fn init_with_security(
    dir: &Path,
    cluster_name: &str,
    node_id: &str,
    security_mode: InitSecurityMode,
    root_backup: &super::ca_cmd::InitRootBackup,
    output: OutputFormat,
) -> Result<(), RelishError> {
    let cluster_identity = crate::config::node::ClusterSection {
        name: cluster_name.to_string(),
        ..crate::config::node::ClusterSection::default()
    };
    cluster_identity
        .validate()
        .map_err(|error| RelishError::InitFailed(error.to_string()))?;
    fs::create_dir_all(dir)?;
    let node_path = dir.join("reliaburger.toml");
    let app_path = dir.join("app.toml");

    if node_path.exists() {
        return Err(RelishError::FileExists {
            path: node_path.display().to_string(),
        });
    }
    if app_path.exists() {
        return Err(RelishError::FileExists {
            path: app_path.display().to_string(),
        });
    }

    if root_backup.path.exists() {
        return Err(RelishError::FileExists {
            path: root_backup.path.display().to_string(),
        });
    }
    // Ask for the passphrase before generating anything.
    let seal = root_backup.seal()?;

    // Generate the security state (CAs, age keypair)
    let init_result = crate::sesame::init::initialize_cluster(cluster_name, node_id)
        .map_err(|e| RelishError::InitFailed(e.to_string()))?;

    // The root key's only copy goes to the operator first: if it can't be
    // written and checked, nothing else is.
    crate::sesame::root_backup::write_backup(
        &init_result.root_backup,
        &crate::sesame::root_backup::BackupDestination::Sealed {
            path: &root_backup.path,
            seal: &seal,
        },
        std::time::SystemTime::now(),
    )?;

    // Persist the master secret to a secure file
    let secret_path = dir.join(format!("{cluster_name}-master.key"));
    let secret_hex = hex::encode(init_result.master_secret);
    crate::sesame::identity::atomic_write_mode(&secret_path, secret_hex.as_bytes(), Some(0o600))?;

    // Write security state to a bootstrap file for bun to load on first startup
    let bootstrap_path = dir.join(format!("{cluster_name}-security-bootstrap.json"));
    let bootstrap_json = serde_json::to_string_pretty(&init_result.security_state)
        .map_err(|e| RelishError::InitFailed(format!("failed to serialise security state: {e}")))?;
    crate::sesame::identity::atomic_write_mode(
        &bootstrap_path,
        bootstrap_json.as_bytes(),
        Some(0o600),
    )?;

    // Persist the first node's identity (certificate, key, CA chain) so bun
    // can serve mTLS. Joiners get theirs through the join ceremony instead.
    let identity_dir = dir.join("identity");
    let node_identity = node_identity_from_init(&init_result)?;
    crate::sesame::identity_store::save(&identity_dir, &node_identity)
        .map_err(|e| RelishError::InitFailed(format!("failed to persist node identity: {e}")))?;
    let root_fingerprint =
        crate::sesame::identity_store::root_ca_fingerprint(&node_identity.root_ca_der);

    // Output the init summary to stderr (join token is sensitive)
    eprint!(
        "{}",
        crate::sesame::init::format_init_output(&init_result, &root_backup.path)
    );
    eprintln!("  Master secret:   {}", secret_path.display());
    eprintln!("  Security state:  {}", bootstrap_path.display());
    eprintln!("  Node identity:   {}", identity_dir.display());
    eprintln!("  Root CA:         {root_fingerprint}");
    eprintln!();
    eprintln!("  Back up {cluster_name}-master.key too, but never with the root CA backup.");
    eprintln!("  Joiners must see the same root CA fingerprint from `relish join`.");

    let mut node_config = crate::config::node::NodeConfig::default();
    node_config.node.name = Some(node_id.to_string());
    node_config.cluster.name = cluster_name.to_string();
    node_config.security.master_key_path = Some(secret_path.clone());
    node_config.security.bootstrap_path = Some(bootstrap_path.clone());
    node_config.security.identity_dir = Some(identity_dir.clone());
    node_config.security.require_mtls = security_mode == InitSecurityMode::MutualTls;
    // Development plaintext is a deliberate, warned choice, so it also
    // acknowledges running the cluster transports in the clear on a routable
    // address — bun otherwise refuses to bind them (C7).
    node_config.security.allow_insecure_cluster =
        security_mode == InitSecurityMode::DevelopmentPlaintext;
    enable_node_data_path(&mut node_config, InitHost::detect());
    let security_header = match security_mode {
        InitSecurityMode::MutualTls => "# Internal cluster transports require mTLS.\n",
        InitSecurityMode::DevelopmentPlaintext => {
            "# WARNING: DEVELOPMENT-ONLY PLAINTEXT CLUSTER TRANSPORTS.\n\
             # Generated by `relish init --development-plaintext`. Do not use on a shared network.\n"
        }
    };
    let node_toml = format!(
        "# Reliaburger node configuration.\n\
         # See docs/README.md for full reference.\n\
         {security_header}\n{}",
        toml::to_string_pretty(&node_config).map_err(|error| RelishError::InitFailed(format!(
            "default node config doesn't serialise: {error}"
        )))?
    );

    let app_toml = r#"# Sample Reliaburger app configuration.
# Deploy with: relish apply app.toml

[app.web]
image = "busybox:1.36"
command = ["/bin/sh", "-c", "mkdir -p /tmp/reliaburger-www && printf 'Reliaburger is running\\n' > /tmp/reliaburger-www/index.html && exec /bin/httpd -f -p 8080 -h /tmp/reliaburger-www"]
port = 8080

[app.web.health]
path = "/"
"#;

    fs::write(&node_path, node_toml)?;
    fs::write(&app_path, app_toml)?;

    if security_mode == InitSecurityMode::DevelopmentPlaintext {
        eprintln!();
        eprintln!("WARNING: generated DEVELOPMENT-ONLY plaintext cluster transports.");
        eprintln!("Do not use this configuration on a shared or production network.");
    }

    print_output(
        &InitReport {
            cluster: cluster_name.to_string(),
            node: node_id.to_string(),
            node_config: node_path,
            app_config: app_path,
            master_key: secret_path,
            security_bootstrap: bootstrap_path,
            identity_dir,
            root_backup: root_backup.path.clone(),
            root_ca_fingerprint: root_fingerprint,
            development_plaintext: security_mode == InitSecurityMode::DevelopmentPlaintext,
        },
        output,
    )
}

/// Assemble the first node's on-disk identity from the init result.
pub(super) fn node_identity_from_init(
    init_result: &crate::sesame::init::InitResult,
) -> Result<crate::sesame::identity_store::NodeIdentity, RelishError> {
    use crate::sesame::types::CaRole;

    let state = &init_result.security_state;
    let node_ca = state.active_ca(CaRole::Node).ok_or_else(|| {
        RelishError::InitFailed("security state is missing the Node CA".to_string())
    })?;
    let root_ca = state.active_ca(CaRole::Root).ok_or_else(|| {
        RelishError::InitFailed("security state is missing the root CA".to_string())
    })?;

    let cert = &init_result.node_certificate;
    Ok(crate::sesame::identity_store::NodeIdentity {
        node_id: cert.node_id.clone(),
        certificate_der: cert.certificate_der.clone(),
        private_key_der: cert.private_key_der.clone(),
        serial: cert.serial,
        ca_generation: cert.ca_generation,
        trust: crate::sesame::trust::TrustSet::single(
            node_ca.certificate_der.clone(),
            root_ca.certificate_der.clone(),
        ),
        node_ca_der: node_ca.certificate_der.clone(),
        root_ca_der: root_ca.certificate_der.clone(),
        not_before: cert.not_before,
        not_after: cert.not_after,
    })
}

/// Retire a node identity after an operator has stopped or fenced its workloads.
pub async fn decommission_node(
    node_id: &str,
    workloads_stopped: bool,
    reason: &str,
    output: OutputFormat,
) -> Result<(), RelishError> {
    let request = crate::cluster::retirement::DecommissionRequest {
        node_id: node_id.into(),
        workloads_stopped,
        reason: reason.into(),
    };
    let retirement = BunClient::default_local()
        .decommission_node(&request)
        .await?;
    match output {
        OutputFormat::Human => {
            let released: u128 = retirement
                .released_placements
                .values()
                .map(|count| u128::from(*count))
                .sum();
            let registry_released: u128 = retirement
                .released_registry_writers
                .values()
                .map(|count| u128::from(*count))
                .sum();
            println!(
                "retired node {} (operator: {}); resolved {released} placement and {registry_released} registry obligations",
                retirement.node_id, retirement.retired_by
            );
            println!("return requires fresh state and enrolment under a new node identity");
        }
        OutputFormat::Json => println!(
            "{}",
            serde_json::to_string_pretty(&retirement).map_err(RelishError::SerialiseJson)?
        ),
        OutputFormat::Yaml => print!(
            "{}",
            yaml_serde::to_string(&retirement).map_err(RelishError::SerialiseYaml)?
        ),
    }
    Ok(())
}

/// Join an existing cluster: fetch a certificate from a member and persist it.
///
/// Contacts `addr` (an existing member's API address) with the join token,
/// receives the certificate bundle, and writes it into the identity
/// directory. The node then restarts to bring up mTLS with its new identity.
pub async fn join(
    token: &str,
    addr: &str,
    node_id: &str,
    identity_dir: Option<&Path>,
    ca_fingerprint: Option<&str>,
    output: OutputFormat,
) -> Result<(), RelishError> {
    let base = normalise_member_base(addr);
    let dir = identity_dir
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("identity"));
    let identity = crate::sesame::join::enrol(&base, token, node_id, ca_fingerprint)
        .await
        .map_err(|e| match e {
            crate::sesame::join::JoinClientError::FingerprintMismatch { offered, expected } => {
                RelishError::JoinFailed(format!(
                    "root CA fingerprint mismatch: member offered {offered}, expected {expected} — \
                     refusing to send the join token"
                ))
            }
            other => RelishError::JoinFailed(other.to_string()),
        })?;

    let fingerprint = crate::sesame::identity_store::root_ca_fingerprint(&identity.root_ca_der);
    crate::sesame::identity_store::save(&dir, &identity)
        .map_err(|e| RelishError::JoinFailed(format!("failed to persist identity: {e}")))?;

    print_output(
        &JoinReport {
            node: node_id.to_string(),
            identity_dir: dir,
            root_ca_fingerprint: fingerprint,
        },
        output,
    )
}

/// What `relish join` saved. Restart Bun to use it.
#[derive(Debug, serde::Serialize)]
pub struct JoinReport {
    pub node: String,
    /// Where the node's certificate, key and CA chain now are.
    pub identity_dir: PathBuf,
    pub root_ca_fingerprint: String,
}

impl std::fmt::Display for JoinReport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(
            f,
            "joined as {}: identity written to {}",
            self.node,
            self.identity_dir.display()
        )?;
        writeln!(f, "  cluster root CA: {}", self.root_ca_fingerprint)?;
        writeln!(f, "  restart bun to bring up mTLS with the new identity.")
    }
}

/// Read a bounded, owner-only join credential without exposing it in process arguments.
pub async fn read_join_token(path: &Path) -> Result<String, RelishError> {
    use tokio::io::AsyncReadExt;
    let file = tokio::fs::File::open(path).await?;
    let metadata = file.metadata().await?;
    if !metadata.is_file() || metadata.len() > 4096 {
        return Err(RelishError::JoinFailed(
            "invalid join token file size or type".into(),
        ));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o077 != 0 {
            return Err(RelishError::JoinFailed(
                "join token file must be owner-only (chmod 600)".into(),
            ));
        }
    }
    let mut token = String::new();
    file.take(4097).read_to_string(&mut token).await?;
    if token.len() > 4096
        || token.trim().is_empty()
        || token.trim().chars().any(char::is_whitespace)
    {
        return Err(RelishError::JoinFailed(
            "invalid join token file contents".into(),
        ));
    }
    Ok(token.trim().to_owned())
}

/// Normalise a member address into a base URL. A bare `host:port` assumes
/// `https`; an explicit scheme is left untouched.
fn normalise_member_base(addr: &str) -> String {
    let trimmed = addr.trim_end_matches('/');
    if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        trimmed.to_string()
    } else {
        format!("https://{trimmed}")
    }
}

/// Show council (Raft) composition and status.
pub async fn council(output: OutputFormat) -> Result<(), RelishError> {
    council_with_client(output, &BunClient::default_local()).await
}

async fn council_with_client(output: OutputFormat, client: &BunClient) -> Result<(), RelishError> {
    use crate::relish::council_view;

    let nodes = client.nodes().await?;
    if nodes.is_empty() {
        // Standalone: one node, no relay, and no council to compare.
        let council = client.council().await?;
        return print_standalone_council(output, &council);
    }
    let observations = council_view::survey_nodes(client, &nodes).await;
    let summary = council_view::summarise(&observations);
    let report = council_view::CouncilReport {
        summary: &summary,
        nodes: &observations,
    };
    match output {
        OutputFormat::Human => {
            print!(
                "{}",
                council_view::render_council_status(&observations, &summary)
            );
        }
        OutputFormat::Json => {
            let json = serde_json::to_string_pretty(&report).map_err(RelishError::SerialiseJson)?;
            println!("{json}");
        }
        OutputFormat::Yaml => {
            let yaml = yaml_serde::to_string(&report).map_err(RelishError::SerialiseYaml)?;
            print!("{yaml}");
        }
    }
    Ok(())
}

fn print_standalone_council(
    output: OutputFormat,
    council: &CouncilStatus,
) -> Result<(), RelishError> {
    match output {
        OutputFormat::Human => print_council_human(council),
        OutputFormat::Json => {
            let json = serde_json::to_string_pretty(council).map_err(RelishError::SerialiseJson)?;
            println!("{json}");
        }
        OutputFormat::Yaml => {
            let yaml = yaml_serde::to_string(council).map_err(RelishError::SerialiseYaml)?;
            print!("{yaml}");
        }
    }
    Ok(())
}

/// Recover a cluster whose entire council was lost (12b.2 D21/CP12).
///
/// Offline by design: run it against a STOPPED node. It restores the desired
/// state (from a sealed backup or the node's own snapshot and committed
/// log), retires the dead cluster's Raft log, and stamps a fresh recovery
/// epoch and recovery id. The next start re-bootstraps a single-voter
/// council the reconciler regrows.
///
/// Before it changes anything it asks every voter the restored state names,
/// on `api_port`, whether it still serves a council, and refuses if one does
/// or if the state names no voters (#694). It also refuses a data directory
/// another recovery has already fenced. `force` overrides both.
pub async fn council_recover(
    data_dir: &std::path::Path,
    from: Option<&str>,
    backup: Option<&str>,
    master_key_path: Option<&std::path::Path>,
    api_port: u16,
    force: bool,
    output: OutputFormat,
) -> Result<(), RelishError> {
    use crate::council::recovery::{
        RecoverySource, load_recovery_state, persisted_voters, recover_data_dir,
    };
    use crate::relish::recover_guard::{VoterEndpoint, check_voters_gone, probe_client};

    if force {
        eprintln!(
            "WARNING: --force skips the live-voter and fenced-directory checks. Recovering a \
             cluster that still has a quorum will split the brain. Continue only if every voter \
             is truly gone."
        );
    }

    // A sealed backup always needs the master key. The node's own Raft log
    // needs it when the cluster encrypts the log, so use one given, or the
    // default one if it exists; a keyless cluster has none.
    let default_key = std::path::Path::new("/etc/reliaburger/master.key");
    let key_path = match master_key_path {
        Some(path) => Some(path),
        None if from.is_some() || default_key.exists() => Some(default_key),
        None => None,
    };
    let master_key = key_path
        .map(|path| {
            crate::sesame::bootstrap::load_master_key(path)
                .map_err(|e| RelishError::Recovery(format!("load master key: {e}")))
        })
        .transpose()?;

    let source = match from {
        Some(url) => RecoverySource::BackupUrl {
            url: url.to_string(),
            backup: backup.map(str::to_string),
        },
        None => RecoverySource::NodeDataDir(data_dir.to_path_buf()),
    };

    let state = load_recovery_state(&source, master_key.as_ref())
        .await
        .map_err(|e| RelishError::Recovery(e.to_string()))?;
    let app_count = state.apps.len();
    let token_count = state.security_state.api_tokens.len();
    let prior_epoch = state.recovery_epoch;

    // The live-voter guard: every voter of the council this state came
    // from must be gone, or recovery starts a second council next to it.
    if !force {
        let voters: Vec<VoterEndpoint> = persisted_voters(&state)
            .iter()
            .map(|info| VoterEndpoint::from_info(info, api_port))
            .collect();
        let client = probe_client()
            .map_err(|e| RelishError::Recovery(format!("build the voter probe: {e}")))?;
        check_voters_gone(&client, &voters)
            .await
            .map_err(|refusal| RelishError::Recovery(refusal.to_string()))?;
    }

    let claim = recover_data_dir(data_dir, state, force)
        .map_err(|e| RelishError::Recovery(e.to_string()))?;
    print_output(
        &RecoveryReport {
            restored_apps: app_count,
            api_tokens: token_count,
            previous_epoch: prior_epoch,
            recovery_epoch: claim.epoch,
            recovery_id: claim.id.map(|id| id.to_string()),
            data_dir: data_dir.to_path_buf(),
        },
        output,
    )
}

/// What `relish council recover` restored.
#[derive(Debug, serde::Serialize)]
pub struct RecoveryReport {
    pub restored_apps: usize,
    pub api_tokens: usize,
    pub previous_epoch: u64,
    pub recovery_epoch: u64,
    pub recovery_id: Option<String>,
    pub data_dir: PathBuf,
}

impl std::fmt::Display for RecoveryReport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "Council recovery complete.")?;
        writeln!(f, "  Restored apps:   {}", self.restored_apps)?;
        writeln!(f, "  API tokens:      {}", self.api_tokens)?;
        writeln!(
            f,
            "  Recovery epoch:  {} -> {}",
            self.previous_epoch, self.recovery_epoch
        )?;
        writeln!(
            f,
            "  Recovery id:     {}",
            self.recovery_id.as_deref().unwrap_or("-")
        )?;
        writeln!(f, "  Data directory:  {}", self.data_dir.display())?;
        writeln!(f)?;
        writeln!(
            f,
            "Start this node to re-bootstrap a single-voter council; the reconciler"
        )?;
        writeln!(
            f,
            "will regrow it from surviving members. Writes after the last backup are lost."
        )?;
        writeln!(f)?;
        writeln!(
            f,
            "The voters this council replaces still hold the old one. If they come back where"
        )?;
        writeln!(
            f,
            "they can see this node, they fence themselves; if they can see each other but not"
        )?;
        writeln!(
            f,
            "this node, nothing can tell them. Re-enrol each one before starting it:"
        )?;
        writeln!(
            f,
            "  relish council re-enrol --data-dir <its data directory>"
        )
    }
}

/// One row of `relish council backups`.
#[derive(Debug, Clone, serde::Serialize)]
struct BackupListing {
    name: String,
    recovery_epoch: u64,
    term: u64,
    index: u64,
    millis: u64,
    recovery_id: Option<String>,
}

/// List the council backups at `from`, newest first: the order
/// `council recover` tries them in, and the names `--backup` takes (#695).
/// Lists object names only, so it needs no master key.
pub async fn council_backups(from: &str, output: OutputFormat) -> Result<(), RelishError> {
    let store = crate::council::backup::BackupStore::from_url(from)
        .map_err(|e| RelishError::Recovery(e.to_string()))?;
    let keys = store
        .list()
        .await
        .map_err(|e| RelishError::Recovery(e.to_string()))?;
    let rows: Vec<BackupListing> = keys
        .iter()
        .rev()
        .map(|key| BackupListing {
            name: key.object_name(),
            recovery_epoch: key.recovery_epoch,
            term: key.term,
            index: key.index,
            millis: key.millis,
            recovery_id: key.recovery_id.map(|id| id.to_string()),
        })
        .collect();
    match output {
        OutputFormat::Human => print_backups_human(&rows),
        OutputFormat::Json => {
            let json = serde_json::to_string_pretty(&rows).map_err(RelishError::SerialiseJson)?;
            println!("{json}");
        }
        OutputFormat::Yaml => {
            let yaml = yaml_serde::to_string(&rows).map_err(RelishError::SerialiseYaml)?;
            print!("{yaml}");
        }
    }
    Ok(())
}

fn print_backups_human(rows: &[BackupListing]) {
    if rows.is_empty() {
        println!("no backups");
        return;
    }
    println!(
        "{:<6} {:<8} {:<10} {:<24} NAME",
        "EPOCH", "TERM", "INDEX", "TAKEN (UTC)"
    );
    for row in rows {
        let taken =
            time::OffsetDateTime::from_unix_timestamp_nanos(i128::from(row.millis) * 1_000_000)
                .ok()
                .and_then(|at| {
                    at.format(&time::format_description::well_known::Rfc3339)
                        .ok()
                })
                .unwrap_or_else(|| "-".to_string());
        println!(
            "{:<6} {:<8} {:<10} {:<24} {}",
            row.recovery_epoch, row.term, row.index, taken, row.name
        );
    }
}

/// Re-enrol a voter fenced out by `council recover` (#424): offline, against
/// a stopped node.
pub fn council_reenrol(
    data_dir: &std::path::Path,
    force: bool,
    output: OutputFormat,
) -> Result<(), RelishError> {
    let fenced_by = crate::council::recovery::reenrol_data_dir(data_dir, force)
        .map_err(|e| RelishError::Recovery(e.to_string()))?;
    print_output(
        &ReenrolReport {
            data_dir: data_dir.to_path_buf(),
            replaced_at_epoch: fenced_by,
        },
        output,
    )
}

/// What `relish council re-enrol` removed.
#[derive(Debug, serde::Serialize)]
pub struct ReenrolReport {
    pub data_dir: PathBuf,
    /// The recovery epoch that fenced this node's old council, when one did.
    pub replaced_at_epoch: Option<u64>,
}

impl std::fmt::Display for ReenrolReport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.replaced_at_epoch {
            Some(epoch) => writeln!(
                f,
                "Removed the Raft state of a council replaced at recovery epoch {epoch}."
            )?,
            None => writeln!(f, "Removed this node's Raft state.")?,
        }
        writeln!(f, "  Data directory:  {}", self.data_dir.display())?;
        writeln!(f)?;
        writeln!(
            f,
            "Start the node with `cluster.join` pointing at the current council. It joins as"
        )?;
        writeln!(
            f,
            "a fresh member, adopts the council's recovery epoch, and the reconciler may"
        )?;
        writeln!(f, "promote it to voter.")
    }
}

fn print_council_human(council: &CouncilStatus) {
    let leader = council.leader.as_deref().unwrap_or("(none)");
    println!("Leader: {leader}");
    println!("Term:   {}", council.term);
    println!("Apps:   {}", council.app_count);
    if let Some(idx) = council.last_applied_log {
        println!("Log:    {idx}");
    }
    println!();

    if council.members.is_empty() {
        println!("no council nodes (single-node mode)");
    } else {
        println!("{:<10} {:<20} {:<22}", "RAFT_ID", "NAME", "ADDRESS");
        for m in &council.members {
            println!("{:<10} {:<20} {:<22}", m.raft_id, m.name, m.address);
        }
    }
}

/// Resolve a service name to its VIP and backends.
pub async fn resolve(
    name: &str,
    namespace: Option<&str>,
    output: OutputFormat,
) -> Result<(), RelishError> {
    let services = BunClient::default_local().resolve_all().await?;
    let service = select_service(services, name, namespace)?;
    println!("{}", format_output(&ResolvedService(service), output)?);
    Ok(())
}

/// Pick `name` out of every service the caller may see. Without a
/// namespace the name must be unique; a name in several namespaces is
/// refused rather than answered from whichever came first.
fn select_service(
    services: Vec<crate::onion::types::ResolveResponse>,
    name: &str,
    namespace: Option<&str>,
) -> Result<crate::onion::types::ResolveResponse, RelishError> {
    let mut matches: Vec<_> = services
        .into_iter()
        .filter(|service| {
            service.app_name == name && namespace.is_none_or(|wanted| service.namespace == wanted)
        })
        .collect();
    if matches.len() > 1 {
        let mut namespaces: Vec<_> = matches.iter().map(|s| s.namespace.as_str()).collect();
        namespaces.sort_unstable();
        return Err(RelishError::InvalidFlag {
            flag: "namespace".to_string(),
            reason: format!(
                "service {name:?} exists in namespaces {}; pass --namespace",
                namespaces.join(", ")
            ),
        });
    }
    matches.pop().ok_or_else(|| RelishError::ApiError {
        status: 404,
        body: match namespace {
            Some(namespace) => format!("service {name:?} not found in namespace {namespace:?}"),
            None => format!("service {name:?} not found"),
        },
    })
}

/// A resolved service, rendered for people.
#[derive(Debug, serde::Serialize)]
#[serde(transparent)]
struct ResolvedService(crate::onion::types::ResolveResponse);

impl std::fmt::Display for ResolvedService {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let info = &self.0;
        writeln!(f, "Service:   {}", info.app_name)?;
        writeln!(f, "Namespace: {}", info.namespace)?;
        writeln!(f, "VIP:       {}", info.vip)?;
        writeln!(f, "Port:      {}", info.port)?;
        write!(
            f,
            "Backends:  {}/{} healthy",
            info.healthy_backends, info.total_backends
        )?;
        if !info.backends.is_empty() {
            write!(
                f,
                "\n\n  {:<20} {:<18} {:<8} {:<8}",
                "INSTANCE", "NODE", "PORT", "HEALTH"
            )?;
            for b in &info.backends {
                let health = if b.healthy { "healthy" } else { "unhealthy" };
                write!(
                    f,
                    "\n  {:<20} {:<18} {:<8} {:<8}",
                    b.instance_id, b.node_ip, b.host_port, health
                )?;
            }
        }
        Ok(())
    }
}

/// Show ingress routing table.
pub async fn routes(output: OutputFormat) -> Result<(), RelishError> {
    routes_with_client(output, &BunClient::default_local()).await
}

async fn routes_with_client(output: OutputFormat, client: &BunClient) -> Result<(), RelishError> {
    print_output(&RouteTable(client.routes().await?), output)
}

/// The ingress routing table; as JSON, the list of routes.
#[derive(Debug, serde::Serialize)]
#[serde(transparent)]
struct RouteTable(Vec<crate::wrapper::types::RouteInfo>);

impl std::fmt::Display for RouteTable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.0.is_empty() {
            return writeln!(f, "no ingress routes configured");
        }
        writeln!(
            f,
            "{:<30} {:<10} {:<15} {:<12} {:<6}",
            "HOST", "PATH", "APP", "BACKENDS", "WS"
        )?;
        for r in &self.0 {
            let backends = format!("{}/{}", r.healthy_backends, r.total_backends);
            let ws = if r.websocket { "yes" } else { "no" };
            writeln!(
                f,
                "{:<30} {:<10} {:<15} {:<12} {:<6}",
                r.host, r.path, r.app_name, backends, ws
            )?;
        }
        Ok(())
    }
}

/// Trigger a rolling deploy from a config file.
///
/// Parses the config, sends it to the agent for a rolling deploy
/// (if the app already exists, the agent performs a rolling update).
/// With `--dry-run`, prints the plan and exits 0 without deploying;
/// otherwise an unreachable agent is an error (X5).
pub async fn deploy(
    path: &Path,
    output: OutputFormat,
    dry_run: bool,
    break_lease: bool,
) -> Result<(), RelishError> {
    let config = load_config_file(path)?;
    config.validate_intrinsic()?;

    let client = BunClient::default_local().breaking_leases(break_lease);

    if dry_run {
        // Same live diff as `apply --dry-run`: a reachable agent supplies
        // current state so the plan shows updates, not universal creates.
        let current = match client.health().await {
            Ok(()) => Some(client.current_resources().await?),
            Err(_) => None,
        };
        let plan = generate_plan(&config, current.as_deref());
        let formatted = format_output(&plan, output)?;
        println!("{formatted}");
        if matches!(output, OutputFormat::Human) {
            println!("\n(dry run — nothing deployed)");
        }
        return Ok(());
    }

    match client.health().await {
        Ok(()) => {
            let result = client.apply(&config).await?;
            match output {
                OutputFormat::Human => println!(
                    "deploy started: {} instance(s): {}",
                    result.created,
                    result.instances.join(", ")
                ),
                OutputFormat::Json => {
                    let json = serde_json::to_string_pretty(&result)
                        .map_err(RelishError::SerialiseJson)?;
                    println!("{json}");
                }
                OutputFormat::Yaml => {
                    let yaml =
                        yaml_serde::to_string(&result).map_err(RelishError::SerialiseYaml)?;
                    print!("{yaml}");
                }
            }
            Ok(())
        }
        Err(error) => {
            let plan = generate_plan(&config, None);
            let formatted = format_output(&plan, super::OutputFormat::Human)?;
            println!("{formatted}");
            eprintln!("\nnothing was deployed (use --dry-run to preview a plan without an agent)");
            Err(error)
        }
    }
}

/// Request cooperative cancellation and wait up to 30 seconds for terminal evidence.
pub async fn cancel_deploy(operation_id: &str, output: OutputFormat) -> Result<(), RelishError> {
    let client = BunClient::default_local();
    let operation = tokio::time::timeout(std::time::Duration::from_secs(30), async {
        let mut operation = client.cancel_deploy(operation_id).await?;
        while operation.outcome.is_none() {
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            let snapshot = client.deploy_operations().await?;
            operation = snapshot.active_deploys.into_iter().chain(snapshot.history)
                .find(|operation| operation.id.as_str() == operation_id)
                .ok_or_else(|| RelishError::ApiError { status: 404,
                    body: format!("operation {operation_id} is no longer retained; cancellation outcome is unknown") })?;
        }
        Ok::<_, RelishError>(operation)
    }).await.map_err(|_| RelishError::ApiError { status: 202,
        body: format!("cancellation of {operation_id} is still pending; in-flight work retains ownership; query or retry the same ID") })??;
    match output {
        OutputFormat::Human => println!(
            "{}: {:?}: {}",
            operation.id,
            operation
                .outcome
                .unwrap_or(crate::bun::deploy_operations::DeployOperationOutcome::Unknown),
            operation.message
        ),
        OutputFormat::Json => println!(
            "{}",
            serde_json::to_string_pretty(&operation).map_err(RelishError::SerialiseJson)?
        ),
        OutputFormat::Yaml => print!(
            "{}",
            yaml_serde::to_string(&operation).map_err(RelishError::SerialiseYaml)?
        ),
    }
    if matches!(
        operation.outcome,
        Some(
            crate::bun::deploy_operations::DeployOperationOutcome::Unknown
                | crate::bun::deploy_operations::DeployOperationOutcome::Failed
        )
    ) {
        return Err(RelishError::ApiError {
            status: 0,
            body: format!(
                "deployment ended with {:?}: {}",
                operation.outcome, operation.message
            ),
        });
    }
    Ok(())
}

/// Show deploy history for an app in a namespace.
pub async fn history(app: &str, namespace: &str, output: OutputFormat) -> Result<(), RelishError> {
    let client = BunClient::default_local();
    // Authenticated + CA-trusting request (M21): the previous bare
    // `reqwest::get` carried no bearer/CA, so it 401'd against a secured agent
    // and silently exited 0. `deploy_history` routes through the configured
    // client and propagates the failure.
    let view = client.deploy_history(app, namespace).await?;
    // Every node records its own rollout; a node that didn't answer leaves a
    // gap the reader must know about, on stderr so `-o json` stays parseable.
    for warning in &view.warnings {
        eprintln!("warning: history incomplete: {warning}");
    }
    let entries = &view.history;

    match output {
        OutputFormat::Human => {
            if entries.is_empty() {
                println!("no deploy history for {app} in namespace {namespace}");
            } else {
                print!("{}", render_history_table(entries));
            }
        }
        OutputFormat::Json => {
            let json =
                serde_json::to_string_pretty(&entries).map_err(RelishError::SerialiseJson)?;
            println!("{json}");
        }
        OutputFormat::Yaml => {
            let yaml = yaml_serde::to_string(&entries).map_err(RelishError::SerialiseYaml)?;
            print!("{yaml}");
        }
    }

    Ok(())
}

/// `relish history` as a table: one row per node that rolled each deploy out.
fn render_history_table(
    entries: &[crate::bun::cluster_view::NodeTagged<
        crate::meat::deploy_types::DeployHistoryEntry,
    >],
) -> String {
    let mut table = format!(
        "{:<8} {:<16} {:<20} {:<12} {:<6} {:<6}\n",
        "ID", "NODE", "IMAGE", "RESULT", "DONE", "TOTAL"
    );
    for entry in entries {
        let image = if entry.row.image.is_empty() {
            "-"
        } else {
            entry.row.image.as_str()
        };
        table.push_str(&format!(
            "{:<8} {:<16} {:<20} {:<12} {:<6} {:<6}\n",
            entry.row.id.0,
            entry.node,
            image,
            format!("{:?}", entry.row.result),
            entry.row.steps_completed,
            entry.row.steps_total,
        ));
    }
    table
}

/// Rollback an app to the previous version. A GitOps-managed app needs
/// `force`.
pub async fn rollback(
    app: &str,
    namespace: &str,
    force: bool,
    break_lease: bool,
    output: OutputFormat,
) -> Result<(), RelishError> {
    let client = BunClient::default_local().breaking_leases(break_lease);
    client.health().await?;
    // X3: rollback used to only print advice. It now calls the server,
    // which redeploys the previous successful spec.
    client.rollback_with_force(app, namespace, force).await?;
    print_output(
        &Rollback {
            app,
            namespace,
            forced: force,
        },
        output,
    )
}

/// A rollback the cluster accepted: it redeploys the app's previous
/// successful version.
#[derive(Debug, serde::Serialize)]
struct Rollback<'a> {
    app: &'a str,
    namespace: &'a str,
    /// `--force` took the app back from GitOps for this rollback.
    forced: bool,
}

impl std::fmt::Display for Rollback<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "rolled {} back to its previous version", self.app)
    }
}

/// Validate a config file without deploying.
///
/// Warnings go to stderr whatever the format, and also into the JSON and
/// YAML report.
pub fn lint(path: &Path, output: OutputFormat) -> Result<(), RelishError> {
    let report = lint_report(path)?;
    for warning in &report.warnings {
        eprintln!("warning: {warning}");
    }
    print_output(&report, output)
}

/// What `relish lint` found in a config that passed validation.
#[derive(Debug, serde::Serialize)]
pub struct LintReport {
    /// Apps the config declares.
    pub apps: usize,
    /// Jobs the config declares.
    pub jobs: usize,
    /// Things that are allowed but probably not meant.
    pub warnings: Vec<String>,
}

impl std::fmt::Display for LintReport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(
            f,
            "config valid: {} app(s), {} job(s)",
            self.apps, self.jobs
        )
    }
}

/// Validate the config at `path` and collect its warnings. An invalid
/// config is an error, not a report.
fn lint_report(path: &Path) -> Result<LintReport, RelishError> {
    let config = load_config_file(path)?;
    config.validate()?;
    let mut warnings: Vec<String> = config
        .warnings()
        .into_iter()
        .map(|warning| warning.to_string())
        .collect();

    // Validate run_before references
    for (name, job) in &config.job {
        for target in &job.run_before {
            let target_exists = config
                .app
                .keys()
                .any(|app_name| format!("app.{app_name}") == *target);
            if !target_exists {
                warnings.push(format!(
                    "job {name} has run_before target {target:?} which doesn't exist in this config"
                ));
            }
        }
    }

    Ok(LintReport {
        apps: config.app.len(),
        jobs: config.job.len(),
        warnings,
    })
}

/// Compile a config file or directory into a single resolved config.
///
/// For people it prints the merged TOML; `-o json` prints the files it
/// merged, the warnings and the merged config as data.
pub fn compile(path: &Path, output: OutputFormat) -> Result<(), RelishError> {
    let result = super::compile::compile(path)?;

    if !result.warnings.is_empty() {
        for w in &result.warnings {
            eprintln!("warning: {w}");
        }
    }

    let app_count = result.config.app.len();
    let job_count = result.config.job.len();
    let file_count = result.merged_from.len();

    if output == OutputFormat::Human {
        // Serialise the merged config as TOML
        let toml = toml::to_string_pretty(&result.config)
            .map_err(|e| RelishError::FormatFailed(e.to_string()))?;
        print!("{toml}");
        eprintln!("compiled {file_count} file(s): {app_count} app(s), {job_count} job(s)");
        return Ok(());
    }
    print_output(&CompileReport::from(result), output)
}

/// `relish compile -o json`: what was merged, and the result.
#[derive(Debug, serde::Serialize)]
pub struct CompileReport {
    pub files: Vec<PathBuf>,
    pub warnings: Vec<String>,
    pub config: Config,
}

impl From<super::compile::CompileResult> for CompileReport {
    fn from(result: super::compile::CompileResult) -> Self {
        Self {
            files: result.merged_from,
            warnings: result.warnings,
            config: result.config,
        }
    }
}

impl std::fmt::Display for CompileReport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(
            f,
            "compiled {} file(s): {} app(s), {} job(s)",
            self.files.len(),
            self.config.app.len(),
            self.config.job.len()
        )
    }
}

/// Show structural diff between two configs. `-o json` prints `added`,
/// `modified` and `removed`, each empty when nothing changed.
pub fn diff(path_a: &Path, path_b: Option<&Path>, output: OutputFormat) -> Result<(), RelishError> {
    let old = load_config_file(path_a)?;
    let new = match path_b {
        Some(p) => load_config_file(p)?,
        None => Config::default(),
    };
    let diff = super::diff::diff_configs(&old, &new);

    if output != OutputFormat::Human {
        return print_output(&diff, output);
    }
    if diff.is_empty() {
        println!("no changes");
    } else {
        print!("{diff}");
    }
    Ok(())
}

/// What `relish fmt` did to a file.
#[derive(Debug, serde::Serialize)]
pub struct FormatReport<'a> {
    pub path: &'a Path,
    /// `formatted` rewrote it; `ok` (with `--check`) found it formatted.
    pub result: FormatResult,
}

/// The outcome in a [`FormatReport`]. A file `--check` finds unformatted is
/// an error, not a result.
#[derive(Debug, Clone, Copy, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FormatResult {
    Formatted,
    Ok,
}

impl std::fmt::Display for FormatReport<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.result {
            FormatResult::Formatted => writeln!(f, "formatted {}", self.path.display()),
            FormatResult::Ok => writeln!(f, "{}: ok", self.path.display()),
        }
    }
}

/// Format a TOML config file with canonical ordering.
pub fn fmt(path: &Path, check: bool, output: OutputFormat) -> Result<(), RelishError> {
    let content = fs::read_to_string(path)?;

    if check {
        if super::fmt::is_formatted(&content)? {
            print_output(
                &FormatReport {
                    path,
                    result: FormatResult::Ok,
                },
                output,
            )?;
        } else {
            eprintln!("{}: not formatted", path.display());
            return Err(RelishError::FormatFailed(format!(
                "{} needs formatting (run without --check to fix)",
                path.display()
            )));
        }
        return Ok(());
    }

    let formatted = super::fmt::format_toml(&content)?;

    // O10: write atomically. `fs::write` truncates first, so an interrupted
    // or failing write leaves a half-file where a valid config used to be —
    // and this is a config the node reads on startup. A temp file beside the
    // target (same filesystem, so the rename is atomic) means the config is
    // either the old one or the new one, never neither.
    write_atomically(path, formatted.as_bytes())?;
    print_output(
        &FormatReport {
            path,
            result: FormatResult::Formatted,
        },
        output,
    )
}

/// Replace a file's contents atomically: write a sibling temp file, then
/// rename over the target.
///
/// The temp file must live in the same directory, not `/tmp` — `rename` is
/// only atomic within a filesystem, and across one it degrades to a
/// copy-then-delete with the same torn-write window we're trying to close.
fn write_atomically(path: &std::path::Path, bytes: &[u8]) -> Result<(), RelishError> {
    let directory = path.parent().unwrap_or_else(|| std::path::Path::new("."));
    let file_name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "config".to_string());
    let temp = directory.join(format!(".{file_name}.{}.tmp", std::process::id()));

    if let Err(e) = fs::write(&temp, bytes) {
        let _ = fs::remove_file(&temp);
        return Err(e.into());
    }
    if let Err(e) = fs::rename(&temp, path) {
        let _ = fs::remove_file(&temp);
        return Err(e.into());
    }
    Ok(())
}

/// Import Kubernetes YAML manifests to Reliaburger TOML.
#[cfg(feature = "kubernetes")]
///
/// For people the TOML goes to stdout and the migration report to stderr;
/// `-o json` prints both as one object, `config` and `report`.
pub fn import_k8s(
    files: &[std::path::PathBuf],
    strict: bool,
    output: OutputFormat,
) -> Result<(), RelishError> {
    let result = super::k8s_import::import_kubernetes(files)?;

    if output == OutputFormat::Human {
        // Print the converted config as TOML
        let toml = toml::to_string_pretty(&result.config)
            .map_err(|e| RelishError::FormatFailed(e.to_string()))?;
        print!("{toml}");

        // Print migration report to stderr
        if !result.report.converted.is_empty()
            || !result.report.warnings.is_empty()
            || !result.report.dropped.is_empty()
        {
            eprint!("{}", result.report);
        }
    } else {
        print_output(
            &ImportReport {
                config: &result.config,
                report: &result.report,
            },
            output,
        )?;
    }

    // #703: a dropped resource is a bigger loss than an approximated one, so
    // `--strict` (a CI gate) fails on either.
    let (warnings, dropped) = (result.report.warnings.len(), result.report.dropped.len());
    if strict && (warnings > 0 || dropped > 0) {
        return Err(RelishError::FormatFailed(format!(
            "import produced {warnings} warning(s) and {dropped} dropped resource(s) (--strict mode)"
        )));
    }

    Ok(())
}

/// `relish import -o json`: the converted config and what the conversion
/// lost or approximated.
#[cfg(feature = "kubernetes")]
#[derive(Debug, serde::Serialize)]
struct ImportReport<'a> {
    config: &'a Config,
    report: &'a super::k8s_import::MigrationReport,
}

#[cfg(feature = "kubernetes")]
impl std::fmt::Display for ImportReport<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.report)
    }
}

/// `relish export -o json`: the manifests, as the YAML text a cluster
/// applies, and the report.
#[cfg(feature = "kubernetes")]
#[derive(Debug, serde::Serialize)]
struct KubernetesExport<'a> {
    manifests: &'a str,
    report: &'a super::k8s_export::ExportReport,
}

#[cfg(feature = "kubernetes")]
impl std::fmt::Display for KubernetesExport<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.manifests)
    }
}

/// Export Reliaburger TOML to Kubernetes YAML manifests.
#[cfg(feature = "kubernetes")]
pub fn export_k8s(file: &Path, strict: bool, output: OutputFormat) -> Result<(), RelishError> {
    let config = Config::from_file(file)?;
    let result = super::k8s_export::export_kubernetes(&config)?;

    if output == OutputFormat::Human {
        print!("{}", result.yaml);

        if !result.report.resources_created.is_empty()
            || !result.report.unsupported.is_empty()
            || !result.report.dropped.is_empty()
        {
            eprint!("{}", result.report);
        }
    } else {
        print_output(
            &KubernetesExport {
                manifests: &result.yaml,
                report: &result.report,
            },
            output,
        )?;
    }

    // #1132: the same CI gate as `import --strict`, in the other direction.
    let (unsupported, dropped) = (result.report.unsupported.len(), result.report.dropped.len());
    if strict && (unsupported > 0 || dropped > 0) {
        return Err(RelishError::FormatFailed(format!(
            "export produced {unsupported} unsupported and {dropped} not-exported field(s) (--strict mode)"
        )));
    }

    Ok(())
}

/// Show every workload in the cluster with its node, state and latest CPU and
/// memory. The figures are the last samples the node's metrics collector took
/// (every few seconds), not a live meter; `-` means no sample yet.
pub async fn top(output: OutputFormat) -> Result<(), RelishError> {
    let client = BunClient::default_local();
    let top = client.cluster_top().await?;
    for warning in &top.warnings {
        eprintln!("warning: {warning}");
    }

    match output {
        OutputFormat::Human => print!("{}", render_top(&top.rows)),
        OutputFormat::Json => {
            let json =
                serde_json::to_string_pretty(&top.rows).map_err(RelishError::SerialiseJson)?;
            println!("{json}");
        }
        OutputFormat::Yaml => {
            let yaml = yaml_serde::to_string(&top.rows).map_err(RelishError::SerialiseYaml)?;
            print!("{yaml}");
        }
    }

    Ok(())
}

/// An instance's PID for a table: `-` when it has none, `?` when the node's
/// runtime didn't answer in time.
pub(crate) fn pid_cell(status: &crate::bun::agent::InstanceStatus) -> String {
    match status.pid {
        Some(pid) => pid.to_string(),
        None if status.runtime_unknown => "?".to_string(),
        None => "-".to_string(),
    }
}

/// The `relish top` table.
fn render_top(rows: &[crate::bun::top::TopRow]) -> String {
    use std::fmt::Write as _;

    if rows.is_empty() {
        return "no workloads running\n".to_string();
    }
    let mut output = format!(
        "{:<18} {:<20} {:<12} {:<10} {:<8} {:<9} {:>7} {:>10}\n",
        "NODE", "APP", "NAMESPACE", "STATE", "PID", "RESTARTS", "CPU", "MEMORY"
    );
    for row in rows {
        // A job row stands for many tasks, so it shows their count, not a PID.
        let pid = row
            .tasks
            .map_or_else(|| pid_cell(&row.instance), |tasks| format!("tasks:{tasks}"));
        let cpu = row
            .cpu_percent
            .map(|cpu| format!("{cpu:.1}%"))
            .unwrap_or_else(|| "-".to_string());
        let memory = row
            .memory_bytes
            .map(format_memory)
            .unwrap_or_else(|| "-".to_string());
        let _ = writeln!(
            output,
            "{:<18} {:<20} {:<12} {:<10} {:<8} {:<9} {:>7} {:>10}",
            row.node,
            row.instance.app_name,
            row.instance.namespace,
            row.instance.state,
            pid,
            row.instance.restart_count,
            cpu,
            memory
        );
    }
    output
}

/// Bytes in binary units, one decimal place above a KiB.
fn format_memory(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["KiB", "MiB", "GiB", "TiB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut value = bytes as f64 / 1024.0;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

/// Rotate or finalise the cluster's secret encryption key, or one
/// namespace's.
/// `relish gitops status`: the cluster's GitOps configuration (never its
/// credentials), the last sync, what GitOps owns, and whether the answering
/// node's own `[gitops]` differs (#693).
pub async fn gitops_status(output: OutputFormat) -> Result<(), RelishError> {
    let status = BunClient::default_local().gitops_status().await?;
    match output {
        OutputFormat::Human => print!("{}", render_gitops_status(&status)),
        OutputFormat::Json => println!(
            "{}",
            serde_json::to_string_pretty(&status).map_err(RelishError::SerialiseJson)?
        ),
        OutputFormat::Yaml => print!(
            "{}",
            yaml_serde::to_string(&status).map_err(RelishError::SerialiseYaml)?
        ),
    }
    Ok(())
}

/// The human view of `relish gitops status`.
fn render_gitops_status(status: &crate::lettuce::types::GitOpsStatus) -> String {
    use crate::lettuce::types::LocalConfigCheck;
    use std::fmt::Write as _;

    let mut out = String::new();
    let Some(config) = &status.config else {
        out.push_str("GitOps is off for this cluster\n");
        return out;
    };
    let _ = writeln!(out, "repository:   {}", config.repo);
    let _ = writeln!(out, "branch:       {}", config.branch);
    let _ = writeln!(out, "path:         {}", config.path);
    let _ = writeln!(out, "poll:         every {}s", config.poll_interval_secs);
    let _ = writeln!(
        out,
        "credentials:  {}",
        if status.has_credentials {
            "sealed in the cluster"
        } else {
            "none"
        }
    );
    if let Some(sync) = &status.sync {
        let commit = sync
            .last_applied_commit
            .as_ref()
            .map_or("none yet", |commit| commit.sha.as_str());
        let _ = writeln!(out, "applied:      {commit}");
        if let Some(error) = &sync.last_error {
            let _ = writeln!(out, "last error:   {error}");
        }
        for warning in &sync.warnings {
            let _ = writeln!(out, "warning:      {warning}");
        }
    }
    let _ = writeln!(
        out,
        "owns:         {} app(s), {} namespace(s), {} permission(s)",
        status.owned.apps.len(),
        status.owned.namespaces.len(),
        status.owned.permissions.len()
    );
    let local = match &status.local {
        LocalConfigCheck::Differs { fields } => format!(
            "differs from the cluster ({}); the cluster's is used",
            fields.join(", ")
        ),
        LocalConfigCheck::Matches => "matches the cluster's".into(),
        LocalConfigCheck::NoLocalConfig => "none; follows the cluster's".into(),
        LocalConfigCheck::Unreadable { reason } => format!("can't read the cluster's: {reason}"),
        LocalConfigCheck::NoClusterConfig | LocalConfigCheck::Unknown => "not checked yet".into(),
    };
    let _ = writeln!(out, "this node:    [gitops] {local}");
    out
}

/// Read a `[gitops]` section from a TOML file: a node's `node.toml`, or a
/// file holding just the section's keys.
fn read_gitops_section(path: &Path) -> Result<crate::lettuce::types::GitOpsConfig, RelishError> {
    let text = fs::read_to_string(path)?;
    let mut value: toml::Table =
        toml::from_str(&text).map_err(crate::config::ConfigError::TomlParse)?;
    let section = match value.remove("gitops") {
        Some(toml::Value::Table(section)) => section,
        Some(_) => {
            return Err(RelishError::InvalidFlag {
                flag: "path".into(),
                reason: format!("{}: [gitops] must be a table", path.display()),
            });
        }
        None => value,
    };
    Ok(section
        .try_into()
        .map_err(crate::config::ConfigError::TomlParse)?)
}

/// `relish gitops set <file>`: replace the cluster's GitOps configuration.
pub async fn gitops_set(path: &Path, output: OutputFormat) -> Result<(), RelishError> {
    let config = read_gitops_section(path)?;
    BunClient::default_local()
        .gitops_config_set(Some(config))
        .await?;
    print_output(
        &GitopsChange {
            enabled: true,
            config: Some(path),
        },
        output,
    )
}

/// `relish gitops disable`: turn GitOps off; what it wrote stays.
pub async fn gitops_disable(output: OutputFormat) -> Result<(), RelishError> {
    BunClient::default_local().gitops_config_set(None).await?;
    print_output(
        &GitopsChange {
            enabled: false,
            config: None,
        },
        output,
    )
}

/// What `relish gitops set` or `disable` changed.
#[derive(Debug, serde::Serialize)]
pub struct GitopsChange<'a> {
    /// GitOps now syncs.
    pub enabled: bool,
    /// The file the new configuration came from.
    pub config: Option<&'a Path>,
}

impl std::fmt::Display for GitopsChange<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.config {
            Some(path) => writeln!(
                f,
                "the cluster's GitOps configuration is now {}; the leader syncs it on its next tick",
                path.display()
            ),
            None => writeln!(
                f,
                "GitOps is off; the apps, namespaces and permissions it wrote stay in place"
            ),
        }
    }
}

/// Rotate or finalise a secret encryption key; the node answers in a
/// sentence, which `-o json` prints as `{"message": ...}`.
pub async fn secret_rotate(
    finalize: bool,
    namespace: Option<&str>,
    output: OutputFormat,
) -> Result<(), RelishError> {
    let client = BunClient::default_local();
    let message = client.secret_rotate(finalize, namespace).await?;
    print_output(&super::output::Message { message }, output)
}

/// Sign a Pickle-hosted image with the operator's key and attach the
/// signature. A tag is resolved to its manifest digest first, and the
/// digest is what gets signed: a tag can move, a digest can't.
pub async fn sign(image: &str, key_path: &Path, output: OutputFormat) -> Result<(), RelishError> {
    let key_text = fs::read_to_string(key_path)?;
    let key = crate::pickle::signing::SigningKey::from_pem(&key_text)?;

    let client = BunClient::default_local();
    let listing = client.images().await?;
    let images: Vec<crate::pickle::types::ImageSummary> =
        serde_json::from_value(listing["images"].clone()).map_err(|e| RelishError::ApiError {
            status: 0,
            body: format!("failed to parse images response: {e}"),
        })?;
    let digest = resolve_image_digest(image, &images)?;

    let submission = key.sign(&digest)?;
    let message = client.sign_image(&submission).await?;
    print_output(
        &SignReport {
            image,
            digest: digest.as_str(),
            message,
        },
        output,
    )
}

/// What `relish sign` signed.
#[derive(Debug, serde::Serialize)]
pub struct SignReport<'a> {
    /// The image as given.
    pub image: &'a str,
    /// The manifest digest the signature covers.
    pub digest: &'a str,
    /// What the cluster said when it attached the signature.
    pub message: String,
}

impl std::fmt::Display for SignReport<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "{}", self.message)
    }
}

/// Generate an image signing key at `out` (PKCS#8 PEM, owner-only
/// permissions) and print the public key in the form
/// `[images.trust_policy] keys` expects. The private key never leaves the
/// file, whatever the format.
pub fn sign_keygen(out: &Path, output: OutputFormat) -> Result<(), RelishError> {
    let key = crate::pickle::signing::SigningKey::generate()?;
    write_private_key(out, key.to_pem().as_bytes())?;
    print_output(
        &SigningKeyReport {
            path: out,
            public_key: key.public_key_base64(),
        },
        output,
    )
}

/// What `relish sign keygen` wrote: where the private key is, and the
/// public key to trust.
#[derive(Debug, serde::Serialize)]
pub struct SigningKeyReport<'a> {
    pub path: &'a Path,
    pub public_key: String,
}

impl std::fmt::Display for SigningKeyReport<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let public_key = &self.public_key;
        writeln!(f, "wrote image signing key to {}", self.path.display())?;
        writeln!(f, "public key: {public_key}")?;
        writeln!(f)?;
        writeln!(f, "Trust it by adding this to every node's config:")?;
        writeln!(f)?;
        writeln!(f, "[images.trust_policy]")?;
        writeln!(f, "require_signatures = true")?;
        writeln!(f, "keys = [\"{public_key}\"]")
    }
}

/// Write a private key, refusing to overwrite and keeping it owner-only.
pub(crate) fn write_private_key(path: &Path, contents: &[u8]) -> Result<(), RelishError> {
    use std::io::Write as _;
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(0o600);
    }
    let mut file = options.open(path).map_err(|e| match e.kind() {
        std::io::ErrorKind::AlreadyExists => RelishError::FileExists {
            path: path.display().to_string(),
        },
        _ => RelishError::Io(e),
    })?;
    file.write_all(contents)?;
    Ok(())
}

/// Resolve the image `relish sign` was given to the manifest digest to sign.
///
/// Accepts a tag reference (`myapp:v1`, `localhost:5050/team/app:v2`), a
/// pinned reference (`myapp@sha256:…`) or a bare digest. The registry host
/// is stripped the same way the deploy-time trust check strips it, so the
/// digest signed here is the one a deploy of the same reference verifies.
pub fn resolve_image_digest(
    image: &str,
    images: &[crate::pickle::types::ImageSummary],
) -> Result<crate::pickle::types::Digest, RelishError> {
    use crate::meat::scheduler::{canonical_repository, split_repo_tag};

    let not_found = || RelishError::ImageNotInRegistry {
        image: image.to_string(),
    };
    // A multi-platform image lists its platform manifests under the index;
    // a digest can name either.
    let holds = |summary: &crate::pickle::types::ImageSummary, digest: &str| {
        summary.digest == digest || summary.platforms.iter().any(|p| p.digest == digest)
    };
    let found = if image.starts_with("sha256:") {
        images
            .iter()
            .find(|summary| holds(summary, image))
            .map(|_| image)
    } else if let Some((name, digest)) = image.split_once('@') {
        let (name, _tag) = split_repo_tag(name);
        let repository = canonical_repository(name);
        images
            .iter()
            .find(|summary| summary.repository == repository && holds(summary, digest))
            .map(|_| digest)
    } else {
        let (name, tag) = split_repo_tag(image);
        let repository = canonical_repository(name);
        images
            .iter()
            .find(|summary| summary.repository == repository && summary.tags.contains(tag))
            .map(|summary| summary.digest.as_str())
    };
    let digest = found.ok_or_else(not_found)?;
    crate::pickle::types::Digest::new(digest).map_err(|_| not_found())
}

/// Split `repository:tag` for `relish images delete`. A registry host in
/// front (`localhost:5050/team/web:v1`) is dropped, the way deploys drop it.
/// A digest, or a name without a tag, is refused: deleting `web` must not
/// quietly mean `web:latest`.
pub fn parse_image_tag(reference: &str) -> Result<(&str, &str), RelishError> {
    let invalid = || RelishError::InvalidImageTag {
        reference: reference.to_string(),
    };
    if reference.contains('@') {
        return Err(invalid());
    }
    let tail_start = reference.rfind('/').map_or(0, |i| i + 1);
    let (name, tag) = reference[tail_start..]
        .rfind(':')
        .map(|i| {
            (
                &reference[..tail_start + i],
                &reference[tail_start + i + 1..],
            )
        })
        .ok_or_else(invalid)?;
    let repository = crate::meat::scheduler::canonical_repository(name);
    if repository.is_empty() || tag.is_empty() || tag.starts_with("sha256") {
        return Err(invalid());
    }
    Ok((repository, tag))
}

/// `relish images delete <repository:tag>`: untag an image in Pickle.
pub async fn image_delete(reference: &str, output: OutputFormat) -> Result<(), RelishError> {
    image_delete_with_client(reference, output, &BunClient::default_local()).await
}

/// The tag `relish images delete` removed.
#[derive(Debug, serde::Serialize)]
pub struct DeletedTag<'a> {
    pub repository: &'a str,
    pub tag: &'a str,
}

impl std::fmt::Display for DeletedTag<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(
            f,
            "deleted {}:{}; the image expires once it has been untagged for \
             gc_retain_days, unless an app or job pins it",
            self.repository, self.tag
        )
    }
}

async fn image_delete_with_client(
    reference: &str,
    output: OutputFormat,
    client: &BunClient,
) -> Result<(), RelishError> {
    let (repository, tag) = parse_image_tag(reference)?;
    let registry = client
        .capabilities()
        .await?
        .service_endpoints
        .registry
        .ok_or(RelishError::RegistryUnavailable)?;
    client.delete_image_tag(&registry, repository, tag).await?;
    print_output(&DeletedTag { repository, tag }, output)
}

pub async fn images(output: OutputFormat) -> Result<(), RelishError> {
    let client = BunClient::default_local();
    let result = client.images().await?;
    if let OutputFormat::Json | OutputFormat::Yaml = output {
        let text = match output {
            OutputFormat::Json => {
                serde_json::to_string_pretty(&result).map_err(RelishError::SerialiseJson)?
            }
            _ => yaml_serde::to_string(&result).map_err(RelishError::SerialiseYaml)?,
        };
        print!("{text}");
        if matches!(output, OutputFormat::Json) {
            println!();
        }
        return Ok(());
    }
    let images: Vec<crate::pickle::types::ImageSummary> =
        serde_json::from_value(result["images"].clone()).map_err(|e| RelishError::ApiError {
            status: 0,
            body: format!("failed to parse images response: {e}"),
        })?;
    print!("{}", format_images_table(&images));
    Ok(())
}

/// Render `relish images` as a table: one row per image, with a
/// multi-platform image's platforms in its PLATFORMS column and `-` for
/// LAYERS (each platform has its own; `--output json` lists them).
pub fn format_images_table(images: &[crate::pickle::types::ImageSummary]) -> String {
    if images.is_empty() {
        return "no images in local registry\n".to_string();
    }
    let header = ["REPOSITORY", "TAG", "PLATFORMS", "LAYERS", "SIZE"].map(str::to_string);
    let rows: Vec<[String; 5]> = images.iter().map(image_row).collect();
    // Each column is as wide as its widest cell, so a long pull-through
    // name like `cache/public.ecr.aws/...` can't push its row out of line.
    let mut widths = header.clone().map(|cell| cell.len());
    for row in &rows {
        for (width, cell) in widths.iter_mut().zip(row) {
            *width = (*width).max(cell.chars().count());
        }
    }
    let [repository, tag, platforms, layers, size] = widths;
    let mut out = String::new();
    for [r, t, p, l, s] in std::iter::once(&header).chain(&rows) {
        let line =
            format!("{r:<repository$}  {t:<tag$}  {p:<platforms$}  {l:>layers$}  {s:>size$}");
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out
}

/// One `relish images` row: repository, tags, platforms, layers and size.
fn image_row(image: &crate::pickle::types::ImageSummary) -> [String; 5] {
    let tags = image.tags.iter().cloned().collect::<Vec<_>>().join(", ");
    let tags = if tags.is_empty() {
        "<none>".to_string()
    } else {
        tags
    };
    let (platforms, layers) = if image.platforms.is_empty() {
        ("-".to_string(), image.layers.to_string())
    } else {
        let names: Vec<&str> = image
            .platforms
            .iter()
            .map(|p| p.platform.as_str())
            .collect();
        (names.join(", "), "-".to_string())
    };
    [
        image.repository.clone(),
        tags,
        platforms,
        layers,
        format_image_size(image.total_size),
    ]
}

fn format_image_size(size: u64) -> String {
    if size >= 1_000_000 {
        format!("{:.1} MB", size as f64 / 1_000_000.0)
    } else if size >= 1_000 {
        format!("{:.1} KB", size as f64 / 1_000.0)
    } else {
        format!("{size} B")
    }
}

/// Poll a build to a terminal state, bounded by `timeout` and Ctrl-C
/// (12b.2, the JOB6 residue: the old loop polled forever). Returns the
/// built image reference; a timeout or interrupt is an error carrying
/// the last known state, so the caller exits non-zero with something
/// useful to print.
pub async fn wait_for_build(
    client: &BunClient,
    build_id: u64,
    timeout: std::time::Duration,
) -> Result<String, RelishError> {
    let deadline = tokio::time::Instant::now() + timeout;
    let mut last_state = "unknown".to_string();
    loop {
        let now = tokio::time::Instant::now();
        if now >= deadline {
            return Err(RelishError::ApiError {
                status: 0,
                body: format!(
                    "timed out after {}s waiting for build {build_id}; \
                     last known state: {last_state}",
                    timeout.as_secs()
                ),
            });
        }
        let pause = std::cmp::min(std::time::Duration::from_secs(2), deadline - now);
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                return Err(RelishError::ApiError {
                    status: 0,
                    body: format!(
                        "interrupted while waiting for build {build_id}; \
                         last known state: {last_state}"
                    ),
                });
            }
            _ = tokio::time::sleep(pause) => {}
        }
        let status = client.build_status(build_id).await?;
        match status["status"].as_str() {
            Some("completed") => {
                return Ok(status["image"].as_str().unwrap_or("?").to_string());
            }
            Some("failed") => {
                return Err(RelishError::ApiError {
                    status: 0,
                    body: format!(
                        "build failed: {}",
                        status["reason"].as_str().unwrap_or("unknown")
                    ),
                });
            }
            other => last_state = other.unwrap_or("unknown").to_string(),
        }
    }
}

/// Poll a batch to a terminal state, bounded by `timeout` and Ctrl-C
/// (12b.2, JOB6 residue). Returns the final summary JSON.
pub async fn wait_for_batch(
    client: &BunClient,
    batch_id: u64,
    timeout: std::time::Duration,
) -> Result<serde_json::Value, RelishError> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let summary = client.batch_status(batch_id).await?;
        if summary["done"].as_bool().unwrap_or(false) {
            return Ok(summary);
        }
        if summary["status"] == "Unknown" {
            return Err(RelishError::ApiError {
                status: 409,
                body: format!(
                    "run {batch_id} needs an operator decision; inspect its unknown_owners before acknowledged replay"
                ),
            });
        }
        let last_summary = summary;
        let now = tokio::time::Instant::now();
        if now >= deadline {
            return Err(RelishError::ApiError {
                status: 0,
                body: format!(
                    "timed out after {}s waiting for batch {batch_id}; \
                     last known state: {last_summary}",
                    timeout.as_secs()
                ),
            });
        }
        let pause = std::cmp::min(std::time::Duration::from_secs(1), deadline - now);
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {
                return Err(RelishError::ApiError {
                    status: 0,
                    body: format!(
                        "interrupted while waiting for batch {batch_id}; \
                         last known state: {last_summary}"
                    ),
                });
            }
            _ = tokio::time::sleep(pause) => {}
        }
    }
}

/// What `relish build` prints about a job before submitting it: where the
/// image goes and the Buildah command the node runs.
fn build_plan_lines(job: &crate::pickle::build::BuildahJob) -> Vec<String> {
    vec![
        format!(
            "  destination: pickle://{}:{}",
            job.destination.name, job.destination.tag
        ),
        format!("  build:  {}", job.build_cmd.join(" ")),
    ]
}

/// Build OCI images and push to Pickle.
///
/// Reads `[build.*]` sections from the config, tars each context,
/// uploads it to Pickle, and submits a build job.
///
/// For people it narrates each step. With `-o json` the narration goes to
/// stderr and stdout gets one report of every image built.
pub async fn build(
    path: &std::path::Path,
    registry_port: Option<u16>,
    timeout_secs: u64,
    output: OutputFormat,
) -> Result<(), RelishError> {
    use super::output::progress;
    use crate::config::Config;
    use crate::pickle::build::{cli_context_upload_url, digest_of, execute_build, tar_context};

    let config = Config::from_file(path)?;
    let mut report = BuildReport { images: Vec::new() };
    if config.build.is_empty() {
        eprintln!("no [build.*] sections found in {}", path.display());
        if output != OutputFormat::Human {
            print_output(&report, output)?;
        }
        return Ok(());
    }

    let client = BunClient::default_local();

    for (name, spec) in &config.build {
        progress(output, format_args!("Building {name}..."));

        // Tar the context
        let context_path = if spec.context.is_relative() {
            path.parent()
                .unwrap_or(std::path::Path::new("."))
                .join(&spec.context)
        } else {
            spec.context.clone()
        };
        let tar_bytes = tar_context(&context_path).map_err(|e| RelishError::ApiError {
            status: 0,
            body: format!("failed to tar context: {e}"),
        })?;
        let digest = digest_of(&tar_bytes);
        progress(
            output,
            format_args!(
                "  context: {} ({} bytes, {digest})",
                context_path.display(),
                tar_bytes.len()
            ),
        );

        // Upload context blob to the Pickle registry (X1: this used to
        // target the Bun API port, which has no /v2 routes). Route through the
        // authenticated client (M21): a bare client carried no bearer/CA, so it
        // 401'd or failed TLS against a secured registry.
        // O2: address the registry the way it actually serves. Hardcoding
        // `http://` failed outright against a TLS registry, and where it
        // worked it pushed the context — the caller's source tree — in clear.
        let upload_url = cli_context_upload_url(
            client.scheme(),
            registry_port,
            client.declared_registry(),
            &digest,
        );
        let resp = client
            .http()?
            .post(&upload_url)
            .body(tar_bytes)
            .send()
            .await
            .map_err(|e| RelishError::ApiError {
                status: 0,
                body: format!("failed to upload context: {e}"),
            })?;
        let upload_status = resp.status();
        if !upload_status.is_success() {
            let body = resp.text().await.unwrap_or_default();
            return Err(RelishError::ApiError {
                status: upload_status.as_u16(),
                body: format!("context upload failed: {body}"),
            });
        }
        progress(output, "  context uploaded to Pickle");

        // Prepare the build job (for display; the agent re-derives it with
        // its own registry port, which a host forward doesn't change).
        let job = execute_build(spec, &digest, None).map_err(|e| RelishError::ApiError {
            status: 0,
            body: format!("build preparation failed: {e}"),
        })?;

        for line in build_plan_lines(&job) {
            progress(output, line);
        }

        // Submit and poll: builds run async on the builder node —
        // minutes-long buildah runs must not hold an HTTP request open.
        // The wait is bounded (JOB6): a stuck build ends with a
        // non-zero exit and the last known state, not an infinite loop.
        let build_id = client.submit_build(name, &digest, spec).await?;
        progress(
            output,
            format_args!("  build {build_id} accepted; waiting (up to {timeout_secs}s)..."),
        );
        let image = wait_for_build(
            &client,
            build_id,
            std::time::Duration::from_secs(timeout_secs),
        )
        .await?;
        progress(output, format_args!("  built and pushed {image}"));
        report.images.push(BuiltImage {
            name: name.clone(),
            context_digest: digest,
            build_id,
            image,
        });
    }

    if output != OutputFormat::Human {
        print_output(&report, output)?;
    }
    Ok(())
}

/// What `relish build -o json` built.
#[derive(Debug, serde::Serialize)]
pub struct BuildReport {
    pub images: Vec<BuiltImage>,
}

/// One `[build.*]` section that built and pushed.
#[derive(Debug, serde::Serialize)]
pub struct BuiltImage {
    /// The `[build.NAME]` section.
    pub name: String,
    /// The uploaded context's digest.
    pub context_digest: String,
    pub build_id: u64,
    /// The image reference it pushed.
    pub image: String,
}

impl std::fmt::Display for BuildReport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for image in &self.images {
            writeln!(f, "built and pushed {}", image.image)?;
        }
        Ok(())
    }
}

/// Longest `Idempotency-Key` the cluster keeps as a request identity.
const MAX_IDEMPOTENCY_KEY_BYTES: usize = 128;

/// The key this submission sends: the caller's `--idempotency-key`, or a
/// fresh random one. Either way it is written to `err` (stderr in
/// production) *before* anything is sent, so a person whose submission times
/// out already holds the key that makes the retry safe.
pub fn submission_key(
    explicit: Option<String>,
    err: &mut dyn std::io::Write,
) -> Result<String, RelishError> {
    let key = match explicit {
        Some(key) => {
            if key.is_empty()
                || key.len() > MAX_IDEMPOTENCY_KEY_BYTES
                || !key.bytes().all(|byte| byte.is_ascii_graphic())
            {
                return Err(RelishError::InvalidFlag {
                    flag: "idempotency-key".into(),
                    reason: format!(
                        "use 1–{MAX_IDEMPOTENCY_KEY_BYTES} printable ASCII characters without spaces"
                    ),
                });
            }
            key
        }
        None => format!("{:032x}", rand::random::<u128>()),
    };
    // A failed write to stderr mustn't stop the submission.
    let _ = writeln!(
        err,
        "idempotency key {key}; if this submission times out, retry with --idempotency-key {key}"
    );
    Ok(key)
}

/// Submit a batch of jobs for high-throughput scheduling.
///
/// `-o json` prints the cluster's answer: `batch_id`, `assigned` and
/// `unschedulable`.
pub async fn batch(
    path: &std::path::Path,
    idempotency_key: Option<String>,
    output: OutputFormat,
) -> Result<(), RelishError> {
    batch_with_client(
        path,
        idempotency_key,
        output,
        &BunClient::default_local(),
        &mut std::io::stderr(),
    )
    .await
}

async fn batch_with_client(
    path: &std::path::Path,
    idempotency_key: Option<String>,
    output: OutputFormat,
    client: &BunClient,
    err: &mut dyn std::io::Write,
) -> Result<(), RelishError> {
    use crate::config::Config;

    let config = Config::from_file(path)?;
    if config.job.is_empty() {
        return Err(RelishError::InvalidFlag {
            flag: "file".into(),
            reason: format!("no [job.*] sections found in {}", path.display()),
        });
    }

    let key = submission_key(idempotency_key, err)?;
    let result = client.submit_batch(&config.job, Some(&key)).await?;
    print_policy_warnings(&result, err);
    if output != OutputFormat::Human {
        println!("{}", format_output(&result, output)?);
        return Ok(());
    }
    println!(
        "batch {} admitted: {} queued jobs",
        result["batch_id"].as_u64().unwrap_or(0),
        result["assigned"].as_u64().unwrap_or(0),
    );
    if let Some(unschedulable) = result["unschedulable"].as_array()
        && !unschedulable.is_empty()
    {
        println!(
            "unschedulable: {}",
            unschedulable
                .iter()
                .filter_map(|v| v.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    println!(
        "check progress with: relish batch-status {}",
        result["batch_id"].as_u64().unwrap_or(0)
    );
    Ok(())
}

/// Show a batch's progress; with `wait`, poll (bounded by `timeout`)
/// until the batch reaches a terminal state.
pub async fn batch_status(
    batch_id: u64,
    wait: bool,
    timeout_secs: u64,
    output: OutputFormat,
) -> Result<(), RelishError> {
    let client = BunClient::default_local();
    let summary = if wait {
        wait_for_batch(
            &client,
            batch_id,
            std::time::Duration::from_secs(timeout_secs),
        )
        .await?
    } else {
        client.batch_status(batch_id).await?
    };
    if matches!(output, OutputFormat::Human) {
        print_batch_summary(batch_id, &summary);
    } else {
        println!("{}", format_output(&summary, output)?);
    }
    Ok(())
}

fn print_batch_summary(batch_id: u64, summary: &serde_json::Value) {
    if summary["kind"] == "array" || summary["kind"] == "manifest" {
        print!("{}", format_array_summary(batch_id, summary));
        return;
    }
    let unschedulable = summary["unschedulable"].as_u64().unwrap_or(0);
    println!(
        "batch {}: {} total, {} pending, {} completed, {} failed{}{}",
        batch_id,
        summary["total"].as_u64().unwrap_or(0),
        summary["pending"].as_u64().unwrap_or(0),
        summary["completed"].as_u64().unwrap_or(0),
        summary["failed"].as_u64().unwrap_or(0),
        if unschedulable > 0 {
            format!(", {unschedulable} unschedulable")
        } else {
            String::new()
        },
        if summary["done"].as_bool().unwrap_or(false) {
            " — done"
        } else {
            ""
        },
    );
}

/// What `relish run --batch` submits.
#[derive(Debug, Clone, PartialEq)]
pub struct TaskArrayRun {
    /// Explicit job backend; host execution requires process.
    pub runtime: crate::config::job::JobRuntime,
    /// Durable UTC schedule; omitted means an immediate manual run.
    pub schedule: Option<String>,
    pub image: Option<String>,
    pub cpu: Option<String>,
    pub memory: Option<String>,
    /// The array's name.
    pub name: String,
    /// Its namespace.
    pub namespace: String,
    /// Host binary every task runs.
    pub exec: Option<std::path::PathBuf>,
    /// Argument template with `{index}` placeholders.
    pub args: Vec<String>,
    /// `KEY=VALUE` pairs for every task's environment.
    pub env: Vec<String>,
    /// Count and policy.
    pub spec: crate::meat::task_array::TaskArraySpec,
    /// `--idempotency-key` from an earlier, uncertain submission.
    pub idempotency_key: Option<String>,
}

/// Turn `relish run --batch` flags into the API request, refusing bad
/// `--env` pairs and a relative `--exec` before anything is sent.
pub fn task_array_request(
    run: TaskArrayRun,
) -> Result<crate::bun::task_array_api::TaskArraySubmitRequest, RelishError> {
    if (run.runtime == crate::config::job::JobRuntime::Process
        && (run.image.is_some() || run.exec.is_none()))
        || (run.runtime != crate::config::job::JobRuntime::Process
            && (run.image.is_none() || run.exec.is_some()))
    {
        return Err(RelishError::InvalidFlag { flag: "runtime".into(), reason: "process requires --exec and refuses --image; runc/shared-runc require --image and refuse --exec".into() });
    }
    if let Some(path) = run.exec.as_ref().filter(|path| !path.is_absolute()) {
        return Err(RelishError::InvalidFlag {
            flag: "exec".to_string(),
            reason: format!(
                "{} isn't an absolute path; nodes allow binaries by absolute path",
                path.display()
            ),
        });
    }
    let mut env = std::collections::BTreeMap::new();
    for pair in &run.env {
        let Some((key, value)) = pair.split_once('=').filter(|(key, _)| !key.is_empty()) else {
            return Err(RelishError::InvalidFlag {
                flag: "env".to_string(),
                reason: format!("{pair:?} isn't KEY=VALUE"),
            });
        };
        env.insert(
            key.to_string(),
            crate::config::types::EnvValue::Plain(value.to_string()),
        );
    }
    Ok(crate::bun::task_array_api::TaskArraySubmitRequest {
        name: run.name,
        namespace: Some(run.namespace),
        template: crate::config::job::JobSpec {
            pids: None,
            runtime: run.runtime,
            image: run.image,
            command: Some(run.args),
            schedule: run.schedule,
            run_before: Vec::new(),
            memory: run
                .memory
                .as_deref()
                .map(crate::config::types::ResourceRange::parse_memory)
                .transpose()?,
            cpu: run
                .cpu
                .as_deref()
                .map(crate::config::types::ResourceRange::parse_cpu)
                .transpose()?,
            env,
            namespace: None,
            exec: run.exec,
            script: None,
            firewall: None,
            egress: None,
            max_attempts: None,
            task_timeout_secs: None,
            overlap: None,
            replay_unknown: false,
            provenance: None,
        },
        spec: run.spec,
    })
}

/// Submit all resource profiles atomically from a compact TOML manifest.
pub async fn submit_task_manifest(
    path: &std::path::Path,
    output: OutputFormat,
    dry_run: bool,
    idempotency_key: Option<String>,
) -> Result<(), RelishError> {
    submit_task_manifest_with_client(
        path,
        output,
        dry_run,
        idempotency_key,
        &BunClient::default_local(),
        &mut std::io::stderr(),
    )
    .await
}

async fn submit_task_manifest_with_client(
    path: &std::path::Path,
    output: OutputFormat,
    dry_run: bool,
    idempotency_key: Option<String>,
    client: &BunClient,
    err: &mut dyn std::io::Write,
) -> Result<(), RelishError> {
    let source = std::fs::read_to_string(path)?;
    let request: crate::bun::task_array_api::TaskManifestRequest = toml::from_str(&source)
        .map_err(|e| RelishError::InvalidFlag {
            flag: "manifest".into(),
            reason: e.to_string(),
        })?;
    let mut trial = crate::meat::task_array_store::TaskArrays::default();
    let mut next = 0;
    trial
        .apply(
            &crate::meat::task_array_store::TaskArrayWrite::RegisterManifest {
                name: request.name.clone(),
                namespace: request.namespace.clone(),
                request_id: "dry-run".into(),
                cohorts: request.cohort.clone(),
                submitted_at_epoch_secs: 0,
            },
            || {
                next += 1;
                next
            },
        )
        .map_err(|error| RelishError::InvalidFlag {
            flag: "manifest".into(),
            reason: error.to_string(),
        })?;
    if dry_run {
        let tasks: u64 = request.cohort.iter().map(|c| u64::from(c.spec.count)).sum();
        return print_output(
            &ManifestCheck {
                valid: true,
                profiles: request.cohort.len(),
                tasks,
            },
            output,
        );
    }
    let key = submission_key(idempotency_key, err)?;
    let response = client.submit_task_manifest(&request, Some(&key)).await?;
    print_policy_warnings(&response, err);
    if matches!(output, OutputFormat::Human) {
        println!(
            "manifest {} submitted; watch with: relish batch watch {}",
            response["batch_id"], response["batch_id"]
        );
    } else {
        println!("{}", format_output(&response, output)?);
    }
    Ok(())
}

/// What `relish batch submit --dry-run` checked.
#[derive(Debug, serde::Serialize)]
pub struct ManifestCheck {
    pub valid: bool,
    pub profiles: usize,
    pub tasks: u64,
}

impl std::fmt::Display for ManifestCheck {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(
            f,
            "valid manifest: {} profiles, {} tasks",
            self.profiles, self.tasks
        )
    }
}

/// One `relish batch watch` sample: the cluster's summary, one line for
/// people, one JSON object per line with `-o json`.
#[derive(Debug, serde::Serialize)]
#[serde(transparent)]
pub struct WatchSample(pub serde_json::Value);

impl std::fmt::Display for WatchSample {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&format_batch_watch(&self.0))
    }
}

/// Observe bounded summaries once a second; never enumerate the task list.
/// With `-o json` each sample is a JSON object on its own line.
pub async fn watch_task_batch(
    batch_id: u64,
    timeout_secs: u64,
    output: OutputFormat,
) -> Result<(), RelishError> {
    let client = BunClient::default_local();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(timeout_secs);
    loop {
        let summary = WatchSample(client.batch_status(batch_id).await?);
        super::output::print_record(&summary, output)?;
        let summary = summary.0;
        if summary["status"] == "Unknown" {
            return Err(RelishError::ApiError {
                status: 409,
                body: format!(
                    "run {batch_id} needs an operator decision; inspect its unknown_owners before acknowledged replay"
                ),
            });
        }
        if summary["done"].as_bool() == Some(true) {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(RelishError::RequestTimeout);
        }
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }
}

/// A rate is unique accepted successes, while durations describe the final
/// attempt. Quantiles merge bucket counts and report bucket upper bounds.
pub fn format_batch_watch(summary: &serde_json::Value) -> String {
    if summary["kind"] == "schedule" {
        let mut line = format!(
            "{}/{}: UTC schedule {}, {} tasks per occurrence; revision {}",
            summary["namespace"].as_str().unwrap_or("default"),
            summary["name"].as_str().unwrap_or("?"),
            summary["cron"]["expression"].as_str().unwrap_or("?"),
            summary["total"],
            summary["revision"]
        );
        if let Some(minute) = summary["skipped"]["minute"].as_i64() {
            line.push_str(&format!(
                " | skipped: capacity ({} so far, latest at minute {minute})",
                summary["skipped_count"].as_u64().unwrap_or(1)
            ));
        }
        return line;
    }

    let rate = summary["rates"]["successes_per_second"]
        .as_f64()
        .map_or("unknown".to_string(), |r| format!("{r:.1}/s"));
    let interval = summary["rates"]["interval_seconds"]
        .as_f64()
        .map_or("unknown".into(), |seconds| format!("{seconds:.1}s"));
    let whole = summary["whole_run_successes_per_second"]
        .as_f64()
        .map_or("unknown".into(), |rate| format!("{rate:.1}/s"));
    let mut output = format!(
        "batch {}: {} / {} succeeded, {} failed, {} not run, {} retries; {} | accepted successes recent {} over {} | since admission {}",
        summary["batch_id"],
        summary["succeeded"],
        summary["total"],
        summary["failed"],
        summary["not_run"],
        summary["retried"],
        summary["status"].as_str().unwrap_or("unknown"),
        rate,
        interval,
        whole
    );
    let count = |key: &str| {
        summary[key]
            .as_u64()
            .map_or("unknown".into(), |value| value.to_string())
    };
    output.push_str(&format!(
        " | mode {} | {} active commands, {} other in-flight attempts",
        summary["runtime"]
            .as_str()
            .unwrap_or(if summary["kind"] == "manifest" {
                "mixed"
            } else {
                "unknown"
            }),
        count("active_commands"),
        count("other_in_flight_attempts")
    ));
    output.push_str(&format!(
        " | {} queued, {} held",
        summary["queued"], summary["held"]
    ));
    if let Some(reason) = summary["quota_blocked_reason"].as_str() {
        output.push_str(&format!(" | quota blocked: {reason}"));
    }
    if let Some(reason) = summary["unrunnable_reason"].as_str() {
        output.push_str(&format!(" | refused: {reason}"));
    }
    let failure_rate = summary["rates"]["failures_per_second"]
        .as_f64()
        .map_or("unknown".into(), |r| format!("{r:.1}/s"));
    output.push_str(&format!(" | terminal failures {failure_rate}"));
    if let Some(counts) = summary["duration_final_attempt_ms"]["counts"].as_array() {
        let counts: Vec<u64> = counts.iter().map(|c| c.as_u64().unwrap_or(0)).collect();
        let total: u64 = counts.iter().sum();
        for (label, percent) in [("p50", 50), ("p95", 95), ("p99", 99)] {
            let mut cumulative = 0;
            let bucket = counts.iter().position(|count| {
                cumulative += count;
                total > 0 && cumulative >= total.saturating_mul(percent).div_ceil(100)
            });
            let value = match bucket {
                Some(b) if b < 15 => format!("<={}ms", 1u64 << b),
                Some(_) => ">16384ms".into(),
                None => "unknown".into(),
            };
            output.push_str(&format!(" | final attempt {label} {value}"));
        }
    }
    for cohort in summary["cohorts"].as_array().into_iter().flatten() {
        output.push_str(&format!(
            "\n  {}: array {}, {} / {} succeeded; request {}m CPU, {}Mi memory",
            cohort["profile"].as_str().unwrap_or("?"),
            cohort["batch_id"],
            cohort["succeeded"],
            cohort["total"],
            cohort["cpu_request_millicores"],
            cohort["memory_request_bytes"].as_u64().unwrap_or(0) / (1024 * 1024)
        ));
    }
    for cohort in summary["cohorts"].as_array().into_iter().flatten() {
        if let Some(reason) = cohort["unrunnable_reason"].as_str() {
            output.push_str(&format!(
                "\n  array {} refused: {reason}",
                cohort["batch_id"]
            ));
        }
    }
    for profile in
        std::iter::once(summary).chain(summary["cohorts"].as_array().into_iter().flatten())
    {
        for node in profile["nodes"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|node| node["refused"].is_string())
            .take(3)
        {
            output.push_str(&format!(
                "\n  array {} refused on {}: {}",
                profile["batch_id"],
                node["node"].as_str().unwrap_or("?"),
                node["refused"].as_str().unwrap_or("?")
            ));
        }
    }
    output
}

/// Print what the `warn` policies said about a submitted job (#1424), one
/// `warning:` line each, to `err`.
fn print_policy_warnings(answer: &serde_json::Value, err: &mut dyn std::io::Write) {
    for warning in answer["policy_warnings"].as_array().into_iter().flatten() {
        if let Some(warning) = warning.as_str() {
            let _ = writeln!(err, "warning: {warning}");
        }
    }
}

/// Submit a task array (`relish run --batch`).
///
/// `-o json` prints the cluster's answer (`batch_id`, `count`, `chunks` for
/// a run; no `batch_id` for a schedule).
pub async fn run_task_array(run: TaskArrayRun, output: OutputFormat) -> Result<(), RelishError> {
    run_task_array_with_client(
        run,
        output,
        &BunClient::default_local(),
        &mut std::io::stderr(),
    )
    .await
}

async fn run_task_array_with_client(
    mut run: TaskArrayRun,
    output: OutputFormat,
    client: &BunClient,
    err: &mut dyn std::io::Write,
) -> Result<(), RelishError> {
    let explicit = run.idempotency_key.take();
    let scheduled = run.schedule.is_some();
    let request = task_array_request(run)?;
    // A schedule registers a definition, not a run, so it needs no key;
    // re-registering the same schedule is already idempotent.
    let key = if scheduled {
        explicit.unwrap_or_else(|| format!("{:032x}", rand::random::<u128>()))
    } else {
        submission_key(explicit, err)?
    };
    let answer = client.submit_task_array(&request, Some(&key)).await?;
    print_policy_warnings(&answer, err);
    if output != OutputFormat::Human {
        println!("{}", format_output(&answer, output)?);
        return Ok(());
    }
    if let Some(id) = answer["batch_id"].as_u64() {
        println!(
            "job run {id} admitted: {} tasks in {} chunks; at-least-once replay policy",
            answer["count"], answer["chunks"]
        );
        println!("check progress with: relish batch-status {id}");
    } else {
        println!(
            "schedule registered for {}/{}; UTC, forbid overlap, skip missed minutes; inspect with relish jobs --definitions",
            request.namespace.as_deref().unwrap_or("default"),
            request.name
        );
    }
    Ok(())
}

/// List bounded runs and rates, or durable definitions, without listing task indices.
pub async fn jobs(definitions: bool, output: OutputFormat) -> Result<(), RelishError> {
    let rows = BunClient::default_local()
        .job_summaries(definitions)
        .await?;
    if !matches!(output, OutputFormat::Human) {
        println!("{}", format_output(&rows, output)?);
        return Ok(());
    }
    if definitions {
        for row in rows["definitions"].as_array().into_iter().flatten() {
            print!("{}", format_definition(row));
        }
    } else {
        for row in rows["batches"].as_array().into_iter().flatten() {
            println!("{}", format_batch_watch(row));
        }
    }
    Ok(())
}
/// Explicitly accept repeating side effects from exactly the named unknown grants.
pub async fn replay_job(
    id: u64,
    node: &str,
    digest: &str,
    output: OutputFormat,
) -> Result<(), RelishError> {
    BunClient::default_local()
        .replay_job(id, node, digest)
        .await?;
    print_output(
        &RunChange {
            run: id,
            change: RunChangeKind::ReplayAcknowledged,
            node: Some(node),
        },
        output,
    )
}

/// Stop a task array (`relish batch cancel`).
pub async fn batch_cancel(batch_id: u64, output: OutputFormat) -> Result<(), RelishError> {
    BunClient::default_local().cancel_batch(batch_id).await?;
    print_output(
        &RunChange {
            run: batch_id,
            change: RunChangeKind::Cancelled,
            node: None,
        },
        output,
    )
}

/// What `relish batch replay` or `batch cancel` changed about a run.
#[derive(Debug, serde::Serialize)]
pub struct RunChange<'a> {
    pub run: u64,
    pub change: RunChangeKind,
    /// The node whose unknown grants the replay acknowledged.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub node: Option<&'a str>,
}

/// The change in a [`RunChange`].
#[derive(Debug, Clone, Copy, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RunChangeKind {
    /// Side effects the run had before may repeat.
    ReplayAcknowledged,
    /// Running tasks are being stopped.
    Cancelled,
}

impl std::fmt::Display for RunChange<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let id = self.run;
        match self.change {
            RunChangeKind::ReplayAcknowledged => writeln!(
                f,
                "run {id}: replay acknowledged for {}; previous side effects may repeat",
                self.node.unwrap_or("?")
            ),
            RunChangeKind::Cancelled => writeln!(
                f,
                "task array {id} cancelled: running tasks are being stopped"
            ),
        }
    }
}

/// Show a task array's per-task outcomes (`relish batch results`).
pub async fn batch_results(
    batch_id: u64,
    failed_only: bool,
    limit: usize,
    after: Option<u32>,
    index: Option<u32>,
    format: OutputFormat,
) -> Result<(), RelishError> {
    let results = BunClient::default_local()
        .batch_results_page(batch_id, failed_only, limit, after, index)
        .await?;
    match format {
        OutputFormat::Human => print!("{}", format_task_results(&results)),
        OutputFormat::Json => println!(
            "{}",
            serde_json::to_string_pretty(&results).map_err(RelishError::SerialiseJson)?
        ),
        OutputFormat::Yaml => print!(
            "{}",
            yaml_serde::to_string(&results).map_err(RelishError::SerialiseYaml)?
        ),
    }
    Ok(())
}

/// Print a failed task's kept output (`relish batch logs --index`).
///
/// `-o json` prints `{run, index, output}`; bytes that aren't UTF-8 become
/// U+FFFD, as they do on a terminal.
pub async fn batch_task_logs(
    batch_id: u64,
    index: u32,
    output: OutputFormat,
) -> Result<(), RelishError> {
    let bytes = BunClient::default_local()
        .task_logs(batch_id, index)
        .await?;
    print_output(
        &TaskOutput {
            run: batch_id,
            index,
            output: String::from_utf8_lossy(&bytes).into_owned(),
        },
        output,
    )
}

/// A task's kept output, from `relish batch logs`.
#[derive(Debug, serde::Serialize)]
pub struct TaskOutput {
    pub run: u64,
    pub index: u32,
    /// The first and last 2 KiB the task wrote.
    pub output: String,
}

impl std::fmt::Display for TaskOutput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.output)
    }
}

/// Render task results as a table.
pub fn format_task_results(results: &crate::bun::task_array_api::TaskResults) -> String {
    let mut out = String::new();
    if results.rows.is_empty() {
        out.push_str("no task results\n");
    } else {
        out.push_str(&format!(
            "{:>10}  {:>8}  {:<9}  {:>6}  {:>9}\n",
            "INDEX", "ATTEMPTS", "OUTCOME", "EXIT", "RUN MS"
        ));
    }
    for row in &results.rows {
        let exit = row
            .exit_code
            .map_or_else(|| "-".to_string(), |code| code.to_string());
        let outcome = if row.not_run {
            "not run"
        } else if row.succeeded {
            "succeeded"
        } else {
            "failed"
        };
        out.push_str(&format!(
            "{:>10}  {:>8}  {:<9}  {:>6}  {:>9}\n",
            row.index, row.attempts, outcome, exit, row.run_ms
        ));
    }
    if results.truncated {
        if let Some(cursor) = results.next_after {
            out.push_str(&format!(
                "(next page: relish batch results {} --after {cursor}; repeat your filter)\n",
                results.batch_id
            ));
        } else {
            out.push_str("(more rows exist in this page)\n");
        }
    }
    if !results.unreachable.is_empty() {
        let nodes: Vec<&str> = results.unreachable.iter().map(|n| n.0.as_str()).collect();
        out.push_str(&format!(
            "(couldn't reach {}; their tasks are missing)\n",
            nodes.join(", ")
        ));
    }
    out
}

/// Render a task array's summary from `GET /v1/batch/{id}`.
pub fn format_array_summary(batch_id: u64, summary: &serde_json::Value) -> String {
    let count = |key: &str| summary[key].as_u64().unwrap_or(0);
    let mut out = format!(
        "task array {batch_id} ({} in {}): {}\n",
        summary["name"].as_str().unwrap_or("?"),
        summary["namespace"].as_str().unwrap_or("?"),
        summary["status"].as_str().unwrap_or("?"),
    );
    out.push_str(&format!(
        "  tasks: {} total, {} succeeded, {} failed, {} not run, {} retries\n",
        count("total"),
        count("succeeded"),
        count("failed"),
        count("not_run"),
        count("retried"),
    ));
    let causes: Vec<String> = [
        ("exit_code", "non-zero exit"),
        ("signal", "signal"),
        ("timeout", "timeout"),
        ("unknown", "unknown outcome"),
        ("launch", "launch failure"),
        ("unrecorded", "not recorded across a restart"),
    ]
    .into_iter()
    .filter_map(|(key, label)| {
        let value = summary["retry_causes"][key].as_u64()?;
        (value > 0).then(|| format!("{value} {label}"))
    })
    .collect();
    if !causes.is_empty() {
        out.push_str(&format!("  retry causes: {}\n", causes.join(", ")));
    }
    out.push_str(&format!(
        "  chunks: {} of {} done; {} tasks held by nodes, {} queued\n",
        count("chunks_done"),
        count("chunks"),
        count("held"),
        count("queued"),
    ));
    if let Some(reason) = summary["quota_blocked_reason"].as_str() {
        out.push_str(&format!("  waiting for quota: {reason}\n"));
    }
    if let Some(reason) = summary["unrunnable_reason"].as_str() {
        out.push_str(&format!("  refused: {reason}\n"));
    }
    if let Some(ranges) = summary["failed_indices"].as_array()
        && !ranges.is_empty()
    {
        let shown: Vec<String> = ranges
            .iter()
            .filter_map(|range| {
                let start = range[0].as_u64()?;
                let end = range[1].as_u64()?;
                Some(if start == end {
                    start.to_string()
                } else {
                    format!("{start}-{end}")
                })
            })
            .collect();
        let hidden = count("failed").saturating_sub(
            ranges
                .iter()
                .filter_map(|r| Some(r[1].as_u64()? - r[0].as_u64()? + 1))
                .sum(),
        );
        out.push_str(&format!("  failed indices: {}", shown.join(", ")));
        if hidden > 0 {
            out.push_str(&format!(" and {hidden} more"));
        }
        out.push_str(&format!(
            " (see relish batch results {batch_id} --failed)\n"
        ));
    }
    for node in summary["nodes"].as_array().into_iter().flatten() {
        let name = node["node"].as_str().unwrap_or("?");
        match node["refused"].as_str() {
            Some(reason) => out.push_str(&format!("  {name}: can't run it: {reason}\n")),
            None => out.push_str(&format!(
                "  {name}: {} slots, {} running, {} succeeded, {} failed\n",
                node["slots"].as_u64().unwrap_or(0),
                node["counters"]["running"].as_u64().unwrap_or(0),
                node["counters"]["succeeded"].as_u64().unwrap_or(0),
                node["counters"]["failed"].as_u64().unwrap_or(0),
            )),
        }
    }
    out
}

/// Create a new API token through the agent.
///
/// The agent mints the token, stores its SHA-256 digest in Raft, and
/// returns the plaintext once; this prints it to stdout and never stores
/// it. Needs a reachable agent and an admin credential.
pub async fn token_create(
    request: &super::client::TokenRequest,
    output: OutputFormat,
) -> Result<(), RelishError> {
    token_create_with_client(request, output, &BunClient::default_local()).await
}

/// A token `relish token create` or `token rotate` just issued. `token` is
/// the secret itself: the cluster keeps only its hash, so this is the one
/// time anyone sees it, in JSON as on a terminal.
#[derive(Debug, serde::Serialize)]
pub struct IssuedTokenReport<'a> {
    pub name: &'a str,
    /// The role, for a new token.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub apps: Option<&'a [String]>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub namespaces: Option<&'a [String]>,
    /// The plaintext secret, shown once.
    pub token: &'a str,
    /// Unix seconds; `null` for never.
    pub expires_at: Option<u64>,
    /// After a rotation, until when the old secret works, Unix seconds.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous_valid_until: Option<u64>,
}

impl std::fmt::Display for IssuedTokenReport<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The details went to stderr; stdout carries only the secret, so
        // `TOKEN=$(relish token create ...)` works.
        writeln!(f, "{}", self.token)
    }
}

/// Create a token via the agent so it's persisted in Raft. The token is minted
/// and hashed server-side; the plaintext is returned once and printed to
/// stdout. An unreachable agent is an error (never a silent exit-0), and the
/// role is validated server-side.
async fn token_create_with_client(
    request: &super::client::TokenRequest,
    output: OutputFormat,
    client: &BunClient,
) -> Result<(), RelishError> {
    let issued = client.token_create(request).await?;

    eprintln!("Token created: {}", request.name);
    eprintln!("  Role: {}", request.role);
    if let Some(apps) = &request.apps {
        eprintln!("  Apps: {}", apps.join(","));
    }
    if let Some(namespaces) = &request.namespaces {
        eprintln!("  Namespaces: {}", namespaces.join(","));
    }
    match issued.expires_at {
        Some(at) => eprintln!("  Expires: {}", format_utc(at)),
        None => eprintln!("  Expires: never"),
    }
    eprintln!();
    print_output(
        &IssuedTokenReport {
            name: &request.name,
            role: Some(&request.role),
            apps: request.apps.as_deref(),
            namespaces: request.namespaces.as_deref(),
            token: &issued.token,
            expires_at: issued.expires_at,
            previous_valid_until: None,
        },
        output,
    )
}

/// Give an API token a new secret through the agent.
///
/// The new secret is printed to stdout once. The old one keeps working
/// until the time printed on stderr, so clients can move over first.
pub async fn token_rotate(
    name: &str,
    grace_hours: Option<u64>,
    output: OutputFormat,
) -> Result<(), RelishError> {
    token_rotate_with_client(name, grace_hours, output, &BunClient::default_local()).await
}

async fn token_rotate_with_client(
    name: &str,
    grace_hours: Option<u64>,
    output: OutputFormat,
    client: &BunClient,
) -> Result<(), RelishError> {
    let issued = client.token_rotate(name, grace_hours).await?;

    eprintln!("Token rotated: {name}");
    match issued.previous_valid_until {
        Some(at) => eprintln!("  Old secret works until: {}", format_utc(at)),
        None => eprintln!("  Old secret: stopped working"),
    }
    match issued.expires_at {
        Some(at) => eprintln!("  Expires: {}", format_utc(at)),
        None => eprintln!("  Expires: never"),
    }
    eprintln!();
    print_output(
        &IssuedTokenReport {
            name,
            role: None,
            apps: None,
            namespaces: None,
            token: &issued.token,
            expires_at: issued.expires_at,
            previous_valid_until: issued.previous_valid_until,
        },
        output,
    )
}

/// Print the cluster's age public key, or one namespace's, for encrypting
/// `ENC[AGE:...]` values.
///
/// With no directory, asks the configured cluster (`GET
/// /v1/secret/public-key`) for its active key, so a quickstart user, or
/// anyone after a rotation, gets the key that will actually decrypt. With
/// a directory, reads the security bootstrap `relish init` wrote there,
/// which works offline but only knows the cluster key. With a namespace,
/// asks for that namespace's own key (F05 I4).
pub async fn secret_pubkey(
    dir: Option<&Path>,
    namespace: Option<&str>,
    output: OutputFormat,
) -> Result<(), RelishError> {
    let key = match dir {
        Some(dir) => resolve_secret_pubkey(dir)?,
        None => fetch_secret_pubkey(&BunClient::default_local(), namespace).await?,
    };
    print_output(
        &PublicKey {
            namespace,
            public_key: key,
        },
        output,
    )
}

/// The age public key `relish secret pubkey` found: the cluster's, or a
/// namespace's (`namespace` is then set).
#[derive(Debug, serde::Serialize)]
pub struct PublicKey<'a> {
    pub namespace: Option<&'a str>,
    pub public_key: String,
}

impl std::fmt::Display for PublicKey<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "{}", self.public_key)
    }
}

/// Ask the cluster for its active age public key, or `namespace`'s.
async fn fetch_secret_pubkey(
    client: &BunClient,
    namespace: Option<&str>,
) -> Result<String, RelishError> {
    Ok(client.secret_public_key(namespace).await?.public_key)
}

/// Find the `*-security-bootstrap.json` in `dir` and return its cluster-wide
/// age public key.
fn resolve_secret_pubkey(dir: &Path) -> Result<String, RelishError> {
    use crate::sesame::types::SecurityState;

    // `relish init` writes `{cluster}-security-bootstrap.json` — find it rather
    // than guess the cluster name (the old code read a `sesame-state.json` that
    // was never written, the X7 bug).
    let bootstrap = fs::read_dir(dir)?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .find(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.ends_with("-security-bootstrap.json"))
        })
        .ok_or_else(|| {
            RelishError::InitFailed(format!(
                "no *-security-bootstrap.json found in {} — run `relish init` first",
                dir.display()
            ))
        })?;

    let data = fs::read_to_string(&bootstrap)?;
    let state: SecurityState = serde_json::from_str(&data).map_err(|e| {
        RelishError::InitFailed(format!("failed to parse {}: {e}", bootstrap.display()))
    })?;

    let keypair = state.cluster_age_keypair().ok_or_else(|| {
        RelishError::InitFailed("no cluster-wide age key in security bootstrap".to_string())
    })?;

    Ok(keypair.public_key.clone())
}

/// Encrypt a plaintext value using an age public key.
///
/// Produces an `ENC[AGE:...]` string suitable for embedding in app
/// config env vars. No cluster access required — encryption is a
/// local operation using only the public key.
///
/// `-o json` prints `{"encrypted": "ENC[AGE:...]"}`; the plaintext is never
/// echoed back.
pub fn secret_encrypt(pubkey: &str, value: &str, output: OutputFormat) -> Result<(), RelishError> {
    use crate::sesame::secret::encrypt_secret;

    let encrypted =
        encrypt_secret(value, pubkey).map_err(|e| RelishError::InitFailed(e.to_string()))?;
    print_output(&Encrypted { encrypted }, output)
}

/// An `ENC[AGE:...]` value from `relish secret encrypt`.
#[derive(Debug, serde::Serialize)]
pub struct Encrypted {
    pub encrypted: String,
}

impl std::fmt::Display for Encrypted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "{}", self.encrypted)
    }
}

/// List API tokens via the agent, with every node's last use merged in.
pub async fn token_list(output: OutputFormat) -> Result<(), RelishError> {
    let listing = BunClient::default_local().token_list().await?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    // A member that didn't answer may hold a more recent use; say so on
    // stderr so `-o json` stays parseable. Expiry warnings go there too.
    for warning in &listing.warnings {
        eprintln!("warning: last use incomplete: {warning}");
    }
    for warning in token_expiry_warnings(&listing.tokens, now) {
        eprintln!("warning: {warning}");
    }
    match output {
        OutputFormat::Human => print!("{}", render_token_list(&listing.tokens, now)),
        OutputFormat::Json => println!(
            "{}",
            serde_json::to_string_pretty(&listing).map_err(RelishError::SerialiseJson)?
        ),
        OutputFormat::Yaml => print!(
            "{}",
            yaml_serde::to_string(&listing).map_err(RelishError::SerialiseYaml)?
        ),
    }
    Ok(())
}

/// One line per token that has expired, or expires within
/// [`crate::sesame::token::TOKEN_EXPIRY_WARNING`] of `now`, saying what to do.
/// The tokens `relish mcp` makes are left out: they live a day and it
/// renews them itself, and rotating one only gets it rejected under it.
fn token_expiry_warnings(tokens: &[super::client::TokenSummary], now: u64) -> Vec<String> {
    let horizon = now.saturating_add(crate::sesame::token::TOKEN_EXPIRY_WARNING.as_secs());
    tokens
        .iter()
        .filter(|token| !super::mcp::credentials::made_by_relish_mcp(&token.name))
        .filter_map(|token| {
            let at = token.expires_at.filter(|at| *at <= horizon)?;
            let name = &token.name;
            Some(if at <= now {
                format!(
                    "token {name} has expired; create a replacement, or rotate it with \
                     `relish token rotate {name}` before the expiry sweep removes it"
                )
            } else {
                format!(
                    "token {name} expires in {}; rotate it with `relish token rotate {name}`",
                    format_duration(at - now)
                )
            })
        })
        .collect()
}

/// The `relish token list` table: UTC creation and expiry times, with how
/// long a live token has left, when it was last used and its scope.
///
/// New columns go on the right, so a script that cuts the older ones out
/// by position keeps working.
fn render_token_list(tokens: &[super::client::TokenSummary], now: u64) -> String {
    use std::fmt::Write as _;
    if tokens.is_empty() {
        return "no tokens\n".to_string();
    }
    let mut out = format!(
        "{:<20} {:<12} {:<21} {:<31} {:<21} {}\n",
        "NAME", "ROLE", "CREATED", "EXPIRES", "LAST USED", "SCOPE"
    );
    for token in tokens {
        let expires = match token.expires_at {
            None => "never".to_string(),
            Some(at) if at <= now => format!("{} (expired)", format_utc(at)),
            Some(at) => format!("{} (in {})", format_utc(at), format_duration(at - now)),
        };
        let last_used = token
            .last_used
            .map_or_else(|| "never".to_string(), format_utc);
        // Writing to a String can't fail.
        let _ = writeln!(
            out,
            "{:<20} {:<12} {:<21} {:<31} {:<21} {}",
            token.name,
            token.role,
            format_utc(token.created_at),
            expires,
            last_used,
            render_token_scope(&token.scope),
        );
    }
    out
}

/// A token's scope for the table: `all`, or the apps and namespaces it's
/// confined to.
fn render_token_scope(scope: &super::client::TokenScopeSummary) -> String {
    let mut parts = Vec::new();
    if let Some(apps) = &scope.apps {
        parts.push(format!("apps={}", apps.join(",")));
    }
    if let Some(namespaces) = &scope.namespaces {
        parts.push(format!("namespaces={}", namespaces.join(",")));
    }
    if parts.is_empty() {
        "all".to_string()
    } else {
        parts.join(" ")
    }
}

/// One job definition for `relish jobs --definitions`: its schedule, then
/// its last outcomes, newest first. The outcomes outlive the runs' details.
fn format_definition(row: &serde_json::Value) -> String {
    use std::fmt::Write as _;
    let mut text = format!(
        "{}/{} revision {}: {} tasks, UTC schedule {}\n",
        row["namespace"].as_str().unwrap_or("?"),
        row["name"].as_str().unwrap_or("?"),
        row["revision"],
        row["count"],
        row["cron"]["expression"].as_str().unwrap_or("disabled")
    );
    let history = row["history"].as_array().map(Vec::as_slice).unwrap_or(&[]);
    for outcome in history.iter().rev() {
        let _ = write!(
            text,
            "  run {} finished {}: {}, {} succeeded, {} failed",
            outcome["run"],
            outcome["finished_at_epoch_secs"]
                .as_u64()
                .map_or_else(|| "?".to_string(), format_utc),
            outcome["status"].as_str().unwrap_or("?"),
            outcome["succeeded"],
            outcome["failed"],
        );
        if outcome["not_run"].as_u64().is_some_and(|count| count > 0) {
            let _ = write!(text, ", {} not run", outcome["not_run"]);
        }
        if let Some(index) = outcome["first_failed_index"].as_u64() {
            let _ = write!(text, "; first failure: task {index}");
        }
        text.push('\n');
    }
    text
}

/// Unix seconds as `YYYY-MM-DD HH:MM UTC`; the raw number if out of range.
fn format_utc(unix_seconds: u64) -> String {
    let Ok(at) = i64::try_from(unix_seconds)
        .map_err(|_| ())
        .and_then(|seconds| time::OffsetDateTime::from_unix_timestamp(seconds).map_err(|_| ()))
    else {
        return unix_seconds.to_string();
    };
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02} UTC",
        at.year(),
        u8::from(at.month()),
        at.day(),
        at.hour(),
        at.minute()
    )
}

/// A coarse remaining time: the largest whole unit (`29d`, `5h`, `12m`, `40s`).
fn format_duration(seconds: u64) -> String {
    match seconds {
        s if s >= 86_400 => format!("{}d", s / 86_400),
        s if s >= 3_600 => format!("{}h", s / 3_600),
        s if s >= 60 => format!("{}m", s / 60),
        s => format!("{s}s"),
    }
}

/// Revoke an API token by name via the agent.
pub async fn token_revoke(name: &str, output: OutputFormat) -> Result<(), RelishError> {
    let client = BunClient::default_local();
    let message = client.token_revoke(name).await?;
    print_output(&super::output::Message { message }, output)
}

/// Mint a short-lived, single-use node join token through the council.
/// The plaintext is shown once, in JSON as on a terminal.
pub async fn join_token_create(
    node_id: &str,
    ttl_seconds: u64,
    output: OutputFormat,
) -> Result<(), RelishError> {
    let client = BunClient::default_local();
    let plaintext = client.join_token_create(node_id, ttl_seconds).await?;
    eprintln!(
        "Join token created for {node_id} (expires in {}).",
        format_ttl(ttl_seconds)
    );
    eprintln!("The joining node must enrol with `relish join --node-id {node_id}`.");
    eprintln!("The plaintext is shown once:");
    print_output(
        &JoinTokenReport {
            node_id,
            ttl_secs: ttl_seconds,
            token: &plaintext,
        },
        output,
    )
}

/// A join token `relish join-token create` minted. `token` is the secret:
/// the council keeps only its hash.
#[derive(Debug, serde::Serialize)]
pub struct JoinTokenReport<'a> {
    /// The only node id the token enrols.
    pub node_id: &'a str,
    pub ttl_secs: u64,
    /// The plaintext, shown once.
    pub token: &'a str,
}

impl std::fmt::Display for JoinTokenReport<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "{}", self.token)
    }
}

/// List the council's join tokens.
pub async fn join_token_list() -> Result<(), RelishError> {
    let tokens = BunClient::default_local().join_token_list().await?;
    print!(
        "{}",
        render_join_tokens(&tokens, std::time::SystemTime::now())
    );
    Ok(())
}

/// Revoke a node id's unused join tokens.
pub async fn join_token_revoke(node_id: &str) -> Result<(), RelishError> {
    let revoked = BunClient::default_local()
        .join_token_revoke(node_id)
        .await?;
    match revoked {
        0 => println!("{node_id} had no unused join tokens"),
        1 => println!("revoked 1 join token for {node_id}"),
        n => println!("revoked {n} join tokens for {node_id}"),
    }
    Ok(())
}

/// The `relish join-token list` table: one row per token, its state
/// relative to `now`.
fn render_join_tokens(
    tokens: &[crate::sesame::join::JoinTokenSummary],
    now: std::time::SystemTime,
) -> String {
    if tokens.is_empty() {
        return "no join tokens\n".to_string();
    }
    let now = now
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let width = tokens
        .iter()
        .map(|t| t.node_id.len())
        .max()
        .unwrap_or(0)
        .max(4);
    let mut out = format!("{:<10} {:<width$} STATE\n", "ID", "NODE");
    for token in tokens {
        let state = if token.consumed {
            "used".to_string()
        } else if token.expires_at <= now {
            "expired".to_string()
        } else {
            format!("valid for {}", format_ttl_rounded(token.expires_at - now))
        };
        out.push_str(&format!(
            "{:<10} {:<width$} {state}\n",
            token.id, token.node_id
        ));
    }
    out
}

/// A duration as its largest whole unit: 6d, 3h, 14m or 40s.
fn format_ttl_rounded(seconds: u64) -> String {
    match seconds {
        s if s >= 86_400 => format!("{}d", s / 86_400),
        s if s >= 3_600 => format!("{}h", s / 3_600),
        s if s >= 60 => format!("{}m", s / 60),
        s => format!("{s}s"),
    }
}

fn format_ttl(seconds: u64) -> String {
    if seconds.is_multiple_of(3600) {
        format!("{}h", seconds / 3600)
    } else if seconds.is_multiple_of(60) {
        format!("{}m", seconds / 60)
    } else {
        format!("{seconds}s")
    }
}

/// Snapshot an app's managed volumes.
pub async fn snapshot_create(
    app: &str,
    namespace: &str,
    volume: Option<&str>,
    name: Option<&str>,
    output: OutputFormat,
) -> Result<(), RelishError> {
    let client = BunClient::default_local();
    let metas = client.snapshot_create(app, namespace, volume, name).await?;
    if output != OutputFormat::Human {
        println!("{}", format_output(&metas, output)?);
        return Ok(());
    }
    if let Some(list) = metas.as_array() {
        for meta in list {
            println!(
                "created snapshot {} of {} ({} bytes)",
                meta["name"].as_str().unwrap_or("?"),
                meta["volume_path"].as_str().unwrap_or("?"),
                meta["size_bytes"].as_u64().unwrap_or(0),
            );
        }
    }
    Ok(())
}

/// List an app's snapshots, newest first.
///
/// `-o json` prints the node's list of snapshot records.
pub async fn snapshot_list(
    app: &str,
    namespace: &str,
    output: OutputFormat,
) -> Result<(), RelishError> {
    let client = BunClient::default_local();
    let metas = client.snapshot_list(app, namespace).await?;
    if output != OutputFormat::Human {
        println!("{}", format_output(&metas, output)?);
        return Ok(());
    }
    let Some(list) = metas.as_array() else {
        println!("no snapshots");
        return Ok(());
    };
    if list.is_empty() {
        println!("no snapshots for {namespace}/{app}");
        return Ok(());
    }
    println!(
        "{:<24} {:<12} {:>12}  EXPORTED TO",
        "NAME", "VOLUME", "SIZE"
    );
    for meta in list {
        let destinations: Vec<&str> = meta["exports"]
            .as_array()
            .map(|exports| {
                exports
                    .iter()
                    .filter_map(|receipt| receipt["destination"].as_str())
                    .collect()
            })
            .unwrap_or_default();
        println!(
            "{:<24} {:<12} {:>12}  {}",
            meta["name"].as_str().unwrap_or("?"),
            meta["volume_path"].as_str().unwrap_or("?"),
            meta["size_bytes"].as_u64().unwrap_or(0),
            if destinations.is_empty() {
                "-".to_string()
            } else {
                destinations.join(", ")
            },
        );
    }
    Ok(())
}

/// Restore a snapshot over its live volume (stop the app first).
/// `volume` picks between volumes that share the snapshot name.
pub async fn snapshot_restore(
    app: &str,
    namespace: &str,
    name: &str,
    volume: Option<&str>,
    output: OutputFormat,
) -> Result<(), RelishError> {
    let client = BunClient::default_local();
    client
        .snapshot_restore(app, namespace, name, volume)
        .await?;
    print_output(
        &SnapshotChange {
            change: SnapshotChangeKind::Restored,
            app,
            namespace,
            snapshot: name,
            volume,
        },
        output,
    )
}

/// What `relish snapshot restore` or `snapshot delete` did.
#[derive(Debug, serde::Serialize)]
pub struct SnapshotChange<'a> {
    pub change: SnapshotChangeKind,
    pub app: &'a str,
    pub namespace: &'a str,
    pub snapshot: &'a str,
    /// The volume, when `--volume` named one.
    pub volume: Option<&'a str>,
}

/// The change in a [`SnapshotChange`].
#[derive(Debug, Clone, Copy, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SnapshotChangeKind {
    Restored,
    Deleted,
}

impl std::fmt::Display for SnapshotChange<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (app, namespace, name) = (self.app, self.namespace, self.snapshot);
        match self.change {
            SnapshotChangeKind::Restored => {
                writeln!(f, "restored {namespace}/{app} from snapshot {name}")
            }
            SnapshotChangeKind::Deleted => {
                writeln!(f, "deleted snapshot {name} of {namespace}/{app}")
            }
        }
    }
}

/// Delete a snapshot. `volume` picks between volumes that share the
/// snapshot name.
pub async fn snapshot_delete(
    app: &str,
    namespace: &str,
    name: &str,
    volume: Option<&str>,
    output: OutputFormat,
) -> Result<(), RelishError> {
    let client = BunClient::default_local();
    client.snapshot_delete(app, namespace, name, volume).await?;
    print_output(
        &SnapshotChange {
            change: SnapshotChangeKind::Deleted,
            app,
            namespace,
            snapshot: name,
            volume,
        },
        output,
    )
}

#[cfg(test)]
mod tests {
    /// #1499: the summary names the node the logs came from and where they
    /// landed, so nobody looks for an archive on the wrong machine.
    #[test]
    fn log_export_summary_names_the_node_and_where_the_files_landed() {
        use super::{ExportLanding, render_export_summary};
        let here = ExportLanding::parse("./archive").unwrap();
        let bucket = ExportLanding::parse("s3://bucket/logs/").unwrap();
        let lines = [
            render_export_summary(&here, "node-1", 2, 4096),
            render_export_summary(&here, "node-1", 0, 0),
            render_export_summary(&bucket, "node-1", 2, 4096),
            render_export_summary(&bucket, "node-1", 0, 0),
        ];
        insta::assert_snapshot!(lines.join("\n"), @r"
        exported 2 file(s) (4096 bytes) of node-1's logs to ./archive/node-1 on this machine
        no new files to export from node-1 to ./archive/node-1 on this machine
        exported 2 file(s) (4096 bytes) of node-1's logs to s3://bucket/logs/node-1
        no new files to export from node-1 to s3://bucket/logs/node-1
        ");
    }

    /// A bare path and `file://` are this machine's disk; every other scheme
    /// is an object store the node writes to.
    #[test]
    fn export_destination_is_this_machine_unless_it_names_an_object_store() {
        use super::ExportLanding;
        let here = |directory: &str| ExportLanding::ThisMachine {
            directory: directory.into(),
        };
        assert_eq!(
            ExportLanding::parse("./archive").unwrap(),
            here("./archive")
        );
        assert_eq!(
            ExportLanding::parse("/srv/logs").unwrap(),
            here("/srv/logs")
        );
        assert_eq!(
            ExportLanding::parse("file:///srv/logs").unwrap(),
            here("/srv/logs")
        );
        assert_eq!(
            ExportLanding::parse("gs://bucket/logs").unwrap(),
            ExportLanding::ObjectStore {
                url: "gs://bucket/logs".into()
            }
        );
        assert!(ExportLanding::parse("file://other-host/srv/logs").is_err());
    }

    fn sha256_hex(bytes: &[u8]) -> String {
        use sha2::{Digest, Sha256};
        hex::encode(Sha256::digest(bytes))
    }

    /// An agent that lists `listing` and serves `body` for `logs_1.parquet`,
    /// or nothing for any other name.
    async fn canned_archive_agent(
        listing: serde_json::Value,
        body: &'static [u8],
    ) -> (super::BunClient, tokio::task::JoinHandle<()>) {
        use axum::routing::get;
        let router = axum::Router::new()
            .route(
                "/v1/logs/archive",
                get(move || {
                    let listing = listing.clone();
                    async move { axum::Json(listing) }
                }),
            )
            .route(
                "/v1/logs/archive/logs_1.parquet",
                get(move || async move { body }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let client = super::BunClient::new_with_token(&format!("http://{address}"), None);
        (client, server)
    }

    fn listing_of(node_id: &str, name: &str, size: usize, sha256: &str) -> serde_json::Value {
        serde_json::json!({
            "node_id": node_id,
            "files": [{"name": name, "size": size, "sha256": sha256}],
        })
    }

    fn names_under(directory: &std::path::Path) -> Vec<String> {
        std::fs::read_dir(directory)
            .map(|entries| {
                entries
                    .map(|entry| entry.unwrap().file_name().into_string().unwrap())
                    .collect()
            })
            .unwrap_or_default()
    }

    #[tokio::test]
    async fn archive_copy_files_each_log_under_its_hash_and_skips_it_next_time() {
        let sha256 = sha256_hex(b"parquet");
        let listing = listing_of("node-1", "logs_1.parquet", 7, &sha256);
        let (client, server) = canned_archive_agent(listing, b"parquet").await;
        let dest = tempfile::tempdir().unwrap();

        let first = super::copy_node_archive(&client, dest.path())
            .await
            .unwrap();
        let second = super::copy_node_archive(&client, dest.path())
            .await
            .unwrap();
        server.abort();

        assert_eq!((first.files, first.bytes), (1, 7));
        assert_eq!((second.files, second.bytes), (0, 0));
        let path = dest
            .path()
            .join("node-1")
            .join(format!("{sha256}-logs_1.parquet"));
        assert_eq!(std::fs::read(path).unwrap(), b"parquet");
        assert_eq!(names_under(&dest.path().join("node-1")).len(), 1);
    }

    /// A node name or file name from the agent never climbs out of `--dest`.
    #[tokio::test]
    async fn archive_copy_refuses_names_that_leave_the_destination() {
        let sha256 = sha256_hex(b"parquet");
        let hostile = [
            listing_of("../outside", "logs_1.parquet", 7, &sha256),
            listing_of("/tmp/outside", "logs_1.parquet", 7, &sha256),
            listing_of("..", "logs_1.parquet", 7, &sha256),
            listing_of("node-1", "../logs_1.parquet", 7, &sha256),
            listing_of("node-1", "logs_1.parquet", 7, "../../outside"),
        ];
        for listing in hostile {
            let (client, server) = canned_archive_agent(listing.clone(), b"parquet").await;
            let parent = tempfile::tempdir().unwrap();
            let dest = parent.path().join("archive");
            let outcome = super::copy_node_archive(&client, &dest).await;
            server.abort();
            assert!(outcome.is_err(), "{listing}");
            assert!(names_under(&dest.join("node-1")).is_empty(), "{listing}");
            assert_eq!(names_under(parent.path()).len(), usize::from(dest.exists()));
        }
    }

    /// Bytes that don't match the listing never take the archive name, and
    /// no partial file stays behind.
    #[tokio::test]
    async fn archive_copy_refuses_bytes_that_do_not_match_the_listing() {
        for (size, body) in [
            (7, b"tampers".as_slice()),
            (7, b"short".as_slice()),
            (3, b"too long".as_slice()),
        ] {
            let listing = listing_of("node-1", "logs_1.parquet", size, &sha256_hex(b"parquet"));
            let (client, server) = canned_archive_agent(listing, body).await;
            let dest = tempfile::tempdir().unwrap();
            let error = super::copy_node_archive(&client, dest.path())
                .await
                .unwrap_err();
            server.abort();
            assert!(
                error.to_string().contains("run the export again"),
                "{error}"
            );
            assert!(names_under(&dest.path().join("node-1")).is_empty());
        }
    }

    /// Retention can remove a file after the listing: that's not a failure.
    #[tokio::test]
    async fn archive_copy_skips_a_file_retention_removed() {
        let listing = listing_of("node-1", "logs_2.parquet", 7, &sha256_hex(b"parquet"));
        let (client, server) = canned_archive_agent(listing, b"parquet").await;
        let dest = tempfile::tempdir().unwrap();
        let copied = super::copy_node_archive(&client, dest.path())
            .await
            .unwrap();
        server.abort();
        assert_eq!((copied.files, copied.bytes), (0, 0));
    }

    #[test]
    fn history_table_names_the_node_of_every_entry() {
        let at = std::time::SystemTime::UNIX_EPOCH;
        let entry = |node: &str, id: u64| crate::bun::cluster_view::NodeTagged {
            node: node.to_string(),
            row: crate::meat::deploy_types::DeployHistoryEntry {
                id: crate::meat::deploy_types::DeployId(id),
                app_id: crate::meat::types::AppId::new("web", "default"),
                image: String::new(),
                result: crate::meat::deploy_types::DeployResult::Completed,
                created_at: at,
                completed_at: at,
                steps_completed: 1,
                steps_total: 1,
                spec: None,
            },
        };
        let table = super::render_history_table(&[entry("node-1", 7), entry("node-2", 7)]);
        let rows: Vec<Vec<&str>> = table
            .lines()
            .map(|line| line.split_whitespace().collect())
            .collect();
        assert_eq!(rows[0], ["ID", "NODE", "IMAGE", "RESULT", "DONE", "TOTAL"]);
        assert_eq!(rows[1], ["7", "node-1", "-", "Completed", "1", "1"]);
        assert_eq!(rows[2], ["7", "node-2", "-", "Completed", "1", "1"]);
    }

    #[test]
    fn join_token_table_shows_used_expired_and_remaining_time() {
        use crate::sesame::join::JoinTokenSummary;
        let now = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000);
        let token = |id: &str, node: &str, expires_at: u64, consumed: bool| JoinTokenSummary {
            id: id.into(),
            node_id: node.into(),
            expires_at,
            consumed,
        };
        let table = render_join_tokens(
            &[
                token("aaaaaaaa", "node-02", 999_000, true),
                token("bbbbbbbb", "node-03", 999_000, false),
                token("cccccccc", "home-10", 1_000_000 + 6 * 86_400 + 5, false),
            ],
            now,
        );
        assert_eq!(
            table,
            "ID         NODE    STATE\n\
             aaaaaaaa   node-02 used\n\
             bbbbbbbb   node-03 expired\n\
             cccccccc   home-10 valid for 6d\n"
        );
        assert_eq!(render_join_tokens(&[], now), "no join tokens\n");
    }

    #[test]
    fn build_plan_shows_the_destination_and_build_but_no_push() {
        let spec = crate::config::build::BuildSpec {
            context: ".".into(),
            dockerfile: "Dockerfile".into(),
            destination: "pickle://burger:v1".into(),
            args: Default::default(),
            namespace: None,
            platform: vec!["linux/amd64".into(), "linux/arm64".into()],
        };
        let job = crate::pickle::build::execute_build(&spec, "sha256:abc", None).unwrap();
        let lines = build_plan_lines(&job);
        assert_eq!(lines[0], "  destination: pickle://burger:v1");
        assert!(lines[1].starts_with("  build:  buildah bud"), "{lines:?}");
        // The node exports an OCI layout and uploads it; it never runs a
        // `buildah push` to `docker://`, so the plan doesn't claim one.
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert!(lines.iter().all(|line| !line.contains("push")), "{lines:?}");
    }

    use super::*;
    use std::io::Write as _;

    /// Z6.7: applying podinfo to the laptop cluster ended with
    /// "deployed 4 instance(s):" and nothing after the colon.
    #[test]
    fn a_cluster_apply_says_the_apps_are_being_placed() {
        assert_eq!(
            apply_summary(4, &[]),
            "applied 4 app(s); the scheduler places them now (watch with `relish apps`)"
        );
        assert_eq!(
            apply_summary(2, &["default__web-0".into(), "default__web-1".into()]),
            "deployed 2 instance(s): default__web-0, default__web-1"
        );
    }

    fn top_row(
        node: &str,
        id: &str,
        pid: Option<u32>,
        cpu: Option<f64>,
        memory: Option<u64>,
    ) -> crate::bun::top::TopRow {
        crate::bun::top::TopRow {
            node: node.to_string(),
            instance: crate::bun::agent::InstanceStatus {
                id: id.to_string(),
                app_name: "podinfo".to_string(),
                namespace: "default".to_string(),
                state: "running".to_string(),
                restart_count: u32::from(node == "rb-3"),
                host_port: None,
                exit_code: None,
                pid,
                runtime_unknown: false,
                status_age_ms: None,
                age_seconds: None,
                reason: None,
            },
            cpu_percent: cpu,
            memory_bytes: memory,
            tasks: None,
        }
    }

    #[test]
    fn job_definitions_list_their_last_outcomes_newest_first() {
        let text = format_definition(&serde_json::json!({
            "namespace": "ops", "name": "nightly", "revision": 2, "count": 1,
            "cron": {"expression": "0 3 * * *"},
            "history": [
                {"run": 40, "finished_at_epoch_secs": 1_791_601_200u64, "status": "Succeeded",
                 "succeeded": 1, "failed": 0, "not_run": 0},
                {"run": 41, "finished_at_epoch_secs": 1_791_687_600u64, "status": "CompletedWithFailures",
                 "succeeded": 0, "failed": 1, "not_run": 0, "first_failed_index": 0},
            ],
        }));
        insta::assert_snapshot!(text);
    }

    #[test]
    fn top_shows_a_job_row_with_its_running_task_count() {
        let mut job = top_row("rb-2", "tasks", None, Some(250.0), Some(8 << 20));
        job.instance.app_name = "render".into();
        job.tasks = Some(27);
        let table = render_top(&[job]);
        let line = table.lines().nth(1).unwrap();
        let columns: Vec<&str> = line.split_whitespace().collect();
        assert_eq!(columns[1], "render", "{table}");
        assert_eq!(columns[4], "tasks:27", "{table}");
        assert_eq!(columns[6], "250.0%", "{table}");
    }

    #[test]
    fn top_lists_every_node_with_cpu_and_memory() {
        insta::assert_snapshot!(render_top(&[
            top_row(
                "rb-0123456789ab-1",
                "default__podinfo-0",
                Some(2311),
                Some(3.4),
                Some(24_117_248)
            ),
            top_row(
                "rb-2",
                "default__podinfo-0",
                Some(2290),
                Some(0.0),
                Some(900)
            ),
            top_row("rb-3", "default__podinfo-0", None, None, None),
        ]));
    }

    #[test]
    fn top_marks_a_pid_the_runtime_did_not_report_in_time() {
        let mut busy = top_row("rb-2", "default__podinfo-1", None, None, None);
        busy.instance.runtime_unknown = true;
        let table = render_top(&[
            top_row("rb-1", "default__podinfo-0", None, None, None),
            busy,
        ]);
        let pid_column = |line: &str| line.split_whitespace().nth(4).map(str::to_string);
        let lines: Vec<&str> = table.lines().collect();
        assert_eq!(pid_column(lines[1]).as_deref(), Some("-"), "{table}");
        assert_eq!(pid_column(lines[2]).as_deref(), Some("?"), "{table}");
    }

    #[test]
    fn top_says_so_when_nothing_runs() {
        assert_eq!(render_top(&[]), "no workloads running\n");
    }

    #[test]
    fn memory_uses_binary_units() {
        assert_eq!(format_memory(512), "512 B");
        assert_eq!(format_memory(1536), "1.5 KiB");
        assert_eq!(format_memory(24_117_248), "23.0 MiB");
        assert_eq!(format_memory(3 * 1024 * 1024 * 1024), "3.0 GiB");
    }

    #[tokio::test]
    async fn manifest_loading_defers_namespace_existence_to_live_admission() {
        for manifest in [
            "[permission.reader]\nactions = ['logs']\napps = ['web']\nnamespaces = ['existing']\n",
            "[build.web]\ncontext = '.'\ndestination = 'pickle://existing/web:v1'\nnamespace = 'existing'\n",
        ] {
            let file = write_temp_config(manifest);
            let loaded = load_manifest(&source(file.path())).await;
            assert!(
                loaded.is_ok(),
                "a live-context reference was rejected locally: {loaded:?}"
            );
        }
    }

    #[tokio::test]
    async fn manifest_loading_defers_build_namespace_existence_to_live_admission() {
        let file = write_temp_config(
            "[build.web]\ncontext = '.'\ndestination = 'pickle://existing/web:v1'\nnamespace = 'existing'\n",
        );
        let loaded = load_manifest(&source(file.path())).await;
        assert!(
            loaded.is_ok(),
            "an existing live build namespace was rejected locally: {loaded:?}"
        );
    }

    #[tokio::test]
    async fn intrinsic_manifest_validation_still_refuses_invalid_fields() {
        for manifest in [
            "[permission.reader]\nactions = ['teleport']\napps = ['web']\nnamespaces = ['existing']\n",
            "[permission.reader]\nactions = ['logs']\napps = ['web']\nnamespaces = ['Existing']\n",
            "[build.web]\ncontext = '.'\ndestination = 'pickle://Existing/web:v1'\nnamespace = 'Existing'\n",
            "[app.web]\nnamespace = 'existing'\n",
        ] {
            let file = write_temp_config(manifest);
            assert!(
                load_manifest(&source(file.path())).await.is_err(),
                "accepted {manifest}"
            );
        }
    }

    #[tokio::test]
    async fn apply_dry_run_refuses_a_failed_live_comparison() {
        let app = axum::Router::new()
            .route(
                "/v1/health",
                axum::routing::get(|| async { axum::Json(serde_json::json!({"status": "ok"})) }),
            )
            .route(
                "/v1/apps",
                axum::routing::get(|| async { axum::http::StatusCode::SERVICE_UNAVAILABLE }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let client =
            BunClient::new_with_token(&format!("http://{}", listener.local_addr().unwrap()), None);
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        client
            .health()
            .await
            .expect("fixture must be a live bun response");
        let file = write_temp_config("[app.web]\nimage = 'web:v1'\n");
        let result =
            apply_with_client(&source(file.path()), OutputFormat::Json, true, &client).await;
        server.abort();
        assert!(
            result.is_err(),
            "a failed live comparison became an offline create plan"
        );
    }

    #[tokio::test]
    async fn join_token_file_rejects_exposed_empty_and_oversized_credentials() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("token");
        crate::sesame::identity::atomic_write_mode(&path, b"one-time-token\n", Some(0o600))
            .unwrap();
        assert_eq!(read_join_token(&path).await.unwrap(), "one-time-token");
        for invalid in [String::new(), "two tokens".into(), "x".repeat(4097)] {
            std::fs::write(&path, invalid).unwrap();
            assert!(read_join_token(&path).await.is_err());
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::write(&path, "secret").unwrap();
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
            assert!(read_join_token(&path).await.is_err());
        }
    }

    /// Port 1 on localhost — nothing listens there, so connections
    /// are refused immediately without waiting for a timeout.
    fn bogus_client() -> BunClient {
        BunClient::new("http://127.0.0.1:1")
    }

    #[cfg(feature = "kubernetes")]
    pub(super) const K8S_DEPLOYMENT: &str = "apiVersion: apps/v1\nkind: Deployment\nmetadata:\n  name: web\nspec:\n  template:\n    spec:\n      containers:\n      - name: web\n        image: nginx:1\n";

    /// `lint`, `diff` and `compile` read Kubernetes YAML the way `apply`
    /// does, through `manifest::parse`.
    #[cfg(feature = "kubernetes")]
    #[test]
    fn local_config_commands_accept_kubernetes_yaml() {
        let file = write_temp_config(K8S_DEPLOYMENT);
        lint(file.path(), OutputFormat::Human).unwrap();
        diff(file.path(), None, OutputFormat::Human).unwrap();
        compile(file.path(), OutputFormat::Human).unwrap();
        let config = load_config_file(file.path()).unwrap();
        assert!(config.app.contains_key("web"));
    }

    fn service(namespace: &str) -> crate::onion::types::ResolveResponse {
        crate::onion::types::ResolveResponse {
            app_name: "redis".into(),
            namespace: namespace.into(),
            vip: "127.128.0.2".into(),
            port: 6379,
            healthy_backends: 1,
            total_backends: 1,
            backends: Vec::new(),
        }
    }

    #[test]
    fn resolve_picks_the_named_namespace() {
        let services = vec![service("prod"), service("staging")];
        let found = select_service(services, "redis", Some("staging")).unwrap();
        assert_eq!(found.namespace, "staging");
    }

    #[test]
    fn resolve_refuses_a_name_in_several_namespaces() {
        let services = vec![service("prod"), service("staging")];
        let message = select_service(services, "redis", None)
            .unwrap_err()
            .to_string();
        assert!(
            message.contains("prod")
                && message.contains("staging")
                && message.contains("--namespace"),
            "{message}"
        );
    }

    #[test]
    fn resolve_without_a_namespace_takes_the_only_match() {
        let found = select_service(vec![service("prod")], "redis", None).unwrap();
        assert_eq!(found.namespace, "prod");
        assert!(select_service(vec![service("prod")], "redis", Some("dev")).is_err());
    }

    #[test]
    fn resolve_prints_the_namespace() {
        let shown = ResolvedService(service("prod")).to_string();
        assert!(shown.contains("Namespace: prod"), "{shown}");
    }

    #[test]
    fn exec_passes_the_remote_exit_code_through() {
        use crate::relish::CommandOutcome;
        assert_eq!(exec_outcome(0), CommandOutcome::Clean);
        assert_eq!(exec_outcome(3), CommandOutcome::Exited(3));
        assert_eq!(exec_outcome(255), CommandOutcome::Exited(255));
        // Outside a process exit code's range: still a failure, never 0.
        assert_eq!(exec_outcome(256), CommandOutcome::Problems);
        assert_eq!(exec_outcome(-1), CommandOutcome::Problems);
    }

    #[test]
    fn secret_pubkey_prints_age_key_from_bootstrap() {
        let dir = tempfile::tempdir().unwrap();
        // `relish init` writes the bootstrap file from the init result; mimic it.
        let init = crate::sesame::init::initialize_cluster("pubkeytest", "node-1").unwrap();
        let bootstrap = dir.path().join("pubkeytest-security-bootstrap.json");
        fs::write(
            &bootstrap,
            serde_json::to_string(&init.security_state).unwrap(),
        )
        .unwrap();

        let key = resolve_secret_pubkey(dir.path()).unwrap();
        assert_eq!(key, init.age_public_key);
        assert!(key.starts_with("age1"), "got {key}");
    }

    #[tokio::test]
    async fn secret_pubkey_fetches_active_key_from_cluster() {
        use axum::{Router, http::HeaderMap, routing::get};
        type Params = axum::extract::Query<std::collections::HashMap<String, String>>;
        let app = Router::new().route(
            "/v1/secret/public-key",
            get(|headers: HeaderMap, params: Params| async move {
                // The command must send the usual bearer token.
                let authorised = headers.get("authorization").and_then(|v| v.to_str().ok())
                    == Some("Bearer rbt_test");
                if !authorised {
                    return Err(axum::http::StatusCode::UNAUTHORIZED);
                }
                let public_key = match params.get("namespace") {
                    Some(namespace) => format!("age1{namespace}key"),
                    None => "age1quickstartkey".to_string(),
                };
                Ok(axum::Json(serde_json::json!({
                    "public_key": public_key,
                    "generation": 2,
                })))
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let base = format!("http://{address}");
        let client = BunClient::new_with_token(&base, Some("rbt_test"));
        let key = fetch_secret_pubkey(&client, None).await;
        let team_a = fetch_secret_pubkey(&client, Some("team-a")).await;
        let anonymous = fetch_secret_pubkey(&BunClient::new_with_token(&base, None), None).await;
        server.abort();
        assert_eq!(key.unwrap(), "age1quickstartkey");
        assert_eq!(team_a.unwrap(), "age1team-akey");
        assert!(anonymous.is_err(), "an HTTP 401 must surface as an error");
    }

    #[tokio::test]
    async fn secret_pubkey_errors_when_cluster_unreachable() {
        assert!(fetch_secret_pubkey(&bogus_client(), None).await.is_err());
    }

    #[test]
    fn secret_pubkey_errors_without_bootstrap_file() {
        let dir = tempfile::tempdir().unwrap();
        let result = resolve_secret_pubkey(dir.path());
        assert!(result.is_err(), "missing bootstrap must error");
    }

    #[test]
    fn token_list_renders_human_times_expiry_last_use_and_scope() {
        use super::super::client::{TokenScopeSummary, TokenSummary};
        // 2026-09-25 12:00:00 UTC.
        let now = 1_790_337_600;
        let tokens = vec![
            TokenSummary {
                name: "ci-bot".to_string(),
                role: "deployer".to_string(),
                scope: TokenScopeSummary {
                    apps: None,
                    namespaces: Some(vec!["shop".to_string()]),
                },
                created_at: now - 86_400,
                expires_at: Some(now + 30 * 86_400),
                last_used: Some(now - 600),
            },
            TokenSummary {
                name: "admin".to_string(),
                role: "admin".to_string(),
                scope: TokenScopeSummary::default(),
                created_at: now - 3 * 86_400,
                expires_at: None,
                last_used: Some(now),
            },
            TokenSummary {
                name: "old-reader".to_string(),
                role: "read-only".to_string(),
                scope: TokenScopeSummary {
                    apps: Some(vec!["web".to_string(), "api".to_string()]),
                    namespaces: Some(vec!["shop".to_string()]),
                },
                created_at: now - 90 * 86_400,
                expires_at: Some(now - 3_600),
                last_used: None,
            },
            TokenSummary {
                name: "short".to_string(),
                role: "read-only".to_string(),
                scope: TokenScopeSummary::default(),
                created_at: now,
                expires_at: Some(now + 5_400),
                last_used: None,
            },
        ];
        insta::assert_snapshot!(render_token_list(&tokens, now), @r"
        NAME                 ROLE         CREATED               EXPIRES                         LAST USED             SCOPE
        ci-bot               deployer     2026-09-24 12:00 UTC  2026-10-25 12:00 UTC (in 30d)   2026-09-25 11:50 UTC  namespaces=shop
        admin                admin        2026-09-22 12:00 UTC  never                           2026-09-25 12:00 UTC  all
        old-reader           read-only    2026-06-27 12:00 UTC  2026-09-25 11:00 UTC (expired)  never                 apps=web,api namespaces=shop
        short                read-only    2026-09-25 12:00 UTC  2026-09-25 13:30 UTC (in 1h)    never                 all
        ");
    }

    /// An older agent's answer has no scope or last use; it still parses,
    /// as unscoped and never used.
    #[test]
    fn token_listing_parses_an_answer_without_scope_or_last_use() {
        let listing: super::super::client::TokenListing = serde_json::from_str(
            r#"{"tokens":[{"name":"a","role":"admin","created_at":1,"expires_at":null}]}"#,
        )
        .unwrap();
        assert_eq!(listing.tokens[0].last_used, None);
        assert_eq!(listing.tokens[0].scope, Default::default());
        assert!(listing.warnings.is_empty());
    }

    #[test]
    fn token_list_says_so_when_empty() {
        assert_eq!(render_token_list(&[], 0), "no tokens\n");
    }

    #[test]
    fn token_list_warns_about_tokens_expiring_within_fourteen_days() {
        use super::super::client::{TokenScopeSummary, TokenSummary};
        let now = 1_790_337_600;
        let summary = |name: &str, expires_at: Option<u64>| TokenSummary {
            name: name.to_string(),
            role: "deployer".to_string(),
            scope: TokenScopeSummary::default(),
            created_at: now - 80 * 86_400,
            expires_at,
            last_used: None,
        };
        let tokens = [
            summary("soon", Some(now + 3 * 86_400)),
            summary("edge", Some(now + 14 * 86_400)),
            summary("later", Some(now + 15 * 86_400)),
            summary("forever", None),
            summary("gone", Some(now - 60)),
        ];
        assert_eq!(
            token_expiry_warnings(&tokens, now),
            [
                "token soon expires in 3d; rotate it with `relish token rotate soon`",
                "token edge expires in 14d; rotate it with `relish token rotate edge`",
                "token gone has expired; create a replacement, or rotate it with \
                 `relish token rotate gone` before the expiry sweep removes it",
            ]
        );
    }

    /// The tokens `relish mcp` makes live a day and renew themselves, so
    /// they'd always be in the window, and rotating one only gets it
    /// rejected under `relish mcp` (#1763 review).
    #[test]
    fn token_list_doesnt_warn_about_the_tokens_relish_mcp_makes() {
        use super::super::client::{TokenScopeSummary, TokenSummary};
        let now = 1_790_337_600;
        let summary = |name: &str, expires_at: u64| TokenSummary {
            name: name.to_string(),
            role: "read-only".to_string(),
            scope: TokenScopeSummary::default(),
            created_at: now - 3_600,
            expires_at: Some(expires_at),
            last_used: None,
        };
        let tokens = [
            summary("relish-mcp-1790334000-4242", now + 23 * 3_600),
            summary("relish-mcp-1790000000-17", now - 60),
            summary("agent", now + 23 * 3_600),
        ];
        assert_eq!(
            token_expiry_warnings(&tokens, now),
            ["token agent expires in 23h; rotate it with `relish token rotate agent`"]
        );
    }

    #[tokio::test]
    async fn token_create_errors_when_agent_unreachable() {
        let request = super::super::client::TokenRequest {
            name: "ci-bot".to_string(),
            role: "deployer".to_string(),
            apps: None,
            namespaces: None,
            lifetime: super::super::client::TokenLifetime::Default,
            inherit_permissions: false,
            agent: false,
        };
        let result = token_create_with_client(&request, OutputFormat::Human, &bogus_client()).await;
        assert!(result.is_err(), "unreachable agent must be an error");
    }

    #[tokio::test]
    async fn token_rotate_errors_when_agent_unreachable() {
        let result =
            token_rotate_with_client("ci-bot", None, OutputFormat::Human, &bogus_client()).await;
        assert!(result.is_err(), "unreachable agent must be an error");
    }

    pub(super) fn write_temp_config(content: &str) -> tempfile::NamedTempFile {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(content.as_bytes()).unwrap();
        f
    }

    fn source(path: &Path) -> crate::relish::manifest::ManifestSource {
        crate::relish::manifest::ManifestSource::File(path.to_path_buf())
    }

    /// Z1.4: Kubernetes YAML applies directly, through the importer.
    #[cfg(feature = "kubernetes")]
    #[tokio::test]
    async fn apply_dry_run_accepts_kubernetes_yaml() {
        let f = write_temp_config(
            r#"
apiVersion: apps/v1
kind: Deployment
metadata:
  name: web
spec:
  template:
    spec:
      containers:
      - name: web
        image: nginx:1
"#,
        );
        apply_with_client(
            &source(f.path()),
            OutputFormat::Human,
            true,
            &bogus_client(),
        )
        .await
        .unwrap();
    }

    /// X5 regression: an unreachable agent used to fall back to a
    /// dry-run plan and exit 0, making dead-agent deploys look green.
    #[tokio::test]
    async fn apply_exits_nonzero_when_agent_unreachable() {
        let f = write_temp_config(
            r#"
            [app.web]
            image = "myapp:v1"
            port = 8080
        "#,
        );
        let err = apply_with_client(
            &source(f.path()),
            OutputFormat::Human,
            false,
            &bogus_client(),
        )
        .await
        .unwrap_err();
        assert!(
            matches!(err, RelishError::AgentUnreachable { .. }),
            "got: {err:?}"
        );
    }

    #[tokio::test]
    async fn apply_dry_run_succeeds_without_agent() {
        let f = write_temp_config(
            r#"
            [app.web]
            image = "myapp:v1"
            port = 8080
        "#,
        );
        assert!(
            apply_with_client(
                &source(f.path()),
                OutputFormat::Human,
                true,
                &bogus_client()
            )
            .await
            .is_ok()
        );
    }

    #[tokio::test]
    async fn apply_with_missing_file_errors() {
        let result = apply_with_client(
            &source(Path::new("/nonexistent/config.toml")),
            OutputFormat::Human,
            false,
            &bogus_client(),
        )
        .await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            matches!(err, RelishError::Config(_)),
            "expected Config error, got: {err:?}"
        );
    }

    #[tokio::test]
    async fn apply_with_invalid_toml_errors() {
        let f = write_temp_config("this is not valid toml [[[");
        let result = apply_with_client(
            &source(f.path()),
            OutputFormat::Human,
            false,
            &bogus_client(),
        )
        .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn apply_with_validation_error() {
        let f = write_temp_config(
            r#"
            [app.broken]
            replicas = 3
        "#,
        );
        let result = apply_with_client(
            &source(f.path()),
            OutputFormat::Human,
            false,
            &bogus_client(),
        )
        .await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            matches!(err, RelishError::Config(_)),
            "expected Config error, got: {err:?}"
        );
    }

    #[test]
    fn since_accepts_epoch_and_duration_suffixes() {
        let now = 1_750_000_000;
        assert_eq!(parse_since("1749000000", now).unwrap(), 1_749_000_000);
        assert_eq!(parse_since("30s", now).unwrap(), now - 30);
        assert_eq!(parse_since("5m", now).unwrap(), now - 300);
        assert_eq!(parse_since("2h", now).unwrap(), now - 7200);
        assert_eq!(parse_since("1d", now).unwrap(), now - 86_400);
    }

    #[test]
    fn oversized_or_unicode_since_values_return_errors() {
        for value in [
            "18446744073709551615d",
            "18446744073709551615h",
            "18446744073709551615m",
            "é",
            "5☃",
        ] {
            assert!(parse_since(value, 1_750_000_000).is_err(), "{value}");
        }
    }

    #[test]
    fn since_rejects_unknown_units_and_garbage() {
        let now = 1_750_000_000;
        assert!(parse_since("5w", now).is_err());
        assert!(parse_since("", now).is_err());
        assert!(parse_since("abc", now).is_err());
    }

    #[test]
    fn json_field_parses_key_value_and_rejects_malformed() {
        assert_eq!(
            parse_json_field("level=error").unwrap(),
            ("level".to_string(), "error".to_string())
        );
        assert!(parse_json_field("no-equals-sign").is_err());
        assert!(parse_json_field("=value").is_err());
    }

    #[test]
    fn log_options_carry_all_flags() {
        let flags = LogFlags {
            tail: Some(50),
            grep: Some("error|warn".to_string()),
            since: Some("5m".to_string()),
            until: Some("1m".to_string()),
            instance: Some("default__web-2".to_string()),
            json_field: Some("level=warn".to_string()),
            ..LogFlags::default()
        };
        let options = build_log_options(flags, 1_750_000_000).unwrap();
        assert_eq!(options.tail, Some(50));
        assert_eq!(options.grep.as_deref(), Some("error|warn"));
        assert_eq!(options.start, Some(1_750_000_000 - 300));
        assert_eq!(options.end, Some(1_750_000_000 - 60));
        assert_eq!(options.instance.as_deref(), Some("default__web-2"));
        assert_eq!(
            options.json_field,
            Some(("level".to_string(), "warn".to_string()))
        );
    }

    /// F07 part 2: `--stream` names stdout or stderr, and isn't offered
    /// with `-f` yet.
    #[test]
    fn stream_takes_stdout_or_stderr_and_not_with_follow() {
        let flags = LogFlags {
            stream: Some("stderr".to_string()),
            ..LogFlags::default()
        };
        let options = build_log_options(flags, 1_750_000_000).unwrap();
        assert_eq!(
            options.stream,
            Some(crate::ketchup::types::LogStream::Stderr)
        );
        let unknown = LogFlags {
            stream: Some("stdin".to_string()),
            ..LogFlags::default()
        };
        assert!(build_log_options(unknown, 1_750_000_000).is_err());
        let following = LogFlags {
            stream: Some("stderr".to_string()),
            follow: true,
            ..LogFlags::default()
        };
        assert!(build_log_options(following, 1_750_000_000).is_err());
    }

    /// F07 part 2: `--grep` is a regular expression, checked before any
    /// request goes out.
    #[test]
    fn an_invalid_grep_pattern_is_a_flag_error() {
        let flags = LogFlags {
            grep: Some("(unclosed".to_string()),
            ..LogFlags::default()
        };
        let error = build_log_options(flags, 1_750_000_000).unwrap_err();
        assert!(error.to_string().contains("grep"), "{error}");
    }

    /// `--until` ends a window: it can't follow new lines, and it can't come
    /// before `--since`.
    #[test]
    fn until_refuses_follow_and_a_window_that_ends_before_it_starts() {
        let following = LogFlags {
            until: Some("1m".to_string()),
            follow: true,
            ..LogFlags::default()
        };
        assert!(build_log_options(following, 1_750_000_000).is_err());
        let backwards = LogFlags {
            since: Some("1m".to_string()),
            until: Some("5m".to_string()),
            ..LogFlags::default()
        };
        assert!(build_log_options(backwards, 1_750_000_000).is_err());
    }

    #[tokio::test]
    async fn logs_returns_agent_unreachable() {
        let options = super::super::client::LogOptions::default();
        let err = logs_with_client(
            "web",
            "default",
            &options,
            OutputFormat::Human,
            &bogus_client(),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, RelishError::AgentUnreachable { .. }));
    }

    #[tokio::test]
    async fn exec_returns_agent_unreachable() {
        let err = exec_with_client(
            "web",
            "default",
            &["sh".to_string()],
            OutputFormat::Human,
            &bogus_client(),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, RelishError::AgentUnreachable { .. }));
    }

    #[tokio::test]
    async fn stop_returns_agent_unreachable() {
        let err = stop_with_client("web", "default", OutputFormat::Human, &bogus_client())
            .await
            .unwrap_err();
        assert!(matches!(err, RelishError::AgentUnreachable { .. }));
    }

    #[test]
    fn init_creates_files() {
        let dir = tempfile::tempdir().unwrap();
        init(dir.path(), "test-cluster", "node-01").unwrap();
        assert!(dir.path().join("reliaburger.toml").exists());
        assert!(dir.path().join("app.toml").exists());
    }

    #[test]
    fn init_enables_ebpf_and_dns_on_rootful_linux() {
        let rootful = InitHost {
            linux: true,
            root: true,
            runc: true,
        };
        let mut config = crate::config::node::NodeConfig::default();
        enable_node_data_path(&mut config, rootful);
        assert!(config.ebpf.enabled, "namespace isolation needs eBPF");
        assert!(
            config.dns.enabled,
            "allowlisted workloads resolve through Bun"
        );

        for host in [
            InitHost {
                linux: false,
                ..rootful
            },
            InitHost {
                root: false,
                ..rootful
            },
            InitHost {
                runc: false,
                ..rootful
            },
        ] {
            let mut config = crate::config::node::NodeConfig::default();
            enable_node_data_path(&mut config, host);
            assert!(
                !config.ebpf.enabled && !config.dns.enabled,
                "{host:?} can't load eBPF, so it must not be told to"
            );
        }
    }

    #[test]
    fn init_creates_a_missing_output_directory() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().join("cluster");

        init(&dir, "test-cluster", "node-01").unwrap();

        assert!(dir.join("reliaburger.toml").exists());
        assert!(dir.join("app.toml").exists());
    }

    #[test]
    fn init_refuses_overwrite() {
        let dir = tempfile::tempdir().unwrap();
        init(dir.path(), "test-cluster", "node-01").unwrap();
        let err = init(dir.path(), "test-cluster", "node-01").unwrap_err();
        assert!(matches!(err, RelishError::FileExists { .. }));
    }

    #[test]
    fn init_generated_config_parses() {
        let dir = tempfile::tempdir().unwrap();
        init(dir.path(), "test-cluster", "node-01").unwrap();

        let node_content = std::fs::read_to_string(dir.path().join("reliaburger.toml")).unwrap();
        let _: crate::config::node::NodeConfig = toml::from_str(&node_content).unwrap();

        let app_content = std::fs::read_to_string(dir.path().join("app.toml")).unwrap();
        let config = Config::parse(&app_content).unwrap();
        config.validate().unwrap();
    }

    #[test]
    fn init_generated_app_has_an_executable_container_command() {
        let dir = tempfile::tempdir().unwrap();
        init(dir.path(), "test-cluster", "node-01").unwrap();

        let app_content = std::fs::read_to_string(dir.path().join("app.toml")).unwrap();
        let config = Config::parse(&app_content).unwrap();
        let web = config.app.get("web").unwrap();

        assert_eq!(web.image.as_deref(), Some("busybox:1.36"));
        assert_eq!(web.port, Some(8080));
        assert_eq!(web.command.first().map(String::as_str), Some("/bin/sh"));
        assert!(
            web.command
                .iter()
                .any(|argument| argument.contains("httpd")),
            "the generated app must not depend on an image entrypoint that runc never loads"
        );
    }

    const ROOT_PASSPHRASE: &str = "correct horse battery staple";

    /// `relish init` with a passphrase backup kept outside `dir`, as an
    /// operator keeps it off the node. Returns where the backup went.
    fn init_with_backup(
        dir: &Path,
        cluster: &str,
        node: &str,
        backup_dir: &Path,
    ) -> Result<PathBuf, RelishError> {
        let passphrase = backup_dir.join("root-passphrase");
        std::fs::write(&passphrase, format!("{ROOT_PASSPHRASE}\n")).unwrap();
        let path = backup_dir.join("root-backup.age");
        init_with_security(
            dir,
            cluster,
            node,
            InitSecurityMode::MutualTls,
            &super::super::ca_cmd::InitRootBackup {
                path: path.clone(),
                target: super::super::ca_cmd::BackupTarget::Passphrase(
                    super::super::ca_cmd::PassphraseSource::File(passphrase),
                ),
            },
            OutputFormat::Human,
        )?;
        Ok(path)
    }

    /// `relish init` for tests that don't look at the backup. It goes into
    /// `dir`'s own temporary tree, which an operator's never would.
    fn init(dir: &Path, cluster: &str, node: &str) -> Result<(), RelishError> {
        std::fs::create_dir_all(dir)?;
        init_with_backup(dir, cluster, node, dir).map(|_| ())
    }

    /// #709: no file `relish init` leaves on the node holds the root key.
    #[test]
    fn init_writes_no_root_key_on_the_node() {
        let dir = tempfile::tempdir().unwrap();
        let backups = tempfile::tempdir().unwrap();
        let backup = init_with_backup(dir.path(), "mycluster", "node-01", backups.path()).unwrap();
        let opened = crate::sesame::root_backup::RootBackup::open(
            &std::fs::read(&backup).unwrap(),
            &crate::sesame::root_backup::BackupOpener::Passphrase(
                age::secrecy::SecretString::from(ROOT_PASSPHRASE.to_string()),
            ),
        )
        .unwrap();
        let root_key = opened.private_key_der().unwrap();
        let mut files = vec![dir.path().to_path_buf()];
        while let Some(path) = files.pop() {
            if path.is_dir() {
                files.extend(std::fs::read_dir(&path).unwrap().map(|e| e.unwrap().path()));
                continue;
            }
            let bytes = std::fs::read(&path).unwrap();
            assert!(
                !bytes
                    .windows(root_key.len())
                    .any(|w| w == root_key.as_slice()),
                "{} holds the root key",
                path.display()
            );
            assert!(
                !path.to_string_lossy().ends_with("root-ca.age"),
                "{}",
                path.display()
            );
        }
    }

    /// The backup opens with the passphrase init was given and holds the
    /// root the node pins.
    #[test]
    fn init_backup_opens_with_the_given_passphrase_and_matches_the_fingerprint() {
        let dir = tempfile::tempdir().unwrap();
        let backups = tempfile::tempdir().unwrap();
        let backup = init_with_backup(dir.path(), "mycluster", "node-01", backups.path()).unwrap();
        let identity = crate::sesame::identity_store::load(&dir.path().join("identity"))
            .unwrap()
            .unwrap();
        let fingerprint = crate::sesame::identity_store::root_ca_fingerprint(&identity.root_ca_der);
        let opened = crate::sesame::root_backup::RootBackup::open(
            &std::fs::read(&backup).unwrap(),
            &crate::sesame::root_backup::BackupOpener::Passphrase(
                age::secrecy::SecretString::from(ROOT_PASSPHRASE.to_string()),
            ),
        )
        .unwrap();
        opened
            .verify(&fingerprint, std::time::SystemTime::now())
            .unwrap();
    }

    /// A backup that can't be written leaves no cluster behind: no config,
    /// no master key, no state.
    #[test]
    fn init_does_not_report_success_when_the_backup_write_fails() {
        let dir = tempfile::tempdir().unwrap();
        let backups = tempfile::tempdir().unwrap();
        let missing = backups.path().join("not-there");
        let passphrase = backups.path().join("passphrase");
        std::fs::write(&passphrase, ROOT_PASSPHRASE).unwrap();
        let err = init_with_security(
            dir.path(),
            "mycluster",
            "node-01",
            InitSecurityMode::MutualTls,
            &super::super::ca_cmd::InitRootBackup {
                path: missing.join("root.age"),
                target: super::super::ca_cmd::BackupTarget::Passphrase(
                    super::super::ca_cmd::PassphraseSource::File(passphrase),
                ),
            },
            OutputFormat::Human,
        )
        .unwrap_err();
        assert!(matches!(err, RelishError::RootBackup(_)), "{err}");
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }

    #[test]
    fn init_never_overwrites_an_existing_backup() {
        let dir = tempfile::tempdir().unwrap();
        let backups = tempfile::tempdir().unwrap();
        std::fs::write(backups.path().join("root-backup.age"), "keep me").unwrap();
        let err = init_with_backup(dir.path(), "mycluster", "node-01", backups.path()).unwrap_err();
        assert!(matches!(err, RelishError::FileExists { .. }), "{err}");
        assert!(!dir.path().join("reliaburger.toml").exists());
    }

    #[test]
    fn init_rejects_an_invalid_spiffe_trust_domain_before_writing_files() {
        let dir = tempfile::tempdir().unwrap();
        let err = init(dir.path(), "Not/A/Domain", "node-01").unwrap_err();
        assert!(matches!(err, RelishError::InitFailed(_)));
        assert!(!dir.path().join("reliaburger.toml").exists());
    }

    #[test]
    fn init_persists_the_first_nodes_identity_next_to_the_master_key() {
        let dir = tempfile::tempdir().unwrap();
        init(dir.path(), "mycluster", "node-01").unwrap();

        let identity = crate::sesame::identity_store::load(&dir.path().join("identity"))
            .unwrap()
            .expect("init should persist the first node's identity");
        assert_eq!(identity.node_id, "node-01");
        assert!(!identity.private_key_der.is_empty());
        crate::sesame::cert::validate_chain(
            &identity.certificate_der,
            &identity.node_ca_der,
            &identity.root_ca_der,
        )
        .unwrap();
    }

    #[test]
    fn init_writes_security_paths_into_the_generated_node_config() {
        let dir = tempfile::tempdir().unwrap();
        init(dir.path(), "mycluster", "node-01").unwrap();

        let node_content = std::fs::read_to_string(dir.path().join("reliaburger.toml")).unwrap();
        let nc: crate::config::node::NodeConfig = toml::from_str(&node_content).unwrap();
        assert_eq!(nc.cluster.name, "mycluster");
        assert_eq!(
            nc.security.master_key_path,
            Some(dir.path().join("mycluster-master.key"))
        );
        assert_eq!(
            nc.security.bootstrap_path,
            Some(dir.path().join("mycluster-security-bootstrap.json"))
        );
        assert_eq!(nc.security.identity_dir, Some(dir.path().join("identity")));
        assert!(
            nc.security.require_mtls,
            "the normal init path must secure cluster transports without a manual config edit"
        );

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.path().join("mycluster-security-bootstrap.json"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600, "Bun refuses broader bootstrap permissions");
        }
    }

    #[test]
    fn init_development_plaintext_is_explicit_and_marks_the_generated_config() {
        let dir = tempfile::tempdir().unwrap();
        let backups = tempfile::tempdir().unwrap();
        let passphrase = backups.path().join("passphrase");
        std::fs::write(&passphrase, ROOT_PASSPHRASE).unwrap();
        init_with_security(
            dir.path(),
            "devcluster",
            "node-01",
            InitSecurityMode::DevelopmentPlaintext,
            &super::super::ca_cmd::InitRootBackup {
                path: backups.path().join("root.age"),
                target: super::super::ca_cmd::BackupTarget::Passphrase(
                    super::super::ca_cmd::PassphraseSource::File(passphrase),
                ),
            },
            OutputFormat::Human,
        )
        .unwrap();

        let node_content = std::fs::read_to_string(dir.path().join("reliaburger.toml")).unwrap();
        let config: crate::config::node::NodeConfig = toml::from_str(&node_content).unwrap();
        assert!(!config.security.require_mtls);
        assert!(node_content.contains("DEVELOPMENT-ONLY PLAINTEXT CLUSTER TRANSPORTS"));
        assert!(node_content.contains("relish init --development-plaintext"));
    }

    #[tokio::test]
    async fn inspect_returns_agent_unreachable() {
        let err = inspect_with_client("web", OutputFormat::Human, &bogus_client())
            .await
            .unwrap_err();
        assert!(matches!(err, RelishError::AgentUnreachable { .. }));
    }

    #[tokio::test]
    async fn council_returns_agent_unreachable() {
        let err = council_with_client(OutputFormat::Human, &bogus_client())
            .await
            .unwrap_err();
        assert!(
            matches!(err, RelishError::AgentUnreachable { .. }),
            "got: {err:?}"
        );
    }

    #[test]
    fn lint_valid_config() {
        let f = write_temp_config(
            r#"
            [app.web]
            image = "myapp:v1"
            port = 8080
        "#,
        );
        lint(f.path(), OutputFormat::Human).unwrap();
    }

    #[test]
    fn lint_invalid_config() {
        let f = write_temp_config(
            r#"
            [app.broken]
            replicas = 3
        "#,
        );
        let result = lint(f.path(), OutputFormat::Human);
        assert!(result.is_err());
    }

    #[test]
    fn lint_missing_file() {
        let result = lint(Path::new("/nonexistent/config.toml"), OutputFormat::Human);
        assert!(result.is_err());
    }

    #[test]
    fn lint_as_json_counts_the_resources_and_carries_the_warnings() {
        let f = write_temp_config(
            r#"
            [app.web]
            image = "myapp:v1"
            port = 8080

            [app.worker]
            exec = "/usr/bin/python3"
            command = ["worker.py"]
            [[app.worker.init]]
            command = ["echo", "hi"]

            [job.migrate]
            image = "myapp:v1"
        "#,
        );
        let report = lint_report(f.path()).unwrap();
        insta::assert_snapshot!(format_output(&report, OutputFormat::Json).unwrap());
        assert_eq!(report.to_string(), "config valid: 2 app(s), 1 job(s)\n");
    }

    #[test]
    fn a_rollback_as_json_names_the_app() {
        let rollback = Rollback {
            app: "web",
            namespace: "default",
            forced: false,
        };
        insta::assert_snapshot!(
            format_output(&rollback, OutputFormat::Json).unwrap(),
            @r#"
        {
          "app": "web",
          "namespace": "default",
          "forced": false
        }
        "#
        );
        assert_eq!(
            rollback.to_string(),
            "rolled web back to its previous version\n"
        );
    }

    #[test]
    fn routes_as_json_are_the_list_of_routes() {
        let routes = RouteTable(vec![crate::wrapper::types::RouteInfo {
            host: "shop.example.com".to_string(),
            path: "/".to_string(),
            app_name: "web".to_string(),
            namespace: "default".to_string(),
            healthy_backends: 2,
            total_backends: 3,
            websocket: true,
        }]);
        insta::assert_snapshot!(format_output(&routes, OutputFormat::Json).unwrap());
        assert_eq!(
            RouteTable(Vec::new()).to_string(),
            "no ingress routes configured\n"
        );
    }

    fn stored_line(
        timestamp: u64,
        node: &str,
        instance: &str,
        stream: crate::ketchup::types::LogStream,
        line: &str,
    ) -> crate::ketchup::types::LogEntry {
        crate::ketchup::types::LogEntry {
            timestamp,
            sequence: timestamp * 1_000_000_000,
            instance: Some(instance.to_string()),
            node: Some(node.to_string()),
            stream,
            line: line.to_string(),
        }
    }

    /// #1566: `relish logs -o json` is JSON Lines, one object per log line,
    /// each saying when, where and on which stream it was written.
    #[test]
    fn logs_as_json_are_one_object_per_line() {
        use crate::ketchup::types::LogStream;
        let lines = super::super::client::LogLines::Stored(vec![
            stored_line(
                1_790_000_000,
                "node-1",
                "default__web-0",
                LogStream::Stdout,
                "listening on :8080",
            ),
            stored_line(
                1_790_000_001,
                "node-2",
                "default__web-1",
                LogStream::Stderr,
                r#"{"level":"error","msg":"no \"db\""}"#,
            ),
        ]);
        insta::assert_snapshot!(render_logs(&lines, OutputFormat::Json).unwrap());
        for line in render_logs(&lines, OutputFormat::Json).unwrap().lines() {
            let record: serde_json::Value = serde_json::from_str(line).unwrap();
            assert!(record["text"].is_string(), "{line}");
        }
    }

    /// The agent's own capture records only the text, so the other fields
    /// are `null` rather than guessed.
    #[test]
    fn captured_logs_as_json_leave_what_the_capture_lacks_null() {
        let lines = super::super::client::LogLines::Captured(vec!["hello".to_string()]);
        assert_eq!(
            render_logs(&lines, OutputFormat::Json).unwrap(),
            r#"{"at":null,"node":null,"instance":null,"stream":null,"text":"hello"}"#
        );
        let empty = super::super::client::LogLines::Captured(Vec::new());
        assert_eq!(render_logs(&empty, OutputFormat::Json).unwrap(), "");
        assert_eq!(render_logs(&empty, OutputFormat::Yaml).unwrap(), "");
    }

    #[test]
    fn logs_as_yaml_are_one_list() {
        use crate::ketchup::types::LogStream;
        let lines = super::super::client::LogLines::Stored(vec![stored_line(
            1_790_000_000,
            "node-1",
            "default__web-0",
            LogStream::Stdout,
            "ready",
        )]);
        insta::assert_snapshot!(render_logs(&lines, OutputFormat::Yaml).unwrap());
    }

    #[tokio::test]
    async fn following_logs_as_json_is_refused_until_the_stream_says_where_lines_came_from() {
        let flags = LogFlags {
            follow: true,
            ..LogFlags::default()
        };
        let error = logs("web", "default", flags, OutputFormat::Json)
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("--output json or yaml"), "{error}");
    }

    // --- relish sign ---

    pub(super) const MYAPP_V1: &str =
        "sha256:1111111111111111111111111111111111111111111111111111111111111111";
    const MYAPP_V2: &str =
        "sha256:2222222222222222222222222222222222222222222222222222222222222222";
    const TEAM_APP: &str =
        "sha256:3333333333333333333333333333333333333333333333333333333333333333";

    fn summary(
        repository: &str,
        digest: &str,
        tags: &[&str],
    ) -> crate::pickle::types::ImageSummary {
        crate::pickle::types::ImageSummary {
            repository: repository.to_string(),
            digest: digest.to_string(),
            tags: tags.iter().map(|t| t.to_string()).collect(),
            layers: 1,
            total_size: 100,
            platforms: Vec::new(),
        }
    }

    fn registry_listing() -> Vec<crate::pickle::types::ImageSummary> {
        vec![
            summary("myapp", MYAPP_V1, &["v1"]),
            summary("myapp", MYAPP_V2, &["v2", "latest"]),
            summary("team/app", TEAM_APP, &["v1"]),
        ]
    }

    const BURGER_INDEX: &str =
        "sha256:4444444444444444444444444444444444444444444444444444444444444444";
    const BURGER_AMD64: &str =
        "sha256:5555555555555555555555555555555555555555555555555555555555555555";
    const BURGER_ARM64: &str =
        "sha256:6666666666666666666666666666666666666666666666666666666666666666";

    fn multi_platform_summary() -> crate::pickle::types::ImageSummary {
        let platform = |name: &str, digest: &str, size| crate::pickle::types::PlatformSummary {
            platform: name.to_string(),
            digest: digest.to_string(),
            layers: 1,
            total_size: size,
        };
        crate::pickle::types::ImageSummary {
            repository: "burger".to_string(),
            digest: BURGER_INDEX.to_string(),
            tags: ["v1".to_string()].into(),
            layers: 0,
            total_size: 9_400_000,
            platforms: vec![
                platform("linux/amd64", BURGER_AMD64, 4_800_000),
                platform("linux/arm64", BURGER_ARM64, 4_600_000),
            ],
        }
    }

    #[test]
    fn images_table_shows_a_multi_platform_image_on_one_row_with_its_platforms() {
        let mut images = registry_listing();
        images.push(multi_platform_summary());
        insta::assert_snapshot!(format_images_table(&images));
    }

    #[test]
    fn images_table_widens_its_columns_to_fit_a_long_cached_repository() {
        let mut images = registry_listing();
        images.push(summary(
            "cache/public.ecr.aws/docker/library/redis",
            MYAPP_V1,
            &["7.2-alpine"],
        ));
        images.push(multi_platform_summary());
        insta::assert_snapshot!(format_images_table(&images));
    }

    #[test]
    fn images_table_says_so_when_the_registry_is_empty() {
        assert_eq!(format_images_table(&[]), "no images in local registry\n");
    }

    #[test]
    fn sign_resolves_a_multi_platform_tag_to_the_index_and_accepts_a_platform_digest() {
        let images = vec![multi_platform_summary()];
        assert_eq!(
            resolve_image_digest("burger:v1", &images).unwrap().as_str(),
            BURGER_INDEX
        );
        let pinned = format!("burger@{BURGER_ARM64}");
        assert_eq!(
            resolve_image_digest(&pinned, &images).unwrap().as_str(),
            BURGER_ARM64
        );
        assert_eq!(
            resolve_image_digest(BURGER_AMD64, &images)
                .unwrap()
                .as_str(),
            BURGER_AMD64
        );
    }

    #[test]
    fn sign_resolves_a_tag_to_its_manifest_digest() {
        let digest = resolve_image_digest("myapp:v1", &registry_listing()).unwrap();
        assert_eq!(digest.as_str(), MYAPP_V1);
    }

    #[test]
    fn sign_resolves_an_untagged_reference_to_latest() {
        let digest = resolve_image_digest("myapp", &registry_listing()).unwrap();
        assert_eq!(digest.as_str(), MYAPP_V2);
    }

    #[test]
    fn sign_strips_the_registry_host_like_the_deploy_check_does() {
        let digest =
            resolve_image_digest("localhost:5050/team/app:v1", &registry_listing()).unwrap();
        assert_eq!(digest.as_str(), TEAM_APP);
    }

    #[test]
    fn sign_accepts_a_pinned_reference_and_a_bare_digest() {
        let pinned = format!("myapp@{MYAPP_V1}");
        assert_eq!(
            resolve_image_digest(&pinned, &registry_listing())
                .unwrap()
                .as_str(),
            MYAPP_V1
        );
        assert_eq!(
            resolve_image_digest(MYAPP_V2, &registry_listing())
                .unwrap()
                .as_str(),
            MYAPP_V2
        );
    }

    #[test]
    fn image_delete_reads_repository_and_tag() {
        assert_eq!(parse_image_tag("web:v1").unwrap(), ("web", "v1"));
        assert_eq!(
            parse_image_tag("team/web:v1.2").unwrap(),
            ("team/web", "v1.2")
        );
        assert_eq!(
            parse_image_tag("localhost:5050/team/web:latest").unwrap(),
            ("team/web", "latest")
        );
    }

    #[test]
    fn image_delete_refuses_a_bare_name_or_a_digest() {
        for reference in [
            "web",
            "localhost:5050/web",
            "web:",
            "web@sha256:1111111111111111111111111111111111111111111111111111111111111111",
        ] {
            assert!(
                matches!(
                    parse_image_tag(reference),
                    Err(RelishError::InvalidImageTag { .. })
                ),
                "{reference}"
            );
        }
    }

    #[test]
    fn sign_refuses_an_image_the_registry_does_not_hold() {
        for image in [
            "myapp:v9",
            "nginx:latest",
            "team/app@sha256:1111111111111111111111111111111111111111111111111111111111111111",
        ] {
            let result = resolve_image_digest(image, &registry_listing());
            assert!(
                matches!(result, Err(RelishError::ImageNotInRegistry { .. })),
                "{image}: {result:?}"
            );
        }
    }

    #[test]
    fn sign_keygen_writes_an_owner_only_key_and_refuses_to_overwrite() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("image-signing.pem");
        sign_keygen(&path, OutputFormat::Human).unwrap();

        let key = crate::pickle::signing::SigningKey::from_pem(&fs::read_to_string(&path).unwrap());
        assert!(key.is_ok(), "the written key must load back: {key:?}");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
        assert!(matches!(
            sign_keygen(&path, OutputFormat::Human),
            Err(RelishError::FileExists { .. })
        ));
    }
}

#[cfg(test)]
mod task_array_tests {
    use super::*;
    use crate::bun::task_array_api::TaskResults;
    use crate::bun::task_array_node::TaskResultRow;
    use crate::meat::task_array::TaskArraySpec;

    /// A stand-in leader that records each `/v1/batch/array` submission's
    /// `Idempotency-Key` header and admits it as run 7.
    async fn key_recording_server() -> (
        BunClient,
        std::sync::Arc<std::sync::Mutex<Vec<Option<String>>>>,
        tokio::task::JoinHandle<()>,
    ) {
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let recorder = seen.clone();
        let app = axum::Router::new().route(
            "/v1/batch/array",
            axum::routing::post(move |headers: axum::http::HeaderMap| {
                let recorder = recorder.clone();
                async move {
                    let key = headers
                        .get("idempotency-key")
                        .and_then(|value| value.to_str().ok())
                        .map(str::to_string);
                    recorder.lock().unwrap().push(key);
                    axum::Json(serde_json::json!({"batch_id": 7, "count": 10, "chunks": 1}))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let client =
            BunClient::new_with_token(&format!("http://{}", listener.local_addr().unwrap()), None);
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (client, seen, server)
    }

    #[tokio::test]
    async fn run_batch_sends_and_prints_an_idempotency_key() {
        let (client, seen, server) = key_recording_server().await;
        let mut err = Vec::new();
        run_task_array_with_client(run(), OutputFormat::Human, &client, &mut err)
            .await
            .unwrap();
        server.abort();
        let sent = seen.lock().unwrap().clone();
        let [Some(key)] = sent.as_slice() else {
            panic!("expected one keyed submission, got {sent:?}");
        };
        assert_eq!(key.len(), 32, "{key}");
        let printed = String::from_utf8(err).unwrap();
        assert!(
            printed.contains(&format!("--idempotency-key {key}")),
            "{printed}"
        );
    }

    /// #1424: a `warn` policy's finding in the answer is printed as a
    /// warning, so `relish run --batch` says what `relish apply` would.
    #[tokio::test]
    async fn run_batch_prints_the_policy_warnings_in_the_answer() {
        let app = axum::Router::new().route(
            "/v1/batch/array",
            axum::routing::post(|| async {
                axum::Json(serde_json::json!({
                    "batch_id": 7, "count": 1, "chunks": 1,
                    "policy_warnings": ["policy pinned warns about job default/render: pin it"],
                }))
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let client =
            BunClient::new_with_token(&format!("http://{}", listener.local_addr().unwrap()), None);
        let server = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let mut err = Vec::new();
        run_task_array_with_client(run(), OutputFormat::Human, &client, &mut err)
            .await
            .unwrap();
        server.abort();
        let printed = String::from_utf8(err).unwrap();
        assert!(
            printed.contains("warning: policy pinned warns about job default/render: pin it"),
            "{printed}"
        );
    }

    #[tokio::test]
    async fn explicit_idempotency_key_is_sent_unchanged() {
        let (client, seen, server) = key_recording_server().await;
        let mut retry = run();
        retry.idempotency_key = Some("retry-after-timeout-1".into());
        let mut err = Vec::new();
        run_task_array_with_client(retry, OutputFormat::Human, &client, &mut err)
            .await
            .unwrap();
        let mut bad = run();
        bad.idempotency_key = Some("has a space".into());
        assert!(
            run_task_array_with_client(bad, OutputFormat::Human, &client, &mut Vec::new())
                .await
                .is_err()
        );
        server.abort();
        assert_eq!(
            *seen.lock().unwrap(),
            vec![Some("retry-after-timeout-1".to_string())],
            "a refused key never reaches the cluster"
        );
        assert!(
            String::from_utf8(err)
                .unwrap()
                .contains("retry-after-timeout-1")
        );
    }

    #[test]
    fn lint_rejects_an_out_of_range_job_policy() {
        for policy in [
            "max_attempts = 0",
            "max_attempts = 11",
            "task_timeout_secs = 86401",
            "overlap = 'allow'",
        ] {
            let file = tempfile::NamedTempFile::new().unwrap();
            std::fs::write(
                file.path(),
                format!("[job.cleanup]\nimage = 'cleanup:v1'\n{policy}\n"),
            )
            .unwrap();
            assert!(lint(file.path(), OutputFormat::Human).is_err(), "{policy}");
        }
        let file = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(
            file.path(),
            "[job.cleanup]\nimage = 'cleanup:v1'\nschedule = '@daily'\nmax_attempts = 2\ntask_timeout_secs = 300\noverlap = 'allow'\nreplay_unknown = true\n",
        )
        .unwrap();
        lint(file.path(), OutputFormat::Human).unwrap();
    }

    fn run() -> TaskArrayRun {
        TaskArrayRun {
            runtime: crate::config::job::JobRuntime::Process,
            schedule: None,
            image: None,
            cpu: None,
            memory: None,
            name: "render".to_string(),
            namespace: "default".to_string(),
            exec: Some(PathBuf::from("/usr/local/bin/rb-task")),
            args: vec!["--frame".to_string(), "{index}".to_string()],
            env: vec!["MODE=fast".to_string(), "EMPTY=".to_string()],
            spec: TaskArraySpec::with_count(10),
            idempotency_key: None,
        }
    }

    #[test]
    fn run_flags_preserve_explicit_container_isolation_and_refuse_host_reuse() {
        use crate::config::job::JobRuntime;
        let mut image = run();
        image.exec = None;
        image.image = Some(
            "fixture@sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
                .into(),
        );
        image.runtime = JobRuntime::SharedRunc;
        let request = task_array_request(image).unwrap();
        assert_eq!(request.template.runtime, JobRuntime::SharedRunc);
        let mut host = run();
        host.runtime = JobRuntime::SharedRunc;
        assert!(
            matches!(task_array_request(host), Err(RelishError::InvalidFlag {flag,..}) if flag == "runtime")
        );
    }

    #[test]
    fn run_flags_become_a_task_array_request() {
        let request = task_array_request(run()).unwrap();
        assert_eq!(request.namespace.as_deref(), Some("default"));
        assert_eq!(
            request.template.exec,
            Some(PathBuf::from("/usr/local/bin/rb-task"))
        );
        assert_eq!(
            request.template.command,
            Some(vec!["--frame".to_string(), "{index}".to_string()])
        );
        assert_eq!(
            request.template.env["MODE"],
            crate::config::types::EnvValue::Plain("fast".to_string())
        );
        assert_eq!(
            request.template.env["EMPTY"],
            crate::config::types::EnvValue::Plain(String::new())
        );
        assert_eq!(request.spec.count, 10);
    }

    #[test]
    fn a_relative_exec_or_a_bad_env_pair_is_refused_before_sending() {
        let mut relative = run();
        relative.exec = Some(PathBuf::from("rb-task"));
        assert!(matches!(
            task_array_request(relative),
            Err(RelishError::InvalidFlag { flag, .. }) if flag == "exec"
        ));
        for pair in ["NOEQUALS", "=value"] {
            let mut bad = run();
            bad.env = vec![pair.to_string()];
            assert!(matches!(
                task_array_request(bad),
                Err(RelishError::InvalidFlag { flag, .. }) if flag == "env"
            ));
        }
    }

    #[test]
    fn an_array_summary_reads_as_counts_chunks_failures_and_nodes() {
        let summary = serde_json::json!({
            "kind": "array", "name": "render", "namespace": "default",
            "status": "Running", "total": 100000, "succeeded": 51200,
            "failed": 5, "not_run": 0, "retried": 15, "queued": 40000,
            "retry_causes": { "exit_code": 10, "signal": 2, "timeout": 0,
                              "unknown": 0, "launch": 3, "unrecorded": 0 },
            "held": 8800, "chunks": 98, "chunks_done": 50,
            "failed_indices": [[7, 7], [1000, 1002]],
            "nodes": [
                { "node": "node-1", "slots": 8,
                  "counters": { "running": 8, "succeeded": 51200, "failed": 5 } },
                { "node": "node-2", "slots": 0,
                  "refused": "/x isn't in this node's [process_workloads] allowed_binaries" },
            ],
        });
        assert_eq!(
            format_array_summary(3, &summary),
            "task array 3 (render in default): Running\n\
             \x20 tasks: 100000 total, 51200 succeeded, 5 failed, 0 not run, 15 retries\n\
             \x20 retry causes: 10 non-zero exit, 2 signal, 3 launch failure\n\
             \x20 chunks: 50 of 98 done; 8800 tasks held by nodes, 40000 queued\n\
             \x20 failed indices: 7, 1000-1002 and 1 more (see relish batch results 3 --failed)\n\
             \x20 node-1: 8 slots, 8 running, 51200 succeeded, 5 failed\n\
             \x20 node-2: can't run it: /x isn't in this node's [process_workloads] allowed_binaries\n"
        );
    }

    #[test]
    fn watch_distinguishes_started_commands_preparation_and_observation_rates() {
        let value = serde_json::json!({"kind":"array","batch_id":9,"succeeded":1000,"total":2000,"failed":0,"not_run":0,"retried":0,"queued":500,"held":500,"status":"Running","runtime":"shared-runc","active_commands":3,"other_in_flight_attempts":5,"whole_run_successes_per_second":100.0,"rates":{"successes_per_second":500.0,"interval_seconds":2.0}});
        let rendered = format_batch_watch(&value);
        for phrase in [
            "mode shared-runc",
            "3 active commands",
            "5 other in-flight attempts",
            "since admission 100.0/s",
            "recent 500.0/s over 2.0s",
        ] {
            assert!(
                rendered.contains(phrase),
                "{phrase:?} missing from {rendered:?}"
            );
        }
        let unknown = format_batch_watch(&serde_json::json!({"kind":"array"}));
        assert!(unknown.contains("unknown active commands"));
    }

    #[test]
    fn status_says_why_no_node_could_run_a_failed_run() {
        let reason =
            "no node can run it: host jobs don't run under [process_workloads] mount_isolation yet";
        let run = serde_json::json!({"kind":"array","batch_id":4,"status":"Failed","not_run":20,"unrunnable_reason":reason});
        assert!(format_batch_watch(&run).contains(&format!("refused: {reason}")));
        assert!(format_array_summary(4, &run).contains(&format!("  refused: {reason}\n")));
        // A manifest names the profile no node could run.
        let manifest = serde_json::json!({"kind":"manifest","batch_id":5,"status":"Failed","cohorts":[
            {"profile":"small","batch_id":6,"succeeded":1,"total":1},
            {"profile":"host","batch_id":7,"succeeded":0,"total":4,"unrunnable_reason":reason}]});
        assert!(format_batch_watch(&manifest).contains(&format!("array 7 refused: {reason}")));
    }

    #[test]
    fn status_says_why_a_run_waits_and_which_occurrences_were_skipped() {
        let reason = "namespace \"tenant\" would exceed CPU quota: 2000+1000 > 2000m";
        let run = serde_json::json!({"kind":"array","batch_id":4,"status":"Running","quota_blocked_reason":reason});
        assert!(format_batch_watch(&run).contains(&format!("quota blocked: {reason}")));
        assert!(format_array_summary(4, &run).contains(&format!("waiting for quota: {reason}")));
        let schedule = serde_json::json!({"kind":"schedule","name":"nightly","namespace":"ops","cron":{"expression":"0 2 * * *"},"total":1,"revision":1,"skipped":{"minute":29000000,"reason":"capacity"},"skipped_count":2});
        assert!(
            format_batch_watch(&schedule)
                .contains("skipped: capacity (2 so far, latest at minute 29000000)")
        );
    }

    #[test]
    fn task_results_render_as_a_table_with_caveats() {
        let results = TaskResults {
            batch_id: 3,
            next_after: None,
            retention_seconds: 3600,
            rows: vec![
                TaskResultRow {
                    grant_attempt: 1,
                    index: 7,
                    attempts: 3,
                    succeeded: false,
                    not_run: false,
                    exit_code: Some(2),
                    run_ms: 15,
                },
                TaskResultRow {
                    grant_attempt: 1,
                    index: 8,
                    attempts: 1,
                    succeeded: false,
                    not_run: false,
                    exit_code: None,
                    run_ms: 600000,
                },
            ],
            truncated: true,
            unreachable: vec![crate::meat::NodeId::new("node-3")],
        };
        assert_eq!(
            format_task_results(&results),
            "     INDEX  ATTEMPTS  OUTCOME      EXIT     RUN MS\n\
             \x20        7         3  failed          2         15\n\
             \x20        8         1  failed          -     600000\n\
             (more rows exist in this page)\n\
             (couldn't reach node-3; their tasks are missing)\n"
        );
    }
}

/// #1566: the JSON shape of every command, one snapshot each.
#[cfg(test)]
mod json_shapes {
    use super::*;

    fn json<T: serde::Serialize + std::fmt::Display>(value: &T) -> String {
        format_output(value, OutputFormat::Json).unwrap()
    }

    #[test]
    fn app_changes_as_json_name_the_action() {
        let mut delete = AppChange::new(AppAction::DeleteRequested, "web", "shop");
        delete.forced = true;
        insta::assert_snapshot!(
            "stop",
            json(&AppChange::new(AppAction::StopRequested, "web", "shop"))
        );
        insta::assert_snapshot!("delete", json(&delete));
        insta::assert_snapshot!(
            "release",
            json(&AppChange::new(AppAction::Released, "web", "shop"))
        );
        assert_eq!(
            AppChange::new(AppAction::Released, "web", "shop").to_string(),
            "released web in shop; relish apply and GitOps manage it again\n"
        );
    }

    #[test]
    fn exec_as_json_carries_the_exit_code_and_output() {
        let command = ["cat".to_string(), "/ready".to_string()];
        insta::assert_snapshot!(json(&ExecReport {
            app: "web",
            namespace: "default",
            command: &command,
            exit_code: 1,
            output: "cat: /ready: No such file or directory\n",
        }));
    }

    #[test]
    fn logs_export_and_search_as_json() {
        let here = ExportLanding::parse("/tmp/archive").unwrap();
        insta::assert_snapshot!("export", json(&ExportReport::new(&here, "node-1", 2, 4096)));
        insta::assert_snapshot!(
            "search",
            json(&SearchRows(vec![serde_json::json!({"count": 3})]))
        );
        assert_eq!(SearchRows(Vec::new()).to_string(), "(no results)\n");
    }

    #[test]
    fn init_join_and_council_reports_as_json_name_files_not_secrets() {
        insta::assert_snapshot!(
            "init",
            json(&InitReport {
                cluster: "prod".to_string(),
                node: "node-01".to_string(),
                node_config: PathBuf::from("/etc/reliaburger/reliaburger.toml"),
                app_config: PathBuf::from("/etc/reliaburger/app.toml"),
                master_key: PathBuf::from("/etc/reliaburger/prod-master.key"),
                security_bootstrap: PathBuf::from("/etc/reliaburger/prod-security-bootstrap.json"),
                identity_dir: PathBuf::from("/etc/reliaburger/identity"),
                root_backup: PathBuf::from("/media/usb/prod-root-backup.age"),
                root_ca_fingerprint: "sha256:ab12".to_string(),
                development_plaintext: false,
            })
        );
        insta::assert_snapshot!(
            "join",
            json(&JoinReport {
                node: "node-02".to_string(),
                identity_dir: PathBuf::from("identity"),
                root_ca_fingerprint: "sha256:ab12".to_string(),
            })
        );
        insta::assert_snapshot!(
            "recover",
            json(&RecoveryReport {
                restored_apps: 4,
                api_tokens: 2,
                previous_epoch: 0,
                recovery_epoch: 1,
                recovery_id: Some("r-1".to_string()),
                data_dir: PathBuf::from("/var/lib/reliaburger"),
            })
        );
        insta::assert_snapshot!(
            "re_enrol",
            json(&ReenrolReport {
                data_dir: PathBuf::from("/var/lib/reliaburger"),
                replaced_at_epoch: Some(1),
            })
        );
    }

    #[test]
    fn config_commands_as_json() {
        let old = super::tests::write_temp_config("[app.web]\nimage = \"web:v1\"\nport = 8080\n");
        let new = super::tests::write_temp_config("[app.web]\nimage = \"web:v2\"\nport = 8080\n");
        let diff = super::super::diff::diff_configs(
            &load_config_file(old.path()).unwrap(),
            &load_config_file(new.path()).unwrap(),
        );
        insta::assert_snapshot!("diff", json(&diff));
        let compiled = CompileReport::from(super::super::compile::compile(new.path()).unwrap());
        let compiled_json: serde_json::Value = serde_json::from_str(&json(&compiled)).unwrap();
        assert_eq!(compiled_json["config"]["app"]["web"]["image"], "web:v2");
        assert_eq!(compiled_json["files"].as_array().unwrap().len(), 1);
        insta::assert_snapshot!(
            "fmt",
            json(&FormatReport {
                path: Path::new("app.toml"),
                result: FormatResult::Formatted,
            })
        );
    }

    #[cfg(feature = "kubernetes")]
    #[test]
    fn kubernetes_import_and_export_as_json() {
        let file = super::tests::write_temp_config(super::tests::K8S_DEPLOYMENT);
        let imported =
            super::super::k8s_import::import_kubernetes(&[file.path().to_path_buf()]).unwrap();
        insta::assert_snapshot!(
            "import",
            json(&ImportReport {
                config: &imported.config,
                report: &imported.report,
            })
        );
        let exported = super::super::k8s_export::export_kubernetes(&imported.config).unwrap();
        let export: serde_json::Value = serde_json::from_str(&json(&KubernetesExport {
            manifests: &exported.yaml,
            report: &exported.report,
        }))
        .unwrap();
        assert!(
            export["manifests"]
                .as_str()
                .unwrap()
                .contains("kind: Deployment")
        );
        assert!(export["report"]["resources_created"].is_array());
    }

    #[test]
    fn cluster_settings_as_json() {
        insta::assert_snapshot!(
            "gitops_set",
            json(&GitopsChange {
                enabled: true,
                config: Some(Path::new("gitops.toml")),
            })
        );
        insta::assert_snapshot!(
            "gitops_disable",
            json(&GitopsChange {
                enabled: false,
                config: None,
            })
        );
        insta::assert_snapshot!(
            "message",
            json(&super::super::output::Message {
                message: "token ci revoked".to_string(),
            })
        );
    }

    #[test]
    fn image_commands_as_json() {
        insta::assert_snapshot!(
            "sign",
            json(&SignReport {
                image: "web:v1",
                digest: super::tests::MYAPP_V1,
                message: "signature attached".to_string(),
            })
        );
        insta::assert_snapshot!(
            "sign_keygen",
            json(&SigningKeyReport {
                path: Path::new("image.key"),
                public_key: "MCowBQYDK2VwAyEA".to_string(),
            })
        );
        insta::assert_snapshot!(
            "images_delete",
            json(&DeletedTag {
                repository: "team/web",
                tag: "v1",
            })
        );
        insta::assert_snapshot!(
            "build",
            json(&BuildReport {
                images: vec![BuiltImage {
                    name: "web".to_string(),
                    context_digest: super::tests::MYAPP_V1.to_string(),
                    build_id: 7,
                    image: "team/web:v1".to_string(),
                }],
            })
        );
    }

    #[test]
    fn batch_commands_as_json() {
        insta::assert_snapshot!(
            "submit_dry_run",
            json(&ManifestCheck {
                valid: true,
                profiles: 2,
                tasks: 40,
            })
        );
        insta::assert_snapshot!(
            "replay",
            json(&RunChange {
                run: 42,
                change: RunChangeKind::ReplayAcknowledged,
                node: Some("node-2"),
            })
        );
        insta::assert_snapshot!(
            "cancel",
            json(&RunChange {
                run: 42,
                change: RunChangeKind::Cancelled,
                node: None,
            })
        );
        insta::assert_snapshot!(
            "task_logs",
            json(&TaskOutput {
                run: 42,
                index: 3,
                output: "exit 1\n".to_string(),
            })
        );
        // `batch watch` is JSON Lines: one compact sample per line.
        let sample = WatchSample(serde_json::json!({"done": false, "total": 10}));
        assert_eq!(
            serde_json::to_string(&sample).unwrap(),
            r#"{"done":false,"total":10}"#
        );
    }

    /// #1566: a new token's secret is in the JSON, once, as it is on a
    /// terminal; nothing else that's secret is.
    #[test]
    fn issued_tokens_as_json_carry_the_secret_once() {
        let apps = vec!["web".to_string()];
        insta::assert_snapshot!(
            "token_create",
            json(&IssuedTokenReport {
                name: "ci",
                role: Some("deployer"),
                apps: Some(&apps),
                namespaces: None,
                token: "rbt_secret",
                expires_at: Some(1_800_000_000),
                previous_valid_until: None,
            })
        );
        insta::assert_snapshot!(
            "token_rotate",
            json(&IssuedTokenReport {
                name: "ci",
                role: None,
                apps: None,
                namespaces: None,
                token: "rbt_new_secret",
                expires_at: None,
                previous_valid_until: Some(1_790_000_000),
            })
        );
        insta::assert_snapshot!(
            "join_token_create",
            json(&JoinTokenReport {
                node_id: "node-02",
                ttl_secs: 900,
                token: "rbj_secret",
            })
        );
    }

    #[test]
    fn secrets_and_snapshots_as_json() {
        insta::assert_snapshot!(
            "pubkey",
            json(&PublicKey {
                namespace: Some("shop"),
                public_key: "age1xyz".to_string(),
            })
        );
        insta::assert_snapshot!(
            "encrypt",
            json(&Encrypted {
                encrypted: "ENC[AGE:abc]".to_string(),
            })
        );
        insta::assert_snapshot!(
            "snapshot_restore",
            json(&SnapshotChange {
                change: SnapshotChangeKind::Restored,
                app: "db",
                namespace: "default",
                snapshot: "nightly",
                volume: None,
            })
        );
    }
}
