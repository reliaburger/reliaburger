/// Userspace DNS responder for `.internal` service names.
///
/// A lightweight UDP server that Bun runs on `127.0.0.53:53` (or a
/// configured port). Containers' `/etc/resolv.conf` points at this
/// address. When a query arrives for `*.internal`, we look up the
/// VIP in the `ServiceMap` and respond. Non-`.internal` queries are
/// forwarded to the upstream resolver.
///
/// This replaces the originally planned in-kernel eBPF DNS
/// interception, which turned out to be infeasible: the cgroup
/// sendmsg4/recvmsg4 hooks can modify socket addresses but can't
/// read or synthesise DNS packet payloads.
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;

use tokio::net::UdpSocket;
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;

use super::service_map::ServiceMap;
use super::vip::VirtualIP;

/// Configuration for the DNS responder.
pub struct DnsConfig {
    /// Address to listen on (e.g. `127.0.0.53:53`).
    pub listen_addr: SocketAddr,
    /// Upstream DNS server for non-`.internal` queries.
    pub upstream: SocketAddr,
}

impl Default for DnsConfig {
    fn default() -> Self {
        Self {
            listen_addr: SocketAddr::new(Ipv4Addr::new(127, 0, 0, 53).into(), 53),
            upstream: SocketAddr::new(Ipv4Addr::new(8, 8, 8, 8).into(), 53),
        }
    }
}

/// Run the DNS responder until the cancellation token is triggered.
///
/// Spawned as a background task by the Bun agent. Reads queries
/// from the UDP socket, checks for `.internal` suffix, and either
/// responds with a VIP or forwards to the upstream resolver.
pub async fn run_dns_responder(
    config: DnsConfig,
    service_map: Arc<RwLock<ServiceMap>>,
    shutdown: CancellationToken,
) -> Result<(), std::io::Error> {
    let socket = UdpSocket::bind(config.listen_addr).await?;
    let upstream_socket = UdpSocket::bind("0.0.0.0:0").await?;

    let mut buf = [0u8; 512];
    let mut upstream_buf = [0u8; 512];

    loop {
        tokio::select! {
            _ = shutdown.cancelled() => break,
            result = socket.recv_from(&mut buf) => {
                let (len, src) = result?;
                let query = &buf[..len];

                if let Some(name) = parse_query_name(query)
                    && let Some(stripped) = name.strip_suffix(".internal")
                {
                    let map = service_map.read().await;
                    if let Some(entry) = map.resolve(stripped) {
                        let response = build_a_response(query, entry.vip);
                        let _ = socket.send_to(&response, src).await;
                        continue;
                    }
                }

                // Not .internal or not found — forward to upstream
                let _ = upstream_socket.send_to(query, config.upstream).await;
                if let Ok(Ok((n, _))) = tokio::time::timeout(
                    std::time::Duration::from_secs(2),
                    upstream_socket.recv_from(&mut upstream_buf),
                )
                .await
                {
                    let _ = socket.send_to(&upstream_buf[..n], src).await;
                }
            }
        }
    }

    Ok(())
}

/// Parse the query name from a DNS packet.
///
/// Returns the name as a lowercase dotted string, or `None` if
/// the packet is malformed.
fn parse_query_name(packet: &[u8]) -> Option<String> {
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

    if name.is_empty() { None } else { Some(name) }
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

    fn build_dns_query(name: &str) -> Vec<u8> {
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

        // QTYPE=A, QCLASS=IN
        packet.extend_from_slice(&[0x00, 0x01]); // A
        packet.extend_from_slice(&[0x00, 0x01]); // IN

        packet
    }

    #[test]
    fn parse_simple_name() {
        let query = build_dns_query("redis.internal");
        let name = parse_query_name(&query).unwrap();
        assert_eq!(name, "redis.internal");
    }

    #[test]
    fn parse_name_case_insensitive() {
        let query = build_dns_query("Redis.INTERNAL");
        let name = parse_query_name(&query).unwrap();
        assert_eq!(name, "redis.internal");
    }

    #[test]
    fn parse_non_internal_name() {
        let query = build_dns_query("api.stripe.com");
        let name = parse_query_name(&query).unwrap();
        assert_eq!(name, "api.stripe.com");
    }

    #[test]
    fn parse_malformed_returns_none() {
        let packet = vec![0u8; 5]; // too short
        assert!(parse_query_name(&packet).is_none());
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
    fn internal_suffix_detection() {
        let query = build_dns_query("redis.internal");
        let name = parse_query_name(&query).unwrap();
        assert!(name.ends_with(".internal"));
        assert_eq!(name.strip_suffix(".internal"), Some("redis"));
    }

    #[test]
    fn non_internal_not_intercepted() {
        let query = build_dns_query("google.com");
        let name = parse_query_name(&query).unwrap();
        assert!(!name.ends_with(".internal"));
    }
}
