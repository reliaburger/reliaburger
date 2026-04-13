/// Lettuce: GitOps sync engine.
///
/// Watches a configured git repository and continuously reconciles
/// the desired state declared in that repository against the current
/// cluster state in Raft. Changes are applied selectively — only
/// modified resources are written.
///
/// Lettuce runs on a single council member elected as the GitOps
/// coordinator. If the coordinator fails, another council member
/// assumes the role within seconds, inheriting the last sync state
/// from Raft.
pub mod coordinator;
pub mod diff;
pub mod git;
pub mod sync;
pub mod types;
pub mod verify;
pub mod webhook;
