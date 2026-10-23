use std::collections::BTreeMap;

use proptest::prelude::*;

use super::*;
use crate::council::config_to_desired_writes;

fn policies(toml: &str) -> BTreeMap<String, PolicySpec> {
    Config::parse(toml).unwrap().policy
}

fn compile(toml: &str) -> PolicySet {
    PolicySet::compile(&policies(toml)).unwrap()
}

fn apps(toml: &str) -> Config {
    Config::parse(toml).unwrap()
}

fn deployer() -> Caller {
    Caller::token("ci", ApiRole::Deployer)
}

/// The decisions as `(policy, action, app)` triples, denials first.
fn decisions(verdict: &Verdict) -> Vec<(String, PolicyAction, String)> {
    verdict
        .denials
        .iter()
        .chain(&verdict.warnings)
        .map(|finding| {
            (
                finding.policy.clone(),
                finding.action,
                format!("{}/{}", finding.app.namespace, finding.app.name),
            )
        })
        .collect()
}

const LIMITS_AND_LATEST: &str = r#"
[policy.prod-needs-limits]
match = "app.namespace == 'prod'"
deny = "!has(app.memory)"
message = "apps in prod need a memory limit"

[policy.no-latest]
deny = "app.image.endsWith(':latest')"
action = "warn"
"#;

/// A decision in the table: the policy and what it did.
type Decision = (String, PolicyAction);

/// Whether a commit error is the one a case expects.
type ErrorCheck = fn(&PolicyError) -> bool;

/// The unit table: one row per kind of decision a policy can make.
#[test]
fn the_decision_table() {
    let deny = |policy: &str| (policy.to_string(), PolicyAction::Deny);
    let warn = |policy: &str| (policy.to_string(), PolicyAction::Warn);
    let rows: Vec<(&str, &str, Vec<Decision>)> = vec![
        // No policy matches nothing.
        ("", "image = 'web:v1'", vec![]),
        // A deny with no match applies to every app.
        ("deny = 'true'", "image = 'web:v1'", vec![deny("rule")]),
        ("deny = 'false'", "image = 'web:v1'", vec![]),
        // A false match skips the app, whatever deny says.
        (
            "match = \"app.namespace == 'prod'\"\ndeny = 'true'",
            "image = 'web:v1'",
            vec![],
        ),
        // The namespace is resolved: an app with none is in `default`.
        (
            "match = \"app.namespace == 'default'\"\ndeny = 'true'",
            "image = 'web:v1'",
            vec![deny("rule")],
        ),
        (
            "match = \"app.namespace == 'prod'\"\ndeny = '!has(app.memory)'",
            "image = 'web:v1'\nnamespace = 'prod'",
            vec![deny("rule")],
        ),
        (
            "match = \"app.namespace == 'prod'\"\ndeny = '!has(app.memory)'",
            "image = 'web:v1'\nnamespace = 'prod'\nmemory = '128Mi-512Mi'",
            vec![],
        ),
        // A warn policy warns and never refuses.
        (
            "deny = \"app.image.endsWith(':latest')\"\naction = 'warn'",
            "image = 'web:latest'",
            vec![warn("rule")],
        ),
        // Whole numbers are ints, so a port compares with a literal.
        (
            "deny = 'app.port == 8080'",
            "image = 'w:1'\nport = 8080",
            vec![deny("rule")],
        ),
        // Maps and lists: env keys and command words.
        (
            "deny = \"!('TEAM' in app.env)\"",
            "image = 'w:1'\nenv = { TEAM = 'payments' }",
            vec![],
        ),
        (
            "deny = \"app.command.exists(word, word == 'sh')\"",
            "image = 'w:1'\ncommand = ['sh', '-c', 'true']",
            vec![deny("rule")],
        ),
        // The app's own name is visible.
        (
            "deny = \"app.name == 'web'\"",
            "image = 'w:1'",
            vec![deny("rule")],
        ),
        // Who and how: the caller's role and the source.
        (
            "deny = \"caller.role == 'deployer'\"",
            "image = 'w:1'",
            vec![deny("rule")],
        ),
        ("deny = \"source == 'gitops'\"", "image = 'w:1'", vec![]),
        // Reading a field the app doesn't set fails, and fails closed.
        (
            "deny = \"app.memory == '1Gi'\"",
            "image = 'w:1'",
            vec![deny("rule")],
        ),
        // So does a deny that isn't a boolean.
        ("deny = \"'yes'\"", "image = 'w:1'", vec![deny("rule")]),
        // A broken warn policy only warns.
        (
            "deny = \"app.memory == '1Gi'\"\naction = 'warn'",
            "image = 'w:1'",
            vec![warn("rule")],
        ),
    ];
    for (policy, app, expected) in rows {
        let policy_toml = if policy.is_empty() {
            String::new()
        } else {
            format!("[policy.rule]\n{policy}\n")
        };
        let set = compile(&policy_toml);
        let config = apps(&format!("[app.web]\n{app}\n"));
        let verdict = evaluate(&set, &config, &deployer(), Source::Apply);
        let got: Vec<Decision> = decisions(&verdict)
            .into_iter()
            .map(|(policy, action, _)| (policy, action))
            .collect();
        assert_eq!(
            got, expected,
            "policy {policy:?} on app {app:?}: {verdict:?}"
        );
    }
}

#[test]
fn a_denial_carries_the_policy_message_or_names_the_policy() {
    let set = compile(LIMITS_AND_LATEST);
    let config = apps("[app.web]\nimage = 'web:latest'\nnamespace = 'prod'\n");
    let verdict = evaluate(&set, &config, &deployer(), Source::Apply);
    assert_eq!(verdict.denials.len(), 1);
    assert_eq!(
        verdict.denials[0].message,
        "apps in prod need a memory limit"
    );
    assert_eq!(
        verdict.denials[0].to_string(),
        "policy prod-needs-limits denies app prod/web: apps in prod need a memory limit"
    );
    assert_eq!(verdict.warnings.len(), 1);
    assert_eq!(verdict.warnings[0].message, "breaks policy no-latest");
}

#[test]
fn a_failed_evaluation_says_why() {
    let set = compile("[policy.rule]\ndeny = \"app.memory == '1Gi'\"\n");
    let verdict = evaluate(
        &set,
        &apps("[app.web]\nimage = 'w:1'\n"),
        &deployer(),
        Source::Apply,
    );
    assert!(
        verdict.denials[0]
            .message
            .starts_with("couldn't evaluate: deny"),
        "{verdict:?}"
    );
}

#[test]
fn a_gitops_sync_is_the_system_caller() {
    let set = compile(
        "[policy.rule]\ndeny = \"caller.role == 'system' && caller.name == 'gitops' && source == 'gitops'\"\n",
    );
    let config = apps("[app.web]\nimage = 'w:1'\n");
    assert!(!evaluate(&set, &config, &Caller::gitops(), Source::GitOps).is_allowed());
    assert!(evaluate(&set, &config, &deployer(), Source::Apply).is_allowed());
}

/// Every category of bad policy is refused when it's committed.
#[test]
fn every_kind_of_bad_policy_is_refused_at_commit() {
    let long = format!("app.name == '{}'", "x".repeat(MAX_EXPRESSION_BYTES));
    let long_message = "m".repeat(MAX_MESSAGE_BYTES + 1);
    let cases: Vec<(&str, PolicySpec, ErrorCheck)> = vec![
        ("Bad_Name", spec(None, "true"), |e| {
            matches!(e, PolicyError::InvalidName { .. })
        }),
        ("rule", spec(None, "  "), |e| {
            matches!(
                e,
                PolicyError::Empty {
                    field: PolicyField::Deny
                }
            )
        }),
        ("rule", spec(Some(""), "true"), |e| {
            matches!(
                e,
                PolicyError::Empty {
                    field: PolicyField::Match
                }
            )
        }),
        ("rule", spec(None, "app.name ==="), |e| {
            matches!(
                e,
                PolicyError::Unparseable {
                    field: PolicyField::Deny,
                    ..
                }
            )
        }),
        ("rule", spec(Some("app.name == ("), "true"), |e| {
            matches!(
                e,
                PolicyError::Unparseable {
                    field: PolicyField::Match,
                    ..
                }
            )
        }),
        ("rule", spec(None, &long), |e| {
            matches!(
                e,
                PolicyError::TooLong {
                    field: PolicyField::Deny,
                    ..
                }
            )
        }),
        ("rule", spec(Some(&long), "true"), |e| {
            matches!(
                e,
                PolicyError::TooLong {
                    field: PolicyField::Match,
                    ..
                }
            )
        }),
        (
            "rule",
            PolicySpec {
                message: Some(long_message.clone()),
                ..spec(None, "true")
            },
            |e| matches!(e, PolicyError::MessageTooLong { .. }),
        ),
        (
            "rule",
            spec(None, "!has(app.memroy)"),
            |e| matches!(e, PolicyError::UnknownField { name, .. } if name == "memroy"),
        ),
        (
            "rule",
            spec(None, "caller.email == 'x'"),
            |e| matches!(e, PolicyError::UnknownField { variable, .. } if variable == "caller"),
        ),
        // A policy can never match policies, permissions or namespaces:
        // it can't even name them.
        (
            "rule",
            spec(None, "policy.name == 'rule'"),
            |e| matches!(e, PolicyError::UnknownVariable { variable, .. } if variable == "policy"),
        ),
        (
            "rule",
            spec(Some("has(permission.actions)"), "true"),
            |e| matches!(e, PolicyError::UnknownVariable { variable, .. } if variable == "permission"),
        ),
        (
            "rule",
            spec(None, "namespaces.prod.cpu == '1'"),
            |e| matches!(e, PolicyError::UnknownVariable { variable, .. } if variable == "namespaces"),
        ),
        // `namespace` alone is a reserved word in CEL, refused as it parses.
        ("rule", spec(None, "namespace.cpu == '1'"), |e| {
            matches!(e, PolicyError::Unparseable { .. })
        }),
        // A loop variable doesn't leak out of its loop.
        (
            "rule",
            spec(None, "app.command.all(w, w != 'x') && w == 'y'"),
            |e| matches!(e, PolicyError::UnknownVariable { variable, .. } if variable == "w"),
        ),
    ];
    for (name, policy, expected) in cases {
        let error = check_policy(name, &policy).unwrap_err();
        assert!(expected(&error), "{name} {policy:?}: {error}");
        // The same policy in a config fails validation, so neither
        // `relish apply` nor GitOps ever commits it.
        let mut config = Config::default();
        config.policy.insert(name.to_string(), policy);
        assert!(config.validate().is_err());
    }
}

fn spec(match_expression: Option<&str>, deny: &str) -> PolicySpec {
    PolicySpec {
        match_expression: match_expression.map(str::to_string),
        deny: deny.to_string(),
        message: None,
        action: PolicyAction::Deny,
    }
}

#[test]
fn good_policies_pass_the_commit_checks() {
    for (match_expression, deny) in [
        (Some("app.namespace == 'prod'"), "!has(app.memory)"),
        (None, "app.image.endsWith(':latest')"),
        (None, "app.command.all(word, word != 'sh') == false"),
        (None, "app.image.lowerAscii().startsWith('docker.io/')"),
        (None, "type(app.port) == int"),
        (Some("source == 'gitops'"), "caller.role != 'system'"),
        (None, "app.volumes.exists(v, has(v.size))"),
    ] {
        check_policy("rule", &spec(match_expression, deny))
            .unwrap_or_else(|error| panic!("{deny}: {error}"));
    }
}

/// `APP_FIELDS` must name exactly the keys an `[app.*]` table takes, plus
/// `name`. serde lists the keys when it refuses an unknown one.
#[test]
fn app_fields_match_the_app_spec() {
    let error = toml::from_str::<AppSpec>("zzz = 1")
        .unwrap_err()
        .to_string();
    let expected = error
        .split("expected one of ")
        .nth(1)
        .unwrap_or_else(|| panic!("{error}"));
    let mut keys: Vec<&str> = expected
        .split(',')
        .map(|key| key.trim().trim_matches('`'))
        .filter(|key| !key.is_empty())
        .map(|key| key.split('`').next().unwrap_or(key))
        .collect();
    keys.push("name");
    keys.sort();
    let mut fields: Vec<&str> = APP_FIELDS.to_vec();
    fields.sort();
    assert_eq!(keys, fields);
}

#[test]
fn admit_fences_app_writes_on_the_generation_after_its_own_policies() {
    let committed = policies("[policy.old]\ndeny = 'false'\n");
    let config = apps(
        r#"
        [namespace.prod]
        [policy.new]
        deny = "app.image.endsWith(':latest')"
        [app.web]
        image = "web:v1"
        namespace = "prod"
        "#,
    );
    let admitted = admit(
        &committed,
        41,
        &config,
        &deployer(),
        Source::Apply,
        config_to_desired_writes(&config),
    )
    .unwrap();
    assert_eq!(admitted.generation, 42);
    assert!(matches!(
        admitted.writes[0],
        RaftRequest::NamespaceSpec { .. }
    ));
    assert!(matches!(admitted.writes[1], RaftRequest::PolicySpec { .. }));
    match &admitted.writes[2] {
        RaftRequest::PolicyFenced { generation, change } => {
            assert_eq!(*generation, 42);
            assert!(matches!(change.as_ref(), RaftRequest::AppSpec { .. }));
        }
        other => panic!("expected a fenced app write, got {other:?}"),
    }
}

#[test]
fn admit_judges_apps_by_the_policies_in_the_same_config() {
    let config = apps(
        r#"
        [policy.no-latest]
        deny = "app.image.endsWith(':latest')"
        [app.web]
        image = "web:latest"
        "#,
    );
    let error = admit(
        &BTreeMap::new(),
        0,
        &config,
        &deployer(),
        Source::Apply,
        config_to_desired_writes(&config),
    )
    .unwrap_err();
    assert!(matches!(error, AdmitError::Denied(_)), "{error}");
    assert!(
        error
            .to_string()
            .contains("policy no-latest denies app default/web")
    );
}

#[test]
fn a_broken_committed_policy_fails_apps_closed_but_not_its_own_replacement() {
    let mut committed = BTreeMap::new();
    committed.insert("old".to_string(), spec(None, "app.name ==="));
    let app_only = apps("[app.web]\nimage = 'w:1'\n");
    let error = admit(
        &committed,
        3,
        &app_only,
        &deployer(),
        Source::Apply,
        config_to_desired_writes(&app_only),
    )
    .unwrap_err();
    assert!(matches!(&error, AdmitError::Broken { name, .. } if name == "old"));
    assert!(error.to_string().contains("relish policy delete old"));

    // A config that replaces the broken policy is judged by its replacement.
    let fix = apps("[policy.old]\ndeny = 'false'\n[app.web]\nimage = 'w:1'\n");
    admit(
        &committed,
        3,
        &fix,
        &deployer(),
        Source::Apply,
        config_to_desired_writes(&fix),
    )
    .unwrap();
}

#[test]
fn fence_leaves_everything_but_apps_alone() {
    let writes = vec![
        RaftRequest::NamespaceDelete {
            name: "prod".into(),
        },
        RaftRequest::AppSpec {
            app_id: AppId::new("web", "default"),
            spec: Box::new(apps("[app.web]\nimage = 'w:1'\n").app["web"].clone()),
        },
    ];
    let fenced = fence(writes, 9);
    assert!(matches!(fenced[0], RaftRequest::NamespaceDelete { .. }));
    assert!(matches!(
        fenced[1],
        RaftRequest::PolicyFenced { generation: 9, .. }
    ));
}

#[test]
fn the_test_report_lists_every_finding_and_the_outcome() {
    let set = compile(LIMITS_AND_LATEST);
    let config = apps(
        r#"
        [app.web]
        image = "web:latest"
        namespace = "prod"
        [app.api]
        image = "api:v2"
        namespace = "prod"
        memory = "256Mi"
        "#,
    );
    let verdict = evaluate(&set, &config, &deployer(), Source::Apply);
    insta::assert_snapshot!(report::render_test(&verdict, 2, 2));
}

#[test]
fn the_test_report_for_a_clean_config() {
    let set = compile(LIMITS_AND_LATEST);
    let config = apps("[app.web]\nimage = 'web:v1'\n");
    let verdict = evaluate(&set, &config, &deployer(), Source::Apply);
    insta::assert_snapshot!(report::render_test(&verdict, 1, 2));
}

/// An app for the property tests: a name, a namespace and an image tag,
/// some of them `latest`.
fn arbitrary_app() -> impl Strategy<Value = (String, String, String)> {
    (
        "[a-z]{1,8}",
        prop_oneof!["default", "prod", "staging"],
        prop_oneof!["latest", "v1", "v2"],
    )
        .prop_map(|(name, namespace, tag)| (name, namespace.to_string(), tag.to_string()))
}

fn config_of(apps_list: &[(String, String, String)]) -> Config {
    let mut toml = String::new();
    let mut seen = std::collections::BTreeSet::new();
    for (name, namespace, tag) in apps_list {
        if !seen.insert(name) {
            continue;
        }
        toml.push_str(&format!(
            "[app.{name}]\nimage = 'img:{tag}'\nnamespace = '{namespace}'\n"
        ));
    }
    Config::parse(&toml).unwrap()
}

proptest! {
    /// A denied config produces no writes at all, and an admitted one
    /// fences every app.
    #[test]
    fn a_denied_config_produces_no_writes(
        apps_list in proptest::collection::vec(arbitrary_app(), 1..6)
    ) {
        let config = config_of(&apps_list);
        let committed = policies(
            "[policy.no-latest]\nmatch = \"app.namespace == 'prod'\"\ndeny = \"app.image.endsWith(':latest')\"\n",
        );
        let outcome = admit(
            &committed,
            5,
            &config,
            &deployer(),
            Source::Apply,
            config_to_desired_writes(&config),
        );
        let breaks_rule = config.app.values().any(|spec| {
            spec.namespace.as_deref() == Some("prod")
                && spec.image.as_deref().is_some_and(|image| image.ends_with(":latest"))
        });
        match outcome {
            Err(AdmitError::Denied(denied)) => {
                prop_assert!(breaks_rule);
                prop_assert!(!denied.denials.is_empty());
            }
            Err(other) => prop_assert!(false, "unexpected {other}"),
            Ok(admitted) => {
                prop_assert!(!breaks_rule);
                prop_assert_eq!(admitted.writes.len(), config.app.len());
                let all_fenced = admitted
                    .writes
                    .iter()
                    .all(|write| matches!(write, RaftRequest::PolicyFenced { generation: 5, .. }));
                prop_assert!(all_fenced);
            }
        }
    }

    /// The verdict doesn't depend on the order policies were declared or
    /// inserted in: they're kept, and evaluated, in name order.
    #[test]
    fn evaluation_order_does_not_depend_on_map_order(
        apps_list in proptest::collection::vec(arbitrary_app(), 1..5),
        order in Just(vec![0usize, 1, 2, 3]).prop_shuffle(),
    ) {
        let rules = [
            ("a-latest", "app.image.endsWith(':latest')", PolicyAction::Deny),
            ("b-prod", "app.namespace == 'prod'", PolicyAction::Warn),
            ("c-short", "size(app.name) < 3", PolicyAction::Warn),
            ("d-never", "false", PolicyAction::Deny),
        ];
        let mut shuffled = BTreeMap::new();
        let mut hashed = std::collections::HashMap::new();
        for index in &order {
            let (name, deny, action) = rules[*index];
            let policy = PolicySpec { action, ..spec(None, deny) };
            shuffled.insert(name.to_string(), policy.clone());
            hashed.insert(name.to_string(), policy);
        }
        let in_order: BTreeMap<_, _> = rules
            .iter()
            .map(|(name, deny, action)| {
                (name.to_string(), PolicySpec { action: *action, ..spec(None, deny) })
            })
            .collect();
        let from_hash: BTreeMap<_, _> = hashed.into_iter().collect();
        let config = config_of(&apps_list);
        let expected = evaluate(
            &PolicySet::compile(&in_order).unwrap(),
            &config,
            &deployer(),
            Source::Apply,
        );
        for set in [&shuffled, &from_hash] {
            let verdict = evaluate(
                &PolicySet::compile(set).unwrap(),
                &config,
                &deployer(),
                Source::Apply,
            );
            prop_assert_eq!(&verdict, &expected);
        }
    }
}
