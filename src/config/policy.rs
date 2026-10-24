/// Policy specification: a cluster-wide rule every applied app must pass.
///
/// A `[policy.<name>]` table holds two CEL expressions. `match` picks the
/// apps the policy applies to (every app when it's absent); `deny` says
/// when to refuse one. When both are true the apply is refused with
/// `message`, or, with `action = "warn"`, accepted with a warning.
///
/// A policy can also carry a `[policy.<name>.default]` table (#1118): app
/// fields it fills in wherever a matching app leaves them out. A default
/// never overwrites a value the app sets.
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
    /// Absent for a policy that only fills defaults.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub deny: Option<String>,
    /// What `relish apply` says when the rule is broken. Absent means a
    /// message naming the policy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// Whether a broken rule refuses the apply or only warns.
    #[serde(default)]
    pub action: PolicyAction,
    /// App fields to fill in where a matching app leaves them out, as a
    /// TOML table shaped like an `[app.*]` table (#1118). Kept as JSON so
    /// it crosses Raft like the rest of desired state.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<serde_json::Map<String, serde_json::Value>>,
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
    fn a_default_table_parses_with_dotted_keys() {
        let config = crate::config::Config::parse(
            r#"
            [policy.team-label]
            match = "app.namespace == 'payments'"

            [policy.team-label.default]
            env.TEAM = "payments"
            deploy.max_unavailable = 1
        "#,
        )
        .unwrap();
        let spec = &config.policy["team-label"];
        assert_eq!(spec.deny, None);
        let default = spec.default.as_ref().unwrap();
        assert_eq!(default["env"]["TEAM"], "payments");
        assert_eq!(default["deploy"]["max_unavailable"], 1);
        // It survives the JSON that carries it through Raft.
        let back: PolicySpec = serde_json::from_str(&serde_json::to_string(spec).unwrap()).unwrap();
        assert_eq!(&back, spec);
    }
}
