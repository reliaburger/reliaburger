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

    /// API token for authenticating to the agent. Overrides `RELIABURGER_TOKEN`.
    #[arg(long, global = true)]
    token: Option<String>,

    /// Path to the cluster CA certificate (PEM). When set, the CLI reaches
    /// the agent API over HTTPS. Overrides `RELIABURGER_CA_CERT`.
    #[arg(long, global = true)]
    ca_cert: Option<PathBuf>,

    /// Bun API base URL. Overrides `RELIABURGER_ENDPOINT` and the default
    /// local address (`http[s]://127.0.0.1:9117`).
    #[arg(long, global = true, value_parser = parse_endpoint)]
    endpoint: Option<String>,

    #[command(subcommand)]
    command: Option<Command>,
}

fn parse_endpoint(value: &str) -> Result<String, String> {
    reliaburger::relish::client::validate_endpoint(value).map_err(|error| error.to_string())?;
    Ok(value.trim_end_matches('/').to_string())
}

#[derive(Subcommand)]
enum Command {
    /// Launch the interactive terminal UI.
    Tui,
    /// Open a read-only web dashboard through the current authenticated context.
    Dashboard {
        /// Loopback port; zero chooses an available port.
        #[arg(long, default_value_t = 0)]
        port: u16,
        /// Print the browser link without launching a browser.
        #[arg(long)]
        no_open: bool,
    },
    /// Apply a Reliaburger TOML or Kubernetes YAML manifest.
    ///
    /// Kubernetes YAML (a document with `apiVersion` and `kind`) is imported
    /// in memory; its migration report goes to stderr.
    #[command(group(clap::ArgGroup::new("manifest").required(true)))]
    Apply {
        /// Manifest path or https:// URL.
        #[arg(group = "manifest", value_name = "PATH_OR_URL")]
        path: Option<String>,
        /// Manifest path or https:// URL (the kubectl spelling).
        #[arg(
            short = 'f',
            long = "file",
            group = "manifest",
            value_name = "PATH_OR_URL"
        )]
        file: Option<String>,
        /// Show the plan without deploying (exits 0 even with no agent).
        #[arg(long)]
        dry_run: bool,
        /// Explicitly rerun jobs with unknown outcomes on the selected node.
        #[arg(long, conflicts_with = "dry_run")]
        rerun_jobs: bool,
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
        /// Namespace the app lives in (as derived by `compile` from its
        /// directory). Defaults to "default".
        #[arg(long, default_value = "default")]
        namespace: String,
    },
    /// Export Parquet log files to a destination directory.
    #[command(name = "logs-export")]
    LogsExport {
        /// Export this local Parquet directory without contacting an agent.
        #[arg(long)]
        source: Option<PathBuf>,
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
    /// Show every workload on every node, with its latest CPU and memory.
    Top,
    /// Show an app's own Prometheus metrics, scraped by the nodes running it.
    ///
    /// Without --name, lists every metric with one number: a gauge's value,
    /// a counter's rate, a histogram's mean. With --name, one line per
    /// instance with a sparkline.
    Metrics {
        /// App name.
        app: String,
        /// Namespace the app lives in.
        #[arg(long, default_value = "default")]
        namespace: String,
        /// One metric to show per instance (a histogram by its base name).
        #[arg(long)]
        name: Option<String>,
        /// How far back to look (e.g. "90s", "15m", "1h").
        #[arg(long, default_value = "15m")]
        since: String,
    },
    /// Execute a command inside a running container.
    Exec {
        /// App name.
        app: String,
        /// Namespace the app lives in (as derived by `compile` from its
        /// directory). Defaults to "default".
        #[arg(long, default_value = "default")]
        namespace: String,
        /// Command to run.
        #[arg(trailing_var_arg = true)]
        command: Vec<String>,
    },
    /// Show detailed info about an app, node, or job.
    Inspect {
        /// Resource name.
        name: String,
    },
    /// Scale an app to zero, keeping its configuration; `relish apply` starts it again.
    Stop {
        /// App name.
        app: String,
        /// Namespace the app lives in (as derived by `compile` from its
        /// directory). Defaults to "default".
        #[arg(long, default_value = "default")]
        namespace: String,
    },
    /// Remove an app from the cluster and stop all its instances.
    Delete {
        /// App name.
        app: String,
        /// Namespace the app lives in. Defaults to "default".
        #[arg(long, default_value = "default")]
        namespace: String,
    },
    /// Initialise a new cluster (generates CAs, age keypair, node identity).
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
        /// Generate a development-only config whose internal cluster
        /// transports are plaintext. Never use this on a shared network.
        #[arg(long)]
        development_plaintext: bool,
    },
    /// List cluster nodes and their gossip state.
    Nodes,
    /// Permanently retire a stopped or fenced node; return requires fresh enrolment.
    DecommissionNode {
        /// Old cluster identity to retire permanently.
        node_id: String,
        /// Confirm the node's workloads have been stopped or fenced externally.
        #[arg(long, required = true)]
        workloads_stopped: bool,
        /// Why the node was stopped or fenced.
        #[arg(long)]
        reason: String,
    },
    /// Show council (Raft) composition and status, or recover from full loss.
    Council {
        #[command(subcommand)]
        action: Option<CouncilCommand>,
    },
    /// Join an existing cluster.
    Join {
        /// Join token issued by `relish init` or `relish join-token create`.
        #[arg(
            long,
            required_unless_present = "token_file",
            conflicts_with = "token_file"
        )]
        token: Option<String>,
        /// Read the join token from a private file instead of command-line arguments.
        #[arg(long, conflicts_with = "token")]
        token_file: Option<PathBuf>,
        /// API address of an existing cluster member, e.g.
        /// `https://10.0.1.5:9117` (bare host:port assumes https).
        addr: String,
        /// This node's identifier (the certificate's common name).
        #[arg(long)]
        node_id: String,
        /// Directory to write the received identity into. Defaults to
        /// `identity` in the current directory (matching `relish init`).
        #[arg(long)]
        identity_dir: Option<std::path::PathBuf>,
        /// Pin the cluster's root CA fingerprint (`sha256:...`). When set,
        /// a member offering a different root CA is refused.
        #[arg(long)]
        ca_fingerprint: Option<String>,
    },
    /// Resolve a service name to its VIP and backends.
    Resolve {
        /// Service name (e.g. "redis").
        name: String,
    },
    /// Show ingress routing table.
    Routes,
    /// Inject faults for chaos testing (Smoker).
    Fault {
        #[command(subcommand)]
        action: FaultAction,
    },
    /// Manage volume snapshots (Btrfs-backed volumes only).
    Snapshot {
        #[command(subcommand)]
        action: SnapshotAction,
    },
    /// Trigger a rolling deploy for an app.
    Deploy {
        /// Path to a TOML config file.
        path: PathBuf,
        /// Show the plan without deploying (exits 0 even with no agent).
        #[arg(long)]
        dry_run: bool,
    },
    /// Cancel a node-local deploy and wait for its current work to finish.
    CancelDeploy {
        /// Operation ID from the apply stream or deploy-operation API.
        operation_id: String,
    },
    /// Show deploy history for an app.
    History {
        /// App name.
        app: String,
        /// Namespace the app lives in. Defaults to "default".
        #[arg(long, default_value = "default")]
        namespace: String,
    },
    /// Rollback an app to the previous version.
    Rollback {
        /// App name.
        app: String,
        /// Namespace the app lives in (as derived by `compile` from its
        /// directory). Defaults to "default".
        #[arg(long, default_value = "default")]
        namespace: String,
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
        /// Pickle registry port for context upload and image push.
        #[arg(long, default_value_t = 5050)]
        registry_port: u16,
        /// Give up waiting after this many seconds (the server-side
        /// build timeout is 900s; the margin covers queueing).
        #[arg(long, default_value_t = 960)]
        timeout: u64,
    },
    /// Submit a batch of jobs for high-throughput scheduling.
    Batch {
        /// Path to a TOML config file with [job.*] sections.
        path: PathBuf,
    },
    /// Show the progress of a submitted batch.
    #[command(name = "batch-status")]
    BatchStatus {
        /// Batch id from `relish batch`.
        id: u64,
        /// Poll until the batch reaches a terminal state.
        #[arg(long)]
        wait: bool,
        /// Give up waiting after this many seconds (jobs time out
        /// server-side after 3600s; the margin covers reporting).
        #[arg(long, default_value_t = 3660)]
        timeout: u64,
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
    /// Manage short-lived node-enrolment tokens.
    JoinToken {
        #[command(subcommand)]
        action: JoinTokenAction,
    },
    /// Sign a Pickle-hosted image with your own key so `require_signatures`
    /// admits it.
    ///
    /// The image's tag is resolved to its manifest digest and the digest is
    /// signed on this machine; only the signature and public key go to the
    /// cluster. Nodes admit the image when their `[images.trust_policy] keys`
    /// lists the public key. Create a key with `relish sign keygen --out PATH`.
    #[command(args_conflicts_with_subcommands = true, subcommand_negates_reqs = true)]
    Sign {
        #[command(subcommand)]
        action: Option<SignAction>,
        /// Image in the Pickle registry: a tag ("myapp:v1"), a pinned
        /// reference ("myapp@sha256:…") or a manifest digest ("sha256:…").
        #[arg(required = true)]
        image: Option<String>,
        /// ECDSA P-256 private key (PKCS#8 PEM) to sign with.
        #[arg(long, required = true)]
        key: Option<PathBuf>,
    },
    /// Manage a local dev cluster (Lima VMs).
    Dev {
        #[command(subcommand)]
        action: DevAction,
    },
    /// Roll a new bun binary across the cluster, or back.
    Upgrade {
        #[command(subcommand)]
        action: UpgradeAction,
    },
    /// Read the built-in manual (searchable TUI; --web for the browser).
    Manual {
        /// Serve the manual as one HTML page and open the browser.
        #[arg(long)]
        web: bool,
        /// Port for --web (0 picks an ephemeral port).
        #[arg(long, default_value_t = 8642)]
        port: u16,
        /// Open this chapter, e.g. `tour` or `chaos`.
        #[arg(conflicts_with = "web")]
        chapter: Option<String>,
        #[command(subcommand)]
        action: Option<ManualAction>,
    },
    /// Browse and fuzzy-search the source this binary was built from.
    Source {
        /// Open with this search query pre-seeded (e.g. "ebpf").
        query: Option<String>,
    },
    /// Remove the CLI, its PATH link, the managed Lima tools and the image cache.
    Uninstall {
        /// Don't ask for confirmation (required without a terminal).
        #[arg(long)]
        yes: bool,
    },
    /// Manage a laptop cluster created by setup --quickstart.
    Local {
        #[arg(value_enum)]
        action: LocalAction,
        /// One node to start or stop: its name as `relish nodes` shows it,
        /// its number (1, 2, 3) or `node-N`. Omit to act on every node.
        node: Option<String>,
        #[arg(long, default_value = "laptop")]
        name: String,
        /// Confirm destroying the cluster, stopping node 1 (which carries the
        /// CLI endpoint and ingress) or stopping a node the quorum needs.
        #[arg(long)]
        yes: bool,
    },
    /// Guided setup: detect or install bun, then write a starter config.
    Setup {
        /// Boot a secure managed Linux cluster and verify a sample container.
        #[arg(long, conflicts_with_all = ["dir", "release_url", "binary_dir"])]
        quickstart: bool,
        #[arg(long, requires = "quickstart")]
        name: Option<String>,
        #[arg(long, requires = "quickstart")]
        nodes: Option<usize>,
        #[arg(long, requires = "quickstart")]
        api_port: Option<u16>,
        #[arg(long, requires = "quickstart")]
        ingress_port: Option<u16>,
        /// Host port forwarded to the managed Pickle registry (default: 15050).
        #[arg(long, requires = "quickstart")]
        registry_port: Option<u16>,
        /// Use explicitly supplied Linux binaries for development before a release exists.
        #[arg(long, requires = "quickstart")]
        development_binaries: Option<PathBuf>,
        /// HTTPS directory containing unchanged signed release candidate assets.
        #[arg(long, requires = "quickstart", conflicts_with = "development_binaries")]
        release_mirror: Option<String>,
        /// Also print every setup step's timing (always saved as timings.json).
        #[arg(long, requires = "quickstart")]
        timings: bool,

        /// Accept the default answer to every question (non-interactive).
        #[arg(long)]
        yes: bool,
        /// Directory to write reliaburger.toml into.
        #[arg(long, default_value = ".")]
        dir: PathBuf,
        /// Release metadata URL.
        #[arg(long, default_value = reliaburger::relish::upgrade::DEFAULT_RELEASE_URL)]
        release_url: String,
        /// Directory to install the bun binary into
        /// (default: ~/.reliaburger/bin).
        #[arg(long)]
        binary_dir: Option<PathBuf>,
    },
    /// Run the built-in integration test suite against the cluster.
    Test {
        /// Comma-separated groups, or exact scenario names with --chaos. Omit for all.
        #[arg(long)]
        filter: Option<String>,
        /// Maximum concurrently running tests.
        #[arg(long, default_value_t = 4)]
        parallel: usize,
        /// Per-test timeout, e.g. "120s", "5m".
        #[arg(long, default_value = "120s")]
        timeout: String,
        /// Run the chaos suite instead of the integration suite.
        #[arg(long)]
        chaos: bool,
        /// Confirm real fault injection without granting server authority.
        #[arg(long, requires = "chaos")]
        yes: bool,
        /// Acceptance profile: development, full-runc, full-apple, process-grill.
        #[arg(long, default_value = "development")]
        profile: String,
        /// Readable rbtest-* prefix; each case receives a unique suffix.
        #[arg(long)]
        namespace: Option<String>,
    },
    /// Run reproducible performance benchmarks against the real data plane.
    Bench {
        /// Abbreviated suite for development and CI.
        #[arg(long)]
        quick: bool,
        /// Compare with a previous JSON benchmark report.
        #[arg(long)]
        compare: Option<PathBuf>,
        /// Deliberately schedule minimal workloads until the cluster is full.
        #[arg(long, requires = "yes")]
        capacity: bool,
        /// Include the leader-failure reconstruction benchmark.
        #[arg(long, requires = "yes")]
        disruptive: bool,
        /// Acknowledge capacity saturation and disruptive benchmark effects.
        #[arg(long)]
        yes: bool,
    },
    /// Diagnose cluster health and correlate likely causes.
    Wtf {
        /// Scope application checks and log correlation to one app.
        #[arg(long)]
        app: Option<String>,
        /// Re-run diagnosis every `--interval` seconds until Ctrl-C.
        #[arg(long)]
        watch: bool,
        /// Seconds between `--watch` collections.
        #[arg(long, default_value_t = 30, requires = "watch", value_parser = clap::value_parser!(u64).range(1..))]
        interval: u64,
    },
    /// Walk the network path from a workload to a destination, hop by hop.
    ///
    /// Checks DNS, the service VIP, the eBPF service map, the firewall, active
    /// faults and a real TCP connect from inside the source workload.
    Path {
        /// Source application name.
        source: String,
        /// Source namespace.
        #[arg(long, default_value = "default")]
        namespace: String,
        /// Destination application, hostname or IP address.
        #[arg(long)]
        to: String,
        /// Namespace of an internal destination.
        #[arg(long, default_value = "default")]
        to_namespace: String,
        /// Destination port. Internal services derive it when omitted.
        #[arg(long)]
        port: Option<u16>,
        /// Repeat the TCP connect this many times (1-10) and report how many
        /// succeeded and how long they took.
        #[arg(long, default_value_t = 1, value_parser = clap::value_parser!(u32).range(1..=10))]
        count: u32,
    },
}

#[derive(Subcommand)]
enum ManualAction {
    /// Write the embedded example configs into a directory.
    Examples {
        /// Target directory (an examples/ tree is created inside).
        #[arg(long, default_value = ".")]
        dir: PathBuf,
    },
}

#[derive(Subcommand)]
enum CouncilCommand {
    /// Recover a cluster whose entire council was lost.
    ///
    /// Run this against a STOPPED surviving node. It restores the desired
    /// state from a sealed backup (or this node's own durable snapshot),
    /// wipes the dead cluster's Raft log, and stamps a fresh recovery epoch.
    /// Starting the node afterwards re-bootstraps a single-voter council that
    /// the reconciler regrows. Writes made after the last backup are lost.
    Recover {
        /// The node's data directory (`[storage] data`), whose `raft/`
        /// subdirectory is recovered in place.
        #[arg(long)]
        data_dir: std::path::PathBuf,
        /// Restore from a sealed backup at this object-store URL
        /// (`file://`, `s3://`, `gs://`). Omit to restore from the node's own
        /// durable snapshot under `data_dir`.
        #[arg(long)]
        from: Option<String>,
        /// Path to the cluster master key file (32-byte hex), needed to
        /// unseal a backup. Defaults to `/etc/reliaburger/master.key`.
        #[arg(long)]
        master_key: Option<std::path::PathBuf>,
        /// Skip the "is a council still alive?" safety check. Only pass this
        /// when you are certain every voter is gone; recovering a live cluster
        /// splits the brain.
        #[arg(long)]
        force: bool,
    },
}

#[derive(Subcommand)]
enum UpgradeAction {
    /// Check for available updates.
    Check {
        /// Release metadata URL.
        #[arg(long, default_value = reliaburger::relish::upgrade::DEFAULT_RELEASE_URL)]
        url: String,
    },
    /// Start a rolling upgrade (network: pass a version; air-gapped:
    /// pass --binary).
    Start {
        /// Target version, e.g. v0.2.0 (downloads via the release metadata).
        version: Option<String>,
        /// Local binary to upgrade from instead (expects {path}.sig).
        #[arg(long)]
        binary: Option<std::path::PathBuf>,
        /// Signature envelope path (default: {binary}.sig).
        #[arg(long)]
        sig: Option<std::path::PathBuf>,
        /// Worker upgrade parallelism.
        #[arg(long, default_value = "1")]
        parallel: u32,
        /// Registry (host:port) relish pushes the binary to AND nodes fetch
        /// it from. By default relish pushes through this connection's
        /// registry (a quickstart host forward, or the API host) and tells
        /// nodes to fetch from the connected node's cluster address.
        #[arg(long)]
        registry: Option<String>,
        /// Release metadata URL.
        #[arg(long, default_value = reliaburger::relish::upgrade::DEFAULT_RELEASE_URL)]
        url: String,
        /// Per-node API address override, node_id=host:port (repeatable).
        #[arg(long = "node-address")]
        node_addresses: Vec<String>,
        /// Allow a target version older than what the nodes run. For
        /// returning to a version you rolled forward from, prefer
        /// `relish upgrade rollback`.
        #[arg(long)]
        allow_downgrade: bool,
    },
    /// Preview the rolling order and estimated duration.
    Plan {
        /// Target version (display only).
        version: String,
        /// Plan for a hypothetical cluster of this size instead of the
        /// live one.
        #[arg(long)]
        cluster_size: Option<usize>,
        /// Worker upgrade parallelism.
        #[arg(long, default_value = "1")]
        parallel: u32,
    },
    /// Show upgrade progress.
    Status,
    /// Roll back to a previous version (cluster: version required).
    Rollback {
        version: Option<String>,
        /// Per-node API address override, node_id=host:port (repeatable).
        #[arg(long = "node-address")]
        node_addresses: Vec<String>,
    },
    /// Resume a paused upgrade.
    Resume,
    /// End a paused upgrade in which no node has moved. When some nodes
    /// already swapped, use `rollback <version>` instead.
    Abort,
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
enum SignAction {
    /// Generate an image signing key and print the public key line for
    /// `[images.trust_policy] keys`.
    Keygen {
        /// Where to write the private key (PKCS#8 PEM, mode 0600). Refuses
        /// to overwrite an existing file.
        #[arg(long)]
        out: PathBuf,
    },
}

#[derive(Subcommand)]
enum JoinTokenAction {
    /// Create a single-use token for enrolling one node.
    Create {
        /// The node id this token may enrol. The joining node must present
        /// exactly this id (`relish join --node-id <id>`); the token cannot be
        /// used for any other node.
        #[arg(long)]
        node_id: String,
        /// Lifetime: an integer followed by s, m or h (1s to 1h).
        #[arg(long, default_value = "15m", value_parser = parse_join_token_ttl)]
        ttl: u64,
    },
}

fn parse_join_token_ttl(value: &str) -> Result<u64, String> {
    let (digits, multiplier) = match value.as_bytes().last().copied() {
        Some(b's') => (&value[..value.len() - 1], 1_u64),
        Some(b'm') => (&value[..value.len() - 1], 60_u64),
        Some(b'h') => (&value[..value.len() - 1], 3_600_u64),
        _ => return Err("TTL must end in s, m or h (for example 15m)".to_string()),
    };
    let amount = digits
        .parse::<u64>()
        .map_err(|_| "TTL must start with a whole number".to_string())?;
    let seconds = amount
        .checked_mul(multiplier)
        .ok_or_else(|| "TTL is too large".to_string())?;
    let min = reliaburger::sesame::join::MIN_JOIN_TOKEN_TTL.as_secs();
    let max = reliaburger::sesame::join::MAX_JOIN_TOKEN_TTL.as_secs();
    if !(min..=max).contains(&seconds) {
        return Err("TTL must be between 1s and 1h".to_string());
    }
    Ok(seconds)
}

#[derive(Subcommand)]
enum SecretAction {
    /// Print the cluster's age public key (for `relish secret encrypt`).
    ///
    /// Asks the configured cluster for its active key, using the same
    /// endpoint, token and CA as every other command. Pass the directory
    /// `relish init` wrote to read the key from disk instead, with no
    /// cluster running.
    Pubkey {
        /// Read the key offline from this `relish init` directory.
        dir: Option<PathBuf>,
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

#[derive(Debug, Clone, Copy, clap::ValueEnum)]
enum LocalAction {
    Status,
    Start,
    Stop,
    Destroy,
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
        /// Container runtime each node runs. `process` runs workloads as host
        /// processes, which the built-in `relish test` suite needs.
        #[arg(long, default_value = "runc", value_parser = ["runc", "process"])]
        runtime: String,
        /// Cluster name.
        #[arg(default_value = "default")]
        name: String,
        /// Pre-built Linux `bun` binary to install (skips the in-VM build).
        #[arg(long)]
        bun: Option<std::path::PathBuf>,
        /// Pre-built Linux `relish` binary to install (skips the in-VM build).
        #[arg(long)]
        relish: Option<std::path::PathBuf>,
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
    /// Generate an Ed25519 release signing keypair.
    Keygen {
        /// Output directory for release.key / release.pub.
        #[arg(long)]
        out: std::path::PathBuf,
    },
    /// Sign a binary, producing a detached .sig envelope.
    SignBinary {
        /// PKCS#8 Ed25519 private key (release key).
        #[arg(long)]
        key: std::path::PathBuf,
        /// Optional second key for the external signature.
        #[arg(long)]
        external_key: Option<std::path::PathBuf>,
        /// Where to write the envelope (default: {binary}.sig).
        #[arg(long)]
        out: Option<std::path::PathBuf>,
        /// Binary to sign.
        binary: std::path::PathBuf,
    },
    /// Add your external signature to a release binary's .sig envelope,
    /// keeping the release signature as it is (no release key needed).
    CountersignBinary {
        /// PKCS#8 Ed25519 private key (DER), e.g. from `relish dev keygen`
        /// or `openssl genpkey -algorithm ed25519 -outform DER`.
        #[arg(long)]
        external_key: std::path::PathBuf,
        /// The release envelope to countersign (default: {binary}.sig).
        #[arg(long)]
        sig: Option<std::path::PathBuf>,
        /// Where to write the countersigned envelope (default: over --sig).
        #[arg(long)]
        out: Option<std::path::PathBuf>,
        /// Binary the envelope belongs to.
        binary: std::path::PathBuf,
    },
}

#[derive(Subcommand)]
enum SnapshotAction {
    /// Snapshot an app's managed volumes.
    Create {
        /// App name.
        app: String,
        /// Namespace (default: "default").
        #[arg(short = 'n', long, default_value = "default")]
        namespace: String,
        /// Snapshot one volume (container mount path, e.g. /data);
        /// omitted = every provisioned volume.
        #[arg(long)]
        volume: Option<String>,
        /// Custom snapshot name (default: unix-seconds timestamp).
        #[arg(long)]
        name: Option<String>,
    },
    /// List an app's snapshots, newest first.
    List {
        /// App name.
        app: String,
        /// Namespace (default: "default").
        #[arg(short = 'n', long, default_value = "default")]
        namespace: String,
    },
    /// Restore a snapshot over its live volume (stop the app first).
    Restore {
        /// App name.
        app: String,
        /// Snapshot name.
        name: String,
        /// Namespace (default: "default").
        #[arg(short = 'n', long, default_value = "default")]
        namespace: String,
    },
    /// Delete a snapshot.
    Delete {
        /// App name.
        app: String,
        /// Snapshot name.
        name: String,
        /// Namespace (default: "default").
        #[arg(short = 'n', long, default_value = "default")]
        namespace: String,
    },
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
        /// Only delay traffic from this app (in the target's namespace).
        #[arg(long)]
        from: Option<String>,
        /// Fault duration (default: 10m).
        #[arg(long)]
        duration: Option<String>,
        #[command(flatten)]
        targeting: reliaburger::relish::fault::FaultTargeting,
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
        #[command(flatten)]
        targeting: reliaburger::relish::fault::FaultTargeting,
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
        #[command(flatten)]
        targeting: reliaburger::relish::fault::FaultTargeting,
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
        #[command(flatten)]
        targeting: reliaburger::relish::fault::FaultTargeting,
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
        #[command(flatten)]
        targeting: reliaburger::relish::fault::FaultTargeting,
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
        #[command(flatten)]
        targeting: reliaburger::relish::fault::FaultTargeting,
    },
    /// Push memory usage toward a service's limit.
    Memory {
        /// Target service name.
        target: String,
        /// Memory fill percentage (e.g. "90%").
        value: String,
        /// Fault duration (default: 10m).
        #[arg(long)]
        duration: Option<String>,
        #[command(flatten)]
        targeting: reliaburger::relish::fault::FaultTargeting,
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
        #[command(flatten)]
        targeting: reliaburger::relish::fault::FaultTargeting,
    },
    /// Kill instances of a service (SIGKILL).
    Kill {
        /// Target service or instance name.
        target: String,
        /// Number of instances to kill (0 = all).
        #[arg(long, default_value = "1")]
        count: u32,
        #[command(flatten)]
        targeting: reliaburger::relish::fault::FaultTargeting,
    },
    /// Freeze instances of a service (SIGSTOP).
    Pause {
        /// Target service name.
        target: String,
        /// Fault duration (default: 10m).
        #[arg(long)]
        duration: Option<String>,
        #[command(flatten)]
        targeting: reliaburger::relish::fault::FaultTargeting,
    },
    /// Resume (unfreeze) previously paused instances of a service.
    Resume {
        /// Target service name.
        target: String,
        #[command(flatten)]
        targeting: reliaburger::relish::fault::FaultTargeting,
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
        /// A human reason recorded alongside the fault.
        #[arg(long)]
        reason: Option<String>,
        /// Override the node-percentage safety rail.
        #[arg(long)]
        override_safety: bool,
        /// Confirm that this destructive node operation is intentional.
        #[arg(long)]
        acknowledge: bool,
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
        /// A human reason recorded alongside the fault.
        #[arg(long)]
        reason: Option<String>,
        /// Override the node-percentage safety rail.
        #[arg(long)]
        override_safety: bool,
        /// Confirm that this destructive node operation is intentional.
        #[arg(long)]
        acknowledge: bool,
    },
    /// Consume bounded CPU and memory capacity on one node.
    NodePressure {
        /// Target node name.
        target: String,
        /// Total-node CPU percentage to consume (e.g. "80%").
        #[arg(long, default_value = "0%")]
        cpu: String,
        /// Total-node memory-usage target (e.g. "90%").
        #[arg(long, default_value = "0%")]
        memory: String,
        /// Pressure duration (default: 10m).
        #[arg(long)]
        duration: Option<String>,
        /// Allow targeting the cluster leader.
        #[arg(long)]
        include_leader: bool,
        /// A human reason recorded alongside the fault.
        #[arg(long)]
        reason: Option<String>,
        /// Override the node-percentage safety rail.
        #[arg(long)]
        override_safety: bool,
        /// Confirm that capacity saturation is intentional.
        #[arg(long)]
        acknowledge: bool,
    },
    /// List all active faults.
    List,
    /// Clear faults — all, by numeric id, or by service name.
    Clear {
        /// Fault id or service name to clear (omit to clear all).
        target: Option<String>,
        /// Namespace of the service to clear. Omit to clear the service in
        /// every namespace (needs an unscoped token); name a namespace to
        /// clear only that tenant's faults.
        #[arg(long, requires = "target")]
        namespace: Option<String>,
        /// Node which owns a node-level fault id.
        #[arg(long, requires = "target")]
        node: Option<String>,
        /// Confirm that reversing a node-level fault is intentional.
        #[arg(long, requires = "node")]
        acknowledge: bool,
    },
    /// Run a scripted chaos scenario from a TOML file.
    #[command(visible_alias = "run")]
    Scenario {
        /// Path to the scenario TOML file.
        path: PathBuf,
        /// Print the scenario plan without executing.
        #[arg(long)]
        dry_run: bool,
        /// Speed multiplier (e.g. 2.0 = double speed).
        #[arg(long, default_value = "1.0")]
        speed: f64,
        /// Confirm that this scenario may inject its workload faults.
        #[arg(long)]
        acknowledge: bool,
    },
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();
    // Parse, then stop: tests/suite/website.rs checks the documented tour
    // commands against this exact parser without running any of them.
    if std::env::var_os("RELISH_PARSE_ONLY").is_some() {
        return ExitCode::SUCCESS;
    }

    // Record the global connection overrides before any client is built.
    reliaburger::relish::client::set_cli_token(cli.token.clone());
    reliaburger::relish::client::set_cli_ca_cert(cli.ca_cert.clone());
    if let Err(reason) = reliaburger::relish::client::set_cli_endpoint(cli.endpoint.clone()) {
        eprintln!("error: {reason}");
        return ExitCode::FAILURE;
    }

    let command = match cli.command {
        Some(command) => command,
        None => return finish(reliaburger::relish::tui::run().await),
    };

    let result = match command {
        Command::Tui => reliaburger::relish::tui::run().await,
        Command::Apply {
            ref path,
            ref file,
            dry_run,
            rerun_jobs,
        } => match reliaburger::relish::manifest::ManifestSource::parse(
            // The ArgGroup makes exactly one of the two present.
            path.as_deref().or(file.as_deref()).unwrap_or_default(),
        ) {
            Err(error) => Err(error),
            Ok(source) if rerun_jobs => commands::rerun_jobs(&source).await,
            Ok(source) => commands::apply(&source, cli.output, dry_run).await,
        },
        Command::Status => commands::status(cli.output).await,
        Command::Dashboard { port, no_open } => {
            reliaburger::relish::dashboard::run(port, no_open).await
        }
        Command::Logs {
            ref name,
            tail,
            follow,
            ref grep,
            ref since,
            ref json_field,
            ref namespace,
        } => {
            commands::logs(
                name,
                tail,
                follow,
                grep.clone(),
                since.clone(),
                json_field.clone(),
                namespace,
            )
            .await
        }
        Command::LogsExport {
            ref source,
            ref dest,
            ref node_id,
        } => commands::logs_export(dest, node_id, source.as_deref()).await,
        Command::LogsSearch {
            ref source,
            ref sql,
        } => commands::logs_search(source, sql).await,
        Command::Top => commands::top(cli.output).await,
        Command::Metrics {
            ref app,
            ref namespace,
            ref name,
            ref since,
        } => {
            reliaburger::relish::metrics_cmd::metrics(
                app,
                namespace,
                name.as_deref(),
                since,
                cli.output,
            )
            .await
        }
        Command::Exec {
            ref app,
            ref command,
            ref namespace,
        } => commands::exec(app, command, namespace).await,
        Command::Inspect { ref name } => commands::inspect(name).await,
        Command::Stop {
            ref app,
            ref namespace,
        } => commands::stop(app, namespace).await,
        Command::Delete {
            ref app,
            ref namespace,
        } => commands::delete(app, namespace).await,
        Command::Init {
            ref dir,
            ref cluster_name,
            ref node_id,
            development_plaintext,
        } => commands::init_with_security(
            dir,
            cluster_name,
            node_id,
            if development_plaintext {
                commands::InitSecurityMode::DevelopmentPlaintext
            } else {
                commands::InitSecurityMode::MutualTls
            },
        ),
        Command::Nodes => commands::nodes(cli.output).await,
        Command::DecommissionNode {
            node_id,
            workloads_stopped,
            reason,
        } => commands::decommission_node(&node_id, workloads_stopped, &reason, cli.output).await,
        Command::Council { ref action } => match action {
            None => commands::council(cli.output).await,
            Some(CouncilCommand::Recover {
                data_dir,
                from,
                master_key,
                force,
            }) => {
                commands::council_recover(data_dir, from.as_deref(), master_key.as_deref(), *force)
                    .await
            }
        },
        Command::Join {
            ref token,
            ref token_file,
            ref addr,
            ref node_id,
            ref identity_dir,
            ref ca_fingerprint,
        } => {
            async {
                let token = match (token, token_file) {
                    (Some(token), None) => token.clone(),
                    (None, Some(path)) => commands::read_join_token(path).await?,
                    _ => {
                        return Err(reliaburger::relish::RelishError::JoinFailed(
                            "provide exactly one of --token and --token-file".into(),
                        ));
                    }
                };
                commands::join(
                    &token,
                    addr,
                    node_id,
                    identity_dir.as_deref(),
                    ca_fingerprint.as_deref(),
                )
                .await
            }
            .await
        }
        Command::Resolve { ref name } => commands::resolve(name).await,
        Command::Routes => commands::routes().await,
        Command::Snapshot { ref action } => match action {
            SnapshotAction::Create {
                app,
                namespace,
                volume,
                name,
            } => {
                commands::snapshot_create(app, namespace, volume.as_deref(), name.as_deref()).await
            }
            SnapshotAction::List { app, namespace } => {
                commands::snapshot_list(app, namespace).await
            }
            SnapshotAction::Restore {
                app,
                name,
                namespace,
            } => commands::snapshot_restore(app, namespace, name).await,
            SnapshotAction::Delete {
                app,
                name,
                namespace,
            } => commands::snapshot_delete(app, namespace, name).await,
        },
        Command::Fault { ref action } => match action {
            FaultAction::Delay {
                target,
                delay,
                jitter,
                from,
                duration,
                targeting,
            } => {
                reliaburger::relish::fault::delay(
                    target,
                    delay,
                    jitter.as_deref(),
                    from.as_deref(),
                    duration,
                    targeting,
                )
                .await
            }
            FaultAction::Drop {
                target,
                percentage,
                duration,
                targeting,
            } => {
                reliaburger::relish::fault::drop_fault(target, percentage, duration, targeting)
                    .await
            }
            FaultAction::Dns {
                target,
                fault_type,
                duration,
                targeting,
            } => reliaburger::relish::fault::dns(target, fault_type, duration, targeting).await,
            FaultAction::Partition {
                target,
                from,
                duration,
                targeting,
            } => {
                reliaburger::relish::fault::partition(target, from.as_deref(), duration, targeting)
                    .await
            }
            FaultAction::Bandwidth {
                target,
                limit,
                duration,
                targeting,
            } => reliaburger::relish::fault::bandwidth(target, limit, duration, targeting).await,
            FaultAction::Cpu {
                target,
                percentage,
                cores,
                duration,
                targeting,
            } => {
                reliaburger::relish::fault::cpu(target, percentage, *cores, duration, targeting)
                    .await
            }
            FaultAction::Memory {
                target,
                value,
                duration,
                targeting,
            } => reliaburger::relish::fault::memory(target, value, duration, targeting).await,
            FaultAction::DiskIo {
                target,
                limit,
                write_only,
                duration,
                targeting,
            } => {
                reliaburger::relish::fault::disk_io(target, limit, *write_only, duration, targeting)
                    .await
            }
            FaultAction::Kill {
                target,
                count,
                targeting,
            } => reliaburger::relish::fault::kill(target, *count, targeting).await,
            FaultAction::Pause {
                target,
                duration,
                targeting,
            } => reliaburger::relish::fault::pause(target, duration, targeting).await,
            FaultAction::Resume { target, targeting } => {
                reliaburger::relish::fault::resume(target, targeting).await
            }
            FaultAction::NodeDrain {
                target,
                duration,
                include_leader,
                reason,
                override_safety,
                acknowledge,
            } => {
                reliaburger::relish::fault::node_drain(
                    target,
                    duration,
                    *include_leader,
                    reason.as_deref(),
                    *override_safety,
                    *acknowledge,
                )
                .await
            }
            FaultAction::NodeKill {
                target,
                duration,
                containers,
                include_leader,
                reason,
                override_safety,
                acknowledge,
            } => {
                reliaburger::relish::fault::node_kill(
                    target,
                    duration,
                    *containers,
                    *include_leader,
                    reason.as_deref(),
                    *override_safety,
                    *acknowledge,
                )
                .await
            }
            FaultAction::NodePressure {
                target,
                cpu,
                memory,
                duration,
                include_leader,
                reason,
                override_safety,
                acknowledge,
            } => {
                reliaburger::relish::fault::node_pressure(
                    target,
                    cpu,
                    memory,
                    duration,
                    *include_leader,
                    reason.as_deref(),
                    *override_safety,
                    *acknowledge,
                )
                .await
            }
            FaultAction::List => reliaburger::relish::fault::list().await,
            FaultAction::Clear {
                target,
                namespace,
                node,
                acknowledge,
            } => {
                reliaburger::relish::fault::clear(
                    target.clone(),
                    namespace.as_deref(),
                    node.as_deref(),
                    *acknowledge,
                )
                .await
            }
            FaultAction::Scenario {
                path,
                dry_run,
                speed,
                acknowledge,
            } => reliaburger::relish::fault::scenario(path, *dry_run, *speed, *acknowledge).await,
        },
        Command::Deploy { ref path, dry_run } => commands::deploy(path, cli.output, dry_run).await,
        Command::CancelDeploy { ref operation_id } => {
            commands::cancel_deploy(operation_id, cli.output).await
        }
        Command::History {
            ref app,
            ref namespace,
        } => commands::history(app, namespace, cli.output).await,
        Command::Rollback {
            ref app,
            ref namespace,
        } => commands::rollback(app, namespace).await,
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
        Command::Images => commands::images(cli.output).await,
        Command::Build {
            ref path,
            registry_port,
            timeout,
        } => commands::build(path, registry_port, timeout).await,
        Command::Batch { ref path } => commands::batch(path).await,
        Command::BatchStatus { id, wait, timeout } => {
            commands::batch_status(id, wait, timeout).await
        }
        Command::Secret { action } => match &action {
            SecretAction::Pubkey { dir } => commands::secret_pubkey(dir.as_deref()).await,
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
            } => {
                commands::token_create(
                    name,
                    role,
                    apps.as_deref(),
                    namespaces.as_deref(),
                    *ttl_days,
                )
                .await
            }
            TokenAction::List => commands::token_list().await,
            TokenAction::Revoke { name } => commands::token_revoke(name).await,
        },
        Command::JoinToken { action } => match &action {
            JoinTokenAction::Create { node_id, ttl } => {
                commands::join_token_create(node_id, *ttl).await
            }
        },
        Command::Sign {
            ref action,
            ref image,
            ref key,
        } => match (action, image, key) {
            (Some(SignAction::Keygen { out }), _, _) => commands::sign_keygen(out),
            (None, Some(image), Some(key)) => commands::sign(image, key).await,
            // clap enforces IMAGE and --key whenever no subcommand is given.
            (None, _, _) => Err(reliaburger::relish::RelishError::InvalidFlag {
                flag: "key".to_string(),
                reason: "relish sign needs IMAGE and --key PATH".to_string(),
            }),
        },
        Command::Dev { action } => match &action {
            DevAction::Create {
                nodes,
                cpus,
                memory,
                runtime,
                name,
                bun,
                relish,
            } => {
                reliaburger::relish::dev::create(
                    name,
                    *nodes,
                    *cpus,
                    memory,
                    runtime,
                    bun.clone(),
                    relish.clone(),
                )
                .await
            }
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
            DevAction::Keygen { out } => reliaburger::relish::dev::keygen(out),
            DevAction::SignBinary {
                key,
                external_key,
                out,
                binary,
            } => reliaburger::relish::dev::sign_binary(
                key,
                binary,
                external_key.as_deref(),
                out.as_deref(),
            ),
            DevAction::CountersignBinary {
                external_key,
                sig,
                out,
                binary,
            } => reliaburger::relish::dev::countersign_binary(
                external_key,
                binary,
                sig.as_deref(),
                out.as_deref(),
            )
            .map(|_| ()),
        },
        Command::Upgrade { action } => {
            let client = reliaburger::relish::client::BunClient::default_local();
            match action {
                UpgradeAction::Check { url } => {
                    reliaburger::relish::upgrade::check(&client, &url).await
                }
                UpgradeAction::Start {
                    version,
                    binary,
                    sig,
                    parallel,
                    registry,
                    url,
                    node_addresses,
                    allow_downgrade,
                } => {
                    reliaburger::relish::upgrade::start(
                        &client,
                        reliaburger::relish::upgrade::StartArgs {
                            version,
                            binary,
                            sig,
                            parallel,
                            registry,
                            metadata_url: url,
                            node_addresses,
                            allow_downgrade,
                        },
                    )
                    .await
                }
                UpgradeAction::Plan {
                    version,
                    cluster_size,
                    parallel,
                } => {
                    reliaburger::relish::upgrade::plan(&client, &version, cluster_size, parallel)
                        .await
                }
                UpgradeAction::Status => reliaburger::relish::upgrade::status(&client).await,
                UpgradeAction::Rollback {
                    version,
                    node_addresses,
                } => reliaburger::relish::upgrade::rollback(&client, version, node_addresses).await,
                UpgradeAction::Resume => reliaburger::relish::upgrade::resume(&client).await,
                UpgradeAction::Abort => reliaburger::relish::upgrade::abort(&client).await,
            }
        }
        Command::Manual {
            web,
            port,
            ref chapter,
            ref action,
        } => match action {
            Some(ManualAction::Examples { dir }) => reliaburger::relish::manual::examples(dir),
            None if web => reliaburger::relish::manual::web::serve(port).await,
            None => reliaburger::relish::manual::run(chapter.as_deref()).await,
        },
        Command::Source { query } => reliaburger::relish::source::run(query).await,
        Command::Uninstall { yes } => reliaburger::relish::uninstall::run(yes).map_err(Into::into),
        Command::Local {
            action,
            node,
            name,
            yes,
        } => {
            use reliaburger::relish::quickstart::lifecycle::{self, Action};
            let action = match action {
                LocalAction::Status => Action::Status,
                LocalAction::Start => Action::Start,
                LocalAction::Stop => Action::Stop,
                LocalAction::Destroy => Action::Destroy,
            };
            lifecycle::run(action, &name, node.as_deref(), yes)
                .await
                .map_err(|error| reliaburger::relish::RelishError::InitFailed(format!("{error:#}")))
        }
        Command::Setup {
            quickstart,
            name,
            nodes,
            api_port,
            ingress_port,
            registry_port,
            development_binaries,
            release_mirror,
            timings,
            yes,
            ref dir,
            ref release_url,
            ref binary_dir,
        } => {
            if quickstart {
                reliaburger::relish::quickstart::runner::run(
                    reliaburger::relish::quickstart::runner::Options {
                        name: name.unwrap_or_else(|| "laptop".into()),
                        nodes: nodes.unwrap_or(3),
                        api_port: api_port.unwrap_or(19117),
                        ingress_port: ingress_port.unwrap_or(18080),
                        registry_port: registry_port.unwrap_or(15050),
                        development_binaries,
                        release_mirror,
                        timings,
                    },
                )
                .await
                .map_err(|error| reliaburger::relish::RelishError::InitFailed(format!("{error:#}")))
            } else {
                reliaburger::relish::setup::run(reliaburger::relish::setup::SetupOptions {
                    yes,
                    dir: dir.clone(),
                    release_url: release_url.clone(),
                    binary_dir: binary_dir.clone(),
                })
                .await
            }
        }
        Command::Test {
            filter,
            parallel,
            timeout,
            chaos,
            yes,
            profile,
            namespace,
        } => {
            // `relish test` reports pass/fail through its exit code, so it
            // bypasses the plain `finish` and maps a CommandOutcome instead.
            return finish_outcome(
                reliaburger::relish::test_cmd::run(reliaburger::relish::test_cmd::TestArgs {
                    filter,
                    parallel,
                    timeout,
                    chaos,
                    yes,
                    profile,
                    namespace,
                    output: cli.output,
                })
                .await,
            );
        }
        Command::Bench {
            quick,
            compare,
            capacity,
            disruptive,
            yes,
        } => {
            return finish_outcome(
                reliaburger::relish::bench_cmd::run(reliaburger::relish::bench_cmd::BenchArgs {
                    quick,
                    compare,
                    capacity,
                    disruptive,
                    yes,
                    output: cli.output,
                })
                .await,
            );
        }
        Command::Wtf {
            app,
            watch,
            interval,
        } => {
            return finish_outcome(
                reliaburger::relish::wtf_cmd::run(reliaburger::relish::wtf_cmd::WtfArgs {
                    app,
                    watch,
                    interval: std::time::Duration::from_secs(interval),
                    output: cli.output,
                })
                .await,
            );
        }
        Command::Path {
            source,
            namespace,
            to,
            to_namespace,
            port,
            count,
        } => {
            return finish_outcome(
                reliaburger::relish::path_cmd::run(reliaburger::relish::path_cmd::PathArgs {
                    source,
                    source_namespace: namespace,
                    destination: to,
                    destination_namespace: to_namespace,
                    port,
                    count,
                    output: cli.output,
                })
                .await,
            );
        }
    };

    finish(result)
}

/// Map a diagnostic command's outcome to a process exit code.
///
/// A `RelishError` is a tool failure (exit 1). A `CommandOutcome` is the tool
/// succeeding and *reporting*: clean (0), problems found (1) or warnings (2).
fn finish_outcome(
    result: Result<reliaburger::relish::CommandOutcome, reliaburger::relish::RelishError>,
) -> ExitCode {
    match result {
        Ok(outcome) => ExitCode::from(outcome.exit_code()),
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn finish(result: Result<(), reliaburger::relish::RelishError>) -> ExitCode {
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

    struct ParsedCli {
        command: Command,
        output: OutputFormat,
        token: Option<String>,
    }

    fn parse(args: &[&str]) -> Result<ParsedCli, clap::Error> {
        Cli::try_parse_from(args).map(|cli| ParsedCli {
            command: cli.command.expect("test supplies a subcommand"),
            output: cli.output,
            token: cli.token,
        })
    }

    /// The README's command list is rendered from `Cli`. With
    /// `RELIABURGER_UPDATE_README` set (`make readme-commands`), this test
    /// rewrites the region instead of checking it.
    // Import and export only exist with the default `kubernetes` feature, so
    // the README describes that build.
    #[cfg(feature = "kubernetes")]
    #[test]
    fn readme_command_list_matches_the_cli() {
        use clap::CommandFactory;
        use reliaburger::relish::command_reference::{
            GROUPS, REGENERATE_COMMAND, render, replace_region,
        };

        let readme_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("README.md");
        let readme = std::fs::read_to_string(&readme_path).unwrap();
        let section = render(&Cli::command(), GROUPS).unwrap_or_else(|e| panic!("{e}"));
        let updated = replace_region(&readme, &section).unwrap_or_else(|e| panic!("{e}"));

        if std::env::var_os("RELIABURGER_UPDATE_README").is_some() {
            std::fs::write(&readme_path, &updated).unwrap();
            return;
        }
        assert!(
            updated == readme,
            "README.md's relish command list is out of date; run `{REGENERATE_COMMAND}`"
        );
    }

    #[test]
    fn secret_pubkey_asks_the_cluster_unless_given_an_init_directory() {
        let cli = Cli::try_parse_from(["relish", "secret", "pubkey"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Secret {
                action: SecretAction::Pubkey { dir: None }
            })
        ));
        let cli = Cli::try_parse_from(["relish", "secret", "pubkey", "cluster"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Secret {
                action: SecretAction::Pubkey { dir: Some(ref dir) }
            }) if dir == std::path::Path::new("cluster")
        ));
    }

    #[test]
    fn job_rerun_requires_an_explicit_flag_and_conflicts_with_dry_run() {
        let cli = Cli::try_parse_from(["relish", "apply", "jobs.toml", "--rerun-jobs"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Apply {
                rerun_jobs: true,
                ..
            })
        ));
        assert!(
            Cli::try_parse_from(["relish", "apply", "jobs.toml", "--rerun-jobs", "--dry-run"])
                .is_err()
        );
    }

    #[test]
    fn decommission_requires_explicit_workload_attestation_and_reason() {
        assert!(
            Cli::try_parse_from([
                "relish",
                "decommission-node",
                "worker",
                "--reason",
                "maintenance"
            ])
            .is_err()
        );
        assert!(
            Cli::try_parse_from([
                "relish",
                "decommission-node",
                "worker",
                "--workloads-stopped"
            ])
            .is_err()
        );
        assert!(matches!(
            Cli::try_parse_from([
                "relish",
                "decommission-node",
                "worker",
                "--workloads-stopped",
                "--reason",
                "maintenance"
            ])
            .unwrap()
            .command,
            Some(Command::DecommissionNode {
                workloads_stopped: true,
                ..
            })
        ));
    }

    #[test]
    fn dashboard_accepts_a_port_and_headless_browser_mode() {
        let cli =
            Cli::try_parse_from(["relish", "dashboard", "--port", "18117", "--no-open"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Dashboard {
                port: 18117,
                no_open: true
            })
        ));
    }

    #[test]
    fn bare_relish_and_tui_are_valid_entry_points() {
        let bare = Cli::try_parse_from(["relish"]).unwrap();
        assert!(bare.command.is_none());
        let tui = Cli::try_parse_from(["relish", "tui"]).unwrap();
        assert!(matches!(tui.command, Some(Command::Tui)));
    }

    #[test]
    fn fault_run_is_an_alias_for_scenario() {
        let via_alias = parse(&["relish", "fault", "run", "scenario.toml"]).unwrap();
        assert!(matches!(
            via_alias.command,
            Command::Fault {
                action: FaultAction::Scenario { .. }
            }
        ));
    }

    #[test]
    fn fault_targeting_flags_parse() {
        let cli = parse(&[
            "relish",
            "fault",
            "delay",
            "redis",
            "200ms",
            "--instance",
            "redis-1",
            "--node",
            "node-2",
            "--reason",
            "game-day",
            "--override-safety",
            "--acknowledge",
        ])
        .unwrap();
        match cli.command {
            Command::Fault {
                action: FaultAction::Delay { targeting, .. },
            } => {
                assert_eq!(targeting.instance.as_deref(), Some("redis-1"));
                assert_eq!(targeting.node.as_deref(), Some("node-2"));
                assert_eq!(targeting.reason.as_deref(), Some("game-day"));
                assert!(targeting.override_safety);
                assert!(targeting.acknowledge);
            }
            _ => panic!("expected a fault delay command"),
        }
    }

    #[test]
    fn fault_clear_and_resume_parse() {
        let clear_by_name = parse(&["relish", "fault", "clear", "redis"]).unwrap();
        assert!(matches!(
            clear_by_name.command,
            Command::Fault {
                action: FaultAction::Clear {
                    target: Some(ref t),
                    ..
                }
            } if t == "redis"
        ));
        let resume = parse(&["relish", "fault", "resume", "redis"]).unwrap();
        assert!(matches!(
            resume.command,
            Command::Fault {
                action: FaultAction::Resume { .. }
            }
        ));
    }

    #[test]
    fn parse_test_command_defaults_and_flags() {
        let bare = parse(&["relish", "test"]).unwrap();
        assert!(matches!(
            bare.command,
            Command::Test {
                filter: None,
                parallel: 4,
                chaos: false,
                ref profile,
                namespace: None,
                ..
            } if profile == "development"
        ));

        let full = parse(&[
            "relish",
            "test",
            "--filter",
            "scheduling,firewall",
            "--parallel",
            "8",
            "--timeout",
            "5m",
            "--chaos",
            "--profile",
            "full-runc",
            "--namespace",
            "rbtest-fixed",
        ])
        .unwrap();
        match full.command {
            Command::Test {
                filter,
                parallel,
                timeout,
                chaos,
                yes,
                profile,
                namespace,
            } => {
                assert_eq!(filter.as_deref(), Some("scheduling,firewall"));
                assert_eq!(parallel, 8);
                assert_eq!(timeout, "5m");
                assert!(chaos);
                assert!(!yes);
                assert_eq!(profile, "full-runc");
                assert_eq!(namespace.as_deref(), Some("rbtest-fixed"));
            }
            _ => panic!("expected a Test command"),
        }
    }

    #[test]
    fn parse_bench_command_defaults_and_explicit_risk_flags() {
        let bare = parse(&["relish", "bench"]).unwrap();
        assert!(matches!(
            bare.command,
            Command::Bench {
                quick: false,
                compare: None,
                capacity: false,
                disruptive: false,
                yes: false,
            }
        ));
        assert!(parse(&["relish", "bench", "--capacity"]).is_err());
        assert!(parse(&["relish", "bench", "--disruptive"]).is_err());

        let full = parse(&[
            "relish",
            "bench",
            "--quick",
            "--compare",
            "base.json",
            "--capacity",
            "--disruptive",
            "--yes",
        ])
        .unwrap();
        assert!(matches!(
            full.command,
            Command::Bench {
                quick: true,
                compare: Some(ref path),
                capacity: true,
                disruptive: true,
                yes: true,
            } if path == std::path::Path::new("base.json")
        ));
    }

    #[test]
    fn parse_wtf_scope_watch_and_machine_output() {
        let bare = parse(&["relish", "wtf"]).unwrap();
        assert!(matches!(
            bare.command,
            Command::Wtf {
                app: None,
                watch: false,
                interval: 30
            }
        ));

        let scoped = parse(&[
            "relish", "--output", "json", "wtf", "--app", "payments", "--watch",
        ])
        .unwrap();
        assert_eq!(scoped.output, OutputFormat::Json);
        assert!(matches!(
            scoped.command,
            Command::Wtf {
                app: Some(ref app),
                watch: true,
                interval: 30
            } if app == "payments"
        ));

        let fast = parse(&["relish", "wtf", "--watch", "--interval", "5"]).unwrap();
        assert!(matches!(
            fast.command,
            Command::Wtf {
                watch: true,
                interval: 5,
                ..
            }
        ));
        assert!(parse(&["relish", "wtf", "--interval", "5"]).is_err());
        assert!(parse(&["relish", "wtf", "--watch", "--interval", "0"]).is_err());
    }

    #[test]
    fn parse_path_namespaces_port_and_machine_output() {
        let parsed = parse(&[
            "relish",
            "--output",
            "yaml",
            "path",
            "api",
            "--namespace",
            "frontend",
            "--to",
            "db",
            "--to-namespace",
            "storage",
            "--port",
            "5432",
        ])
        .unwrap();
        assert_eq!(parsed.output, OutputFormat::Yaml);
        assert!(matches!(
            parsed.command,
            Command::Path {
                source,
                namespace,
                to,
                to_namespace,
                port: Some(5432),
                count: 1,
            } if source == "api"
                && namespace == "frontend"
                && to == "db"
                && to_namespace == "storage"
        ));
    }

    #[test]
    fn path_count_repeats_the_connect_up_to_ten_times() {
        let parsed = parse(&[
            "relish", "path", "frontend", "--to", "redis", "--count", "10",
        ])
        .unwrap();
        assert!(matches!(parsed.command, Command::Path { count: 10, .. }));
        assert!(
            parse(&[
                "relish", "path", "frontend", "--to", "redis", "--count", "0"
            ])
            .is_err()
        );
        assert!(
            parse(&[
                "relish", "path", "frontend", "--to", "redis", "--count", "11"
            ])
            .is_err()
        );
    }

    #[test]
    fn trace_is_not_a_subcommand() {
        assert!(parse(&["relish", "trace", "frontend", "--to", "redis"]).is_err());
    }

    #[test]
    fn test_command_has_no_client_side_production_override_but_accepts_consent() {
        assert!(parse(&["relish", "test", "--override"]).is_err());
        let parsed = parse(&["relish", "test", "--chaos", "--yes"]).unwrap();
        assert!(matches!(
            parsed.command,
            Command::Test {
                chaos: true,
                yes: true,
                ..
            }
        ));
    }

    #[test]
    fn parse_apply_command() {
        let cli = parse(&["relish", "apply", "config.toml"]).unwrap();
        assert!(
            matches!(cli.command, Command::Apply { ref path, file: None, dry_run: false, rerun_jobs: false } if path.as_deref() == Some("config.toml"))
        );
    }

    /// Z1.4: `-f` takes a path or URL, the kubectl way; the positional
    /// form keeps working, and exactly one of them is required.
    #[test]
    fn parse_apply_file_flag_and_url() {
        for flag in ["-f", "--file"] {
            let cli = parse(&["relish", "apply", flag, "app.yaml"]).unwrap();
            assert!(
                matches!(cli.command, Command::Apply { path: None, ref file, .. } if file.as_deref() == Some("app.yaml"))
            );
        }
        let url = "https://reliaburger.com/demo/podinfo.yaml";
        let cli = parse(&["relish", "apply", "-f", url, "--dry-run"]).unwrap();
        assert!(
            matches!(cli.command, Command::Apply { ref file, dry_run: true, .. } if file.as_deref() == Some(url))
        );
        assert!(
            parse(&["relish", "apply"]).is_err(),
            "a manifest is required"
        );
        assert!(
            parse(&["relish", "apply", "a.toml", "-f", "b.yaml"]).is_err(),
            "one manifest at a time"
        );
    }

    #[test]
    fn parse_apply_dry_run_flag() {
        let cli = parse(&["relish", "apply", "config.toml", "--dry-run"]).unwrap();
        assert!(matches!(cli.command, Command::Apply { dry_run: true, .. }));
    }

    #[test]
    fn parse_logs_filter_flags() {
        let cli = parse(&[
            "relish",
            "logs",
            "web",
            "--grep",
            "error",
            "--since",
            "5m",
            "--json-field",
            "level=warn",
        ])
        .unwrap();
        assert!(matches!(
            cli.command,
            Command::Logs {
                ref grep,
                ref since,
                ref json_field,
                ..
            } if grep.as_deref() == Some("error")
                && since.as_deref() == Some("5m")
                && json_field.as_deref() == Some("level=warn")
        ));
    }

    #[test]
    fn parse_status_command() {
        let cli = parse(&["relish", "status"]).unwrap();
        assert!(matches!(cli.command, Command::Status));
    }

    #[test]
    fn delete_parses_app_and_namespace() {
        let cli = parse(&["relish", "delete", "web", "--namespace", "team-a"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Delete { ref app, ref namespace } if app == "web" && namespace == "team-a"
        ));
    }

    #[test]
    fn parse_namespace_flag_threads_through_app_commands() {
        // M21: logs/exec/stop/rollback accept --namespace so an app in a
        // non-default namespace can be managed, defaulting to "default".
        let cli = parse(&["relish", "stop", "web", "--namespace", "team-a"]).unwrap();
        assert!(matches!(cli.command, Command::Stop { namespace, .. } if namespace == "team-a"));
        let cli = parse(&["relish", "rollback", "web"]).unwrap();
        assert!(
            matches!(cli.command, Command::Rollback { namespace, .. } if namespace == "default")
        );
        let cli = parse(&["relish", "exec", "web", "--namespace", "team-a", "sh"]).unwrap();
        assert!(matches!(cli.command, Command::Exec { namespace, .. } if namespace == "team-a"));
    }

    #[test]
    fn parse_exec_with_trailing_args() {
        let cli = parse(&["relish", "exec", "web", "sh", "-c", "ls"]).unwrap();
        match cli.command {
            Command::Exec { app, command, .. } => {
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
                development_plaintext,
            } => {
                assert_eq!(dir.to_str(), Some("."));
                assert_eq!(cluster_name, "default");
                assert_eq!(node_id, "node-01");
                assert!(!development_plaintext);
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
    fn parse_init_requires_an_explicit_development_plaintext_flag() {
        let cli = parse(&["relish", "init", "--development-plaintext"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Init {
                development_plaintext: true,
                ..
            }
        ));
    }

    #[test]
    fn parse_sign_takes_an_image_and_a_key() {
        let cli = parse(&["relish", "sign", "myapp:v1", "--key", "ci.pem"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Sign { action: None, image: Some(ref image), key: Some(ref key) }
                if image == "myapp:v1" && key == &PathBuf::from("ci.pem")
        ));
    }

    #[test]
    fn parse_sign_requires_a_key() {
        assert!(parse(&["relish", "sign", "myapp:v1"]).is_err());
        assert!(parse(&["relish", "sign", "--key", "ci.pem"]).is_err());
    }

    #[test]
    fn parse_sign_keygen_needs_no_image() {
        let cli = parse(&["relish", "sign", "keygen", "--out", "ci.pem"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Sign { action: Some(SignAction::Keygen { ref out }), .. }
                if out == &PathBuf::from("ci.pem")
        ));
    }

    #[test]
    fn parse_nodes_command() {
        let cli = parse(&["relish", "nodes"]).unwrap();
        assert!(matches!(cli.command, Command::Nodes));
    }

    #[test]
    fn parse_council_command() {
        let cli = parse(&["relish", "council"]).unwrap();
        assert!(matches!(cli.command, Command::Council { action: None }));
    }

    #[test]
    fn parse_council_recover_command() {
        let cli = parse(&[
            "relish",
            "council",
            "recover",
            "--data-dir",
            "/var/lib/reliaburger/data",
            "--from",
            "file:///var/backups/council",
            "--force",
        ])
        .unwrap();
        assert!(matches!(
            cli.command,
            Command::Council {
                action: Some(CouncilCommand::Recover { force: true, .. })
            }
        ));
    }

    #[test]
    fn parse_release_mirror_requires_managed_signed_quickstart() {
        assert!(
            parse(&[
                "relish",
                "setup",
                "--quickstart",
                "--release-mirror",
                "https://example.com/candidate/"
            ])
            .is_ok()
        );
        assert!(
            parse(&[
                "relish",
                "setup",
                "--release-mirror",
                "https://example.com/candidate/"
            ])
            .is_err()
        );
        assert!(
            parse(&[
                "relish",
                "setup",
                "--quickstart",
                "--release-mirror",
                "https://example.com/candidate/",
                "--development-binaries",
                "/tmp/binaries"
            ])
            .is_err()
        );
    }

    #[test]
    fn parse_managed_quickstart_and_explicit_destroy() {
        assert!(parse(&["relish", "setup", "--quickstart", "--nodes", "3"]).is_ok());
        assert!(parse(&["relish", "setup", "--quickstart", "--timings"]).is_ok());
        assert!(parse(&["relish", "setup", "--timings"]).is_err());
        assert!(parse(&["relish", "local", "status"]).is_ok());
        let cli = parse(&["relish", "local", "stop", "node-3"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Local { node: Some(ref node), yes: false, .. } if node == "node-3"
        ));
        let cli = parse(&["relish", "local", "start", "2", "--name", "demo"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Local { node: Some(ref node), ref name, .. } if node == "2" && name == "demo"
        ));
        assert!(parse(&["relish", "local", "stop", "1", "--yes"]).is_ok());
        assert!(parse(&["relish", "local", "destroy", "--name", "laptop", "--yes"]).is_ok());
        assert!(parse(&["relish", "setup", "--nodes", "3"]).is_err());
    }

    #[test]
    fn parse_join_token_file_and_reject_conflicting_credentials() {
        assert!(
            parse(&[
                "relish",
                "join",
                "--token-file",
                "/private/join.token",
                "--node-id",
                "node-02",
                "https://10.0.1.5:9117"
            ])
            .is_ok()
        );
        assert!(
            parse(&[
                "relish",
                "join",
                "--token",
                "secret",
                "--token-file",
                "/private/join.token",
                "--node-id",
                "node-02",
                "https://10.0.1.5:9117"
            ])
            .is_err()
        );
        assert!(
            parse(&[
                "relish",
                "join",
                "--node-id",
                "node-02",
                "https://10.0.1.5:9117"
            ])
            .is_err()
        );
    }

    #[test]
    fn parse_join_command() {
        let cli = parse(&[
            "relish",
            "join",
            "--token",
            "abc123",
            "--node-id",
            "node-02",
            "https://10.0.1.5:9117",
        ])
        .unwrap();
        match cli.command {
            Command::Join {
                token,
                token_file,
                addr,
                node_id,
                identity_dir,
                ca_fingerprint,
            } => {
                assert_eq!(token.as_deref(), Some("abc123"));
                assert!(token_file.is_none());
                assert_eq!(addr, "https://10.0.1.5:9117");
                assert_eq!(node_id, "node-02");
                assert!(identity_dir.is_none());
                assert!(ca_fingerprint.is_none());
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
        assert!(matches!(cli.command, Command::Stop { ref app, .. } if app == "web"));
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
                        ..
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
            "relish", "dev", "create", "big", "--nodes", "5", "--cpus", "4", "--memory", "4GiB",
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
                        ..
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
    fn parse_metrics_command_defaults_and_flags() {
        let cli = parse(&["relish", "metrics", "web"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Metrics { ref app, ref namespace, name: None, ref since }
                if app == "web" && namespace == "default" && since == "15m"
        ));
        let cli = parse(&[
            "relish",
            "metrics",
            "web",
            "--namespace",
            "shop",
            "--name",
            "http_requests_total",
            "--since",
            "1h",
        ])
        .unwrap();
        assert!(matches!(
            cli.command,
            Command::Metrics { ref namespace, name: Some(ref name), ref since, .. }
                if namespace == "shop" && name == "http_requests_total" && since == "1h"
        ));
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
    fn parse_cancel_deploy_requires_an_operation_id() {
        let cli = parse(&["relish", "cancel-deploy", "deploy-123"]).unwrap();
        assert!(
            matches!(cli.command, Command::CancelDeploy { operation_id } if operation_id == "deploy-123")
        );
        assert!(parse(&["relish", "cancel-deploy"]).is_err());
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
            Command::History { app, namespace } => {
                assert_eq!(app, "web");
                assert_eq!(namespace, "default");
            }
            _ => panic!("expected History"),
        }
    }

    #[test]
    fn parse_history_command_with_namespace() {
        let cli = parse(&["relish", "history", "web", "--namespace", "team-a"]).unwrap();
        match cli.command {
            Command::History { app, namespace } => {
                assert_eq!(app, "web");
                assert_eq!(namespace, "team-a");
            }
            _ => panic!("expected History"),
        }
    }

    #[test]
    fn parse_rollback_command() {
        let cli = parse(&["relish", "rollback", "web"]).unwrap();
        match cli.command {
            Command::Rollback { app, .. } => assert_eq!(app, "web"),
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
    fn parse_fault_delay_from_one_source() {
        let cli = parse(&[
            "relish", "fault", "delay", "redis", "300ms", "--from", "frontend",
        ])
        .unwrap();
        match cli.command {
            Command::Fault {
                action: FaultAction::Delay { from, .. },
            } => assert_eq!(from.as_deref(), Some("frontend")),
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
                        ..
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
                action: FaultAction::Kill { target, count, .. },
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
            "--acknowledge",
        ])
        .unwrap();
        match cli.command {
            Command::Fault {
                action:
                    FaultAction::NodeKill {
                        target,
                        containers,
                        include_leader,
                        acknowledge,
                        duration,
                        ..
                    },
            } => {
                assert_eq!(target, "node-05");
                assert!(containers);
                assert!(include_leader);
                assert!(acknowledge);
                assert_eq!(duration.as_deref(), Some("30s"));
            }
            _ => panic!("expected Fault NodeKill"),
        }
    }

    #[test]
    fn parse_fault_node_pressure_with_server_gated_targets() {
        let cli = parse(&[
            "relish",
            "fault",
            "node-pressure",
            "worker-2",
            "--cpu",
            "80%",
            "--memory",
            "90%",
            "--duration",
            "30s",
            "--acknowledge",
        ])
        .unwrap();
        match cli.command {
            Command::Fault {
                action:
                    FaultAction::NodePressure {
                        target,
                        cpu,
                        memory,
                        duration,
                        acknowledge,
                        ..
                    },
            } => {
                assert_eq!(target, "worker-2");
                assert_eq!(cpu, "80%");
                assert_eq!(memory, "90%");
                assert_eq!(duration.as_deref(), Some("30s"));
                assert!(acknowledge);
            }
            _ => panic!("expected Fault NodePressure"),
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
                action: FaultAction::Clear { target, .. },
            } => assert!(target.is_none()),
            _ => panic!("expected Fault Clear"),
        }
    }

    #[test]
    fn parse_fault_clear_by_id() {
        let cli = parse(&["relish", "fault", "clear", "42"]).unwrap();
        match cli.command {
            Command::Fault {
                action: FaultAction::Clear { target, .. },
            } => assert_eq!(target.as_deref(), Some("42")),
            _ => panic!("expected Fault Clear"),
        }
    }

    #[test]
    fn parse_fault_clear_on_node_requires_explicit_targeting() {
        let cli = parse(&[
            "relish",
            "fault",
            "clear",
            "42",
            "--node",
            "node-05",
            "--acknowledge",
        ])
        .unwrap();
        match cli.command {
            Command::Fault {
                action: FaultAction::Clear { target, .. },
            } => assert_eq!(target.as_deref(), Some("42")),
            _ => panic!("expected Fault Clear"),
        }
    }

    #[test]
    fn parse_build_command() {
        let cli = parse(&["relish", "build", "build.toml"]).unwrap();
        match cli.command {
            Command::Build { timeout, .. } => assert_eq!(timeout, 960),
            _ => panic!("expected Build"),
        }
    }

    #[test]
    fn parse_batch_status_wait_flags() {
        let cli = parse(&["relish", "batch-status", "7", "--wait", "--timeout", "30"]).unwrap();
        match cli.command {
            Command::BatchStatus { id, wait, timeout } => {
                assert_eq!(id, 7);
                assert!(wait);
                assert_eq!(timeout, 30);
            }
            _ => panic!("expected BatchStatus"),
        }
    }

    #[test]
    fn parse_batch_command() {
        let cli = parse(&["relish", "batch", "jobs.toml"]).unwrap();
        assert!(matches!(cli.command, Command::Batch { .. }));
    }

    #[test]
    fn parse_snapshot_commands() {
        let cli = parse(&[
            "relish", "snapshot", "create", "db", "-n", "prod", "--volume", "/data",
        ])
        .unwrap();
        assert!(matches!(
            cli.command,
            Command::Snapshot {
                action: SnapshotAction::Create { .. }
            }
        ));

        let cli = parse(&["relish", "snapshot", "restore", "db", "1752000000"]).unwrap();
        match cli.command {
            Command::Snapshot {
                action:
                    SnapshotAction::Restore {
                        app,
                        name,
                        namespace,
                    },
            } => {
                assert_eq!(app, "db");
                assert_eq!(name, "1752000000");
                assert_eq!(namespace, "default");
            }
            _ => panic!("expected a snapshot restore command"),
        }
    }

    #[test]
    fn token_flag_parses_globally() {
        // --token is accepted after the subcommand (global).
        let cli = parse(&["relish", "status", "--token", "rbrg_abc"]).unwrap();
        assert_eq!(cli.token.as_deref(), Some("rbrg_abc"));
        // Absent by default.
        let cli = parse(&["relish", "status"]).unwrap();
        assert!(cli.token.is_none());
    }

    #[test]
    fn endpoint_flag_parses_globally() {
        let cli = Cli::try_parse_from([
            "relish",
            "status",
            "--endpoint",
            "https://node-01.example:9117",
        ])
        .unwrap();
        assert_eq!(
            cli.endpoint.as_deref(),
            Some("https://node-01.example:9117")
        );
    }

    #[test]
    fn endpoint_flag_rejects_remote_plaintext() {
        let result = Cli::try_parse_from([
            "relish",
            "status",
            "--endpoint",
            "http://node-01.example:9117",
        ]);
        let error = match result {
            Ok(_) => panic!("remote plaintext endpoint should be rejected"),
            Err(error) => error,
        };
        assert!(error.to_string().contains("HTTPS"));
    }

    #[test]
    fn parse_manual_defaults_to_the_reader() {
        let cli = parse(&["relish", "manual"]).unwrap();
        match cli.command {
            Command::Manual {
                web,
                port,
                ref chapter,
                ref action,
            } => {
                assert!(!web);
                assert_eq!(port, 8642);
                assert!(chapter.is_none());
                assert!(action.is_none());
            }
            _ => panic!("expected Manual command"),
        }
    }

    #[test]
    fn parse_manual_web_with_port() {
        let cli = parse(&["relish", "manual", "--web", "--port", "0"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Manual {
                web: true,
                port: 0,
                chapter: None,
                action: None,
            }
        ));
    }

    #[test]
    fn parse_manual_chapter_and_keep_examples_a_subcommand() {
        let cli = parse(&["relish", "manual", "tour"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Manual { chapter: Some(ref chapter), action: None, .. } if chapter == "tour"
        ));
        let cli = parse(&["relish", "manual", "examples"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Manual {
                chapter: None,
                action: Some(ManualAction::Examples { .. }),
                ..
            }
        ));
    }

    #[test]
    fn parse_manual_examples_with_dir() {
        let cli = parse(&["relish", "manual", "examples", "--dir", "/tmp/burger"]).unwrap();
        match cli.command {
            Command::Manual {
                action: Some(ManualAction::Examples { ref dir }),
                ..
            } => assert_eq!(dir.to_str(), Some("/tmp/burger")),
            _ => panic!("expected Manual Examples command"),
        }
    }

    #[test]
    fn parse_source_without_query() {
        let cli = parse(&["relish", "source"]).unwrap();
        assert!(matches!(cli.command, Command::Source { query: None }));
    }

    #[test]
    fn parse_source_with_seed_query() {
        let cli = parse(&["relish", "source", "ebpf"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Source { query: Some(ref q) } if q == "ebpf"
        ));
    }

    #[test]
    fn parse_setup_defaults() {
        let cli = parse(&["relish", "setup"]).unwrap();
        match cli.command {
            Command::Setup {
                yes,
                ref dir,
                ref release_url,
                ref binary_dir,
                ..
            } => {
                assert!(!yes);
                assert_eq!(dir.to_str(), Some("."));
                assert_eq!(
                    release_url,
                    reliaburger::relish::upgrade::DEFAULT_RELEASE_URL
                );
                assert!(binary_dir.is_none());
            }
            _ => panic!("expected Setup command"),
        }
    }

    #[test]
    fn parse_setup_with_flags() {
        let cli = parse(&[
            "relish",
            "setup",
            "--yes",
            "--dir",
            "/tmp/burger",
            "--binary-dir",
            "/opt/reliaburger/bin",
        ])
        .unwrap();
        match cli.command {
            Command::Setup {
                yes,
                ref dir,
                ref binary_dir,
                ..
            } => {
                assert!(yes);
                assert_eq!(dir.to_str(), Some("/tmp/burger"));
                assert_eq!(
                    binary_dir.as_ref().and_then(|p| p.to_str()),
                    Some("/opt/reliaburger/bin")
                );
            }
            _ => panic!("expected Setup command"),
        }
    }

    #[test]
    fn parse_join_token_create_with_a_bounded_ttl() {
        let cli = Cli::try_parse_from([
            "relish",
            "join-token",
            "create",
            "--node-id",
            "node-02",
            "--ttl",
            "30m",
        ])
        .unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::JoinToken {
                action: JoinTokenAction::Create { ttl: 1_800, .. }
            })
        ));
        // TTL defaults to 15m when omitted; node id is required.
        let cli = Cli::try_parse_from(["relish", "join-token", "create", "--node-id", "node-02"])
            .unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::JoinToken {
                action: JoinTokenAction::Create { ttl: 900, .. }
            })
        ));
        // --node-id is mandatory: a token must be bound to exactly one node.
        assert!(Cli::try_parse_from(["relish", "join-token", "create"]).is_err());
        assert!(
            Cli::try_parse_from([
                "relish",
                "join-token",
                "create",
                "--node-id",
                "node-02",
                "--ttl",
                "0s"
            ])
            .is_err()
        );
        assert!(
            Cli::try_parse_from([
                "relish",
                "join-token",
                "create",
                "--node-id",
                "node-02",
                "--ttl",
                "61m"
            ])
            .is_err()
        );
    }
}
