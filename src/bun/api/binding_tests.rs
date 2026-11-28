//! Route-level tests for binding images to digests at apply (F03 U1, #361).
//!
//! The binder is attached as a layer, as `bun` attaches it when its own
//! runtime pulls images. The registry is a fixed answer, so every test runs
//! without a network.

use std::time::{Duration, SystemTime};

use super::tests::seeded_council;
use super::*;
use crate::bun::agent::BunAgent;
use crate::grill::mock::MockGrill;
use crate::grill::port::PortAllocator;
use crate::meat::deploy_types::{DeployId, DeployResult};
use crate::meat::types::AppId;
use crate::pickle::binding::{FixedUpstream, ImageBinder};
use crate::pickle::types::Digest;
use axum::body::Body;
use http_body_util::BodyExt;
use tokio_util::sync::CancellationToken;
use tower::ServiceExt;

fn digest(i: u64) -> Digest {
    Digest::new(&format!("sha256:{i:064x}")).unwrap()
}

/// A binder whose registry answers every tag with `digest(i)`, or is down.
fn binder(answer: Option<u64>) -> ImageBinder {
    ImageBinder::with_upstream(Arc::new(FixedUpstream(answer.map(digest))))
}

/// A router over a live standalone agent (a `MockGrill`), with `council`
/// and `history` when given and the binder layered on when given.
fn router_with(
    council: Option<Arc<crate::council::CouncilNode>>,
    history: Option<Arc<RwLock<crate::bun::agent::DeployHistory>>>,
    binder: Option<ImageBinder>,
) -> (Router, CancellationToken) {
    router_with_service(council, history, binder, None)
}

fn router_with_service(
    council: Option<Arc<crate::council::CouncilNode>>,
    history: Option<Arc<RwLock<crate::bun::agent::DeployHistory>>>,
    binder: Option<ImageBinder>,
    service: Option<Arc<crate::bun::task_array_leader::TaskArrayService>>,
) -> (Router, CancellationToken) {
    let (cmd_tx, cmd_rx) = mpsc::channel(32);
    let shutdown = CancellationToken::new();
    let mut agent = BunAgent::new(
        MockGrill::new(),
        PortAllocator::new(30000, 31000),
        cmd_rx,
        shutdown.clone(),
    );
    tokio::spawn(async move { agent.run().await });
    let app = router_with_upgrade(
        cmd_tx,
        None,
        None,
        history,
        None,
        None,
        council,
        None,
        None,
        None,
        None,
        None,
        None,
        9117,
        None,
        None,
        None,
        "default".into(),
        None,
        crate::bun::build_runner::BuildSettings::with_timeout(900),
        crate::cluster::ClusterHttp::plaintext(),
        5050,
        "http",
        256 * 1024 * 1024,
        false,
        crate::bun::capabilities::StaticCapabilities::default(),
        crate::bun::readiness::ReadinessTracker::new(),
        None,
        None,
        None,
        service,
    );
    let app = match binder {
        Some(binder) => app.layer(axum::Extension(binder)),
        None => app,
    };
    (app, shutdown)
}

async fn post(app: Router, uri: &str, body: &str) -> (StatusCode, String) {
    let response = app
        .oneshot(
            axum::http::Request::post(uri)
                .body(Body::from(body.to_string()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8_lossy(&body).into_owned())
}

async fn desired_image(council: &crate::council::CouncilNode, app: &str) -> Option<String> {
    council
        .desired_state()
        .await
        .apps
        .get(&AppId::new(app, "default"))
        .and_then(|spec| spec.image.clone())
}

#[tokio::test]
async fn a_cluster_apply_stores_the_bound_reference_and_prints_the_binding() {
    let council = seeded_council("bind-cluster-apply").await;
    let (app, shutdown) = router_with(Some(council.clone()), None, Some(binder(Some(7))));

    let (status, body) = post(app, "/v1/apply", "[app.web]\nimage = \"nginx:1.27\"\n").await;

    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.contains("web: nginx:1.27 → sha256:"), "{body}");
    assert_eq!(
        desired_image(&council, "web").await,
        Some(format!("nginx:1.27@{}", digest(7).as_str()))
    );
    shutdown.cancel();
    council.raft().shutdown().await.unwrap();
}

/// Decision 2: with the registry down and nothing cached, the apply fails
/// and Raft keeps no unbound tag.
#[tokio::test]
async fn a_cluster_apply_fails_and_writes_nothing_when_the_registry_is_down() {
    let council = seeded_council("bind-registry-down").await;
    let (app, shutdown) = router_with(Some(council.clone()), None, Some(binder(None)));

    let (_, body) = post(app, "/v1/apply", "[app.web]\nimage = \"nginx:1.27\"\n").await;

    assert!(body.contains("\"Error\""), "{body}");
    assert!(body.contains("nginx:1.27"), "{body}");
    assert_eq!(desired_image(&council, "web").await, None);
    shutdown.cancel();
    council.raft().shutdown().await.unwrap();
}

/// #2528: an image with a layer no node can unpack is refused at apply,
/// naming the layer's media type, and nothing reaches Raft. Before, the
/// apply said yes and every node's pull failed.
#[tokio::test]
async fn a_cluster_apply_refuses_an_image_whose_layers_no_node_can_unpack() {
    let council = seeded_council("bind-unpackable-layer").await;
    let lz4 = "application/vnd.oci.image.layer.v1.tar+lz4";
    let binder = ImageBinder::with_upstream(Arc::new(crate::pickle::binding::LayeredUpstream(lz4)));
    let (app, shutdown) = router_with(Some(council.clone()), None, Some(binder));

    let (_, body) = post(app, "/v1/apply", "[app.web]\nimage = \"nginx:1.27\"\n").await;

    assert!(body.contains("\"Error\""), "{body}");
    assert!(body.contains(lz4), "{body}");
    assert_eq!(desired_image(&council, "web").await, None);
    shutdown.cancel();
    council.raft().shutdown().await.unwrap();
}

/// Decision 5: a node whose runtime pulls nothing (ProcessGrill) has no
/// binder, and stores the image as written.
#[tokio::test]
async fn without_a_binder_the_image_is_stored_as_written() {
    let council = seeded_council("bind-no-binder").await;
    let (app, shutdown) = router_with(Some(council.clone()), None, None);

    let (status, body) = post(app, "/v1/apply", "[app.web]\nimage = \"nginx:1.27\"\n").await;

    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        desired_image(&council, "web").await,
        Some("nginx:1.27".to_string())
    );
    shutdown.cancel();
    council.raft().shutdown().await.unwrap();
}

/// A standalone node binds before its agent deploys, so the instance runs
/// (and reports) the bound reference.
#[tokio::test]
async fn a_standalone_apply_deploys_the_bound_reference() {
    let (app, shutdown) = router_with(None, None, Some(binder(Some(9))));

    let (status, body) = post(
        app.clone(),
        "/v1/apply",
        "[app.web]\nimage = \"nginx:1.27\"\n",
    )
    .await;

    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(body.contains("web: nginx:1.27 → sha256:"), "{body}");
    let response = app
        .oneshot(
            axum::http::Request::get("/v1/apps")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let apps = response.into_body().collect().await.unwrap().to_bytes();
    let apps = String::from_utf8_lossy(&apps);
    assert!(
        apps.contains(&format!("nginx:1.27@{}", digest(9).as_str())),
        "{apps}"
    );
    shutdown.cancel();
}

#[tokio::test]
async fn a_standalone_apply_fails_before_deploying_when_the_registry_is_down() {
    let (app, shutdown) = router_with(None, None, Some(binder(None)));

    let (status, body) = post(app, "/v1/apply", "[app.web]\nimage = \"nginx:1.27\"\n").await;

    assert_eq!(status, StatusCode::BAD_GATEWAY, "{body}");
    assert!(body.contains("nginx:1.27"), "{body}");
    shutdown.cancel();
}

/// #2145: an image nobody built is the caller's mistake, not a registry
/// outage: a 404 that names Pickle and `relish build`.
#[tokio::test]
async fn a_standalone_apply_of_an_image_nobody_built_is_a_404_naming_pickle() {
    let binder = ImageBinder::with_upstream(Arc::new(crate::pickle::binding::MissingUpstream));
    let (app, shutdown) = router_with(None, None, Some(binder));

    let (status, body) = post(app, "/v1/apply", "[app.web]\nimage = \"burger:v1\"\n").await;

    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
    assert!(
        body.contains("burger:v1 isn't in this cluster's registry (Pickle)"),
        "{body}"
    );
    shutdown.cancel();
}

/// #2157: on a cluster the binding fails inside the apply's event stream,
/// after its `200`, and relish still reports the 404 a standalone node
/// answers, with Pickle's message.
#[tokio::test]
async fn a_cluster_apply_of_an_image_nobody_built_reaches_relish_as_a_404_naming_pickle() {
    let council = seeded_council("bind-cluster-not-built").await;
    let binder = ImageBinder::with_upstream(Arc::new(crate::pickle::binding::MissingUpstream));
    let (app, shutdown) = router_with(Some(council.clone()), None, Some(binder));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let client =
        crate::relish::client::BunClient::new_with_token(&format!("http://{address}"), None);
    let config = Config::parse("[app.web]\nimage = \"burger:v1\"\n").unwrap();

    let error = client.apply(&config).await.unwrap_err();

    match error {
        crate::relish::RelishError::ApiError { status, body } => {
            assert_eq!(status, 404, "{body}");
            assert!(
                body.contains("burger:v1 isn't in this cluster's registry (Pickle)"),
                "{body}"
            );
        }
        other => panic!("expected an API error, got {other:?}"),
    }
    assert_eq!(desired_image(&council, "web").await, None);
    server.abort();
    shutdown.cancel();
    council.raft().shutdown().await.unwrap();
}

/// `[images.trust_policy.upstream_default] allow = false` with one rule
/// for Docker Hub's official images.
fn allow_list() -> crate::config::node::TrustPolicySection {
    crate::config::node::TrustPolicySection {
        upstream: vec![crate::config::node::UpstreamTrustRule {
            pattern: "docker.io/library/*".to_string(),
            require_signatures: false,
            cosign_keys: vec![],
        }],
        upstream_default: crate::config::node::UpstreamDefault { allow: false },
        ..Default::default()
    }
}

/// F03 U2: the leader refuses an image its upstream rules don't allow,
/// names it, and commits nothing from the apply.
#[tokio::test]
async fn a_cluster_apply_refuses_an_image_the_upstream_rules_do_not_allow() {
    let council = seeded_council("bind-upstream-refused").await;
    let binder = binder(Some(7)).with_policy(allow_list());
    let (app, shutdown) = router_with(Some(council.clone()), None, Some(binder));

    let (_, body) = post(
        app,
        "/v1/apply",
        "[app.web]\nimage = \"nginx:1.27\"\n[app.miner]\nimage = \"ghcr.io/evil/miner:1\"\n",
    )
    .await;

    assert!(body.contains("\"Error\""), "{body}");
    assert!(body.contains("ghcr.io/evil/miner:1"), "{body}");
    assert!(body.contains("not allowed"), "{body}");
    assert_eq!(desired_image(&council, "web").await, None);
    assert_eq!(desired_image(&council, "miner").await, None);
    shutdown.cancel();
    council.raft().shutdown().await.unwrap();
}

#[tokio::test]
async fn a_standalone_apply_refuses_an_image_the_upstream_rules_do_not_allow() {
    let binder = binder(Some(7)).with_policy(allow_list());
    let (app, shutdown) = router_with(None, None, Some(binder));

    let (status, body) = post(
        app,
        "/v1/apply",
        "[app.miner]\nimage = \"ghcr.io/evil/miner:1\"\n",
    )
    .await;

    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert!(body.contains("ghcr.io/evil/miner:1"), "{body}");
    shutdown.cancel();
}

fn completed(id: u64, created_secs: u64, image: &str) -> DeployHistoryEntry {
    let at = SystemTime::UNIX_EPOCH + Duration::from_secs(created_secs);
    let spec: crate::config::app::AppSpec =
        toml::from_str(&format!("image = \"{image}\"")).unwrap();
    DeployHistoryEntry {
        id: DeployId(id),
        app_id: AppId::new("web", "default"),
        image: image.to_string(),
        result: DeployResult::Completed,
        created_at: at,
        completed_at: at,
        steps_completed: 1,
        steps_total: 1,
        spec: Some(Box::new(spec)),
    }
}

/// A rollback restores the reference that ran before, digest and all. The
/// registry is down, so a re-resolution of the tag would fail the rollback.
#[tokio::test]
async fn a_rollback_restores_the_bound_digest_without_asking_the_registry() {
    let council = seeded_council("bind-rollback").await;
    let old = format!("nginx:1.27@{}", digest(1).as_str());
    let new = format!("nginx:1.27@{}", digest(2).as_str());
    let spec: crate::config::app::AppSpec = toml::from_str(&format!("image = \"{new}\"")).unwrap();
    council
        .write(crate::council::RaftRequest::AppSpec {
            app_id: AppId::new("web", "default"),
            spec: Box::new(spec),
        })
        .await
        .unwrap();
    let history = Arc::new(RwLock::new(
        vec![completed(1, 10, &old), completed(2, 20, &new)]
            .into_iter()
            .collect::<crate::bun::agent::DeployHistory>(),
    ));
    let (app, shutdown) = router_with(Some(council.clone()), Some(history), Some(binder(None)));

    let (status, body) = post(app, "/v1/rollback/web/default", "").await;

    assert_eq!(status, StatusCode::OK, "{body}");
    assert!(!body.contains("\"Error\""), "{body}");
    assert_eq!(desired_image(&council, "web").await, Some(old));
    shutdown.cancel();
    council.raft().shutdown().await.unwrap();
}

/// Delegated entry points must honour the same binding policy as /v1/apply.
#[tokio::test]
async fn delegated_submissions_bind_before_registration_and_refuse_unresolved_images() {
    for manifest in [false, true] {
        for answer in [Some(7), None] {
            let council = seeded_council("bind-delegated").await;
            let (app, shutdown) = router_with(Some(council.clone()), None, Some(binder(answer)));
            let template =
                Config::parse("[job.render]\nimage = \"nginx:1.27\"\ncommand = [\"/bin/true\"]\n")
                    .unwrap()
                    .job
                    .remove("render")
                    .unwrap();
            let profile = serde_json::json!({"name":"small", "count":1, "template":template});
            let (uri, request) = if manifest {
                (
                    "/v1/batch/manifest",
                    serde_json::json!({"name":"render", "cohort":[profile]}),
                )
            } else {
                (
                    "/v1/batch/array",
                    serde_json::json!({"name":"render", "spec":{"count":1}, "template":template}),
                )
            };
            let (status, body) = post(app, uri, &request.to_string()).await;
            let desired = council.desired_state().await;
            if answer.is_some() {
                assert_eq!(status, StatusCode::ACCEPTED, "{uri}: {body}");
                let record = desired.task_arrays.iter().next().unwrap().1;
                assert_eq!(
                    record.template.image.as_deref(),
                    Some(format!("nginx:1.27@{}", digest(7).as_str()).as_str())
                );
            } else {
                assert_eq!(status, StatusCode::BAD_GATEWAY, "{uri}: {body}");
                assert_eq!(desired.batch_ids.next_batch_id, 1);
                assert_eq!(desired.task_arrays.iter().count(), 0);
            }
            shutdown.cancel();
            council.raft().shutdown().await.unwrap();
        }
    }
}

#[tokio::test]
async fn delegated_submissions_honour_upstream_allowlist_before_registration() {
    for manifest in [false, true] {
        let council = seeded_council("trust-delegated").await;
        let restricted = binder(Some(7)).with_policy(crate::config::node::TrustPolicySection {
            upstream_default: crate::config::node::UpstreamDefault { allow: false },
            ..Default::default()
        });
        let (app, shutdown) = router_with(Some(council.clone()), None, Some(restricted));
        let template = Config::parse(
            "[job.render]\nimage = \"ghcr.io/evil/miner:1\"\ncommand = [\"/bin/true\"]\n",
        )
        .unwrap()
        .job
        .remove("render")
        .unwrap();
        let (uri, request) = if manifest {
            (
                "/v1/batch/manifest",
                serde_json::json!({"name":"render", "cohort":[{"name":"small", "count":1, "template":template}]}),
            )
        } else {
            (
                "/v1/batch/array",
                serde_json::json!({"name":"render", "spec":{"count":1}, "template":template}),
            )
        };
        let (status, body) = post(app, uri, &request.to_string()).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{uri}: {body}");
        let desired = council.desired_state().await;
        assert_eq!(desired.batch_ids.next_batch_id, 1);
        assert_eq!(desired.task_arrays.iter().count(), 0);
        shutdown.cancel();
        council.raft().shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn delegated_submissions_refuse_a_required_cosign_check_without_a_signature_source() {
    for manifest in [false, true] {
        let council = seeded_council("cosign-delegated").await;
        let policy = crate::config::node::TrustPolicySection {
            upstream: vec![crate::config::node::UpstreamTrustRule {
                pattern: "docker.io/library/nginx".into(),
                require_signatures: true,
                cosign_keys: vec![],
            }],
            ..Default::default()
        };
        let service = Arc::new(
            crate::bun::task_array_leader::TaskArrayService::new(None)
                .with_trust_policy(policy.clone()),
        );
        let (app, shutdown) = router_with_service(
            Some(council.clone()),
            None,
            Some(binder(Some(7)).with_policy(policy)),
            Some(service),
        );
        let template =
            Config::parse("[job.render]\nimage = \"nginx:1.27\"\ncommand = [\"/bin/true\"]\n")
                .unwrap()
                .job
                .remove("render")
                .unwrap();
        let (uri, request) = if manifest {
            (
                "/v1/batch/manifest",
                serde_json::json!({"name":"render", "cohort":[{"name":"small", "count":1, "template":template}]}),
            )
        } else {
            (
                "/v1/batch/array",
                serde_json::json!({"name":"render", "spec":{"count":1}, "template":template}),
            )
        };
        let (status, body) = post(app, uri, &request.to_string()).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{uri}: {body}");
        assert!(body.contains("cosign"), "{body}");
        let desired = council.desired_state().await;
        assert_eq!(desired.batch_ids.next_batch_id, 1);
        assert_eq!(desired.task_arrays.iter().count(), 0);
        shutdown.cancel();
        council.raft().shutdown().await.unwrap();
    }
}

/// Every route that keeps a job definition, with a body that submits one
/// `report:latest` job named `nightly` in `default` (#1322).
const DEFINITION_ROUTES: &[(&str, &str)] = &[
    (
        "/v1/jobs/runs",
        r#"{"name":"nightly","namespace":"default","definition":{"template":{"image":"report:latest","command":["true"]},"cron":{"expression":"0 3 * * *"}}}"#,
    ),
    (
        "/v1/batch",
        r#"{"jobs":[{"name":"nightly","namespace":"default","spec":{"image":"report:latest","command":["true"]}}]}"#,
    ),
    (
        "/v1/batch/array",
        r#"{"name":"nightly","namespace":"default","template":{"image":"report:latest","command":["true"]},"spec":{"count":1}}"#,
    ),
];

/// #1322: a job bound at admission keeps the tag it declared beside the
/// digest it runs, on every route that keeps a definition, and the policy
/// audit judges the tag: binding `report:latest` doesn't hide it from a
/// rule against `:latest`.
#[tokio::test]
async fn a_bound_job_keeps_its_declared_image_for_the_audit_on_every_route() {
    let bound = format!("report:latest@{}", digest(7).as_str());
    let policies = Config::parse(
        "[policy.pinned-jobs]\napplies_to = 'jobs'\ndeny = \"job.image.endsWith(':latest')\"\nmessage = 'pin a version'\n",
    )
    .unwrap()
    .policy;
    for (route, request) in DEFINITION_ROUTES {
        let council = seeded_council("bind-declared-job").await;
        let (app, shutdown) = router_with(Some(council.clone()), None, Some(binder(Some(7))));
        let (status, body) = post(app, route, request).await;
        assert_eq!(status, StatusCode::ACCEPTED, "{route}: {body}");

        let mut desired = council.desired_state().await;
        let record = desired
            .task_arrays
            .jobs()
            .definition("default", "nightly")
            .unwrap_or_else(|| panic!("{route}: no definition"));
        assert_eq!(
            record.definition.template.image.as_deref(),
            Some(bound.as_str()),
            "{route}"
        );
        assert_eq!(
            record.definition.declared_image.as_deref(),
            Some("report:latest"),
            "{route}"
        );
        desired.policies = policies.clone();
        let audit = crate::policy::audit::audit(&desired);
        assert_eq!(
            audit
                .verdict
                .denials
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>(),
            vec!["policy pinned-jobs denies job default/nightly: pin a version".to_string()],
            "{route}"
        );
        shutdown.cancel();
        council.raft().shutdown().await.unwrap();
    }
}

/// #1322: a job manifest's deployment keeps each job's declared image, so
/// the definitions it commits later carry it.
#[tokio::test]
async fn a_job_manifest_keeps_the_images_its_jobs_declared() {
    let council = seeded_council("bind-declared-manifest").await;
    let (app, shutdown) = router_with(Some(council.clone()), None, Some(binder(Some(7))));
    let digest = digest(7);
    let (status, body) = post(
        app,
        "/v1/apply",
        &format!(
            "[job.nightly]\nimage = 'report:latest'\ncommand = ['true']\nschedule = '0 3 * * *'\n\
             [job.pinned]\nimage = 'report:v1@{}'\ncommand = ['true']\nschedule = '0 4 * * *'\n",
            digest.as_str()
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");

    let desired = council.desired_state().await;
    let (_, deployment) = desired
        .task_arrays
        .deployments()
        .next()
        .expect("the manifest's deployment");
    assert_eq!(
        deployment.config.job["nightly"].image.as_deref(),
        Some(format!("report:latest@{}", digest.as_str()).as_str())
    );
    assert_eq!(
        deployment.declared_images,
        std::collections::BTreeMap::from([("nightly".to_string(), "report:latest".to_string())])
    );
    shutdown.cancel();
    council.raft().shutdown().await.unwrap();
}
