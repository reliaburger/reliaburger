/// Command executors for the Relish CLI.
///
/// Each subcommand is an async function that returns `Result<(), RelishError>`.
/// Commands try to reach the live Bun agent first. If the agent is
/// unreachable, `apply` falls back to a dry-run plan.
use std::fs;
use std::path::Path;

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
pub async fn apply(path: &Path, output: OutputFormat, dry_run: bool) -> Result<(), RelishError> {
    apply_with_client(path, output, dry_run, &BunClient::default_local()).await
}

async fn apply_with_client(
    path: &Path,
    output: OutputFormat,
    dry_run: bool,
    client: &BunClient,
) -> Result<(), RelishError> {
    let config = Config::from_file(path)?;
    config.validate()?;

    if dry_run {
        let plan = generate_plan(&config, None);
        let formatted = format_output(&plan, output)?;
        println!("{formatted}");
        println!("\n(dry run — nothing deployed)");
        return Ok(());
    }

    match client.health().await {
        Ok(()) => {
            // Agent is alive — send the config (progress streams to stderr)
            let result = client.apply(&config).await?;
            println!(
                "deployed {} instance(s): {}",
                result.created,
                result.instances.join(", ")
            );
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
pub async fn status() -> Result<(), RelishError> {
    status_with_client(&BunClient::default_local()).await
}

async fn status_with_client(client: &BunClient) -> Result<(), RelishError> {
    let statuses = client.status().await?;

    if statuses.is_empty() {
        println!("no workloads running");
    } else {
        println!(
            "{:<20} {:<15} {:<12} {:<10} {:<10} {:<6}",
            "INSTANCE", "APP", "NAMESPACE", "STATE", "PID", "RESTARTS"
        );
        for s in &statuses {
            let pid = s
                .pid
                .map(|p| p.to_string())
                .unwrap_or_else(|| "-".to_string());
            println!(
                "{:<20} {:<15} {:<12} {:<10} {:<10} {:<6}",
                s.id, s.app_name, s.namespace, s.state, pid, s.restart_count
            );
        }
    }

    Ok(())
}

/// Stream logs from an app or job.
pub async fn logs(
    name: &str,
    tail: Option<usize>,
    follow: bool,
    grep: Option<String>,
    since: Option<String>,
    json_field: Option<String>,
) -> Result<(), RelishError> {
    let options = build_log_options(tail, follow, grep, since, json_field, unix_now())?;
    logs_with_client(name, &options, &BunClient::default_local()).await
}

/// Translate the CLI flags into [`LogOptions`], validating as we go.
fn build_log_options(
    tail: Option<usize>,
    follow: bool,
    grep: Option<String>,
    since: Option<String>,
    json_field: Option<String>,
    now_epoch: u64,
) -> Result<super::client::LogOptions, RelishError> {
    let start = match since {
        Some(s) => Some(parse_since(&s, now_epoch)?),
        None => None,
    };
    let json_field = match json_field {
        Some(s) => Some(parse_json_field(&s)?),
        None => None,
    };
    Ok(super::client::LogOptions {
        tail,
        follow,
        grep,
        start,
        json_field,
    })
}

/// Parse a `--since` value: raw epoch seconds, or a duration like
/// `30s`, `5m`, `2h`, `1d` subtracted from now.
fn parse_since(value: &str, now_epoch: u64) -> Result<u64, RelishError> {
    if value.chars().all(|c| c.is_ascii_digit()) && !value.is_empty() {
        return value.parse::<u64>().map_err(|e| RelishError::InvalidFlag {
            flag: "since".to_string(),
            reason: e.to_string(),
        });
    }

    let (number, unit) = value.split_at(value.len().saturating_sub(1));
    let multiplier = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 3600,
        "d" => 86_400,
        _ => {
            return Err(RelishError::InvalidFlag {
                flag: "since".to_string(),
                reason: format!("{value:?} — use epoch seconds or a duration like 30s, 5m, 2h, 1d"),
            });
        }
    };
    let amount: u64 = number.parse().map_err(|_| RelishError::InvalidFlag {
        flag: "since".to_string(),
        reason: format!("{value:?} — use epoch seconds or a duration like 30s, 5m, 2h, 1d"),
    })?;
    Ok(now_epoch.saturating_sub(amount * multiplier))
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
    options: &super::client::LogOptions,
    client: &BunClient,
) -> Result<(), RelishError> {
    let log_output = client.logs(name, "default", options).await?;
    if !log_output.is_empty() {
        println!("{log_output}");
    }
    Ok(())
}

/// Export Parquet log files to a destination directory.
///
/// Triggers an immediate export from the running Bun agent's LogStore
/// to the specified destination. Falls back to direct file copy if the
/// agent is unreachable.
pub async fn logs_export(dest: &Path, node_id: &str) -> Result<(), RelishError> {
    // Try to find the local log store directory
    let log_dir = dirs::data_local_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("/tmp/reliaburger"))
        .join("reliaburger")
        .join("logs")
        .join("parquet");

    if !log_dir.exists() {
        // Try the default config path
        let alt_dir = std::path::PathBuf::from("/var/lib/reliaburger/logs/parquet");
        if !alt_dir.exists() {
            return Err(RelishError::ApiError {
                status: 0,
                body: format!(
                    "no log store found at {} or {}",
                    log_dir.display(),
                    alt_dir.display()
                ),
            });
        }
        return logs_export_from(&alt_dir, dest, node_id);
    }

    logs_export_from(&log_dir, dest, node_id)
}

fn logs_export_from(source: &Path, dest: &Path, node_id: &str) -> Result<(), RelishError> {
    use crate::ketchup::export::{ExportCheckpoint, export_logs};

    let checkpoint_path = source.join("_export_checkpoint.json");
    let mut checkpoint = ExportCheckpoint::load(&checkpoint_path);

    let dest_str = dest.to_str().unwrap_or(".");
    match export_logs(source, dest_str, node_id, &mut checkpoint) {
        Ok(result) => {
            if result.files_exported == 0 {
                println!("no new files to export");
            } else {
                println!(
                    "exported {} file(s) ({} bytes) to {}/{}",
                    result.files_exported, result.bytes_written, dest_str, node_id,
                );
                checkpoint.save(&checkpoint_path).ok();
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
pub async fn exec(app: &str, command: &[String]) -> Result<(), RelishError> {
    exec_with_client(app, command, &BunClient::default_local()).await
}

async fn exec_with_client(
    app: &str,
    command: &[String],
    client: &BunClient,
) -> Result<(), RelishError> {
    let output = client.exec(app, "default", command).await?;
    if !output.is_empty() {
        print!("{output}");
    }
    Ok(())
}

/// Stop all instances of an app.
pub async fn stop(app: &str) -> Result<(), RelishError> {
    stop_with_client(app, &BunClient::default_local()).await
}

async fn stop_with_client(app: &str, client: &BunClient) -> Result<(), RelishError> {
    client.stop(app, "default").await?;
    println!("stopped {app}");
    Ok(())
}

/// Show detailed info about an app, node, or job.
pub async fn inspect(name: &str) -> Result<(), RelishError> {
    inspect_with_client(name, &BunClient::default_local()).await
}

async fn inspect_with_client(name: &str, client: &BunClient) -> Result<(), RelishError> {
    let statuses = client.status().await?;
    let matching: Vec<_> = statuses.iter().filter(|s| s.app_name == name).collect();

    if matching.is_empty() {
        println!("no instances found for {name}");
    } else {
        for s in &matching {
            println!("Instance: {}", s.id);
            println!("  App:       {}", s.app_name);
            println!("  Namespace: {}", s.namespace);
            println!("  State:     {}", s.state);
            println!("  Restarts:  {}", s.restart_count);
            if let Some(pid) = s.pid {
                println!("  PID:       {pid}");
            }
            if let Some(port) = s.host_port {
                println!("  Port:      {port}");
            }
            println!();
        }
    }

    Ok(())
}

/// Initialise a new cluster with starter config files and PKI.
///
/// Creates `reliaburger.toml` (node config) and `app.toml` (sample app)
/// in the given directory. Generates the CA hierarchy, age keypair,
/// first node certificate, and a join token.
pub fn init(dir: &Path, cluster_name: &str, node_id: &str) -> Result<(), RelishError> {
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
    fs::write(&secret_path, &secret_hex)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&secret_path, fs::Permissions::from_mode(0o600))?;
    }

    // Write security state to a bootstrap file for bun to load on first startup
    let bootstrap_path = dir.join(format!("{cluster_name}-security-bootstrap.json"));
    let bootstrap_json = serde_json::to_string_pretty(&init_result.security_state)
        .map_err(|e| RelishError::InitFailed(format!("failed to serialise security state: {e}")))?;
    fs::write(&bootstrap_path, &bootstrap_json)?;

    // Output the init summary to stderr (join token is sensitive)
    let output = crate::sesame::init::format_init_output(&init_result);
    eprint!("{output}");
    eprintln!("  Master secret:   {}", secret_path.display());
    eprintln!("  Security state:  {}", bootstrap_path.display());
    eprintln!();
    eprintln!(
        "  Back up {}-master.key alongside the sealed root CA key.",
        cluster_name
    );

    let node_config = crate::config::node::NodeConfig::default();
    let node_toml = format!(
        "# Reliaburger node configuration.\n\
         # See docs/README.md for full reference.\n\n{}",
        toml::to_string_pretty(&node_config).expect("failed to serialise default node config")
    );

    let app_toml = "\
# Sample Reliaburger app configuration.
# Deploy with: relish apply app.toml

[app.web]
image = \"nginx:latest\"
port = 8080

[app.web.health]
path = \"/\"
";

    fs::write(&node_path, node_toml)?;
    fs::write(&app_path, app_toml)?;

    println!("created {}", node_path.display());
    println!("created {}", app_path.display());

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

/// Run a chaos testing scenario or action.
pub async fn chaos(action: &str) -> Result<(), RelishError> {
    let client = BunClient::default_local();
    match action {
        "council-partition" => super::chaos::council_partition(&client).await,
        "worker-isolation" => super::chaos::worker_isolation(&client).await,
        "status" => super::chaos::status(&client).await,
        "heal" => super::chaos::heal(&client).await,
        other => {
            eprintln!("unknown chaos action: {other}");
            eprintln!();
            eprintln!("available actions:");
            eprintln!("  council-partition   partition a council minority from the majority");
            eprintln!("  worker-isolation    isolate a worker from all council members");
            eprintln!("  status              show active fault injections");
            eprintln!("  heal                remove all fault injections");
            Err(RelishError::ApiError {
                status: 0,
                body: format!("unknown chaos action: {other}"),
            })
        }
    }
}

/// Join an existing cluster.
pub async fn join(token: &str, addr: &str) -> Result<(), RelishError> {
    join_with_client(token, addr, &BunClient::default_local()).await
}

async fn join_with_client(token: &str, addr: &str, client: &BunClient) -> Result<(), RelishError> {
    let message = client.join(token, addr).await?;
    println!("{message}");
    Ok(())
}

/// Show council (Raft) composition and status.
pub async fn council(output: OutputFormat) -> Result<(), RelishError> {
    council_with_client(output, &BunClient::default_local()).await
}

async fn council_with_client(output: OutputFormat, client: &BunClient) -> Result<(), RelishError> {
    let council = client.council().await?;

    match output {
        OutputFormat::Human => {
            print_council_human(&council);
        }
        OutputFormat::Json => {
            let json =
                serde_json::to_string_pretty(&council).map_err(RelishError::SerialiseJson)?;
            println!("{json}");
        }
        OutputFormat::Yaml => {
            let yaml = serde_yaml::to_string(&council).map_err(RelishError::SerialiseYaml)?;
            print!("{yaml}");
        }
    }

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
pub async fn deploy(path: &Path, dry_run: bool) -> Result<(), RelishError> {
    let config = Config::from_file(path)?;
    config.validate()?;

    if dry_run {
        let plan = generate_plan(&config, None);
        let formatted = format_output(&plan, super::OutputFormat::Human)?;
        println!("{formatted}");
        println!("\n(dry run — nothing deployed)");
        return Ok(());
    }

    let client = BunClient::default_local();
    match client.health().await {
        Ok(()) => {
            let result = client.apply(&config).await?;
            println!(
                "deploy started: {} instance(s): {}",
                result.created,
                result.instances.join(", ")
            );
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

/// Show deploy history for an app.
pub async fn history(app: &str) -> Result<(), RelishError> {
    let client = BunClient::default_local();
    client.health().await?;

    let url = format!("{}/v1/deploys/history/{app}", client.base_url());
    let resp = reqwest::get(&url)
        .await
        .map_err(|_| RelishError::AgentUnreachable)?;
    let body: serde_json::Value = resp.json().await.map_err(|e| RelishError::ApiError {
        status: 0,
        body: e.to_string(),
    })?;

    if let Some(entries) = body["history"].as_array() {
        if entries.is_empty() {
            println!("no deploy history for {app}");
        } else {
            println!(
                "{:<8} {:<20} {:<12} {:<6} {:<6}",
                "ID", "IMAGE", "RESULT", "DONE", "TOTAL"
            );
            for e in entries {
                println!(
                    "{:<8} {:<20} {:<12} {:<6} {:<6}",
                    e["id"].as_u64().unwrap_or(0),
                    e["image"].as_str().unwrap_or("-"),
                    e["result"].as_str().unwrap_or("-"),
                    e["steps_completed"].as_u64().unwrap_or(0),
                    e["steps_total"].as_u64().unwrap_or(0),
                );
            }
        }
    }

    Ok(())
}

/// Rollback an app to the previous version.
pub async fn rollback(app: &str) -> Result<(), RelishError> {
    let client = BunClient::default_local();
    client.health().await?;
    // X3: rollback used to only print advice. It now calls the server,
    // which redeploys the previous successful spec.
    client.rollback(app, "default").await?;
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
    fs::write(path, &formatted)?;
    println!("formatted {}", path.display());
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

    if !result.report.resources_created.is_empty() || !result.report.unsupported.is_empty() {
        eprint!("{}", result.report);
    }

    Ok(())
}

/// Show live resource usage for all apps and nodes.
pub async fn top() -> Result<(), RelishError> {
    let client = BunClient::default_local();
    let statuses = client.status().await?;

    if statuses.is_empty() {
        println!("no workloads running");
        return Ok(());
    }

    println!(
        "{:<20} {:<12} {:<10} {:<10} {:<10}",
        "APP", "NAMESPACE", "STATE", "PID", "RESTARTS"
    );
    for s in &statuses {
        let pid = s
            .pid
            .map(|p| p.to_string())
            .unwrap_or_else(|| "-".to_string());
        println!(
            "{:<20} {:<12} {:<10} {:<10} {:<10}",
            s.app_name, s.namespace, s.state, pid, s.restart_count
        );
    }

    Ok(())
}

/// List images in the local Pickle registry.
/// Rotate or finalise the cluster's secret encryption key.
pub async fn secret_rotate(finalize: bool) -> Result<(), RelishError> {
    let client = BunClient::default_local();
    let result = client.secret_rotate(finalize).await?;
    println!("{result}");
    Ok(())
}

/// Sign an image in the Pickle registry and attach the signature via Raft.
pub async fn sign(image: &str) -> Result<(), RelishError> {
    let client = BunClient::default_local();
    let result = client.sign_image(image).await?;
    println!("{result}");
    Ok(())
}

pub async fn images() -> Result<(), RelishError> {
    let client = BunClient::default_local();
    let result = client.images().await?;
    let images = result["images"].as_array();
    match images {
        Some(imgs) if imgs.is_empty() => {
            println!("no images in local registry");
        }
        Some(imgs) => {
            println!(
                "{:<30} {:<15} {:>8} {:>12}",
                "REPOSITORY", "TAG", "LAYERS", "SIZE"
            );
            for img in imgs {
                let repo = img["repository"].as_str().unwrap_or("?");
                let tags = img["tags"]
                    .as_array()
                    .map(|t| {
                        t.iter()
                            .filter_map(|v| v.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    })
                    .unwrap_or_default();
                let tag_display = if tags.is_empty() { "<none>" } else { &tags };
                let layers = img["layers"].as_u64().unwrap_or(0);
                let size = img["total_size"].as_u64().unwrap_or(0);
                let size_display = if size >= 1_000_000 {
                    format!("{:.1} MB", size as f64 / 1_000_000.0)
                } else if size >= 1_000 {
                    format!("{:.1} KB", size as f64 / 1_000.0)
                } else {
                    format!("{size} B")
                };
                println!("{repo:<30} {tag_display:<15} {layers:>8} {size_display:>12}");
            }
        }
        None => {
            println!("no images in local registry");
        }
    }
    Ok(())
}

/// Build OCI images and push to Pickle.
///
/// Reads `[build.*]` sections from the config, tars each context,
/// uploads it to Pickle, and submits a build job.
pub async fn build(path: &std::path::Path, registry_port: u16) -> Result<(), RelishError> {
    use crate::config::Config;
    use crate::pickle::build::{digest_of, execute_build, tar_context};

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
        // target the Bun API port, which has no /v2 routes).
        let upload_url = crate::pickle::build::context_upload_url(registry_port, &digest);
        let resp = reqwest::Client::new()
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

        // Prepare the build job (for display; the agent re-derives it)
        let job = execute_build(spec, &digest, Some(registry_port)).map_err(|e| {
            RelishError::ApiError {
                status: 0,
                body: format!("build preparation failed: {e}"),
            }
        })?;

        println!(
            "  destination: pickle://{}:{}",
            job.destination.name, job.destination.tag
        );
        println!("  build:  {}", job.build_cmd.join(" "));
        println!("  push:   {}", job.push_cmd.join(" "));

        // Submit and poll: builds run async on the builder node —
        // minutes-long buildah runs must not hold an HTTP request open.
        let build_id = client
            .submit_build(name, &digest, registry_port, spec)
            .await?;
        println!("  build {build_id} accepted; waiting...");
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            let status = client.build_status(build_id).await?;
            match status["status"].as_str() {
                Some("completed") => {
                    println!(
                        "  built and pushed {}",
                        status["image"].as_str().unwrap_or("?")
                    );
                    break;
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
                _ => {} // running / delegated — keep polling
            }
        }
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

/// Show a batch's progress.
pub async fn batch_status(batch_id: u64) -> Result<(), RelishError> {
    let client = BunClient::default_local();
    let summary = client.batch_status(batch_id).await?;
    println!(
        "batch {}: {} total, {} pending, {} completed, {} failed{}",
        batch_id,
        summary["total"].as_u64().unwrap_or(0),
        summary["pending"].as_u64().unwrap_or(0),
        summary["completed"].as_u64().unwrap_or(0),
        summary["failed"].as_u64().unwrap_or(0),
        if summary["done"].as_bool().unwrap_or(false) {
            " — done"
        } else {
            ""
        },
    );
    Ok(())
}

/// Create a new API token (local operation — no agent needed).
///
/// Generates a token, hashes it with Argon2id, and prints the plaintext
/// to stdout (shown once, never stored).
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

/// Print the cluster's age public key from the init output directory.
///
/// Reads the security bootstrap `relish init` wrote and extracts the
/// cluster-wide age public key. This key can be used offline to encrypt
/// secrets for `ENC[AGE:...]` config values.
pub fn secret_pubkey(dir: &Path) -> Result<(), RelishError> {
    println!("{}", resolve_secret_pubkey(dir)?);
    Ok(())
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

/// List API tokens from SecurityState via the agent.
pub async fn token_list() -> Result<(), RelishError> {
    let client = BunClient::default_local();
    let result = client.token_list().await?;
    let tokens = result["tokens"].as_array();
    match tokens {
        Some(toks) if toks.is_empty() => {
            println!("no tokens");
        }
        Some(toks) => {
            println!("{:<20} {:<12} {:<20}", "NAME", "ROLE", "CREATED");
            for t in toks {
                let name = t["name"].as_str().unwrap_or("?");
                let role = t["role"].as_str().unwrap_or("?");
                let created = t["created_at"].as_u64().unwrap_or(0);
                println!("{:<20} {:<12} {:<20}", name, role, created);
            }
        }
        None => {
            println!("no tokens");
        }
    }
    Ok(())
}

/// Revoke an API token by name via the agent.
pub async fn token_revoke(name: &str) -> Result<(), RelishError> {
    let client = BunClient::default_local();
    let result = client.token_revoke(name).await?;
    println!("{result}");
    Ok(())
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
    println!("{:<24} {:<12} {:>12}  UPLOADED", "NAME", "VOLUME", "SIZE");
    for meta in list {
        println!(
            "{:<24} {:<12} {:>12}  {}",
            meta["name"].as_str().unwrap_or("?"),
            meta["volume_path"].as_str().unwrap_or("?"),
            meta["size_bytes"].as_u64().unwrap_or(0),
            if meta["uploaded"].as_bool().unwrap_or(false) {
                "yes"
            } else {
                "no"
            },
        );
    }
    Ok(())
}

/// Restore a snapshot over its live volume (stop the app first).
pub async fn snapshot_restore(app: &str, namespace: &str, name: &str) -> Result<(), RelishError> {
    let client = BunClient::default_local();
    client.snapshot_restore(app, namespace, name).await?;
    println!("restored {namespace}/{app} from snapshot {name}");
    Ok(())
}

/// Delete a snapshot.
pub async fn snapshot_delete(app: &str, namespace: &str, name: &str) -> Result<(), RelishError> {
    let client = BunClient::default_local();
    client.snapshot_delete(app, namespace, name).await?;
    println!("deleted snapshot {name} of {namespace}/{app}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

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

    #[test]
    fn secret_pubkey_errors_without_bootstrap_file() {
        let dir = tempfile::tempdir().unwrap();
        let result = resolve_secret_pubkey(dir.path());
        assert!(result.is_err(), "missing bootstrap must error");
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
        let err = apply_with_client(f.path(), OutputFormat::Human, false, &bogus_client())
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
            apply_with_client(f.path(), OutputFormat::Human, true, &bogus_client())
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn apply_with_missing_file_errors() {
        let result = apply_with_client(
            Path::new("/nonexistent/config.toml"),
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
        let result = apply_with_client(f.path(), OutputFormat::Human, false, &bogus_client()).await;
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
        let result = apply_with_client(f.path(), OutputFormat::Human, false, &bogus_client()).await;
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
        let options = build_log_options(
            Some(50),
            false,
            Some("error".to_string()),
            Some("5m".to_string()),
            Some("level=warn".to_string()),
            1_750_000_000,
        )
        .unwrap();
        assert_eq!(options.tail, Some(50));
        assert_eq!(options.grep.as_deref(), Some("error"));
        assert_eq!(options.start, Some(1_750_000_000 - 300));
        assert_eq!(
            options.json_field,
            Some(("level".to_string(), "warn".to_string()))
        );
    }

    #[tokio::test]
    async fn status_returns_agent_unreachable() {
        let err = status_with_client(&bogus_client()).await.unwrap_err();
        assert!(matches!(err, RelishError::AgentUnreachable), "got: {err:?}");
    }

    #[tokio::test]
    async fn logs_returns_agent_unreachable() {
        let options = super::super::client::LogOptions::default();
        let err = logs_with_client("web", &options, &bogus_client())
            .await
            .unwrap_err();
        assert!(matches!(err, RelishError::AgentUnreachable));
    }

    #[tokio::test]
    async fn exec_returns_agent_unreachable() {
        let err = exec_with_client("web", &["sh".to_string()], &bogus_client())
            .await
            .unwrap_err();
        assert!(matches!(err, RelishError::AgentUnreachable));
    }

    #[tokio::test]
    async fn stop_returns_agent_unreachable() {
        let err = stop_with_client("web", &bogus_client()).await.unwrap_err();
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
    fn init_creates_sealed_root_ca() {
        let dir = tempfile::tempdir().unwrap();
        init(dir.path(), "mycluster", "node-01").unwrap();
        assert!(dir.path().join("mycluster-root-ca.age").exists());
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
}
