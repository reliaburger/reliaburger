/// Smoker — built-in fault injection engine.
///
/// Provides chaos engineering capabilities using eBPF for network faults,
/// cgroups for resource faults, and Unix signals for process faults.
/// Zero overhead when inactive — no sidecars, no iptables, no extra
/// processes.
///
/// # Architecture
///
/// Smoker is a library consumed by both the agent (Bun) and the CLI
/// (Relish). It does not run as a separate process.
///
/// - **types** — Core data structures shared between agent and CLI.
/// - **safety** — Safety rail evaluation (quorum, replica, leader, node%).
/// - **registry** — In-memory fault store with expiry tracking.
/// - **process** — SIGKILL/SIGSTOP/SIGCONT signal injection.
/// - **resource** — CPU stress, memory pressure, disk I/O throttle (Linux).
/// - **node** — Node drain and node kill simulation.
pub mod node;
pub mod process;
pub mod registry;
pub mod resource;
pub mod safety;
pub mod types;
