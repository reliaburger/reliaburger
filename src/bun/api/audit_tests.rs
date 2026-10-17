//! The audit trail through the real router (#685): which actions are
//! recorded, with what, and who may read them back.

use axum::body::Body;
use http_body_util::BodyExt;
use tower::ServiceExt;

use super::audit::{AUDITED_ROUTES, HANDLER_AUDITED_ROUTES, UNAUDITED_ROUTES};
use super::tests::{
    lease_static_capabilities, named_user_token, seeded_council,
    setup_with_auth_leases_events_and_council, setup_with_auth_readiness_leases_and_events,
    workload_fault_static_capabilities,
};
use super::*;
use crate::bun::audit::{AuditFilter, AuditLimits, AuditListing, AuditLog, AuditRecord};
use crate::bun::events::EventStore;
use crate::sesame::types::ApiRole;

/// The address every test request comes from.
fn caller() -> std::net::SocketAddr {
    "203.0.113.9:40000".parse().unwrap()
}

/// An event store with a durable audit log in a temporary directory.
struct Audited {
    events: Arc<RwLock<EventStore>>,
    log: AuditLog,
    _directory: tempfile::TempDir,
}

fn audited() -> Audited {
    let directory = tempfile::tempdir().unwrap();
    let log = AuditLog::open(
        directory.path().join("audit"),
        AuditLimits::from_total(1 << 20),
    )
    .unwrap();
    let mut store = EventStore::new();
    store.set_audit_log(log.clone());
    Audited {
        events: Arc::new(RwLock::new(store)),
        log,
        _directory: directory,
    }
}

impl Audited {
    async fn records(&self, action: &str) -> Vec<AuditRecord> {
        self.log
            .query(AuditFilter {
                action: Some(action.to_string()),
                ..AuditFilter::default()
            })
            .await
            .unwrap()
    }
}

/// Send one request as `bearer` from [`caller`], the way the server's
/// connection layer attaches the client's address.
async fn call(
    app: &Router,
    method: &str,
    uri: &str,
    bearer: &str,
    body: &str,
) -> (StatusCode, String) {
    let mut request = axum::http::Request::builder()
        .method(method)
        .uri(uri)
        .header("authorization", format!("Bearer {bearer}"))
        .extension(axum::extract::ConnectInfo(caller()));
    if uri != "/v1/apply" && !body.is_empty() {
        request = request.header("content-type", "application/json");
    }
    let response = app
        .clone()
        .oneshot(request.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

fn principal_of(token: &crate::sesame::types::ApiToken, plaintext: &str) -> String {
    crate::sesame::auth::authenticate(plaintext, std::slice::from_ref(token))
        .unwrap()
        .principal_id
}

/// A standalone router whose agent says yes to every exec and stop.
async fn answering_agent_router(
    tokens: Vec<crate::sesame::types::ApiToken>,
    events: Arc<RwLock<EventStore>>,
) -> Router {
    let (commands, mut receiver) = mpsc::channel(8);
    tokio::spawn(async move {
        while let Some(command) = receiver.recv().await {
            match command {
                AgentCommand::Exec { response, .. } => {
                    let _ = response.send(Ok(crate::grill::ExecOutput {
                        output: "ok".into(),
                        exit_code: 0,
                    }));
                }
                AgentCommand::Stop { response, .. } => {
                    let _ = response.send(Ok(()));
                }
                _ => {}
            }
        }
    });
    let store = crate::sesame::auth::new_token_store();
    *store.write().await = tokens;
    router(
        commands,
        None,
        None,
        None,
        None,
        None,
        None,
        Some(store),
        None,
        None,
        None,
        None,
        9117,
        Some(events),
    )
}

#[tokio::test]
async fn apply_records_an_audit_entry_with_namespace_and_principal() {
    let (token, plaintext) = named_user_token("alice", ApiRole::Deployer);
    let principal = principal_of(&token, &plaintext);
    let audit = audited();
    let (app, shutdown) = setup_with_auth_readiness_leases_and_events(
        vec![token],
        None,
        crate::bun::readiness::ReadinessTracker::new(),
        crate::bun::capabilities::StaticCapabilities::default(),
        None,
        Some(Arc::clone(&audit.events)),
    )
    .await;
    let manifest = "[app.web]\nimage = \"test:v1\"\nnamespace = \"team-a\"\n";
    let (status, body) = call(&app, "POST", "/v1/apply", &plaintext, manifest).await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let records = audit.records("app.applied").await;
    assert_eq!(records.len(), 1, "{records:?}");
    let record = &records[0];
    assert_eq!(record.principal, principal);
    assert_eq!(record.namespace.as_deref(), Some("team-a"));
    assert_eq!(record.app.as_deref(), Some("web"));
    assert_eq!(
        record.details.get("token_name").map(String::as_str),
        Some("alice")
    );
    assert_eq!(
        record.details.get("workloads").map(String::as_str),
        Some("web@team-a")
    );
    assert_eq!(
        record.source_address.as_deref(),
        Some(caller().to_string().as_str())
    );
    // The ring and its live stream still show it.
    let ring = audit.events.read().await.recent(10, None, None);
    assert!(
        ring.iter()
            .any(|event| event.action.as_deref() == Some("app.applied"))
    );
    shutdown.cancel();
}

#[tokio::test]
async fn delete_exec_upgrade_and_decommission_are_audited() {
    // Exec and delete on a standalone node whose agent answers both.
    let (token, plaintext) = named_user_token("alice", ApiRole::Deployer);
    let audit = audited();
    let app = answering_agent_router(vec![token], Arc::clone(&audit.events)).await;
    let (status, body) = call(
        &app,
        "POST",
        "/v1/exec/web/default",
        &plaintext,
        r#"{"command":["true"]}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = call(&app, "POST", "/v1/delete/web/default", &plaintext, "").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    for action in ["app.exec", "app.deleted"] {
        let records = audit.records(action).await;
        assert_eq!(records.len(), 1, "{action}: {records:?}");
        assert_eq!(records[0].app.as_deref(), Some("web"));
        assert_eq!(records[0].namespace.as_deref(), Some("default"));
    }
    // A refused request changed nothing, so it records nothing.
    let (status, _) = call(&app, "POST", "/v1/delete/web/default", "rbrg_wrong", "").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(audit.records("app.deleted").await.len(), 1);

    // Decommission, through a council.
    let council = seeded_council("audit-decommission").await;
    let (admin, admin_key) = named_user_token("operator", ApiRole::Admin);
    let audit = audited();
    let (app, shutdown) = setup_with_auth_leases_events_and_council(
        vec![admin],
        Some("internal".into()),
        crate::bun::readiness::ReadinessTracker::new(),
        lease_static_capabilities(),
        None,
        Some(Arc::clone(&audit.events)),
        Some(council.clone()),
    )
    .await;
    let body = r#"{"node_id":"worker","workloads_stopped":true,"reason":"powered off"}"#;
    let (status, reply) = call(&app, "POST", "/v1/nodes/decommission", &admin_key, body).await;
    assert_eq!(status, StatusCode::OK, "{reply}");
    let records = audit.records("node.decommissioned").await;
    assert_eq!(records.len(), 1);
    assert_eq!(
        records[0].details.get("target_node").map(String::as_str),
        Some("worker")
    );
    assert_eq!(
        records[0].details.get("reason").map(String::as_str),
        Some("powered off")
    );
    shutdown.cancel();
    council.shutdown().await.unwrap();

    // Upgrade abort, on a worker that forwards it to the leader.
    upgrade_abort_is_audited_where_it_was_asked().await;
}

/// An upgrade call a worker forwards to the leader is recorded on the
/// worker, which is where the operator's request arrived.
async fn upgrade_abort_is_audited_where_it_was_asked() {
    use crate::council::CouncilNode;
    use crate::council::log_store::MemLogStore;
    use crate::council::network::{InMemoryRaftNetworkFactory, InMemoryRaftRouter};
    use crate::council::state_machine::CouncilStateMachine;
    use crate::council::types::CouncilConfig;

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let leader_api = listener.local_addr().unwrap();
    let _seen = super::tests::serve_recording_leader(listener);
    let worker = Arc::new(
        CouncilNode::new(
            9,
            CouncilConfig::default(),
            InMemoryRaftNetworkFactory::new(9, InMemoryRaftRouter::new()),
            MemLogStore::new(),
            CouncilStateMachine::new(),
            None,
        )
        .await
        .unwrap(),
    );
    let (_directory_tx, directory_rx) =
        tokio::sync::watch::channel(crate::mustard::directory::NodeDirectory {
            leader: Some(crate::mustard::message::LeaderHint {
                node_id: crate::meat::NodeId::new("node-1"),
                term: 1,
                recovery_epoch: 0,
                api_address: leader_api,
                reporting_address: leader_api,
            }),
            ..Default::default()
        });
    let (admin, admin_key) = named_user_token("operator", ApiRole::Admin);
    let (commands, _receiver) = mpsc::channel(4);
    let store = crate::sesame::auth::new_token_store();
    *store.write().await = vec![admin];
    let audit = audited();
    let app = router(
        commands,
        None,
        None,
        None,
        None,
        None,
        Some(worker.clone()),
        Some(store),
        Some("internal".into()),
        None,
        None,
        None,
        1,
        Some(Arc::clone(&audit.events)),
    )
    .layer(axum::Extension(LeaderDirectory(directory_rx)));
    let (status, reply) = call(&app, "POST", "/v1/upgrade/abort", &admin_key, "").await;
    assert_eq!(status, StatusCode::ACCEPTED, "{reply}");
    let records = audit.records("upgrade.aborted").await;
    assert_eq!(records.len(), 1);
    assert_eq!(
        records[0].details.get("token_name").map(String::as_str),
        Some("operator")
    );
    worker.shutdown().await.unwrap();
}

#[tokio::test]
async fn fault_injected_audit_carries_namespace_and_override_flags() {
    let (token, plaintext) = named_user_token("alice", ApiRole::Deployer);
    let audit = audited();
    let (app, shutdown) = setup_with_auth_readiness_leases_and_events(
        vec![token],
        None,
        crate::bun::readiness::ReadinessTracker::new(),
        workload_fault_static_capabilities(),
        None,
        Some(Arc::clone(&audit.events)),
    )
    .await;
    let body = serde_json::to_string(&crate::smoker::types::FaultRequest {
        fault_type: crate::smoker::types::FaultType::DnsNxdomain,
        target_service: "web".to_string(),
        namespace: None,
        target_instance: None,
        target_node: None,
        duration: std::time::Duration::from_secs(30),
        injected_by: "mallory".to_string(),
        reason: Some("game day".to_string()),
        include_leader: true,
        override_safety: true,
        acknowledged: true,
    })
    .unwrap();
    let (status, reply) = call(&app, "POST", "/v1/fault", &plaintext, &body).await;
    assert_eq!(status, StatusCode::OK, "{reply}");
    let records = audit.records("fault.injected").await;
    assert_eq!(records.len(), 1);
    let record = &records[0];
    // The API fills in `default` for a workload fault, and the record says
    // so rather than leaving the namespace out.
    assert_eq!(record.namespace.as_deref(), Some("default"));
    assert_eq!(record.app.as_deref(), Some("web"));
    assert_eq!(
        record.details.get("override_safety").map(String::as_str),
        Some("true")
    );
    assert_eq!(
        record.details.get("include_leader").map(String::as_str),
        Some("true")
    );
    assert_eq!(
        record.severity,
        crate::bun::events::EventSeverity::Critical,
        "an override is recorded with extra prominence"
    );
    assert_eq!(
        record.source_address.as_deref(),
        Some(caller().to_string().as_str())
    );
    shutdown.cancel();
}

#[tokio::test]
async fn audit_read_needs_cluster_wide_admin() {
    let (admin, admin_key) = named_user_token("root", ApiRole::Admin);
    let (mut scoped, scoped_key) = named_user_token("tenant-admin", ApiRole::Admin);
    scoped.scope.namespaces = Some(vec!["team-a".into()]);
    let (deployer, deployer_key) = named_user_token("ci", ApiRole::Deployer);
    let (reader, reader_key) = named_user_token("viewer", ApiRole::ReadOnly);
    let audit = audited();
    let (app, shutdown) = setup_with_auth_readiness_leases_and_events(
        vec![admin, scoped, deployer, reader],
        None,
        crate::bun::readiness::ReadinessTracker::new(),
        crate::bun::capabilities::StaticCapabilities::default(),
        None,
        Some(Arc::clone(&audit.events)),
    )
    .await;
    // Something to read: the admin's own failed-free action.
    let (status, _) = call(
        &app,
        "POST",
        "/v1/apply",
        &deployer_key,
        "[app.web]\nimage = \"test:v1\"\n",
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    for (key, expected) in [
        (&scoped_key, StatusCode::FORBIDDEN),
        (&deployer_key, StatusCode::FORBIDDEN),
        (&reader_key, StatusCode::FORBIDDEN),
    ] {
        assert_eq!(call(&app, "GET", "/v1/audit", key, "").await.0, expected);
    }
    let (status, body) = call(&app, "GET", "/v1/audit?action=app.*", &admin_key, "").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let listing: AuditListing = serde_json::from_str(&body).unwrap();
    assert_eq!(listing.records.len(), 1);
    assert_eq!(listing.records[0].action, "app.applied");
    assert!(listing.warnings.is_empty(), "{:?}", listing.warnings);
    // Reading the log is not itself a change, so it isn't audited.
    assert!(audit.records("audit.*").await.is_empty());
    shutdown.cancel();
}

#[tokio::test]
async fn service_principal_cannot_read_the_audit_log() {
    let (admin, _) = named_user_token("root", ApiRole::Admin);
    let audit = audited();
    let (app, shutdown) = setup_with_auth_readiness_leases_and_events(
        vec![admin],
        Some("internal".into()),
        crate::bun::readiness::ReadinessTracker::new(),
        crate::bun::capabilities::StaticCapabilities::default(),
        None,
        Some(Arc::clone(&audit.events)),
    )
    .await;
    let (status, _) = call(&app, "GET", "/v1/audit", "internal", "").await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = call(&app, "GET", "/v1/audit?cluster=true", "internal", "").await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    shutdown.cancel();
}

#[tokio::test]
async fn a_node_without_an_audit_log_says_so() {
    let (admin, admin_key) = named_user_token("root", ApiRole::Admin);
    let (app, shutdown) = setup_with_auth_readiness_leases_and_events(
        vec![admin],
        None,
        crate::bun::readiness::ReadinessTracker::new(),
        crate::bun::capabilities::StaticCapabilities::default(),
        None,
        Some(Arc::new(RwLock::new(EventStore::new()))),
    )
    .await;
    let (status, body) = call(&app, "GET", "/v1/audit", &admin_key, "").await;
    assert_eq!(status, StatusCode::OK);
    let listing: AuditListing = serde_json::from_str(&body).unwrap();
    assert!(listing.records.is_empty());
    assert_eq!(listing.warnings.len(), 1);
    assert!(listing.warnings[0].contains("keeps no audit log"));
    shutdown.cancel();
}

#[tokio::test]
async fn token_creation_is_audited_without_its_plaintext() {
    let (admin, admin_key) = named_user_token("root", ApiRole::Admin);
    let council = seeded_council("audit-token-create").await;
    council
        .write(crate::council::RaftRequest::CreateApiToken(admin.clone()))
        .await
        .unwrap();
    let audit = audited();
    let (app, shutdown) = setup_with_auth_leases_events_and_council(
        vec![admin],
        None,
        crate::bun::readiness::ReadinessTracker::new(),
        crate::bun::capabilities::StaticCapabilities::default(),
        None,
        Some(Arc::clone(&audit.events)),
        Some(council.clone()),
    )
    .await;
    let (status, body) = call(
        &app,
        "POST",
        "/v1/token/create",
        &admin_key,
        r#"{"name":"ci","role":"deployer"}"#,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let created: serde_json::Value = serde_json::from_str(&body).unwrap();
    let plaintext = created["token"].as_str().unwrap().to_string();
    assert!(plaintext.starts_with("rbrg_"));
    let records = audit.records("token.created").await;
    assert_eq!(records.len(), 1);
    assert_eq!(
        records[0].source_address.as_deref(),
        Some(caller().to_string().as_str())
    );
    let on_disk: String = std::fs::read_dir(audit.log.directory())
        .unwrap()
        .map(|entry| std::fs::read_to_string(entry.unwrap().path()).unwrap())
        .collect();
    assert!(on_disk.contains("token.created"));
    assert!(!on_disk.contains(&plaintext), "the audit log holds a token");
    shutdown.cancel();
    council.shutdown().await.unwrap();
}

#[tokio::test]
async fn a_broken_audit_log_does_not_fail_the_call() {
    let (token, plaintext) = named_user_token("alice", ApiRole::Deployer);
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("audit");
    // One record per segment, then no directory to make the next one in.
    let log = AuditLog::open(
        path.clone(),
        AuditLimits {
            max_total_bytes: 1 << 20,
            segment_bytes: 1,
        },
    )
    .unwrap();
    assert!(
        log.submit(crate::bun::events::AuditEvent {
            timestamp: 1,
            kind: crate::bun::events::EventKind::Operation,
            severity: crate::bun::events::EventSeverity::Info,
            action: "app.applied".to_string(),
            principal: "token:alice".to_string(),
            app: None,
            namespace: None,
            node: None,
            source_address: None,
            details: Default::default(),
            message: String::new(),
        })
        .written()
        .await
    );
    std::fs::remove_dir_all(&path).unwrap();
    std::fs::write(&path, b"not a directory").unwrap();
    let mut store = EventStore::new();
    store.set_audit_log(log.clone());
    let (app, shutdown) = setup_with_auth_readiness_leases_and_events(
        vec![token],
        None,
        crate::bun::readiness::ReadinessTracker::new(),
        crate::bun::capabilities::StaticCapabilities::default(),
        None,
        Some(Arc::new(RwLock::new(store))),
    )
    .await;
    let (status, body) = call(
        &app,
        "POST",
        "/v1/apply",
        &plaintext,
        "[app.web]\nimage = \"test:v1\"\n",
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(log.failures(), 1);
    shutdown.cancel();
}

/// Every `POST` and `DELETE` route is audited by the layer, audits itself,
/// or is listed as unaudited with a reason. A new mutating route has to
/// choose.
#[test]
fn every_mutating_route_is_audited_or_exempt() {
    use crate::bun::authz::{Method, ROUTE_MATRIX};
    let mut unaccounted = Vec::new();
    for route in ROUTE_MATRIX
        .iter()
        .filter(|route| route.method != Method::Get)
    {
        let listed = AUDITED_ROUTES
            .iter()
            .filter(|audited| audited.method == route.method && audited.path == route.path)
            .count()
            + HANDLER_AUDITED_ROUTES
                .iter()
                .filter(|(method, path)| *method == route.method && *path == route.path)
                .count()
            + UNAUDITED_ROUTES
                .iter()
                .filter(|(method, path, _)| *method == route.method && *path == route.path)
                .count();
        if listed != 1 {
            unaccounted.push(format!(
                "{:?} {} (listed {listed} times)",
                route.method, route.path
            ));
        }
    }
    assert!(unaccounted.is_empty(), "{unaccounted:#?}");
    // And every audited route is really mounted.
    for audited in AUDITED_ROUTES {
        assert!(
            ROUTE_MATRIX
                .iter()
                .any(|route| route.method == audited.method && route.path == audited.path),
            "{} is not a mounted route",
            audited.path
        );
    }
}

/// A Deployer token confined to namespace `a`.
fn scoped_token(name: &str) -> (crate::sesame::types::ApiToken, String) {
    let created = crate::sesame::token::create_token(
        name,
        ApiRole::Deployer,
        crate::sesame::types::TokenScope {
            apps: None,
            namespaces: Some(vec!["a".to_string()]),
        },
        None,
    )
    .unwrap();
    (created.token, created.plaintext)
}

/// A standalone router with `tokens` and the durable log of `audit`.
async fn refusing_router(
    tokens: Vec<crate::sesame::types::ApiToken>,
    audit: &Audited,
) -> (Router, tokio_util::sync::CancellationToken) {
    setup_with_auth_readiness_leases_and_events(
        tokens,
        None,
        crate::bun::readiness::ReadinessTracker::new(),
        crate::bun::capabilities::StaticCapabilities::default(),
        None,
        Some(Arc::clone(&audit.events)),
    )
    .await
}

fn detail<'a>(record: &'a AuditRecord, key: &str) -> Option<&'a str> {
    record.details.get(key).map(String::as_str)
}

#[tokio::test]
async fn a_refused_apply_is_audited_with_its_reason() {
    let (scoped, scoped_key) = scoped_token("tenant-a");
    let scoped_principal = principal_of(&scoped, &scoped_key);
    let (reader, reader_key) = named_user_token("viewer", ApiRole::ReadOnly);
    let audit = audited();
    let (app, shutdown) = refusing_router(vec![scoped, reader], &audit).await;

    // A scoped token reaching another namespace.
    let manifest = "[app.web]\nimage = \"test:v1\"\nnamespace = \"b\"\n";
    let (status, _) = call(&app, "POST", "/v1/apply", &scoped_key, manifest).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    // A role that may not apply at all.
    let (status, _) = call(&app, "POST", "/v1/apply", &reader_key, manifest).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    // No credential the node knows.
    let (status, _) = call(&app, "POST", "/v1/delete/web/b", "rbrg_wrong", "").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    // Nothing happened, so nothing is recorded as done.
    assert!(audit.records("app.applied").await.is_empty());

    let records = audit.records("api.refused").await;
    assert_eq!(records.len(), 3, "{records:?}");
    let scope = &records[0];
    assert_eq!(scope.principal, scoped_principal);
    assert_eq!(detail(scope, "reason"), Some("scope"));
    assert_eq!(detail(scope, "route"), Some("POST /v1/apply"));
    assert_eq!(detail(scope, "attempted"), Some("app.applied"));
    assert_eq!(detail(scope, "token_name"), Some("tenant-a"));
    assert_eq!(scope.namespace.as_deref(), Some("b"));
    assert_eq!(
        scope.source_address.as_deref(),
        Some(caller().to_string().as_str())
    );
    assert_eq!(scope.severity, crate::bun::events::EventSeverity::Warning);

    let role = &records[1];
    assert_eq!(detail(role, "reason"), Some("forbidden"));
    assert_eq!(detail(role, "token_name"), Some("viewer"));

    let anonymous = &records[2];
    assert_eq!(anonymous.principal, "anonymous");
    assert_eq!(detail(anonymous, "reason"), Some("unauthenticated"));
    assert_eq!(
        detail(anonymous, "route"),
        Some("POST /v1/delete/{app}/{namespace}")
    );
    assert_eq!(anonymous.app.as_deref(), Some("web"));
    assert_eq!(anonymous.namespace.as_deref(), Some("b"));
    assert!(!anonymous.details.contains_key("token_name"));
    shutdown.cancel();
}

#[tokio::test]
async fn a_burst_of_refusals_is_not_stored_one_by_one() {
    let (token, _) = named_user_token("alice", ApiRole::Deployer);
    let audit = audited();
    let (app, shutdown) = refusing_router(vec![token], &audit).await;
    for _ in 0..30 {
        let (status, _) = call(&app, "POST", "/v1/delete/web/b", "rbrg_wrong", "").await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }
    let records = audit.records("api.refused").await;
    assert_eq!(
        records.len(),
        crate::bun::audit::refusals::REFUSALS_PER_WINDOW as usize
    );
    shutdown.cancel();
}

#[tokio::test]
async fn refusals_of_unaudited_routes_are_not_recorded() {
    let (token, _) = named_user_token("alice", ApiRole::Deployer);
    let audit = audited();
    let (app, shutdown) = refusing_router(vec![token], &audit).await;
    let (status, _) = call(&app, "GET", "/v1/status", "rbrg_wrong", "").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert!(audit.records("api.refused").await.is_empty());
    shutdown.cancel();
}

#[tokio::test]
async fn refused_requests_never_write_a_token_plaintext() {
    let (scoped, scoped_key) = scoped_token("tenant-a");
    let audit = audited();
    let (app, shutdown) = refusing_router(vec![scoped], &audit).await;
    // A well-formed token the node doesn't know, and a real one refused by
    // its scope.
    let unknown = format!("rbrg_{}", "ab".repeat(32));
    let (status, _) = call(&app, "POST", "/v1/apply", &unknown, "[app.x]\n").await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let manifest = "[app.web]\nimage = \"test:v1\"\nnamespace = \"b\"\n";
    let (status, _) = call(&app, "POST", "/v1/apply", &scoped_key, manifest).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(audit.records("api.refused").await.len(), 2);
    let on_disk: String = std::fs::read_dir(audit.log.directory())
        .unwrap()
        .map(|entry| std::fs::read_to_string(entry.unwrap().path()).unwrap())
        .collect();
    assert!(on_disk.contains("api.refused"));
    for secret in [&unknown, &scoped_key] {
        assert!(
            !on_disk.contains(secret.as_str()),
            "the audit log holds a token"
        );
    }
    assert!(
        !on_disk.contains("test:v1"),
        "the audit log holds a request body"
    );
    shutdown.cancel();
}

#[test]
fn a_suppressed_refusal_summary_says_how_many_and_from_whom() {
    use crate::bun::audit::refusals::{RefusalSource, SuppressedRefusals};
    let event = super::audit::suppressed_refusals_event(
        SuppressedRefusals {
            source: RefusalSource::Caller {
                address: Some("203.0.113.9".parse().unwrap()),
                principal: "anonymous".to_string(),
            },
            suppressed: 45,
            window_start: 1_800_000_000,
        },
        "node-1".to_string(),
        1_800_000_060,
    );
    assert_eq!(event.action, "api.refusals_suppressed");
    assert_eq!(event.principal, "anonymous");
    assert_eq!(
        event.details.get("suppressed").map(String::as_str),
        Some("45")
    );
    assert_eq!(
        event.details.get("source").map(String::as_str),
        Some("203.0.113.9")
    );
    assert_eq!(
        event.details.get("window_start").map(String::as_str),
        Some("1800000000")
    );
    assert!(
        event.message.contains("45 more refused requests"),
        "{}",
        event.message
    );

    let overflow = super::audit::suppressed_refusals_event(
        SuppressedRefusals {
            source: RefusalSource::Overflow,
            suppressed: 7,
            window_start: 1_800_000_000,
        },
        "node-1".to_string(),
        1_800_000_060,
    );
    assert_eq!(overflow.principal, "*");
    assert!(
        overflow.message.contains("other sources"),
        "{}",
        overflow.message
    );
}
