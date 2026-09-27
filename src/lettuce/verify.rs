//! Commit signature verification for Lettuce.
//!
//! Verifies GPG and SSH signatures by shelling out to git. This is
//! the pragmatic approach — avoids pulling in full PGP/SSH stacks
//! while still providing real verification.

use std::path::Path;
use std::process::Command;

use super::types::{CommitInfo, LettuceError, SignatureStatus};

/// Verify the signature on a commit.
///
/// Shells out to `git verify-commit` which handles both GPG and SSH
/// signatures. Returns the updated `SignatureStatus`.
pub fn verify_commit(
    repo_path: &Path,
    commit: &CommitInfo,
    trusted_keys: &[String],
) -> SignatureStatus {
    if trusted_keys.is_empty() {
        return SignatureStatus::NotChecked;
    }

    let output = Command::new("git")
        .args(["verify-commit", "--raw", &commit.sha])
        .current_dir(repo_path)
        .output();

    let output = match output {
        Ok(o) => o,
        Err(_) => return SignatureStatus::NotChecked,
    };

    let stderr = String::from_utf8_lossy(&output.stderr);
    classify_verification(output.status.success(), &stderr, trusted_keys)
}

/// Turn `git verify-commit --raw`'s exit status and stderr into a status.
///
/// A trusted fingerprint only counts when git itself accepted the signature:
/// a failed verification is never `Verified`, whatever its output says.
fn classify_verification(
    succeeded: bool,
    stderr: &str,
    trusted_keys: &[String],
) -> SignatureStatus {
    if succeeded {
        if is_key_trusted(stderr, trusted_keys) {
            SignatureStatus::Verified
        } else {
            SignatureStatus::UntrustedKey
        }
    } else if stderr.contains("no signature") || stderr.contains("Signature not found") {
        SignatureStatus::Unsigned
    } else {
        SignatureStatus::InvalidSignature
    }
}

/// Check whether a commit modifies any `script` field.
///
/// Parses the diff and looks for lines adding or changing a `script`
/// field in a TOML file.
pub fn commit_modifies_script(
    repo_path: &Path,
    sha: &str,
    parent_sha: Option<&str>,
) -> Result<bool, LettuceError> {
    let diff_args = match parent_sha {
        Some(parent) => vec!["diff", parent, sha, "--", "*.toml"],
        None => vec!["diff", "--root", sha, "--", "*.toml"],
    };

    let output = Command::new("git")
        .args(&diff_args)
        .current_dir(repo_path)
        .output()
        .map_err(|e| LettuceError::GitFailed(e.to_string()))?;

    let diff_text = String::from_utf8_lossy(&output.stdout);
    Ok(diff_text
        .lines()
        .any(|line| line.starts_with('+') && !line.starts_with("+++") && line.contains("script")))
}

/// Check whether the key that made a *valid* signature is in the trusted set.
///
/// `git verify-commit --raw` prints the verifier's machine-readable output on
/// stderr. For GPG that's the `--status-fd` protocol, where a good signature
/// produces
///
/// ```text
/// [GNUPG:] VALIDSIG <signing-fpr> <date> <ts> <expiry> <ver> <reserved>
///          <pk-algo> <hash-algo> <class> <primary-fpr>
/// ```
///
/// For SSH it's `ssh-keygen -Y verify`'s `Good "git" signature for <principal>
/// with <TYPE> key SHA256:<base64>` line. Only those fingerprint fields are
/// compared, and only for equality.
///
/// H12 regression: this used to `return true` after the loop when no trusted
/// key matched, so a validly-signed commit from ANY key was accepted.
///
/// B14 regression: it then matched each trusted key as a *substring* of the
/// whole output, so a user id, a date or a longer fingerprint that merely
/// contained the configured one satisfied it (an attacker's key sitting in
/// the node's ambient keyring qualifies). Now a GPG fingerprint (signing
/// subkey or primary key) must equal a trusted key, ignoring case and
/// spaces, and an SSH `SHA256:` fingerprint must equal one exactly (base64
/// is case-sensitive).
fn is_key_trusted(verify_output: &str, trusted_keys: &[String]) -> bool {
    let signers = valid_signer_fingerprints(verify_output);
    trusted_keys.iter().any(|trusted| {
        let trusted = trusted.trim();
        signers.iter().any(|signer| match signer {
            Signer::Gpg(fingerprint) => normalise_gpg(trusted) == normalise_gpg(fingerprint),
            Signer::Ssh(fingerprint) => trusted == *fingerprint,
        })
    })
}

/// A fingerprint the verifier reported against a valid signature.
enum Signer<'a> {
    /// A GPG key fingerprint (hex), from a `VALIDSIG` status line.
    Gpg(&'a str),
    /// An SSH key fingerprint, `SHA256:<base64>`.
    Ssh(&'a str),
}

/// Every fingerprint reported for a valid signature in `verify_output`.
fn valid_signer_fingerprints(verify_output: &str) -> Vec<Signer<'_>> {
    let mut signers = Vec::new();
    for line in verify_output.lines() {
        if let Some(fields) = line.trim().strip_prefix("[GNUPG:] VALIDSIG ") {
            let fields: Vec<&str> = fields.split_whitespace().collect();
            // Field 0 is the signing (sub)key, field 9 the primary key.
            signers.extend(fields.first().copied().map(Signer::Gpg));
            signers.extend(fields.get(9).copied().map(Signer::Gpg));
        } else if line.starts_with("Good \"git\" signature for ") {
            // The fingerprint is the last word; the principal before it is
            // free text and never compared.
            let key = line
                .split_whitespace()
                .last()
                .filter(|word| word.starts_with("SHA256:"));
            signers.extend(key.map(Signer::Ssh));
        }
    }
    signers
}

/// A GPG fingerprint in canonical form: no spaces, no `0x`, upper case.
fn normalise_gpg(fingerprint: &str) -> String {
    let fingerprint = fingerprint.trim();
    let fingerprint = fingerprint
        .strip_prefix("0x")
        .or_else(|| fingerprint.strip_prefix("0X"))
        .unwrap_or(fingerprint);
    fingerprint
        .chars()
        .filter(|c| !c.is_whitespace())
        .collect::<String>()
        .to_ascii_uppercase()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_trusted_keys_returns_not_checked() {
        let commit = CommitInfo {
            sha: "abc".to_string(),
            message: "test".to_string(),
            author: "dev".to_string(),
            timestamp: 0,
            signature: SignatureStatus::NotChecked,
        };
        let result = verify_commit(Path::new("/nonexistent"), &commit, &[]);
        assert_eq!(result, SignatureStatus::NotChecked);
    }

    const TRUSTED_FPR: &str = "0123456789ABCDEF0123456789ABCDEF01234567";
    const TRUSTED_SUBKEY_FPR: &str = "89ABCDEF0123456789ABCDEF0123456789ABCDEF";

    /// `git verify-commit --raw` output for a good GPG signature by
    /// `signing_fpr`, whose primary key is `primary_fpr`.
    fn gpg_good(signing_fpr: &str, primary_fpr: &str, uid: &str) -> String {
        format!(
            "[GNUPG:] NEWSIG\n\
             [GNUPG:] KEY_CONSIDERED {primary_fpr} 0\n\
             [GNUPG:] SIG_ID abc 2026-09-27 1790000000\n\
             [GNUPG:] GOODSIG {} {uid}\n\
             [GNUPG:] VALIDSIG {signing_fpr} 2026-09-27 1790000000 0 4 0 22 10 00 {primary_fpr}\n\
             [GNUPG:] TRUST_ULTIMATE 0 pgp\n",
            &signing_fpr[24..]
        )
    }

    fn trusted() -> Vec<String> {
        vec![TRUSTED_FPR.to_string()]
    }

    /// H12 regression: a valid signature from the trusted key is verified.
    #[test]
    fn valid_signature_from_trusted_key_is_verified() {
        let raw = gpg_good(TRUSTED_FPR, TRUSTED_FPR, "Dev <dev@example.com>");
        assert_eq!(
            classify_verification(true, &raw, &trusted()),
            SignatureStatus::Verified
        );
    }

    /// A trusted primary key admits a signature made by its signing subkey,
    /// and the configured fingerprint may use lower case, spaces or `0x`.
    #[test]
    fn trusted_primary_key_admits_its_subkey_ignoring_case_and_spaces() {
        let raw = gpg_good(TRUSTED_SUBKEY_FPR, TRUSTED_FPR, "Dev <dev@example.com>");
        let configured = vec!["0x0123 4567 89ab cdef 0123  4567 89ab cdef 0123 4567".to_string()];
        assert_eq!(
            classify_verification(true, &raw, &configured),
            SignatureStatus::Verified
        );
    }

    /// H12 regression: a valid signature from an UNLISTED key is NOT
    /// trusted (it used to be, via the fall-through `return true`).
    #[test]
    fn valid_signature_from_unlisted_key_is_untrusted() {
        let attacker = "DEADBEEFDEADBEEFDEADBEEFDEADBEEFDEADBEEF";
        let raw = gpg_good(attacker, attacker, "Mallory <mallory@evil.example>");
        assert_eq!(
            classify_verification(true, &raw, &trusted()),
            SignatureStatus::UntrustedKey
        );
    }

    /// B14 regression: the attacker's fingerprint *contains* the trusted
    /// one, and the attacker's user id quotes it too. The old substring
    /// match over the whole output accepted this.
    #[test]
    fn fingerprint_containing_the_trusted_key_is_untrusted() {
        let attacker = format!("FFFF{}", &TRUSTED_FPR[..36]);
        let short_trusted = vec![TRUSTED_FPR[..36].to_string()];
        let raw = gpg_good(&attacker, &attacker, &format!("Mallory ({TRUSTED_FPR})"));
        assert_eq!(
            classify_verification(true, &raw, &short_trusted),
            SignatureStatus::UntrustedKey
        );
        assert_eq!(
            classify_verification(true, &raw, &trusted()),
            SignatureStatus::UntrustedKey
        );
    }

    /// A bad signature is invalid even when the output names a trusted key.
    #[test]
    fn bad_signature_is_invalid_even_from_a_trusted_key() {
        let raw = format!(
            "[GNUPG:] NEWSIG\n\
             [GNUPG:] KEY_CONSIDERED {TRUSTED_FPR} 0\n\
             [GNUPG:] BADSIG 89ABCDEF01234567 Dev <dev@example.com>\n"
        );
        assert_eq!(
            classify_verification(false, &raw, &trusted()),
            SignatureStatus::InvalidSignature
        );
    }

    /// Without a `VALIDSIG` line nothing is trusted, even if git exited
    /// zero and the trusted fingerprint appears elsewhere in the output.
    #[test]
    fn output_without_validsig_is_untrusted() {
        let raw = format!(
            "[GNUPG:] KEY_CONSIDERED {TRUSTED_FPR} 0\n\
             [GNUPG:] GOODSIG 89ABCDEF01234567 Dev <dev@example.com>\n"
        );
        assert_eq!(
            classify_verification(true, &raw, &trusted()),
            SignatureStatus::UntrustedKey
        );
    }

    /// SSH signatures: the `SHA256:` fingerprint must match exactly.
    #[test]
    fn ssh_fingerprint_must_match_exactly() {
        let raw = "Good \"git\" signature for dev@example.com with ED25519 key \
                   SHA256:yu3tWf+N23rCf3/3KPnMs+nCYonE4xZLWOO7Ycab30s\n";
        let exact = vec!["SHA256:yu3tWf+N23rCf3/3KPnMs+nCYonE4xZLWOO7Ycab30s".to_string()];
        assert_eq!(
            classify_verification(true, raw, &exact),
            SignatureStatus::Verified
        );

        for near_miss in [
            "SHA256:yu3tWf+N23rCf3",
            "SHA256:YU3TWF+N23RCF3/3KPNMS+NCYONE4XZLWOO7YCAB30S",
            "dev@example.com",
        ] {
            assert_eq!(
                classify_verification(true, raw, &[near_miss.to_string()]),
                SignatureStatus::UntrustedKey,
                "{near_miss} must not match"
            );
        }
    }

    #[test]
    fn empty_trusted_set_trusts_nothing() {
        let raw = gpg_good(TRUSTED_FPR, TRUSTED_FPR, "Dev <dev@example.com>");
        assert!(!is_key_trusted(&raw, &[]));
    }
}
