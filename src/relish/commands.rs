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
/// If no agent is reachable, falls back to showing the dry-run plan.
pub async fn apply(path: &Path, output: OutputFormat) -> Result<(), RelishError> {
    apply_with_client(path, output, &BunClient::default_local()).await
}

async fn apply_with_client(
    path: &Path,
    output: OutputFormat,
    client: &BunClient,
) -> Result<(), RelishError> {
    let config = Config::from_file(path)?;
    config.validate()?;

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
            // Agent unreachable — fall back to dry-run
            let plan = generate_plan(&config, None);
            let formatted = format_output(&plan, output)?;
            println!("{formatted}");
            println!("\n(dry run — bun agent not reachable, showing plan only)");
            Ok(())
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
pub async fn logs(name: &str, tail: Option<usize>, follow: bool) -> Result<(), RelishError> {
    logs_with_client(name, tail, follow, &BunClient::default_local()).await
}

async fn logs_with_client(
    name: &str,
    tail: Option<usize>,
    follow: bool,
    client: &BunClient,
) -> Result<(), RelishError> {
    let log_output = client.logs(name, "default", tail, follow).await?;
    if !log_output.is_empty() {
        println!("{log_output}");
    }
    Ok(())
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
pub async fn deploy(path: &Path) -> Result<(), RelishError> {
    let config = Config::from_file(path)?;
    config.validate()?;

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
            println!("\n(dry run — bun agent not reachable)");
            Ok(())
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

    // Find the last successful deploy image from history
    let url = format!("{}/v1/deploys/history/{app}", client.base_url());
    let resp = reqwest::get(&url)
        .await
        .map_err(|_| RelishError::AgentUnreachable)?;
    let body: serde_json::Value = resp.json().await.map_err(|e| RelishError::ApiError {
        status: 0,
        body: e.to_string(),
    })?;

    let last_good = body["history"].as_array().and_then(|entries| {
        entries
            .iter()
            .rev()
            .find(|e| e["result"].as_str() == Some("Completed"))
    });

    match last_good {
        Some(entry) => {
            let image = entry["image"].as_str().unwrap_or("unknown");
            println!("rollback {app} to image: {image}");
            println!("(use `relish apply` with the previous config to rollback)");
        }
        None => {
            println!("no successful deploy found in history for {app}");
        }
    }

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
pub async fn build(path: &std::path::Path) -> Result<(), RelishError> {
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

        // Upload context blob to Pickle
        let upload_url = crate::pickle::build::context_upload_url(9117, &digest);
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

        // Prepare the build job
        let job = execute_build(spec, &digest, None).map_err(|e| RelishError::ApiError {
            status: 0,
            body: format!("build preparation failed: {e}"),
        })?;

        println!(
            "  destination: pickle://{}:{}",
            job.destination.name, job.destination.tag
        );
        println!("  build:  {}", job.build_cmd.join(" "));
        println!("  push:   {}", job.push_cmd.join(" "));

        // Submit build job to agent
        let result = client
            .submit_build(name, &digest, &spec.destination)
            .await?;
        println!("  {result}");
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
    let job_names: Vec<String> = config.job.keys().cloned().collect();
    let result = client.submit_batch(&job_names).await?;
    println!("{result}");
    Ok(())
}

/// Create a new API token (local operation — no agent needed).
///
/// Generates a token, hashes it with Argon2id, and prints the plaintext
/// to stdout (shown once, never stored).
pub fn token_create(
    name: &str,
    role_str: &str,
    apps: Option<&str>,
    namespaces: Option<&str>,
    ttl_days: Option<u64>,
) -> Result<(), RelishError> {
    use crate::sesame::token::create_token;
    use crate::sesame::types::{ApiRole, TokenScope};
    use std::time::{Duration, SystemTime};

    let role = match role_str {
        "admin" => ApiRole::Admin,
        "deployer" => ApiRole::Deployer,
        "read-only" | "readonly" => ApiRole::ReadOnly,
        other => {
            return Err(RelishError::InitFailed(format!(
                "unknown role: {other} (expected admin, deployer, or read-only)"
            )));
        }
    };

    let scope = TokenScope {
        apps: apps.map(|a| a.split(',').map(|s| s.trim().to_string()).collect()),
        namespaces: namespaces.map(|n| n.split(',').map(|s| s.trim().to_string()).collect()),
    };

    let expires_at = ttl_days.map(|days| SystemTime::now() + Duration::from_secs(days * 86400));

    let created = create_token(name, role, scope, expires_at)
        .map_err(|e| RelishError::InitFailed(e.to_string()))?;

    eprintln!("Token created: {name}");
    eprintln!("  Role: {role}");
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
    println!("{}", created.plaintext);

    Ok(())
}

/// Print the cluster's age public key from the init output directory.
///
/// Reads the security state written by `relish init` and extracts the
/// age public key. This key can be used offline to encrypt secrets for
/// `ENC[AGE:...]` config values.
pub fn secret_pubkey(dir: &Path) -> Result<(), RelishError> {
    let init_output_path = dir.join("sesame-state.json");
    if !init_output_path.exists() {
        return Err(RelishError::InitFailed(
            "sesame-state.json not found — run `relish init` first".to_string(),
        ));
    }
    let data = fs::read_to_string(&init_output_path)?;
    let state: serde_json::Value = serde_json::from_str(&data)
        .map_err(|e| RelishError::InitFailed(format!("failed to parse sesame-state.json: {e}")))?;

    let pubkey = state["age_public_key"]
        .as_str()
        .ok_or_else(|| RelishError::InitFailed("age_public_key not found in state".to_string()))?;

    println!("{pubkey}");
    Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    /// Port 1 on localhost — nothing listens there, so connections
    /// are refused immediately without waiting for a timeout.
    fn bogus_client() -> BunClient {
        BunClient::new("http://127.0.0.1:1")
    }

    fn write_temp_config(content: &str) -> tempfile::NamedTempFile {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(content.as_bytes()).unwrap();
        f
    }

    #[tokio::test]
    async fn apply_with_valid_config_falls_back_to_dry_run() {
        let f = write_temp_config(
            r#"
            [app.web]
            image = "myapp:v1"
            port = 8080
        "#,
        );
        assert!(
            apply_with_client(f.path(), OutputFormat::Human, &bogus_client())
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn apply_with_missing_file_errors() {
        let result = apply_with_client(
            Path::new("/nonexistent/config.toml"),
            OutputFormat::Human,
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
        let result = apply_with_client(f.path(), OutputFormat::Human, &bogus_client()).await;
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
        let result = apply_with_client(f.path(), OutputFormat::Human, &bogus_client()).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(
            matches!(err, RelishError::Config(_)),
            "expected Config error, got: {err:?}"
        );
    }

    #[tokio::test]
    async fn status_returns_agent_unreachable() {
        let err = status_with_client(&bogus_client()).await.unwrap_err();
        assert!(matches!(err, RelishError::AgentUnreachable), "got: {err:?}");
    }

    #[tokio::test]
    async fn logs_returns_agent_unreachable() {
        let err = logs_with_client("web", None, false, &bogus_client())
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
