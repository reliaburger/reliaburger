/// Wrapper: ingress reverse proxy.
///
/// Routes external HTTP(S) traffic to backend containers. Runs on
/// a dedicated tokio runtime so a traffic flood can't starve cluster
/// operations (gossip, Raft, health checks).
///
/// Reads the routing table from the Onion `ServiceMap`. Supports
/// host/path routing, round-robin load balancing, WebSocket upgrade,
/// connection draining, and per-IP rate limiting.
pub mod proxy;
pub mod routing;
pub mod types;
