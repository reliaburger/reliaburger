//! Invalid development-cluster inputs must fail before touching a VM.
#![cfg(unix)]

use std::os::unix::{ffi::OsStringExt, fs::PermissionsExt};
use std::path::Path;

fn fixture(root: &Path) {
    std::fs::create_dir_all(root.join("bin")).unwrap();
    let lima = root.join("bin/limactl");
    std::fs::write(&lima, "#!/bin/sh\nprintf '%s\\n' \"$*\" >> \"$HOME/calls\"\ncase \"$1\" in --version) echo fake;; list) echo unrelated-vm;; esac\n").unwrap();
    std::fs::set_permissions(lima, std::fs::Permissions::from_mode(0o700)).unwrap();
}

fn command(root: &Path) -> tokio::process::Command {
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_relish"));
    command
        .env("HOME", root)
        .env(
            "PATH",
            format!("{}:/usr/bin:/bin", root.join("bin").display()),
        )
        .kill_on_drop(true);
    command
}

#[tokio::test]
async fn corrupt_saved_ownership_never_operates_on_an_unrelated_vm() {
    let root = tempfile::tempdir().unwrap();
    fixture(root.path());
    let directory = root.path().join(".reliaburger/dev/clusters");
    std::fs::create_dir_all(&directory).unwrap();
    let state = directory.join("safe.json");
    let bytes = br#"{"name":"safe","runtime":"process","nodes":[{"name":"unrelated-vm","ip":"192.168.1.1","cpus":2,"memory":"2GiB"}]}"#;
    let original: serde_json::Value = serde_json::from_slice(bytes).unwrap();
    let mut malformed = vec![
        original.clone(),
        serde_json::json!({"name":"safe","nodes":[]}),
    ];
    for field in ["ip", "runtime", "name"] {
        let mut value = original.clone();
        value["nodes"][0]["name"] = serde_json::json!("reliaburger-safe-1");
        match field {
            "ip" => value["nodes"][0]["ip"] = serde_json::Value::Null,
            "runtime" => value["runtime"] = serde_json::json!("process; touch leaked"),
            _ => value["name"] = serde_json::json!("different"),
        }
        malformed.push(value);
    }
    for value in malformed {
        let bytes = serde_json::to_vec(&value).unwrap();
        for action in ["start", "stop", "destroy"] {
            std::fs::write(&state, &bytes).unwrap();
            let output = command(root.path())
                .args(["dev", action, "safe"])
                .output()
                .await
                .unwrap();
            assert_eq!(
                output.status.code(),
                Some(1),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            assert_eq!(std::fs::read(&state).unwrap(), bytes);
            assert!(
                !root.path().join("calls").exists(),
                "invalid ownership reached Lima"
            );
        }
    }
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn non_utf8_checkout_is_refused_before_recreating_the_test_vm() {
    let root = tempfile::tempdir().unwrap();
    fixture(root.path());
    let checkout = root
        .path()
        .join(std::ffi::OsString::from_vec(b"checkout-\xff".to_vec()));
    std::fs::create_dir(&checkout).unwrap();
    let output = command(root.path())
        .current_dir(checkout)
        .args(["dev", "test", "--recreate"])
        .output()
        .await
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("UTF-8"));
    assert!(!root.path().join("calls").exists());
}

#[tokio::test]
async fn zero_nodes_is_a_validation_error_before_lima() {
    let root = tempfile::tempdir().unwrap();
    fixture(root.path());
    let output = command(root.path())
        .args(["dev", "create", "safe", "--nodes", "0"])
        .output()
        .await
        .unwrap();
    assert_eq!(output.status.code(), Some(1), "zero nodes must not panic");
    assert!(String::from_utf8_lossy(&output.stderr).contains("nodes"));
    assert!(!root.path().join("calls").exists());
}

#[tokio::test]
async fn missing_owned_vm_preserves_saved_state_and_other_vms() {
    let root = tempfile::tempdir().unwrap();
    fixture(root.path());
    let directory = root.path().join(".reliaburger/dev/clusters");
    std::fs::create_dir_all(&directory).unwrap();
    let state = directory.join("safe.json");
    let bytes = br#"{"name":"safe","runtime":"process","nodes":[{"name":"reliaburger-safe-1","ip":"192.168.1.1","cpus":2,"memory":"2GiB"}]}"#;
    std::fs::write(&state, bytes).unwrap();
    let output = command(root.path())
        .args(["dev", "destroy", "safe"])
        .output()
        .await
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("missing from Lima"));
    assert_eq!(std::fs::read(&state).unwrap(), bytes);
    let calls = std::fs::read_to_string(root.path().join("calls")).unwrap();
    assert!(calls.lines().all(|line| line.starts_with("list ")));
}

#[tokio::test]
async fn checkout_and_filter_shell_metacharacters_remain_literal_arguments() {
    let root = tempfile::tempdir().unwrap();
    fixture(root.path());
    let tools = root.path().join("bin");
    let scripts = [
        (
            "limactl",
            "#!/bin/sh\ncase \"$1\" in --version) exit 0;; list) if [ \"$3\" = '{{.Name}}' ]; then echo reliaburger-test; else echo 'reliaburger-test Running'; fi;; shell) shift 2; exec \"$@\";; *) exit 90;; esac\n",
        ),
        ("sudo", "#!/bin/sh\n[ \"$1\" = -E ] && shift\nexec \"$@\"\n"),
        (
            "cargo",
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > \"$HOME/cargo-args\"\n",
        ),
        ("mkdir", "#!/bin/sh\nexit 0\n"),
    ];
    for (name, script) in scripts {
        let path = tools.join(name);
        std::fs::write(&path, script).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    std::fs::create_dir(root.path().join(".cargo")).unwrap();
    std::fs::write(root.path().join(".cargo/env"), "# fixture\n").unwrap();
    let checkout = root.path().join("repo with 'quotes' $(touch leaked)");
    std::fs::create_dir(&checkout).unwrap();
    let filter = "test_name; touch leaked-filter";
    let output = command(root.path())
        .current_dir(&checkout)
        .args(["dev", "test", filter])
        .output()
        .await
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let arguments = std::fs::read_to_string(root.path().join("cargo-args")).unwrap();
    assert!(arguments.lines().any(|argument| argument == filter));
    assert!(!checkout.join("leaked").exists());
    assert!(!checkout.join("leaked-filter").exists());
}

#[tokio::test]
async fn non_utf8_binary_argument_is_refused_before_lima() {
    let root = tempfile::tempdir().unwrap();
    fixture(root.path());
    let binary = root
        .path()
        .join(std::ffi::OsString::from_vec(b"bun-\xff".to_vec()));
    let output = command(root.path())
        .args(["dev", "create", "safe", "--bun"])
        .arg(binary)
        .output()
        .await
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&output.stderr).contains("UTF-8"));
    assert!(!root.path().join("calls").exists());
}
