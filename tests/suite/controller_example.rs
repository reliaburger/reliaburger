//! The example controller (`examples/controller/main.rs`) against a real
//! council and the real API over HTTP (#1125).
//!
//! The example is compiled into this test with `#[path]`, so the test runs
//! the very code a reader runs. It talks to the cluster the way any
//! controller would: a bearer token scoped to its namespace, JSON in and
//! out, and nothing from the `reliaburger` crate.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use reliaburger::council::log_store::MemLogStore;
use reliaburger::council::network::{InMemoryRaftNetworkFactory, InMemoryRaftRouter};
use reliaburger::council::node::CouncilNode;
use reliaburger::council::state_machine::CouncilStateMachine;
use reliaburger::council::types::{CouncilConfig, CouncilNodeInfo};
use reliaburger::meat::types::AppId;
use reliaburger::sesame::types::{ApiRole, TokenScope};
use tokio_util::sync::CancellationToken;

#[allow(dead_code)]
#[path = "../../examples/controller/main.rs"]
mod controller;

use controller::{Api, AppView, Controller, Decision, Settings, decide};

const PAIR: &str = include_str!("../../examples/controller/pair.toml");

fn settings(endpoint: &str, token: &str) -> Settings {
    Settings {
        endpoint: endpoint.to_string(),
        token: token.to_string(),
        ca_cert: None,
        name: "db-failover".to_string(),
        namespace: "pg".to_string(),
        manifest: PAIR.to_string(),
        threshold: 2,
    }
}

fn view(owner: Option<&str>, running: usize) -> AppView {
    AppView {
        exists: true,
        owner: owner.map(str::to_string),
        running,
    }
}

const OURS: Option<&str> = Some("db-failover");

#[test]
fn a_fresh_pair_starts_the_first_app() {
    let none = AppView::default();
    assert!(matches!(
        decide([&none, &none], 0, &settings("", "")),
        Decision::Write { active: 0, .. }
    ));
}

#[test]
fn a_running_active_app_is_left_alone() {
    assert!(matches!(
        decide([&view(OURS, 1), &AppView::default()], 0, &settings("", "")),
        Decision::Wait { .. }
    ));
}

#[test]
fn the_controller_fails_over_only_after_the_threshold() {
    let (a, b) = (AppView::default(), view(OURS, 0));
    assert!(matches!(
        decide([&a, &b], 1, &settings("", "")),
        Decision::Wait { .. }
    ));
    assert!(matches!(
        decide([&a, &b], 2, &settings("", "")),
        Decision::Write { active: 0, .. }
    ));
}

#[test]
fn an_unowned_active_app_is_claimed_where_it_is() {
    assert!(matches!(
        decide([&AppView::default(), &view(None, 1)], 0, &settings("", "")),
        Decision::Write { active: 1, .. }
    ));
}

#[test]
fn with_both_apps_left_the_running_one_stays() {
    assert!(matches!(
        decide([&view(OURS, 0), &view(OURS, 1)], 0, &settings("", "")),
        Decision::Write { active: 1, .. }
    ));
}

#[test]
fn an_app_someone_else_owns_blocks_the_controller() {
    assert!(matches!(
        decide(
            [&view(Some("someone-else"), 1), &AppView::default()],
            0,
            &settings("", "")
        ),
        Decision::Blocked { .. }
    ));
}

#[test]
fn the_manifest_is_the_active_app_in_the_namespace_owned_by_the_token() {
    let written = controller::manifest(&settings("", ""), 1).unwrap();
    let config = reliaburger::config::Config::parse(&written).unwrap();
    config.validate_intrinsic().unwrap();
    assert_eq!(config.app.keys().collect::<Vec<_>>(), vec!["db-b"]);
    let b = &config.app["db-b"];
    assert_eq!(b.owner.as_deref(), Some("db-failover"));
    assert_eq!(b.namespace.as_deref(), Some("pg"));
    assert_eq!(b.image.as_deref(), Some("postgres:17"));
}

const TEMPLATED_PAIR: &str = include_str!("../../examples/controller/postgres-pair.toml");

fn templated_settings(endpoint: &str, token: &str) -> Settings {
    Settings {
        manifest: TEMPLATED_PAIR.to_string(),
        ..settings(endpoint, token)
    }
}

#[test]
fn a_templated_pair_names_its_apps_by_each_uses_name() {
    assert_eq!(
        controller::app_names(&templated_settings("", "")).unwrap(),
        ["db-a".to_string(), "db-b".to_string()]
    );
}

#[test]
fn the_templated_manifest_is_the_active_use_with_the_controllers_owner_and_namespace() {
    let mut settings = templated_settings("", "");
    settings.name = "pg-ha".to_string();
    settings.namespace = "orders".to_string();
    let written = controller::manifest(&settings, 1).unwrap();
    let mut config = reliaburger::config::Config::parse(&written).unwrap();
    assert_eq!(config.uses.keys().collect::<Vec<_>>(), vec!["db-b"]);
    config.expand_templates().unwrap();
    config.validate_intrinsic().unwrap();
    assert_eq!(config.app.keys().collect::<Vec<_>>(), vec!["db-b"]);
    let b = &config.app["db-b"];
    assert_eq!(b.owner.as_deref(), Some("pg-ha"));
    assert_eq!(b.namespace.as_deref(), Some("orders"));
    let provenance = b.provenance.as_ref().unwrap();
    assert_eq!(provenance.template, "postgres-member");
    assert_eq!(provenance.instance, "db-b");
}

#[test]
fn a_templated_pair_needs_two_uses_that_name_their_apps() {
    let one = "[template.t]\nparams = { name = 'string' }\nbody = '''\n[app.${name}]\nscript = 'true'\n'''\n[use.a]\ntemplate = 't'\nname = 'a'\n";
    let mut settings = templated_settings("", "");
    settings.manifest = one.to_string();
    assert!(controller::app_names(&settings).is_err());
    assert!(controller::manifest(&settings, 0).is_err());
    settings.manifest = TEMPLATED_PAIR.replace("name = \"db-b\"\n", "");
    let error = controller::app_names(&settings).unwrap_err();
    assert!(error.contains("use.db-b"), "{error}");
}

async fn leader() -> Arc<CouncilNode> {
    let router = InMemoryRaftRouter::new();
    let node = CouncilNode::new(
        1,
        CouncilConfig::default(),
        InMemoryRaftNetworkFactory::new(1, router.clone()),
        MemLogStore::new(),
        CouncilStateMachine::new(),
        None,
    )
    .await
    .unwrap();
    router.register(1, node.raft().clone()).await;
    node.initialize(BTreeMap::from([(1, CouncilNodeInfo::default())]))
        .await
        .unwrap();
    let node = Arc::new(node);
    tokio::time::timeout(Duration::from_secs(5), async {
        while !node.is_leader().await {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    node
}

/// A council behind the real router, served over TCP, with an admin token
/// and the controller's token scoped to `pg`.
struct Served {
    council: Arc<CouncilNode>,
    endpoint: String,
    admin: String,
    controller: String,
    shutdown: CancellationToken,
}

async fn serve() -> Served {
    let council = leader().await;
    let admin = reliaburger::sesame::token::create_token(
        "operator",
        ApiRole::Admin,
        TokenScope::default(),
        None,
    )
    .unwrap();
    let scoped = reliaburger::sesame::token::create_token(
        "db-failover",
        ApiRole::Deployer,
        TokenScope {
            apps: None,
            namespaces: Some(vec!["pg".to_string()]),
        },
        None,
    )
    .unwrap();
    let store = reliaburger::sesame::auth::new_token_store();
    store.write().await.push(admin.token);
    store.write().await.push(scoped.token);

    // The agent this API fronts runs nothing, so every instance list is
    // empty: the active app never has a running instance.
    let (cmd_tx, mut cmd_rx) = tokio::sync::mpsc::channel(8);
    tokio::spawn(async move {
        while let Some(command) = cmd_rx.recv().await {
            if let reliaburger::bun::agent::AgentCommand::Status { response } = command {
                let _ = response.send(Vec::new());
            }
        }
    });
    let app = reliaburger::bun::api::router(
        cmd_tx,
        None,
        None,
        None,
        None,
        None,
        Some(Arc::clone(&council)),
        Some(store),
        None,
        None,
        None,
        None,
        0,
        None,
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let shutdown = CancellationToken::new();
    let serving = shutdown.clone();
    tokio::spawn(async move {
        axum::serve(
            listener,
            app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .with_graceful_shutdown(async move { serving.cancelled().await })
        .await
        .unwrap();
    });
    Served {
        council,
        endpoint,
        admin: admin.plaintext,
        controller: scoped.plaintext,
        shutdown,
    }
}

async fn post(served: &Served, token: &str, manifest: &str) -> String {
    let response = reqwest::Client::new()
        .post(format!("{}/v1/apply", served.endpoint))
        .bearer_auth(token)
        .body(manifest.to_string())
        .send()
        .await
        .unwrap();
    let status = response.status();
    let body = response.text().await.unwrap();
    assert!(status.is_success(), "{status}: {body}");
    body
}

async fn owner_of(council: &CouncilNode, name: &str) -> Option<Option<String>> {
    council
        .desired_state()
        .await
        .apps
        .get(&AppId::new(name, "pg"))
        .map(|spec| spec.owner.clone())
}

/// End to end: the controller starts and claims the first app, a human's
/// apply of it is skipped, and when it never runs the controller fails
/// over to the standby, all through the public API.
#[tokio::test]
async fn the_example_controller_claims_its_app_and_fails_over() {
    let served = serve().await;
    post(&served, &served.admin, "[namespace.pg]\n").await;

    let api = Api::new(settings(&served.endpoint, &served.controller)).unwrap();
    let mut controller = Controller::default();
    let first = controller.reconcile(&api).await.unwrap();
    assert!(
        matches!(first, Decision::Write { active: 0, .. }),
        "{first:?}"
    );
    let owned = Some(Some("db-failover".to_string()));
    assert_eq!(owner_of(&served.council, "db-a").await, owned);
    assert_eq!(owner_of(&served.council, "db-b").await, None);

    // A human applies db-a without the owner: skipped, not fought over.
    let body = post(
        &served,
        &served.admin,
        "[app.db-a]\nimage = 'postgres:18'\nnamespace = 'pg'\nreplicas = 3\n",
    )
    .await;
    assert!(
        body.contains("is owned by 'db-failover'; skipped"),
        "{body}"
    );
    let db_a = served.council.desired_state().await.apps[&AppId::new("db-a", "pg")].clone();
    assert_eq!(db_a.image.as_deref(), Some("postgres:17"));

    // db-a never runs here. One miss is under the threshold of two...
    let second = controller.reconcile(&api).await.unwrap();
    assert!(matches!(second, Decision::Wait { .. }), "{second:?}");
    // ...and the second fails over to db-b, deleting db-a.
    let third = controller.reconcile(&api).await.unwrap();
    assert!(
        matches!(third, Decision::Write { active: 1, .. }),
        "{third:?}"
    );
    assert_eq!(owner_of(&served.council, "db-a").await, None);
    assert_eq!(owner_of(&served.council, "db-b").await, owned);

    served.shutdown.cancel();
    served.council.shutdown().await.ok();
}

/// The same failover, driving a template (#1124): each write is one `use`,
/// which the cluster expands into an ordinary app owned by the controller
/// and marked with the template it came from.
#[tokio::test]
async fn the_example_controller_fails_a_templated_pair_over() {
    let served = serve().await;
    post(&served, &served.admin, "[namespace.pg]\n").await;
    let api = Api::new(templated_settings(&served.endpoint, &served.controller)).unwrap();
    let mut controller = Controller::default();

    let first = controller.reconcile(&api).await.unwrap();
    assert!(
        matches!(first, Decision::Write { active: 0, .. }),
        "{first:?}"
    );
    let state = served.council.desired_state().await;
    let db_a = &state.apps[&AppId::new("db-a", "pg")];
    assert_eq!(db_a.owner.as_deref(), Some("db-failover"));
    assert_eq!(
        db_a.provenance.as_ref().map(|p| p.template.as_str()),
        Some("postgres-member")
    );
    assert!(!state.apps.contains_key(&AppId::new("db-b", "pg")));

    // Nothing runs here, so after two missed checks it fails over.
    controller.reconcile(&api).await.unwrap();
    let third = controller.reconcile(&api).await.unwrap();
    assert!(
        matches!(third, Decision::Write { active: 1, .. }),
        "{third:?}"
    );
    let state = served.council.desired_state().await;
    assert!(!state.apps.contains_key(&AppId::new("db-a", "pg")));
    let db_b = &state.apps[&AppId::new("db-b", "pg")];
    assert_eq!(db_b.owner.as_deref(), Some("db-failover"));
    assert_eq!(
        db_b.placement.as_ref().unwrap().required,
        vec!["rack=b".to_string()],
        "the use's own parameters"
    );

    served.shutdown.cancel();
    served.council.shutdown().await.ok();
}

/// The controller's token can't reach outside its namespace, whatever its
/// settings say.
#[tokio::test]
async fn the_example_controller_cant_write_outside_its_namespace() {
    let served = serve().await;
    post(&served, &served.admin, "[namespace.pg]\n[namespace.shop]\n").await;
    let mut wrong = settings(&served.endpoint, &served.controller);
    wrong.namespace = "shop".to_string();
    let api = Api::new(wrong).unwrap();
    let error = Controller::default().reconcile(&api).await.unwrap_err();
    assert!(error.contains("403"), "{error}");
    assert!(served.council.desired_state().await.apps.is_empty());
    served.shutdown.cancel();
    served.council.shutdown().await.ok();
}
