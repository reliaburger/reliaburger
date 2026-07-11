//! How this node reaches peer HTTP APIs.
//!
//! Node-to-node calls (placement polling, metric/log fan-out, batch dispatch,
//! build delegation, upgrade directives) all target another node's agent API.
//! When the cluster runs mTLS that API speaks HTTPS with a node certificate,
//! so the caller needs two things: the `https` scheme and a client that trusts
//! the cluster CA. `ClusterHttp` bundles both, built once at startup and shared
//! so every peer call is consistent.
//!
//! It carries no client certificate: node-to-node calls still authenticate
//! with the service token, which now rides inside TLS.

use reqwest::Client;

/// The scheme and client this node uses for peer agent-API calls.
#[derive(Clone)]
pub struct ClusterHttp {
    scheme: &'static str,
    client: Client,
}

impl ClusterHttp {
    /// Plaintext `http` with a default client (mTLS off, or single-node).
    pub fn plaintext() -> Self {
        Self {
            scheme: "http",
            client: Client::new(),
        }
    }

    /// `https` with a CA-trusting client (mTLS on).
    pub fn secure(client: Client) -> Self {
        Self {
            scheme: "https",
            client,
        }
    }

    /// The shared reqwest client.
    pub fn client(&self) -> &Client {
        &self.client
    }

    /// The URL scheme, `http` or `https`.
    pub fn scheme(&self) -> &'static str {
        self.scheme
    }

    /// Build a peer URL from an `authority` (host or `ip:port`) and a `path`
    /// (which must include its leading `/`, or be empty).
    pub fn url(&self, authority: &str, path: &str) -> String {
        format!("{}://{authority}{path}", self.scheme)
    }
}

impl Default for ClusterHttp {
    fn default() -> Self {
        Self::plaintext()
    }
}

impl std::fmt::Debug for ClusterHttp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClusterHttp")
            .field("scheme", &self.scheme)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plaintext_builds_http_urls() {
        let h = ClusterHttp::plaintext();
        assert_eq!(h.scheme(), "http");
        assert_eq!(
            h.url("10.0.0.1:9117", "/v1/health"),
            "http://10.0.0.1:9117/v1/health"
        );
    }

    #[test]
    fn secure_builds_https_urls() {
        let h = ClusterHttp::secure(Client::new());
        assert_eq!(h.scheme(), "https");
        assert_eq!(
            h.url("node-2:9117", "/v1/placements/n2"),
            "https://node-2:9117/v1/placements/n2"
        );
    }
}
