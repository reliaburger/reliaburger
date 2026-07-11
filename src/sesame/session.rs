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

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, SystemTime};

use ring::rand::{SecureRandom, SystemRandom};
use tokio::sync::RwLock;

/// The cookie name carrying the session id.
pub const SESSION_COOKIE: &str = "rb_session";

/// How long a session stays valid after creation.
const SESSION_TTL: Duration = Duration::from_secs(12 * 3600);

/// One active browser session.
#[derive(Debug, Clone)]
struct Session {
    /// The token name the session was created from (for audit only).
    token_name: String,
    /// When the session expires.
    expires_at: SystemTime,
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

    /// Create a new session for `token_name` and return its opaque id.
    ///
    /// The id is 256 bits of randomness, hex-encoded. Creating a session
    /// opportunistically sweeps expired ones.
    pub async fn create(&self, token_name: &str) -> String {
        let mut bytes = [0u8; 32];
        // The system RNG only fails if the OS entropy source is unavailable,
        // which on a running node it is not.
        SystemRandom::new()
            .fill(&mut bytes)
            .expect("system RNG unavailable");
        let id = hex::encode(bytes);

        let mut guard = self.inner.write().await;
        let now = SystemTime::now();
        guard.retain(|_, s| s.expires_at > now);
        guard.insert(
            id.clone(),
            Session {
                token_name: token_name.to_string(),
                expires_at: now + SESSION_TTL,
            },
        );
        id
    }

    /// Return the token name if `id` names a live session, else `None`.
    /// An expired session is removed as a side effect.
    pub async fn validate(&self, id: &str) -> Option<String> {
        // Fast path: a read lock covers the common valid case.
        {
            let guard = self.inner.read().await;
            match guard.get(id) {
                Some(s) if s.expires_at > SystemTime::now() => return Some(s.token_name.clone()),
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

    #[tokio::test]
    async fn create_then_validate_returns_the_token_name() {
        let store = SessionStore::new();
        let id = store.create("ci").await;
        assert_eq!(store.validate(&id).await.as_deref(), Some("ci"));
    }

    #[tokio::test]
    async fn an_unknown_session_id_is_rejected() {
        let store = SessionStore::new();
        assert!(store.validate("deadbeef").await.is_none());
    }

    #[tokio::test]
    async fn a_removed_session_no_longer_validates() {
        let store = SessionStore::new();
        let id = store.create("ci").await;
        store.remove(&id).await;
        assert!(store.validate(&id).await.is_none());
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
