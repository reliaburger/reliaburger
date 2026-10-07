//! API token generation, hashing, and validation.
//!
//! Tokens are 256-bit random values, hashed with Argon2id before storage.
//! Each token has a role (Admin, Deployer, ReadOnly) and optional scope.

use std::time::{Duration, SystemTime};

use argon2::Argon2;
use argon2::password_hash::rand_core::OsRng;
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use ring::rand::{SecureRandom, SystemRandom};

use super::types::{ApiRole, ApiToken, PreviousSecret, TokenRotation, TokenScope};

/// Errors from token operations.
#[derive(Debug, thiserror::Error)]
pub enum TokenError {
    #[error("failed to generate token: {0}")]
    GenerationFailed(String),
    #[error("failed to hash token: {0}")]
    HashFailed(String),
    #[error("token validation failed")]
    ValidationFailed,
    #[error("token has expired")]
    Expired,
    #[error("token was rotated and this secret has stopped working")]
    Rotated,
    #[error("insufficient permissions: requires {required} role")]
    InsufficientRole { required: String },
    #[error("token scope does not allow this operation")]
    OutOfScope,
    #[error("token name {0:?} is reserved for the internal service principal")]
    ReservedName(String),
}

/// The result of creating a new API token.
pub struct CreatedToken {
    /// The plaintext token to return to the user (shown once, never stored).
    pub plaintext: String,
    /// The token struct for Raft storage (contains hash, not plaintext).
    pub token: ApiToken,
}

/// A freshly minted secret: the plaintext for the caller, and the hash and
/// salt for Raft.
struct MintedSecret {
    plaintext: String,
    token_hash: Vec<u8>,
    token_salt: Vec<u8>,
}

/// Mint a random `rbrg_` secret and hash it with Argon2id.
fn mint_secret() -> Result<MintedSecret, TokenError> {
    let rng = SystemRandom::new();
    let mut token_bytes = [0u8; 32];
    rng.fill(&mut token_bytes)
        .map_err(|_| TokenError::GenerationFailed("RNG failed".to_string()))?;

    let plaintext = format!("rbrg_{}", hex::encode(token_bytes));

    let salt = SaltString::generate(&mut OsRng);
    let password_hash = Argon2::default()
        .hash_password(plaintext.as_bytes(), &salt)
        .map_err(|e| TokenError::HashFailed(e.to_string()))?;

    Ok(MintedSecret {
        plaintext,
        token_hash: password_hash.to_string().into_bytes(),
        token_salt: salt.as_str().as_bytes().to_vec(),
    })
}

/// Generate a new API token.
///
/// Returns the plaintext (for the user) and the hashed token (for Raft).
/// The plaintext is prefixed with `rbrg_` for easy identification.
pub fn create_token(
    name: &str,
    role: ApiRole,
    scope: TokenScope,
    expires_at: Option<SystemTime>,
) -> Result<CreatedToken, TokenError> {
    // The service principal name is not a real user token; a user token minted
    // with this name would match `SYSTEM_PRINCIPAL` in the auth layer and
    // bypass every scope/role confinement. Refuse it at the source.
    if name == super::auth::SYSTEM_PRINCIPAL {
        return Err(TokenError::ReservedName(name.to_string()));
    }

    let secret = mint_secret()?;
    let token = ApiToken {
        name: name.to_string(),
        token_hash: secret.token_hash,
        token_salt: secret.token_salt,
        role,
        scope,
        expires_at,
        created_at: SystemTime::now(),
        previous_secret: None,
    };

    Ok(CreatedToken {
        plaintext: secret.plaintext,
        token,
    })
}

/// How long a rotated-out secret keeps working unless the caller asks for
/// another grace period: a day, long enough to roll the new secret out to
/// every client.
pub const DEFAULT_ROTATION_GRACE: Duration = Duration::from_secs(24 * 60 * 60);

/// The result of rotating an API token.
pub struct RotatedToken {
    /// The new plaintext secret, shown once and never stored.
    pub plaintext: String,
    /// The Raft payload that installs it.
    pub rotation: TokenRotation,
}

/// Mint a new secret for `stored`, rotated at `now`.
///
/// The new secret gets the lifetime length the old one had (a 90-day token
/// stays a 90-day token), or none if it had none. The old secret keeps
/// working for `grace`, but never past the token's old expiry; a zero
/// grace, or a token that has already expired, ends it at once.
pub fn rotate_token(
    stored: &ApiToken,
    now: SystemTime,
    grace: Duration,
) -> Result<RotatedToken, TokenError> {
    let secret = mint_secret()?;
    let expires_at = stored.expires_at.map(|at| {
        let lifetime = at.duration_since(stored.created_at).unwrap_or_default();
        now.checked_add(lifetime).unwrap_or(at)
    });
    let previous_valid_until = now
        .checked_add(grace)
        .map(|until| stored.expires_at.map_or(until, |at| until.min(at)))
        .filter(|until| *until > now);
    Ok(RotatedToken {
        plaintext: secret.plaintext,
        rotation: TokenRotation {
            name: stored.name.clone(),
            token_hash: secret.token_hash,
            token_salt: secret.token_salt,
            rotated_at: now,
            expires_at,
            previous_valid_until,
        },
    })
}

/// Install `rotation` on `token`: the new secret becomes current and the
/// old one, if it has a grace period, becomes the previous secret. Any
/// earlier previous secret is dropped, so at most two secrets ever work.
///
/// The name, role and scope don't change, and neither does the token's
/// `[permission]` spec, because specs are keyed by name.
pub fn apply_rotation(token: &mut ApiToken, rotation: &TokenRotation) {
    // `std::mem::replace` puts the new hash in and hands back the old one,
    // so the old bytes move into the previous secret instead of being copied.
    let old_hash = std::mem::replace(&mut token.token_hash, rotation.token_hash.clone());
    let old_salt = std::mem::replace(&mut token.token_salt, rotation.token_salt.clone());
    token.previous_secret = rotation
        .previous_valid_until
        .map(|valid_until| PreviousSecret {
            token_hash: old_hash,
            token_salt: old_salt,
            valid_until,
        });
    token.created_at = rotation.rotated_at;
    token.expires_at = rotation.expires_at;
}

/// The default lifetime of a new Deployer or ReadOnly API token (F05 I3).
pub const DEFAULT_TOKEN_TTL: Duration = Duration::from_secs(90 * 24 * 60 * 60);

/// How close to its expiry a token has to be before `relish token list` and
/// `relish wtf` warn about it.
pub const TOKEN_EXPIRY_WARNING: Duration = Duration::from_secs(14 * 24 * 60 * 60);

/// How old an Admin token without an expiry may get before `relish wtf`
/// suggests rotating it: the lifetime the other roles get by default.
pub const ADMIN_TOKEN_AGE_WARNING: Duration = DEFAULT_TOKEN_TTL;

/// The lifetime a node gives a token created without `--ttl-days` or
/// `--no-expiry` (`[security.tokens] default_ttl`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TokenLifetimePolicy {
    /// The default lifetime; `None` leaves such tokens without an expiry.
    pub default_ttl: Option<Duration>,
}

impl Default for TokenLifetimePolicy {
    fn default() -> Self {
        Self {
            default_ttl: Some(DEFAULT_TOKEN_TTL),
        }
    }
}

impl TokenLifetimePolicy {
    /// The expiry a new `role` token created at `now` gets by default.
    ///
    /// Admin tokens are exempt: once every Admin token had expired, nobody
    /// could create another, and the bootstrap window wouldn't reopen
    /// because the store isn't empty. `relish wtf` warns about old ones
    /// instead.
    pub fn default_expiry(&self, role: ApiRole, now: SystemTime) -> Option<SystemTime> {
        if role == ApiRole::Admin {
            return None;
        }
        self.default_ttl.and_then(|ttl| now.checked_add(ttl))
    }
}

/// Which of a stored token's secrets a bearer matched.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatchedSecret {
    /// The token's current secret.
    Current,
    /// The secret it had before its last rotation, inside its grace period.
    Previous,
}

/// Validate a plaintext token against a stored `ApiToken` now.
///
/// See [`validate_token_at`].
pub fn validate_token(plaintext: &str, stored: &ApiToken) -> Result<MatchedSecret, TokenError> {
    validate_token_at(plaintext, stored, SystemTime::now())
}

/// Validate a plaintext token against a stored `ApiToken` at `now`.
///
/// Checks the Argon2id hash of the current secret, then of the previous
/// one, and then whether the secret that matched still works: the token's
/// expiry for the current secret, the end of the grace period for the
/// previous one. Does not check scope — that's the caller's responsibility
/// based on the specific operation.
pub fn validate_token_at(
    plaintext: &str,
    stored: &ApiToken,
    now: SystemTime,
) -> Result<MatchedSecret, TokenError> {
    if hash_matches(plaintext, &stored.token_hash) {
        if stored.expires_at.is_some_and(|at| now > at) {
            return Err(TokenError::Expired);
        }
        return Ok(MatchedSecret::Current);
    }
    let Some(previous) = &stored.previous_secret else {
        return Err(TokenError::ValidationFailed);
    };
    if !hash_matches(plaintext, &previous.token_hash) {
        return Err(TokenError::ValidationFailed);
    }
    if now >= previous.valid_until {
        return Err(TokenError::Rotated);
    }
    Ok(MatchedSecret::Previous)
}

/// Whether `plaintext` is the secret behind the stored Argon2id `hash`.
fn hash_matches(plaintext: &str, hash: &[u8]) -> bool {
    let Ok(hash_str) = std::str::from_utf8(hash) else {
        return false;
    };
    let Ok(parsed_hash) = PasswordHash::new(hash_str) else {
        return false;
    };
    Argon2::default()
        .verify_password(plaintext.as_bytes(), &parsed_hash)
        .is_ok()
}

/// Check that a token's role is sufficient for a required role.
///
/// Admin > Deployer > ReadOnly.
pub fn check_role(token_role: ApiRole, required: ApiRole) -> Result<(), TokenError> {
    let level = |r: ApiRole| -> u8 {
        match r {
            ApiRole::Admin => 3,
            ApiRole::Deployer => 2,
            ApiRole::ReadOnly => 1,
        }
    };

    if level(token_role) >= level(required) {
        Ok(())
    } else {
        Err(TokenError::InsufficientRole {
            required: required.to_string(),
        })
    }
}

/// Find the stored token whose secret `plaintext` is.
///
/// Returns the token and which of its secrets matched. A secret that
/// matches but no longer works (an expired token, or an old secret past its
/// grace period) is reported as such, so the caller can say why.
pub fn find_valid_token<'a>(
    plaintext: &str,
    tokens: &'a [ApiToken],
) -> Result<(&'a ApiToken, MatchedSecret), TokenError> {
    for token in tokens {
        match validate_token(plaintext, token) {
            Ok(matched) => return Ok((token, matched)),
            Err(TokenError::ValidationFailed) => continue,
            Err(error) => return Err(error),
        }
    }
    Err(TokenError::ValidationFailed)
}

/// How long a token stays in the store after it expires, before the expiry
/// sweep removes it. An expired token already gets 401; the grace keeps it
/// visible in `relish token list` for a day, so an operator can see why a
/// client started failing.
pub const EXPIRED_TOKEN_GRACE: Duration = Duration::from_secs(24 * 60 * 60);

/// Names of the tokens the expiry sweep removes at `now_unix_ms`, in store
/// order.
///
/// A token is due once `now_unix_ms` is past its expiry plus
/// [`EXPIRED_TOKEN_GRACE`]; a token without an expiry never is. Two rules
/// override that, because an empty store reopens the API to everyone (the
/// bootstrap window in [`super::auth::auth_middleware`]):
///
/// - the store never loses its last Admin. If every Admin is due, the one
///   that expired most recently stays;
/// - the store is never emptied. With no Admin at all, the token that
///   expired most recently stays.
///
/// Ties go to the greater name. The answer depends only on the tokens and
/// `now_unix_ms`, so every Raft replica applying the same entry removes the
/// same tokens.
pub fn tokens_to_sweep(tokens: &[ApiToken], now_unix_ms: u64) -> Vec<String> {
    let grace_ms = EXPIRED_TOKEN_GRACE.as_millis() as u64;
    let is_due = |token: &ApiToken| {
        token
            .expires_at
            .is_some_and(|at| unix_millis(at).saturating_add(grace_ms) < now_unix_ms)
    };
    // The survivor among `candidates`: the latest expiry, then the greater name.
    let latest = |candidates: Vec<&ApiToken>| {
        candidates
            .into_iter()
            .max_by_key(|token| (token.expires_at.map(unix_millis), token.name.clone()))
            .map(|token| token.name.clone())
    };

    let due: Vec<&ApiToken> = tokens.iter().filter(|token| is_due(token)).collect();
    let admins = tokens.iter().filter(|t| t.role == ApiRole::Admin).count();
    let due_admins: Vec<&ApiToken> = due
        .iter()
        .copied()
        .filter(|t| t.role == ApiRole::Admin)
        .collect();
    let keep = if admins > 0 && due_admins.len() == admins {
        latest(due_admins)
    } else if !due.is_empty() && due.len() == tokens.len() {
        latest(due.clone())
    } else {
        None
    };

    due.into_iter()
        .filter(|token| Some(&token.name) != keep.as_ref())
        .map(|token| token.name.clone())
        .collect()
}

/// Milliseconds since the Unix epoch; zero for a time before it.
fn unix_millis(at: SystemTime) -> u64 {
    at.duration_since(SystemTime::UNIX_EPOCH)
        .map(|elapsed| u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}

/// Length of the hex body of a well-formed token (`rbrg_` + 64 hex chars).
const TOKEN_HEX_LEN: usize = 64;

/// Cheap, non-secret shape check on a bearer before we hash it.
///
/// Every token we mint is `rbrg_` followed by 64 lower-case hex characters
/// (see [`create_token`]). A bearer that doesn't fit that shape can't match
/// any stored hash, so we can reject it without running Argon2 even once.
/// This is the short-circuit "index" for AUTH5: a burst of junk bearers is
/// turned away by a string check, not by an Argon2 hash per stored token.
///
/// The check reads only the untrusted input's shape, never a secret, so it
/// leaks nothing an attacker doesn't already control.
pub fn looks_like_token(candidate: &str) -> bool {
    let Some(hex) = candidate.strip_prefix("rbrg_") else {
        return false;
    };
    hex.len() == TOKEN_HEX_LEN && hex.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Fixed HKDF salt for the internal service token. The salt may be public;
/// the security comes from the master key (`ikm`) staying secret.
const SERVICE_TOKEN_SALT: [u8; 32] = *b"reliaburger-service-token-salt!!";

/// Derive the cluster's internal service token from the master key.
///
/// Deterministic: every node derives the same token from the same `ikm` (the
/// shared master secret loaded from `master.key`), so one node's cross-node
/// fan-out call authenticates on any other node. It is never stored in the
/// `SecurityState` — it's a side-channel credential the middleware accepts
/// directly, so it doesn't count towards the "any user tokens?" check that
/// gates enforcement.
pub fn derive_service_token(ikm: &[u8; 32]) -> Result<String, TokenError> {
    let bytes =
        super::crypto::hkdf_derive_key(ikm, &SERVICE_TOKEN_SALT, "reliaburger-service-token-v1")
            .map_err(|e| TokenError::GenerationFailed(e.to_string()))?;
    Ok(format!("rbrg_{}", hex::encode(bytes)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn service_token_is_deterministic_for_the_same_ikm() {
        let ikm = [9u8; 32];
        assert_eq!(
            derive_service_token(&ikm).unwrap(),
            derive_service_token(&ikm).unwrap()
        );
    }

    #[test]
    fn service_token_differs_for_different_ikm() {
        assert_ne!(
            derive_service_token(&[1u8; 32]).unwrap(),
            derive_service_token(&[2u8; 32]).unwrap()
        );
    }

    #[test]
    fn service_token_carries_the_rbrg_prefix() {
        assert!(
            derive_service_token(&[7u8; 32])
                .unwrap()
                .starts_with("rbrg_")
        );
    }

    #[test]
    fn create_token_rejects_the_reserved_system_name() {
        let result = create_token(
            super::super::auth::SYSTEM_PRINCIPAL,
            ApiRole::Admin,
            TokenScope::default(),
            None,
        );
        assert!(matches!(result, Err(TokenError::ReservedName(_))));
    }

    #[test]
    fn create_token_produces_unique_values() {
        let t1 = create_token("t1", ApiRole::Admin, TokenScope::default(), None).unwrap();
        let t2 = create_token("t2", ApiRole::Admin, TokenScope::default(), None).unwrap();
        assert_ne!(t1.plaintext, t2.plaintext);
        assert!(t1.plaintext.starts_with("rbrg_"));
        assert!(t2.plaintext.starts_with("rbrg_"));
    }

    #[test]
    fn argon2_hash_verify_round_trip() {
        let created = create_token("test", ApiRole::Deployer, TokenScope::default(), None).unwrap();
        validate_token(&created.plaintext, &created.token).unwrap();
    }

    #[test]
    fn wrong_token_fails_validation() {
        let created = create_token("test", ApiRole::Admin, TokenScope::default(), None).unwrap();
        let result = validate_token("rbrg_wrong_token_value", &created.token);
        assert!(result.is_err());
    }

    #[test]
    fn expired_token_fails_validation() {
        let expires = SystemTime::now() - Duration::from_secs(60);
        let created =
            create_token("test", ApiRole::Admin, TokenScope::default(), Some(expires)).unwrap();
        let result = validate_token(&created.plaintext, &created.token);
        assert!(matches!(result, Err(TokenError::Expired)));
    }

    #[test]
    fn check_role_admin_covers_all() {
        check_role(ApiRole::Admin, ApiRole::Admin).unwrap();
        check_role(ApiRole::Admin, ApiRole::Deployer).unwrap();
        check_role(ApiRole::Admin, ApiRole::ReadOnly).unwrap();
    }

    #[test]
    fn check_role_deployer_limited() {
        check_role(ApiRole::Deployer, ApiRole::Deployer).unwrap();
        check_role(ApiRole::Deployer, ApiRole::ReadOnly).unwrap();
        assert!(check_role(ApiRole::Deployer, ApiRole::Admin).is_err());
    }

    #[test]
    fn check_role_readonly_most_limited() {
        check_role(ApiRole::ReadOnly, ApiRole::ReadOnly).unwrap();
        assert!(check_role(ApiRole::ReadOnly, ApiRole::Deployer).is_err());
        assert!(check_role(ApiRole::ReadOnly, ApiRole::Admin).is_err());
    }

    #[test]
    fn find_valid_token_from_list() {
        let t1 = create_token("t1", ApiRole::Admin, TokenScope::default(), None).unwrap();
        let t2 = create_token("t2", ApiRole::Deployer, TokenScope::default(), None).unwrap();

        let tokens = vec![t1.token.clone(), t2.token.clone()];
        let (found, matched) = find_valid_token(&t2.plaintext, &tokens).unwrap();
        assert_eq!(matched, MatchedSecret::Current);
        assert_eq!(found.name, "t2");
        assert_eq!(found.role, ApiRole::Deployer);
    }

    #[test]
    fn find_valid_token_none_match() {
        let t1 = create_token("t1", ApiRole::Admin, TokenScope::default(), None).unwrap();
        let tokens = vec![t1.token];
        let result = find_valid_token("rbrg_doesnotexist", &tokens);
        assert!(result.is_err());
    }

    #[test]
    fn looks_like_token_accepts_a_minted_token() {
        let created = create_token("t", ApiRole::ReadOnly, TokenScope::default(), None).unwrap();
        assert!(looks_like_token(&created.plaintext));
    }

    #[test]
    fn looks_like_token_rejects_junk() {
        assert!(!looks_like_token(""));
        assert!(!looks_like_token("rbrg_"));
        assert!(!looks_like_token("nope"));
        // Right prefix, wrong length.
        assert!(!looks_like_token("rbrg_abc"));
        // Right length, non-hex character.
        assert!(!looks_like_token(&format!("rbrg_{}", "z".repeat(64))));
        // No prefix.
        assert!(!looks_like_token(&"a".repeat(64)));
    }

    /// A token with a fixed expiry, `expires_ms` after the epoch.
    fn expiring(name: &str, role: ApiRole, expires_ms: Option<u64>) -> ApiToken {
        ApiToken {
            name: name.to_string(),
            token_hash: name.as_bytes().to_vec(),
            token_salt: Vec::new(),
            role,
            scope: TokenScope::default(),
            expires_at: expires_ms.map(|ms| SystemTime::UNIX_EPOCH + Duration::from_millis(ms)),
            created_at: SystemTime::UNIX_EPOCH,
            previous_secret: None,
        }
    }

    const DAY_MS: u64 = 24 * 60 * 60 * 1000;
    const NOW_MS: u64 = 1_000 * DAY_MS;

    #[test]
    fn sweep_removes_a_token_expired_for_longer_than_the_grace() {
        let tokens = [
            expiring("admin", ApiRole::Admin, None),
            expiring("ci", ApiRole::Deployer, Some(NOW_MS - DAY_MS - 1)),
        ];
        assert_eq!(tokens_to_sweep(&tokens, NOW_MS), ["ci"]);
    }

    #[test]
    fn sweep_keeps_a_token_still_inside_the_grace() {
        let tokens = [
            expiring("admin", ApiRole::Admin, None),
            expiring("ci", ApiRole::Deployer, Some(NOW_MS - 60 * 60 * 1000)),
            expiring("edge", ApiRole::ReadOnly, Some(NOW_MS - DAY_MS)),
        ];
        assert!(tokens_to_sweep(&tokens, NOW_MS).is_empty());
    }

    #[test]
    fn sweep_never_removes_a_token_without_an_expiry() {
        let tokens = [
            expiring("admin", ApiRole::Admin, None),
            expiring("reader", ApiRole::ReadOnly, None),
        ];
        assert!(tokens_to_sweep(&tokens, u64::MAX).is_empty());
    }

    #[test]
    fn sweep_keeps_the_last_admin_even_when_it_has_expired() {
        let tokens = [
            expiring("admin", ApiRole::Admin, Some(NOW_MS - 10 * DAY_MS)),
            expiring("ci", ApiRole::Deployer, None),
        ];
        assert!(tokens_to_sweep(&tokens, NOW_MS).is_empty());
    }

    #[test]
    fn sweep_removes_an_expired_admin_while_a_live_admin_remains() {
        let tokens = [
            expiring("old-admin", ApiRole::Admin, Some(NOW_MS - 10 * DAY_MS)),
            expiring("admin", ApiRole::Admin, Some(NOW_MS + DAY_MS)),
        ];
        assert_eq!(tokens_to_sweep(&tokens, NOW_MS), ["old-admin"]);
    }

    #[test]
    fn sweep_keeps_the_most_recently_expiring_admin_when_every_admin_has_expired() {
        let tokens = [
            expiring("first", ApiRole::Admin, Some(NOW_MS - 30 * DAY_MS)),
            expiring("latest", ApiRole::Admin, Some(NOW_MS - 5 * DAY_MS)),
            expiring("middle", ApiRole::Admin, Some(NOW_MS - 10 * DAY_MS)),
            expiring("ci", ApiRole::Deployer, Some(NOW_MS - 10 * DAY_MS)),
        ];
        assert_eq!(tokens_to_sweep(&tokens, NOW_MS), ["first", "middle", "ci"]);
    }

    #[test]
    fn sweep_never_empties_a_store_without_an_admin() {
        let tokens = [
            expiring("old", ApiRole::Deployer, Some(NOW_MS - 30 * DAY_MS)),
            expiring("newer", ApiRole::ReadOnly, Some(NOW_MS - 5 * DAY_MS)),
        ];
        assert_eq!(tokens_to_sweep(&tokens, NOW_MS), ["old"]);
    }

    // -- Rotation (F05 I3) --------------------------------------------------

    const HOUR: Duration = Duration::from_secs(60 * 60);

    /// `stored` rotated at `now` with `grace`, and the token after the
    /// rotation is applied, as the state machine would.
    fn rotated(stored: &ApiToken, now: SystemTime, grace: Duration) -> (String, ApiToken) {
        let rotation = rotate_token(stored, now, grace).unwrap();
        let mut token = stored.clone();
        apply_rotation(&mut token, &rotation.rotation);
        (rotation.plaintext, token)
    }

    #[test]
    fn a_rotated_tokens_new_secret_works_at_once() {
        let created = create_token("ci", ApiRole::Deployer, TokenScope::default(), None).unwrap();
        let now = SystemTime::now();
        let (new_secret, token) = rotated(&created.token, now, DEFAULT_ROTATION_GRACE);
        assert_ne!(new_secret, created.plaintext);
        assert_eq!(
            validate_token_at(&new_secret, &token, now).unwrap(),
            MatchedSecret::Current
        );
    }

    #[test]
    fn the_old_secret_works_inside_the_grace_period_and_stops_after_it() {
        let created = create_token("ci", ApiRole::Deployer, TokenScope::default(), None).unwrap();
        let now = SystemTime::now();
        let (_, token) = rotated(&created.token, now, DEFAULT_ROTATION_GRACE);
        let inside = now + DEFAULT_ROTATION_GRACE - Duration::from_secs(1);
        assert_eq!(
            validate_token_at(&created.plaintext, &token, inside).unwrap(),
            MatchedSecret::Previous
        );
        let after = now + DEFAULT_ROTATION_GRACE;
        assert!(matches!(
            validate_token_at(&created.plaintext, &token, after),
            Err(TokenError::Rotated)
        ));
    }

    #[test]
    fn a_rotation_without_grace_drops_the_old_secret_at_once() {
        let created = create_token("ci", ApiRole::Deployer, TokenScope::default(), None).unwrap();
        let now = SystemTime::now();
        let (_, token) = rotated(&created.token, now, Duration::ZERO);
        assert_eq!(token.previous_secret, None);
        assert!(validate_token_at(&created.plaintext, &token, now).is_err());
    }

    #[test]
    fn a_second_rotation_ends_the_first_old_secret() {
        let created = create_token("ci", ApiRole::Deployer, TokenScope::default(), None).unwrap();
        let now = SystemTime::now();
        let (second, token) = rotated(&created.token, now, DEFAULT_ROTATION_GRACE);
        let (_, token) = rotated(&token, now + HOUR, DEFAULT_ROTATION_GRACE);
        assert!(validate_token_at(&created.plaintext, &token, now + HOUR).is_err());
        assert_eq!(
            validate_token_at(&second, &token, now + HOUR).unwrap(),
            MatchedSecret::Previous
        );
    }

    #[test]
    fn rotation_keeps_the_name_role_scope_and_lifetime_length() {
        let created_at = SystemTime::UNIX_EPOCH + 1_000 * DAY;
        let mut stored = create_token(
            "ci",
            ApiRole::Deployer,
            TokenScope {
                apps: Some(vec!["web".into()]),
                namespaces: Some(vec!["shop".into()]),
            },
            Some(created_at + 30 * DAY),
        )
        .unwrap()
        .token;
        stored.created_at = created_at;
        let now = created_at + 10 * DAY;
        let (_, token) = rotated(&stored, now, DEFAULT_ROTATION_GRACE);
        assert_eq!(token.name, "ci");
        assert_eq!(token.role, ApiRole::Deployer);
        assert_eq!(token.scope, stored.scope);
        assert_eq!(token.created_at, now);
        assert_eq!(token.expires_at, Some(now + 30 * DAY));
    }

    #[test]
    fn a_token_without_an_expiry_stays_without_one_after_rotation() {
        let created = create_token("admin", ApiRole::Admin, TokenScope::default(), None).unwrap();
        let (_, token) = rotated(&created.token, SystemTime::now(), DEFAULT_ROTATION_GRACE);
        assert_eq!(token.expires_at, None);
    }

    #[test]
    fn the_old_secret_never_outlives_the_old_expiry() {
        let now = SystemTime::now();
        let created = create_token(
            "ci",
            ApiRole::Deployer,
            TokenScope::default(),
            Some(now + HOUR),
        )
        .unwrap();
        let (_, token) = rotated(&created.token, now, DEFAULT_ROTATION_GRACE);
        assert_eq!(
            token.previous_secret.map(|previous| previous.valid_until),
            Some(now + HOUR)
        );
    }

    const DAY: Duration = Duration::from_secs(24 * 60 * 60);

    #[test]
    fn the_default_lifetime_applies_to_deployer_and_read_only_tokens() {
        let policy = TokenLifetimePolicy::default();
        let now = SystemTime::UNIX_EPOCH + 1_000 * DAY;
        for role in [ApiRole::Deployer, ApiRole::ReadOnly] {
            assert_eq!(policy.default_expiry(role, now), Some(now + 90 * DAY));
        }
    }

    #[test]
    fn admin_tokens_are_exempt_from_the_default_lifetime() {
        let now = SystemTime::UNIX_EPOCH + 1_000 * DAY;
        assert_eq!(
            TokenLifetimePolicy::default().default_expiry(ApiRole::Admin, now),
            None
        );
    }

    #[test]
    fn a_policy_without_a_default_lifetime_leaves_tokens_unbounded() {
        let policy = TokenLifetimePolicy { default_ttl: None };
        let now = SystemTime::UNIX_EPOCH;
        assert_eq!(policy.default_expiry(ApiRole::ReadOnly, now), None);
    }

    #[test]
    fn sweep_breaks_an_expiry_tie_by_name_so_every_replica_agrees() {
        let tokens = [
            expiring("b", ApiRole::Admin, Some(NOW_MS - 5 * DAY_MS)),
            expiring("a", ApiRole::Admin, Some(NOW_MS - 5 * DAY_MS)),
        ];
        let mut reversed = tokens.clone();
        reversed.reverse();
        assert_eq!(tokens_to_sweep(&tokens, NOW_MS), ["a"]);
        assert_eq!(tokens_to_sweep(&reversed, NOW_MS), ["a"]);
    }
}
