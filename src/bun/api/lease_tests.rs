//! Agent leases through the real router (#1686): who may take, renew,
//! release, break and read them, what a conflict answers, and what the
//! audit log keeps.

use axum::body::Body;
use http_body_util::BodyExt;
use tower::ServiceExt;

use super::tests::{named_user_token, seeded_council, setup_with_auth_leases_events_and_council};
use super::*;
use crate::bun::audit::{AuditFilter, AuditLimits, AuditLog, AuditRecord};
use crate::bun::events::EventStore;
use crate::sesame::types::{ApiRole, TokenScope};

const WEB: &str = r#"{"kind":"app","namespace":"shop","app":"web"}"#;
const SHOP: &str = r#"{"kind":"namespace","namespace":"shop"}"#;

struct Cluster {
    app: Router,
    council: Arc<crate::council::CouncilNode>,
    log: AuditLog,
    events: Arc<RwLock<EventStore>>,
    holmes: String,
    capacity: String,
    alice: String,
    reader: String,
    scoped: String,
    _directory: tempfile::TempDir,
    _shutdown: tokio_util::sync::CancellationToken,
}

async fn cluster(tag: &str) -> Cluster {
    let council = seeded_council(tag).await;
    let (holmes_token, holmes) = named_user_token("holmes", ApiRole::Deployer);
    let (capacity_token, capacity) = named_user_token("capacity", ApiRole::Deployer);
    let (alice_token, alice) = named_user_token("alice", ApiRole::Admin);
    let (reader_token, reader) = named_user_token("reader", ApiRole::ReadOnly);
    let scoped_created = crate::sesame::token::create_token(
        "billing-bot",
        ApiRole::Deployer,
        TokenScope {
            apps: None,
            namespaces: Some(vec!["billing".to_string()]),
        },
        None,
    )
    .unwrap();
    let directory = tempfile::tempdir().unwrap();
    let log = AuditLog::open(
        directory.path().join("audit"),
        AuditLimits::from_total(1 << 20),
    )
    .unwrap();
    let mut store = EventStore::new();
    store.set_audit_log(log.clone());
    let events = Arc::new(RwLock::new(store));
    let (app, shutdown) = setup_with_auth_leases_events_and_council(
        vec![
            holmes_token,
            capacity_token,
            alice_token,
            reader_token,
            scoped_created.token,
        ],
        None,
        crate::bun::readiness::ReadinessTracker::new(),
        crate::bun::capabilities::StaticCapabilities::default(),
        None,
        Some(Arc::clone(&events)),
        Some(Arc::clone(&council)),
    )
    .await;
    Cluster {
        app,
        council,
        log,
        events,
        holmes,
        capacity,
        alice,
        reader,
        scoped: scoped_created.plaintext,
        _directory: directory,
        _shutdown: shutdown,
    }
}

impl Cluster {
    async fn send(
        &self,
        request: axum::http::request::Builder,
        bearer: &str,
        body: String,
    ) -> (StatusCode, serde_json::Value) {
        let request = request
            .header("authorization", format!("Bearer {bearer}"))
            .header("content-type", "application/json")
            .extension(axum::extract::ConnectInfo(
                "203.0.113.9:40000".parse::<std::net::SocketAddr>().unwrap(),
            ))
            .body(Body::from(body))
            .unwrap();
        let response = self.app.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let json = serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| serde_json::json!(String::from_utf8_lossy(&bytes)));
        (status, json)
    }

    async fn post(&self, uri: &str, bearer: &str, body: String) -> (StatusCode, serde_json::Value) {
        self.send(axum::http::Request::post(uri), bearer, body)
            .await
    }

    async fn take(
        &self,
        bearer: &str,
        target: &str,
        purpose: &str,
    ) -> (StatusCode, serde_json::Value) {
        self.post(
            "/v1/leases",
            bearer,
            format!(
                r#"{{"target":{target},"purpose":{}}}"#,
                serde_json::json!(purpose)
            ),
        )
        .await
    }

    async fn list(&self, bearer: &str) -> serde_json::Value {
        let (status, body) = self
            .send(
                axum::http::Request::get("/v1/leases"),
                bearer,
                String::new(),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        body
    }

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

/// An agent takes a free lease; a second agent is refused with 409
/// `lease_held`, told who holds it, why and for how long; both are audited.
#[tokio::test]
async fn a_second_holder_is_refused_with_who_holds_it() {
    let cluster = cluster("lease-conflict").await;
    let (status, body) = cluster
        .take(&cluster.holmes, WEB, "investigating crash loop")
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["outcome"], "taken");
    assert_eq!(body["lease"]["target"], "app:shop/web");
    assert_eq!(body["lease"]["holder"], "holmes");
    let left = body["lease"]["expires_in_seconds"].as_u64().unwrap();
    assert!((119..=120).contains(&left), "{left}");

    let (status, body) = cluster.take(&cluster.capacity, WEB, "scaling up").await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["code"], "lease_held");
    assert_eq!(body["lease"]["holder"], "holmes");
    assert_eq!(body["lease"]["purpose"], "investigating crash loop");
    assert!(body["lease"]["expires_in_seconds"].as_u64().unwrap() > 100);

    let taken = cluster.records("lease.taken").await;
    assert_eq!(taken.len(), 1);
    assert_eq!(taken[0].app.as_deref(), Some("web"));
    assert_eq!(taken[0].namespace.as_deref(), Some("shop"));
    assert_eq!(taken[0].details["holder"], "holmes");
    assert_eq!(taken[0].details["purpose"], "investigating crash loop");
    let refused = cluster.records("lease.refused").await;
    assert_eq!(refused.len(), 1);
    assert_eq!(refused[0].details["token_name"], "capacity");
    assert_eq!(refused[0].details["holder"], "holmes");
}

/// Taking your own lease again renews it; renew and release work only for
/// the holder; each is audited.
#[tokio::test]
async fn only_the_holder_renews_and_releases() {
    let cluster = cluster("lease-holder").await;
    cluster.take(&cluster.holmes, WEB, "first").await;
    let (status, body) = cluster.take(&cluster.holmes, WEB, "second").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["outcome"], "renewed");
    assert_eq!(body["lease"]["purpose"], "second");

    let renew = format!(r#"{{"target":{WEB},"ttl_seconds":600}}"#);
    let (status, body) = cluster
        .post("/v1/leases/renew", &cluster.capacity, renew.clone())
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["code"], "lease_held");
    let (status, body) = cluster
        .post("/v1/leases/renew", &cluster.holmes, renew)
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["outcome"], "renewed");
    assert!(body["lease"]["expires_in_seconds"].as_u64().unwrap() >= 599);

    let release = format!(r#"{{"target":{WEB}}}"#);
    let (status, _) = cluster
        .post("/v1/leases/release", &cluster.capacity, release.clone())
        .await;
    assert_eq!(status, StatusCode::CONFLICT);
    let (status, body) = cluster
        .post("/v1/leases/release", &cluster.holmes, release.clone())
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["outcome"], "released");
    let (status, body) = cluster
        .post("/v1/leases/release", &cluster.holmes, release)
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert!(
        cluster
            .council
            .desired_state()
            .await
            .agent_leases
            .leases
            .is_empty()
    );

    assert_eq!(cluster.records("lease.renewed").await.len(), 2);
    assert_eq!(cluster.records("lease.released").await.len(), 1);
}

/// A namespace lease keeps every app in it, even one that doesn't exist
/// yet, and a break ends it, recording who broke whose lease.
#[tokio::test]
async fn a_namespace_lease_blocks_its_apps_until_broken() {
    let cluster = cluster("lease-namespace").await;
    let (status, body) = cluster.take(&cluster.alice, SHOP, "mid-incident").await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = cluster.take(&cluster.holmes, WEB, "a fix").await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["lease"]["target"], "namespace:shop");

    let (status, body) = cluster
        .post(
            "/v1/leases/break",
            &cluster.holmes,
            format!(r#"{{"target":{SHOP}}}"#),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["outcome"], "broken");
    let broken = cluster.records("lease.broken").await;
    assert_eq!(broken.len(), 1);
    assert_eq!(broken[0].details["holder"], "alice");
    assert_eq!(broken[0].details["token_name"], "holmes");
    let (status, _) = cluster.take(&cluster.holmes, WEB, "a fix").await;
    assert_eq!(status, StatusCode::OK);
}

/// A read-only token reads leases but can't take one; a scoped token can't
/// lease outside its scope, and doesn't see leases there.
#[tokio::test]
async fn read_only_and_out_of_scope_tokens_cannot_take_a_lease() {
    let cluster = cluster("lease-authz").await;
    let (status, _) = cluster.take(&cluster.reader, WEB, "").await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, _) = cluster.take(&cluster.scoped, WEB, "").await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    let (status, body) = cluster
        .take(
            &cluster.scoped,
            r#"{"kind":"app","namespace":"billing","app":"api"}"#,
            "",
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    cluster.take(&cluster.holmes, WEB, "").await;

    let everything = cluster.list(&cluster.reader).await;
    assert_eq!(everything["leases"].as_array().unwrap().len(), 2);
    let billing = cluster.list(&cluster.scoped).await;
    let targets: Vec<_> = billing["leases"]
        .as_array()
        .unwrap()
        .iter()
        .map(|lease| lease["target"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(targets, ["app:billing/api"]);
    assert!(cluster.records("lease.taken").await.len() == 2);
}

/// A TTL outside 30..=900 seconds and a malformed target are refused before
/// anything is written; a purpose is cleaned before it's stored.
#[tokio::test]
async fn bad_requests_write_nothing_and_purposes_are_cleaned() {
    let cluster = cluster("lease-validation").await;
    for ttl in [29, 901] {
        let (status, body) = cluster
            .post(
                "/v1/leases",
                &cluster.holmes,
                format!(r#"{{"target":{WEB},"ttl_seconds":{ttl}}}"#),
            )
            .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    }
    let (status, _) = cluster
        .take(
            &cluster.holmes,
            r#"{"kind":"app","namespace":"shop","app":"a/b"}"#,
            "",
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(
        cluster
            .council
            .desired_state()
            .await
            .agent_leases
            .leases
            .is_empty()
    );

    let (status, body) = cluster
        .take(&cluster.holmes, WEB, "line one\nline two\u{1b}[31m")
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let purpose = body["lease"]["purpose"].as_str().unwrap();
    assert!(!purpose.chars().any(char::is_control), "{purpose:?}");
}

/// The leader's reaper removes a lapsed lease with a `lease.expired`
/// record under the `system` principal, and moves the generation.
#[tokio::test]
async fn the_reaper_expires_a_lapsed_lease_and_audits_it() {
    let cluster = cluster("lease-reaper").await;
    let (status, _) = cluster.take(&cluster.holmes, WEB, "brief").await;
    assert_eq!(status, StatusCode::OK);
    let lease = cluster.council.desired_state().await.agent_leases.leases["app:shop/web"].clone();
    let before = crate::bun::lease_reaper::expire_leases_once(
        &cluster.council,
        Some(&cluster.events),
        "node-1",
        lease.expires_at_unix_ms - 1,
    )
    .await
    .unwrap();
    assert!(before.is_empty());
    let expired = crate::bun::lease_reaper::expire_leases_once(
        &cluster.council,
        Some(&cluster.events),
        "node-1",
        lease.expires_at_unix_ms,
    )
    .await
    .unwrap();
    assert_eq!(expired.len(), 1);
    let leases = cluster.council.desired_state().await.agent_leases;
    assert!(leases.leases.is_empty());
    assert_eq!(leases.generation, 2);
    let records = cluster.records("lease.expired").await;
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].principal, "system");
    assert_eq!(records[0].details["holder"], "holmes");
    assert_eq!(
        cluster.list(&cluster.reader).await["leases"],
        serde_json::json!([])
    );
}
