//! #881: a process app whose binary is missing fails at once, with the reason.
//!
//! A real Bun (process runtime) allows a binary that doesn't exist on the
//! node, so placement admits it and the launch is what finds it missing.
//! The deploy must fail fast with "binary not found", and `relish status` and
//! `relish wtf` must say so, instead of the instance sitting in `starting`
//! while the deploy retries.

use std::time::{Duration, Instant};

use crate::bun_process;
use bun_process::{
    reserve_address, run_relish, spawn_bun_with_port_retry, wait_for_relish,
    wait_for_relish_output, write_portable_node_config,
};

/// Long enough for a loaded runner to launch and refuse once, far shorter
/// than the deploy's health budget the old behaviour waited out.
const FAIL_FAST: Duration = Duration::from_secs(15);

#[test]
fn a_process_app_with_a_missing_binary_fails_fast_with_binary_not_found() {
    let root = tempfile::tempdir().unwrap();
    let missing = root.path().join("not-installed").join("ghost-server");
    let missing = missing.to_str().unwrap().to_string();
    let (mut bun, address) = spawn_bun_with_port_retry(false, || {
        let config = write_portable_node_config(root.path());
        let mut text = std::fs::read_to_string(&config).unwrap();
        // The node allows the binary, so this is a launch failure, not a
        // placement refusal (#702).
        text.push_str(&format!(
            "\n[process_workloads]\nallowed_binaries = [{missing:?}]\nmount_isolation = false\n"
        ));
        std::fs::write(&config, text).unwrap();
        (config, reserve_address(), root.path().join("bun.log"))
    });
    let endpoint = format!("http://{address}");
    wait_for_relish(&mut bun, &["--endpoint", &endpoint, "status"]);

    let manifest = root.path().join("ghost.toml");
    std::fs::write(&manifest, format!("[app.ghost]\nexec = {missing:?}\n")).unwrap();
    let started = Instant::now();
    let apply = run_relish(&["--endpoint", &endpoint, "apply", manifest.to_str().unwrap()]);
    let elapsed = started.elapsed();
    let stderr = String::from_utf8_lossy(&apply.stderr).to_string();
    assert!(
        !apply.status.success(),
        "the deploy must fail\nstdout={}\nstderr={stderr}",
        String::from_utf8_lossy(&apply.stdout)
    );
    let reason = format!("binary not found: {missing}");
    assert!(
        stderr.contains(&reason),
        "the deploy must name the missing binary\nstderr={stderr}\nbun log={}",
        bun_process::bun_log_diagnostic(&bun.log_path)
    );
    assert!(
        elapsed < FAIL_FAST,
        "the deploy took {elapsed:?} to fail; a missing binary must not wait out a budget"
    );

    // The instance is failed, with the reason, not stuck in `starting`.
    let status = wait_for_relish_output(
        &mut bun,
        &["--endpoint", &endpoint, "--output", "json", "status"],
        &reason,
    );
    let instances: serde_json::Value = serde_json::from_slice(&status.stdout).unwrap();
    let ghost = instances
        .as_array()
        .unwrap()
        .iter()
        .find(|instance| instance["app_name"] == "ghost")
        .unwrap_or_else(|| panic!("no ghost instance in {instances}"));
    assert_eq!(ghost["state"], "failed", "{ghost}");
    assert_eq!(ghost["reason"], reason.as_str(), "{ghost}");

    let table = run_relish(&["--endpoint", &endpoint, "status"]);
    let table = String::from_utf8_lossy(&table.stdout);
    assert!(table.contains(&reason), "relish status: {table}");

    let wtf = run_relish(&["--endpoint", &endpoint, "wtf"]);
    let wtf = String::from_utf8_lossy(&wtf.stdout);
    assert!(wtf.contains(&reason), "relish wtf: {wtf}");
}
