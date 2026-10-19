/// eBPF program loading and BPF map management (Linux only).
///
/// Loads the compiled eBPF connect program (`onion_connect.bpf.o`, or its
/// pinned `onion_connect_owned.bpf.o` build) into the kernel via aya,
/// attaches it to the root cgroup v2, and provides typed wrappers for
/// reading and writing the BPF hash maps.
pub mod loader;
pub mod maps;
