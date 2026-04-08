/// HTTP client for talking to the Bun agent.
///
/// Sends requests to the Bun local API at `http://127.0.0.1:9117`.
/// Used by Relish CLI commands when a live agent is available.
///
/// The `apply` endpoint returns Server-Sent Events, which the client
/// reads incrementally — printing progress to stderr and collecting
/// the final result.
use futures_util::StreamExt;

use crate::bun::agent::{
    ApplyEvent, ApplyResult, ChaosState, CouncilStatus, InstanceStatus, NodeStatus,
};
use crate::config::Config;

use super::RelishError;

/// Client for the Bun agent HTTP API.
pub struct BunClient {
    base_url: String,
    client: reqwest::Client,
}

/// Classify a reqwest send error as either a timeout or a connection failure.
fn classify_error(e: reqwest::Error) -> RelishError {
    if e.is_timeout() {
        RelishError::RequestTimeout
    } else {
        RelishError::AgentUnreachable
    }
}

impl BunClient {
    /// Create a client pointing at the given base URL.
    pub fn new(base_url: &str) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(300))
                .build()
                .expect("failed to create HTTP client"),
        }
    }

    /// Get the base URL.
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// Create a client pointing at the default local agent.
    pub fn default_local() -> Self {
        Self::new("http://127.0.0.1:9117")
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
        let url = format!("{}/v1/apply", self.base_url);
        let toml_str = toml::to_string_pretty(config).map_err(|e| RelishError::ApiError {
            status: 0,
            body: format!("failed to serialise config: {e}"),
        })?;

        let response = self
            .client
            .post(&url)
            .body(toml_str)
            .send()
            .await
            .map_err(classify_error)?;

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

    /// Get logs for an app.
    ///
    /// When `follow` is false, returns the (optionally tailed) log output
    /// as a string. When `follow` is true, streams log lines to stdout
    /// via SSE and returns `Ok(String::new())` when the stream ends.
    pub async fn logs(
        &self,
        app: &str,
        namespace: &str,
        tail: Option<usize>,
        follow: bool,
    ) -> Result<String, RelishError> {
        let mut url = format!("{}/v1/logs/{}/{}", self.base_url, app, namespace);

        // Build query string
        let mut params = Vec::new();
        if let Some(n) = tail {
            params.push(format!("tail={n}"));
        }
        if follow {
            params.push("follow=true".to_string());
        }
        if !params.is_empty() {
            url.push('?');
            url.push_str(&params.join("&"));
        }

        let response = self.client.get(&url).send().await.map_err(classify_error)?;

        let status = response.status().as_u16();
        if !response.status().is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(RelishError::ApiError { status, body });
        }

        if follow {
            // Read SSE stream, printing each data: line to stdout
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
                            println!("{}", data.trim());
                        }
                    }
                }
            }

            // Flush remaining buffer
            for line in buffer.lines() {
                if let Some(data) = line.strip_prefix("data:") {
                    println!("{}", data.trim());
                }
            }

            Ok(String::new())
        } else {
            let json: serde_json::Value =
                response.json().await.map_err(|e| RelishError::ApiError {
                    status: 0,
                    body: format!("failed to parse response: {e}"),
                })?;

            Ok(json["logs"].as_str().unwrap_or("").to_string())
        }
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

    /// Join an existing cluster.
    pub async fn join(&self, token: &str, addr: &str) -> Result<String, RelishError> {
        let url = format!("{}/v1/cluster/join", self.base_url);
        let response = self
            .client
            .post(&url)
            .json(&serde_json::json!({ "token": token, "addr": addr }))
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

        Ok(json["message"].as_str().unwrap_or("joined").to_string())
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
    ) -> Result<String, RelishError> {
        let url = format!("{}/v1/chaos/partition", self.base_url);
        let response = self
            .client
            .post(&url)
            .json(&serde_json::json!({ "peers": peers, "duration_secs": duration_secs }))
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
}
