//! Production startup boundaries for the first supported state/protocol format.

use std::time::Duration;

#[tokio::test]
async fn compatibility_query_requires_no_configuration_or_runtime_state() {
    let directory = tempfile::tempdir().unwrap();
    let output = tokio::process::Command::new(env!("CARGO_BIN_EXE_bun"))
        .arg("--compatibility")
        .current_dir(directory.path())
        .kill_on_drop(true)
        .output()
        .await
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: reliaburger::compatibility::Compatibility =
        serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(value, reliaburger::compatibility::CURRENT);
    assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 0);
}

#[tokio::test]
async fn unmarked_development_state_is_refused_before_subsystems_start() {
    let directory = tempfile::tempdir().unwrap();
    let data = directory.path().join("data");
    std::fs::create_dir(&data).unwrap();
    let legacy = data.join("development-state");
    std::fs::write(&legacy, b"preserve this state").unwrap();
    let config = directory.path().join("node.toml");
    let root = directory.path().display();
    let registry_port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    std::fs::write(
        &config,
        format!(
            r#"
[node]
name = "compatibility-test"
[storage]
data = "{root}/data"
images = "{root}/images"
logs = "{root}/logs"
metrics = "{root}/metrics"
volumes = "{root}/volumes"
[images]
registry_port = {registry_port}
"#
        ),
    )
    .unwrap();
    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_bun"))
        .args([
            "--config",
            config.to_str().unwrap(),
            "--runtime",
            "process",
            "--listen",
            "127.0.0.1:0",
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let status = tokio::time::timeout(Duration::from_secs(30), child.wait()).await;
    if status.is_err() {
        child.kill().await.unwrap();
        child.wait().await.unwrap();
    }
    let mut error = String::new();
    use tokio::io::AsyncReadExt;
    child
        .stderr
        .take()
        .unwrap()
        .read_to_string(&mut error)
        .await
        .unwrap();
    assert!(
        matches!(status, Ok(Ok(status)) if !status.success()),
        "legacy startup was not refused: {error}"
    );
    assert!(error.contains("fresh cluster"), "{error}");
    assert_eq!(std::fs::read(&legacy).unwrap(), b"preserve this state");
    assert_eq!(std::fs::read_dir(&data).unwrap().count(), 1);
}
