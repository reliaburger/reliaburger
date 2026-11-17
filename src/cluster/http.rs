//! How this node reaches peer HTTP APIs.
//!
//! Node-to-node calls (placement polling, metric/log fan-out, batch dispatch,
//! build delegation, upgrade directives) all target another node's agent API.
//! When the cluster runs mTLS that API speaks HTTPS with a node certificate,
//! so the caller needs two things: the `https` scheme and a client that trusts
//! the cluster CA. `ClusterHttp` bundles both, built once at startup and shared
//! so every peer call is consistent.
//!
//! It presents the node certificate so the transport is mutually
//! authenticated. Route authorisation remains the service token's job.

use reqwest::Client;

/// The scheme and client this node uses for peer agent-API calls.
#[derive(Clone)]
pub struct ClusterHttp {
    scheme: &'static str,
    client: Client,
    /// The internal service token, presented as `Authorization: Bearer` on
    /// requests that go through [`Self::get`]. `None` when the node has no
    /// master key (single-node / keyless). Route authorisation on the peer
    /// still checks it; without it a routable Pickle read or a self-upgrade
    /// binary fetch 401s (B2).
    bearer: Option<String>,
}

impl ClusterHttp {
    /// Plaintext `http` with a default client (mTLS off, or single-node).
    pub fn plaintext() -> Self {
        Self {
            scheme: "http",
            client: crate::http_client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .expect("valid plaintext cluster HTTP configuration"),
            bearer: None,
        }
    }

    /// Plaintext HTTP with an explicitly configured client, for local control
    /// transports which need bounded or non-redirecting request behaviour.
    pub fn plaintext_with_client(client: Client) -> Self {
        Self {
            scheme: "http",
            client,
            bearer: None,
        }
    }

    /// `https` with a CA-trusting client (mTLS on).
    pub fn secure(client: Client) -> Self {
        Self {
            scheme: "https",
            client,
            bearer: None,
        }
    }

    /// Attach the internal service token as a default bearer for
    /// [`Self::get`]. Builder-style so callers that don't need it (the common
    /// case) stay unchanged.
    pub fn with_bearer(mut self, bearer: Option<String>) -> Self {
        self.bearer = bearer;
        self
    }

    /// The service-token bearer, if one was attached.
    pub fn bearer(&self) -> Option<&str> {
        self.bearer.as_deref()
    }

    /// The shared reqwest client.
    pub fn client(&self) -> &Client {
        &self.client
    }

    /// A `GET` to `url` carrying the attached bearer (if any). Callers that
    /// build their own requests can read [`Self::bearer`] and apply it
    /// themselves; this is the convenience path for a plain authenticated GET.
    pub fn get(&self, url: &str) -> reqwest::RequestBuilder {
        let request = self.client.get(url);
        match &self.bearer {
            Some(token) => request.bearer_auth(token),
            None => request,
        }
    }

    /// The URL scheme, `http` or `https`.
    pub fn scheme(&self) -> &'static str {
        self.scheme
    }

    /// Build a peer URL from an `authority` (host or `ip:port`) and a `path`
    /// (which must include its leading `/`, or be empty).
    ///
    /// `path` goes in as written, so it must be a constant or come from
    /// [`peer_path`]: never interpolate a caller's value into it.
    pub fn url(&self, authority: &str, path: &str) -> String {
        format!("{}://{authority}{path}", self.scheme)
    }
}

/// Why [`peer_path`] refused to build a path.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PeerPathError {
    /// An empty segment would put `//` in the path.
    #[error("a path segment is empty")]
    EmptySegment,
    /// `.` or `..`, which a URL resolves away.
    #[error("path segment {0:?} is a dot segment")]
    DotSegment(String),
    /// A URL parser drops tabs and newlines silently, so `.\t.` would become
    /// `..`; every control character is refused.
    #[error("path segment {0:?} holds a control character")]
    ControlCharacter(String),
    /// The URL encoder itself refused; it never does for an `http` base.
    #[error("the URL encoder refused the path")]
    Encoder,
}

/// Every peer path is encoded against this base; only its path is kept.
const ENCODING_BASE: &str = "http://peer.invalid/";

/// Build the path of a peer API request from its segments, each one
/// percent-encoded on its own: `["v1", "test", "leases", id]` becomes
/// `/v1/test/leases/{id}`, whatever `id` holds.
///
/// A segment can't add, remove or climb out of a path level. `/`, `\`, `%`,
/// `?` and `#` are encoded (an `http` URL treats `\` as `/`), and an empty
/// segment, a dot segment or a control character is refused. Every node to
/// node request that carries a caller's value in its path builds the path
/// here.
pub fn peer_path<S: AsRef<str>>(segments: &[S]) -> Result<String, PeerPathError> {
    for segment in segments {
        let segment = segment.as_ref();
        if segment.is_empty() {
            return Err(PeerPathError::EmptySegment);
        }
        if segment == "." || segment == ".." {
            return Err(PeerPathError::DotSegment(segment.to_string()));
        }
        if segment.chars().any(char::is_control) {
            return Err(PeerPathError::ControlCharacter(segment.to_string()));
        }
    }
    let mut url = url::Url::parse(ENCODING_BASE).map_err(|_| PeerPathError::Encoder)?;
    url.path_segments_mut()
        .map_err(|()| PeerPathError::Encoder)?
        .pop_if_empty()
        .extend(segments);
    Ok(url.path().to_string())
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

    #[test]
    fn peer_path_joins_plain_segments() {
        assert_eq!(
            peer_path(&["v1", "test", "leases", "lease-1"]).unwrap(),
            "/v1/test/leases/lease-1"
        );
    }

    /// An `http` URL treats `\` as `/` and then resolves `..`, so an
    /// unencoded `..\..\cluster\x` would climb two levels.
    #[test]
    fn peer_path_encodes_backslashes_slashes_and_percent_signs() {
        let path = peer_path(&["v1", "deploys", "history", "..\\..\\token\\list"]).unwrap();
        assert_eq!(path, "/v1/deploys/history/..%5C..%5Ctoken%5Clist");
        assert_eq!(
            peer_path(&["v1", "logs", "a/../b", "%2e%2e"]).unwrap(),
            "/v1/logs/a%2F..%2Fb/%252e%252e"
        );
        assert_eq!(peer_path(&["v1", "x", "a?b#c"]).unwrap(), "/v1/x/a%3Fb%23c");
    }

    /// What reqwest sends is what the peer routes on: the encoded path
    /// survives being parsed again with the scheme and authority.
    #[test]
    fn a_peer_path_keeps_its_levels_once_parsed_as_a_url() {
        let path = peer_path(&["v1", "test", "leases", "..\\..\\token\\list"]).unwrap();
        let url = url::Url::parse(&ClusterHttp::plaintext().url("10.0.0.1:9117", &path)).unwrap();
        let segments: Vec<&str> = url.path_segments().unwrap().collect();
        assert_eq!(segments, ["v1", "test", "leases", "..%5C..%5Ctoken%5Clist"]);
    }

    #[test]
    fn peer_path_refuses_dot_and_empty_segments() {
        for segment in [".", ".."] {
            assert_eq!(
                peer_path(&["v1", "test", "leases", segment]),
                Err(PeerPathError::DotSegment(segment.to_string()))
            );
        }
        assert_eq!(
            peer_path(&["v1", "", "leases"]),
            Err(PeerPathError::EmptySegment)
        );
    }

    /// A URL parser strips tabs and newlines, which would turn `.\t.` into
    /// `..` after the dot check had passed it.
    #[test]
    fn peer_path_refuses_tabs_newlines_and_other_control_characters() {
        for segment in [".\t.", ".\n.", "a\rb", "a\u{0}b", "a\u{7f}b", "a\u{85}b"] {
            assert_eq!(
                peer_path(&["v1", "test", "leases", segment]),
                Err(PeerPathError::ControlCharacter(segment.to_string())),
                "{segment:?}"
            );
        }
    }

    #[test]
    fn without_a_bearer_none_is_reported() {
        assert!(ClusterHttp::plaintext().bearer().is_none());
    }

    #[test]
    fn with_bearer_attaches_the_token() {
        let h = ClusterHttp::plaintext().with_bearer(Some("rbrg_service".to_string()));
        assert_eq!(h.bearer(), Some("rbrg_service"));
    }

    /// B2: a `get` built from a bearer-carrying `ClusterHttp` sends the
    /// `Authorization: Bearer` header, so a routable Pickle read (self-upgrade
    /// binary fetch) authenticates instead of 401ing. Driven through a raw TCP
    /// capture server so we observe the wire request.
    #[tokio::test]
    async fn get_sends_the_bearer_on_the_wire() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let capture = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 4096];
            let n = socket.read(&mut buf).await.unwrap();
            let request = String::from_utf8_lossy(&buf[..n]).to_string();
            let _ = socket
                .write_all(b"HTTP/1.1 404 Not Found\r\ncontent-length: 0\r\n\r\n")
                .await;
            request
        });

        let http = ClusterHttp::plaintext().with_bearer(Some("rbrg_service".to_string()));
        let _ = http.get(&format!("http://{addr}/v2/blobs")).send().await;

        let request = capture.await.unwrap().to_lowercase();
        assert!(
            request.contains("authorization: bearer rbrg_service"),
            "request did not carry the bearer:\n{request}"
        );
    }
}
