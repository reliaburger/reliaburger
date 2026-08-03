/// HTTP client for talking to the Bun agent.
///
/// Sends requests to the Bun local API at `http://127.0.0.1:9117`.
/// Used by Relish CLI commands when a live agent is available.
///
/// The `apply` endpoint returns Server-Sent Events, which the client
/// reads incrementally — printing progress to stderr and collecting
/// the final result.
use futures_util::StreamExt;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

use crate::bun::agent::{
    ApplyEvent, ApplyResult, ChaosState, CouncilStatus, InstanceStatus, NodeStatus,
};
use crate::config::Config;

use super::RelishError;

/// Client for the Bun agent HTTP API.
#[derive(Clone)]
pub struct BunClient {
    base_url: String,
    client: reqwest::Client,
    token: Option<String>,
}

/// Options for fetching or streaming logs.
///
/// `grep` and `start` are also sent server-side where the endpoint
/// supports them; `json_field` is client-side only (there is no
/// server-side equivalent).
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
        let ca_pem = resolve_ca_cert().and_then(|ca_path| match std::fs::read(&ca_path) {
            Ok(pem) => Some(pem),
            Err(e) => {
                eprintln!(
                    "relish: warning — could not load --ca-cert {}: {e}",
                    ca_path.display()
                );
                None
            }
        });
        Self::build(base_url, token, ca_pem.as_deref())
    }

    /// Build a client with an explicit cluster CA (PEM), if any.
    ///
    /// Split out from [`new_with_token`] so the TLS trust configuration can be
    /// exercised without the process-global `--ca-cert` state.
    fn build(base_url: &str, token: Option<&str>, ca_pem: Option<&[u8]>) -> Self {
        let mut builder = reqwest::Client::builder().timeout(std::time::Duration::from_secs(300));
        if let Some(t) = token {
            let mut headers = reqwest::header::HeaderMap::new();
            if let Ok(mut value) = reqwest::header::HeaderValue::from_str(&format!("Bearer {t}")) {
                value.set_sensitive(true);
                headers.insert(reqwest::header::AUTHORIZATION, value);
                builder = builder.default_headers(headers);
            }
        }
        // Under mTLS the API cert is issued for the node id, not "127.0.0.1",
        // so we trust the cluster CA and skip the hostname check. For that to
        // be safe the cluster CA must be the *only* trust anchor: reqwest keeps
        // the built-in webpki/system roots by default, so without disabling
        // them any certificate chaining to a public CA would be accepted while
        // the hostname check is off — a MITM could then present a valid public
        // cert and harvest the bearer token. `tls_built_in_root_certs(false)`
        // makes the pin real, mirroring the cluster HTTP client's empty root
        // store (`sesame::mtls`).
        if let Some(pem) = ca_pem {
            match reqwest::Certificate::from_pem(pem) {
                Ok(cert) => {
                    builder = builder
                        .tls_built_in_root_certs(false)
                        .add_root_certificate(cert)
                        .danger_accept_invalid_hostnames(true);
                }
                Err(e) => {
                    eprintln!("relish: warning — invalid --ca-cert PEM: {e}");
                }
            }
        }
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            client: builder.build().expect("failed to create HTTP client"),
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
            token: self.token.clone(),
        }
    }

    /// The underlying HTTP client, pre-configured with the resolved bearer
    /// token and cluster-CA trust. Used for requests to an explicit URL that
    /// isn't relative to `base_url` — e.g. a Pickle registry `/v2` upload on a
    /// different port (M21) — so they, too, are authenticated and TLS-trusting.
    pub fn http(&self) -> &reqwest::Client {
        &self.client
    }

    /// Create a client pointing at the default local agent. Uses HTTPS when a
    /// cluster CA cert is configured (`--ca-cert` / `RELIABURGER_CA_CERT`). An
    /// explicit `--endpoint` / `RELIABURGER_ENDPOINT` replaces the whole URL.
    pub fn default_local() -> Self {
        if let Some(endpoint) = resolve_endpoint() {
            return Self::new(&endpoint);
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
        self.client
            .get(&url)
            .timeout(std::time::Duration::from_secs(5))
            .send()
            .await
            .map_err(|_| RelishError::AgentUnreachable)?;
        Ok(())
    }

    /// Deploy workloads from a config, streaming progress to stderr.
    ///
    /// The agent returns Server-Sent Events. Each `data:` line
    /// contains a JSON `ApplyEvent`. Progress events are printed to
    /// stderr as they arrive; the final `Complete` event is returned
    /// as an `ApplyResult`.
    pub async fn apply(&self, config: &Config) -> Result<ApplyResult, RelishError> {
        self.apply_request(config, None, false).await
    }

    /// Deploy apps under a server-owned Phase 15 resource lease.
    pub async fn apply_with_lease(
        &self,
        config: &Config,
        lease_id: &str,
    ) -> Result<ApplyResult, RelishError> {
        self.apply_request(config, Some(lease_id), false).await
    }

    /// Deploy a deliberately saturating app under both lease and capacity policy.
    pub async fn apply_capacity_with_lease(
        &self,
        config: &Config,
        lease_id: &str,
    ) -> Result<ApplyResult, RelishError> {
        self.apply_request(config, Some(lease_id), true).await
    }

    async fn apply_request(
        &self,
        config: &Config,
        lease_id: Option<&str>,
        capacity_probe: bool,
    ) -> Result<ApplyResult, RelishError> {
        let url = format!("{}/v1/apply", self.base_url);
        let toml_str = toml::to_string_pretty(config).map_err(|e| RelishError::ApiError {
            status: 0,
            body: format!("failed to serialise config: {e}"),
        })?;

        let mut request = self.client.post(&url).body(toml_str);
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
            return Err(RelishError::ApiError { status, body });
        }

        // Read the SSE stream
        let mut stream = response.bytes_stream();
        let mut buffer = String::new();
        let mut result = None;

        while let Some(chunk) = stream.next().await {
            let bytes = chunk.map_err(classify_error)?;
            buffer.push_str(&String::from_utf8_lossy(&bytes));

            // Process complete SSE events (separated by double newline)
            while let Some(event_end) = buffer.find("\n\n") {
                let event_text = buffer[..event_end].to_string();
                buffer = buffer[event_end + 2..].to_string();

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
        if let Some(data) = buffer.lines().find_map(|line| line.strip_prefix("data:"))
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
            .client
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
        let mut buffer = String::new();
        while let Some(chunk) = stream.next().await {
            let bytes = chunk.map_err(classify_error)?;
            buffer.push_str(&String::from_utf8_lossy(&bytes));
            while let Some(end) = buffer.find("\n\n") {
                let event_text = buffer[..end].to_string();
                buffer = buffer[end + 2..].to_string();
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

    /// Get status of all instances.
    pub async fn status(&self) -> Result<Vec<InstanceStatus>, RelishError> {
        let url = format!("{}/v1/status", self.base_url);
        let response = self.client.get(&url).send().await.map_err(classify_error)?;

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

    /// List alert statuses as their wire JSON representation.
    pub async fn alerts(&self) -> Result<Vec<serde_json::Value>, RelishError> {
        let value: serde_json::Value = self.get_typed_json("/v1/alerts").await?;
        Ok(value["alerts"].as_array().cloned().unwrap_or_default())
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

    /// Fetch desired application replicas and current scheduler coverage.
    pub async fn desired_apps(
        &self,
    ) -> Result<Vec<crate::bun::diagnostics::DesiredAppEvidence>, RelishError> {
        self.get_typed_json("/v1/diagnostics/apps").await
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

    /// Create a server-owned test resource lease.
    pub async fn create_test_lease(
        &self,
        ttl_seconds: u64,
        namespace: Option<&str>,
    ) -> Result<crate::testkit::lease::TestLease, RelishError> {
        let response = self
            .client
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

    /// Renew an active test resource lease.
    pub async fn renew_test_lease(
        &self,
        lease_id: &str,
        ttl_seconds: u64,
    ) -> Result<crate::testkit::lease::TestLease, RelishError> {
        let response = self
            .client
            .post(format!("{}/v1/test/leases/{lease_id}/renew", self.base_url))
            .json(&serde_json::json!({ "ttl_seconds": ttl_seconds }))
            .send()
            .await
            .map_err(classify_error)?;
        parse_typed_response(response).await
    }

    /// Release a lease and wait for server-confirmed cleanup.
    pub async fn release_test_lease(&self, lease_id: &str) -> Result<(), RelishError> {
        let response = self
            .client
            .delete(format!("{}/v1/test/leases/{lease_id}", self.base_url))
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

    /// Fetch metrics recorded for one app.
    pub async fn app_metrics(
        &self,
        app: &str,
        namespace: &str,
    ) -> Result<crate::mayo::rollup::MetricsQueryResult, RelishError> {
        self.get_typed_json(&format!("/v1/metrics/app/{app}/{namespace}"))
            .await
    }

    async fn get_typed_json<T: serde::de::DeserializeOwned>(
        &self,
        path: &str,
    ) -> Result<T, RelishError> {
        let response = self
            .client
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
        let (stream, _) = tokio_tungstenite::connect_async(request)
            .await
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
            .client
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
            .client
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
        let response = self.client.get(&url).send().await.map_err(classify_error)?;

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
            .client
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
            .client
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
            // Follow mode uses the SSE endpoint (local only). The SSE
            // path does not filter server-side, so filters apply here.
            return self.logs_follow(app, namespace, options).await;
        }

        // Non-follow: try the cross-node query endpoint first.
        // In cluster mode this fans out to all nodes running the app.
        // In single-node mode it queries the local LogStore.
        let url = format!("{}/v1/logs/query/{}/{}", self.base_url, app, namespace);

        if let Ok(response) = self
            .client
            .get(&url)
            .query(&options.query_params())
            .send()
            .await
            && response.status().is_success()
            && let Ok(result) = response.json::<serde_json::Value>().await
        {
            let mut output = String::new();
            if let Some(entries) = result["entries"].as_array() {
                for entry in entries {
                    let line = entry["line"].as_str().unwrap_or("");
                    // Filters also apply client-side: json_field has no
                    // server-side equivalent, and grep re-checking is
                    // harmless when the server already filtered.
                    if options.matches(line) {
                        output.push_str(line);
                        output.push('\n');
                    }
                }
            }

            // Show warnings if any nodes were unreachable
            if let Some(warnings) = result["warnings"].as_array() {
                for w in warnings {
                    if let Some(node_id) = w.get("NodeUnresponsive") {
                        let id = node_id["node_id"].as_str().unwrap_or("unknown");
                        eprintln!("warning: node {id} did not respond");
                    }
                }
            }

            if output.ends_with('\n') {
                output.pop();
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
            .client
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

    /// Follow logs via SSE stream (local node only).
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
            .client
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
        let mut buffer = String::new();

        while let Some(chunk) = stream.next().await {
            let bytes = chunk.map_err(classify_error)?;
            buffer.push_str(&String::from_utf8_lossy(&bytes));

            while let Some(event_end) = buffer.find("\n\n") {
                let event_text = buffer[..event_end].to_string();
                buffer = buffer[event_end + 2..].to_string();

                for line in event_text.lines() {
                    if let Some(data) = line.strip_prefix("data:") {
                        let data = data.trim();
                        if options.matches(data) {
                            println!("{data}");
                        }
                    }
                }
            }
        }

        for line in buffer.lines() {
            if let Some(data) = line.strip_prefix("data:") {
                let data = data.trim();
                if options.matches(data) {
                    println!("{data}");
                }
            }
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
            .client
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

    /// Get cluster node membership.
    pub async fn nodes(&self) -> Result<Vec<NodeStatus>, RelishError> {
        let url = format!("{}/v1/cluster/nodes", self.base_url);
        let response = self.client.get(&url).send().await.map_err(classify_error)?;

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
        let response = self.client.get(&url).send().await.map_err(classify_error)?;

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

    /// Inject a network partition (chaos testing).
    pub async fn inject_partition(
        &self,
        peers: &[String],
        duration_secs: u64,
        acknowledged: bool,
    ) -> Result<crate::smoker::types::FaultSummary, RelishError> {
        let url = format!("{}/v1/chaos/partition", self.base_url);
        let response = self
            .client
            .post(&url)
            .json(&serde_json::json!({
                "peers": peers,
                "duration_secs": duration_secs,
                "acknowledged": acknowledged,
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
        serde_json::from_value(json["fault"].clone()).map_err(|error| RelishError::ApiError {
            status: 0,
            body: format!("partition response omitted its owned fault: {error}"),
        })
    }

    /// Remove all network partitions (chaos testing).
    pub async fn heal_partition(&self) -> Result<String, RelishError> {
        let url = format!("{}/v1/chaos/heal", self.base_url);
        let response = self
            .client
            .post(&url)
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

    /// Query chaos status.
    pub async fn chaos_status(&self) -> Result<ChaosState, RelishError> {
        let url = format!("{}/v1/chaos/status", self.base_url);
        let response = self.client.get(&url).send().await.map_err(classify_error)?;

        let status = response.status().as_u16();
        if !response.status().is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(RelishError::ApiError { status, body });
        }

        let state: ChaosState = response.json().await.map_err(|e| RelishError::ApiError {
            status: 0,
            body: format!("failed to parse response: {e}"),
        })?;
        Ok(state)
    }

    /// Inject a fault (Smoker API).
    pub async fn inject_fault(
        &self,
        request: &crate::smoker::types::FaultRequest,
    ) -> Result<crate::smoker::types::FaultSummary, RelishError> {
        let url = format!("{}/v1/fault", self.base_url);
        let response = self
            .client
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
        let mut request = self.client.delete(&url);
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
            .client
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
    pub async fn clear_faults_by_service(&self, service: &str) -> Result<String, RelishError> {
        let url = format!("{}/v1/fault?service={}", self.base_url, service);
        let response = self
            .client
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
        let response = self.client.get(&url).send().await.map_err(classify_error)?;

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

    /// Resolve a service name to its VIP and backends.
    pub async fn resolve(
        &self,
        name: &str,
    ) -> Result<crate::onion::types::ResolveResponse, RelishError> {
        let url = format!("{}/v1/resolve/{name}", self.base_url);
        let response = self.client.get(&url).send().await.map_err(classify_error)?;

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
        let response = self.client.get(&url).send().await.map_err(classify_error)?;

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
        let response = self.client.get(&url).send().await.map_err(classify_error)?;

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
        let response = self.client.get(&url).send().await.map_err(classify_error)?;

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
            .client
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
        let response = self.client.get(&url).send().await.map_err(classify_error)?;

        let status = response.status().as_u16();
        if !response.status().is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(RelishError::ApiError { status, body });
        }
        response.json().await.map_err(classify_error)
    }

    /// List API tokens from SecurityState.
    pub async fn token_list(&self) -> Result<serde_json::Value, RelishError> {
        let url = format!("{}/v1/token/list", self.base_url);
        let response = self.client.get(&url).send().await.map_err(classify_error)?;

        let status = response.status().as_u16();
        if !response.status().is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(RelishError::ApiError { status, body });
        }

        response.json().await.map_err(|e| RelishError::ApiError {
            status: 0,
            body: format!("failed to parse token list: {e}"),
        })
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
        let url = format!("{}/v1/token/create", self.base_url);
        let response = self
            .client
            .post(&url)
            .json(&serde_json::json!({
                "name": name,
                "role": role,
                "apps": apps,
                "namespaces": namespaces,
                "ttl_days": ttl_days,
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
            .client
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
            .client
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

    /// Rotate or finalise the secret encryption key.
    pub async fn secret_rotate(&self, finalize: bool) -> Result<String, RelishError> {
        let url = format!("{}/v1/secret/rotate", self.base_url);
        let response = self
            .client
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

    /// Sign an image manifest and attach the signature via Raft.
    pub async fn sign_image(&self, image: &str) -> Result<String, RelishError> {
        let url = format!("{}/v1/identity/sign", self.base_url);
        let response = self
            .client
            .post(&url)
            .json(&serde_json::json!({ "digest": image }))
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
            .client
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
        let response = self.client.get(&url).send().await.map_err(classify_error)?;

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

    /// Apply a node-level upgrade directive.
    pub async fn upgrade_apply(
        &self,
        directive: &crate::upgrade::types::UpgradeDirective,
    ) -> Result<(), RelishError> {
        let body = serde_json::to_string(directive).map_err(RelishError::SerialiseJson)?;
        self.post_json("/v1/upgrade/apply", body).await.map(|_| ())
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

    async fn get_json(&self, path: &str) -> Result<serde_json::Value, RelishError> {
        let url = format!("{}{path}", self.base_url);
        let response = self.client.get(&url).send().await.map_err(classify_error)?;
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
            .client
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
        let router = Router::new().route("/v1/health", get(|| async { "ok" }));
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
                            .serve_connection(hyper_util::rt::TokioIo::new(tls), svc)
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

        shutdown.cancel();
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
                    "ok"
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
