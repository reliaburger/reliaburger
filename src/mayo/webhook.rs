//! Alert webhook delivery.
//!
//! Constructs JSON payloads for alert state transitions and delivers
//! them to configured HTTP endpoints with optional HMAC-SHA256 signing.
//! Failed deliveries are retried 3 times with exponential backoff
//! (1s, 5s, 25s).

use std::collections::HashMap;
use std::time::{Duration, SystemTime};

use ring::hmac;
use serde::Serialize;

use crate::config::node::AlertDestination;
use crate::mayo::alert::{AlertSeverity, AlertTransition, TransitionKind};

// ---------------------------------------------------------------------------
// Webhook payload types
// ---------------------------------------------------------------------------

/// Top-level webhook payload matching the design doc spec.
#[derive(Debug, Clone, Serialize)]
pub struct WebhookPayload {
    pub version: &'static str,
    pub alert: WebhookAlert,
    pub cluster: String,
    pub timestamp: u64,
}

/// Alert details within the webhook payload.
#[derive(Debug, Clone, Serialize)]
pub struct WebhookAlert {
    pub name: String,
    pub severity: String,
    pub status: String,
    pub message: String,
    pub value: Option<f64>,
    pub fired_at: Option<u64>,
}

// ---------------------------------------------------------------------------
// HMAC signing
// ---------------------------------------------------------------------------

/// Sign a payload body with HMAC-SHA256 and return the header value.
///
/// Format: `sha256={hex_digest}`. Set as `X-Mayo-Signature-256`.
pub fn sign_payload(secret: &str, body: &[u8]) -> String {
    let key = hmac::Key::new(hmac::HMAC_SHA256, secret.as_bytes());
    let tag = hmac::sign(&key, body);
    format!("sha256={}", hex::encode(tag.as_ref()))
}

/// Verify an HMAC-SHA256 signature against a payload body.
pub fn verify_signature(secret: &str, body: &[u8], signature: &str) -> bool {
    let Some(hex_sig) = signature.strip_prefix("sha256=") else {
        return false;
    };
    let Ok(expected) = hex::decode(hex_sig) else {
        return false;
    };
    let key = hmac::Key::new(hmac::HMAC_SHA256, secret.as_bytes());
    hmac::verify(&key, body, &expected).is_ok()
}

// ---------------------------------------------------------------------------
// Webhook dispatcher
// ---------------------------------------------------------------------------

/// Dispatches alert transitions to configured webhook destinations.
#[derive(Clone)]
pub struct WebhookDispatcher {
    client: reqwest::Client,
    destinations: Vec<AlertDestination>,
    cluster_name: String,
}

impl WebhookDispatcher {
    /// Create a new dispatcher.
    pub fn new(
        client: reqwest::Client,
        destinations: Vec<AlertDestination>,
        cluster_name: String,
    ) -> Self {
        Self {
            client,
            destinations,
            cluster_name,
        }
    }

    /// Dispatch a transition to all matching destinations.
    pub async fn dispatch(&self, transition: &AlertTransition) {
        let payload = self.build_payload(transition);
        let body = match serde_json::to_vec(&payload) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("mayo: failed to serialise webhook payload: {e}");
                return;
            }
        };

        for dest in &self.destinations {
            if !severity_matches(dest, transition.severity) {
                continue;
            }
            self.send_with_retry(dest, &body).await;
        }
    }

    /// Build the webhook payload from a transition.
    fn build_payload(&self, t: &AlertTransition) -> WebhookPayload {
        let now = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        let status = match t.kind {
            TransitionKind::Firing => "firing",
            TransitionKind::Resolved => "resolved",
        };

        WebhookPayload {
            version: "1",
            alert: WebhookAlert {
                name: t.rule_name.clone(),
                severity: format!("{:?}", t.severity).to_lowercase(),
                status: status.to_string(),
                message: t.description.clone(),
                value: t.value,
                fired_at: t.fired_at.and_then(|s| {
                    s.duration_since(SystemTime::UNIX_EPOCH)
                        .ok()
                        .map(|d| d.as_secs())
                }),
            },
            cluster: self.cluster_name.clone(),
            timestamp: now,
        }
    }

    /// Send to one destination with 3 retries (1s, 5s, 25s backoff).
    async fn send_with_retry(&self, dest: &AlertDestination, body: &[u8]) {
        let delays = [1, 5, 25];
        for (attempt, delay) in delays.iter().enumerate() {
            match self.send_once(dest, body).await {
                Ok(()) => return,
                Err(e) => {
                    eprintln!(
                        "mayo: webhook attempt {} to {} failed: {}",
                        attempt + 1,
                        dest.url,
                        e
                    );
                    if attempt < delays.len() - 1 {
                        tokio::time::sleep(Duration::from_secs(*delay)).await;
                    }
                }
            }
        }
        eprintln!(
            "mayo: webhook delivery to {} failed after 3 attempts",
            dest.url
        );
    }

    /// Send a single HTTP POST to a webhook destination.
    async fn send_once(&self, dest: &AlertDestination, body: &[u8]) -> Result<(), String> {
        let mut req = self
            .client
            .post(&dest.url)
            .header("Content-Type", "application/json")
            .body(body.to_vec());

        if let Some(ref secret) = dest.secret {
            let sig = sign_payload(secret, body);
            req = req.header("X-Mayo-Signature-256", sig);
        }

        let resp = req.send().await.map_err(|e| e.to_string())?;
        if resp.status().is_success() {
            Ok(())
        } else {
            Err(format!("HTTP {}", resp.status()))
        }
    }
}

/// Check if a destination's severity filter matches the alert severity.
///
/// An empty filter matches all severities.
fn severity_matches(dest: &AlertDestination, severity: AlertSeverity) -> bool {
    if dest.severity.is_empty() {
        return true;
    }
    let sev_str = match severity {
        AlertSeverity::Critical => "critical",
        AlertSeverity::Warning => "warning",
    };
    dest.severity.iter().any(|s| s == sev_str)
}

// ---------------------------------------------------------------------------
// Latest values helper
// ---------------------------------------------------------------------------

/// Gather the latest metric values from a MayoStore for alert evaluation.
///
/// Queries the most recent value per metric name from the last 120
/// seconds, then computes derived percentage metrics needed by the
/// default alert rules.
pub async fn gather_latest_values(store: &crate::mayo::store::MayoStore) -> HashMap<String, f64> {
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let window_start = now.saturating_sub(120);

    let mut values: HashMap<String, f64> = HashMap::new();

    let sql = format!(
        "SELECT timestamp, metric_name, labels, value FROM metrics \
         WHERE timestamp >= {window_start} \
         ORDER BY timestamp DESC"
    );
    if let Ok(rows) = store.query_sql(&sql).await {
        for (_ts, name, _labels, val) in rows {
            // First seen per metric = latest (DESC order).
            values.entry(name).or_insert(val);
        }
    }

    // Compute derived percentage metrics for the default alert rules.
    if let (Some(&used), Some(&total)) = (
        values.get("node_memory_used_bytes"),
        values.get("node_memory_total_bytes"),
    ) && total > 0.0
    {
        values.insert(
            "node_memory_usage_percent".to_string(),
            (used / total) * 100.0,
        );
    }
    if let (Some(&used), Some(&total)) = (
        values.get("node_disk_used_bytes"),
        values.get("node_disk_total_bytes"),
    ) && total > 0.0
    {
        values.insert(
            "node_disk_usage_percent".to_string(),
            (used / total) * 100.0,
        );
    }

    values
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mayo::alert::AlertSeverity;

    fn firing_transition() -> AlertTransition {
        AlertTransition {
            rule_name: "cpu_throttle".to_string(),
            severity: AlertSeverity::Critical,
            description: "CPU usage above 90% for 5 minutes".to_string(),
            kind: TransitionKind::Firing,
            value: Some(95.3),
            fired_at: Some(SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000)),
        }
    }

    fn resolved_transition() -> AlertTransition {
        AlertTransition {
            rule_name: "cpu_throttle".to_string(),
            severity: AlertSeverity::Critical,
            description: "CPU usage above 90% for 5 minutes".to_string(),
            kind: TransitionKind::Resolved,
            value: Some(42.1),
            fired_at: None,
        }
    }

    #[test]
    fn payload_json_matches_spec() {
        let dispatcher = WebhookDispatcher::new(reqwest::Client::new(), vec![], "prod".to_string());
        let payload = dispatcher.build_payload(&firing_transition());
        let json = serde_json::to_value(&payload).unwrap();

        assert_eq!(json["version"], "1");
        assert_eq!(json["cluster"], "prod");
        assert!(json["timestamp"].is_u64());
        assert_eq!(json["alert"]["name"], "cpu_throttle");
        assert_eq!(json["alert"]["severity"], "critical");
        assert_eq!(json["alert"]["status"], "firing");
        assert_eq!(json["alert"]["value"], 95.3);
        assert_eq!(json["alert"]["fired_at"], 1_700_000_000u64);
    }

    #[test]
    fn firing_transition_produces_firing_status() {
        let dispatcher = WebhookDispatcher::new(reqwest::Client::new(), vec![], "test".to_string());
        let payload = dispatcher.build_payload(&firing_transition());
        assert_eq!(payload.alert.status, "firing");
    }

    #[test]
    fn resolved_transition_produces_resolved_status() {
        let dispatcher = WebhookDispatcher::new(reqwest::Client::new(), vec![], "test".to_string());
        let payload = dispatcher.build_payload(&resolved_transition());
        assert_eq!(payload.alert.status, "resolved");
        assert!(payload.alert.fired_at.is_none());
    }

    #[test]
    fn hmac_signing_produces_valid_signature() {
        let secret = "test-secret";
        let body = b"{\"version\":\"1\"}";
        let sig = sign_payload(secret, body);

        assert!(sig.starts_with("sha256="));
        assert!(verify_signature(secret, body, &sig));
    }

    #[test]
    fn hmac_wrong_secret_fails() {
        let body = b"payload";
        let sig = sign_payload("correct-secret", body);
        assert!(!verify_signature("wrong-secret", body, &sig));
    }

    #[test]
    fn hmac_tampered_body_fails() {
        let secret = "secret";
        let sig = sign_payload(secret, b"original");
        assert!(!verify_signature(secret, b"tampered", &sig));
    }

    #[test]
    fn severity_filter_critical_only_skips_warning() {
        let dest = AlertDestination {
            dest_type: "webhook".to_string(),
            url: "https://example.com".to_string(),
            severity: vec!["critical".to_string()],
            secret: None,
        };
        assert!(severity_matches(&dest, AlertSeverity::Critical));
        assert!(!severity_matches(&dest, AlertSeverity::Warning));
    }

    #[test]
    fn severity_filter_empty_matches_all() {
        let dest = AlertDestination {
            dest_type: "webhook".to_string(),
            url: "https://example.com".to_string(),
            severity: vec![],
            secret: None,
        };
        assert!(severity_matches(&dest, AlertSeverity::Critical));
        assert!(severity_matches(&dest, AlertSeverity::Warning));
    }

    #[test]
    fn severity_filter_multiple() {
        let dest = AlertDestination {
            dest_type: "webhook".to_string(),
            url: "https://example.com".to_string(),
            severity: vec!["critical".to_string(), "warning".to_string()],
            secret: None,
        };
        assert!(severity_matches(&dest, AlertSeverity::Critical));
        assert!(severity_matches(&dest, AlertSeverity::Warning));
    }

    /// Integration test: dispatch to a local mock server.
    #[tokio::test]
    async fn dispatch_delivers_to_mock_server() {
        use axum::Router;
        use axum::routing::post;
        use std::sync::Arc;
        use tokio::sync::Mutex;

        let received = Arc::new(Mutex::new(Vec::<Vec<u8>>::new()));
        let received_clone = Arc::clone(&received);

        let app = Router::new().route(
            "/hook",
            post(move |body: axum::body::Bytes| {
                let received = Arc::clone(&received_clone);
                async move {
                    received.lock().await.push(body.to_vec());
                    "ok"
                }
            }),
        );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.ok();
        });

        let dest = AlertDestination {
            dest_type: "webhook".to_string(),
            url: format!("http://{addr}/hook"),
            severity: vec![],
            secret: None,
        };

        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap();

        let dispatcher = WebhookDispatcher::new(client, vec![dest], "test-cluster".to_string());
        dispatcher.dispatch(&firing_transition()).await;

        // Give the mock server a moment to process
        tokio::time::sleep(Duration::from_millis(50)).await;

        let bodies = received.lock().await;
        assert_eq!(bodies.len(), 1);

        let payload: serde_json::Value = serde_json::from_slice(&bodies[0]).unwrap();
        assert_eq!(payload["version"], "1");
        assert_eq!(payload["alert"]["name"], "cpu_throttle");
        assert_eq!(payload["alert"]["status"], "firing");
        assert_eq!(payload["cluster"], "test-cluster");
    }

    /// Integration test: HMAC header is set when secret is configured.
    #[tokio::test]
    async fn dispatch_sends_hmac_header() {
        use axum::Router;
        use axum::http::HeaderMap;
        use axum::routing::post;
        use std::sync::Arc;
        use tokio::sync::Mutex;

        let received_headers = Arc::new(Mutex::new(Vec::<(HeaderMap, Vec<u8>)>::new()));
        let received_clone = Arc::clone(&received_headers);

        let app = Router::new().route(
            "/hook",
            post(move |headers: HeaderMap, body: axum::body::Bytes| {
                let received = Arc::clone(&received_clone);
                async move {
                    received.lock().await.push((headers, body.to_vec()));
                    "ok"
                }
            }),
        );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.ok();
        });

        let secret = "my-webhook-secret";
        let dest = AlertDestination {
            dest_type: "webhook".to_string(),
            url: format!("http://{addr}/hook"),
            severity: vec![],
            secret: Some(secret.to_string()),
        };

        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap();

        let dispatcher = WebhookDispatcher::new(client, vec![dest], "test".to_string());
        dispatcher.dispatch(&firing_transition()).await;

        tokio::time::sleep(Duration::from_millis(50)).await;

        let entries = received_headers.lock().await;
        assert_eq!(entries.len(), 1);

        let (headers, body) = &entries[0];
        let sig = headers
            .get("X-Mayo-Signature-256")
            .expect("missing signature header")
            .to_str()
            .unwrap();

        assert!(verify_signature(secret, body, sig));
    }

    /// Integration test: skips destinations that don't match severity.
    #[tokio::test]
    async fn dispatch_skips_non_matching_severity() {
        use axum::Router;
        use axum::routing::post;
        use std::sync::Arc;
        use tokio::sync::Mutex;

        let received = Arc::new(Mutex::new(0u32));
        let received_clone = Arc::clone(&received);

        let app = Router::new().route(
            "/hook",
            post(move || {
                let received = Arc::clone(&received_clone);
                async move {
                    *received.lock().await += 1;
                    "ok"
                }
            }),
        );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.ok();
        });

        // Destination only accepts critical, but we send a warning transition.
        let dest = AlertDestination {
            dest_type: "webhook".to_string(),
            url: format!("http://{addr}/hook"),
            severity: vec!["critical".to_string()],
            secret: None,
        };

        let client = reqwest::Client::new();
        let dispatcher = WebhookDispatcher::new(client, vec![dest], "test".to_string());

        let warning_transition = AlertTransition {
            rule_name: "test".to_string(),
            severity: AlertSeverity::Warning,
            description: "test".to_string(),
            kind: TransitionKind::Firing,
            value: Some(75.0),
            fired_at: None,
        };

        dispatcher.dispatch(&warning_transition).await;
        tokio::time::sleep(Duration::from_millis(50)).await;

        assert_eq!(*received.lock().await, 0);
    }
}
