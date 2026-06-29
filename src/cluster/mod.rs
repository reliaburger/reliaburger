//! Cluster runtime wiring for the `bun` binary.
//!
//! The gossip, Raft council, and reporting-tree subsystems are implemented
//! in `mustard`, `council`, and `reporting` and tested with in-memory
//! harnesses. This module is the glue that starts them with *real* network
//! transports and assembles a [`crate::bun::agent::ClusterHandle`], so the
//! shipping binary actually forms a multi-node cluster rather than running
//! single-node.
//!
//! It is built incrementally, one subsystem at a time. Right now it wires
//! the gossip layer: a node joins by address, gossip membership converges,
//! and the agent sees the cluster via `membership_rx`. Raft council and the
//! reporting tree are layered on in follow-up changes.

pub mod identity;
pub mod runtime;
