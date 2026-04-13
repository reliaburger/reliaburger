//! Data structures for the Lettuce GitOps engine.
//!
//! All Raft-replicated types derive Serialize and Deserialize.
//! Timestamps are Unix milliseconds (u64).

use std::collections::{HashMap, VecDeque};
use std::time::Duration;

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// GitOps configuration
// ---------------------------------------------------------------------------

/// Top-level GitOps configuration, parsed from the `[gitops]` section
/// of the cluster config TOML.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct GitOpsConfig {
    /// SSH or HTTPS URL of the git repository.
    pub repo: String,
    /// Branch to track (default: "main").
    #[serde(default = "default_branch")]
    pub branch: String,
    /// Path within the repository to watch (default: "/").
    #[serde(default = "default_path")]
    pub path: String,
    /// How often to poll the remote in seconds (default: 30).
    #[serde(default = "default_poll_interval")]
    pub poll_interval_secs: u64,
    /// Require all commits to be signed.
    #[serde(default)]
    pub require_signed_commits: bool,
    /// Trusted signing key fingerprints.
    #[serde(default)]
    pub trusted_signing_keys: Vec<String>,
    /// HMAC-SHA256 secret for webhook validation.
    #[serde(default)]
    pub webhook_secret: Option<String>,
    /// Whether to recurse into subdirectories.
    #[serde(default)]
    pub recursive: bool,
    /// Maximum webhook triggers per minute (default: 10).
    #[serde(default = "default_webhook_rate_limit")]
    pub webhook_rate_limit: u32,
}

fn default_branch() -> String {
    "main".to_string()
}
fn default_path() -> String {
    "/".to_string()
}
fn default_poll_interval() -> u64 {
    30
}
fn default_webhook_rate_limit() -> u32 {
    10
}

impl GitOpsConfig {
    /// Get the poll interval as a Duration.
    pub fn poll_interval(&self) -> Duration {
        Duration::from_secs(self.poll_interval_secs)
    }
}

// ---------------------------------------------------------------------------
// Sync state (Raft-replicated)
// ---------------------------------------------------------------------------

/// Current state of the GitOps sync loop.
///
/// Replicated via Raft so any council member (and Brioche) can
/// display sync status.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct SyncState {
    /// The commit that was last successfully applied.
    pub last_applied_commit: Option<CommitInfo>,
    /// The commit that was last fetched (may differ if verify/parse failed).
    pub last_fetched_commit: Option<CommitInfo>,
    /// Current sync phase.
    pub phase: SyncPhase,
    /// Timestamp of the last successful sync (Unix ms).
    pub last_sync_at: Option<u64>,
    /// Timestamp of the last sync attempt (Unix ms).
    pub last_attempt_at: Option<u64>,
    /// Duration of the last sync cycle in milliseconds.
    pub last_sync_duration_ms: u64,
    /// Number of consecutive sync failures. Reset to 0 on success.
    pub consecutive_failures: u32,
    /// Error message from the last failed sync.
    pub last_error: Option<String>,
    /// Per-file parse errors from the last sync attempt.
    pub file_errors: HashMap<String, String>,
    /// Summary of the last applied diff.
    pub last_diff_summary: Option<DiffSummary>,
    /// History of recent syncs (ring buffer, max 100).
    pub history: VecDeque<SyncHistoryEntry>,
    /// Node ID of the current GitOps coordinator.
    pub coordinator_node_id: Option<String>,
}

/// Phase of the sync loop.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum SyncPhase {
    /// Idle, waiting for next poll or webhook.
    #[default]
    Idle,
    /// Fetching from git remote.
    Fetching,
    /// Verifying commit signatures.
    Verifying,
    /// Parsing TOML files.
    Parsing,
    /// Computing diff against current state.
    Diffing,
    /// Applying changes to Raft.
    Applying,
    /// Sync failed, will retry on next trigger.
    Error,
}

// ---------------------------------------------------------------------------
// Commit information
// ---------------------------------------------------------------------------

/// Information about a git commit.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CommitInfo {
    /// Full SHA-1 hash.
    pub sha: String,
    /// Commit message (first line).
    pub message: String,
    /// Author name.
    pub author: String,
    /// Commit timestamp (Unix ms).
    pub timestamp: u64,
    /// Signature verification status.
    pub signature: SignatureStatus,
}

/// Result of commit signature verification.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SignatureStatus {
    /// Signature verified against a trusted key.
    Verified,
    /// Signed, but the key is not in the trusted set.
    UntrustedKey,
    /// Signature is present but invalid.
    InvalidSignature,
    /// No signature present.
    Unsigned,
    /// Verification was not attempted.
    NotChecked,
}

// ---------------------------------------------------------------------------
// Sync results and history
// ---------------------------------------------------------------------------

/// A single entry in sync history.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SyncHistoryEntry {
    /// The commit that was synced.
    pub commit: CommitInfo,
    /// When this sync occurred (Unix ms).
    pub timestamp: u64,
    /// How long the sync took (ms).
    pub duration_ms: u64,
    /// The outcome.
    pub result: SyncResult,
    /// Summary of changes, if any.
    pub diff_summary: Option<DiffSummary>,
}

/// Outcome of a sync attempt.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum SyncResult {
    /// All changes applied successfully.
    Success,
    /// Some files had errors, but others were applied.
    PartialSuccess { errors: Vec<String> },
    /// Sync failed entirely.
    Failure { error: String },
    /// No changes needed (HEAD unchanged).
    Skipped { reason: String },
}

/// Summary of a diff (what changed).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffSummary {
    /// Number of resources added.
    pub added: usize,
    /// Number of resources modified.
    pub modified: usize,
    /// Number of resources removed.
    pub removed: usize,
}

// ---------------------------------------------------------------------------
// Coordinator election
// ---------------------------------------------------------------------------

/// A coordinator election entry, written to Raft.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CoordinatorElection {
    /// Node ID of the elected coordinator.
    pub node_id: String,
    /// Reason for the election (initial, failover, rebalance).
    pub reason: CoordinatorElectionReason,
    /// Timestamp of the election (Unix ms).
    pub timestamp: u64,
}

/// Why a coordinator election happened.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum CoordinatorElectionReason {
    /// First election when GitOps is enabled.
    Initial,
    /// Previous coordinator failed.
    Failover,
    /// Rebalance after council membership change.
    Rebalance,
}

// ---------------------------------------------------------------------------
// Webhook types
// ---------------------------------------------------------------------------

/// Validated webhook payload.
#[derive(Debug, Clone)]
pub struct WebhookEvent {
    /// The branch that was pushed.
    pub branch: String,
    /// The commit SHA (head of push).
    pub commit_sha: String,
    /// Delivery ID (for replay detection).
    pub delivery_id: Option<String>,
}

// ---------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------

/// Errors from Lettuce operations.
#[derive(Debug, thiserror::Error)]
pub enum LettuceError {
    #[error("git operation failed: {0}")]
    GitFailed(String),

    #[error("commit signature verification failed: {0}")]
    SignatureVerificationFailed(String),

    #[error("TOML parse error in {file}: {error}")]
    ParseError { file: String, error: String },

    #[error("diff computation failed: {0}")]
    DiffFailed(String),

    #[error("raft write failed: {0}")]
    RaftWriteFailed(String),

    #[error("webhook validation failed: {0}")]
    WebhookInvalid(String),

    #[error("not the coordinator")]
    NotCoordinator,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gitops_config_defaults() {
        let toml = r#"repo = "git@github.com:myorg/infra.git""#;
        let config: GitOpsConfig = toml::from_str(toml).unwrap();
        assert_eq!(config.branch, "main");
        assert_eq!(config.path, "/");
        assert_eq!(config.poll_interval_secs, 30);
        assert!(!config.require_signed_commits);
        assert!(config.webhook_secret.is_none());
        assert!(!config.recursive);
        assert_eq!(config.webhook_rate_limit, 10);
    }

    #[test]
    fn gitops_config_custom() {
        let toml = r#"
            repo = "https://github.com/myorg/infra.git"
            branch = "production"
            path = "/apps"
            poll_interval_secs = 60
            require_signed_commits = true
            trusted_signing_keys = ["SHA256:abc123"]
            webhook_secret = "mysecret"
            recursive = true
            webhook_rate_limit = 5
        "#;
        let config: GitOpsConfig = toml::from_str(toml).unwrap();
        assert_eq!(config.branch, "production");
        assert_eq!(config.path, "/apps");
        assert_eq!(config.poll_interval_secs, 60);
        assert!(config.require_signed_commits);
        assert_eq!(config.trusted_signing_keys, vec!["SHA256:abc123"]);
        assert_eq!(config.webhook_secret.as_deref(), Some("mysecret"));
        assert!(config.recursive);
        assert_eq!(config.webhook_rate_limit, 5);
    }

    #[test]
    fn sync_state_default() {
        let state = SyncState::default();
        assert_eq!(state.phase, SyncPhase::Idle);
        assert!(state.last_applied_commit.is_none());
        assert_eq!(state.consecutive_failures, 0);
        assert!(state.history.is_empty());
    }

    #[test]
    fn sync_state_serde_round_trip() {
        let mut state = SyncState::default();
        state.phase = SyncPhase::Applying;
        state.consecutive_failures = 3;
        state.last_error = Some("connection refused".to_string());

        let json = serde_json::to_string(&state).unwrap();
        let decoded: SyncState = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.phase, SyncPhase::Applying);
        assert_eq!(decoded.consecutive_failures, 3);
    }

    #[test]
    fn commit_info_serde_round_trip() {
        let commit = CommitInfo {
            sha: "abc123".to_string(),
            message: "fix: update redis".to_string(),
            author: "dev".to_string(),
            timestamp: 1700000000000,
            signature: SignatureStatus::Verified,
        };
        let json = serde_json::to_string(&commit).unwrap();
        let decoded: CommitInfo = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.sha, "abc123");
        assert_eq!(decoded.signature, SignatureStatus::Verified);
    }

    #[test]
    fn coordinator_election_serde() {
        let election = CoordinatorElection {
            node_id: "node-02".to_string(),
            reason: CoordinatorElectionReason::Failover,
            timestamp: 1700000000000,
        };
        let json = serde_json::to_string(&election).unwrap();
        let decoded: CoordinatorElection = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded.node_id, "node-02");
        assert_eq!(decoded.reason, CoordinatorElectionReason::Failover);
    }
}
