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
use std::path::PathBuf;
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
#[derive(Debug, Clone, PartialEq, Eq)]
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
            if !found.iter().any(|m| m.macs == machine.macs) {
                found.push(machine);
            }
        }
    }
    let _ = daemon.shutdown();
    found.sort_by(|a, b| a.addresses.cmp(&b.addresses));
    Ok(found)
}

/// Find the machine `target` (a MAC or an IP) among `machines`.
pub fn find<'a>(machines: &'a [Discovered], target: &str) -> Option<&'a Discovered> {
    let wanted = target.to_ascii_lowercase().replace('-', ":");
    machines
        .iter()
        .find(|m| m.macs.contains(&wanted) || m.addresses.iter().any(|a| a.to_string() == target))
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
        let address = machine
            .addresses
            .iter()
            .find(|a| a.is_ipv4())
            .or(machine.addresses.first())
            .map(ToString::to_string)
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
    let client = reqwest::Client::builder()
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
            let address = machine
                .addresses
                .iter()
                .find(|a| a.is_ipv4())
                .or(machine.addresses.first())
                .copied()
                .ok_or_else(|| failed(&format!("{target} announced no address")))?;
            Ok((address, announced))
        })
        .collect()
}

/// `relish machines claim`: check each machine's claim key, make its seed
/// (a new cluster's, or a join to the cluster in the directory) and post it.
pub async fn run_claim(options: &ClaimOptions) -> Result<(), RelishError> {
    let discovered = if options.targets.iter().all(|t| t.parse::<IpAddr>().is_ok()) {
        Vec::new()
    } else {
        browse(Duration::from_secs(3))?
    };
    let mut machines = Vec::new();
    for (address, announced) in resolve(&options.targets, &discovered)? {
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
    if !options.trust_lan {
        confirm(&machines)?;
    }

    let pairs: Vec<(String, IpAddr)> = machines
        .iter()
        .map(|(mac, address, _)| (mac.clone(), *address))
        .collect();
    let (created, seeds) = match &options.create {
        Some(cluster) => {
            let created = bare_metal::create(&bare_metal::CreateOptions {
                directory: options.directory.clone(),
                cluster: cluster.name.clone(),
                machines: pairs,
                operators: cluster.operators.clone(),
                network: cluster.network.clone(),
                faults: cluster.faults,
                ssh_key: options.ssh_key.clone(),
                token_ttl: options.token_ttl,
                external_signing_key: cluster.external_signing_key.clone(),
            })?;
            let seeds = created
                .fleet
                .nodes
                .iter()
                .cloned()
                .zip(created.seeds.iter().cloned())
                .collect::<Vec<_>>();
            (Some(created), seeds)
        }
        None => (None, {
            open_join_windows(&pairs).await?;
            bare_metal::add(
                &options.directory,
                &pairs,
                options.token_ttl,
                options.ssh_key.clone(),
            )
            .await?
        }),
    };
    for ((node, seed), (_, address, fingerprint)) in seeds.iter().zip(&machines) {
        post_seed(
            SocketAddr::new(*address, CLAIM_PORT),
            fingerprint,
            std::fs::read(seed)?,
        )
        .await
        .map_err(|e| {
            failed(&format!(
                "{e}; its seed is still in {}, for a stick",
                seed.display()
            ))
        })?;
        println!(
            "  {} ({} at {}): claimed",
            node.name, node.mac, node.address
        );
    }
    if let Some(created) = &created {
        println!(
            "cluster {} created in {}",
            created.fleet.cluster,
            options.directory.display()
        );
        bare_metal::adopt(created, &options.directory)?;
    }
    Ok(())
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
    use std::io::{BufRead, IsTerminal, Write};
    if !std::io::stdin().is_terminal() {
        return Err(failed(
            "compare the claim keys with the machines' consoles: run this in a terminal, \
             or pass --trust-lan if you trust everything on this network",
        ));
    }
    for (mac, address, fingerprint) in machines {
        print!(
            "Does the console of {mac} ({address}) show claim key {}? [y/N] ",
            short_fingerprint(fingerprint)
        );
        std::io::stdout().flush()?;
        let mut answer = String::new();
        std::io::stdin().lock().read_line(&mut answer)?;
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

    fn machine(address: &str, mac: &str) -> Discovered {
        Discovered {
            addresses: vec![address.parse().unwrap()],
            macs: vec![mac.into()],
            arch: "x86_64".into(),
            short_fingerprint: "3f9a-12bc-77de-0a41".into(),
        }
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
