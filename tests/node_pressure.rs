//! Privileged Linux acceptance for the node-scoped pressure primitive.

#![cfg(target_os = "linux")]

use std::path::Path;

use reliaburger::smoker::node_pressure::{
    NODE_PRESSURE_CGROUP_ROOT, NodePressureController, NodePressureLimits, memory_bytes_to_target,
    node_cpu_max, node_memory_max, parse_linux_meminfo,
};
use reliaburger::smoker::types::FaultId;

fn pressure_target() -> (u8, u64) {
    let meminfo = std::fs::read_to_string("/proc/meminfo").expect("read meminfo");
    let (total, available) = parse_linux_meminfo(&meminfo).expect("parse meminfo");
    let used = total.saturating_sub(available);
    let used_percentage_ceiling = used
        .saturating_mul(100)
        .saturating_add(total.saturating_sub(1))
        / total.max(1);
    let target = u8::try_from(used_percentage_ceiling.saturating_add(1).min(90)).unwrap();
    (target, memory_bytes_to_target(total, available, target))
}

#[tokio::test]
#[ignore = "requires rootful Linux cgroup v2 (RELIABURGER_NODE_PRESSURE_TESTS=1)"]
async fn node_pressure_consumes_capacity_outside_bun_and_cleans_up() {
    if std::env::var("RELIABURGER_NODE_PRESSURE_TESTS").as_deref() != Ok("1") {
        eprintln!("skipped: set RELIABURGER_NODE_PRESSURE_TESTS=1");
        return;
    }

    let (memory_percentage, expected_memory_bytes) = pressure_target();
    eprintln!(
        "requesting {memory_percentage}% memory pressure (initial delta {expected_memory_bytes} bytes)"
    );
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

    let id = FaultId(42_424);
    controller
        .apply(id, 5, memory_percentage)
        .await
        .expect("apply node pressure");
    let cgroup = Path::new(NODE_PRESSURE_CGROUP_ROOT).join(id.to_string());
    assert!(cgroup.exists());
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
    let (total, available) = parse_linux_meminfo(&meminfo).expect("parse meminfo after pressure");
    let target_bytes = node_memory_max(total, memory_percentage);
    assert_eq!(
        std::fs::read_to_string(cgroup.join("memory.max"))
            .unwrap()
            .trim()
            .parse::<u64>()
            .unwrap(),
        target_bytes
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

    let used_after = total.saturating_sub(available);
    let measurement_tolerance = 32 * 1024 * 1024;
    assert!(
        used_after.saturating_add(measurement_tolerance) >= target_bytes,
        "node uses {used_after} bytes, below the {target_bytes}-byte target"
    );
    if expected_memory_bytes >= 32 * 1024 * 1024 {
        let helper_memory = std::fs::read_to_string(cgroup.join("memory.current"))
            .unwrap()
            .trim()
            .parse::<u64>()
            .unwrap();
        assert!(
            helper_memory >= 16 * 1024 * 1024,
            "helper cgroup made only {helper_memory} bytes resident"
        );
    }

    controller.clear(id).await.expect("clear node pressure");
    assert!(!cgroup.exists(), "clear must remove the owned cgroup");

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
}
