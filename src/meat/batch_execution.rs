//! Shared, canonical specification evidence for trusted batch ownership.

use sha2::{Digest, Sha256};

/// Bind the resolved namespace and submitted label to the complete job spec.
/// Runtime execution names are separate from this logical evidence.
pub(crate) fn spec_digest(
    namespace: &str,
    logical_name: &str,
    spec: &crate::config::job::JobSpec,
) -> Result<String, serde_json::Error> {
    let mut normalized = spec.clone();
    normalized.namespace = Some(namespace.to_string());
    Ok(format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(&(namespace, logical_name, normalized))?)
    ))
}

pub(crate) fn valid_digest(digest: &str) -> bool {
    digest.len() == 64
        && digest
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
