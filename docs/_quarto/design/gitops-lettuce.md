# Lettuce: GitOps Sync Engine

## 1. Overview

Lettuce is Reliaburger's built-in GitOps sync engine, compiled directly into the Bun binary. It replaces external tools like ArgoCD and Flux with a first-class subsystem that understands the native TOML configuration format, the Raft-based desired state model, and the autoscaler's runtime overrides.

Lettuce watches a configured git repository and continuously reconciles the desired state declared in that repository against the actual state of the cluster. Reconciliation is triggered either by a configurable poll interval (default 30 seconds) or by an incoming webhook from a git hosting provider. When differences are detected, Lettuce computes a minimal diff and applies only the changed resources, forwarding write operations to the Raft leader.

The engine runs on a single council member elected as the **GitOps coordinator**. This isn't necessarily the Raft leader -- the coordinator role is a separate election that distributes work across the council. If the coordinator fails, another council member assumes the role within seconds, inheriting the last-known sync state from Raft.

Lettuce isn't a separate daemon, sidecar, or CRD controller. It's a module within Bun, sharing the same process, the same mTLS identity, and the same Raft client as every other subsystem. There's no additional binary to deploy, upgrade, or monitor.

### Design Principles

- **Single source of truth.** When GitOps is enabled, the git repository is authoritative for application-level desired state (app specs, jobs, secrets, namespaces, config files). Operational state (scheduling assignments, PKI keys, API tokens, image manifests, runtime configuration) remains in Raft.
- **Autoscaler-aware.** The `replicas` field is treated independently. Lettuce never resets autoscaler overrides unless the `replicas` value itself changes in git.
- **Script-aware security.** Any commit that adds or modifies an inline `script` field automatically requires a signed commit, regardless of the global `require_signed_commits` setting.
- **Minimal blast radius.** Only changed resources are applied. Unchanged apps, jobs, and namespaces aren't touched.
- **Coexistence with manual operations.** GitOps can manage production while developers use `relish apply` for staging. The two modes coexist without conflict.

---

## 2. Dependencies

### Internal Subsystems

| Subsystem | Role in Lettuce | Notes |
|-----------|----------------|-------|
| **Bun** | Host process. Lettuce runs as a module inside the Bun binary on the elected GitOps coordinator (a council member). | Shares the Bun process lifecycle, mTLS identity, and signal handling. |
| **Raft (Council)** | Stores sync state, coordinator election, and desired state replication. Lettuce reads current desired state from Raft to compute diffs and forwards applies to the leader. | Coordinator election is a Raft-replicated state machine entry, not a separate protocol. |
| **Sesame** | Provides the mTLS certificate used for git-over-HTTPS client authentication (if configured) and verifies GPG/SSH commit signatures against the cluster's trusted key set. | Lettuce calls into Sesame's signature verification API rather than implementing its own PGP/SSH parsing. |
| **Patty** | Scheduler. After Lettuce applies a changed app spec to Raft, Patty handles scheduling decisions (replica placement, rolling deploys). | Lettuce doesn't schedule directly; it writes desired state and Patty reacts. |
| **Brioche** | Web UI. Displays sync status, last applied commit, pending change preview, and sync history. | Brioche reads Lettuce state from the Raft-replicated `SyncState` struct. |
| **Mustard** | Gossip protocol. Used to detect coordinator node health. If the coordinator's gossip heartbeat stops, the council elects a replacement. | Lettuce doesn't use Mustard directly; the council handles coordinator failover. |

### External

| External | Purpose |
|----------|---------|
| **Git remote** | The repository containing TOML configuration files. Accessed via SSH or HTTPS. |
| **Webhook source** (optional) | GitHub, GitLab, Gitea, or any provider that sends POST webhooks on push events. |

---

## 3. Architecture

### 3.1 Sync Loop

The core of Lettuce is a continuous sync loop running on the coordinator:

```
                     ┌──────────────────────────────────────────────┐
                     │           Lettuce Sync Loop                  │
                     │         (on coordinator node)                │
                     │                                              │
   ┌──────────┐     │  ┌───────┐    ┌──────┐    ┌──────┐         │
   │ Poll     │────▶│  │ Git   │───▶│ TOML │───▶│ Diff │         │
   │ Timer    │     │  │ Pull  │    │Parse │    │Engine│         │
   └──────────┘     │  └───────┘    └──────┘    └──┬───┘         │
                     │       ▲                      │              │
   ┌──────────┐     │       │                      ▼              │
   │ Webhook  │────▶│  ┌────┴──┐              ┌────────┐         │
   │ Receiver │     │  │Verify │              │Selective│         │
   └──────────┘     │  │Commit │              │ Apply  │──────┐  │
                     │  │ Sig.  │              └────────┘      │  │
                     │  └───────┘                              │  │
                     │                                         ▼  │
                     │                              ┌──────────┐  │
                     │                              │  Raft    │  │
                     │                              │  Leader  │  │
                     │                              │ (write)  │  │
                     │                              └──────────┘  │
                     └──────────────────────────────────────────────┘
```

**Step-by-step flow:**

1. **Trigger.** Either the poll timer fires (every `poll_interval`, default 30s) or a validated webhook arrives.
2. **Git fetch.** Lettuce performs `git fetch origin <branch>` on the local bare clone. If the remote HEAD hasn't changed since the last sync, the loop short-circuits.
3. **Commit signature verification.** If `require_signed_commits` is enabled globally, or if the incoming commit modifies any `script` field (auto-enforcement per Section 17), the commit signature is verified against `trusted_signing_keys`. Unsigned or untrusted commits are rejected and an alert fires via the event system.
4. **TOML parse.** All `.toml` files under the configured `path` are parsed into the internal `DesiredState` representation. Parse errors are non-fatal per file -- a single malformed file doesn't block sync of other files, but the malformed file is flagged as an error in sync status.
5. **Diff computation.** The parsed desired state is compared field-by-field against the current desired state stored in Raft. The `replicas` field is compared independently (see Section 5.6).
6. **Selective apply.** Only changed resources are written to Raft via the leader. Unchanged resources are skipped entirely.
7. **State update.** The `SyncState` struct in Raft is updated with the new commit hash, sync timestamp, applied changes, and any errors.

### 3.2 Coordinator Election

The GitOps coordinator is a council member role, elected via a Raft-replicated state machine entry. The election process:

1. When GitOps is first enabled (via `relish apply` of a config containing `[gitops]`, or via the initial cluster config), the leader writes a `CoordinatorElection` entry to the Raft log.
2. The leader selects the coordinator from council members using the same scoring heuristic as council selection (stability, resource availability, zone diversity), with a preference for non-leader members to distribute load.
3. The elected coordinator receives the `CoordinatorElection` entry via Raft replication and starts the Lettuce sync loop.
4. All other council members see the election entry and know not to start their own sync loops.

**Failover:**

- The council monitors the coordinator via Raft heartbeats (not gossip -- the coordinator is always a council member, so Raft heartbeats are authoritative).
- If the coordinator misses heartbeats (default: 3 consecutive, approximately 450ms with 150ms Raft heartbeat interval, not the 500ms Mustard gossip interval), the leader writes a new `CoordinatorElection` entry selecting a replacement.
- The replacement coordinator reads the last `SyncState` from Raft (which includes the last applied commit hash and the local bare clone path) and resumes the sync loop from where the previous coordinator left off.
- The replacement coordinator must `git clone` the repository fresh if it doesn't already have a local clone. During this window (typically a few seconds), sync is paused but not lost -- the next poll after clone completion catches up.

### 3.3 TOML Parsing Pipeline

Lettuce parses TOML files in a defined order to handle cross-references:

1. **Discovery.** Walk all `.toml` files under `path` (non-recursive by default; `recursive = true` in config enables subdirectory scanning).
2. **Namespace resolution.** Files can declare `[namespace]` blocks. If a file doesn't declare a namespace, it inherits the namespace from its parent directory name, or falls back to `default`.
3. **Parse and validate.** Each file is parsed with the `toml` crate. Validation checks:
   - Required fields present (`image` or `exec`/`script` for apps/jobs)
   - Resource values parseable (`cpu`, `memory` strings)
   - Port numbers in valid range
   - Label keys and values conform to naming rules
   - No duplicate app/job names within a namespace
4. **Script field detection.** If any `script` field is present in the parsed output, the file is flagged for mandatory signed-commit verification, regardless of the global setting.
5. **Assembly.** All parsed files are assembled into a single `DesiredState` struct representing the full desired state of the cluster as declared in git.

### 3.4 Autoscaler Interaction

The autoscaler writes runtime replica overrides to Raft. Lettuce must not fight these overrides:

- When computing the diff, Lettuce compares the `replicas` field from git against the `replicas` field in the **desired state in Raft** (which is the git-sourced base), not against the runtime replica count (which may have been adjusted by the autoscaler).
- If the `replicas` value in git has changed (e.g., from 3 to 5), Lettuce writes the new value to Raft, which resets the autoscaler's baseline. The autoscaler's min/max constraints from `[app.*.autoscale]` are also updated from git.
- If the `replicas` value in git has **not** changed, Lettuce doesn't touch it, even if other fields in the same app spec have changed. An image tag update triggers a rolling deploy but doesn't reset the autoscaler's current replica count.
- `relish diff` shows autoscaler overrides as "expected runtime drift" with the annotation `(autoscaler adjusted)`, not as configuration drift requiring remediation.

---

## 4. Data Structures

All structs are Rust. Raft-replicated structs derive `Serialize` and `Deserialize` (via `serde`). All timestamps are `u64` Unix milliseconds.

### 4.1 GitOpsConfig

Stored in Raft as part of the cluster configuration. Written when GitOps is enabled or reconfigured.

```rust
/// Top-level GitOps configuration, parsed from the [gitops] section
/// of the cluster config TOML.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct GitOpsConfig {
    /// SSH or HTTPS URL of the git repository.
    /// Examples: "git@github.com:myorg/infra.git", "https://github.com/myorg/infra.git"
    pub repo: String,

    /// Branch to track. Default: "main".
    pub branch: String,

    /// Path within the repository to watch. Only TOML files under this path
    /// are considered. Default: "/" (repository root).
    pub path: String,

    /// How often to poll the remote for changes. Default: 30s.
    /// Set to 0 to disable polling (webhook-only mode).
    pub poll_interval: Duration,

    /// If true, all commits must be signed by a key in `trusted_signing_keys`.
    /// Even if false, commits that modify `script` fields are always verified.
    pub require_signed_commits: bool,

    /// GPG or SSH key fingerprints trusted for commit signing.
    /// Format: "SHA256:<base64>" for SSH keys, full GPG fingerprint for GPG keys.
    pub trusted_signing_keys: Vec<String>,

    /// HMAC-SHA256 secret for webhook payload validation. If None, the webhook
    /// endpoint is disabled.
    pub webhook_secret: Option<String>,

    /// Whether to recurse into subdirectories under `path`. Default: false.
    pub recursive: bool,

    /// Maximum webhook triggers processed per minute. Default: 10.
    pub webhook_rate_limit: u32,
}
```

### 4.2 SyncState

Raft-replicated. Updated after every sync attempt (success or failure). Read by Brioche for dashboard display.

```rust
/// Current state of the GitOps sync loop. Replicated via Raft so that
/// any council member (and Brioche) can display sync status.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncState {
    /// The commit hash that was last successfully applied.
    pub last_applied_commit: Option<CommitInfo>,

    /// The commit hash that was last fetched (may differ from applied if
    /// verification or parse failed).
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

    /// If the last sync failed, the error message.
    pub last_error: Option<String>,

    /// Per-file parse errors from the last sync attempt. Keys are file
    /// paths relative to the gitops `path`.
    pub file_errors: HashMap<String, String>,

    /// Summary of the last applied diff.
    pub last_diff_summary: Option<DiffSummary>,

    /// History of recent syncs (ring buffer, default 100 entries).
    pub history: VecDeque<SyncHistoryEntry>,

    /// The node ID of the current GitOps coordinator.
    pub coordinator_node_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum SyncPhase {
    /// Idle, waiting for next poll or webhook.
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncHistoryEntry {
    pub commit: CommitInfo,
    pub timestamp: u64,
    pub duration_ms: u64,
    pub result: SyncResult,
    pub diff_summary: Option<DiffSummary>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SyncResult {
    Success,
    PartialSuccess { errors: Vec<String> },
    Failure { error: String },
    Skipped { reason: String },
}
```

### 4.3 CommitInfo

```rust
/// Metadata about a git commit, extracted from libgit2.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CommitInfo {
    /// Full 40-character hex SHA of the commit.
    pub sha: String,

    /// Short commit message (first line).
    pub message: String,

    /// Author name.
    pub author_name: String,

    /// Author email.
    pub author_email: String,

    /// Commit timestamp (Unix seconds).
    pub timestamp: i64,

    /// Whether the commit signature was verified.
    pub signature_status: SignatureStatus,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum SignatureStatus {
    /// Commit was signed and the signature is valid against a trusted key.
    Verified { key_fingerprint: String },
    /// Commit was signed but the key is not in the trusted set.
    UntrustedKey { key_fingerprint: String },
    /// Commit was signed but the signature is invalid.
    InvalidSignature,
    /// Commit is unsigned.
    Unsigned,
    /// Signature verification was not performed (require_signed_commits is false
    /// and no script fields present).
    NotChecked,
}
```

### 4.4 DiffResult

```rust
/// The complete diff between git desired state and Raft desired state.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiffResult {
    /// Resources that exist in git but not in Raft (new deployments).
    pub added: Vec<ResourceChange>,

    /// Resources that exist in both but differ.
    pub modified: Vec<ResourceChange>,

    /// Resources that exist in Raft but not in git (deletions).
    pub removed: Vec<ResourceChange>,

    /// Resources unchanged.
    pub unchanged_count: usize,

    /// Files that failed to parse (and thus could not be diffed).
    pub parse_errors: Vec<FileParseError>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResourceChange {
    /// Fully qualified resource name: "namespace/kind/name"
    /// e.g. "production/app/web", "default/job/db-migrate"
    pub resource_id: String,

    /// The kind of resource (App, Job, Namespace, Secret, ConfigFile).
    pub kind: ResourceKind,

    /// For modifications, the specific fields that changed.
    pub field_changes: Vec<FieldChange>,

    /// Whether this change affects the replicas field specifically.
    /// Used to determine autoscaler interaction.
    pub replicas_changed: bool,

    /// Whether this change involves a `script` field (triggers mandatory
    /// signed-commit verification).
    pub contains_script: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FieldChange {
    /// Dot-separated field path: "env.DATABASE_URL", "image", "replicas"
    pub field_path: String,

    /// Previous value (serialised to string for display).
    pub old_value: Option<String>,

    /// New value.
    pub new_value: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileParseError {
    /// Path relative to the gitops `path`.
    pub file_path: String,

    /// Line number where the error occurred (if available).
    pub line: Option<usize>,

    /// Human-readable error message.
    pub message: String,
}

/// Compact summary for dashboard display and history.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiffSummary {
    pub added: usize,
    pub modified: usize,
    pub removed: usize,
    pub unchanged: usize,
    pub parse_errors: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum ResourceKind {
    App,
    Job,
    Namespace,
    Secret,
    ConfigFile,
}
```

### 4.5 WebhookPayload

```rust
/// Parsed and validated webhook payload. Lettuce supports GitHub, GitLab,
/// and Gitea webhook formats. The payload is validated for HMAC-SHA256
/// signature before parsing.
#[derive(Debug, Clone)]
pub struct WebhookPayload {
    /// The full commit SHA referenced by the push event.
    pub commit_sha: String,

    /// Branch that was pushed to.
    pub branch: String,

    /// The pusher's identity (from the webhook payload, not the commit).
    pub pusher: String,

    /// Raw delivery ID (for deduplication / replay detection).
    pub delivery_id: String,

    /// Timestamp when the webhook was received (Unix ms).
    pub received_at: u64,
}

/// Webhook validation result.
#[derive(Debug)]
pub enum WebhookValidation {
    Valid(WebhookPayload),
    InvalidSignature,
    WrongBranch,
    RateLimited { retry_after_ms: u64 },
    MalformedPayload { error: String },
    ReplayDetected { delivery_id: String },
}
```

### 4.6 SigningKeySet

```rust
/// The set of trusted signing keys, parsed from `trusted_signing_keys` in
/// the GitOps config. Supports both GPG and SSH key fingerprints.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SigningKeySet {
    /// SSH key fingerprints in "SHA256:<base64>" format.
    pub ssh_fingerprints: Vec<String>,

    /// GPG key fingerprints (40-character hex).
    pub gpg_fingerprints: Vec<String>,
}

impl SigningKeySet {
    /// Returns true if the given fingerprint (SSH or GPG) is in the trusted set.
    pub fn is_trusted(&self, fingerprint: &str) -> bool {
        self.ssh_fingerprints.contains(&fingerprint.to_string())
            || self.gpg_fingerprints.contains(&fingerprint.to_string())
    }

    /// Parse the raw fingerprint strings from config into categorised sets.
    pub fn from_config(raw: &[String]) -> Self {
        let mut ssh = Vec::new();
        let mut gpg = Vec::new();
        for fp in raw {
            if fp.starts_with("SHA256:") {
                ssh.push(fp.clone());
            } else {
                gpg.push(fp.clone());
            }
        }
        SigningKeySet {
            ssh_fingerprints: ssh,
            gpg_fingerprints: gpg,
        }
    }
}
```

### 4.7 CoordinatorElection

```rust
/// Raft log entry for GitOps coordinator election.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CoordinatorElection {
    /// Node ID of the elected coordinator.
    pub node_id: String,

    /// Term in which this election occurred (Raft term).
    pub raft_term: u64,

    /// Timestamp of election (Unix ms).
    pub elected_at: u64,

    /// Reason for election (initial, failover, rebalance).
    pub reason: CoordinatorElectionReason,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum CoordinatorElectionReason {
    /// GitOps was just enabled.
    Initial,
    /// Previous coordinator failed.
    Failover { previous_node_id: String },
    /// Manual rebalance or council membership change.
    Rebalance,
}
```

---

## 5. Operations

### 5.1 Poll-Based Sync

The default sync mode. A tokio timer fires every `poll_interval` (default 30 seconds, configurable from 5s to 1h).

**Procedure:**

1. Timer fires. Lettuce acquires the sync lock (a local mutex -- only one sync runs at a time on the coordinator).
2. `git fetch origin <branch>` via `git2::Remote::fetch()`. Credentials are loaded from the configured SSH key path or HTTPS credentials (see Section 8).
3. Compare `FETCH_HEAD` against `last_applied_commit.sha` in `SyncState`. If they match, release lock and return (no-op sync).
4. If they differ, proceed to commit verification, parse, diff, and apply (Steps 3-7 of Section 3.1).
5. Update `SyncState` in Raft with the result.
6. Release the sync lock.

**Back-off on failure:** If git fetch fails (network error, auth failure), Lettuce applies exponential back-off: 30s, 60s, 120s, 240s, capped at `poll_interval * 8`. The back-off resets on a successful fetch. During back-off, incoming webhooks still trigger immediate sync attempts.

**Jitter:** To avoid thundering-herd effects in multi-cluster setups where many clusters point at the same repo, Lettuce adds random jitter of +/- 10% to the poll interval.

### 5.2 Webhook-Triggered Sync

For instant deploys on push. The webhook endpoint is served on the cluster API port (the same port used by `relish` CLI and Brioche), behind TLS (required -- plaintext webhook endpoints are rejected at config validation time).

**Endpoint:** `POST /api/v1/gitops/webhook`

**Validation procedure:**

1. **Method check.** Only POST is accepted. Other methods return 405.
2. **HMAC-SHA256 signature validation.** The `X-Hub-Signature-256` header (GitHub format) is checked against the payload body using the configured `webhook_secret`. GitLab uses `X-Gitlab-Token` (compared directly). Lettuce auto-detects the provider from the headers. Invalid signatures return 401.
3. **Rate limiting.** A token bucket rate limiter allows `webhook_rate_limit` (default 10) triggers per minute. Excess triggers return 429 with a `Retry-After` header.
4. **Replay detection.** The `X-GitHub-Delivery` (or equivalent) header is stored in a bounded deduplication set (1000 entries, LRU eviction). Duplicate delivery IDs are rejected with 409.
5. **Branch filter.** The payload is parsed to extract the pushed branch. If it doesn't match the configured branch, the webhook is acknowledged (200) but no sync is triggered.
6. **Trigger sync.** A signal is sent to the sync loop to run immediately, bypassing the poll timer. The webhook returns 202 (accepted) before the sync completes.

**Response codes:**

| Code | Meaning |
|------|---------|
| 202 | Accepted, sync triggered |
| 200 | Acknowledged but no action (wrong branch) |
| 401 | Invalid HMAC signature |
| 405 | Method not allowed |
| 409 | Duplicate delivery (replay) |
| 429 | Rate limited |
| 503 | Coordinator unavailable (failover in progress) |

### 5.3 Commit Signature Verification

Two modes of enforcement:

1. **Global enforcement.** When `require_signed_commits = true`, every commit that Lettuce attempts to apply must be signed by a key in `trusted_signing_keys`. Unsigned commits or commits signed by unknown keys are rejected.
2. **Auto-enforcement for scripts.** Regardless of the global setting, any commit that adds or modifies a `script` field in any TOML file is subject to mandatory signed-commit verification. This is determined by diffing the incoming commit against the previous applied commit and checking whether any `script` fields changed. This closes the RCE-via-git-push attack vector.

**Verification procedure:**

1. Extract the commit's signature using `git2::Commit::header_field_bytes("gpgsig")`.
2. Determine signature type (GPG or SSH) from the signature armor header.
3. For GPG signatures: parse with `sequoia-openpgp`, extract the signing key fingerprint, and check against `SigningKeySet.gpg_fingerprints`.
4. For SSH signatures: parse the SSH signature format, extract the key fingerprint in `SHA256:<base64>` format, and check against `SigningKeySet.ssh_fingerprints`.
5. If the signature is valid and the key is trusted, set `SignatureStatus::Verified`.
6. If the signature is valid but the key isn't trusted, set `SignatureStatus::UntrustedKey` and reject.
7. If the signature is invalid, set `SignatureStatus::InvalidSignature` and reject.
8. If there's no signature and verification is required, set `SignatureStatus::Unsigned` and reject.

**On rejection:** An event is emitted to the Ketchup event log with severity `warning`, including the commit SHA, author, and reason for rejection. The `SyncState.last_error` field is updated. Brioche displays a banner on the GitOps dashboard. If configured, an alert fires via the alerting subsystem.

### 5.4 Diff Computation

The diff engine compares two `DesiredState` trees: one from the parsed git TOML, one from the current Raft desired state.

**Algorithm:**

1. Build two maps keyed by `resource_id` (format: `namespace/kind/name`).
2. Resources present in git but not in Raft are `added`.
3. Resources present in Raft but not in git are `removed`.
4. Resources present in both are compared field-by-field:
   - Primitive fields (strings, numbers, booleans): direct equality.
   - List fields (e.g., `args`, `trusted_signing_keys`): ordered comparison.
   - Map fields (e.g., `env`, `labels`): key-by-key comparison.
   - Nested structs (e.g., `[app.*.autoscale]`, `[app.*.ingress]`): recursive field comparison.
5. The `replicas` field is handled by the special autoscaler-aware logic (Section 5.6).
6. Fields that are equal are skipped. Fields that differ produce a `FieldChange` entry.

**Complexity:** The diff is O(n * m) where n is the number of resources and m is the average number of fields per resource. For a typical cluster (hundreds of resources, tens of fields each), this completes in sub-millisecond time.

### 5.5 Selective Apply

Only changed resources are written to Raft. The apply phase:

1. For each `added` resource: write the full resource spec to Raft via the leader.
2. For each `modified` resource: write the updated resource spec. Patty determines the deploy strategy (rolling, blue-green) based on what changed -- an image tag change triggers a rolling deploy, an env-only change may trigger a config reload if the app supports it.
3. For each `removed` resource: write a deletion marker. Patty drains and stops the affected workloads.
4. All writes are batched into a single Raft proposal to ensure atomicity. Either all changes from a single commit are applied, or none are.

**Partial failure handling:** If the Raft write fails (e.g., leader step-down during apply), the entire batch is retried on the next sync cycle. The `SyncState` isn't updated, so the next poll or webhook re-processes the same commit.

### 5.6 Autoscaler Replica Interaction

This is critical to prevent Lettuce from fighting the autoscaler during traffic spikes.

**Invariant:** A change to the `replicas` value in git is the only event that resets the autoscaler's runtime override. All other field changes are applied independently.

**Implementation:**

1. During diff computation, the `replicas` field is extracted and compared separately from all other fields.
2. If `replicas` in git equals `replicas` in the Raft desired state (the git-sourced base, not the runtime count), the `replicas` field is excluded from the `modified` field changes, even if other fields changed.
3. If `replicas` in git differs from `replicas` in the Raft desired state, the change is included and `replicas_changed` is set to `true` on the `ResourceChange`. Patty resets the autoscaler's baseline to the new value.
4. The autoscaler's `min` and `max` from `[app.*.autoscale]` are always synced from git (they are configuration, not runtime state).

**Example scenario:**

- Git declares `replicas = 3` for `app.web`. Autoscaler has scaled to 7 due to load.
- A developer pushes a commit changing `app.web.env.LOG_LEVEL` from "info" to "debug".
- Lettuce diffs: `replicas` is 3 in git, 3 in Raft desired state (unchanged). `env.LOG_LEVEL` changed.
- Lettuce applies only the `env.LOG_LEVEL` change. Replicas remain at 7 (autoscaler override intact).
- Later, a developer pushes a commit changing `replicas` from 3 to 5.
- Lettuce diffs: `replicas` changed (3 to 5). Applies the change. Autoscaler baseline resets to 5. Autoscaler may scale further from 5 based on load.

### 5.7 Coordinator Failover

See Section 3.2. Summary of the failover timeline:

| Time | Event |
|------|-------|
| T+0 | Coordinator node fails (Raft heartbeat stops). |
| T+500ms | First missed heartbeat detected by council. |
| T+1.5s | Third missed heartbeat. Council marks coordinator as failed. |
| T+1.5s | Leader writes new `CoordinatorElection` entry to Raft. |
| T+1.6s | New coordinator receives election entry, begins startup. |
| T+1.6s-5s | New coordinator clones git repo (if not already cached locally). |
| T+5s | Sync loop resumes. First poll fetches from git and reconciles. |

**Worst-case sync gap:** `poll_interval + failover_time` (approximately 35 seconds with default settings). In webhook mode, the gap is just the failover time (approximately 5 seconds), since the next push webhook triggers immediate sync.

### 5.8 Inline Script Enforcement

Per Section 17 of the whitepaper, Lettuce automatically enforces `require_signed_commits` for any configuration that contains a `script` field, regardless of the global setting.

**Detection logic:**

1. When computing the diff, Lettuce checks every `added` and `modified` resource for the presence of a `script` field.
2. If any `script` field is found (whether newly added or modified from a previous value), the commit must be signed.
3. This check applies even if `require_signed_commits = false` globally.
4. If the commit is unsigned and contains script changes, Lettuce rejects it with an error message: `"Commit <sha> modifies script fields but is not signed. Script changes always require signed commits (Section 17)."`
5. If `trusted_signing_keys` is empty but a script change is detected, the sync fails with: `"Cannot apply script changes: no trusted signing keys configured. Add keys to [gitops] trusted_signing_keys."`

---

## 6. Configuration

### 6.1 Full Configuration Reference

All configuration lives in the `[gitops]` section of the cluster configuration TOML (applied via `relish apply` or present in the git repo itself for bootstrapping).

```toml
[gitops]
# Required: URL of the git repository.
# Supports SSH (git@...) and HTTPS (https://...) protocols.
repo = "git@github.com:myorg/infra.git"

# Branch to track. Default: "main".
branch = "main"

# Path within the repo to scan for TOML files. Default: "/".
# Only files under this path are parsed. Supports trailing slash.
path = "production/"

# Poll interval. Default: "30s". Minimum: "5s". Maximum: "1h".
# Set to "0s" to disable polling entirely (webhook-only mode).
poll_interval = "30s"

# Require all commits to be signed. Default: false.
# Even when false, commits modifying `script` fields are always verified.
require_signed_commits = true

# Trusted signing key fingerprints. Required if require_signed_commits is true
# or if any workload uses script fields.
# SSH keys: "SHA256:<base64>"
# GPG keys: full 40-character hex fingerprint
trusted_signing_keys = [
    "SHA256:abc123def456...",
    "SHA256:789ghi012jkl...",
    "ABCD1234EFGH5678IJKL9012MNOP3456QRST7890",
]

# HMAC-SHA256 secret for webhook validation. If omitted, webhook endpoint is
# disabled and only polling is used.
webhook_secret = "whsec_super_secret_value"

# Recurse into subdirectories under `path`. Default: false.
recursive = true

# Maximum webhook triggers per minute. Default: 10.
webhook_rate_limit = 10
```

### 6.2 Git Credentials

Git credentials aren't stored in the `[gitops]` config block (which is Raft-replicated and visible to all council members). Instead, each council member node configures them via `node.toml`:

```toml
# In /etc/reliaburger/node.toml
[gitops.credentials]
# For SSH: path to the private key.
ssh_key_path = "/etc/reliaburger/git_ssh_key"

# For HTTPS: username and token/password.
# These values can be encrypted with age (same as secrets).
https_username = "git"
https_token = "ENC[AGE:Z2hwX3Rva2VuX3ZhbHVl...]"
```

The coordinator loads credentials from its local `node.toml`. On failover, the new coordinator loads credentials from its own `node.toml`. All council members must have valid git credentials configured if they may become the GitOps coordinator.

### 6.3 Validation

`relish lint` validates the `[gitops]` configuration:

- `repo` is a valid git URL (SSH or HTTPS).
- `branch` is non-empty.
- `poll_interval` parses as a duration and is within bounds.
- `webhook_secret` is at least 32 characters (if present).
- `trusted_signing_keys` is non-empty if `require_signed_commits` is true.
- `webhook_rate_limit` is between 1 and 1000.
- If any file under `path` contains a `script` field and `trusted_signing_keys` is empty, a warning is emitted.

---

## 7. Failure Modes

### 7.1 Git Unreachable

**Cause:** Network partition, DNS failure, git hosting provider outage, revoked credentials.

**Behaviour:** `git fetch` fails. Lettuce logs the error, increments `consecutive_failures`, applies exponential back-off, and retries on the next poll cycle. The `SyncState.phase` is set to `Error` with the error message. Brioche shows the error banner.

**Impact:** The cluster continues running the last successfully applied state. No state is lost. Applications are unaffected. Manual `relish apply` commands still work (they bypass Lettuce entirely).

**Recovery:** Automatic. When the git remote becomes reachable again, the next poll succeeds, back-off resets, and Lettuce catches up to the latest commit.

### 7.2 Invalid TOML in Repository

**Cause:** Syntax error in a TOML file committed to the repo. Missing required fields. Invalid values.

**Behaviour:** Per-file parse errors are recorded in `SyncState.file_errors`. Files that parse successfully are still diffed and applied. The sync is a `PartialSuccess` -- valid files are applied, invalid files are skipped with errors.

**Impact:** Resources defined in valid files are updated. Resources defined in invalid files retain their last-known-good state. This prevents a typo in one file from blocking deploys of other applications.

**Mitigation:** `relish lint` should be run in CI before merging. Lettuce logs a warning for each parse error and the Brioche dashboard highlights files with errors.

### 7.3 Signature Verification Failure

**Cause:** Unsigned commit when `require_signed_commits` is true, or unsigned commit modifying `script` fields. Commit signed by an untrusted key. Corrupt signature.

**Behaviour:** The entire commit is rejected. No changes are applied (even for files that don't contain scripts). `SyncState.last_error` records the reason. An alert fires.

**Impact:** The cluster remains on the last successfully applied commit. The rejected commit is retried on the next sync cycle (in case the operator adds the key to `trusted_signing_keys` in the meantime).

**Design rationale:** Rejecting the entire commit (not just script-containing files) prevents an attacker from bundling malicious script changes with legitimate config changes in the same commit and hoping the scripts slip through.

### 7.4 Coordinator Council Member Failure

**Cause:** Node crash, hardware failure, network partition isolating the coordinator from the council.

**Behaviour:** The council detects the failure via Raft heartbeat timeout (approximately 1.5 seconds). The leader elects a new coordinator. The new coordinator clones the repo (if needed) and resumes syncing.

**Impact:** Sync gap of approximately 5 seconds (failover) plus up to `poll_interval` for the next poll. In webhook mode, the gap is just the failover time because the next webhook triggers immediate sync. Applications continue running unaffected.

### 7.5 Webhook Replay Attacks

**Cause:** An attacker captures a valid webhook payload and re-sends it.

**Behaviour:** The deduplication set rejects duplicate `delivery_id` values with 409. Even if the deduplication set has evicted the entry (after 1000 newer deliveries), a replayed webhook for an already-applied commit is a no-op (the commit SHA matches `last_applied_commit`, so the sync short-circuits).

**Residual risk:** An attacker could replay a webhook for a commit that has been reverted in git. However, since Lettuce always fetches the latest commit from the remote (not the commit referenced in the webhook), a replayed webhook simply triggers a normal sync cycle that applies whatever is currently at HEAD. The webhook commit SHA isn't used for checkout.

### 7.6 Raft Leader Unavailable During Apply

**Cause:** Leader step-down or failure between diff computation and write completion.

**Behaviour:** The Raft write fails. Lettuce doesn't update `SyncState`, so the same commit is retried on the next cycle. The Raft client automatically discovers the new leader.

**Impact:** Apply is delayed by one sync cycle. No partial state is written (the batch is atomic).

### 7.7 Race Between Manual Apply and GitOps

**Cause:** An operator runs `relish apply` while Lettuce is mid-sync, or vice versa.

**Behaviour:** Both writes go through the Raft leader, which serializes them. The last write wins. On the next Lettuce sync cycle, Lettuce computes a diff that includes the manual change (if it differs from git) and reverts it back to the git state.

**Mitigation:** `relish apply` warns when GitOps is enabled for the target namespace: `"Warning: namespace 'production' is managed by GitOps. Manual changes will be reverted on the next sync cycle."`

---

## 8. Security Considerations

### 8.1 Signed Commit Enforcement

The primary security boundary. Signed commits ensure that only authorised developers can change cluster state via git. Without signed commits, anyone with git write access (including compromised CI systems, stolen credentials, or malicious insiders) can push arbitrary configuration changes.

**Auto-enforcement for scripts.** Even if `require_signed_commits` is false, inline `script` fields are RCE vectors (they execute arbitrary commands on cluster nodes). Lettuce automatically requires signed commits for any change touching `script` fields. This isn't configurable and can't be disabled.

**Key rotation.** To rotate signing keys, add the new key to `trusted_signing_keys` first, then have developers switch to the new key, then remove the old key. During the transition, both keys are trusted. Lettuce doesn't support "trust on first use" -- keys must be explicitly listed.

### 8.2 Webhook HMAC Validation

Every incoming webhook is validated using HMAC-SHA256 before any processing occurs. The validation uses constant-time comparison to prevent timing attacks. The `webhook_secret` must be at least 32 characters.

**Provider compatibility:**

| Provider | Signature Header | Format |
|----------|-----------------|--------|
| GitHub | `X-Hub-Signature-256` | `sha256=<hex>` |
| GitLab | `X-Gitlab-Token` | Direct comparison |
| Gitea | `X-Gitea-Signature` | `<hex>` |

Lettuce auto-detects the provider from the headers present in the request.

### 8.3 Git Credential Storage

Git credentials (SSH keys, HTTPS tokens) are stored on each council member node in `node.toml` or as files on disk. They are never stored in the Raft log or the git repository itself.

**SSH keys:** The SSH private key should be readable only by the Bun process user. Lettuce verifies the file permissions at startup and warns if the key is world-readable.

**HTTPS tokens:** Can be encrypted with the cluster's `age` public key (same encryption used for secrets). Bun decrypts at runtime.

**No credential passthrough.** Lettuce never logs, emits, or includes git credentials in error messages, webhook responses, or `SyncState`. Error messages from `git2` are sanitized to remove any credential material before being stored.

### 8.4 Inline Script RCE Prevention

The multi-layered defense against arbitrary code execution via git:

1. **Signed commits for script fields.** The primary defense. Prevents unsigned changes to `script` fields.
2. **Binary allowlist.** Even if a malicious script is deployed, it can only call binaries on the node's allowlist (`node.toml` `[process_workloads] allowed_binaries`). A script that tries to call `/usr/bin/curl` fails if `curl` isn't on the allowlist.
3. **Isolation.** Scripts run in a restricted namespace: cgroup limits, PID namespace, network namespace with Onion eBPF, restricted mount namespace, seccomp profile. They run as the `burger` user, not root.
4. **Audit logging.** The SHA-256 hash of every deployed script is logged in the event history. `relish history` shows which script content was deployed and by which commit.
5. **Lint warnings.** `relish lint` flags scripts containing suspicious patterns: `eval`, `base64 -d`, `curl | sh`, `wget | bash`, `python -c`, and downloads from unknown URLs.

### 8.5 TLS Requirement for Webhooks

The webhook endpoint requires TLS. Lettuce refuses to start the webhook listener on a plaintext port. This prevents credential sniffing (the webhook secret is transmitted in HTTP headers, so TLS is mandatory for confidentiality).

---

## 9. Performance

### 9.1 Sync Latency

Total sync latency is the sum of:

| Phase | Typical Duration | Notes |
|-------|-----------------|-------|
| Git fetch | 200-2000ms | Depends on network latency to git remote and repo size. Fetch is incremental (only new objects). |
| Signature verification | 1-5ms | Single GPG/SSH verify operation per commit. |
| TOML parse | 1-10ms | For up to 1000 TOML files. Parsing is CPU-bound and fast. |
| Diff computation | <1ms | Map comparison of hundreds of resources. |
| Raft write | 5-50ms | Single batched proposal to leader, replicated to council majority. |
| **Total** | **200-2100ms** | Dominated by git fetch network latency. |

**End-to-end deploy latency:**

- **Poll mode:** `poll_interval / 2` (average) + sync latency. With default 30s poll, average 15s + 1s sync = 16 seconds.
- **Webhook mode:** webhook network delivery (typically <1s) + sync latency. Total approximately 1-3 seconds from push to applied.

### 9.2 Diff Computation for Large Repos

The diff engine operates on the parsed `DesiredState` in memory, not on raw file content. For a large repo (1000 TOML files, 10,000 resources):

- Parsing: ~100ms (10,000 TOML tables at ~10us each).
- Diff: ~10ms (10,000 map lookups + field comparisons).
- Memory: ~50MB for the parsed state (estimated 5KB per resource).

This is well within acceptable bounds. The bottleneck for large repos is the git fetch, not the diff.

### 9.3 Git Clone Optimisation

The coordinator maintains a bare clone locally. Only the initial clone transfers the full repo. Subsequent fetches transfer only new objects (typically a few KB per commit).

On coordinator failover, the new coordinator must clone fresh if it has no local cache. To mitigate this:

- All council members periodically `git fetch` in the background (every 5 minutes) to keep a warm cache, even if they aren't the coordinator. This is a read-only operation with no sync side effects.
- The failover clone is therefore typically fast (only a few seconds of objects to fetch).

---

## 10. Testing Strategy

### 10.1 Sync Loop Testing

**Unit tests:**

- `test_sync_noop`: Fetch returns same commit as `last_applied_commit`. Verify no diff is computed and no Raft write occurs.
- `test_sync_new_commit`: Fetch returns a new commit. Verify diff is computed and correct resources are applied.
- `test_sync_partial_parse_error`: One file has invalid TOML. Verify other files are applied and the error is recorded.
- `test_sync_git_fetch_failure`: Git remote is unreachable. Verify exponential back-off is applied and `consecutive_failures` increments.
- `test_sync_raft_write_failure`: Raft leader rejects the write. Verify `SyncState` isn't updated and the commit is retried.

**Integration tests:**

- Spin up a local git repo (using `git init --bare`), push TOML files, and verify that Lettuce applies them to a mock Raft state store.
- Modify a file, push, and verify that only the changed resource appears in the diff.
- Delete a file, push, and verify that the resource is removed.

### 10.2 Signature Verification Testing

- `test_verify_gpg_valid`: Commit signed with a trusted GPG key. Verify `SignatureStatus::Verified`.
- `test_verify_gpg_untrusted`: Commit signed with an unknown GPG key. Verify `SignatureStatus::UntrustedKey` and rejection.
- `test_verify_ssh_valid`: Commit signed with a trusted SSH key. Verify `SignatureStatus::Verified`.
- `test_verify_unsigned_global_required`: Unsigned commit with `require_signed_commits = true`. Verify rejection.
- `test_verify_unsigned_script_field`: Unsigned commit that modifies a `script` field with `require_signed_commits = false`. Verify rejection.
- `test_verify_unsigned_no_script`: Unsigned commit that doesn't touch scripts with `require_signed_commits = false`. Verify acceptance.
- `test_verify_empty_trusted_keys_with_script`: `trusted_signing_keys` is empty but a script change is detected. Verify error message.

### 10.3 Autoscaler Interaction Testing

- `test_replicas_unchanged_other_fields_changed`: Git changes `env.LOG_LEVEL` but not `replicas`. Verify `replicas` isn't in the diff.
- `test_replicas_changed`: Git changes `replicas` from 3 to 5. Verify `replicas_changed = true` and the new value is applied.
- `test_autoscale_config_always_synced`: Git changes `[app.*.autoscale] max` from 10 to 20. Verify this is applied even when `replicas` is unchanged.
- `test_replicas_removed_from_git`: Git removes the `replicas` field entirely. Verify Lettuce uses the default (1) and resets the autoscaler.

### 10.4 Webhook Testing

- `test_webhook_valid_signature`: Valid HMAC-SHA256 signature. Verify 202 response and sync trigger.
- `test_webhook_invalid_signature`: Invalid signature. Verify 401 response and no sync trigger.
- `test_webhook_rate_limit`: Send 11 webhooks in 1 minute (limit is 10). Verify the 11th returns 429.
- `test_webhook_replay`: Send the same delivery ID twice. Verify the second returns 409.
- `test_webhook_wrong_branch`: Webhook for branch "staging" when config is "main". Verify 200 response and no sync trigger.
- `test_webhook_malformed_payload`: Invalid JSON body. Verify 400 response.

### 10.5 Coordinator Failover Testing

- `test_failover_new_coordinator_elected`: Kill the coordinator. Verify a new coordinator is elected within 2 seconds.
- `test_failover_resumes_from_last_state`: After failover, verify the new coordinator's first sync starts from the `last_applied_commit` in Raft.
- `test_failover_during_apply`: Kill the coordinator mid-apply. Verify no partial state is written (atomic batch). Verify the new coordinator re-applies the full commit.

### 10.6 End-to-End Testing

- `test_e2e_full_sync_cycle`: Push a new TOML file to a git repo. Wait for poll. Verify the app is deployed in the cluster.
- `test_e2e_webhook_deploy`: Push a commit and send a webhook. Verify the app is deployed within 5 seconds.
- `test_e2e_rollback_via_git`: Revert a commit in git. Verify Lettuce rolls back the change on the next sync.
- `test_e2e_manual_override_warning`: With GitOps enabled, run `relish apply`. Verify the warning message is displayed.

---

## 11. Prior Art

### 11.1 ArgoCD

**Architecture:** ArgoCD runs as a set of Kubernetes controllers (application-controller, repo-server, API server, Redis cache, Dex for SSO) in a dedicated namespace. It watches for `Application` CRDs and reconciles them against a git repository. The repo-server clones and caches repositories. The application-controller computes diffs using Kubernetes manifests and applies changes via `kubectl apply` semantics.

- **ArgoCD Architecture:** https://argo-cd.readthedocs.io/en/stable/operator-manual/architecture/

**What we borrow:** The concept of a sync status dashboard (Brioche's GitOps view is directly inspired by ArgoCD's UI). The notion of "sync waves" for ordered deploys (potential future feature).

**What we do differently:** ArgoCD is an external system that must be installed, configured, and upgraded separately. It requires its own HA setup (multiple replicas of each controller, Redis for caching). Lettuce is compiled into Bun -- zero additional infrastructure. ArgoCD operates on YAML/Helm/Kustomize; Lettuce operates on native TOML. ArgoCD has no awareness of autoscaler overrides; Lettuce handles the `replicas` field specially.

### 11.2 Flux

**Architecture:** Flux v2 uses a set of specialised controllers (source-controller for git/Helm repos, kustomize-controller for applying manifests, notification-controller for webhooks/alerts, image-reflector for image tag tracking). Each controller watches CRDs and reconciles independently. The source-controller polls git repositories and produces artifacts. The kustomize-controller applies them.

- **Flux Design:** https://fluxcd.io/flux/components/

**What we borrow:** The poll-based sync loop model is closely inspired by Flux's source-controller. The configurable poll interval, the incremental fetch, and the short-circuit on no-change are all patterns pioneered by Flux. The webhook-as-notification (trigger a reconciliation, not a deployment) model is also from Flux.

**What we do differently:** Flux is Kubernetes-native (CRDs, controllers, kube-apiserver). Lettuce is built into the orchestrator. Flux has no opinion on commit signing; Lettuce makes it a first-class security feature with auto-enforcement for scripts. Flux doesn't understand autoscaler interactions; Lettuce does.

### 11.3 HashiCorp Waypoint

**Architecture:** Waypoint provides a higher-level abstraction over the deploy pipeline. It uses `waypoint.hcl` files to define build, deploy, and release steps. Waypoint runs as a server that orchestrates these steps.

**Relevance:** Waypoint's approach of a single configuration file per application (rather than scattered YAML) influenced Reliaburger's TOML-per-app model. However, Waypoint is a deployment tool, not a GitOps reconciliation engine -- it doesn't continuously sync state.

### 11.4 OpenGitOps Principles

- **OpenGitOps:** https://opengitops.dev/

The OpenGitOps specification defines four principles:

1. **Declarative.** System state is expressed declaratively. Reliaburger TOML files satisfy this.
2. **Versioned and Immutable.** Desired state is stored in git, which is versioned and immutable (commits are content-addressed).
3. **Pulled Automatically.** Lettuce automatically pulls desired state from git (poll or webhook-triggered).
4. **Continuously Reconciled.** Lettuce continuously reconciles actual state with desired state.

Lettuce conforms to all four principles. The autoscaler interaction (Section 5.6) is an intentional deviation from strict reconciliation -- runtime replica overrides are treated as expected drift, not as configuration drift to be remediated. This is documented in `relish diff` output.

---

## 12. Libraries & Dependencies

All dependencies are Rust crates, compiled into the Bun binary. No runtime dependencies beyond the operating system.

| Crate | Version (min) | Purpose | Notes |
|-------|--------------|---------|-------|
| `git2` | 0.18 | libgit2 bindings for git operations (clone, fetch, checkout, log). | Statically links libgit2. Handles SSH and HTTPS transports. |
| `toml` | 0.8 | TOML parsing and serialisation. | Already used throughout Reliaburger for config parsing. |
| `sequoia-openpgp` | 1.17 | OpenPGP signature verification. Parses GPG signatures from commits and verifies against trusted keys. | Preferred over `pgp` crate for its active maintenance and correct implementation of the OpenPGP standard. |
| `ssh-key` | 0.6 | SSH key parsing. Extracts fingerprints from SSH signatures for verification against `trusted_signing_keys`. | Used alongside `ssh-encoding` for SSH signature format parsing. |
| `hmac` | 0.12 | HMAC computation for webhook signature validation. | Used with `sha2` for HMAC-SHA256. |
| `sha2` | 0.10 | SHA-256 hash computation. Used for webhook HMAC and script content hashing. | Already used elsewhere in Bun. |
| `reqwest` | 0.12 | HTTP client (not for webhook receiving, which uses the existing Bun API server). Reserved for future use: outbound notifications on sync events. | Optional dependency; may not be needed if notifications go through the existing alerting subsystem. |
| `serde` / `serde_json` | 1.0 | Serialisation for webhook payload parsing and `SyncState` Raft replication. | Already a core dependency. |
| `tokio` | 1.0 | Async runtime for the sync loop timer, webhook handler, and git fetch. | Already the async runtime for Bun. |
| `ring` | 0.17 | Constant-time HMAC comparison for webhook validation. | Alternative to `hmac` crate; provides `ring::hmac::verify` with constant-time comparison built in. Either `ring` or `hmac` should be chosen, not both. |

**Binary size impact:** The `git2` crate (with statically linked libgit2) adds approximately 2-3 MB to the Bun binary. `sequoia-openpgp` adds approximately 1-2 MB. Total Lettuce contribution to binary size: approximately 4-6 MB.

---

## 13. Open Questions

### 13.1 Multi-Repo Support

**Question:** Should Lettuce support watching multiple git repositories simultaneously?

**Use case:** Large organisations may have separate repos for infrastructure (namespaces, quotas), application teams (app specs), and security (firewall rules, secrets). Each repo has different access controls and review workflows.

**Current design:** Single repo. Multiple paths within the repo can simulate some of this (`path = "team-a/"` for one namespace, `path = "team-b/"` for another), but they share the same repo, branch, and signing keys.

**Proposed extension:** Allow an array of `[[gitops.sources]]` with per-source `repo`, `branch`, `path`, and `trusted_signing_keys`. Each source gets its own sync loop but shares the coordinator. Conflicts (two sources defining the same resource) are rejected at diff time.

**Concern:** Multi-repo increases coordinator complexity and memory usage. It also introduces ordering questions (which source wins when both modify a shared namespace?).

**Status:** Deferred to v2. Single-repo covers the majority of use cases.

### 13.2 Partial Sync (Apply Only Specific Apps)

**Question:** Should Lettuce support applying only a subset of resources from a commit?

**Use case:** A monorepo contains 50 apps. A developer wants to deploy only their app without triggering updates to others. Or: an operator wants to exclude a specific app from GitOps sync temporarily (e.g., during an incident).

**Proposed approach:**

1. **Ignore annotations.** A `gitops_ignore = true` field in an app's TOML causes Lettuce to skip it during diff and apply.
2. **Selective sync CLI.** `relish gitops sync --app web` triggers a sync that only applies changes to the named app.
3. **Namespace scoping.** Different namespaces can opt in or out of GitOps independently.

**Concern:** Partial sync weakens the "git is the source of truth" guarantee. If some resources are excluded, the cluster state diverges from git in expected but hard-to-track ways.

**Status:** Under discussion. Namespace-level opt-in is likely for v1; app-level ignore is a v2 candidate.

### 13.3 Drift Remediation Policy

**Question:** When Lettuce detects drift (actual state differs from desired state in git), should it auto-fix or alert only?

**Current design:** Auto-fix. Lettuce applies the git state on every sync, which overwrites any manual changes. This is the standard GitOps behaviour (Flux and ArgoCD both do this by default).

**Alternative:** An `alert-only` mode where Lettuce detects drift and fires an alert but doesn't apply changes. The operator must manually approve the sync (via `relish gitops sync --approve` or the Brioche UI).

**Use case for alert-only:** Regulated environments where every change requires a human approval step, even if the change is already committed in git. Incident response where operators need to make manual changes without GitOps reverting them.

**Proposed config:**

```toml
[gitops]
remediation = "auto"     # "auto" (default) or "alert-only"
```

In `alert-only` mode, `relish diff` shows the drift, and `relish gitops sync --approve` applies the pending changes. Lettuce still computes the diff on every poll and updates the Brioche dashboard, but doesn't write to Raft without approval.

**Concern:** `alert-only` mode requires some persistence of "pending changes" and "approval state," adding complexity. It also means the cluster can drift arbitrarily far from git if no one approves.

**Status:** Under discussion. Auto-fix is the v1 default. Alert-only mode is a strong candidate for v1.1.

### 13.4 Git History Depth

**Question:** Should Lettuce fetch full git history or shallow clones?

**Trade-off:** Full history enables `relish gitops log` to show the complete change history for any resource. Shallow clones (`--depth 1`) reduce clone time and disk usage on the coordinator.

**Proposed default:** Shallow clone with `depth = 50`. Sufficient for recent history display in Brioche. Full history can be enabled via `[gitops] clone_depth = 0`.

**Status:** Deferred. Initial implementation uses full clone for simplicity.

### 13.5 Secrets in Git

**Question:** How should secrets stored in the git repository be handled?

**Current assumption:** Secrets in git are encrypted with the cluster's `age` public key (Section 5.3 of the whitepaper). Lettuce applies them as-is; Bun decrypts at runtime when mounting into containers.

**Open question:** Should Lettuce support re-encryption? If a secret is encrypted with an old key, should Lettuce detect this and trigger re-encryption with the current key?

**Status:** Deferred. The current model (age-encrypted secrets, decrypted by Bun at runtime) works for v1.
