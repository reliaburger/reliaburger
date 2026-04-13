//! Webhook receiver for Lettuce GitOps.
//!
//! Validates incoming webhook payloads from git hosting providers
//! (GitHub, GitLab, Gitea) using HMAC-SHA256 signatures. Rate-limited
//! with replay detection.

use std::collections::VecDeque;
use std::time::Instant;

use ring::hmac;

use super::types::{LettuceError, WebhookEvent};

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
}

impl WebhookValidator {
    /// Create a new validator with the given HMAC secret and rate limit.
    pub fn new(secret: &str, rate_limit: u32) -> Self {
        Self {
            secret: secret.as_bytes().to_vec(),
            rate_limit,
            recent_ids: VecDeque::with_capacity(1000),
            max_replay_entries: 1000,
            recent_triggers: VecDeque::with_capacity(rate_limit as usize),
        }
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

        // Replay detection
        if let Some(id) = delivery_id {
            if self.recent_ids.iter().any(|existing| existing == id) {
                return Err(LettuceError::WebhookInvalid(
                    "duplicate delivery ID (replay)".to_string(),
                ));
            }
            self.recent_ids.push_back(id.to_string());
            if self.recent_ids.len() > self.max_replay_entries {
                self.recent_ids.pop_front();
            }
        }

        // Rate limiting
        let now = Instant::now();
        let one_minute_ago = now - std::time::Duration::from_secs(60);
        while self
            .recent_triggers
            .front()
            .is_some_and(|t| *t < one_minute_ago)
        {
            self.recent_triggers.pop_front();
        }
        if self.recent_triggers.len() >= self.rate_limit as usize {
            return Err(LettuceError::WebhookInvalid(format!(
                "rate limit exceeded ({}/min)",
                self.rate_limit
            )));
        }
        self.recent_triggers.push_back(now);

        // Extract commit SHA from webhook body (simplified — real impl
        // would parse the JSON from GitHub/GitLab/Gitea format)
        let commit_sha = extract_head_commit(body).unwrap_or_default();

        Ok(WebhookEvent {
            branch: branch.to_string(),
            commit_sha,
            delivery_id: delivery_id.map(String::from),
        })
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
}
