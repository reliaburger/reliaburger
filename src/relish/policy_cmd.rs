//! `relish policy`: list and delete the cluster's policies, and test
//! policies against apps offline (#1117).
//!
//! `relish policy test` runs the same pure function the leader runs, on
//! files, with no cluster. That's what makes it a CI check: a pull request
//! that would be refused by the cluster fails before it merges.

use std::path::PathBuf;

use crate::config::Config;
use crate::policy::PolicyListing;
use crate::policy::{Caller, Source};
use crate::sesame::types::ApiRole;

use super::RelishError;
use super::client::BunClient;
use super::output::OutputFormat;

/// Who `relish policy test` pretends to be, and how the change arrives.
#[derive(Debug, Clone)]
pub struct TestIdentity {
    /// The caller's token name.
    pub caller: String,
    /// The caller's role.
    pub role: ApiRole,
    /// `apply` or `gitops`.
    pub source: Source,
}

impl TestIdentity {
    fn caller(&self) -> Caller {
        match self.source {
            Source::GitOps => Caller::gitops(),
            Source::Apply => Caller::token(&self.caller, self.role),
        }
    }
}

/// `relish policy list`: every policy in force, with its action and rule.
pub async fn policy_list(output: OutputFormat) -> Result<(), RelishError> {
    let listing = BunClient::default_local().policies().await?;
    match output {
        OutputFormat::Human => print!("{}", render_policy_list(&listing)),
        OutputFormat::Json => println!(
            "{}",
            serde_json::to_string_pretty(&listing).map_err(RelishError::SerialiseJson)?
        ),
        OutputFormat::Yaml => print!(
            "{}",
            yaml_serde::to_string(&listing).map_err(RelishError::SerialiseYaml)?
        ),
    }
    Ok(())
}

/// The human view of `relish policy list`.
pub fn render_policy_list(listing: &PolicyListing) -> String {
    if listing.policies.is_empty() {
        return format!("No policies (generation {}).\n", listing.generation);
    }
    let name_width = listing
        .policies
        .keys()
        .map(String::len)
        .max()
        .unwrap_or(0)
        .max("NAME".len());
    let mut out = format!("{:name_width$}  ACTION  OWNER   RULE\n", "NAME");
    for (name, spec) in &listing.policies {
        let owner = if listing.gitops_owned.contains(name) {
            "gitops"
        } else {
            "apply"
        };
        let rule = match &spec.match_expression {
            Some(matching) => format!("match {matching}; deny {}", spec.deny),
            None => format!("deny {}", spec.deny),
        };
        out.push_str(&format!(
            "{name:name_width$}  {:6}  {owner:6}  {rule}\n",
            spec.action.as_str()
        ));
    }
    out.push_str(&format!("\nGeneration {}.\n", listing.generation));
    out
}

/// `relish policy delete NAME`: remove a policy. Policies never judge
/// policies, so this works even when a broken one refuses every app.
pub async fn policy_delete(name: &str) -> Result<(), RelishError> {
    BunClient::default_local().delete_policy(name).await?;
    println!("deleted policy {name}");
    Ok(())
}

/// `relish policy test -f policies.toml -f apps.toml`: judge the files'
/// apps by the files' policies, offline, print the report, and fail when
/// an apply would be refused.
pub fn policy_test(
    files: &[PathBuf],
    identity: &TestIdentity,
    output: OutputFormat,
) -> Result<(), RelishError> {
    let mut merged = Config::default();
    for file in files {
        let config = super::commands::load_config_file(file)?;
        merge(&mut merged, config, file)?;
    }
    merged.validate()?;
    let set = crate::policy::PolicySet::compile(&merged.policy).map_err(|error| {
        RelishError::InvalidFlag {
            flag: "file".into(),
            reason: error.to_string(),
        }
    })?;
    let verdict = crate::policy::evaluate(&set, &merged, &identity.caller(), identity.source);
    match output {
        OutputFormat::Human => println!(
            "{}",
            crate::policy::report::render_test(&verdict, merged.app.len(), merged.policy.len())
        ),
        OutputFormat::Json => println!(
            "{}",
            serde_json::to_string_pretty(&verdict).map_err(RelishError::SerialiseJson)?
        ),
        OutputFormat::Yaml => print!(
            "{}",
            yaml_serde::to_string(&verdict).map_err(RelishError::SerialiseYaml)?
        ),
    }
    if verdict.is_allowed() {
        Ok(())
    } else {
        Err(RelishError::PolicyDenied {
            denials: verdict.denials.iter().map(ToString::to_string).collect(),
        })
    }
}

/// Add `config`'s apps and policies to `merged`. The same name in two files
/// is a mistake, not an override.
fn merge(merged: &mut Config, config: Config, file: &std::path::Path) -> Result<(), RelishError> {
    let duplicate = |kind: &str, name: &str| RelishError::InvalidFlag {
        flag: "file".into(),
        reason: format!(
            "{kind} {name} is declared twice (again in {})",
            file.display()
        ),
    };
    for (name, spec) in config.app {
        if merged.app.contains_key(&name) {
            return Err(duplicate("app", &name));
        }
        merged.app.insert(name, spec);
    }
    for (name, spec) in config.policy {
        if merged.policy.contains_key(&name) {
            return Err(duplicate("policy", &name));
        }
        merged.policy.insert(name, spec);
    }
    // Namespaces let an app's namespace validate; the rest isn't judged.
    merged.namespace.extend(config.namespace);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &tempfile::TempDir, name: &str, body: &str) -> PathBuf {
        let path = dir.path().join(name);
        std::fs::write(&path, body).unwrap();
        path
    }

    fn deployer() -> TestIdentity {
        TestIdentity {
            caller: "ci".into(),
            role: ApiRole::Deployer,
            source: Source::Apply,
        }
    }

    const POLICIES: &str = r#"
[policy.prod-needs-limits]
match = "app.namespace == 'prod'"
deny = "!has(app.memory)"
message = "apps in prod need a memory limit"
"#;

    #[test]
    fn policy_test_fails_when_an_apply_would_be_refused() {
        let dir = tempfile::tempdir().unwrap();
        let policies = write(&dir, "policies.toml", POLICIES);
        let apps = write(
            &dir,
            "apps.toml",
            "[app.web]\nimage = 'web:v1'\nnamespace = 'prod'\n",
        );
        let error = policy_test(&[policies, apps], &deployer(), OutputFormat::Json).unwrap_err();
        assert!(
            matches!(&error, RelishError::PolicyDenied { denials } if denials.len() == 1),
            "{error}"
        );
        assert!(error.to_string().contains("prod-needs-limits"), "{error}");
    }

    #[test]
    fn policy_test_passes_a_clean_config() {
        let dir = tempfile::tempdir().unwrap();
        let policies = write(&dir, "policies.toml", POLICIES);
        let apps = write(
            &dir,
            "apps.toml",
            "[app.web]\nimage = 'web:v1'\nnamespace = 'prod'\nmemory = '256Mi'\n",
        );
        policy_test(&[policies, apps], &deployer(), OutputFormat::Json).unwrap();
    }

    #[test]
    fn policy_test_refuses_a_name_declared_twice() {
        let dir = tempfile::tempdir().unwrap();
        let first = write(&dir, "a.toml", POLICIES);
        let second = write(&dir, "b.toml", POLICIES);
        let error = policy_test(&[first, second], &deployer(), OutputFormat::Json).unwrap_err();
        assert!(error.to_string().contains("declared twice"), "{error}");
    }

    #[test]
    fn policy_test_refuses_a_bad_policy_like_the_cluster_would() {
        let dir = tempfile::tempdir().unwrap();
        let policies = write(
            &dir,
            "policies.toml",
            "[policy.rule]\ndeny = \"policy.name == 'rule'\"\n",
        );
        let error = policy_test(&[policies], &deployer(), OutputFormat::Json).unwrap_err();
        assert!(
            error.to_string().contains("policies see only app"),
            "{error}"
        );
    }

    #[test]
    fn the_list_shows_each_policy_its_action_and_owner() {
        let config = Config::parse(&format!(
            "{POLICIES}\n[policy.no-latest]\ndeny = \"app.image.endsWith(':latest')\"\naction = 'warn'\n"
        ))
        .unwrap();
        let listing = PolicyListing {
            policies: config.policy,
            generation: 4,
            gitops_owned: ["no-latest".to_string()].into(),
        };
        insta::assert_snapshot!(render_policy_list(&listing));
    }

    #[test]
    fn an_empty_list_names_the_generation() {
        assert_eq!(
            render_policy_list(&PolicyListing::default()),
            "No policies (generation 0).\n"
        );
    }
}
