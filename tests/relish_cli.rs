//! Black-box contracts for the compiled `relish` executable.
//!
//! Parser unit tests are useful, but they cannot catch broken exit codes,
//! stdout/stderr routing, or a binary that was wired to the wrong handler.

use std::process::{Command, Output};

fn run(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_relish"))
        .args(args)
        .env_remove("RELIABURGER_TOKEN")
        .env_remove("RELIABURGER_CA_CERT")
        .output()
        .unwrap()
}

#[test]
fn version_reports_the_compiled_binary() {
    let output = run(&["--version"]);
    assert!(output.status.success());
    assert_eq!(String::from_utf8_lossy(&output.stdout), "relish 0.1.0\n");
    assert!(output.stderr.is_empty());
}

#[test]
fn invalid_output_format_exits_with_clap_error() {
    let output = run(&["--output", "xml", "status"]);
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("invalid value 'xml'"));
}

#[test]
fn apply_dry_run_succeeds_without_an_agent() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("app.toml");
    std::fs::write(
        &config,
        r#"
[app.web]
image = "proc-grill:image-ignored"
command = ["true"]
"#,
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_relish"))
        .arg("apply")
        .arg(&config)
        .arg("--dry-run")
        .env_remove("RELIABURGER_TOKEN")
        .env_remove("RELIABURGER_CA_CERT")
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("web"), "{stdout}");
    assert!(stdout.contains("dry run"), "{stdout}");
    assert!(output.stderr.is_empty());
}

#[test]
fn missing_apply_file_exits_nonzero_and_uses_stderr() {
    let output = run(&["apply", "/definitely/missing/reliaburger.toml", "--dry-run"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("error:"));
}

#[test]
fn endpoint_environment_rejects_remote_plaintext_before_dispatch() {
    let output = Command::new(env!("CARGO_BIN_EXE_relish"))
        .arg("status")
        .env("RELIABURGER_ENDPOINT", "http://192.0.2.10:9117")
        .env_remove("RELIABURGER_TOKEN")
        .env_remove("RELIABURGER_CA_CERT")
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("must use HTTPS"));
}

#[test]
fn offline_log_export_refuses_an_unreadable_checkpoint_before_upload() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("source");
    let dest = dir.path().join("dest");
    std::fs::create_dir(&source).unwrap();
    std::fs::write(source.join("logs.parquet"), b"exported bytes").unwrap();
    std::fs::create_dir(source.join("_export_checkpoint.json")).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_relish"))
        .args(["logs-export", "--source"])
        .arg(&source)
        .arg("--dest")
        .arg(&dest)
        .env("RELIABURGER_HOME", dir.path().join("home"))
        .env_remove("RELIABURGER_ENDPOINT")
        .env_remove("RELIABURGER_TOKEN")
        .env_remove("RELIABURGER_CA_CERT")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        std::fs::read(source.join("logs.parquet")).unwrap(),
        b"exported bytes"
    );
    assert!(
        !dest.exists(),
        "invalid checkpoint must be refused before upload"
    );
    assert!(String::from_utf8_lossy(&output.stderr).contains("checkpoint"));
    assert!(
        output.stdout.is_empty(),
        "must not report a complete successful export"
    );
}

#[test]
fn offline_log_export_persists_progress_between_runs() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("source");
    std::fs::create_dir(&source).unwrap();
    std::fs::write(source.join("logs.parquet"), b"exported bytes").unwrap();
    for expected in ["exported 1 file(s)", "no new files to export"] {
        let output = Command::new(env!("CARGO_BIN_EXE_relish"))
            .args(["logs-export", "--source"])
            .arg(&source)
            .arg("--dest")
            .arg(dir.path().join("dest"))
            .env("RELIABURGER_HOME", dir.path().join("home"))
            .output()
            .unwrap();
        assert!(output.status.success(), "{:?}", output);
        assert!(String::from_utf8_lossy(&output.stdout).contains(expected));
    }
}

#[cfg(unix)]
#[test]
fn offline_log_export_rejects_a_non_utf8_destination() {
    use std::os::unix::ffi::OsStringExt;
    let dir = tempfile::tempdir().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_relish"))
        .args(["logs-export", "--source"])
        .arg(dir.path())
        .arg("--dest")
        .arg(std::ffi::OsString::from_vec(vec![0xff]))
        .env("RELIABURGER_HOME", dir.path().join("home"))
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("UTF-8"));
}

#[test]
fn offline_log_export_rejects_a_missing_source() {
    let dir = tempfile::tempdir().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_relish"))
        .args(["logs-export", "--source"])
        .arg(dir.path().join("missing"))
        .arg("--dest")
        .arg(dir.path().join("dest"))
        .env("RELIABURGER_HOME", dir.path().join("home"))
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
}

#[tokio::test]
async fn cancel_deploy_waits_for_terminal_evidence_and_refuses_unknown_success() {
    use axum::{
        Json, Router,
        routing::{get, post},
    };
    for outcome in ["cancelled", "unknown"] {
        let accepted = serde_json::json!({
            "id": "deploy-test", "phase": "deploying_apps", "outcome": null,
            "started_at": 1, "phase_changed_at": 1, "finished_at": null,
            "cancellation_requested_at": 2, "targets": [], "current_target": null,
            "message": "cancellation requested",
        });
        let mut finished = accepted.clone();
        finished["phase"] = "finished".into();
        finished["outcome"] = outcome.into();
        finished["finished_at"] = 3.into();
        finished["message"] = "worker finished".into();
        let app = Router::new()
            .route(
                "/v1/deploys/operations/deploy-test/cancel",
                post(move || async move { (axum::http::StatusCode::ACCEPTED, Json(accepted)) }),
            )
            .route(
                "/v1/deploys/operations",
                get(move || async move {
                    Json(serde_json::json!({"active_deploys": [], "history": [finished]}))
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let output = tokio::time::timeout(
            std::time::Duration::from_secs(60),
            tokio::process::Command::new(env!("CARGO_BIN_EXE_relish"))
                .args(["--output", "json", "cancel-deploy", "deploy-test"])
                .env("RELIABURGER_ENDPOINT", endpoint)
                .env_remove("RELIABURGER_TOKEN")
                .env_remove("RELIABURGER_CA_CERT")
                .kill_on_drop(true)
                .output(),
        )
        .await
        .unwrap()
        .unwrap();
        server.abort();
        let _ = server.await;
        assert_eq!(
            output.status.success(),
            outcome == "cancelled",
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let record: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(record["outcome"], outcome);
        assert_eq!(record["phase"], "finished");
    }
}
