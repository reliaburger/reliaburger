//! What the console asks bun: the cluster summary, `wtf`'s checks, this
//! node's workloads and the council, each through the same client `relish`
//! uses.
//!
//! The console holds no token a person made. It reads the node's master
//! key (it runs as root, beside bun), derives the cluster's service token
//! from it, and from that the console token, which every node accepts as
//! a read-only principal (`crate::sesame::token::derive_console_token`).
//! It talks to bun at the node's LAN address, the one its certificate
//! names, and checks that certificate against the cluster's root CA.

use std::net::IpAddr;
use std::path::PathBuf;
use std::time::Duration;

use crate::bun::summary::ClusterSummary;
use crate::bun::summary::verdict::WtfCheck;
use crate::bun::top::TopRow;
use crate::config::node::NodeConfig;
use crate::relish::client::BunClient;
use crate::relish::council_view::CouncilNodeObservation;

/// bun's API port on an appliance (`--listen`'s default).
pub const API_PORT: u16 = 9117;

/// How long one question to bun gets before the panel says it failed.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// How long `wtf`'s checks get, as `relish status` gives them more.
const WTF_BUDGET: Duration = Duration::from_secs(30);

/// Where and how to reach this node's bun. Its `Debug` leaves the token
/// out.
#[derive(Clone, PartialEq, Eq)]
pub struct Endpoint {
    pub base_url: String,
    /// The console token; `None` on a node with no master key, which
    /// enforces no tokens anyway.
    pub token: Option<String>,
    /// The cluster's root CA, PEM; `None` for plain HTTP.
    pub root_ca_pem: Option<Vec<u8>>,
}

impl std::fmt::Debug for Endpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Endpoint")
            .field("base_url", &self.base_url)
            .field("token", &self.token.as_ref().map(|_| "<redacted>"))
            .field("root_ca_pem", &self.root_ca_pem.as_ref().map(Vec::len))
            .finish()
    }
}

/// Why the console can't ask bun anything yet.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EndpointError {
    #[error("not seeded yet: no node.toml")]
    NotSeeded,
    #[error("node.toml: {0}")]
    Config(String),
    #[error("{0}")]
    Credentials(String),
}

/// The endpoint `config` describes. With an identity (an appliance, whose
/// API is TLS) it's the advertised address, which the node's certificate
/// names; without one it's plain HTTP on loopback.
pub fn endpoint_for(
    config: &NodeConfig,
    root_ca_pem: Option<Vec<u8>>,
    service_token: Option<&str>,
) -> Result<Endpoint, EndpointError> {
    let token = service_token
        .map(crate::sesame::token::derive_console_token)
        .transpose()
        .map_err(|error| EndpointError::Credentials(error.to_string()))?;
    let base_url = match &root_ca_pem {
        Some(_) => {
            let address = advertised_ip(config).ok_or_else(|| {
                EndpointError::Config("no [network] advertise_address".to_string())
            })?;
            match address {
                IpAddr::V4(v4) => format!("https://{v4}:{API_PORT}"),
                IpAddr::V6(v6) => format!("https://[{v6}]:{API_PORT}"),
            }
        }
        None => format!("http://127.0.0.1:{API_PORT}"),
    };
    Ok(Endpoint {
        base_url,
        token,
        root_ca_pem,
    })
}

/// The address `[network] advertise_address` names, as an address or the
/// address part of `address:port`.
fn advertised_ip(config: &NodeConfig) -> Option<IpAddr> {
    let configured = config.network.advertise_address.as_deref()?;
    configured.parse::<IpAddr>().ok().or_else(|| {
        configured
            .parse::<std::net::SocketAddr>()
            .ok()
            .map(|socket| socket.ip())
    })
}

/// Read node.toml, the root CA and the master key, and build the client.
/// The files are small, but they're read on a blocking thread all the same.
async fn build_client(config_dir: PathBuf) -> Result<BunClient, EndpointError> {
    let endpoint = tokio::task::spawn_blocking(move || read_endpoint(&config_dir))
        .await
        .map_err(|error| EndpointError::Credentials(error.to_string()))??;
    match &endpoint.root_ca_pem {
        Some(pem) => BunClient::new_with_ca(&endpoint.base_url, endpoint.token.as_deref(), pem)
            .map_err(|error| EndpointError::Credentials(error.to_string())),
        None => Ok(BunClient::new_with_token(
            &endpoint.base_url,
            endpoint.token.as_deref(),
        )),
    }
}

fn read_endpoint(config_dir: &std::path::Path) -> Result<Endpoint, EndpointError> {
    let text = match std::fs::read_to_string(config_dir.join("node.toml")) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(EndpointError::NotSeeded);
        }
        Err(error) => return Err(EndpointError::Config(error.to_string())),
    };
    let config: NodeConfig =
        toml::from_str(&text).map_err(|error| EndpointError::Config(error.to_string()))?;
    let root_ca_pem = match &config.security.identity_dir {
        Some(dir) => Some(std::fs::read(dir.join("root-ca.crt")).map_err(|error| {
            EndpointError::Credentials(format!("can't read the cluster's root CA: {error}"))
        })?),
        None => None,
    };
    let service_token = match &config.security.master_key_path {
        Some(path) => {
            let key = crate::sesame::bootstrap::load_master_key(path)
                .map_err(|error| EndpointError::Credentials(error.to_string()))?;
            Some(
                crate::sesame::token::derive_service_token(&key)
                    .map_err(|error| EndpointError::Credentials(error.to_string()))?,
            )
        }
        None => None,
    };
    endpoint_for(&config, root_ca_pem, service_token.as_deref())
}

/// `future`, or a timeout error after [`REQUEST_TIMEOUT`].
async fn bounded<T>(
    future: impl Future<Output = Result<T, crate::relish::RelishError>>,
) -> Result<T, String> {
    match tokio::time::timeout(REQUEST_TIMEOUT, future).await {
        Ok(result) => result.map_err(|error| error.to_string()),
        Err(_) => Err(format!("no answer within {} s", REQUEST_TIMEOUT.as_secs())),
    }
}

/// The sources `wtf` reads that only an admin may, so the read-only
/// console principal always gets them as unknown: `GET /v1/token/list`.
const ADMIN_ONLY_SOURCES: [&str; 1] = ["tokens"];

/// `wtf`'s counts as the console shows them: a check only an admin can run
/// isn't counted as unknown, or every screen would say `1 unknown`.
pub fn console_check(report: &crate::relish::wtf::WtfReport) -> WtfCheck {
    let unknown = report
        .unknown
        .iter()
        .filter(|unknown| !ADMIN_ONLY_SOURCES.contains(&unknown.source.as_str()))
        .count();
    WtfCheck::Checked {
        critical: report.critical.len(),
        warning: report.warnings.len(),
        unknown,
        first_critical: report.critical.first().map(|finding| finding.title.clone()),
    }
}

/// bun's API for the console, with one client kept between fetches.
///
/// Building a client reads node.toml and the master key, derives two
/// tokens and sets up TLS, so it's done once and again only after a fetch
/// fails: a restarted bun, a renewed CA or a node seeded since all show
/// up as a failure first.
pub struct Remote {
    config_dir: PathBuf,
    client: tokio::sync::Mutex<Option<BunClient>>,
}

impl Remote {
    pub fn new(config_dir: PathBuf) -> Self {
        Self {
            config_dir,
            client: tokio::sync::Mutex::new(None),
        }
    }

    async fn client(&self) -> Result<BunClient, String> {
        let mut cached = self.client.lock().await;
        if let Some(client) = cached.as_ref() {
            return Ok(client.clone());
        }
        let client = build_client(self.config_dir.clone())
            .await
            .map_err(|error| error.to_string())?;
        *cached = Some(client.clone());
        Ok(client)
    }

    /// Drop the cached client after a failure, so the next fetch rebuilds it.
    async fn forget<T, E>(&self, result: Result<T, E>) -> Result<T, E> {
        if result.is_err() {
            *self.client.lock().await = None;
        }
        result
    }

    /// The cluster summary, as `relish status` reads it.
    pub async fn summary(&self) -> Result<ClusterSummary, String> {
        let client = self.client().await?;
        self.forget(bounded(client.cluster_summary()).await).await
    }

    /// `wtf`'s checks, as `relish status` runs them, counted by
    /// [`console_check`].
    pub async fn wtf(&self) -> WtfCheck {
        let client = match self.client().await {
            Ok(client) => client,
            Err(reason) => return WtfCheck::Failed { reason },
        };
        let collected =
            tokio::time::timeout(WTF_BUDGET, crate::relish::wtf::collect(&client, None)).await;
        match collected {
            Err(_) => WtfCheck::Late {
                budget_seconds: WTF_BUDGET.as_secs(),
            },
            Ok(Err(error)) => {
                *self.client.lock().await = None;
                WtfCheck::Failed {
                    reason: error.to_string(),
                }
            }
            Ok(Ok(inputs)) => console_check(&crate::relish::wtf::diagnose(&inputs)),
        }
    }

    /// This node's workloads with their CPU and memory.
    pub async fn workloads(&self) -> Result<Vec<TopRow>, String> {
        let client = self.client().await?;
        self.forget(bounded(client.node_top()).await).await
    }

    /// What every node says about the council.
    pub async fn council(&self) -> Result<Vec<CouncilNodeObservation>, String> {
        let client = self.client().await?;
        self.forget(bounded(crate::relish::council_view::survey(&client)).await)
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn appliance_config() -> NodeConfig {
        let mut config = NodeConfig::default();
        config.network.advertise_address = Some("192.168.1.52".into());
        config.security.identity_dir = Some("/etc/reliaburger/identity".into());
        config
    }

    #[test]
    fn an_appliance_is_reached_at_its_lan_address_with_the_console_token() {
        let service = crate::sesame::token::derive_service_token(&[1u8; 32]).unwrap();
        let endpoint =
            endpoint_for(&appliance_config(), Some(b"pem".to_vec()), Some(&service)).unwrap();
        assert_eq!(endpoint.base_url, "https://192.168.1.52:9117");
        assert_eq!(
            endpoint.token,
            Some(crate::sesame::token::derive_console_token(&service).unwrap())
        );
        assert_ne!(endpoint.token.as_deref(), Some(service.as_str()));
    }

    #[test]
    fn an_advertised_socket_address_gives_its_ip() {
        let mut config = appliance_config();
        config.network.advertise_address = Some("[fd00::5]:9443".into());
        let endpoint = endpoint_for(&config, Some(b"pem".to_vec()), None).unwrap();
        assert_eq!(endpoint.base_url, "https://[fd00::5]:9117");
        assert_eq!(endpoint.token, None);
    }

    #[test]
    fn a_node_without_an_identity_is_plain_http_on_loopback() {
        let endpoint = endpoint_for(&NodeConfig::default(), None, None).unwrap();
        assert_eq!(endpoint.base_url, "http://127.0.0.1:9117");
    }

    #[test]
    fn tls_without_an_advertised_address_is_an_error() {
        let mut config = appliance_config();
        config.network.advertise_address = None;
        assert!(matches!(
            endpoint_for(&config, Some(b"pem".to_vec()), None),
            Err(EndpointError::Config(_))
        ));
    }

    #[test]
    fn an_endpoints_debug_never_shows_its_token() {
        let service = crate::sesame::token::derive_service_token(&[1u8; 32]).unwrap();
        let endpoint =
            endpoint_for(&appliance_config(), Some(b"pem".to_vec()), Some(&service)).unwrap();
        let token = endpoint.token.clone().unwrap();
        let shown = format!("{endpoint:?}");
        assert!(!shown.contains(&token), "{shown}");
        assert!(shown.contains("<redacted>"), "{shown}");
        assert!(shown.contains("https://192.168.1.52:9117"), "{shown}");
    }

    fn report(unknown_sources: &[&str]) -> crate::relish::wtf::WtfReport {
        crate::relish::wtf::WtfReport {
            schema_version: 1,
            cluster_name: "home".into(),
            collected_at: 0,
            node_count: 3,
            critical: vec![],
            warnings: vec![],
            unknown: unknown_sources
                .iter()
                .map(|source| crate::relish::wtf::WtfUnknown {
                    source: source.to_string(),
                    reason: "403".into(),
                    affected_resource: "cluster".into(),
                })
                .collect(),
            ok: vec![],
            summary: crate::relish::wtf::WtfSummary {
                critical_count: 0,
                warning_count: 0,
                unknown_count: unknown_sources.len(),
                ok_count: 0,
            },
        }
    }

    /// The console's principal can't list tokens, so that check is left
    /// out rather than shown as unknown on every screen.
    #[test]
    fn checks_only_an_admin_can_run_are_not_counted_unknown() {
        assert_eq!(
            console_check(&report(&["tokens"])),
            WtfCheck::Checked {
                critical: 0,
                warning: 0,
                unknown: 0,
                first_critical: None
            }
        );
        assert!(matches!(
            console_check(&report(&["tokens", "gitops"])),
            WtfCheck::Checked { unknown: 1, .. }
        ));
    }

    #[test]
    fn an_unseeded_node_has_no_endpoint() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(read_endpoint(dir.path()), Err(EndpointError::NotSeeded));
    }
}
