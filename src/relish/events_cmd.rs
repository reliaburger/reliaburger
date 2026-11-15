//! `relish events`: the cluster's recent events (D36).
//!
//! Every node keeps its newest 1024 events in memory: deploys, restarts,
//! health changes, faults, token and secret changes, audited operations.
//! The node relish talks to asks every live member and merges the answers
//! by time. It's a window on what just happened; `relish audit` is the
//! durable record.

use std::fmt;

use serde::Serialize;

use super::RelishError;
use super::client::BunClient;
use super::output::{OutputFormat, format_output};
use crate::bun::cluster_view::ClusterEvents;
use crate::bun::events::{ClusterEvent, EventSeverity};

/// What `relish events` asks for. The node filters, so `limit` counts
/// matching events.
#[derive(Debug, Clone)]
pub struct EventsFilter {
    /// How many of the newest matching events to show.
    pub limit: usize,
    /// Only this app's events.
    pub app: Option<String>,
    /// Only events at least this severe.
    pub severity: Option<EventSeverity>,
}

impl EventsFilter {
    /// The `/v1/events` request for this filter.
    pub fn path(&self) -> String {
        let mut params = url::form_urlencoded::Serializer::new(String::new());
        params.append_pair("limit", &self.limit.to_string());
        if let Some(app) = &self.app {
            params.append_pair("app", app);
        }
        if let Some(severity) = self.severity {
            params.append_pair("severity", severity_name(severity));
        }
        format!("/v1/events?{}", params.finish())
    }
}

/// `relish events`.
pub async fn events(filter: EventsFilter, output: OutputFormat) -> Result<(), RelishError> {
    let listing = BunClient::default_local().events_matching(&filter).await?;
    for warning in &listing.warnings {
        eprintln!("warning: {warning}");
    }
    println!("{}", format_output(&EventsReport(listing), output)?);
    Ok(())
}

/// The lowercase name the API and the table use for a severity.
fn severity_name(severity: EventSeverity) -> &'static str {
    match severity {
        EventSeverity::Info => "info",
        EventSeverity::Warning => "warning",
        EventSeverity::Critical => "critical",
    }
}

/// An events listing, rendered as a table for people.
#[derive(Debug, Serialize)]
#[serde(transparent)]
struct EventsReport(ClusterEvents);

impl fmt::Display for EventsReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.0.events.is_empty() {
            return write!(f, "no events match");
        }
        write!(
            f,
            "{:<20}  {:<8}  {:<14}  {:<12}  {:<24}  MESSAGE",
            "TIME (UTC)", "SEVERITY", "KIND", "NODE", "TARGET"
        )?;
        for event in &self.0.events {
            write!(
                f,
                "\n{:<20}  {:<8}  {:<14}  {:<12}  {:<24}  {}",
                super::audit_cmd::format_time(event.timestamp),
                severity_name(event.severity),
                kind_name(event),
                event.node.as_deref().unwrap_or("-"),
                target(event),
                event.message,
            )?;
        }
        Ok(())
    }
}

/// The kebab-case kind the API uses, so the table and `-o json` agree.
fn kind_name(event: &ClusterEvent) -> String {
    match serde_json::to_value(event.kind) {
        Ok(serde_json::Value::String(kind)) => kind,
        _ => format!("{:?}", event.kind),
    }
}

/// The event's target column, the same as `relish audit`'s.
fn target(event: &ClusterEvent) -> String {
    super::audit_cmd::target(
        event.app.as_deref(),
        event.namespace.as_deref(),
        &event.details,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bun::events::{ClusterEvent, EventKind, EventSeverity};

    fn event(kind: EventKind, severity: EventSeverity, message: &str) -> ClusterEvent {
        ClusterEvent {
            sequence: 1,
            timestamp: 1_700_000_000,
            kind,
            severity,
            app: Some("web".into()),
            namespace: Some("default".into()),
            node: Some("node-1".into()),
            action: None,
            principal: None,
            source_address: None,
            details: Default::default(),
            message: message.into(),
        }
    }

    #[test]
    fn a_multi_app_apply_names_its_apps_in_the_target_column() {
        let mut apply = event(EventKind::Operation, EventSeverity::Info, "app.applied");
        apply.app = None;
        apply.details.insert(
            "workloads".to_string(),
            "backend@default,frontend@default".to_string(),
        );
        assert_eq!(target(&apply), "backend, frontend @default");
    }

    #[test]
    fn events_render_as_a_table_with_time_node_and_target() {
        let report = EventsReport(ClusterEvents {
            events: vec![event(
                EventKind::Restart,
                EventSeverity::Warning,
                "instance web-0 restarted",
            )],
            warnings: Vec::new(),
        });
        let shown = report.to_string();
        assert!(shown.starts_with("TIME (UTC)"), "{shown}");
        for part in [
            "2023-11-14 22:13:20",
            "warning",
            "restart",
            "node-1",
            "web@default",
            "instance web-0 restarted",
        ] {
            assert!(shown.contains(part), "missing {part:?}:\n{shown}");
        }
    }

    #[test]
    fn no_events_says_so() {
        let shown = EventsReport(ClusterEvents::default()).to_string();
        assert_eq!(shown, "no events match");
    }

    #[test]
    fn events_serialise_as_the_api_answer() {
        let report = EventsReport(ClusterEvents {
            events: vec![event(EventKind::Deploy, EventSeverity::Info, "deployed")],
            warnings: vec!["node-3 timed out".into()],
        });
        let json: serde_json::Value = serde_json::from_str(
            &crate::relish::output::format_output(&report, OutputFormat::Json).unwrap(),
        )
        .unwrap();
        assert_eq!(json["events"][0]["kind"], "deploy");
        assert_eq!(json["warnings"][0], "node-3 timed out");
    }

    #[test]
    fn filters_become_the_api_query() {
        let filter = EventsFilter {
            limit: 20,
            app: Some("web&co".into()),
            severity: Some(EventSeverity::Warning),
        };
        assert_eq!(
            filter.path(),
            "/v1/events?limit=20&app=web%26co&severity=warning"
        );
        let plain = EventsFilter {
            limit: 100,
            app: None,
            severity: None,
        };
        assert_eq!(plain.path(), "/v1/events?limit=100");
    }
}
