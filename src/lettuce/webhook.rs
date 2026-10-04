//! Webhook receiver for Lettuce GitOps.
//!
//! Validates incoming webhook payloads from git hosting providers
//! (GitHub, GitLab, Gitea) using HMAC-SHA256 signatures. Rate-limited
//! with replay detection.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use ring::hmac;

use super::types::{LettuceError, WebhookEvent};

/// A source of the current time, injectable so tests can move the
/// rate-limit window without sleeping.
pub type Clock = Box<dyn Fn() -> Instant + Send>;

/// How long a delivery counts against the rate budget.
const RATE_WINDOW: Duration = Duration::from_secs(60);

/// Validates and rate-limits incoming webhooks.
pub struct WebhookValidator {
    /// HMAC key for signature validation.
    secret: Vec<u8>,
    /// Maximum triggers per minute.
    rate_limit: u32,
    /// Recent delivery IDs for replay detection.
    recent_ids: VecDeque<String>,
    /// Max entries in the replay detection set.
    max_replay_entries: usize,
    /// Timestamps of recent triggers for rate limiting.
    recent_triggers: VecDeque<Instant>,
    /// Where "now" comes from for the rate window.
    clock: Clock,
}

/// An authenticated reservation is committed only after durable admission.
/// Dropping a canceled or failed request restores the bounded local budgets.
pub struct WebhookAdmission<'a> {
    validator: &'a mut WebhookValidator,
    ids: VecDeque<String>,
    triggers: VecDeque<Instant>,
    committed: bool,
}

impl WebhookAdmission<'_> {
    /// Retain the local replay and rate reservations after successful admission.
    pub fn commit(mut self) {
        self.committed = true;
    }
}

impl Drop for WebhookAdmission<'_> {
    fn drop(&mut self) {
        if !self.committed {
            self.validator.recent_ids = std::mem::take(&mut self.ids);
            self.validator.recent_triggers = std::mem::take(&mut self.triggers);
        }
    }
}

impl WebhookValidator {
    /// Create a new validator with the given HMAC secret and rate limit.
    pub fn new(secret: &str, rate_limit: u32) -> Self {
        Self::with_clock(secret, rate_limit, Box::new(Instant::now))
    }

    /// Create a validator that reads the time from `clock`.
    pub fn with_clock(secret: &str, rate_limit: u32, clock: Clock) -> Self {
        Self {
            secret: secret.as_bytes().to_vec(),
            rate_limit,
            recent_ids: VecDeque::with_capacity(1000),
            max_replay_entries: 1000,
            recent_triggers: VecDeque::with_capacity(rate_limit as usize),
            clock,
        }
    }

    /// Authenticate and reserve local budgets until admission commits or drops.
    pub fn reserve<'a>(
        &'a mut self,
        body: &[u8],
        signature: Option<&str>,
        gitlab_token: Option<&str>,
        delivery: Option<&str>,
        branch: &str,
    ) -> Result<WebhookAdmission<'a>, LettuceError> {
        let ids = self.recent_ids.clone();
        let triggers = self.recent_triggers.clone();
        if let Some(token) = gitlab_token {
            self.validate_gitlab(body, token, delivery, branch)?;
        } else {
            self.validate(body, signature, delivery, branch)?;
        }
        Ok(WebhookAdmission {
            validator: self,
            ids,
            triggers,
            committed: false,
        })
    }

    /// Validate a webhook request.
    ///
    /// Checks the HMAC signature, rate limit, and replay detection.
    /// Returns a `WebhookEvent` on success.
    pub fn validate(
        &mut self,
        body: &[u8],
        signature_header: Option<&str>,
        delivery_id: Option<&str>,
        branch: &str,
    ) -> Result<WebhookEvent, LettuceError> {
        // HMAC validation
        self.verify_signature(body, signature_header)?;

        // Replay + rate limit.
        self.check_replay_and_rate(delivery_id)?;

        // Extract commit SHA from webhook body (simplified — real impl
        // would parse the JSON from GitHub/GitLab/Gitea format)
        let commit_sha = extract_head_commit(body).unwrap_or_default();

        Ok(WebhookEvent {
            branch: branch.to_string(),
            commit_sha,
            delivery_id: delivery_id.map(String::from),
        })
    }

    /// Validate a GitLab webhook, whose `X-Gitlab-Token` carries the
    /// shared secret verbatim rather than an HMAC signature.
    ///
    /// The token is compared to the configured secret in constant time,
    /// then replay and rate-limit checks run exactly as for GitHub.
    pub fn validate_gitlab(
        &mut self,
        body: &[u8],
        token: &str,
        delivery_id: Option<&str>,
        branch: &str,
    ) -> Result<WebhookEvent, LettuceError> {
        if !constant_time_eq(token.as_bytes(), &self.secret) {
            return Err(LettuceError::WebhookInvalid("token mismatch".to_string()));
        }
        self.check_replay_and_rate(delivery_id)?;

        let commit_sha = extract_head_commit(body).unwrap_or_default();
        Ok(WebhookEvent {
            branch: branch.to_string(),
            commit_sha,
            delivery_id: delivery_id.map(String::from),
        })
    }

    /// Run the replay and rate-limit checks shared by every provider.
    fn check_replay_and_rate(&mut self, delivery_id: Option<&str>) -> Result<(), LettuceError> {
        // A delivery ID is mandatory (M27): the endpoint is secret-authenticated,
        // and without an id there is nothing to deduplicate against, so a
        // captured request could be replayed indefinitely. Refuse rather than
        // silently skip replay protection.
        let id = delivery_id.ok_or_else(|| {
            LettuceError::WebhookInvalid(
                "missing delivery ID; replay protection requires one".to_string(),
            )
        })?;
        if self.recent_ids.iter().any(|existing| existing == id) {
            return Err(LettuceError::WebhookInvalid(
                "duplicate delivery ID (replay)".to_string(),
            ));
        }

        let now = (self.clock)();
        while self
            .recent_triggers
            .front()
            .is_some_and(|t| now.duration_since(*t) > RATE_WINDOW)
        {
            self.recent_triggers.pop_front();
        }
        if self.recent_triggers.len() >= self.rate_limit as usize {
            return Err(LettuceError::WebhookInvalid(format!(
                "rate limit exceeded ({}/min)",
                self.rate_limit
            )));
        }

        // Admission is all or nothing (B10). The delivery ID used to be
        // recorded before the rate check, so a delivery refused for being
        // over budget was already "seen", and the provider's retry after
        // the window was refused as a replay. Only an admitted delivery
        // consumes its ID and a slot in the window.
        self.recent_ids.push_back(id.to_string());
        if self.recent_ids.len() > self.max_replay_entries {
            self.recent_ids.pop_front();
        }
        self.recent_triggers.push_back(now);
        Ok(())
    }

    /// Verify the HMAC-SHA256 signature.
    fn verify_signature(
        &self,
        body: &[u8],
        signature_header: Option<&str>,
    ) -> Result<(), LettuceError> {
        let sig_hex = signature_header
            .and_then(|s| s.strip_prefix("sha256="))
            .ok_or_else(|| {
                LettuceError::WebhookInvalid("missing or invalid signature header".to_string())
            })?;

        let expected_sig = hex::decode(sig_hex)
            .map_err(|_| LettuceError::WebhookInvalid("invalid hex in signature".to_string()))?;

        let key = hmac::Key::new(hmac::HMAC_SHA256, &self.secret);
        hmac::verify(&key, body, &expected_sig)
            .map_err(|_| LettuceError::WebhookInvalid("signature mismatch".to_string()))
    }
}

/// Constant-time byte-slice equality for the GitLab shared-secret token.
///
/// A plain `==` short-circuits at the first differing byte, leaking how
/// many leading bytes matched — enough to forge a token byte by byte.
/// This folds every byte difference into one accumulator, so the running
/// time depends only on the length. (GitHub's HMAC path already gets this
/// for free from `ring::hmac::verify`.)
fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Extract the head commit SHA from a webhook JSON body.
/// Handles GitHub's `{"after": "sha"}` format.
fn extract_head_commit(body: &[u8]) -> Option<String> {
    let json: serde_json::Value = serde_json::from_slice(body).ok()?;
    json.get("after").and_then(|v| v.as_str()).map(String::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sign_payload(secret: &str, body: &[u8]) -> String {
        let key = hmac::Key::new(hmac::HMAC_SHA256, secret.as_bytes());
        let tag = hmac::sign(&key, body);
        format!("sha256={}", hex::encode(tag.as_ref()))
    }

    #[test]
    fn webhook_hmac_validation_success() {
        let mut validator = WebhookValidator::new("mysecret", 10);
        let body = br#"{"after": "abc123", "ref": "refs/heads/main"}"#;
        let sig = sign_payload("mysecret", body);

        let event = validator
            .validate(body, Some(&sig), Some("delivery-1"), "main")
            .unwrap();
        assert_eq!(event.commit_sha, "abc123");
        assert_eq!(event.branch, "main");
    }

    #[test]
    fn webhook_hmac_validation_failure() {
        let mut validator = WebhookValidator::new("mysecret", 10);
        let body = b"some payload";
        let result = validator.validate(body, Some("sha256=badbeef"), None, "main");
        assert!(result.is_err());
    }

    #[test]
    fn webhook_missing_signature_rejected() {
        let mut validator = WebhookValidator::new("mysecret", 10);
        let result = validator.validate(b"body", None, None, "main");
        assert!(result.is_err());
    }

    #[test]
    fn webhook_replay_detection() {
        let mut validator = WebhookValidator::new("mysecret", 10);
        let body = br#"{"after": "abc"}"#;
        let sig = sign_payload("mysecret", body);

        // First delivery succeeds
        validator
            .validate(body, Some(&sig), Some("id-1"), "main")
            .unwrap();

        // Same delivery ID rejected
        let result = validator.validate(body, Some(&sig), Some("id-1"), "main");
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("duplicate delivery ID")
        );
    }

    /// M27: a valid signature with no delivery ID is refused — replay
    /// protection is impossible without one, so we must not silently skip it.
    #[test]
    fn webhook_without_delivery_id_is_rejected() {
        let mut validator = WebhookValidator::new("mysecret", 10);
        let body = br#"{"after": "abc"}"#;
        let sig = sign_payload("mysecret", body);
        let err = validator
            .validate(body, Some(&sig), None, "main")
            .unwrap_err();
        assert!(err.to_string().contains("delivery ID"), "got: {err}");
    }

    #[test]
    fn webhook_rate_limiting() {
        let mut validator = WebhookValidator::new("mysecret", 2); // 2 per minute
        let body = br#"{"after": "abc"}"#;
        let sig = sign_payload("mysecret", body);

        // First two succeed
        validator
            .validate(body, Some(&sig), Some("id-1"), "main")
            .unwrap();
        validator
            .validate(body, Some(&sig), Some("id-2"), "main")
            .unwrap();

        // Third is rate-limited
        let result = validator.validate(body, Some(&sig), Some("id-3"), "main");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("rate limit"));
    }

    /// B10: a delivery refused for being over the rate budget was already
    /// recorded as seen, so the provider's retry after the window was
    /// refused as a replay. Only an admitted delivery may consume its ID.
    #[test]
    fn rate_limited_delivery_is_accepted_when_retried_after_the_window() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicU64, Ordering};

        let base = Instant::now();
        let elapsed_secs = Arc::new(AtomicU64::new(0));
        let clock_secs = Arc::clone(&elapsed_secs);
        let mut validator = WebhookValidator::with_clock(
            "mysecret",
            1,
            Box::new(move || base + Duration::from_secs(clock_secs.load(Ordering::SeqCst))),
        );
        let body = br#"{"after": "abc"}"#;
        let sig = sign_payload("mysecret", body);

        // Exhaust the budget, then get a fresh delivery refused.
        validator
            .validate(body, Some(&sig), Some("id-1"), "main")
            .unwrap();
        let limited = validator.validate(body, Some(&sig), Some("id-2"), "main");
        assert!(
            limited.unwrap_err().to_string().contains("rate limit"),
            "the second delivery must be rate limited"
        );

        // The provider retries id-2 once the window has passed.
        elapsed_secs.store(61, Ordering::SeqCst);
        validator
            .validate(body, Some(&sig), Some("id-2"), "main")
            .expect("a retried rate-limited delivery must be admitted");

        // A true replay of the admitted delivery is still refused.
        elapsed_secs.store(200, Ordering::SeqCst);
        let replay = validator.validate(body, Some(&sig), Some("id-2"), "main");
        assert!(replay.unwrap_err().to_string().contains("duplicate"));
    }

    #[test]
    fn gitlab_token_accepted_and_wrong_token_rejected() {
        let mut validator = WebhookValidator::new("shared-secret", 10);
        let body = br#"{"after": "abc"}"#;

        let event = validator
            .validate_gitlab(body, "shared-secret", Some("uuid-1"), "main")
            .unwrap();
        assert_eq!(event.commit_sha, "abc");

        let wrong = validator.validate_gitlab(body, "not-it", Some("uuid-2"), "main");
        assert!(wrong.is_err());
    }

    #[test]
    fn gitlab_replay_and_rate_limit_apply() {
        let mut validator = WebhookValidator::new("s", 1);
        let body = br#"{}"#;
        validator
            .validate_gitlab(body, "s", Some("d-1"), "main")
            .unwrap();
        // Same delivery id → replay.
        let replay = validator.validate_gitlab(body, "s", Some("d-1"), "main");
        assert!(replay.unwrap_err().to_string().contains("duplicate"));
        // Fresh id, but the single per-minute slot is used → rate limited.
        let flooded = validator.validate_gitlab(body, "s", Some("d-2"), "main");
        assert!(flooded.unwrap_err().to_string().contains("rate limit"));
    }

    #[test]
    fn constant_time_eq_matches_plain_equality() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
    }

    #[test]
    fn extract_head_commit_github_format() {
        let body = br#"{"after": "abc123def456"}"#;
        assert_eq!(extract_head_commit(body), Some("abc123def456".to_string()));
    }

    #[test]
    fn extract_head_commit_missing() {
        let body = br#"{"ref": "refs/heads/main"}"#;
        assert_eq!(extract_head_commit(body), None);
    }
    #[test]
    fn canceled_admission_restores_both_delivery_and_rate_reservations() {
        let body = b"{}";
        let signature = sign_payload("secret", body);
        let mut validator = WebhookValidator::new("secret", 1);
        drop(
            validator
                .reserve(body, Some(&signature), None, Some("retry"), "main")
                .unwrap(),
        );
        validator
            .reserve(body, Some(&signature), None, Some("retry"), "main")
            .unwrap()
            .commit();
        assert!(
            validator
                .validate(body, Some(&signature), Some("retry"), "main")
                .unwrap_err()
                .to_string()
                .contains("replay")
        );
        assert!(
            validator
                .validate(body, Some(&signature), Some("different"), "main")
                .unwrap_err()
                .to_string()
                .contains("rate limit")
        );
    }
}
