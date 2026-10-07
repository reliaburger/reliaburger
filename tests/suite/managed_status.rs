//! Managed status must communicate unhealthy nodes through its process exit code.
#![cfg(unix)]

use reliaburger::relish::quickstart::{
    security,
    state::{ClusterSpec, Operation},
};
use std::os::unix::fs::PermissionsExt;
use std::sync::{Arc, Mutex};
use tokio::io::AsyncReadExt;

async fn capture_status_output(
    mut reader: impl tokio::io::AsyncRead + Unpin + Send,
    captured: Arc<Mutex<Vec<u8>>>,
) -> std::io::Result<()> {
    let mut chunk = [0_u8; 4096];
    loop {
        let length = reader.read(&mut chunk).await?;
        if length == 0 {
            return Ok(());
        }
        captured.lock().unwrap().extend_from_slice(&chunk[..length]);
    }
}

async fn finish_status_readers(
    readers: &mut [tokio::task::JoinHandle<std::io::Result<()>>],
) -> Vec<String> {
    let mut errors = Vec::new();
    for (index, reader) in readers.iter_mut().enumerate() {
        match tokio::time::timeout(std::time::Duration::from_secs(1), &mut *reader).await {
            Ok(Ok(Ok(()))) => {}
            Ok(result) => errors.push(format!("reader {index} failed: {result:?}")),
            Err(_) => {
                reader.abort();
                let joined = reader.await;
                errors.push(format!(
                    "reader {index} did not reach EOF; aborted: {joined:?}"
                ));
            }
        }
    }
    // Finish both readers before reporting any failure so one bad pipe cannot
    // leave its sibling task unowned, and a truncated capture cannot pass.
    errors
}

#[tokio::test]
async fn managed_status_fails_for_missing_stopped_and_unreachable_nodes() {
    for status in [None, Some("Stopped"), Some("Running")] {
        let phase = status.unwrap_or("missing");
        let started = tokio::time::Instant::now();
        eprintln!("managed-status phase={phase} setup-begin");
        let root = tempfile::tempdir().unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let operation = Operation::open(
            root.path(),
            &ClusterSpec {
                name: "status-test".into(),
                nodes: 1,
                version: "v0.1.0".parse().unwrap(),
                api_port: port,
                ingress_port: if port == 18080 { 18081 } else { 18080 },
                registry_port: if port == 15050 { 15051 } else { 15050 },
            },
        )
        .unwrap();
        security::prepare(&operation).unwrap();
        let name = operation.state.nodes[0].name.clone();
        drop(operation);
        // Holding a listener without accepting proves that a running VM with
        // an unresponsive TLS API must not succeed either.
        let lima = root.path().join("tools/lima-2.1.0/bin/limactl");
        std::fs::create_dir_all(lima.parent().unwrap()).unwrap();
        let lima_ready = lima.parent().unwrap().join("ready");
        let output = status
            .map(|status| serde_json::json!({"name":name,"status":status}).to_string())
            .unwrap_or_default();
        std::fs::write(
            &lima,
            format!("#!/bin/sh\n[ \"$1\" = list ] || exit 70\ndir=${{0%/*}}\ntouch \"$dir/ready\"\nprintf '%s\\n' '{output}'\n"),
        )
        .unwrap();
        std::fs::set_permissions(&lima, std::fs::Permissions::from_mode(0o700)).unwrap();
        eprintln!(
            "managed-status phase={phase} setup-end elapsed={:?}",
            started.elapsed()
        );
        let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_relish"))
            .args(["local", "status", "--name", "status-test"])
            .env("RELIABURGER_HOME", root.path())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .unwrap();
        let pid = child.id().unwrap();
        let stdout_bytes = Arc::new(Mutex::new(Vec::new()));
        let stderr_bytes = Arc::new(Mutex::new(Vec::new()));
        let mut readers = [
            tokio::spawn(capture_status_output(
                child.stdout.take().unwrap(),
                stdout_bytes.clone(),
            )),
            tokio::spawn(capture_status_output(
                child.stderr.take().unwrap(),
                stderr_bytes.clone(),
            )),
        ];
        eprintln!(
            "managed-status phase={phase} child-spawned pid={pid} elapsed={:?}",
            started.elapsed()
        );
        let waited = tokio::time::timeout(std::time::Duration::from_secs(15), child.wait()).await;
        let exit = match waited {
            Ok(Ok(status)) => status,
            error => {
                // Own the actual Child through kill and reap. Never signal a
                // saved numeric pid; the number is diagnostic evidence only.
                let kill = child.start_kill();
                let reaped =
                    tokio::time::timeout(std::time::Duration::from_secs(3), child.wait()).await;
                let reader_errors = finish_status_readers(&mut readers).await;
                panic!(
                    "managed-status phase={phase} pid={pid} elapsed={:?} lima-ready={} wait={error:?} kill={kill:?} reap={reaped:?} reader-errors={reader_errors:?} stdout={} stderr={}",
                    started.elapsed(),
                    lima_ready.exists(),
                    String::from_utf8_lossy(&stdout_bytes.lock().unwrap()),
                    String::from_utf8_lossy(&stderr_bytes.lock().unwrap())
                );
            }
        };
        let reader_errors = finish_status_readers(&mut readers).await;
        assert!(
            reader_errors.is_empty(),
            "managed-status phase={phase} incomplete output capture: {reader_errors:?}"
        );
        let result = std::process::Output {
            status: exit,
            stdout: stdout_bytes.lock().unwrap().clone(),
            stderr: stderr_bytes.lock().unwrap().clone(),
        };
        eprintln!(
            "managed-status phase={phase} child-completed pid={pid} elapsed={:?} lima-ready={}",
            started.elapsed(),
            lima_ready.exists()
        );
        assert!(
            lima_ready.exists(),
            "managed-status phase={phase} did not reach the Lima fixture"
        );
        let stdout = String::from_utf8(result.stdout).unwrap();
        assert!(
            stdout.contains(&name),
            "status was not inspected: {stdout}; {}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert_eq!(
            result.status.code(),
            Some(1),
            "unhealthy {status:?} reported success: {stdout}"
        );
        drop(listener);
    }
}
