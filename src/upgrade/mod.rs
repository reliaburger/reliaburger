//! Self-upgrade: rolling binary replacement (Phase 14).
//!
//! Reliaburger is a single binary, so upgrading a node means replacing that
//! binary and restarting the Bun process in place. This module provides the
//! pieces: version handling, dual-signature verification, the on-disk binary
//! store with atomic symlink activation, the node-level upgrade state
//! machine with automatic rollback, and the leader-side rolling
//! orchestration across the cluster.
//!
//! Detailed design: `docs/plans/2026-07-06-plan-self-upgrade.md` and
//! `docs/design/agent-bun.md` §5.5.

mod compatibility;
pub mod error;
pub mod keys;
pub mod manager;
pub mod marker;
pub mod metadata;
pub mod orchestrator;
pub mod plan;
pub mod rejoin;
pub mod signing;
pub mod store;
pub mod types;
pub mod version;

pub use error::UpgradeError;
pub use version::{BinaryVersion, resolve_running_version};

/// Pickle repository name for distributing reliaburger binaries
/// (single path segment — the registry routes require it).
pub const BINARY_BLOB_REPO: &str = "reliaburger-bun";

/// Does this HTTP status mean "not right now" rather than "no"?
///
/// Server errors, request timeouts and rate limits say nothing about the
/// request itself, so repeating it later may well succeed. Every other
/// non-success status is an answer about the request and won't change.
pub(crate) fn is_transient_status(status: reqwest::StatusCode) -> bool {
    status.is_server_error()
        || status == reqwest::StatusCode::REQUEST_TIMEOUT
        || status == reqwest::StatusCode::TOO_MANY_REQUESTS
}
