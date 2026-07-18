/// Onion: eBPF service discovery.
///
/// Resolves `*.internal` names through Bun's supervised userspace DNS
/// responder, then rewrites `connect()` calls for the returned virtual IPs
/// to healthy backend addresses with an in-kernel eBPF hook.
///
/// The userspace `ServiceMap` works on all platforms and provides
/// the data model for `relish resolve`. On Linux with the `ebpf`
/// feature, the map is additionally synced to BPF hash maps in the
/// kernel for zero-latency, zero-copy connection steering.
pub mod catalog;
pub mod dns;
pub mod service_id;
pub mod service_map;
pub mod types;
pub mod vip;

#[cfg(target_os = "linux")]
pub mod ebpf;
