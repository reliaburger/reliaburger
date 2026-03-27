/// eBPF program loading and BPF map management (Linux only).
///
/// Loads the compiled eBPF programs (`onion_connect.bpf.o`,
/// `onion_dns.bpf.o`) into the kernel via aya, attaches them to
/// the root cgroup v2, and provides typed wrappers for reading
/// and writing the BPF hash maps.
pub mod loader;
pub mod maps;
