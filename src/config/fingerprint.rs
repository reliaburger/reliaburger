//! Canonical desired-spec evidence for deployment previews.
use serde::Serialize;
use sha2::{Digest, Sha256};

use super::{AppSpec, JobSpec};

/// Fingerprint every serialised field; unavailable evidence remains unknown.
pub fn spec_fingerprint(spec: &impl Serialize) -> Option<String> {
    let bytes = serde_json::to_vec(spec).ok()?;
    Some(format!("{:x}", Sha256::digest(bytes)))
}

/// Fingerprint an app's effective namespace and ignore placement-only ordinals.
pub fn app_fingerprint(spec: &AppSpec) -> Option<String> {
    let mut effective = spec.clone();
    effective.namespace = Some(spec.namespace.as_deref().unwrap_or("default").into());
    effective.ordinals = None;
    spec_fingerprint(&effective)
}

/// Use the namespace from the authoritative resource identity.
pub fn app_fingerprint_in(spec: &AppSpec, namespace: &str) -> Option<String> {
    let mut effective = spec.clone();
    effective.namespace = Some(namespace.into());
    app_fingerprint(&effective)
}

/// Fingerprint a job's effective namespace and complete execution specification.
pub fn job_fingerprint(spec: &JobSpec) -> Option<String> {
    let mut effective = spec.clone();
    effective.namespace = Some(spec.namespace.as_deref().unwrap_or("default").into());
    spec_fingerprint(&effective)
}

/// Plan identity of an app, including its non-default namespace.
pub fn app_resource_key(name: &str, namespace: &str) -> String {
    resource_key("app", name, namespace)
}

/// Plan identity of a job, including its non-default namespace.
pub fn job_resource_key(name: &str, namespace: &str) -> String {
    resource_key("job", name, namespace)
}

fn resource_key(kind: &str, name: &str, namespace: &str) -> String {
    if namespace == "default" {
        format!("{kind}.{name}")
    } else {
        format!("{kind}.{namespace}/{name}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    #[test]
    fn placement_ordinals_and_implicit_default_do_not_change_desired_spec() {
        let spec = Config::parse("[app.web]\nimage = 'web:v1'\n")
            .unwrap()
            .app
            .remove("web")
            .unwrap();
        let mut assigned = spec.clone();
        assigned.namespace = Some("default".into());
        assigned.ordinals = Some(vec![4, 9]);
        assert_eq!(app_fingerprint(&spec), app_fingerprint(&assigned));
        assert_ne!(app_fingerprint(&spec), app_fingerprint_in(&spec, "team"));
    }

    #[test]
    fn same_image_changes_to_workload_settings_change_the_fingerprint() {
        let base = Config::parse("[app.web]\nimage = 'web:v1'\n").unwrap();
        for setting in [
            "replicas = 3",
            "port = 8080",
            "memory = '128Mi'",
            "cpu = '250m'",
            "command = ['run-v2']",
            "env = { MODE = 'new' }",
            "health = { path = '/ready' }",
            "deploy = { strategy = 'blue-green' }",
            "placement = { required = ['zone=west'] }",
        ] {
            let changed =
                Config::parse(&format!("[app.web]\nimage = 'web:v1'\n{setting}\n")).unwrap();
            assert_ne!(
                app_fingerprint(&base.app["web"]),
                app_fingerprint(&changed.app["web"]),
                "missed {setting}"
            );
        }
    }
}
