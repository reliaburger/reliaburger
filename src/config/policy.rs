/// Policy specification: a cluster-wide rule every applied app must pass.
///
/// A `[policy.<name>]` table holds two CEL expressions. `match` picks the
/// apps the policy applies to (every app when it's absent); `deny` says
/// when to refuse one. When both are true the apply is refused with
/// `message`, or, with `action = "warn"`, accepted with a warning.
///
/// Policies are committed to Raft like permissions, and only an admin may
/// write them. The leader evaluates them (`crate::policy`); the state
/// machine only stores them. See chapter 18 of the book.
use serde::{Deserialize, Serialize};

/// One `[policy.<name>]` table.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PolicySpec {
    /// CEL expression choosing the apps this policy applies to. Absent
    /// means every app.
    #[serde(rename = "match", default, skip_serializing_if = "Option::is_none")]
    pub match_expression: Option<String>,
    /// CEL expression that is true when a matching app breaks the rule.
    pub deny: String,
    /// What `relish apply` says when the rule is broken. Absent means a
    /// message naming the policy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// Whether a broken rule refuses the apply or only warns.
    #[serde(default)]
    pub action: PolicyAction,
}

/// What happens when an app breaks a policy.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PolicyAction {
    /// Refuse the apply. The default.
    #[default]
    Deny,
    /// Accept the apply, with a warning naming the policy.
    Warn,
}

impl PolicyAction {
    /// The lowercase word used in TOML and in output.
    pub fn as_str(self) -> &'static str {
        match self {
            PolicyAction::Deny => "deny",
            PolicyAction::Warn => "warn",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn match_is_spelt_match_in_toml() {
        let spec: PolicySpec = toml::from_str(
            r#"
            match = "app.namespace == 'prod'"
            deny = "!has(app.memory)"
            message = "apps in prod need a memory limit"
        "#,
        )
        .unwrap();
        assert_eq!(
            spec.match_expression.as_deref(),
            Some("app.namespace == 'prod'")
        );
        assert_eq!(spec.action, PolicyAction::Deny);
        let back = toml::to_string(&spec).unwrap();
        assert!(back.contains("match = "), "{back}");
    }

    #[test]
    fn warn_is_the_only_other_action() {
        let spec: PolicySpec = toml::from_str("deny = 'true'\naction = 'warn'").unwrap();
        assert_eq!(spec.action, PolicyAction::Warn);
        assert!(toml::from_str::<PolicySpec>("deny = 'true'\naction = 'audit'").is_err());
    }

    #[test]
    fn a_policy_without_deny_does_not_parse() {
        assert!(toml::from_str::<PolicySpec>("match = 'true'").is_err());
    }
}
