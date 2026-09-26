//! Privileged Linux acceptance for the node-scoped pressure primitive.

#![cfg(target_os = "linux")]

use std::path::Path;

use reliaburger::smoker::node_pressure::{
    NODE_PRESSURE_CGROUP_ROOT, NodePressureController, NodePressureLimits, memory_bytes_to_target,
    node_cpu_max, node_memory_max, parse_linux_meminfo,
};
use reliaburger::smoker::types::FaultId;

/// A memory-usage target a few points above the node's current usage.
fn pressure_target() -> u8 {
    let meminfo = std::fs::read_to_string("/proc/meminfo").expect("read meminfo");
    let (total, available) = parse_linux_meminfo(&meminfo).expect("parse meminfo");
    let used = total.saturating_sub(available);
    let used_percentage_ceiling = used
        .saturating_mul(100)
        .saturating_add(total.saturating_sub(1))
        / total.max(1);
    // Target three points above current usage, not one: the delta must dwarf
    // the tens of megabytes a live host reclaims and frees on its own, or the
    // resident-ballast assertion measures noise instead of the helper.
    u8::try_from(used_percentage_ceiling.saturating_add(3).min(90)).unwrap()
}

#[tokio::test]
#[ignore = "requires rootful Linux cgroup v2 (RELIABURGER_NODE_PRESSURE_TESTS=1)"]
async fn node_pressure_consumes_capacity_outside_bun_and_cleans_up() {
    if std::env::var("RELIABURGER_NODE_PRESSURE_TESTS").as_deref() != Ok("1") {
        eprintln!("skipped: set RELIABURGER_NODE_PRESSURE_TESTS=1");
        return;
    }

    // This binary runs with every nextest thread reserved, but the suite
    // that just finished is still being torn down: the kernel reclaims the
    // exited processes' memory for a while. Let usage settle before
    // snapshotting the target, or the baseline is a moving number.
    tokio::time::sleep(std::time::Duration::from_secs(2)).await;

    let memory_percentage = pressure_target();
    let limits = NodePressureLimits {
        max_cpu_percentage: 5,
        max_memory_percentage: memory_percentage,
    };
    let helper = std::env::var_os("RELIABURGER_BUN_BINARY")
        .map(std::path::PathBuf::from)
        .expect("RELIABURGER_BUN_BINARY must name the Linux bun under test");
    let mut controller = NodePressureController::default();
    assert!(
        controller.configure(limits, helper.clone()),
        "rootful cgroup-v2 controller should be available"
    );

    // Snapshot the delta right before apply, so only the milliseconds until
    // the helper's own MemAvailable read separate the two measurements.
    let meminfo = std::fs::read_to_string("/proc/meminfo").expect("read meminfo");
    let (total, available) = parse_linux_meminfo(&meminfo).expect("parse meminfo");
    let expected_memory_bytes = memory_bytes_to_target(total, available, memory_percentage);
    eprintln!(
        "requesting {memory_percentage}% memory pressure (delta {expected_memory_bytes} bytes)"
    );
    let id = FaultId(42_424);
    controller
        .apply(id, 5, memory_percentage)
        .await
        .expect("apply node pressure");
    let cgroup = Path::new(NODE_PRESSURE_CGROUP_ROOT).join(id.to_string());
    assert!(cgroup.exists());
    assert!(controller.confirm_no_helpers().await.is_err());
    // A new process must inspect kernel ownership, not its empty in-memory map.
    assert!(
        NodePressureController::default()
            .confirm_no_helpers()
            .await
            .is_err()
    );
    assert_eq!(
        std::fs::read_to_string(cgroup.join("cpu.max"))
            .unwrap()
            .trim(),
        node_cpu_max(
            5,
            std::thread::available_parallelism()
                .map(|cores| cores.get() as u32)
                .unwrap_or(1)
        )
    );
    let meminfo = std::fs::read_to_string("/proc/meminfo").expect("read meminfo after pressure");
    let (total, _) = parse_linux_meminfo(&meminfo).expect("parse meminfo after pressure");
    let target_bytes = node_memory_max(total, memory_percentage);
    // Hosted runners use memory-balloon drivers, so MemTotal can drift a few
    // kilobytes between the controller's read and this one. The ceiling only
    // has to match the recomputed target within that noise.
    let memory_ceiling = std::fs::read_to_string(cgroup.join("memory.max"))
        .unwrap()
        .trim()
        .parse::<u64>()
        .unwrap();
    assert!(
        memory_ceiling.abs_diff(target_bytes) <= 1024 * 1024,
        "memory.max {memory_ceiling} deviates from the {target_bytes}-byte target by over 1 MiB"
    );

    let helper_pid = std::fs::read_to_string(cgroup.join("cgroup.procs"))
        .unwrap()
        .lines()
        .next()
        .expect("helper pid")
        .parse::<u32>()
        .unwrap();
    assert_ne!(helper_pid, std::process::id());
    let helper_membership =
        std::fs::read_to_string(format!("/proc/{helper_pid}/cgroup")).expect("helper cgroup");
    assert!(helper_membership.contains("reliaburger-chaos/fault-42424"));
    let parent_membership = std::fs::read_to_string("/proc/self/cgroup").unwrap();
    assert!(!parent_membership.contains("reliaburger-chaos/fault-42424"));

    // The helper sizes its ballast once, from the node's usage at the moment
    // it joins the cgroup; it does not chase the node-wide figure afterwards.
    // Other processes on a shared runner keep allocating and freeing (the
    // previous suite's teardown can hand back 100 MB after the helper has
    // read MemAvailable), so `MemTotal - MemAvailable` measured here is not
    // what the controller promises. What it does promise is that the helper
    // cgroup holds the delta it was asked for, resident and charged. The
    // helper's read happens milliseconds after ours, so the tolerance only
    // covers background growth inside that window plus the helper's own
    // pre-join footprint.
    let measurement_tolerance = 64 * 1024 * 1024;
    let helper_memory = std::fs::read_to_string(cgroup.join("memory.current"))
        .unwrap()
        .trim()
        .parse::<u64>()
        .unwrap();
    assert!(
        helper_memory.saturating_add(measurement_tolerance) >= expected_memory_bytes,
        "helper cgroup holds {helper_memory} bytes, below the requested \
         {expected_memory_bytes}-byte delta"
    );
    assert!(
        helper_memory <= memory_ceiling,
        "helper cgroup holds {helper_memory} bytes, above its {memory_ceiling}-byte ceiling"
    );

    controller.clear(id).await.expect("clear node pressure");
    assert!(!cgroup.exists(), "clear must remove the owned cgroup");
    controller.confirm_no_helpers().await.unwrap();

    // Dropping an owner sends SIGKILL through Child::kill_on_drop. A fresh
    // controller then sweeps the now-empty stale cgroup, modelling the
    // restart path after Bun disappears between apply and clear.
    let stale_id = FaultId(42_425);
    controller
        .apply(stale_id, 1, 0)
        .await
        .expect("apply pressure for crash cleanup");
    let stale_cgroup = Path::new(NODE_PRESSURE_CGROUP_ROOT).join(stale_id.to_string());
    drop(controller);
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    let mut restarted = NodePressureController::default();
    assert!(restarted.configure(limits, helper));
    assert!(
        !stale_cgroup.exists(),
        "startup sweep must remove a previous owner's cgroup"
    );
    restarted.confirm_no_helpers().await.unwrap();
}

#[tokio::test]
#[ignore = "requires rootful Linux cgroup v2 (RELIABURGER_NODE_PRESSURE_TESTS=1)"]
async fn disabling_pressure_still_reclaims_previous_helpers() {
    assert_eq!(
        std::env::var("RELIABURGER_NODE_PRESSURE_TESTS").as_deref(),
        Ok("1")
    );
    let root = Path::new(NODE_PRESSURE_CGROUP_ROOT);
    std::fs::create_dir_all(root).unwrap();
    let cgroup = root.join(format!("fault-{}", std::process::id()));
    std::fs::create_dir(&cgroup).unwrap();
    let mut child = tokio::process::Command::new("sleep")
        .arg("60")
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    std::fs::write(cgroup.join("cgroup.procs"), child.id().unwrap().to_string()).unwrap();
    let mut controller = NodePressureController::default();
    let available = controller.configure(NodePressureLimits::default(), "/unused-helper".into());
    let removed = !cgroup.exists();
    let exited = tokio::time::timeout(std::time::Duration::from_secs(1), child.wait())
        .await
        .is_ok();
    // Keep a failing pre-fix regression from leaving its own pressure fixture.
    if !exited {
        child.kill().await.unwrap();
    }
    if cgroup.exists() {
        std::fs::remove_dir(&cgroup).unwrap();
    }
    assert!(
        removed,
        "disabled policy left the previous owner's cgroup behind"
    );
    assert!(exited, "disabled policy left the previous helper running");
    assert!(!available, "cleanup must not enable new pressure requests");
    assert!(controller.apply(FaultId(1), 1, 0).await.is_err());
}

#[tokio::test]
#[ignore = "requires rootful Linux cgroup v2 (RELIABURGER_NODE_PRESSURE_TESTS=1)"]
async fn noisy_helpers_do_not_block_readiness_or_erase_failure_diagnostics() {
    use std::os::unix::fs::PermissionsExt;
    use std::time::{Duration, Instant};

    assert_eq!(
        std::env::var("RELIABURGER_NODE_PRESSURE_TESTS").as_deref(),
        Ok("1")
    );
    let directory = tempfile::tempdir().unwrap();
    let helper = directory.path().join("helper.py");
    for (index, source, succeeds) in [
        (
            0,
            "import os, time\nos.write(2, b'prefix:' + b'x' * 262144)\nprint('ready', flush=True)\ntime.sleep(30)\n",
            true,
        ),
        (
            1,
            "import os, time\nos.write(2, b'prefix: failed allocation')\nprint('failed', flush=True)\ntime.sleep(30)\n",
            false,
        ),
        (
            2,
            "import os, time\nos.write(2, b'prefix:' + b'x' * 262144)\ntime.sleep(30)\n",
            false,
        ),
    ] {
        std::fs::write(&helper, format!("#!/usr/bin/python3\n{source}")).unwrap();
        std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut controller = NodePressureController::default();
        assert!(controller.configure(
            NodePressureLimits {
                max_cpu_percentage: 1,
                max_memory_percentage: 0,
            },
            helper.clone()
        ));
        let id = FaultId(42_500 + index);
        let started = Instant::now();
        let outcome = controller.apply(id, 1, 0).await;
        controller.clear(id).await.unwrap();
        controller.confirm_no_helpers().await.unwrap();
        assert!(
            !Path::new(NODE_PRESSURE_CGROUP_ROOT)
                .join(id.to_string())
                .exists()
        );
        assert!(started.elapsed() < Duration::from_secs(7));
        if succeeds {
            assert!(
                outcome.is_ok(),
                "stderr blocked successful startup: {outcome:?}"
            );
        } else {
            let error = outcome.unwrap_err();
            assert!(
                error.contains("prefix:"),
                "lost the helper diagnostic: {error}"
            );
            assert!(error.len() < 12_000, "diagnostic capture must be bounded");
            if index == 2 {
                assert!(error.contains("within 4 seconds"));
                assert!(error.contains("truncated"));
            }
        }
    }
}

#[tokio::test]
#[ignore = "requires rootful Linux cgroup v2 (RELIABURGER_NODE_PRESSURE_TESTS=1)"]
async fn helper_dies_with_creating_thread_or_parent_and_stale_cgroup_is_reclaimed() {
    use std::time::Duration;

    assert_eq!(
        std::env::var("RELIABURGER_NODE_PRESSURE_TESTS").as_deref(),
        Ok("1")
    );
    let binary = std::env::var("RELIABURGER_BUN_BINARY").unwrap();
    let script = r#"
import os, subprocess, sys, threading
child = None
def start():
    global child
    command = [sys.argv[1], '__node-pressure-helper',
        '--cgroup', sys.argv[2], '--parent-pid', str(os.getpid()),
        '--parent-tid', str(threading.get_native_id()),
        '--memory-percentage', '0', '--cpu-workers', '0']
    if sys.argv[3] == 'before':
        command = ['/bin/sh', '-c', 'sleep 0.25; exec "$@"', 'delayed-helper'] + command
    child = subprocess.Popen(command,
        stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
    if sys.argv[3] == 'before':
        return
    if child.stdout.readline().strip() != 'ready':
        print(child.stderr.read(), file=sys.stderr)
        os._exit(2)
if sys.argv[3] in ('thread', 'before'):
    thread = threading.Thread(target=start)
    thread.start()
    thread.join()
    try:
        result = child.wait(timeout=2)
    except subprocess.TimeoutExpired:
        child.kill()
        child.wait()
        raise
    if sys.argv[3] == 'before':
        assert result != 0, result
        assert 'lost its Bun parent thread' in child.stderr.read()
    else:
        assert result == -9, result
    # This parent process remains alive to observe its creator thread's death.
else:
    start()
    os._exit(0)
"#;
    for (index, mode) in ["thread", "process", "before"].iter().enumerate() {
        let mut controller = NodePressureController::default();
        assert!(controller.configure(
            NodePressureLimits {
                max_cpu_percentage: 1,
                max_memory_percentage: 0,
            },
            binary.clone().into()
        ));
        let cgroup =
            Path::new(NODE_PRESSURE_CGROUP_ROOT).join(FaultId(42_600 + index as u64).to_string());
        std::fs::create_dir(&cgroup).unwrap();
        let result = tokio::time::timeout(
            Duration::from_secs(10),
            tokio::process::Command::new("python3")
                .args(["-c", script, &binary])
                .arg(&cgroup)
                .arg(mode)
                .kill_on_drop(true)
                .output(),
        )
        .await
        .unwrap()
        .unwrap();
        assert!(
            result.status.success(),
            "{mode}: {}",
            String::from_utf8_lossy(&result.stderr)
        );
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if std::fs::read_to_string(cgroup.join("cgroup.procs"))
                    .unwrap()
                    .trim()
                    .is_empty()
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("parent-death signal must remove the pressure process");
        controller.configure(NodePressureLimits::default(), binary.clone().into());
        assert!(
            !cgroup.exists(),
            "startup must reclaim the former owner's directory"
        );
        controller.confirm_no_helpers().await.unwrap();
    }
}
