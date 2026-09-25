//! Black-box contracts for `relish uninstall` against a throwaway home.

use std::path::Path;
use std::process::{Command, Output};

fn uninstall(home: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_relish"))
        .arg("uninstall")
        .args(args)
        .env("HOME", home)
        .env("RELIABURGER_HOME", home.join(".reliaburger"))
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap()
}

fn installed(home: &Path) {
    let root = home.join(".reliaburger");
    std::fs::create_dir_all(root.join("bin")).unwrap();
    std::fs::create_dir_all(root.join("tools/lima-2.1.0")).unwrap();
    std::fs::create_dir_all(root.join("cache")).unwrap();
    std::fs::write(root.join("bin/relish"), "binary").unwrap();
    std::fs::write(root.join("cache/guest.img"), "image").unwrap();
}

#[test]
fn uninstall_without_a_terminal_needs_yes_and_changes_nothing() {
    let home = tempfile::tempdir().unwrap();
    installed(home.path());
    let output = uninstall(home.path(), &[]);
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("--yes"));
    assert!(home.path().join(".reliaburger/bin/relish").exists());
}

#[test]
fn uninstall_yes_removes_the_installation() {
    let home = tempfile::tempdir().unwrap();
    installed(home.path());
    let output = uninstall(home.path(), &["--yes"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!home.path().join(".reliaburger").exists());
}

#[test]
fn uninstall_refuses_while_a_quickstart_cluster_exists() {
    let home = tempfile::tempdir().unwrap();
    installed(home.path());
    let cluster = home.path().join(".reliaburger/clusters/laptop");
    std::fs::create_dir_all(&cluster).unwrap();
    std::fs::write(cluster.join("operation.lock"), "").unwrap();
    std::fs::write(cluster.join("state.json"), "{}").unwrap();
    let output = uninstall(home.path(), &["--yes"]);
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("laptop") && stderr.contains("relish local destroy"));
    assert!(home.path().join(".reliaburger/tools").exists());
}

#[test]
fn uninstall_succeeds_after_destroy_leaves_only_the_operation_lock() {
    let home = tempfile::tempdir().unwrap();
    installed(home.path());
    let cluster = home.path().join(".reliaburger/clusters/laptop");
    std::fs::create_dir_all(&cluster).unwrap();
    std::fs::write(cluster.join("operation.lock"), "").unwrap();
    let output = uninstall(home.path(), &["--yes"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!home.path().join(".reliaburger/clusters").exists());
    assert!(!home.path().join(".reliaburger/tools").exists());
}

#[test]
fn uninstall_removes_the_context_lock_left_after_destroy() {
    let home = tempfile::tempdir().unwrap();
    installed(home.path());
    // `relish local destroy` removes context.json but leaves its lock.
    std::fs::write(home.path().join(".reliaburger/context.lock"), "").unwrap();
    let output = uninstall(home.path(), &["--yes"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!home.path().join(".reliaburger").exists());
}

#[test]
fn uninstall_keeps_a_saved_context_and_its_lock() {
    let home = tempfile::tempdir().unwrap();
    installed(home.path());
    let root = home.path().join(".reliaburger");
    std::fs::write(root.join("context.json"), "{}").unwrap();
    std::fs::write(root.join("context.lock"), "").unwrap();
    let output = uninstall(home.path(), &["--yes"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(root.join("context.json").exists());
    assert!(root.join("context.lock").exists());
    assert!(!root.join("tools").exists());
}
