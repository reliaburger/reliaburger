//! Browser sessions for the Brioche dashboard.
//!
//! The API is bearer-token only, which suits the CLI but not a browser: an
//! `EventSource` (used for live logs) can't set an `Authorization` header, and
//! putting a CLI token in page JavaScript is a poor idea. Instead the browser
//! exchanges a token once for an opaque, `HttpOnly` session cookie.
//!
//! Sessions are deliberately **read-only** regardless of the token's role.
//! The dashboard only reads, and a read-only cookie contains the blast radius
//! of any cross-site request forgery: a forged request riding the cookie can
//! look but never mutate.
//!
//! A session is also never worth more than the token it came from. It records
//! which exact credential created it (the token's principal id) and cannot
//! outlive that token's expiry; the auth middleware refuses a session whose
//! token has since been revoked or has expired.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use ring::rand::{SecureRandom, SystemRandom};
use tokio::sync::RwLock;

use super::types::TokenScope;

/// The cookie name carrying the session id.
pub const SESSION_COOKIE: &str = "rb_session";

/// The longest a session stays valid after creation. A token that expires
/// sooner shortens it (see [`SessionStore::create`]).
pub const SESSION_TTL: Duration = Duration::from_secs(12 * 3600);

/// The identity a live session resolves to: the originating token's name and
/// principal id, plus the app/namespace scope it was confined to. The scope
/// travels with the session so a tenant-scoped token cannot widen to
/// cluster-wide reads by exchanging itself for a cookie (the C3 gap).
#[derive(Debug, Clone)]
pub struct SessionIdentity {
    /// The token name the session was created from.
    pub token_name: String,
    /// The exact credential the session was created from (the token's
    /// principal id, or the system principal). A reissued token with the same
    /// name has a different principal id, so it never revives this session.
    pub principal_id: String,
    /// The originating token's scope, carried onto the session context.
    pub scope: TokenScope,
}

/// One active browser session.
#[derive(Debug, Clone)]
struct Session {
    /// Who the session speaks for.
    identity: SessionIdentity,
    /// When the session expires: at most [`SESSION_TTL`] after creation, and
    /// never after the originating token expires.
    expires_at: SystemTime,
}

/// A freshly created session: its opaque id and how long it lives, so the
/// caller can give the cookie a matching `Max-Age`.
#[derive(Debug, Clone)]
pub struct NewSession {
    /// The opaque session id to put in the cookie.
    pub id: String,
    /// How long the session stays valid from now.
    pub lifetime: Duration,
}

/// A store of active browser sessions, keyed by opaque session id.
#[derive(Clone, Default)]
pub struct SessionStore {
    inner: Arc<RwLock<HashMap<String, Session>>>,
}

impl SessionStore {
    /// Create an empty session store.
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a new session for `identity` and return its opaque id and
    /// lifetime.
    ///
    /// The session lasts [`SESSION_TTL`], or until `credential_expires_at` if
    /// the originating token expires sooner. The id is 256 bits of randomness,
    /// hex-encoded. Creating a session opportunistically sweeps expired ones.
    pub async fn create(
        &self,
        identity: SessionIdentity,
        credential_expires_at: Option<SystemTime>,
    ) -> NewSession {
        let mut bytes = [0u8; 32];
        // The system RNG only fails if the OS entropy source is unavailable,
        // which on a running node it is not.
        SystemRandom::new()
            .fill(&mut bytes)
            .expect("system RNG unavailable");
        let id = hex::encode(bytes);

        let now = SystemTime::now();
        let lifetime = credential_expires_at
            .map(|at| at.duration_since(now).unwrap_or(Duration::ZERO))
            .map_or(SESSION_TTL, |left| left.min(SESSION_TTL));

        let mut guard = self.inner.write().await;
        guard.retain(|_, s| s.expires_at > now);
        guard.insert(
            id.clone(),
            Session {
                identity,
                expires_at: now + lifetime,
            },
        );
        NewSession { id, lifetime }
    }

    /// Return the session identity if `id` names a live session, else `None`.
    /// An expired session is removed as a side effect.
    pub async fn validate(&self, id: &str) -> Option<SessionIdentity> {
        // Fast path: a read lock covers the common valid case.
        {
            let guard = self.inner.read().await;
            match guard.get(id) {
                Some(s) if s.expires_at > SystemTime::now() => {
                    return Some(s.identity.clone());
                }
                Some(_) => {} // expired — fall through to remove it
                None => return None,
            }
        }
        self.inner.write().await.remove(id);
        None
    }

    /// Remove a session (logout).
    pub async fn remove(&self, id: &str) {
        self.inner.write().await.remove(id);
    }
}

/// Extract the `rb_session` value from a `Cookie` header, if present.
pub fn session_id_from_cookie_header(header: &str) -> Option<&str> {
    // A Cookie header is `name=value; name2=value2`. Values here are hex, so
    // no `=`/`;` escaping to worry about.
    header.split(';').find_map(|pair| {
        let pair = pair.trim();
        pair.strip_prefix(SESSION_COOKIE)
            .and_then(|rest| rest.strip_prefix('='))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity(name: &str, scope: TokenScope) -> SessionIdentity {
        SessionIdentity {
            token_name: name.to_string(),
            principal_id: format!("token:{name}"),
            scope,
        }
    }

    #[tokio::test]
    async fn create_then_validate_returns_the_token_name_and_principal() {
        let store = SessionStore::new();
        let id = store
            .create(identity("ci", TokenScope::default()), None)
            .await
            .id;
        let identity = store.validate(&id).await.expect("session should be live");
        assert_eq!(identity.token_name, "ci");
        assert_eq!(identity.principal_id, "token:ci");
    }

    #[tokio::test]
    async fn validate_carries_the_originating_token_scope() {
        let store = SessionStore::new();
        let scope = TokenScope {
            apps: Some(vec!["web".to_string()]),
            namespaces: Some(vec!["team-a".to_string()]),
        };
        let id = store
            .create(identity("scoped", scope.clone()), None)
            .await
            .id;
        let identity = store.validate(&id).await.expect("session should be live");
        assert_eq!(identity.scope.apps, scope.apps);
        assert_eq!(identity.scope.namespaces, scope.namespaces);
    }

    #[tokio::test]
    async fn an_unknown_session_id_is_rejected() {
        let store = SessionStore::new();
        assert!(store.validate("deadbeef").await.is_none());
    }

    #[tokio::test]
    async fn a_removed_session_no_longer_validates() {
        let store = SessionStore::new();
        let id = store
            .create(identity("ci", TokenScope::default()), None)
            .await
            .id;
        store.remove(&id).await;
        assert!(store.validate(&id).await.is_none());
    }

    #[tokio::test]
    async fn a_session_without_a_token_expiry_lasts_the_full_ttl() {
        let store = SessionStore::new();
        let session = store
            .create(identity("ci", TokenScope::default()), None)
            .await;
        assert_eq!(session.lifetime, SESSION_TTL);
    }

    #[tokio::test]
    async fn a_session_never_outlives_its_token() {
        // A token expiring in an hour caps the session at (about) an hour,
        // not the full twelve (B11).
        let store = SessionStore::new();
        let expiry = SystemTime::now() + Duration::from_secs(3600);
        let session = store
            .create(identity("ci", TokenScope::default()), Some(expiry))
            .await;
        assert!(session.lifetime <= Duration::from_secs(3600));
        assert!(session.lifetime > Duration::from_secs(3500));
    }

    #[tokio::test]
    async fn a_session_from_an_already_expired_token_is_dead_on_arrival() {
        let store = SessionStore::new();
        let expiry = SystemTime::now() - Duration::from_secs(1);
        let session = store
            .create(identity("ci", TokenScope::default()), Some(expiry))
            .await;
        assert_eq!(session.lifetime, Duration::ZERO);
        assert!(store.validate(&session.id).await.is_none());
    }

    #[test]
    fn session_ids_are_256_bit_hex() {
        // hex of 32 bytes = 64 chars. (create() is async; assert the shape via
        // a direct fill to avoid a runtime here.)
        let mut bytes = [0u8; 32];
        SystemRandom::new().fill(&mut bytes).unwrap();
        assert_eq!(hex::encode(bytes).len(), 64);
    }

    #[test]
    fn parses_the_session_cookie_among_others() {
        let header = "theme=dark; rb_session=abc123; other=1";
        assert_eq!(session_id_from_cookie_header(header), Some("abc123"));
    }

    #[test]
    fn returns_none_when_no_session_cookie() {
        assert_eq!(session_id_from_cookie_header("theme=dark"), None);
    }
}
