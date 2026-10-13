//! CPU, memory and pause faults against a real cgroup v2 tree, through
//! the agent: injected, reverted, and reverted again after the Bun that
//! injected them is gone (#699).
//!
//! Dropping an agent is what a SIGKILL leaves behind: its registry, with
//! every saved value and paused pid, disappears, and the workload and its
//! cgroup carry on. A second agent then adopts the instance from its record.

use super::*;

/// A private cgroup subtree standing in for the cgroup mount, with the CPU
/// and memory controllers enabled down to the workload's leaf.
struct CgroupTree {
    mount: std::path::PathBuf,
    leaf: std::path::PathBuf,
    children: Vec<std::process::Child>,
}

impl CgroupTree {
    fn new(label: &str) -> Self {
        assert!(
            std::env::var("RELIABURGER_CGROUP_TESTS").is_ok(),
            "run via `make test-linux` (needs RELIABURGER_CGROUP_TESTS=1)"
        );
        let mount = std::path::PathBuf::from(format!(
            "/sys/fs/cgroup/rbtest-chaos-{label}-{}",
            std::process::id()
        ));
        let mut level = mount.clone();
        std::fs::create_dir(&level).unwrap();
        for part in ["reliaburger", "default", "web", "0"] {
            std::fs::write(level.join("cgroup.subtree_control"), "+cpu +memory").unwrap();
            level = level.join(part);
            std::fs::create_dir(&level).unwrap();
        }
        Self {
            mount,
            leaf: level,
            children: Vec::new(),
        }
    }

    /// Start a process in the workload's cgroup, as its runtime would.
    fn spawn(&mut self) -> u32 {
        let child = std::process::Command::new("sleep")
            .arg("600")
            .spawn()
            .unwrap();
        let pid = child.id();
        std::fs::write(self.leaf.join("cgroup.procs"), pid.to_string()).unwrap();
        self.children.push(child);
        pid
    }

    fn read(&self, file: &str) -> String {
        std::fs::read_to_string(self.leaf.join(file))
            .unwrap()
            .trim()
            .to_string()
    }
}

impl Drop for CgroupTree {
    fn drop(&mut self) {
        for child in &mut self.children {
            let _ = child.kill();
            let _ = child.wait();
        }
        let mut level = self.leaf.clone();
        while level.starts_with(&self.mount) {
            let _ = std::fs::remove_dir(&level);
            if !level.pop() {
                break;
            }
        }
    }
}

/// An agent that adopts the `web` instance from `records`, its cgroup
/// under `tree`.
async fn adopting_agent(
    records: &std::path::Path,
    tree: &CgroupTree,
    pid: Option<u32>,
) -> (TestAgent, MockGrill) {
    let (mut agent, _tx, _shutdown, grill) = test_agent_with_grill();
    agent.set_records_dir(records.to_path_buf());
    agent.set_cgroup_mount(tree.mount.clone());
    grill.set_adopt_result(&InstanceId("default__web-0".to_string()), true);
    if let Some(pid) = pid {
        grill.set_pid(pid);
    }
    assert_eq!(agent.adopt_recorded_instances().await.unwrap(), 1);
    (agent, grill)
}

/// Record the `web` instance, and give its cgroup the limits runc would
/// from that record.
fn record_web(records: &std::path::Path, tree: &CgroupTree) {
    let record = limited_adoption_record("default__web-0", "web");
    crate::grill::records::write_record(records, &record).unwrap();
    std::fs::write(
        tree.leaf.join("memory.max"),
        (128u64 * 1024 * 1024).to_string(),
    )
    .unwrap();
}

/// Inject `fault_type` on `web` through the agent, expiring after `duration`.
async fn inject(
    agent: &mut BunAgent<MockGrill>,
    fault_type: crate::smoker::types::FaultType,
    duration: std::time::Duration,
) {
    let rule = agent
        .fault_registry
        .insert(&crate::smoker::types::FaultRequest {
            fault_type,
            target_service: "web".to_string(),
            namespace: Some("default".to_string()),
            target_instance: None,
            target_node: None,
            duration,
            injected_by: "test".to_string(),
            reason: None,
            include_leader: false,
            override_safety: false,
            acknowledged: true,
        });
    agent.apply_fault(&rule).await.unwrap();
}

/// Let every injected fault expire, which reverses it.
async fn expire(agent: &mut BunAgent<MockGrill>) {
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    agent.expire_faults().await;
    assert!(agent.fault_registry.is_empty());
}

#[tokio::test]
#[ignore = "requires Linux root and cgroup v2 (RELIABURGER_CGROUP_TESTS=1); run with make test-linux"]
async fn cpu_fault_round_trip_restores_cpu_max() {
    let tree = CgroupTree::new("cpu");
    let records = tempfile::tempdir().unwrap();
    record_web(records.path(), &tree);
    let (mut agent, _grill) = adopting_agent(records.path(), &tree, None).await;
    assert_eq!(tree.read("cpu.max"), "50000 100000");

    inject(
        &mut agent,
        crate::smoker::types::FaultType::CpuStress {
            percentage: 80,
            cores: Some(1),
        },
        std::time::Duration::from_millis(1),
    )
    .await;
    assert_eq!(tree.read("cpu.max"), "20000 100000");

    expire(&mut agent).await;
    assert_eq!(tree.read("cpu.max"), "50000 100000");
}

#[tokio::test]
#[ignore = "requires Linux root and cgroup v2 (RELIABURGER_CGROUP_TESTS=1); run with make test-linux"]
async fn memory_fault_round_trip_restores_memory_high() {
    let tree = CgroupTree::new("memory");
    let records = tempfile::tempdir().unwrap();
    record_web(records.path(), &tree);
    let (mut agent, _grill) = adopting_agent(records.path(), &tree, None).await;
    assert_eq!(tree.read("memory.high"), "33554432");

    inject(
        &mut agent,
        crate::smoker::types::FaultType::MemoryPressure { percentage: 50 },
        std::time::Duration::from_millis(1),
    )
    .await;
    assert_eq!(tree.read("memory.high"), (64u64 * 1024 * 1024).to_string());

    expire(&mut agent).await;
    assert_eq!(tree.read("memory.high"), "33554432");
}

#[tokio::test]
#[ignore = "requires Linux root and cgroup v2 (RELIABURGER_CGROUP_TESTS=1); run with make test-linux"]
async fn cpu_fault_is_reverted_after_bun_is_killed() {
    let tree = CgroupTree::new("cpu-killed");
    let records = tempfile::tempdir().unwrap();
    record_web(records.path(), &tree);
    let (mut killed, _grill) = adopting_agent(records.path(), &tree, None).await;
    inject(
        &mut killed,
        crate::smoker::types::FaultType::CpuStress {
            percentage: 99,
            cores: Some(1),
        },
        std::time::Duration::from_secs(3600),
    )
    .await;
    assert_eq!(tree.read("cpu.max"), "1000 100000");
    drop(killed);
    assert_eq!(
        tree.read("cpu.max"),
        "1000 100000",
        "the cap outlives its Bun"
    );

    let (_next, _grill) = adopting_agent(records.path(), &tree, None).await;

    assert_eq!(tree.read("cpu.max"), "50000 100000");
}

#[tokio::test]
#[ignore = "requires Linux root and cgroup v2 (RELIABURGER_CGROUP_TESTS=1); run with make test-linux"]
async fn paused_instance_resumes_after_bun_is_killed() {
    let mut tree = CgroupTree::new("pause-killed");
    let pid = tree.spawn();
    let records = tempfile::tempdir().unwrap();
    record_web(records.path(), &tree);
    let (mut killed, _grill) = adopting_agent(records.path(), &tree, Some(pid)).await;
    inject(
        &mut killed,
        crate::smoker::types::FaultType::Pause,
        std::time::Duration::from_secs(3600),
    )
    .await;
    wait_for_stopped(pid, true).await;
    drop(killed);
    assert!(process_is_stopped(pid), "the pause outlives its Bun");

    // The next Bun's runtime can't name the pid, so the cgroup must.
    let (_next, _grill) = adopting_agent(records.path(), &tree, None).await;

    wait_for_stopped(pid, false).await;
}
