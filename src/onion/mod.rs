/// Onion: eBPF service discovery.
///
/// Replaces DNS servers and proxy processes with in-kernel eBPF
/// programs. DNS queries for `*.internal` names are intercepted at
/// the socket layer and resolved to virtual IPs. `connect()` calls
/// to those VIPs are rewritten to healthy backend addresses.
///
/// The userspace `ServiceMap` works on all platforms and provides
/// the data model for `relish resolve`. On Linux with the `ebpf`
/// feature, the map is additionally synced to BPF hash maps in the
/// kernel for zero-latency, zero-copy service discovery.
pub mod service_map;
pub mod types;
pub mod vip;

#[cfg(target_os = "linux")]
pub mod ebpf;
