//! Self-upgrade: rolling binary replacement (Phase 14).
//!
//! Reliaburger is a single binary, so upgrading a node means replacing that
//! binary and restarting the Bun process in place. This module provides the
//! pieces: version handling, dual-signature verification, the on-disk binary
//! store with atomic symlink activation, the node-level upgrade state
//! machine with automatic rollback, and the leader-side rolling
//! orchestration across the cluster.
//!
//! Detailed design: `docs/self-upgrade-plan-2026-07.md` and
//! `docs/design/agent-bun.md` §5.5.

pub mod error;
pub mod keys;
pub mod manager;
pub mod marker;
pub mod metadata;
pub mod orchestrator;
pub mod signing;
pub mod store;
pub mod types;
pub mod version;

pub use error::UpgradeError;
pub use version::{BinaryVersion, resolve_running_version};

/// Pickle repository name for distributing reliaburger binaries
/// (single path segment — the registry routes require it).
pub const BINARY_BLOB_REPO: &str = "reliaburger-bun";
