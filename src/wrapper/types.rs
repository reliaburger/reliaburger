/// Types and configuration for the Wrapper ingress proxy.
use std::net::SocketAddr;

use serde::{Deserialize, Serialize};

/// Errors from Wrapper operations.
#[derive(Debug, thiserror::Error)]
pub enum WrapperError {
    #[error("failed to bind ingress listener on {addr}: {reason}")]
    BindFailed { addr: SocketAddr, reason: String },

    #[error("proxy request failed: {0}")]
    ProxyFailed(String),

    #[error("wrapper not running")]
    NotRunning,
}

/// Configuration for the Wrapper proxy.
#[derive(Debug, Clone)]
pub struct WrapperConfig {
    /// HTTP listen port (default: 80).
    pub http_port: u16,
    /// Maximum concurrent proxy connections (default: 10,000).
    pub max_connections: usize,
    /// Number of tokio worker threads for the proxy runtime (default: 4).
    pub worker_threads: usize,
}

impl Default for WrapperConfig {
    fn default() -> Self {
        Self {
            http_port: 80,
            max_connections: 10_000,
            worker_threads: 4,
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
