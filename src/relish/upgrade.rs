//! `relish upgrade` — driving rolling binary upgrades from the CLI.
//!
//! Talks to the local bun's API (like every other relish command). On a
//! cluster it records the plan with the leader and the orchestrator does
//! the walking; on a single node it sends the node-level directive
//! directly.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::compatibility::Compatibility;
use crate::upgrade::metadata::{PlatformArtifact, Release, ReleaseMetadata};
use crate::upgrade::signing::{self, SignatureEnvelope};
use crate::upgrade::types::{BinarySource, PlatformBinary, UpgradeDirective};
use crate::upgrade::{BinaryVersion, metadata};

use super::RelishError;
use super::client::BunClient;

/// Default release metadata endpoint; `relish upgrade check --url` overrides it.
pub use crate::upgrade::metadata::DEFAULT_RELEASE_URL;

/// Assumed duration of one node's swap+verify, for `plan` estimates.
const SECONDS_PER_NODE: u64 = 45;

/// How long relish waits for one node to say which platform it runs on.
const PLATFORM_QUERY_TIMEOUT: Duration = Duration::from_secs(10);

/// What relish is talking to, as `GET /v1/upgrade/cluster` answers it.
#[derive(Debug)]
pub enum Topology {
    /// A node with a council; carries the cluster upgrade state.
    Cluster(serde_json::Value),
    /// A node that said, explicitly, that it has no council.
    SingleNode,
}

/// Read the answer of `GET /v1/upgrade/cluster`.
///
/// Only the explicit "no council on this node" 503 means a single node.
/// Every other failure (a timeout, a reset, a 5xx) is returned as the
/// error it is: reading it as "single node" used to upgrade or roll back
/// just the connected node, outside the rolling order, the quorum checks
/// and the run record.
pub fn topology(answer: Result<serde_json::Value, RelishError>) -> Result<Topology, RelishError> {
    match answer {
        Ok(state) => Ok(Topology::Cluster(state)),
        Err(RelishError::ApiError {
            status: 503,
            ref body,
        }) if says_no_council(body) => Ok(Topology::SingleNode),
        Err(error) => Err(error),
    }
}

fn says_no_council(body: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(body)
        .is_ok_and(|answer| answer["error"] == crate::upgrade::NO_COUNCIL)
}

/// One node and the platform (`linux-x86_64`) it reports.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodePlatform {
    /// How to name the node in a message: `node n1`, or `this node`.
    pub node: String,
    pub platform: String,
}

/// Ask every live node which platform it runs on.
///
/// The release artefact has to fit the node, not the machine relish runs
/// on: from a Mac, relish's own platform (`macos-aarch64`) has no bun
/// build at all, and from an x86_64 admin box it would push the wrong
/// architecture to arm64 nodes. Other nodes answer through the connected
/// node's relay, which reaches them even when relish can't.
async fn node_platforms(
    client: &BunClient,
    topology: &Topology,
) -> Result<Vec<NodePlatform>, RelishError> {
    let nodes = match topology {
        Topology::SingleNode => {
            let version = client.version().await?;
            return Ok(vec![reported_platform("this node", version.platform)?]);
        }
        Topology::Cluster(_) => client.nodes().await?,
    };
    let queries = nodes.iter().filter(|node| !node.is_down()).map(|node| {
        let name = format!("node {}", node.node_id);
        async move {
            let unanswered = |reason: String| {
                RelishError::FormatFailed(format!("could not ask {name} its platform: {reason}"))
            };
            let relay = client.via_node(&node.node_id)?;
            let version = tokio::time::timeout(PLATFORM_QUERY_TIMEOUT, relay.version())
                .await
                .map_err(|_| unanswered(format!("no answer in {PLATFORM_QUERY_TIMEOUT:?}")))?
                .map_err(|e| unanswered(e.to_string()))?;
            reported_platform(&name, version.platform)
        }
    });
    futures_util::future::join_all(queries)
        .await
        .into_iter()
        .collect()
}

fn reported_platform(node: &str, platform: Option<String>) -> Result<NodePlatform, RelishError> {
    platform
        .map(|platform| NodePlatform {
            node: node.to_string(),
            platform,
        })
        .ok_or_else(|| RelishError::FormatFailed(format!("{node} does not report its platform")))
}

/// The release artefact for each platform the nodes run on, one per
/// platform, in platform order.
pub fn artefacts_for_nodes<'a>(
    release: &'a Release,
    nodes: &[NodePlatform],
) -> Result<Vec<(&'a str, &'a PlatformArtifact)>, RelishError> {
    let mut chosen = BTreeMap::new();
    for node in nodes {
        let (platform, artifact) =
            release
                .platforms
                .get_key_value(&node.platform)
                .ok_or_else(|| {
                    RelishError::FormatFailed(format!(
                        "{} runs on {}, and the release metadata has no {} artefact for it",
                        node.node, node.platform, release.version
                    ))
                })?;
        chosen.insert(platform.as_str(), artifact);
    }
    Ok(chosen.into_iter().collect())
}

/// Why `release` can't roll onto nodes speaking `running`, if it can't.
///
/// Until 1.0 every release that changes a cluster format needs a fresh
/// cluster: nodes refuse peers and state of another generation, so a
/// rolling upgrade across the change can never finish.
pub fn format_change(running: Option<Compatibility>, release: &Release) -> Option<String> {
    let (running, target) = (running?, release.compatibility?);
    (running != target).then(|| {
        format!(
            "{} changes the cluster formats (protocol {} -> {}, state {} -> {}), so it needs \
             a fresh cluster, not `relish upgrade start`; see {}",
            release.version,
            running.protocol,
            target.protocol,
            running.state,
            target.state,
            crate::compatibility::POLICY_URL
        )
    })
}

/// `relish upgrade check`: compare what the nodes run with the latest release.
pub async fn check(client: &BunClient, url: &str) -> Result<(), RelishError> {
    let metadata = metadata::fetch(url).await?;
    let running = client.version().await?;
    let topology = topology(client.upgrade_cluster().await)?;
    let platforms = node_platforms(client, &topology).await?;
    let running_version: BinaryVersion = running
        .version
        .parse()
        .map_err(|e: crate::upgrade::UpgradeError| RelishError::FormatFailed(e.to_string()))?;
    print!(
        "{}",
        render_check(
            &running_version,
            running.compatibility,
            &metadata,
            &platforms
        )
    );
    Ok(())
}

fn render_check(
    running: &BinaryVersion,
    running_formats: Option<Compatibility>,
    metadata: &ReleaseMetadata,
    nodes: &[NodePlatform],
) -> String {
    let latest = &metadata.latest;
    let mut out = format!("running: {running}\nlatest:  {latest}\n");
    if running >= latest {
        out.push_str("up to date\n");
        return out;
    }
    let Some(release) = metadata.release(latest) else {
        out.push_str(&format!(
            "the release metadata names {latest} but lists no artefacts for it\n"
        ));
        return out;
    };
    if let Some(change) = format_change(running_formats, release) {
        out.push_str(&change);
        out.push('\n');
        return out;
    }
    match artefacts_for_nodes(release, nodes) {
        Ok(_) => out.push_str(&format!(
            "upgrade available: relish upgrade start {latest} --external-key <your key>\n"
        )),
        Err(e) => out.push_str(&format!("a newer version exists, but {e}\n")),
    }
    out
}

/// Arguments for `relish upgrade start`.
pub struct StartArgs {
    /// Target version (network flow). Mutually exclusive with `binary`.
    pub version: Option<String>,
    /// Local binary path (air-gapped flow). Expects `{path}.sig` beside it
    /// unless `sig` is given.
    pub binary: Option<PathBuf>,
    /// A countersigned signature envelope: for `binary`, or for the version
    /// form when every node shares one platform.
    pub sig: Option<PathBuf>,
    /// The operator's external private key (PKCS#8): the version form
    /// countersigns each downloaded artefact with it.
    pub external_key: Option<PathBuf>,
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

/// One platform's target binary with its bytes, ready to push or stage.
struct Build {
    binary: PlatformBinary,
    bytes: Vec<u8>,
}

/// Where `relish upgrade start` gets the binary from.
enum StartForm<'a> {
    /// A released version, downloaded through the release metadata.
    Release(&'a str),
    /// A local file (`--binary`).
    File(&'a Path),
}

/// `relish upgrade start`: network (a version) or air-gapped (`--binary`).
pub async fn start(client: &BunClient, args: StartArgs) -> Result<(), RelishError> {
    // Matching on a tuple of both options checks every combination at once.
    let form = match (&args.version, &args.binary) {
        (Some(version), None) => StartForm::Release(version.as_str()),
        (None, Some(path)) => StartForm::File(path.as_path()),
        _ => {
            return Err(RelishError::FormatFailed(
                "pass either a version (network) or --binary <path> (air-gapped)".to_string(),
            ));
        }
    };
    if args.binary.is_some() && args.external_key.is_some() {
        return Err(RelishError::FormatFailed(
            "--external-key countersigns downloaded releases; for --binary, countersign \
             with `relish dev countersign-binary` and pass the envelope with --sig"
                .to_string(),
        ));
    }
    let topology = topology(client.upgrade_cluster().await)?;
    match form {
        StartForm::Release(version) => start_from_release(client, &args, &topology, version).await,
        StartForm::File(path) => start_from_file(client, &args, &topology, path).await,
    }
}

/// The network form: download the target for every platform the nodes run
/// on, countersign it if asked, and roll it out.
async fn start_from_release(
    client: &BunClient,
    args: &StartArgs,
    topology: &Topology,
    version: &str,
) -> Result<(), RelishError> {
    let version: BinaryVersion = version
        .parse()
        .map_err(|e: crate::upgrade::UpgradeError| RelishError::FormatFailed(e.to_string()))?;
    let metadata = metadata::fetch(&args.metadata_url).await?;
    let release = metadata.release(&version).ok_or_else(|| {
        RelishError::FormatFailed(format!("the release metadata has no {version}"))
    })?;
    if let Some(change) = format_change(client.version().await?.compatibility, release) {
        return Err(RelishError::FormatFailed(change));
    }
    let nodes = node_platforms(client, topology).await?;
    let artefacts = artefacts_for_nodes(release, &nodes)?;
    if args.sig.is_some() && artefacts.len() > 1 {
        return Err(RelishError::FormatFailed(format!(
            "the nodes run on {} platforms and --sig signs one binary; \
             pass --external-key to countersign each build",
            artefacts.len()
        )));
    }
    let envelope = args
        .sig
        .as_deref()
        .map(SignatureEnvelope::load)
        .transpose()?;
    let external_key = args
        .external_key
        .as_deref()
        .map(std::fs::read)
        .transpose()?;

    let mut builds = Vec::new();
    for (platform, artifact) in artefacts {
        eprintln!("downloading {} ...", artifact.url);
        let bytes = download(&artifact.url).await?;
        let actual = signing::sha256_hex(&bytes);
        if !actual.eq_ignore_ascii_case(&artifact.sha256) {
            return Err(RelishError::FormatFailed(format!(
                "downloaded binary hash mismatch: expected {}, got {actual}",
                artifact.sha256
            )));
        }
        let external_signature =
            external_signature_for(artifact, &bytes, envelope.as_ref(), external_key.as_deref())?;
        builds.push(Build {
            binary: PlatformBinary {
                platform: platform.to_string(),
                sha256: actual,
                embedded_signature: artifact.embedded_signature.clone(),
                external_signature: Some(external_signature),
            },
            bytes,
        });
    }

    match topology {
        Topology::Cluster(_) => start_cluster(client, args, &version, &builds).await,
        Topology::SingleNode => {
            let [build] = builds.as_slice() else {
                return Err(RelishError::FormatFailed(
                    "a single node runs on one platform".to_string(),
                ));
            };
            // Stage the download where the local bun can read it. The
            // directory is private and removed once the node has answered
            // (it reads the file before it does).
            let staged = StagedBinary::new(&build.bytes, &version.file_name("bun"))?;
            let directive = UpgradeDirective {
                upgrade_id: format!("cli-{}", build.binary.sha256),
                target_version: version.clone(),
                binary_sha256: build.binary.sha256.clone(),
                embedded_signature: build.binary.embedded_signature.clone(),
                external_signature: build.binary.external_signature.clone(),
                source: BinarySource::LocalFile {
                    path: staged.path().to_path_buf(),
                },
                // Downloaded, so still a network upgrade that needs the
                // external signature, though it arrives as a file (M5).
                network_provenance: true,
                allow_downgrade: args.allow_downgrade,
            };
            apply_on_this_node(client, &directive).await
        }
    }
}

/// The operator's signature for one downloaded release artefact.
///
/// With `--external-key`, relish countersigns the bytes itself. With a
/// countersigned `--sig`, it takes the external signature from that
/// envelope, which must belong to this artefact. Otherwise only release
/// metadata from a private host that already carries one will do: public
/// metadata never does, because only the operator holds the external key.
pub fn external_signature_for(
    artifact: &PlatformArtifact,
    bytes: &[u8],
    envelope: Option<&SignatureEnvelope>,
    external_key: Option<&[u8]>,
) -> Result<String, RelishError> {
    if let Some(pkcs8) = external_key {
        let release = SignatureEnvelope {
            schema: 1,
            sha256: artifact.sha256.clone(),
            embedded: artifact.embedded_signature.clone(),
            external: None,
        };
        let countersigned = signing::countersign(&release, pkcs8, bytes)?;
        return countersigned.external.ok_or_else(|| {
            RelishError::FormatFailed("countersigning produced no signature".to_string())
        });
    }
    if let Some(envelope) = envelope {
        if !envelope.sha256.eq_ignore_ascii_case(&artifact.sha256) {
            return Err(RelishError::FormatFailed(format!(
                "--sig is for the binary with sha256 {}, not this release's {}",
                envelope.sha256, artifact.sha256
            )));
        }
        return envelope.external.clone().ok_or_else(|| {
            RelishError::FormatFailed(
                "--sig carries no external signature; countersign it first with \
                 `relish dev countersign-binary`"
                    .to_string(),
            )
        });
    }
    artifact.external_signature.clone().ok_or_else(|| {
        RelishError::FormatFailed(
            "network upgrades need your countersignature: pass --external-key <your key>, \
             or --sig with an envelope countersigned by `relish dev countersign-binary`"
                .to_string(),
        )
    })
}

/// The air-gapped form: roll a local binary with its signature envelope.
async fn start_from_file(
    client: &BunClient,
    args: &StartArgs,
    topology: &Topology,
    path: &Path,
) -> Result<(), RelishError> {
    let bytes = std::fs::read(path)?;
    let sig_path = args.sig.clone().unwrap_or_else(|| sidecar_sig_path(path));
    let envelope = SignatureEnvelope::load(&sig_path)?;
    let version = version_from_file_name(path).ok_or_else(|| {
        RelishError::FormatFailed(format!(
            "cannot derive a version from {:?}; name the file like bun-v0.2.0",
            path.display()
        ))
    })?;
    let binary_sha256 = signing::sha256_hex(&bytes);

    match topology {
        Topology::Cluster(_) => {
            // One local file is one build: it fits a cluster of one platform.
            let nodes = node_platforms(client, topology).await?;
            let platforms: std::collections::BTreeSet<&str> =
                nodes.iter().map(|node| node.platform.as_str()).collect();
            let [platform] = platforms.into_iter().collect::<Vec<_>>()[..] else {
                return Err(RelishError::FormatFailed(
                    "the nodes run on several platforms and --binary names one build; \
                     use the version form with --external-key"
                        .to_string(),
                ));
            };
            let build = Build {
                binary: PlatformBinary {
                    platform: platform.to_string(),
                    sha256: binary_sha256,
                    embedded_signature: envelope.embedded,
                    external_signature: envelope.external,
                },
                bytes,
            };
            start_cluster(client, args, &version, std::slice::from_ref(&build)).await
        }
        Topology::SingleNode => {
            let directive = UpgradeDirective {
                upgrade_id: format!("cli-{binary_sha256}"),
                target_version: version,
                binary_sha256,
                embedded_signature: envelope.embedded,
                external_signature: envelope.external,
                source: BinarySource::LocalFile {
                    path: std::fs::canonicalize(path)?,
                },
                // Air-gapped `--binary` is not network: the release
                // signature alone suffices.
                network_provenance: false,
                allow_downgrade: args.allow_downgrade,
            };
            apply_on_this_node(client, &directive).await
        }
    }
}

/// Push every build to the registry and record the cluster run with the
/// leader; the orchestrator walks the fleet and every node fetches its own
/// platform's build from that registry's cluster address.
async fn start_cluster(
    client: &BunClient,
    args: &StartArgs,
    target_version: &BinaryVersion,
    builds: &[Build],
) -> Result<(), RelishError> {
    let nodes = client.nodes().await?;
    let overrides = parse_overrides(&args.node_addresses)?;
    let node_list = build_node_list(&nodes, &overrides)?;
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
    for build in builds {
        eprintln!(
            "pushing the {} binary via {}; nodes fetch it from {}",
            build.binary.platform, route.push_origin, route.fetch_address
        );
        push_blob(
            client,
            &route.push_origin,
            &build.bytes,
            &build.binary.sha256,
        )
        .await?;
    }

    let binaries: Vec<&PlatformBinary> = builds.iter().map(|build| &build.binary).collect();
    let request = serde_json::json!({
        "target_version": target_version,
        "binaries": binaries,
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
    Ok(())
}

/// Hand a node-level directive straight to the connected (single) node.
async fn apply_on_this_node(
    client: &BunClient,
    directive: &UpgradeDirective,
) -> Result<(), RelishError> {
    let response = client.upgrade_apply(directive).await?;
    if response["status"] == "already_running" {
        println!(
            "{}",
            response["detail"].as_str().unwrap_or("already running")
        );
        return Ok(());
    }
    println!("node upgrade to {} started", directive.target_version);
    println!("watch it with: relish upgrade status");
    Ok(())
}

/// A downloaded binary staged for the local bun to read, in a private
/// directory with an unpredictable name. Dropping it removes both.
///
/// It replaces a fixed `$TMPDIR/reliaburger-upgrade-<sha>` path that was
/// written through `std::fs::write`, which follows a symlink planted there,
/// and was never removed.
pub struct StagedBinary {
    // Held for its `Drop`, which deletes the directory and the file.
    _directory: tempfile::TempDir,
    path: PathBuf,
}

impl StagedBinary {
    /// Write `bytes` to a new file called `name` in a fresh private directory.
    pub fn new(bytes: &[u8], name: &str) -> Result<Self, RelishError> {
        use std::io::Write as _;

        let mut builder = tempfile::Builder::new();
        builder.prefix("reliaburger-upgrade-");
        // Owner only, from the moment it exists.
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            builder.permissions(std::fs::Permissions::from_mode(0o700));
        }
        let directory = builder.tempdir()?;
        let path = directory.path().join(name);
        let mut options = std::fs::OpenOptions::new();
        // `create_new` refuses an existing file or symlink at the path.
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            options.mode(0o600);
        }
        options.open(&path)?.write_all(bytes)?;
        Ok(Self {
            _directory: directory,
            path,
        })
    }

    /// Where the binary is.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

/// `relish upgrade status`: cluster state on a cluster, else node state.
pub async fn status(client: &BunClient) -> Result<(), RelishError> {
    match topology(client.upgrade_cluster().await)? {
        Topology::Cluster(cluster) => print!("{}", render_cluster_status(&cluster)),
        Topology::SingleNode => {
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
            Ok(mut nodes) => {
                nodes.retain(|node| !node.is_down());
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
    match topology(client.upgrade_cluster().await)? {
        Topology::Cluster(_) => {
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
        Topology::SingleNode => {
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
        // The listing shows dead members too; only live ones take a binary.
        .filter(|node| !node.is_down())
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

    #[test]
    fn build_node_list_leaves_out_dead_members() {
        let mut dead = member("n2", "10.0.0.2:9443");
        dead.state = "dead".to_string();
        let mut live = member("n1", "10.0.0.1:9443");
        live.api_address = Some("10.0.0.1:9117".parse().unwrap());
        // The dead member has no address either; listing it would refuse
        // the whole plan.
        let list = build_node_list(&[live, dead], &[]).unwrap();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0]["node_id"], "n1");
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

    fn api_error(status: u16, error: &str) -> RelishError {
        RelishError::ApiError {
            status,
            body: serde_json::json!({ "error": error }).to_string(),
        }
    }

    #[test]
    fn a_cluster_timeout_does_not_fall_back_to_single_node() {
        for failure in [
            RelishError::RequestTimeout,
            RelishError::AgentUnreachable,
            api_error(500, "internal error"),
            api_error(503, "agent unavailable"),
        ] {
            let shown = failure.to_string();
            let answer = topology(Err(failure));
            assert!(
                answer.is_err(),
                "{shown} must not select the single-node path"
            );
        }
    }

    #[test]
    fn only_no_council_answer_selects_the_single_node_path() {
        let single = topology(Err(api_error(503, crate::upgrade::NO_COUNCIL))).unwrap();
        assert!(matches!(single, Topology::SingleNode));
        // The same words with another status are not that answer.
        assert!(topology(Err(api_error(500, crate::upgrade::NO_COUNCIL))).is_err());
        let cluster = topology(Ok(serde_json::json!({"active": null}))).unwrap();
        assert!(matches!(cluster, Topology::Cluster(_)));
    }

    fn release_fixture(compatibility: Option<Compatibility>) -> ReleaseMetadata {
        let artefact = |name: &str| PlatformArtifact {
            url: format!("https://releases.example/{name}"),
            sha256: format!("{name}-sha"),
            embedded_signature: "c2ln".to_string(),
            external_signature: None,
        };
        ReleaseMetadata {
            schema: 1,
            latest: "v0.2.0".parse().unwrap(),
            releases: vec![Release {
                version: "v0.2.0".parse().unwrap(),
                compatibility,
                platforms: BTreeMap::from([
                    ("linux-aarch64".to_string(), artefact("bun-linux-aarch64")),
                    ("linux-x86_64".to_string(), artefact("bun-linux-x86_64")),
                ]),
            }],
        }
    }

    fn on(node: &str, platform: &str) -> NodePlatform {
        NodePlatform {
            node: format!("node {node}"),
            platform: platform.to_string(),
        }
    }

    #[test]
    fn artefact_is_chosen_per_node_platform_not_cli_host() {
        let metadata = release_fixture(None);
        let release = &metadata.releases[0];

        // Whatever relish runs on (a Mac has no bun build at all), an arm64
        // cluster gets the arm64 build and nothing else.
        let arm = artefacts_for_nodes(release, &[on("a", "linux-aarch64")]).unwrap();
        let platforms: Vec<&str> = arm.iter().map(|(platform, _)| *platform).collect();
        assert_eq!(platforms, ["linux-aarch64"]);
        assert_eq!(arm[0].1.sha256, "bun-linux-aarch64-sha");

        // A mixed cluster gets one build per platform present.
        let mixed = artefacts_for_nodes(
            release,
            &[
                on("a", "linux-x86_64"),
                on("b", "linux-aarch64"),
                on("c", "linux-x86_64"),
            ],
        )
        .unwrap();
        let platforms: Vec<&str> = mixed.iter().map(|(platform, _)| *platform).collect();
        assert_eq!(platforms, ["linux-aarch64", "linux-x86_64"]);

        // A node on a platform the release lacks is named.
        let missing = artefacts_for_nodes(release, &[on("m", "macos-aarch64")]).unwrap_err();
        assert!(missing.to_string().contains("node m"), "{missing}");
    }

    #[test]
    fn check_reports_a_format_change_as_needing_a_fresh_cluster() {
        let running: BinaryVersion = "v0.1.6".parse().unwrap();
        let old = Compatibility {
            protocol: 46,
            state: 63,
        };
        let new = Compatibility {
            protocol: 51,
            state: 68,
        };
        let nodes = [on("a", "linux-x86_64")];

        let changed = render_check(&running, Some(old), &release_fixture(Some(new)), &nodes);
        assert!(changed.contains("needs a fresh cluster"), "{changed}");
        assert!(changed.contains("protocol 46 -> 51"), "{changed}");
        assert!(!changed.contains("upgrade available"), "{changed}");

        let same = render_check(&running, Some(new), &release_fixture(Some(new)), &nodes);
        assert!(
            same.contains("upgrade available: relish upgrade start v0.2.0"),
            "{same}"
        );
    }

    #[test]
    fn version_form_carries_the_countersigned_external_signature() {
        let (release_pkcs8, release_public) = signing::generate_keypair().unwrap();
        let (operator_pkcs8, operator_public) = signing::generate_keypair().unwrap();
        let bytes = b"bun v0.2.0 for linux-x86_64".to_vec();
        let artifact = PlatformArtifact {
            url: "https://releases.example/bun".to_string(),
            sha256: signing::sha256_hex(&bytes),
            embedded_signature: signing::sign(&release_pkcs8, &bytes).unwrap(),
            // Published metadata: no operator signature.
            external_signature: None,
        };
        let verifies = |external: String| {
            let envelope = SignatureEnvelope {
                schema: 1,
                sha256: artifact.sha256.clone(),
                embedded: artifact.embedded_signature.clone(),
                external: Some(external),
            };
            signing::verify_binary(
                &bytes,
                &envelope,
                &[release_public],
                Some(&operator_public),
                true,
            )
            .is_ok()
        };

        // --external-key: relish countersigns the download itself.
        let signed = external_signature_for(&artifact, &bytes, None, Some(&operator_pkcs8));
        assert!(verifies(signed.unwrap()));

        // --sig: an envelope countersigned beforehand.
        let release_envelope = SignatureEnvelope {
            schema: 1,
            sha256: artifact.sha256.clone(),
            embedded: artifact.embedded_signature.clone(),
            external: None,
        };
        let countersigned =
            signing::countersign(&release_envelope, &operator_pkcs8, &bytes).unwrap();
        let from_sig = external_signature_for(&artifact, &bytes, Some(&countersigned), None);
        assert!(verifies(from_sig.unwrap()));

        // Neither: refused here with the remedy, not by every node later.
        let missing = external_signature_for(&artifact, &bytes, None, None).unwrap_err();
        assert!(missing.to_string().contains("--external-key"), "{missing}");

        // An envelope for other bytes, or one never countersigned, is refused.
        let mut other = countersigned.clone();
        other.sha256 = "0".repeat(64);
        assert!(external_signature_for(&artifact, &bytes, Some(&other), None).is_err());
        assert!(external_signature_for(&artifact, &bytes, Some(&release_envelope), None).is_err());
    }

    #[test]
    fn network_download_is_staged_privately_and_removed() {
        let staged = StagedBinary::new(b"bun bytes", "bun-v0.2.0").unwrap();
        let path = staged.path().to_path_buf();
        assert_eq!(std::fs::read(&path).unwrap(), b"bun bytes");
        let directory = path.parent().unwrap().to_path_buf();
        // Not the old predictable `$TMPDIR/reliaburger-upgrade-<sha>` file:
        // a fresh directory with a random name.
        assert_ne!(directory, std::env::temp_dir());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&directory).unwrap().permissions().mode();
            assert_eq!(mode & 0o077, 0, "only the owner may enter: {mode:o}");
        }
        let again = StagedBinary::new(b"bun bytes", "bun-v0.2.0").unwrap();
        assert_ne!(again.path(), path, "each staging gets its own directory");

        drop(staged);
        assert!(!path.exists(), "the staged binary is removed");
        assert!(!directory.exists(), "and so is its directory");
    }

    #[test]
    fn hypothetical_roles_split_sensibly() {
        assert_eq!(hypothetical_roles(1), (0, 0, 1));
        assert_eq!(hypothetical_roles(3), (0, 2, 1));
        assert_eq!(hypothetical_roles(10), (7, 2, 1));
    }
}
