/// Permission specification for access control.
///
/// Permissions define which actions a principal (identified by its token name)
/// can perform on which apps and namespaces. Valid actions: deploy, scale,
/// logs, metrics, exec, host-exec, admin, secret-read, secret-write.
///
/// Enforcement (see `sesame::auth::authorize_permission`): a spec named after a
/// token is an **additional** allow-list on top of that token's role and scope
/// — it can restrict a principal to named actions/apps/namespaces but never
/// grant beyond its role. A principal with no spec is governed by role and
/// scope alone, so permissions are opt-in per principal.
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PermissionSpec {
    /// Actions this permission grants.
    #[serde(default)]
    pub actions: Vec<String>,
    /// Apps this permission applies to. Use `["*"]` for all apps.
    #[serde(default)]
    pub apps: Vec<String>,
    /// Namespaces this permission is scoped to. If omitted, applies to all.
    pub namespaces: Option<Vec<String>>,
}

/// An action a [`PermissionSpec`] may grant. The wire form (used in TOML and in
/// the stored spec's `actions` list) is the kebab-case string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PermissionAction {
    Deploy,
    Scale,
    Logs,
    Metrics,
    Exec,
    HostExec,
    Admin,
    SecretRead,
    SecretWrite,
}

impl PermissionAction {
    /// The kebab-case token used in `[permission]` `actions` lists.
    pub fn as_str(self) -> &'static str {
        match self {
            PermissionAction::Deploy => "deploy",
            PermissionAction::Scale => "scale",
            PermissionAction::Logs => "logs",
            PermissionAction::Metrics => "metrics",
            PermissionAction::Exec => "exec",
            PermissionAction::HostExec => "host-exec",
            PermissionAction::Admin => "admin",
            PermissionAction::SecretRead => "secret-read",
            PermissionAction::SecretWrite => "secret-write",
        }
    }
}

impl PermissionSpec {
    /// Whether this spec grants `action` on `app` in `namespace`.
    ///
    /// The `admin` action is a super-grant covering every action. An app entry
    /// of `*` matches any app; an empty `apps` list matches nothing (a spec
    /// that names no apps grants nothing). `namespaces = None` matches every
    /// namespace.
    pub fn allows(&self, action: PermissionAction, app: &str, namespace: &str) -> bool {
        let action_ok = self
            .actions
            .iter()
            .any(|a| a == action.as_str() || a == PermissionAction::Admin.as_str());
        if !action_ok {
            return false;
        }
        let app_ok = self.apps.iter().any(|a| a == "*" || a == app);
        if !app_ok {
            return false;
        }
        match &self.namespaces {
            None => true,
            Some(namespaces) => namespaces.iter().any(|n| n == namespace),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_permission_with_all_fields() {
        let toml_str = r#"
            actions = ["deploy", "scale", "logs"]
            apps = ["web", "api"]
            namespaces = ["production", "staging"]
        "#;
        let p: PermissionSpec = toml::from_str(toml_str).unwrap();
        assert_eq!(p.actions, vec!["deploy", "scale", "logs"]);
        assert_eq!(p.apps, vec!["web", "api"]);
        assert_eq!(
            p.namespaces,
            Some(vec!["production".to_string(), "staging".to_string()])
        );
    }

    #[test]
    fn parse_permission_without_namespaces() {
        let toml_str = r#"
            actions = ["admin"]
            apps = ["*"]
        "#;
        let p: PermissionSpec = toml::from_str(toml_str).unwrap();
        assert_eq!(p.actions, vec!["admin"]);
        assert_eq!(p.apps, vec!["*"]);
        assert_eq!(p.namespaces, None);
    }

    #[test]
    fn allows_matches_action_app_and_namespace() {
        let spec = PermissionSpec {
            actions: vec!["deploy".to_string(), "logs".to_string()],
            apps: vec!["web".to_string()],
            namespaces: Some(vec!["prod".to_string()]),
        };
        assert!(spec.allows(PermissionAction::Deploy, "web", "prod"));
        assert!(spec.allows(PermissionAction::Logs, "web", "prod"));
        // Wrong action, app, or namespace all deny.
        assert!(!spec.allows(PermissionAction::Exec, "web", "prod"));
        assert!(!spec.allows(PermissionAction::Deploy, "api", "prod"));
        assert!(!spec.allows(PermissionAction::Deploy, "web", "staging"));
    }

    #[test]
    fn admin_action_is_a_super_grant_and_star_matches_any_app() {
        let spec = PermissionSpec {
            actions: vec!["admin".to_string()],
            apps: vec!["*".to_string()],
            namespaces: None,
        };
        assert!(spec.allows(PermissionAction::Deploy, "anything", "anywhere"));
        assert!(spec.allows(PermissionAction::SecretWrite, "x", "y"));
    }

    #[test]
    fn an_empty_apps_list_grants_nothing() {
        let spec = PermissionSpec {
            actions: vec!["deploy".to_string()],
            apps: vec![],
            namespaces: None,
        };
        assert!(!spec.allows(PermissionAction::Deploy, "web", "prod"));
    }
}
