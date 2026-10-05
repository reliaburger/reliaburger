//! Which images from outside Pickle a node will run (F03 U2, #361).
//!
//! `[[images.trust_policy.upstream]]` rules in `node.toml` name
//! repositories, exactly (`docker.io/library/nginx`) or by prefix
//! (`ghcr.io/acme/*`). The most specific rule matching an image applies;
//! an image no rule matches falls to `upstream_default.allow`, which is
//! `true` unless the operator turns the rules into an allow-list.
//!
//! The rules live in node config on purpose: nothing an API token can
//! change should decide what the cluster trusts. The node handling an apply
//! checks them first, for an early, readable refusal; Bun checks them again
//! before every deploy, which is the enforcement.

use crate::config::node::{TrustPolicySection, UpstreamTrustRule};
use crate::grill::image::ImageReference;

/// An upstream image the policy refuses.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error(
    "image {image} is not allowed on this node: no [[images.trust_policy.upstream]] rule matches {repository}, and upstream_default.allow is false"
)]
pub struct UpstreamRefused {
    /// The image as written.
    pub image: String,
    /// The repository the rules were matched against.
    pub repository: String,
}

/// The repository an image's rules match: registry and path, Docker Hub
/// shorthand spelled out, tag and digest dropped (`nginx:1.27` is
/// `docker.io/library/nginx`).
pub fn upstream_repository(image: &str) -> String {
    match ImageReference::parse(image) {
        Ok(reference) => format!("{}/{}", reference.registry, reference.repository),
        Err(_) => image.to_string(),
    }
}

/// The most specific rule matching `repository`, if any: an exact match
/// beats any prefix, and a longer prefix beats a shorter one.
pub fn matching_rule<'a>(
    policy: &'a TrustPolicySection,
    repository: &str,
) -> Option<&'a UpstreamTrustRule> {
    policy
        .upstream
        .iter()
        .filter_map(|rule| {
            let specificity = match rule.pattern.strip_suffix('*') {
                Some(prefix) => repository.starts_with(prefix).then_some((0, prefix.len())),
                None => (rule.pattern == repository).then_some((1, rule.pattern.len())),
            }?;
            Some((specificity, rule))
        })
        .max_by_key(|(specificity, _)| *specificity)
        .map(|(_, rule)| rule)
}

/// Check an image from outside Pickle against the upstream rules.
///
/// The caller decides what counts as upstream: an image Pickle's catalogue
/// holds (other than the pull-through cache's copies) never reaches here.
pub fn check_upstream(policy: &TrustPolicySection, image: &str) -> Result<(), UpstreamRefused> {
    let repository = upstream_repository(image);
    if policy.upstream_default.allow || matching_rule(policy, &repository).is_some() {
        return Ok(());
    }
    Err(UpstreamRefused {
        image: image.to_string(),
        repository,
    })
}

/// A cosign signature check an image owes before it deploys: the most
/// specific rule matching it says `require_signatures = true` (F03 U3).
///
/// It's a value rather than a call because the check reads the network
/// (the `.sig` image), and Bun runs it off its agent loop.
#[derive(Clone)]
pub struct CosignCheck {
    image: String,
    keys: Vec<String>,
    source: Option<super::cosign::SignatureSource>,
}

impl CosignCheck {
    /// The check `image` owes under `policy`, if any. `source` is where this
    /// node reads signatures from; without one the check refuses.
    pub fn for_image(
        policy: &TrustPolicySection,
        image: &str,
        source: Option<&super::cosign::SignatureSource>,
    ) -> Option<Self> {
        let rule = matching_rule(policy, &upstream_repository(image))?;
        rule.require_signatures.then(|| Self {
            image: image.to_string(),
            keys: rule.cosign_keys.clone(),
            source: source.cloned(),
        })
    }

    /// The image this check is for, as written.
    pub fn image(&self) -> &str {
        &self.image
    }

    /// Fetch the image's cosign signature and verify it over the digest the
    /// image is bound to. Every refusal names the image.
    pub async fn run(self) -> Result<(), String> {
        let image = &self.image;
        let refuse =
            |reason: String| format!("image {image} is not allowed on this node: {reason}");
        let reference = ImageReference::parse(image).map_err(|e| refuse(e.to_string()))?;
        let digest = reference
            .tag
            .starts_with("sha256:")
            .then(|| super::types::Digest::new(&reference.tag).ok())
            .flatten()
            .ok_or_else(|| {
                refuse(
                    "its rule requires a cosign signature, which covers a digest, and the image isn't bound to one; apply it again".into(),
                )
            })?;
        let Some(source) = &self.source else {
            return Err(refuse(
                "its rule requires a cosign signature and this node has no registry client to fetch one".into(),
            ));
        };
        let keys =
            super::cosign::CosignKey::parse_all(&self.keys).map_err(|e| refuse(e.to_string()))?;
        let payloads = source
            .payloads(&reference, &digest)
            .await
            .map_err(|e| refuse(e.to_string()))?;
        super::cosign::verify_signature(&digest, &payloads, &keys)
            .map_err(|e| refuse(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::node::UpstreamDefault;

    fn policy(patterns: &[&str], allow: bool) -> TrustPolicySection {
        TrustPolicySection {
            upstream: patterns
                .iter()
                .map(|pattern| UpstreamTrustRule {
                    pattern: pattern.to_string(),
                    require_signatures: false,
                    cosign_keys: vec![],
                })
                .collect(),
            upstream_default: UpstreamDefault { allow },
            ..Default::default()
        }
    }

    #[test]
    fn the_most_specific_rule_wins() {
        let policy = policy(
            &[
                "docker.io/*",
                "docker.io/library/*",
                "docker.io/library/nginx",
                "ghcr.io/acme/*",
            ],
            true,
        );
        for (repository, expected) in [
            ("docker.io/library/nginx", Some("docker.io/library/nginx")),
            ("docker.io/library/redis", Some("docker.io/library/*")),
            ("docker.io/bitnami/redis", Some("docker.io/*")),
            ("ghcr.io/acme/team/web", Some("ghcr.io/acme/*")),
            ("ghcr.io/other/web", None),
            // A prefix is a string prefix: `nginx` is not under `nginx-exporter`.
            (
                "docker.io/library/nginx-exporter",
                Some("docker.io/library/*"),
            ),
        ] {
            assert_eq!(
                matching_rule(&policy, repository).map(|rule| rule.pattern.as_str()),
                expected,
                "{repository}"
            );
        }
    }

    #[test]
    fn rules_match_the_repository_whatever_the_image_says_about_tag_or_digest() {
        let digest = format!("sha256:{}", "a".repeat(64));
        for image in [
            "nginx".to_string(),
            "nginx:1.27".to_string(),
            "docker.io/library/nginx:1.27".to_string(),
            format!("nginx:1.27@{digest}"),
            format!("nginx@{digest}"),
        ] {
            assert_eq!(
                upstream_repository(&image),
                "docker.io/library/nginx",
                "{image}"
            );
        }
        assert_eq!(
            upstream_repository("localhost:5000/team/app:v1"),
            "localhost:5000/team/app"
        );
    }

    #[test]
    fn with_no_rules_every_upstream_image_is_allowed_by_default() {
        let policy = TrustPolicySection::default();
        assert!(check_upstream(&policy, "anything.example/x/y:1").is_ok());
    }

    #[test]
    fn an_allow_list_refuses_an_unmatched_image_by_name() {
        let policy = policy(&["docker.io/library/*"], false);
        assert!(check_upstream(&policy, "nginx:1.27").is_ok());
        let refused = check_upstream(&policy, "ghcr.io/evil/miner:latest").unwrap_err();
        assert_eq!(refused.image, "ghcr.io/evil/miner:latest");
        let message = refused.to_string();
        assert!(message.contains("ghcr.io/evil/miner:latest"), "{message}");
        assert!(message.contains("upstream_default.allow"), "{message}");
    }

    #[test]
    fn an_empty_allow_list_refuses_every_upstream_image() {
        let policy = policy(&[], false);
        assert!(check_upstream(&policy, "nginx:1.27").is_err());
    }
}
