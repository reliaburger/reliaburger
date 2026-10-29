//! The hollow runtime the hollow-node harness runs its fleet on (#1401).
//!
//! A hollow node's measurements are only as good as the fleet-wide running
//! count: the harness reads it to decide that every replica runs. These
//! tests pin how that count moves.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use reliaburger::grill::oci::{OciLinux, OciProcess, OciRoot, OciSpec, OciUser};
use reliaburger::grill::state::ContainerState;
use reliaburger::grill::{Grill, InstanceId};

use crate::hollow::{FleetCounters, HollowGrill, PORTS_PER_NODE, free_blocks};

fn instance(name: &str) -> InstanceId {
    InstanceId(name.to_string())
}

fn spec() -> OciSpec {
    OciSpec {
        reusable_executor: false,
        host_process: false,
        initialiser: false,
        root: OciRoot {
            path: "/".into(),
            readonly: true,
        },
        process: OciProcess {
            rlimits: Vec::new(),
            args: vec!["hollow".into()],
            env: Vec::new(),
            cwd: "/".into(),
            user: OciUser { uid: 0, gid: 0 },
            capabilities: None,
            oom_score_adj: None,
            overrides: None,
        },
        mounts: vec![],
        linux: OciLinux {
            namespaces: vec![],
            resources: None,
            cgroups_path: None,
            uid_mappings: None,
            gid_mappings: None,
            sysctl: Default::default(),
            devices: Vec::new(),
        },
        port_mapping: None,
        stop: None,
        runtime_knobs: None,
    }
}

#[tokio::test]
async fn a_started_workload_counts_as_running_until_it_stops() {
    let counters = Arc::new(FleetCounters::default());
    let grill = HollowGrill::new(Arc::clone(&counters));
    let web = instance("default/web-0");

    grill.create(&web, &spec()).await.unwrap();
    assert_eq!(counters.running.load(Ordering::Relaxed), 0);
    grill.start(&web).await.unwrap();
    assert_eq!(grill.state(&web).await.unwrap(), ContainerState::Running);
    assert_eq!(counters.running.load(Ordering::Relaxed), 1);

    grill.stop(&web).await.unwrap();
    assert_eq!(grill.state(&web).await.unwrap(), ContainerState::Stopped);
    assert_eq!(counters.running.load(Ordering::Relaxed), 0);
    assert_eq!(counters.starts.load(Ordering::Relaxed), 1);
}

#[tokio::test]
async fn recreating_a_running_instance_counts_it_once() {
    let counters = Arc::new(FleetCounters::default());
    let grill = HollowGrill::new(Arc::clone(&counters));
    let web = instance("default/web-0");

    grill.create(&web, &spec()).await.unwrap();
    grill.start(&web).await.unwrap();
    grill.create(&web, &spec()).await.unwrap();
    grill.start(&web).await.unwrap();
    grill.start(&web).await.unwrap();

    assert_eq!(counters.running.load(Ordering::Relaxed), 1);
    assert_eq!(grill.running().await, 1);
    grill.kill(&web).await.unwrap();
    grill.kill(&web).await.unwrap();
    assert_eq!(counters.running.load(Ordering::Relaxed), 0);
}

#[tokio::test]
async fn nodes_sharing_counters_add_up_to_the_fleet() {
    let counters = Arc::new(FleetCounters::default());
    let first = HollowGrill::new(Arc::clone(&counters));
    let second = HollowGrill::new(Arc::clone(&counters));
    for (grill, name) in [(&first, "default/web-0"), (&second, "default/web-1")] {
        grill.create(&instance(name), &spec()).await.unwrap();
        grill.start(&instance(name)).await.unwrap();
    }
    assert_eq!(counters.running.load(Ordering::Relaxed), 2);
    assert_eq!(first.running().await, 1);
}

#[test]
fn a_hollow_runtime_claims_containers_so_image_apps_place_on_it() {
    let grill = HollowGrill::new(Arc::default());
    assert!(grill.runs_containers());
}

#[test]
fn free_blocks_skips_a_block_another_socket_holds() {
    let held = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = held.local_addr().unwrap().port();
    // A range of three blocks whose first block contains the held port.
    let start = port.saturating_sub(1).max(1024);
    let blocks = free_blocks(2, start, start + 3 * PORTS_PER_NODE);
    assert!(
        blocks
            .iter()
            .all(|base| !(*base..*base + PORTS_PER_NODE).contains(&port)),
        "handed out a block containing a held port: {blocks:?}"
    );
}
