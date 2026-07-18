/// Userspace DNS responder for `.internal` service names.
///
/// A lightweight UDP/TCP server that Bun binds before reporting node
/// readiness. Runc containers use the node-side veth gateway as their
/// nameserver; on Linux, Bun uses `IP_FREEBIND` so it can bind that precise
/// address before the first veth creates it.
/// When a query arrives for `*.internal`, we answer from the service map;
/// everything else is forwarded to the upstream resolver.
///
/// The responder is authoritative for `.internal`: names that don't
/// resolve get NXDOMAIN locally and are never forwarded, so internal
/// service names cannot leak to a public resolver.
///
/// This replaces the originally planned in-kernel eBPF DNS
/// interception, which turned out to be infeasible: the cgroup
/// sendmsg4/recvmsg4 hooks can modify socket addresses but can't
/// read or synthesise DNS packet payloads.
use std::collections::BTreeMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, UdpSocket};
use tokio::sync::{Semaphore, watch};
use tokio_util::sync::CancellationToken;

use super::service_id::ServiceId;
use super::service_map::ServiceMap;
use super::vip::VirtualIP;

/// Which services the Smoker is currently forcing NXDOMAIN for.
///
/// This is the userspace equivalent of the retired in-kernel DNS fault
/// map. The Smoker's `DnsNxdomain` fault used to write a `fault_dns_map`
/// entry into an eBPF object that was never loaded, so the fault did
/// nothing. DNS resolution lives entirely in this responder now, so the
/// fault lives here too: the agent publishes the set of faulted service
/// names (with their expiry) on a `watch` channel, and [`answer_internal`]
/// returns NXDOMAIN for any name that matches while the fault is live.
///
/// A fault targets a service by its bare app name (`redis`), the same way
/// the Smoker keys every other fault, so a name is faulted in every
/// namespace it appears in. Expiry is belt-and-braces: the agent removes
/// a fault from the published set when it clears or expires, and the
/// resolver also ignores any entry whose deadline has already passed, so
/// a fault can never outlive its window even if a publish is missed.
#[derive(Debug, Clone, Default)]
pub struct DnsFaultState {
    /// App name → expiry (`CLOCK_MONOTONIC` nanoseconds, matching the
    /// Smoker registry's `expires_at_ns`). `0` means "no expiry".
    faulted: BTreeMap<String, u64>,
}

impl DnsFaultState {
    /// Build a fault state from `(app name, expiry_ns)` pairs.
    pub fn from_faults(faults: impl IntoIterator<Item = (String, u64)>) -> Self {
        Self {
            faulted: faults.into_iter().collect(),
        }
    }

    /// Whether `app` should be forced to NXDOMAIN right now.
    ///
    /// `now_ns` is the current `CLOCK_MONOTONIC` reading. A fault with a
    /// non-zero expiry that has already passed is treated as gone even if
    /// it's still in the map, so a stale publish can't keep a name dark.
    pub fn is_faulted(&self, app: &str, now_ns: u64) -> bool {
        match self.faulted.get(app) {
            Some(&expires_ns) => expires_ns == 0 || now_ns < expires_ns,
            None => false,
        }
    }
}

/// Practical EDNS0 UDP payload limit (RFC 6891 recommendation).
const MAX_PACKET: usize = 1232;

/// Upstream replies can be larger than what we advertise; oversized
/// relays get the TC bit so clients retry over TCP with the upstream.
const UPSTREAM_BUFFER: usize = 4096;

/// Maximum concurrent upstream forwards. Bounds the number of spawned
/// forwarding tasks so a query flood cannot exhaust memory.
const MAX_INFLIGHT_FORWARDS: usize = 64;

/// Maximum clients allowed to hold a DNS-over-TCP task at once.
const MAX_INFLIGHT_TCP: usize = 64;

/// A client that never finishes its length-prefixed query must release its
/// TCP slot promptly instead of pinning one forever.
const TCP_QUERY_TIMEOUT: Duration = Duration::from_secs(5);

/// DNS RCODEs we produce.
const RCODE_SERVFAIL: u8 = 2;
const RCODE_NXDOMAIN: u8 = 3;
const RCODE_NOTIMP: u8 = 4;
const RCODE_REFUSED: u8 = 5;

/// QTYPEs we care about.
const QTYPE_A: u16 = 1;
const QTYPE_AAAA: u16 = 28;

/// Live DNS capability published by a node.
///
/// `ready` only becomes true after both UDP and TCP sockets have bound.
/// `workload_reachable` additionally means the selected runtime can install
/// and reach the advertised resolver address; a host-side socket alone isn't
/// enough.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DnsCapability {
    /// The operator enabled the `.internal` resolver on this node.
    pub enabled: bool,
    /// Both DNS transports bound before Bun entered its ready state.
    pub ready: bool,
    /// The workload resolver path supports DNS over IPv4.
    pub ipv4: bool,
    /// The workload resolver path supports DNS over IPv6.
    pub ipv6: bool,
    /// The selected runtime can reach the address installed in workloads.
    pub workload_reachable: bool,
}

impl DnsCapability {
    /// Whether this node can safely host a workload in a DNS-enabled cluster.
    pub fn can_resolve_internal(self) -> bool {
        self.enabled && self.ready && self.workload_reachable && (self.ipv4 || self.ipv6)
    }
}

/// Configuration for the DNS responder.
#[derive(Debug, Clone)]
pub struct DnsConfig {
    /// Address to listen on (e.g. `0.0.0.0:53` for runc workloads).
    pub listen_addr: SocketAddr,
    /// Upstream DNS server for non-`.internal` queries.
    pub upstream: SocketAddr,
    /// How long to wait for an upstream reply before SERVFAIL.
    pub upstream_timeout: Duration,
    /// Namespace a bare `<app>.internal` query resolves within.
    ///
    /// A container that asks for `redis.internal` (rather than the fully
    /// qualified `redis.payments.internal`) means "redis in my namespace".
    /// The userspace responder can't see the querying container's cgroup,
    /// so it falls back to this namespace — set per-node to the namespace
    /// the node predominantly serves, `default` otherwise. Fully qualified
    /// `<app>.<namespace>.internal` queries ignore it.
    pub default_namespace: String,
    /// Which source addresses may query the `.internal` zone.
    pub source_acl: SourceAcl,
}

impl Default for DnsConfig {
    fn default() -> Self {
        Self {
            listen_addr: SocketAddr::new(Ipv4Addr::UNSPECIFIED.into(), 53),
            upstream: SocketAddr::new(Ipv4Addr::new(8, 8, 8, 8).into(), 53),
            upstream_timeout: Duration::from_secs(2),
            default_namespace: "default".to_string(),
            source_acl: SourceAcl::default(),
        }
    }
}

/// DNS sockets bound during Bun startup.
///
/// Keeping binding separate from serving closes the readiness race: callers
/// can fail startup before they publish a capability or create a workload.
pub struct BoundDnsResponder {
    config: Arc<DnsConfig>,
    udp: Arc<UdpSocket>,
    tcp: TcpListener,
}

impl BoundDnsResponder {
    /// Bind both UDP and TCP on the configured address.
    pub async fn bind(mut config: DnsConfig) -> Result<Self, std::io::Error> {
        let udp = Arc::new(UdpSocket::bind(config.listen_addr).await?);
        // Port zero is useful for tests and embedded callers. Bind UDP first,
        // then make TCP use the same kernel-selected port rather than letting
        // each transport receive a different ephemeral port.
        if config.listen_addr.port() == 0 {
            config.listen_addr.set_port(udp.local_addr()?.port());
        }
        let tcp = TcpListener::bind(config.listen_addr).await?;
        Ok(Self {
            config: Arc::new(config),
            udp,
            tcp,
        })
    }

    /// Bind an IPv4 address that the kernel will add after startup.
    ///
    /// Rootful runc creates its gateway address with the first workload veth,
    /// but readiness must be established before any workload exists. Linux's
    /// `IP_FREEBIND` provides exactly that ordering without binding wildcard
    /// port 53 (which commonly conflicts with the host resolver).
    #[cfg(target_os = "linux")]
    pub fn bind_freebind(config: DnsConfig) -> Result<Self, std::io::Error> {
        use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

        fn socket(config: &DnsConfig, socket_type: libc::c_int) -> std::io::Result<OwnedFd> {
            let IpAddr::V4(ip) = config.listen_addr.ip() else {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "IP_FREEBIND DNS requires an IPv4 listen address",
                ));
            };
            // SAFETY: `socket` receives valid AF_INET/type/protocol constants.
            // On success the returned descriptor is immediately owned by
            // `OwnedFd`, so every later error closes it exactly once.
            let raw = unsafe {
                libc::socket(
                    libc::AF_INET,
                    socket_type | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
                    0,
                )
            };
            if raw < 0 {
                return Err(std::io::Error::last_os_error());
            }
            // SAFETY: `raw` is a fresh successful socket descriptor and
            // ownership has not been transferred anywhere else.
            let fd = unsafe { OwnedFd::from_raw_fd(raw) };
            let enabled: libc::c_int = 1;
            // SAFETY: the descriptor is live, and the pointer/length describe
            // one initialised `c_int`, as IP_FREEBIND requires.
            let result = unsafe {
                libc::setsockopt(
                    fd.as_raw_fd(),
                    libc::IPPROTO_IP,
                    libc::IP_FREEBIND,
                    std::ptr::from_ref(&enabled).cast(),
                    std::mem::size_of_val(&enabled) as libc::socklen_t,
                )
            };
            if result < 0 {
                return Err(std::io::Error::last_os_error());
            }

            let address = libc::sockaddr_in {
                sin_family: libc::AF_INET as libc::sa_family_t,
                sin_port: config.listen_addr.port().to_be(),
                sin_addr: libc::in_addr {
                    s_addr: u32::from_ne_bytes(ip.octets()),
                },
                sin_zero: [0; 8],
            };
            // SAFETY: `address` is a fully initialised IPv4 sockaddr and its
            // pointer remains valid for the duration of this call.
            let result = unsafe {
                libc::bind(
                    fd.as_raw_fd(),
                    std::ptr::from_ref(&address).cast(),
                    std::mem::size_of_val(&address) as libc::socklen_t,
                )
            };
            if result < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(fd)
        }

        let udp = std::net::UdpSocket::from(socket(&config, libc::SOCK_DGRAM)?);
        let tcp = std::net::TcpListener::from(socket(&config, libc::SOCK_STREAM)?);
        // SAFETY: `tcp` is a bound stream socket and backlog is a valid
        // non-negative listen queue length.
        if unsafe { libc::listen(tcp.as_raw_fd(), 128) } < 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(Self {
            config: Arc::new(config),
            udp: Arc::new(UdpSocket::from_std(udp)?),
            tcp: TcpListener::from_std(tcp)?,
        })
    }

    /// Return the address on which the UDP responder is listening.
    pub fn local_addr(&self) -> Result<SocketAddr, std::io::Error> {
        self.udp.local_addr()
    }

    /// Serve the already-bound sockets until shutdown.
    pub async fn run(
        self,
        service_map: watch::Receiver<ServiceMap>,
        dns_faults: watch::Receiver<DnsFaultState>,
        shutdown: CancellationToken,
    ) {
        let udp = serve(
            Arc::clone(&self.config),
            self.udp,
            service_map.clone(),
            dns_faults.clone(),
            shutdown.clone(),
        );
        let tcp = serve_tcp(self.config, self.tcp, service_map, dns_faults, shutdown);
        // Both loops normally finish only on shutdown. If either returns or
        // panics first, this combined future also ends, and Bun's outer task
        // monitor withdraws readiness by shutting the node down.
        tokio::select! {
            _ = udp => {}
            _ = tcp => {}
        }
    }
}

/// Which client addresses may resolve `.internal` service names.
///
/// The `.internal` zone exposes the cluster's internal topology, so only
/// container-reachable clients should see it: loopback (the node's own
/// resolver path) and the private ranges containers get addresses from
/// (RFC 1918 plus the CGNAT block Lima/runc bridges use). A query from a
/// public address is refused with REFUSED, never answered and never
/// forwarded — internal names must not leak to, or be probed by, the
/// outside world.
#[derive(Debug, Clone)]
pub struct SourceAcl {
    /// When true, only loopback and private-range sources are served
    /// internal answers. When false, every source is served (used only
    /// in tests that drive the parser directly).
    pub restrict_to_private: bool,
}

impl Default for SourceAcl {
    fn default() -> Self {
        Self {
            restrict_to_private: true,
        }
    }
}

impl SourceAcl {
    /// Whether `src` may resolve internal names under this ACL.
    pub fn allows(&self, src: IpAddr) -> bool {
        if !self.restrict_to_private {
            return true;
        }
        match src {
            IpAddr::V4(v4) => {
                v4.is_loopback()
                    || v4.is_private()
                    // 100.64.0.0/10 — CGNAT, used by Lima/runc bridges.
                    || (v4.octets()[0] == 100 && (64..=127).contains(&v4.octets()[1]))
                    // VIP range itself, so a container reusing its resolver works.
                    || VirtualIP::is_in_vip_range(v4)
            }
            // IPv6 loopback only; the networking model is IPv4 today.
            IpAddr::V6(v6) => v6.is_loopback(),
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
    dns_faults: watch::Receiver<DnsFaultState>,
    shutdown: CancellationToken,
) -> Result<(), std::io::Error> {
    BoundDnsResponder::bind(config)
        .await?
        .run(service_map, dns_faults, shutdown)
        .await;
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

/// Serve DNS on an already-bound UDP socket until shutdown.
pub async fn serve(
    config: Arc<DnsConfig>,
    socket: Arc<UdpSocket>,
    service_map: watch::Receiver<ServiceMap>,
    dns_faults: watch::Receiver<DnsFaultState>,
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

                // Bun binds only the runc gateway in production, but the
                // responder also supports explicit/wildcard test listeners.
                // Never let one become an open recursive resolver: public
                // sources are refused for internal and external names.
                if !config.source_acl.allows(src.ip()) {
                    let response = build_status_response(&query, RCODE_REFUSED);
                    let _ = socket.send_to(&response, src).await;
                    continue;
                }

                if let Some(stripped) = name.strip_suffix(".internal") {
                    let response = answer_internal(
                        &config,
                        &service_map,
                        &dns_faults,
                        &query,
                        stripped,
                        qtype,
                        src.ip(),
                    );
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

/// Serve DNS over TCP until shutdown.
///
/// TCP exists for answers larger than a UDP datagram: a client that gets a
/// truncated (TC-bit) UDP reply retries the whole query over TCP. Each DNS
/// message is length-prefixed with a 2-byte big-endian length. We only
/// answer `.internal` names here; a non-internal name over TCP gets
/// SERVFAIL rather than a forwarded relay (upstream TCP relaying isn't
/// needed for the internal zone this responder is authoritative for).
async fn serve_tcp(
    config: Arc<DnsConfig>,
    listener: TcpListener,
    service_map: watch::Receiver<ServiceMap>,
    dns_faults: watch::Receiver<DnsFaultState>,
    shutdown: CancellationToken,
) {
    let clients = Arc::new(Semaphore::new(MAX_INFLIGHT_TCP));
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => break,
            accepted = listener.accept() => {
                let Ok((stream, peer)) = accepted else { continue };
                let Ok(permit) = Arc::clone(&clients).try_acquire_owned() else {
                    continue;
                };
                let config = Arc::clone(&config);
                let service_map = service_map.clone();
                let dns_faults = dns_faults.clone();
                tokio::spawn(async move {
                    let _permit = permit;
                    let _ = tokio::time::timeout(
                        TCP_QUERY_TIMEOUT,
                        answer_tcp_query(stream, peer, config, service_map, dns_faults),
                    )
                    .await;
                });
            }
        }
    }
}

/// Read and answer one bounded DNS-over-TCP query.
async fn answer_tcp_query(
    mut stream: tokio::net::TcpStream,
    peer: SocketAddr,
    config: Arc<DnsConfig>,
    service_map: watch::Receiver<ServiceMap>,
    dns_faults: watch::Receiver<DnsFaultState>,
) {
    let mut len_buf = [0u8; 2];
    if stream.read_exact(&mut len_buf).await.is_err() {
        return;
    }
    let qlen = u16::from_be_bytes(len_buf) as usize;
    if qlen == 0 || qlen > UPSTREAM_BUFFER {
        return;
    }
    let mut query = vec![0u8; qlen];
    if stream.read_exact(&mut query).await.is_err() {
        return;
    }
    let Some((name, qtype)) = parse_query(&query) else {
        return;
    };
    let response = if !config.source_acl.allows(peer.ip()) {
        build_status_response(&query, RCODE_REFUSED)
    } else {
        match name.strip_suffix(".internal") {
            Some(stripped) => answer_internal(
                &config,
                &service_map,
                &dns_faults,
                &query,
                stripped,
                qtype,
                peer.ip(),
            ),
            None => build_status_response(&query, RCODE_SERVFAIL),
        }
    };
    let mut framed = Vec::with_capacity(response.len() + 2);
    framed.extend_from_slice(&(response.len() as u16).to_be_bytes());
    framed.extend_from_slice(&response);
    let _ = stream.write_all(&framed).await;
}

/// Answer a query for a name under `.internal`.
///
/// Enforces the source ACL first: a client outside the container-reachable
/// ranges gets REFUSED, so the internal topology never leaks. The name is
/// then resolved namespace-aware: `<app>.<namespace>` targets that exact
/// service, while a bare `<app>` resolves in `config.default_namespace`.
///
/// Authoritative: resolves A queries from the service map, returns an
/// empty NOERROR for AAAA on known names (we only have IPv4 VIPs),
/// NOTIMP for other query types, and NXDOMAIN for unknown names.
/// Never forwarded — internal names must not leak upstream.
///
/// A Smoker `DnsNxdomain` fault against the queried app forces NXDOMAIN
/// even for a name that would otherwise resolve. That's the whole point
/// of the fault — make an operator prove their service tolerates its
/// dependency's name going dark — so it's checked before the service map.
#[allow(clippy::too_many_arguments)]
fn answer_internal(
    config: &DnsConfig,
    service_map: &watch::Receiver<ServiceMap>,
    dns_faults: &watch::Receiver<DnsFaultState>,
    query: &[u8],
    stripped: &str,
    qtype: u16,
    src: IpAddr,
) -> Vec<u8> {
    if !config.source_acl.allows(src) {
        return build_status_response(query, RCODE_REFUSED);
    }

    let service_id = service_id_for(stripped, &config.default_namespace);

    // Smoker DNS fault: a targeted app is forced to NXDOMAIN regardless of
    // whether it resolves. The fault is keyed by bare app name (matching
    // how every Smoker fault targets a service), so it bites in whatever
    // namespace the query lands in.
    let now_ns = crate::smoker::types::monotonic_now_ns();
    if dns_faults.borrow().is_faulted(&service_id.name, now_ns) {
        return build_status_response(query, RCODE_NXDOMAIN);
    }

    let vip = service_map.borrow().resolve(&service_id).map(|e| e.vip);
    match (vip, qtype) {
        (Some(vip), QTYPE_A) => build_a_response(query, vip),
        // The name exists but only as IPv4: empty NOERROR tells the
        // client "no AAAA records" without denying the name.
        (Some(_), QTYPE_AAAA) => build_status_response(query, 0),
        (Some(_), _) => build_status_response(query, RCODE_NOTIMP),
        (None, _) => build_status_response(query, RCODE_NXDOMAIN),
    }
}

/// Map the stripped `.internal` label(s) to a [`ServiceId`].
///
/// `<app>.<namespace>` becomes `ServiceId { namespace, app }`; a bare
/// `<app>` (no dot) resolves in `default_namespace`. A name with more than
/// two labels keeps the first as the app and the second as the namespace
/// (deeper labels are ignored — there's no third level in the scheme).
fn service_id_for(stripped: &str, default_namespace: &str) -> ServiceId {
    match stripped.split_once('.') {
        Some((app, namespace)) => ServiceId::new(namespace, app),
        None => ServiceId::new(default_namespace, stripped),
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

    /// A test config that serves every source (the ACL is exercised
    /// separately), resolving bare names in `default`.
    fn test_config() -> DnsConfig {
        DnsConfig {
            source_acl: SourceAcl {
                restrict_to_private: false,
            },
            ..DnsConfig::default()
        }
    }

    const LOOPBACK: IpAddr = IpAddr::V4(Ipv4Addr::LOCALHOST);

    /// A receiver carrying no active DNS faults (the normal case).
    fn no_dns_faults() -> watch::Receiver<DnsFaultState> {
        watch::channel(DnsFaultState::default()).1
    }

    #[test]
    fn service_id_for_qualified_name() {
        let id = service_id_for("api.payments", "default");
        assert_eq!(id.namespace, "payments");
        assert_eq!(id.name, "api");
    }

    #[test]
    fn service_id_for_bare_name_uses_default_namespace() {
        let id = service_id_for("api", "team-b");
        assert_eq!(id.namespace, "team-b");
        assert_eq!(id.name, "api");
    }

    #[test]
    fn answer_internal_resolves_qualified_name_to_vip() {
        let mut map = ServiceMap::new();
        map.register_app("redis", "default", 6379, None).unwrap();
        let vip = map
            .resolve(&ServiceId::new("default", "redis"))
            .unwrap()
            .vip;
        let (_tx, rx) = watch::channel(map);

        let query = build_dns_query("redis.default.internal");
        let response = answer_internal(
            &test_config(),
            &rx,
            &no_dns_faults(),
            &query,
            "redis.default",
            QTYPE_A,
            LOOPBACK,
        );
        let len = response.len();
        assert_eq!(&response[len - 4..], &vip.0.octets());
    }

    #[test]
    fn answer_internal_bare_name_resolves_in_default_namespace() {
        let mut map = ServiceMap::new();
        map.register_app("redis", "default", 6379, None).unwrap();
        let vip = map
            .resolve(&ServiceId::new("default", "redis"))
            .unwrap()
            .vip;
        let (_tx, rx) = watch::channel(map);

        let query = build_dns_query("redis.internal");
        let response = answer_internal(
            &test_config(),
            &rx,
            &no_dns_faults(),
            &query,
            "redis",
            QTYPE_A,
            LOOPBACK,
        );
        let len = response.len();
        assert_eq!(&response[len - 4..], &vip.0.octets());
    }

    #[test]
    fn answer_internal_qualified_names_resolve_each_namespace_independently() {
        // The D3/codex-M1 regression, at the DNS layer: `api` exists in two
        // namespaces and each qualified query resolves to its own VIP.
        let mut map = ServiceMap::new();
        map.register_app("api", "default", 3000, None).unwrap();
        map.register_app("api", "payments", 3000, None).unwrap();
        let default_vip = map.resolve(&ServiceId::new("default", "api")).unwrap().vip;
        let payments_vip = map.resolve(&ServiceId::new("payments", "api")).unwrap().vip;
        assert_ne!(default_vip, payments_vip);
        let (_tx, rx) = watch::channel(map);

        let faults = no_dns_faults();
        let q1 = build_dns_query("api.default.internal");
        let r1 = answer_internal(
            &test_config(),
            &rx,
            &faults,
            &q1,
            "api.default",
            QTYPE_A,
            LOOPBACK,
        );
        assert_eq!(&r1[r1.len() - 4..], &default_vip.0.octets());

        let q2 = build_dns_query("api.payments.internal");
        let r2 = answer_internal(
            &test_config(),
            &rx,
            &faults,
            &q2,
            "api.payments",
            QTYPE_A,
            LOOPBACK,
        );
        assert_eq!(&r2[r2.len() - 4..], &payments_vip.0.octets());
    }

    #[test]
    fn answer_internal_refuses_non_container_source() {
        let mut map = ServiceMap::new();
        map.register_app("redis", "default", 6379, None).unwrap();
        let (_tx, rx) = watch::channel(map);

        // A public source address must be REFUSED, never answered — even for
        // a name that exists.
        let public = IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8));
        let config = DnsConfig::default(); // restrict_to_private = true
        let query = build_dns_query("redis.internal");
        let response = answer_internal(
            &config,
            &rx,
            &no_dns_faults(),
            &query,
            "redis",
            QTYPE_A,
            public,
        );
        assert_eq!(response[3] & 0x0F, RCODE_REFUSED);
    }

    #[test]
    fn source_acl_allows_private_and_loopback_rejects_public() {
        let acl = SourceAcl::default();
        assert!(acl.allows(IpAddr::V4(Ipv4Addr::LOCALHOST)));
        assert!(acl.allows(IpAddr::V4(Ipv4Addr::new(10, 0, 2, 2))));
        assert!(acl.allows(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 5))));
        assert!(acl.allows(IpAddr::V4(Ipv4Addr::new(100, 88, 0, 1)))); // CGNAT
        assert!(!acl.allows(IpAddr::V4(Ipv4Addr::new(8, 8, 8, 8))));
        assert!(!acl.allows(IpAddr::V4(Ipv4Addr::new(1, 1, 1, 1))));
    }

    #[test]
    fn answer_internal_unknown_name_is_nxdomain() {
        let (_tx, rx) = watch::channel(ServiceMap::new());
        let query = build_dns_query("ghost.internal");
        let response = answer_internal(
            &test_config(),
            &rx,
            &no_dns_faults(),
            &query,
            "ghost",
            QTYPE_A,
            LOOPBACK,
        );
        assert_eq!(response[3] & 0x0F, RCODE_NXDOMAIN);
    }

    #[test]
    fn answer_internal_aaaa_on_known_name_is_empty_noerror() {
        let mut map = ServiceMap::new();
        map.register_app("redis", "default", 6379, None).unwrap();
        let (_tx, rx) = watch::channel(map);

        let query = build_dns_query_typed("redis.internal", QTYPE_AAAA);
        let response = answer_internal(
            &test_config(),
            &rx,
            &no_dns_faults(),
            &query,
            "redis",
            QTYPE_AAAA,
            LOOPBACK,
        );
        assert_eq!(response[3] & 0x0F, 0, "expected NOERROR");
        assert_eq!(&response[6..8], &[0x00, 0x00], "expected no answers");
    }

    #[test]
    fn answer_internal_unsupported_qtype_is_notimp() {
        let mut map = ServiceMap::new();
        map.register_app("redis", "default", 6379, None).unwrap();
        let (_tx, rx) = watch::channel(map);

        let query = build_dns_query_typed("redis.internal", 15); // MX
        let response = answer_internal(
            &test_config(),
            &rx,
            &no_dns_faults(),
            &query,
            "redis",
            15,
            LOOPBACK,
        );
        assert_eq!(response[3] & 0x0F, RCODE_NOTIMP);
    }

    #[test]
    fn dns_nxdomain_fault_forces_nxdomain_for_the_targeted_service() {
        // Two services that both normally resolve; the fault targets only
        // `redis`. `redis` must go NXDOMAIN while `api` still resolves.
        let mut map = ServiceMap::new();
        map.register_app("redis", "default", 6379, None).unwrap();
        map.register_app("api", "default", 3000, None).unwrap();
        let api_vip = map.resolve(&ServiceId::new("default", "api")).unwrap().vip;
        let (_map_tx, map_rx) = watch::channel(map);

        // No expiry (0) — the fault is live until cleared.
        let (_fault_tx, fault_rx) =
            watch::channel(DnsFaultState::from_faults([("redis".to_string(), 0)]));

        let redis_query = build_dns_query("redis.internal");
        let redis_resp = answer_internal(
            &test_config(),
            &map_rx,
            &fault_rx,
            &redis_query,
            "redis",
            QTYPE_A,
            LOOPBACK,
        );
        assert_eq!(
            redis_resp[3] & 0x0F,
            RCODE_NXDOMAIN,
            "faulted service must return NXDOMAIN even though it resolves to a VIP"
        );

        let api_query = build_dns_query("api.internal");
        let api_resp = answer_internal(
            &test_config(),
            &map_rx,
            &fault_rx,
            &api_query,
            "api",
            QTYPE_A,
            LOOPBACK,
        );
        assert_eq!(
            &api_resp[api_resp.len() - 4..],
            &api_vip.0.octets(),
            "a service the fault doesn't target must still resolve"
        );
    }

    #[test]
    fn dns_nxdomain_fault_is_reversed_on_clear_and_expiry() {
        let mut map = ServiceMap::new();
        map.register_app("redis", "default", 6379, None).unwrap();
        let vip = map
            .resolve(&ServiceId::new("default", "redis"))
            .unwrap()
            .vip;
        let (_map_tx, map_rx) = watch::channel(map);

        let query = build_dns_query("redis.internal");
        let resolves = |fault_rx: &watch::Receiver<DnsFaultState>| {
            let resp = answer_internal(
                &test_config(),
                &map_rx,
                fault_rx,
                &query,
                "redis",
                QTYPE_A,
                LOOPBACK,
            );
            resp[resp.len() - 4..] == vip.0.octets()
        };

        // Clear: the agent publishes an empty state, and the service resolves
        // again immediately.
        let (fault_tx, fault_rx) =
            watch::channel(DnsFaultState::from_faults([("redis".to_string(), 0)]));
        assert!(!resolves(&fault_rx), "fault active — should not resolve");
        fault_tx.send(DnsFaultState::default()).unwrap();
        assert!(
            resolves(&fault_rx),
            "after clear the service resolves again"
        );

        // Expiry: an entry whose deadline has already passed is ignored even
        // if a publish is missed. `1` ns is comfortably in the past for the
        // monotonic clock (which reads seconds-since-boot).
        let (_expired_tx, expired_rx) =
            watch::channel(DnsFaultState::from_faults([("redis".to_string(), 1)]));
        assert!(
            resolves(&expired_rx),
            "an expired fault must not keep the name dark"
        );
    }
}
