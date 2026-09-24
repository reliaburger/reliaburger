//! Black-box watch recovery and interrupt exit status.
#![cfg(unix)]

use axum::{Json, Router, routing::get};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};

#[tokio::test]
async fn watch_recovers_from_collection_failure_and_retains_warning_exit() {
    let calls = Arc::new(AtomicUsize::new(0));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let app = Router::new()
        .route(
            "/v1/health",
            get(move || {
                let calls = Arc::clone(&calls);
                async move {
                    let status = if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                        axum::http::StatusCode::SERVICE_UNAVAILABLE
                    } else {
                        axum::http::StatusCode::OK
                    };
                    (status, Json(serde_json::json!({"status":"ok"})))
                }
            }),
        )
        .route(
            "/v1/cluster/nodes",
            get(|| async { Json(serde_json::json!([])) }),
        );
    let server = tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_relish"))
        .args([
            "--endpoint",
            &format!("http://{address}"),
            "wtf",
            "--watch",
            "--interval",
            "1",
        ])
        .env_remove("RELIABURGER_TOKEN")
        .env_remove("RELIABURGER_CA_CERT")
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    let result: Result<(), String> = async {
        for recovered in [false, true] {
            let report = tokio::time::timeout(Duration::from_secs(40), async {
                let mut report = String::new();
                while let Some(line) = lines.next_line().await.map_err(|error| error.to_string())? {
                    report.push_str(&line);
                    report.push('\n');
                    if line.starts_with("Summary:") {
                        return Ok(report);
                    }
                }
                Err("watch exited instead of producing a report".to_string())
            })
            .await
            .map_err(|_| "watch did not collect again".to_string())??;
            if !report.contains("UNKNOWN") {
                return Err(format!("incomplete evidence was hidden: {report}"));
            }
            if recovered && !report.contains("standalone mode does not require a council") {
                return Err(format!("watch failed to recover: {report}"));
            }
        }
        let pid = nix::unistd::Pid::from_raw(child.id().ok_or("watch already exited")? as i32);
        nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGINT)
            .map_err(|error| error.to_string())?;
        let status = tokio::time::timeout(Duration::from_secs(5), child.wait())
            .await
            .map_err(|_| "watch ignored interrupt".to_string())?
            .map_err(|error| error.to_string())?;
        if status.code() != Some(2) {
            return Err(format!("expected warning exit 2, got {status}"));
        }
        Ok(())
    }
    .await;
    if child.try_wait().unwrap().is_none() {
        child.kill().await.unwrap();
    }
    let _ = child.wait().await;
    server.abort();
    let _ = server.await;
    assert!(result.is_ok(), "{result:?}");
}
