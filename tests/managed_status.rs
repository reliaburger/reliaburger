//! Managed status must communicate unhealthy nodes through its process exit code.
#![cfg(unix)]

use reliaburger::relish::quickstart::{
    security,
    state::{ClusterSpec, Operation},
};
use std::os::unix::fs::PermissionsExt;

#[tokio::test]
async fn managed_status_fails_for_missing_stopped_and_unreachable_nodes() {
    for status in [None, Some("Stopped"), Some("Running")] {
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
                registry_port: None,
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
        let output = status
            .map(|status| serde_json::json!({"name":name,"status":status}).to_string())
            .unwrap_or_default();
        std::fs::write(
            &lima,
            format!("#!/bin/sh\n[ \"$1\" = list ] || exit 70\nprintf '%s\\n' '{output}'\n"),
        )
        .unwrap();
        std::fs::set_permissions(&lima, std::fs::Permissions::from_mode(0o700)).unwrap();
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(15),
            tokio::process::Command::new(env!("CARGO_BIN_EXE_relish"))
                .args(["local", "status", "--name", "status-test"])
                .env("RELIABURGER_HOME", root.path())
                .kill_on_drop(true)
                .output(),
        )
        .await
        .unwrap()
        .unwrap();
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
