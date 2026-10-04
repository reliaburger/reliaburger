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
use super::output::{OutputFormat, format_output};
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
) -> Result<(), RelishError> {
    apply_with_client(source, output, dry_run, &BunClient::default_local()).await
}

/// Read a manifest for `apply`, printing any migration report to stderr.
async fn load_manifest(source: &super::manifest::ManifestSource) -> Result<Config, RelishError> {
    let loaded = super::manifest::load(source).await?;
    if let Some(report) = &loaded.migration_report {
        eprint!("{report}");
        eprintln!();
    }
    loaded.config.validate_intrinsic()?;
    Ok(loaded.config)
}

/// Explicitly rerun a node-local job manifest, including unknown prior outcomes.
pub async fn rerun_jobs(source: &super::manifest::ManifestSource) -> Result<(), RelishError> {
    let config = load_manifest(source).await?;
    let result = BunClient::default_local()
        .apply_rerunning_jobs(&config)
        .await?;
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
            "applied {created} app(s); the scheduler places them now (watch with `relish status`)"
        )
    } else {
        format!("deployed {created} instance(s): {}", instances.join(", "))
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
        let current = match client.health().await {
            Ok(()) => Some(client.current_resources().await?),
            Err(_) => None,
        };
        let plan = generate_plan(&config, current.as_deref());
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
        Err(_) => {
            // X5: show the plan for reference, but fail — a dead agent
            // must not make a deploy look successful.
            let plan = generate_plan(&config, None);
            let formatted = format_output(&plan, output)?;
            println!("{formatted}");
            eprintln!("\nerror: bun agent not reachable — nothing was deployed");
            eprintln!("(use --dry-run to preview a plan without an agent)");
            Err(RelishError::AgentUnreachable)
        }
    }
}

/// Show cluster and app status.
pub async fn status(output: OutputFormat) -> Result<(), RelishError> {
    status_with_client(output, &BunClient::default_local()).await
}

async fn status_with_client(output: OutputFormat, client: &BunClient) -> Result<(), RelishError> {
    let statuses = client.cluster_status().await?;

    match output {
        OutputFormat::Human => {
            // One line of council health first: a split or fenced council
            // must not hide behind a healthy-looking instance list (#424).
            // A node list that can't be read just drops the header.
            if let Ok(nodes) = client.nodes().await
                && !nodes.is_empty()
            {
                let observations = crate::relish::council_view::survey_nodes(client, &nodes).await;
                let summary = crate::relish::council_view::summarise(&observations);
                print!(
                    "{}",
                    crate::relish::council_view::render_status_header(&summary)
                );
            }
            // The council knows why an app isn't placed. That is extra
            // detail: an agent that can't say still shows its instances.
            let desired = client.desired_apps().await.unwrap_or_default();
            print!("{}", render_status(&statuses, &desired));
        }
        OutputFormat::Json => {
            let json =
                serde_json::to_string_pretty(&statuses).map_err(RelishError::SerialiseJson)?;
            println!("{json}");
        }
        OutputFormat::Yaml => {
            let yaml = serde_yaml::to_string(&statuses).map_err(RelishError::SerialiseYaml)?;
            print!("{yaml}");
        }
    }

    Ok(())
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
pub async fn logs(name: &str, namespace: &str, flags: LogFlags) -> Result<(), RelishError> {
    let options = build_log_options(flags, unix_now())?;
    logs_with_client(name, namespace, &options, &BunClient::default_local()).await
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
fn parse_time(flag: &str, value: &str, now_epoch: u64) -> Result<u64, RelishError> {
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
fn unix_now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

async fn logs_with_client(
    name: &str,
    namespace: &str,
    options: &super::client::LogOptions,
    client: &BunClient,
) -> Result<(), RelishError> {
    let log_output = client.logs(name, namespace, options).await?;
    if !log_output.is_empty() {
        println!("{log_output}");
    }
    Ok(())
}

/// Export Parquet log files to a destination.
///
/// Asks the running Bun agent to export its LogStore (`POST
/// /v1/logs/export`): the destination is resolved agent-side (a path on the
/// agent host, `file://`, `s3://` or `gs://`) and the agent files the export
/// under its own node name — `--node-id` only affects the local fallback.
/// When no agent answers the health probe, falls back to reading the local
/// Parquet store directly, preferring the agent's default
/// `/var/lib/reliaburger/logs/parquet` and then the per-user data dir (the
/// same order bun itself uses); a custom `[storage] logs` path is only
/// reachable through the agent or an explicit `source` directory.
pub async fn logs_export(
    dest: &Path,
    node_id: &str,
    source: Option<&Path>,
) -> Result<(), RelishError> {
    let dest_str = dest.to_str().ok_or_else(|| RelishError::InvalidFlag {
        flag: "--dest".into(),
        reason: "destination must be valid UTF-8".into(),
    })?;
    if let Some(source) = source {
        return logs_export_from(source, dest_str, node_id).await;
    }
    let client = BunClient::default_local();
    if client.health().await.is_ok() {
        let outcome = client.logs_export(dest_str).await?;
        if outcome.files_exported == 0 {
            println!("no new files to export");
        } else {
            println!(
                "agent exported {} file(s) ({} bytes) to {}/{}",
                outcome.files_exported, outcome.bytes_written, dest_str, outcome.node_id,
            );
            if !outcome.checkpoint_saved {
                eprintln!(
                    "warning: the agent could not persist its export checkpoint; \
                     a later export may re-ship these files"
                );
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
    logs_export_from(&source, dest_str, node_id).await
}

async fn logs_export_from(source: &Path, dest_str: &str, node_id: &str) -> Result<(), RelishError> {
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
        Ok(result) => {
            if result.files_exported == 0 {
                println!("no new files to export");
            } else {
                println!(
                    "exported {} file(s) ({} bytes) to {}/{}",
                    result.files_exported, result.bytes_written, dest_str, node_id,
                );
            }
            Ok(())
        }
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
pub async fn logs_search(source: &str, sql: &str) -> Result<(), RelishError> {
    use crate::ketchup::remote_query::query_remote_json;

    match query_remote_json(source, sql).await {
        Ok(rows) => {
            if rows.is_empty() {
                println!("(no results)");
            } else {
                for row in &rows {
                    println!(
                        "{}",
                        serde_json::to_string(row).unwrap_or_else(|_| format!("{row:?}"))
                    );
                }
            }
            Ok(())
        }
        Err(e) => Err(RelishError::ApiError {
            status: 0,
            body: format!("search failed: {e}"),
        }),
    }
}

/// Execute a command inside a running container.
pub async fn exec(app: &str, command: &[String], namespace: &str) -> Result<(), RelishError> {
    exec_with_client(app, namespace, command, &BunClient::default_local()).await
}

async fn exec_with_client(
    app: &str,
    namespace: &str,
    command: &[String],
    client: &BunClient,
) -> Result<(), RelishError> {
    // The entry node only runs its own instances; exec on the node that
    // runs this app (through the entry node's relay on a laptop cluster).
    let target = super::path_cmd::find_source_client(client, app, namespace).await?;
    let output = target.exec(app, namespace, command).await?;
    if !output.is_empty() {
        print!("{output}");
    }
    Ok(())
}

/// Stop all instances of an app.
pub async fn stop(app: &str, namespace: &str) -> Result<(), RelishError> {
    stop_with_client(app, namespace, &BunClient::default_local()).await
}

async fn stop_with_client(
    app: &str,
    namespace: &str,
    client: &BunClient,
) -> Result<(), RelishError> {
    client.stop(app, namespace).await?;
    println!(
        "stop requested for {app}; check `relish status` for completion; `relish apply` starts it again"
    );
    Ok(())
}

/// Remove an app from the cluster.
pub async fn delete(app: &str, namespace: &str) -> Result<(), RelishError> {
    delete_with_client(app, namespace, &BunClient::default_local()).await
}

async fn delete_with_client(
    app: &str,
    namespace: &str,
    client: &BunClient,
) -> Result<(), RelishError> {
    client.delete(app, namespace).await?;
    println!("delete requested for {app}; check `relish status` for remaining instances");
    Ok(())
}

/// Show every instance of an app across the cluster, with its node.
pub async fn inspect(name: &str) -> Result<(), RelishError> {
    inspect_with_client(name, &BunClient::default_local()).await
}

async fn inspect_with_client(name: &str, client: &BunClient) -> Result<(), RelishError> {
    let inspection = super::inspect::collect(client, name).await?;
    print!("{}", super::inspect::render(&inspection));
    Ok(())
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

/// Initialise a new cluster with starter config files and PKI, using mTLS for
/// its internal transports.
pub fn init(dir: &Path, cluster_name: &str, node_id: &str) -> Result<(), RelishError> {
    init_with_security(dir, cluster_name, node_id, InitSecurityMode::MutualTls)
}

/// Initialise a new cluster with starter config files, PKI and an explicit
/// transport-security mode.
///
/// Creates `reliaburger.toml` (node config) and `app.toml` (sample app)
/// in the given directory. Generates the CA hierarchy, age keypair,
/// first node certificate, and a join token.
pub fn init_with_security(
    dir: &Path,
    cluster_name: &str,
    node_id: &str,
    security_mode: InitSecurityMode,
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

    // Generate the security state (CAs, age keypair, join token)
    let init_result = crate::sesame::init::initialize_cluster(cluster_name, node_id, dir)
        .map_err(|e| RelishError::InitFailed(e.to_string()))?;

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
    let output = crate::sesame::init::format_init_output(&init_result);
    eprint!("{output}");
    eprintln!("  Master secret:   {}", secret_path.display());
    eprintln!("  Security state:  {}", bootstrap_path.display());
    eprintln!("  Node identity:   {}", identity_dir.display());
    eprintln!("  Root CA:         {root_fingerprint}");
    eprintln!();
    eprintln!(
        "  Back up {}-master.key alongside the sealed root CA key.",
        cluster_name
    );
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
        toml::to_string_pretty(&node_config).expect("failed to serialise default node config")
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

    println!("created {}", node_path.display());
    println!("created {}", app_path.display());

    Ok(())
}

/// Assemble the first node's on-disk identity from the init result.
pub(super) fn node_identity_from_init(
    init_result: &crate::sesame::init::InitResult,
) -> Result<crate::sesame::identity_store::NodeIdentity, RelishError> {
    use crate::sesame::types::CaRole;

    let state = &init_result.security_state;
    let node_ca = state.get_ca(CaRole::Node).ok_or_else(|| {
        RelishError::InitFailed("security state is missing the Node CA".to_string())
    })?;
    let root_ca = state.get_ca(CaRole::Root).ok_or_else(|| {
        RelishError::InitFailed("security state is missing the root CA".to_string())
    })?;

    let cert = &init_result.node_certificate;
    Ok(crate::sesame::identity_store::NodeIdentity {
        node_id: cert.node_id.clone(),
        certificate_der: cert.certificate_der.clone(),
        private_key_der: cert.private_key_der.clone(),
        serial: cert.serial,
        ca_generation: cert.ca_generation,
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
            serde_yaml::to_string(&retirement).map_err(RelishError::SerialiseYaml)?
        ),
    }
    Ok(())
}

/// List cluster nodes and their gossip state.
pub async fn nodes(output: OutputFormat) -> Result<(), RelishError> {
    nodes_with_client(output, &BunClient::default_local()).await
}

async fn nodes_with_client(output: OutputFormat, client: &BunClient) -> Result<(), RelishError> {
    let nodes = client.nodes().await?;

    if nodes.is_empty() {
        println!("no cluster nodes (single-node mode)");
    } else {
        match output {
            OutputFormat::Human => {
                println!(
                    "{:<20} {:<22} {:<10} {:<8} {:<8}",
                    "NODE", "ADDRESS", "STATE", "COUNCIL", "LEADER"
                );
                for n in &nodes {
                    println!(
                        "{:<20} {:<22} {:<10} {:<8} {:<8}",
                        n.node_id,
                        n.address,
                        n.state,
                        if n.is_council { "yes" } else { "-" },
                        if n.is_leader { "yes" } else { "-" },
                    );
                }
            }
            OutputFormat::Json => {
                let json =
                    serde_json::to_string_pretty(&nodes).map_err(RelishError::SerialiseJson)?;
                println!("{json}");
            }
            OutputFormat::Yaml => {
                let yaml = serde_yaml::to_string(&nodes).map_err(RelishError::SerialiseYaml)?;
                print!("{yaml}");
            }
        }
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
) -> Result<(), RelishError> {
    let base = normalise_member_base(addr);
    let ca_url = format!("{base}/v1/cluster/ca");
    let member_url = format!("{base}/v1/cluster/join");
    let dir = identity_dir
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("identity"));

    // Phase 1 — fetch the cluster's public CA over trust-on-first-use. This
    // reveals no secret; a joiner has no CA yet, so it cannot verify the
    // member's certificate on this first contact. If a fingerprint was pinned,
    // a mismatch is refused *here*, before the one-time token is ever sent — so
    // a man-in-the-middle cannot capture and replay a token the real cluster
    // never consumed.
    let tofu_client = reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|e| RelishError::JoinFailed(e.to_string()))?;

    let ca = crate::sesame::join::fetch_ca(&tofu_client, &ca_url)
        .await
        .map_err(|e| RelishError::JoinFailed(e.to_string()))?;
    let offered = ca
        .root_ca_fingerprint()
        .map_err(|e| RelishError::JoinFailed(e.to_string()))?;
    if let Some(expected) = ca_fingerprint
        && offered != expected
    {
        return Err(RelishError::JoinFailed(format!(
            "root CA fingerprint mismatch: member offered {offered}, expected {expected} — \
             refusing to send the join token"
        )));
    }
    let (node_ca_der, root_ca_der) = ca
        .decode()
        .map_err(|e| RelishError::JoinFailed(e.to_string()))?;

    // Phase 2 — send the token only over a connection whose server certificate
    // is cryptographically verified to chain to the CA we just fetched (and, if
    // pinned, fingerprint-checked). A man-in-the-middle without the cluster's
    // key cannot present such a chain, so the handshake fails before the token
    // leaves this node. `request_join` re-checks the pinned fingerprint against
    // the returned bundle as defence in depth.
    let pinned_client = crate::sesame::mtls::build_ca_pinned_client(node_ca_der, root_ca_der)
        .map_err(|e| RelishError::JoinFailed(e.to_string()))?;

    let identity = crate::sesame::join::request_join(
        &pinned_client,
        &member_url,
        token,
        node_id,
        ca_fingerprint,
    )
    .await
    .map_err(|e| RelishError::JoinFailed(e.to_string()))?;

    let fingerprint = crate::sesame::identity_store::root_ca_fingerprint(&identity.root_ca_der);
    crate::sesame::identity_store::save(&dir, &identity)
        .map_err(|e| RelishError::JoinFailed(format!("failed to persist identity: {e}")))?;

    println!("joined as {node_id}: identity written to {}", dir.display());
    println!("  cluster root CA: {fingerprint}");
    println!("  restart bun to bring up mTLS with the new identity.");
    Ok(())
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
            let yaml = serde_yaml::to_string(&report).map_err(RelishError::SerialiseYaml)?;
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
            let yaml = serde_yaml::to_string(council).map_err(RelishError::SerialiseYaml)?;
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
/// epoch. The next start
/// re-bootstraps a single-voter council the reconciler regrows.
pub async fn council_recover(
    data_dir: &std::path::Path,
    from: Option<&str>,
    master_key_path: Option<&std::path::Path>,
    force: bool,
) -> Result<(), RelishError> {
    use crate::council::recovery::{RecoverySource, load_recovery_state, recover_data_dir};

    // Safety check: refuse if a live council still answers, unless forced. In a
    // genuine full-council loss the local agent is down, so this check simply
    // passes; it only bites when someone runs recovery against a healthy
    // cluster by mistake.
    if !force {
        if let Ok(nodes) = BunClient::default_local().nodes().await {
            let live_voter = nodes.iter().find(|n| n.is_council && !n.is_down());
            if let Some(voter) = live_voter {
                return Err(RelishError::Recovery(format!(
                    "a live council voter ({}) is still reachable; stop the cluster or pass --force",
                    voter.node_id
                )));
            }
        }
    } else {
        eprintln!(
            "WARNING: --force skips the live-council check. Recovering a cluster that still has a \
             quorum will split the brain. Continue only if every voter is truly gone."
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
        Some(url) => RecoverySource::BackupUrl(url.to_string()),
        None => RecoverySource::NodeDataDir(data_dir.to_path_buf()),
    };

    let state = load_recovery_state(&source, master_key.as_ref())
        .await
        .map_err(|e| RelishError::Recovery(e.to_string()))?;
    let app_count = state.apps.len();
    let token_count = state.security_state.api_tokens.len();
    let prior_epoch = state.recovery_epoch;

    recover_data_dir(data_dir, state).map_err(|e| RelishError::Recovery(e.to_string()))?;

    println!("Council recovery complete.");
    println!("  Restored apps:   {app_count}");
    println!("  API tokens:      {token_count}");
    println!("  Recovery epoch:  {} -> {}", prior_epoch, prior_epoch + 1);
    println!("  Data directory:  {}", data_dir.display());
    println!();
    println!("Start this node to re-bootstrap a single-voter council; the reconciler");
    println!("will regrow it from surviving members. Writes after the last backup are lost.");
    println!();
    println!("The voters this council replaces still hold the old one. If they come back where");
    println!("they can see this node, they fence themselves; if they can see each other but not");
    println!("this node, nothing can tell them. Re-enrol each one before starting it:");
    println!("  relish council re-enrol --data-dir <its data directory>");
    Ok(())
}

/// Re-enrol a voter fenced out by `council recover` (#424): offline, against
/// a stopped node.
pub fn council_reenrol(data_dir: &std::path::Path, force: bool) -> Result<(), RelishError> {
    let fenced_by = crate::council::recovery::reenrol_data_dir(data_dir, force)
        .map_err(|e| RelishError::Recovery(e.to_string()))?;
    match fenced_by {
        Some(epoch) => {
            println!("Removed the Raft state of a council replaced at recovery epoch {epoch}.")
        }
        None => println!("Removed this node's Raft state."),
    }
    println!("  Data directory:  {}", data_dir.display());
    println!();
    println!("Start the node with `cluster.join` pointing at the current council. It joins as");
    println!("a fresh member, adopts the council's recovery epoch, and the reconciler may");
    println!("promote it to voter.");
    Ok(())
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
pub async fn resolve(name: &str) -> Result<(), RelishError> {
    resolve_with_client(name, &BunClient::default_local()).await
}

async fn resolve_with_client(name: &str, client: &BunClient) -> Result<(), RelishError> {
    let info = client.resolve(name).await?;

    println!("Service:  {}", info.app_name);
    println!("VIP:      {}", info.vip);
    println!("Port:     {}", info.port);
    println!(
        "Backends: {}/{} healthy",
        info.healthy_backends, info.total_backends
    );

    if !info.backends.is_empty() {
        println!();
        println!(
            "  {:<20} {:<18} {:<8} {:<8}",
            "INSTANCE", "NODE", "PORT", "HEALTH"
        );
        for b in &info.backends {
            let health = if b.healthy { "healthy" } else { "unhealthy" };
            println!(
                "  {:<20} {:<18} {:<8} {:<8}",
                b.instance_id, b.node_ip, b.host_port, health
            );
        }
    }

    Ok(())
}

/// Show ingress routing table.
pub async fn routes() -> Result<(), RelishError> {
    routes_with_client(&BunClient::default_local()).await
}

async fn routes_with_client(client: &BunClient) -> Result<(), RelishError> {
    let routes = client.routes().await?;

    if routes.is_empty() {
        println!("no ingress routes configured");
    } else {
        println!(
            "{:<30} {:<10} {:<15} {:<12} {:<6}",
            "HOST", "PATH", "APP", "BACKENDS", "WS"
        );
        for r in &routes {
            let backends = format!("{}/{}", r.healthy_backends, r.total_backends);
            let ws = if r.websocket { "yes" } else { "no" };
            println!(
                "{:<30} {:<10} {:<15} {:<12} {:<6}",
                r.host, r.path, r.app_name, backends, ws
            );
        }
    }

    Ok(())
}

/// Trigger a rolling deploy from a config file.
///
/// Parses the config, sends it to the agent for a rolling deploy
/// (if the app already exists, the agent performs a rolling update).
/// With `--dry-run`, prints the plan and exits 0 without deploying;
/// otherwise an unreachable agent is an error (X5).
pub async fn deploy(path: &Path, output: OutputFormat, dry_run: bool) -> Result<(), RelishError> {
    let config = Config::from_file(path)?;
    config.validate_intrinsic()?;

    let client = BunClient::default_local();

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
                        serde_yaml::to_string(&result).map_err(RelishError::SerialiseYaml)?;
                    print!("{yaml}");
                }
            }
            Ok(())
        }
        Err(_) => {
            let plan = generate_plan(&config, None);
            let formatted = format_output(&plan, super::OutputFormat::Human)?;
            println!("{formatted}");
            eprintln!("\nerror: bun agent not reachable — nothing was deployed");
            eprintln!("(use --dry-run to preview a plan without an agent)");
            Err(RelishError::AgentUnreachable)
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
            serde_yaml::to_string(&operation).map_err(RelishError::SerialiseYaml)?
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
            let yaml = serde_yaml::to_string(&entries).map_err(RelishError::SerialiseYaml)?;
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

/// Rollback an app to the previous version.
pub async fn rollback(app: &str, namespace: &str) -> Result<(), RelishError> {
    let client = BunClient::default_local();
    client.health().await?;
    // X3: rollback used to only print advice. It now calls the server,
    // which redeploys the previous successful spec.
    client.rollback(app, namespace).await?;
    println!("rolled {app} back to its previous version");
    Ok(())
}

/// Validate a config file without deploying.
pub fn lint(path: &Path) -> Result<(), RelishError> {
    let config = Config::from_file(path)?;
    config.validate()?;

    // Count resources
    let app_count = config.app.len();
    let job_count = config.job.len();

    // Validate run_before references
    for (name, job) in &config.job {
        for target in &job.run_before {
            let target_exists = config
                .app
                .keys()
                .any(|app_name| format!("app.{app_name}") == *target);
            if !target_exists {
                eprintln!(
                    "warning: job {name} has run_before target {target:?} which doesn't exist in this config"
                );
            }
        }
    }

    println!("config valid: {app_count} app(s), {job_count} job(s)");
    Ok(())
}

/// Compile a config file or directory into a single resolved config.
pub fn compile(path: &Path) -> Result<(), RelishError> {
    let result = super::compile::compile(path)?;

    if !result.warnings.is_empty() {
        for w in &result.warnings {
            eprintln!("warning: {w}");
        }
    }

    let app_count = result.config.app.len();
    let job_count = result.config.job.len();
    let file_count = result.merged_from.len();

    // Serialise the merged config as TOML
    let toml = toml::to_string_pretty(&result.config)
        .map_err(|e| RelishError::FormatFailed(e.to_string()))?;
    print!("{toml}");

    eprintln!("compiled {file_count} file(s): {app_count} app(s), {job_count} job(s)");
    Ok(())
}

/// Show structural diff between two configs.
pub fn diff(path_a: &Path, path_b: Option<&Path>) -> Result<(), RelishError> {
    let old = Config::from_file(path_a)?;
    let new = match path_b {
        Some(p) => Config::from_file(p)?,
        None => Config::default(),
    };
    let diff = super::diff::diff_configs(&old, &new);

    if diff.is_empty() {
        println!("no changes");
    } else {
        print!("{diff}");
    }
    Ok(())
}

/// Format a TOML config file with canonical ordering.
pub fn fmt(path: &Path, check: bool) -> Result<(), RelishError> {
    let content = fs::read_to_string(path)?;

    if check {
        if super::fmt::is_formatted(&content)? {
            println!("{}: ok", path.display());
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

    // O10: comment loss is by design (the formatter round-trips through the
    // `toml` crate's typed representation), but silently eating an
    // operator's annotations is not. Say so, once, when there was something
    // to lose.
    if content
        .lines()
        .any(|line| line.trim_start().starts_with('#'))
    {
        eprintln!(
            "warning: {} had comments; the formatter round-trips through TOML's \
             typed representation, which discards them",
            path.display()
        );
    }

    // O10: write atomically. `fs::write` truncates first, so an interrupted
    // or failing write leaves a half-file where a valid config used to be —
    // and this is a config the node reads on startup. A temp file beside the
    // target (same filesystem, so the rename is atomic) means the config is
    // either the old one or the new one, never neither.
    write_atomically(path, formatted.as_bytes())?;
    println!("formatted {}", path.display());
    Ok(())
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
pub fn import_k8s(files: &[std::path::PathBuf], strict: bool) -> Result<(), RelishError> {
    let result = super::k8s_import::import_kubernetes(files)?;

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

    if strict && !result.report.warnings.is_empty() {
        return Err(RelishError::FormatFailed(
            "import produced warnings (--strict mode)".to_string(),
        ));
    }

    Ok(())
}

/// Export Reliaburger TOML to Kubernetes YAML manifests.
#[cfg(feature = "kubernetes")]
pub fn export_k8s(file: &Path) -> Result<(), RelishError> {
    let config = Config::from_file(file)?;
    let result = super::k8s_export::export_kubernetes(&config)?;

    print!("{}", result.yaml);

    if !result.report.resources_created.is_empty()
        || !result.report.unsupported.is_empty()
        || !result.report.dropped.is_empty()
    {
        eprint!("{}", result.report);
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
            let yaml = serde_yaml::to_string(&top.rows).map_err(RelishError::SerialiseYaml)?;
            print!("{yaml}");
        }
    }

    Ok(())
}

/// The human `relish status` output: every instance with its node, then each
/// app the scheduler won't place and why (#326). Without that line an
/// over-quota app is just missing from the table.
fn render_status(
    statuses: &[crate::bun::agent::ClusterInstanceStatus],
    desired: &[crate::bun::diagnostics::DesiredAppEvidence],
) -> String {
    use std::fmt::Write as _;

    let mut output = String::new();
    if statuses.is_empty() {
        output.push_str("no workloads running\n");
    } else {
        let _ = writeln!(
            output,
            "{:<24} {:<20} {:<15} {:<12} {:<10} {:<10} {:<6}",
            "NODE", "INSTANCE", "APP", "NAMESPACE", "STATE", "PID", "RESTARTS"
        );
        for row in statuses {
            let s = &row.instance;
            let _ = writeln!(
                output,
                "{:<24} {:<20} {:<15} {:<12} {:<10} {:<10} {:<6}",
                row.node,
                s.id,
                s.app_name,
                s.namespace,
                s.state,
                pid_cell(s),
                s.restart_count
            );
        }
    }
    let blocked: Vec<_> = desired
        .iter()
        .filter_map(|app| app.blocked.as_ref().map(|reason| (app, reason)))
        .collect();
    let waiting: Vec<_> = desired
        .iter()
        .filter_map(|app| app.volume_home_away.as_ref().map(|home| (app, home)))
        .collect();
    if !blocked.is_empty() || !waiting.is_empty() {
        output.push('\n');
    }
    for (app, reason) in blocked {
        let _ = writeln!(
            output,
            "{} (namespace {}) is not placed, blocked: {reason}",
            app.app, app.namespace
        );
    }
    // #423: the app isn't lost, it waits for the node holding its data.
    for (app, home) in waiting {
        let _ = writeln!(
            output,
            "{} (namespace {}) waits for {home}, which holds its volume and is out of the cluster",
            app.app, app.namespace
        );
    }
    output
}

/// An instance's PID for a table: `-` when it has none, `?` when the node's
/// runtime didn't answer in time.
fn pid_cell(status: &crate::bun::agent::InstanceStatus) -> String {
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
        let pid = pid_cell(&row.instance);
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
pub async fn secret_rotate(finalize: bool, namespace: Option<&str>) -> Result<(), RelishError> {
    let client = BunClient::default_local();
    let result = client.secret_rotate(finalize, namespace).await?;
    println!("{result}");
    Ok(())
}

/// Sign a Pickle-hosted image with the operator's key and attach the
/// signature. A tag is resolved to its manifest digest first, and the
/// digest is what gets signed: a tag can move, a digest can't.
pub async fn sign(image: &str, key_path: &Path) -> Result<(), RelishError> {
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
    let result = client.sign_image(&submission).await?;
    println!("{result}");
    Ok(())
}

/// Generate an image signing key at `out` (PKCS#8 PEM, owner-only
/// permissions) and print the public key in the form
/// `[images.trust_policy] keys` expects.
pub fn sign_keygen(out: &Path) -> Result<(), RelishError> {
    let key = crate::pickle::signing::SigningKey::generate()?;
    write_private_key(out, key.to_pem().as_bytes())?;
    let public_key = key.public_key_base64();
    println!("wrote image signing key to {}", out.display());
    println!("public key: {public_key}");
    println!();
    println!("Trust it by adding this to every node's config:");
    println!();
    println!("[images.trust_policy]");
    println!("require_signatures = true");
    println!("keys = [\"{public_key}\"]");
    Ok(())
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

pub async fn images(output: OutputFormat) -> Result<(), RelishError> {
    let client = BunClient::default_local();
    let result = client.images().await?;
    if let OutputFormat::Json | OutputFormat::Yaml = output {
        let text = match output {
            OutputFormat::Json => {
                serde_json::to_string_pretty(&result).map_err(RelishError::SerialiseJson)?
            }
            _ => serde_yaml::to_string(&result).map_err(RelishError::SerialiseYaml)?,
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
pub async fn build(
    path: &std::path::Path,
    registry_port: Option<u16>,
    timeout_secs: u64,
) -> Result<(), RelishError> {
    use crate::config::Config;
    use crate::pickle::build::{cli_context_upload_url, digest_of, execute_build, tar_context};

    let config = Config::from_file(path)?;
    if config.build.is_empty() {
        eprintln!("no [build.*] sections found in {}", path.display());
        return Ok(());
    }

    let client = BunClient::default_local();

    for (name, spec) in &config.build {
        println!("Building {name}...");

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
        println!(
            "  context: {} ({} bytes, {digest})",
            context_path.display(),
            tar_bytes.len()
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
        println!("  context uploaded to Pickle");

        // Prepare the build job (for display; the agent re-derives it with
        // its own registry port, which a host forward doesn't change).
        let job = execute_build(spec, &digest, None).map_err(|e| RelishError::ApiError {
            status: 0,
            body: format!("build preparation failed: {e}"),
        })?;

        for line in build_plan_lines(&job) {
            println!("{line}");
        }

        // Submit and poll: builds run async on the builder node —
        // minutes-long buildah runs must not hold an HTTP request open.
        // The wait is bounded (JOB6): a stuck build ends with a
        // non-zero exit and the last known state, not an infinite loop.
        let build_id = client.submit_build(name, &digest, spec).await?;
        println!("  build {build_id} accepted; waiting (up to {timeout_secs}s)...");
        let image = wait_for_build(
            &client,
            build_id,
            std::time::Duration::from_secs(timeout_secs),
        )
        .await?;
        println!("  built and pushed {image}");
    }

    Ok(())
}

/// Submit a batch of jobs for high-throughput scheduling.
pub async fn batch(path: &std::path::Path) -> Result<(), RelishError> {
    use crate::config::Config;

    let config = Config::from_file(path)?;
    if config.job.is_empty() {
        eprintln!("no [job.*] sections found in {}", path.display());
        return Ok(());
    }

    let client = BunClient::default_local();
    let result = client.submit_batch(&config.job).await?;
    println!(
        "batch {} submitted: {} assigned",
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
pub async fn batch_status(batch_id: u64, wait: bool, timeout_secs: u64) -> Result<(), RelishError> {
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
    print_batch_summary(batch_id, &summary);
    Ok(())
}

fn print_batch_summary(batch_id: u64, summary: &serde_json::Value) {
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

/// Create a new API token through the agent.
///
/// The agent mints the token, stores its Argon2id hash in Raft, and
/// returns the plaintext once; this prints it to stdout and never stores
/// it. Needs a reachable agent and an admin credential.
pub async fn token_create(
    name: &str,
    role_str: &str,
    apps: Option<&str>,
    namespaces: Option<&str>,
    ttl_days: Option<u64>,
) -> Result<(), RelishError> {
    token_create_with_client(
        name,
        role_str,
        apps,
        namespaces,
        ttl_days,
        &BunClient::default_local(),
    )
    .await
}

/// Create a token via the agent so it's persisted in Raft. The token is minted
/// and hashed server-side; the plaintext is returned once and printed to
/// stdout. An unreachable agent is an error (never a silent exit-0), and the
/// role is validated server-side.
async fn token_create_with_client(
    name: &str,
    role_str: &str,
    apps: Option<&str>,
    namespaces: Option<&str>,
    ttl_days: Option<u64>,
    client: &BunClient,
) -> Result<(), RelishError> {
    let apps_vec = apps.map(|a| a.split(',').map(|s| s.trim().to_string()).collect());
    let namespaces_vec = namespaces.map(|n| n.split(',').map(|s| s.trim().to_string()).collect());

    let plaintext = client
        .token_create(name, role_str, apps_vec, namespaces_vec, ttl_days)
        .await?;

    eprintln!("Token created: {name}");
    eprintln!("  Role: {role_str}");
    if let Some(apps) = apps {
        eprintln!("  Apps: {apps}");
    }
    if let Some(namespaces) = namespaces {
        eprintln!("  Namespaces: {namespaces}");
    }
    if let Some(days) = ttl_days {
        eprintln!("  TTL: {days} days");
    }
    eprintln!();
    println!("{plaintext}");

    Ok(())
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
pub async fn secret_pubkey(dir: Option<&Path>, namespace: Option<&str>) -> Result<(), RelishError> {
    let key = match dir {
        Some(dir) => resolve_secret_pubkey(dir)?,
        None => fetch_secret_pubkey(&BunClient::default_local(), namespace).await?,
    };
    println!("{key}");
    Ok(())
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
pub fn secret_encrypt(pubkey: &str, value: &str) -> Result<(), RelishError> {
    use crate::sesame::secret::encrypt_secret;

    let encrypted =
        encrypt_secret(value, pubkey).map_err(|e| RelishError::InitFailed(e.to_string()))?;

    println!("{encrypted}");
    Ok(())
}

/// List API tokens via the agent, with every node's last use merged in.
pub async fn token_list(output: OutputFormat) -> Result<(), RelishError> {
    let listing = BunClient::default_local().token_list().await?;
    // A member that didn't answer may hold a more recent use; say so on
    // stderr so `-o json` stays parseable.
    for warning in &listing.warnings {
        eprintln!("warning: last use incomplete: {warning}");
    }
    match output {
        OutputFormat::Human => {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            print!("{}", render_token_list(&listing.tokens, now));
        }
        OutputFormat::Json => println!(
            "{}",
            serde_json::to_string_pretty(&listing).map_err(RelishError::SerialiseJson)?
        ),
        OutputFormat::Yaml => print!(
            "{}",
            serde_yaml::to_string(&listing).map_err(RelishError::SerialiseYaml)?
        ),
    }
    Ok(())
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
pub async fn token_revoke(name: &str) -> Result<(), RelishError> {
    let client = BunClient::default_local();
    let result = client.token_revoke(name).await?;
    println!("{result}");
    Ok(())
}

/// Mint a short-lived, single-use node join token through the council.
pub async fn join_token_create(node_id: &str, ttl_seconds: u64) -> Result<(), RelishError> {
    let client = BunClient::default_local();
    let plaintext = client.join_token_create(node_id, ttl_seconds).await?;
    eprintln!(
        "Join token created for {node_id} (expires in {}).",
        format_ttl(ttl_seconds)
    );
    eprintln!("The joining node must enrol with `relish join --node-id {node_id}`.");
    eprintln!("The plaintext is shown once:");
    println!("{plaintext}");
    Ok(())
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
) -> Result<(), RelishError> {
    let client = BunClient::default_local();
    let metas = client.snapshot_create(app, namespace, volume, name).await?;
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
pub async fn snapshot_list(app: &str, namespace: &str) -> Result<(), RelishError> {
    let client = BunClient::default_local();
    let metas = client.snapshot_list(app, namespace).await?;
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
) -> Result<(), RelishError> {
    let client = BunClient::default_local();
    client
        .snapshot_restore(app, namespace, name, volume)
        .await?;
    println!("restored {namespace}/{app} from snapshot {name}");
    Ok(())
}

/// Delete a snapshot. `volume` picks between volumes that share the
/// snapshot name.
pub async fn snapshot_delete(
    app: &str,
    namespace: &str,
    name: &str,
    volume: Option<&str>,
) -> Result<(), RelishError> {
    let client = BunClient::default_local();
    client.snapshot_delete(app, namespace, name, volume).await?;
    println!("deleted snapshot {name} of {namespace}/{app}");
    Ok(())
}

#[cfg(test)]
mod tests {
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
            "applied 4 app(s); the scheduler places them now (watch with `relish status`)"
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
            },
            cpu_percent: cpu,
            memory_bytes: memory,
        }
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
            "[build.web]\ncontext = '.'\ndestination = 'pickle://web:v1'\nnamespace = 'existing'\n",
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
            "[build.web]\ncontext = '.'\ndestination = 'pickle://web:v1'\nnamespace = 'existing'\n",
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
            "[build.web]\ncontext = '.'\ndestination = 'pickle://web:v1'\nnamespace = 'Existing'\n",
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

    #[test]
    fn secret_pubkey_prints_age_key_from_bootstrap() {
        let dir = tempfile::tempdir().unwrap();
        // `relish init` writes the bootstrap file from the init result; mimic it.
        let init =
            crate::sesame::init::initialize_cluster("pubkeytest", "node-1", dir.path()).unwrap();
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

    #[tokio::test]
    async fn token_create_errors_when_agent_unreachable() {
        let result =
            token_create_with_client("ci-bot", "deployer", None, None, None, &bogus_client()).await;
        assert!(result.is_err(), "unreachable agent must be an error");
    }

    fn write_temp_config(content: &str) -> tempfile::NamedTempFile {
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
        assert!(matches!(err, RelishError::AgentUnreachable), "got: {err:?}");
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

    fn evidence(app: &str, replicas: u32) -> crate::bun::diagnostics::DesiredAppEvidence {
        crate::bun::diagnostics::DesiredAppEvidence {
            app: app.to_string(),
            namespace: "prod".to_string(),
            desired_replicas: replicas,
            scheduled_replicas: 0,
            placements: Default::default(),
            service_port: None,
            blocked: None,
            volume_home_away: None,
            volume_homes: Vec::new(),
        }
    }

    /// #326: an app the namespace quota keeps off every node has no
    /// instance row, so `relish status` names it and says why.
    #[test]
    fn status_names_an_over_quota_app_and_why() {
        let blocked = crate::bun::diagnostics::DesiredAppEvidence {
            blocked: Some(crate::meat::quota::QuotaError::CpuExceeded {
                namespace: "prod".to_string(),
                current: 0,
                requested: 1600,
                limit: 1000,
            }),
            ..evidence("greedy", 2)
        };
        let output = render_status(&[], &[evidence("fine", 1), blocked]);
        assert_eq!(
            output,
            "no workloads running\n\n\
             greedy (namespace prod) is not placed, blocked: \
             namespace \"prod\" would exceed CPU quota: 0+1600 > 1000m\n"
        );
    }

    /// #423: an app waiting for the node that holds its volume isn't just
    /// missing from the table; status says which node it waits for.
    #[test]
    fn status_names_a_volume_app_waiting_for_its_home_node() {
        let waiting = crate::bun::diagnostics::DesiredAppEvidence {
            volume_home_away: Some("node-2".to_string()),
            ..evidence("db", 1)
        };
        let output = render_status(&[], &[evidence("fine", 1), waiting]);
        assert_eq!(
            output,
            "no workloads running\n\n\
             db (namespace prod) waits for node-2, which holds its volume and is out of the \
             cluster\n"
        );
    }

    #[test]
    fn status_without_blocked_apps_is_just_the_table() {
        assert_eq!(
            render_status(&[], &[evidence("fine", 1)]),
            "no workloads running\n"
        );
    }

    #[tokio::test]
    async fn status_returns_agent_unreachable() {
        let err = status_with_client(OutputFormat::Human, &bogus_client())
            .await
            .unwrap_err();
        assert!(matches!(err, RelishError::AgentUnreachable), "got: {err:?}");
    }

    #[tokio::test]
    async fn logs_returns_agent_unreachable() {
        let options = super::super::client::LogOptions::default();
        let err = logs_with_client("web", "default", &options, &bogus_client())
            .await
            .unwrap_err();
        assert!(matches!(err, RelishError::AgentUnreachable));
    }

    #[tokio::test]
    async fn exec_returns_agent_unreachable() {
        let err = exec_with_client("web", "default", &["sh".to_string()], &bogus_client())
            .await
            .unwrap_err();
        assert!(matches!(err, RelishError::AgentUnreachable));
    }

    #[tokio::test]
    async fn stop_returns_agent_unreachable() {
        let err = stop_with_client("web", "default", &bogus_client())
            .await
            .unwrap_err();
        assert!(matches!(err, RelishError::AgentUnreachable));
    }

    #[test]
    fn init_creates_files() {
        let dir = tempfile::tempdir().unwrap();
        init(dir.path(), "test-cluster", "node-01").unwrap();
        assert!(dir.path().join("reliaburger.toml").exists());
        assert!(dir.path().join("app.toml").exists());
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

    #[test]
    fn init_creates_sealed_root_ca() {
        let dir = tempfile::tempdir().unwrap();
        init(dir.path(), "mycluster", "node-01").unwrap();
        assert!(dir.path().join("mycluster-root-ca.age").exists());
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
        init_with_security(
            dir.path(),
            "devcluster",
            "node-01",
            InitSecurityMode::DevelopmentPlaintext,
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
        let err = inspect_with_client("web", &bogus_client())
            .await
            .unwrap_err();
        assert!(matches!(err, RelishError::AgentUnreachable));
    }

    #[tokio::test]
    async fn nodes_returns_agent_unreachable() {
        let err = nodes_with_client(OutputFormat::Human, &bogus_client())
            .await
            .unwrap_err();
        assert!(matches!(err, RelishError::AgentUnreachable), "got: {err:?}");
    }

    #[tokio::test]
    async fn council_returns_agent_unreachable() {
        let err = council_with_client(OutputFormat::Human, &bogus_client())
            .await
            .unwrap_err();
        assert!(matches!(err, RelishError::AgentUnreachable), "got: {err:?}");
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
        lint(f.path()).unwrap();
    }

    #[test]
    fn lint_invalid_config() {
        let f = write_temp_config(
            r#"
            [app.broken]
            replicas = 3
        "#,
        );
        let result = lint(f.path());
        assert!(result.is_err());
    }

    #[test]
    fn lint_missing_file() {
        let result = lint(Path::new("/nonexistent/config.toml"));
        assert!(result.is_err());
    }

    // --- relish sign ---

    const MYAPP_V1: &str =
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
        sign_keygen(&path).unwrap();

        let key = crate::pickle::signing::SigningKey::from_pem(&fs::read_to_string(&path).unwrap());
        assert!(key.is_ok(), "the written key must load back: {key:?}");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
        assert!(matches!(
            sign_keygen(&path),
            Err(RelishError::FileExists { .. })
        ));
    }
}
