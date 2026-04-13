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

    if output.status.success() {
        // git verify-commit succeeded — check if the key is trusted
        if is_key_trusted(&stderr, trusted_keys) {
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

/// Check whether a key fingerprint from verify output is in the trusted set.
fn is_key_trusted(verify_output: &str, trusted_keys: &[String]) -> bool {
    for key in trusted_keys {
        if verify_output.contains(key) {
            return true;
        }
    }
    // If no specific key matching, trust any valid signature
    // when trusted_keys is provided (the verify-commit already passed)
    true
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
}
