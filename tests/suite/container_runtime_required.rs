//! #914: an app that names an image needs a container runtime.
//!
//! A real Bun on `--runtime process` refuses an image app at launch, with a
//! reason that says it needs a container runtime, and `relish apply`,
//! `relish status` and `relish wtf` all show it. It doesn't run the app's
//! `command` on the host, or `sleep` in its place. A host `exec` app on the
//! same node still runs.

use crate::bun_process;
use bun_process::{
    reserve_address, run_relish, spawn_bun_with_port_retry, wait_for_relish,
    wait_for_relish_output, write_portable_node_config,
};

const REASON: &str =
    "needs a container runtime to run image nginx:1.27, and this node's runtime is process";

#[test]
fn an_image_app_is_refused_on_the_process_runtime_and_a_host_app_still_runs() {
    let root = tempfile::tempdir().unwrap();
    let (mut bun, address) = spawn_bun_with_port_retry(false, || {
        let config = write_portable_node_config(root.path());
        let mut text = std::fs::read_to_string(&config).unwrap();
        text.push_str(
            "\n[process_workloads]\nallowed_binaries = [\"/bin/sleep\"]\nmount_isolation = false\n",
        );
        std::fs::write(&config, text).unwrap();
        (config, reserve_address(), root.path().join("bun.log"))
    });
    let endpoint = format!("http://{address}");
    wait_for_relish(&mut bun, &["--endpoint", &endpoint, "status"]);

    // The command is one the host could run, so only the image refuses it.
    let image = root.path().join("web.toml");
    std::fs::write(
        &image,
        "[app.web]\nimage = \"nginx:1.27\"\ncommand = [\"/bin/sleep\", \"300\"]\n",
    )
    .unwrap();
    let apply = run_relish(&["--endpoint", &endpoint, "apply", image.to_str().unwrap()]);
    let stderr = String::from_utf8_lossy(&apply.stderr).to_string();
    assert!(
        !apply.status.success(),
        "the deploy must fail\nstdout={}\nstderr={stderr}",
        String::from_utf8_lossy(&apply.stdout)
    );
    assert!(
        stderr.contains("default__web-0") && stderr.contains(REASON),
        "the deploy must name the app and say it needs a container runtime\nstderr={stderr}\nbun log={}",
        bun_process::bun_log_diagnostic(&bun.log_path)
    );

    let status = wait_for_relish_output(
        &mut bun,
        &["--endpoint", &endpoint, "--output", "json", "status"],
        REASON,
    );
    let instances: serde_json::Value = serde_json::from_slice(&status.stdout).unwrap();
    let web = instances
        .as_array()
        .unwrap()
        .iter()
        .find(|instance| instance["app_name"] == "web")
        .unwrap_or_else(|| panic!("no web instance in {instances}"));
    assert_eq!(web["state"], "failed", "{web}");
    assert_eq!(web["reason"], REASON, "{web}");

    let wtf = run_relish(&["--endpoint", &endpoint, "wtf"]);
    let wtf = String::from_utf8_lossy(&wtf.stdout);
    assert!(wtf.contains(REASON), "relish wtf: {wtf}");

    // A host workload is what the process runtime is for.
    let host = root.path().join("tool.toml");
    std::fs::write(
        &host,
        "[app.tool]\nexec = \"/bin/sleep\"\ncommand = [\"300\"]\n",
    )
    .unwrap();
    let apply = run_relish(&["--endpoint", &endpoint, "apply", host.to_str().unwrap()]);
    assert!(
        apply.status.success(),
        "the exec app must deploy\nstdout={}\nstderr={}\nbun log={}",
        String::from_utf8_lossy(&apply.stdout),
        String::from_utf8_lossy(&apply.stderr),
        bun_process::bun_log_diagnostic(&bun.log_path)
    );
    let status = run_relish(&["--endpoint", &endpoint, "--output", "json", "status"]);
    let instances: serde_json::Value = serde_json::from_slice(&status.stdout).unwrap();
    let tool = instances
        .as_array()
        .unwrap()
        .iter()
        .find(|instance| instance["app_name"] == "tool")
        .unwrap_or_else(|| panic!("no tool instance in {instances}"));
    assert_eq!(tool["state"], "running", "{tool}");
}
