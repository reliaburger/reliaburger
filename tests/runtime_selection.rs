//! Release runtime selection must not depend on an installed Apple daemon.
#![cfg(target_os = "macos")]

use std::os::unix::fs::PermissionsExt;
use std::time::Duration;

#[tokio::test]
async fn automatic_selection_uses_process_even_with_apple_cli_installed() {
    let root = tempfile::tempdir().unwrap();
    let binary = root.path().join("container");
    std::fs::write(&binary, "#!/bin/sh\nexit 91\n").unwrap();
    std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();
    // Give only the child a different PATH; other async tests keep their environment.
    let output = tokio::time::timeout(
        Duration::from_secs(10),
        tokio::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--ignored",
                "--exact",
                "automatic_selection_child",
                "--nocapture",
            ])
            .env("RELIABURGER_RUNTIME_SELECTION_CHILD", "1")
            .env("PATH", format!("{}:/usr/bin:/bin", root.path().display()))
            .kill_on_drop(true)
            .output(),
    )
    .await
    .expect("runtime detection child timed out")
    .unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[tokio::test]
#[ignore = "subprocess fixture with a private PATH"]
async fn automatic_selection_child() {
    if std::env::var_os("RELIABURGER_RUNTIME_SELECTION_CHILD").is_none() {
        return;
    }
    assert_eq!(
        reliaburger::grill::detect_runtime().await,
        reliaburger::grill::DetectedRuntime::Process
    );
}
