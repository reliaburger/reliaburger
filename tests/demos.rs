//! The scripted demos under `scripts/demo/`, run for real.
//!
//! Each demo starts its own one-node cluster through `scripts/demo/lib.sh`
//! (the process runtime, so it needs no container runtime), drives it with
//! the compiled `relish` and fails on the first step that doesn't do what
//! it says. These tests run each script with this build's binaries and a
//! reserved block of ports, and check it finished.
//!
//! Gated behind `make test-cluster`, with the other suites that start real
//! clusters:
//! `RELIABURGER_CLUSTER_TESTS=1 cargo test --test demos -- --ignored`.

use std::path::Path;
use std::process::Command;

#[path = "support/bun_process.rs"]
mod bun_process;

/// Run one demo script to completion and return its stdout.
fn run_demo(script: &str) -> String {
    assert!(
        std::env::var_os("RELIABURGER_CLUSTER_TESTS").is_some(),
        "set RELIABURGER_CLUSTER_TESTS=1 (make test-cluster)"
    );
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    // The demo binds six ports from its base. The lease keeps other
    // fixtures off them until the demo is done.
    let ports = bun_process::reserve_port_block_lease(6);
    let base = ports.base;
    let output = Command::new("bash")
        .arg(root.join("scripts/demo").join(script))
        .current_dir(root)
        .env("RELISH", env!("CARGO_BIN_EXE_relish"))
        .env("BUN", env!("CARGO_BIN_EXE_bun"))
        .env("TESTAPP", env!("CARGO_BIN_EXE_testapp"))
        .env("DEMO_PORT_BASE", base.to_string())
        .env_remove("RELIABURGER_ENDPOINT")
        .env_remove("RELIABURGER_TOKEN")
        .env_remove("RELIABURGER_CA_CERT")
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    assert!(
        output.status.success(),
        "{script} failed ({})\nstdout:\n{stdout}\nstderr:\n{}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        stdout.contains("demo complete"),
        "{script} exited 0 without finishing:\n{stdout}"
    );
    stdout
}

#[test]
#[ignore = "starts a real one-node cluster; run with make test-cluster"]
fn rollout_demo_rolls_back_a_bad_version_and_marks_it_failed() {
    let stdout = run_demo("rollout.sh");
    assert!(stdout.contains("RolloutFailed"), "{stdout}");
    assert!(stdout.contains("serving v1 after the rollback"), "{stdout}");
    assert!(stdout.contains("serving v1 after the bad v3"), "{stdout}");
}

#[test]
#[ignore = "starts a real one-node cluster; run with make test-cluster"]
fn gitops_demo_syncs_a_repository_and_leaves_applied_apps_alone() {
    let stdout = run_demo("gitops.sh");
    assert!(stdout.contains("GitOps removed storefront"), "{stdout}");
    assert!(
        stdout.contains("scratch, applied by hand, survived"),
        "{stdout}"
    );
}
