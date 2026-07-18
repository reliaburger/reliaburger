//! Integration tests for the hardened Onion DNS responder (Stage 4 W3,
//! L9 + M8). Each test runs a real responder on an ephemeral UDP port
//! and speaks wire-format DNS to it.

use std::sync::Arc;
use std::time::Duration;

use reliaburger::onion::dns::{
    BoundDnsResponder, DnsCapability, DnsConfig, DnsFaultState, bind_dns_responder, serve,
};
use reliaburger::onion::service_id::ServiceId;
use reliaburger::onion::service_map::ServiceMap;
use reliaburger::onion::vip::VirtualIP;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UdpSocket;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

const QTYPE_A: u16 = 1;
const QTYPE_AAAA: u16 = 28;

/// A running responder plus the handles the tests poke at.
struct DnsHarness {
    addr: std::net::SocketAddr,
    map_tx: watch::Sender<ServiceMap>,
    fault_tx: watch::Sender<DnsFaultState>,
    shutdown: CancellationToken,
}

impl DnsHarness {
    /// Start a responder forwarding to `upstream` (use an unroutable
    /// TEST-NET address when the test never expects forwarding).
    async fn start(upstream: std::net::SocketAddr, map: ServiceMap) -> Self {
        let (map_tx, map_rx) = watch::channel(map);
        let (fault_tx, fault_rx) = watch::channel(DnsFaultState::default());
        let shutdown = CancellationToken::new();

        let config = DnsConfig {
            listen_addr: "127.0.0.1:0".parse().unwrap(),
            upstream,
            upstream_timeout: Duration::from_millis(400),
            ..DnsConfig::default()
        };
        let (socket, addr) = bind_dns_responder(&config).await.unwrap();

        let serve_shutdown = shutdown.clone();
        tokio::spawn(async move {
            serve(Arc::new(config), socket, map_rx, fault_rx, serve_shutdown).await;
        });

        Self {
            addr,
            map_tx,
            fault_tx,
            shutdown,
        }
    }

    async fn query(&self, packet: &[u8]) -> Option<Vec<u8>> {
        let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        socket.send_to(packet, self.addr).await.unwrap();
        let mut buf = [0u8; 1500];
        match tokio::time::timeout(Duration::from_secs(2), socket.recv_from(&mut buf)).await {
            Ok(Ok((n, _))) => Some(buf[..n].to_vec()),
            _ => None,
        }
    }
}

impl Drop for DnsHarness {
    fn drop(&mut self) {
        self.shutdown.cancel();
    }
}

fn unroutable_upstream() -> std::net::SocketAddr {
    // TEST-NET-1 (RFC 5737): guaranteed not to answer.
    "192.0.2.1:53".parse().unwrap()
}

fn build_query(name: &str, qtype: u16) -> Vec<u8> {
    build_query_with_id(name, qtype, [0x12, 0x34])
}

fn build_query_with_id(name: &str, qtype: u16, id: [u8; 2]) -> Vec<u8> {
    let mut packet = Vec::new();
    packet.extend_from_slice(&id);
    packet.extend_from_slice(&[0x01, 0x00]); // RD=1
    packet.extend_from_slice(&[0x00, 0x01]); // QDCOUNT=1
    packet.extend_from_slice(&[0x00, 0x00, 0x00, 0x00, 0x00, 0x00]);
    for label in name.split('.') {
        packet.push(label.len() as u8);
        packet.extend_from_slice(label.as_bytes());
    }
    packet.push(0x00);
    packet.extend_from_slice(&qtype.to_be_bytes());
    packet.extend_from_slice(&[0x00, 0x01]); // IN
    packet
}

fn rcode(response: &[u8]) -> u8 {
    response[3] & 0x0F
}

fn map_with(app: &str) -> ServiceMap {
    let mut map = ServiceMap::new();
    map.register_app(app, "default", 8080, None).unwrap();
    map
}

#[tokio::test]
async fn resolves_registered_app_to_vip() {
    let harness = DnsHarness::start(unroutable_upstream(), map_with("redis")).await;

    let response = harness
        .query(&build_query("redis.internal", QTYPE_A))
        .await
        .expect("no response");

    assert_eq!(rcode(&response), 0);
    assert_eq!(&response[6..8], &[0x00, 0x01], "expected one answer");
    let vip = VirtualIP::from_service_id(&ServiceId::new("default", "redis"));
    let len = response.len();
    assert_eq!(&response[len - 4..], &vip.0.octets());
}

#[tokio::test]
async fn dns_nxdomain_fault_forces_nxdomain_over_the_wire_then_reverses_on_clear() {
    // End-to-end over a real UDP socket: a service that resolves goes
    // NXDOMAIN while the Smoker publishes a DnsNxdomain fault for it, and
    // resolves again the moment the fault is cleared (empty publish).
    let harness = DnsHarness::start(unroutable_upstream(), map_with("redis")).await;

    // Baseline: resolves.
    let before = harness
        .query(&build_query("redis.internal", QTYPE_A))
        .await
        .expect("no response");
    assert_eq!(rcode(&before), 0, "service should resolve before the fault");

    // Publish the fault (no expiry) and expect NXDOMAIN.
    harness
        .fault_tx
        .send(DnsFaultState::from_faults([("redis".to_string(), 0)]))
        .unwrap();
    let during = harness
        .query(&build_query("redis.internal", QTYPE_A))
        .await
        .expect("no response");
    assert_eq!(rcode(&during), 3, "faulted service must return NXDOMAIN");

    // Clear the fault and expect resolution again.
    harness.fault_tx.send(DnsFaultState::default()).unwrap();
    let after = harness
        .query(&build_query("redis.internal", QTYPE_A))
        .await
        .expect("no response");
    assert_eq!(rcode(&after), 0, "service resolves again after clear");
}

#[tokio::test]
async fn unmatched_internal_returns_nxdomain_and_never_hits_upstream() {
    // Real socket standing in for the upstream: it must stay silent
    // AND receive nothing.
    let fake_upstream = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
    let upstream_addr = fake_upstream.local_addr().unwrap();

    let harness = DnsHarness::start(upstream_addr, ServiceMap::new()).await;

    let response = harness
        .query(&build_query("ghost.internal", QTYPE_A))
        .await
        .expect("no response");
    assert_eq!(rcode(&response), 3, "expected NXDOMAIN");

    // The internal name must not leak to the upstream resolver.
    let mut buf = [0u8; 512];
    let leaked = tokio::time::timeout(
        Duration::from_millis(300),
        fake_upstream.recv_from(&mut buf),
    )
    .await;
    assert!(leaked.is_err(), ".internal query leaked to upstream");
}

#[tokio::test]
async fn aaaa_query_returns_empty_noerror_for_known_name() {
    let harness = DnsHarness::start(unroutable_upstream(), map_with("redis")).await;

    let response = harness
        .query(&build_query("redis.internal", QTYPE_AAAA))
        .await
        .expect("no response");

    assert_eq!(rcode(&response), 0, "expected NOERROR");
    assert_eq!(&response[6..8], &[0x00, 0x00], "expected zero answers");
}

#[tokio::test]
async fn malformed_packet_does_not_kill_the_loop() {
    let harness = DnsHarness::start(unroutable_upstream(), map_with("redis")).await;

    // Garbage first: must be dropped without an answer or a crash.
    let garbage = vec![0xFF; 5];
    assert!(harness.query(&garbage).await.is_none());

    // A valid query afterwards still gets answered.
    let response = harness
        .query(&build_query("redis.internal", QTYPE_A))
        .await
        .expect("responder died after malformed packet");
    assert_eq!(rcode(&response), 0);
}

#[tokio::test]
async fn forwarded_query_relays_valid_upstream_reply() {
    let fake_upstream = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
    let upstream_addr = fake_upstream.local_addr().unwrap();

    // Fake upstream: echo a minimal response with the SAME transaction
    // ID as whatever query arrives.
    let responder = Arc::clone(&fake_upstream);
    tokio::spawn(async move {
        let mut buf = [0u8; 512];
        if let Ok((n, src)) = responder.recv_from(&mut buf).await {
            let mut reply = buf[..n].to_vec();
            reply[2] = 0x80; // QR=1
            let _ = responder.send_to(&reply, src).await;
        }
    });

    let harness = DnsHarness::start(upstream_addr, ServiceMap::new()).await;
    let response = harness
        .query(&build_query("example.com", QTYPE_A))
        .await
        .expect("no relayed response");

    assert_eq!(&response[..2], &[0x12, 0x34], "transaction ID preserved");
    assert_eq!(response[2] & 0x80, 0x80, "QR bit set");
}

#[tokio::test]
async fn upstream_reply_with_wrong_id_yields_servfail() {
    let fake_upstream = Arc::new(UdpSocket::bind("127.0.0.1:0").await.unwrap());
    let upstream_addr = fake_upstream.local_addr().unwrap();

    // Fake upstream replies with a WRONG transaction ID — a spoof.
    let responder = Arc::clone(&fake_upstream);
    tokio::spawn(async move {
        let mut buf = [0u8; 512];
        if let Ok((n, src)) = responder.recv_from(&mut buf).await {
            let mut reply = buf[..n].to_vec();
            reply[0] = 0xDE;
            reply[1] = 0xAD;
            reply[2] = 0x80;
            let _ = responder.send_to(&reply, src).await;
        }
    });

    let harness = DnsHarness::start(upstream_addr, ServiceMap::new()).await;
    let response = harness
        .query(&build_query("example.com", QTYPE_A))
        .await
        .expect("expected SERVFAIL, got silence");

    assert_eq!(&response[..2], &[0x12, 0x34], "reply must match OUR id");
    assert_eq!(rcode(&response), 2, "spoofed reply must yield SERVFAIL");
}

#[tokio::test]
async fn run_dns_responder_fails_closed_when_it_cannot_bind() {
    // Occupy a UDP port, then point the responder at it. `run_dns_responder`
    // must return an error rather than swallowing the bind failure, so the
    // caller (bun startup) fails the deploy closed instead of running without
    // a resolver.
    let occupier = UdpSocket::bind("127.0.0.1:0").await.unwrap();
    let taken = occupier.local_addr().unwrap();

    let config = DnsConfig {
        listen_addr: taken,
        upstream: unroutable_upstream(),
        upstream_timeout: Duration::from_millis(400),
        ..DnsConfig::default()
    };
    let (_tx, rx) = watch::channel(ServiceMap::new());
    let (_fault_tx, fault_rx) = watch::channel(DnsFaultState::default());
    let shutdown = CancellationToken::new();

    let result = reliaburger::onion::dns::run_dns_responder(config, rx, fault_rx, shutdown).await;
    assert!(
        result.is_err(),
        "responder must fail closed when the listen address is unavailable"
    );
}

#[tokio::test]
async fn startup_binding_requires_both_udp_and_tcp() {
    // Leave UDP free but occupy TCP. A UDP-only startup would look healthy
    // until a resolver retried a truncated answer over TCP.
    let tcp = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let taken = tcp.local_addr().unwrap();
    let config = DnsConfig {
        listen_addr: taken,
        ..DnsConfig::default()
    };

    let result = BoundDnsResponder::bind(config).await;
    assert!(result.is_err(), "TCP bind conflict must fail readiness");
}

#[tokio::test]
async fn bound_responder_answers_internal_query_over_tcp_on_the_same_port() {
    let (map_tx, map_rx) = watch::channel(map_with("redis"));
    let (_fault_tx, fault_rx) = watch::channel(DnsFaultState::default());
    let shutdown = CancellationToken::new();
    let responder = BoundDnsResponder::bind(DnsConfig {
        listen_addr: "127.0.0.1:0".parse().unwrap(),
        upstream: unroutable_upstream(),
        ..DnsConfig::default()
    })
    .await
    .unwrap();
    let addr = responder.local_addr().unwrap();
    let task_shutdown = shutdown.clone();
    let task = tokio::spawn(responder.run(map_rx, fault_rx, task_shutdown));

    let query = build_query("redis.internal", QTYPE_A);
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
    stream
        .write_all(&(query.len() as u16).to_be_bytes())
        .await
        .unwrap();
    stream.write_all(&query).await.unwrap();
    let response_len = stream.read_u16().await.unwrap() as usize;
    let mut response = vec![0; response_len];
    stream.read_exact(&mut response).await.unwrap();

    assert_eq!(rcode(&response), 0);
    assert_eq!(&response[6..8], &[0x00, 0x01]);
    let vip = map_tx
        .borrow()
        .resolve(&ServiceId::new("default", "redis"))
        .unwrap()
        .vip;
    assert_eq!(&response[response.len() - 4..], &vip.0.octets());

    shutdown.cancel();
    task.await.unwrap();
}

#[test]
fn dns_capability_requires_binding_family_and_workload_reachability() {
    let mut capability = DnsCapability {
        enabled: true,
        ready: true,
        ipv4: true,
        ipv6: false,
        workload_reachable: true,
    };
    assert!(capability.can_resolve_internal());

    capability.ready = false;
    assert!(!capability.can_resolve_internal());
    capability.ready = true;
    capability.workload_reachable = false;
    assert!(!capability.can_resolve_internal());
    capability.workload_reachable = true;
    capability.ipv4 = false;
    assert!(!capability.can_resolve_internal());
}

#[tokio::test]
async fn resolves_namespace_qualified_name_over_udp() {
    // The end-to-end namespaced path: two `api` services in different
    // namespaces, each qualified query resolving to its own VIP.
    let mut map = ServiceMap::new();
    map.register_app("api", "default", 3000, None).unwrap();
    map.register_app("api", "payments", 3000, None).unwrap();
    let default_vip = VirtualIP::from_service_id(&ServiceId::new("default", "api"));
    let payments_vip = VirtualIP::from_service_id(&ServiceId::new("payments", "api"));

    let harness = DnsHarness::start(unroutable_upstream(), map).await;

    let r1 = harness
        .query(&build_query("api.default.internal", QTYPE_A))
        .await
        .expect("no response");
    assert_eq!(rcode(&r1), 0);
    assert_eq!(&r1[r1.len() - 4..], &default_vip.0.octets());

    let r2 = harness
        .query(&build_query("api.payments.internal", QTYPE_A))
        .await
        .expect("no response");
    assert_eq!(rcode(&r2), 0);
    assert_eq!(&r2[r2.len() - 4..], &payments_vip.0.octets());
}

#[tokio::test]
async fn service_map_updates_are_visible_without_restart() {
    let harness = DnsHarness::start(unroutable_upstream(), ServiceMap::new()).await;

    // Unknown at first…
    let response = harness
        .query(&build_query("late.internal", QTYPE_A))
        .await
        .expect("no response");
    assert_eq!(rcode(&response), 3);

    // …then the agent publishes a new snapshot (as it does on deploy).
    let mut map = ServiceMap::new();
    map.register_app("late", "default", 9000, None).unwrap();
    let vip = map.resolve(&ServiceId::new("default", "late")).unwrap().vip;
    harness.map_tx.send(map).unwrap();

    let response = harness
        .query(&build_query("late.internal", QTYPE_A))
        .await
        .expect("no response");
    assert_eq!(rcode(&response), 0);
    let len = response.len();
    assert_eq!(&response[len - 4..], &vip.0.octets());

    // Silence the unused-field lint path: address is used implicitly.
    let _ = harness.addr;
}
