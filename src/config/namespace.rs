/// Namespace resource quota specification.
///
/// Namespaces provide optional workload isolation with resource budgets.
/// The cluster uses a single default namespace unless others are created.
use serde::{Deserialize, Serialize};

/// Resource quotas for a namespace.
///
/// All fields are optional — omitted fields mean no quota for that resource.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NamespaceSpec {
    /// Total CPU budget, e.g. "8000m".
    pub cpu: Option<String>,
    /// Total memory budget, e.g. "16Gi".
    pub memory: Option<String>,
    /// Total GPU count available to this namespace.
    pub gpu: Option<u32>,
    /// Maximum number of distinct apps allowed.
    pub max_apps: Option<u32>,
    /// Maximum total replica count across all apps.
    pub max_replicas: Option<u32>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_namespace_with_all_fields() {
        let toml_str = r#"
            cpu = "8000m"
            memory = "16Gi"
            gpu = 2
            max_apps = 50
            max_replicas = 200
        "#;
        let ns: NamespaceSpec = toml::from_str(toml_str).unwrap();
        assert_eq!(ns.cpu.as_deref(), Some("8000m"));
        assert_eq!(ns.memory.as_deref(), Some("16Gi"));
        assert_eq!(ns.gpu, Some(2));
        assert_eq!(ns.max_apps, Some(50));
        assert_eq!(ns.max_replicas, Some(200));
    }

    #[test]
    fn parse_namespace_with_partial_fields() {
        let toml_str = r#"cpu = "4000m""#;
        let ns: NamespaceSpec = toml::from_str(toml_str).unwrap();
        assert_eq!(ns.cpu.as_deref(), Some("4000m"));
        assert_eq!(ns.memory, None);
        assert_eq!(ns.gpu, None);
        assert_eq!(ns.max_apps, None);
        assert_eq!(ns.max_replicas, None);
    }
}
