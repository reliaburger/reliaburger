//! `relish gym` through the binary (#1683): the scenario list, and the
//! namespace refusals, which must happen before anything changes. The stub
//! API below records every request, so a refusal that sent a mutating one
//! fails here.

use std::process::Output;
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::extract::Request;
use axum::response::IntoResponse;

fn relish(args: &[&str], endpoint: Option<&str>) -> Output {
    let home = tempfile::tempdir().unwrap();
    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_relish"));
    command
        .args(args)
        .env_remove("RELIABURGER_TOKEN")
        .env_remove("RELIABURGER_CA_CERT")
        .env("RELIABURGER_HOME", home.path())
        .stdin(std::process::Stdio::null());
    match endpoint {
        Some(endpoint) => command.env("RELIABURGER_ENDPOINT", endpoint),
        None => command.env_remove("RELIABURGER_ENDPOINT"),
    };
    command.output().unwrap()
}

/// Only with the scenarios: a `relish` built from the source bundle has
/// none, and `relish agent patch test` runs every test there (#1823's
/// review).
#[cfg(gym_scenarios)]
#[test]
fn gym_list_names_every_scenario_and_no_node_fault() {
    let output = relish(&["-o", "json", "gym", "list"], None);
    assert!(output.status.success(), "{output:?}");
    let listed: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let ids: Vec<&str> = listed
        .as_array()
        .unwrap()
        .iter()
        .map(|scenario| scenario["id"].as_str().unwrap())
        .collect();
    for id in [
        "crash-loop",
        "log-injection",
        "unschedulable",
        "paused-app",
        "killed-instance",
        "report-redaction",
    ] {
        assert!(ids.contains(&id), "{id} missing from {ids:?}");
    }
    assert!(!ids.contains(&"partitioned-node"));
}

/// A stub Bun API: a cluster named `stub` whose namespace `staging` holds
/// someone else's app. It records the method and path of every request.
async fn stub(requests: Arc<Mutex<Vec<String>>>) -> String {
    let capabilities = serde_json::to_value(reliaburger::bun::capabilities::ClusterCapabilities {
        cluster_name: "stub".into(),
        container_runtime: "runc".into(),
        ..Default::default()
    })
    .unwrap();
    let app = Router::new().fallback(move |request: Request| {
        let requests = requests.clone();
        let capabilities = capabilities.clone();
        async move {
            let path = request
                .uri()
                .path_and_query()
                .map(|path| path.to_string())
                .unwrap_or_default();
            requests
                .lock()
                .unwrap()
                .push(format!("{} {path}", request.method()));
            let body = match request.uri().path() {
                "/v1/capabilities" => capabilities,
                "/v1/diagnostics/apps" => serde_json::json!([{
                    "app": "checkout", "namespace": "staging",
                    "desired_replicas": 1, "scheduled_replicas": 1, "service_port": null,
                }]),
                "/v1/status" => serde_json::json!([]),
                "/v1/jobs" => serde_json::json!({"jobs": [], "warnings": []}),
                _ => return axum::http::StatusCode::NOT_FOUND.into_response(),
            };
            axum::Json(body).into_response()
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{address}")
}

#[tokio::test]
async fn gym_run_refuses_default_before_sending_anything() {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let endpoint = stub(requests.clone()).await;
    let out = tempfile::tempdir().unwrap();
    let report = out.path().join("report");
    let output = tokio::task::spawn_blocking(move || {
        relish(
            &[
                "gym",
                "run",
                "--namespace",
                "default",
                "--yes",
                "--out",
                report.to_str().unwrap(),
            ],
            Some(&endpoint),
        )
    })
    .await
    .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("never runs in `default`"), "{stderr}");
    assert!(requests.lock().unwrap().is_empty(), "{requests:?}");
}

/// Only with the scenarios: without them `--scenario` is refused before the
/// namespace check this test is about.
#[cfg(gym_scenarios)]
#[tokio::test]
async fn gym_run_refuses_someone_elses_namespace_without_changing_anything() {
    let requests = Arc::new(Mutex::new(Vec::new()));
    let endpoint = stub(requests.clone()).await;
    let out = tempfile::tempdir().unwrap();
    let report = out.path().join("report");
    let output = tokio::task::spawn_blocking(move || {
        relish(
            &[
                "gym",
                "run",
                "--namespace",
                "staging",
                "--yes",
                "--out",
                report.to_str().unwrap(),
            ],
            Some(&endpoint),
        )
    })
    .await
    .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("checkout"), "{stderr}");
    assert!(stderr.contains("--confirm-namespace staging"), "{stderr}");
    let requests = requests.lock().unwrap();
    assert!(!requests.is_empty(), "it should have looked first");
    assert!(
        requests.iter().all(|request| request.starts_with("GET ")),
        "a refusal sent a change: {requests:?}"
    );
    assert!(!out.path().join("report").exists());
}
