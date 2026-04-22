//! Relish — the Reliaburger CLI.
//!
//! Command-line interface for managing a Reliaburger cluster.
//! Launches a TUI when invoked with no arguments.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use reliaburger::relish::OutputFormat;
use reliaburger::relish::commands;

#[derive(Parser)]
#[command(name = "relish", version, about = "Reliaburger CLI")]
struct Cli {
    /// Output format: human, json, or yaml.
    #[arg(long, default_value = "human", global = true)]
    output: OutputFormat,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Apply configuration from a file or directory.
    Apply {
        /// Path to a TOML config file or directory.
        path: PathBuf,
    },
    /// Show cluster and app status.
    Status,
    /// Stream logs from an app or job.
    Logs {
        /// App or job name.
        name: String,
        /// Show only the last N lines.
        #[arg(long)]
        tail: Option<usize>,
        /// Follow log output (stream new lines as they appear).
        #[arg(long, short = 'f')]
        follow: bool,
        /// Filter lines matching this substring.
        #[arg(long)]
        grep: Option<String>,
        /// Show logs since this time (e.g. "1h", "30m", epoch seconds).
        #[arg(long)]
        since: Option<String>,
        /// Filter structured JSON logs by field (key=value).
        #[arg(long)]
        json_field: Option<String>,
    },
    /// Export Parquet log files to a destination directory.
    #[command(name = "logs-export")]
    LogsExport {
        /// Destination directory path.
        #[arg(long)]
        dest: PathBuf,
        /// Node ID to use in export path. Default: "local".
        #[arg(long, default_value = "local")]
        node_id: String,
    },
    /// Search exported Parquet log archives with SQL.
    #[command(name = "logs-search")]
    LogsSearch {
        /// Path to exported Parquet directory.
        source: String,
        /// SQL query against the `logs` table.
        sql: String,
    },
    /// Show live resource usage (CPU, memory) for all apps.
    Top,
    /// Execute a command inside a running container.
    Exec {
        /// App name.
        app: String,
        /// Command to run.
        #[arg(trailing_var_arg = true)]
        command: Vec<String>,
    },
    /// Show detailed info about an app, node, or job.
    Inspect {
        /// Resource name.
        name: String,
    },
    /// Stop all instances of an app.
    Stop {
        /// App name.
        app: String,
    },
    /// Initialise a new cluster (generates CAs, age keypair, join token).
    Init {
        /// Directory to create config files in.
        #[arg(default_value = ".")]
        dir: PathBuf,
        /// Cluster name.
        #[arg(long, default_value = "default")]
        cluster_name: String,
        /// Node ID for this node.
        #[arg(long, default_value = "node-01")]
        node_id: String,
    },
    /// List cluster nodes and their gossip state.
    Nodes,
    /// Show council (Raft) composition and status.
    Council,
    /// Join an existing cluster.
    Join {
        /// Join token (validated in Phase 4).
        #[arg(long)]
        token: String,
        /// Address of an existing cluster member (gossip endpoint).
        addr: String,
    },
    /// Resolve a service name to its VIP and backends.
    Resolve {
        /// Service name (e.g. "redis").
        name: String,
    },
    /// Show ingress routing table.
    Routes,
    /// Run chaos testing scenarios or manage fault injections.
    Chaos {
        /// Scenario or action: council-partition, worker-isolation, status, heal.
        action: String,
    },
    /// Inject faults for chaos testing (Smoker).
    Fault {
        #[command(subcommand)]
        action: FaultAction,
    },
    /// Trigger a rolling deploy for an app.
    Deploy {
        /// Path to a TOML config file.
        path: PathBuf,
    },
    /// Show deploy history for an app.
    History {
        /// App name.
        app: String,
    },
    /// Rollback an app to the previous version.
    Rollback {
        /// App name.
        app: String,
    },
    /// Validate a config file without deploying.
    Lint {
        /// Path to a TOML config file.
        path: PathBuf,
    },
    /// Compile configs into a single resolved output.
    ///
    /// Merges all .toml files, applies _defaults.toml fields to apps
    /// missing them, and derives namespaces from subdirectory names.
    /// Recurses into subdirectories.
    Compile {
        /// Path to a TOML file or directory of TOML files.
        path: PathBuf,
    },
    /// Show structural diff between two configs.
    Diff {
        /// First config path (old).
        path_a: PathBuf,
        /// Second config path (new). If omitted, diffs against empty.
        path_b: Option<PathBuf>,
    },
    /// Format a TOML config file with canonical ordering.
    Fmt {
        /// Path to a TOML config file.
        path: PathBuf,
        /// Check formatting without modifying the file.
        #[arg(long)]
        check: bool,
    },
    /// Convert Kubernetes YAML manifests to Reliaburger TOML.
    #[cfg(feature = "kubernetes")]
    Import {
        /// Kubernetes YAML files to import.
        #[arg(short = 'f', long = "file", required = true)]
        files: Vec<PathBuf>,
        /// Exit non-zero if any warnings are generated.
        #[arg(long)]
        strict: bool,
    },
    /// Export Reliaburger TOML to Kubernetes YAML manifests.
    #[cfg(feature = "kubernetes")]
    Export {
        /// Path to a Reliaburger TOML config file.
        #[arg(short = 'f', long = "file")]
        file: PathBuf,
    },
    /// List images in the local Pickle registry.
    Images,
    /// Build an OCI image and push to Pickle.
    Build {
        /// Path to a TOML config file with [build.*] sections.
        path: PathBuf,
    },
    /// Submit a batch of jobs for high-throughput scheduling.
    Batch {
        /// Path to a TOML config file with [job.*] sections.
        path: PathBuf,
    },
    /// Manage secrets (encrypt values for use in app configs).
    Secret {
        #[command(subcommand)]
        action: SecretAction,
    },
    /// Manage API tokens.
    Token {
        #[command(subcommand)]
        action: TokenAction,
    },
    /// Sign an image in the Pickle registry and attach the signature.
    Sign {
        /// Image reference or manifest digest (e.g. "myapp:v1" or "sha256:abc...").
        image: String,
    },
    /// Manage a local dev cluster (Lima VMs).
    Dev {
        #[command(subcommand)]
        action: DevAction,
    },
}

#[derive(Subcommand)]
enum TokenAction {
    /// Create a new API token.
    Create {
        /// Token name (e.g. "ci-deploy").
        #[arg(long)]
        name: String,
        /// Role: admin, deployer, or read-only.
        #[arg(long, default_value = "read-only")]
        role: String,
        /// Restrict to specific apps (comma-separated).
        #[arg(long)]
        apps: Option<String>,
        /// Restrict to specific namespaces (comma-separated).
        #[arg(long)]
        namespaces: Option<String>,
        /// TTL in days (e.g. 90).
        #[arg(long)]
        ttl_days: Option<u64>,
    },
    /// List all API tokens.
    List,
    /// Revoke an API token by name.
    Revoke {
        /// Token name to revoke.
        name: String,
    },
}

#[derive(Subcommand)]
enum SecretAction {
    /// Print the cluster's age public key (for encrypting secrets offline).
    Pubkey {
        /// Directory containing the cluster config (from relish init).
        #[arg(default_value = ".")]
        dir: PathBuf,
    },
    /// Encrypt a plaintext value for use in app config ENC[AGE:...] fields.
    Encrypt {
        /// The age public key (from `relish secret pubkey`).
        #[arg(long)]
        pubkey: String,
        /// The plaintext value to encrypt.
        value: String,
    },
    /// Rotate the secret encryption key (start or finalise).
    Rotate {
        /// Finalise rotation: remove old read-only keypair.
        #[arg(long)]
        finalize: bool,
    },
}

#[derive(Subcommand)]
enum DevAction {
    /// Create a new dev cluster.
    Create {
        /// Number of nodes.
        #[arg(long, default_value = "3")]
        nodes: usize,
        /// CPUs per node.
        #[arg(long, default_value = "2")]
        cpus: usize,
        /// Memory per node (e.g. "2GiB").
        #[arg(long, default_value = "2GiB")]
        memory: String,
        /// Cluster name.
        #[arg(long, default_value = "default")]
        name: String,
    },
    /// Show dev cluster status.
    Status {
        /// Cluster name.
        #[arg(default_value = "default")]
        name: String,
    },
    /// Open a shell on a node.
    Shell {
        /// Node name (e.g. reliaburger-1).
        node: String,
    },
    /// Stop a dev cluster (VMs stay on disk).
    Stop {
        /// Cluster name.
        #[arg(default_value = "default")]
        name: String,
    },
    /// Start a stopped dev cluster.
    Start {
        /// Cluster name.
        #[arg(default_value = "default")]
        name: String,
    },
    /// Destroy a dev cluster (delete all VMs).
    Destroy {
        /// Cluster name.
        #[arg(default_value = "default")]
        name: String,
    },
    /// Run tests in a Linux VM (all Linux-gated tests enabled).
    Test {
        /// Optional test name filter (passed to cargo test).
        filter: Option<String>,
        /// Delete and recreate the test VM before running tests.
        #[arg(long)]
        recreate: bool,
    },
    /// Show disk usage in the test VM.
    Disk,
    /// Clean cargo build artefacts in the test VM.
    Clean,
}

#[derive(Subcommand)]
enum FaultAction {
    /// Add latency to connections to a service.
    Delay {
        /// Target service name.
        target: String,
        /// Delay duration (e.g. "200ms", "1s").
        delay: String,
        /// Jitter (e.g. "50ms").
        #[arg(long)]
        jitter: Option<String>,
        /// Fault duration (default: 10m).
        #[arg(long)]
        duration: Option<String>,
    },
    /// Fail a percentage of connections.
    Drop {
        /// Target service name.
        target: String,
        /// Drop percentage (e.g. "10%").
        percentage: String,
        /// Fault duration (default: 10m).
        #[arg(long)]
        duration: Option<String>,
    },
    /// Return NXDOMAIN for DNS resolution.
    Dns {
        /// Target service name.
        target: String,
        /// Fault type: "nxdomain".
        fault_type: String,
        /// Fault duration (default: 10m).
        #[arg(long)]
        duration: Option<String>,
    },
    /// Block traffic between services.
    Partition {
        /// Target service name.
        target: String,
        /// Source service to block traffic from.
        #[arg(long)]
        from: Option<String>,
        /// Fault duration (default: 10m).
        #[arg(long)]
        duration: Option<String>,
    },
    /// Throttle bandwidth to a service.
    Bandwidth {
        /// Target service name.
        target: String,
        /// Bandwidth limit (e.g. "1mbps").
        limit: String,
        /// Fault duration (default: 10m).
        #[arg(long)]
        duration: Option<String>,
    },
    /// Consume CPU in a service's cgroup.
    Cpu {
        /// Target service name.
        target: String,
        /// CPU consumption percentage (e.g. "50%").
        percentage: String,
        /// Number of cores to stress.
        #[arg(long)]
        cores: Option<u32>,
        /// Fault duration (default: 10m).
        #[arg(long)]
        duration: Option<String>,
    },
    /// Push memory usage toward a service's limit.
    Memory {
        /// Target service name.
        target: String,
        /// Memory fill percentage or "oom" (e.g. "90%", "oom").
        value: String,
        /// Fault duration (default: 10m).
        #[arg(long)]
        duration: Option<String>,
    },
    /// Throttle disk I/O for a service.
    DiskIo {
        /// Target service name.
        target: String,
        /// I/O bandwidth limit (e.g. "10mbps").
        limit: String,
        /// Only throttle writes.
        #[arg(long)]
        write_only: bool,
        /// Fault duration (default: 10m).
        #[arg(long)]
        duration: Option<String>,
    },
    /// Kill instances of a service (SIGKILL).
    Kill {
        /// Target service or instance name.
        target: String,
        /// Number of instances to kill (0 = all).
        #[arg(long, default_value = "1")]
        count: u32,
    },
    /// Freeze instances of a service (SIGSTOP).
    Pause {
        /// Target service name.
        target: String,
        /// Fault duration (default: 10m).
        #[arg(long)]
        duration: Option<String>,
    },
    /// Simulate graceful node departure.
    NodeDrain {
        /// Target node name.
        target: String,
        /// Drain duration (default: 10m).
        #[arg(long)]
        duration: Option<String>,
        /// Allow targeting the cluster leader.
        #[arg(long)]
        include_leader: bool,
    },
    /// Simulate abrupt node failure.
    NodeKill {
        /// Target node name.
        target: String,
        /// Kill duration (default: 10m).
        #[arg(long)]
        duration: Option<String>,
        /// Also stop all containers on the node.
        #[arg(long)]
        containers: bool,
        /// Allow targeting the cluster leader.
        #[arg(long)]
        include_leader: bool,
    },
    /// List all active faults.
    List,
    /// Clear all active faults (or a specific one by ID).
    Clear {
        /// Fault ID to clear (omit to clear all).
        id: Option<u64>,
    },
    /// Run a scripted chaos scenario from a TOML file.
    Scenario {
        /// Path to the scenario TOML file.
        path: PathBuf,
        /// Print the scenario plan without executing.
        #[arg(long)]
        dry_run: bool,
        /// Speed multiplier (e.g. 2.0 = double speed).
        #[arg(long, default_value = "1.0")]
        speed: f64,
    },
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();

    let result = match cli.command {
        Command::Apply { ref path } => commands::apply(path, cli.output).await,
        Command::Status => commands::status().await,
        Command::Logs {
            ref name,
            tail,
            follow,
            grep: ref _grep,
            since: ref _since,
            json_field: ref _json_field,
        } => commands::logs(name, tail, follow).await,
        Command::LogsExport {
            ref dest,
            ref node_id,
        } => commands::logs_export(dest, node_id).await,
        Command::LogsSearch {
            ref source,
            ref sql,
        } => commands::logs_search(source, sql).await,
        Command::Top => commands::top().await,
        Command::Exec {
            ref app,
            ref command,
        } => commands::exec(app, command).await,
        Command::Inspect { ref name } => commands::inspect(name).await,
        Command::Stop { ref app } => commands::stop(app).await,
        Command::Init {
            ref dir,
            ref cluster_name,
            ref node_id,
        } => commands::init(dir, cluster_name, node_id),
        Command::Nodes => commands::nodes(cli.output).await,
        Command::Council => commands::council(cli.output).await,
        Command::Join {
            ref token,
            ref addr,
        } => commands::join(token, addr).await,
        Command::Resolve { ref name } => commands::resolve(name).await,
        Command::Routes => commands::routes().await,
        Command::Chaos { ref action } => commands::chaos(action).await,
        Command::Fault { ref action } => match action {
            FaultAction::Delay {
                target,
                delay,
                jitter,
                duration,
            } => {
                reliaburger::relish::fault::delay(target, delay, jitter.as_deref(), duration).await
            }
            FaultAction::Drop {
                target,
                percentage,
                duration,
            } => reliaburger::relish::fault::drop_fault(target, percentage, duration).await,
            FaultAction::Dns {
                target,
                fault_type,
                duration,
            } => reliaburger::relish::fault::dns(target, fault_type, duration).await,
            FaultAction::Partition {
                target,
                from,
                duration,
            } => reliaburger::relish::fault::partition(target, from.as_deref(), duration).await,
            FaultAction::Bandwidth {
                target,
                limit,
                duration,
            } => reliaburger::relish::fault::bandwidth(target, limit, duration).await,
            FaultAction::Cpu {
                target,
                percentage,
                cores,
                duration,
            } => reliaburger::relish::fault::cpu(target, percentage, *cores, duration).await,
            FaultAction::Memory {
                target,
                value,
                duration,
            } => reliaburger::relish::fault::memory(target, value, duration).await,
            FaultAction::DiskIo {
                target,
                limit,
                write_only,
                duration,
            } => reliaburger::relish::fault::disk_io(target, limit, *write_only, duration).await,
            FaultAction::Kill { target, count } => {
                reliaburger::relish::fault::kill(target, *count).await
            }
            FaultAction::Pause { target, duration } => {
                reliaburger::relish::fault::pause(target, duration).await
            }
            FaultAction::NodeDrain {
                target,
                duration,
                include_leader,
            } => reliaburger::relish::fault::node_drain(target, duration, *include_leader).await,
            FaultAction::NodeKill {
                target,
                duration,
                containers,
                include_leader,
            } => {
                reliaburger::relish::fault::node_kill(
                    target,
                    duration,
                    *containers,
                    *include_leader,
                )
                .await
            }
            FaultAction::List => reliaburger::relish::fault::list().await,
            FaultAction::Clear { id } => reliaburger::relish::fault::clear(*id).await,
            FaultAction::Scenario {
                path,
                dry_run,
                speed,
            } => reliaburger::relish::fault::scenario(path, *dry_run, *speed).await,
        },
        Command::Deploy { ref path } => commands::deploy(path).await,
        Command::History { ref app } => commands::history(app).await,
        Command::Rollback { ref app } => commands::rollback(app).await,
        Command::Lint { ref path } => commands::lint(path),
        Command::Compile { ref path } => commands::compile(path),
        Command::Diff {
            ref path_a,
            ref path_b,
        } => commands::diff(path_a, path_b.as_deref()),
        Command::Fmt { ref path, check } => commands::fmt(path, check),
        #[cfg(feature = "kubernetes")]
        Command::Import { ref files, strict } => commands::import_k8s(files, strict),
        #[cfg(feature = "kubernetes")]
        Command::Export { ref file } => commands::export_k8s(file),
        Command::Images => commands::images().await,
        Command::Build { ref path } => commands::build(path).await,
        Command::Batch { ref path } => commands::batch(path).await,
        Command::Secret { action } => match &action {
            SecretAction::Pubkey { dir } => commands::secret_pubkey(dir),
            SecretAction::Encrypt { pubkey, value } => commands::secret_encrypt(pubkey, value),
            SecretAction::Rotate { finalize } => commands::secret_rotate(*finalize).await,
        },
        Command::Token { action } => match &action {
            TokenAction::Create {
                name,
                role,
                apps,
                namespaces,
                ttl_days,
            } => commands::token_create(
                name,
                role,
                apps.as_deref(),
                namespaces.as_deref(),
                *ttl_days,
            ),
            TokenAction::List => commands::token_list().await,
            TokenAction::Revoke { name } => commands::token_revoke(name).await,
        },
        Command::Sign { ref image } => commands::sign(image).await,
        Command::Dev { action } => match &action {
            DevAction::Create {
                nodes,
                cpus,
                memory,
                name,
            } => reliaburger::relish::dev::create(name, *nodes, *cpus, memory).await,
            DevAction::Status { name } => reliaburger::relish::dev::status(name).await,
            DevAction::Shell { node } => reliaburger::relish::dev::shell(node).await,
            DevAction::Stop { name } => reliaburger::relish::dev::stop(name).await,
            DevAction::Start { name } => reliaburger::relish::dev::start(name).await,
            DevAction::Destroy { name } => reliaburger::relish::dev::destroy(name).await,
            DevAction::Test { filter, recreate } => {
                reliaburger::relish::dev::test(filter.as_deref(), *recreate).await
            }
            DevAction::Disk => reliaburger::relish::dev::disk().await,
            DevAction::Clean => reliaburger::relish::dev::clean().await,
        },
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    fn parse(args: &[&str]) -> Result<Cli, clap::Error> {
        Cli::try_parse_from(args)
    }

    #[test]
    fn parse_apply_command() {
        let cli = parse(&["relish", "apply", "config.toml"]).unwrap();
        assert!(
            matches!(cli.command, Command::Apply { ref path } if path.to_str() == Some("config.toml"))
        );
    }

    #[test]
    fn parse_status_command() {
        let cli = parse(&["relish", "status"]).unwrap();
        assert!(matches!(cli.command, Command::Status));
    }

    #[test]
    fn parse_exec_with_trailing_args() {
        let cli = parse(&["relish", "exec", "web", "sh", "-c", "ls"]).unwrap();
        match cli.command {
            Command::Exec { app, command } => {
                assert_eq!(app, "web");
                assert_eq!(command, vec!["sh", "-c", "ls"]);
            }
            _ => panic!("expected Exec command"),
        }
    }

    #[test]
    fn output_flag_json() {
        let cli = parse(&["relish", "--output", "json", "status"]).unwrap();
        assert_eq!(cli.output, OutputFormat::Json);
    }

    #[test]
    fn output_flag_yaml() {
        let cli = parse(&["relish", "--output", "yaml", "status"]).unwrap();
        assert_eq!(cli.output, OutputFormat::Yaml);
    }

    #[test]
    fn default_output_is_human() {
        let cli = parse(&["relish", "status"]).unwrap();
        assert_eq!(cli.output, OutputFormat::Human);
    }

    #[test]
    fn parse_init_command() {
        let cli = parse(&["relish", "init"]).unwrap();
        match cli.command {
            Command::Init {
                ref dir,
                ref cluster_name,
                ref node_id,
            } => {
                assert_eq!(dir.to_str(), Some("."));
                assert_eq!(cluster_name, "default");
                assert_eq!(node_id, "node-01");
            }
            _ => panic!("expected Init command"),
        }
    }

    #[test]
    fn parse_init_with_dir() {
        let cli = parse(&["relish", "init", "/tmp/myproject"]).unwrap();
        assert!(
            matches!(cli.command, Command::Init { ref dir, .. } if dir.to_str() == Some("/tmp/myproject"))
        );
    }

    #[test]
    fn parse_init_with_cluster_name() {
        let cli = parse(&["relish", "init", "--cluster-name", "prod"]).unwrap();
        match cli.command {
            Command::Init {
                ref cluster_name, ..
            } => assert_eq!(cluster_name, "prod"),
            _ => panic!("expected Init command"),
        }
    }

    #[test]
    fn parse_nodes_command() {
        let cli = parse(&["relish", "nodes"]).unwrap();
        assert!(matches!(cli.command, Command::Nodes));
    }

    #[test]
    fn parse_council_command() {
        let cli = parse(&["relish", "council"]).unwrap();
        assert!(matches!(cli.command, Command::Council));
    }

    #[test]
    fn parse_join_command() {
        let cli = parse(&["relish", "join", "--token", "abc123", "10.0.1.5:9443"]).unwrap();
        match cli.command {
            Command::Join { token, addr } => {
                assert_eq!(token, "abc123");
                assert_eq!(addr, "10.0.1.5:9443");
            }
            _ => panic!("expected Join command"),
        }
    }

    #[test]
    fn parse_join_missing_token_rejected() {
        let result = parse(&["relish", "join", "10.0.1.5:9443"]);
        assert!(result.is_err());
    }

    #[test]
    fn invalid_output_format_rejected() {
        let result = parse(&["relish", "--output", "csv", "status"]);
        assert!(result.is_err());
    }

    #[test]
    fn parse_resolve_command() {
        let cli = parse(&["relish", "resolve", "redis"]).unwrap();
        assert!(matches!(cli.command, Command::Resolve { ref name } if name == "redis"));
    }

    #[test]
    fn parse_stop_command() {
        let cli = parse(&["relish", "stop", "web"]).unwrap();
        assert!(matches!(cli.command, Command::Stop { ref app } if app == "web"));
    }

    #[test]
    fn parse_logs_with_tail() {
        let cli = parse(&["relish", "logs", "web", "--tail", "10"]).unwrap();
        match cli.command {
            Command::Logs {
                name, tail, follow, ..
            } => {
                assert_eq!(name, "web");
                assert_eq!(tail, Some(10));
                assert!(!follow);
            }
            _ => panic!("expected Logs command"),
        }
    }

    #[test]
    fn parse_logs_with_follow_short() {
        let cli = parse(&["relish", "logs", "web", "-f"]).unwrap();
        match cli.command {
            Command::Logs {
                name, tail, follow, ..
            } => {
                assert_eq!(name, "web");
                assert_eq!(tail, None);
                assert!(follow);
            }
            _ => panic!("expected Logs command"),
        }
    }

    #[test]
    fn parse_logs_with_follow_and_tail() {
        let cli = parse(&["relish", "logs", "web", "--follow", "--tail", "5"]).unwrap();
        match cli.command {
            Command::Logs {
                name, tail, follow, ..
            } => {
                assert_eq!(name, "web");
                assert_eq!(tail, Some(5));
                assert!(follow);
            }
            _ => panic!("expected Logs command"),
        }
    }

    #[test]
    fn parse_dev_create_defaults() {
        let cli = parse(&["relish", "dev", "create"]).unwrap();
        match cli.command {
            Command::Dev {
                action:
                    DevAction::Create {
                        nodes,
                        cpus,
                        memory,
                        name,
                    },
            } => {
                assert_eq!(nodes, 3);
                assert_eq!(cpus, 2);
                assert_eq!(memory, "2GiB");
                assert_eq!(name, "default");
            }
            _ => panic!("expected Dev Create command"),
        }
    }

    #[test]
    fn parse_dev_create_custom() {
        let cli = parse(&[
            "relish", "dev", "create", "--nodes", "5", "--cpus", "4", "--memory", "4GiB", "--name",
            "big",
        ])
        .unwrap();
        match cli.command {
            Command::Dev {
                action:
                    DevAction::Create {
                        nodes,
                        cpus,
                        memory,
                        name,
                    },
            } => {
                assert_eq!(nodes, 5);
                assert_eq!(cpus, 4);
                assert_eq!(memory, "4GiB");
                assert_eq!(name, "big");
            }
            _ => panic!("expected Dev Create command"),
        }
    }

    #[test]
    fn parse_dev_destroy() {
        let cli = parse(&["relish", "dev", "destroy"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Dev {
                action: DevAction::Destroy { .. }
            }
        ));
    }

    #[test]
    fn parse_dev_shell() {
        let cli = parse(&["relish", "dev", "shell", "reliaburger-1"]).unwrap();
        match cli.command {
            Command::Dev {
                action: DevAction::Shell { node },
            } => assert_eq!(node, "reliaburger-1"),
            _ => panic!("expected Dev Shell command"),
        }
    }

    #[test]
    fn parse_images_command() {
        let cli = parse(&["relish", "images"]).unwrap();
        assert!(matches!(cli.command, Command::Images));
    }

    #[test]
    fn parse_top_command() {
        let cli = parse(&["relish", "top"]).unwrap();
        assert!(matches!(cli.command, Command::Top));
    }

    #[test]
    fn parse_logs_with_grep() {
        let cli = parse(&["relish", "logs", "web", "--grep", "ERROR"]).unwrap();
        match cli.command {
            Command::Logs { grep, .. } => assert_eq!(grep.as_deref(), Some("ERROR")),
            _ => panic!("expected Logs command"),
        }
    }

    #[test]
    fn parse_logs_with_since() {
        let cli = parse(&["relish", "logs", "web", "--since", "1h"]).unwrap();
        match cli.command {
            Command::Logs { since, .. } => assert_eq!(since.as_deref(), Some("1h")),
            _ => panic!("expected Logs command"),
        }
    }

    #[test]
    fn parse_deploy_command() {
        let cli = parse(&["relish", "deploy", "app.toml"]).unwrap();
        assert!(matches!(cli.command, Command::Deploy { .. }));
    }

    #[test]
    fn parse_history_command() {
        let cli = parse(&["relish", "history", "web"]).unwrap();
        match cli.command {
            Command::History { app } => assert_eq!(app, "web"),
            _ => panic!("expected History"),
        }
    }

    #[test]
    fn parse_rollback_command() {
        let cli = parse(&["relish", "rollback", "web"]).unwrap();
        match cli.command {
            Command::Rollback { app } => assert_eq!(app, "web"),
            _ => panic!("expected Rollback"),
        }
    }

    #[test]
    fn parse_lint_command() {
        let cli = parse(&["relish", "lint", "app.toml"]).unwrap();
        assert!(matches!(cli.command, Command::Lint { .. }));
    }

    #[test]
    fn parse_compile_command() {
        let cli = parse(&["relish", "compile", "configs/"]).unwrap();
        assert!(matches!(cli.command, Command::Compile { .. }));
    }

    #[test]
    fn parse_diff_command_two_paths() {
        let cli = parse(&["relish", "diff", "old.toml", "new.toml"]).unwrap();
        match cli.command {
            Command::Diff { path_a, path_b } => {
                assert_eq!(path_a.to_str().unwrap(), "old.toml");
                assert_eq!(path_b.unwrap().to_str().unwrap(), "new.toml");
            }
            _ => panic!("expected Diff command"),
        }
    }

    #[test]
    fn parse_diff_command_one_path() {
        let cli = parse(&["relish", "diff", "old.toml"]).unwrap();
        match cli.command {
            Command::Diff { path_b, .. } => assert!(path_b.is_none()),
            _ => panic!("expected Diff command"),
        }
    }

    #[test]
    fn parse_fmt_command() {
        let cli = parse(&["relish", "fmt", "app.toml"]).unwrap();
        match cli.command {
            Command::Fmt { check, .. } => assert!(!check),
            _ => panic!("expected Fmt command"),
        }
    }

    #[test]
    fn parse_fmt_check_flag() {
        let cli = parse(&["relish", "fmt", "app.toml", "--check"]).unwrap();
        match cli.command {
            Command::Fmt { check, .. } => assert!(check),
            _ => panic!("expected Fmt command"),
        }
    }

    // -----------------------------------------------------------------------
    // Fault subcommand tests
    // -----------------------------------------------------------------------

    #[test]
    fn parse_fault_delay() {
        let cli = parse(&["relish", "fault", "delay", "redis", "200ms"]).unwrap();
        match cli.command {
            Command::Fault {
                action: FaultAction::Delay { target, delay, .. },
            } => {
                assert_eq!(target, "redis");
                assert_eq!(delay, "200ms");
            }
            _ => panic!("expected Fault Delay"),
        }
    }

    #[test]
    fn parse_fault_delay_with_jitter_and_duration() {
        let cli = parse(&[
            "relish",
            "fault",
            "delay",
            "redis",
            "200ms",
            "--jitter",
            "50ms",
            "--duration",
            "5m",
        ])
        .unwrap();
        match cli.command {
            Command::Fault {
                action:
                    FaultAction::Delay {
                        target,
                        delay,
                        jitter,
                        duration,
                    },
            } => {
                assert_eq!(target, "redis");
                assert_eq!(delay, "200ms");
                assert_eq!(jitter.as_deref(), Some("50ms"));
                assert_eq!(duration.as_deref(), Some("5m"));
            }
            _ => panic!("expected Fault Delay"),
        }
    }

    #[test]
    fn parse_fault_drop() {
        let cli = parse(&["relish", "fault", "drop", "api", "10%"]).unwrap();
        match cli.command {
            Command::Fault {
                action:
                    FaultAction::Drop {
                        target, percentage, ..
                    },
            } => {
                assert_eq!(target, "api");
                assert_eq!(percentage, "10%");
            }
            _ => panic!("expected Fault Drop"),
        }
    }

    #[test]
    fn parse_fault_dns_nxdomain() {
        let cli = parse(&["relish", "fault", "dns", "redis", "nxdomain"]).unwrap();
        match cli.command {
            Command::Fault {
                action:
                    FaultAction::Dns {
                        target, fault_type, ..
                    },
            } => {
                assert_eq!(target, "redis");
                assert_eq!(fault_type, "nxdomain");
            }
            _ => panic!("expected Fault Dns"),
        }
    }

    #[test]
    fn parse_fault_partition_with_from() {
        let cli = parse(&["relish", "fault", "partition", "web", "--from", "payment"]).unwrap();
        match cli.command {
            Command::Fault {
                action: FaultAction::Partition { target, from, .. },
            } => {
                assert_eq!(target, "web");
                assert_eq!(from.as_deref(), Some("payment"));
            }
            _ => panic!("expected Fault Partition"),
        }
    }

    #[test]
    fn parse_fault_kill() {
        let cli = parse(&["relish", "fault", "kill", "web", "--count", "2"]).unwrap();
        match cli.command {
            Command::Fault {
                action: FaultAction::Kill { target, count },
            } => {
                assert_eq!(target, "web");
                assert_eq!(count, 2);
            }
            _ => panic!("expected Fault Kill"),
        }
    }

    #[test]
    fn parse_fault_kill_default_count() {
        let cli = parse(&["relish", "fault", "kill", "web"]).unwrap();
        match cli.command {
            Command::Fault {
                action: FaultAction::Kill { count, .. },
            } => assert_eq!(count, 1),
            _ => panic!("expected Fault Kill"),
        }
    }

    #[test]
    fn parse_fault_pause() {
        let cli = parse(&["relish", "fault", "pause", "web"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Fault {
                action: FaultAction::Pause { .. }
            }
        ));
    }

    #[test]
    fn parse_fault_node_kill_with_flags() {
        let cli = parse(&[
            "relish",
            "fault",
            "node-kill",
            "node-05",
            "--containers",
            "--include-leader",
            "--duration",
            "30s",
        ])
        .unwrap();
        match cli.command {
            Command::Fault {
                action:
                    FaultAction::NodeKill {
                        target,
                        containers,
                        include_leader,
                        duration,
                    },
            } => {
                assert_eq!(target, "node-05");
                assert!(containers);
                assert!(include_leader);
                assert_eq!(duration.as_deref(), Some("30s"));
            }
            _ => panic!("expected Fault NodeKill"),
        }
    }

    #[test]
    fn parse_fault_list() {
        let cli = parse(&["relish", "fault", "list"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Fault {
                action: FaultAction::List
            }
        ));
    }

    #[test]
    fn parse_fault_clear_all() {
        let cli = parse(&["relish", "fault", "clear"]).unwrap();
        match cli.command {
            Command::Fault {
                action: FaultAction::Clear { id },
            } => assert!(id.is_none()),
            _ => panic!("expected Fault Clear"),
        }
    }

    #[test]
    fn parse_fault_clear_by_id() {
        let cli = parse(&["relish", "fault", "clear", "42"]).unwrap();
        match cli.command {
            Command::Fault {
                action: FaultAction::Clear { id },
            } => assert_eq!(id, Some(42)),
            _ => panic!("expected Fault Clear"),
        }
    }

    #[test]
    fn parse_build_command() {
        let cli = parse(&["relish", "build", "build.toml"]).unwrap();
        assert!(matches!(cli.command, Command::Build { .. }));
    }

    #[test]
    fn parse_batch_command() {
        let cli = parse(&["relish", "batch", "jobs.toml"]).unwrap();
        assert!(matches!(cli.command, Command::Batch { .. }));
    }
}
