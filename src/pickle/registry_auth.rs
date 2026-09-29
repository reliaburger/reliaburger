//! Registry authentication, quotas, and upload-session expiry (REG4/REG8).
//!
//! The Pickle OCI registry reuses the cluster's existing authentication
//! ([`crate::sesame::auth`]) rather than inventing a parallel scheme: the
//! same bearer tokens and internal service token that guard the agent API
//! guard registry requests. Reads stay open only on the loopback listener;
//! routable reads require any valid principal. Writes require a principal
//! with at least `Deployer` role, or the internal service token that
//! node-to-node replication presents. A token scoped to apps or namespaces
//! is further held to repositories named `<namespace>/<app>` inside its
//! scope, for reads and writes alike ([`check_repository_scope`]).
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

use crate::sesame::auth::{AuthContext, AuthState};
use crate::sesame::types::{ApiRole, TokenScope};

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
    authenticate_writer(auth, bearer, allow_unauthenticated_bootstrap)
        .await
        .map(|_| ())
}

/// Authenticate a deploy-class writer without discarding its exact identity.
/// `None` represents the explicitly permitted standalone bootstrap window.
pub async fn authenticate_writer(
    auth: &AuthState,
    bearer: Option<&str>,
    allow_unauthenticated_bootstrap: bool,
) -> Result<Option<crate::sesame::auth::AuthContext>, WriteDenied> {
    // The internal service token authenticates node-to-node replication.
    if let (Some(bearer), Some(service)) = (bearer, auth.service_token.as_deref())
        && crate::sesame::auth::tokens_equal(bearer, service)
    {
        return Ok(Some(crate::sesame::auth::system_context()));
    }

    let tokens = { auth.tokens.read().await.clone() };
    if allow_unauthenticated_bootstrap && tokens.is_empty() && auth.service_token.is_none() {
        return Ok(None);
    }

    let Some(bearer) = bearer else {
        return Err(WriteDenied::Unauthenticated);
    };
    match crate::sesame::auth::authenticate(bearer, &tokens) {
        Ok(ctx) => {
            // A registry push is a deploy-class mutation.
            if crate::sesame::token::check_role(ctx.role, ApiRole::Deployer).is_ok() {
                Ok(Some(ctx))
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
///
/// Returns the authenticated principal so the caller can hold the read to
/// the token's app/namespace scope ([`check_repository_scope`]).
pub async fn authorise_read(
    auth: &AuthState,
    bearer: Option<&str>,
) -> Result<AuthContext, WriteDenied> {
    if let (Some(bearer), Some(service)) = (bearer, auth.service_token.as_deref())
        && crate::sesame::auth::tokens_equal(bearer, service)
    {
        return Ok(crate::sesame::auth::system_context());
    }

    let Some(bearer) = bearer else {
        return Err(WriteDenied::Unauthenticated);
    };
    let tokens = { auth.tokens.read().await.clone() };
    crate::sesame::auth::authenticate(bearer, &tokens).map_err(|_| WriteDenied::Unauthenticated)
}

/// Why a write was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriteDenied {
    /// No valid principal — 401.
    Unauthenticated,
    /// Valid principal but insufficient role — 403.
    Forbidden,
}

/// What a registry request does to the repository it names, for the token
/// scope decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepositoryAccess {
    /// Pull a manifest, list tags, or fetch or probe a blob.
    Read,
    /// Start, continue or finish a blob upload.
    WriteBlob,
    /// Publish a manifest, and so move a tag.
    WriteManifest,
}

/// Why a token's app/namespace scope refused a repository.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ScopeDenied {
    /// The name doesn't follow the `<namespace>/<app>` convention, so a
    /// scoped token has no namespace it could be held to.
    #[error(
        "repository {repository:?} is not named <namespace>/<app>; \
         a scoped token may only use repositories in its own namespaces"
    )]
    OutsideConvention { repository: String },
    /// The repository belongs to a namespace or app the token doesn't cover.
    #[error("token scope does not allow app {app:?} in namespace {namespace:?}")]
    OutOfScope { namespace: String, app: String },
}

/// The `(namespace, app)` a repository belongs to, or `None` when its name
/// doesn't say.
///
/// The first path segment is the namespace and everything after the first
/// `/` is the app: `team-a/web` is app `web` in `team-a`, and
/// `team-a/web/debug` is app `web/debug` in `team-a`. This is the rule
/// `/v1/build` applies to a `pickle://` destination. A bare name (`web`,
/// `reliaburger-bun`, `_buildcontext`), a name with an empty, `.` or `..`
/// segment, and the pull-through cache's reserved `cache/` prefix name no
/// namespace, so they answer `None`.
pub fn repository_namespace(repository: &str) -> Option<(&str, &str)> {
    let (namespace, app) = repository.split_once('/')?;
    let well_formed = repository
        .split('/')
        .all(|segment| !segment.is_empty() && segment != "." && segment != "..");
    if !well_formed || namespace == "cache" {
        return None;
    }
    Some((namespace, app))
}

/// Whether a principal is held to an app/namespace scope at all. The
/// anonymous bootstrap, the internal system principal and unscoped tokens
/// are not.
pub fn is_scoped(principal: Option<&AuthContext>) -> bool {
    principal.is_some_and(|principal| {
        principal.token_name != crate::sesame::auth::SYSTEM_PRINCIPAL
            && (principal.scoped_apps.is_some() || principal.scoped_namespaces.is_some())
    })
}

/// Scratch repositories that only ever receive bare blobs from `relish`:
/// `relish build` uploads the caller's source tarball to the first before
/// `/v1/build` checks the scope of the image it will produce, and
/// `relish upgrade` uploads the new binary to the second before
/// `/v1/upgrade/start` checks for an admin.
pub const BLOB_ONLY_REPOSITORIES: [&str; 2] = [
    super::build::BUILD_CONTEXT_REPOSITORY,
    crate::upgrade::BINARY_BLOB_REPO,
];

/// Hold a registry request to the caller's token scope.
///
/// Anyone [`is_scoped`] says isn't scoped passes: that keeps the standalone
/// bootstrap, the system principal (replication, the build runner, upgrade
/// fetches) and unscoped Admin/Deployer/ReadOnly tokens exactly where they
/// were. A scoped token may use a repository only when
/// [`repository_namespace`] places it in a namespace and app its scope
/// allows; a name the convention can't place is refused rather than guessed.
///
/// One exception: a scoped token may upload *blobs* (never a manifest) to the
/// two blob-only scratch repositories in [`BLOB_ONLY_REPOSITORIES`]. The API
/// route that consumes each blob does its own authorisation, and a
/// content-addressed blob with no manifest or tag grants nothing.
pub fn check_repository_scope(
    principal: Option<&AuthContext>,
    repository: &str,
    access: RepositoryAccess,
) -> Result<(), ScopeDenied> {
    let Some(principal) = principal.filter(|principal| is_scoped(Some(principal))) else {
        return Ok(());
    };
    if access == RepositoryAccess::WriteBlob && BLOB_ONLY_REPOSITORIES.contains(&repository) {
        return Ok(());
    }
    let Some((namespace, app)) = repository_namespace(repository) else {
        return Err(ScopeDenied::OutsideConvention {
            repository: repository.to_string(),
        });
    };
    let scope = TokenScope {
        apps: principal.scoped_apps.clone(),
        namespaces: principal.scoped_namespaces.clone(),
    };
    if scope.allows(app, namespace) {
        Ok(())
    } else {
        Err(ScopeDenied::OutOfScope {
            namespace: namespace.to_string(),
            app: app.to_string(),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum UploadState {
    Active,
    Retiring,
}

/// One in-flight chunked upload session's metadata (REG8).
#[derive(Debug, Clone)]
struct UploadSession {
    /// Last time this session saw activity (create or chunk).
    last_activity: SystemTime,
    /// The repository the upload targets (for quota accounting).
    repository: String,
    /// Exact authenticated credential; None is anonymous standalone bootstrap.
    principal_id: Option<String>,
    /// Bytes written so far.
    written: u64,
    state: UploadState,
    writer: Arc<tokio::sync::Semaphore>,
}

/// Tracks chunked upload sessions so they can expire and be swept (REG8).
///
/// Sessions are keyed by upload id. `touch` refreshes activity on every
/// chunk; `is_active` checks its TTL and lifecycle state; `sweep`
/// fences expired sessions until their on-disk temporary files are confirmed
/// absent. Failed deletions retain ownership for the next sweep.
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

    /// Record a new upload session for `repository` and its exact creator.
    pub async fn register(
        &self,
        upload_id: &str,
        repository: &str,
        principal_id: Option<&str>,
        now: SystemTime,
    ) {
        self.inner.write().await.insert(
            upload_id.to_string(),
            UploadSession {
                last_activity: now,
                repository: repository.to_string(),
                principal_id: principal_id.map(str::to_owned),
                written: 0,
                state: UploadState::Active,
                writer: Arc::new(tokio::sync::Semaphore::new(1)),
            },
        );
    }

    /// Claim the creator's sole writer for this repository, preventing PATCH/PUT races.
    pub async fn claim_writer(
        &self,
        upload_id: &str,
        repository: &str,
        principal_id: Option<&str>,
    ) -> Option<tokio::sync::OwnedSemaphorePermit> {
        let guard = self.inner.read().await;
        let session = guard.get(upload_id)?;
        if session.state == UploadState::Retiring
            || session.repository != repository
            || session.principal_id.as_deref() != principal_id
        {
            return None;
        }
        Arc::clone(&session.writer).try_acquire_owned().ok()
    }

    /// Refresh a session's activity and record its running size. Returns
    /// `false` if the session is unknown or already expired — the caller
    /// then rejects the chunk.
    pub async fn touch(&self, upload_id: &str, written: u64, now: SystemTime) -> bool {
        let mut guard = self.inner.write().await;
        let Some(session) = guard.get_mut(upload_id) else {
            return false;
        };
        if session.state == UploadState::Retiring
            || now
                .duration_since(session.last_activity)
                .map(|elapsed| elapsed > self.ttl)
                .unwrap_or(false)
        {
            // Keep ownership until the file deletion is confirmed.
            session.state = UploadState::Retiring;
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
            Some(session) => {
                session.state == UploadState::Active
                    && now
                        .duration_since(session.last_activity)
                        .map(|elapsed| elapsed <= self.ttl)
                        .unwrap_or(true)
            }
            None => false,
        }
    }

    /// Refuse future writers while retaining cleanup ownership. A current
    /// writer keeps its permit until its own bounded operation completes.
    pub async fn retire(&self, upload_id: &str) {
        if let Some(session) = self.inner.write().await.get_mut(upload_id) {
            session.state = UploadState::Retiring;
        }
    }

    /// Reclaim every expired or retired session whose writer has exited.
    /// Failed deletions stay fenced and are returned for logging and retry.
    pub async fn cleanup_expired(
        &self,
        store: &super::store::BlobStore,
        now: SystemTime,
    ) -> Vec<(String, super::types::PickleError)> {
        let mut failures = Vec::new();
        for id in self.sweep(now).await {
            match store.cancel_upload(&id).await {
                Ok(()) => {
                    self.complete(&id).await;
                }
                Err(error) => failures.push((id, error)),
            }
        }
        failures
    }

    /// Fence and delete this repository's partial uploads, retaining failed cleanup.
    /// The caller first excludes repository writers, including unpublished creation.
    pub async fn cleanup_repository(
        &self,
        store: &super::store::BlobStore,
        repository: &str,
    ) -> Result<(), super::types::PickleError> {
        let mut sessions = self.inner.write().await;
        let mut uploads = Vec::new();
        for (id, session) in sessions
            .iter_mut()
            .filter(|(_, session)| session.repository == repository)
        {
            session.state = UploadState::Retiring;
            let permit = session.writer.clone().try_acquire_owned().map_err(|_| {
                super::types::PickleError::ReplicationFailed(
                    "repository still has an active upload writer".into(),
                )
            })?;
            uploads.push((id.clone(), permit));
        }
        drop(sessions);
        for (id, _permit) in uploads {
            store.cancel_upload(&id).await?;
            self.complete(&id).await;
        }
        Ok(())
    }

    #[cfg(test)]
    pub(crate) async fn pause_registration(&self) -> impl Send + 'static {
        self.inner.clone().write_owned().await
    }

    /// Complete (remove) a session, returning its target repository.
    pub async fn complete(&self, upload_id: &str) -> Option<String> {
        self.inner
            .write()
            .await
            .remove(upload_id)
            .map(|s| s.repository)
    }

    /// Fence expired writers and return their IDs for cleanup. Ownership
    /// remains until `complete` confirms that the temporary file is absent.
    pub async fn sweep(&self, now: SystemTime) -> Vec<String> {
        let mut guard = self.inner.write().await;
        let expired: Vec<String> = guard
            .iter()
            .filter(|(_, s)| {
                s.writer.available_permits() > 0
                    && (s.state == UploadState::Retiring
                        || now
                            .duration_since(s.last_activity)
                            .map(|elapsed| elapsed > self.ttl)
                            .unwrap_or(false))
            })
            .map(|(id, _)| id.clone())
            .collect();
        for id in &expired {
            if let Some(session) = guard.get_mut(id) {
                session.state = UploadState::Retiring;
            }
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

    // --- token scope on repositories ---

    fn principal(role: ApiRole, apps: Option<&[&str]>, namespaces: Option<&[&str]>) -> AuthContext {
        let owned = |list: &[&str]| list.iter().map(|s| s.to_string()).collect();
        AuthContext {
            token_name: "ci".into(),
            principal_id: "ci-1".into(),
            role,
            scoped_apps: apps.map(owned),
            scoped_namespaces: namespaces.map(owned),
        }
    }

    const EVERY_ACCESS: [RepositoryAccess; 3] = [
        RepositoryAccess::Read,
        RepositoryAccess::WriteBlob,
        RepositoryAccess::WriteManifest,
    ];

    #[test]
    fn repository_namespace_splits_on_the_first_slash() {
        assert_eq!(repository_namespace("team-a/web"), Some(("team-a", "web")));
        assert_eq!(
            repository_namespace("team-a/web/debug"),
            Some(("team-a", "web/debug"))
        );
        assert_eq!(
            repository_namespace("library/redis"),
            Some(("library", "redis"))
        );
    }

    #[test]
    fn repository_namespace_refuses_names_that_place_nothing() {
        for name in [
            "web",
            "reliaburger-bun",
            "_buildcontext",
            "",
            "/web",
            "team-a/",
            "team-a//web",
            "./web",
            "team-a/../team-b/web",
            "cache/docker.io/library/redis",
        ] {
            assert_eq!(repository_namespace(name), None, "{name:?}");
        }
    }

    #[test]
    fn namespace_scoped_deployer_may_use_its_own_namespace() {
        let ctx = principal(ApiRole::Deployer, None, Some(&["team-a"]));
        for access in EVERY_ACCESS {
            assert!(check_repository_scope(Some(&ctx), "team-a/web", access).is_ok());
            assert!(check_repository_scope(Some(&ctx), "team-a/web/debug", access).is_ok());
        }
    }

    #[test]
    fn namespace_scoped_deployer_is_refused_other_namespaces() {
        let ctx = principal(ApiRole::Deployer, None, Some(&["team-a"]));
        for access in EVERY_ACCESS {
            assert_eq!(
                check_repository_scope(Some(&ctx), "team-b/web", access),
                Err(ScopeDenied::OutOfScope {
                    namespace: "team-b".into(),
                    app: "web".into(),
                })
            );
        }
    }

    /// A bare name carries no namespace, so a scoped token can't be held to
    /// one there and is refused rather than guessed (even one scoped to
    /// `default`).
    #[test]
    fn scoped_tokens_are_refused_names_outside_the_convention() {
        let ctx = principal(ApiRole::Deployer, None, Some(&["default", "team-a"]));
        for name in ["web", "api", "team-a//web", "cache/team-a/web"] {
            for access in EVERY_ACCESS {
                assert!(
                    matches!(
                        check_repository_scope(Some(&ctx), name, access),
                        Err(ScopeDenied::OutsideConvention { .. })
                    ),
                    "{name:?} {access:?}"
                );
            }
        }
    }

    #[test]
    fn app_scope_is_held_to_the_app_segment() {
        let ctx = principal(ApiRole::Deployer, Some(&["web"]), Some(&["team-a"]));
        assert!(
            check_repository_scope(Some(&ctx), "team-a/web", RepositoryAccess::WriteBlob).is_ok()
        );
        assert!(
            check_repository_scope(Some(&ctx), "team-a/api", RepositoryAccess::WriteBlob).is_err()
        );
        assert!(
            check_repository_scope(Some(&ctx), "team-a/web/debug", RepositoryAccess::Read).is_err()
        );
    }

    /// `relish build` uploads the caller's source tarball, and `relish
    /// upgrade` the new binary, as bare blobs before the API route that uses
    /// them checks the caller. Only blobs, though.
    #[test]
    fn scoped_tokens_may_upload_scratch_blobs_but_nothing_else_there() {
        let ctx = principal(ApiRole::Admin, None, Some(&["team-a"]));
        for scratch in ["_buildcontext", "reliaburger-bun"] {
            assert!(BLOB_ONLY_REPOSITORIES.contains(&scratch));
            assert!(
                check_repository_scope(Some(&ctx), scratch, RepositoryAccess::WriteBlob).is_ok()
            );
            assert!(
                check_repository_scope(Some(&ctx), scratch, RepositoryAccess::WriteManifest)
                    .is_err()
            );
            assert!(check_repository_scope(Some(&ctx), scratch, RepositoryAccess::Read).is_err());
        }
    }

    #[test]
    fn unscoped_tokens_and_the_system_principal_are_unaffected() {
        let unscoped = [
            principal(ApiRole::Admin, None, None),
            principal(ApiRole::Deployer, None, None),
            principal(ApiRole::ReadOnly, None, None),
            crate::sesame::auth::system_context(),
        ];
        for ctx in &unscoped {
            assert!(!is_scoped(Some(ctx)));
            for name in ["web", "team-b/web", "reliaburger-bun", "cache/x/y", ""] {
                for access in EVERY_ACCESS {
                    assert!(
                        check_repository_scope(Some(ctx), name, access).is_ok(),
                        "{} {name:?} {access:?}",
                        ctx.token_name
                    );
                }
            }
        }
        // The anonymous standalone bootstrap has no scope either.
        assert!(check_repository_scope(None, "web", RepositoryAccess::WriteManifest).is_ok());
    }

    #[test]
    fn a_scoped_admin_is_scoped_too() {
        let ctx = principal(ApiRole::Admin, None, Some(&["team-a"]));
        assert!(is_scoped(Some(&ctx)));
        assert!(
            check_repository_scope(Some(&ctx), "team-b/web", RepositoryAccess::WriteManifest)
                .is_err()
        );
    }

    #[tokio::test]
    async fn authorised_reads_return_the_principal_for_scoping() {
        let user = create_token(
            "reader",
            ApiRole::ReadOnly,
            TokenScope {
                apps: None,
                namespaces: Some(vec!["team-a".into()]),
            },
            None,
        )
        .unwrap();
        let store = new_token_store();
        store.write().await.push(user.token);
        let auth = AuthState::new(store, Some("rbrg_service".to_string()));
        let reader = authorise_read(&auth, Some(&user.plaintext)).await.unwrap();
        assert_eq!(reader.scoped_namespaces, Some(vec!["team-a".to_string()]));
        let system = authorise_read(&auth, Some("rbrg_service")).await.unwrap();
        assert!(!is_scoped(Some(&system)));
    }

    // --- upload session expiry (REG8) ---

    #[tokio::test]
    async fn expired_upload_stays_owned_until_cleanup_is_confirmed() {
        let sessions = UploadSessions::new(Duration::from_secs(60));
        let start = SystemTime::UNIX_EPOCH;
        sessions.register("abandoned", "web", None, start).await;
        let writer = sessions
            .claim_writer("abandoned", "web", None)
            .await
            .unwrap();
        let later = start + Duration::from_secs(61);
        assert!(sessions.sweep(later).await.is_empty());
        drop(writer);
        assert_eq!(sessions.sweep(later).await, vec!["abandoned"]);
        assert!(
            sessions
                .claim_writer("abandoned", "web", None)
                .await
                .is_none()
        );
        assert!(!sessions.touch("abandoned", 0, start).await);
        // A failed file removal must leave the next sweep an owner to retry.
        assert_eq!(sessions.sweep(later).await, vec!["abandoned"]);
        sessions.complete("abandoned").await.unwrap();
        assert!(sessions.sweep(later).await.is_empty());
    }

    #[tokio::test]
    async fn expired_upload_session_is_rejected_and_swept() {
        let sessions = UploadSessions::new(Duration::from_secs(60));
        let start = SystemTime::UNIX_EPOCH;
        sessions.register("abc", "web", None, start).await;
        assert!(sessions.is_active("abc", start).await);

        // 61s later: past the TTL.
        let later = start + Duration::from_secs(61);
        assert!(!sessions.touch("abc", 10, later).await, "expired touch");
        // Expiry fences writes, but cleanup still owns the temporary file.
        let swept = sessions.sweep(later).await;
        assert_eq!(swept, vec!["abc"]);
        assert!(!sessions.is_active("abc", later).await);
    }

    #[tokio::test]
    async fn sweep_removes_only_expired_sessions() {
        let sessions = UploadSessions::new(Duration::from_secs(60));
        let start = SystemTime::UNIX_EPOCH;
        sessions.register("old", "web", None, start).await;
        let recent = start + Duration::from_secs(120);
        sessions.register("fresh", "web", None, recent).await;

        let swept = sessions.sweep(recent).await;
        assert_eq!(swept, vec!["old".to_string()]);
        assert!(sessions.is_active("fresh", recent).await);
    }

    #[tokio::test]
    async fn touch_within_ttl_keeps_the_session_alive() {
        let sessions = UploadSessions::new(Duration::from_secs(60));
        let start = SystemTime::UNIX_EPOCH;
        sessions.register("abc", "web", None, start).await;
        let within = start + Duration::from_secs(30);
        assert!(sessions.touch("abc", 5, within).await);
        assert!(sessions.is_active("abc", within).await);
    }
}
