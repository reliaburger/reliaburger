/// Types and configuration for the Wrapper ingress proxy.
use std::net::SocketAddr;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Errors from Wrapper operations.
#[derive(Debug, thiserror::Error)]
pub enum WrapperError {
    #[error("failed to bind ingress listener on {addr}: {reason}")]
    BindFailed { addr: SocketAddr, reason: String },

    #[error("proxy request failed: {0}")]
    ProxyFailed(String),

    #[error("WebSocket upgrade failed: {0}")]
    WebSocketUpgradeFailed(String),

    #[error("wrapper not running")]
    NotRunning,
}

/// How a route terminates TLS.
///
/// Parsed from `IngressSpec.tls`. A route with a mode other than
/// [`TlsMode::Disabled`] must be reached over HTTPS: the plain-HTTP
/// listener redirects it (see the proxy handler), it is never served in
/// the clear. Unsupported modes (`auto`, `acme`) are rejected up front by
/// [`TlsMode::parse`] rather than silently downgraded to plaintext.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum TlsMode {
    /// No TLS. The route is served over plain HTTP.
    #[default]
    Disabled,
    /// The ingress certificate is issued from the cluster's Sesame Ingress
    /// CA (air-gapped clusters). Clients must trust the cluster root.
    Cluster,
    /// The operator supplies the certificate and key explicitly, via
    /// `WrapperConfig::tls_cert_path` / `tls_key_path`.
    Explicit,
}

impl TlsMode {
    /// Parse a `tls` field into a mode.
    ///
    /// `None`/`"none"`/`"off"` disable TLS. `"cluster"` uses the cluster
    /// Ingress CA. `"explicit"` uses operator-supplied certs. Anything else
    /// (including the not-yet-implemented `"auto"`/`"acme"`) is a config
    /// error, so a TLS-requiring route can never quietly fall back to
    /// plaintext.
    pub fn parse(value: Option<&str>) -> Result<Self, TlsModeError> {
        match value.map(|v| v.trim().to_ascii_lowercase()).as_deref() {
            None | Some("") | Some("none") | Some("off") | Some("disabled") => {
                Ok(TlsMode::Disabled)
            }
            Some("cluster") => Ok(TlsMode::Cluster),
            Some("explicit") => Ok(TlsMode::Explicit),
            Some(other) => Err(TlsModeError::Unsupported(other.to_string())),
        }
    }

    /// Whether this route requires HTTPS (so plain HTTP must redirect).
    pub fn requires_tls(self) -> bool {
        !matches!(self, TlsMode::Disabled)
    }
}

/// An unsupported or unimplemented TLS mode was requested in the config.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum TlsModeError {
    #[error("unsupported ingress tls mode {0:?}: expected \"cluster\", \"explicit\", or \"none\"")]
    Unsupported(String),
}

/// Configuration for the Wrapper proxy.
#[derive(Debug, Clone)]
pub struct WrapperConfig {
    /// HTTP listen port (default: 80).
    pub http_port: u16,
    /// HTTPS listen port (default: 443).
    pub https_port: u16,
    /// Maximum concurrent proxy connections (default: 10,000).
    pub max_connections: usize,
    /// Number of tokio worker threads for the proxy runtime (default: 4).
    pub worker_threads: usize,
    /// Path to a TLS certificate PEM file. If not set, a self-signed
    /// cert is generated on startup.
    pub tls_cert_path: Option<PathBuf>,
    /// Path to a TLS private key PEM file.
    pub tls_key_path: Option<PathBuf>,
    /// Maximum size, in bytes, of a request body the proxy will forward.
    /// A request whose body exceeds this is rejected with 413. Response
    /// bodies stream and are not bounded by this (ING3).
    pub max_request_body_bytes: usize,
    /// Maximum number of TLS handshakes in progress at once. Slow or
    /// malicious handshakers beyond this are dropped rather than allowed
    /// to pile up tasks (ING2).
    pub max_tls_handshakes: usize,
    /// How long a single TLS handshake may take before the connection is
    /// dropped (ING2).
    pub tls_handshake_timeout: std::time::Duration,
}

impl Default for WrapperConfig {
    fn default() -> Self {
        Self {
            http_port: 80,
            https_port: 443,
            max_connections: 10_000,
            worker_threads: 4,
            tls_cert_path: None,
            tls_key_path: None,
            max_request_body_bytes: 10 * 1024 * 1024,
            max_tls_handshakes: 512,
            tls_handshake_timeout: std::time::Duration::from_secs(10),
        }
    }
}

/// Load balancing strategy for a backend pool.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum LoadBalanceStrategy {
    /// Distribute requests evenly across healthy backends.
    #[default]
    RoundRobin,
    /// Route to the backend with fewest active connections.
    LeastConnections,
}

/// Rate limiting configuration for a route.
#[derive(Debug, Clone)]
pub struct RateLimitConfig {
    /// Requests per second per client IP.
    pub rps: u32,
    /// Maximum burst size.
    pub burst: u32,
}

/// Summary of a route for the API/CLI.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouteInfo {
    pub host: String,
    pub path: String,
    pub app_name: String,
    pub healthy_backends: usize,
    pub total_backends: usize,
    pub websocket: bool,
}
