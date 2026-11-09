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
    assert_eq!(applied[0].details["principal_kind"], "policy");
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
        .submit("/ui/proposal/1/approve", &bob, &[("csrf", &csrf)])
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
    assert_eq!(applied[0].details["principal_kind"], "human");

    let after = cluster.page("/ui/proposal/1?done=approve", &bob).await;
    assert!(after.html.contains("Approved. The leader applied it"));
    assert!(after.html.contains("action=\"/ui/proposal/1/undo\""));
    assert!(!after.html.contains("/approve\""));

    let undone = cluster
        .submit("/ui/proposal/1/undo", &bob, &[("csrf", &csrf)])
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
            &[("csrf", &csrf), ("reason", "not during the sale")],
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
        .submit("/ui/proposal/1/approve", &bob, &[("csrf", &alices)])
        .await;
    assert_eq!(swapped.status, StatusCode::FORBIDDEN);
    let cross = cluster
        .submit_with(
            "/ui/proposal/1/approve",
            &bob,
            &[("csrf", &bobs)],
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
            &[("csrf", &csrf_for_cookie(&holmes))],
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
            &[("csrf", &csrf_for_cookie(&carol))],
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
            &[("csrf", &csrf_for_cookie(&bob))],
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
