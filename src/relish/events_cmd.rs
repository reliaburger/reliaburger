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
