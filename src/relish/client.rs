/// HTTP client for talking to the Bun agent.
///
/// Sends requests to the Bun local API at `http://127.0.0.1:9117`.
/// Used by Relish CLI commands when a live agent is available.
use crate::bun::agent::{ApplyResult, InstanceStatus};
use crate::config::Config;

use super::RelishError;

/// Client for the Bun agent HTTP API.
pub struct BunClient {
    base_url: String,
    client: reqwest::Client,
}

impl BunClient {
    /// Create a client pointing at the given base URL.
    pub fn new(base_url: &str) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(10))
                .build()
                .expect("failed to create HTTP client"),
        }
    }

    /// Create a client pointing at the default local agent.
    pub fn default_local() -> Self {
        Self::new("http://127.0.0.1:9117")
    }

    /// Check if the agent is reachable.
    pub async fn health(&self) -> Result<(), RelishError> {
        let url = format!("{}/v1/health", self.base_url);
        self.client
            .get(&url)
            .send()
            .await
            .map_err(|_| RelishError::AgentUnreachable)?;
        Ok(())
    }

    /// Deploy workloads from a config.
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
            .map_err(|_| RelishError::AgentUnreachable)?;

        let status = response.status().as_u16();
        if !response.status().is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(RelishError::ApiError { status, body });
        }

        let result: ApplyResult = response.json().await.map_err(|e| RelishError::ApiError {
            status: 0,
            body: format!("failed to parse response: {e}"),
        })?;

        Ok(result)
    }

    /// Get status of all instances.
    pub async fn status(&self) -> Result<Vec<InstanceStatus>, RelishError> {
        let url = format!("{}/v1/status", self.base_url);
        let response = self
            .client
            .get(&url)
            .send()
            .await
            .map_err(|_| RelishError::AgentUnreachable)?;

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
            .map_err(|_| RelishError::AgentUnreachable)?;

        let status = response.status().as_u16();
        if !response.status().is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(RelishError::ApiError { status, body });
        }

        Ok(())
    }

    /// Get logs for an app.
    pub async fn logs(&self, app: &str, namespace: &str) -> Result<String, RelishError> {
        let url = format!("{}/v1/logs/{}/{}", self.base_url, app, namespace);
        let response = self
            .client
            .get(&url)
            .send()
            .await
            .map_err(|_| RelishError::AgentUnreachable)?;

        let status = response.status().as_u16();
        if !response.status().is_success() {
            let body = response.text().await.unwrap_or_default();
            return Err(RelishError::ApiError { status, body });
        }

        let json: serde_json::Value = response.json().await.map_err(|e| RelishError::ApiError {
            status: 0,
            body: format!("failed to parse response: {e}"),
        })?;

        Ok(json["logs"].as_str().unwrap_or("").to_string())
    }
}
