/// Lettuce: GitOps sync engine.
///
/// Watches a configured git repository and continuously reconciles
/// the desired state declared in that repository against the current
/// cluster state in Raft. Changes are applied selectively — only
/// modified resources are written.
///
/// Lettuce runs on the Raft leader, which is the GitOps coordinator
/// (see `sync::coordinator_for_leader`). If the leader fails, the next
/// leader's sync loop takes over, inheriting the last sync state from
/// Raft.
pub mod diff;
pub mod git;
pub mod runner;
pub mod subprocess;
pub mod sync;
pub mod types;
pub mod verify;
pub mod webhook;
