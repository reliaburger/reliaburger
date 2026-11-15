//! Data structures for the Lettuce GitOps engine.
//!
//! All Raft-replicated types derive Serialize and Deserialize.
//! Timestamps are Unix milliseconds (u64).

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::meat::types::AppId;
use crate::sesame::types::WrappedKey;

// ---------------------------------------------------------------------------
// GitOps configuration
// ---------------------------------------------------------------------------

/// Top-level GitOps configuration, parsed from the `[gitops]` section
/// of the cluster config TOML.
///
/// The sync always walks every subdirectory under `path` (`git ls-tree
/// -r`); there is no shallow mode, and so no `recursive` key. An unknown
/// key, including that one, is a parse error.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
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

    /// The names of the settings that differ between `self` and `other`.
    ///
    /// Credentials are compared but never printed: a different token in
    /// the repository URL shows up as `repo`, a different webhook secret
    /// as `webhook_secret`.
    pub fn differing_fields(&self, other: &GitOpsConfig) -> Vec<&'static str> {
        [
            ("repo", self.repo != other.repo),
            ("branch", self.branch != other.branch),
            ("path", self.path != other.path),
            (
                "poll_interval_secs",
                self.poll_interval_secs != other.poll_interval_secs,
            ),
            (
                "require_signed_commits",
                self.require_signed_commits != other.require_signed_commits,
            ),
            (
                "trusted_signing_keys",
                self.trusted_signing_keys != other.trusted_signing_keys,
            ),
            (
                "webhook_secret",
                self.webhook_secret != other.webhook_secret,
            ),
            (
                "webhook_rate_limit",
                self.webhook_rate_limit != other.webhook_rate_limit,
            ),
        ]
        .into_iter()
        .filter_map(|(name, differs)| differs.then_some(name))
        .collect()
    }
}

// ---------------------------------------------------------------------------
// GitOps configuration as Raft holds it
// ---------------------------------------------------------------------------

/// HKDF info string scoping the key that seals GitOps credentials.
const CREDENTIALS_WRAP_INFO: &str = "reliaburger-gitops-credentials-wrap-v1";

/// The cluster's `[gitops]` configuration, as Raft holds it (#693).
///
/// Every council member reads the same copy, so a leader failover keeps
/// syncing the same repository, branch and path. The credentials (any
/// `user:token@` in the repository URL, and the webhook secret) are taken
/// out of `config` and sealed with a key derived from the cluster master
/// key, so the Raft log and its snapshots never hold them in clear.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StoredGitOpsConfig {
    /// The configuration with every credential removed.
    pub config: GitOpsConfig,
    /// The removed credentials, sealed. `None` when there were none.
    pub sealed_credentials: Option<WrappedKey>,
}

/// What [`StoredGitOpsConfig`] seals: the secret parts of a config.
#[derive(Debug, Default, Serialize, Deserialize)]
struct GitOpsCredentials {
    /// The `user:token` part of a URL-style repository address.
    repo_userinfo: Option<String>,
    /// The webhook HMAC secret.
    webhook_secret: Option<String>,
}

impl StoredGitOpsConfig {
    /// Split `config` into its public part and its sealed credentials.
    ///
    /// `master_key` is the cluster master key. A config that holds a
    /// credential is refused without one: Raft never holds it in clear.
    pub fn seal(config: &GitOpsConfig, master_key: Option<&[u8]>) -> Result<Self, LettuceError> {
        let (repo, repo_userinfo) = split_url_userinfo(&config.repo);
        let credentials = GitOpsCredentials {
            repo_userinfo,
            webhook_secret: config.webhook_secret.clone(),
        };
        let public = GitOpsConfig {
            repo,
            webhook_secret: None,
            ..config.clone()
        };
        if credentials.repo_userinfo.is_none() && credentials.webhook_secret.is_none() {
            return Ok(Self {
                config: public,
                sealed_credentials: None,
            });
        }
        let Some(master_key) = master_key else {
            return Err(LettuceError::Credentials(
                "the repository URL or webhook_secret holds a credential, and sealing it \
                 needs the cluster master key ([security] master_key_path)"
                    .into(),
            ));
        };
        let plaintext = serde_json::to_vec(&credentials)
            .map_err(|e| LettuceError::Credentials(e.to_string()))?;
        let sealed = crate::sesame::crypto::wrap_key(master_key, &plaintext, CREDENTIALS_WRAP_INFO)
            .map_err(|e| LettuceError::Credentials(e.to_string()))?;
        Ok(Self {
            config: public,
            sealed_credentials: Some(sealed),
        })
    }

    /// The full configuration, credentials included.
    pub fn unseal(&self, master_key: Option<&[u8]>) -> Result<GitOpsConfig, LettuceError> {
        let Some(sealed) = &self.sealed_credentials else {
            return Ok(self.config.clone());
        };
        let Some(master_key) = master_key else {
            return Err(LettuceError::Credentials(
                "the cluster's GitOps credentials are sealed and this node has no master key"
                    .into(),
            ));
        };
        let plaintext = crate::sesame::crypto::unwrap_key(master_key, sealed)
            .map_err(|e| LettuceError::Credentials(e.to_string()))?;
        let credentials: GitOpsCredentials = serde_json::from_slice(&plaintext)
            .map_err(|e| LettuceError::Credentials(e.to_string()))?;
        let repo = match &credentials.repo_userinfo {
            Some(userinfo) => join_url_userinfo(&self.config.repo, userinfo),
            None => self.config.repo.clone(),
        };
        Ok(GitOpsConfig {
            repo,
            webhook_secret: credentials.webhook_secret,
            ..self.config.clone()
        })
    }
}

/// Split `scheme://userinfo@host/path` into `scheme://host/path` and the
/// userinfo. An scp-style address (`git@host:org/repo`) has no scheme and
/// carries a user name, not a credential, so it comes back unchanged.
///
/// Unlike `git::split_credentials`, which moves only a password out of
/// git's argv, this takes the whole userinfo: a bare `token@host` is a
/// credential too. It splits on the last `@`, so a password holding `@` or
/// `/` is never left in clear, and [`join_url_userinfo`] always rebuilds
/// the exact URL.
fn split_url_userinfo(url: &str) -> (String, Option<String>) {
    let Some((scheme, rest)) = url.split_once("://") else {
        return (url.to_string(), None);
    };
    match rest.rsplit_once('@') {
        Some((userinfo, host)) if !userinfo.is_empty() => {
            (format!("{scheme}://{host}"), Some(userinfo.to_string()))
        }
        _ => (url.to_string(), None),
    }
}

/// Put `userinfo` back into a URL [`split_url_userinfo`] took it from.
fn join_url_userinfo(url: &str, userinfo: &str) -> String {
    match url.split_once("://") {
        Some((scheme, rest)) => format!("{scheme}://{userinfo}@{rest}"),
        None => url.to_string(),
    }
}

// ---------------------------------------------------------------------------
// Ownership
// ---------------------------------------------------------------------------

/// The resources Lettuce wrote, and so the only ones it may prune (D9, #693).
///
/// Held in Raft and changed in the same entry as the write that creates,
/// adopts or prunes the resource, so the set never disagrees with the
/// desired state it describes. A manually applied resource isn't in it,
/// and GitOps leaves it alone.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GitOpsOwnership {
    /// Apps GitOps wrote, by identity.
    pub apps: BTreeSet<AppId>,
    /// Namespaces GitOps wrote, by name.
    pub namespaces: BTreeSet<String>,
    /// Permissions GitOps wrote, by name.
    pub permissions: BTreeSet<String>,
    /// Policies GitOps wrote, by name (#1117).
    pub policies: BTreeSet<String>,
}

impl GitOpsOwnership {
    /// The resources in `config` that GitOps owns, labelled for a message
    /// (`namespace 'production'`, `app 'web' in namespace 'default'`),
    /// namespaces first.
    pub fn managed_in(&self, config: &crate::config::Config) -> Vec<String> {
        let namespaces = config
            .namespace
            .keys()
            .filter(|name| self.namespaces.contains(*name))
            .map(|name| format!("namespace '{name}'"));
        let permissions = config
            .permission
            .keys()
            .filter(|name| self.permissions.contains(*name))
            .map(|name| format!("permission '{name}'"));
        let policies = config
            .policy
            .keys()
            .filter(|name| self.policies.contains(*name))
            .map(|name| format!("policy '{name}'"));
        let apps = config.app.iter().filter_map(|(name, spec)| {
            let namespace = spec.namespace.as_deref().unwrap_or("default");
            self.apps
                .contains(&AppId::new(name, namespace))
                .then(|| app_label(name, namespace))
        });
        namespaces
            .chain(permissions)
            .chain(policies)
            .chain(apps)
            .collect()
    }
}

/// How messages name an app.
fn app_label(name: &str, namespace: &str) -> String {
    format!("app '{name}' in namespace '{namespace}'")
}

/// The warning `relish apply` prints for a GitOps-managed resource: the
/// change goes ahead, and the next sync puts back what git declares.
pub fn managed_apply_warning(resource: &str) -> String {
    format!(
        "warning: {resource} is managed by GitOps; manual changes are reverted on the next sync"
    )
}

/// Why `relish delete` or `relish rollback` refuses a GitOps-managed app
/// without `--force`: the next sync would undo it.
pub fn managed_change_refusal(verb: &str, name: &str, namespace: &str) -> String {
    format!(
        "{} is managed by GitOps, so the next sync would undo this {verb}; change the \
         repository instead, or pass --force",
        app_label(name, namespace)
    )
}

// ---------------------------------------------------------------------------
// This node's view of the configuration
// ---------------------------------------------------------------------------

/// How this node's own `[gitops]` section compares with the cluster's (#693).
///
/// Raft holds the configuration that runs. A node's `node.toml` only seeds
/// it when the cluster has none, so a differing local section is reported
/// here and in `relish wtf`, and otherwise ignored.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum LocalConfigCheck {
    /// The sync loop hasn't looked yet.
    #[default]
    Unknown,
    /// The cluster has no GitOps configuration.
    NoClusterConfig,
    /// This node has no `[gitops]` section; it follows the cluster's.
    NoLocalConfig,
    /// This node's `[gitops]` section matches the cluster's.
    Matches,
    /// This node's `[gitops]` section differs, and is not used.
    Differs {
        /// The settings that differ, by name; never their values.
        fields: Vec<String>,
    },
    /// The cluster's sealed credentials won't open on this node.
    Unreadable {
        /// Why they won't open.
        reason: String,
    },
}

/// `GET /v1/gitops`: the cluster's GitOps configuration and sync status,
/// as seen from the node that answers.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct GitOpsStatus {
    /// The cluster's configuration, without credentials; `None` when
    /// GitOps is off.
    pub config: Option<GitOpsConfig>,
    /// Whether the configuration carries sealed credentials.
    pub has_credentials: bool,
    /// The last sync's state.
    pub sync: Option<SyncState>,
    /// The resources GitOps owns.
    pub owned: GitOpsOwnership,
    /// How the answering node's own `[gitops]` compares with the cluster's.
    pub local: LocalConfigCheck,
}

/// `POST /v1/gitops/config`: replace the cluster's GitOps configuration,
/// or turn GitOps off with `config: None`. Admin only, and audited.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GitOpsConfigRequest {
    /// The new configuration, credentials included; the leader seals them.
    pub config: Option<GitOpsConfig>,
}

/// Compare this node's `[gitops]` section with the cluster's.
///
/// `cluster` is `None` when Raft holds no configuration, and an error when
/// its credentials couldn't be unsealed.
pub fn check_local_config(
    local: Option<&GitOpsConfig>,
    cluster: Option<Result<&GitOpsConfig, &LettuceError>>,
) -> LocalConfigCheck {
    match (cluster, local) {
        (None, _) => LocalConfigCheck::NoClusterConfig,
        (Some(Err(error)), _) => LocalConfigCheck::Unreadable {
            reason: error.to_string(),
        },
        (Some(Ok(_)), None) => LocalConfigCheck::NoLocalConfig,
        (Some(Ok(cluster)), Some(local)) => {
            let fields = local.differing_fields(cluster);
            if fields.is_empty() {
                LocalConfigCheck::Matches
            } else {
                LocalConfigCheck::Differs {
                    fields: fields.into_iter().map(str::to_string).collect(),
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Plaintext secrets in git
// ---------------------------------------------------------------------------

/// Whether an env key names something secret (D8).
///
/// Matches, ignoring case, a key containing `PASSWORD`, `SECRET`, `TOKEN`
/// or `CREDENTIAL`, or ending in `_KEY`.
pub fn is_secret_looking_key(key: &str) -> bool {
    let key = key.to_ascii_uppercase();
    ["PASSWORD", "SECRET", "TOKEN", "CREDENTIAL"]
        .iter()
        .any(|word| key.contains(word))
        || key.ends_with("_KEY")
}

/// One warning per app or init container env value that looks secret but
/// isn't encrypted.
///
/// GitOps applies such a value anyway (D8): refusing would break existing
/// repositories with no way to migrate. The warning names the app and the
/// key, never the value.
pub fn plaintext_secret_warnings(config: &crate::config::Config) -> Vec<String> {
    let mut warnings = Vec::new();
    for (name, spec) in &config.app {
        let namespace = spec.namespace.as_deref().unwrap_or("default");
        let init_env = spec.init.iter().flat_map(|init| &init.env);
        for (key, value) in spec.env.iter().chain(init_env) {
            if is_secret_looking_key(key)
                && !value.is_encrypted()
                && !value.is_provider()
                && !value.as_str().is_empty()
            {
                warnings.push(format!(
                    "app {namespace}/{name}: env {key} looks like a secret but is plaintext \
                     in git; encrypt it with `relish secret encrypt` (ENC[AGE:...])"
                ));
            }
        }
    }
    warnings
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
    /// Accepted webhook generations survive coordinator replacement.
    #[serde(default)]
    pub requested_generation: u64,
    /// A successful run acknowledges only the generation it captured at start.
    #[serde(default)]
    pub completed_generation: u64,
    /// Bounded replay receipts, hashed to avoid retaining provider payloads.
    #[serde(default)]
    pub webhook_receipts: VecDeque<[u8; 32]>,
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
    ///
    /// The runner appends one entry per completed sync — every success and
    /// every commit-bearing failure — and drops the oldest once the ring is
    /// full (`runner::push_history`). Repo-access and panic failures never
    /// reach a commit, so they update the scalar `last_error` above without an
    /// entry here. Brioche's GitOps page renders this as the sync timeline.
    pub history: VecDeque<SyncHistoryEntry>,
    /// Node ID of the current GitOps coordinator.
    ///
    /// The coordinator **is** the Raft leader — the only node that can apply a
    /// sync, since only the leader writes desired state. The runner stamps the
    /// leader's name here on each recorded sync (`runner::record_coordinator`),
    /// so this reflects the node actually driving syncs, and a leadership
    /// handover shows up as a `CoordinatorElectionReason::Failover`. Failover
    /// therefore rides on Raft leader election rather than a separate protocol;
    /// the field is honest visibility, not an exclusion mechanism.
    pub coordinator_node_id: Option<String>,
    /// Warnings from the last sync that applied: plaintext secrets in git
    /// (D8). They don't stop a sync; `relish wtf` reports them.
    #[serde(default)]
    pub warnings: Vec<String>,
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

    #[error("gitops credentials: {0}")]
    Credentials(String),
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
            webhook_rate_limit = 5
        "#;
        let config: GitOpsConfig = toml::from_str(toml).unwrap();
        assert_eq!(config.branch, "production");
        assert_eq!(config.path, "/apps");
        assert_eq!(config.poll_interval_secs, 60);
        assert!(config.require_signed_commits);
        assert_eq!(config.trusted_signing_keys, vec!["SHA256:abc123"]);
        assert_eq!(config.webhook_secret.as_deref(), Some("mysecret"));
        assert_eq!(config.webhook_rate_limit, 5);
    }

    #[test]
    fn gitops_config_rejects_recursive_key() {
        // The sync always recurses; a `recursive` key is a leftover that
        // must fail loudly rather than suggest a shallow mode exists.
        for value in ["true", "false"] {
            let toml = format!("repo = \"git@github.com:myorg/infra.git\"\nrecursive = {value}");
            let error = toml::from_str::<GitOpsConfig>(&toml).unwrap_err();
            assert!(error.to_string().contains("recursive"), "{error}");
        }
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
        let state = SyncState {
            phase: SyncPhase::Applying,
            consecutive_failures: 3,
            last_error: Some("connection refused".to_string()),
            ..Default::default()
        };

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

    fn config_with_credentials() -> GitOpsConfig {
        toml::from_str(
            r#"
            repo = "https://deploy:s3cr3t-token@git.example.com/org/infra.git"
            webhook_secret = "hook-secret"
            "#,
        )
        .unwrap()
    }

    const MASTER_KEY: &[u8; 32] = b"test-wrapping-material-32bytes!!";

    #[test]
    fn sealed_config_keeps_no_credential_in_clear() {
        let config = config_with_credentials();
        let stored = StoredGitOpsConfig::seal(&config, Some(MASTER_KEY)).unwrap();
        assert_eq!(stored.config.repo, "https://git.example.com/org/infra.git");
        assert!(stored.config.webhook_secret.is_none());
        let json = serde_json::to_string(&stored).unwrap();
        assert!(!json.contains("s3cr3t-token"), "{json}");
        assert!(!json.contains("hook-secret"), "{json}");
        assert_eq!(stored.unseal(Some(MASTER_KEY)).unwrap(), config);
    }

    #[test]
    fn every_url_credential_is_sealed_and_restored_exactly() {
        for url in [
            "https://ghp_bare_token@github.com/org/repo",
            "https://user:p@ss/word@example.com/org/repo",
        ] {
            let config: GitOpsConfig = toml::from_str(&format!("repo = '{url}'")).unwrap();
            let stored = StoredGitOpsConfig::seal(&config, Some(MASTER_KEY)).unwrap();
            assert!(!stored.config.repo.contains('@'), "{}", stored.config.repo);
            assert_eq!(stored.unseal(Some(MASTER_KEY)).unwrap().repo, url);
        }
    }

    #[test]
    fn a_credential_without_a_master_key_is_refused() {
        let error = StoredGitOpsConfig::seal(&config_with_credentials(), None).unwrap_err();
        assert!(error.to_string().contains("master key"), "{error}");
    }

    #[test]
    fn a_config_without_credentials_needs_no_master_key() {
        let config: GitOpsConfig =
            toml::from_str(r#"repo = "git@github.com:org/infra.git""#).unwrap();
        let stored = StoredGitOpsConfig::seal(&config, None).unwrap();
        assert!(stored.sealed_credentials.is_none());
        assert_eq!(stored.unseal(None).unwrap(), config);
    }

    #[test]
    fn sealed_credentials_need_the_master_key_to_open() {
        let stored =
            StoredGitOpsConfig::seal(&config_with_credentials(), Some(MASTER_KEY)).unwrap();
        assert!(stored.unseal(None).is_err());
        assert!(
            stored
                .unseal(Some(b"another-master-key-of-32-bytes!!"))
                .is_err()
        );
    }

    /// #1498: an init container's own env is in Git too, so a plaintext
    /// secret there gets the same warning as one in the app's.
    #[test]
    fn a_plaintext_secret_in_an_init_containers_env_warns() {
        let config = crate::config::Config::parse(
            r#"
            [app.web]
            image = "web:1"
            [[app.web.init]]
            command = ["migrate"]
            env = { DB_PASSWORD = "hunter2", MODE = "migrate", TOKEN = "PROVIDER[vault:web/token]" }
            "#,
        )
        .unwrap();
        let warnings = plaintext_secret_warnings(&config);
        assert_eq!(warnings.len(), 1, "{warnings:?}");
        assert!(warnings[0].contains("env DB_PASSWORD"), "{warnings:?}");
        assert!(!warnings[0].contains("hunter2"), "{warnings:?}");
    }

    #[test]
    fn secret_looking_keys_match_the_documented_list() {
        for key in [
            "DB_PASSWORD",
            "password",
            "CLIENT_SECRET",
            "GitHub_Token",
            "STRIPE_API_KEY",
            "AWS_CREDENTIALS",
        ] {
            assert!(is_secret_looking_key(key), "{key}");
        }
        for key in ["PORT", "KEYBOARD", "LOG_LEVEL", "KEY_ID"] {
            assert!(!is_secret_looking_key(key), "{key}");
        }
    }

    #[test]
    fn local_config_check_names_differing_fields_without_values() {
        let cluster = config_with_credentials();
        let mut local = cluster.clone();
        local.branch = "staging".into();
        local.webhook_secret = Some("other".into());
        let check = check_local_config(Some(&local), Some(Ok(&cluster)));
        assert_eq!(
            check,
            LocalConfigCheck::Differs {
                fields: vec!["branch".into(), "webhook_secret".into()]
            }
        );
        assert_eq!(
            check_local_config(Some(&cluster), Some(Ok(&cluster))),
            LocalConfigCheck::Matches
        );
        assert_eq!(
            check_local_config(None, Some(Ok(&cluster))),
            LocalConfigCheck::NoLocalConfig
        );
        assert_eq!(
            check_local_config(Some(&local), None),
            LocalConfigCheck::NoClusterConfig
        );
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
