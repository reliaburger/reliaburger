//! `relish machines`: find unclaimed appliances on the LAN and claim them
//! (docs/plans/2026-10-01-plan-appliance-product.md, W5).
//!
//! An unclaimed machine announces itself over mDNS and serves its claim API
//! with a self-signed key (`crate::appliance::claim`). relish never trusts
//! that key on its own: every call is pinned to a fingerprint, and the seed,
//! which may carry the cluster's master key, only goes over a connection
//! whose certificate hashes to the fingerprint the operator compared with
//! the machine's console (or chose to trust with `--trust-lan`).

use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rustls::DigitallySignedStruct;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};

use crate::appliance::claim::{
    CLAIM_PORT, MachineInfo, SERVICE_TYPE, certificate_fingerprint, short_fingerprint,
};
use crate::relish::RelishError;
use crate::relish::bare_metal;

/// An unclaimed machine found on the LAN.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Discovered {
    pub addresses: Vec<IpAddr>,
    pub macs: Vec<String>,
    pub arch: String,
    /// From the announcement: unauthenticated, only for display and lookup.
    pub short_fingerprint: String,
}

/// Listen for unclaimed machines' announcements for `wait`.
pub fn browse(wait: Duration) -> Result<Vec<Discovered>, RelishError> {
    let daemon = mdns_sd::ServiceDaemon::new().map_err(|e| failed(&format!("mDNS: {e}")))?;
    let receiver = daemon
        .browse(SERVICE_TYPE)
        .map_err(|e| failed(&format!("mDNS: {e}")))?;
    let deadline = std::time::Instant::now() + wait;
    let mut found: Vec<Discovered> = Vec::new();
    while let Some(left) = deadline.checked_duration_since(std::time::Instant::now()) {
        let Ok(event) = receiver.recv_timeout(left) else {
            break;
        };
        if let mdns_sd::ServiceEvent::ServiceResolved(service) = event {
            let property = |key: &str| {
                service
                    .get_property_val_str(key)
                    .unwrap_or_default()
                    .to_string()
            };
            let machine = Discovered {
                addresses: service
                    .get_addresses()
                    .iter()
                    .map(|a| a.to_ip_addr())
                    .collect(),
                macs: property("mac")
                    .split(',')
                    .filter(|m| !m.is_empty())
                    .map(str::to_string)
                    .collect(),
                arch: property("arch"),
                short_fingerprint: property("fp"),
            };
            // A machine answers more than once, and a later answer may
            // carry an address an earlier one didn't (its IPv4 one, once
            // DHCP has given it).
            match found.iter_mut().find(|m| m.macs == machine.macs) {
                Some(known) => {
                    for address in machine.addresses {
                        if !known.addresses.contains(&address) {
                            known.addresses.push(address);
                        }
                    }
                }
                None => found.push(machine),
            }
        }
    }
    let _ = daemon.shutdown();
    found.sort_by(|a, b| a.addresses.cmp(&b.addresses));
    Ok(found)
}

/// The address to reach `machine` at: its IPv4 one, else a routable IPv6
/// one. Never an IPv6 link-local address, which can't be reached without
/// naming the interface, and mDNS doesn't say which.
pub fn reachable_address(machine: &Discovered) -> Option<IpAddr> {
    let addresses = &machine.addresses;
    addresses.iter().find(|a| a.is_ipv4()).copied().or_else(|| {
        addresses
            .iter()
            .find(|a| matches!(a, IpAddr::V6(v6) if !v6.is_unicast_link_local()))
            .copied()
    })
}

/// Find the machine `target` (a MAC or an IP) among `machines`.
pub fn find<'a>(machines: &'a [Discovered], target: &str) -> Option<&'a Discovered> {
    let wanted = target.to_ascii_lowercase().replace('-', ":");
    machines
        .iter()
        .find(|m| m.macs.contains(&wanted) || m.addresses.iter().any(|a| a.to_string() == target))
}

/// What `relish machines` answers: the machines that announced themselves,
/// as a list for scripts and the table for people.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(transparent)]
pub struct MachineList(pub Vec<Discovered>);

impl std::fmt::Display for MachineList {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&render(&self.0))
    }
}

/// The `relish machines` table.
pub fn render(machines: &[Discovered]) -> String {
    if machines.is_empty() {
        return "no unclaimed machines answered\n".to_string();
    }
    let mut out = format!(
        "{:<17} {:<17} {:<8} {}\n",
        "ADDRESS", "MAC", "ARCH", "CLAIM KEY"
    );
    for machine in machines {
        // Show the link-local address rather than nothing: it's still a
        // sign of life, though relish can't claim by it.
        let address = reachable_address(machine)
            .or(machine.addresses.first().copied())
            .map(|a| a.to_string())
            .unwrap_or_else(|| "-".into());
        out.push_str(&format!(
            "{address:<17} {:<17} {:<8} {}\n",
            machine.macs.first().map(String::as_str).unwrap_or("-"),
            machine.arch,
            machine.short_fingerprint
        ));
    }
    out
}

/// The fingerprint a handshake saw. A `std` mutex, because rustls calls
/// the verifier synchronously and holds it only to store one string.
type SeenFingerprint = Arc<Mutex<Option<String>>>;

/// A TLS verifier for one claim key: it accepts exactly the certificate
/// whose SHA-256 is `expected`, or with `expected` unset (a first look),
/// any certificate, remembering its fingerprint.
#[derive(Debug)]
struct ClaimKeyVerifier {
    expected: Option<String>,
    seen: SeenFingerprint,
    provider: Arc<rustls::crypto::CryptoProvider>,
}

impl ServerCertVerifier for ClaimKeyVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        let fingerprint = certificate_fingerprint(end_entity);
        if let Ok(mut seen) = self.seen.lock() {
            *seen = Some(fingerprint.clone());
        }
        match &self.expected {
            Some(expected) if *expected != fingerprint => Err(rustls::Error::General(format!(
                "the machine's claim key is {fingerprint}, not {expected}"
            ))),
            _ => Ok(ServerCertVerified::assertion()),
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

fn claim_client(
    expected: Option<String>,
) -> Result<(reqwest::Client, SeenFingerprint), RelishError> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let seen = Arc::new(Mutex::new(None));
    let verifier = Arc::new(ClaimKeyVerifier {
        expected,
        seen: seen.clone(),
        provider: provider.clone(),
    });
    let tls = rustls::ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|e| failed(&e.to_string()))?
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_no_client_auth();
    let client = crate::http_client::builder()
        .use_preconfigured_tls(tls)
        .timeout(Duration::from_secs(15))
        .build()
        .map_err(|e| failed(&e.to_string()))?;
    Ok((client, seen))
}

fn claim_url(address: SocketAddr) -> String {
    format!("https://{address}/v1/claim")
}

/// Ask a machine what it is, learning its claim key's full fingerprint. If
/// the announcement gave a short fingerprint, the key must match it.
pub async fn inspect(
    address: SocketAddr,
    announced: Option<&str>,
) -> Result<(MachineInfo, String), RelishError> {
    let (client, seen) = claim_client(None)?;
    let response = client
        .get(claim_url(address))
        .send()
        .await
        .map_err(|e| failed(&format!("{address}: {e}")))?;
    let info: MachineInfo = response
        .json()
        .await
        .map_err(|e| failed(&format!("{address}: {e}")))?;
    let fingerprint = seen
        .lock()
        .ok()
        .and_then(|seen| seen.clone())
        .ok_or_else(|| failed(&format!("{address} presented no certificate")))?;
    if info.fingerprint != fingerprint {
        return Err(failed(&format!(
            "{address} claims key {} but presented {fingerprint}",
            info.fingerprint
        )));
    }
    if let Some(announced) = announced
        && short_fingerprint(&fingerprint) != announced
    {
        return Err(failed(&format!(
            "{address}'s claim key {} doesn't match its announcement {announced}",
            short_fingerprint(&fingerprint)
        )));
    }
    Ok((info, fingerprint))
}

/// Post `seed` to the machine at `address`, over TLS pinned to `fingerprint`.
pub async fn post_seed(
    address: SocketAddr,
    fingerprint: &str,
    seed: Vec<u8>,
) -> Result<(), RelishError> {
    let (client, _) = claim_client(Some(fingerprint.to_string()))?;
    let response = client
        .post(claim_url(address))
        .body(seed)
        .send()
        .await
        .map_err(|e| failed(&format!("{address}: {e}")))?;
    if !response.status().is_success() {
        let status = response.status();
        let body = response.text().await.unwrap_or_default();
        return Err(failed(&format!(
            "{address} refused the seed ({status}): {body}"
        )));
    }
    Ok(())
}

/// The cluster `relish machines claim --create` makes from the machines
/// it claims.
#[derive(Debug, Clone)]
pub struct NewCluster {
    pub name: String,
    pub operators: Vec<String>,
    pub network: Option<String>,
    pub faults: bool,
    pub external_signing_key: Option<String>,
    /// How many voters the council grows to.
    pub council_size: crate::council::CouncilSize,
    /// `--yes`: don't wait for the operator to confirm the master-key
    /// backup.
    pub yes: bool,
}

impl NewCluster {
    /// What `bare_metal::create` is asked for when this cluster is made
    /// from the claimed `machines`, node 1 first.
    pub fn create_options(
        &self,
        directory: &std::path::Path,
        machines: Vec<(String, IpAddr)>,
        token_ttl: Duration,
        ssh_key: Option<Vec<u8>>,
        root_backup_keys: PathBuf,
    ) -> bare_metal::CreateOptions {
        bare_metal::CreateOptions {
            directory: directory.to_path_buf(),
            cluster: self.name.clone(),
            machines,
            operators: self.operators.clone(),
            network: self.network.clone(),
            faults: self.faults,
            ssh_key,
            token_ttl,
            external_signing_key: self.external_signing_key.clone(),
            council_size: self.council_size,
            root_backup_keys,
        }
    }
}

/// What `relish machines claim` was asked to do.
#[derive(Debug, Clone)]
pub struct ClaimOptions {
    /// The cluster directory (`relish cluster create --bare-metal`'s).
    pub directory: PathBuf,
    /// Each machine as a MAC (found over mDNS) or an address.
    pub targets: Vec<String>,
    /// Create this cluster from the machines instead of joining them to the
    /// one in `directory`.
    pub create: Option<NewCluster>,
    pub token_ttl: Duration,
    pub ssh_key: Option<Vec<u8>>,
    /// Skip comparing claim keys with the machines' consoles.
    pub trust_lan: bool,
}

/// Where to reach each target, and the short fingerprint it announced if
/// it was found over mDNS. A MAC must have been discovered; an address
/// needn't (mDNS may not cross the operator's network).
pub fn resolve(
    targets: &[String],
    discovered: &[Discovered],
) -> Result<Vec<(IpAddr, Option<String>)>, RelishError> {
    targets
        .iter()
        .map(|target| {
            let found = find(discovered, target);
            let announced = found.map(|m| m.short_fingerprint.clone());
            if let Ok(address) = target.parse::<IpAddr>() {
                return Ok((address, announced));
            }
            let machine = found.ok_or_else(|| {
                failed(&format!(
                    "no unclaimed machine with MAC {target} answered over mDNS; give its address instead"
                ))
            })?;
            let address = reachable_address(machine).ok_or_else(|| {
                failed(&format!(
                    "{target} announced no address relish can reach (only IPv6 link-local ones, or none); give its IPv4 address instead"
                ))
            })?;
            Ok((address, announced))
        })
        .collect()
}

/// Refuse a machine named twice among `targets`, whether by the same name
/// or by its MAC and its address (`resolved` holds where each target
/// reaches). Two claims of one machine would give it two seeds, and the
/// second would overwrite the first.
pub fn refuse_repeated_targets(
    targets: &[String],
    resolved: &[(IpAddr, Option<String>)],
) -> Result<(), RelishError> {
    for (index, (address, _)) in resolved.iter().enumerate() {
        let Some(first) = resolved[..index].iter().position(|(a, _)| a == address) else {
            continue;
        };
        let (earlier, later) = (&targets[first], &targets[index]);
        return Err(if earlier.eq_ignore_ascii_case(later) {
            failed(&format!("{later} is named twice: name each machine once"))
        } else {
            failed(&format!(
                "{earlier} and {later} are the same machine ({address}): name each machine once"
            ))
        });
    }
    Ok(())
}

/// A seed to send, and where: the node it makes, its file in the cluster
/// directory, and the claim key the connection is pinned to.
#[derive(Debug, Clone)]
struct Delivery {
    node: bare_metal::FleetNode,
    seed: PathBuf,
    fingerprint: String,
}

/// A machine whose seed didn't arrive, and why.
#[derive(Debug)]
struct Undelivered {
    node: bare_metal::FleetNode,
    seed: PathBuf,
    error: RelishError,
}

/// Send every seed with `post` (`post_seed`, or a test's stand-in), one
/// machine after another. A machine that can't be reached doesn't stop the
/// rest: by now `fleet.json` names them all, so a run that stopped halfway
/// couldn't simply be repeated. Returns the machines that didn't get theirs.
async fn deliver_seeds<F, Fut>(deliveries: &[Delivery], mut post: F) -> Vec<Undelivered>
where
    F: FnMut(SocketAddr, String, Vec<u8>) -> Fut,
    Fut: std::future::Future<Output = Result<(), RelishError>>,
{
    let mut undelivered = Vec::new();
    for delivery in deliveries {
        let node = &delivery.node;
        let sent = match tokio::fs::read(&delivery.seed).await {
            Ok(seed) => {
                post(
                    SocketAddr::new(node.address, CLAIM_PORT),
                    delivery.fingerprint.clone(),
                    seed,
                )
                .await
            }
            Err(error) => Err(RelishError::from(error)),
        };
        match sent {
            Ok(()) => println!(
                "  {} ({} at {}): claimed",
                node.name, node.mac, node.address
            ),
            Err(error) => undelivered.push(Undelivered {
                node: node.clone(),
                seed: delivery.seed.clone(),
                error,
            }),
        }
    }
    undelivered
}

/// The error `relish machines claim` ends with when some seeds didn't
/// arrive: each machine and why, and the command that sends them again.
fn undelivered_error(directory: &Path, trust_lan: bool, failures: &[Undelivered]) -> RelishError {
    let mut message = match failures.len() {
        1 => "1 machine didn't get its seed:\n".to_string(),
        n => format!("{n} machines didn't get their seed:\n"),
    };
    for failure in failures {
        let node = &failure.node;
        // Without the "cluster initialisation failed" each of this module's
        // errors starts with, which would repeat on every line.
        let reason = match &failure.error {
            RelishError::InitFailed(reason) => reason.clone(),
            other => other.to_string(),
        };
        message.push_str(&format!(
            "  {} ({} at {}): {reason}\n",
            node.name, node.mac, node.address
        ));
    }
    let addresses: Vec<String> = failures
        .iter()
        .map(|f| f.node.address.to_string())
        .collect();
    let seeds: Vec<String> = failures
        .iter()
        .map(|f| f.seed.display().to_string())
        .collect();
    message.push_str(&format!(
        "Their seeds are still in the cluster directory. Once the machines answer, send \
         them again with:\n  relish machines claim {}{} {}\nor copy {} into seeds/ on an \
         RBSEED stick and boot the machines with it",
        directory.display(),
        if trust_lan { " --trust-lan" } else { "" },
        addresses.join(" "),
        seeds.join(", ")
    ));
    failed(&message)
}

/// What [`split_known`] answers: the machines to send their seed again,
/// and the machines to add.
type Split = (Vec<(bare_metal::FleetNode, PathBuf)>, Vec<(String, IpAddr)>);

/// The machines `fleet` already has (a claim that didn't arrive, sent
/// again), each with its seed in `directory`, and the machines it doesn't,
/// which join. A fleet machine answering at another address is refused:
/// its seed names the old one.
fn split_known(
    directory: &Path,
    fleet: Option<&bare_metal::Fleet>,
    machines: &[(String, IpAddr)],
) -> Result<Split, RelishError> {
    let mut again = Vec::new();
    let mut new = Vec::new();
    for (mac, address) in machines {
        let known = fleet.and_then(|fleet| fleet.nodes.iter().find(|n| &n.mac == mac));
        match known {
            None => new.push((mac.clone(), *address)),
            Some(node) if node.address != *address => {
                return Err(failed(&format!(
                    "{mac} is {} at {} in {}, but answered at {address}; give it back its address",
                    node.name,
                    node.address,
                    directory.join("fleet.json").display()
                )));
            }
            Some(node) => again.push((
                node.clone(),
                directory.join("stick").join(bare_metal::stick_file(mac)),
            )),
        }
    }
    Ok((again, new))
}

/// The fleet in `directory`, if it has one.
async fn read_fleet(directory: &Path) -> Result<Option<bare_metal::Fleet>, RelishError> {
    let path = directory.join("fleet.json");
    match tokio::fs::read(&path).await {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|e| failed(&format!("{}: {e}", path.display()))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

/// `relish machines claim`: check each machine's claim key, make its seed
/// (a new cluster's, or a join to the cluster in the directory) and post it.
pub async fn run_claim(options: &ClaimOptions) -> Result<(), RelishError> {
    use std::io::IsTerminal;
    let secrets = options.directory.join("secrets");
    let backup = match &options.create {
        Some(cluster) => Some(bare_metal::backup_check(
            cluster.yes,
            std::io::stdin().is_terminal(),
            &secrets,
        )?),
        None => None,
    };
    let discovered = if options.targets.iter().all(|t| t.parse::<IpAddr>().is_ok()) {
        Vec::new()
    } else {
        browse(Duration::from_secs(3))?
    };
    let resolved = resolve(&options.targets, &discovered)?;
    refuse_repeated_targets(&options.targets, &resolved)?;
    let mut machines = Vec::new();
    for (address, announced) in resolved {
        let (info, fingerprint) =
            inspect(SocketAddr::new(address, CLAIM_PORT), announced.as_deref()).await?;
        let mac = info
            .macs
            .first()
            .cloned()
            .ok_or_else(|| failed(&format!("{address} reported no MAC address")))?;
        println!(
            "{mac} at {address} ({}): claim key {}",
            info.arch,
            short_fingerprint(&fingerprint)
        );
        machines.push((mac, address, fingerprint));
    }
    let pairs: Vec<(String, IpAddr)> = machines
        .iter()
        .map(|(mac, address, _)| (mac.clone(), *address))
        .collect();
    // One machine at two addresses (its IPv4 and its IPv6 one, say).
    bare_metal::refuse_repeats(&pairs)?;
    if !options.trust_lan {
        confirm(&machines)?;
    }

    let (created, seeds) = match &options.create {
        Some(cluster) => {
            let keys = crate::relish::quickstart::security::root_backup_keys_in(
                &crate::relish::local_context::root_directory()?,
            );
            let create_options = cluster.create_options(
                &options.directory,
                pairs,
                options.token_ttl,
                options.ssh_key.clone(),
                keys,
            );
            // A backup that isn't confirmed removes what create wrote, so
            // nothing is claimed and the command can simply run again.
            // BLOCKING-OK: relish CLI: a one-shot step before the command does anything long-lived
            let created = bare_metal::create_confirmed(&create_options, || match backup {
                Some(check) => {
                    bare_metal::back_up(check, &secrets, crate::relish::output::OutputFormat::Human)
                }
                None => Ok(()),
            })?;
            println!(
                "The root CA key is only in {}, which {} opens. Move both offline, apart \
                 from each other: no machine holds either.",
                created.root_backup.backup.display(),
                created.root_backup.identity.display()
            );
            let seeds = created
                .fleet
                .nodes
                .iter()
                .cloned()
                .zip(created.seeds.iter().cloned())
                .collect::<Vec<_>>();
            (Some(created), seeds)
        }
        None => {
            let fleet = read_fleet(&options.directory).await?;
            let (mut seeds, new) = split_known(&options.directory, fleet.as_ref(), &pairs)?;
            if new.is_empty() {
                // Only seeds sent again, and their join tokens are already
                // in them. A window still helps a machine added after the
                // cluster was made, but there may be no cluster to ask yet:
                // node 1's seed may be among them.
                if let Err(error) = open_join_windows(&pairs).await {
                    eprintln!("warning: no join windows opened: {error}");
                }
            } else {
                open_join_windows(&pairs).await?;
                seeds.extend(
                    bare_metal::add(
                        &options.directory,
                        &new,
                        options.token_ttl,
                        options.ssh_key.clone(),
                    )
                    .await?,
                );
            }
            (None, seeds)
        }
    };
    let deliveries: Vec<Delivery> = seeds
        .into_iter()
        .map(|(node, seed)| {
            let (_, _, fingerprint) = machines
                .iter()
                .find(|(mac, ..)| *mac == node.mac)
                .ok_or_else(|| failed(&format!("no claim key for {}", node.mac)))?;
            Ok(Delivery {
                fingerprint: fingerprint.clone(),
                node,
                seed,
            })
        })
        .collect::<Result<_, RelishError>>()?;
    let failures = deliver_seeds(&deliveries, |address, fingerprint, seed| async move {
        post_seed(address, &fingerprint, seed).await
    })
    .await;
    // The cluster is in the directory whether or not node 1 got its seed,
    // so relish points at it either way: the context is then ready when a
    // retry reaches node 1.
    if let Some(created) = &created {
        println!(
            "cluster {} created in {}",
            created.fleet.cluster,
            options.directory.display()
        );
        // BLOCKING-OK: relish CLI: a one-shot step before the command does anything long-lived
        match bare_metal::adopt(created, &options.directory) {
            Ok(context) => print!("{context}"),
            Err(error) if failures.is_empty() => return Err(error),
            // The seeds that didn't arrive matter more: say what to run.
            Err(error) => eprintln!("warning: relish doesn't point at the cluster: {error}"),
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(undelivered_error(
            &options.directory,
            options.trust_lan,
            &failures,
        ))
    }
}

/// How long a claimed machine has to enrol through the cluster's
/// firewalls before they shut it out again.
const JOIN_WINDOW_MINUTES: u64 = 15;

/// Let the machines through every node's perimeter while they enrol (G2),
/// so joining doesn't depend on the cluster having been created with
/// `--network` covering them. A node that can't be reached is reported and
/// skipped: if it's down, its firewall doesn't matter.
async fn open_join_windows(machines: &[(String, IpAddr)]) -> Result<(), RelishError> {
    let client = super::client::BunClient::default_local();
    let nodes = client.nodes().await?;
    for node in nodes.iter().filter(|n| n.state == "alive") {
        let node_client = match client.for_node(node) {
            Ok(node_client) => node_client,
            Err(error) => {
                eprintln!("warning: {error}");
                continue;
            }
        };
        for (_, address) in machines {
            if let Err(error) = node_client
                .perimeter_admit(*address, JOIN_WINDOW_MINUTES)
                .await
            {
                eprintln!(
                    "warning: {} didn't open a join window for {address}: {error}",
                    node.node_id
                );
            }
        }
    }
    println!("opened a {JOIN_WINDOW_MINUTES}-minute join window on the cluster's nodes");
    Ok(())
}

/// Ask the operator to compare each claim key with the machine's console.
/// Anyone on the LAN can answer mDNS, so this is what stops a seed, and the
/// cluster's secrets in it, going to the wrong machine.
fn confirm(machines: &[(String, IpAddr, String)]) -> Result<(), RelishError> {
    use std::io::IsTerminal;
    confirm_with(
        machines,
        std::io::stdin().is_terminal(),
        std::io::stdin().lock(),
        std::io::stdout(),
    )
}

/// [`confirm`], reading the answers from `input` and asking on `output`, so
/// a test can be the operator.
fn confirm_with(
    machines: &[(String, IpAddr, String)],
    is_terminal: bool,
    mut input: impl std::io::BufRead,
    mut output: impl std::io::Write,
) -> Result<(), RelishError> {
    if !is_terminal {
        return Err(failed(
            "compare the claim keys with the machines' consoles: run this in a terminal, \
             or pass --trust-lan if you trust everything on this network",
        ));
    }
    for (mac, address, fingerprint) in machines {
        write!(
            output,
            "Does the console of {mac} ({address}) show claim key {}? [y/N] ",
            short_fingerprint(fingerprint)
        )?;
        output.flush()?;
        let mut answer = String::new();
        input.read_line(&mut answer)?;
        if !matches!(answer.trim(), "y" | "Y" | "yes") {
            return Err(failed(&format!(
                "{mac} not claimed: its claim key wasn't confirmed"
            )));
        }
    }
    Ok(())
}

fn failed(message: &str) -> RelishError {
    RelishError::InitFailed(message.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY_ONE: &str = "sha256:3f9a12bc77de0a41aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const KEY_TWO: &str = "sha256:9c0e44d1b2a37f58bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    fn two_machines() -> Vec<(String, IpAddr, String)> {
        vec![
            (
                "52:54:00:42:00:01".into(),
                "10.42.0.11".parse().unwrap(),
                KEY_ONE.into(),
            ),
            (
                "52:54:00:42:00:02".into(),
                "10.42.0.12".parse().unwrap(),
                KEY_TWO.into(),
            ),
        ]
    }

    /// Run the comparison with `answers` typed at a terminal; return the
    /// outcome and what the operator saw.
    fn compare(answers: &str) -> (Result<(), RelishError>, String) {
        let mut shown = Vec::new();
        let outcome = confirm_with(&two_machines(), true, answers.as_bytes(), &mut shown);
        (outcome, String::from_utf8(shown).unwrap())
    }

    #[test]
    fn each_claim_key_is_shown_short_beside_its_machine_and_a_yes_claims_both() {
        let (outcome, shown) = compare("y\nyes\n");
        outcome.unwrap();
        assert!(
            shown.contains("Does the console of 52:54:00:42:00:01 (10.42.0.11) show claim key 3f9a-12bc-77de-0a41? [y/N] "),
            "{shown}"
        );
        assert!(
            shown.contains("Does the console of 52:54:00:42:00:02 (10.42.0.12) show claim key 9c0e-44d1-b2a3-7f58? [y/N] "),
            "{shown}"
        );
    }

    #[test]
    fn a_no_refuses_that_machine_and_asks_about_no_more() {
        let (outcome, shown) = compare("Y\nn\n");
        let error = outcome.unwrap_err().to_string();
        assert!(
            error.contains("52:54:00:42:00:02 not claimed: its claim key wasn't confirmed"),
            "{error}"
        );

        let (outcome, shown_after_first) = compare("no\ny\n");
        let error = outcome.unwrap_err().to_string();
        assert!(error.contains("52:54:00:42:00:01 not claimed"), "{error}");
        assert!(
            !shown_after_first.contains("52:54:00:42:00:02"),
            "{shown_after_first}"
        );
        assert!(shown.contains("52:54:00:42:00:02"), "{shown}");
    }

    #[test]
    fn anything_but_yes_is_a_no_and_so_is_an_empty_answer() {
        for answers in ["\n", "", "yep\n", "maybe\n", "1\n"] {
            let (outcome, _) = compare(answers);
            assert!(outcome.is_err(), "{answers:?} claimed a machine");
        }
    }

    #[test]
    fn without_a_terminal_the_comparison_refuses_and_points_at_trust_lan() {
        let mut shown = Vec::new();
        let error = confirm_with(&two_machines(), false, "y\ny\n".as_bytes(), &mut shown)
            .unwrap_err()
            .to_string();
        assert!(error.contains("run this in a terminal"), "{error}");
        assert!(error.contains("--trust-lan"), "{error}");
        assert!(shown.is_empty(), "asked without a terminal: {shown:?}");
    }

    fn machine(address: &str, mac: &str) -> Discovered {
        Discovered {
            addresses: vec![address.parse().unwrap()],
            macs: vec![mac.into()],
            arch: "x86_64".into(),
            short_fingerprint: "3f9a-12bc-77de-0a41".into(),
        }
    }

    #[test]
    fn a_machine_is_reached_at_its_ipv4_address_never_a_link_local_one() {
        let mut both = machine("fe80::5054:ff:fe00:1", "52:54:00:00:00:01");
        both.addresses.push("10.42.0.11".parse().unwrap());
        assert_eq!(
            reachable_address(&both),
            Some("10.42.0.11".parse().unwrap())
        );

        let routable = machine("2001:db8::11", "52:54:00:00:00:02");
        assert_eq!(
            reachable_address(&routable),
            Some("2001:db8::11".parse().unwrap())
        );

        let link_local_only = machine("fe80::5054:ff:fe00:3", "52:54:00:00:00:03");
        assert_eq!(reachable_address(&link_local_only), None);
        // The table still shows it, as a sign of life.
        assert!(render(&[link_local_only]).contains("fe80::5054:ff:fe00:3"));
        assert!(render(&[both]).contains("10.42.0.11"));
    }

    #[test]
    fn machines_are_found_by_mac_or_address() {
        let machines = [
            machine("192.168.1.51", "d8:9e:f3:12:34:56"),
            machine("192.168.1.52", "d8:9e:f3:12:34:57"),
        ];
        assert_eq!(
            find(&machines, "D8-9E-F3-12-34-57").unwrap().addresses[0].to_string(),
            "192.168.1.52"
        );
        assert_eq!(
            find(&machines, "192.168.1.51").unwrap().macs[0],
            "d8:9e:f3:12:34:56"
        );
        assert!(find(&machines, "192.168.1.99").is_none());
    }

    #[test]
    fn the_table_lists_address_mac_arch_and_claim_key() {
        let table = render(&[machine("192.168.1.51", "d8:9e:f3:12:34:56")]);
        assert_eq!(
            table,
            "ADDRESS           MAC               ARCH     CLAIM KEY\n\
             192.168.1.51      d8:9e:f3:12:34:56 x86_64   3f9a-12bc-77de-0a41\n"
        );
        assert_eq!(render(&[]), "no unclaimed machines answered\n");
    }

    #[test]
    fn the_list_as_data_keeps_every_address_and_mac() {
        let mut both = machine("fe80::5054:ff:fe00:1", "52:54:00:00:00:01");
        both.addresses.push("10.42.0.11".parse().unwrap());
        assert_eq!(
            serde_json::to_value(MachineList(vec![both])).unwrap(),
            serde_json::json!([{
                "addresses": ["fe80::5054:ff:fe00:1", "10.42.0.11"],
                "macs": ["52:54:00:00:00:01"],
                "arch": "x86_64",
                "short_fingerprint": "3f9a-12bc-77de-0a41"
            }])
        );
        assert_eq!(
            serde_json::to_value(MachineList(vec![])).unwrap(),
            serde_json::json!([])
        );
    }

    #[test]
    fn a_mac_resolves_through_mdns_and_an_address_needs_nothing() {
        let machines = [machine("192.168.1.51", "d8:9e:f3:12:34:56")];
        let targets = ["d8:9e:f3:12:34:56".to_string(), "10.0.0.7".to_string()];
        assert_eq!(
            resolve(&targets, &machines).unwrap(),
            [
                (
                    "192.168.1.51".parse().unwrap(),
                    Some("3f9a-12bc-77de-0a41".to_string())
                ),
                ("10.0.0.7".parse().unwrap(), None)
            ]
        );
        assert_eq!(
            resolve(&["192.168.1.51".to_string()], &machines).unwrap()[0]
                .1
                .as_deref(),
            Some("3f9a-12bc-77de-0a41"),
            "an address that also announced itself is checked against its announcement"
        );
        assert!(resolve(&["d8:9e:f3:00:00:00".to_string()], &machines).is_err());
    }

    #[test]
    fn claim_create_hands_the_council_size_to_the_new_cluster() {
        let cluster = NewCluster {
            name: "lab".into(),
            operators: vec!["10.42.0.1".into()],
            network: None,
            faults: false,
            external_signing_key: None,
            council_size: crate::council::CouncilSize::APPLIANCE,
            yes: false,
        };
        let dir = tempfile::tempdir().unwrap();
        let options = cluster.create_options(
            dir.path(),
            vec![("d8:9e:f3:00:00:01".into(), "10.42.0.11".parse().unwrap())],
            Duration::from_secs(3600),
            None,
            dir.path().join("operator-keys"),
        );
        assert_eq!(options.council_size, crate::council::CouncilSize::APPLIANCE);
        assert_eq!(options.cluster, "lab");
        let created = bare_metal::create(&options).unwrap();
        assert_eq!(
            created.fleet.council_size,
            Some(crate::council::CouncilSize::APPLIANCE),
            "fleet.json records it"
        );
    }

    #[test]
    fn a_machine_named_twice_is_refused_naming_both_targets() {
        let resolved = resolve(
            &[
                "10.42.0.11".to_string(),
                "10.42.0.12".to_string(),
                "d8:9e:f3:12:34:56".to_string(),
            ],
            &[machine("10.42.0.11", "d8:9e:f3:12:34:56")],
        )
        .unwrap();
        let targets = [
            "10.42.0.11".to_string(),
            "10.42.0.12".to_string(),
            "d8:9e:f3:12:34:56".to_string(),
        ];
        let error = refuse_repeated_targets(&targets, &resolved)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("10.42.0.11 and d8:9e:f3:12:34:56 are the same machine"),
            "{error}"
        );
        refuse_repeated_targets(&targets[..2], &resolved[..2]).unwrap();
    }

    #[test]
    fn two_answers_with_one_mac_are_one_machine_and_refused() {
        let error = bare_metal::refuse_repeats(&[
            ("d8:9e:f3:12:34:56".into(), "10.42.0.11".parse().unwrap()),
            ("d8:9e:f3:12:34:56".into(), "2001:db8::11".parse().unwrap()),
        ])
        .unwrap_err()
        .to_string();
        assert!(error.contains("listed twice"), "{error}");
    }

    /// A cluster of `machines` made in `dir`, as `--create` makes it.
    fn created(dir: &std::path::Path, machines: &[(&str, &str)]) -> bare_metal::Created {
        let cluster = NewCluster {
            name: "home".into(),
            operators: vec!["10.42.0.1".into()],
            network: None,
            faults: false,
            external_signing_key: None,
            council_size: crate::council::CouncilSize::APPLIANCE,
            yes: true,
        };
        bare_metal::create(
            &cluster.create_options(
                dir,
                machines
                    .iter()
                    .map(|(mac, address)| (mac.to_string(), address.parse().unwrap()))
                    .collect(),
                Duration::from_secs(3600),
                None,
                dir.join("operator-keys"),
            ),
        )
        .unwrap()
    }

    const THREE: [(&str, &str); 3] = [
        ("52:54:00:42:00:01", "10.42.0.11"),
        ("52:54:00:42:00:02", "10.42.0.12"),
        ("52:54:00:42:00:03", "10.42.0.13"),
    ];

    fn deliveries(created: &bare_metal::Created) -> Vec<Delivery> {
        created
            .fleet
            .nodes
            .iter()
            .zip(&created.seeds)
            .map(|(node, seed)| Delivery {
                node: node.clone(),
                seed: seed.clone(),
                fingerprint: KEY_ONE.into(),
            })
            .collect()
    }

    #[tokio::test]
    async fn every_machine_is_sent_its_seed_though_one_refuses_it() {
        let dir = tempfile::tempdir().unwrap();
        let created = created(dir.path(), &THREE);
        let posted = Mutex::new(Vec::new());
        let failures = deliver_seeds(&deliveries(&created), |address, fingerprint, seed| {
            posted
                .lock()
                .unwrap()
                .push((address, fingerprint, seed.len()));
            async move {
                if address.ip().to_string() == "10.42.0.12" {
                    Err(failed("10.42.0.12: connection refused"))
                } else {
                    Ok(())
                }
            }
        })
        .await;
        let posted = posted.into_inner().unwrap();
        let addresses: Vec<String> = posted.iter().map(|(a, _, _)| a.to_string()).collect();
        assert_eq!(
            addresses,
            [
                "10.42.0.11:9119".to_string(),
                "10.42.0.12:9119".to_string(),
                "10.42.0.13:9119".to_string()
            ],
            "the third machine still gets its seed"
        );
        assert!(
            posted
                .iter()
                .all(|(_, key, len)| key == KEY_ONE && *len > 0)
        );
        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0].node.name, "home-2");
        assert!(
            failures[0].error.to_string().contains("connection refused"),
            "{}",
            failures[0].error
        );
    }

    #[tokio::test]
    async fn a_seed_that_cannot_be_read_is_a_failure_for_that_machine_only() {
        let dir = tempfile::tempdir().unwrap();
        let created = created(dir.path(), &THREE);
        std::fs::remove_file(&created.seeds[0]).unwrap();
        let failures = deliver_seeds(&deliveries(&created), |_, _, _| async { Ok(()) }).await;
        let names: Vec<&str> = failures.iter().map(|f| f.node.name.as_str()).collect();
        assert_eq!(names, ["home-1"]);
    }

    #[test]
    fn the_failure_report_names_each_machine_and_the_command_that_sends_it_again() {
        let dir = tempfile::tempdir().unwrap();
        let created = created(dir.path(), &THREE);
        let failures: Vec<Undelivered> = deliveries(&created)
            .into_iter()
            .skip(1)
            .map(|delivery| Undelivered {
                error: failed(&format!("{}: timed out", delivery.node.address)),
                node: delivery.node,
                seed: delivery.seed,
            })
            .collect();
        for (trust_lan, flag) in [(true, " --trust-lan"), (false, "")] {
            let report = undelivered_error(dir.path(), trust_lan, &failures).to_string();
            assert!(
                report.contains("2 machines didn't get their seed"),
                "{report}"
            );
            assert!(
                report.contains("home-2 (52:54:00:42:00:02 at 10.42.0.12): 10.42.0.12: timed out"),
                "{report}"
            );
            assert!(report.contains("home-3 (52:54:00:42:00:03"), "{report}");
            let command = format!(
                "relish machines claim {}{flag} 10.42.0.12 10.42.0.13",
                dir.path().display()
            );
            assert!(report.contains(&command), "{report}");
            assert!(
                report.contains(&created.seeds[1].display().to_string()),
                "{report}"
            );
        }
    }

    #[test]
    fn a_machine_already_in_the_fleet_is_sent_its_seed_again_and_a_new_one_is_added() {
        let dir = tempfile::tempdir().unwrap();
        let created = created(dir.path(), &THREE[..2]);
        let new: (String, IpAddr) = ("52:54:00:42:00:09".into(), "10.42.0.19".parse().unwrap());
        let (again, added) = split_known(
            dir.path(),
            Some(&created.fleet),
            &[
                ("52:54:00:42:00:02".into(), "10.42.0.12".parse().unwrap()),
                new.clone(),
            ],
        )
        .unwrap();
        assert_eq!(
            again,
            [(created.fleet.nodes[1].clone(), created.seeds[1].clone())]
        );
        assert_eq!(added, [new]);
    }

    #[test]
    fn a_fleet_machine_answering_at_another_address_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let created = created(dir.path(), &THREE[..2]);
        let error = split_known(
            dir.path(),
            Some(&created.fleet),
            &[("52:54:00:42:00:02".into(), "10.42.0.99".parse().unwrap())],
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("home-2"), "{error}");
        assert!(error.contains("10.42.0.12"), "{error}");
        assert!(error.contains("10.42.0.99"), "{error}");
    }

    #[tokio::test]
    async fn a_directory_without_a_fleet_has_no_known_machines() {
        let dir = tempfile::tempdir().unwrap();
        let fleet = read_fleet(dir.path()).await.unwrap();
        assert!(fleet.is_none());
        let machines = vec![(
            "52:54:00:42:00:09".to_string(),
            "10.42.0.19".parse().unwrap(),
        )];
        let (again, added) = split_known(dir.path(), fleet.as_ref(), &machines).unwrap();
        assert!(again.is_empty());
        assert_eq!(added, machines);
    }

    /// The whole claim over real TLS on loopback: inspect learns the key,
    /// a pinned post with the right key claims, a wrong pin never connects.
    #[tokio::test]
    async fn a_seed_only_goes_to_the_pinned_claim_key() {
        let dir = tempfile::tempdir().unwrap();
        let key = crate::appliance::claim::ClaimKey::load_or_create(dir.path()).unwrap();
        let info = MachineInfo {
            macs: vec!["d8:9e:f3:12:34:56".into()],
            arch: "x86_64".into(),
            os_version: None,
            fingerprint: key.fingerprint(),
        };
        let seed_path = dir.path().join("claimed.seed");
        // An ephemeral port and no mDNS: the production port may be taken
        // by another test, and announcing isn't what this test is about.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let local = listener.local_addr().unwrap();
        let server = tokio::spawn({
            let info = info.clone();
            let seed_path = seed_path.clone();
            async move { crate::appliance::claim::serve(listener, &key, info, seed_path).await }
        });
        let learned = inspect(local, Some(&short_fingerprint(&info.fingerprint)))
            .await
            .unwrap();
        let (seen_info, fingerprint) = learned;
        assert_eq!(seen_info, info);
        assert_eq!(fingerprint, info.fingerprint);

        let seed = crate::appliance::seed::tests::tarball(&[(
            "seed.toml",
            crate::appliance::seed::tests::JOIN_TOML.as_bytes(),
        )]);
        let wrong = format!("sha256:{}", "0".repeat(64));
        assert!(
            post_seed(local, &wrong, seed.clone()).await.is_err(),
            "a wrong pin"
        );
        assert!(!seed_path.exists());

        post_seed(local, &fingerprint, seed.clone()).await.unwrap();
        tokio::time::timeout(Duration::from_secs(10), server)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(std::fs::read(&seed_path).unwrap(), seed);
    }
}
