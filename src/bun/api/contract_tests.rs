//! The controller contract's JSON, pinned (#1125).
//!
//! A controller is an API client written by someone else, against the
//! shapes documented in `docs/manual/15_writing-a-controller.md`. Each
//! snapshot here is one of those shapes. When a test fails, the wire
//! format has changed: update the manual's field tables in the same
//! change, then accept the new snapshot.

use crate::bun::agent::{ApplyEvent, ClusterInstanceStatus, InstanceStatus};
use crate::bun::cluster_view::ClusterEvents;
use crate::bun::diagnostics::DesiredAppEvidence;
use crate::bun::events::{ClusterEvent, EventKind, EventSeverity};
use crate::council::owner::OwnerError;
use crate::meat::AppId;

/// `POST /v1/apply` streams these, one per SSE `data:` line.
#[test]
fn apply_events() {
    let events = vec![
        ApplyEvent::Progress {
            message: "app 'db' in namespace 'pg' is owned by 'pg-failover'; skipped, so this \
                      apply doesn't fight it"
                .into(),
        },
        ApplyEvent::Progress {
            message: "app db-a: committed to the cluster".into(),
        },
        ApplyEvent::Complete {
            created: 1,
            instances: vec![],
        },
        ApplyEvent::Error {
            message: "app db-a: refused by the cluster: app pg/db-a is owned by \"other\"".into(),
        },
    ];
    insta::assert_json_snapshot!(events);
}

/// A refusal from the owner rules: the 403 or 409 body.
#[test]
fn owner_refusal_body() {
    let errors = [
        OwnerError::NotTheCaller {
            app: AppId::new("db", "pg"),
            owner: "pg-failover".into(),
            caller: "alice".into(),
        },
        OwnerError::Claimed {
            app: AppId::new("db", "pg"),
            current: "pg-failover".into(),
            declared: "other".into(),
        },
        OwnerError::NotOwner {
            app: AppId::new("db", "pg"),
            owner: "pg-failover".into(),
            verb: "delete",
        },
        OwnerError::NotOwned {
            app: AppId::new("db", "pg"),
        },
    ];
    let bodies: Vec<serde_json::Value> = errors
        .iter()
        .map(|error| {
            serde_json::json!({
                "status": error.status(),
                "body": { "error": error.to_string() },
            })
        })
        .collect();
    insta::assert_json_snapshot!(bodies);
}

/// `GET /v1/diagnostics/apps`: one row per app the token can see.
#[test]
fn desired_apps() {
    let rows = vec![DesiredAppEvidence {
        app: "db-a".into(),
        namespace: "pg".into(),
        desired_replicas: 1,
        scheduled_replicas: 1,
        placements: [("node-2".to_string(), 1)].into(),
        service_port: Some(5432),
        blocked: None,
        volume_home_away: None,
        unschedulable: None,
        volume_homes: Vec::new(),
        rollout_failed: None,
        owner: Some("db-failover".into()),
    }];
    insta::assert_json_snapshot!(rows);
}

/// `GET /v1/status?cluster=true`: one row per instance the token can see.
#[test]
fn cluster_instance_status() {
    let rows = vec![ClusterInstanceStatus {
        node: "node-2".into(),
        instance: InstanceStatus {
            id: "db-a-0".into(),
            app_name: "db-a".into(),
            namespace: "pg".into(),
            state: "running".into(),
            restart_count: 0,
            host_port: Some(31_002),
            exit_code: None,
            pid: Some(4242),
            runtime_unknown: false,
            status_age_ms: Some(120),
            reason: None,
        },
    }];
    insta::assert_json_snapshot!(rows);
}

/// `GET /v1/events`, and each `data:` line of `?follow=true`.
#[test]
fn events() {
    let view = ClusterEvents {
        events: vec![ClusterEvent {
            sequence: 41,
            timestamp: 1_792_000_000,
            kind: EventKind::Health,
            severity: EventSeverity::Warning,
            app: Some("db-a".into()),
            namespace: Some("pg".into()),
            node: Some("node-2".into()),
            action: None,
            principal: None,
            source_address: None,
            details: Default::default(),
            message: "db-a-0 failed its health check".into(),
        }],
        warnings: vec!["node-3: did not answer within 2s".into()],
    };
    insta::assert_json_snapshot!(view);
}

/// The `kind` and `severity` values an event can carry.
#[test]
fn event_kinds_and_severities() {
    let kinds = [
        EventKind::Deploy,
        EventKind::Restart,
        EventKind::Health,
        EventKind::Stop,
        EventKind::JobCompleted,
        EventKind::JobFailed,
        EventKind::JobSkipped,
        EventKind::JobCancelled,
        EventKind::JobUnknown,
        EventKind::Alert,
        EventKind::Fault,
        EventKind::Token,
        EventKind::Secret,
        EventKind::Identity,
        EventKind::Operation,
        EventKind::Refusal,
        EventKind::Placement,
    ];
    let severities = [
        EventSeverity::Info,
        EventSeverity::Warning,
        EventSeverity::Critical,
    ];
    insta::assert_json_snapshot!(serde_json::json!({
        "kinds": kinds,
        "severities": severities,
    }));
}
