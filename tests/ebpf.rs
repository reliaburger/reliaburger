//! eBPF integration tests for Onion service discovery.
//!
//! These tests load real eBPF programs into the kernel, populate BPF
//! maps, and verify that connect() calls to VIPs are rewritten to
//! real backend addresses.
//!
//! Requirements: Linux 5.7+, root, cgroup v2, `--features ebpf`.
//! Gated behind `RELIABURGER_EBPF_TESTS=1`.
//!
//! Run via: `relish dev test ebpf`

#![cfg(target_os = "linux")]
#![cfg(feature = "ebpf")]

use std::net::{Ipv4Addr, SocketAddr, TcpStream};
use std::path::PathBuf;
use std::time::Duration;

use reliaburger::onion::ebpf::loader::OnionEbpf;
use reliaburger::onion::ebpf::maps::BpfServiceMap;
use reliaburger::onion::service_id::ServiceId;
use reliaburger::onion::service_map::ServiceMap;
use reliaburger::onion::types::BackendInstance;
use reliaburger::onion::vip::VirtualIP;
use tokio_util::sync::CancellationToken;

#[path = "support/task_harness.rs"]
mod task_harness;
use task_harness::TestTasks;

fn ebpf_tests_enabled() -> bool {
    std::env::var("RELIABURGER_EBPF_TESTS").is_ok()
}

/// Find the directory containing compiled .bpf.o files.
///
/// build.rs puts them in OUT_DIR, which is under the target directory.
/// We search for them relative to the current cargo target dir.
fn find_bpf_obj_dir() -> PathBuf {
    // The OUT_DIR from build.rs is something like:
    // target/debug/build/reliaburger-HASH/out/
    // We search for onion_connect.bpf.o under the target dir.
    let target_dir = std::env::var("CARGO_TARGET_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("target"));

    // Walk the build directory to find the .bpf.o file
    for entry in walkdir(&target_dir) {
        if entry.ends_with("onion_connect.bpf.o") {
            return entry.parent().unwrap().to_path_buf();
        }
    }

    panic!(
        "onion_connect.bpf.o not found under {}. Did you build with --features ebpf?",
        target_dir.display()
    );
}

/// Recursively walk a directory and return all file paths.
fn walkdir(dir: &std::path::Path) -> Vec<PathBuf> {
    let mut results = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                results.extend(walkdir(&path));
            } else {
                results.push(path);
            }
        }
    }
    results
}

const CGROUP_PATH: &str = "/sys/fs/cgroup";

// ---------------------------------------------------------------------------
// Tier 1: Load and map verification
// ---------------------------------------------------------------------------

#[test]
#[ignore = "requires Linux root and RELIABURGER_EBPF_TESTS=1"]
fn embedded_program_loads_without_an_object_directory() {
    assert!(ebpf_tests_enabled(), "set RELIABURGER_EBPF_TESTS=1");
    let loader = OnionEbpf::load_embedded(std::path::Path::new(CGROUP_PATH)).unwrap();
    assert!(loader.is_attached());
    for name in reliaburger::onion::ebpf::loader::REQUIRED_MAPS {
        assert!(loader.bpf.map(name).is_some(), "missing map {name}");
    }
}

#[tokio::test]
#[ignore = "requires Linux root and RELIABURGER_EBPF_TESTS=1"]
async fn ebpf_load_and_attach() {
    assert!(
        ebpf_tests_enabled(),
        "set RELIABURGER_EBPF_TESTS=1 after provisioning eBPF prerequisites"
    );

    let obj_dir = find_bpf_obj_dir();
    let mut ebpf =
        OnionEbpf::load(&obj_dir, CGROUP_PATH.as_ref()).expect("failed to load eBPF program");

    assert!(ebpf.is_attached());
    assert!(ebpf.connect6_attached());
    assert!(ebpf.sendmsg4_attached());
    assert!(ebpf.sendmsg6_attached());
    ebpf.detach().unwrap();
}

#[tokio::test]
#[ignore = "requires Linux root and RELIABURGER_EBPF_TESTS=1"]
async fn ebpf_backend_map_write_and_read() {
    assert!(
        ebpf_tests_enabled(),
        "set RELIABURGER_EBPF_TESTS=1 after provisioning eBPF prerequisites"
    );

    let obj_dir = find_bpf_obj_dir();
    let mut ebpf =
        OnionEbpf::load(&obj_dir, CGROUP_PATH.as_ref()).expect("failed to load eBPF program");

    let mut bpf_map = BpfServiceMap::new();

    // Create a service entry
    let mut svc_map = ServiceMap::new();
    svc_map
        .register_app("redis", "default", 6379, None)
        .unwrap();
    svc_map
        .add_backend(
            &ServiceId::new("default", "redis"),
            BackendInstance {
                instance_id: "redis-0".to_string(),
                node_ip: Ipv4Addr::new(10, 0, 2, 2),
                host_port: 30891,
                healthy: true,
            },
        )
        .unwrap();

    // Sync to BPF maps
    bpf_map
        .sync_from_service_map(&svc_map, &mut ebpf)
        .expect("sync to backend_map");

    // Read back
    let vip = VirtualIP::from_service_id(&ServiceId::new("default", "redis"));
    let value = bpf_map
        .read_backends(&mut ebpf, vip, 6379)
        .expect("backend_map read failed")
        .expect("entry not found in backend_map");

    assert_eq!(value.count, 1);
    assert_eq!(value.backends[0].healthy, 1);
    assert_eq!(
        value.backends[0].host_ip,
        u32::from(Ipv4Addr::new(10, 0, 2, 2)).to_be()
    );
    assert_eq!(value.backends[0].host_port, 30891u16.to_be());

    ebpf.detach().unwrap();
}

#[tokio::test]
#[ignore = "requires Linux root and RELIABURGER_EBPF_TESTS=1"]
async fn ebpf_backend_map_remove() {
    assert!(
        ebpf_tests_enabled(),
        "set RELIABURGER_EBPF_TESTS=1 after provisioning eBPF prerequisites"
    );

    let obj_dir = find_bpf_obj_dir();
    let mut ebpf =
        OnionEbpf::load(&obj_dir, CGROUP_PATH.as_ref()).expect("failed to load eBPF program");

    let mut bpf_map = BpfServiceMap::new();

    let mut svc_map = ServiceMap::new();
    svc_map
        .register_app("redis", "default", 6379, None)
        .unwrap();

    bpf_map
        .sync_from_service_map(&svc_map, &mut ebpf)
        .expect("sync to backend_map");

    let vip = VirtualIP::from_service_id(&ServiceId::new("default", "redis"));
    assert!(
        bpf_map
            .read_backends(&mut ebpf, vip, 6379)
            .unwrap()
            .is_some()
    );

    // Remove
    bpf_map
        .remove_backends_bpf(&mut ebpf, vip, 6379)
        .expect("remove backend entry");
    assert!(
        bpf_map
            .read_backends(&mut ebpf, vip, 6379)
            .unwrap()
            .is_none()
    );

    ebpf.detach().unwrap();
}

#[tokio::test]
#[ignore = "requires Linux root and RELIABURGER_EBPF_TESTS=1"]
async fn ebpf_service_map_sync_multiple() {
    assert!(
        ebpf_tests_enabled(),
        "set RELIABURGER_EBPF_TESTS=1 after provisioning eBPF prerequisites"
    );

    let obj_dir = find_bpf_obj_dir();
    let mut ebpf =
        OnionEbpf::load(&obj_dir, CGROUP_PATH.as_ref()).expect("failed to load eBPF program");

    let mut bpf_map = BpfServiceMap::new();
    let mut svc_map = ServiceMap::new();

    svc_map
        .register_app("redis", "default", 6379, None)
        .unwrap();
    svc_map.register_app("web", "default", 8080, None).unwrap();
    svc_map.register_app("api", "prod", 3000, None).unwrap();

    bpf_map
        .sync_from_service_map(&svc_map, &mut ebpf)
        .expect("sync to backend_map");

    // All three should be in the map
    assert!(
        bpf_map
            .read_backends(
                &mut ebpf,
                VirtualIP::from_service_id(&ServiceId::new("default", "redis")),
                6379
            )
            .unwrap()
            .is_some()
    );
    assert!(
        bpf_map
            .read_backends(
                &mut ebpf,
                VirtualIP::from_service_id(&ServiceId::new("default", "web")),
                8080
            )
            .unwrap()
            .is_some()
    );
    assert!(
        bpf_map
            .read_backends(
                &mut ebpf,
                VirtualIP::from_service_id(&ServiceId::new("prod", "api")),
                3000
            )
            .unwrap()
            .is_some()
    );

    ebpf.detach().unwrap();
}

// ---------------------------------------------------------------------------
// Tier 2: Connect rewrite verification
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires Linux root and RELIABURGER_EBPF_TESTS=1"]
async fn ebpf_connect_to_vip_rewrites_destination() {
    assert!(
        ebpf_tests_enabled(),
        "set RELIABURGER_EBPF_TESTS=1 after provisioning eBPF prerequisites"
    );

    let obj_dir = find_bpf_obj_dir();
    let mut ebpf =
        OnionEbpf::load(&obj_dir, CGROUP_PATH.as_ref()).expect("failed to load eBPF program");

    // Start a TCP listener on an ephemeral port
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let backend_port = listener.local_addr().unwrap().port();
    listener.set_nonblocking(false).unwrap();

    // Populate backend_map: VIP → our listener
    let vip = VirtualIP::from_service_id(&ServiceId::new("default", "test-service"));
    let service_port: u16 = 9999;

    let mut svc_map = ServiceMap::new();
    svc_map
        .register_app("test-service", "default", service_port, None)
        .unwrap();
    svc_map
        .add_backend(
            &ServiceId::new("default", "test-service"),
            BackendInstance {
                instance_id: "test-0".to_string(),
                node_ip: Ipv4Addr::LOCALHOST,
                host_port: backend_port,
                healthy: true,
            },
        )
        .unwrap();

    let mut bpf_map = BpfServiceMap::new();
    bpf_map
        .sync_from_service_map(&svc_map, &mut ebpf)
        .expect("sync to backend_map");

    // Connect to the VIP — the eBPF program should rewrite to our listener
    let vip_addr = SocketAddr::new(vip.0.into(), service_port);
    let connect_result = TcpStream::connect_timeout(&vip_addr, Duration::from_secs(2));

    match connect_result {
        Ok(_stream) => {
            // The connect succeeded, which means the kernel rewrote
            // the destination from the VIP to 127.0.0.1:{backend_port}
            // and the TCP handshake completed with our listener.
            eprintln!("connect rewrite verified: VIP {vip} → 127.0.0.1:{backend_port}");
        }
        Err(e) => {
            panic!(
                "connect to VIP {vip_addr} failed: {e}. The eBPF program may not have rewritten the address."
            );
        }
    }

    ebpf.detach().unwrap();
}

#[tokio::test]
#[ignore = "requires Linux root and RELIABURGER_EBPF_TESTS=1"]
async fn ebpf_connect_to_vip_no_backends_refused() {
    assert!(
        ebpf_tests_enabled(),
        "set RELIABURGER_EBPF_TESTS=1 after provisioning eBPF prerequisites"
    );

    let obj_dir = find_bpf_obj_dir();
    let mut ebpf =
        OnionEbpf::load(&obj_dir, CGROUP_PATH.as_ref()).expect("failed to load eBPF program");

    // Register a service with no backends
    let vip = VirtualIP::from_service_id(&ServiceId::new("default", "empty-service"));
    let mut svc_map = ServiceMap::new();
    svc_map
        .register_app("empty-service", "default", 7777, None)
        .unwrap();

    let mut bpf_map = BpfServiceMap::new();
    bpf_map
        .sync_from_service_map(&svc_map, &mut ebpf)
        .expect("sync to backend_map");

    // Connect to the VIP — should get ECONNREFUSED
    let vip_addr = SocketAddr::new(vip.0.into(), 7777);
    let result = TcpStream::connect_timeout(&vip_addr, Duration::from_secs(2));

    assert!(
        result.is_err(),
        "connect to VIP with no backends should fail"
    );
    // The BPF connect4 hook returns 0 to deny, which the kernel
    // translates to EPERM (not ECONNREFUSED as you might expect).
    let err = result.unwrap_err();
    assert_eq!(
        err.kind(),
        std::io::ErrorKind::PermissionDenied,
        "expected EPERM from BPF deny, got: {err}"
    );

    ebpf.detach().unwrap();
}

#[tokio::test]
#[ignore = "requires Linux root and RELIABURGER_EBPF_TESTS=1"]
async fn ebpf_connect_non_vip_passes_through() {
    assert!(
        ebpf_tests_enabled(),
        "set RELIABURGER_EBPF_TESTS=1 after provisioning eBPF prerequisites"
    );

    let obj_dir = find_bpf_obj_dir();
    let mut ebpf =
        OnionEbpf::load(&obj_dir, CGROUP_PATH.as_ref()).expect("failed to load eBPF program");

    // Start a listener on localhost (not a VIP)
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();

    // Connect directly — should NOT be rewritten
    let addr = SocketAddr::new(Ipv4Addr::LOCALHOST.into(), port);
    let result = TcpStream::connect_timeout(&addr, Duration::from_secs(2));
    assert!(
        result.is_ok(),
        "non-VIP connect should pass through: {:?}",
        result.err()
    );

    ebpf.detach().unwrap();
}

// ---------------------------------------------------------------------------
// Tier 2a: Agent-driven backend_map sync (L8 completeness)
// ---------------------------------------------------------------------------

/// A real `BunAgent` with a loaded eBPF handle must mirror a deployed
/// app's backends into the kernel `backend_map` — the production path,
/// not a manual `BpfServiceMap` write. Deploy through the agent command
/// channel and read the map back.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires Linux root and RELIABURGER_EBPF_TESTS=1"]
async fn agent_deploy_populates_backend_map() {
    use reliaburger::bun::agent::{AgentCommand, BunAgent};
    use reliaburger::config::Config;
    use reliaburger::grill::port::PortAllocator;
    use reliaburger::grill::process::ProcessGrill;
    use std::sync::Arc;
    use tokio::sync::{Mutex, mpsc};
    use tokio_util::sync::CancellationToken;

    assert!(
        ebpf_tests_enabled(),
        "set RELIABURGER_EBPF_TESTS=1 after provisioning eBPF prerequisites"
    );

    let obj_dir = find_bpf_obj_dir();
    // Keep our own clone of the handle to read the map after the deploy.
    let ebpf = Arc::new(Mutex::new(
        OnionEbpf::load(&obj_dir, CGROUP_PATH.as_ref()).expect("failed to load eBPF program"),
    ));

    let (cmd_tx, cmd_rx) = mpsc::channel(64);
    let shutdown = CancellationToken::new();
    let mut agent = BunAgent::new(
        ProcessGrill::new(),
        PortAllocator::new(41100, 41400),
        cmd_rx,
        shutdown.clone(),
    );
    agent.set_onion_ebpf(Arc::clone(&ebpf)).await;
    let agent_task = tokio::spawn(async move { agent.run().await });
    let _tasks = TestTasks::new(shutdown.clone(), vec![agent_task]);

    let config = Config::parse(
        r#"
        [app.web]
        image = "proc-grill:image-ignored"
        command = ["sleep", "600"]
        port = 8080
    "#,
    )
    .unwrap();
    let (ev_tx, mut ev_rx) = mpsc::channel(64);
    cmd_tx
        .send(AgentCommand::Deploy {
            config,
            events: ev_tx,
        })
        .await
        .unwrap();
    while ev_rx.recv().await.is_some() {}

    // Let the instance reach Running and register its backend.
    let vip = VirtualIP::from_service_id(&ServiceId::new("default", "web"));
    let bpf = BpfServiceMap::new();
    let populated = {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        let mut ok = false;
        while tokio::time::Instant::now() < deadline {
            let has_backend = {
                let mut e = ebpf.lock().await;
                bpf.read_backends(&mut e, vip, 8080)
                    .unwrap()
                    .is_some_and(|v| v.count >= 1)
            };
            if has_backend {
                ok = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        ok
    };
    assert!(
        populated,
        "agent never mirrored the deployed app's backend into backend_map"
    );

    shutdown.cancel();
}

/// A `Drop` fault injected through the agent must land in the kernel
/// `fault_connect_map` and refuse connections to the service VIP with
/// EPERM — the eBPF fault path, driven end to end (P2). Before the fault
/// the VIP has a backend, so a connect is *rewritten* (and refused by the
/// absent listener with ECONNREFUSED, not EPERM); after the fault it is
/// dropped (EPERM) regardless of backends.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires Linux root and RELIABURGER_EBPF_TESTS=1"]
async fn agent_drop_fault_refuses_vip_with_eperm() {
    use reliaburger::bun::agent::{AgentCommand, BunAgent};
    use reliaburger::config::Config;
    use reliaburger::grill::port::PortAllocator;
    use reliaburger::grill::process::ProcessGrill;
    use reliaburger::smoker::types::{FaultRequest, FaultType};
    use std::io::ErrorKind;
    use std::sync::Arc;
    use tokio::sync::{Mutex, mpsc, oneshot};
    use tokio_util::sync::CancellationToken;

    assert!(
        ebpf_tests_enabled(),
        "set RELIABURGER_EBPF_TESTS=1 after provisioning eBPF prerequisites"
    );

    let obj_dir = find_bpf_obj_dir();
    let ebpf = Arc::new(Mutex::new(
        OnionEbpf::load(&obj_dir, CGROUP_PATH.as_ref()).expect("failed to load eBPF program"),
    ));

    let (cmd_tx, cmd_rx) = mpsc::channel(64);
    let shutdown = CancellationToken::new();
    let mut agent = BunAgent::new(
        ProcessGrill::new(),
        PortAllocator::new(41500, 41800),
        cmd_rx,
        shutdown.clone(),
    );
    agent.set_onion_ebpf(Arc::clone(&ebpf)).await;
    let agent_task = tokio::spawn(async move { agent.run().await });
    let _tasks = TestTasks::new(shutdown.clone(), vec![agent_task]);

    let service_port: u16 = 8090;
    let config = Config::parse(&format!(
        r#"
        [app.faulty]
        image = "proc-grill:image-ignored"
        command = ["sleep", "600"]
        port = {service_port}
    "#
    ))
    .unwrap();
    let (ev_tx, mut ev_rx) = mpsc::channel(64);
    cmd_tx
        .send(AgentCommand::Deploy {
            config,
            events: ev_tx,
        })
        .await
        .unwrap();
    while ev_rx.recv().await.is_some() {}

    let vip = VirtualIP::from_service_id(&ServiceId::new("default", "faulty"));
    let bpf = BpfServiceMap::new();
    // Wait for the backend to register so the VIP resolves to a real entry.
    for _ in 0..25 {
        let has = {
            let mut e = ebpf.lock().await;
            bpf.read_backends(&mut e, vip, service_port)
                .unwrap()
                .is_some_and(|v| v.count >= 1)
        };
        if has {
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    let vip_addr = SocketAddr::new(vip.0.into(), service_port);

    // Control: with a backend but no fault, the connect is rewritten to the
    // (non-listening) backend → ECONNREFUSED, never EPERM.
    let before = TcpStream::connect_timeout(&vip_addr, Duration::from_secs(1));
    assert_ne!(
        before.as_ref().err().map(|e| e.kind()),
        Some(ErrorKind::PermissionDenied),
        "VIP should not be dropped before the fault"
    );

    // Inject a 100% Drop fault on the service.
    let (resp_tx, resp_rx) = oneshot::channel();
    cmd_tx
        .send(AgentCommand::InjectFault {
            reservation: None,
            request: FaultRequest {
                fault_type: FaultType::Drop { probability: 100 },
                target_service: "faulty".into(),
                namespace: None,
                target_instance: None,
                target_node: None,
                duration: Duration::from_secs(30),
                injected_by: "test".into(),
                reason: None,
                include_leader: false,
                override_safety: false,
                acknowledged: false,
            },
            response: resp_tx,
        })
        .await
        .unwrap();
    resp_rx
        .await
        .unwrap()
        .expect("drop fault should be accepted");

    // Now the VIP is dropped before any rewrite → EPERM.
    let dropped = {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        let mut ok = false;
        while tokio::time::Instant::now() < deadline {
            let r = TcpStream::connect_timeout(&vip_addr, Duration::from_secs(1));
            if r.as_ref().err().map(|e| e.kind()) == Some(ErrorKind::PermissionDenied) {
                ok = true;
                break;
            }
            tokio::time::sleep(Duration::from_millis(150)).await;
        }
        ok
    };
    assert!(
        dropped,
        "Drop fault did not refuse the VIP with EPERM (fault_connect_map not applied)"
    );

    shutdown.cancel();
}

/// A service partition is keyed by the source cgroup, not merely by the
/// destination VIP. Program the current test cgroup as the source and prove
/// the connect hook refuses the VIP, then delete the exact key and prove the
/// refusal disappears.
#[tokio::test]
#[ignore = "requires Linux root and RELIABURGER_EBPF_TESTS=1"]
async fn partition_fault_blocks_its_source_cgroup_and_clears() {
    use reliaburger::sesame::egress::cgroup_id_of_pid;
    use reliaburger::smoker::bpf_maps;
    use reliaburger::smoker::bpf_types::{
        BpfConnectFaultValue, FAULT_ACTION_PARTITION, partition_fault_key,
    };
    use std::io::ErrorKind;

    assert!(
        ebpf_tests_enabled(),
        "set RELIABURGER_EBPF_TESTS=1 after provisioning eBPF prerequisites"
    );

    let obj_dir = find_bpf_obj_dir();
    let mut ebpf =
        OnionEbpf::load(&obj_dir, CGROUP_PATH.as_ref()).expect("failed to load eBPF program");
    let vip = VirtualIP::from_service_id(&ServiceId::new("default", "partition-target"));
    let port = 18_080u16;
    let address = SocketAddr::new(vip.0.into(), port);
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind test backend");
    let mut service_map = ServiceMap::new();
    service_map
        .register_app("partition-target", "default", port, None)
        .expect("register partition target");
    service_map
        .add_backend(
            &ServiceId::new("default", "partition-target"),
            BackendInstance {
                instance_id: "partition-target-0".to_string(),
                node_ip: Ipv4Addr::LOCALHOST,
                host_port: listener.local_addr().expect("backend address").port(),
                healthy: true,
            },
        )
        .expect("register test backend");
    BpfServiceMap::new()
        .sync_from_service_map(&service_map, &mut ebpf)
        .expect("publish partition target");
    let source_cgroup_id =
        cgroup_id_of_pid(std::process::id()).expect("resolve the test source cgroup");
    let key = partition_fault_key(vip.to_network_byte_order(), port.to_be(), source_cgroup_id);
    let value = BpfConnectFaultValue {
        action: FAULT_ACTION_PARTITION,
        probability: 100,
        _pad: [0; 6],
        delay_ns: 0,
        jitter_ns: 0,
        expires_ns: 0,
    };

    assert_ne!(
        TcpStream::connect_timeout(&address, Duration::from_secs(1))
            .as_ref()
            .err()
            .map(std::io::Error::kind),
        Some(ErrorKind::PermissionDenied),
        "VIP must not be denied before the partition key exists"
    );
    let other_source_key = partition_fault_key(
        vip.to_network_byte_order(),
        port.to_be(),
        source_cgroup_id.wrapping_add(1),
    );
    bpf_maps::write_connect_fault(&mut ebpf.bpf, other_source_key, value)
        .expect("install a key for a different source cgroup");
    assert_ne!(
        TcpStream::connect_timeout(&address, Duration::from_secs(1))
            .as_ref()
            .err()
            .map(std::io::Error::kind),
        Some(ErrorKind::PermissionDenied),
        "a partition for another source cgroup blocked this caller"
    );
    bpf_maps::delete_connect_fault(&mut ebpf.bpf, &other_source_key)
        .expect("delete the different-source control key");

    bpf_maps::write_connect_fault(&mut ebpf.bpf, key, value)
        .expect("install source-scoped partition key");
    assert_eq!(
        TcpStream::connect_timeout(&address, Duration::from_secs(1))
            .as_ref()
            .err()
            .map(std::io::Error::kind),
        Some(ErrorKind::PermissionDenied),
        "partition key did not block its source cgroup"
    );

    bpf_maps::delete_connect_fault(&mut ebpf.bpf, &key).expect("delete partition key");
    assert_ne!(
        TcpStream::connect_timeout(&address, Duration::from_secs(1))
            .as_ref()
            .err()
            .map(std::io::Error::kind),
        Some(ErrorKind::PermissionDenied),
        "source remained partitioned after deleting the owned key"
    );
    ebpf.detach().unwrap();
}

// ---------------------------------------------------------------------------
// Tier 2b: Egress allowlist (L16)
// ---------------------------------------------------------------------------

/// With egress enforcement on for a cgroup, a listed destination is
/// reachable and every other destination is denied (EPERM). We enforce
/// on the *test's own* cgroup — the process doing the connects — so the
/// eBPF `bpf_get_current_cgroup_id()` matches what we program.
#[tokio::test]
#[ignore = "requires Linux root and RELIABURGER_EBPF_TESTS=1"]
async fn egress_denied_by_default_allowed_when_listed() {
    use reliaburger::sesame::egress::{self, EGRESS_ALLOW, EgressKey, EgressValue};

    assert!(
        ebpf_tests_enabled(),
        "set RELIABURGER_EBPF_TESTS=1 after provisioning eBPF prerequisites"
    );

    let obj_dir = find_bpf_obj_dir();
    let mut ebpf =
        OnionEbpf::load(&obj_dir, CGROUP_PATH.as_ref()).expect("failed to load eBPF program");

    // Two real listeners: one we'll allow, one we won't.
    let allowed = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let allowed_port = allowed.local_addr().unwrap().port();
    let denied = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let denied_port = denied.local_addr().unwrap().port();

    let cgroup_id =
        egress::cgroup_id_of_pid(std::process::id()).expect("failed to resolve own cgroup id");

    // Allow only 127.0.0.1:allowed_port for our cgroup, then enforce.
    let key = EgressKey {
        src_cgroup_id: cgroup_id,
        dst_ip: u32::from(Ipv4Addr::LOCALHOST).to_be(),
        dst_port: allowed_port.to_be(),
        _pad: 0,
    };
    egress::write_egress_entry(
        &mut ebpf.bpf,
        key,
        EgressValue {
            action: EGRESS_ALLOW,
        },
    )
    .expect("write egress entry");
    egress::set_egress_enforced(&mut ebpf.bpf, cgroup_id).expect("enable egress enforcement");

    // The allowed destination connects.
    let ok = TcpStream::connect_timeout(
        &SocketAddr::new(Ipv4Addr::LOCALHOST.into(), allowed_port),
        Duration::from_secs(2),
    );
    assert!(
        ok.is_ok(),
        "listed egress destination should connect: {ok:?}"
    );

    // The unlisted destination is denied with EPERM (BPF returns 0).
    let blocked = TcpStream::connect_timeout(
        &SocketAddr::new(Ipv4Addr::LOCALHOST.into(), denied_port),
        Duration::from_secs(2),
    );
    assert!(
        matches!(
            blocked.as_ref().map_err(|e| e.kind()),
            Err(std::io::ErrorKind::PermissionDenied)
        ),
        "unlisted egress destination should be denied with EPERM, got {blocked:?}"
    );

    // Unconnected UDP uses sendmsg()/sendto(), not connect(). The matching
    // cgroup hooks must enforce the same policy or UDP walks around it.
    let udp_allowed = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let udp_allowed_port = udp_allowed.local_addr().unwrap().port();
    let udp_denied = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    let udp_denied_port = udp_denied.local_addr().unwrap().port();
    let udp_key = EgressKey {
        src_cgroup_id: cgroup_id,
        dst_ip: u32::from(Ipv4Addr::LOCALHOST).to_be(),
        dst_port: udp_allowed_port.to_be(),
        _pad: 0,
    };
    egress::write_egress_entry(
        &mut ebpf.bpf,
        udp_key,
        EgressValue {
            action: EGRESS_ALLOW,
        },
    )
    .unwrap();
    let sender = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
    assert_eq!(
        sender
            .send_to(&[1], (Ipv4Addr::LOCALHOST, udp_allowed_port))
            .unwrap(),
        1
    );
    let udp_blocked = sender.send_to(&[1], (Ipv4Addr::LOCALHOST, udp_denied_port));
    assert!(
        matches!(
            udp_blocked.as_ref().map_err(|error| error.kind()),
            Err(std::io::ErrorKind::PermissionDenied)
        ),
        "unlisted UDP destination should be denied with EPERM, got {udp_blocked:?}"
    );

    // Lift enforcement so the harness's own connections are unaffected.
    egress::clear_egress_enforced(&mut ebpf.bpf, cgroup_id).expect("clear enforcement");
    ebpf.detach().unwrap();
}

/// NET6: clearing a cgroup's egress must delete its allow entries, not just
/// flip the enable flag — otherwise a recycled cgroup id inherits a departed
/// instance's destinations. This drives the exact map operations `clear_egress`
/// performs (a `delete_egress_entry` per destination, then
/// `clear_egress_enforced`) and asserts nothing survives.
#[tokio::test]
#[ignore = "requires Linux root and RELIABURGER_EBPF_TESTS=1"]
async fn egress_cleanup_deletes_destinations_not_just_the_flag() {
    use reliaburger::sesame::egress::{self, EGRESS_ALLOW, EgressKey, EgressValue};

    assert!(
        ebpf_tests_enabled(),
        "set RELIABURGER_EBPF_TESTS=1 after provisioning eBPF prerequisites"
    );

    let obj_dir = find_bpf_obj_dir();
    let mut ebpf =
        OnionEbpf::load(&obj_dir, CGROUP_PATH.as_ref()).expect("failed to load eBPF program");

    // A synthetic cgroup id (well outside the real range) with two allowed
    // destinations and enforcement enabled.
    let cgroup_id: u64 = 0xDEAD_BEEF_CAFE_6006;
    let dests = [
        (Ipv4Addr::new(10, 0, 0, 1), 443u16),
        (Ipv4Addr::new(10, 0, 0, 2), 80u16),
    ];
    let keys: Vec<EgressKey> = dests
        .iter()
        .map(|(ip, port)| EgressKey {
            src_cgroup_id: cgroup_id,
            dst_ip: u32::from(*ip).to_be(),
            dst_port: port.to_be(),
            _pad: 0,
        })
        .collect();
    for key in &keys {
        egress::write_egress_entry(
            &mut ebpf.bpf,
            *key,
            EgressValue {
                action: EGRESS_ALLOW,
            },
        )
        .expect("write egress entry");
    }
    egress::set_egress_enforced(&mut ebpf.bpf, cgroup_id).expect("enable enforcement");

    // Programmed: both destinations allowed, enforcement on.
    assert!(egress::egress_enforced(&mut ebpf.bpf, cgroup_id).unwrap());
    for key in &keys {
        assert!(
            egress::egress_allowed(&mut ebpf.bpf, *key).unwrap(),
            "destination should be allowed after write"
        );
    }

    // The cleanup clear_egress now performs: delete each destination, then the
    // enable flag.
    for key in &keys {
        egress::delete_egress_entry(&mut ebpf.bpf, *key).expect("delete egress entry");
    }
    egress::clear_egress_enforced(&mut ebpf.bpf, cgroup_id).expect("clear enforcement");

    // Nothing lingers for a future cgroup that reuses this id.
    assert!(
        !egress::egress_enforced(&mut ebpf.bpf, cgroup_id).unwrap(),
        "enforcement flag should be cleared"
    );
    for key in &keys {
        assert!(
            !egress::egress_allowed(&mut ebpf.bpf, *key).unwrap(),
            "destination should be gone after delete (NET6)"
        );
    }

    ebpf.detach().unwrap();
}

// ---------------------------------------------------------------------------
// Tier 2c: Namespace isolation (NET5)
// ---------------------------------------------------------------------------

/// NET5: the connect hook only enforces namespace isolation once the source
/// cgroup's namespace is recorded in `cgroup_namespace_map`. With that written,
/// a connect to a service in a *different* namespace is denied (EPERM) unless
/// `firewall_map` carries an explicit `(src_cgroup, dst_app)` allow. This is
/// the userspace half NET5 was missing — the C hook already implemented the
/// check. We enforce against the *test's own* cgroup so
/// `bpf_get_current_cgroup_id()` matches what we program.
#[tokio::test]
#[ignore = "requires Linux root and RELIABURGER_EBPF_TESTS=1"]
async fn namespace_isolation_denies_cross_namespace_by_default() {
    use reliaburger::onion::ebpf::maps::BpfServiceMap;
    use reliaburger::sesame::firewall::{
        self, FIREWALL_ALLOW, ResolvedFirewallRule, rules_to_bpf_entries,
    };

    assert!(
        ebpf_tests_enabled(),
        "set RELIABURGER_EBPF_TESTS=1 after provisioning eBPF prerequisites"
    );

    let obj_dir = find_bpf_obj_dir();
    let mut ebpf =
        OnionEbpf::load(&obj_dir, CGROUP_PATH.as_ref()).expect("failed to load eBPF program");

    // A destination service in "backend-ns" with one real backend, mirrored
    // into backend_map so the VIP resolves (count >= 1) and the hook reaches
    // the firewall check rather than the no-backend early-out.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let backend_port = listener.local_addr().unwrap().port();
    let mut map = ServiceMap::new();
    map.register_app("dest", "backend-ns", backend_port, None)
        .unwrap();
    map.add_backend(
        &ServiceId::new("backend-ns", "dest"),
        BackendInstance {
            instance_id: "dest-0".to_string(),
            node_ip: Ipv4Addr::LOCALHOST,
            host_port: backend_port,
            healthy: true,
        },
    )
    .unwrap();
    let entry = map
        .resolve(&ServiceId::new("backend-ns", "dest"))
        .unwrap()
        .clone();
    BpfServiceMap::new()
        .update_backends_bpf(&mut ebpf, entry.vip, entry.port, &entry)
        .expect("write backend entry");

    // Put the test process's cgroup in a *different* namespace than the
    // destination service. Any id that differs from the service's forces the
    // hook's cross-namespace branch.
    let src_cgroup = reliaburger::sesame::egress::cgroup_id_of_pid(std::process::id())
        .expect("failed to resolve own cgroup id");
    let src_ns = entry.namespace_id.wrapping_add(1);
    firewall::write_cgroup_namespace_entry(&mut ebpf.bpf, src_cgroup, src_ns)
        .expect("write cgroup-namespace entry");

    let vip_dst = SocketAddr::new(entry.vip.0.into(), entry.port);

    // Cross-namespace, no allow entry → denied with EPERM.
    let blocked = TcpStream::connect_timeout(&vip_dst, Duration::from_secs(2));
    assert!(
        matches!(
            blocked.as_ref().map_err(|e| e.kind()),
            Err(std::io::ErrorKind::PermissionDenied)
        ),
        "cross-namespace connect should be denied with EPERM, got {blocked:?}"
    );

    // Add the explicit allow → the same connect is now rewritten to the live
    // backend and succeeds. Only the firewall entry changed between the two.
    for (key, value) in rules_to_bpf_entries(&[ResolvedFirewallRule {
        src_cgroup_id: src_cgroup,
        dst_app_id: entry.app_id,
        action: FIREWALL_ALLOW,
    }]) {
        firewall::write_firewall_entry(&mut ebpf.bpf, key, value).expect("write firewall entry");
    }
    let allowed = TcpStream::connect_timeout(&vip_dst, Duration::from_secs(2));
    assert!(
        allowed.is_ok(),
        "allowed cross-namespace connect should reach the backend: {allowed:?}"
    );

    // Forget the namespace mapping so the harness's own connections aren't
    // caught by a lingering isolation identity on a reused cgroup.
    firewall::delete_cgroup_namespace_entry(&mut ebpf.bpf, src_cgroup).ok();
    ebpf.detach().unwrap();
}

/// An explicit grant must identify the destination service, not its bare name.
#[tokio::test]
#[ignore = "requires Linux root and RELIABURGER_EBPF_TESTS=1"]
async fn namespace_grant_cannot_authorise_a_same_named_destination() {
    use reliaburger::sesame::{egress, firewall};
    use std::collections::HashMap;
    assert!(ebpf_tests_enabled());
    let mut ebpf = OnionEbpf::load_embedded(CGROUP_PATH.as_ref()).unwrap();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let mut services = ServiceMap::new();
    for namespace in ["permitted", "private"] {
        let id = ServiceId::new(namespace, "database");
        let allowed = if namespace == "permitted" {
            vec!["frontend/client".into()]
        } else {
            vec![]
        };
        services.register(&id, port, Some(allowed)).unwrap();
        services
            .add_backend(
                &id,
                BackendInstance {
                    instance_id: format!("{namespace}__database-0"),
                    node_ip: Ipv4Addr::LOCALHOST,
                    host_port: port,
                    healthy: true,
                },
            )
            .unwrap();
        let entry = services.resolve(&id).unwrap();
        BpfServiceMap::new()
            .update_backends_bpf(&mut ebpf, entry.vip, entry.port, entry)
            .unwrap();
    }
    let cgroup = egress::cgroup_id_of_pid(std::process::id()).unwrap();
    firewall::write_cgroup_namespace_entry(
        &mut ebpf.bpf,
        cgroup,
        reliaburger::onion::vip::name_to_id("frontend"),
    )
    .unwrap();
    let sources = HashMap::from([(("frontend".into(), "client".into()), vec![cgroup])]);
    let entries = services
        .resolve_all()
        .into_iter()
        .cloned()
        .collect::<Vec<_>>();
    for (key, value) in
        firewall::rules_to_bpf_entries(&firewall::resolve_firewall_rules(&entries, &sources))
    {
        firewall::write_firewall_entry(&mut ebpf.bpf, key, value).unwrap();
    }
    let permitted = services
        .resolve(&ServiceId::new("permitted", "database"))
        .unwrap();
    let private = services
        .resolve(&ServiceId::new("private", "database"))
        .unwrap();
    let allowed = TcpStream::connect_timeout(
        &SocketAddr::new(permitted.vip.0.into(), port),
        Duration::from_secs(2),
    );
    let denied = TcpStream::connect_timeout(
        &SocketAddr::new(private.vip.0.into(), port),
        Duration::from_secs(2),
    );
    firewall::delete_cgroup_firewall_state(&mut ebpf.bpf, cgroup).unwrap();
    ebpf.detach().unwrap();
    assert!(
        allowed.is_ok(),
        "explicitly allowed destination failed: {allowed:?}"
    );
    assert!(
        matches!(denied, Err(error) if error.kind() == std::io::ErrorKind::PermissionDenied),
        "grant to permitted/database also authorised private/database"
    );
}

// ---------------------------------------------------------------------------
// Tier 3: DNS responder
// ---------------------------------------------------------------------------

#[tokio::test]
#[ignore = "requires Linux root and RELIABURGER_EBPF_TESTS=1"]
async fn dns_responder_resolves_internal_name() {
    assert!(
        ebpf_tests_enabled(),
        "set RELIABURGER_EBPF_TESTS=1 after provisioning eBPF prerequisites"
    );

    let mut map = ServiceMap::new();
    map.register_app("redis", "default", 6379, None).unwrap();
    let (_map_tx, map_rx) = tokio::sync::watch::channel(map);

    let shutdown = CancellationToken::new();

    // Use a high port to avoid needing root for port 53
    let config = reliaburger::onion::dns::DnsConfig {
        listen_addr: "127.0.0.1:15353".parse().unwrap(),
        upstream: "8.8.8.8:53".parse().unwrap(),
        upstream_timeout: Duration::from_secs(2),
        source_namespaces: tokio::sync::watch::channel(
            reliaburger::onion::dns::DnsSourceNamespaces::from_bindings([(
                "127.0.0.1".parse().unwrap(),
                "default".into(),
            )]),
        )
        .1,
        ..reliaburger::onion::dns::DnsConfig::default()
    };

    let (_fault_tx, fault_rx) =
        tokio::sync::watch::channel(reliaburger::onion::dns::DnsFaultState::default());
    let shutdown_clone = shutdown.clone();
    tokio::spawn(async move {
        let _ =
            reliaburger::onion::dns::run_dns_responder(config, map_rx, fault_rx, shutdown_clone)
                .await;
    });

    // Give the responder a moment to bind
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Send a DNS query for redis.internal
    let socket = tokio::net::UdpSocket::bind("0.0.0.0:0").await.unwrap();
    let query = build_dns_query("redis.internal");
    socket.send_to(&query, "127.0.0.1:15353").await.unwrap();

    let mut buf = [0u8; 512];
    let (len, _) = tokio::time::timeout(Duration::from_secs(2), socket.recv_from(&mut buf))
        .await
        .expect("DNS response timed out")
        .expect("recv failed");

    let response = &buf[..len];

    // Response should have ANCOUNT=1
    assert_eq!(response[6], 0x00);
    assert_eq!(response[7], 0x01);

    // Last 4 bytes should be the VIP
    let vip = VirtualIP::from_service_id(&ServiceId::new("default", "redis"));
    assert_eq!(&response[len - 4..], &vip.0.octets());

    shutdown.cancel();
}

#[tokio::test]
#[ignore = "requires Linux root and RELIABURGER_EBPF_TESTS=1"]
async fn dns_responder_non_internal_times_out() {
    assert!(
        ebpf_tests_enabled(),
        "set RELIABURGER_EBPF_TESTS=1 after provisioning eBPF prerequisites"
    );

    let (_map_tx, map_rx) = tokio::sync::watch::channel(ServiceMap::new());
    let shutdown = CancellationToken::new();

    // Point upstream at a non-existent resolver so forwarding times out
    let config = reliaburger::onion::dns::DnsConfig {
        listen_addr: "127.0.0.1:15354".parse().unwrap(),
        upstream: "192.0.2.1:53".parse().unwrap(), // TEST-NET, unreachable
        upstream_timeout: Duration::from_millis(500),
        source_namespaces: tokio::sync::watch::channel(
            reliaburger::onion::dns::DnsSourceNamespaces::from_bindings([(
                "127.0.0.1".parse().unwrap(),
                "default".into(),
            )]),
        )
        .1,
        ..reliaburger::onion::dns::DnsConfig::default()
    };

    let (_fault_tx, fault_rx) =
        tokio::sync::watch::channel(reliaburger::onion::dns::DnsFaultState::default());
    let shutdown_clone = shutdown.clone();
    tokio::spawn(async move {
        let _ =
            reliaburger::onion::dns::run_dns_responder(config, map_rx, fault_rx, shutdown_clone)
                .await;
    });

    tokio::time::sleep(Duration::from_millis(100)).await;

    let socket = tokio::net::UdpSocket::bind("0.0.0.0:0").await.unwrap();
    let query = build_dns_query("example.com");
    socket.send_to(&query, "127.0.0.1:15354").await.unwrap();

    let mut buf = [0u8; 512];
    let (len, _) = tokio::time::timeout(Duration::from_secs(3), socket.recv_from(&mut buf))
        .await
        .expect("expected a SERVFAIL response, not silence")
        .expect("recv failed");

    // M8 hardening: an unreachable upstream now yields SERVFAIL
    // (RCODE 2) instead of leaving the client to time out.
    assert_eq!(buf[3] & 0x0F, 2, "expected SERVFAIL for dead upstream");
    assert!(len >= 12);

    shutdown.cancel();
}

// ---------------------------------------------------------------------------
// Tier 2d: IPv6 + CIDR egress (NET7) and kernel-truth sweep (NET8)
// ---------------------------------------------------------------------------

/// NET7: with egress enforcement on, a listed IPv6 destination connects
/// and an unlisted one is denied with EPERM — the connect6 hook, driven
/// against the test's own cgroup.
#[tokio::test]
#[ignore = "requires Linux root and RELIABURGER_EBPF_TESTS=1"]
async fn connect6_denies_unlisted_and_allows_listed_ipv6() {
    use reliaburger::sesame::egress::{self, EGRESS_ALLOW, EgressValue, exact_v6_key};
    use std::net::Ipv6Addr;

    assert!(
        ebpf_tests_enabled(),
        "set RELIABURGER_EBPF_TESTS=1 after provisioning eBPF prerequisites"
    );

    let obj_dir = find_bpf_obj_dir();
    let mut ebpf =
        OnionEbpf::load(&obj_dir, CGROUP_PATH.as_ref()).expect("failed to load eBPF program");
    assert!(
        ebpf.connect6_attached(),
        "connect6 must attach on the test kernel"
    );

    let allowed = std::net::TcpListener::bind("[::1]:0").unwrap();
    let allowed_port = allowed.local_addr().unwrap().port();
    let denied = std::net::TcpListener::bind("[::1]:0").unwrap();
    let denied_port = denied.local_addr().unwrap().port();

    let cgroup_id =
        egress::cgroup_id_of_pid(std::process::id()).expect("failed to resolve own cgroup id");

    egress::write_egress6_entry(
        &mut ebpf.bpf,
        exact_v6_key(cgroup_id, Ipv6Addr::LOCALHOST, allowed_port),
        EgressValue {
            action: EGRESS_ALLOW,
        },
    )
    .expect("write egress6 entry");
    egress::set_egress_enforced(&mut ebpf.bpf, cgroup_id).expect("enable enforcement");

    let ok = TcpStream::connect_timeout(
        &SocketAddr::new(Ipv6Addr::LOCALHOST.into(), allowed_port),
        Duration::from_secs(2),
    );
    let blocked = TcpStream::connect_timeout(
        &SocketAddr::new(Ipv6Addr::LOCALHOST.into(), denied_port),
        Duration::from_secs(2),
    );
    let udp_allowed = std::net::UdpSocket::bind("[::1]:0").unwrap();
    let udp_allowed_port = udp_allowed.local_addr().unwrap().port();
    let udp_denied = std::net::UdpSocket::bind("[::1]:0").unwrap();
    let udp_denied_port = udp_denied.local_addr().unwrap().port();
    egress::write_egress6_entry(
        &mut ebpf.bpf,
        exact_v6_key(cgroup_id, Ipv6Addr::LOCALHOST, udp_allowed_port),
        EgressValue {
            action: EGRESS_ALLOW,
        },
    )
    .unwrap();
    let udp_sender = std::net::UdpSocket::bind("[::1]:0").unwrap();
    let udp_ok = udp_sender.send_to(&[1], (Ipv6Addr::LOCALHOST, udp_allowed_port));
    let udp_blocked = udp_sender.send_to(&[1], (Ipv6Addr::LOCALHOST, udp_denied_port));

    // Lift enforcement before asserting, so a failure never leaves the
    // harness cgroup restricted.
    egress::delete_cgroup_egress_state(&mut ebpf.bpf, cgroup_id).expect("scrub");
    ebpf.detach().unwrap();

    assert!(ok.is_ok(), "listed IPv6 destination should connect: {ok:?}");
    assert_eq!(
        udp_ok.unwrap(),
        1,
        "listed IPv6 UDP destination should send"
    );
    assert!(
        matches!(
            blocked.as_ref().map_err(|e| e.kind()),
            Err(std::io::ErrorKind::PermissionDenied)
        ),
        "unlisted IPv6 destination should be denied with EPERM, got {blocked:?}"
    );
    assert!(
        matches!(
            udp_blocked.as_ref().map_err(|error| error.kind()),
            Err(std::io::ErrorKind::PermissionDenied)
        ),
        "unlisted IPv6 UDP destination should be denied, got {udp_blocked:?}"
    );
}

/// NET7 regression: a v4-only allowlist used to be a suggestion — any
/// dual-stack workload could bypass it entirely over IPv6. The old
/// behaviour (v6 connect succeeds) must now deny, while the same v4
/// destination stays reachable both natively and as a v4-mapped
/// (::ffff:a.b.c.d) connect through the connect6 hook.
#[tokio::test]
#[ignore = "requires Linux root and RELIABURGER_EBPF_TESTS=1"]
async fn v4_only_allowlist_no_longer_bypassed_over_ipv6() {
    use reliaburger::sesame::egress::{self, EGRESS_ALLOW, EgressValue, exact_v4_key};
    use std::net::{Ipv6Addr, SocketAddrV6};

    assert!(
        ebpf_tests_enabled(),
        "set RELIABURGER_EBPF_TESTS=1 after provisioning eBPF prerequisites"
    );

    let obj_dir = find_bpf_obj_dir();
    let mut ebpf =
        OnionEbpf::load(&obj_dir, CGROUP_PATH.as_ref()).expect("failed to load eBPF program");

    // A v4 listener we allow, and a v6 listener we do not.
    let v4_listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let v4_port = v4_listener.local_addr().unwrap().port();
    let v6_listener = std::net::TcpListener::bind("[::1]:0").unwrap();
    let v6_port = v6_listener.local_addr().unwrap().port();

    let cgroup_id =
        egress::cgroup_id_of_pid(std::process::id()).expect("failed to resolve own cgroup id");

    egress::write_egress_entry(
        &mut ebpf.bpf,
        exact_v4_key(cgroup_id, Ipv4Addr::LOCALHOST, v4_port),
        EgressValue {
            action: EGRESS_ALLOW,
        },
    )
    .expect("write egress entry");
    egress::set_egress_enforced(&mut ebpf.bpf, cgroup_id).expect("enable enforcement");

    // Native v4: allowed.
    let v4_ok = TcpStream::connect_timeout(
        &SocketAddr::new(Ipv4Addr::LOCALHOST.into(), v4_port),
        Duration::from_secs(2),
    );
    // v4-mapped through connect6: same policy, still allowed.
    let mapped = Ipv6Addr::new(0, 0, 0, 0, 0, 0xffff, 0x7f00, 0x0001);
    let mapped_ok = TcpStream::connect_timeout(
        &SocketAddr::V6(SocketAddrV6::new(mapped, v4_port, 0, 0)),
        Duration::from_secs(2),
    );
    // Plain IPv6 to an unlisted destination: the old bypass, now denied.
    let v6_blocked = TcpStream::connect_timeout(
        &SocketAddr::new(Ipv6Addr::LOCALHOST.into(), v6_port),
        Duration::from_secs(2),
    );
    // v4-mapped to an unlisted v4 port: denied through connect6 too.
    let mapped_blocked = TcpStream::connect_timeout(
        &SocketAddr::V6(SocketAddrV6::new(mapped, v6_port, 0, 0)),
        Duration::from_secs(2),
    );

    egress::delete_cgroup_egress_state(&mut ebpf.bpf, cgroup_id).expect("scrub");
    ebpf.detach().unwrap();

    assert!(
        v4_ok.is_ok(),
        "listed v4 destination should connect: {v4_ok:?}"
    );
    assert!(
        mapped_ok.is_ok(),
        "listed v4 destination should connect as v4-mapped IPv6: {mapped_ok:?}"
    );
    assert!(
        matches!(
            v6_blocked.as_ref().map_err(|e| e.kind()),
            Err(std::io::ErrorKind::PermissionDenied)
        ),
        "IPv6 must no longer bypass a v4 allowlist, got {v6_blocked:?}"
    );
    assert!(
        matches!(
            mapped_blocked.as_ref().map_err(|e| e.kind()),
            Err(std::io::ErrorKind::PermissionDenied)
        ),
        "unlisted v4-mapped destination should be denied, got {mapped_blocked:?}"
    );
}

/// NET7: CIDR entries are enforced via the LPM trie — an address inside
/// an allowed prefix connects on the listed port and is denied on any
/// other port.
#[tokio::test]
#[ignore = "requires Linux root and RELIABURGER_EBPF_TESTS=1"]
async fn cidr_egress_allowed_via_lpm_trie() {
    use reliaburger::sesame::egress::{self, EgressDestination, merge_cidr_ports};
    use std::net::IpAddr;

    assert!(
        ebpf_tests_enabled(),
        "set RELIABURGER_EBPF_TESTS=1 after provisioning eBPF prerequisites"
    );

    let obj_dir = find_bpf_obj_dir();
    let mut ebpf =
        OnionEbpf::load(&obj_dir, CGROUP_PATH.as_ref()).expect("failed to load eBPF program");

    // Two loopback listeners; 127.0.0.0/8 covers both addresses, but only
    // one port is allowed.
    let allowed = std::net::TcpListener::bind("127.0.0.5:0").unwrap();
    let allowed_port = allowed.local_addr().unwrap().port();
    let denied = std::net::TcpListener::bind("127.0.0.6:0").unwrap();
    let denied_port = denied.local_addr().unwrap().port();

    let cgroup_id =
        egress::cgroup_id_of_pid(std::process::id()).expect("failed to resolve own cgroup id");

    let dests = vec![EgressDestination::Cidr {
        network: IpAddr::V4(Ipv4Addr::new(127, 0, 0, 0)),
        prefix_len: 8,
        port: allowed_port,
    }];
    let merged = merge_cidr_ports(&dests).expect("merge");
    egress::write_egress_destinations(&mut ebpf.bpf, cgroup_id, &dests, &merged)
        .expect("program CIDR destinations");
    egress::set_egress_enforced(&mut ebpf.bpf, cgroup_id).expect("enable enforcement");

    let inside_cidr = TcpStream::connect_timeout(
        &SocketAddr::new(Ipv4Addr::new(127, 0, 0, 5).into(), allowed_port),
        Duration::from_secs(2),
    );
    let wrong_port = TcpStream::connect_timeout(
        &SocketAddr::new(Ipv4Addr::new(127, 0, 0, 6).into(), denied_port),
        Duration::from_secs(2),
    );

    egress::delete_cgroup_egress_state(&mut ebpf.bpf, cgroup_id).expect("scrub");
    ebpf.detach().unwrap();

    assert!(
        inside_cidr.is_ok(),
        "address inside the allowed CIDR should connect on the listed port: {inside_cidr:?}"
    );
    assert!(
        matches!(
            wrong_port.as_ref().map_err(|e| e.kind()),
            Err(std::io::ErrorKind::PermissionDenied)
        ),
        "same CIDR on an unlisted port should be denied, got {wrong_port:?}"
    );
}

/// NET8 sweep: kernel egress state for a cgroup with no live instance is
/// planned stale and scrubbed across all four allow maps plus the flag.
#[tokio::test]
#[ignore = "requires Linux root and RELIABURGER_EBPF_TESTS=1"]
async fn sweep_scrubs_orphaned_cgroup_state() {
    use reliaburger::sesame::egress::{
        self, EgressDestination, merge_cidr_ports, plan_egress_sweep,
    };
    use std::collections::HashSet;
    use std::net::IpAddr;

    assert!(
        ebpf_tests_enabled(),
        "set RELIABURGER_EBPF_TESTS=1 after provisioning eBPF prerequisites"
    );

    let obj_dir = find_bpf_obj_dir();
    let mut ebpf =
        OnionEbpf::load(&obj_dir, CGROUP_PATH.as_ref()).expect("failed to load eBPF program");

    // A synthetic cgroup id (well outside the real range) with exact v4,
    // exact v6 and CIDR entries, plus the enforcement flag.
    let orphan: u64 = 0xDEAD_BEEF_5EEE_0001;
    let dests = vec![
        EgressDestination::Ip {
            ip: IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)),
            port: 443,
        },
        EgressDestination::Ip {
            ip: "2001:db8::7".parse().unwrap(),
            port: 443,
        },
        EgressDestination::Cidr {
            network: IpAddr::V4(Ipv4Addr::new(10, 0, 0, 0)),
            prefix_len: 8,
            port: 80,
        },
    ];
    let merged = merge_cidr_ports(&dests).unwrap();
    egress::write_egress_destinations(&mut ebpf.bpf, orphan, &dests, &merged)
        .expect("program orphan destinations");
    egress::set_egress_enforced(&mut ebpf.bpf, orphan).expect("enable enforcement");

    let enforced = egress::list_enforced_cgroups(&mut ebpf.bpf).unwrap();
    let entries = egress::list_egress_entry_cgroups(&mut ebpf.bpf).unwrap();
    assert!(enforced.contains(&orphan));
    assert!(entries.contains(&orphan));

    // No live instance expects this cgroup: the plan marks it stale.
    let expected: HashSet<u64> = HashSet::new();
    let plan = plan_egress_sweep(&expected, &enforced, &entries);
    assert!(plan.stale.contains(&orphan), "orphan not planned stale");

    // Scrub, then nothing lingers for a recycled cgroup id.
    egress::delete_cgroup_egress_state(&mut ebpf.bpf, orphan).expect("scrub");
    assert!(
        !egress::list_enforced_cgroups(&mut ebpf.bpf)
            .unwrap()
            .contains(&orphan)
    );
    assert!(
        !egress::list_egress_entry_cgroups(&mut ebpf.bpf)
            .unwrap()
            .contains(&orphan)
    );

    ebpf.detach().unwrap();
}

// ---------------------------------------------------------------------------
// Tier 2e: pre-start programming (create → program → start)
// ---------------------------------------------------------------------------

/// The agent programs egress against the cgroup directory's inode
/// *before* `start`. The mock grill never runs a process (`pid()` is
/// `None`), so the post-start pid path cannot have programmed anything:
/// enforcement being active for the cgroup path proves the pre-start
/// ordering — there is no window in which the workload runs unpoliced.
#[tokio::test]
#[ignore = "requires Linux root and RELIABURGER_EBPF_TESTS=1"]
async fn egress_programmed_before_start_via_cgroup_path() {
    use reliaburger::bun::agent::{AgentCommand, BunAgent};
    use reliaburger::config::Config;
    use reliaburger::grill::mock::MockGrill;
    use reliaburger::grill::port::PortAllocator;
    use reliaburger::sesame::egress::{self, exact_v4_key};
    use std::sync::Arc;
    use tokio::sync::{Mutex, mpsc};
    use tokio_util::sync::CancellationToken;

    assert!(
        ebpf_tests_enabled(),
        "set RELIABURGER_EBPF_TESTS=1 after provisioning eBPF prerequisites"
    );

    let obj_dir = find_bpf_obj_dir();
    let ebpf = Arc::new(Mutex::new(
        OnionEbpf::load(&obj_dir, CGROUP_PATH.as_ref()).expect("failed to load eBPF program"),
    ));

    let grill = MockGrill::new();
    grill.set_honours_cgroup_path(true);
    let (cmd_tx, cmd_rx) = mpsc::channel(64);
    let shutdown = CancellationToken::new();
    let mut agent = BunAgent::new(
        grill.clone(),
        PortAllocator::new(42100, 42400),
        cmd_rx,
        shutdown.clone(),
    );
    agent.set_onion_ebpf(Arc::clone(&ebpf)).await;
    let agent_task = tokio::spawn(async move { agent.run().await });

    let config = Config::parse(
        r#"
        [app.prestart]
        image = "mock:image"
        command = ["sleep", "600"]

        [app.prestart.egress]
        allow = ["203.0.113.9:443"]
    "#,
    )
    .unwrap();
    let (ev_tx, mut ev_rx) = mpsc::channel(64);
    cmd_tx
        .send(AgentCommand::Deploy {
            config,
            events: ev_tx,
        })
        .await
        .unwrap();
    while let Some(event) = ev_rx.recv().await {
        assert!(
            !matches!(event, reliaburger::bun::agent::ApplyEvent::Error { .. }),
            "pre-start deployment failed: {event:?}"
        );
    }

    // The agent created the cgroup directory and programmed enforcement
    // against its inode before ever calling start.
    let cgroup_dir = std::path::Path::new("/sys/fs/cgroup/reliaburger/default/prestart/0");
    let cgroup_id =
        egress::cgroup_id_of_path(cgroup_dir).expect("agent should have created the cgroup dir");
    {
        let mut e = ebpf.lock().await;
        assert!(
            egress::egress_enforced(&mut e.bpf, cgroup_id).unwrap(),
            "enforcement flag missing: egress was not programmed pre-start"
        );
        assert!(
            egress::egress_allowed(
                &mut e.bpf,
                exact_v4_key(cgroup_id, Ipv4Addr::new(203, 0, 113, 9), 443)
            )
            .unwrap(),
            "allow entry missing: egress was not programmed pre-start"
        );
    }

    // create must precede start, and both must have happened.
    let calls = grill.calls();
    let create_pos = calls.iter().position(|(op, _)| op == "create");
    let start_pos = calls.iter().position(|(op, _)| op == "start");
    assert!(create_pos.is_some() && start_pos.is_some());
    assert!(create_pos < start_pos);

    // Clean up the kernel state for the synthetic cgroup.
    {
        let mut e = ebpf.lock().await;
        let _ = egress::delete_cgroup_egress_state(&mut e.bpf, cgroup_id);
    }
    shutdown.cancel();
    agent_task.await.unwrap();
}

/// A newly prepared cgroup may reuse an inode whose old map entries survived
/// an unclean exit. Pre-start programming must scrub that whole cgroup policy
/// before opening the destinations requested by the new workload.
#[tokio::test]
#[ignore = "requires Linux root and RELIABURGER_EBPF_TESTS=1"]
async fn pre_start_programming_scrubs_recycled_cgroup_allows() {
    use reliaburger::bun::agent::{AgentCommand, BunAgent};
    use reliaburger::config::Config;
    use reliaburger::grill::mock::MockGrill;
    use reliaburger::grill::port::PortAllocator;
    use reliaburger::sesame::egress::{self, EGRESS_ALLOW, EgressValue, exact_v4_key};
    use std::sync::Arc;
    use tokio::sync::{Mutex, mpsc};
    use tokio_util::sync::CancellationToken;

    assert!(ebpf_tests_enabled());
    let ebpf = Arc::new(Mutex::new(
        OnionEbpf::load(&find_bpf_obj_dir(), CGROUP_PATH.as_ref()).unwrap(),
    ));

    let cgroup_dir = std::path::Path::new("/sys/fs/cgroup/reliaburger/default/recycled/0");
    std::fs::create_dir_all(cgroup_dir).unwrap();
    let cgroup_id = egress::cgroup_id_of_path(cgroup_dir).unwrap();
    let stale_key = exact_v4_key(cgroup_id, Ipv4Addr::new(198, 51, 100, 77), 8443);
    {
        let mut e = ebpf.lock().await;
        egress::delete_cgroup_egress_state(&mut e.bpf, cgroup_id).unwrap();
        egress::write_egress_entry(
            &mut e.bpf,
            stale_key,
            EgressValue {
                action: EGRESS_ALLOW,
            },
        )
        .unwrap();
    }

    let grill = MockGrill::new();
    grill.set_honours_cgroup_path(true);
    let (cmd_tx, cmd_rx) = mpsc::channel(64);
    let shutdown = CancellationToken::new();
    let mut agent = BunAgent::new(
        grill,
        PortAllocator::new(42400, 42500),
        cmd_rx,
        shutdown.clone(),
    );
    agent.set_onion_ebpf(Arc::clone(&ebpf)).await;
    tokio::spawn(async move { agent.run().await });

    let config = Config::parse(
        r#"
        [app.recycled]
        image = "mock:image"
        command = ["sleep", "600"]

        [app.recycled.egress]
        allow = ["203.0.113.9:443"]
    "#,
    )
    .unwrap();
    let (events, mut event_rx) = mpsc::channel(64);
    cmd_tx
        .send(AgentCommand::Deploy { config, events })
        .await
        .unwrap();
    while event_rx.recv().await.is_some() {}

    {
        let mut e = ebpf.lock().await;
        assert!(
            !egress::egress_allowed(&mut e.bpf, stale_key).unwrap(),
            "a recycled cgroup retained an allow from its previous owner"
        );
        assert!(
            egress::egress_allowed(
                &mut e.bpf,
                exact_v4_key(cgroup_id, Ipv4Addr::new(203, 0, 113, 9), 443)
            )
            .unwrap()
        );
        egress::delete_cgroup_egress_state(&mut e.bpf, cgroup_id).unwrap();
    }
    shutdown.cancel();
}

/// A live hook is part of the security boundary. If it disappears after
/// deploy, the agent must stop the protected workload rather than leave it
/// running with a decorative allowlist.
#[tokio::test]
#[ignore = "requires Linux root and RELIABURGER_EBPF_TESTS=1"]
async fn live_egress_hook_loss_stops_protected_workload() {
    use reliaburger::bun::agent::{AgentCommand, BunAgent};
    use reliaburger::config::Config;
    use reliaburger::grill::mock::MockGrill;
    use reliaburger::grill::port::PortAllocator;
    use std::sync::Arc;
    use tokio::sync::{Mutex, mpsc};
    use tokio_util::sync::CancellationToken;

    assert!(ebpf_tests_enabled());
    let ebpf = Arc::new(Mutex::new(
        OnionEbpf::load(&find_bpf_obj_dir(), CGROUP_PATH.as_ref()).unwrap(),
    ));
    let grill = MockGrill::new();
    grill.set_honours_cgroup_path(true);
    let (cmd_tx, cmd_rx) = mpsc::channel(64);
    let shutdown = CancellationToken::new();
    let mut agent = BunAgent::new(
        grill.clone(),
        PortAllocator::new(42800, 43100),
        cmd_rx,
        shutdown.clone(),
    );
    agent.set_onion_ebpf(Arc::clone(&ebpf)).await;
    let readiness = reliaburger::bun::readiness::ReadinessTracker::new();
    agent.set_readiness_tracker(readiness.clone());
    let owner = tokio::spawn(async move { agent.run().await });

    let config = Config::parse(
        r#"
        [app.guarded]
        image = "mock:image"
        command = ["sleep", "600"]

        [app.guarded.egress]
        allow = ["203.0.113.9:443"]
    "#,
    )
    .unwrap();
    let (events, mut event_rx) = mpsc::channel(64);
    cmd_tx
        .send(AgentCommand::Deploy { config, events })
        .await
        .unwrap();
    while event_rx.recv().await.is_some() {}

    assert!(
        readiness
            .capability_snapshot()
            .await
            .egress
            .can_enforce_allowlist()
    );
    ebpf.lock().await.detach().unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(4), async {
        loop {
            if grill
                .calls()
                .iter()
                .any(|(operation, _)| operation == "stop")
                && !readiness
                    .capability_snapshot()
                    .await
                    .egress
                    .can_enforce_allowlist()
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("agent did not stop the workload after hook loss");

    shutdown.cancel();
    owner.await.unwrap();
}

/// Fail closed: a pre-start programming error (here: an allowlist that
/// cannot be represented in the kernel CIDR value) fails the deploy and
/// leaves no running process — the mock grill records a create and a
/// stop, but never a start.
#[tokio::test]
#[ignore = "requires Linux root and RELIABURGER_EBPF_TESTS=1"]
async fn pre_start_programming_error_fails_deploy_with_no_running_process() {
    use reliaburger::bun::agent::{AgentCommand, BunAgent};
    use reliaburger::config::Config;
    use reliaburger::grill::mock::MockGrill;
    use reliaburger::grill::port::PortAllocator;
    use std::sync::Arc;
    use tokio::sync::{Mutex, mpsc};
    use tokio_util::sync::CancellationToken;

    assert!(
        ebpf_tests_enabled(),
        "set RELIABURGER_EBPF_TESTS=1 after provisioning eBPF prerequisites"
    );

    let obj_dir = find_bpf_obj_dir();
    let ebpf = Arc::new(Mutex::new(
        OnionEbpf::load(&obj_dir, CGROUP_PATH.as_ref()).expect("failed to load eBPF program"),
    ));

    let grill = MockGrill::new();
    grill.set_honours_cgroup_path(true);
    let (cmd_tx, cmd_rx) = mpsc::channel(64);
    let shutdown = CancellationToken::new();
    let mut agent = BunAgent::new(
        grill.clone(),
        PortAllocator::new(42500, 42800),
        cmd_rx,
        shutdown.clone(),
    );
    agent.set_onion_ebpf(Arc::clone(&ebpf)).await;
    tokio::spawn(async move { agent.run().await });

    // Nine ports on one CIDR overflows the kernel value (MAX_CIDR_PORTS
    // is 8): a permanent representation error, so the deploy must fail.
    let allow: Vec<String> = (1..=9).map(|p| format!("\"10.0.0.0/8:{p}\"")).collect();
    let config = Config::parse(&format!(
        r#"
        [app.badcidr]
        image = "mock:image"
        command = ["sleep", "600"]

        [app.badcidr.egress]
        allow = [{}]
    "#,
        allow.join(", ")
    ))
    .unwrap();
    let (ev_tx, mut ev_rx) = mpsc::channel(64);
    cmd_tx
        .send(AgentCommand::Deploy {
            config,
            events: ev_tx,
        })
        .await
        .unwrap();
    let mut saw_error = false;
    while let Some(event) = ev_rx.recv().await {
        if matches!(event, reliaburger::bun::agent::ApplyEvent::Error { .. }) {
            saw_error = true;
        }
    }
    assert!(saw_error, "deploy should report the pre-start failure");

    let calls = grill.calls();
    assert!(
        calls.iter().any(|(op, _)| op == "create"),
        "container should have been created"
    );
    assert!(
        !calls.iter().any(|(op, _)| op == "start"),
        "fail-closed: the workload must never start, got {calls:?}"
    );
    assert!(
        calls.iter().any(|(op, _)| op == "stop"),
        "the created container should be removed, got {calls:?}"
    );

    shutdown.cancel();
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn build_dns_query(name: &str) -> Vec<u8> {
    let mut packet = Vec::new();

    // Header
    packet.extend_from_slice(&[0x12, 0x34]); // ID
    packet.extend_from_slice(&[0x01, 0x00]); // flags: RD=1
    packet.extend_from_slice(&[0x00, 0x01]); // QDCOUNT=1
    packet.extend_from_slice(&[0x00, 0x00]); // ANCOUNT=0
    packet.extend_from_slice(&[0x00, 0x00]); // NSCOUNT=0
    packet.extend_from_slice(&[0x00, 0x00]); // ARCOUNT=0

    // Question: DNS wire format labels
    for label in name.split('.') {
        packet.push(label.len() as u8);
        packet.extend_from_slice(label.as_bytes());
    }
    packet.push(0x00); // end of name

    // QTYPE=A, QCLASS=IN
    packet.extend_from_slice(&[0x00, 0x01]);
    packet.extend_from_slice(&[0x00, 0x01]);

    packet
}

// BPF_MAP_FREEZE makes userspace deletion fail while preserving readable
// evidence. Each test owns a fresh, unpinned map destroyed with its loader.
fn freeze_egress_map(ebpf: &OnionEbpf, name: &str) {
    use std::os::fd::{AsFd, AsRawFd};
    let aya::maps::Map::HashMap(map) = ebpf.bpf.map(name).unwrap() else {
        panic!("expected a hash map");
    };
    #[repr(C)]
    struct FreezeAttributes {
        map_fd: u32,
    }
    let attributes = FreezeAttributes {
        map_fd: map.fd().as_fd().as_raw_fd() as u32,
    };
    // Linux uapi bpf_cmd::BPF_MAP_FREEZE = 22; its only input is map_fd.
    // SAFETY: the kernel reads the initialised repr(C) input for the stated
    // length; the borrowed map descriptor remains open throughout the call.
    let result = unsafe {
        nix::libc::syscall(
            nix::libc::SYS_bpf,
            22,
            &attributes,
            std::mem::size_of::<FreezeAttributes>(),
        )
    };
    assert_eq!(result, 0, "{}", std::io::Error::last_os_error());
}

#[test]
#[ignore = "requires Linux root and RELIABURGER_EBPF_TESTS=1"]
fn egress_cleanup_refuses_a_frozen_destination_map_and_keeps_enforcement() {
    use reliaburger::sesame::egress::{self, EGRESS_ALLOW, EgressKey, EgressValue};
    assert!(ebpf_tests_enabled());
    let mut ebpf = OnionEbpf::load(&find_bpf_obj_dir(), CGROUP_PATH.as_ref()).unwrap();
    let cgroup = 0xDEAD_BEEF_CAFE_6101;
    let key = EgressKey {
        src_cgroup_id: cgroup,
        dst_ip: u32::from(Ipv4Addr::new(203, 0, 113, 1)).to_be(),
        dst_port: 443u16.to_be(),
        _pad: 0,
    };
    egress::write_egress_entry(
        &mut ebpf.bpf,
        key,
        EgressValue {
            action: EGRESS_ALLOW,
        },
    )
    .unwrap();
    egress::set_egress_enforced(&mut ebpf.bpf, cgroup).unwrap();
    freeze_egress_map(&ebpf, "egress_map");
    let result = egress::delete_cgroup_egress_state(&mut ebpf.bpf, cgroup);
    let still_allowed = egress::egress_allowed(&mut ebpf.bpf, key).unwrap();
    let still_enforced = egress::egress_enforced(&mut ebpf.bpf, cgroup).unwrap();
    ebpf.detach().unwrap();
    assert!(result.is_err(), "cleanup ignored a kernel deletion refusal");
    assert!(
        still_allowed && still_enforced,
        "failed cleanup must retain enforcement and the undeleted entry"
    );
}

#[test]
#[ignore = "requires Linux root and RELIABURGER_EBPF_TESTS=1"]
fn egress_cleanup_refuses_a_frozen_enforcement_flag() {
    use reliaburger::sesame::egress;
    assert!(ebpf_tests_enabled());
    let mut ebpf = OnionEbpf::load(&find_bpf_obj_dir(), CGROUP_PATH.as_ref()).unwrap();
    let cgroup = 0xDEAD_BEEF_CAFE_6102;
    egress::set_egress_enforced(&mut ebpf.bpf, cgroup).unwrap();
    freeze_egress_map(&ebpf, "egress_enabled_map");
    let result = egress::delete_cgroup_egress_state(&mut ebpf.bpf, cgroup);
    let still_enforced = egress::egress_enforced(&mut ebpf.bpf, cgroup).unwrap();
    ebpf.detach().unwrap();
    assert!(
        result.is_err(),
        "cleanup ignored a kernel enforcement deletion refusal"
    );
    assert!(still_enforced);
}

#[tokio::test]
#[ignore = "requires Linux root and RELIABURGER_EBPF_TESTS=1"]
async fn agent_retirement_keeps_its_record_when_kernel_egress_cleanup_fails() {
    use reliaburger::bun::agent::{AgentCommand, ApplyEvent, BunAgent};
    use reliaburger::config::Config;
    use reliaburger::grill::mock::MockGrill;
    use reliaburger::grill::port::PortAllocator;
    use std::sync::Arc;
    use tokio::sync::{Mutex, mpsc, oneshot};
    assert!(ebpf_tests_enabled());
    let root = tempfile::tempdir().unwrap();
    let records = root.path().join("records");
    std::fs::create_dir(&records).unwrap();
    let ebpf = Arc::new(Mutex::new(
        OnionEbpf::load(&find_bpf_obj_dir(), CGROUP_PATH.as_ref()).unwrap(),
    ));
    let grill = MockGrill::new();
    grill.set_honours_cgroup_path(true);
    grill.set_pid(std::process::id());
    let (commands, receiver) = mpsc::channel(64);
    let shutdown = CancellationToken::new();
    let mut agent = BunAgent::new(
        grill,
        PortAllocator::new(43400, 43500),
        receiver,
        shutdown.clone(),
    );
    agent.set_records_dir(records.clone());
    agent.set_volumes_dir(root.path().join("volumes"));
    agent.set_onion_ebpf(Arc::clone(&ebpf)).await;
    let task = tokio::spawn(async move { agent.run().await });
    let config = Config::parse(
        r#"
        [app.egress-retirement]
        image = "mock:image"
        command = ["sleep", "600"]
        [app.egress-retirement.egress]
        allow = ["203.0.113.9:443"]
    "#,
    )
    .unwrap();
    let (events, mut results) = mpsc::channel(64);
    commands
        .send(AgentCommand::Deploy { config, events })
        .await
        .unwrap();
    while let Some(event) = results.recv().await {
        assert!(!matches!(event, ApplyEvent::Error { .. }), "{event:?}");
    }
    let record = reliaburger::grill::records::record_path(&records, "default__egress-retirement-0");
    assert!(
        record.exists(),
        "deployment did not persist its adoption record"
    );
    freeze_egress_map(&*ebpf.lock().await, "egress_map");
    let (response, result) = oneshot::channel();
    commands
        .send(AgentCommand::Retire {
            app_name: "egress-retirement".into(),
            namespace: "default".into(),
            response,
        })
        .await
        .unwrap();
    let result = tokio::time::timeout(Duration::from_secs(10), result)
        .await
        .unwrap()
        .unwrap();
    let retained_record = record.exists();
    let (response, retry) = oneshot::channel();
    commands
        .send(AgentCommand::Retire {
            app_name: "egress-retirement".into(),
            namespace: "default".into(),
            response,
        })
        .await
        .unwrap();
    let retry = tokio::time::timeout(Duration::from_secs(10), retry)
        .await
        .unwrap()
        .unwrap();
    let retained_after_retry = record.exists();
    let (response, status) = oneshot::channel();
    commands
        .send(AgentCommand::Status { response })
        .await
        .unwrap();
    let retained_instances = status.await.unwrap();
    shutdown.cancel();
    task.await.unwrap();
    ebpf.lock().await.detach().unwrap();
    assert!(result.is_err(), "retirement accepted failed kernel cleanup");
    assert!(
        retained_record,
        "retirement discarded the durable cleanup owner"
    );
    assert!(retry.is_err(), "retry forgot the failed kernel binding");
    assert!(
        retained_after_retry,
        "retry discarded the durable cleanup owner"
    );
    assert_eq!(
        retained_instances.len(),
        1,
        "retirement forgot its stopped cleanup owner"
    );
}

struct OwnedPolicyFixture {
    root: tempfile::TempDir,
    cgroup: PathBuf,
    pin: PathBuf,
    child: Option<std::process::Child>,
}

impl OwnedPolicyFixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let suffix = root
            .path()
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .trim_start_matches('.');
        let cgroup = PathBuf::from(format!("/sys/fs/cgroup/rbtest-egress-owner-{suffix}"));
        let pin = PathBuf::from(format!("/sys/fs/bpf/rbtest-egress-owner-{suffix}"));
        std::fs::create_dir(&cgroup).unwrap();
        std::fs::create_dir(cgroup.join("probe")).unwrap();
        Self {
            root,
            cgroup,
            pin,
            child: None,
        }
    }

    fn load(&self) -> Result<OnionEbpf, reliaburger::onion::types::OnionError> {
        OnionEbpf::load_owned(
            None,
            &self.cgroup,
            &self.root.path().join("ownership"),
            &self.pin,
        )
    }

    fn stop_child(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            child.wait().unwrap();
        }
    }
}

impl Drop for OwnedPolicyFixture {
    fn drop(&mut self) {
        self.stop_child();
        // Private test infrastructure only. Release the loader before the fixture
        // so no live process retains its exclusive claim during cleanup.
        if self.root.path().join("ownership/owner.json").exists()
            && let Err(error) = OnionEbpf::retire_owned_state(
                &self.cgroup,
                &self.root.path().join("ownership"),
                &self.pin,
            )
        {
            eprintln!("test kernel ownership cleanup failed: {error}");
        }
        let _ = std::fs::remove_dir(&self.pin);
        let _ = std::fs::remove_dir(self.cgroup.join("probe"));
        let _ = std::fs::remove_dir(&self.cgroup);
    }
}

#[test]
#[ignore = "requires Linux root and RELIABURGER_EBPF_TESTS=1"]
fn egress_policy_survives_actual_loader_process_death() {
    use std::process::Command;
    assert!(ebpf_tests_enabled());
    let mut owned = OwnedPolicyFixture::new();
    let root = owned.root.path().to_owned();
    let cgroup = owned.cgroup.clone();
    let leaf = cgroup.join("probe");
    let pin = owned.pin.clone();
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let fixture = |mode: &str| {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args([
                "--exact",
                "egress_owner_process_fixture",
                "--ignored",
                "--nocapture",
            ])
            .env("RELIABURGER_EGRESS_OWNER_FIXTURE", root.as_path())
            .env("RELIABURGER_EGRESS_OWNER_CGROUP", &cgroup)
            .env("RELIABURGER_EGRESS_OWNER_MODE", mode)
            .env("RELIABURGER_EGRESS_OWNER_PORT", port.to_string());
        command
    };
    owned.child = Some(
        fixture("owner")
            .env("RELIABURGER_EGRESS_OWNER_PINS", &pin)
            .spawn()
            .unwrap(),
    );
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while !root.as_path().join("ready").exists() {
        assert!(
            std::time::Instant::now() < deadline,
            "loader never became ready"
        );
        assert!(
            owned.child.as_mut().unwrap().try_wait().unwrap().is_none(),
            "loader exited before readiness"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(fixture("probe").status().unwrap().success());
    let before = std::fs::read_to_string(root.as_path().join("result")).unwrap();
    owned.stop_child();
    assert!(fixture("probe").status().unwrap().success());
    let after = std::fs::read_to_string(root.as_path().join("result")).unwrap();
    let mut recovered =
        OnionEbpf::load_owned(None, &cgroup, &root.as_path().join("ownership"), &pin).unwrap();
    assert!(fixture("probe").status().unwrap().success());
    let restored = std::fs::read_to_string(root.as_path().join("result")).unwrap();
    let id = reliaburger::sesame::egress::cgroup_id_of_path(&leaf).unwrap();
    reliaburger::sesame::egress::delete_cgroup_egress_state(&mut recovered.bpf, id).unwrap();
    assert!(fixture("probe").status().unwrap().success());
    let removed = std::fs::read_to_string(root.as_path().join("result")).unwrap();
    recovered.retire_owned().unwrap();
    drop(recovered);
    assert_eq!(std::fs::read_dir(&pin).unwrap().count(), 0);
    assert_eq!(
        restored, "PermissionDenied",
        "recovery discarded the retained policy"
    );
    assert_eq!(
        removed, "connected",
        "recovered maps did not control the live policy"
    );
    assert_eq!(
        before, "PermissionDenied",
        "the live loader did not enforce the policy"
    );
    assert_eq!(
        after, "PermissionDenied",
        "the policy disappeared when its loader died"
    );
}

#[test]
#[ignore = "subprocess fixture for actual egress owner death"]
fn egress_owner_process_fixture() {
    let Some(root) = std::env::var_os("RELIABURGER_EGRESS_OWNER_FIXTURE") else {
        return;
    };
    let root = PathBuf::from(root);
    let cgroup = PathBuf::from(std::env::var_os("RELIABURGER_EGRESS_OWNER_CGROUP").unwrap());
    let leaf = cgroup.join("probe");
    if std::env::var("RELIABURGER_EGRESS_OWNER_MODE").unwrap() == "probe" {
        std::fs::write(leaf.join("cgroup.procs"), std::process::id().to_string()).unwrap();
        let port: u16 = std::env::var("RELIABURGER_EGRESS_OWNER_PORT")
            .unwrap()
            .parse()
            .unwrap();
        let result = match TcpStream::connect_timeout(
            &std::net::SocketAddr::from(([127, 0, 0, 1], port)),
            Duration::from_secs(2),
        ) {
            Ok(_) => "connected".to_string(),
            Err(error) => format!("{:?}", error.kind()),
        };
        std::fs::write(root.join("result"), result).unwrap();
        return;
    }
    let pin = PathBuf::from(std::env::var_os("RELIABURGER_EGRESS_OWNER_PINS").unwrap());
    let mut ebpf = OnionEbpf::load_owned(None, &cgroup, &root.join("ownership"), &pin).unwrap();
    let id = reliaburger::sesame::egress::cgroup_id_of_path(&leaf).unwrap();
    reliaburger::sesame::egress::set_egress_enforced(&mut ebpf.bpf, id).unwrap();
    std::fs::write(root.join("ready"), "ready").unwrap();
    loop {
        std::thread::park();
    }
}

#[test]
#[ignore = "requires Linux root and RELIABURGER_EBPF_TESTS=1"]
fn persistent_policy_refuses_conflicting_owners_and_retired_state() {
    assert!(ebpf_tests_enabled());
    let owned = OwnedPolicyFixture::new();
    let mut loader = owned.load().unwrap();
    assert!(
        loader.detach().is_err(),
        "ephemeral detach accepted a persistent owner"
    );
    assert!(loader.is_attached());
    assert!(
        owned.load().is_err(),
        "two loaders acquired one ownership claim"
    );
    assert!(
        OnionEbpf::load_owned(
            None,
            &owned.cgroup,
            &owned.root.path().join("foreign"),
            &owned.pin
        )
        .is_err()
    );
    drop(loader);
    assert!(
        OnionEbpf::load_owned(
            None,
            &owned.cgroup.join("probe"),
            &owned.root.path().join("ownership"),
            &owned.pin
        )
        .is_err()
    );
    loader = owned.load().unwrap();
    assert!(
        loader.is_attached()
            && loader.connect6_attached()
            && loader.sendmsg4_attached()
            && loader.sendmsg6_attached()
    );
    loader.retire_owned().unwrap();
    assert!(!loader.is_attached());
    drop(loader);
    assert!(owned.load().is_err(), "retired ownership was reused");
    OnionEbpf::retire_owned_state(
        &owned.cgroup,
        &owned.root.path().join("ownership"),
        &owned.pin,
    )
    .unwrap();
    assert_eq!(std::fs::read_dir(&owned.pin).unwrap().count(), 0);
}

#[test]
#[ignore = "requires Linux root and RELIABURGER_EBPF_TESTS=1"]
fn persistent_policy_refuses_obsolete_destination_identity() {
    assert!(ebpf_tests_enabled());
    let owned = OwnedPolicyFixture::new();
    drop(owned.load().unwrap());
    let path = owned.root.path().join("ownership/owner.json");
    let original = std::fs::read(&path).unwrap();
    let mut obsolete: serde_json::Value = serde_json::from_slice(&original).unwrap();
    obsolete["version"] = 1.into();
    std::fs::write(&path, serde_json::to_vec(&obsolete).unwrap()).unwrap();
    let result = owned.load();
    let refused = result.is_err();
    drop(result);
    // Restore only this fixture's original ownership before its explicit cleanup.
    std::fs::write(path, original).unwrap();
    assert!(
        refused,
        "retained bare-name firewall identities were accepted"
    );
}

#[test]
#[ignore = "requires Linux root and RELIABURGER_EBPF_TESTS=1"]
fn persistent_policy_recovers_partial_startup_and_interrupted_retirement() {
    assert!(ebpf_tests_enabled());
    let owned = OwnedPolicyFixture::new();
    let loader = owned.load().unwrap();
    drop(loader);
    let manifest_path = owned.root.path().join("ownership/owner.json");
    let mut manifest: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&manifest_path).unwrap()).unwrap();
    manifest["phase"] = "Preparing".into();
    std::fs::write(&manifest_path, serde_json::to_vec(&manifest).unwrap()).unwrap();
    std::fs::remove_file(owned.pin.join("onion_sendmsg4_link")).unwrap();
    std::fs::remove_file(owned.pin.join("onion_sendmsg6_link")).unwrap();
    let loader = owned.load().unwrap();
    assert!(loader.is_attached() && loader.sendmsg4_attached() && loader.sendmsg6_attached());
    std::fs::remove_file(owned.pin.join("onion_sendmsg6_link")).unwrap();
    assert!(
        !loader.sendmsg6_attached(),
        "missing pin still advertised durable enforcement"
    );
    drop(loader);
    assert!(
        owned.load().is_err(),
        "active ownership silently recreated a missing link"
    );
    manifest["phase"] = "Retiring".into();
    std::fs::write(&manifest_path, serde_json::to_vec(&manifest).unwrap()).unwrap();
    assert!(
        owned.load().is_err(),
        "interrupted retirement allowed reactivation"
    );
    OnionEbpf::retire_owned_state(
        &owned.cgroup,
        &owned.root.path().join("ownership"),
        &owned.pin,
    )
    .unwrap();
    assert_eq!(std::fs::read_dir(&owned.pin).unwrap().count(), 0);
}

#[test]
#[ignore = "requires Linux root and RELIABURGER_EBPF_TESTS=1"]
fn persistent_policy_refuses_wrong_map_layout_and_foreign_map_identity() {
    assert!(ebpf_tests_enabled());
    for foreign_name in ["backend_map", "egress_map"] {
        let owned = OwnedPolicyFixture::new();
        let loader = owned.load().unwrap();
        drop(loader);
        let foreign_cgroup = owned.cgroup.join("foreign");
        std::fs::create_dir(&foreign_cgroup).unwrap();
        let mut foreign = OnionEbpf::load_embedded(&foreign_cgroup).unwrap();
        let path = owned.pin.join("egress_map");
        std::fs::remove_file(&path).unwrap();
        foreign.bpf.map(foreign_name).unwrap().pin(&path).unwrap();
        let error = owned
            .load()
            .err()
            .expect("foreign map accepted")
            .to_string();
        foreign.detach().unwrap();
        drop(foreign);
        std::fs::remove_dir(&foreign_cgroup).unwrap();
        assert!(
            error.contains(if foreign_name == "backend_map" {
                "incompatible ABI"
            } else {
                "expected maps"
            }),
            "{error}"
        );
        OnionEbpf::retire_owned_state(
            &owned.cgroup,
            &owned.root.path().join("ownership"),
            &owned.pin,
        )
        .unwrap();
        assert_eq!(std::fs::read_dir(&owned.pin).unwrap().count(), 0);
    }
}

#[tokio::test]
#[ignore = "requires Linux root and RELIABURGER_EBPF_TESTS=1"]
async fn agent_adoption_restores_durable_egress_ownership_before_live_checks() {
    use reliaburger::bun::agent::{AgentCommand, ApplyEvent, BunAgent};
    use reliaburger::config::Config;
    use reliaburger::grill::{InstanceId, mock::MockGrill, port::PortAllocator};
    use std::sync::Arc;
    use tokio::sync::{Mutex, mpsc, oneshot};
    assert!(ebpf_tests_enabled());
    let root = tempfile::tempdir().unwrap();
    let records = root.path().join("records");
    std::fs::create_dir(&records).unwrap();
    let ebpf = Arc::new(Mutex::new(
        OnionEbpf::load_embedded(CGROUP_PATH.as_ref()).unwrap(),
    ));
    let grill = MockGrill::new();
    grill.set_honours_cgroup_path(true);
    grill.set_pid(std::process::id());
    let (commands, receiver) = mpsc::channel(64);
    let mut agent = BunAgent::new(
        grill.clone(),
        PortAllocator::new(43400, 43500),
        receiver,
        CancellationToken::new(),
    );
    agent.set_records_dir(records.clone());
    agent.set_volumes_dir(root.path().join("volumes"));
    agent.set_onion_ebpf(Arc::clone(&ebpf)).await;
    let task = tokio::spawn(async move { agent.run().await });
    let config = Config::parse(
        "[app.egress-adoption]\nimage = 'mock:image'\ncommand = ['sleep', '600']\n[app.egress-adoption.egress]\nallow = ['203.0.113.9:443']\n",
    ).unwrap();
    let (events, mut results) = mpsc::channel(64);
    commands
        .send(AgentCommand::Deploy { config, events })
        .await
        .unwrap();
    while let Some(event) = results.recv().await {
        assert!(!matches!(event, ApplyEvent::Error { .. }), "{event:?}");
    }
    let checkpoint_present = records.join("egress-owners.checkpoint").exists();
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    let id = InstanceId("default__egress-adoption-0".into());
    grill.set_adopt_result(&id, true);
    let shutdown = CancellationToken::new();
    let (commands, receiver) = mpsc::channel(64);
    let mut restored = BunAgent::new(
        grill,
        PortAllocator::new(43400, 43500),
        receiver,
        shutdown.clone(),
    );
    restored.set_records_dir(records.clone());
    restored.set_volumes_dir(root.path().join("volumes"));
    restored.set_onion_ebpf(Arc::clone(&ebpf)).await;
    assert_eq!(restored.adopt_recorded_instances().await.unwrap(), 1);
    let task = tokio::spawn(async move { restored.run().await });
    // Cross two one-second health observations. A missing restored binding
    // previously caused the live checker to stop an otherwise healthy owner.
    tokio::time::sleep(Duration::from_millis(1200)).await;
    let (response, status) = oneshot::channel();
    commands
        .send(AgentCommand::Status { response })
        .await
        .unwrap();
    let instances = status.await.unwrap();
    let running = instances
        .iter()
        .any(|instance| instance.id == id.0 && instance.state == "running");
    let (response, retired) = oneshot::channel();
    commands
        .send(AgentCommand::Retire {
            app_name: "egress-adoption".into(),
            namespace: "default".into(),
            response,
        })
        .await
        .unwrap();
    retired.await.unwrap().unwrap();
    shutdown.cancel();
    task.await.unwrap();
    ebpf.lock().await.detach().unwrap();
    assert!(
        checkpoint_present,
        "policy was programmed without durable workload ownership"
    );
    assert!(
        running,
        "adoption did not restore policy ownership before live checks"
    );
}

struct EgressRecoveryFixture {
    root: tempfile::TempDir,
    ebpf: std::sync::Arc<tokio::sync::Mutex<OnionEbpf>>,
    grill: reliaburger::grill::mock::MockGrill,
    commands: tokio::sync::mpsc::Sender<reliaburger::bun::agent::AgentCommand>,
    task: Option<tokio::task::JoinHandle<()>>,
}

impl EgressRecoveryFixture {
    async fn deploy(name: &str) -> Self {
        Self::prepare(name, false).await
    }

    async fn prepare(name: &str, failed_checkpoint: bool) -> Self {
        Self::prepare_with_service(name, failed_checkpoint, false).await
    }

    async fn prepare_with_service(name: &str, failed_checkpoint: bool, service: bool) -> Self {
        Self::prepare_with_policy(name, failed_checkpoint, service, true).await
    }

    async fn prepare_with_policy(
        name: &str,
        failed_checkpoint: bool,
        service: bool,
        allowlist: bool,
    ) -> Self {
        use reliaburger::bun::agent::{AgentCommand, ApplyEvent};
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir(root.path().join("records")).unwrap();
        if failed_checkpoint {
            std::fs::create_dir(root.path().join("records/egress-owners.checkpoint")).unwrap();
        }
        let ebpf = std::sync::Arc::new(tokio::sync::Mutex::new(
            OnionEbpf::load_embedded(CGROUP_PATH.as_ref()).unwrap(),
        ));
        let grill = reliaburger::grill::mock::MockGrill::new();
        grill.set_honours_cgroup_path(true);
        grill.set_pid(std::process::id());
        let (commands, _) = tokio::sync::mpsc::channel(64);
        let mut fixture = Self {
            root,
            ebpf,
            grill,
            commands,
            task: None,
        };
        let (mut agent, commands, _) = fixture.agent().await;
        fixture.commands = commands;
        fixture.task = Some(tokio::spawn(async move { agent.run().await }));
        let port = if service { "port = 8080\n" } else { "" };
        let policy = if allowlist {
            format!("[app.{name}.egress]\nallow = ['203.0.113.9:443']\n")
        } else {
            String::new()
        };
        let config = reliaburger::config::Config::parse(&format!(
            "[app.{name}]\nimage = 'mock:image'\ncommand = ['sleep', '600']\n{port}{policy}"
        ))
        .unwrap();
        let (events, mut results) = tokio::sync::mpsc::channel(64);
        fixture
            .commands
            .send(AgentCommand::Deploy { config, events })
            .await
            .unwrap();
        let mut failed = false;
        while let Some(event) = results.recv().await {
            if matches!(event, ApplyEvent::Error { .. }) {
                failed = true;
            }
        }
        assert_eq!(failed, failed_checkpoint);

        fixture
    }

    async fn agent(
        &self,
    ) -> (
        reliaburger::bun::agent::BunAgent<reliaburger::grill::mock::MockGrill>,
        tokio::sync::mpsc::Sender<reliaburger::bun::agent::AgentCommand>,
        CancellationToken,
    ) {
        let (commands, receiver) = tokio::sync::mpsc::channel(64);
        let shutdown = CancellationToken::new();
        let mut agent = reliaburger::bun::agent::BunAgent::new(
            self.grill.clone(),
            reliaburger::grill::port::PortAllocator::new(43400, 43500),
            receiver,
            shutdown.clone(),
        );
        agent.set_records_dir(self.root.path().join("records"));
        agent.set_volumes_dir(self.root.path().join("volumes"));
        agent
            .set_onion_ebpf(std::sync::Arc::clone(&self.ebpf))
            .await;
        (agent, commands, shutdown)
    }

    async fn crash(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
            assert!(task.await.unwrap_err().is_cancelled());
        }
    }

    async fn retire(&self, name: &str) -> Result<(), reliaburger::bun::BunError> {
        let (response, result) = tokio::sync::oneshot::channel();
        self.commands
            .send(reliaburger::bun::agent::AgentCommand::Retire {
                app_name: name.into(),
                namespace: "default".into(),
                response,
            })
            .await
            .unwrap();
        result.await.unwrap()
    }
}

impl Drop for EgressRecoveryFixture {
    fn drop(&mut self) {
        if let Some(task) = &self.task {
            task.abort();
        }
        if let Ok(entries) = std::fs::read_dir(self.root.path().join("volumes/.identity")) {
            for entry in entries.flatten() {
                if let Err(error) =
                    reliaburger::sesame::identity::cleanup_identity_dir(&entry.path())
                {
                    eprintln!("test identity cleanup failed: {error}");
                }
            }
        }
    }
}

#[tokio::test]
#[ignore = "requires Linux root and RELIABURGER_EBPF_TESTS=1"]
async fn confirmed_egress_retirement_survives_interrupted_adoption_record_cleanup() {
    assert!(ebpf_tests_enabled());
    let mut fixture = EgressRecoveryFixture::deploy("egress-tombstone").await;
    let identity = reliaburger::sesame::identity::instance_identity_dir(
        &fixture.root.path().join("volumes"),
        "default__egress-tombstone-0",
    );
    std::fs::create_dir_all(identity.parent().unwrap()).unwrap();
    reliaburger::sesame::identity::cleanup_identity_dir(&identity).unwrap();
    std::fs::write(&identity, b"block identity cleanup").unwrap();
    assert!(fixture.retire("egress-tombstone").await.is_err());
    let records = fixture.root.path().join("records");
    let document: serde_json::Value =
        serde_json::from_slice(&std::fs::read(records.join("egress-owners.checkpoint")).unwrap())
            .unwrap();
    let tombstone = document["owners"].as_array().unwrap().iter().any(|entry| {
        entry["instance_id"] == "default__egress-tombstone-0"
            && entry["binding"]["phase"] == "Retired"
    });
    let record = reliaburger::grill::records::record_path(&records, "default__egress-tombstone-0");
    assert!(record.exists());
    fixture.crash().await;
    std::fs::remove_file(identity).unwrap();
    let (mut restored, _, _) = fixture.agent().await;
    assert_eq!(restored.adopt_recorded_instances().await.unwrap(), 0);
    assert!(!record.exists());
    fixture.ebpf.lock().await.detach().unwrap();
    assert!(
        tombstone,
        "confirmed kernel retirement disappeared before adoption cleanup completed"
    );
}

#[tokio::test]
#[ignore = "requires Linux root and RELIABURGER_EBPF_TESTS=1"]
async fn missing_policy_owner_refuses_even_a_stopped_recorded_runtime() {
    assert!(ebpf_tests_enabled());
    let mut fixture = EgressRecoveryFixture::deploy("egress-missing-owner").await;
    fixture.crash().await;
    let records = fixture.root.path().join("records");
    std::fs::remove_file(records.join("egress-owners.checkpoint")).unwrap();
    let (mut restored, _, _) = fixture.agent().await;
    let result = restored.adopt_recorded_instances().await;
    let retained =
        reliaburger::grill::records::record_path(&records, "default__egress-missing-owner-0")
            .exists();
    fixture.ebpf.lock().await.detach().unwrap();
    assert!(
        result.is_err(),
        "runtime absence substituted for lost kernel ownership"
    );
    assert!(
        retained,
        "missing policy ownership discarded the adoption record"
    );
}

#[tokio::test]
#[ignore = "requires Linux root and RELIABURGER_EBPF_TESTS=1"]
async fn failed_policy_checkpoint_prevents_kernel_programming_and_workload_start() {
    assert!(ebpf_tests_enabled());
    let mut fixture = EgressRecoveryFixture::prepare("egress-checkpoint-failure", true).await;
    let never_started = !fixture
        .grill
        .calls()
        .iter()
        .any(|(operation, _)| operation == "start");
    let path = reliaburger::grill::cgroup::cgroup_path("default", "egress-checkpoint-failure", 0);
    let id = reliaburger::sesame::egress::cgroup_id_of_path(&path).unwrap();
    let enforced =
        reliaburger::sesame::egress::egress_enforced(&mut fixture.ebpf.lock().await.bpf, id)
            .unwrap();
    fixture.crash().await;
    fixture.ebpf.lock().await.detach().unwrap();
    assert!(
        never_started,
        "workload started despite failed policy ownership persistence"
    );
    assert!(
        !enforced,
        "kernel policy was programmed before durable ownership"
    );
}

#[tokio::test]
#[ignore = "requires Linux root and RELIABURGER_EBPF_TESTS=1"]
async fn agent_recovery_keeps_policy_ownership_when_kernel_retirement_is_refused() {
    assert!(ebpf_tests_enabled());
    let mut fixture = EgressRecoveryFixture::deploy("egress-recovery-cleanup").await;
    fixture.crash().await;
    freeze_egress_map(&*fixture.ebpf.lock().await, "egress_map");
    let records = fixture.root.path().join("records");
    let (mut restored, _, _) = fixture.agent().await;
    assert!(restored.adopt_recorded_instances().await.is_err());
    assert!(restored.adopt_recorded_instances().await.is_err());
    let retained_record =
        reliaburger::grill::records::record_path(&records, "default__egress-recovery-cleanup-0")
            .exists();
    let document: serde_json::Value =
        serde_json::from_slice(&std::fs::read(records.join("egress-owners.checkpoint")).unwrap())
            .unwrap();
    let retained_owner = document["owners"]
        .as_array()
        .unwrap()
        .iter()
        .any(|entry| entry["binding"]["phase"] == "Owned");
    fixture.ebpf.lock().await.detach().unwrap();
    assert!(
        retained_record && retained_owner,
        "recovery forgot unconfirmed kernel cleanup"
    );
}

#[tokio::test]
#[ignore = "requires Linux root and RELIABURGER_EBPF_TESTS=1"]
async fn adoption_fences_missing_enforcement_before_publishing_the_workload() {
    use reliaburger::sesame::egress;
    assert!(ebpf_tests_enabled());
    let mut fixture = EgressRecoveryFixture::deploy("egress-missing-flag").await;
    fixture.crash().await;
    let id = reliaburger::grill::InstanceId("default__egress-missing-flag-0".into());
    fixture.grill.set_adopt_result(&id, true);
    let path = reliaburger::grill::cgroup::cgroup_path("default", "egress-missing-flag", 0);
    let cgroup = egress::cgroup_id_of_path(&path).unwrap();
    egress::clear_egress_enforced(&mut fixture.ebpf.lock().await.bpf, cgroup).unwrap();
    let (mut restored, _, _) = fixture.agent().await;
    let result = restored.adopt_recorded_instances().await;
    let killed = fixture
        .grill
        .calls()
        .iter()
        .any(|(operation, instance)| operation == "kill" && instance == &id);
    fixture.grill.set_adopt_result(&id, false);
    assert_eq!(restored.adopt_recorded_instances().await.unwrap(), 0);
    let records = fixture.root.path().join("records");
    assert!(!reliaburger::grill::records::record_path(&records, &id.0).exists());
    fixture.ebpf.lock().await.detach().unwrap();
    assert!(
        result.is_err() && killed,
        "unprotected adoption did not fence its runtime"
    );
}

#[tokio::test]
#[ignore = "requires Linux root and RELIABURGER_EBPF_TESTS=1"]
async fn agent_namespace_binding_uses_the_workload_cgroup_instead_of_its_launcher() {
    use reliaburger::sesame::{egress, firewall};
    assert!(ebpf_tests_enabled());
    let mut fixture =
        EgressRecoveryFixture::prepare_with_service("source-cgroup", false, true).await;
    let path = reliaburger::grill::cgroup::cgroup_path("default", "source-cgroup", 0);
    let workload = egress::cgroup_id_of_path(&path).unwrap();
    let launcher = egress::cgroup_id_of_pid(std::process::id()).unwrap();
    let (workload_namespace, launcher_namespace) = {
        let mut ebpf = fixture.ebpf.lock().await;
        (
            firewall::read_firewall_state(&mut ebpf.bpf, workload, 0)
                .unwrap()
                .source_namespace_id,
            firewall::read_firewall_state(&mut ebpf.bpf, launcher, 0)
                .unwrap()
                .source_namespace_id,
        )
    };
    fixture.retire("source-cgroup").await.unwrap();
    fixture.crash().await;
    fixture.ebpf.lock().await.detach().unwrap();
    std::fs::remove_dir(path).unwrap();
    assert_ne!(workload, launcher);
    assert_eq!(
        workload_namespace,
        Some(reliaburger::onion::vip::name_to_id("default"))
    );
    assert_eq!(launcher_namespace, None);
}

#[test]
#[ignore = "requires Linux root and RELIABURGER_EBPF_TESTS=1"]
fn firewall_cleanup_refuses_frozen_maps_and_preserves_their_entries() {
    use reliaburger::onion::types::{FirewallKey, FirewallValue};
    use reliaburger::sesame::firewall;
    assert!(ebpf_tests_enabled());
    let mut ebpf = OnionEbpf::load(&find_bpf_obj_dir(), CGROUP_PATH.as_ref()).unwrap();
    let cgroup = 0xDEAD_BEEF_CAFE_6201;
    let key = FirewallKey {
        src_cgroup_id: cgroup,
        dst_app_id: 37,
        _pad: 0,
    };
    firewall::write_cgroup_namespace_entry(&mut ebpf.bpf, cgroup, 19).unwrap();
    firewall::write_firewall_entry(
        &mut ebpf.bpf,
        key,
        FirewallValue {
            action: firewall::FIREWALL_ALLOW,
        },
    )
    .unwrap();
    freeze_egress_map(&ebpf, "firewall_map");
    freeze_egress_map(&ebpf, "cgroup_namespace_map");
    let allow_removed = firewall::delete_firewall_entry(&mut ebpf.bpf, key);
    let namespace_removed = firewall::delete_cgroup_namespace_entry(&mut ebpf.bpf, cgroup);
    let retained = firewall::read_firewall_state(&mut ebpf.bpf, cgroup, key.dst_app_id).unwrap();
    ebpf.detach().unwrap();
    assert!(allow_removed.is_err(), "accepted refused firewall removal");
    assert!(
        namespace_removed.is_err(),
        "accepted refused namespace removal"
    );
    assert_eq!(retained.source_namespace_id, Some(19));
    assert_eq!(retained.action, Some(firewall::FIREWALL_ALLOW));
}

#[test]
#[ignore = "requires Linux root and RELIABURGER_EBPF_TESTS=1"]
fn firewall_cleanup_confirms_removal_and_accepts_already_absent_entries() {
    use reliaburger::onion::types::{FirewallKey, FirewallValue};
    use reliaburger::sesame::firewall;
    assert!(ebpf_tests_enabled());
    let mut ebpf = OnionEbpf::load(&find_bpf_obj_dir(), CGROUP_PATH.as_ref()).unwrap();
    let cgroup = 0xDEAD_BEEF_CAFE_6202;
    let key = FirewallKey {
        src_cgroup_id: cgroup,
        dst_app_id: 37,
        _pad: 0,
    };
    firewall::write_cgroup_namespace_entry(&mut ebpf.bpf, cgroup, 19).unwrap();
    firewall::write_firewall_entry(
        &mut ebpf.bpf,
        key,
        FirewallValue {
            action: firewall::FIREWALL_ALLOW,
        },
    )
    .unwrap();
    for _ in 0..2 {
        firewall::delete_firewall_entry(&mut ebpf.bpf, key).unwrap();
        firewall::delete_cgroup_namespace_entry(&mut ebpf.bpf, cgroup).unwrap();
        let state = firewall::read_firewall_state(&mut ebpf.bpf, cgroup, key.dst_app_id).unwrap();
        assert_eq!(state.source_namespace_id, None);
        assert_eq!(state.action, None);
    }
    assert!(
        !firewall::list_cgroup_namespace_keys(&mut ebpf.bpf)
            .unwrap()
            .contains(&cgroup)
    );
    ebpf.detach().unwrap();
}

#[test]
#[ignore = "requires Linux root and RELIABURGER_EBPF_TESTS=1"]
fn firewall_reconciliation_retains_refused_cleanup_for_repeated_attempts() {
    use reliaburger::onion::types::{FirewallKey, FirewallValue};
    use reliaburger::sesame::firewall::{self, CgroupNamespaceEntry};
    assert!(ebpf_tests_enabled());
    for frozen in ["firewall_map", "cgroup_namespace_map"] {
        let mut ebpf = OnionEbpf::load(&find_bpf_obj_dir(), CGROUP_PATH.as_ref()).unwrap();
        let cgroup = 0xDEAD_BEEF_CAFE_6301;
        let key = FirewallKey {
            src_cgroup_id: cgroup,
            dst_app_id: 37,
            _pad: 0,
        };
        let mut namespaces = Default::default();
        let mut rules = Default::default();
        firewall::reconcile_firewall_maps(
            &mut ebpf.bpf,
            &[CgroupNamespaceEntry {
                cgroup_id: cgroup,
                namespace_id: 19,
            }],
            &[(
                key,
                FirewallValue {
                    action: firewall::FIREWALL_ALLOW,
                },
            )],
            &mut namespaces,
            &mut rules,
        )
        .unwrap();
        freeze_egress_map(&ebpf, frozen);
        for _ in 0..2 {
            let result = firewall::reconcile_firewall_maps(
                &mut ebpf.bpf,
                &[],
                &[],
                &mut namespaces,
                &mut rules,
            );
            let retained =
                firewall::read_firewall_state(&mut ebpf.bpf, cgroup, key.dst_app_id).unwrap();
            assert!(result.is_err(), "forgot refused cleanup in {frozen}");
            assert!(namespaces.contains(&cgroup));
            assert_eq!(retained.source_namespace_id, Some(19));
            if frozen == "firewall_map" {
                assert!(rules.contains(&key));
                assert_eq!(retained.action, Some(firewall::FIREWALL_ALLOW));
            } else {
                assert!(!rules.contains(&key));
                assert_eq!(retained.action, None);
            }
        }
        ebpf.detach().unwrap();
    }
}

#[test]
#[ignore = "requires Linux root and RELIABURGER_EBPF_TESTS=1"]
fn firewall_reconciliation_retains_partial_publication_and_unrelated_entries() {
    use reliaburger::onion::types::{FirewallKey, FirewallValue};
    use reliaburger::sesame::firewall::{self, CgroupNamespaceEntry};
    assert!(ebpf_tests_enabled());
    let mut ebpf = OnionEbpf::load(&find_bpf_obj_dir(), CGROUP_PATH.as_ref()).unwrap();
    let cgroup = 0xDEAD_BEEF_CAFE_6302;
    let other = cgroup + 1;
    let key = FirewallKey {
        src_cgroup_id: cgroup,
        dst_app_id: 37,
        _pad: 0,
    };
    let mut namespaces = Default::default();
    let mut rules = Default::default();
    firewall::write_cgroup_namespace_entry(&mut ebpf.bpf, other, 23).unwrap();
    freeze_egress_map(&ebpf, "firewall_map");
    let result = firewall::reconcile_firewall_maps(
        &mut ebpf.bpf,
        &[CgroupNamespaceEntry {
            cgroup_id: cgroup,
            namespace_id: 19,
        }],
        &[(
            key,
            FirewallValue {
                action: firewall::FIREWALL_ALLOW,
            },
        )],
        &mut namespaces,
        &mut rules,
    );
    let state = firewall::read_firewall_state(&mut ebpf.bpf, cgroup, key.dst_app_id).unwrap();
    let unrelated = firewall::read_firewall_state(&mut ebpf.bpf, other, 0).unwrap();
    ebpf.detach().unwrap();
    assert!(result.is_err());
    assert!(namespaces.contains(&cgroup));
    assert!(rules.contains(&key), "lost attempted publication");
    assert!(!namespaces.contains(&other), "claimed an unrelated entry");
    assert_eq!(state.source_namespace_id, Some(19));
    assert_eq!(state.action, None);
    assert_eq!(unrelated.source_namespace_id, Some(23));
}

#[test]
#[ignore = "requires Linux root and RELIABURGER_EBPF_TESTS=1"]
fn firewall_reconciliation_forgets_confirmed_removals_only() {
    use reliaburger::onion::types::{FirewallKey, FirewallValue};
    use reliaburger::sesame::firewall::{self, CgroupNamespaceEntry};
    assert!(ebpf_tests_enabled());
    let mut ebpf = OnionEbpf::load(&find_bpf_obj_dir(), CGROUP_PATH.as_ref()).unwrap();
    let cgroup = 0xDEAD_BEEF_CAFE_6304;
    let key = FirewallKey {
        src_cgroup_id: cgroup,
        dst_app_id: 37,
        _pad: 0,
    };
    let mut namespaces = Default::default();
    let mut rules = Default::default();
    firewall::reconcile_firewall_maps(
        &mut ebpf.bpf,
        &[CgroupNamespaceEntry {
            cgroup_id: cgroup,
            namespace_id: 19,
        }],
        &[(
            key,
            FirewallValue {
                action: firewall::FIREWALL_ALLOW,
            },
        )],
        &mut namespaces,
        &mut rules,
    )
    .unwrap();
    for _ in 0..2 {
        firewall::reconcile_firewall_maps(&mut ebpf.bpf, &[], &[], &mut namespaces, &mut rules)
            .unwrap();
        assert!(namespaces.is_empty() && rules.is_empty());
        let state = firewall::read_firewall_state(&mut ebpf.bpf, cgroup, key.dst_app_id).unwrap();
        assert_eq!(state.source_namespace_id, None);
        assert_eq!(state.action, None);
    }
    ebpf.detach().unwrap();
}

#[tokio::test]
#[ignore = "requires Linux root and RELIABURGER_EBPF_TESTS=1"]
async fn agent_namespace_binding_includes_outbound_only_workloads() {
    use reliaburger::sesame::{egress, firewall};
    assert!(ebpf_tests_enabled());
    let mut fixture =
        EgressRecoveryFixture::prepare_with_service("outbound-source", false, false).await;
    let path = reliaburger::grill::cgroup::cgroup_path("default", "outbound-source", 0);
    let workload = egress::cgroup_id_of_path(&path).unwrap();
    let launcher = egress::cgroup_id_of_pid(std::process::id()).unwrap();
    let (workload_namespace, launcher_namespace) = {
        let mut ebpf = fixture.ebpf.lock().await;
        (
            firewall::read_firewall_state(&mut ebpf.bpf, workload, 0)
                .unwrap()
                .source_namespace_id,
            firewall::read_firewall_state(&mut ebpf.bpf, launcher, 0)
                .unwrap()
                .source_namespace_id,
        )
    };
    fixture.retire("outbound-source").await.unwrap();
    fixture.crash().await;
    fixture.ebpf.lock().await.detach().unwrap();
    std::fs::remove_dir(path).unwrap();
    assert_ne!(workload, launcher);
    assert_eq!(
        workload_namespace,
        Some(reliaburger::onion::vip::name_to_id("default"))
    );
    assert_eq!(launcher_namespace, None);
}

#[tokio::test]
#[ignore = "requires Linux root and RELIABURGER_EBPF_TESTS=1"]
async fn agent_retirement_preserves_ownership_when_backend_removal_is_refused() {
    assert!(ebpf_tests_enabled());
    let name = "backend-retirement";
    let mut fixture = EgressRecoveryFixture::prepare_with_service(name, false, true).await;
    let service = ServiceId::new("default", name);
    let vip = VirtualIP::from_service_id(&service);
    let record = reliaburger::grill::records::record_path(
        &fixture.root.path().join("records"),
        &format!("default__{name}-0"),
    );
    let bpf = BpfServiceMap::new();
    let before = bpf
        .read_backends(&mut *fixture.ebpf.lock().await, vip, 8080)
        .unwrap();
    freeze_egress_map(&*fixture.ebpf.lock().await, "backend_map");
    let first = fixture.retire(name).await;
    let first_record = record.exists();
    let second = fixture.retire(name).await;
    let second_record = record.exists();
    let retained = bpf
        .read_backends(&mut *fixture.ebpf.lock().await, vip, 8080)
        .unwrap();
    fixture.crash().await;
    fixture.ebpf.lock().await.detach().unwrap();
    std::fs::remove_dir(reliaburger::grill::cgroup::cgroup_path("default", name, 0)).unwrap();
    assert!(before.is_some_and(|entry| entry.count == 1));
    assert!(
        first.is_err() && second.is_err(),
        "accepted refused backend retirement: {first:?}, {second:?}"
    );
    assert!(
        first_record && second_record,
        "discarded backend cleanup owner"
    );
    assert!(retained.is_some_and(|entry| entry.count == 1));
}

#[tokio::test]
#[ignore = "requires Linux root and RELIABURGER_EBPF_TESTS=1"]
async fn agent_retirement_confirms_backend_absence_before_forgetting_ownership() {
    assert!(ebpf_tests_enabled());
    let name = "backend-confirmed";
    let mut fixture = EgressRecoveryFixture::prepare_with_service(name, false, true).await;
    let vip = VirtualIP::from_service_id(&ServiceId::new("default", name));
    let record = reliaburger::grill::records::record_path(
        &fixture.root.path().join("records"),
        &format!("default__{name}-0"),
    );
    fixture.retire(name).await.unwrap();
    fixture.retire(name).await.unwrap();
    let retained_record = record.exists();
    let backend = BpfServiceMap::new()
        .read_backends(&mut *fixture.ebpf.lock().await, vip, 8080)
        .unwrap();
    fixture.crash().await;
    fixture.ebpf.lock().await.detach().unwrap();
    std::fs::remove_dir(reliaburger::grill::cgroup::cgroup_path("default", name, 0)).unwrap();
    assert!(!retained_record);
    assert!(backend.is_none());
}

async fn assert_source_policy_precedes_start(job: bool) {
    use reliaburger::bun::agent::{AgentCommand, ApplyEvent, BunAgent};
    use reliaburger::grill::{mock::MockGrill, port::PortAllocator};
    use reliaburger::sesame::{egress, firewall};
    use std::sync::Arc;
    use tokio::sync::{Mutex, mpsc, oneshot};
    assert!(ebpf_tests_enabled());
    let name = if job {
        "source-before-job"
    } else {
        "source-before-app"
    };
    let target_name = if job {
        "source-target-job"
    } else {
        "source-target-app"
    };
    let kind = if job { "job" } else { "app" };
    let root = tempfile::tempdir().unwrap();
    let records = root.path().join("records");
    let ebpf = Arc::new(Mutex::new(
        OnionEbpf::load_embedded(CGROUP_PATH.as_ref()).unwrap(),
    ));
    let grill = MockGrill::new();
    grill.set_honours_cgroup_path(true);
    grill.set_pid(std::process::id());
    let (commands, receiver) = mpsc::channel(64);
    let shutdown = CancellationToken::new();
    let mut agent = BunAgent::new(
        grill.clone(),
        PortAllocator::new(43400, 43500),
        receiver,
        shutdown.clone(),
    );
    agent.set_records_dir(records.clone());
    agent.set_volumes_dir(root.path().join("volumes"));
    agent.set_onion_ebpf(Arc::clone(&ebpf)).await;
    let task = tokio::spawn(async move { agent.run().await });
    let _tasks = TestTasks::new(shutdown.clone(), vec![task]);
    let target_config = reliaburger::config::Config::parse(&format!(
        "[app.{target_name}]\nimage = 'mock:image'\ncommand = ['sleep', '600']\nnamespace = 'backend'\nport = 8080\n[app.{target_name}.firewall]\nallow_from = ['default/{name}']\n"
    )).unwrap();
    let (target_events, mut target_results) = mpsc::channel(64);
    commands
        .send(AgentCommand::Deploy {
            config: target_config,
            events: target_events,
        })
        .await
        .unwrap();
    while let Some(event) = target_results.recv().await {
        assert!(
            !matches!(event, ApplyEvent::Error { .. }),
            "target deploy failed: {event:?}"
        );
    }
    grill.block_starts();
    let config = reliaburger::config::Config::parse(&format!(
        "[{kind}.{name}]\nimage = 'mock:image'\ncommand = ['sleep', '600']\n"
    ))
    .unwrap();
    let (events, mut results) = mpsc::channel(64);
    commands
        .send(AgentCommand::Deploy { config, events })
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(10), grill.wait_for_starts(1))
        .await
        .unwrap();
    let path = reliaburger::grill::cgroup::cgroup_path("default", name, 0);
    let cgroup = egress::cgroup_id_of_path(&path);
    let namespace = if let Some(cgroup) = cgroup {
        firewall::read_firewall_state(&mut ebpf.lock().await.bpf, cgroup, 0)
            .unwrap()
            .source_namespace_id
    } else {
        None
    };
    let action = if let Some(cgroup) = cgroup {
        firewall::read_firewall_state(
            &mut ebpf.lock().await.bpf,
            cgroup,
            u32::from(VirtualIP::from_service_id(&ServiceId::new("backend", target_name)).0),
        )
        .unwrap()
        .action
    } else {
        None
    };
    let protected = if let Some(cgroup) = cgroup {
        egress::egress_enforced(&mut ebpf.lock().await.bpf, cgroup).unwrap()
    } else {
        false
    };
    let checkpoint = std::fs::read(records.join("egress-owners.checkpoint"))
        .ok()
        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok());
    grill.release_starts(1);
    let mut failed = false;
    while let Some(event) = results.recv().await {
        failed |= matches!(event, ApplyEvent::Error { .. });
    }
    let (response, result) = oneshot::channel();
    commands
        .send(AgentCommand::Retire {
            app_name: name.into(),
            namespace: "default".into(),
            response,
        })
        .await
        .unwrap();
    result.await.unwrap().unwrap();
    let (response, result) = oneshot::channel();
    commands
        .send(AgentCommand::Retire {
            app_name: target_name.into(),
            namespace: "backend".into(),
            response,
        })
        .await
        .unwrap();
    result.await.unwrap().unwrap();
    shutdown.cancel();
    ebpf.lock().await.detach().unwrap();
    if path.exists() {
        std::fs::remove_dir(path).unwrap();
    }
    let target_path = reliaburger::grill::cgroup::cgroup_path("backend", target_name, 0);
    if target_path.exists() {
        std::fs::remove_dir(target_path).unwrap();
    }
    assert!(!failed);
    assert_eq!(
        action,
        Some(firewall::FIREWALL_ALLOW),
        "explicit allow rule was missing at start"
    );
    assert!(
        !protected,
        "source-only ownership unexpectedly imposed an egress allowlist"
    );
    let namespace_id = reliaburger::onion::vip::name_to_id("default");
    assert_eq!(
        namespace,
        Some(namespace_id),
        "namespace was missing at start"
    );
    let owners = checkpoint
        .as_ref()
        .and_then(|value| value["owners"].as_array())
        .expect("no durable source owner at start");
    assert!(
        owners
            .iter()
            .any(|owner| owner["instance_id"] == format!("default__{name}-0")
                && owner["binding"]["phase"] == "Owned"
                && owner["binding"]["source_namespace"] == namespace_id
                && owner["binding"]["cgroup_id"].as_u64() == cgroup)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires Linux root and RELIABURGER_EBPF_TESTS=1"]
async fn source_policy_precedes_portless_application_start() {
    assert_source_policy_precedes_start(false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires Linux root and RELIABURGER_EBPF_TESTS=1"]
async fn source_policy_precedes_job_start() {
    assert_source_policy_precedes_start(true).await;
}

#[tokio::test]
#[ignore = "requires Linux root and RELIABURGER_EBPF_TESTS=1"]
async fn namespace_retirement_keeps_original_ownership_through_recovery() {
    assert!(ebpf_tests_enabled());
    let name = "namespace-retirement";
    let mut fixture = EgressRecoveryFixture::deploy(name).await;
    freeze_egress_map(&*fixture.ebpf.lock().await, "cgroup_namespace_map");
    let first = fixture.retire(name).await;
    let second = fixture.retire(name).await;
    fixture.crash().await;
    let (mut recovered, _, _) = fixture.agent().await;
    let recovery = recovered.adopt_recorded_instances().await;
    let record = reliaburger::grill::records::record_path(
        &fixture.root.path().join("records"),
        &format!("default__{name}-0"),
    );
    let retained_record = record.exists();
    let owners: serde_json::Value = serde_json::from_slice(
        &std::fs::read(fixture.root.path().join("records/egress-owners.checkpoint")).unwrap(),
    )
    .unwrap();
    let retained_owner = owners["owners"]
        .as_array()
        .unwrap()
        .iter()
        .any(|owner| owner["binding"]["phase"] == "Owned");
    fixture.ebpf.lock().await.detach().unwrap();
    std::fs::remove_dir(reliaburger::grill::cgroup::cgroup_path("default", name, 0)).unwrap();
    assert!(first.is_err() && second.is_err() && recovery.is_err());
    assert!(retained_record && retained_owner);
}

#[tokio::test]
#[ignore = "requires Linux root and RELIABURGER_EBPF_TESTS=1"]
async fn namespace_adoption_refuses_missing_original_enforcement() {
    use reliaburger::sesame::{egress, firewall};
    assert!(ebpf_tests_enabled());
    let name = "namespace-adoption";
    let mut fixture = EgressRecoveryFixture::deploy(name).await;
    fixture.crash().await;
    let id = reliaburger::grill::InstanceId(format!("default__{name}-0"));
    fixture.grill.set_adopt_result(&id, true);
    let path = reliaburger::grill::cgroup::cgroup_path("default", name, 0);
    let cgroup = egress::cgroup_id_of_path(&path).unwrap();
    firewall::delete_cgroup_namespace_entry(&mut fixture.ebpf.lock().await.bpf, cgroup).unwrap();
    let (mut recovered, _, _) = fixture.agent().await;
    let result = recovered.adopt_recorded_instances().await;
    let killed = fixture
        .grill
        .calls()
        .iter()
        .any(|(operation, instance)| operation == "kill" && instance == &id);
    fixture.grill.set_adopt_result(&id, false);
    recovered.adopt_recorded_instances().await.unwrap();
    fixture.ebpf.lock().await.detach().unwrap();
    std::fs::remove_dir(path).unwrap();
    assert!(result.is_err() && killed);
}

#[tokio::test]
#[ignore = "requires Linux root and RELIABURGER_EBPF_TESTS=1"]
async fn source_namespace_loss_fences_a_live_workload() {
    use reliaburger::sesame::{egress, firewall};
    assert!(ebpf_tests_enabled());
    let name = "namespace-live-loss";
    let mut fixture = EgressRecoveryFixture::prepare_with_policy(name, false, false, false).await;
    let id = reliaburger::grill::InstanceId(format!("default__{name}-0"));
    let path = reliaburger::grill::cgroup::cgroup_path("default", name, 0);
    let cgroup = egress::cgroup_id_of_path(&path).unwrap();
    firewall::delete_cgroup_namespace_entry(&mut fixture.ebpf.lock().await.bpf, cgroup).unwrap();
    let stopped = tokio::time::timeout(Duration::from_secs(4), async {
        loop {
            if reliaburger::grill::Grill::state(&fixture.grill, &id)
                .await
                .unwrap()
                == reliaburger::grill::ContainerState::Stopped
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .is_ok();
    fixture.retire(name).await.unwrap();
    fixture.crash().await;
    fixture.ebpf.lock().await.detach().unwrap();
    std::fs::remove_dir(path).unwrap();
    assert!(
        stopped,
        "continued running without original namespace enforcement"
    );
}

#[tokio::test]
#[ignore = "requires Linux root and RELIABURGER_EBPF_TESTS=1"]
async fn source_namespace_recovery_refuses_an_owner_with_erased_identity() {
    use reliaburger::sesame::{egress, firewall};
    assert!(ebpf_tests_enabled());
    let name = "namespace-erased-owner";
    let mut fixture = EgressRecoveryFixture::deploy(name).await;
    fixture.crash().await;
    let id = reliaburger::grill::InstanceId(format!("default__{name}-0"));
    fixture.grill.set_adopt_result(&id, true);
    let path = reliaburger::grill::cgroup::cgroup_path("default", name, 0);
    let cgroup = egress::cgroup_id_of_path(&path).unwrap();
    firewall::delete_cgroup_namespace_entry(&mut fixture.ebpf.lock().await.bpf, cgroup).unwrap();
    let checkpoint = fixture.root.path().join("records/egress-owners.checkpoint");
    let mut document: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&checkpoint).unwrap()).unwrap();
    document["owners"][0]["binding"]["source_namespace"] = serde_json::Value::Null;
    std::fs::write(checkpoint, serde_json::to_vec(&document).unwrap()).unwrap();
    let (mut recovered, _, _) = fixture.agent().await;
    let result = recovered.adopt_recorded_instances().await;
    let retained =
        reliaburger::grill::records::record_path(&fixture.root.path().join("records"), &id.0)
            .exists();
    fixture.ebpf.lock().await.detach().unwrap();
    std::fs::remove_dir(path).unwrap();
    assert!(
        result.is_err(),
        "adopted an owner with its namespace identity erased"
    );
    assert!(retained);
}

#[tokio::test]
#[ignore = "requires Linux root and RELIABURGER_EBPF_TESTS=1"]
async fn source_only_checkpoint_failure_prevents_execution() {
    use reliaburger::sesame::{egress, firewall};
    assert!(ebpf_tests_enabled());
    let name = "source-checkpoint-failure";
    let mut fixture = EgressRecoveryFixture::prepare_with_policy(name, true, false, false).await;
    let started = fixture
        .grill
        .calls()
        .iter()
        .any(|(operation, _)| operation == "start");
    let path = reliaburger::grill::cgroup::cgroup_path("default", name, 0);
    let cgroup = egress::cgroup_id_of_path(&path).unwrap();
    let namespace = firewall::read_firewall_state(&mut fixture.ebpf.lock().await.bpf, cgroup, 0)
        .unwrap()
        .source_namespace_id;
    fixture.crash().await;
    fixture.ebpf.lock().await.detach().unwrap();
    std::fs::remove_dir(path).unwrap();
    assert!(!started && namespace.is_none());
}

#[tokio::test]
#[ignore = "requires Linux root and RELIABURGER_EBPF_TESTS=1"]
async fn source_only_adoption_preserves_namespace_without_an_egress_allowlist() {
    use reliaburger::bun::agent::AgentCommand;
    use reliaburger::sesame::{egress, firewall};
    assert!(ebpf_tests_enabled());
    let name = "source-only-adoption";
    let mut fixture = EgressRecoveryFixture::prepare_with_policy(name, false, false, false).await;
    fixture.crash().await;
    let id = reliaburger::grill::InstanceId(format!("default__{name}-0"));
    fixture.grill.set_adopt_result(&id, true);
    let (mut recovered, commands, shutdown) = fixture.agent().await;
    assert_eq!(recovered.adopt_recorded_instances().await.unwrap(), 1);
    let path = reliaburger::grill::cgroup::cgroup_path("default", name, 0);
    let cgroup = egress::cgroup_id_of_path(&path).unwrap();
    let namespace = firewall::read_firewall_state(&mut fixture.ebpf.lock().await.bpf, cgroup, 0)
        .unwrap()
        .source_namespace_id;
    let allowlist = egress::egress_enforced(&mut fixture.ebpf.lock().await.bpf, cgroup).unwrap();
    let task = tokio::spawn(async move { recovered.run().await });
    let (response, result) = tokio::sync::oneshot::channel();
    commands
        .send(AgentCommand::Retire {
            app_name: name.into(),
            namespace: "default".into(),
            response,
        })
        .await
        .unwrap();
    result.await.unwrap().unwrap();
    shutdown.cancel();
    task.await.unwrap();
    fixture.ebpf.lock().await.detach().unwrap();
    std::fs::remove_dir(path).unwrap();
    assert_eq!(
        namespace,
        Some(reliaburger::onion::vip::name_to_id("default"))
    );
    assert!(!allowlist);
}

type InitPolicyStart = (String, Option<u32>, bool);

/// Supplies an offline BusyBox rootfs while retaining the real owned OCI path.
#[derive(Clone)]
struct InitPolicyGrill {
    runtime: reliaburger::grill::runc::RuncGrill,
    bundles: PathBuf,
    bpf: std::sync::Arc<tokio::sync::Mutex<OnionEbpf>>,
    starts: std::sync::Arc<tokio::sync::Mutex<Vec<InitPolicyStart>>>,
    refuse_init_cleanup: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

impl reliaburger::grill::Grill for InitPolicyGrill {
    async fn retain_network_reference(
        &self,
        id: &reliaburger::grill::InstanceId,
    ) -> Result<
        Option<reliaburger::grill::runc_intent::NetworkReference>,
        reliaburger::grill::GrillError,
    > {
        self.runtime.retain_network_reference(id).await
    }
    async fn network_reference(
        &self,
        id: &reliaburger::grill::InstanceId,
    ) -> Result<
        Option<reliaburger::grill::runc_intent::NetworkReference>,
        reliaburger::grill::GrillError,
    > {
        self.runtime.network_reference(id).await
    }
    async fn release_network_reference(
        &self,
        reference: &reliaburger::grill::runc_intent::NetworkReference,
    ) -> Result<(), reliaburger::grill::GrillError> {
        self.runtime.release_network_reference(reference).await
    }

    async fn create(
        &self,
        id: &reliaburger::grill::InstanceId,
        spec: &reliaburger::grill::OciSpec,
    ) -> Result<(), reliaburger::grill::GrillError> {
        self.runtime.create(id, spec).await?;
        let bin = self.bundles.join(&id.0).join("rootfs/bin");
        tokio::fs::create_dir_all(&bin).await.unwrap();
        tokio::fs::copy("/usr/bin/busybox", bin.join("busybox"))
            .await
            .unwrap();
        tokio::fs::write(self.bundles.join(&id.0).join("rootfs/index.html"), &id.0)
            .await
            .unwrap();
        Ok(())
    }

    async fn start(
        &self,
        id: &reliaburger::grill::InstanceId,
    ) -> Result<(), reliaburger::grill::GrillError> {
        let launches = self.runtime.launch_inventory().await?.unwrap();
        let original = launches
            .iter()
            .find(|launch| launch.instance_id == *id)
            .unwrap();
        let path = original.spec.linux.host_cgroup_path().unwrap();
        let cgroup = reliaburger::sesame::egress::cgroup_id_of_path(&path);
        let mut bpf = self.bpf.lock().await;
        let (namespace, enforced) = if let Some(cgroup) = cgroup {
            (
                reliaburger::sesame::firewall::read_firewall_state(&mut bpf.bpf, cgroup, 0)
                    .unwrap()
                    .source_namespace_id,
                reliaburger::sesame::egress::egress_enforced(&mut bpf.bpf, cgroup).unwrap(),
            )
        } else {
            (None, false)
        };
        drop(bpf);
        self.starts
            .lock()
            .await
            .push((id.0.clone(), namespace, enforced));
        self.runtime.start(id).await
    }

    async fn stop(
        &self,
        id: &reliaburger::grill::InstanceId,
    ) -> Result<(), reliaburger::grill::GrillError> {
        self.runtime.stop(id).await
    }
    async fn kill(
        &self,
        id: &reliaburger::grill::InstanceId,
    ) -> Result<(), reliaburger::grill::GrillError> {
        if id.0.ends_with("init-0")
            && self
                .refuse_init_cleanup
                .load(std::sync::atomic::Ordering::SeqCst)
        {
            return Err(reliaburger::grill::GrillError::StateUnavailable {
                instance: id.clone(),
                reason: "injected initialiser cleanup refusal".into(),
            });
        }
        self.runtime.kill(id).await
    }
    async fn state(
        &self,
        id: &reliaburger::grill::InstanceId,
    ) -> Result<reliaburger::grill::ContainerState, reliaburger::grill::GrillError> {
        if id.0.ends_with("init-0")
            && self
                .refuse_init_cleanup
                .load(std::sync::atomic::Ordering::SeqCst)
        {
            return Err(reliaburger::grill::GrillError::StateUnavailable {
                instance: id.clone(),
                reason: "injected initialiser inspection refusal".into(),
            });
        }
        self.runtime.state(id).await
    }
    async fn launch_inventory(
        &self,
    ) -> Result<Option<Vec<reliaburger::grill::RuntimeLaunch>>, reliaburger::grill::GrillError>
    {
        self.runtime.launch_inventory().await
    }
    fn runtime_kind(&self) -> reliaburger::grill::records::RuntimeKind {
        self.runtime.runtime_kind()
    }
    fn honours_cgroup_path(&self) -> bool {
        self.runtime.honours_cgroup_path()
    }
    async fn pid(&self, id: &reliaburger::grill::InstanceId) -> Option<u32> {
        self.runtime.pid(id).await
    }
    async fn container_ip(&self, id: &reliaburger::grill::InstanceId) -> Option<Ipv4Addr> {
        self.runtime.container_ip(id).await
    }
    async fn exit_code(&self, id: &reliaburger::grill::InstanceId) -> Option<i32> {
        self.runtime.exit_code(id).await
    }
    async fn workload_cgroup(
        &self,
        id: &reliaburger::grill::InstanceId,
    ) -> Result<Option<u64>, reliaburger::grill::GrillError> {
        self.runtime.workload_cgroup(id).await
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires Linux root, runc, static BusyBox and RELIABURGER_EBPF_TESTS=1"]
async fn init_exit_preserves_policy_before_the_next_container_starts() {
    use reliaburger::bun::agent::{AgentCommand, ApplyEvent, BunAgent};
    use reliaburger::grill::{Grill, ImageStore, port::PortAllocator, runc::RuncGrill};
    use std::sync::Arc;
    use tokio::sync::{Mutex, mpsc, oneshot};
    assert!(ebpf_tests_enabled());
    let root = tempfile::tempdir().unwrap();
    let ebpf = Arc::new(Mutex::new(
        OnionEbpf::load_embedded(CGROUP_PATH.as_ref()).unwrap(),
    ));
    let bundles = root.path().join("bundles");
    let runtime = RuncGrill::new(
        bundles.clone(),
        ImageStore::new(root.path().join("images")),
        false,
        root.path().join("runc-state"),
    )
    .with_owner(env!("CARGO_BIN_EXE_bun").into())
    .unwrap();
    let starts = Arc::new(Mutex::new(Vec::new()));
    let grill = InitPolicyGrill {
        runtime: runtime.clone(),
        bundles,
        bpf: Arc::clone(&ebpf),
        starts: Arc::clone(&starts),
        refuse_init_cleanup: Arc::new(std::sync::atomic::AtomicBool::new(false)),
    };
    let (commands, receiver) = mpsc::channel(64);
    let shutdown = CancellationToken::new();
    let mut agent = BunAgent::new(
        grill,
        PortAllocator::new(43500, 43600),
        receiver,
        shutdown.clone(),
    );
    agent.set_records_dir(root.path().join("records"));
    agent.set_volumes_dir(root.path().join("volumes"));
    agent.set_onion_ebpf(Arc::clone(&ebpf)).await;
    let task = tokio::spawn(async move { agent.run().await });
    let config = reliaburger::config::Config::parse(
        r#"
        [app.init-policy-boundary]
        image = "/empty-fixture"
        command = ["/bin/busybox", "sleep", "60"]
        [app.init-policy-boundary.egress]
        allow = ["203.0.113.9:443"]
        [[app.init-policy-boundary.init]]
        command = ["/bin/busybox", "true"]
        [[app.init-policy-boundary.init]]
        command = ["/bin/busybox", "true"]
    "#,
    )
    .unwrap();
    let (events, mut results) = mpsc::channel(64);
    commands
        .send(AgentCommand::Deploy { config, events })
        .await
        .unwrap();
    let mut failures = Vec::new();
    while let Some(event) = results.recv().await {
        if let ApplyEvent::Error { message } = event {
            failures.push(message);
        }
    }
    let observed = starts.lock().await.clone();
    let (response, result) = oneshot::channel();
    commands
        .send(AgentCommand::Retire {
            app_name: "init-policy-boundary".into(),
            namespace: "default".into(),
            response,
        })
        .await
        .unwrap();
    let retired = result.await.unwrap();
    shutdown.cancel();
    task.await.unwrap();
    for launch in runtime.launch_inventory().await.unwrap().unwrap() {
        runtime.kill(&launch.instance_id).await.unwrap();
    }
    ebpf.lock().await.detach().unwrap();
    let cgroup = reliaburger::grill::cgroup::cgroup_path("default", "init-policy-boundary", 0);
    if cgroup.exists() {
        std::fs::remove_dir(cgroup).unwrap();
    }
    assert!(failures.is_empty(), "deployment failed: {failures:?}");
    assert!(retired.is_ok(), "retirement failed: {retired:?}");
    assert_eq!(
        observed.len(),
        3,
        "expected two initialisers and the main workload: {observed:?}"
    );
    let namespace = reliaburger::onion::vip::name_to_id("default");
    assert!(
        observed
            .iter()
            .all(|(_, source, egress)| *source == Some(namespace) && *egress),
        "policy disappeared between init/main containers: {observed:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires Linux root, runc, static BusyBox and RELIABURGER_EBPF_TESTS=1"]
async fn uncertain_initialiser_preserves_parent_policy_until_confirmed_retirement() {
    use reliaburger::bun::agent::{AgentCommand, ApplyEvent, BunAgent};
    use reliaburger::grill::{Grill, ImageStore, port::PortAllocator, runc::RuncGrill};
    use std::sync::Arc;
    use tokio::sync::{Mutex, mpsc, oneshot};
    assert!(ebpf_tests_enabled());
    let root = tempfile::tempdir().unwrap();
    let ebpf = Arc::new(Mutex::new(
        OnionEbpf::load_embedded(CGROUP_PATH.as_ref()).unwrap(),
    ));
    let bundles = root.path().join("bundles");
    let runtime = RuncGrill::new(
        bundles.clone(),
        ImageStore::new(root.path().join("images")),
        false,
        root.path().join("runc-state"),
    )
    .with_owner(env!("CARGO_BIN_EXE_bun").into())
    .unwrap();
    let starts = Arc::new(Mutex::new(Vec::new()));
    let grill = InitPolicyGrill {
        runtime: runtime.clone(),
        bundles,
        bpf: Arc::clone(&ebpf),
        starts: Arc::clone(&starts),
        refuse_init_cleanup: Arc::new(std::sync::atomic::AtomicBool::new(false)),
    };
    let refusal = Arc::clone(&grill.refuse_init_cleanup);
    refusal.store(true, std::sync::atomic::Ordering::SeqCst);
    let (commands, receiver) = mpsc::channel(64);
    let shutdown = CancellationToken::new();
    let mut agent = BunAgent::new(
        grill.clone(),
        PortAllocator::new(43500, 43600),
        receiver,
        shutdown.clone(),
    );
    agent.set_records_dir(root.path().join("records"));
    agent.set_volumes_dir(root.path().join("volumes"));
    agent.set_onion_ebpf(Arc::clone(&ebpf)).await;
    let task = tokio::spawn(async move { agent.run().await });
    let config = reliaburger::config::Config::parse(
        r#"
        [app.init-retirement-boundary]
        image = "/empty-fixture"
        command = ["/bin/busybox", "sleep", "60"]
        [app.init-retirement-boundary.egress]
        allow = ["203.0.113.9:443"]
        [[app.init-retirement-boundary.init]]
        command = ["/bin/busybox", "sleep", "60"]
    "#,
    )
    .unwrap();
    let (events, mut results) = mpsc::channel(64);
    commands
        .send(AgentCommand::Deploy { config, events })
        .await
        .unwrap();
    let mut failures = Vec::new();
    while let Some(event) = results.recv().await {
        if let ApplyEvent::Error { message } = event {
            failures.push(message);
        }
    }
    let observed = starts.lock().await.clone();
    let (response, result) = oneshot::channel();
    commands
        .send(AgentCommand::Retire {
            app_name: "init-retirement-boundary".into(),
            namespace: "default".into(),
            response,
        })
        .await
        .unwrap();
    let first = result.await.unwrap();
    let initialiser = reliaburger::grill::InstanceId(observed[0].0.clone());
    let first_state = runtime.state(&initialiser).await.unwrap();
    let path = reliaburger::grill::cgroup::cgroup_path("default", "init-retirement-boundary", 0);
    let cgroup = reliaburger::sesame::egress::cgroup_id_of_path(&path).unwrap();
    let namespace =
        reliaburger::sesame::firewall::read_firewall_state(&mut ebpf.lock().await.bpf, cgroup, 0)
            .unwrap()
            .source_namespace_id;
    task.abort();
    let _ = task.await;
    let (_, receiver) = mpsc::channel(64);
    let mut recovered = BunAgent::new(
        grill,
        PortAllocator::new(43500, 43600),
        receiver,
        CancellationToken::new(),
    );
    recovered.set_records_dir(root.path().join("records"));
    recovered.set_volumes_dir(root.path().join("volumes"));
    recovered.set_onion_ebpf(Arc::clone(&ebpf)).await;
    let recovery_refused = recovered.adopt_recorded_instances().await.is_err();
    let recovered_namespace =
        reliaburger::sesame::firewall::read_firewall_state(&mut ebpf.lock().await.bpf, cgroup, 0)
            .unwrap()
            .source_namespace_id;
    refusal.store(false, std::sync::atomic::Ordering::SeqCst);
    let second = recovered.adopt_recorded_instances().await;
    let second_state = runtime.state(&initialiser).await.unwrap();
    for launch in runtime.launch_inventory().await.unwrap().unwrap() {
        runtime.kill(&launch.instance_id).await.unwrap();
    }
    ebpf.lock().await.detach().unwrap();
    let cgroup = reliaburger::grill::cgroup::cgroup_path("default", "init-retirement-boundary", 0);
    if cgroup.exists() {
        std::fs::remove_dir(cgroup).unwrap();
    }
    assert!(
        !failures.is_empty(),
        "injected init failure was not reported"
    );
    assert!(
        first.is_err(),
        "parent retired despite uncertain live initialiser"
    );
    assert_eq!(first_state, reliaburger::grill::ContainerState::Running);
    assert_eq!(
        namespace,
        Some(reliaburger::onion::vip::name_to_id("default")),
        "parent retirement lifted a live initialiser's policy"
    );
    assert!(
        recovery_refused,
        "unknown initialiser was accepted during recovery"
    );
    assert_eq!(
        recovered_namespace,
        Some(reliaburger::onion::vip::name_to_id("default")),
        "recovery lifted policy before all launches stopped"
    );
    assert!(second.is_ok(), "confirmed retry failed: {second:?}");
    assert_eq!(second_state, reliaburger::grill::ContainerState::Stopped);
}

async fn read_runtime_fixture_page(address: SocketAddr) -> anyhow::Result<String> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    tokio::time::timeout(Duration::from_secs(8), async {
        let mut stream = loop {
            match tokio::net::TcpStream::connect(address).await {
                Ok(stream) => break stream,
                Err(_) => tokio::time::sleep(Duration::from_millis(20)).await,
            }
        };
        stream
            .write_all(b"GET / HTTP/1.0\r\nHost: fixture\r\n\r\n")
            .await?;
        let mut response = String::new();
        stream.take(4096).read_to_string(&mut response).await?;
        Ok::<_, anyhow::Error>(response)
    })
    .await?
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires Linux root, runc, static BusyBox and RELIABURGER_EBPF_TESTS=1"]
async fn refused_backend_withdrawal_cannot_redirect_a_vip_to_a_new_workload() {
    check_backend_retirement(None, true, false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires Linux root, runc, static BusyBox and RELIABURGER_EBPF_TESTS=1"]
async fn refused_rollout_withdrawal_cannot_retire_the_original_destination() {
    check_backend_retirement(Some("rolling"), true, false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires Linux root, runc, static BusyBox and RELIABURGER_EBPF_TESTS=1"]
async fn refused_blue_green_withdrawal_cannot_retire_the_original_destination() {
    check_backend_retirement(Some("blue-green"), true, false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires Linux root, runc, static BusyBox and RELIABURGER_EBPF_TESTS=1"]
async fn confirmed_rollout_withdrawal_keeps_the_replacement_reachable() {
    check_backend_retirement(Some("rolling"), false, false).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires Linux root, runc, static BusyBox and RELIABURGER_EBPF_TESTS=1"]
async fn standalone_discovery_release_allows_confirmed_address_reuse() {
    check_backend_retirement(None, false, true).await;
}

async fn check_backend_retirement(strategy: Option<&str>, freeze: bool, durable: bool) {
    use reliaburger::bun::agent::{AgentCommand, ApplyEvent, BunAgent};
    use reliaburger::grill::{Grill, ImageStore, InstanceId, port::PortAllocator, runc::RuncGrill};
    use std::sync::Arc;
    use tokio::sync::{Mutex, mpsc, oneshot};
    assert!(ebpf_tests_enabled());
    let root = tempfile::tempdir().unwrap();
    let ebpf = Arc::new(Mutex::new(
        OnionEbpf::load_embedded(CGROUP_PATH.as_ref()).unwrap(),
    ));
    let bundles = root.path().join("bundles");
    let runtime = RuncGrill::new(
        bundles.clone(),
        ImageStore::new(root.path().join("images")),
        false,
        root.path().join("runc-state"),
    )
    .with_owner(env!("CARGO_BIN_EXE_bun").into())
    .unwrap();
    let grill = InitPolicyGrill {
        runtime: runtime.clone(),
        bundles,
        bpf: Arc::clone(&ebpf),
        starts: Arc::new(Mutex::new(Vec::new())),
        refuse_init_cleanup: Arc::new(std::sync::atomic::AtomicBool::new(false)),
    };
    let (commands, receiver) = mpsc::channel(64);
    let shutdown = CancellationToken::new();
    let mut agent = BunAgent::new(
        grill,
        PortAllocator::new(43600, 43700),
        receiver,
        shutdown.clone(),
    );
    agent.set_records_dir(root.path().join("records"));
    agent.set_volumes_dir(root.path().join("volumes"));
    agent.set_onion_ebpf(Arc::clone(&ebpf)).await;
    if durable {
        agent
            .enable_fresh_discovery_ownership(&root.path().join("discovery"))
            .await
            .unwrap();
    }
    let services = agent.service_map_watch();
    let task = tokio::spawn(async move { agent.run().await });
    let exercise = async {
        let mut original_address = None;
        for name in ["address-predecessor", "address-successor"] {
            let port = if name == "address-predecessor" { "port = 8080" } else { "" };
            let config = reliaburger::config::Config::parse(&format!(
                "[app.{name}]\nimage = '/empty-fixture'\ncommand = ['/bin/busybox', 'httpd', '-f', '-p', '8080', '-h', '/']\n{port}\n"
            ))?;
            let (events, mut results) = mpsc::channel(64);
            commands.send(AgentCommand::Deploy { config, events }).await?;
            while let Some(event) = results.recv().await {
                if let ApplyEvent::Error { message } = event { anyhow::bail!(message); }
            }
            let id = InstanceId(format!("default__{name}-0"));
            let ip = runtime.container_ip(&id).await.ok_or_else(|| anyhow::anyhow!("runtime omitted container address"))?;
            if durable {
                if name == "address-predecessor" { original_address = Some(ip); }
                else { anyhow::ensure!(original_address == Some(ip), "confirmed release did not make the original address reusable"); }
            }
            let ready = read_runtime_fixture_page(SocketAddr::new(ip.into(), 8080))
                .await.map_err(|error| anyhow::anyhow!("direct {id} ({ip}): {error}"))?;
            anyhow::ensure!(ready.contains(&id.0), "fixture did not serve its own identity");
            if name == "address-predecessor" {
                let vip = VirtualIP::from_service_id(&ServiceId::new("default", name));
                let ready = read_runtime_fixture_page(SocketAddr::new(vip.0.into(), 8080))
                    .await.map_err(|error| anyhow::anyhow!("original VIP before retirement: {error}"))?;
                anyhow::ensure!(ready.contains(&id.0), "original VIP did not serve its own identity");
                if freeze {
                    freeze_egress_map(&*ebpf.lock().await, "backend_map");
                }
                if let Some(strategy) = strategy {
                    let config = reliaburger::config::Config::parse(&format!(
                        "[app.{name}]\nimage = '/empty-fixture'\ncommand = ['/bin/busybox', 'httpd', '-f', '-p', '8080', '-h', '/']\nport = 8080\n[app.{name}.deploy]\nstrategy = '{strategy}'\ndrain_timeout = '0s'\n"
                    ))?;
                    let (events, mut results) = mpsc::channel(64);
                    commands.send(AgentCommand::Deploy { config, events }).await?;
                    let mut refused = false;
                    let mut completed = false;
                    while let Some(event) = results.recv().await {
                        match event {
                            ApplyEvent::Error { message } if freeze => {
                                refused |= message.contains("cannot retire backend") || message.contains("cannot publish backend");
                            }
                            ApplyEvent::Error { message } => anyhow::bail!(message),
                            ApplyEvent::Complete { .. } => completed = true,
                            _ => {}
                        }
                    }
                    if freeze {
                        anyhow::ensure!(refused && !completed, "frozen backend rollout was acknowledged");
                        let service = ServiceId::new("default", name);
                        let view = services.borrow().clone();
                        anyhow::ensure!(view.resolve(&service).is_some_and(|entry|
                            entry.backends.iter().all(|backend| backend.instance_id == id.0 || !backend.healthy)),
                            "userspace advertised a replacement whose kernel publication was refused");
                        anyhow::ensure!(runtime.state(&id).await? == reliaburger::grill::ContainerState::Running,
                            "rollout retired the original destination before confirmed backend withdrawal");
                    } else {
                        anyhow::ensure!(completed, "rollout did not complete");
                        anyhow::ensure!(runtime.state(&id).await? == reliaburger::grill::ContainerState::Stopped,
                            "confirmed rollout did not retire the original destination");
                    }
                } else {
                    let (response, result) = oneshot::channel();
                    commands.send(AgentCommand::Retire { app_name: name.into(), namespace: "default".into(), response }).await?;
                    let retired = result.await?;
                    if durable {
                        retired?;
                        anyhow::ensure!(runtime.network_reference(&id).await?.is_none(), "runtime retained its released reference");
                        let checkpoint: serde_json::Value = serde_json::from_slice(&std::fs::read(root.path().join("discovery/discovery.json"))?)?;
                        let inventory: reliaburger::bun::discovery_owners::DiscoveryInventory = serde_json::from_value(checkpoint["inventory"].clone())?;
                        anyhow::ensure!(inventory.references.is_empty(), "acknowledged release remains in the checkpoint");
                    } else {
                        anyhow::ensure!(retired.is_err(), "frozen backend retirement was acknowledged");
                    }
                }
            }
        }
        let vip = VirtualIP::from_service_id(&ServiceId::new("default", "address-predecessor"));
        let response = read_runtime_fixture_page(SocketAddr::new(vip.0.into(), 8080)).await;
        if durable {
            anyhow::ensure!(response.is_err(), "retired VIP reached a reused address");
            Ok(String::new())
        } else { response.map_err(|error| anyhow::anyhow!("original VIP after refused retirement: {error}")) }
    }.await;
    shutdown.cancel();
    task.await.unwrap();
    let mut owned_cgroups = Vec::new();
    for launch in runtime.launch_inventory().await.unwrap().unwrap() {
        owned_cgroups.extend(launch.spec.linux.host_cgroup_path());
        runtime.kill(&launch.instance_id).await.unwrap();
    }
    ebpf.lock().await.detach().unwrap();
    for launch in runtime.launch_inventory().await.unwrap().unwrap() {
        if let Some(reference) = runtime
            .network_reference(&launch.instance_id)
            .await
            .unwrap()
        {
            runtime.release_network_reference(&reference).await.unwrap();
        }
    }
    for path in owned_cgroups {
        if path.exists() {
            std::fs::remove_dir(path).unwrap();
        }
    }
    let response = exercise.unwrap();
    if durable {
        return;
    }
    assert!(
        !response.contains("default__address-successor-0"),
        "old VIP served an unrelated replacement: {response}"
    );
    let expected = if freeze {
        "default__address-predecessor-0"
    } else {
        "default__address-predecessor-g1-0"
    };
    assert!(
        response.contains(expected),
        "VIP lost its intended endpoint {expected}: {response}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires Linux root and RELIABURGER_EBPF_TESTS=1"]
async fn refused_backend_publication_cannot_report_a_completed_deployment() {
    use reliaburger::bun::agent::{AgentCommand, ApplyEvent, BunAgent};
    use reliaburger::grill::{
        Grill, GrillError, InstanceId, port::PortAllocator, process::ProcessGrill,
    };
    use std::sync::Arc;
    use tokio::sync::{Mutex, mpsc};
    assert!(ebpf_tests_enabled());
    let root = tempfile::tempdir().unwrap();
    let ebpf = Arc::new(Mutex::new(
        OnionEbpf::load_embedded(CGROUP_PATH.as_ref()).unwrap(),
    ));
    freeze_egress_map(&*ebpf.lock().await, "backend_map");
    let (commands, receiver) = mpsc::channel(64);
    let shutdown = CancellationToken::new();
    let runtime = ProcessGrill::new();
    let mut agent = BunAgent::new(
        runtime.clone(),
        PortAllocator::new(43800, 43900),
        receiver,
        shutdown.clone(),
    );
    agent.set_records_dir(root.path().join("records"));
    agent.set_volumes_dir(root.path().join("volumes"));
    agent.set_onion_ebpf(Arc::clone(&ebpf)).await;
    let task = tokio::spawn(async move { agent.run().await });
    let config = reliaburger::config::Config::parse("[app.publication-refusal]\nimage = 'proc-grill:image-ignored'\ncommand = ['sleep', '60']\nport = 8080\n").unwrap();
    let (events, mut results) = mpsc::channel(64);
    commands
        .send(AgentCommand::Deploy { config, events })
        .await
        .unwrap();
    let observation = tokio::time::timeout(Duration::from_secs(5), async {
        let mut completed = false;
        let mut failed = false;
        while let Some(event) = results.recv().await {
            completed |= matches!(event, ApplyEvent::Complete { .. });
            if let ApplyEvent::Error { message } = event {
                failed |= message.contains("cannot publish backend");
            }
        }
        (completed, failed)
    })
    .await;
    let record = reliaburger::grill::records::record_path(
        &root.path().join("records"),
        "default__publication-refusal-0",
    )
    .exists();
    let vip = VirtualIP::from_service_id(&ServiceId::new("default", "publication-refusal"));
    let backend = BpfServiceMap::new()
        .read_backends(&mut *ebpf.lock().await, vip, 8080)
        .unwrap();
    let runtime_state = runtime
        .state(&InstanceId("default__publication-refusal-0".into()))
        .await;
    shutdown.cancel();
    task.await.unwrap();
    ebpf.lock().await.detach().unwrap();
    let (completed, failed) = observation.unwrap();
    assert!(
        backend.is_none(),
        "injected backend refusal was ineffective"
    );
    assert!(
        !record && matches!(runtime_state, Err(GrillError::NotFound { .. })),
        "initial publication refusal still launched a workload: record={record}, state={runtime_state:?}"
    );
    assert!(
        !completed && failed,
        "deployment completed without a published kernel backend"
    );
}

async fn check_destination_grant_retirement(frozen: bool) {
    use reliaburger::bun::agent::AgentCommand;
    use reliaburger::onion::types::{FirewallKey, FirewallValue};
    use reliaburger::sesame::firewall;
    assert!(ebpf_tests_enabled());
    let name = if frozen {
        "grant-refused"
    } else {
        "grant-confirmed"
    };
    let mut fixture = EgressRecoveryFixture::prepare_with_service(name, false, true).await;
    let vip = VirtualIP::from_service_id(&ServiceId::new("default", name));
    let source = 0xDEAD_BEEF_CAFE_6401;
    let original = FirewallKey {
        src_cgroup_id: source,
        dst_app_id: u32::from(vip.0),
        _pad: 0,
    };
    let unrelated = FirewallKey {
        src_cgroup_id: source,
        dst_app_id: original.dst_app_id + 1,
        _pad: 0,
    };
    {
        let mut kernel = fixture.ebpf.lock().await;
        // Represent grants retained in the kernel but absent from the agent's
        // transient written-key cache. Destination ownership must still retire them.
        for key in [original, unrelated] {
            firewall::write_firewall_entry(
                &mut kernel.bpf,
                key,
                FirewallValue {
                    action: firewall::FIREWALL_ALLOW,
                },
            )
            .unwrap();
        }
        if frozen {
            freeze_egress_map(&kernel, "firewall_map");
        }
    }
    let first = fixture.retire(name).await;
    let second = fixture.retire(name).await;
    let record_retained = reliaburger::grill::records::record_path(
        &fixture.root.path().join("records"),
        &format!("default__{name}-0"),
    )
    .exists();
    let (response, reply) = tokio::sync::oneshot::channel();
    fixture
        .commands
        .send(AgentCommand::Resolve {
            app_name: name.into(),
            response,
        })
        .await
        .unwrap();
    let service_retained = reply.await.unwrap().is_some();
    let (grant, other_grant) = {
        let mut kernel = fixture.ebpf.lock().await;
        (
            firewall::read_firewall_state(&mut kernel.bpf, source, original.dst_app_id)
                .unwrap()
                .action,
            firewall::read_firewall_state(&mut kernel.bpf, source, unrelated.dst_app_id)
                .unwrap()
                .action,
        )
    };
    fixture.crash().await;
    fixture.ebpf.lock().await.detach().unwrap();
    std::fs::remove_dir(reliaburger::grill::cgroup::cgroup_path("default", name, 0)).unwrap();
    assert_eq!(
        other_grant,
        Some(firewall::FIREWALL_ALLOW),
        "removed another destination's grant"
    );
    if frozen {
        assert!(
            first.is_err() && second.is_err(),
            "accepted unretired destination grants: {first:?}, {second:?}"
        );
        assert!(
            first
                .unwrap_err()
                .to_string()
                .contains("destination grants")
        );
        assert!(
            second
                .unwrap_err()
                .to_string()
                .contains("destination grants")
        );
        assert!(
            record_retained && service_retained,
            "freed the original destination owner"
        );
        assert_eq!(grant, Some(firewall::FIREWALL_ALLOW));
    } else {
        assert!(first.is_ok() && second.is_ok(), "{first:?}, {second:?}");
        assert!(!record_retained && !service_retained);
        assert_eq!(
            grant, None,
            "service retirement retained an allow grant to a reusable VIP"
        );
    }
}

#[tokio::test]
#[ignore = "requires Linux root and RELIABURGER_EBPF_TESTS=1"]
async fn refused_destination_grant_removal_retains_the_original_service() {
    check_destination_grant_retirement(true).await;
}

#[tokio::test]
#[ignore = "requires Linux root and RELIABURGER_EBPF_TESTS=1"]
async fn confirmed_destination_retirement_removes_only_its_own_grants() {
    check_destination_grant_retirement(false).await;
}

#[tokio::test]
#[ignore = "requires Linux root and RELIABURGER_EBPF_TESTS=1"]
async fn natural_exit_keeps_its_address_while_a_retained_backend_can_reach_it() {
    check_stopped_address_retention(false, false).await;
}

#[tokio::test]
#[ignore = "requires Linux root and RELIABURGER_EBPF_TESTS=1"]
async fn lost_enforcement_stops_execution_even_when_backend_withdrawal_refuses() {
    check_stopped_address_retention(true, false).await;
}

#[tokio::test]
#[ignore = "requires Linux root and RELIABURGER_EBPF_TESTS=1"]
async fn durable_discovery_retains_original_reference_after_controller_loss() {
    check_stopped_address_retention(false, true).await;
}

async fn check_stopped_address_retention(lose_enforcement: bool, durable_discovery: bool) {
    use reliaburger::bun::agent::{AgentCommand, ApplyEvent, BunAgent};
    use reliaburger::grill::{
        ContainerState, Grill, ImageStore, InstanceId, port::PortAllocator, runc::RuncGrill,
    };
    use std::sync::Arc;
    use tokio::sync::{Mutex, mpsc};
    assert!(ebpf_tests_enabled());
    let root = tempfile::tempdir().unwrap();
    let ebpf = Arc::new(Mutex::new(
        OnionEbpf::load_embedded(CGROUP_PATH.as_ref()).unwrap(),
    ));
    let bundles = root.path().join("bundles");
    let runtime = RuncGrill::new(
        bundles.clone(),
        ImageStore::new(root.path().join("images")),
        false,
        root.path().join("runc-state"),
    )
    .with_owner(env!("CARGO_BIN_EXE_bun").into())
    .unwrap();
    let grill = InitPolicyGrill {
        runtime: runtime.clone(),
        bundles: bundles.clone(),
        bpf: Arc::clone(&ebpf),
        starts: Arc::new(Mutex::new(Vec::new())),
        refuse_init_cleanup: Arc::new(std::sync::atomic::AtomicBool::new(false)),
    };
    let (commands, receiver) = mpsc::channel(64);
    let mut agent = BunAgent::new(
        grill.clone(),
        PortAllocator::new(43600, 43700),
        receiver,
        CancellationToken::new(),
    );
    agent.set_records_dir(root.path().join("records"));
    agent.set_volumes_dir(root.path().join("volumes"));
    agent.set_onion_ebpf(Arc::clone(&ebpf)).await;
    if durable_discovery {
        agent
            .enable_fresh_discovery_ownership(&root.path().join("discovery"))
            .await
            .unwrap();
    }
    let mut task = Some(tokio::spawn(async move { agent.run().await }));
    let old = InstanceId("default__natural-predecessor-0".into());
    let new = InstanceId("default__natural-successor-0".into());
    let vip = VirtualIP::from_service_id(&ServiceId::new("default", "natural-predecessor"));
    let exercise = async {
        let mut config = reliaburger::config::Config::parse("[app.natural-predecessor]\nimage = '/empty-fixture'\nport = 8080\n")?;
        config.app.get_mut("natural-predecessor").unwrap().command = vec![
            "/bin/busybox".into(), "sh".into(), "-c".into(),
            "/bin/busybox httpd -f -p 8080 -h / & server=$!; while [ ! -f /exit-now ]; do /bin/busybox sleep 0.01; done; kill \"$server\"; wait \"$server\"; exit 0".into(),
        ];
        if lose_enforcement {
            config.app.get_mut("natural-predecessor").unwrap().egress = Some(
                toml::from_str("allow = ['203.0.113.9:443']")?,
            );
        }
        let (events, mut results) = mpsc::channel(64);
        commands.send(AgentCommand::Deploy { config, events }).await?;
        while let Some(event) = results.recv().await {
            if let ApplyEvent::Error { message } = event { anyhow::bail!(message); }
        }
        let original_ip = runtime.container_ip(&old).await.ok_or_else(|| anyhow::anyhow!("original address absent"))?;
        let original = read_runtime_fixture_page(SocketAddr::new(vip.0.into(), 8080)).await?;
        anyhow::ensure!(original.contains(&old.0), "original VIP failed its positive control");
        freeze_egress_map(&*ebpf.lock().await, "backend_map");
        if lose_enforcement {
            let launch = runtime.launch_inventory().await?.unwrap().into_iter()
                .find(|launch| launch.instance_id == old).unwrap();
            let cgroup = reliaburger::sesame::egress::cgroup_id_of_path(
                &launch.spec.linux.host_cgroup_path().unwrap(),
            ).unwrap();
            let mut kernel = ebpf.lock().await;
            anyhow::ensure!(reliaburger::sesame::egress::egress_enforced(&mut kernel.bpf, cgroup)?,
                "positive control had no enforcement");
            reliaburger::sesame::egress::clear_egress_enforced(&mut kernel.bpf, cgroup)?;
            freeze_egress_map(&kernel, "egress_enabled_map");
        } else {
            // Controller loss must not authorise reuse after natural exit.
            let actor = task.take().unwrap();
            actor.abort();
            let _ = actor.await;
            if durable_discovery {
                let reference = runtime.network_reference(&old).await?
                    .ok_or_else(|| anyhow::anyhow!("original runtime hold disappeared"))?;
                let journal = reliaburger::bun::discovery_owners::DiscoveryJournal::open(&root.path().join("discovery"))?;
                anyhow::ensure!(journal.inventory().references.len() == 1,
                    "controller loss has no complete original reference");
                anyhow::ensure!(journal.inventory().references[0].reference == reference,
                    "discovery checkpoint changed the original runtime generation or allocation");
                anyhow::ensure!(journal.inventory().references[0].phase == reliaburger::bun::discovery_owners::ReferencePhase::Held,
                    "controller loss authorised address release");
            }

            tokio::fs::write(bundles.join(&old.0).join("rootfs/exit-now"), b"exit").await?;
        }
        tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                if runtime.state(&old).await? == ContainerState::Stopped { return Ok::<(), anyhow::Error>(()); }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }).await??;
        if lose_enforcement {
            anyhow::ensure!(runtime.network_reference(&old).await?.is_some(),
                "security fencing released an unconfirmed network reference");
            let (reply, result) = tokio::sync::oneshot::channel();
            commands.send(AgentCommand::Stop {
                app_name: "natural-predecessor".into(), namespace: "default".into(), response: reply,
            }).await?;
            anyhow::ensure!(result.await?.is_err(), "failed withdrawal was acknowledged as cleanup");
            anyhow::ensure!(reliaburger::grill::records::load_records(&root.path().join("records"))?
                .iter().any(|record| record.instance_id == old.0),
                "security fencing forgot the unretired adoption record");
            let actor = task.take().unwrap();
            actor.abort();
            let _ = actor.await;
        } else {
            anyhow::ensure!(runtime.exit_code(&old).await == Some(0), "original did not exit naturally");
        }
        let successor: reliaburger::config::app::AppSpec = toml::from_str("image = '/empty-fixture'\ncommand = ['/bin/busybox', 'httpd', '-f', '-p', '8080', '-h', '/']\n")?;
        let cgroup = reliaburger::grill::cgroup::instance_cgroup_path("default", "natural-successor", &new)?;
        let spec = reliaburger::grill::oci::generate_oci_spec("natural-successor", "default", &successor, &new.0, None, &cgroup.to_string_lossy(), None, None);
        grill.create(&new, &spec).await?;
        grill.start(&new).await?;
        let successor_ip = runtime.container_ip(&new).await.ok_or_else(|| anyhow::anyhow!("successor address absent"))?;
        let direct = read_runtime_fixture_page(SocketAddr::new(successor_ip.into(), 8080)).await?;
        anyhow::ensure!(direct.contains(&new.0), "successor failed its direct positive control");
        let old_route = read_runtime_fixture_page(SocketAddr::new(vip.0.into(), 8080)).await.ok();
        Ok::<_, anyhow::Error>((original_ip, successor_ip, old_route))
    }.await;
    if let Some(actor) = task.take() {
        actor.abort();
        let _ = actor.await;
    }
    let launches = runtime.launch_inventory().await.unwrap().unwrap();
    let paths: Vec<_> = launches
        .iter()
        .filter_map(|launch| launch.spec.linux.host_cgroup_path())
        .collect();
    for launch in launches {
        runtime.kill(&launch.instance_id).await.unwrap();
    }
    ebpf.lock().await.detach().unwrap();
    for launch in runtime.launch_inventory().await.unwrap().unwrap() {
        if let Some(reference) = runtime
            .network_reference(&launch.instance_id)
            .await
            .unwrap()
        {
            runtime.release_network_reference(&reference).await.unwrap();
        }
    }
    for path in paths {
        if path.exists() {
            std::fs::remove_dir(path).unwrap();
        }
    }
    let (original_ip, successor_ip, old_route) = exercise.unwrap();
    assert!(
        old_route.is_none(),
        "old VIP reached a successor after natural exit: {old_route:?}"
    );
    assert_ne!(
        original_ip, successor_ip,
        "reused an address still held by a backend route"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "requires Linux root and RELIABURGER_EBPF_TESTS=1"]
async fn refused_health_publication_prevents_restart_and_preserves_ownership() {
    use reliaburger::bun::agent::{AgentCommand, ApplyEvent, BunAgent};
    use reliaburger::grill::{Grill, InstanceId, port::PortAllocator, process::ProcessGrill};
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        sync::{Mutex, mpsc, oneshot},
    };
    assert!(ebpf_tests_enabled());
    let root = tempfile::tempdir().unwrap();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let health_port = listener.local_addr().unwrap().port();
    let healthy = Arc::new(AtomicBool::new(true));
    let response_health = Arc::clone(&healthy);
    let server = tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            let mut request = [0_u8; 1024];
            if tokio::time::timeout(Duration::from_secs(1), stream.read(&mut request))
                .await
                .is_err()
            {
                continue;
            }
            let status = if response_health.load(Ordering::SeqCst) {
                "200 OK"
            } else {
                "503 Unavailable"
            };
            let response =
                format!("HTTP/1.1 {status}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
            let _ = stream.write_all(response.as_bytes()).await;
        }
    });
    let ebpf = Arc::new(Mutex::new(
        OnionEbpf::load_embedded(CGROUP_PATH.as_ref()).unwrap(),
    ));
    let runtime = ProcessGrill::new();
    let (commands, receiver) = mpsc::channel(64);
    let shutdown = CancellationToken::new();
    let mut agent = BunAgent::new(
        runtime.clone(),
        PortAllocator::new(43900, 44000),
        receiver,
        shutdown.clone(),
    );
    agent.set_records_dir(root.path().join("records"));
    agent.set_volumes_dir(root.path().join("volumes"));
    agent.set_onion_ebpf(Arc::clone(&ebpf)).await;
    let task = tokio::spawn(async move { agent.run().await });
    let id = InstanceId("default__health-refusal-0".into());
    let exercise = tokio::time::timeout(Duration::from_secs(12), async {
        let config = reliaburger::config::Config::parse(&format!(
            "[app.health-refusal]\nimage = 'proc-grill:image-ignored'\ncommand = ['sleep', '60']\nport = 8080\n[app.health-refusal.health]\npath = '/'\nport = {health_port}\ninterval = 1\ntimeout = 1\nthreshold_unhealthy = 1\nthreshold_healthy = 1\n"
        ))?;
        let (events, mut results) = mpsc::channel(64);
        commands.send(AgentCommand::Deploy { config, events }).await?;
        while let Some(event) = results.recv().await {
            if let ApplyEvent::Error { message } = event { anyhow::bail!(message); }
        }
        let mut frozen = false;
        let mut unhealthy_since = None;
        loop {
            let (response, result) = oneshot::channel();
            commands.send(AgentCommand::Status { response }).await?;
            let instances = result.await?;
            let instance = instances.iter().find(|instance| instance.id == id.0)
                .ok_or_else(|| anyhow::anyhow!("health owner disappeared"))?;
            if !frozen && instance.state == "running" {
                freeze_egress_map(&*ebpf.lock().await, "backend_map");
                healthy.store(false, Ordering::SeqCst);
                frozen = true;
            }
            anyhow::ensure!(instance.restart_count == 0, "restart began before kernel health withdrawal was confirmed: {instance:?}");
            if frozen && instance.state == "unhealthy" {
                let since = unhealthy_since.get_or_insert_with(std::time::Instant::now);
                if since.elapsed() >= Duration::from_secs(2) { break; }
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        anyhow::ensure!(runtime.state(&id).await? == reliaburger::grill::ContainerState::Running,
            "refused health withdrawal retired the original runtime");
        anyhow::ensure!(reliaburger::grill::records::record_path(&root.path().join("records"), &id.0).exists(),
            "refused health withdrawal lost adoption ownership");
        Ok::<(), anyhow::Error>(())
    }).await;
    shutdown.cancel();
    task.await.unwrap();
    runtime.kill(&id).await.unwrap();
    ebpf.lock().await.detach().unwrap();
    server.abort();
    let _ = server.await;
    exercise.unwrap().unwrap();
}
