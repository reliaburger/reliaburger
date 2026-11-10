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
    events: Arc<RwLock<EventStore>>,
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
        events,
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

    /// Commit `toml`'s policies, as an admin's `relish apply` would.
    async fn write_policies(&self, toml: &str) {
        for (name, spec) in crate::config::Config::parse(toml).unwrap().policy {
            self.council
                .write(crate::council::types::RaftRequest::PolicySpec {
                    name,
                    spec: Box::new(spec),
                })
                .await
                .unwrap();
        }
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
    assert_eq!(
        applied[0].principal_kind,
        crate::sesame::types::PrincipalKind::Human
    );
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

    // A person writes web in between: the approval answers `stale`, the
    // proposal is recorded failed, and nothing changes.
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
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["code"], "stale");
    assert_eq!(body["view"]["state"], "failed");
    assert!(
        body["view"]["outcome"]
            .as_str()
            .unwrap()
            .contains("changed"),
        "{body}"
    );
    // Asking again answers the same.
    let (status, body) = cluster
        .post("/v1/proposals/1/approve", &cluster.alice, String::new())
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["code"], "stale");
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
    assert_eq!(
        refused[0].principal_kind,
        crate::sesame::types::PrincipalKind::Agent
    );
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

/// The example guardrails (#1682).
const GUARDRAILS: &str = include_str!("../../../examples/agents/guardrails.toml");

/// An approve policy applies an agent's small scale at once, without a
/// person, and the audit log says which agent asked, why, and which policy
/// approved it.
#[tokio::test]
async fn an_approve_policy_applies_an_agents_small_scale_at_once() {
    let cluster = cluster("proposal-auto-approve").await;
    cluster.write_policies(GUARDRAILS).await;
    let (status, body) = cluster.scale(&cluster.holmes, "shop", "web", 5).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["state"], "applied", "{body}");
    assert_eq!(body["approval"]["approved_by"], "policy:agents-scale-shop");
    assert_eq!(
        cluster.replicas("web", "shop").await,
        crate::config::Replicas::Fixed(5)
    );
    let applied = cluster.records("proposal.applied").await;
    assert_eq!(applied.len(), 1);
    assert_eq!(applied[0].principal, "policy:agents-scale-shop");
    assert_eq!(
        applied[0].principal_kind,
        crate::sesame::types::PrincipalKind::Policy
    );
    let app_applied = cluster.records("app.applied").await;
    assert_eq!(app_applied.len(), 1, "{app_applied:?}");
    assert_eq!(app_applied[0].principal, "policy:agents-scale-shop");
    assert_eq!(
        app_applied[0].principal_kind,
        crate::sesame::types::PrincipalKind::Policy
    );
    assert_eq!(app_applied[0].details["author"], "holmes");
    assert_eq!(app_applied[0].details["proposal"], "1");
    assert_eq!(applied[0].details["author"], "holmes");
    assert_eq!(applied[0].details["author_kind"], "agent");
    assert_eq!(applied[0].details["reason"], "p99 over the SLO");
    assert_eq!(
        applied[0].details["approved_by"],
        "policy:agents-scale-shop"
    );
    // A person's undo still works on it.
    let (status, body) = cluster
        .post("/v1/proposals/1/undo", &cluster.alice, String::new())
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        cluster.replicas("web", "shop").await,
        crate::config::Replicas::Fixed(3)
    );
}

/// A deny guardrail refuses an agent's delete with the code `guardrail`,
/// naming the policy; a person's proposal to do the same isn't refused.
#[tokio::test]
async fn a_guardrail_refuses_an_agents_delete_but_not_a_persons() {
    let cluster = cluster("proposal-guardrail-delete").await;
    cluster.write_policies(GUARDRAILS).await;
    let delete = serde_json::json!({
        "kind": "delete", "namespace": "shop", "app": "web", "reason": "unused",
    });
    let (status, body) = cluster.propose(&cluster.holmes, delete.clone()).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(body["code"], "guardrail");
    assert_eq!(body["rule"], "policy:agents-never-delete");
    let refused = cluster.records("proposal.refused").await;
    assert_eq!(refused[0].details["rule"], "policy:agents-never-delete");

    let (status, body) = cluster.propose(&cluster.bob, delete).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["state"], "pending");
}

/// Built-in rule 3 beats a matching approve policy: a flagged proposal
/// waits for a person, and its creation record says why.
#[tokio::test]
async fn a_flagged_proposal_waits_for_a_person_even_when_a_policy_matches() {
    let cluster = cluster("proposal-held").await;
    cluster
        .write_policies(
            "[policy.yes]\napplies_to = 'proposals'\naction = 'approve'\nmatch = 'true'\n\
             [policy.big]\napplies_to = 'proposals'\naction = 'warn'\n\
             deny = 'proposal.replicas_to > 4'\nmessage = 'that is a lot of replicas'",
        )
        .await;
    let (status, body) = cluster.scale(&cluster.holmes, "shop", "web", 5).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["state"], "pending", "{body}");
    assert!(
        body["findings"][0]
            .as_str()
            .unwrap()
            .contains("that is a lot of replicas"),
        "{body}"
    );
    let created = cluster.records("proposal.created").await;
    assert!(
        created[0]
            .message
            .contains("policy yes matched, but a warn policy flagged it"),
        "{}",
        created[0].message
    );
    // The reaper doesn't approve it either.
    crate::bun::proposals::reap_once(
        &cluster.council,
        Some(&cluster.events),
        "node-1",
        crate::testkit::lease::now_unix_millis(),
    )
    .await
    .unwrap();
    let (_, body) = cluster.get("/v1/proposals/1", &cluster.bob).await;
    assert_eq!(body["state"], "pending");
}

/// A policy written after a proposal approves it on the reaper's next
/// pass, which is also how a new leader finishes an approval the old one
/// didn't.
#[tokio::test]
async fn the_reaper_approves_a_pending_proposal_a_later_policy_allows() {
    let cluster = cluster("proposal-reaper-approves").await;
    let (status, body) = cluster.scale(&cluster.holmes, "shop", "web", 5).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["state"], "pending");
    cluster.write_policies(GUARDRAILS).await;
    crate::bun::proposals::reap_once(
        &cluster.council,
        Some(&cluster.events),
        "node-1",
        crate::testkit::lease::now_unix_millis(),
    )
    .await
    .unwrap();
    let (_, body) = cluster.get("/v1/proposals/1", &cluster.bob).await;
    assert_eq!(body["state"], "applied", "{body}");
    assert_eq!(body["approval"]["approved_by"], "policy:agents-scale-shop");
    assert_eq!(
        cluster.replicas("web", "shop").await,
        crate::config::Replicas::Fixed(5)
    );
    let applied = cluster.records("proposal.applied").await;
    assert_eq!(applied[0].principal, "policy:agents-scale-shop");
    assert_eq!(applied[0].details["reason"], "p99 over the SLO");
}

/// An app policy that judges by who's asking, written after the proposal.
const ONLY_ADMINS_GO_BIG: &str = "[policy.only-admins-go-big]\n\
     deny = \"caller.role != 'admin' && app.replicas > 4\"\n\
     message = 'only an admin runs more than four replicas'\n";

/// A policy approval judges the proposal as its author: nobody else stands
/// behind it. A caller-conditioned deny written after the proposal was
/// made keeps the reaper from approving it, although the cluster itself
/// would pass the deny.
#[tokio::test]
async fn the_reaper_judges_a_policy_approval_as_the_author() {
    let cluster = cluster("proposal-reaper-as-author").await;
    let (status, body) = cluster.scale(&cluster.holmes, "shop", "web", 5).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["state"], "pending");
    cluster.write_policies(GUARDRAILS).await;
    cluster.write_policies(ONLY_ADMINS_GO_BIG).await;
    crate::bun::proposals::reap_once(
        &cluster.council,
        Some(&cluster.events),
        "node-1",
        crate::testkit::lease::now_unix_millis(),
    )
    .await
    .unwrap();
    let (_, body) = cluster.get("/v1/proposals/1", &cluster.bob).await;
    assert_eq!(body["state"], "pending", "{body}");
    assert_eq!(
        cluster.replicas("web", "shop").await,
        crate::config::Replicas::Fixed(3)
    );
}

/// The approval itself judges a policy approval as the author too, so a
/// deny that lands between the approve policy's match and the write
/// still refuses it.
#[tokio::test]
async fn a_policy_approval_is_judged_as_the_author_not_the_cluster() {
    let cluster = cluster("proposal-approve-as-author").await;
    let (status, body) = cluster.scale(&cluster.holmes, "shop", "web", 5).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    cluster.write_policies(ONLY_ADMINS_GO_BIG).await;
    let outcome = crate::bun::proposals::approve(
        &cluster.council,
        crate::proposal::ProposalId(1),
        crate::proposal::Approval::Policy {
            name: "agents-scale-shop".to_string(),
        },
    )
    .await
    .unwrap();
    let crate::proposal::ProposalOutcome::Failed { proposal } = outcome else {
        panic!("a caller-conditioned deny refuses the policy approval: {outcome:?}");
    };
    let crate::proposal::ProposalState::Failed { why, .. } = &proposal.state else {
        panic!("{:?}", proposal.state);
    };
    assert!(
        matches!(why, crate::proposal::FailedReason::Guardrail { rule, .. } if rule == "policy:only-admins-go-big"),
        "{why:?}"
    );
    assert_eq!(
        cluster.replicas("web", "shop").await,
        crate::config::Replicas::Fixed(3)
    );
}

/// The gym's finding (#1682): an agent's undo of a change a person has
/// changed again since is refused when it's proposed, with the code
/// `stale`, as a person's `relish undo` is. Approving it would have
/// overwritten the person's change.
#[tokio::test]
async fn an_agents_undo_is_stale_once_a_person_changed_the_app_again() {
    let cluster = cluster("proposal-undo-stale").await;
    let (status, body) = cluster.scale(&cluster.holmes, "shop", "web", 5).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let (status, body) = cluster
        .post("/v1/proposals/1/approve", &cluster.bob, String::new())
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    // A person ships a new version and scales back by hand.
    let mut person = web_spec(3);
    person.image = Some("web:1.5".to_string());
    cluster
        .council
        .write(crate::council::types::RaftRequest::AppSpec {
            app_id: crate::meat::AppId::new("web", "shop"),
            spec: person,
        })
        .await
        .unwrap();

    let undo = serde_json::json!({
        "kind": "undo", "namespace": "shop", "of": 1, "reason": "the scale didn't help",
    });
    let (status, body) = cluster.propose(&cluster.holmes, undo).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["code"], "stale");
    assert!(
        body["error"]
            .as_str()
            .unwrap()
            .contains("after proposal 1 applied"),
        "{body}"
    );
    let (status, body) = cluster
        .post("/v1/proposals/1/undo", &cluster.alice, String::new())
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["code"], "stale");
    let web =
        cluster.council.desired_state().await.apps[&crate::meat::AppId::new("web", "shop")].clone();
    assert_eq!(web.image.as_deref(), Some("web:1.5"));
}

/// Proposing an undo of a proposal outside the token's scope answers what
/// a missing one does, `not_found`, and nothing in the answer or the
/// refusal records names the original's apps.
#[tokio::test]
async fn an_undo_of_a_proposal_out_of_scope_is_not_found_and_names_nothing() {
    let cluster = cluster("proposal-undo-scope").await;
    cluster.scale(&cluster.holmes, "shop", "web", 5).await;
    let (status, body) = cluster
        .post("/v1/proposals/1/approve", &cluster.bob, String::new())
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    // A person changes web again, so building the undo would answer
    // `stale`, naming shop/web.
    cluster
        .council
        .write(crate::council::types::RaftRequest::AppSpec {
            app_id: crate::meat::AppId::new("web", "shop"),
            spec: web_spec(4),
        })
        .await
        .unwrap();

    let undo = |of: u64| {
        serde_json::json!({
            "kind": "undo", "namespace": "billing", "of": of, "reason": "probing",
        })
    };
    let (status, out_of_scope) = cluster.propose(&cluster.billing_bot, undo(1)).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{out_of_scope}");
    assert_eq!(out_of_scope["code"], "not_found");
    assert!(!out_of_scope.to_string().contains("web"), "{out_of_scope}");
    assert!(!out_of_scope.to_string().contains("shop"), "{out_of_scope}");
    let (status, missing) = cluster.propose(&cluster.billing_bot, undo(99)).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{missing}");
    assert_eq!(missing["code"], out_of_scope["code"]);

    for record in cluster.records("proposal.refused").await {
        let text = format!("{} {:?} {:?}", record.message, record.details, record.app);
        assert!(!text.contains("web"), "a refusal names the app: {text}");
    }
}

/// An agent's undo starts from exactly what the original left: its
/// `before` is the original's `after`.
#[tokio::test]
async fn an_agents_undo_starts_from_what_the_original_left() {
    let cluster = cluster("proposal-undo-before").await;
    cluster.scale(&cluster.holmes, "shop", "web", 5).await;
    let (_, original) = cluster
        .post("/v1/proposals/1/approve", &cluster.bob, String::new())
        .await;
    let undo = serde_json::json!({
        "kind": "undo", "namespace": "shop", "of": 1, "reason": "the scale didn't help",
    });
    let (status, body) = cluster.propose(&cluster.holmes, undo).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["kind"], "undo");
    assert_eq!(
        body["revisions"][0]["before"], original["revisions"][0]["after"],
        "{body}"
    );
    assert_eq!(
        body["revisions"][0]["after"], original["revisions"][0]["before"],
        "{body}"
    );
}

/// `relish status`'s agents line reads the summary: the lease a proposal
/// pins and the proposal waiting, within the caller's scope, and the
/// proposal turns stale once a person changes its app.
#[tokio::test]
async fn the_cluster_summary_counts_leases_and_waiting_proposals() {
    let cluster = cluster("proposal-summary").await;
    let (status, body) = cluster.scale(&cluster.holmes, "shop", "web", 5).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let (status, body) = cluster
        .scale(&cluster.billing_bot, "billing", "ledger", 5)
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");

    let (status, summary) = cluster.get("/v1/cluster/summary", &cluster.watcher).await;
    assert_eq!(status, StatusCode::OK, "{summary}");
    let agents = &summary["agents"];
    assert_eq!(agents["source"], "complete", "{agents}");
    assert_eq!(agents["value"]["leases"], 2, "{agents}");
    assert_eq!(agents["value"]["waiting"], 2, "{agents}");
    assert_eq!(agents["value"]["stale"], 0, "{agents}");
    let targets: Vec<&str> = agents["value"]["first_leases"]
        .as_array()
        .unwrap()
        .iter()
        .map(|lease| lease["target"].as_str().unwrap())
        .collect();
    assert_eq!(targets, ["app:billing/ledger", "app:shop/web"]);
    assert_eq!(agents["value"]["first_leases"][1]["holder"], "holmes");

    // Holmes's scope reaches shop only, as `GET /v1/proposals` does.
    let (_, summary) = cluster.get("/v1/cluster/summary", &cluster.holmes).await;
    let agents = &summary["agents"]["value"];
    assert_eq!(agents["leases"], 1, "{agents}");
    assert_eq!(agents["waiting"], 1, "{agents}");
    assert_eq!(agents["first_proposals"][0]["author"], "holmes");
    assert_eq!(agents["first_proposals"][0]["apps"][0], "web");

    // A person writes web in between: approving would fail, so it's stale.
    cluster
        .council
        .write(crate::council::types::RaftRequest::AppSpec {
            app_id: crate::meat::AppId::new("web", "shop"),
            spec: web_spec(4),
        })
        .await
        .unwrap();
    let (_, summary) = cluster.get("/v1/cluster/summary", &cluster.watcher).await;
    let agents = &summary["agents"]["value"];
    assert_eq!(agents["stale"], 1, "{agents}");
    assert_eq!(
        agents["first_proposals"][0]["stale"],
        "shop/web changed after it was made"
    );
}

/// #1682's design: every audit record says what kind of principal acted,
/// and the app records a proposal's apply writes name the proposal, its
/// author and the author's reason, so "which agent made which change, and
/// why" is answered from the change's own record.
#[tokio::test]
async fn a_proposals_app_records_name_the_agent_author_and_every_record_its_principal_kind() {
    let cluster = cluster("proposal-app-records").await;
    let (status, body) = cluster.scale(&cluster.holmes, "shop", "web", 5).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let (status, body) = cluster
        .post("/v1/proposals/1/approve", &cluster.bob, String::new())
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    // The person approved it, so the app record is theirs; the proposal,
    // its agent author and the agent's reason ride along.
    let applied = cluster.records("app.applied").await;
    assert_eq!(applied.len(), 1, "{applied:?}");
    let record = &applied[0];
    assert_eq!(record.app.as_deref(), Some("web"));
    assert_eq!(record.namespace.as_deref(), Some("shop"));
    assert_eq!(
        record.principal_kind,
        crate::sesame::types::PrincipalKind::Human
    );
    assert_eq!(record.details["token_name"], "bob");
    assert_eq!(record.details["proposal"], "1");
    assert_eq!(record.details["author"], "holmes");
    assert_eq!(record.details["author_kind"], "agent");
    assert_eq!(record.details["reason"], "p99 over the SLO");
    assert_eq!(record.details["approved_by"], "bob");

    // The proposal's own records say who acted too, as a field.
    let created = cluster.records("proposal.created").await;
    assert_eq!(
        created[0].principal_kind,
        crate::sesame::types::PrincipalKind::Agent
    );
    let approved = cluster.records("proposal.applied").await;
    assert_eq!(
        approved[0].principal_kind,
        crate::sesame::types::PrincipalKind::Human
    );

    // A stop writes app.stopped, also naming the proposal.
    let (status, body) = cluster
        .propose(
            &cluster.holmes,
            serde_json::json!({
                "kind": "stop", "namespace": "shop", "app": "api", "reason": "leaking memory",
            }),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let (status, body) = cluster
        .post("/v1/proposals/2/approve", &cluster.bob, String::new())
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let stopped = cluster.records("app.stopped").await;
    assert_eq!(stopped.len(), 1, "{stopped:?}");
    assert_eq!(stopped[0].app.as_deref(), Some("api"));
    assert_eq!(stopped[0].details["proposal"], "2");
    assert_eq!(stopped[0].details["reason"], "leaking memory");

    // `relish audit --principal-kind agent` and `--proposal 1`, through
    // the real route.
    let (status, body) = cluster
        .get("/v1/audit?principal_kind=agent", &cluster.alice)
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let records = body["records"].as_array().unwrap();
    assert!(!records.is_empty(), "{body}");
    assert!(
        records
            .iter()
            .all(|record| record["principal_kind"] == "agent"),
        "{body}"
    );
    assert!(
        records
            .iter()
            .any(|record| record["action"] == "proposal.created"),
        "{body}"
    );
    let (status, body) = cluster.get("/v1/audit?proposal=1", &cluster.alice).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let actions: Vec<&str> = body["records"]
        .as_array()
        .unwrap()
        .iter()
        .map(|record| record["action"].as_str().unwrap())
        .collect();
    assert!(actions.contains(&"proposal.created"), "{actions:?}");
    assert!(actions.contains(&"proposal.applied"), "{actions:?}");
    assert!(actions.contains(&"app.applied"), "{actions:?}");
    assert!(!actions.contains(&"app.stopped"), "{actions:?}");
    let (status, _) = cluster
        .get("/v1/audit?principal_kind=robot", &cluster.alice)
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

/// A restart (#1682) stamps the spec when a person approves it, so the
/// rollout replaces every instance; it says it can't be undone, refuses
/// `relish undo`, and its app record names the proposal like any other.
#[tokio::test]
async fn a_restart_stamps_the_spec_and_cant_be_undone() {
    let cluster = cluster("proposal-restart").await;
    let (status, body) = cluster
        .propose(
            &cluster.holmes,
            serde_json::json!({
                "kind": "restart", "namespace": "shop", "app": "web",
                "reason": "a leak; a fresh process buys a day",
            }),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["kind"], "restart");
    assert_eq!(body["reversibility"], "none");
    assert_eq!(body["undo"]["command"], serde_json::Value::Null);
    assert_eq!(body["blast_radius"]["instances_replaced"], 3);
    let (status, body) = cluster
        .post("/v1/proposals/1/approve", &cluster.bob, String::new())
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let web = crate::meat::AppId::new("web", "shop");
    let stamped = cluster.council.desired_state().await.apps[&web].restarted_at;
    assert!(stamped.is_some());
    let (status, body) = cluster
        .post("/v1/proposals/1/undo", &cluster.alice, String::new())
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["code"], "no_undo");
    let applied = cluster.records("app.applied").await;
    assert_eq!(applied.len(), 1, "{applied:?}");
    assert_eq!(applied[0].details["kind"], "restart");
    assert_eq!(applied[0].details["proposal"], "1");

    // Applying the same spec again by hand keeps the stamp: no second
    // restart.
    cluster
        .council
        .write(crate::council::types::RaftRequest::AppSpec {
            app_id: web.clone(),
            spec: web_spec(3),
        })
        .await
        .unwrap();
    assert_eq!(
        cluster.council.desired_state().await.apps[&web].restarted_at,
        stamped
    );
}

/// Built-in rule 3's one exception: an approve policy may approve a
/// restart, though it can't be undone.
#[tokio::test]
async fn an_approve_policy_may_approve_a_small_restart() {
    let cluster = cluster("proposal-restart-policy").await;
    cluster.write_policies(GUARDRAILS).await;
    let (status, body) = cluster
        .propose(
            &cluster.holmes,
            serde_json::json!({
                "kind": "restart", "namespace": "shop", "app": "web", "reason": "stuck",
            }),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["state"], "applied", "{body}");
    assert_eq!(
        body["approval"]["approved_by"],
        "policy:agents-restart-small-apps"
    );
}

/// Cancelling a rollout puts the last released spec back; undoing the
/// cancel puts the new spec back, and its rollout starts again.
#[tokio::test]
async fn cancelling_a_rollout_goes_back_to_the_released_spec_and_undo_resumes_it() {
    let cluster = cluster("proposal-cancel-deploy").await;
    let web = crate::meat::AppId::new("web", "shop");
    // 1.3 rolled out everywhere; then 1.4 was applied and is rolling out.
    let mut released = web_spec(3);
    released.image = Some("web:1.3".to_string());
    for request in [
        crate::council::types::RaftRequest::AppSpec {
            app_id: web.clone(),
            spec: released.clone(),
        },
        crate::council::types::RaftRequest::RolloutReleased {
            app_id: web.clone(),
            spec: released,
        },
        crate::council::types::RaftRequest::AppSpec {
            app_id: web.clone(),
            spec: web_spec(3),
        },
    ] {
        cluster.council.write(request).await.unwrap();
    }
    let (status, body) = cluster
        .propose(
            &cluster.holmes,
            serde_json::json!({
                "kind": "cancel_deploy", "namespace": "shop", "app": "web",
                "reason": "1.4 fails its health checks on the first node",
            }),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["reversibility"], "exact");
    assert!(
        body["summary"][0]
            .as_str()
            .unwrap()
            .starts_with("Cancel the rollout of shop/web to web:1.4"),
        "{body}"
    );
    let (status, body) = cluster
        .post("/v1/proposals/1/approve", &cluster.bob, String::new())
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let image = |desired: crate::council::types::DesiredState| desired.apps[&web].image.clone();
    assert_eq!(
        image(cluster.council.desired_state().await).as_deref(),
        Some("web:1.3")
    );
    let (status, body) = cluster
        .post("/v1/proposals/1/undo", &cluster.alice, String::new())
        .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        image(cluster.council.desired_state().await).as_deref(),
        Some("web:1.4")
    );

    // Once 1.4 finished rolling out there's nothing to cancel.
    cluster
        .council
        .write(crate::council::types::RaftRequest::RolloutReleased {
            app_id: web.clone(),
            spec: web_spec(3),
        })
        .await
        .unwrap();
    let (status, body) = cluster
        .propose(
            &cluster.holmes,
            serde_json::json!({
                "kind": "cancel_deploy", "namespace": "shop", "app": "web", "reason": "again",
            }),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(
        body["error"]
            .as_str()
            .unwrap()
            .contains("no rollout in progress")
    );
}

/// The blast radius is computed again when a person approves (#1682). If
/// the app reached a node the approver wasn't shown, the approval is
/// refused as stale, nothing is written, and `relish status` gives the
/// same reason beforehand.
#[tokio::test]
async fn an_approval_is_stale_when_the_blast_radius_grew() {
    let cluster = cluster("proposal-blast-grew").await;
    let (status, body) = cluster
        .propose(
            &cluster.holmes,
            serde_json::json!({
                "kind": "stop", "namespace": "shop", "app": "web", "reason": "leaking",
            }),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(body["blast_radius"]["nodes"], serde_json::json!([]));

    let web = crate::meat::AppId::new("web", "shop");
    let desired = cluster.council.desired_state().await;
    cluster
        .council
        .write(crate::council::types::RaftRequest::SchedulingDecisions {
            expected_log_id: desired.last_applied_log,
            decisions: vec![crate::meat::types::SchedulingDecision {
                app_id: web.clone(),
                placements: vec![crate::meat::types::Placement {
                    node_id: crate::meat::NodeId::new("node-z"),
                    resources: crate::meat::types::Resources::default(),
                    ordinal: 0,
                }],
            }],
        })
        .await
        .unwrap();
    let waiting = crate::bun::summary::waiting_proposals(&cluster.council.desired_state().await, 0);
    assert_eq!(
        waiting[0].stale.as_deref(),
        Some("its blast radius grew after it was made: it now touches node-z too")
    );

    let (status, body) = cluster
        .post("/v1/proposals/1/approve", &cluster.bob, String::new())
        .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["code"], "stale");
    assert_eq!(body["view"]["state"], "failed");
    assert!(body["error"].as_str().unwrap().contains("node-z"), "{body}");
    assert!(
        !cluster
            .council
            .desired_state()
            .await
            .stopped_apps
            .contains(&web)
    );
}

/// A proposed clear names a fault the nodes hold; with none, there's
/// nothing to propose.
#[tokio::test]
async fn a_clear_fault_proposal_needs_an_active_fault() {
    let cluster = cluster("proposal-clear-missing").await;
    let (status, body) = cluster
        .propose(
            &cluster.holmes,
            serde_json::json!({
                "kind": "clear_fault", "fault": 7, "reason": "the experiment is over",
            }),
        )
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    assert!(
        body["error"]
            .as_str()
            .unwrap()
            .contains("there's no active fault 7"),
        "{body}"
    );
}

/// A `clear_fault` proposal applies only through an approval that clears
/// the fault: the clear runs once, before the entry records it, and only
/// when the proposal would apply. Approved with nothing to clear it (the
/// reaper's path), it's recorded failed, never applied.
#[tokio::test]
async fn a_clear_fault_proposal_records_what_clearing_did() {
    let cluster = cluster("proposal-clear-action").await;
    let fault = crate::proposal::FaultTarget {
        id: 7,
        node: "node-b".to_string(),
        app: crate::meat::AppId::new("web", "shop"),
        fault_type: "delay 200ms".to_string(),
        instance: None,
        remaining_secs: 240,
        injected_by: "alice".to_string(),
    };
    let request = crate::proposal::ProposalRequest {
        kind: crate::proposal::RequestKind::ClearFault,
        namespace: None,
        app: None,
        replicas: None,
        config: None,
        of: None,
        fault: Some(7),
        node: None,
        reason: "the experiment is over".to_string(),
        ttl_seconds: None,
        idempotency_key: None,
    };
    let author = crate::proposal::Principal {
        name: "holmes".to_string(),
        principal_id: "token:holmes".to_string(),
        kind: TokenKind::Agent,
        role: ApiRole::Deployer,
    };
    let bob = crate::proposal::Principal {
        name: "bob".to_string(),
        principal_id: "token:bob".to_string(),
        kind: TokenKind::Human,
        role: ApiRole::Deployer,
    };
    let context = crate::proposal::build::BuildContext {
        history_target: None,
        fault: Some(fault),
    };
    let create = |now| {
        let proposal = crate::proposal::build::build_with_context(
            &crate::council::types::DesiredState::default(),
            &request,
            &author,
            now,
            crate::proposal::build::as_declared,
            &context,
        )
        .unwrap();
        crate::council::types::RaftRequest::ProposalCreate {
            proposal: Box::new(proposal),
            observed_at_unix_ms: now,
        }
    };
    let now = crate::testkit::lease::now_unix_millis();
    cluster.council.write(create(now)).await.unwrap();
    let cleared = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counter = cleared.clone();
    let outcome = crate::bun::proposals::approve_with(
        &cluster.council,
        crate::proposal::ProposalId(1),
        crate::proposal::Approval::Human(bob.clone()),
        Some(move || async move {
            counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            None
        }),
    )
    .await
    .unwrap();
    assert!(
        matches!(outcome, crate::proposal::ProposalOutcome::Applied { .. }),
        "{outcome:?}"
    );
    assert_eq!(cleared.load(std::sync::atomic::Ordering::SeqCst), 1);

    // The node refuses: recorded failed, with what it said.
    cluster.council.write(create(now + 1)).await.unwrap();
    let outcome = crate::bun::proposals::approve_with(
        &cluster.council,
        crate::proposal::ProposalId(2),
        crate::proposal::Approval::Human(bob.clone()),
        Some(|| async {
            Some(crate::proposal::FailedReason::Fault {
                reason: "fault 7 on node node-b wasn't cleared: no such fault".to_string(),
            })
        }),
    )
    .await
    .unwrap();
    let crate::proposal::ProposalOutcome::Failed { proposal } = outcome else {
        panic!("expected failed, got {outcome:?}");
    };
    assert!(
        proposal
            .view(now)
            .outcome
            .unwrap()
            .contains("no such fault"),
        "{proposal:?}"
    );

    // Approved with nothing to clear it: failed, never applied.
    cluster.council.write(create(now + 2)).await.unwrap();
    let outcome = crate::bun::proposals::approve(
        &cluster.council,
        crate::proposal::ProposalId(3),
        crate::proposal::Approval::Policy {
            name: "yes".to_string(),
        },
    )
    .await
    .unwrap();
    assert!(
        matches!(outcome, crate::proposal::ProposalOutcome::Failed { .. }),
        "{outcome:?}"
    );
}

/// A node's fault ids restart at 1 when its bun restarts, so by approval
/// the id a `clear_fault` proposal names may be another app's fault. That
/// one is refused, and stays active; the same fault still matches.
#[test]
fn a_clear_refuses_a_fault_that_took_the_same_id_on_another_app() {
    let recorded = crate::proposal::FaultTarget {
        id: 1,
        node: "node-b".to_string(),
        app: crate::meat::AppId::new("web", "shop"),
        fault_type: "delay 200ms".to_string(),
        instance: None,
        remaining_secs: 240,
        injected_by: "alice".to_string(),
    };
    let listed = |service: &str, kind: &str| -> crate::smoker::types::FaultSummary {
        serde_json::from_value(serde_json::json!({
            "id": 1, "fault_type": kind, "target_service": service, "namespace": "shop",
            "target_instance": null, "target_node": null, "remaining_secs": 100,
            "injected_by": "bob", "node": "node-b",
        }))
        .unwrap()
    };
    assert_eq!(
        super::proposals::fault_still_matches(&recorded, &[listed("web", "delay 200ms")]),
        Ok(())
    );
    let other = super::proposals::fault_still_matches(&recorded, &[listed("api", "delay 200ms")])
        .unwrap_err();
    assert!(other.contains("shop/api"), "{other}");
    assert!(other.contains("wasn't cleared"), "{other}");
    assert!(super::proposals::fault_still_matches(&recorded, &[listed("web", "kill")]).is_err());
    let gone = super::proposals::fault_still_matches(&recorded, &[]).unwrap_err();
    assert!(gone.contains("is gone"), "{gone}");
}

// ---------------------------------------------------------------------------
// Brioche's Proposals pages and decision forms (#1682).

/// A page or a form post through the real router, as a browser sends it.
struct Page {
    status: StatusCode,
    html: String,
    location: Option<String>,
    /// The `name=value` of a cookie the answer set.
    cookie: Option<String>,
}

impl Cluster {
    /// Log in at `/ui/session` with `token`, as the login form does, and
    /// return the session cookie.
    async fn login(&self, token: &str) -> String {
        let body: String = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("token", token)
            .finish();
        let page = self
            .browser(axum::http::Request::post("/ui/session"), None, body, &[])
            .await;
        assert_eq!(page.status, StatusCode::SEE_OTHER, "{}", page.html);
        page.cookie.expect("a session cookie")
    }

    async fn browser(
        &self,
        request: axum::http::request::Builder,
        cookie: Option<&str>,
        body: String,
        headers: &[(&str, &str)],
    ) -> Page {
        let mut request = request
            .header("content-type", "application/x-www-form-urlencoded")
            .header("accept", "text/html")
            .extension(axum::extract::ConnectInfo(
                "203.0.113.9:40000".parse::<std::net::SocketAddr>().unwrap(),
            ));
        if let Some(cookie) = cookie {
            request = request.header("cookie", cookie);
        }
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        let response = self
            .app
            .clone()
            .oneshot(request.body(Body::from(body)).unwrap())
            .await
            .unwrap();
        let status = response.status();
        let location = response
            .headers()
            .get("location")
            .map(|value| value.to_str().unwrap().to_string());
        let cookie = response.headers().get("set-cookie").map(|value| {
            let text = value.to_str().unwrap();
            text.split(';').next().unwrap().to_string()
        });
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        Page {
            status,
            html: String::from_utf8_lossy(&bytes).into_owned(),
            location,
            cookie,
        }
    }

    async fn page(&self, uri: &str, cookie: &str) -> Page {
        self.browser(
            axum::http::Request::get(uri),
            Some(cookie),
            String::new(),
            &[],
        )
        .await
    }

    async fn submit(&self, uri: &str, cookie: &str, fields: &[(&str, &str)]) -> Page {
        self.submit_with(uri, cookie, fields, &[]).await
    }

    async fn submit_with(
        &self,
        uri: &str,
        cookie: &str,
        fields: &[(&str, &str)],
        headers: &[(&str, &str)],
    ) -> Page {
        let mut body = url::form_urlencoded::Serializer::new(String::new());
        for (name, value) in fields {
            body.append_pair(name, value);
        }
        self.browser(
            axum::http::Request::post(uri),
            Some(cookie),
            body.finish(),
            headers,
        )
        .await
    }
}

/// The anti-forgery token a page's forms carry.
fn csrf_of(html: &str) -> String {
    let marker = "name=\"csrf\" value=\"";
    let start = html.find(marker).expect("a form with a csrf token") + marker.len();
    html[start..].split('"').next().unwrap().to_string()
}

/// The anti-forgery token for a session cookie, derived the way the server
/// derives it, for a viewer whose page has no form: then a refusal is the
/// role's, not the token's.
fn csrf_for_cookie(cookie: &str) -> String {
    let id = crate::sesame::session::session_id_from_cookie_header(cookie).unwrap();
    crate::sesame::session::csrf_token_for(id)
}

/// A person opens the list, reads proposal 1, approves it with the form,
/// and undoes it the same way. The decisions land as bob's, through the
/// same handler and audit record as `relish proposal approve`.
#[tokio::test]
async fn a_person_decides_and_undoes_a_proposal_on_the_dashboard() {
    let cluster = cluster("proposal-ui-path").await;
    cluster.scale(&cluster.holmes, "shop", "web", 5).await;
    let bob = cluster.login(&cluster.bob).await;

    let list = cluster.page("/ui/proposals", &bob).await;
    assert_eq!(list.status, StatusCode::OK);
    assert!(
        list.html.contains("href=\"/ui/proposal/1\""),
        "{}",
        list.html
    );
    assert!(list.html.contains("Waiting for approval"));
    assert!(list.html.contains("p99 over the SLO"));

    let detail = cluster.page("/ui/proposal/1", &bob).await;
    assert_eq!(detail.status, StatusCode::OK);
    assert!(detail.html.contains("Scale shop/web from 3 to 5 replicas."));
    assert!(detail.html.contains("action=\"/ui/proposal/1/approve\""));
    assert!(detail.html.contains("<code>relish undo 1</code>"));
    let csrf = csrf_of(&detail.html);

    let approved = cluster
        .submit(
            "/ui/proposal/1/approve",
            &bob,
            &[("csrf", &csrf), ("token", &cluster.bob)],
        )
        .await;
    assert_eq!(approved.status, StatusCode::SEE_OTHER, "{}", approved.html);
    assert_eq!(
        approved.location.as_deref(),
        Some("/ui/proposal/1?done=approve")
    );
    assert_eq!(
        cluster.replicas("web", "shop").await,
        crate::config::Replicas::Fixed(5)
    );
    let applied = cluster.records("proposal.applied").await;
    assert_eq!(applied.len(), 1);
    assert_eq!(applied[0].details["approved_by"], "bob");
    assert_eq!(
        applied[0].principal_kind,
        crate::sesame::types::PrincipalKind::Human
    );

    let after = cluster.page("/ui/proposal/1?done=approve", &bob).await;
    assert!(after.html.contains("Approved. The leader applied it"));
    assert!(after.html.contains("action=\"/ui/proposal/1/undo\""));
    assert!(!after.html.contains("/approve\""));

    let undone = cluster
        .submit(
            "/ui/proposal/1/undo",
            &bob,
            &[("csrf", &csrf), ("token", &cluster.bob)],
        )
        .await;
    assert_eq!(undone.status, StatusCode::SEE_OTHER, "{}", undone.html);
    assert_eq!(
        cluster.replicas("web", "shop").await,
        crate::config::Replicas::Fixed(3)
    );
    assert_eq!(cluster.records("proposal.undone").await.len(), 1);
}

#[tokio::test]
async fn a_person_rejects_with_a_reason_on_the_dashboard() {
    let cluster = cluster("proposal-ui-reject").await;
    cluster.scale(&cluster.holmes, "shop", "web", 5).await;
    let bob = cluster.login(&cluster.bob).await;
    let csrf = csrf_of(&cluster.page("/ui/proposal/1", &bob).await.html);
    let rejected = cluster
        .submit(
            "/ui/proposal/1/reject",
            &bob,
            &[
                ("csrf", &csrf),
                ("reason", "not during the sale"),
                ("token", &cluster.bob),
            ],
        )
        .await;
    assert_eq!(rejected.status, StatusCode::SEE_OTHER, "{}", rejected.html);
    let (_, body) = cluster.get("/v1/proposals/1", &cluster.bob).await;
    assert_eq!(body["state"], "rejected");
    assert_eq!(body["outcome"], "not during the sale");
    assert_eq!(
        cluster.replicas("web", "shop").await,
        crate::config::Replicas::Fixed(3)
    );
}

/// A form without the session's token, with another session's, or that
/// the browser marks cross-site changes nothing; nor does a bearer token
/// posting to the form route.
#[tokio::test]
async fn a_forged_or_cross_site_form_changes_nothing() {
    let cluster = cluster("proposal-ui-csrf").await;
    cluster.scale(&cluster.holmes, "shop", "web", 5).await;
    let bob = cluster.login(&cluster.bob).await;
    let alice = cluster.login(&cluster.alice).await;
    let bobs = csrf_of(&cluster.page("/ui/proposal/1", &bob).await.html);
    let alices = csrf_of(&cluster.page("/ui/proposal/1", &alice).await.html);
    assert_ne!(bobs, alices, "each session has its own token");

    let missing = cluster.submit("/ui/proposal/1/approve", &bob, &[]).await;
    assert_eq!(missing.status, StatusCode::FORBIDDEN);
    assert!(missing.html.contains("anti-forgery token"));
    let swapped = cluster
        .submit(
            "/ui/proposal/1/approve",
            &bob,
            &[("csrf", &alices), ("token", &cluster.bob)],
        )
        .await;
    assert_eq!(swapped.status, StatusCode::FORBIDDEN);
    let cross = cluster
        .submit_with(
            "/ui/proposal/1/approve",
            &bob,
            &[("csrf", &bobs), ("token", &cluster.bob)],
            &[("sec-fetch-site", "cross-site")],
        )
        .await;
    assert_eq!(cross.status, StatusCode::FORBIDDEN);
    let (status, _) = cluster
        .post(
            "/ui/proposal/1/approve",
            &cluster.bob,
            format!("csrf={bobs}"),
        )
        .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "a bearer uses /v1");

    let (_, body) = cluster.get("/v1/proposals/1", &cluster.bob).await;
    assert_eq!(body["state"], "pending");
    assert_eq!(
        cluster.replicas("web", "shop").await,
        crate::config::Replicas::Fixed(3)
    );
}

/// The page offers forms only to a viewer whose token may decide, and the
/// server refuses the others anyway: an agent, a read-only agent, a person
/// outside the proposal's namespace, and the author themselves.
#[tokio::test]
async fn viewers_who_may_not_decide_see_no_buttons_and_are_refused() {
    let cluster = cluster("proposal-ui-who").await;
    cluster.scale(&cluster.holmes, "shop", "web", 5).await;

    let holmes = cluster.login(&cluster.holmes).await;
    let page = cluster.page("/ui/proposal/1", &holmes).await;
    assert_eq!(page.status, StatusCode::OK);
    assert!(!page.html.contains("<form"), "{}", page.html);
    assert!(!page.html.contains("<button"), "{}", page.html);
    assert!(page.html.contains("<code>relish proposal approve 1</code>"));
    let refused = cluster
        .submit(
            "/ui/proposal/1/approve",
            &holmes,
            &[
                ("csrf", &csrf_for_cookie(&holmes)),
                ("token", &cluster.holmes),
            ],
        )
        .await;
    assert_eq!(refused.status, StatusCode::FORBIDDEN, "{}", refused.html);

    let watcher = cluster.login(&cluster.watcher).await;
    let page = cluster.page("/ui/proposal/1", &watcher).await;
    assert!(!page.html.contains("<form"), "{}", page.html);

    // Carol's token reaches billing only: shop's proposal isn't hers to see.
    let carol = cluster.login(&cluster.carol).await;
    let page = cluster.page("/ui/proposal/1", &carol).await;
    assert_eq!(page.status, StatusCode::NOT_FOUND);
    let list = cluster.page("/ui/proposals", &carol).await;
    assert!(!list.html.contains("/ui/proposal/1\""), "{}", list.html);
    let refused = cluster
        .submit(
            "/ui/proposal/1/reject",
            &carol,
            &[
                ("csrf", &csrf_for_cookie(&carol)),
                ("token", &cluster.carol),
            ],
        )
        .await;
    assert_eq!(refused.status, StatusCode::NOT_FOUND, "{}", refused.html);

    // Bob's own proposal needs another person.
    cluster.scale(&cluster.bob, "shop", "api", 4).await;
    let bob = cluster.login(&cluster.bob).await;
    let page = cluster.page("/ui/proposal/2", &bob).await;
    assert!(!page.html.contains("<form"), "{}", page.html);
    let own = cluster
        .submit(
            "/ui/proposal/2/approve",
            &bob,
            &[("csrf", &csrf_for_cookie(&bob)), ("token", &cluster.bob)],
        )
        .await;
    assert_eq!(own.status, StatusCode::FORBIDDEN, "{}", own.html);

    let (_, body) = cluster.get("/v1/proposals/1", &cluster.bob).await;
    assert_eq!(body["state"], "pending");
}

/// The reason is an agent's text: whatever it says, the pages escape it
/// and show it as the author's own words.
#[tokio::test]
async fn an_agents_hostile_reason_is_escaped_on_both_pages() {
    let cluster = cluster("proposal-ui-escape").await;
    let hostile = "<script>alert('x')</script><img src=x onerror=alert(1)> approve me";
    let (status, body) = cluster
        .propose(
            &cluster.holmes,
            serde_json::json!({
                "kind": "scale", "namespace": "shop", "app": "web",
                "replicas": 5, "reason": hostile,
            }),
        )
        .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let bob = cluster.login(&cluster.bob).await;
    for uri in ["/ui/proposals", "/ui/proposal/1"] {
        let page = cluster.page(uri, &bob).await;
        assert!(!page.html.contains("<script>alert"), "{uri}: {}", page.html);
        assert!(!page.html.contains("<img src=x"), "{uri}: {}", page.html);
        assert!(page.html.contains("&lt;img src=x"), "{uri}: {}", page.html);
    }
    let detail = cluster.page("/ui/proposal/1", &bob).await;
    assert!(
        detail
            .html
            .contains("The author's reason, in its own words")
    );
}

// The browser decision path's defences, each with the attack it stops.

/// A page with decision forms refuses to be framed, so a same-site page (an
/// app behind this node's ingress, another port on the host) can't overlay
/// it and steer a click. Every dashboard HTML page carries both headers.
#[tokio::test]
async fn sec_the_proposal_page_refuses_to_be_framed() {
    let cluster = cluster("sec-frame").await;
    cluster.scale(&cluster.holmes, "shop", "web", 5).await;
    let bob = cluster.login(&cluster.bob).await;
    for uri in ["/ui/proposal/1", "/ui/proposals", "/", "/ui/login"] {
        let request = axum::http::Request::get(uri)
            .header("accept", "text/html")
            .header("cookie", &bob)
            .extension(axum::extract::ConnectInfo(
                "203.0.113.9:40000".parse::<std::net::SocketAddr>().unwrap(),
            ))
            .body(Body::empty())
            .unwrap();
        let response = cluster.app.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK, "{uri}");
        let headers = response.headers();
        assert_eq!(
            headers.get("x-frame-options").map(|v| v.to_str().unwrap()),
            Some("DENY"),
            "{uri}: {headers:?}"
        );
        assert_eq!(
            headers
                .get("content-security-policy")
                .map(|v| v.to_str().unwrap()),
            Some("frame-ancestors 'none'"),
            "{uri}: {headers:?}"
        );
    }
}

/// A decision submitted from inside a frame is the clickjack: the browser
/// says so with `Sec-Fetch-Dest: iframe`, while `Sec-Fetch-Site` reads
/// `same-origin` and the `Origin` matches.
#[tokio::test]
async fn sec_a_decision_posted_from_inside_a_frame_changes_nothing() {
    let cluster = cluster("sec-frame-post").await;
    cluster.scale(&cluster.holmes, "shop", "web", 5).await;
    let bob = cluster.login(&cluster.bob).await;
    let csrf = csrf_of(&cluster.page("/ui/proposal/1", &bob).await.html);
    let framed = cluster
        .submit_with(
            "/ui/proposal/1/approve",
            &bob,
            &[("csrf", &csrf), ("token", &cluster.bob)],
            &[
                ("host", "node-a:9117"),
                ("origin", "https://node-a:9117"),
                ("sec-fetch-site", "same-origin"),
                ("sec-fetch-dest", "iframe"),
                ("sec-fetch-mode", "navigate"),
            ],
        )
        .await;
    assert_eq!(framed.status, StatusCode::FORBIDDEN, "{}", framed.html);
    assert_eq!(
        cluster.replicas("web", "shop").await,
        crate::config::Replicas::Fixed(3)
    );
    // The same post as a top-level document goes through.
    let top = cluster
        .submit_with(
            "/ui/proposal/1/approve",
            &bob,
            &[("csrf", &csrf), ("token", &cluster.bob)],
            &[
                ("host", "node-a:9117"),
                ("origin", "https://node-a:9117"),
                ("sec-fetch-site", "same-origin"),
                ("sec-fetch-dest", "document"),
            ],
        )
        .await;
    assert_eq!(top.status, StatusCode::SEE_OTHER, "{}", top.html);
}

/// The session cookie stays a read credential. Someone holding only the
/// cookie (and so the anti-forgery token derived from it) decides nothing;
/// another person's token doesn't help; the person's own token does.
#[tokio::test]
async fn sec_a_replayed_cookie_alone_cannot_approve() {
    let cluster = cluster("sec-replay").await;
    cluster.scale(&cluster.holmes, "shop", "web", 5).await;
    let stolen = cluster.login(&cluster.bob).await;
    let csrf = csrf_for_cookie(&stolen);
    let alone = cluster
        .submit("/ui/proposal/1/approve", &stolen, &[("csrf", &csrf)])
        .await;
    assert_eq!(alone.status, StatusCode::UNAUTHORIZED, "{}", alone.html);
    assert!(
        alone.html.contains("paste your API token"),
        "{}",
        alone.html
    );
    let wrong = cluster
        .submit(
            "/ui/proposal/1/approve",
            &stolen,
            &[("csrf", &csrf), ("token", "rbrg_not_a_real_token")],
        )
        .await;
    assert_eq!(wrong.status, StatusCode::UNAUTHORIZED, "{}", wrong.html);
    let someone_else = cluster
        .submit(
            "/ui/proposal/1/approve",
            &stolen,
            &[("csrf", &csrf), ("token", &cluster.alice)],
        )
        .await;
    assert_eq!(
        someone_else.status,
        StatusCode::FORBIDDEN,
        "{}",
        someone_else.html
    );
    assert_eq!(
        cluster.replicas("web", "shop").await,
        crate::config::Replicas::Fixed(3)
    );
    let own = cluster
        .submit(
            "/ui/proposal/1/approve",
            &stolen,
            &[("csrf", &csrf), ("token", &cluster.bob)],
        )
        .await;
    assert_eq!(own.status, StatusCode::SEE_OTHER, "{}", own.html);
    assert_eq!(
        cluster.replicas("web", "shop").await,
        crate::config::Replicas::Fixed(5)
    );
}

/// Every form asks for the token, and the page says why.
#[tokio::test]
async fn sec_every_decision_form_asks_for_the_token() {
    let cluster = cluster("sec-form-token").await;
    cluster.scale(&cluster.holmes, "shop", "web", 5).await;
    let bob = cluster.login(&cluster.bob).await;
    let html = cluster.page("/ui/proposal/1", &bob).await.html;
    assert_eq!(html.matches("<form").count(), 2, "{html}");
    assert_eq!(
        html.matches("type=\"password\" name=\"token\" required")
            .count(),
        2,
        "{html}"
    );
}

/// A refused browser decision is audited as the token the person pasted,
/// not as the session, and keeps the API's reason for the refusal.
#[tokio::test]
async fn sec_a_refused_browser_decision_is_audited_as_the_token() {
    let cluster = cluster("sec-refusal-audit").await;
    let (status, body) = cluster.scale(&cluster.bob, "shop", "api", 4).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let bob = cluster.login(&cluster.bob).await;
    let own = cluster
        .submit(
            "/ui/proposal/1/approve",
            &bob,
            &[("csrf", &csrf_for_cookie(&bob)), ("token", &cluster.bob)],
        )
        .await;
    assert_eq!(own.status, StatusCode::FORBIDDEN, "{}", own.html);
    let refused = cluster.records("api.refused").await;
    let record = refused
        .iter()
        .find(|record| record.details["route"] == "POST /ui/proposal/{id}/approve")
        .expect("the refusal is audited");
    assert!(
        record.principal.starts_with("token:"),
        "audited as {}",
        record.principal
    );
    assert_eq!(record.details["token_name"], "bob");
    assert_eq!(record.details["reason"], "forbidden");
}
