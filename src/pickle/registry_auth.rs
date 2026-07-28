//! Registry authentication, quotas, and upload-session expiry (REG4/REG8).
//!
//! The Pickle OCI registry reuses the cluster's existing authentication
//! ([`crate::sesame::auth`]) rather than inventing a parallel scheme: the
//! same bearer tokens and internal service token that guard the agent API
//! guard registry requests. Reads stay open only on the loopback listener;
//! routable reads require any valid principal. Writes require a principal
//! with at least `Deployer` role, or the internal service token that
//! node-to-node replication presents.
//!
//! Two policy dimensions live here alongside the auth gate:
//!
//! - **Quotas** — a per-repository byte ceiling and an aggregate registry
//!   ceiling, so one repository (or one runaway client) can't fill the disk.
//! - **Upload-session expiry** — chunked upload sessions carry a TTL; a
//!   session that goes quiet past the TTL is rejected and swept, so an
//!   abandoned push doesn't leak a temp file forever (REG8).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use tokio::sync::RwLock;

use crate::sesame::auth::AuthState;
use crate::sesame::types::ApiRole;

/// How long a chunked upload session lives without activity before it's
/// rejected and swept (REG8). Generous enough for a slow large-layer push,
/// short enough that abandoned sessions don't accumulate.
pub const DEFAULT_UPLOAD_TTL: Duration = Duration::from_secs(3600);

/// Quota configuration for the registry.
///
/// `0` means unlimited on either axis — the default, so an operator opts
/// into limits rather than tripping over them.
#[derive(Debug, Clone, Copy, Default)]
pub struct QuotaConfig {
    /// Maximum stored bytes for any single repository. `0` = unlimited.
    pub per_repository_bytes: u64,
    /// Maximum stored bytes across the whole registry. `0` = unlimited.
    pub total_bytes: u64,
}

impl QuotaConfig {
    /// Whether both axes are unlimited (the fast, common path).
    pub fn is_unlimited(&self) -> bool {
        self.per_repository_bytes == 0 && self.total_bytes == 0
    }
}

/// Why a quota check refused an upload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum QuotaError {
    /// The repository would exceed its byte ceiling.
    RepositoryExceeded {
        repository: String,
        current: u64,
        incoming: u64,
        limit: u64,
    },
    /// The registry as a whole would exceed its byte ceiling.
    TotalExceeded {
        current: u64,
        incoming: u64,
        limit: u64,
    },
}

impl std::fmt::Display for QuotaError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            QuotaError::RepositoryExceeded {
                repository,
                current,
                incoming,
                limit,
            } => write!(
                f,
                "repository {repository} quota exceeded: {current}+{incoming} over {limit} bytes"
            ),
            QuotaError::TotalExceeded {
                current,
                incoming,
                limit,
            } => write!(
                f,
                "registry quota exceeded: {current}+{incoming} over {limit} bytes"
            ),
        }
    }
}

/// Check whether admitting `incoming` bytes into `repository` would breach
/// either quota axis (REG4).
///
/// `repository_current` is the repository's current stored size,
/// `total_current` the whole registry's. A `0` limit on an axis skips it.
pub fn check_quota(
    config: &QuotaConfig,
    repository: &str,
    incoming: u64,
    repository_current: u64,
    total_current: u64,
) -> Result<(), QuotaError> {
    if config.per_repository_bytes != 0
        && repository_current.saturating_add(incoming) > config.per_repository_bytes
    {
        return Err(QuotaError::RepositoryExceeded {
            repository: repository.to_string(),
            current: repository_current,
            incoming,
            limit: config.per_repository_bytes,
        });
    }
    if config.total_bytes != 0 && total_current.saturating_add(incoming) > config.total_bytes {
        return Err(QuotaError::TotalExceeded {
            current: total_current,
            incoming,
            limit: config.total_bytes,
        });
    }
    Ok(())
}

/// Whether a registry write is authorised (REG4).
///
/// Reuses [`AuthState`] exactly like the agent API: the internal service
/// token (node-to-node replication) is accepted; a user bearer must resolve
/// to at least `Deployer`. During a standalone bootstrap window (no user
/// tokens and no service token) writes may be open when the caller explicitly
/// permits it. Clustered callers pass `false`, so a missing master key cannot
/// turn a peer-reachable registry into an anonymous write endpoint.
///
/// Returns `Ok(())` when the request may write, or `Err(reason)` — the
/// caller maps that to a 401/403.
pub async fn authorise_write(
    auth: &AuthState,
    bearer: Option<&str>,
    allow_unauthenticated_bootstrap: bool,
) -> Result<(), WriteDenied> {
    // The internal service token authenticates node-to-node replication.
    if let (Some(bearer), Some(service)) = (bearer, auth.service_token.as_deref())
        && crate::sesame::auth::tokens_equal(bearer, service)
    {
        return Ok(());
    }

    let tokens = { auth.tokens.read().await.clone() };
    if allow_unauthenticated_bootstrap && tokens.is_empty() && auth.service_token.is_none() {
        return Ok(());
    }

    let Some(bearer) = bearer else {
        return Err(WriteDenied::Unauthenticated);
    };
    match crate::sesame::auth::authenticate(bearer, &tokens) {
        Ok(ctx) => {
            // A registry push is a deploy-class mutation.
            if crate::sesame::token::check_role(ctx.role, ApiRole::Deployer).is_ok() {
                Ok(())
            } else {
                Err(WriteDenied::Forbidden)
            }
        }
        Err(_) => Err(WriteDenied::Unauthenticated),
    }
}

/// Whether a registry *read* is authorised (O1).
///
/// Reads were open unconditionally, which is right for the loopback default
/// — a local `docker pull` and the node's own image fetches shouldn't need a
/// token — but wrong once the registry is published on a routable address.
/// There, any client that can route a packet could enumerate and pull every
/// image in the cluster, including the `cache/` copies of private upstream
/// registries pulled with the operator's credentials.
///
/// The caller decides whether reads need a principal (see
/// `PickleState::require_read_auth`); this function answers *who counts* when
/// they do. The bar is deliberately lower than for writes: any valid token,
/// no minimum role, because pulling is what read-only tokens are for.
pub async fn authorise_read(auth: &AuthState, bearer: Option<&str>) -> Result<(), WriteDenied> {
    if let (Some(bearer), Some(service)) = (bearer, auth.service_token.as_deref())
        && crate::sesame::auth::tokens_equal(bearer, service)
    {
        return Ok(());
    }

    let Some(bearer) = bearer else {
        return Err(WriteDenied::Unauthenticated);
    };
    let tokens = { auth.tokens.read().await.clone() };
    match crate::sesame::auth::authenticate(bearer, &tokens) {
        Ok(_) => Ok(()),
        Err(_) => Err(WriteDenied::Unauthenticated),
    }
}

/// Why a write was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteDenied {
    /// No valid principal — 401.
    Unauthenticated,
    /// Valid principal but insufficient role — 403.
    Forbidden,
}

/// One in-flight chunked upload session's metadata (REG8).
#[derive(Debug, Clone)]
struct UploadSession {
    /// Last time this session saw activity (create or chunk).
    last_activity: SystemTime,
    /// The repository the upload targets (for quota accounting).
    repository: String,
    /// Bytes written so far.
    written: u64,
}

/// Tracks chunked upload sessions so they can expire and be swept (REG8).
///
/// Sessions are keyed by upload id. `touch` refreshes activity on every
/// chunk; `is_expired` reports whether a session outlived its TTL; `sweep`
/// removes expired sessions and returns their ids so the caller can delete
/// the on-disk temp files.
#[derive(Clone, Default)]
pub struct UploadSessions {
    inner: Arc<RwLock<HashMap<String, UploadSession>>>,
    ttl: Duration,
}

impl UploadSessions {
    /// Create a session tracker with the given TTL.
    pub fn new(ttl: Duration) -> Self {
        Self {
            inner: Arc::new(RwLock::new(HashMap::new())),
            ttl,
        }
    }

    /// Record a new upload session for `repository`.
    pub async fn register(&self, upload_id: &str, repository: &str, now: SystemTime) {
        self.inner.write().await.insert(
            upload_id.to_string(),
            UploadSession {
                last_activity: now,
                repository: repository.to_string(),
                written: 0,
            },
        );
    }

    /// Refresh a session's activity and record its running size. Returns
    /// `false` if the session is unknown or already expired — the caller
    /// then rejects the chunk.
    pub async fn touch(&self, upload_id: &str, written: u64, now: SystemTime) -> bool {
        let mut guard = self.inner.write().await;
        let Some(session) = guard.get_mut(upload_id) else {
            return false;
        };
        if now
            .duration_since(session.last_activity)
            .map(|elapsed| elapsed > self.ttl)
            .unwrap_or(false)
        {
            // Expired: drop it so the sweep (or the caller) cleans up.
            guard.remove(upload_id);
            return false;
        }
        session.last_activity = now;
        session.written = written;
        true
    }

    /// Whether a session exists and is still within its TTL.
    pub async fn is_active(&self, upload_id: &str, now: SystemTime) -> bool {
        let guard = self.inner.read().await;
        match guard.get(upload_id) {
            Some(session) => now
                .duration_since(session.last_activity)
                .map(|elapsed| elapsed <= self.ttl)
                .unwrap_or(true),
            None => false,
        }
    }

    /// Complete (remove) a session, returning its target repository.
    pub async fn complete(&self, upload_id: &str) -> Option<String> {
        self.inner
            .write()
            .await
            .remove(upload_id)
            .map(|s| s.repository)
    }

    /// Remove every session past its TTL, returning the swept ids so the
    /// caller can delete their temp files.
    pub async fn sweep(&self, now: SystemTime) -> Vec<String> {
        let mut guard = self.inner.write().await;
        let expired: Vec<String> = guard
            .iter()
            .filter(|(_, s)| {
                now.duration_since(s.last_activity)
                    .map(|elapsed| elapsed > self.ttl)
                    .unwrap_or(false)
            })
            .map(|(id, _)| id.clone())
            .collect();
        for id in &expired {
            guard.remove(id);
        }
        expired
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sesame::auth::new_token_store;
    use crate::sesame::token::create_token;
    use crate::sesame::types::TokenScope;

    // --- quotas ---

    #[test]
    fn unlimited_quota_admits_everything() {
        let config = QuotaConfig::default();
        assert!(config.is_unlimited());
        assert!(check_quota(&config, "web", 1_000_000_000, 0, 0).is_ok());
    }

    #[test]
    fn per_repository_quota_refuses_over_limit() {
        let config = QuotaConfig {
            per_repository_bytes: 1000,
            total_bytes: 0,
        };
        // 900 stored + 200 incoming = 1100 > 1000.
        let err = check_quota(&config, "web", 200, 900, 5000).unwrap_err();
        assert!(matches!(err, QuotaError::RepositoryExceeded { .. }));
    }

    #[test]
    fn per_repository_quota_admits_within_limit() {
        let config = QuotaConfig {
            per_repository_bytes: 1000,
            total_bytes: 0,
        };
        assert!(check_quota(&config, "web", 100, 900, 900).is_ok());
    }

    #[test]
    fn total_quota_refuses_over_limit() {
        let config = QuotaConfig {
            per_repository_bytes: 0,
            total_bytes: 2000,
        };
        let err = check_quota(&config, "web", 500, 100, 1800).unwrap_err();
        assert!(matches!(err, QuotaError::TotalExceeded { .. }));
    }

    // --- auth ---

    #[tokio::test]
    async fn write_is_open_during_the_bootstrap_window() {
        let auth = AuthState::new(new_token_store(), None);
        assert!(authorise_write(&auth, None, true).await.is_ok());
    }

    #[tokio::test]
    async fn clustered_bootstrap_requires_authentication_even_without_a_service_token() {
        let auth = AuthState::new(new_token_store(), None);
        assert_eq!(
            authorise_write(&auth, None, false).await.unwrap_err(),
            WriteDenied::Unauthenticated
        );
    }

    #[tokio::test]
    async fn clustered_service_token_authenticates_the_first_write() {
        let auth = AuthState::new(new_token_store(), Some("rbrg_service".to_string()));
        assert_eq!(
            authorise_write(&auth, None, false).await.unwrap_err(),
            WriteDenied::Unauthenticated
        );
        assert!(
            authorise_write(&auth, Some("rbrg_service"), false)
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn routable_read_fails_closed_before_the_first_user_token() {
        let auth = AuthState::new(new_token_store(), Some("rbrg_service".to_string()));
        assert_eq!(
            authorise_read(&auth, None).await.unwrap_err(),
            WriteDenied::Unauthenticated
        );
        assert!(authorise_read(&auth, Some("rbrg_service")).await.is_ok());
    }

    #[tokio::test]
    async fn anonymous_write_is_refused_once_a_token_exists() {
        let user = create_token("u", ApiRole::Deployer, TokenScope::default(), None).unwrap();
        let store = new_token_store();
        store.write().await.push(user.token);
        let auth = AuthState::new(store, None);
        assert_eq!(
            authorise_write(&auth, None, false).await.unwrap_err(),
            WriteDenied::Unauthenticated
        );
    }

    #[tokio::test]
    async fn read_only_bearer_is_forbidden_to_write() {
        let user = create_token("u", ApiRole::ReadOnly, TokenScope::default(), None).unwrap();
        let store = new_token_store();
        store.write().await.push(user.token.clone());
        let auth = AuthState::new(store, None);
        assert_eq!(
            authorise_write(&auth, Some(&user.plaintext), false)
                .await
                .unwrap_err(),
            WriteDenied::Forbidden
        );
    }

    #[tokio::test]
    async fn deployer_bearer_may_write() {
        let user = create_token("u", ApiRole::Deployer, TokenScope::default(), None).unwrap();
        let store = new_token_store();
        store.write().await.push(user.token.clone());
        let auth = AuthState::new(store, None);
        assert!(
            authorise_write(&auth, Some(&user.plaintext), false)
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn the_service_token_authenticates_node_replication() {
        let user = create_token("u", ApiRole::Deployer, TokenScope::default(), None).unwrap();
        let store = new_token_store();
        store.write().await.push(user.token);
        let auth = AuthState::new(store, Some("rbrg_service".to_string()));
        assert!(
            authorise_write(&auth, Some("rbrg_service"), false)
                .await
                .is_ok()
        );
    }

    // --- upload session expiry (REG8) ---

    #[tokio::test]
    async fn expired_upload_session_is_rejected_and_swept() {
        let sessions = UploadSessions::new(Duration::from_secs(60));
        let start = SystemTime::UNIX_EPOCH;
        sessions.register("abc", "web", start).await;
        assert!(sessions.is_active("abc", start).await);

        // 61s later: past the TTL.
        let later = start + Duration::from_secs(61);
        assert!(!sessions.touch("abc", 10, later).await, "expired touch");
        // The failed touch already dropped it; the sweep sees nothing left.
        let swept = sessions.sweep(later).await;
        assert!(swept.is_empty());
        assert!(!sessions.is_active("abc", later).await);
    }

    #[tokio::test]
    async fn sweep_removes_only_expired_sessions() {
        let sessions = UploadSessions::new(Duration::from_secs(60));
        let start = SystemTime::UNIX_EPOCH;
        sessions.register("old", "web", start).await;
        let recent = start + Duration::from_secs(120);
        sessions.register("fresh", "web", recent).await;

        let swept = sessions.sweep(recent).await;
        assert_eq!(swept, vec!["old".to_string()]);
        assert!(sessions.is_active("fresh", recent).await);
    }

    #[tokio::test]
    async fn touch_within_ttl_keeps_the_session_alive() {
        let sessions = UploadSessions::new(Duration::from_secs(60));
        let start = SystemTime::UNIX_EPOCH;
        sessions.register("abc", "web", start).await;
        let within = start + Duration::from_secs(30);
        assert!(sessions.touch("abc", 5, within).await);
        assert!(sessions.is_active("abc", within).await);
    }
}
