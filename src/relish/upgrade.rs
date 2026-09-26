//! `relish upgrade` — driving rolling binary upgrades from the CLI.
//!
//! Talks to the local bun's API (like every other relish command). On a
//! cluster it records the plan with the leader and the orchestrator does
//! the walking; on a single node it sends the node-level directive
//! directly.

use std::path::Path;

use crate::upgrade::signing::{self, SignatureEnvelope};
use crate::upgrade::types::{BinarySource, UpgradeDirective};
use crate::upgrade::{BinaryVersion, metadata};

use super::RelishError;
use super::client::BunClient;

/// Default release metadata endpoint; `relish upgrade check --url` overrides it.
pub use crate::upgrade::metadata::DEFAULT_RELEASE_URL;

/// Assumed duration of one node's swap+verify, for `plan` estimates.
const SECONDS_PER_NODE: u64 = 45;

/// `relish upgrade check` — compare this node's version to the latest release.
pub async fn check(client: &BunClient, url: &str) -> Result<(), RelishError> {
    let metadata = metadata::fetch(url).await?;
    let running = client.node_version().await?;

    println!("running: {running}");
    println!("latest:  {}", metadata.latest);
    let running_version: BinaryVersion = running
        .parse()
        .map_err(|e: crate::upgrade::UpgradeError| RelishError::FormatFailed(e.to_string()))?;
    if running_version < metadata.latest {
        let platform = metadata::platform_key();
        match metadata.artifact_for(&metadata.latest, &platform) {
            Some(_) => println!(
                "upgrade available: relish upgrade start {}",
                metadata.latest
            ),
            None => println!(
                "a newer version exists but has no artefact for this platform ({platform})"
            ),
        }
    } else {
        println!("up to date");
    }
    Ok(())
}

/// Arguments for `relish upgrade start`.
pub struct StartArgs {
    /// Target version (network flow). Mutually exclusive with `binary`.
    pub version: Option<String>,
    /// Local binary path (air-gapped flow). Expects `{path}.sig` beside it
    /// unless `sig` is given.
    pub binary: Option<std::path::PathBuf>,
    pub sig: Option<std::path::PathBuf>,
    pub parallel: u32,
    /// Registry (`host:port`) to push to and fetch from. `None` resolves
    /// the two separately (see [`resolve_registry_route`]).
    pub registry: Option<String>,
    pub metadata_url: String,
    /// Per-node API address overrides, `node_id=host:port`.
    pub node_addresses: Vec<String>,
    /// Allow a target older than the running version.
    pub allow_downgrade: bool,
}

/// `relish upgrade start` — network or air-gapped.
pub async fn start(client: &BunClient, args: StartArgs) -> Result<(), RelishError> {
    // Obtain the binary bytes + signatures.
    let (bytes, target_version, embedded_signature, external_signature) =
        match (&args.version, &args.binary) {
            (None, None) => {
                return Err(RelishError::FormatFailed(
                    "pass a version (network) or --binary <path> (air-gapped)".to_string(),
                ));
            }
            (Some(_), Some(_)) => {
                return Err(RelishError::FormatFailed(
                    "pass either a version or --binary, not both".to_string(),
                ));
            }
            (None, Some(path)) => {
                let bytes = std::fs::read(path)?;
                let sig_path = args.sig.clone().unwrap_or_else(|| sidecar_sig_path(path));
                let envelope = SignatureEnvelope::load(&sig_path)?;
                let version = version_from_file_name(path).ok_or_else(|| {
                    RelishError::FormatFailed(format!(
                        "cannot derive a version from {:?}; name the file like bun-v0.2.0",
                        path.display()
                    ))
                })?;
                (bytes, version, envelope.embedded, envelope.external)
            }
            (Some(version), None) => {
                let version: BinaryVersion =
                    version.parse().map_err(|e: crate::upgrade::UpgradeError| {
                        RelishError::FormatFailed(e.to_string())
                    })?;
                let metadata = metadata::fetch(&args.metadata_url).await?;
                let platform = metadata::platform_key();
                let artifact = metadata.artifact_for(&version, &platform).ok_or_else(|| {
                    RelishError::FormatFailed(format!(
                        "no artefact for {version} on {platform} in the release metadata"
                    ))
                })?;
                eprintln!("downloading {} ...", artifact.url);
                let bytes = download(&artifact.url).await?;
                let actual = signing::sha256_hex(&bytes);
                if !actual.eq_ignore_ascii_case(&artifact.sha256) {
                    return Err(RelishError::FormatFailed(format!(
                        "downloaded binary hash mismatch: expected {}, got {actual}",
                        artifact.sha256
                    )));
                }
                (
                    bytes,
                    version,
                    artifact.embedded_signature.clone(),
                    artifact.external_signature.clone(),
                )
            }
        };

    let binary_sha256 = signing::sha256_hex(&bytes);

    // Cluster or single node? The cluster endpoint tells us.
    let cluster = client.upgrade_cluster().await;
    match cluster {
        Ok(_) => {
            let nodes = client.nodes().await?;
            let overrides = parse_overrides(&args.node_addresses)?;
            let node_list = build_node_list(&nodes, &overrides)?;
            // Cluster flow: push the blob to the connected node's registry,
            // then record the plan; the orchestrator walks the fleet and
            // every node fetches from that registry's cluster address.
            let route = match &args.registry {
                Some(explicit) => RegistryRoute::explicit(client.scheme(), explicit),
                None => {
                    let reported = client.capabilities_as_reported().await?;
                    resolve_registry_route(
                        client.scheme(),
                        client.base_url(),
                        client.declared_registry(),
                        &reported.node_id,
                        reported.service_endpoints.registry.as_deref(),
                        &nodes,
                    )?
                }
            };
            eprintln!(
                "pushing the binary via {}; nodes fetch it from {}",
                route.push_origin, route.fetch_address
            );
            push_blob(client, &route.push_origin, &bytes, &binary_sha256).await?;

            let request = serde_json::json!({
                "target_version": target_version,
                "binary_sha256": binary_sha256,
                "embedded_signature": embedded_signature,
                "external_signature": external_signature,
                "parallel": args.parallel,
                "registry_address": route.fetch_address,
                "nodes": node_list,
                "allow_downgrade": args.allow_downgrade,
            });
            let response = client.upgrade_start(&request).await?;
            if response["status"] == "already_running" {
                println!(
                    "{}",
                    response["detail"].as_str().unwrap_or("already running")
                );
                return Ok(());
            }
            println!(
                "cluster upgrade to {target_version} started ({})",
                response["upgrade_id"].as_str().unwrap_or("?")
            );
            println!("watch it with: relish upgrade status");
        }
        Err(_) => {
            // Single node: hand the directive straight to the local bun.
            let path = match &args.binary {
                Some(path) => std::fs::canonicalize(path)?,
                None => {
                    // Network flow on a single node: stage the download
                    // where the local bun can read it.
                    let staged =
                        std::env::temp_dir().join(format!("reliaburger-upgrade-{binary_sha256}"));
                    std::fs::write(&staged, &bytes)?;
                    staged
                }
            };
            let directive = UpgradeDirective {
                upgrade_id: format!("cli-{binary_sha256}"),
                target_version: target_version.clone(),
                binary_sha256,
                embedded_signature,
                external_signature,
                source: BinarySource::LocalFile { path },
                // Air-gapped `--binary` is not network; a downloaded artefact
                // staged as a local file IS, so it still requires the external
                // signature (M5).
                network_provenance: args.binary.is_none(),
                allow_downgrade: args.allow_downgrade,
            };
            let response = client.upgrade_apply(&directive).await?;
            if response["status"] == "already_running" {
                println!(
                    "{}",
                    response["detail"].as_str().unwrap_or("already running")
                );
                return Ok(());
            }
            println!("node upgrade to {target_version} started");
            println!("watch it with: relish upgrade status");
        }
    }
    Ok(())
}

/// `relish upgrade status` — cluster state if available, else node state.
pub async fn status(client: &BunClient) -> Result<(), RelishError> {
    match client.upgrade_cluster().await {
        Ok(cluster) => print!("{}", render_cluster_status(&cluster)),
        Err(_) => {
            let node = client.upgrade_status().await?;
            print!("{}", render_node_status(&node));
        }
    }
    Ok(())
}

/// `relish upgrade plan` — offline preview of the rolling order.
pub async fn plan(
    client: &BunClient,
    version: &str,
    cluster_size: Option<usize>,
    parallel: u32,
) -> Result<(), RelishError> {
    let (workers, council, leader) = match cluster_size {
        Some(size) => hypothetical_roles(size),
        None => match client.nodes().await {
            Ok(nodes) => {
                let leader = nodes.iter().filter(|n| n.is_leader).count();
                let council = nodes
                    .iter()
                    .filter(|n| n.is_council && !n.is_leader)
                    .count();
                (nodes.len() - council - leader, council, leader)
            }
            // No cluster reachable: plan for a single node.
            Err(_) => (0, 0, 1),
        },
    };
    print!(
        "{}",
        render_plan(version, workers, council, leader, parallel)
    );
    Ok(())
}

/// `relish upgrade rollback [version]`.
pub async fn rollback(
    client: &BunClient,
    version: Option<String>,
    node_addresses: Vec<String>,
) -> Result<(), RelishError> {
    match client.upgrade_cluster().await {
        Ok(_) => {
            let Some(version) = version else {
                return Err(RelishError::FormatFailed(
                    "cluster rollback needs an explicit version: relish upgrade rollback v0.1.0"
                        .to_string(),
                ));
            };
            let nodes = client.nodes().await?;
            let overrides = parse_overrides(&node_addresses)?;
            let request = serde_json::json!({
                "target_version": version,
                "nodes": build_node_list(&nodes, &overrides)?,
            });
            client.upgrade_cluster_rollback(&request).await?;
            println!("cluster rollback to {version} started");
        }
        Err(_) => {
            client.upgrade_node_rollback(version.as_deref()).await?;
            match version {
                Some(version) => println!("node rollback to {version} started"),
                None => println!("node rollback to the previous version started"),
            }
        }
    }
    Ok(())
}

/// `relish upgrade resume`.
pub async fn resume(client: &BunClient) -> Result<(), RelishError> {
    client.upgrade_resume().await?;
    println!("upgrade resumed");
    Ok(())
}

/// `relish upgrade abort` — end a paused upgrade that moved no node.
pub async fn abort(client: &BunClient) -> Result<(), RelishError> {
    let upgrade_id = client.upgrade_abort().await?;
    println!("upgrade {upgrade_id} aborted; every node stays on its current version");
    Ok(())
}

// ---------------------------------------------------------------------------
// Rendering (pure, snapshot-tested)
// ---------------------------------------------------------------------------

fn render_plan(
    version: &str,
    workers: usize,
    council: usize,
    leader: usize,
    parallel: u32,
) -> String {
    use std::fmt::Write as _;

    let parallel = parallel.max(1) as usize;
    let mut out = String::new();
    let total = workers + council + leader;
    writeln!(out, "upgrade plan to {version} ({total} node(s))").unwrap();

    let mut step = 1;
    let mut estimate = 0u64;
    if workers > 0 {
        let batches = workers.div_ceil(parallel);
        writeln!(
            out,
            "  {step}. workers: {workers} node(s) in {batches} batch(es) of up to {parallel}"
        )
        .unwrap();
        estimate += batches as u64 * SECONDS_PER_NODE;
        step += 1;
    }
    if council > 0 {
        writeln!(
            out,
            "  {step}. council members: {council} node(s), strictly one at a time"
        )
        .unwrap();
        estimate += council as u64 * SECONDS_PER_NODE;
        step += 1;
    }
    if leader > 0 {
        // The leader always upgrades last, in place (openraft 0.9 can't
        // gracefully hand off; a >=3-node council keeps quorum through the
        // sub-second exec bounce).
        writeln!(out, "  {step}. the leader, in place (last)").unwrap();
        estimate += SECONDS_PER_NODE;
    }
    writeln!(
        out,
        "estimated duration: ~{} min (assuming {SECONDS_PER_NODE}s per node)",
        estimate.div_ceil(60)
    )
    .unwrap();
    out.push_str("workloads keep running throughout (adoption across exec)\n");
    out
}

fn render_cluster_status(cluster: &serde_json::Value) -> String {
    use std::fmt::Write as _;

    let mut out = String::new();
    match cluster.get("active").filter(|a| !a.is_null()) {
        Some(active) => {
            writeln!(
                out,
                "upgrade {} to {} — phase: {}",
                active["upgrade_id"].as_str().unwrap_or("?"),
                active["target_version"].as_str().unwrap_or("?"),
                render_phase(&active["phase"]),
            )
            .unwrap();
            writeln!(out, "{:<20} {:<10} {:<12} FROM", "NODE", "ROLE", "PHASE").unwrap();
            for node in active["nodes"].as_array().into_iter().flatten() {
                writeln!(
                    out,
                    "{:<20} {:<10} {:<12} {}",
                    node["node_id"].as_str().unwrap_or("?"),
                    render_phase(&node["role"]).to_lowercase(),
                    render_phase(&node["phase"]),
                    node["from_version"].as_str().unwrap_or("-"),
                )
                .unwrap();
            }
        }
        None => {
            out.push_str("no upgrade in progress\n");
            if let Some(last) = cluster["history"].as_array().and_then(|h| h.last()) {
                writeln!(
                    out,
                    "last upgrade: {} to {} ({})",
                    last["upgrade_id"].as_str().unwrap_or("?"),
                    last["target_version"].as_str().unwrap_or("?"),
                    render_phase(&last["phase"]),
                )
                .unwrap();
            }
        }
    }
    out
}

fn render_node_status(node: &serde_json::Value) -> String {
    use std::fmt::Write as _;

    let mut out = String::new();
    writeln!(
        out,
        "running: {}",
        node["running_version"].as_str().unwrap_or("?")
    )
    .unwrap();
    match node.get("in_flight").filter(|m| !m.is_null()) {
        Some(marker) => writeln!(
            out,
            "in flight: {} -> {} (phase: {}, boot attempts: {})",
            marker["previous_version"].as_str().unwrap_or("?"),
            marker["target_version"].as_str().unwrap_or("?"),
            render_phase(&marker["phase"]),
            marker["boot_attempts"].as_u64().unwrap_or(0),
        )
        .unwrap(),
        None => out.push_str("no upgrade in flight\n"),
    }
    for entry in node["history"].as_array().into_iter().flatten() {
        writeln!(
            out,
            "  {} {} -> {}: {}",
            render_phase(&entry["outcome"]).to_lowercase(),
            entry["from_version"].as_str().unwrap_or("?"),
            entry["to_version"].as_str().unwrap_or("?"),
            entry["detail"].as_str().unwrap_or(""),
        )
        .unwrap();
    }
    out
}

/// Enum JSON comes as `"Completed"` or `{"Paused": {"reason": ...}}`;
/// render both shapes as a short label.
fn render_phase(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Object(map) => match map.iter().next() {
            Some((key, detail)) => {
                let reason = detail["reason"].as_str().unwrap_or("");
                if reason.is_empty() {
                    key.clone()
                } else {
                    format!("{key} ({reason})")
                }
            }
            None => "?".to_string(),
        },
        _ => "?".to_string(),
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn sidecar_sig_path(binary: &Path) -> std::path::PathBuf {
    let mut name = binary
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_default();
    name.push(".sig");
    binary.with_file_name(name)
}

/// `bun-v0.2.0` -> `v0.2.0`.
fn version_from_file_name(path: &Path) -> Option<BinaryVersion> {
    let name = path.file_name()?.to_str()?;
    let (_, suffix) = name.rsplit_once("-v")?;
    format!("v{suffix}").parse().ok()
}

/// Where a cluster upgrade's binary travels: relish uploads it through
/// `push_origin`, and every node downloads it from `fetch_address`.
///
/// These are two addresses because relish and the nodes stand in different
/// places. On a quickstart laptop cluster relish reaches node 1's registry
/// through a host forward (`https://127.0.0.1:15050`), which means nothing
/// inside the VMs; the nodes need node 1's own cluster address. Sending the
/// push address to the nodes (as relish once did) made every node fetch
/// from its own loopback.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RegistryRoute {
    /// Origin relish pushes to, `scheme://host:port`.
    pub push_origin: String,
    /// `host:port` the nodes fetch from.
    pub fetch_address: String,
}

impl RegistryRoute {
    /// `--registry host:port`: the operator names one address for both.
    fn explicit(scheme: &str, address: &str) -> Self {
        Self {
            push_origin: format!("{scheme}://{address}"),
            fetch_address: address.to_string(),
        }
    }
}

/// Work out the [`RegistryRoute`] for the node this connection talks to.
///
/// - `listener` is that node's registry listener as it reports it
///   (`https://0.0.0.0:5050`), `serving_node` its node id.
/// - The fetch address is the listener if it is bound to a specific
///   routable IP, otherwise the node's gossip IP with the listener's port.
/// - The push origin is the connection's declared registry forward when it
///   has one (quickstart), otherwise the API host with the listener's port.
pub fn resolve_registry_route(
    scheme: &str,
    api_base_url: &str,
    declared_forward: Option<&str>,
    serving_node: &str,
    listener: Option<&str>,
    nodes: &[crate::bun::agent::NodeStatus],
) -> Result<RegistryRoute, RelishError> {
    let unresolved = |why: String| {
        RelishError::FormatFailed(format!(
            "{why}; pass --registry host:port to name the upgrade registry"
        ))
    };
    let listener: std::net::SocketAddr = listener
        .map(|origin| origin.rsplit("://").next().unwrap_or(origin))
        .and_then(|address| address.trim_end_matches('/').parse().ok())
        .ok_or_else(|| {
            unresolved(format!(
                "node {serving_node} reports no usable registry listener ({listener:?})"
            ))
        })?;

    let fetch_address = if listener.ip().is_unspecified() || listener.ip().is_loopback() {
        let member = nodes
            .iter()
            .find(|node| node.node_id == serving_node)
            .ok_or_else(|| {
                unresolved(format!(
                    "node {serving_node} is not in the cluster membership"
                ))
            })?;
        let gossip: std::net::SocketAddr = member.address.parse().map_err(|_| {
            unresolved(format!(
                "node {serving_node} has an unparseable cluster address {:?}",
                member.address
            ))
        })?;
        std::net::SocketAddr::new(gossip.ip(), listener.port()).to_string()
    } else {
        listener.to_string()
    };

    let push_origin = match declared_forward {
        Some(forward) => forward.trim_end_matches('/').to_string(),
        None => {
            let host = reqwest::Url::parse(api_base_url)
                .ok()
                .and_then(|url| url.host_str().map(String::from))
                .ok_or_else(|| unresolved(format!("cannot read a host from {api_base_url:?}")))?;
            format!("{scheme}://{host}:{}", listener.port())
        }
    };

    Ok(RegistryRoute {
        push_origin,
        fetch_address,
    })
}

/// Download a URL into memory (shared with `relish setup`).
pub(crate) async fn download(url: &str) -> Result<Vec<u8>, RelishError> {
    let response = reqwest::get(url)
        .await
        .map_err(|e| RelishError::FormatFailed(format!("download failed: {e}")))?;
    if !response.status().is_success() {
        return Err(RelishError::FormatFailed(format!(
            "download failed: status {}",
            response.status()
        )));
    }
    Ok(response
        .bytes()
        .await
        .map_err(|e| RelishError::FormatFailed(format!("download failed: {e}")))?
        .to_vec())
}

/// Push the binary as a content-addressed blob (monolithic upload).
/// `client` supplies both the scheme and a CA-trusting, bearer-carrying HTTP
/// client (O3): the registry gains TLS with the agent API, and a registry
/// published on a routable address now wants a token for writes as well as
/// reads. A bare `reqwest::Client` on a hardcoded `http://` failed both tests.
async fn push_blob(
    client: &BunClient,
    origin: &str,
    bytes: &[u8],
    sha256: &str,
) -> Result<(), RelishError> {
    let url = format!(
        "{origin}/v2/{}/blobs/uploads/?digest=sha256:{sha256}",
        crate::upgrade::BINARY_BLOB_REPO
    );
    let response = client
        .http()?
        .post(&url)
        .body(bytes.to_vec())
        .send()
        .await
        .map_err(|e| RelishError::FormatFailed(format!("blob push failed: {e}")))?;
    if !response.status().is_success() {
        return Err(RelishError::FormatFailed(format!(
            "blob push to {origin} failed: status {}",
            response.status()
        )));
    }
    Ok(())
}

fn parse_overrides(overrides: &[String]) -> Result<Vec<(String, String)>, RelishError> {
    overrides
        .iter()
        .map(|entry| {
            entry
                .split_once('=')
                .map(|(id, address)| (id.to_string(), address.to_string()))
                .ok_or_else(|| {
                    RelishError::FormatFailed(format!(
                        "--node-address must be node_id=host:port, got {entry:?}"
                    ))
                })
        })
        .collect()
}

/// Build the start-request node list from gossip membership.
///
/// Use each node's resolved API address unless explicitly overridden.
/// Missing address evidence refuses the operation before an upgrade starts.
fn build_node_list(
    nodes: &[crate::bun::agent::NodeStatus],
    overrides: &[(String, String)],
) -> Result<Vec<serde_json::Value>, RelishError> {
    nodes
        .iter()
        .map(|node| {
            let address = overrides
                .iter()
                .find(|(id, _)| *id == node.node_id)
                .map(|(_, address)| address.clone())
                .or_else(|| {
                    node.api_address
                        .filter(|address| address.port() != 0 && !address.ip().is_unspecified())
                        .map(|address| address.to_string())
                })
                .ok_or_else(|| RelishError::ApiError {
                    status: 0,
                    body: format!(
                        "node {} has no advertised API endpoint; use --node-address to supply one",
                        node.node_id
                    ),
                })?;
            let role = if node.is_leader {
                "Leader"
            } else if node.is_council {
                "Council"
            } else {
                "Worker"
            };
            Ok(serde_json::json!({
                "node_id": node.node_id,
                "address": address,
                "role": role,
            }))
        })
        .collect()
}

fn hypothetical_roles(size: usize) -> (usize, usize, usize) {
    // Convention: up to 3 council members (one of which leads), the rest
    // are workers.
    let council_total = size.min(3);
    let leader = usize::from(council_total > 0);
    let council = council_total - leader;
    (size - council_total, council, leader)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_renders_all_three_groups() {
        insta::assert_snapshot!(render_plan("v0.2.0", 5, 2, 1, 2));
    }

    #[test]
    fn plan_renders_single_node() {
        insta::assert_snapshot!(render_plan("v0.2.0", 0, 0, 1, 1));
    }

    #[test]
    fn cluster_status_renders_active_upgrade() {
        let cluster = serde_json::json!({
            "active": {
                "upgrade_id": "up-1",
                "target_version": "v0.2.0",
                "phase": "UpgradingWorkers",
                "nodes": [
                    {"node_id": "n1", "role": "Worker", "phase": "Healthy", "from_version": "v0.1.0"},
                    {"node_id": "n2", "role": "Worker", "phase": "Directed", "from_version": "v0.1.0"},
                    {"node_id": "n3", "role": "Leader", "phase": "Pending", "from_version": null},
                ],
            },
            "history": [],
        });
        insta::assert_snapshot!(render_cluster_status(&cluster));
    }

    #[test]
    fn cluster_status_renders_paused_phase_with_reason() {
        let cluster = serde_json::json!({
            "active": {
                "upgrade_id": "up-1",
                "target_version": "v0.2.0",
                "phase": {"Paused": {"reason": "node n2 reverted to v0.1.0"}},
                "nodes": [],
            },
            "history": [],
        });
        insta::assert_snapshot!(render_cluster_status(&cluster));
    }

    #[test]
    fn node_status_renders_history() {
        let node = serde_json::json!({
            "running_version": "v0.1.0",
            "in_flight": null,
            "history": [
                {"outcome": "Reverted", "from_version": "v0.1.0", "to_version": "v0.2.0",
                 "detail": "reverted after 3 boot attempt(s) on v0.2.0"},
            ],
        });
        insta::assert_snapshot!(render_node_status(&node));
    }

    #[test]
    fn version_from_file_name_parses_versioned_binaries() {
        assert_eq!(
            version_from_file_name(Path::new("/tmp/bun-v0.2.0")),
            Some("v0.2.0".parse().unwrap())
        );
        assert_eq!(version_from_file_name(Path::new("/tmp/bun")), None);
    }

    #[test]
    fn build_node_list_uses_advertised_addresses_overrides_and_roles() {
        let nodes = vec![
            crate::bun::agent::NodeStatus {
                node_id: "n1".to_string(),
                address: "10.0.0.1:9443".to_string(),
                api_address: None,
                state: "alive".to_string(),
                incarnation: 1,
                is_council: false,
                is_leader: false,
                labels: Default::default(),
            },
            crate::bun::agent::NodeStatus {
                node_id: "n2".to_string(),
                address: "[2001:db8::2]:9443".to_string(),
                api_address: Some("[2001:db8::2]:19443".parse().unwrap()),
                state: "alive".to_string(),
                incarnation: 1,
                is_council: true,
                is_leader: true,
                labels: Default::default(),
            },
        ];
        let overrides = vec![("n1".to_string(), "10.0.0.1:8000".to_string())];

        assert!(build_node_list(&nodes, &[]).is_err());
        let list = build_node_list(&nodes, &overrides).unwrap();

        assert_eq!(list[0]["address"], "10.0.0.1:8000"); // override wins
        assert_eq!(list[0]["role"], "Worker");
        assert_eq!(list[1]["address"], "[2001:db8::2]:19443"); // advertised
        assert_eq!(list[1]["role"], "Leader");
    }

    fn member(node_id: &str, gossip: &str) -> crate::bun::agent::NodeStatus {
        crate::bun::agent::NodeStatus {
            node_id: node_id.to_string(),
            address: gossip.to_string(),
            api_address: None,
            state: "alive".to_string(),
            incarnation: 1,
            is_council: true,
            is_leader: false,
            labels: Default::default(),
        }
    }

    #[test]
    fn quickstart_pushes_through_the_host_forward_and_nodes_fetch_from_node_one() {
        // relish on the Mac: API and registry are 127.0.0.1 host forwards;
        // node 1 listens on the wildcard inside its VM.
        let nodes = [
            member("rb-1", "192.168.105.2:9443"),
            member("rb-2", "192.168.105.3:9443"),
        ];
        let route = resolve_registry_route(
            "https",
            "https://127.0.0.1:19117",
            Some("https://127.0.0.1:15050"),
            "rb-1",
            Some("https://0.0.0.0:5050"),
            &nodes,
        )
        .unwrap();
        assert_eq!(route.push_origin, "https://127.0.0.1:15050");
        assert_eq!(route.fetch_address, "192.168.105.2:5050");
    }

    #[test]
    fn plain_cluster_pushes_to_the_api_host_on_the_registry_port() {
        let nodes = [member("n1", "10.0.0.5:9443")];
        let route = resolve_registry_route(
            "https",
            "https://10.0.0.5:9117",
            None,
            "n1",
            Some("https://0.0.0.0:5050"),
            &nodes,
        )
        .unwrap();
        assert_eq!(route.push_origin, "https://10.0.0.5:5050");
        assert_eq!(route.fetch_address, "10.0.0.5:5050");
    }

    #[test]
    fn relish_on_a_node_via_loopback_never_sends_loopback_to_the_fleet() {
        // `relish` run on node 1 against https://127.0.0.1:9117 used to tell
        // every node to fetch from 127.0.0.1:5050, i.e. its own registry.
        let nodes = [member("n1", "10.0.0.5:9443")];
        let route = resolve_registry_route(
            "https",
            "https://127.0.0.1:9117",
            None,
            "n1",
            Some("https://0.0.0.0:5050"),
            &nodes,
        )
        .unwrap();
        assert_eq!(route.push_origin, "https://127.0.0.1:5050");
        assert_eq!(route.fetch_address, "10.0.0.5:5050");
    }

    #[test]
    fn a_listener_on_a_specific_address_is_fetched_from_directly() {
        let route = resolve_registry_route(
            "http",
            "http://[2001:db8::5]:9117",
            None,
            "n1",
            Some("http://[2001:db8::5]:15051"),
            &[],
        )
        .unwrap();
        assert_eq!(route.push_origin, "http://[2001:db8::5]:15051");
        assert_eq!(route.fetch_address, "[2001:db8::5]:15051");
    }

    #[test]
    fn unresolvable_routes_point_at_the_registry_flag() {
        let missing_listener =
            resolve_registry_route("https", "https://10.0.0.5:9117", None, "n1", None, &[])
                .unwrap_err();
        assert!(missing_listener.to_string().contains("--registry"));

        let unknown_node = resolve_registry_route(
            "https",
            "https://10.0.0.5:9117",
            None,
            "ghost",
            Some("https://0.0.0.0:5050"),
            &[member("n1", "10.0.0.5:9443")],
        )
        .unwrap_err();
        assert!(
            unknown_node.to_string().contains("--registry"),
            "{unknown_node}"
        );
    }

    #[test]
    fn explicit_registry_is_both_push_and_fetch() {
        let route = RegistryRoute::explicit("https", "10.0.0.9:5050");
        assert_eq!(route.push_origin, "https://10.0.0.9:5050");
        assert_eq!(route.fetch_address, "10.0.0.9:5050");
    }

    #[test]
    fn hypothetical_roles_split_sensibly() {
        assert_eq!(hypothetical_roles(1), (0, 0, 1));
        assert_eq!(hypothetical_roles(3), (0, 2, 1));
        assert_eq!(hypothetical_roles(10), (7, 2, 1));
    }
}
