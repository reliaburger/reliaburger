//! Alert webhook delivery.
//!
//! Constructs JSON payloads for alert state transitions and delivers
//! them to configured HTTP endpoints with optional HMAC-SHA256 signing.
//! Failed deliveries are retried 3 times with exponential backoff
//! (1s, 5s, 25s).

use crate::mayo::types::MetricKey;
use std::collections::{BTreeMap, HashMap};
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
    /// Exact label set of the alert instance.
    pub labels: BTreeMap<String, String>,
    pub name: String,
    pub severity: String,
    pub status: String,
    pub message: String,
    pub value: Option<f64>,
    pub fired_at: Option<u64>,
}

// ---------------------------------------------------------------------------
// Provider-specific payloads (Slack, PagerDuty)
// ---------------------------------------------------------------------------

/// Slack incoming-webhook body: one or more coloured attachments.
#[derive(Debug, Clone, Serialize)]
pub struct SlackPayload {
    pub attachments: Vec<SlackAttachment>,
}

/// A single Slack message attachment.
#[derive(Debug, Clone, Serialize)]
pub struct SlackAttachment {
    /// `good`, `warning`, `danger`, or a hex colour.
    pub color: String,
    /// Plain-text summary shown in notifications.
    pub fallback: String,
    pub title: String,
    pub text: String,
}

/// PagerDuty Events API v2 event.
#[derive(Debug, Clone, Serialize)]
pub struct PagerDutyPayload {
    pub routing_key: String,
    /// `trigger` or `resolve`.
    pub event_action: String,
    /// Stable key so a resolve closes the matching trigger.
    pub dedup_key: String,
    /// Required on `trigger`, omitted on `resolve`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub payload: Option<PagerDutyDetails>,
}

/// The `payload` object required on a PagerDuty `trigger`.
#[derive(Debug, Clone, Serialize)]
pub struct PagerDutyDetails {
    /// Labels included in the incident details.
    pub custom_details: BTreeMap<String, String>,
    pub summary: String,
    pub source: String,
    /// `critical`, `error`, `warning`, or `info`.
    pub severity: String,
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
    ///
    /// Each destination's payload matches its provider's contract (OBS4): a
    /// generic webhook gets the Mayo schema, Slack gets an attachment, and
    /// PagerDuty gets an Events API v2 event. One generic shape posted to all
    /// three would be silently dropped by Slack and PagerDuty.
    pub async fn dispatch(&self, transition: &AlertTransition) {
        for dest in &self.destinations {
            if !severity_matches(dest, transition.severity) {
                continue;
            }
            let body = match self.body_for(dest, transition) {
                Ok(b) => b,
                Err(e) => {
                    eprintln!(
                        "mayo: failed to serialise payload for {}: {e}",
                        redact_url(&dest.url)
                    );
                    continue;
                }
            };
            self.send_with_retry(dest, &body).await;
        }
    }

    /// Serialise the provider-specific payload body for a destination.
    fn body_for(
        &self,
        dest: &AlertDestination,
        transition: &AlertTransition,
    ) -> Result<Vec<u8>, serde_json::Error> {
        match dest.dest_type.as_str() {
            "slack" => serde_json::to_vec(&self.build_slack_payload(transition)),
            "pagerduty" => serde_json::to_vec(&self.build_pagerduty_payload(dest, transition)),
            // "webhook" and anything unknown fall back to the generic schema.
            _ => serde_json::to_vec(&self.build_payload(transition)),
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
                labels: t.labels.clone(),
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

    /// Build a Slack incoming-webhook payload.
    ///
    /// Slack renders `attachments` with a coloured bar: red for a firing
    /// critical, green when resolved. A firing warning is amber. The `text`
    /// carries the human message; `fallback` is what Slack shows in
    /// notifications.
    fn build_slack_payload(&self, t: &AlertTransition) -> SlackPayload {
        let firing = t.kind == TransitionKind::Firing;
        let color = match (firing, t.severity) {
            (false, _) => "good",
            (true, AlertSeverity::Critical) => "danger",
            (true, AlertSeverity::Warning) => "warning",
        };
        let status = if firing { "FIRING" } else { "RESOLVED" };
        let title = format!("[{status}] {} ({})", t.rule_name, self.cluster_name);
        let mut text = t.description.clone();
        if !t.labels.is_empty() {
            text.push_str(&format!("\nlabels: {:?}", t.labels));
        }
        if let Some(v) = t.value {
            text.push_str(&format!(" (value: {v})"));
        }
        SlackPayload {
            attachments: vec![SlackAttachment {
                color: color.to_string(),
                fallback: title.clone(),
                title,
                text,
            }],
        }
    }

    /// Build a PagerDuty Events API v2 payload.
    ///
    /// A firing alert is a `trigger`; a resolved alert is a `resolve`. The
    /// `dedup_key` ties the two together so PagerDuty auto-resolves the right
    /// incident. `routing_key` is the destination's integration key, carried in
    /// `secret`.
    fn build_pagerduty_payload(
        &self,
        dest: &AlertDestination,
        t: &AlertTransition,
    ) -> PagerDutyPayload {
        let firing = t.kind == TransitionKind::Firing;
        let severity = match t.severity {
            AlertSeverity::Critical => "critical",
            AlertSeverity::Warning => "warning",
        };
        PagerDutyPayload {
            routing_key: dest.secret.clone().unwrap_or_default(),
            event_action: if firing { "trigger" } else { "resolve" }.to_string(),
            dedup_key: format!(
                "{}/{}/{}",
                self.cluster_name,
                t.rule_name,
                hex::encode(
                    ring::digest::digest(
                        &ring::digest::SHA256,
                        MetricKey::with_labels(&t.rule_name, t.labels.clone())
                            .labels_json()
                            .as_bytes()
                    )
                    .as_ref()
                )
            ),
            payload: firing.then(|| PagerDutyDetails {
                custom_details: t.labels.clone(),
                summary: t.description.clone(),
                source: self.cluster_name.clone(),
                severity: severity.to_string(),
            }),
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
                        redact_url(&dest.url),
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
            redact_url(&dest.url)
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

/// Redact a webhook URL for logging: keep only `scheme://host[:port]` and drop
/// the path/query (M18). Slack incoming-webhook URLs carry the secret in the
/// path, so logging the full URL on a delivery failure writes the credential to
/// the node's stderr/journal (and possibly the collected logs). PagerDuty and
/// generic secrets live in `dest.secret`, which is never logged.
fn redact_url(url: &str) -> String {
    // Split off the scheme, then trim the authority at the first `/`, `?`, `#`.
    match url.split_once("://") {
        Some((scheme, rest)) => {
            let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
            format!("{scheme}://{}/…", &rest[..authority_end])
        }
        // No scheme — don't risk echoing an opaque secret-bearing string.
        None => "<redacted>".to_string(),
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

/// How old a reading may be and still count as "current" for alerting.
///
/// The query window used to be the only bound, which conflated two
/// different questions: "how far back do we look?" and "how stale may the
/// answer be?" A metric that stopped being emitted 110 seconds ago would
/// still be evaluated as though it were live. Naming the freshness bound
/// separately makes it a decision rather than a side effect of the window
/// (M20).
const MAX_VALUE_AGE_SECS: u64 = 90;

/// How far back to look for readings.
const QUERY_WINDOW_SECS: u64 = 120;

/// Keep each fresh labelled series and derive percentages within that label set.
/// Invalid label JSON or non-finite readings provide no recovery evidence.
pub fn latest_series_values(
    series: &HashMap<(String, String), (u64, f64)>,
    now_secs: u64,
) -> HashMap<MetricKey, f64> {
    let mut fresh: HashMap<MetricKey, (u64, f64)> = HashMap::new();
    for ((name, labels), (timestamp, value)) in series {
        if now_secs.saturating_sub(*timestamp) > MAX_VALUE_AGE_SECS || !value.is_finite() {
            continue;
        }
        let Ok(labels) = serde_json::from_str(labels) else {
            continue;
        };
        let key = MetricKey::with_labels(name, labels);
        let previous = fresh.entry(key).or_insert((*timestamp, *value));
        if *timestamp > previous.0
            || (*timestamp == previous.0 && value.total_cmp(&previous.1).is_gt())
        {
            *previous = (*timestamp, *value);
        }
    }
    let mut result: HashMap<_, _> = fresh
        .iter()
        .map(|(key, (_, value))| (key.clone(), *value))
        .collect();
    for (key, (_, used)) in &fresh {
        for (used_name, total_name, percent_name) in [
            (
                "node_memory_used_bytes",
                "node_memory_total_bytes",
                "node_memory_usage_percent",
            ),
            (
                "node_disk_used_bytes",
                "node_disk_total_bytes",
                "node_disk_usage_percent",
            ),
        ] {
            if key.name.0 != used_name {
                continue;
            }
            let total_key = MetricKey::with_labels(total_name, key.labels.clone());
            if let Some((_, total)) = fresh.get(&total_key)
                && *total > 0.0
            {
                let percent = used / total * 100.0;
                if percent.is_finite() {
                    result.insert(
                        MetricKey::with_labels(percent_name, key.labels.clone()),
                        percent,
                    );
                }
            }
        }
    }
    result
}

/// Gather the latest metric values from a MayoStore for alert evaluation.
///
/// Reads the recent window, preserving the newest value of each metric/label
/// identity. Freshness and derived percentages use [`latest_series_values`].
pub async fn gather_latest_values(
    store: &crate::mayo::store::MayoStore,
) -> HashMap<MetricKey, f64> {
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let window_start = now.saturating_sub(QUERY_WINDOW_SECS);

    let mut series: HashMap<(String, String), (u64, f64)> = HashMap::new();

    let sql = format!(
        "SELECT timestamp, metric_name, labels, value FROM metrics \
         WHERE timestamp >= {window_start} \
         ORDER BY timestamp DESC"
    );
    if let Ok(rows) = store.query_sql(&sql).await {
        for (timestamp, name, labels, value) in rows {
            // DESC order means the first row per series is its newest.
            series.entry((name, labels)).or_insert((timestamp, value));
        }
    }

    latest_series_values(&series, now)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mayo::alert::AlertSeverity;

    fn firing_transition() -> AlertTransition {
        AlertTransition {
            labels: BTreeMap::new(),
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
            labels: BTreeMap::new(),
            rule_name: "cpu_throttle".to_string(),
            severity: AlertSeverity::Critical,
            description: "CPU usage above 90% for 5 minutes".to_string(),
            kind: TransitionKind::Resolved,
            value: Some(42.1),
            fired_at: None,
        }
    }

    fn firing_warning_transition() -> AlertTransition {
        AlertTransition {
            labels: BTreeMap::new(),
            rule_name: "memory_high".to_string(),
            severity: AlertSeverity::Warning,
            description: "Memory usage above 70%".to_string(),
            kind: TransitionKind::Firing,
            value: Some(75.0),
            fired_at: Some(SystemTime::UNIX_EPOCH + Duration::from_secs(1_700_000_000)),
        }
    }

    #[test]
    fn notifications_keep_labels_and_resolve_only_the_matching_incident() {
        let dispatcher = WebhookDispatcher::new(reqwest::Client::new(), vec![], "prod".into());
        let destination = AlertDestination {
            dest_type: "pagerduty".into(),
            url: "http://unused".into(),
            severity: vec![],
            secret: Some("fixture".into()),
        };
        let mut a = firing_transition();
        a.labels.insert("node".into(), "a".into());
        let mut b = a.clone();
        b.labels.insert("node".into(), "b".into());
        let key_a = dispatcher
            .build_pagerduty_payload(&destination, &a)
            .dedup_key;
        assert_ne!(
            key_a,
            dispatcher
                .build_pagerduty_payload(&destination, &b)
                .dedup_key
        );
        assert_eq!(dispatcher.build_payload(&a).alert.labels, a.labels);
        assert_eq!(
            dispatcher
                .build_pagerduty_payload(&destination, &a)
                .payload
                .unwrap()
                .custom_details,
            a.labels
        );
        a.kind = TransitionKind::Resolved;
        assert_eq!(
            key_a,
            dispatcher
                .build_pagerduty_payload(&destination, &a)
                .dedup_key
        );
        assert!(
            dispatcher.build_slack_payload(&b).attachments[0]
                .text
                .contains("node")
        );
    }

    #[tokio::test]
    async fn real_store_preserves_two_labelled_series_for_evaluation() {
        let directory = tempfile::tempdir().unwrap();
        let mut store = crate::mayo::store::MayoStore::new(directory.path().into());
        for (node, value) in [("hot", 95.0), ("healthy", 10.0)] {
            store.insert_now(
                &MetricKey::with_labels("cpu", BTreeMap::from([("node".into(), node.into())])),
                value,
            );
        }
        store.flush().await.unwrap();
        let values = gather_latest_values(&store).await;
        assert_eq!(values.len(), 2);
        assert_eq!(
            values.get(&MetricKey::with_labels(
                "cpu",
                BTreeMap::from([("node".into(), "hot".into())])
            )),
            Some(&95.0)
        );
        assert_eq!(
            values.get(&MetricKey::with_labels(
                "cpu",
                BTreeMap::from([("node".into(), "healthy".into())])
            )),
            Some(&10.0)
        );
    }

    #[test]
    fn stale_denominator_and_invalid_labels_provide_no_derived_value() {
        let values = latest_series_values(
            &series(&[
                (
                    "node_memory_used_bytes",
                    r#"{"node":"a"}"#,
                    MAX_VALUE_AGE_SECS + 1,
                    90.0,
                ),
                ("node_memory_total_bytes", r#"{"node":"a"}"#, 0, 100.0),
                ("cpu", "invalid labels", MAX_VALUE_AGE_SECS + 1, 50.0),
            ]),
            MAX_VALUE_AGE_SECS + 1,
        );
        assert_eq!(values.len(), 1);
        assert!(
            values
                .keys()
                .all(|key| key.name.0 == "node_memory_used_bytes")
        );
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
    fn slack_payload_matches_provider_shape() {
        // OBS4: Slack expects `attachments`, not the generic Mayo schema.
        let dispatcher = WebhookDispatcher::new(reqwest::Client::new(), vec![], "prod".to_string());
        let payload = dispatcher.build_slack_payload(&firing_transition());
        let json = serde_json::to_value(&payload).unwrap();

        let attachments = json["attachments"].as_array().expect("attachments array");
        assert_eq!(attachments.len(), 1);
        let a = &attachments[0];
        // Firing critical is red.
        assert_eq!(a["color"], "danger");
        assert!(a["title"].as_str().unwrap().contains("FIRING"));
        assert!(a["title"].as_str().unwrap().contains("cpu_throttle"));
        assert!(a["fallback"].is_string());
    }

    #[test]
    fn slack_resolved_is_green() {
        let dispatcher = WebhookDispatcher::new(reqwest::Client::new(), vec![], "prod".to_string());
        let payload = dispatcher.build_slack_payload(&resolved_transition());
        let json = serde_json::to_value(&payload).unwrap();
        assert_eq!(json["attachments"][0]["color"], "good");
        assert!(
            json["attachments"][0]["title"]
                .as_str()
                .unwrap()
                .contains("RESOLVED")
        );
    }

    #[test]
    fn pagerduty_payload_matches_events_v2_shape() {
        // OBS4: PagerDuty's Events API v2 needs routing_key/event_action/
        // dedup_key and a payload object with summary/source/severity.
        let dispatcher = WebhookDispatcher::new(reqwest::Client::new(), vec![], "prod".to_string());
        let dest = AlertDestination {
            dest_type: "pagerduty".to_string(),
            url: "https://events.pagerduty.com/v2/enqueue".to_string(),
            severity: vec![],
            secret: Some("routing-key-123".to_string()),
        };
        let json =
            serde_json::to_value(dispatcher.build_pagerduty_payload(&dest, &firing_transition()))
                .unwrap();

        assert_eq!(json["routing_key"], "routing-key-123");
        assert_eq!(json["event_action"], "trigger");
        assert!(json["dedup_key"].as_str().unwrap().contains("cpu_throttle"));
        assert_eq!(json["payload"]["severity"], "critical");
        assert_eq!(json["payload"]["source"], "prod");
        assert!(json["payload"]["summary"].is_string());
    }

    #[test]
    fn pagerduty_resolve_omits_payload_and_keeps_dedup_key() {
        let dispatcher = WebhookDispatcher::new(reqwest::Client::new(), vec![], "prod".to_string());
        let dest = AlertDestination {
            dest_type: "pagerduty".to_string(),
            url: "https://events.pagerduty.com/v2/enqueue".to_string(),
            severity: vec![],
            secret: Some("routing-key-123".to_string()),
        };
        let json =
            serde_json::to_value(dispatcher.build_pagerduty_payload(&dest, &resolved_transition()))
                .unwrap();
        assert_eq!(json["event_action"], "resolve");
        // A resolve carries the same dedup_key but no payload.
        assert!(json["dedup_key"].as_str().unwrap().contains("cpu_throttle"));
        assert!(json.get("payload").is_none() || json["payload"].is_null());
    }

    #[test]
    fn slack_firing_warning_is_amber() {
        let dispatcher = WebhookDispatcher::new(reqwest::Client::new(), vec![], "prod".to_string());
        let json =
            serde_json::to_value(dispatcher.build_slack_payload(&firing_warning_transition()))
                .unwrap();
        assert_eq!(json["attachments"][0]["color"], "warning");
    }

    #[test]
    fn pagerduty_warning_severity_maps_through() {
        let dispatcher = WebhookDispatcher::new(reqwest::Client::new(), vec![], "prod".to_string());
        let dest = AlertDestination {
            dest_type: "pagerduty".to_string(),
            url: "https://events.pagerduty.com/v2/enqueue".to_string(),
            severity: vec![],
            secret: Some("rk".to_string()),
        };
        let json = serde_json::to_value(
            dispatcher.build_pagerduty_payload(&dest, &firing_warning_transition()),
        )
        .unwrap();
        assert_eq!(json["payload"]["severity"], "warning");
    }

    #[test]
    fn body_for_dispatches_by_destination_type() {
        let dispatcher = WebhookDispatcher::new(reqwest::Client::new(), vec![], "prod".to_string());
        let make = |t: &str| AlertDestination {
            dest_type: t.to_string(),
            url: "https://example.com".to_string(),
            severity: vec![],
            secret: Some("rk".to_string()),
        };

        // Slack → attachments; PagerDuty → routing_key; anything else → the
        // generic Mayo schema (version field).
        let slack: serde_json::Value = serde_json::from_slice(
            &dispatcher
                .body_for(&make("slack"), &firing_transition())
                .unwrap(),
        )
        .unwrap();
        assert!(slack["attachments"].is_array());

        let pd: serde_json::Value = serde_json::from_slice(
            &dispatcher
                .body_for(&make("pagerduty"), &firing_transition())
                .unwrap(),
        )
        .unwrap();
        assert!(pd["routing_key"].is_string());

        for unknown in ["webhook", "something-else"] {
            let generic: serde_json::Value = serde_json::from_slice(
                &dispatcher
                    .body_for(&make(unknown), &firing_transition())
                    .unwrap(),
            )
            .unwrap();
            assert_eq!(
                generic["version"], "1",
                "{unknown} should get generic shape"
            );
        }
    }

    /// Integration test: a Slack destination receives an attachments body.
    #[tokio::test]
    async fn dispatch_slack_delivers_attachments() {
        use axum::Router;
        use axum::routing::post;
        use std::sync::Arc;
        use tokio::sync::Mutex;

        let received = Arc::new(Mutex::new(Vec::<Vec<u8>>::new()));
        let received_clone = Arc::clone(&received);
        let app = Router::new().route(
            "/slack",
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
            dest_type: "slack".to_string(),
            url: format!("http://{addr}/slack"),
            severity: vec![],
            secret: None,
        };
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap();
        let dispatcher = WebhookDispatcher::new(client, vec![dest], "prod".to_string());
        dispatcher.dispatch(&firing_transition()).await;

        // Poll for delivery instead of sleeping a fixed interval.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        let body = loop {
            if let Some(b) = received.lock().await.first().cloned() {
                break b;
            }
            if tokio::time::Instant::now() >= deadline {
                panic!("slack webhook not delivered");
            }
            tokio::task::yield_now().await;
        };
        let json: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(json["attachments"].is_array());
        assert_eq!(json["attachments"][0]["color"], "danger");
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
            labels: BTreeMap::new(),
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

    /// M18: a Slack webhook URL carries its secret in the path, so failure logs
    /// must keep only the scheme+host and drop the path.
    #[test]
    fn redact_url_drops_the_secret_bearing_path() {
        let redacted = redact_url("https://hooks.slack.com/services/T00/B00/XXXXsecretXXXX");
        assert_eq!(redacted, "https://hooks.slack.com/…");
        assert!(!redacted.contains("secret"));
        // Port is preserved (part of the authority), path is not.
        assert_eq!(
            redact_url("http://example.com:8080/a/b?token=abc"),
            "http://example.com:8080/…"
        );
        // A schemeless / opaque string is fully redacted.
        assert_eq!(redact_url("weird-opaque-token"), "<redacted>");
    }

    #[test]
    fn healthy_series_cannot_hide_another_nodes_alert() {
        use crate::mayo::alert::{AlertEvaluator, AlertOperator, AlertRule};
        let values = latest_series_values(
            &series(&[
                ("cpu", r#"{"node":"hot"}"#, 99, 95.0),
                ("cpu", r#"{"node":"healthy"}"#, 100, 10.0),
            ]),
            100,
        );
        let mut evaluator = AlertEvaluator::new(vec![AlertRule {
            name: "cpu-high".into(),
            metric_name: "cpu".into(),
            threshold: 80.0,
            operator: AlertOperator::GreaterThan,
            for_duration: Duration::ZERO,
            severity: AlertSeverity::Critical,
            description: "CPU too high".into(),
        }]);
        evaluator.evaluate(&values);
        evaluator.evaluate(&values);
        assert_eq!(evaluator.firing_alerts().len(), 1);
    }

    // -- latest_series_values (M20) ------------------------------------------------

    fn series(entries: &[(&str, &str, u64, f64)]) -> HashMap<(String, String), (u64, f64)> {
        entries
            .iter()
            .map(|(name, labels, ts, value)| {
                ((name.to_string(), labels.to_string()), (*ts, *value))
            })
            .collect()
    }

    /// M20: derived percentages used to divide whichever `used` happened to
    /// be newest by whichever `total` happened to be newest, across
    /// different label sets. Two nodes reporting into one store could
    /// produce a percentage that belonged to neither.
    #[test]
    fn derived_percentages_stay_within_one_label_set() {
        let values = latest_series_values(
            &series(&[
                // node-a: 50% used, and the newest reading.
                ("node_memory_used_bytes", r#"{"node":"a"}"#, 100, 50.0),
                ("node_memory_total_bytes", r#"{"node":"a"}"#, 100, 100.0),
                // node-b: tiny total, would give a wild percentage if
                // crossed with node-a's `used`.
                ("node_memory_used_bytes", r#"{"node":"b"}"#, 90, 1.0),
                ("node_memory_total_bytes", r#"{"node":"b"}"#, 90, 2.0),
            ]),
            100,
        );
        assert_eq!(
            values.get(&MetricKey::with_labels(
                "node_memory_usage_percent",
                BTreeMap::from([("node".into(), "a".into())])
            )),
            Some(&50.0)
        );
    }

    /// A reading inside the query window but past the freshness bound is not
    /// "current" — the window says how far back to look, not how stale an
    /// answer may be.
    #[test]
    fn readings_past_the_freshness_bound_are_dropped() {
        let values = latest_series_values(
            &series(&[("node_cpu_percent", r#"{"node":"a"}"#, 0, 99.0)]),
            MAX_VALUE_AGE_SECS + 1,
        );
        assert!(values.is_empty());

        let values = latest_series_values(
            &series(&[("node_cpu_percent", r#"{"node":"a"}"#, 1, 99.0)]),
            MAX_VALUE_AGE_SECS + 1,
        );
        assert_eq!(
            values.get(&MetricKey::with_labels(
                "node_cpu_percent",
                BTreeMap::from([("node".into(), "a".into())])
            )),
            Some(&99.0)
        );
    }

    /// The collapse must not depend on which row the query happened to
    /// return first: same series, same answer, every time.
    #[test]
    fn collapsing_is_deterministic_across_equal_timestamps() {
        let entries = [
            ("node_cpu_percent", r#"{"node":"a"}"#, 100, 10.0),
            ("node_cpu_percent", r#"{"node":"b"}"#, 100, 20.0),
            ("node_cpu_percent", r#"{"node":"c"}"#, 100, 30.0),
        ];
        let first = latest_series_values(&series(&entries), 100);
        let mut reversed = entries;
        reversed.reverse();
        let second = latest_series_values(&series(&reversed), 100);
        assert_eq!(first, second);
    }

    /// Each label set keeps its own fresh reading.
    #[test]
    fn both_label_sets_keep_their_fresh_readings() {
        let values = latest_series_values(
            &series(&[
                ("node_cpu_percent", r#"{"node":"a"}"#, 90, 10.0),
                ("node_cpu_percent", r#"{"node":"z"}"#, 100, 70.0),
            ]),
            100,
        );
        assert_eq!(
            values.get(&MetricKey::with_labels(
                "node_cpu_percent",
                BTreeMap::from([("node".into(), "z".into())])
            )),
            Some(&70.0)
        );
    }
}
