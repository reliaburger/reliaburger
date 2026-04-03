//! API token generation, hashing, and validation.
//!
//! Tokens are 256-bit random values, hashed with Argon2id before storage.
//! Each token has a role (Admin, Deployer, ReadOnly) and optional scope.

use std::time::SystemTime;

use argon2::Argon2;
use argon2::password_hash::rand_core::OsRng;
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use ring::rand::{SecureRandom, SystemRandom};

use super::types::{ApiRole, ApiToken, TokenScope};

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
    #[error("insufficient permissions: requires {required} role")]
    InsufficientRole { required: String },
    #[error("token scope does not allow this operation")]
    OutOfScope,
}

/// The result of creating a new API token.
pub struct CreatedToken {
    /// The plaintext token to return to the user (shown once, never stored).
    pub plaintext: String,
    /// The token struct for Raft storage (contains hash, not plaintext).
    pub token: ApiToken,
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
    let rng = SystemRandom::new();
    let mut token_bytes = [0u8; 32];
    rng.fill(&mut token_bytes)
        .map_err(|_| TokenError::GenerationFailed("RNG failed".to_string()))?;

    let plaintext = format!("rbrg_{}", hex::encode(token_bytes));

    // Hash with Argon2id
    let salt = SaltString::generate(&mut OsRng);
    let argon2 = Argon2::default();
    let password_hash = argon2
        .hash_password(plaintext.as_bytes(), &salt)
        .map_err(|e| TokenError::HashFailed(e.to_string()))?;

    let hash_string = password_hash.to_string();
    let token_hash = hash_string.as_bytes().to_vec();
    let token_salt = salt.as_str().as_bytes().to_vec();

    let token = ApiToken {
        name: name.to_string(),
        token_hash,
        token_salt,
        role,
        scope,
        expires_at,
        created_at: SystemTime::now(),
    };

    Ok(CreatedToken { plaintext, token })
}

/// Validate a plaintext token against a stored `ApiToken`.
///
/// Checks the Argon2id hash and expiry. Does not check scope — that's
/// the caller's responsibility based on the specific operation.
pub fn validate_token(plaintext: &str, stored: &ApiToken) -> Result<(), TokenError> {
    // Check expiry first (cheap)
    if let Some(expires_at) = stored.expires_at
        && SystemTime::now() > expires_at
    {
        return Err(TokenError::Expired);
    }

    // Verify Argon2id hash
    let hash_str =
        String::from_utf8(stored.token_hash.clone()).map_err(|_| TokenError::ValidationFailed)?;
    let parsed_hash = PasswordHash::new(&hash_str).map_err(|_| TokenError::ValidationFailed)?;

    Argon2::default()
        .verify_password(plaintext.as_bytes(), &parsed_hash)
        .map_err(|_| TokenError::ValidationFailed)?;

    Ok(())
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

/// Find a matching token from a list of stored tokens.
///
/// Returns a reference to the matching `ApiToken` if the plaintext
/// matches any stored hash and the token is not expired.
pub fn find_valid_token<'a>(
    plaintext: &str,
    tokens: &'a [ApiToken],
) -> Result<&'a ApiToken, TokenError> {
    for token in tokens {
        if validate_token(plaintext, token).is_ok() {
            return Ok(token);
        }
    }
    Err(TokenError::ValidationFailed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

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
        let found = find_valid_token(&t2.plaintext, &tokens).unwrap();
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
}
