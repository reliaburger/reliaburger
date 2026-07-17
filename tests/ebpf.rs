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
    ebpf.detach();
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

    ebpf.detach();
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

    ebpf.detach();
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

    ebpf.detach();
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

    ebpf.detach();
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

    ebpf.detach();
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

    ebpf.detach();
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
    agent.set_onion_ebpf(Arc::clone(&ebpf));
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
    agent.set_onion_ebpf(Arc::clone(&ebpf));
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
            request: FaultRequest {
                fault_type: FaultType::Drop { probability: 100 },
                target_service: "faulty".into(),
                target_instance: None,
                target_node: None,
                duration: Duration::from_secs(30),
                injected_by: "test".into(),
                reason: None,
                include_leader: false,
                override_safety: false,
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

    // Lift enforcement so the harness's own connections are unaffected.
    egress::clear_egress_enforced(&mut ebpf.bpf, cgroup_id).expect("clear enforcement");
    ebpf.detach();
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

    ebpf.detach();
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
    ebpf.detach();
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

    // Lift enforcement before asserting, so a failure never leaves the
    // harness cgroup restricted.
    egress::delete_cgroup_egress_state(&mut ebpf.bpf, cgroup_id).expect("scrub");
    ebpf.detach();

    assert!(ok.is_ok(), "listed IPv6 destination should connect: {ok:?}");
    assert!(
        matches!(
            blocked.as_ref().map_err(|e| e.kind()),
            Err(std::io::ErrorKind::PermissionDenied)
        ),
        "unlisted IPv6 destination should be denied with EPERM, got {blocked:?}"
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
    ebpf.detach();

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
    ebpf.detach();

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

    ebpf.detach();
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
    agent.set_onion_ebpf(Arc::clone(&ebpf));
    tokio::spawn(async move { agent.run().await });

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
    while ev_rx.recv().await.is_some() {}

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
    agent.set_onion_ebpf(Arc::clone(&ebpf));
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
