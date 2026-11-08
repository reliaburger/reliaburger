//! Agent proposals through the real router (#1682): who may propose,
//! approve, reject, withdraw and undo, what each answers, and what the
//! audit log keeps.

use axum::body::Body;
use http_body_util::BodyExt;
use tower::ServiceExt;

use super::tests::{named_user_token, seeded_council, setup_with_auth_leases_events_and_council};
use super::*;
use crate::bun::audit::{AuditFilter, AuditLimits, AuditLog, AuditRecord};
use crate::bun::events::EventStore;
use crate::sesame::types::{ApiRole, TokenKind, TokenScope};

struct Cluster {
    app: Router,
    council: Arc<crate::council::CouncilNode>,
    log: AuditLog,
    /// A deployer agent token, scoped to `shop`.
    holmes: String,
    /// A read-only agent token.
    watcher: String,
    /// A deployer agent token scoped to `billing`.
    billing_bot: String,
    /// A human admin.
    alice: String,
    /// A human deployer.
    bob: String,
    /// A human deployer scoped to `billing`.
    carol: String,
    _directory: tempfile::TempDir,
    _shutdown: tokio_util::sync::CancellationToken,
}

fn agent_token(
    name: &str,
    role: ApiRole,
    namespaces: Option<Vec<&str>>,
) -> crate::sesame::token::CreatedToken {
    let mut created = crate::sesame::token::create_token(
        name,
        role,
        TokenScope {
            apps: None,
            namespaces: namespaces.map(|list| list.into_iter().map(str::to_string).collect()),
        },
        None,
    )
    .unwrap();
    created.token.kind = TokenKind::Agent;
    created
}

fn web_spec(replicas: u32) -> Box<crate::config::app::AppSpec> {
    Box::new(
        toml::from_str(&format!(
            "image = \"web:1.4\"\nreplicas = {replicas}\nnamespace = \"shop\""
        ))
        .unwrap(),
    )
}

async fn cluster(tag: &str) -> Cluster {
    let council = seeded_council(tag).await;
    for (name, namespace) in [("web", "shop"), ("api", "shop"), ("ledger", "billing")] {
        let mut spec = web_spec(3);
        spec.namespace = Some(namespace.to_string());
        council
            .write(crate::council::types::RaftRequest::AppSpec {
                app_id: crate::meat::AppId::new(name, namespace),
                spec,
            })
            .await
            .unwrap();
    }
    let holmes = agent_token("holmes", ApiRole::Deployer, Some(vec!["shop"]));
    let watcher = agent_token("watcher", ApiRole::ReadOnly, None);
    let billing_bot = agent_token("billing-bot", ApiRole::Deployer, Some(vec!["billing"]));
    let (alice_token, alice) = named_user_token("alice", ApiRole::Admin);
    let (bob_token, bob) = named_user_token("bob", ApiRole::Deployer);
    let carol = crate::sesame::token::create_token(
        "carol",
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
            holmes.token,
            watcher.token,
            billing_bot.token,
            alice_token,
            bob_token,
            carol.token,
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
        holmes: holmes.plaintext,
        watcher: watcher.plaintext,
        billing_bot: billing_bot.plaintext,
        alice,
        bob,
        carol: carol.plaintext,
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

    async fn get(&self, uri: &str, bearer: &str) -> (StatusCode, serde_json::Value) {
        self.send(axum::http::Request::get(uri), bearer, String::new())
            .await
    }

    async fn propose(
        &self,
        bearer: &str,
        body: serde_json::Value,
    ) -> (StatusCode, serde_json::Value) {
        self.post("/v1/proposals", bearer, body.to_string()).await
    }

    async fn scale(
        &self,
        bearer: &str,
        namespace: &str,
        app: &str,
        replicas: u32,
    ) -> (StatusCode, serde_json::Value) {
        self.propose(
            bearer,
            serde_json::json!({
                "kind": "scale", "namespace": namespace, "app": app,
                "replicas": replicas, "reason": "p99 over the SLO",
            }),
        )
        .await
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

    async fn replicas(&self, app: &str, namespace: &str) -> crate::config::Replicas {
        self.council.desired_state().await.apps[&crate::meat::AppId::new(app, namespace)].replicas
    }
}

/// The whole path: an agent proposes, the answer carries the cluster's
/// summary, blast radius and undo command, a person approves and it
/// applies, and the undo command puts it back. Each step is audited with
/// the author, its kind and its reason.
#[tokio::test]
async fn an_agent_proposes_a_person_approves_and_undo_puts_it_back() {
    let cluster = cluster("proposal-path").await;
    let (status, body) = cluster.scale(&cluster.holmes, "shop", "web", 5).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["id"], 1);
    assert_eq!(body["state"], "pending");
    assert_eq!(body["author_kind"], "agent");
    assert_eq!(body["summary"][0], "Scale shop/web from 3 to 5 replicas.");
    assert_eq!(body["undo"]["command"], "relish undo 1");
    assert_eq!(body["approval"]["needs"], "human");
    assert_eq!(body["leases"][0], "app:shop/web");
    assert_eq!(body["reason"], "p99 over the SLO");
    assert_eq!(
        cluster.replicas("web", "shop").await,
        crate::config::Replicas::Fixed(3)
    );

    let (status, body) = cluster
        .post("/v1/proposals/1/approve", &cluster.bob, String::new())
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["state"], "applied");
    assert_eq!(body["approval"]["approved_by"], "bob");
    assert_eq!(
        cluster.replicas("web", "shop").await,
        crate::config::Replicas::Fixed(5)
    );
    // The same approver asking again gets the outcome.
    let (status, body) = cluster
        .post("/v1/proposals/1/approve", &cluster.bob, String::new())
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["state"], "applied");

    let (status, body) = cluster
        .post("/v1/proposals/1/undo", &cluster.alice, String::new())
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["state"], "undone");
    assert_eq!(
        cluster.replicas("web", "shop").await,
        crate::config::Replicas::Fixed(3)
    );

    let created = cluster.records("proposal.created").await;
    assert_eq!(created.len(), 1);
    assert_eq!(created[0].details["author"], "holmes");
    assert_eq!(created[0].details["author_kind"], "agent");
    assert_eq!(created[0].details["reason"], "p99 over the SLO");
    assert_eq!(created[0].details["undo"], "relish undo 1");
    let applied = cluster.records("proposal.applied").await;
    assert_eq!(applied.len(), 1);
    assert_eq!(applied[0].details["approved_by"], "bob");
    assert_eq!(applied[0].details["token_name"], "bob");
    assert_eq!(applied[0].details["principal_kind"], "human");
    assert_eq!(cluster.records("proposal.undone").await.len(), 1);
}

/// Built-in rules 1 and 2: an agent token can't change anything directly,
/// can't approve anything, and nobody approves their own proposal.
#[tokio::test]
async fn agents_never_mutate_or_approve_and_nobody_approves_their_own() {
    let cluster = cluster("proposal-rules").await;
    // A deployer agent token can't apply, stop or delete directly.
    let (status, body) = cluster
        .post("/v1/stop/web/shop", &cluster.holmes, String::new())
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert!(
        body.to_string().contains("relish agent propose"),
        "the refusal points at proposing: {body}"
    );
    let (status, _) = cluster
        .post("/v1/delete/web/shop", &cluster.holmes, String::new())
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    cluster.scale(&cluster.holmes, "shop", "web", 5).await;
    // Holmes can't approve its own; another agent can't approve at all.
    let (status, body) = cluster
        .post("/v1/proposals/1/approve", &cluster.holmes, String::new())
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    let (status, body) = cluster
        .post(
            "/v1/proposals/1/reject",
            &cluster.billing_bot,
            "{}".to_string(),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    let (status, _) = cluster
        .post("/v1/proposals/1/undo", &cluster.holmes, String::new())
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // A person's own proposal needs another person.
    let (status, body) = cluster.scale(&cluster.bob, "shop", "api", 4).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let id = body["id"].as_u64().unwrap();
    let (status, body) = cluster
        .post(
            &format!("/v1/proposals/{id}/approve"),
            &cluster.bob,
            String::new(),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(body["code"], "forbidden");
    let (status, body) = cluster
        .post(
            &format!("/v1/proposals/{id}/approve"),
            &cluster.alice,
            String::new(),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

/// Scope and role: a read-only token can't propose, an agent can't propose
/// outside its namespace or see proposals there, and an approver must
/// cover every touched app.
#[tokio::test]
async fn scope_and_role_bound_who_proposes_sees_and_approves() {
    let cluster = cluster("proposal-scope").await;
    let (status, body) = cluster.scale(&cluster.watcher, "shop", "web", 5).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    let (status, body) = cluster.scale(&cluster.holmes, "billing", "ledger", 5).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    let (status, body) = cluster
        .scale(&cluster.billing_bot, "billing", "ledger", 5)
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let ledger = body["id"].as_u64().unwrap();
    let (status, _) = cluster.scale(&cluster.holmes, "shop", "web", 5).await;
    assert_eq!(status, StatusCode::CREATED);

    // Holmes sees only shop's proposal; the read-only agent sees both.
    let (_, listing) = cluster.get("/v1/proposals", &cluster.holmes).await;
    assert_eq!(listing["proposals"].as_array().unwrap().len(), 1);
    let (status, _) = cluster
        .get(&format!("/v1/proposals/{ledger}"), &cluster.holmes)
        .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (_, listing) = cluster.get("/v1/proposals", &cluster.watcher).await;
    assert_eq!(listing["proposals"].as_array().unwrap().len(), 2);

    // Carol covers billing, not shop.
    let (status, _) = cluster
        .post("/v1/proposals/2/approve", &cluster.carol, String::new())
        .await;
    assert!(
        status == StatusCode::FORBIDDEN || status == StatusCode::NOT_FOUND,
        "{status}"
    );
    let (status, body) = cluster
        .post(
            &format!("/v1/proposals/{ledger}/approve"),
            &cluster.carol,
            String::new(),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
}

/// A second proposal on the same app names the first; another agent's
/// proposal on a leased app is `lease_held`; a person's write in between
/// fails the proposal as stale and writes nothing.
#[tokio::test]
async fn conflicts_leases_and_stale_proposals() {
    let cluster = cluster("proposal-conflicts").await;
    cluster.scale(&cluster.holmes, "shop", "web", 5).await;
    let (status, body) = cluster.scale(&cluster.holmes, "shop", "web", 6).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["code"], "conflict");
    assert_eq!(body["proposal"], 1);

    // Bob holds a lease on api; holmes's proposal there is refused.
    let (status, body) = cluster
        .post(
            "/v1/leases",
            &cluster.bob,
            r#"{"target":{"kind":"app","namespace":"shop","app":"api"},"purpose":"by hand"}"#
                .to_string(),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (status, body) = cluster.scale(&cluster.holmes, "shop", "api", 4).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["code"], "lease_held");
    assert_eq!(body["lease"]["holder"], "bob");
    let refused = cluster.records("proposal.refused").await;
    assert!(
        refused
            .iter()
            .any(|record| record.details["code"] == "lease_held")
    );

    // A person writes web in between: approval fails, nothing changes.
    cluster
        .council
        .write(crate::council::types::RaftRequest::AppSpec {
            app_id: crate::meat::AppId::new("web", "shop"),
            spec: web_spec(4),
        })
        .await
        .unwrap();
    let (status, body) = cluster
        .post("/v1/proposals/1/approve", &cluster.alice, String::new())
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["state"], "failed");
    assert!(
        body["outcome"].as_str().unwrap().contains("changed"),
        "{body}"
    );
    assert_eq!(
        cluster.replicas("web", "shop").await,
        crate::config::Replicas::Fixed(4)
    );
    assert_eq!(cluster.records("proposal.failed").await.len(), 1);
}

/// A dry run answers the whole proposal and stores nothing; the author
/// withdraws; a person rejects; a retry with the idempotency key returns
/// the same proposal.
#[tokio::test]
async fn dry_run_withdraw_reject_and_idempotency() {
    let cluster = cluster("proposal-lifecycle").await;
    let (status, body) = cluster
        .post(
            "/v1/proposals?dry_run=true",
            &cluster.holmes,
            serde_json::json!({"kind": "stop", "namespace": "shop", "app": "web", "reason": "x"})
                .to_string(),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["summary"][0], "Stop shop/web.");
    assert!(
        cluster
            .council
            .desired_state()
            .await
            .proposals
            .proposals
            .is_empty()
    );

    let keyed = serde_json::json!({
        "kind": "scale", "namespace": "shop", "app": "web", "replicas": 5,
        "reason": "x", "idempotency_key": "holmes-1",
    });
    let (_, first) = cluster.propose(&cluster.holmes, keyed.clone()).await;
    let (status, again) = cluster.propose(&cluster.holmes, keyed).await;
    assert_eq!(status, StatusCode::OK, "{again}");
    assert_eq!(first["id"], again["id"]);

    let (status, body) = cluster
        .post(
            "/v1/proposals/1/withdraw",
            &cluster.billing_bot,
            String::new(),
        )
        .await;
    assert!(status.is_client_error(), "{body}");
    let (status, body) = cluster
        .post("/v1/proposals/1/withdraw", &cluster.holmes, String::new())
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["state"], "withdrawn");

    let (_, second) = cluster.scale(&cluster.holmes, "shop", "web", 6).await;
    let id = second["id"].as_u64().unwrap();
    let (status, body) = cluster
        .post(
            &format!("/v1/proposals/{id}/reject"),
            &cluster.alice,
            r#"{"reason":"not during the sale"}"#.to_string(),
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["state"], "rejected");
    assert_eq!(body["outcome"], "not during the sale");
    assert_eq!(cluster.records("proposal.withdrawn").await.len(), 1);
    assert_eq!(cluster.records("proposal.rejected").await.len(), 1);
}

/// An app policy's deny refuses a proposal at creation with the error code
/// `guardrail`, naming the policy, and the refusal is audited.
#[tokio::test]
async fn an_app_policy_refuses_a_proposal_as_a_guardrail() {
    let cluster = cluster("proposal-policy").await;
    let policy: crate::config::PolicySpec =
        toml::from_str("deny = 'app.replicas > 4'\nmessage = \"at most four replicas\"").unwrap();
    cluster
        .council
        .write(crate::council::types::RaftRequest::PolicySpec {
            name: "small".to_string(),
            spec: Box::new(policy),
        })
        .await
        .unwrap();
    let (status, body) = cluster.scale(&cluster.holmes, "shop", "web", 5).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(body["code"], "guardrail");
    assert_eq!(body["rule"], "policy:small");
    let refused = cluster.records("proposal.refused").await;
    assert_eq!(refused[0].details["rule"], "policy:small");
    assert_eq!(refused[0].details["principal_kind"], "agent");
}

/// An agent token's lifetime and role are bounded when it's made.
#[tokio::test]
async fn an_agent_token_is_never_admin_and_short_lived() {
    let cluster = cluster("proposal-agent-token").await;
    let create = |body: serde_json::Value| {
        cluster.post("/v1/token/create", &cluster.alice, body.to_string())
    };
    let (status, body) =
        create(serde_json::json!({"name": "a1", "role": "admin", "agent": true})).await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    let (status, body) = create(
        serde_json::json!({"name": "a2", "role": "deployer", "agent": true, "ttl_seconds": 700_000}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    let (status, body) =
        create(serde_json::json!({"name": "a3", "role": "read-only", "agent": true})).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["kind"], "agent");
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let expires = body["expires_at"].as_u64().unwrap();
    assert!(
        (now + 8 * 3600 - 60..=now + 8 * 3600 + 60).contains(&expires),
        "{expires}"
    );
}
