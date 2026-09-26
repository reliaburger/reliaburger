/// HTTP client for talking to the Bun agent.
///
/// Sends requests to the Bun local API at `http://127.0.0.1:9117`.
/// Used by Relish CLI commands when a live agent is available.
///
/// The `apply` endpoint returns Server-Sent Events, which the client
/// reads incrementally — printing progress to stderr and collecting
/// the final result.
use futures_util::StreamExt;
use rustls::pki_types::{CertificateDer, pem::PemObject};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

use crate::bun::agent::{ApplyEvent, ApplyResult, CouncilStatus, InstanceStatus, NodeStatus};
use crate::config::Config;

use super::RelishError;

/// One API token as `GET /v1/token/list` describes it; never the secret.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
pub struct TokenSummary {
    /// Token name, as given to `relish token create`.
    pub name: String,
    /// Role the token grants.
    pub role: String,
    /// Creation time, Unix seconds.
    pub created_at: u64,
    /// Expiry, Unix seconds; `None` for a token that never expires.
    pub expires_at: Option<u64>,
}

/// Client for the Bun agent HTTP API.
#[derive(Clone)]
pub struct BunClient {
    base_url: String,
    client: Result<reqwest::Client, String>,
    websocket_tls: Option<std::sync::Arc<rustls::ClientConfig>>,
    token: Option<String>,
    ca_pem: Option<Vec<u8>>,
    service_endpoints: Option<crate::bun::capabilities::ServiceEndpoints>,
}

/// Options for fetching or streaming logs.
///
/// `grep` and `start` are also sent server-side where the endpoint
/// supports them; `json_field` is client-side only (there is no
/// server-side equivalent).
/// Outcome of an agent-side log export (`POST /v1/logs/export`).
#[derive(Debug, Clone, serde::Deserialize)]
pub struct LogsExportOutcome {
    /// Files newly shipped to the destination.
    pub files_exported: u64,
    /// Total bytes written.
    pub bytes_written: u64,
    /// The node name the agent filed the export under.
    pub node_id: String,
    /// False when the files landed but the export checkpoint could not be
    /// persisted — a later export may re-ship the same files.
    pub checkpoint_saved: bool,
}

/// Print one event of a followed log stream: lines to stdout (after the
/// client-side filters), warnings to stderr.
fn print_followed_event(event: &crate::ketchup::sse::SseEvent, options: &LogOptions) {
    if event.event.as_deref() == Some(crate::ketchup::sse::WARNING_EVENT) {
        eprintln!("warning: {}", event.data);
    } else if options.matches(&event.data) {
        println!("{}", event.data);
    }
}

#[derive(Debug, Clone, Default)]
pub struct LogOptions {
    pub tail: Option<usize>,
    pub follow: bool,
    /// Keep only lines containing this substring.
    pub grep: Option<String>,
    /// Keep only entries at or after this unix timestamp (seconds).
    pub start: Option<u64>,
    /// Keep only lines that parse as JSON where `field == value`.
    pub json_field: Option<(String, String)>,
}

impl LogOptions {
    /// Query parameters understood by the server-side log endpoints.
    fn query_params(&self) -> Vec<(String, String)> {
        let mut params = Vec::new();
        if let Some(n) = self.tail {
            params.push(("tail".to_string(), n.to_string()));
        }
        if let Some(ref g) = self.grep {
            params.push(("grep".to_string(), g.clone()));
        }
        if let Some(s) = self.start {
            params.push(("start".to_string(), s.to_string()));
        }
        params
    }

    /// Client-side line filter: substring grep plus JSON field match.
    fn matches(&self, line: &str) -> bool {
        if let Some(ref g) = self.grep
            && !line.contains(g.as_str())
        {
            return false;
        }
        if let Some((ref key, ref want)) = self.json_field {
            let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
                return false;
            };
            let Some(field) = value.get(key) else {
                return false;
            };
            // Compare string fields directly; render other JSON types
            // (numbers, bools) to text so `level=3` matches `"level": 3`.
            let found = match field.as_str() {
                Some(s) => s == want,
                None => &field.to_string() == want,
            };
            if !found {
                return false;
            }
        }
        true
    }
}

/// Render queried log entries as lines, oldest first.
///
/// When the lines come from more than one instance, each line starts with
/// `[instance] `. During a rolling deploy or after a restart an old and a new
/// instance both appear in a tail, and unlabelled their output reads as one
/// app skipping values.
fn render_log_entries(entries: &[crate::ketchup::types::LogEntry], options: &LogOptions) -> String {
    let shown: Vec<&crate::ketchup::types::LogEntry> = entries
        .iter()
        .filter(|entry| options.matches(&entry.line))
        .collect();
    let instances: std::collections::BTreeSet<Option<&str>> = shown
        .iter()
        .map(|entry| entry.instance.as_deref())
        .collect();
    let label = instances.len() > 1;
    let mut output = String::new();
    for entry in shown {
        if label {
            output.push_str(&format!("[{}] ", entry.instance.as_deref().unwrap_or("-")));
        }
        output.push_str(&entry.line);
        output.push('\n');
    }
    output.pop();
    output
}

/// Classify a reqwest send error as either a timeout or a connection failure.
fn classify_error(e: reqwest::Error) -> RelishError {
    if e.is_timeout() {
        RelishError::RequestTimeout
    } else {
        RelishError::AgentUnreachable
    }
}

async fn parse_typed_response<T: serde::de::DeserializeOwned>(
    response: reqwest::Response,
) -> Result<T, RelishError> {
    let status = response.status().as_u16();
    if !response.status().is_success() {
        let body = response.text().await.unwrap_or_default();
        return Err(RelishError::ApiError { status, body });
    }
    response
        .json()
        .await
        .map_err(|error| RelishError::ApiError {
            status: 0,
            body: format!("failed to parse response: {error}"),
        })
}

/// The `--token` CLI override, set once from `main` before any client is built.
static CLI_TOKEN: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();

/// The `--ca-cert` CLI override (path to the cluster CA PEM), set once from
/// `main`. When present, the CLI reaches the agent API over HTTPS trusting
/// this CA.
static CLI_CA_CERT: std::sync::OnceLock<Option<std::path::PathBuf>> = std::sync::OnceLock::new();

/// The `--endpoint` CLI override, set once from `main` before dispatch.
static CLI_ENDPOINT: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();

/// An invalid Bun API base URL supplied through Relish's connection options.
#[derive(Debug, thiserror::Error)]
pub enum EndpointError {
    /// The value isn't an absolute URL.
    #[error("invalid endpoint URL: {0}")]
    InvalidUrl(#[from] url::ParseError),
    /// The URL has no host component.
    #[error("endpoint URL is missing a host")]
    MissingHost,
    /// The URL embeds user-info.
    #[error("endpoint URL must not contain credentials")]
    Credentials,
    /// The URL includes components that can't be part of an API base URL.
    #[error("endpoint URL must not contain a query or fragment")]
    QueryOrFragment,
    /// A non-loopback endpoint selected plaintext HTTP.
    #[error("remote Bun endpoints must use HTTPS")]
    RemotePlaintext,
    /// The scheme isn't supported by the HTTP client.
    #[error("endpoint URL must use HTTP or HTTPS")]
    UnsupportedScheme,
}

/// Record the `--token` CLI flag. Call once, in `main`, before dispatch.
pub fn set_cli_token(token: Option<String>) {
    let _ = CLI_TOKEN.set(token);
}

/// Record the `--ca-cert` CLI flag. Call once, in `main`, before dispatch.
pub fn set_cli_ca_cert(path: Option<std::path::PathBuf>) {
    let _ = CLI_CA_CERT.set(path);
}

/// Record the API endpoint before dispatch, falling back to
/// `RELIABURGER_ENDPOINT` when the flag is absent. Remote plaintext and
/// credential-bearing URLs are rejected before a bearer token can be sent.
pub fn set_cli_endpoint(endpoint: Option<String>) -> Result<(), EndpointError> {
    let endpoint = pick_endpoint(Some(&endpoint), std::env::var("RELIABURGER_ENDPOINT").ok());
    if let Some(ref value) = endpoint {
        validate_endpoint(value)?;
    }
    let _ = CLI_ENDPOINT.set(endpoint);
    Ok(())
}

/// Resolve the CA cert path: `--ca-cert` flag, else `RELIABURGER_CA_CERT`.
fn resolve_ca_cert() -> Option<std::path::PathBuf> {
    match CLI_CA_CERT.get() {
        Some(Some(p)) => Some(p.clone()),
        _ => std::env::var_os("RELIABURGER_CA_CERT").map(std::path::PathBuf::from),
    }
}

/// Resolve the auth token: the `--token` flag takes precedence over the
/// `RELIABURGER_TOKEN` environment variable.
fn resolve_token() -> Option<String> {
    pick_token(CLI_TOKEN.get(), std::env::var("RELIABURGER_TOKEN").ok())
}

/// Resolve the Bun API endpoint: `--endpoint`, then
/// `RELIABURGER_ENDPOINT`, then the ordinary local default.
fn resolve_endpoint() -> Option<String> {
    CLI_ENDPOINT.get().cloned().flatten()
}

/// Precedence rule for [`resolve_token`], split out to be testable without the
/// process-global flag and environment. A present `--token` flag wins;
/// otherwise fall back to the environment value.
fn pick_token(cli_flag: Option<&Option<String>>, env: Option<String>) -> Option<String> {
    match cli_flag {
        Some(Some(t)) => Some(t.clone()),
        _ => env,
    }
}

/// Precedence rule for [`resolve_endpoint`], kept pure for unit tests.
fn pick_endpoint(cli_flag: Option<&Option<String>>, env: Option<String>) -> Option<String> {
    match cli_flag {
        Some(Some(endpoint)) => Some(endpoint.clone()),
        _ => env,
    }
}

/// Validate an operator-supplied Bun API base URL.
///
/// HTTPS is required off-host so bearer tokens and API responses never cross
/// a network in plaintext. IP-literal loopback HTTP remains available for
/// standalone development. User-info is rejected because embedding
/// credentials in URLs leaks them through shell history, process listings and
/// logs.
///
/// ```
/// use reliaburger::relish::client::{validate_endpoint, EndpointError};
///
/// validate_endpoint("https://bun.example:9117").expect("remote HTTPS endpoint");
/// validate_endpoint("http://127.0.0.1:9117").expect("local development endpoint");
/// assert!(matches!(
///     validate_endpoint("http://bun.example:9117"),
///     Err(EndpointError::RemotePlaintext)
/// ));
/// ```
///
/// This checks the URL only; it does not open a connection, authenticate a
/// caller or establish that the node is healthy.
pub fn validate_endpoint(value: &str) -> Result<(), EndpointError> {
    let parsed = url::Url::parse(value)?;
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(EndpointError::Credentials);
    }
    if parsed.query().is_some() || parsed.fragment().is_some() {
        return Err(EndpointError::QueryOrFragment);
    }
    match parsed.scheme() {
        "https" => Ok(()),
        "http" => {
            let host = parsed.host().ok_or(EndpointError::MissingHost)?;
            let loopback = match host {
                // Keep the bootstrap boundary free of DNS/hosts-file TOCTOU:
                // plaintext is for IP-literal loopback only.
                url::Host::Domain(_) => false,
                url::Host::Ipv4(address) => address.is_loopback(),
                url::Host::Ipv6(address) => address.is_loopback(),
            };
            if loopback {
                Ok(())
            } else {
                Err(EndpointError::RemotePlaintext)
            }
        }
        _ => Err(EndpointError::UnsupportedScheme),
    }
}

impl BunClient {
    /// Create a client pointing at the given base URL, attaching the resolved
    /// auth token (`--token` flag or `RELIABURGER_TOKEN`) to every request.
    pub fn new(base_url: &str) -> Self {
        Self::new_with_token(base_url, resolve_token().as_deref())
    }

    /// Create a client with an explicit token (or none). Used by tests; `new`
    /// resolves the token from the flag/env instead.
    pub fn new_with_token(base_url: &str, token: Option<&str>) -> Self {
        let ca_pem = match resolve_ca_cert().map(std::fs::read).transpose() {
            Ok(pem) => pem,
            Err(error) => {
                return Self {
                    base_url: base_url.trim_end_matches('/').to_string(),
                    client: Err(format!("failed to read cluster CA: {error}")),
                    websocket_tls: None,
                    ca_pem: None,
                    service_endpoints: None,
                    token: token.map(str::to_string),
                };
            }
        };
        Self::build(base_url, token, ca_pem.as_deref())
    }

    /// Create a client pinned to an explicit cluster CA, validating configuration
    /// before any request can send credentials.
    pub fn new_with_ca(
        base_url: &str,
        token: Option<&str>,
        ca_pem: &[u8],
    ) -> Result<Self, RelishError> {
        let client = Self::build(base_url, token, Some(ca_pem));
        client.http()?;
        Ok(client)
    }

    fn build(base_url: &str, token: Option<&str>, ca_pem: Option<&[u8]>) -> Self {
        let mut websocket_tls = None;
        let client = (|| -> Result<reqwest::Client, String> {
            let mut builder = reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(300))
                .redirect(reqwest::redirect::Policy::none());
            if let Some(token) = token {
                let mut headers = reqwest::header::HeaderMap::new();
                let mut value = reqwest::header::HeaderValue::from_str(&format!("Bearer {token}"))
                    .map_err(|_| "invalid bearer token header".to_string())?;
                value.set_sensitive(true);
                headers.insert(reqwest::header::AUTHORIZATION, value);
                builder = builder.default_headers(headers);
            }
            if let Some(pem) = ca_pem {
                let certificates = CertificateDer::pem_slice_iter(pem)
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(|error| format!("invalid cluster CA PEM: {error}"))?;
                if certificates.is_empty() {
                    return Err("cluster CA PEM contains no certificates".to_string());
                }
                let mut roots = rustls::RootCertStore::empty();
                for certificate in certificates {
                    roots
                        .add(certificate)
                        .map_err(|error| format!("invalid cluster CA certificate: {error}"))?;
                }
                let tls = super::tls::cluster_config(roots)?;
                builder = builder.use_preconfigured_tls((*tls).clone());
                websocket_tls = Some(tls);
            }
            builder
                .build()
                .map_err(|error| format!("failed to create HTTP client: {error}"))
        })();
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            client,
            websocket_tls,
            ca_pem: ca_pem.map(<[u8]>::to_vec),
            service_endpoints: None,
            token: token.map(str::to_string),
        }
    }

    /// Get the base URL.
    /// The scheme this client addresses the agent by, `"http"` or `"https"`.
    ///
    /// The Pickle registry is a second listener on the same host, and it
    /// gains TLS under the same condition the agent API does (the node
    /// holding an mTLS identity), so this is how the CLI knows whether to
    /// address the registry as https when uploading a build context (O2).
    pub fn scheme(&self) -> &'static str {
        if self.base_url.starts_with("https://") {
            "https"
        } else {
            "http"
        }
    }

    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Address another node with the same authenticated, CA-pinned HTTP
    /// client. Cluster fan-out must not silently lose the caller's bearer or
    /// replace its trust roots while changing only the authority.
    pub(crate) fn with_base_url(&self, base_url: &str) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            client: self.client.clone(),
            websocket_tls: self.websocket_tls.clone(),
            ca_pem: self.ca_pem.clone(),
            service_endpoints: None,
            token: self.token.clone(),
        }
    }

    /// Address one discovered node using its API endpoint and this client's identity.
    ///
    /// Gossip addresses and the entry node's port are not API-address evidence.
    pub fn for_node(&self, node: &NodeStatus) -> Result<Self, RelishError> {
        let address = node
            .api_address
            .filter(|address| address.port() != 0 && !address.ip().is_unspecified())
            .ok_or_else(|| RelishError::ApiError {
                status: 0,
                body: format!(
                    "node {} has no usable advertised API endpoint",
                    node.node_id
                ),
            })?;
        let endpoint = format!("{}://{address}", self.scheme());
        validate_endpoint(&endpoint).map_err(|error| RelishError::ApiError {
            status: 0,
            body: format!("node {} API endpoint is invalid: {error}", node.node_id),
        })?;
        Ok(self.with_base_url(&endpoint))
    }

    /// Address one node through this entry node's relay
    /// (`/v1/nodes/{node}/relay/...`), with this client's credential.
    ///
    /// The entry node reaches its peers on the cluster network even when the
    /// caller can't, as on a laptop behind Lima's user-mode network. The
    /// relay forwards only the per-node reads `wtf` and `path` need, and the
    /// target repeats every check against the caller's own credential.
    pub fn via_node(&self, node_id: &str) -> Result<Self, RelishError> {
        let valid = !node_id.is_empty()
            && node_id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"-_.".contains(&byte));
        if !valid {
            return Err(RelishError::ApiError {
                status: 0,
                body: format!("node name {node_id:?} can't be used in a relay path"),
            });
        }
        Ok(self.with_base_url(&format!("{}/v1/nodes/{node_id}/relay", self.base_url)))
    }

    /// Use another bearer credential with this connection's existing trust roots and forwards.
    pub fn with_token(&self, token: &str) -> Self {
        let mut client = Self::build(&self.base_url, Some(token), self.ca_pem.as_deref());
        client.service_endpoints = self.service_endpoints.clone();
        client
    }

    /// Declare the host forwards owned by this managed connection. Missing
    /// forwards remain unavailable instead of falling back to guest addresses.
    pub fn with_service_endpoints(
        mut self,
        endpoints: crate::bun::capabilities::ServiceEndpoints,
    ) -> Self {
        self.service_endpoints = Some(endpoints);
        self
    }

    /// Public trust anchors configured for this cluster client.
    pub(crate) fn cluster_ca_pem(&self) -> Option<&[u8]> {
        self.ca_pem.as_deref()
    }

    /// Build a separate workload client with normal hostname verification and
    /// the cluster CA, without the API bearer or client identity.
    pub fn workload_http_builder(&self) -> Result<reqwest::ClientBuilder, String> {
        self.http().map_err(|error| error.to_string())?;
        let mut builder = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(std::time::Duration::from_secs(3));
        if let Some(pem) = &self.ca_pem {
            for certificate in CertificateDer::pem_slice_iter(pem) {
                let certificate = certificate.map_err(|error| error.to_string())?;
                builder = builder.add_root_certificate(
                    reqwest::Certificate::from_der(&certificate)
                        .map_err(|error| error.to_string())?,
                );
            }
        }
        Ok(builder)
    }

    /// Address a declared Pickle endpoint with control-plane credentials. Remote
    /// plaintext and credentials embedded in URLs are refused before sending.
    pub fn registry_http_client(&self, endpoint: &str) -> Result<reqwest::Client, String> {
        validate_endpoint(endpoint).map_err(|error| error.to_string())?;
        let client = Self::build(endpoint, self.token.as_deref(), self.ca_pem.as_deref());
        client.http().cloned().map_err(|error| error.to_string())
    }

    /// The underlying HTTP client, pre-configured with the resolved bearer
    /// token and cluster-CA trust. Used for requests to an explicit URL that
    /// isn't relative to `base_url` — e.g. a Pickle registry `/v2` upload on a
    /// different port (M21) — so they, too, are authenticated and TLS-trusting.
    /// Invalid client configuration returns an error before any request is sent.
    pub fn http(&self) -> Result<&reqwest::Client, RelishError> {
        self.client
            .as_ref()
            .map_err(|reason| RelishError::ApiError {
                status: 0,
                body: reason.clone(),
            })
    }

    /// Create a client pointing at the default local agent. Uses HTTPS when a
    /// cluster CA cert is configured (`--ca-cert` / `RELIABURGER_CA_CERT`). An
    /// explicit `--endpoint` / `RELIABURGER_ENDPOINT` replaces the whole URL
    /// and bypasses saved context credentials. Otherwise a managed context takes
    /// precedence over the ordinary localhost default.
    pub fn default_local() -> Self {
        if let Some(endpoint) = resolve_endpoint() {
            let client = Self::new(&endpoint);
            // An explicit endpoint picks the node; the managed context's host
            // forwards still describe how this host reaches the cluster's
            // registry and ingress, when it is the same cluster.
            let forwards = super::local_context::default_path()
                .and_then(|path| super::local_context::LocalContext::load(&path))
                .ok()
                .flatten()
                .and_then(|context| context.forwards_for_ca(client.ca_pem.as_deref()));
            return match forwards {
                Some(forwards) => client.with_service_endpoints(forwards),
                None => client,
            };
        }
        let context = super::local_context::default_path()
            .and_then(|path| super::local_context::LocalContext::load(&path));
        match context {
            Ok(Some(context)) => {
                return context
                    .client(resolve_token().as_deref(), resolve_ca_cert().as_deref())
                    .unwrap_or_else(|error| Self {
                        base_url: context.endpoint,
                        client: Err(error.to_string()),
                        websocket_tls: None,
                        ca_pem: None,
                        service_endpoints: None,
                        token: None,
                    });
            }
            Err(error) => {
                return Self {
                    base_url: "https://127.0.0.1:19117".to_string(),
                    client: Err(error.to_string()),
                    websocket_tls: None,
                    ca_pem: None,
                    service_endpoints: None,
                    token: None,
                };
            }
            Ok(None) => {}
        }
        let scheme = if resolve_ca_cert().is_some() {
            "https"
        } else {
            "http"
        };
        Self::new(&format!("{scheme}://127.0.0.1:9117"))
    }

    /// Check if the agent is reachable.
    ///
    /// Uses a short timeout (5 seconds) — if the health endpoint
    /// doesn't respond quickly, the agent is effectively unreachable.
    pub async fn health(&self) -> Result<(), RelishError> {
        let url = format!("{}/v1/health", self.base_url);
        let mut response = self
            .http()?
            .get(&url)
            .timeout(std::time::Duration::from_secs(5))
            .send()
            .await
            .map_err(|_| RelishError::AgentUnreachable)?;
        let status = response.status().as_u16();
        if !response.status().is_success() {
            return Err(RelishError::ApiError {
                status,
                body: "bun liveness check failed".to_string(),
            });
        }
        // Keep an unrelated service's response from exhausting the CLI's memory.
        let mut body = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(classify_error)? {
            if body.len().saturating_add(chunk.len()) > 4096 {
                return Err(RelishError::ApiError {
                    status,
                    body: "invalid bun liveness response".to_string(),
                });
            }
            body.extend_from_slice(&chunk);
        }
        let json: serde_json::Value =
            serde_json::from_slice(&body).map_err(|_| RelishError::ApiError {
                status,
                body: "invalid bun liveness response".to_string(),
            })?;
        if json["status"] != "ok" {
            return Err(RelishError::ApiError {
                status,
                body: "bun is not live".to_string(),
            });
        }
        Ok(())
    }

    /// Read authenticated critical-subsystem readiness; liveness alone is insufficient.
    pub async fn readiness(
        &self,
    ) -> Result<crate::bun::readiness::NodeReadinessEvidence, RelishError> {
        self.get_typed_json("/v1/readiness").await
    }

    /// Deploy workloads from a config, streaming progress to stderr.
    ///
    /// The agent returns Server-Sent Events. Each `data:` line
    /// contains a JSON `ApplyEvent`. Progress events are printed to
    /// stderr as they arrive; the final `Complete` event is returned
    /// as an `ApplyResult`.
    pub async fn apply(&self, config: &Config) -> Result<ApplyResult, RelishError> {
        self.apply_request(config, None, false, false).await
    }

    /// Deploy apps under a server-owned Phase 15 resource lease.
    pub async fn apply_with_lease(
        &self,
        config: &Config,
        lease_id: &str,
    ) -> Result<ApplyResult, RelishError> {
        self.apply_request(config, Some(lease_id), false, false)
            .await
    }

    /// Deploy a deliberately saturating app under both lease and capacity policy.
    pub async fn apply_capacity_with_lease(
        &self,
        config: &Config,
        lease_id: &str,
    ) -> Result<ApplyResult, RelishError> {
        self.apply_request(config, Some(lease_id), true, false)
            .await
    }

    /// Explicitly rerun unknown jobs on this node after retiring their old runtime.
    pub async fn apply_rerunning_jobs(&self, config: &Config) -> Result<ApplyResult, RelishError> {
        crate::bun::jobs::validate_rerun(config).map_err(|error| RelishError::ApiError {
            status: 400,
            body: error.into(),
        })?;
        self.apply_request(config, None, false, true).await
    }

    async fn apply_request(
        &self,
        config: &Config,
        lease_id: Option<&str>,
        capacity_probe: bool,
        rerun_jobs: bool,
    ) -> Result<ApplyResult, RelishError> {
        let url = format!("{}/v1/apply", self.base_url);
        let toml_str = toml::to_string_pretty(config).map_err(|e| RelishError::ApiError {
            status: 0,
            body: format!("failed to serialise config: {e}"),
        })?;

        let mut request = self.http()?.post(&url).body(toml_str);
        if rerun_jobs {
            request = request.header("x-reliaburger-rerun-jobs", "acknowledged");
        }
        if let Some(lease_id) = lease_id {
            request = request.header("x-reliaburger-test-lease", lease_id);
        }
        if capacity_probe {
            request = request.header("x-reliaburger-capacity-probe", "acknowledged");
        }
        let response = request.send().await.map_err(classify_error)?;

        let status = response.status().as_u16();
        if !response.status().is_success() {
            let body = response.text().await.unwrap_or_default();
            if capacity_probe
                && status == 422
                && let Ok(refusal) =
                    serde_json::from_str::<crate::cluster::capacity::SchedulingRefusal>(&body)
            {
                return Err(RelishError::SchedulingRejected(refusal.error));
            }
            return Err(RelishError::ApiError { status, body });
        }

        // Read the SSE stream
        let mut stream = response.bytes_stream();
        let mut buffer = Vec::new();
        let mut result = None;

        while let Some(chunk) = stream.next().await {
            let bytes = chunk.map_err(classify_error)?;
            buffer.extend_from_slice(&bytes);

            // Decode only complete frames: a UTF-8 character can span network chunks.
            while let Some(event_end) = buffer.windows(2).position(|pair| pair == b"\n\n") {
                let event_text = String::from_utf8_lossy(&buffer[..event_end]).into_owned();
                buffer.drain(..event_end + 2);

                if let Some(data) = event_text
                    .lines()
                    .find_map(|line| line.strip_prefix("data:"))
                    && let Ok(event) = serde_json::from_str::<ApplyEvent>(data.trim())
                {
                    match &event {
                        ApplyEvent::Accepted { operation_id } => {
                            eprintln!("  operation {operation_id}");
                        }
                        ApplyEvent::Progress { message } => {
                            eprintln!("  {message}");
                        }
                        ApplyEvent::InstanceCreated { id, app } => {
                            eprintln!("  created {id} ({app})");
                        }
                        ApplyEvent::Complete { created, instances } => {
                            result = Some(ApplyResult {
                                created: *created,
                                instances: instances.clone(),
                            });
                        }
                        ApplyEvent::Error { message } => {
                            return Err(RelishError::ApiError {
                                status: 500,
                                body: message.clone(),
                            });
                        }
                    }
                }
            }
        }

        // Check for any remaining data in the buffer
        if let Some(data) = String::from_utf8_lossy(&buffer)
            .lines()
            .find_map(|line| line.strip_prefix("data:"))
            && let Ok(event) = serde_json::from_str::<ApplyEvent>(data.trim())
        {
            match event {
                ApplyEvent::Complete { created, instances } => {
                    result = Some(ApplyResult { created, instances });
                }
                ApplyEvent::Error { message } => {
                    return Err(RelishError::ApiError {
                        status: 500,
                        body: message,
                    });
                }
                _ => {}
            }
        }

        result.ok_or_else(|| RelishError::ApiError {
            status: 0,
            body: "stream ended without a Complete event".to_string(),
        })
    }

    /// Roll an app back to its previous successful spec (X3).
    ///
    /// The server streams deploy progress as SSE; we drain it, surfacing
    /// any error, and return once the stream closes.
    pub async fn rollback(&self, app: &str, namespace: &str) -> Result<(), RelishError> {
        let url = format!("{}/v1/rollback/{app}/{namespace}", self.base_url);
        let response = self
            .http()?
            .post(&url)
            .send()
            .await
            .map_err(classify_error)?;

        let status = response.status().as_u16();
        if !response.status().is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(RelishError::ApiError { status, body });
        }

        let mut stream = response.bytes_stream();
        let mut buffer = Vec::new();
        while let Some(chunk) = stream.next().await {
            let bytes = chunk.map_err(classify_error)?;
            buffer.extend_from_slice(&bytes);
            while let Some(end) = buffer.windows(2).position(|pair| pair == b"\n\n") {
                let event_text = String::from_utf8_lossy(&buffer[..end]).into_owned();
                buffer.drain(..end + 2);
                if let Some(data) = event_text.lines().find_map(|l| l.strip_prefix("data:"))
                    && let Ok(event) = serde_json::from_str::<ApplyEvent>(data.trim())
                {
                    match event {
                        ApplyEvent::Progress { message } => eprintln!("  {message}"),
                        ApplyEvent::Error { message } => {
                            return Err(RelishError::ApiError {
                                status: 500,
                                body: message,
                            });
                        }
                        _ => {}
                    }
                }
            }
        }
        Ok(())
    }

    /// Read every reachable cluster member, failing if the result is incomplete.
    pub async fn cluster_status(
        &self,
    ) -> Result<Vec<crate::bun::agent::ClusterInstanceStatus>, RelishError> {
        self.get_typed_json("/v1/status?cluster=true").await
    }

    /// Get status of all instances.
    pub async fn status(&self) -> Result<Vec<InstanceStatus>, RelishError> {
        let url = format!("{}/v1/status", self.base_url);
        let response = self
            .http()?
            .get(&url)
            .send()
            .await
            .map_err(classify_error)?;

        let status = response.status().as_u16();
        if !response.status().is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(RelishError::ApiError { status, body });
        }

        let statuses: Vec<InstanceStatus> =
            response.json().await.map_err(|e| RelishError::ApiError {
                status: 0,
                body: format!("failed to parse response: {e}"),
            })?;

        Ok(statuses)
    }

    /// List validated alert statuses; missing or malformed evidence is an error.
    pub async fn alerts(&self) -> Result<Vec<crate::mayo::alert::AlertStatus>, RelishError> {
        let response: crate::mayo::alert::AlertsResponse =
            self.get_typed_json("/v1/alerts").await?;
        Ok(response.alerts)
    }

    /// List run-to-completion workload instances.
    pub async fn jobs(&self) -> Result<Vec<crate::bun::agent::JobStatus>, RelishError> {
        self.get_typed_json("/v1/jobs").await
    }

    /// Fetch recent cluster events.
    pub async fn events(
        &self,
        limit: usize,
    ) -> Result<Vec<crate::bun::events::ClusterEvent>, RelishError> {
        let value: serde_json::Value = self
            .get_typed_json(&format!("/v1/events?limit={limit}"))
            .await?;
        serde_json::from_value(value["events"].clone()).map_err(|error| RelishError::ApiError {
            status: 0,
            body: format!("failed to parse events response: {error}"),
        })
    }

    /// Ask a node what it has wired up (Phase 15).
    ///
    /// The test runner, `wtf` and `bench` all consult this before deciding
    /// whether a check is meaningful, so that an absent subsystem reports as
    /// skipped rather than as a mysterious failure.
    pub async fn capabilities(
        &self,
    ) -> Result<crate::bun::capabilities::ClusterCapabilities, RelishError> {
        let mut report: crate::bun::capabilities::ClusterCapabilities =
            self.get_typed_json("/v1/capabilities").await?;
        if let Some(endpoints) = &self.service_endpoints {
            report.service_endpoints = endpoints.clone();
        }
        Ok(report)
    }

    /// The registry origin this managed connection declares (a quickstart
    /// host forward such as `https://127.0.0.1:15050`), if any.
    pub fn declared_registry(&self) -> Option<&str> {
        self.service_endpoints.as_ref()?.registry.as_deref()
    }

    /// The node's own capability report, *without* substituting this
    /// connection's declared forwards: the listeners as the node sees them.
    pub async fn capabilities_as_reported(
        &self,
    ) -> Result<crate::bun::capabilities::ClusterCapabilities, RelishError> {
        self.get_typed_json("/v1/capabilities").await
    }

    /// Fetch an authenticated, bounded collection from current cluster peers.
    pub async fn cluster_capabilities(
        &self,
    ) -> Result<crate::bun::capabilities::ClusterCapabilityReport, RelishError> {
        self.get_typed_json("/v1/capabilities/cluster").await
    }

    /// Fetch bounded local disk, cgroup and public certificate evidence.
    pub async fn diagnostics(
        &self,
        cpu_window_seconds: u64,
    ) -> Result<crate::bun::diagnostics::LocalDiagnosticSnapshot, RelishError> {
        self.get_typed_json(&format!(
            "/v1/diagnostics?window_seconds={}",
            cpu_window_seconds.clamp(1, 10)
        ))
        .await
    }

    /// Run the fixed server-side path probe on the node hosting the source
    /// workload.
    pub async fn probe_path(
        &self,
        request: &crate::onion::trace::TraceRequest,
    ) -> Result<crate::onion::trace::TraceResult, RelishError> {
        let response = self
            .http()?
            .post(format!("{}/v1/path", self.base_url))
            .json(request)
            .send()
            .await
            .map_err(classify_error)?;
        parse_typed_response(response).await
    }

    /// Fetch desired application replicas and current scheduler coverage.
    pub async fn desired_apps(
        &self,
    ) -> Result<Vec<crate::bun::diagnostics::DesiredAppEvidence>, RelishError> {
        self.get_typed_json("/v1/diagnostics/apps").await
    }

    /// Fetch the currently deployed resources in plan format
    /// (`GET /v1/apps`), for `--dry-run` diffing.
    pub async fn current_resources(
        &self,
    ) -> Result<Vec<crate::relish::plan::CurrentResource>, RelishError> {
        let rows: Vec<crate::bun::agent::CurrentResourceStatus> =
            self.get_typed_json("/v1/apps").await?;
        Ok(rows
            .into_iter()
            .map(|row| crate::relish::plan::CurrentResource {
                resource: row.resource,
                image: row.image,
            })
            .collect())
    }

    /// Trigger an immediate log export on the agent (`POST /v1/logs/export`).
    ///
    /// The destination is resolved agent-side — a path on the agent host,
    /// `file://`, `s3://` or `gs://` — and the agent names the subdirectory
    /// after its own node name.
    pub async fn logs_export(&self, destination: &str) -> Result<LogsExportOutcome, RelishError> {
        let response = self
            .http()?
            .post(format!("{}/v1/logs/export", self.base_url))
            .json(&serde_json::json!({ "destination": destination }))
            .send()
            .await
            .map_err(classify_error)?;
        parse_typed_response(response).await
    }

    /// Fetch structured, cluster-aware recent log entries for one application.
    pub async fn log_entries(
        &self,
        app: &str,
        namespace: &str,
        tail: usize,
        start: u64,
    ) -> Result<crate::ketchup::types::LogQueryResult, RelishError> {
        self.get_typed_json(&format!(
            "/v1/logs/query/{app}/{namespace}?tail={tail}&start={start}"
        ))
        .await
    }

    /// Fetch deploy history for an app in a namespace.
    pub async fn deploy_history(
        &self,
        app: &str,
        namespace: &str,
    ) -> Result<Vec<serde_json::Value>, RelishError> {
        // Encode both: an app or namespace carrying `/`, `&` or a space would
        // otherwise split into extra path segments or query parameters.
        let app_segment: String = url::form_urlencoded::byte_serialize(app.as_bytes()).collect();
        let namespace_value: String =
            url::form_urlencoded::byte_serialize(namespace.as_bytes()).collect();
        let value: serde_json::Value = self
            .get_typed_json(&format!(
                "/v1/deploys/history/{app_segment}?namespace={namespace_value}"
            ))
            .await?;
        Ok(value["history"].as_array().cloned().unwrap_or_default())
    }

    /// Fetch live deploy operations and bounded terminal history.
    pub async fn deploy_operations(
        &self,
    ) -> Result<crate::bun::deploy_operations::DeployOperationSnapshot, RelishError> {
        self.get_typed_json("/v1/deploys/operations").await
    }

    /// Request cancellation of a node-local operation; admission is not completion.
    pub async fn cancel_deploy(
        &self,
        operation_id: &str,
    ) -> Result<crate::bun::deploy_operations::DeployOperation, RelishError> {
        let segment: String =
            url::form_urlencoded::byte_serialize(operation_id.as_bytes()).collect();
        let response = self
            .http()?
            .post(format!(
                "{}/v1/deploys/operations/{segment}/cancel",
                self.base_url
            ))
            .send()
            .await
            .map_err(classify_error)?;
        parse_typed_response(response).await
    }

    /// Create a server-owned test resource lease.
    pub async fn create_test_lease(
        &self,
        ttl_seconds: u64,
        namespace: Option<&str>,
    ) -> Result<crate::testkit::lease::TestLease, RelishError> {
        let response = self
            .http()?
            .post(format!("{}/v1/test/leases", self.base_url))
            .json(&serde_json::json!({
                "ttl_seconds": ttl_seconds,
                "namespace": namespace,
            }))
            .send()
            .await
            .map_err(classify_error)?;
        parse_typed_response(response).await
    }

    /// Create a durable job lease on this exact node, even in a cluster.
    pub async fn create_node_job_lease(
        &self,
        ttl_seconds: u64,
    ) -> Result<crate::testkit::lease::TestLease, RelishError> {
        let response = self
            .http()?
            .post(format!("{}/v1/test/leases", self.base_url))
            .json(&serde_json::json!({"ttl_seconds": ttl_seconds, "scope": "node_jobs"}))
            .send()
            .await
            .map_err(classify_error)?;
        parse_typed_response(response).await
    }

    /// Renew an active test resource lease as its authenticated owner.
    ///
    /// The node-local job recovery integration test uses this endpoint. The
    /// catalogue runner instead requests a lease covering its bounded case
    /// deadline plus teardown; it does not spawn a background renewal loop.
    pub async fn renew_test_lease(
        &self,
        lease_id: &str,
        ttl_seconds: u64,
    ) -> Result<crate::testkit::lease::TestLease, RelishError> {
        let response = self
            .http()?
            .post(format!("{}/v1/test/leases/{lease_id}/renew", self.base_url))
            .json(&serde_json::json!({ "ttl_seconds": ttl_seconds }))
            .send()
            .await
            .map_err(classify_error)?;
        parse_typed_response(response).await
    }

    /// Release a lease and wait up to 30 seconds for server-confirmed cleanup.
    /// An accepted request keeps polling durable ownership until it disappears.
    /// Transient leader unavailability retries within the same overall deadline.
    pub async fn release_test_lease(&self, lease_id: &str) -> Result<(), RelishError> {
        tokio::time::timeout(std::time::Duration::from_secs(30), async {
            let url = format!("{}/v1/test/leases/{lease_id}", self.base_url);
            let response = loop {
                let response = self
                    .http()?
                    .delete(&url)
                    .send()
                    .await
                    .map_err(classify_error)?;
                if response.status() != reqwest::StatusCode::SERVICE_UNAVAILABLE {
                    break response;
                }
                // Retrying this idempotent mutation preserves its original lease.
                drop(response);
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            };
            match response.status() {
                reqwest::StatusCode::NO_CONTENT => return Ok(()),
                reqwest::StatusCode::ACCEPTED => {}
                status => {
                    return Err(RelishError::ApiError {
                        status: status.as_u16(),
                        body: response.text().await.unwrap_or_default(),
                    });
                }
            }
            loop {
                let response = self
                    .http()?
                    .get(&url)
                    .send()
                    .await
                    .map_err(classify_error)?;
                match response.status() {
                    reqwest::StatusCode::NOT_FOUND => return Ok(()),
                    reqwest::StatusCode::OK | reqwest::StatusCode::SERVICE_UNAVAILABLE => {}
                    status => {
                        return Err(RelishError::ApiError {
                            status: status.as_u16(),
                            body: response.text().await.unwrap_or_default(),
                        });
                    }
                }
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        })
        .await
        .map_err(|_| RelishError::RequestTimeout)?
    }

    /// Fetch metrics recorded for one app.
    pub async fn app_metrics(
        &self,
        app: &str,
        namespace: &str,
    ) -> Result<crate::mayo::rollup::MetricsQueryResult, RelishError> {
        self.get_typed_json(&format!("/v1/metrics/app/{app}/{namespace}"))
            .await
    }

    /// Fetch one app's metrics from `start` (unix seconds) on, optionally
    /// one metric by name and only the newest `per_series` samples of each
    /// series. The node answering fans out to every node running the app.
    pub async fn app_metrics_since(
        &self,
        app: &str,
        namespace: &str,
        name: Option<&str>,
        start: u64,
        per_series: Option<u32>,
    ) -> Result<crate::mayo::rollup::MetricsQueryResult, RelishError> {
        let mut query = url::form_urlencoded::Serializer::new(String::new());
        query.append_pair("start", &start.to_string());
        if let Some(name) = name {
            query.append_pair("name", name);
        }
        if let Some(per_series) = per_series {
            query.append_pair("per_series", &per_series.to_string());
        }
        self.get_typed_json(&format!(
            "/v1/metrics/app/{app}/{namespace}?{}",
            query.finish()
        ))
        .await
    }

    async fn get_typed_json<T: serde::de::DeserializeOwned>(
        &self,
        path: &str,
    ) -> Result<T, RelishError> {
        let response = self
            .http()?
            .get(format!("{}{}", self.base_url, path))
            .send()
            .await
            .map_err(classify_error)?;
        let status = response.status().as_u16();
        if !response.status().is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(RelishError::ApiError { status, body });
        }
        response
            .json()
            .await
            .map_err(|error| RelishError::ApiError {
                status: 0,
                body: format!("failed to parse response: {error}"),
            })
    }

    /// Open an authenticated WebSocket to an agent path.
    pub async fn ws_connect(
        &self,
        path_and_query: &str,
    ) -> Result<
        tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
        RelishError,
    > {
        self.http()?;
        let scheme = if self.base_url.starts_with("https://") {
            "wss://"
        } else {
            "ws://"
        };
        let authority = self
            .base_url
            .split_once("://")
            .map(|(_, rest)| rest)
            .unwrap_or(&self.base_url);
        let url = format!("{scheme}{authority}{path_and_query}");
        let mut request = url
            .into_client_request()
            .map_err(|error| RelishError::WebSocket(error.to_string()))?;
        if let Some(token) = &self.token {
            let value = format!("Bearer {token}")
                .parse()
                .map_err(|_| RelishError::WebSocket("bad token".to_string()))?;
            request.headers_mut().insert("Authorization", value);
        }
        let _ = rustls::crypto::ring::default_provider().install_default();
        let connector = self
            .websocket_tls
            .clone()
            .map(tokio_tungstenite::Connector::Rustls);
        let (stream, _) = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            tokio_tungstenite::connect_async_tls_with_config(request, None, false, connector),
        )
        .await
        .map_err(|_| RelishError::WebSocket("connection timed out".to_string()))?
        .map_err(|error| RelishError::WebSocket(error.to_string()))?;
        Ok(stream)
    }

    /// Follow app logs over WebSocket.
    pub async fn ws_logs(
        &self,
        app: &str,
        namespace: &str,
        tail: usize,
    ) -> Result<
        tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
        RelishError,
    > {
        self.ws_connect(&format!("/v1/ws/logs/{app}/{namespace}?tail={tail}"))
            .await
    }

    /// Follow cluster events over WebSocket.
    pub async fn ws_events(
        &self,
    ) -> Result<
        tokio_tungstenite::WebSocketStream<
            tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
        >,
        RelishError,
    > {
        self.ws_connect("/v1/ws/events").await
    }

    /// Stop an app.
    pub async fn stop(&self, app: &str, namespace: &str) -> Result<(), RelishError> {
        let url = format!("{}/v1/stop/{}/{}", self.base_url, app, namespace);
        let response = self
            .http()?
            .post(&url)
            .send()
            .await
            .map_err(classify_error)?;

        let status = response.status().as_u16();
        if !response.status().is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(RelishError::ApiError { status, body });
        }

        Ok(())
    }

    /// Remove an app from the cluster (`POST /v1/delete/{app}/{namespace}`).
    pub async fn delete(&self, app: &str, namespace: &str) -> Result<(), RelishError> {
        let url = format!("{}/v1/delete/{}/{}", self.base_url, app, namespace);
        let response = self
            .http()?
            .post(&url)
            .send()
            .await
            .map_err(classify_error)?;

        let status = response.status().as_u16();
        if !response.status().is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(RelishError::ApiError { status, body });
        }

        Ok(())
    }

    /// Snapshot an app's managed volumes; returns the created
    /// snapshots' metadata.
    pub async fn snapshot_create(
        &self,
        app: &str,
        namespace: &str,
        volume: Option<&str>,
        name: Option<&str>,
    ) -> Result<serde_json::Value, RelishError> {
        let url = format!("{}/v1/snapshots/{}/{}", self.base_url, namespace, app);
        let body = serde_json::json!({ "volume": volume, "name": name });
        let response = self
            .http()?
            .post(&url)
            .json(&body)
            .send()
            .await
            .map_err(classify_error)?;

        let status = response.status().as_u16();
        if !response.status().is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(RelishError::ApiError { status, body });
        }
        response.json().await.map_err(classify_error)
    }

    /// List an app's snapshots, newest first.
    pub async fn snapshot_list(
        &self,
        app: &str,
        namespace: &str,
    ) -> Result<serde_json::Value, RelishError> {
        let url = format!("{}/v1/snapshots/{}/{}", self.base_url, namespace, app);
        let response = self
            .http()?
            .get(&url)
            .send()
            .await
            .map_err(classify_error)?;

        let status = response.status().as_u16();
        if !response.status().is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(RelishError::ApiError { status, body });
        }
        response.json().await.map_err(classify_error)
    }

    /// Restore a snapshot over its live volume. The app must be
    /// stopped first; a 409 means it isn't.
    pub async fn snapshot_restore(
        &self,
        app: &str,
        namespace: &str,
        name: &str,
    ) -> Result<(), RelishError> {
        let url = format!(
            "{}/v1/snapshots/{}/{}/restore",
            self.base_url, namespace, app
        );
        let response = self
            .http()?
            .post(&url)
            .json(&serde_json::json!({ "name": name }))
            .send()
            .await
            .map_err(classify_error)?;

        let status = response.status().as_u16();
        if !response.status().is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(RelishError::ApiError { status, body });
        }
        Ok(())
    }

    /// Delete a snapshot.
    pub async fn snapshot_delete(
        &self,
        app: &str,
        namespace: &str,
        name: &str,
    ) -> Result<(), RelishError> {
        let url = format!(
            "{}/v1/snapshots/{}/{}/{}",
            self.base_url, namespace, app, name
        );
        let response = self
            .http()?
            .delete(&url)
            .send()
            .await
            .map_err(classify_error)?;

        let status = response.status().as_u16();
        if !response.status().is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(RelishError::ApiError { status, body });
        }
        Ok(())
    }

    /// Get logs for an app.
    ///
    /// When `follow` is false, returns the (optionally tailed) log output
    /// as a string. When `follow` is true, streams log lines to stdout
    /// via SSE and returns `Ok(String::new())` when the stream ends.
    pub async fn logs(
        &self,
        app: &str,
        namespace: &str,
        options: &LogOptions,
    ) -> Result<String, RelishError> {
        if options.follow {
            // Follow mode uses the SSE endpoint, which fans out across the
            // cluster but does not filter server-side, so filters apply here.
            return self.logs_follow(app, namespace, options).await;
        }

        // Non-follow: try the cross-node query endpoint first.
        // In cluster mode this fans out to all nodes running the app.
        // In single-node mode it queries the local LogStore.
        let url = format!("{}/v1/logs/query/{}/{}", self.base_url, app, namespace);

        if let Ok(response) = self
            .http()?
            .get(&url)
            .query(&options.query_params())
            .send()
            .await
            && response.status().is_success()
            && let Ok(result) = response
                .json::<crate::ketchup::types::LogQueryResult>()
                .await
        {
            // Filters also apply client-side: json_field has no server-side
            // equivalent, and grep re-checking is harmless when the server
            // already filtered.
            let output = render_log_entries(&result.entries, options);

            // Show warnings if any nodes were unreachable
            for warning in &result.warnings {
                match warning {
                    crate::ketchup::types::LogQueryWarning::NodeUnresponsive { node_id } => {
                        eprintln!("warning: node {node_id} did not respond");
                    }
                }
            }

            // If we got entries, return them
            if !output.is_empty() {
                return Ok(output);
            }
        }

        // Fall back to the local agent endpoint (process logs that
        // haven't been ingested into the LogStore yet)
        self.logs_local(app, namespace, options).await
    }

    /// Query local agent logs (process stdout/stderr).
    async fn logs_local(
        &self,
        app: &str,
        namespace: &str,
        options: &LogOptions,
    ) -> Result<String, RelishError> {
        let url = format!("{}/v1/logs/{}/{}", self.base_url, app, namespace);

        let response = self
            .http()?
            .get(&url)
            .query(&options.query_params())
            .send()
            .await
            .map_err(classify_error)?;

        let status = response.status().as_u16();
        if !response.status().is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(RelishError::ApiError { status, body });
        }

        let json: serde_json::Value = response.json().await.map_err(|e| RelishError::ApiError {
            status: 0,
            body: format!("failed to parse response: {e}"),
        })?;

        let logs = json["logs"].as_str().unwrap_or("");
        let filtered: Vec<&str> = logs.lines().filter(|l| options.matches(l)).collect();
        Ok(filtered.join("\n"))
    }

    /// Follow logs via the SSE stream. On a cluster the node follows every
    /// node that runs the app and prefixes each line with `[node instance]`;
    /// a node dropping out arrives as a warning on stderr and the stream
    /// carries on.
    async fn logs_follow(
        &self,
        app: &str,
        namespace: &str,
        options: &LogOptions,
    ) -> Result<String, RelishError> {
        let url = format!("{}/v1/logs/{}/{}", self.base_url, app, namespace);
        let mut params = options.query_params();
        params.push(("follow".to_string(), "true".to_string()));

        let response = self
            .http()?
            .get(&url)
            .query(&params)
            .send()
            .await
            .map_err(classify_error)?;

        let status = response.status().as_u16();
        if !response.status().is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(RelishError::ApiError { status, body });
        }

        let mut stream = response.bytes_stream();
        let mut decoder = crate::ketchup::sse::SseDecoder::default();
        while let Some(chunk) = stream.next().await {
            let bytes = chunk.map_err(classify_error)?;
            for event in decoder.push(&bytes) {
                print_followed_event(&event, options);
            }
        }
        if let Some(event) = decoder.finish() {
            print_followed_event(&event, options);
        }

        Ok(String::new())
    }

    /// Execute a command inside a running instance.
    pub async fn exec(
        &self,
        app: &str,
        namespace: &str,
        command: &[String],
    ) -> Result<String, RelishError> {
        let url = format!("{}/v1/exec/{}/{}", self.base_url, app, namespace);
        let response = self
            .http()?
            .post(&url)
            .json(&serde_json::json!({ "command": command }))
            .send()
            .await
            .map_err(classify_error)?;

        let status = response.status().as_u16();
        if !response.status().is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(RelishError::ApiError { status, body });
        }

        let json: serde_json::Value = response.json().await.map_err(|e| RelishError::ApiError {
            status: 0,
            body: format!("failed to parse response: {e}"),
        })?;

        Ok(json["output"].as_str().unwrap_or("").to_string())
    }

    /// Permanently retire a node after explicit external workload fencing.
    pub async fn decommission_node(
        &self,
        request: &crate::cluster::retirement::DecommissionRequest,
    ) -> Result<crate::cluster::retirement::NodeRetirement, RelishError> {
        request.validate().map_err(|reason| RelishError::ApiError {
            status: 0,
            body: reason.into(),
        })?;
        let body = serde_json::to_string(request).map_err(|error| RelishError::ApiError {
            status: 0,
            body: error.to_string(),
        })?;
        let response = self.post_json("/v1/nodes/decommission", body).await?;
        let retirement: crate::cluster::retirement::NodeRetirement =
            serde_json::from_value(response).map_err(|error| RelishError::ApiError {
                status: 0,
                body: format!("invalid decommission response: {error}"),
            })?;
        if retirement.node_id != request.node_id
            || retirement.retired_by.is_empty()
            || retirement.retired_at_unix_ms == 0
        {
            return Err(RelishError::ApiError {
                status: 0,
                body: "decommission response does not confirm this identity".into(),
            });
        }
        Ok(retirement)
    }

    /// Get cluster node membership.
    pub async fn nodes(&self) -> Result<Vec<NodeStatus>, RelishError> {
        let url = format!("{}/v1/cluster/nodes", self.base_url);
        let response = self
            .http()?
            .get(&url)
            .send()
            .await
            .map_err(classify_error)?;

        let status = response.status().as_u16();
        if !response.status().is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(RelishError::ApiError { status, body });
        }

        let nodes: Vec<NodeStatus> = response.json().await.map_err(|e| RelishError::ApiError {
            status: 0,
            body: format!("failed to parse response: {e}"),
        })?;

        Ok(nodes)
    }

    /// Get council (Raft) status.
    pub async fn council(&self) -> Result<CouncilStatus, RelishError> {
        let url = format!("{}/v1/cluster/council", self.base_url);
        let response = self
            .http()?
            .get(&url)
            .send()
            .await
            .map_err(classify_error)?;

        let status = response.status().as_u16();
        if !response.status().is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(RelishError::ApiError { status, body });
        }

        let council: CouncilStatus = response.json().await.map_err(|e| RelishError::ApiError {
            status: 0,
            body: format!("failed to parse response: {e}"),
        })?;

        Ok(council)
    }

    /// Inject a fault (Smoker API).
    pub async fn inject_fault(
        &self,
        request: &crate::smoker::types::FaultRequest,
    ) -> Result<crate::smoker::types::FaultSummary, RelishError> {
        let url = format!("{}/v1/fault", self.base_url);
        let response = self
            .http()?
            .post(&url)
            .json(request)
            .send()
            .await
            .map_err(classify_error)?;

        let status = response.status().as_u16();
        if !response.status().is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(RelishError::ApiError { status, body });
        }

        response.json().await.map_err(|e| RelishError::ApiError {
            status: 0,
            body: format!("failed to parse response: {e}"),
        })
    }

    /// Clear a specific fault by ID.
    pub async fn clear_fault(
        &self,
        id: u64,
        node: Option<&str>,
        acknowledged: bool,
    ) -> Result<String, RelishError> {
        let url = format!("{}/v1/fault/{id}", self.base_url);
        let mut request = self.http()?.delete(&url);
        if let Some(node) = node {
            request = request.query(&[
                ("node", node),
                ("acknowledged", if acknowledged { "true" } else { "false" }),
            ]);
        }
        let response = request.send().await.map_err(classify_error)?;

        let status = response.status().as_u16();
        if !response.status().is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(RelishError::ApiError { status, body });
        }

        let json: serde_json::Value = response.json().await.map_err(|e| RelishError::ApiError {
            status: 0,
            body: format!("failed to parse response: {e}"),
        })?;
        Ok(json["message"].as_str().unwrap_or("ok").to_string())
    }

    /// Clear all active faults.
    pub async fn clear_all_faults(&self) -> Result<String, RelishError> {
        let url = format!("{}/v1/fault", self.base_url);
        let response = self
            .http()?
            .delete(&url)
            .send()
            .await
            .map_err(classify_error)?;

        let status = response.status().as_u16();
        if !response.status().is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(RelishError::ApiError { status, body });
        }

        let json: serde_json::Value = response.json().await.map_err(|e| RelishError::ApiError {
            status: 0,
            body: format!("failed to parse response: {e}"),
        })?;
        Ok(json["message"].as_str().unwrap_or("ok").to_string())
    }

    /// Clear every active fault targeting `service`.
    pub async fn clear_faults_by_service(
        &self,
        service: &str,
        namespace: Option<&str>,
    ) -> Result<String, RelishError> {
        let url = match namespace {
            Some(namespace) => format!(
                "{}/v1/fault?service={}&namespace={}",
                self.base_url, service, namespace
            ),
            None => format!("{}/v1/fault?service={}", self.base_url, service),
        };
        let response = self
            .http()?
            .delete(&url)
            .send()
            .await
            .map_err(classify_error)?;

        let status = response.status().as_u16();
        if !response.status().is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(RelishError::ApiError { status, body });
        }

        let json: serde_json::Value = response.json().await.map_err(|e| RelishError::ApiError {
            status: 0,
            body: format!("failed to parse response: {e}"),
        })?;
        Ok(json["message"].as_str().unwrap_or("ok").to_string())
    }

    /// List all active faults.
    pub async fn list_faults(
        &self,
    ) -> Result<Vec<crate::smoker::types::FaultSummary>, RelishError> {
        let url = format!("{}/v1/fault", self.base_url);
        let response = self
            .http()?
            .get(&url)
            .send()
            .await
            .map_err(classify_error)?;

        let status = response.status().as_u16();
        if !response.status().is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(RelishError::ApiError { status, body });
        }

        response.json().await.map_err(|e| RelishError::ApiError {
            status: 0,
            body: format!("failed to parse response: {e}"),
        })
    }

    /// Every node's workloads with their latest CPU and memory samples.
    pub async fn cluster_top(&self) -> Result<crate::bun::top::ClusterTop, RelishError> {
        self.get_typed_json("/v1/top?cluster=true").await
    }

    /// List every node's active faults, each tagged with its node.
    pub async fn list_cluster_faults(
        &self,
    ) -> Result<crate::bun::api::ClusterFaultList, RelishError> {
        self.get_typed_json("/v1/fault?cluster=true").await
    }

    /// Resolve a service name to its VIP and backends.
    pub async fn resolve(
        &self,
        name: &str,
    ) -> Result<crate::onion::types::ResolveResponse, RelishError> {
        let url = format!("{}/v1/resolve/{name}", self.base_url);
        let response = self
            .http()?
            .get(&url)
            .send()
            .await
            .map_err(classify_error)?;

        let status = response.status().as_u16();
        if !response.status().is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(RelishError::ApiError { status, body });
        }

        response.json().await.map_err(|e| RelishError::ApiError {
            status: 0,
            body: format!("failed to parse resolve response: {e}"),
        })
    }

    /// List all registered services.
    pub async fn resolve_all(
        &self,
    ) -> Result<Vec<crate::onion::types::ResolveResponse>, RelishError> {
        let url = format!("{}/v1/resolve", self.base_url);
        let response = self
            .http()?
            .get(&url)
            .send()
            .await
            .map_err(classify_error)?;

        let status = response.status().as_u16();
        if !response.status().is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(RelishError::ApiError { status, body });
        }

        response.json().await.map_err(|e| RelishError::ApiError {
            status: 0,
            body: format!("failed to parse resolve response: {e}"),
        })
    }

    /// List all ingress routes.
    pub async fn routes(&self) -> Result<Vec<crate::wrapper::types::RouteInfo>, RelishError> {
        let url = format!("{}/v1/routes", self.base_url);
        let response = self
            .http()?
            .get(&url)
            .send()
            .await
            .map_err(classify_error)?;

        let status = response.status().as_u16();
        if !response.status().is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(RelishError::ApiError { status, body });
        }

        response.json().await.map_err(|e| RelishError::ApiError {
            status: 0,
            body: format!("failed to parse routes response: {e}"),
        })
    }

    /// List images in the local Pickle registry.
    pub async fn images(&self) -> Result<serde_json::Value, RelishError> {
        let url = format!("{}/v1/images", self.base_url);
        let response = self
            .http()?
            .get(&url)
            .send()
            .await
            .map_err(classify_error)?;

        let status = response.status().as_u16();
        if !response.status().is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(RelishError::ApiError { status, body });
        }

        response.json().await.map_err(|e| RelishError::ApiError {
            status: 0,
            body: format!("failed to parse images response: {e}"),
        })
    }

    /// Submit a build job to the agent.
    ///
    /// The registry destination is server-owned (JOB2): the node uses
    /// its own `[images] registry_port`, so the CLI does not (and must
    /// not) send one in the body.
    pub async fn submit_build(
        &self,
        name: &str,
        context_digest: &str,
        spec: &crate::config::build::BuildSpec,
    ) -> Result<u64, RelishError> {
        let url = format!("{}/v1/build", self.base_url);
        let response = self
            .http()?
            .post(&url)
            .json(&serde_json::json!({
                "name": name,
                "context_digest": context_digest,
                "spec": spec,
            }))
            .send()
            .await
            .map_err(classify_error)?;

        let status = response.status().as_u16();
        if !response.status().is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(RelishError::ApiError { status, body });
        }

        let json: serde_json::Value = response.json().await.map_err(|e| RelishError::ApiError {
            status: 0,
            body: format!("failed to parse response: {e}"),
        })?;
        json["build_id"]
            .as_u64()
            .ok_or_else(|| RelishError::ApiError {
                status,
                body: format!("no build_id in response: {json}"),
            })
    }

    /// Progress of a submitted build (`GET /v1/build/{id}`).
    pub async fn build_status(&self, build_id: u64) -> Result<serde_json::Value, RelishError> {
        let url = format!("{}/v1/build/{build_id}", self.base_url);
        let response = self
            .http()?
            .get(&url)
            .send()
            .await
            .map_err(classify_error)?;

        let status = response.status().as_u16();
        if !response.status().is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(RelishError::ApiError { status, body });
        }
        response.json().await.map_err(classify_error)
    }

    /// List API tokens from SecurityState (names, roles and times only).
    pub async fn token_list(&self) -> Result<Vec<TokenSummary>, RelishError> {
        #[derive(serde::Deserialize)]
        struct TokenList {
            tokens: Vec<TokenSummary>,
        }
        Ok(self
            .get_typed_json::<TokenList>("/v1/token/list")
            .await?
            .tokens)
    }

    /// Create an API token via the agent (persisted in Raft). Returns the
    /// plaintext, shown once.
    pub async fn token_create(
        &self,
        name: &str,
        role: &str,
        apps: Option<Vec<String>>,
        namespaces: Option<Vec<String>>,
        ttl_days: Option<u64>,
    ) -> Result<String, RelishError> {
        self.create_token_request(serde_json::json!({
            "name": name, "role": role, "apps": apps,
            "namespaces": namespaces, "ttl_days": ttl_days,
        }))
        .await
    }

    /// Mint a namespace-scoped Deployer token whose lifetime and cleanup belong to a test lease.
    pub async fn token_create_with_lease(
        &self,
        name: &str,
        namespace: &str,
        lease_id: &str,
    ) -> Result<String, RelishError> {
        self.create_token_request(serde_json::json!({
            "name": name, "role": "deployer", "namespaces": [namespace], "lease_id": lease_id,
        }))
        .await
    }

    async fn create_token_request(&self, body: serde_json::Value) -> Result<String, RelishError> {
        let url = format!("{}/v1/token/create", self.base_url);
        let response = self
            .http()?
            .post(&url)
            .json(&body)
            .send()
            .await
            .map_err(classify_error)?;

        let status = response.status().as_u16();
        if !response.status().is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(RelishError::ApiError { status, body });
        }

        let json: serde_json::Value = response.json().await.map_err(|e| RelishError::ApiError {
            status: 0,
            body: format!("failed to parse response: {e}"),
        })?;
        json["token"]
            .as_str()
            .map(str::to_string)
            .ok_or_else(|| RelishError::ApiError {
                status: 0,
                body: "response missing token".to_string(),
            })
    }

    /// Revoke an API token by name.
    pub async fn token_revoke(&self, name: &str) -> Result<String, RelishError> {
        let url = format!("{}/v1/token/revoke", self.base_url);
        let response = self
            .http()?
            .post(&url)
            .json(&serde_json::json!({ "name": name }))
            .send()
            .await
            .map_err(classify_error)?;

        let status = response.status().as_u16();
        if !response.status().is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(RelishError::ApiError { status, body });
        }

        let json: serde_json::Value = response.json().await.map_err(|e| RelishError::ApiError {
            status: 0,
            body: format!("failed to parse response: {e}"),
        })?;
        Ok(json["message"]
            .as_str()
            .unwrap_or("token revoked")
            .to_string())
    }

    /// Create a single-use node join token. The server commits only its hash
    /// to Raft and returns the plaintext once.
    pub async fn join_token_create(
        &self,
        node_id: &str,
        ttl_seconds: u64,
    ) -> Result<String, RelishError> {
        let url = format!("{}/v1/join-token/create", self.base_url);
        let response = self
            .http()?
            .post(&url)
            .json(&serde_json::json!({ "ttl_seconds": ttl_seconds, "node_id": node_id }))
            .send()
            .await
            .map_err(classify_error)?;

        let status = response.status().as_u16();
        if !response.status().is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(RelishError::ApiError { status, body });
        }

        let json: serde_json::Value = response.json().await.map_err(|e| RelishError::ApiError {
            status: 0,
            body: format!("failed to parse join-token response: {e}"),
        })?;
        json["token"]
            .as_str()
            .map(str::to_string)
            .ok_or_else(|| RelishError::ApiError {
                status: 0,
                body: "response missing join token".to_string(),
            })
    }

    /// Fetch the active public recipient without accessing cluster key files.
    pub async fn secret_public_key(
        &self,
    ) -> Result<crate::sesame::types::SecretPublicKey, RelishError> {
        self.get_typed_json("/v1/secret/public-key").await
    }

    /// Rotate or finalise the secret encryption key.
    pub async fn secret_rotate(&self, finalize: bool) -> Result<String, RelishError> {
        let url = format!("{}/v1/secret/rotate", self.base_url);
        let response = self
            .http()?
            .post(&url)
            .json(&serde_json::json!({ "finalize": finalize }))
            .send()
            .await
            .map_err(classify_error)?;

        let status = response.status().as_u16();
        if !response.status().is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(RelishError::ApiError { status, body });
        }

        let json: serde_json::Value = response.json().await.map_err(|e| RelishError::ApiError {
            status: 0,
            body: format!("failed to parse response: {e}"),
        })?;
        Ok(json["message"]
            .as_str()
            .unwrap_or("rotation complete")
            .to_string())
    }

    /// Submit a locally made image signature for the cluster to attach.
    pub async fn sign_image(
        &self,
        submission: &crate::pickle::signing::SignatureSubmission,
    ) -> Result<String, RelishError> {
        let url = format!("{}/v1/identity/sign", self.base_url);
        let response = self
            .http()?
            .post(&url)
            .json(submission)
            .send()
            .await
            .map_err(classify_error)?;

        let status = response.status().as_u16();
        if !response.status().is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(RelishError::ApiError { status, body });
        }

        let json: serde_json::Value = response.json().await.map_err(|e| RelishError::ApiError {
            status: 0,
            body: format!("failed to parse response: {e}"),
        })?;
        Ok(json["message"]
            .as_str()
            .unwrap_or("signature attached")
            .to_string())
    }

    /// Submit a batch of jobs for high-throughput scheduling.
    /// Submit a batch: full job specs travel with the request, so the
    /// cluster needs no prior deploy of them. Returns the response
    /// JSON (`batch_id`, `assigned`, `unschedulable`).
    pub async fn submit_batch(
        &self,
        jobs: &std::collections::BTreeMap<String, crate::config::job::JobSpec>,
    ) -> Result<serde_json::Value, RelishError> {
        let url = format!("{}/v1/batch", self.base_url);
        let payload = serde_json::json!({
            "jobs": jobs
                .iter()
                .map(|(name, spec)| {
                    serde_json::json!({ "name": name, "spec": spec })
                })
                .collect::<Vec<_>>(),
        });
        let response = self
            .http()?
            .post(&url)
            .json(&payload)
            .send()
            .await
            .map_err(classify_error)?;

        let status = response.status().as_u16();
        if !response.status().is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(RelishError::ApiError { status, body });
        }

        response.json().await.map_err(|e| RelishError::ApiError {
            status: 0,
            body: format!("failed to parse response: {e}"),
        })
    }

    /// Progress of a submitted batch (`GET /v1/batch/{id}`).
    pub async fn batch_status(&self, batch_id: u64) -> Result<serde_json::Value, RelishError> {
        let url = format!("{}/v1/batch/{batch_id}", self.base_url);
        let response = self
            .http()?
            .get(&url)
            .send()
            .await
            .map_err(classify_error)?;

        let status = response.status().as_u16();
        if !response.status().is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(RelishError::ApiError { status, body });
        }
        response.json().await.map_err(classify_error)
    }

    // ---- self-upgrade (Phase 14) ----

    /// The version the node reports (`GET /v1/version`).
    pub async fn node_version(&self) -> Result<String, RelishError> {
        let json = self.get_json("/v1/version").await?;
        Ok(json["version"].as_str().unwrap_or("?").to_string())
    }

    /// Node-level upgrade status.
    pub async fn upgrade_status(&self) -> Result<serde_json::Value, RelishError> {
        self.get_json("/v1/upgrade/status").await
    }

    /// Cluster-level upgrade state (errors when there is no council).
    pub async fn upgrade_cluster(&self) -> Result<serde_json::Value, RelishError> {
        self.get_json("/v1/upgrade/cluster").await
    }

    /// Apply a node-level upgrade directive. The response's `status` is
    /// `upgrading`, or `already_running` when the node runs exactly this
    /// binary already.
    pub async fn upgrade_apply(
        &self,
        directive: &crate::upgrade::types::UpgradeDirective,
    ) -> Result<serde_json::Value, RelishError> {
        let body = serde_json::to_string(directive).map_err(RelishError::SerialiseJson)?;
        self.post_json("/v1/upgrade/apply", body).await
    }

    /// Start a cluster-wide rolling upgrade (leader only).
    pub async fn upgrade_start(
        &self,
        request: &serde_json::Value,
    ) -> Result<serde_json::Value, RelishError> {
        self.post_json("/v1/upgrade/start", request.to_string())
            .await
    }

    /// Start a cluster-wide rolling rollback (leader only).
    pub async fn upgrade_cluster_rollback(
        &self,
        request: &serde_json::Value,
    ) -> Result<serde_json::Value, RelishError> {
        self.post_json("/v1/upgrade/cluster-rollback", request.to_string())
            .await
    }

    /// Roll this node back to a previous binary version.
    pub async fn upgrade_node_rollback(&self, version: Option<&str>) -> Result<(), RelishError> {
        let body = match version {
            Some(version) => serde_json::json!({ "version": version }).to_string(),
            None => String::new(),
        };
        self.post_json("/v1/upgrade/rollback", body)
            .await
            .map(|_| ())
    }

    /// Un-pause a paused cluster upgrade (leader only).
    pub async fn upgrade_resume(&self) -> Result<(), RelishError> {
        self.post_json("/v1/upgrade/resume", String::new())
            .await
            .map(|_| ())
    }

    /// End a paused cluster upgrade in which no node moved (leader only).
    /// Returns the aborted run's id.
    pub async fn upgrade_abort(&self) -> Result<String, RelishError> {
        let response = self.post_json("/v1/upgrade/abort", String::new()).await?;
        Ok(response["upgrade_id"].as_str().unwrap_or("?").to_string())
    }

    async fn get_json(&self, path: &str) -> Result<serde_json::Value, RelishError> {
        let url = format!("{}{path}", self.base_url);
        let response = self
            .http()?
            .get(&url)
            .send()
            .await
            .map_err(classify_error)?;
        let status = response.status().as_u16();
        if !response.status().is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(RelishError::ApiError { status, body });
        }
        response.json().await.map_err(|e| RelishError::ApiError {
            status: 0,
            body: format!("failed to parse response: {e}"),
        })
    }

    async fn post_json(&self, path: &str, body: String) -> Result<serde_json::Value, RelishError> {
        let url = format!("{}{path}", self.base_url);
        let response = self
            .http()?
            .post(&url)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(body)
            .send()
            .await
            .map_err(classify_error)?;
        let status = response.status().as_u16();
        if !response.status().is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(RelishError::ApiError { status, body });
        }
        response
            .json()
            .await
            .or_else(|_| Ok(serde_json::Value::Null))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[test]
    fn explicit_ca_constructor_refuses_invalid_trust_material() {
        assert!(
            BunClient::new_with_ca("https://127.0.0.1:9117", None, b"not a certificate").is_err()
        );
    }

    #[tokio::test]
    async fn malformed_bearer_is_an_error_instead_of_an_anonymous_request() {
        let client = BunClient::new_with_token("http://127.0.0.1:9", Some("bad\nheader"));
        let error = client.health().await.unwrap_err();
        assert!(matches!(error, RelishError::ApiError { body, .. } if body.contains("bearer")));
    }

    #[tokio::test]
    async fn deployment_errors_preserve_unicode_across_network_chunks() {
        use axum::{Router, routing::post};
        let message = "cannot deploy café 🍔";
        let event = format!(
            "data: {}\n\n",
            serde_json::to_string(&ApplyEvent::Error {
                message: message.to_string(),
            })
            .unwrap()
        );
        let split = event.find('é').unwrap() + 1;
        let chunks = vec![
            event.as_bytes()[..split].to_vec(),
            event.as_bytes()[split..].to_vec(),
        ];
        let handler = move || {
            let chunks = chunks.clone();
            async move {
                let stream = futures_util::stream::iter(chunks).then(|chunk| async move {
                    tokio::time::sleep(std::time::Duration::from_millis(30)).await;
                    Ok::<_, std::io::Error>(chunk)
                });
                axum::body::Body::from_stream(stream)
            }
        };
        let app = Router::new()
            .route("/v1/apply", post(handler.clone()))
            .route("/v1/rollback/web/default", post(handler));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let client = BunClient::new_with_token(&format!("http://{address}"), None);
        let applied = client.apply(&Config::default()).await.unwrap_err();
        let rolled_back = client.rollback("web", "default").await.unwrap_err();
        server.abort();
        for error in [applied, rolled_back] {
            assert!(matches!(error, RelishError::ApiError { body, .. } if body == message));
        }
    }

    /// Serve one canned `/v1/logs/query` answer and return what `relish logs`
    /// prints for it.
    async fn render_queried_logs(entries: Vec<crate::ketchup::types::LogEntry>) -> String {
        use axum::{Json, Router, routing::get};
        let result = crate::ketchup::types::LogQueryResult {
            entries,
            node_count: 1,
            warnings: vec![],
        };
        let app = Router::new().route(
            "/v1/logs/query/{app}/{namespace}",
            get(move || {
                let result = result.clone();
                async move { Json(result) }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let client = BunClient::new_with_token(&format!("http://{address}"), None);
        let output = client
            .logs("soak-redis-client", "default", &LogOptions::default())
            .await
            .unwrap();
        server.abort();
        output
    }

    fn queried(sequence: u64, instance: &str, line: &str) -> crate::ketchup::types::LogEntry {
        crate::ketchup::types::LogEntry {
            timestamp: sequence / 1_000_000_000,
            sequence,
            instance: Some(instance.to_string()),
            stream: crate::ketchup::types::LogStream::Stdout,
            line: line.to_string(),
        }
    }

    #[tokio::test]
    async fn logs_from_one_instance_print_bare() {
        let output = render_queried_logs(vec![
            queried(1, "soak-redis-client-0", "INCR 1"),
            queried(2, "soak-redis-client-0", "INCR 2"),
        ])
        .await;
        assert_eq!(output, "INCR 1\nINCR 2");
    }

    /// V02 soak: during a rolling deploy two instances INCR the same counter,
    /// and an unlabelled tail read as one client stepping by two.
    #[tokio::test]
    async fn logs_from_several_instances_name_each_line_s_instance() {
        let output = render_queried_logs(vec![
            queried(1, "soak-redis-client-0", "INCR 3550"),
            queried(2, "soak-redis-client-1", "INCR 3551"),
            queried(3, "soak-redis-client-0", "INCR 3552"),
        ])
        .await;
        assert_eq!(
            output,
            "[soak-redis-client-0] INCR 3550\n\
             [soak-redis-client-1] INCR 3551\n\
             [soak-redis-client-0] INCR 3552"
        );
    }

    #[tokio::test]
    async fn health_rejects_http_errors_and_unrelated_services() {
        use axum::{Router, routing::get};
        for (status, body) in [
            (404, r#"{"status":"ok"}"#),
            (500, r#"{"status":"ok"}"#),
            (200, "welcome to another server"),
            (200, r#"{"status":"starting"}"#),
        ] {
            let app =
                Router::new().route(
                    "/v1/health",
                    get(move || async move {
                        (axum::http::StatusCode::from_u16(status).unwrap(), body)
                    }),
                );
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let address = listener.local_addr().unwrap();
            let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
            let client = BunClient::new_with_token(&format!("http://{address}"), None);
            let result = client.health().await;
            server.abort();
            assert!(result.is_err(), "accepted HTTP {status}: {body}");
        }
    }

    #[tokio::test]
    async fn health_accepts_the_bun_liveness_response() {
        let app = axum::Router::new().route(
            "/v1/health",
            axum::routing::get(|| async { axum::Json(serde_json::json!({"status": "ok"})) }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let client = BunClient::new_with_token(&format!("http://{address}"), None);
        let result = client.health().await;
        server.abort();
        result.unwrap();
    }

    /// PEM-encode a DER certificate for `reqwest::Certificate::from_pem`.
    fn pem_cert(der: &[u8]) -> Vec<u8> {
        pem::encode(&pem::Pem::new("CERTIFICATE", der.to_vec())).into_bytes()
    }

    /// Serve `GET /v1/health` over TLS with `identity`'s cert on an ephemeral
    /// port. Returns the address; the task ends when `shutdown` fires.
    async fn spawn_tls_health(
        identity: &crate::sesame::identity_store::NodeIdentity,
        shutdown: tokio_util::sync::CancellationToken,
    ) -> std::net::SocketAddr {
        use axum::{Router, routing::get};
        use tower::Service as _;

        let acceptor = tokio_rustls::TlsAcceptor::from(
            crate::sesame::mtls::build_api_server_config(
                identity,
                crate::sesame::mtls::CrlHandle::default(),
            )
            .unwrap(),
        );
        let router = Router::new().route(
            "/v1/health",
            get(|| async { axum::Json(serde_json::json!({"status": "ok"})) }),
        );
        let router = router.route(
            "/ws",
            get(
                |headers: axum::http::HeaderMap, ws: axum::extract::WebSocketUpgrade| async move {
                    use axum::response::IntoResponse;
                    if headers
                        .get("authorization")
                        .and_then(|value| value.to_str().ok())
                        != Some("Bearer rbrg_ws")
                    {
                        return axum::http::StatusCode::UNAUTHORIZED.into_response();
                    }
                    ws.on_upgrade(|_socket| async {}).into_response()
                },
            ),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let mut make_service = router.into_make_service();
            loop {
                tokio::select! {
                    _ = shutdown.cancelled() => return,
                    accepted = listener.accept() => {
                        let Ok((tcp, _)) = accepted else { continue };
                        let acceptor = acceptor.clone();
                        let service = match make_service.call(()).await {
                            Ok(s) => s,
                            Err(infallible) => match infallible {},
                        };
                        tokio::spawn(async move {
                            let Ok(tls) = acceptor.accept(tcp).await else { return };
                            let svc = hyper_util::service::TowerToHyperService::new(service);
                            let _ = hyper_util::server::conn::auto::Builder::new(
                                hyper_util::rt::TokioExecutor::new(),
                            )
                            .serve_connection_with_upgrades(hyper_util::rt::TokioIo::new(tls), svc)
                            .await;
                        });
                    }
                }
            }
        });
        addr
    }

    fn test_identity(
        hierarchy: &crate::sesame::ca::CaHierarchy,
        node_id: &str,
    ) -> crate::sesame::identity_store::NodeIdentity {
        use std::time::{Duration, SystemTime};
        let (cert_der, key_der, serial) = crate::sesame::ca::issue_node_cert(
            node_id,
            crate::sesame::types::SerialNumber(1),
            &hierarchy.node.signing_keypair,
            &hierarchy.node.certificate_params,
        )
        .unwrap();
        let now = SystemTime::now();
        crate::sesame::identity_store::NodeIdentity {
            node_id: node_id.to_string(),
            certificate_der: cert_der,
            private_key_der: key_der,
            serial,
            ca_generation: 0,
            node_ca_der: hierarchy.node.ca.certificate_der.clone(),
            root_ca_der: hierarchy.root.ca.certificate_der.clone(),
            not_before: now,
            not_after: now + Duration::from_secs(3600),
        }
    }

    #[tokio::test]
    async fn websocket_uses_the_pinned_cluster_ca_and_bearer() {
        let hierarchy = crate::sesame::ca::generate_ca_hierarchy("websocket-test", b"ikm").unwrap();
        let identity = test_identity(&hierarchy, "node-01");
        let shutdown = tokio_util::sync::CancellationToken::new();
        let address = spawn_tls_health(&identity, shutdown.clone()).await;
        let ca = pem_cert(&hierarchy.node.ca.certificate_der);
        let client =
            BunClient::new_with_ca(&format!("https://{address}"), Some("rbrg_ws"), &ca).unwrap();
        let result = client.ws_connect("/ws").await;
        shutdown.cancel();
        assert!(
            result.is_ok(),
            "pinned WebSocket connection failed: {result:?}"
        );
    }

    #[tokio::test]
    async fn replacing_bearer_preserves_explicit_ca_and_uses_the_new_credential() {
        let hierarchy = crate::sesame::ca::generate_ca_hierarchy("scoped-client", b"ikm").unwrap();
        let identity = test_identity(&hierarchy, "node-01");
        let shutdown = tokio_util::sync::CancellationToken::new();
        let address = spawn_tls_health(&identity, shutdown.clone()).await;
        let ca = pem_cert(&hierarchy.node.ca.certificate_der);
        let client =
            BunClient::new_with_ca(&format!("https://{address}"), Some("original"), &ca).unwrap();
        // This endpoint accepts only rbrg_ws. The replacement must keep the
        // explicit CA without inheriting the old credential's default header.
        let result = client.with_token("rbrg_ws").ws_connect("/ws").await;
        shutdown.cancel();
        assert!(result.is_ok(), "replacement credential failed: {result:?}");
    }

    #[tokio::test]
    async fn websocket_refuses_an_unrelated_ca() {
        let hierarchy =
            crate::sesame::ca::generate_ca_hierarchy("websocket-server", b"ikm").unwrap();
        let other = crate::sesame::ca::generate_ca_hierarchy("unrelated", b"other").unwrap();
        let identity = test_identity(&hierarchy, "node-01");
        let shutdown = tokio_util::sync::CancellationToken::new();
        let address = spawn_tls_health(&identity, shutdown.clone()).await;
        let ca = pem_cert(&other.node.ca.certificate_der);
        let client =
            BunClient::new_with_ca(&format!("https://{address}"), Some("rbrg_ws"), &ca).unwrap();
        let result = client.ws_connect("/ws").await;
        shutdown.cancel();
        assert!(result.is_err());
    }

    /// The legitimate mTLS path must keep working with built-in roots disabled:
    /// a client trusting the cluster CA reaches the agent over HTTPS even though
    /// the server certificate's name (`node-01`) doesn't match `127.0.0.1`.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn ca_cert_client_completes_https_round_trip_with_hostname_mismatch() {
        let hierarchy =
            crate::sesame::ca::generate_ca_hierarchy("client-tls-test", b"ikm").unwrap();
        let server = test_identity(&hierarchy, "node-01");
        let shutdown = tokio_util::sync::CancellationToken::new();
        let addr = spawn_tls_health(&server, shutdown.clone()).await;

        let ca_pem = pem_cert(&hierarchy.node.ca.certificate_der);
        let client = BunClient::build(&format!("https://{addr}"), None, Some(&ca_pem));
        client
            .health()
            .await
            .expect("cluster-CA client should reach the agent over HTTPS");

        let workload = client.workload_http_builder().unwrap().build().unwrap();
        assert!(
            workload
                .get(format!("https://{addr}/v1/health"))
                .send()
                .await
                .is_err(),
            "workload clients must not inherit the control-plane hostname exception"
        );

        shutdown.cancel();
    }

    async fn assert_lease_cleanup_responses(
        responses: Vec<(axum::http::Method, axum::http::StatusCode)>,
        expected_error: Option<u16>,
    ) {
        use std::collections::VecDeque;
        use std::sync::Arc;
        let remaining = Arc::new(tokio::sync::Mutex::new(VecDeque::from(responses)));
        let handler_remaining = remaining.clone();
        let router = axum::Router::new().route(
            "/v1/test/leases/fixture",
            axum::routing::any(move |method: axum::http::Method| {
                let remaining = handler_remaining.clone();
                async move {
                    let (expected, status) = remaining.lock().await.pop_front().unwrap();
                    assert_eq!(method, expected);
                    (status, "fixture response")
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let result = BunClient::new_with_token(&format!("http://{address}"), None)
            .release_test_lease("fixture")
            .await;
        server.abort();
        let _ = server.await;
        match expected_error {
            Some(expected) => assert!(
                matches!(result, Err(RelishError::ApiError { status, .. }) if status == expected),
                "{result:?}"
            ),
            None => result.unwrap(),
        }
        assert!(remaining.lock().await.is_empty());
    }

    #[tokio::test]
    async fn lease_cleanup_retries_unavailable_leaders_until_positive_confirmation() {
        use axum::http::{Method, StatusCode};
        assert_lease_cleanup_responses(
            vec![
                (Method::DELETE, StatusCode::SERVICE_UNAVAILABLE),
                (Method::DELETE, StatusCode::ACCEPTED),
                (Method::GET, StatusCode::SERVICE_UNAVAILABLE),
                (Method::GET, StatusCode::OK),
                (Method::GET, StatusCode::NOT_FOUND),
            ],
            None,
        )
        .await;
        assert_lease_cleanup_responses(
            vec![
                (Method::DELETE, StatusCode::SERVICE_UNAVAILABLE),
                (Method::DELETE, StatusCode::NO_CONTENT),
            ],
            None,
        )
        .await;
    }

    #[tokio::test]
    async fn lease_cleanup_preserves_permanent_refusals_without_retrying() {
        use axum::http::{Method, StatusCode};
        for status in [
            StatusCode::UNAUTHORIZED,
            StatusCode::FORBIDDEN,
            StatusCode::CONFLICT,
            StatusCode::NOT_FOUND,
        ] {
            assert_lease_cleanup_responses(vec![(Method::DELETE, status)], Some(status.as_u16()))
                .await;
        }
        for status in [
            StatusCode::UNAUTHORIZED,
            StatusCode::FORBIDDEN,
            StatusCode::CONFLICT,
        ] {
            assert_lease_cleanup_responses(
                vec![
                    (Method::DELETE, StatusCode::ACCEPTED),
                    (Method::GET, status),
                ],
                Some(status.as_u16()),
            )
            .await;
        }
    }

    #[tokio::test]
    async fn lease_cleanup_unavailable_leader_remains_bounded_by_the_original_deadline() {
        use std::sync::Arc;
        let entered = Arc::new(tokio::sync::Notify::new());
        let handler_entered = entered.clone();
        let router = axum::Router::new().route(
            "/v1/test/leases/fixture",
            axum::routing::delete(move || {
                let entered = handler_entered.clone();
                async move {
                    entered.notify_one();
                    axum::http::StatusCode::SERVICE_UNAVAILABLE
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let cleanup = tokio::spawn(async move {
            BunClient::new_with_token(&format!("http://{address}"), None)
                .release_test_lease("fixture")
                .await
        });
        tokio::time::timeout(std::time::Duration::from_secs(2), entered.notified())
            .await
            .unwrap();
        tokio::time::pause();
        tokio::time::advance(std::time::Duration::from_secs(31)).await;
        let result = cleanup.await.unwrap();
        server.abort();
        let _ = server.await;
        assert!(
            matches!(result, Err(RelishError::RequestTimeout)),
            "{result:?}"
        );
    }

    #[tokio::test]
    async fn managed_capabilities_use_only_declared_host_forwards() {
        use crate::bun::capabilities::{ClusterCapabilities, ServiceEndpoints};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let router = axum::Router::new().route(
            "/v1/capabilities",
            axum::routing::get(|| async {
                axum::Json(ClusterCapabilities {
                    service_endpoints: ServiceEndpoints {
                        registry: Some("https://192.168.104.2:5050".into()),
                        ingress_http: Some("http://0.0.0.0:80".into()),
                        ingress_https: Some("https://0.0.0.0:443".into()),
                    },
                    ..Default::default()
                })
            }),
        );
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let forwards = ServiceEndpoints {
            registry: Some("https://127.0.0.1:15050".into()),
            ingress_http: Some("http://127.0.0.1:18080".into()),
            ingress_https: None,
        };
        let client = BunClient::new_with_token(&format!("http://{address}"), None)
            .with_service_endpoints(forwards.clone());
        assert_eq!(
            client.capabilities().await.unwrap().service_endpoints,
            forwards
        );
        assert_eq!(
            client
                .with_token("scoped")
                .capabilities()
                .await
                .unwrap()
                .service_endpoints,
            forwards
        );
        assert!(
            client
                .registry_http_client("http://192.168.104.2:5050")
                .is_err()
        );
        server.abort();
    }

    /// With built-in roots off, only the configured CA is trusted: a client
    /// handed a *different* CA must refuse the connection rather than fall back
    /// to any other trust anchor.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn client_rejects_a_server_signed_by_an_untrusted_ca() {
        let server_hierarchy =
            crate::sesame::ca::generate_ca_hierarchy("server-ca", b"ikm").unwrap();
        let other_hierarchy = crate::sesame::ca::generate_ca_hierarchy("other-ca", b"ikm").unwrap();
        let server = test_identity(&server_hierarchy, "node-01");
        let shutdown = tokio_util::sync::CancellationToken::new();
        let addr = spawn_tls_health(&server, shutdown.clone()).await;

        let wrong_ca_pem = pem_cert(&other_hierarchy.node.ca.certificate_der);
        let client = BunClient::build(&format!("https://{addr}"), None, Some(&wrong_ca_pem));
        assert!(
            client.health().await.is_err(),
            "a client trusting a different CA must refuse the connection"
        );

        shutdown.cancel();
    }

    /// Start a throwaway server that records the Authorization header it sees on
    /// `GET /v1/health`. Returns its base URL and the captured value.
    async fn capture_server() -> (String, Arc<Mutex<Option<String>>>) {
        use axum::{Router, http::HeaderMap, routing::get};

        let captured: Arc<Mutex<Option<String>>> = Arc::new(Mutex::new(None));
        let cap = Arc::clone(&captured);
        let app = Router::new().route(
            "/v1/health",
            get(move |headers: HeaderMap| {
                let cap = Arc::clone(&cap);
                async move {
                    *cap.lock().unwrap() = headers
                        .get("authorization")
                        .and_then(|v| v.to_str().ok())
                        .map(String::from);
                    axum::Json(serde_json::json!({"status": "ok"}))
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        (format!("http://{addr}"), captured)
    }

    #[tokio::test]
    async fn client_attaches_bearer_header_when_token_present() {
        let (url, captured) = capture_server().await;
        let client = BunClient::new_with_token(&url, Some("rbrg_abc"));
        client.health().await.unwrap();
        assert_eq!(captured.lock().unwrap().as_deref(), Some("Bearer rbrg_abc"));
    }

    #[tokio::test]
    async fn client_sends_no_authorization_without_a_token() {
        let (url, captured) = capture_server().await;
        let client = BunClient::new_with_token(&url, None);
        client.health().await.unwrap();
        assert!(captured.lock().unwrap().is_none());
    }

    #[tokio::test]
    async fn changing_only_the_node_address_preserves_authentication() {
        let (url, captured) = capture_server().await;
        let entry = BunClient::new_with_token("http://127.0.0.1:1", Some("rbrg_cluster"));
        entry.with_base_url(&url).health().await.unwrap();
        assert_eq!(
            captured.lock().unwrap().as_deref(),
            Some("Bearer rbrg_cluster")
        );
    }

    #[test]
    fn resolve_token_prefers_flag_over_env() {
        let flag = Some("rbrg_flag".to_string());
        assert_eq!(
            pick_token(Some(&flag), Some("rbrg_env".to_string())),
            Some("rbrg_flag".to_string())
        );
        // Flag explicitly absent -> fall back to env.
        assert_eq!(
            pick_token(Some(&None), Some("rbrg_env".to_string())),
            Some("rbrg_env".to_string())
        );
        // Flag never set -> env.
        assert_eq!(
            pick_token(None, Some("rbrg_env".to_string())),
            Some("rbrg_env".to_string())
        );
        // Neither -> none.
        assert_eq!(pick_token(None, None), None);
    }

    #[test]
    fn resolve_endpoint_prefers_flag_over_env() {
        let flag = Some("https://flag.example:9117".to_string());
        assert_eq!(
            pick_endpoint(Some(&flag), Some("https://env.example:9117".to_string())),
            Some("https://flag.example:9117".to_string())
        );
        assert_eq!(
            pick_endpoint(Some(&None), Some("https://env.example:9117".to_string())),
            Some("https://env.example:9117".to_string())
        );
        assert_eq!(pick_endpoint(None, None), None);
    }

    #[tokio::test]
    async fn capacity_refusal_uses_the_code_not_human_wording() {
        let router = axum::Router::new().route("/v1/apply", axum::routing::post(|| async {
            (axum::http::StatusCode::UNPROCESSABLE_ENTITY, axum::Json(serde_json::json!({
                "error": {"code": "no_eligible_nodes", "app_id": {"name": "capacity-0", "namespace": "rbtest-capacity"}}
            })))
        }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let client = BunClient::new(&format!("http://{}", listener.local_addr().unwrap()));
        let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        let config = Config::parse(
            "[app.capacity-0]\nimage = \"busybox\"\nnamespace = \"rbtest-capacity\"\n",
        )
        .unwrap();
        let error = client
            .apply_capacity_with_lease(&config, "lease-capacity")
            .await
            .unwrap_err();
        server.abort();
        assert!(matches!(error, RelishError::SchedulingRejected(
            crate::meat::scheduler::ScheduleError::NoEligibleNodes { app_id }
        ) if app_id == crate::meat::AppId::new("capacity-0", "rbtest-capacity")));
    }

    #[tokio::test]
    async fn malformed_capacity_refusals_remain_api_failures() {
        for payload in [
            serde_json::json!({"error":"no eligible nodes"}),
            serde_json::json!({"error":{"code":"no_eligible_nodes"}}),
            serde_json::json!({"error":{"code":"mystery", "app_id":{"name":"a", "namespace":"default"}}}),
        ] {
            let router = axum::Router::new().route(
                "/v1/apply",
                axum::routing::post(move || {
                    let payload = payload.clone();
                    async move {
                        (
                            axum::http::StatusCode::UNPROCESSABLE_ENTITY,
                            axum::Json(payload),
                        )
                    }
                }),
            );
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let client = BunClient::new(&format!("http://{}", listener.local_addr().unwrap()));
            let server = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
            let config = Config::parse("[app.a]\nimage = \"busybox\"\n").unwrap();
            let result = client
                .apply_capacity_with_lease(&config, "lease-capacity")
                .await;
            server.abort();
            assert!(matches!(
                result,
                Err(RelishError::ApiError { status: 422, .. })
            ));
        }
    }

    #[tokio::test]
    async fn alerts_refuse_missing_malformed_or_unknown_status_evidence() {
        for payload in [
            serde_json::json!({}),
            serde_json::json!({"alerts": null}),
            serde_json::json!({"alerts": {}}),
            serde_json::json!({"alerts": [{}]}),
            serde_json::json!({"alerts": [{"rule_name":"cpu", "state":"mystery", "severity":"Critical", "description":"hot", "since":1}]}),
            serde_json::json!({"alerts": [{"rule_name":"cpu", "state":"firing", "description":"hot", "since":1}]}),
        ] {
            let body = payload.clone();
            let app = axum::Router::new().route(
                "/v1/alerts",
                axum::routing::get(move || {
                    let body = body.clone();
                    async move { axum::Json(body) }
                }),
            );
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let client = BunClient::new(&format!("http://{}", listener.local_addr().unwrap()));
            let server = tokio::spawn(async move {
                axum::serve(listener, app).await.unwrap();
            });
            let result = client.alerts().await;
            server.abort();
            assert!(
                result.is_err(),
                "accepted invalid alert evidence: {payload}"
            );
        }
    }

    #[tokio::test]
    async fn alerts_preserve_empty_inventory_and_labelled_firing_status() {
        use crate::mayo::alert::{AlertPhase, AlertSeverity};

        for payload in [
            serde_json::json!({"alerts": []}),
            serde_json::json!({"alerts": [{
                "rule_name": "cpu", "state": "firing", "severity": "Critical",
                "description": "hot", "since": 42, "labels": {"app": "web"}
            }]}),
        ] {
            let body = payload.clone();
            let app = axum::Router::new().route(
                "/v1/alerts",
                axum::routing::get(move || {
                    let body = body.clone();
                    async move { axum::Json(body) }
                }),
            );
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let client = BunClient::new(&format!("http://{}", listener.local_addr().unwrap()));
            let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
            let alerts = client.alerts().await.unwrap();
            server.abort();
            assert_eq!(serde_json::to_value(&alerts).unwrap(), payload["alerts"]);
            if let Some(alert) = alerts.first() {
                assert_eq!(alert.state, AlertPhase::Firing);
                assert_eq!(alert.severity, AlertSeverity::Critical);
                assert_eq!(alert.labels["app"], "web");
                assert_eq!(alert.since, Some(42));
            }
        }
    }

    #[test]
    fn endpoint_validation_allows_loopback_http_and_requires_remote_https() {
        assert!(validate_endpoint("http://127.0.0.1:9117").is_ok());
        assert!(validate_endpoint("http://[::1]:9117").is_ok());
        assert!(validate_endpoint("http://localhost:9117").is_err());
        assert!(validate_endpoint("https://node-01.example:9117").is_ok());
        assert!(validate_endpoint("http://node-01.example:9117").is_err());
        assert!(validate_endpoint("ftp://node-01.example:9117").is_err());
        assert!(validate_endpoint("https://user:secret@node-01.example:9117").is_err());
    }
}
