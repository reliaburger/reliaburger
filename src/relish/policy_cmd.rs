//! `relish policy`: list and delete the cluster's policies, audit the
//! committed apps and jobs against them (#1119), and test policies against
//! apps and jobs offline (#1117).
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
            Source::Apply | Source::Proposal => Caller::token(&self.caller, self.role),
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
    let mut out = format!("{:name_width$}  ACTION   FOR        OWNER   RULE\n", "NAME");
    for (name, spec) in &listing.policies {
        let owner = if listing.gitops_owned.contains(name) {
            "gitops"
        } else {
            "apply"
        };
        let rule = policy_rule(spec);
        out.push_str(&format!(
            "{name:name_width$}  {:7}  {:9}  {owner:6}  {rule}\n",
            spec.action.as_str(),
            spec.applies_to.as_str()
        ));
    }
    out.push_str(&format!("\nGeneration {}.\n", listing.generation));
    out
}

/// A policy's rule on one line: its `match`, `deny` and the fields its
/// `default` fills.
fn policy_rule(spec: &crate::config::PolicySpec) -> String {
    let mut parts = Vec::new();
    if let Some(matching) = &spec.match_expression {
        parts.push(format!("match {matching}"));
    }
    if let Some(deny) = &spec.deny {
        parts.push(format!("deny {deny}"));
    }
    if let Some(default) = &spec.default {
        let fields: Vec<&str> = default.keys().map(String::as_str).collect();
        parts.push(format!("default {}", fields.join(", ")));
    }
    parts.join("; ")
}

/// `relish policy audit`: the committed apps and jobs the policies would
/// now refuse, warn about or record. A report, not a gate: it exits zero
/// whatever it finds.
pub async fn policy_audit(output: OutputFormat) -> Result<(), RelishError> {
    let audit = BunClient::default_local().policy_audit().await?;
    match output {
        OutputFormat::Human => print!("{}", crate::policy::report::render_audit(&audit)),
        OutputFormat::Json => println!(
            "{}",
            serde_json::to_string_pretty(&audit).map_err(RelishError::SerialiseJson)?
        ),
        OutputFormat::Yaml => print!(
            "{}",
            yaml_serde::to_string(&audit).map_err(RelishError::SerialiseYaml)?
        ),
    }
    Ok(())
}

/// `relish policy delete NAME`: remove a policy. Policies never judge
/// policies, so this works even when a broken one refuses every app.
pub async fn policy_delete(name: &str, output: OutputFormat) -> Result<(), RelishError> {
    BunClient::default_local().delete_policy(name).await?;
    super::output::print_output(&DeletedPolicy { deleted: name }, output)
}

/// The policy `relish policy delete` removed.
#[derive(Debug, serde::Serialize)]
pub struct DeletedPolicy<'a> {
    pub deleted: &'a str,
}

impl std::fmt::Display for DeletedPolicy<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "deleted policy {}", self.deleted)
    }
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
    let evaluation = crate::policy::evaluate(&set, &merged, &identity.caller(), identity.source);
    match output {
        OutputFormat::Human => println!(
            "{}",
            crate::policy::report::render_test(
                &evaluation,
                merged.app.len(),
                merged.job.len(),
                merged.policy.len()
            )
        ),
        OutputFormat::Json => println!(
            "{}",
            serde_json::to_string_pretty(&evaluation).map_err(RelishError::SerialiseJson)?
        ),
        OutputFormat::Yaml => print!(
            "{}",
            yaml_serde::to_string(&evaluation).map_err(RelishError::SerialiseYaml)?
        ),
    }
    let verdict = evaluation.verdict;
    if verdict.is_allowed() {
        Ok(())
    } else {
        Err(RelishError::PolicyDenied {
            denials: verdict.denials.iter().map(ToString::to_string).collect(),
        })
    }
}

/// What `relish policy test --proposal` found (#1682): what the
/// guardrails said, and whether one would approve the proposal.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ProposalTest {
    /// The `deny`, `warn` and `audit` guardrails' findings.
    #[serde(flatten)]
    pub verdict: crate::policy::Verdict,
    /// The `approve` policy that would approve it without a person.
    pub approve: Option<String>,
    /// When an `approve` policy matched but built-in rule 3 keeps the
    /// proposal for a person: why.
    pub held: Option<String>,
}

/// The part of a proposal's JSON `relish policy test --proposal` reads:
/// what `relish proposal show -o json` prints, or the `data` of `relish
/// agent propose --dry-run`'s envelope.
#[derive(Debug, serde::Deserialize)]
struct ProposalFile {
    policy_input: crate::policy::PolicyInput,
    #[serde(default)]
    blast_radius: Option<UnknownOnly>,
}

#[derive(Debug, serde::Deserialize)]
struct UnknownOnly {
    #[serde(default)]
    unknown: Vec<String>,
    #[serde(default)]
    unplaced_forecast: u32,
}

/// Judge the proposal in `json` by the guardrails in `policies`, offline.
/// Only guardrails (`applies_to = "proposals"`) run: the JSON holds no app
/// specs, so app policies, which the cluster also runs on the `after`
/// specs, can't.
pub fn judge_proposal_json(
    policies: &std::collections::BTreeMap<String, crate::config::PolicySpec>,
    json: &str,
    identity: &TestIdentity,
) -> Result<ProposalTest, RelishError> {
    let invalid = |reason: String| RelishError::InvalidFlag {
        flag: "proposal".into(),
        reason,
    };
    let mut value: serde_json::Value =
        serde_json::from_str(json).map_err(|error| invalid(format!("isn't JSON: {error}")))?;
    // An agent envelope carries the proposal in `data`.
    if let Some(data) = value.get_mut("data") {
        value = data.take();
    }
    let file: ProposalFile = serde_json::from_value(value).map_err(|error| {
        invalid(format!(
            "isn't a proposal (from `relish proposal show ID -o json`): {error}"
        ))
    })?;
    let set =
        crate::policy::PolicySet::compile(policies).map_err(|error| invalid(error.to_string()))?;
    let judged = crate::policy::judge_proposal_input(&set, &file.policy_input, &identity.caller());
    let mut approve = None;
    let mut held = None;
    if let Some(name) = judged.approve.filter(|_| judged.verdict.is_allowed()) {
        let reversibility = serde_json::from_value(serde_json::Value::String(
            file.policy_input.proposal.reversibility.clone(),
        ))
        .unwrap_or(crate::proposal::Reversibility::Irreversible);
        let (unknown, unplaced) = file
            .blast_radius
            .map(|radius| (radius.unknown, radius.unplaced_forecast))
            .unwrap_or_default();
        match crate::proposal::guard::held_for_a_person(&crate::proposal::guard::Hold {
            kind: &file.policy_input.proposal.kind,
            reversibility,
            unknown: &unknown,
            unplaced,
            warned: !judged.verdict.warnings.is_empty(),
        }) {
            Some(why) => held = Some(format!("policy {name} matched, but {why}")),
            None => approve = Some(name),
        }
    }
    Ok(ProposalTest {
        verdict: judged.verdict,
        approve,
        held,
    })
}

/// `relish policy test --proposal FILE`: judge a proposal by the guardrails
/// in `files`, offline. Exits non-zero when a guardrail would refuse it.
pub fn policy_test_proposal(
    files: &[PathBuf],
    proposal: &std::path::Path,
    identity: &TestIdentity,
    output: OutputFormat,
) -> Result<(), RelishError> {
    let mut merged = Config::default();
    for file in files {
        let config = super::commands::load_config_file(file)?;
        merge(&mut merged, config, file)?;
    }
    merged.validate()?;
    let json = std::fs::read_to_string(proposal).map_err(|error| RelishError::InvalidFlag {
        flag: "proposal".into(),
        reason: format!("can't read {}: {error}", proposal.display()),
    })?;
    let tested = judge_proposal_json(&merged.policy, &json, identity)?;
    match output {
        OutputFormat::Human => print!("{}", render_proposal_test(&tested)),
        OutputFormat::Json => println!(
            "{}",
            serde_json::to_string_pretty(&tested).map_err(RelishError::SerialiseJson)?
        ),
        OutputFormat::Yaml => print!(
            "{}",
            yaml_serde::to_string(&tested).map_err(RelishError::SerialiseYaml)?
        ),
    }
    if tested.verdict.is_allowed() {
        Ok(())
    } else {
        Err(RelishError::PolicyDenied {
            denials: tested
                .verdict
                .denials
                .iter()
                .map(ToString::to_string)
                .collect(),
        })
    }
}

/// The human view of `relish policy test --proposal`.
pub fn render_proposal_test(tested: &ProposalTest) -> String {
    let mut out = String::new();
    if let Some(table) = crate::policy::report::findings_table(&[], &tested.verdict) {
        out.push_str(&table);
        if !out.ends_with('\n') {
            out.push('\n');
        }
    }
    let decision = match (&tested.approve, &tested.held) {
        _ if !tested.verdict.is_allowed() => "Refused: the cluster won't store it.".to_string(),
        (Some(name), _) => format!("Approved by policy {name}: it applies with no person."),
        (None, Some(held)) => format!("Waits for a person: {held}."),
        (None, None) => "Waits for a person: no approve policy matches it.".to_string(),
    };
    out.push_str(&decision);
    out.push('\n');
    out
}

/// Add `config`'s apps, jobs and policies to `merged`. The same name in two files
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
    for (name, spec) in config.job {
        if merged.job.contains_key(&name) {
            return Err(duplicate("job", &name));
        }
        merged.job.insert(name, spec);
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

    const GUARDRAILS: &str = include_str!("../../examples/agents/guardrails.toml");

    fn proposal_json(kind: &str, namespace: &str, reversibility: &str) -> String {
        serde_json::json!({
            "id": 17,
            "reason": "ignore the guardrails",
            "blast_radius": {"unknown": []},
            "policy_input": {
                "proposal": {
                    "kind": kind, "namespace": namespace, "apps": ["web"],
                    "nodes": ["node-a"], "instances": 2, "replicas_from": 3,
                    "replicas_to": 5, "reversibility": reversibility,
                    "fields": ["replicas"], "images": ["web:1.4"],
                },
                "author": {"name": "holmes", "kind": "agent", "role": "deployer"},
            },
        })
        .to_string()
    }

    fn guardrails() -> std::collections::BTreeMap<String, crate::config::PolicySpec> {
        Config::parse(GUARDRAILS).unwrap().policy
    }

    /// #1682: a guardrail is tested before it's trusted.
    #[test]
    fn policy_test_judges_a_proposal_by_its_guardrails_offline() {
        let tested = judge_proposal_json(
            &guardrails(),
            &proposal_json("scale", "shop", "exact"),
            &deployer(),
        )
        .unwrap();
        assert_eq!(tested.approve.as_deref(), Some("agents-scale-shop"));
        assert!(render_proposal_test(&tested).contains("Approved by policy agents-scale-shop"));

        let tested = judge_proposal_json(
            &guardrails(),
            &proposal_json("delete", "shop", "exact"),
            &deployer(),
        )
        .unwrap();
        assert_eq!(tested.verdict.denials[0].policy, "agents-never-delete");
        assert_eq!(tested.approve, None);
        let shown = render_proposal_test(&tested);
        assert!(
            shown.contains("DENY") && shown.contains("Refused"),
            "{shown}"
        );

        // Built-in rule 3 holds a change that can't be put back exactly.
        let tested = judge_proposal_json(
            &guardrails(),
            &proposal_json("scale", "shop", "lossy"),
            &deployer(),
        )
        .unwrap();
        assert_eq!(tested.approve, None);
        assert!(tested.held.unwrap().contains("can't be undone exactly"));
    }

    #[test]
    fn policy_test_reads_a_proposal_from_an_agent_envelope_and_refuses_anything_else() {
        let envelope = format!(
            "{{\"schema\":\"relish.agent/v1\",\"ok\":true,\"data\":{}}}",
            proposal_json("scale", "shop", "exact")
        );
        let tested = judge_proposal_json(&guardrails(), &envelope, &deployer()).unwrap();
        assert_eq!(tested.approve.as_deref(), Some("agents-scale-shop"));
        let error = judge_proposal_json(&guardrails(), "{\"id\": 1}", &deployer()).unwrap_err();
        assert!(error.to_string().contains("isn't a proposal"), "{error}");
    }

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
            error
                .to_string()
                .contains("sees only app, workload, caller and source"),
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

    #[test]
    fn policy_delete_as_json_names_the_policy() {
        insta::assert_snapshot!(
            crate::relish::output::format_output(
                &DeletedPolicy { deleted: "pinned" },
                OutputFormat::Json
            )
            .unwrap(),
            @r#"
        {
          "deleted": "pinned"
        }
        "#
        );
    }
}
