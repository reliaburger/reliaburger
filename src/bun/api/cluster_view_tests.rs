//! Route-level tests for the cluster-wide views (F07, #365).
//!
//! Each view is served by one real router whose membership names a fake
//! peer: a small axum app on an ephemeral port that answers the way another
//! node would. The tests check the merge (every node's rows, tagged with
//! their node), partial failure (a member that doesn't answer is a warning,
//! not an error or a silent gap), that peers are asked with `local=true` so
//! they never fan out again, and that the caller's namespace scope trims the
//! merged rows.

use std::net::SocketAddr;
use std::time::{Duration, SystemTime};

use axum::body::Body;
use axum::extract::Query;
use http_body_util::BodyExt;
use tower::ServiceExt;

use super::*;
use crate::bun::agent::JobStatus;
use crate::bun::cluster_view::{ClusterDeployHistory, ClusterEvents, ClusterJobs};
use crate::bun::events::{EventKind, EventSeverity, EventStore};
use crate::meat::deploy_types::{DeployId, DeployResult};
use crate::meat::types::AppId;

type Params = Query<std::collections::HashMap<String, String>>;

/// Serve `peer` on an ephemeral port and return its address.
async fn serve_peer(peer: Router) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, peer).await.unwrap();
    });
    address
}

/// An address nothing listens on, so a request to it fails fast.
async fn dead_address() -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    listener.local_addr().unwrap()
}

fn members(nodes: &[(&str, SocketAddr)]) -> Arc<RwLock<Vec<NodeMembershipInfo>>> {
    Arc::new(RwLock::new(
        nodes
            .iter()
            .map(|(name, address)| NodeMembershipInfo {
                node_id: crate::meat::NodeId::new(*name),
                address: *address,
                api_advertised: true,
            })
            .collect(),
    ))
}

/// Refuse a fan-out request that would make the peer fan out in turn.
fn require_local(params: &std::collections::HashMap<String, String>) -> Result<(), StatusCode> {
    match params.get("local").map(String::as_str) {
        Some("true") => Ok(()),
        _ => Err(StatusCode::BAD_REQUEST),
    }
}

fn history_entry(app: &str, namespace: &str, id: u64, created_secs: u64) -> DeployHistoryEntry {
    let at = SystemTime::UNIX_EPOCH + Duration::from_secs(created_secs);
    DeployHistoryEntry {
        id: DeployId(id),
        app_id: AppId::new(app, namespace),
        image: format!("{app}:{id}"),
        result: DeployResult::Completed,
        created_at: at,
        completed_at: at,
        steps_completed: 1,
        steps_total: 1,
        spec: None,
    }
}

fn job(name: &str, namespace: &str) -> JobStatus {
    JobStatus {
        name: name.into(),
        namespace: namespace.into(),
        instance_id: format!("{name}-0"),
        image: "proc-grill:image-ignored".into(),
        state: "stopped".into(),
        restart_count: 0,
        age_seconds: 5,
    }
}

/// A stand-in agent loop: answers status, jobs and follow requests.
fn spawn_agent(
    statuses: Vec<InstanceStatus>,
    jobs: Vec<JobStatus>,
    follow_lines: Vec<String>,
) -> mpsc::Sender<AgentCommand> {
    let (cmd_tx, mut cmd_rx) = mpsc::channel(8);
    tokio::spawn(async move {
        while let Some(command) = cmd_rx.recv().await {
            match command {
                AgentCommand::Status { response } => {
                    let _ = response.send(statuses.clone());
                }
                AgentCommand::JobStatus { response } => {
                    let _ = response.send(jobs.clone());
                }
                AgentCommand::FollowLogs { lines, .. } => {
                    let follow_lines = follow_lines.clone();
                    tokio::spawn(async move {
                        for line in follow_lines {
                            if lines.send(line).await.is_err() {
                                return;
                            }
                        }
                        // Hold the stream open like a running replica.
                        lines.closed().await;
                    });
                }
                _ => {}
            }
        }
    });
    cmd_tx
}

struct Setup {
    cmd_tx: mpsc::Sender<AgentCommand>,
    history: Vec<DeployHistoryEntry>,
    events: Option<Arc<RwLock<EventStore>>>,
    members: Option<Arc<RwLock<Vec<NodeMembershipInfo>>>>,
    tokens: Vec<crate::sesame::types::ApiToken>,
    council: Option<Arc<crate::council::CouncilNode>>,
    service_token: Option<String>,
}

impl Setup {
    fn new(cmd_tx: mpsc::Sender<AgentCommand>) -> Self {
        Self {
            cmd_tx,
            history: Vec::new(),
            events: None,
            members: None,
            tokens: Vec::new(),
            council: None,
            service_token: None,
        }
    }

    async fn router(self) -> Router {
        let token_store = if self.tokens.is_empty() {
            None
        } else {
            let store = crate::sesame::auth::new_token_store();
            *store.write().await = self.tokens;
            Some(store)
        };
        router(
            self.cmd_tx,
            None,
            None,
            Some(Arc::new(RwLock::new(self.history))),
            None,
            None,
            self.council,
            token_store,
            self.service_token,
            None,
            self.members,
            None,
            9117,
            self.events,
        )
    }
}

async fn get(app: Router, uri: &str, bearer: Option<&str>) -> (StatusCode, Vec<u8>) {
    let mut request = axum::http::Request::builder().uri(uri);
    if let Some(bearer) = bearer {
        request = request.header("authorization", format!("Bearer {bearer}"));
    }
    let response = app
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    (status, body.to_vec())
}

fn scoped_token(namespace: &str) -> crate::sesame::token::CreatedToken {
    crate::sesame::token::create_token(
        "tenant-reader",
        crate::sesame::types::ApiRole::ReadOnly,
        crate::sesame::types::TokenScope {
            apps: None,
            namespaces: Some(vec![namespace.to_string()]),
        },
        None,
    )
    .unwrap()
}

// ---------------------------------------------------------------------------
// Deploy history
// ---------------------------------------------------------------------------

fn history_peer(entries: Vec<DeployHistoryEntry>) -> Router {
    Router::new().route(
        "/v1/deploys/history/{app}",
        axum::routing::get(move |Query(params): Params| {
            let entries = entries.clone();
            async move {
                require_local(&params)?;
                let namespace = params.get("namespace").cloned().unwrap_or_default();
                Ok::<_, StatusCode>(Json(ClusterDeployHistory {
                    app: "web".into(),
                    namespace,
                    history: crate::bun::cluster_view::tag_rows(vec![(
                        "ignored-by-the-merge".to_string(),
                        entries,
                    )]),
                    warnings: Vec::new(),
                }))
            }
        }),
    )
}

#[tokio::test]
async fn deploy_history_merges_every_members_record_with_its_node() {
    let peer = serve_peer(history_peer(vec![history_entry("web", "default", 2, 200)])).await;
    let mut setup = Setup::new(spawn_agent(Vec::new(), Vec::new(), Vec::new()));
    setup.history = vec![
        history_entry("web", "default", 1, 100),
        history_entry("web", "other", 9, 150),
    ];
    setup.members = Some(members(&[("peer", peer)]));
    let app = setup.router().await;

    let (status, body) = get(app, "/v1/deploys/history/web?namespace=default", None).await;
    assert_eq!(status, StatusCode::OK);
    let view: ClusterDeployHistory = serde_json::from_slice(&body).unwrap();
    let rows: Vec<(&str, u64)> = view
        .history
        .iter()
        .map(|entry| (entry.node.as_str(), entry.row.id.0))
        .collect();
    assert_eq!(rows, [("local", 1), ("peer", 2)]);
    assert!(view.warnings.is_empty(), "{:?}", view.warnings);
}

#[tokio::test]
async fn deploy_history_names_a_member_that_did_not_answer() {
    let dead = dead_address().await;
    let mut setup = Setup::new(spawn_agent(Vec::new(), Vec::new(), Vec::new()));
    setup.history = vec![history_entry("web", "default", 1, 100)];
    setup.members = Some(members(&[("gone", dead)]));
    let app = setup.router().await;

    let (status, body) = get(app, "/v1/deploys/history/web?namespace=default", None).await;
    assert_eq!(status, StatusCode::OK);
    let view: ClusterDeployHistory = serde_json::from_slice(&body).unwrap();
    assert_eq!(view.history.len(), 1, "this node's own record survives");
    assert_eq!(view.warnings.len(), 1, "{:?}", view.warnings);
    assert!(view.warnings[0].contains("gone"), "{:?}", view.warnings);
}

#[tokio::test]
async fn a_peer_asked_for_its_own_history_does_not_fan_out_again() {
    let dead = dead_address().await;
    let mut setup = Setup::new(spawn_agent(Vec::new(), Vec::new(), Vec::new()));
    setup.history = vec![history_entry("web", "default", 1, 100)];
    setup.members = Some(members(&[("gone", dead)]));
    let app = setup.router().await;

    let (status, body) = get(
        app,
        "/v1/deploys/history/web?namespace=default&local=true",
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let view: ClusterDeployHistory = serde_json::from_slice(&body).unwrap();
    assert_eq!(view.history.len(), 1);
    assert!(view.warnings.is_empty(), "{:?}", view.warnings);
}

// ---------------------------------------------------------------------------
// Events
// ---------------------------------------------------------------------------

fn record(store: &mut EventStore, timestamp: u64, message: &str) {
    store.record(
        timestamp,
        EventKind::Deploy,
        EventSeverity::Info,
        Some("web".into()),
        Some("default".into()),
        None,
        message.into(),
    );
}

#[tokio::test]
async fn events_merge_every_members_newest_with_their_node() {
    let mut peer_store = EventStore::new();
    record(&mut peer_store, 20, "peer deployed");
    record(&mut peer_store, 40, "peer restarted");
    let peer_events = peer_store.recent(100, None, None);
    let peer = serve_peer(Router::new().route(
        "/v1/events",
        axum::routing::get(move |Query(params): Params| {
            let events = peer_events.clone();
            async move {
                require_local(&params)?;
                Ok::<_, StatusCode>(Json(ClusterEvents {
                    events,
                    warnings: Vec::new(),
                }))
            }
        }),
    ))
    .await;
    let mut local_store = EventStore::new();
    record(&mut local_store, 10, "local deployed");
    record(&mut local_store, 30, "local stopped");

    let mut setup = Setup::new(spawn_agent(Vec::new(), Vec::new(), Vec::new()));
    setup.events = Some(Arc::new(RwLock::new(local_store)));
    setup.members = Some(members(&[("peer", peer)]));
    let app = setup.router().await;

    let (status, body) = get(app, "/v1/events?limit=3", None).await;
    assert_eq!(status, StatusCode::OK);
    let view: ClusterEvents = serde_json::from_slice(&body).unwrap();
    let rows: Vec<(&str, &str)> = view
        .events
        .iter()
        .map(|event| (event.node.as_deref().unwrap(), event.message.as_str()))
        .collect();
    assert_eq!(
        rows,
        [
            ("peer", "peer deployed"),
            ("local", "local stopped"),
            ("peer", "peer restarted"),
        ]
    );
    assert!(view.warnings.is_empty(), "{:?}", view.warnings);
}

#[tokio::test]
async fn events_name_a_member_that_did_not_answer() {
    let dead = dead_address().await;
    let mut local_store = EventStore::new();
    record(&mut local_store, 10, "local deployed");
    let mut setup = Setup::new(spawn_agent(Vec::new(), Vec::new(), Vec::new()));
    setup.events = Some(Arc::new(RwLock::new(local_store)));
    setup.members = Some(members(&[("gone", dead)]));
    let app = setup.router().await;

    let (status, body) = get(app, "/v1/events", None).await;
    assert_eq!(status, StatusCode::OK);
    let view: ClusterEvents = serde_json::from_slice(&body).unwrap();
    assert_eq!(view.events.len(), 1);
    assert!(
        view.warnings.iter().any(|warning| warning.contains("gone")),
        "{:?}",
        view.warnings
    );
}

// ---------------------------------------------------------------------------
// Jobs
// ---------------------------------------------------------------------------

fn jobs_peer(jobs: Vec<JobStatus>) -> Router {
    Router::new().route(
        "/v1/jobs",
        axum::routing::get(move |Query(params): Params| {
            let jobs = jobs.clone();
            async move {
                // A peer answers the single-node shape for itself.
                if params.contains_key("cluster") {
                    return Err(StatusCode::BAD_REQUEST);
                }
                Ok(Json(jobs))
            }
        }),
    )
}

#[tokio::test]
async fn cluster_jobs_list_every_members_jobs_with_their_node() {
    let peer = serve_peer(jobs_peer(vec![job("seed", "default")])).await;
    let dead = dead_address().await;
    let mut setup = Setup::new(spawn_agent(
        Vec::new(),
        vec![job("migrate", "default")],
        Vec::new(),
    ));
    setup.members = Some(members(&[("peer", peer), ("gone", dead)]));
    let app = setup.router().await;

    let (status, body) = get(app, "/v1/jobs?cluster=true", None).await;
    assert_eq!(status, StatusCode::OK);
    let view: ClusterJobs = serde_json::from_slice(&body).unwrap();
    let rows: Vec<(&str, &str)> = view
        .jobs
        .iter()
        .map(|job| (job.node.as_str(), job.row.name.as_str()))
        .collect();
    assert_eq!(rows, [("local", "migrate"), ("peer", "seed")]);
    assert_eq!(view.warnings.len(), 1, "{:?}", view.warnings);
    assert!(view.warnings[0].contains("gone"));
}

/// Peers answer with the service token, which sees every namespace, so the
/// entry node trims the merged jobs to the caller's scope, locally and
/// cluster-wide.
#[tokio::test]
async fn namespace_scoped_token_sees_only_its_namespaces_jobs() {
    let peer = serve_peer(jobs_peer(vec![
        job("seed", "team-a"),
        job("seed", "team-b"),
    ]))
    .await;
    let created = scoped_token("team-a");
    let mut setup = Setup::new(spawn_agent(
        Vec::new(),
        vec![job("migrate", "team-a"), job("migrate", "team-b")],
        Vec::new(),
    ));
    setup.members = Some(members(&[("peer", peer)]));
    setup.tokens = vec![created.token];
    let app = setup.router().await;

    let (status, body) = get(app.clone(), "/v1/jobs", Some(&created.plaintext)).await;
    assert_eq!(status, StatusCode::OK);
    let local: Vec<JobStatus> = serde_json::from_slice(&body).unwrap();
    let namespaces: Vec<&str> = local.iter().map(|job| job.namespace.as_str()).collect();
    assert_eq!(namespaces, ["team-a"]);

    let (status, body) = get(app, "/v1/jobs?cluster=true", Some(&created.plaintext)).await;
    assert_eq!(status, StatusCode::OK);
    let view: ClusterJobs = serde_json::from_slice(&body).unwrap();
    assert!(
        view.jobs.iter().all(|job| job.row.namespace == "team-a"),
        "{:?}",
        view.jobs
    );
    assert_eq!(view.jobs.len(), 2);
}

// ---------------------------------------------------------------------------
// Dashboard node page
// ---------------------------------------------------------------------------

fn instance(id: &str) -> InstanceStatus {
    serde_json::from_value(serde_json::json!({
        "id": id, "app_name": "web", "namespace": "default", "state": "running",
        "restart_count": 0, "host_port": null, "pid": null
    }))
    .unwrap()
}

#[tokio::test]
async fn node_page_lists_another_nodes_workloads_without_this_nodes_charts() {
    let peer_statuses = vec![instance("peer-web-0")];
    let peer = serve_peer(Router::new().route(
        "/v1/status",
        axum::routing::get(move || {
            let statuses = peer_statuses.clone();
            async move { Json(statuses) }
        }),
    ))
    .await;
    let mut setup = Setup::new(spawn_agent(
        vec![instance("local-web-0")],
        Vec::new(),
        Vec::new(),
    ));
    setup.members = Some(members(&[("peer", peer)]));
    let app = setup.router().await;

    let (status, body) = get(app, "/ui/node/peer", None).await;
    assert_eq!(status, StatusCode::OK);
    let html = String::from_utf8(body).unwrap();
    assert!(
        html.contains("peer-web-0"),
        "the peer's workload is missing"
    );
    assert!(
        !html.contains("local-web-0"),
        "this node's workload is shown as the peer's"
    );
    assert!(
        !html.contains("node_cpu_usage_percent"),
        "this node's CPU chart is drawn under the peer's name"
    );
}

#[tokio::test]
async fn node_page_says_when_that_node_did_not_answer() {
    let dead = dead_address().await;
    let mut setup = Setup::new(spawn_agent(Vec::new(), Vec::new(), Vec::new()));
    setup.members = Some(members(&[("gone", dead)]));
    let app = setup.router().await;

    let (status, body) = get(app, "/ui/node/gone", None).await;
    assert_eq!(status, StatusCode::OK);
    let html = String::from_utf8(body).unwrap();
    assert!(
        html.contains("did not answer"),
        "a silent empty list hides the failure"
    );
}

// ---------------------------------------------------------------------------
// WebSocket log stream
// ---------------------------------------------------------------------------

/// Each WebSocket frame is a [`crate::ketchup::follow::LogFrame`], so the
/// TUI can tell a log line from a "node left" notice.
#[tokio::test]
async fn websocket_log_frames_are_tagged_lines() {
    use futures_util::StreamExt as _;
    use tokio_tungstenite::tungstenite::Message as WsMessage;

    let setup = Setup::new(spawn_agent(
        Vec::new(),
        Vec::new(),
        vec!["hello".into(), "world".into()],
    ));
    let app = setup.router().await;
    let address = serve_peer(app).await;
    let (mut socket, _) =
        tokio_tungstenite::connect_async(format!("ws://{address}/v1/ws/logs/web/default"))
            .await
            .unwrap();
    let mut frames = Vec::new();
    while frames.len() < 2 {
        let frame = tokio::time::timeout(Duration::from_secs(5), socket.next())
            .await
            .expect("no frame within 5s")
            .unwrap()
            .unwrap();
        if let WsMessage::Text(text) = frame {
            frames.push(serde_json::from_str::<crate::ketchup::follow::LogFrame>(&text).unwrap());
        }
    }
    assert_eq!(
        frames,
        [
            crate::ketchup::follow::LogFrame::Line("hello".into()),
            crate::ketchup::follow::LogFrame::Line("world".into()),
        ]
    );
}

// ---------------------------------------------------------------------------
// Live event stream
// ---------------------------------------------------------------------------

/// A peer that answers `/v1/events` the way another node does: its backlog
/// as JSON, or, asked to follow with `local=true`, `live` over SSE and then
/// nothing more.
async fn events_peer(
    backlog: Vec<crate::bun::events::ClusterEvent>,
    live: crate::bun::events::ClusterEvent,
) -> SocketAddr {
    use futures_util::StreamExt as _;
    serve_peer(Router::new().route(
        "/v1/events",
        axum::routing::get(move |Query(params): Params| {
            let backlog = backlog.clone();
            let live = live.clone();
            async move {
                require_local(&params)?;
                if params.get("follow").map(String::as_str) != Some("true") {
                    return Ok::<_, StatusCode>(
                        Json(ClusterEvents {
                            events: backlog,
                            warnings: Vec::new(),
                        })
                        .into_response(),
                    );
                }
                let json = serde_json::to_string(&live).unwrap();
                let stream = futures_util::stream::once(async move {
                    Ok::<_, std::convert::Infallible>(Event::default().data(json))
                })
                .chain(futures_util::stream::pending());
                Ok(Sse::new(stream).into_response())
            }
        }),
    ))
    .await
}

/// F07 part 2: the live event stream covers the cluster. It used to send
/// only the connected node's events, so the TUI refreshed the merged
/// history every 2 s instead. Now it opens with every member's recent
/// events, then relays each member's new ones as they happen, each tagged
/// with its node.
#[tokio::test]
async fn the_live_event_stream_merges_every_members_events() {
    use futures_util::StreamExt as _;
    use tokio_tungstenite::tungstenite::Message as WsMessage;

    let mut peer_store = EventStore::new();
    record(&mut peer_store, 20, "peer earlier");
    let mut peer_live = EventStore::new();
    record(&mut peer_live, 50, "peer live");
    let peer = events_peer(
        peer_store.recent(100, None, None),
        peer_live.recent(1, None, None).remove(0),
    )
    .await;

    let mut local_store = EventStore::new();
    record(&mut local_store, 10, "local earlier");
    let local_store = Arc::new(RwLock::new(local_store));
    let mut setup = Setup::new(spawn_agent(Vec::new(), Vec::new(), Vec::new()));
    setup.events = Some(Arc::clone(&local_store));
    setup.members = Some(members(&[("peer", peer)]));
    let address = serve_peer(setup.router().await).await;

    let (mut socket, _) = tokio_tungstenite::connect_async(format!("ws://{address}/v1/ws/events"))
        .await
        .unwrap();
    let mut seen: Vec<(String, String)> = Vec::new();
    let mut recorded_live = false;
    while seen.len() < 4 {
        let frame = tokio::time::timeout(Duration::from_secs(10), socket.next())
            .await
            .unwrap_or_else(|_| panic!("stalled after {seen:?}"))
            .unwrap()
            .unwrap();
        if let WsMessage::Text(text) = frame {
            let event: crate::bun::events::ClusterEvent = serde_json::from_str(&text).unwrap();
            seen.push((event.node.unwrap_or_default(), event.message));
        }
        // Once the backlog is in, something new happens on this node.
        if seen.len() == 2 && !recorded_live {
            recorded_live = true;
            record(&mut *local_store.write().await, 60, "local live");
        }
    }
    assert_eq!(
        seen[..2],
        [
            ("local".to_string(), "local earlier".to_string()),
            ("peer".to_string(), "peer earlier".to_string()),
        ],
        "the backlog is every member's, oldest first"
    );
    let mut live = seen[2..].to_vec();
    live.sort();
    assert_eq!(
        live,
        [
            ("local".to_string(), "local live".to_string()),
            ("peer".to_string(), "peer live".to_string()),
        ]
    );
}

/// The per-node live feed a merged stream reads: `follow=true&local=true`
/// streams this node's new events over SSE, tagged with its name.
#[tokio::test]
async fn a_node_streams_its_own_new_events_on_request() {
    use futures_util::StreamExt as _;

    let local_store = Arc::new(RwLock::new(EventStore::new()));
    let mut setup = Setup::new(spawn_agent(Vec::new(), Vec::new(), Vec::new()));
    setup.events = Some(Arc::clone(&local_store));
    let address = serve_peer(setup.router().await).await;

    let response = reqwest::get(format!("http://{address}/v1/events?follow=true&local=true"))
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    record(&mut *local_store.write().await, 70, "fresh");
    let mut body = response.bytes_stream();
    let mut decoder = crate::ketchup::sse::SseDecoder::default();
    let event = loop {
        let chunk = tokio::time::timeout(Duration::from_secs(5), body.next())
            .await
            .expect("no event within 5s")
            .unwrap()
            .unwrap();
        if let Some(event) = decoder.push(&chunk).into_iter().next() {
            break event;
        }
    };
    let event: crate::bun::events::ClusterEvent = serde_json::from_str(&event.data).unwrap();
    assert_eq!(event.message, "fresh");
    assert_eq!(event.node.as_deref(), Some("local"));
}

// ---------------------------------------------------------------------------
// API tokens (F05 I2)
// ---------------------------------------------------------------------------

/// A council whose token store holds `tokens`, and the same tokens for the
/// router's auth layer.
async fn council_with_tokens(
    tag: &str,
    tokens: &[crate::sesame::types::ApiToken],
) -> Arc<crate::council::CouncilNode> {
    let council = super::tests::seeded_council(tag).await;
    for token in tokens {
        council
            .write(crate::council::RaftRequest::CreateApiToken(token.clone()))
            .await
            .unwrap();
    }
    council
}

fn token_named<'a>(
    view: &'a crate::bun::cluster_view::ClusterTokens,
    name: &str,
) -> &'a crate::bun::cluster_view::TokenSummary {
    view.tokens
        .iter()
        .find(|token| token.name == name)
        .unwrap_or_else(|| panic!("{name} missing from {:?}", view.tokens))
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

#[tokio::test]
async fn token_list_shows_scope_expiry_and_last_use_after_a_request() {
    use crate::sesame::types::{ApiRole, TokenScope};
    let admin =
        crate::sesame::token::create_token("admin", ApiRole::Admin, TokenScope::default(), None)
            .unwrap();
    let expiry = SystemTime::now() + Duration::from_secs(10 * 86_400);
    let reader = crate::sesame::token::create_token(
        "reader",
        ApiRole::ReadOnly,
        TokenScope {
            apps: Some(vec!["web".into()]),
            namespaces: Some(vec!["team".into()]),
        },
        Some(expiry),
    )
    .unwrap();
    let tokens = vec![admin.token.clone(), reader.token.clone()];
    let mut setup = Setup::new(spawn_agent(Vec::new(), Vec::new(), Vec::new()));
    setup.council = Some(council_with_tokens("token-last-used", &tokens).await);
    setup.tokens = tokens;
    let app = setup.router().await;

    let before = unix_now();
    let (status, body) = get(app.clone(), "/v1/token/list", Some(&admin.plaintext)).await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    let view: crate::bun::cluster_view::ClusterTokens = serde_json::from_slice(&body).unwrap();
    let listed = token_named(&view, "reader");
    assert_eq!(listed.role, "read-only");
    assert_eq!(listed.scope.apps, Some(vec!["web".to_string()]));
    assert_eq!(listed.scope.namespaces, Some(vec!["team".to_string()]));
    assert_eq!(
        listed.expires_at,
        Some(
            expiry
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_secs()
        )
    );
    assert_eq!(listed.last_used, None, "the reader hasn't been used yet");
    assert!(
        token_named(&view, "admin").last_used >= Some(before),
        "the listing request itself is a use of the admin token"
    );

    // The reader authenticates once; the next listing shows when.
    let (status, _) = get(app.clone(), "/v1/status", Some(&reader.plaintext)).await;
    assert_eq!(status, StatusCode::OK);
    let (_, body) = get(app, "/v1/token/list", Some(&admin.plaintext)).await;
    let view: crate::bun::cluster_view::ClusterTokens = serde_json::from_slice(&body).unwrap();
    let last_used = token_named(&view, "reader").last_used;
    assert!(
        last_used >= Some(before) && last_used <= Some(unix_now()),
        "{last_used:?}"
    );
}

#[tokio::test]
async fn token_list_merges_every_members_last_use_and_names_a_silent_one() {
    use crate::bun::cluster_view::{ClusterTokens, TokenSummary};
    use crate::sesame::types::{ApiRole, TokenScope};
    let admin =
        crate::sesame::token::create_token("admin", ApiRole::Admin, TokenScope::default(), None)
            .unwrap();
    let ci =
        crate::sesame::token::create_token("ci", ApiRole::Deployer, TokenScope::default(), None)
            .unwrap();
    let ci_principal = crate::sesame::auth::token_principal_id(&ci.token);
    // The peer saw `ci` in the future (as far as this node knows), and a
    // token this node has never heard of, which the merge must ignore.
    let peer_view = ClusterTokens {
        tokens: vec![
            TokenSummary {
                name: "ci".into(),
                principal: ci_principal,
                role: "deployer".into(),
                scope: TokenScope::default(),
                created_at: 0,
                expires_at: None,
                last_used: Some(4_000_000_000),
            },
            TokenSummary {
                name: "stranger".into(),
                principal: "token:stranger".into(),
                role: "admin".into(),
                scope: TokenScope::default(),
                created_at: 0,
                expires_at: None,
                last_used: Some(4_000_000_001),
            },
        ],
        warnings: Vec::new(),
    };
    let peer = serve_peer(Router::new().route(
        "/v1/token/list",
        axum::routing::get(move |Query(params): Params| {
            let view = peer_view.clone();
            async move {
                require_local(&params)?;
                Ok::<_, StatusCode>(Json(view))
            }
        }),
    ))
    .await;
    let dead = dead_address().await;

    let tokens = vec![admin.token.clone(), ci.token.clone()];
    let mut setup = Setup::new(spawn_agent(Vec::new(), Vec::new(), Vec::new()));
    setup.council = Some(council_with_tokens("token-merge", &tokens).await);
    setup.tokens = tokens;
    setup.members = Some(members(&[("peer", peer), ("gone", dead)]));
    let app = setup.router().await;

    let (status, body) = get(app, "/v1/token/list", Some(&admin.plaintext)).await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    let view: ClusterTokens = serde_json::from_slice(&body).unwrap();
    let names: Vec<&str> = view.tokens.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(
        names,
        ["admin", "ci"],
        "only the tokens in this node's store"
    );
    assert_eq!(token_named(&view, "ci").last_used, Some(4_000_000_000));
    assert_eq!(view.warnings.len(), 1, "{:?}", view.warnings);
    assert!(view.warnings[0].contains("gone"), "{:?}", view.warnings);
}

/// A peer answers its share for the node fan-out, which presents the
/// service token. That principal may read the local answer, but it still
/// can't ask for the cluster-wide list: it stays off user management.
#[tokio::test]
async fn the_service_principal_reads_only_a_nodes_own_token_list() {
    use crate::sesame::types::{ApiRole, TokenScope};
    let admin =
        crate::sesame::token::create_token("admin", ApiRole::Admin, TokenScope::default(), None)
            .unwrap();
    let service = crate::sesame::token::derive_service_token(&[3u8; 32]).unwrap();
    let tokens = vec![admin.token.clone()];
    let mut setup = Setup::new(spawn_agent(Vec::new(), Vec::new(), Vec::new()));
    setup.council = Some(council_with_tokens("token-service", &tokens).await);
    setup.tokens = tokens;
    setup.service_token = Some(service.clone());
    let app = setup.router().await;

    let (status, body) = get(app.clone(), "/v1/token/list?local=true", Some(&service)).await;
    assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
    let (status, _) = get(app, "/v1/token/list", Some(&service)).await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}
