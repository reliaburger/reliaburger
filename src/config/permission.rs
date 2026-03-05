/// Permission specification for access control.
///
/// Permissions define which actions a principal can perform on which
/// apps and namespaces. Valid actions: deploy, scale, logs, metrics,
/// exec, host-exec, admin, secret-read, secret-write.
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
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
}
