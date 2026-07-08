/// Userspace DNS responder for `.internal` service names.
///
/// A lightweight UDP server that Bun runs on `127.0.0.53:53` (or a
/// configured address). Containers' `/etc/resolv.conf` points at this
/// address. When a query arrives for `*.internal`, we answer from the
/// service map; everything else is forwarded to the upstream resolver.
///
/// The responder is authoritative for `.internal`: names that don't
/// resolve get NXDOMAIN locally and are never forwarded, so internal
/// service names cannot leak to a public resolver.
///
/// This replaces the originally planned in-kernel eBPF DNS
/// interception, which turned out to be infeasible: the cgroup
/// sendmsg4/recvmsg4 hooks can modify socket addresses but can't
/// read or synthesise DNS packet payloads.
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use tokio::net::UdpSocket;
use tokio::sync::{Semaphore, watch};
use tokio_util::sync::CancellationToken;

use super::service_map::ServiceMap;
use super::vip::VirtualIP;

/// Practical EDNS0 UDP payload limit (RFC 6891 recommendation).
const MAX_PACKET: usize = 1232;

/// Upstream replies can be larger than what we advertise; oversized
/// relays get the TC bit so clients retry over TCP with the upstream.
const UPSTREAM_BUFFER: usize = 4096;

/// Maximum concurrent upstream forwards. Bounds the number of spawned
/// forwarding tasks so a query flood cannot exhaust memory.
const MAX_INFLIGHT_FORWARDS: usize = 64;

/// DNS RCODEs we produce.
const RCODE_SERVFAIL: u8 = 2;
const RCODE_NXDOMAIN: u8 = 3;
const RCODE_NOTIMP: u8 = 4;

/// QTYPEs we care about.
const QTYPE_A: u16 = 1;
const QTYPE_AAAA: u16 = 28;

/// Configuration for the DNS responder.
pub struct DnsConfig {
    /// Address to listen on (e.g. `127.0.0.53:53`).
    pub listen_addr: SocketAddr,
    /// Upstream DNS server for non-`.internal` queries.
    pub upstream: SocketAddr,
    /// How long to wait for an upstream reply before SERVFAIL.
    pub upstream_timeout: Duration,
}

impl Default for DnsConfig {
    fn default() -> Self {
        Self {
            listen_addr: SocketAddr::new(Ipv4Addr::new(127, 0, 0, 53).into(), 53),
            upstream: SocketAddr::new(Ipv4Addr::new(8, 8, 8, 8).into(), 53),
            upstream_timeout: Duration::from_secs(2),
        }
    }
}

/// Run the DNS responder until the cancellation token is triggered.
///
/// Reads the service map from a watch channel — the agent publishes a
/// snapshot whenever the map changes, so lookups here never contend
/// with the agent's event loop.
///
/// Returns an error only if the listen socket cannot be bound; once
/// serving, per-packet errors are logged and skipped, never fatal.
pub async fn run_dns_responder(
    config: DnsConfig,
    service_map: watch::Receiver<ServiceMap>,
    shutdown: CancellationToken,
) -> Result<(), std::io::Error> {
    let socket = Arc::new(UdpSocket::bind(config.listen_addr).await?);
    serve(config, socket, service_map, shutdown).await;
    Ok(())
}

/// Bind on the configured address and report the bound address.
///
/// Split from [`run_dns_responder`] so tests can bind port 0 and learn
/// the assigned port before sending queries.
pub async fn bind_dns_responder(
    config: &DnsConfig,
) -> Result<(Arc<UdpSocket>, SocketAddr), std::io::Error> {
    let socket = Arc::new(UdpSocket::bind(config.listen_addr).await?);
    let addr = socket.local_addr()?;
    Ok((socket, addr))
}

/// Serve DNS on an already-bound socket until shutdown.
pub async fn serve(
    config: DnsConfig,
    socket: Arc<UdpSocket>,
    service_map: watch::Receiver<ServiceMap>,
    shutdown: CancellationToken,
) {
    let forwards = Arc::new(Semaphore::new(MAX_INFLIGHT_FORWARDS));
    let mut buf = [0u8; MAX_PACKET];

    loop {
        tokio::select! {
            _ = shutdown.cancelled() => break,
            result = socket.recv_from(&mut buf) => {
                let (len, src) = match result {
                    Ok(pair) => pair,
                    // A single bad datagram (ICMP-induced error, truncated
                    // read) must never kill the resolver for every container
                    // on the node.
                    Err(e) => {
                        eprintln!("onion-dns: recv error (ignored): {e}");
                        continue;
                    }
                };
                let query = buf[..len].to_vec();

                let Some((name, qtype)) = parse_query(&query) else {
                    // Malformed packets are dropped, not forwarded: we
                    // won't relay bytes we can't parse to the upstream.
                    continue;
                };

                if let Some(stripped) = name.strip_suffix(".internal") {
                    let response = answer_internal(&service_map, &query, stripped, qtype);
                    let _ = socket.send_to(&response, src).await;
                    continue;
                }

                // Public name: forward on a task of its own so one slow
                // upstream exchange never delays other queries.
                let Ok(permit) = Arc::clone(&forwards).try_acquire_owned() else {
                    let _ = socket.send_to(&build_status_response(&query, RCODE_SERVFAIL), src).await;
                    continue;
                };
                let socket = Arc::clone(&socket);
                let upstream = config.upstream;
                let timeout = config.upstream_timeout;
                tokio::spawn(async move {
                    let _permit = permit;
                    let response = forward_upstream(&query, upstream, timeout)
                        .await
                        .unwrap_or_else(|| build_status_response(&query, RCODE_SERVFAIL));
                    let _ = socket.send_to(&response, src).await;
                });
            }
        }
    }
}

/// Answer a query for a name under `.internal`.
///
/// Authoritative: resolves A queries from the service map, returns an
/// empty NOERROR for AAAA on known names (we only have IPv4 VIPs),
/// NOTIMP for other query types, and NXDOMAIN for unknown names.
/// Never forwarded — internal names must not leak upstream.
fn answer_internal(
    service_map: &watch::Receiver<ServiceMap>,
    query: &[u8],
    app_name: &str,
    qtype: u16,
) -> Vec<u8> {
    let vip = service_map.borrow().resolve(app_name).map(|e| e.vip);
    match (vip, qtype) {
        (Some(vip), QTYPE_A) => build_a_response(query, vip),
        // The name exists but only as IPv4: empty NOERROR tells the
        // client "no AAAA records" without denying the name.
        (Some(_), QTYPE_AAAA) => build_status_response(query, 0),
        (Some(_), _) => build_status_response(query, RCODE_NOTIMP),
        (None, _) => build_status_response(query, RCODE_NXDOMAIN),
    }
}

/// Forward a query to the upstream resolver and return its reply.
///
/// Uses a fresh connected socket per query, so replies from any other
/// address are rejected by the kernel, and validates that the reply's
/// transaction ID matches the query (anti-spoofing). `None` means
/// timeout or error — the caller sends SERVFAIL.
async fn forward_upstream(
    query: &[u8],
    upstream: SocketAddr,
    timeout: Duration,
) -> Option<Vec<u8>> {
    let socket = UdpSocket::bind("0.0.0.0:0").await.ok()?;
    socket.connect(upstream).await.ok()?;
    socket.send(query).await.ok()?;

    let mut reply_buf = [0u8; UPSTREAM_BUFFER];
    let deadline = tokio::time::Instant::now() + timeout;

    // Loop: a reply with the wrong transaction ID is a spoof attempt
    // or a stale packet; keep waiting for the real one until deadline.
    loop {
        let remaining = deadline
            .checked_duration_since(tokio::time::Instant::now())
            .filter(|d| !d.is_zero())?;
        let n = tokio::time::timeout(remaining, socket.recv(&mut reply_buf))
            .await
            .ok()?
            .ok()?;
        if n >= 2 && reply_buf[..2] == query[..2] {
            let mut reply = reply_buf[..n].to_vec();
            // A reply that fills our whole buffer was probably cut off
            // mid-packet; set TC so the client retries over TCP.
            if n == UPSTREAM_BUFFER && reply.len() > 2 {
                reply[2] |= 0x02;
            }
            return Some(reply);
        }
    }
}

/// Parse the query name and QTYPE from a DNS packet.
///
/// Returns the name as a lowercase dotted string, or `None` if the
/// packet is malformed.
fn parse_query(packet: &[u8]) -> Option<(String, u16)> {
    // DNS header is 12 bytes
    if packet.len() < 13 {
        return None;
    }

    let mut pos = 12; // skip header
    let mut name = String::new();

    loop {
        if pos >= packet.len() {
            return None;
        }

        let label_len = packet[pos] as usize;
        pos += 1;

        if label_len == 0 {
            break; // end of name
        }

        if label_len > 63 || pos + label_len > packet.len() {
            return None; // invalid label
        }

        if !name.is_empty() {
            name.push('.');
        }

        for &b in &packet[pos..pos + label_len] {
            name.push(b.to_ascii_lowercase() as char);
        }

        pos += label_len;
    }

    if name.is_empty() {
        return None;
    }

    // QTYPE follows the name terminator
    if pos + 2 > packet.len() {
        return None;
    }
    let qtype = u16::from_be_bytes([packet[pos], packet[pos + 1]]);

    Some((name, qtype))
}

/// Build a minimal DNS A record response for a VIP.
fn build_a_response(query: &[u8], vip: VirtualIP) -> Vec<u8> {
    if query.len() < 12 {
        return Vec::new();
    }

    let mut response = Vec::with_capacity(query.len() + 16);

    // Copy the query ID
    response.extend_from_slice(&query[..2]);

    // Flags: QR=1 (response), AA=1 (authoritative), RCODE=0
    response.push(0x84); // QR=1, Opcode=0, AA=1, TC=0, RD=0
    response.push(0x00); // RA=0, Z=0, RCODE=0

    // QDCOUNT=1 (copy from query)
    response.extend_from_slice(&query[4..6]);
    // ANCOUNT=1
    response.push(0x00);
    response.push(0x01);
    // NSCOUNT=0
    response.push(0x00);
    response.push(0x00);
    // ARCOUNT=0
    response.push(0x00);
    response.push(0x00);

    // Copy the question section from the query
    let question_end = find_question_end(query);
    if question_end > 12 {
        response.extend_from_slice(&query[12..question_end]);
    }

    // Answer section: pointer to name in question (compression)
    response.push(0xC0); // pointer
    response.push(0x0C); // offset 12 (start of question name)

    // TYPE = A (1)
    response.push(0x00);
    response.push(0x01);
    // CLASS = IN (1)
    response.push(0x00);
    response.push(0x01);
    // TTL = 0 (always re-resolve; map is always current)
    response.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]);
    // RDLENGTH = 4
    response.push(0x00);
    response.push(0x04);
    // RDATA = IPv4 address
    response.extend_from_slice(&vip.0.octets());

    response
}

/// Build an answerless response with the given RCODE.
///
/// RCODE 0 with ANCOUNT=0 is the "name exists, no records of that
/// type" answer (used for AAAA on IPv4-only names); 2 is SERVFAIL,
/// 3 NXDOMAIN, 4 NOTIMP.
fn build_status_response(query: &[u8], rcode: u8) -> Vec<u8> {
    if query.len() < 12 {
        return Vec::new();
    }

    let mut response = Vec::with_capacity(query.len());

    response.extend_from_slice(&query[..2]); // ID
    response.push(0x84); // QR=1, AA=1
    response.push(rcode & 0x0F); // RA=0, Z=0, RCODE
    response.extend_from_slice(&query[4..6]); // QDCOUNT
    response.extend_from_slice(&[0x00, 0x00]); // ANCOUNT=0
    response.extend_from_slice(&[0x00, 0x00]); // NSCOUNT=0
    response.extend_from_slice(&[0x00, 0x00]); // ARCOUNT=0

    let question_end = find_question_end(query);
    if question_end > 12 {
        response.extend_from_slice(&query[12..question_end]);
    }

    response
}

/// Find the end of the question section in a DNS packet.
fn find_question_end(packet: &[u8]) -> usize {
    let mut pos = 12;

    // Skip the query name
    while pos < packet.len() {
        let label_len = packet[pos] as usize;
        pos += 1;
        if label_len == 0 {
            break;
        }
        pos += label_len;
    }

    // Skip QTYPE (2 bytes) and QCLASS (2 bytes)
    pos += 4;

    pos.min(packet.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    pub(crate) fn build_dns_query_typed(name: &str, qtype: u16) -> Vec<u8> {
        let mut packet = Vec::new();

        // Header: ID=0x1234, flags=standard query
        packet.extend_from_slice(&[0x12, 0x34]); // ID
        packet.extend_from_slice(&[0x01, 0x00]); // flags: RD=1
        packet.extend_from_slice(&[0x00, 0x01]); // QDCOUNT=1
        packet.extend_from_slice(&[0x00, 0x00]); // ANCOUNT=0
        packet.extend_from_slice(&[0x00, 0x00]); // NSCOUNT=0
        packet.extend_from_slice(&[0x00, 0x00]); // ARCOUNT=0

        // Question: encode name as DNS labels
        for label in name.split('.') {
            packet.push(label.len() as u8);
            packet.extend_from_slice(label.as_bytes());
        }
        packet.push(0x00); // end of name

        packet.extend_from_slice(&qtype.to_be_bytes());
        packet.extend_from_slice(&[0x00, 0x01]); // IN

        packet
    }

    fn build_dns_query(name: &str) -> Vec<u8> {
        build_dns_query_typed(name, QTYPE_A)
    }

    #[test]
    fn parse_simple_name() {
        let query = build_dns_query("redis.internal");
        let (name, qtype) = parse_query(&query).unwrap();
        assert_eq!(name, "redis.internal");
        assert_eq!(qtype, QTYPE_A);
    }

    #[test]
    fn parse_name_case_insensitive() {
        let query = build_dns_query("Redis.INTERNAL");
        let (name, _) = parse_query(&query).unwrap();
        assert_eq!(name, "redis.internal");
    }

    #[test]
    fn parse_aaaa_qtype() {
        let query = build_dns_query_typed("redis.internal", QTYPE_AAAA);
        let (_, qtype) = parse_query(&query).unwrap();
        assert_eq!(qtype, QTYPE_AAAA);
    }

    #[test]
    fn parse_non_internal_name() {
        let query = build_dns_query("api.stripe.com");
        let (name, _) = parse_query(&query).unwrap();
        assert_eq!(name, "api.stripe.com");
    }

    #[test]
    fn parse_malformed_returns_none() {
        let packet = vec![0u8; 5]; // too short
        assert!(parse_query(&packet).is_none());
    }

    #[test]
    fn parse_query_without_qtype_returns_none() {
        let mut query = build_dns_query("redis.internal");
        query.truncate(query.len() - 4); // drop QTYPE + QCLASS
        assert!(parse_query(&query).is_none());
    }

    #[test]
    fn build_response_has_correct_id() {
        let query = build_dns_query("redis.internal");
        let vip = VirtualIP(Ipv4Addr::new(127, 128, 0, 3));
        let response = build_a_response(&query, vip);

        assert_eq!(response[0], 0x12);
        assert_eq!(response[1], 0x34);
    }

    #[test]
    fn build_response_has_answer() {
        let query = build_dns_query("redis.internal");
        let vip = VirtualIP(Ipv4Addr::new(127, 128, 0, 3));
        let response = build_a_response(&query, vip);

        // ANCOUNT should be 1
        assert_eq!(response[6], 0x00);
        assert_eq!(response[7], 0x01);

        // Response should end with the VIP bytes
        let len = response.len();
        assert_eq!(&response[len - 4..], &[127, 128, 0, 3]);
    }

    #[test]
    fn build_response_is_authoritative() {
        let query = build_dns_query("redis.internal");
        let vip = VirtualIP(Ipv4Addr::new(127, 128, 0, 3));
        let response = build_a_response(&query, vip);

        // Flags byte: QR=1, AA=1
        assert_eq!(response[2] & 0x84, 0x84);
    }

    #[test]
    fn status_response_carries_rcode_and_no_answers() {
        let query = build_dns_query("ghost.internal");
        let response = build_status_response(&query, RCODE_NXDOMAIN);

        assert_eq!(&response[..2], &[0x12, 0x34]); // ID copied
        assert_eq!(response[3] & 0x0F, RCODE_NXDOMAIN);
        assert_eq!(&response[6..8], &[0x00, 0x00]); // ANCOUNT=0
    }

    #[test]
    fn internal_suffix_detection() {
        let query = build_dns_query("redis.internal");
        let (name, _) = parse_query(&query).unwrap();
        assert!(name.ends_with(".internal"));
        assert_eq!(name.strip_suffix(".internal"), Some("redis"));
    }

    #[test]
    fn non_internal_not_intercepted() {
        let query = build_dns_query("google.com");
        let (name, _) = parse_query(&query).unwrap();
        assert!(!name.ends_with(".internal"));
    }

    #[test]
    fn answer_internal_resolves_known_name_to_vip() {
        let mut map = ServiceMap::new();
        map.register_app("redis", "default", 6379, None).unwrap();
        let vip = map.resolve("redis").unwrap().vip;
        let (_tx, rx) = watch::channel(map);

        let query = build_dns_query("redis.internal");
        let response = answer_internal(&rx, &query, "redis", QTYPE_A);
        let len = response.len();
        assert_eq!(&response[len - 4..], &vip.0.octets());
    }

    #[test]
    fn answer_internal_unknown_name_is_nxdomain() {
        let (_tx, rx) = watch::channel(ServiceMap::new());
        let query = build_dns_query("ghost.internal");
        let response = answer_internal(&rx, &query, "ghost", QTYPE_A);
        assert_eq!(response[3] & 0x0F, RCODE_NXDOMAIN);
    }

    #[test]
    fn answer_internal_aaaa_on_known_name_is_empty_noerror() {
        let mut map = ServiceMap::new();
        map.register_app("redis", "default", 6379, None).unwrap();
        let (_tx, rx) = watch::channel(map);

        let query = build_dns_query_typed("redis.internal", QTYPE_AAAA);
        let response = answer_internal(&rx, &query, "redis", QTYPE_AAAA);
        assert_eq!(response[3] & 0x0F, 0, "expected NOERROR");
        assert_eq!(&response[6..8], &[0x00, 0x00], "expected no answers");
    }

    #[test]
    fn answer_internal_unsupported_qtype_is_notimp() {
        let mut map = ServiceMap::new();
        map.register_app("redis", "default", 6379, None).unwrap();
        let (_tx, rx) = watch::channel(map);

        let query = build_dns_query_typed("redis.internal", 15); // MX
        let response = answer_internal(&rx, &query, "redis", 15);
        assert_eq!(response[3] & 0x0F, RCODE_NOTIMP);
    }
}
