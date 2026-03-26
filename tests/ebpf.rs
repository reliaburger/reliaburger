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
use std::sync::Arc;
use std::time::Duration;

use reliaburger::onion::ebpf::loader::OnionEbpf;
use reliaburger::onion::ebpf::maps::{BpfServiceMap, service_entry_to_backend_value};
use reliaburger::onion::service_map::ServiceMap;
use reliaburger::onion::types::{BackendInstance, BackendKey};
use reliaburger::onion::vip::VirtualIP;
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;

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
async fn ebpf_load_and_attach() {
    if !ebpf_tests_enabled() {
        eprintln!("skipping eBPF test (set RELIABURGER_EBPF_TESTS=1)");
        return;
    }

    let obj_dir = find_bpf_obj_dir();
    let mut ebpf =
        OnionEbpf::load(&obj_dir, CGROUP_PATH.as_ref()).expect("failed to load eBPF program");

    assert!(ebpf.is_attached());
    ebpf.detach();
}

#[tokio::test]
async fn ebpf_backend_map_write_and_read() {
    if !ebpf_tests_enabled() {
        eprintln!("skipping eBPF test (set RELIABURGER_EBPF_TESTS=1)");
        return;
    }

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
            "redis",
            BackendInstance {
                instance_id: "redis-0".to_string(),
                node_ip: Ipv4Addr::new(10, 0, 2, 2),
                host_port: 30891,
                healthy: true,
            },
        )
        .unwrap();

    // Sync to BPF maps
    bpf_map.sync_from_service_map(&svc_map, &mut ebpf);

    // Read back
    let vip = VirtualIP::from_app_name("redis");
    let value = bpf_map
        .read_backends(&mut ebpf, vip, 6379)
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
async fn ebpf_backend_map_remove() {
    if !ebpf_tests_enabled() {
        eprintln!("skipping eBPF test (set RELIABURGER_EBPF_TESTS=1)");
        return;
    }

    let obj_dir = find_bpf_obj_dir();
    let mut ebpf =
        OnionEbpf::load(&obj_dir, CGROUP_PATH.as_ref()).expect("failed to load eBPF program");

    let mut bpf_map = BpfServiceMap::new();

    let mut svc_map = ServiceMap::new();
    svc_map
        .register_app("redis", "default", 6379, None)
        .unwrap();

    bpf_map.sync_from_service_map(&svc_map, &mut ebpf);

    let vip = VirtualIP::from_app_name("redis");
    assert!(bpf_map.read_backends(&mut ebpf, vip, 6379).is_some());

    // Remove
    bpf_map.remove_backends_bpf(&mut ebpf, vip, 6379);
    assert!(bpf_map.read_backends(&mut ebpf, vip, 6379).is_none());

    ebpf.detach();
}

#[tokio::test]
async fn ebpf_service_map_sync_multiple() {
    if !ebpf_tests_enabled() {
        eprintln!("skipping eBPF test (set RELIABURGER_EBPF_TESTS=1)");
        return;
    }

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

    bpf_map.sync_from_service_map(&svc_map, &mut ebpf);

    // All three should be in the map
    assert!(
        bpf_map
            .read_backends(&mut ebpf, VirtualIP::from_app_name("redis"), 6379)
            .is_some()
    );
    assert!(
        bpf_map
            .read_backends(&mut ebpf, VirtualIP::from_app_name("web"), 8080)
            .is_some()
    );
    assert!(
        bpf_map
            .read_backends(&mut ebpf, VirtualIP::from_app_name("api"), 3000)
            .is_some()
    );

    ebpf.detach();
}

// ---------------------------------------------------------------------------
// Tier 2: Connect rewrite verification
// ---------------------------------------------------------------------------

#[tokio::test]
async fn ebpf_connect_to_vip_rewrites_destination() {
    if !ebpf_tests_enabled() {
        eprintln!("skipping eBPF test (set RELIABURGER_EBPF_TESTS=1)");
        return;
    }

    let obj_dir = find_bpf_obj_dir();
    let mut ebpf =
        OnionEbpf::load(&obj_dir, CGROUP_PATH.as_ref()).expect("failed to load eBPF program");

    // Start a TCP listener on an ephemeral port
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let backend_port = listener.local_addr().unwrap().port();
    listener.set_nonblocking(false).unwrap();

    // Populate backend_map: VIP → our listener
    let vip = VirtualIP::from_app_name("test-service");
    let service_port: u16 = 9999;

    let mut svc_map = ServiceMap::new();
    svc_map
        .register_app("test-service", "default", service_port, None)
        .unwrap();
    svc_map
        .add_backend(
            "test-service",
            BackendInstance {
                instance_id: "test-0".to_string(),
                node_ip: Ipv4Addr::LOCALHOST,
                host_port: backend_port,
                healthy: true,
            },
        )
        .unwrap();

    let mut bpf_map = BpfServiceMap::new();
    bpf_map.sync_from_service_map(&svc_map, &mut ebpf);

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
async fn ebpf_connect_to_vip_no_backends_refused() {
    if !ebpf_tests_enabled() {
        eprintln!("skipping eBPF test (set RELIABURGER_EBPF_TESTS=1)");
        return;
    }

    let obj_dir = find_bpf_obj_dir();
    let mut ebpf =
        OnionEbpf::load(&obj_dir, CGROUP_PATH.as_ref()).expect("failed to load eBPF program");

    // Register a service with no backends
    let vip = VirtualIP::from_app_name("empty-service");
    let mut svc_map = ServiceMap::new();
    svc_map
        .register_app("empty-service", "default", 7777, None)
        .unwrap();

    let mut bpf_map = BpfServiceMap::new();
    bpf_map.sync_from_service_map(&svc_map, &mut ebpf);

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
async fn ebpf_connect_non_vip_passes_through() {
    if !ebpf_tests_enabled() {
        eprintln!("skipping eBPF test (set RELIABURGER_EBPF_TESTS=1)");
        return;
    }

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
// Tier 3: DNS responder
// ---------------------------------------------------------------------------

#[tokio::test]
async fn dns_responder_resolves_internal_name() {
    if !ebpf_tests_enabled() {
        eprintln!("skipping eBPF test (set RELIABURGER_EBPF_TESTS=1)");
        return;
    }

    let svc_map = Arc::new(RwLock::new(ServiceMap::new()));
    {
        let mut map = svc_map.write().await;
        map.register_app("redis", "default", 6379, None).unwrap();
    }

    let shutdown = CancellationToken::new();

    // Use a high port to avoid needing root for port 53
    let config = reliaburger::onion::dns::DnsConfig {
        listen_addr: "127.0.0.1:15353".parse().unwrap(),
        upstream: "8.8.8.8:53".parse().unwrap(),
    };

    let map_clone = svc_map.clone();
    let shutdown_clone = shutdown.clone();
    tokio::spawn(async move {
        let _ = reliaburger::onion::dns::run_dns_responder(config, map_clone, shutdown_clone).await;
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
    let vip = VirtualIP::from_app_name("redis");
    assert_eq!(&response[len - 4..], &vip.0.octets());

    shutdown.cancel();
}

#[tokio::test]
async fn dns_responder_non_internal_times_out() {
    if !ebpf_tests_enabled() {
        eprintln!("skipping eBPF test (set RELIABURGER_EBPF_TESTS=1)");
        return;
    }

    let svc_map = Arc::new(RwLock::new(ServiceMap::new()));
    let shutdown = CancellationToken::new();

    // Point upstream at a non-existent resolver so forwarding times out
    let config = reliaburger::onion::dns::DnsConfig {
        listen_addr: "127.0.0.1:15354".parse().unwrap(),
        upstream: "192.0.2.1:53".parse().unwrap(), // TEST-NET, unreachable
    };

    let map_clone = svc_map.clone();
    let shutdown_clone = shutdown.clone();
    tokio::spawn(async move {
        let _ = reliaburger::onion::dns::run_dns_responder(config, map_clone, shutdown_clone).await;
    });

    tokio::time::sleep(Duration::from_millis(100)).await;

    let socket = tokio::net::UdpSocket::bind("0.0.0.0:0").await.unwrap();
    let query = build_dns_query("example.com");
    socket.send_to(&query, "127.0.0.1:15354").await.unwrap();

    let mut buf = [0u8; 512];
    let result = tokio::time::timeout(Duration::from_secs(3), socket.recv_from(&mut buf)).await;

    // Should time out since upstream is unreachable
    assert!(
        result.is_err(),
        "non-.internal query should not be answered locally"
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
