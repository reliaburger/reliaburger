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
                format!("{}/{}", finding.workload.namespace, finding.workload.name),
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
        // Stop hooks are app fields like any other (#1121).
        (
            "deny = \"has(app.stop_signal) && size(app.pre_stop) == 0\"",
            "image = 'w:1'\nstop_signal = 'SIGQUIT'",
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
        let verdict = evaluate(&set, &config, &deployer(), Source::Apply).verdict;
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
    let verdict = evaluate(&set, &config, &deployer(), Source::Apply).verdict;
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
    )
    .verdict;
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
    assert!(
        !evaluate(&set, &config, &Caller::gitops(), Source::GitOps)
            .verdict
            .is_allowed()
    );
    assert!(
        evaluate(&set, &config, &deployer(), Source::Apply)
            .verdict
            .is_allowed()
    );
}

/// #1125: a policy sees an app's `owner`, so a platform team can say, for
/// example, that only the database controller writes apps in `pg`.
#[test]
fn a_policy_sees_the_owner() {
    let set = compile(
        "[policy.pg-is-the-controllers]\nmatch = \"app.namespace == 'pg'\"\n\
         deny = \"!has(app.owner) || app.owner != 'pg-failover'\"\n",
    );
    let owned = apps("[app.db]\nimage = 'pg:17'\nnamespace = 'pg'\nowner = 'pg-failover'\n");
    let unowned = apps("[app.db]\nimage = 'pg:17'\nnamespace = 'pg'\n");
    let controller = Caller::token("pg-failover", ApiRole::Deployer);
    assert!(
        evaluate(&set, &owned, &controller, Source::Apply)
            .verdict
            .is_allowed()
    );
    assert!(
        !evaluate(&set, &unowned, &deployer(), Source::Apply)
            .verdict
            .is_allowed()
    );
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
        deny: Some(deny.to_string()),
        message: None,
        action: PolicyAction::Deny,
        default: None,
        applies_to: PolicyTarget::Apps,
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
        RaftRequest::PolicyFenced {
            generation, change, ..
        } => {
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
    let fenced = fence(writes, 9, &BTreeMap::new());
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
    let evaluation = evaluate(&set, &config, &deployer(), Source::Apply);
    insta::assert_snapshot!(report::render_test(&evaluation, 2, 0, 2));
}

#[test]
fn the_test_report_for_a_clean_config() {
    let set = compile(LIMITS_AND_LATEST);
    let config = apps("[app.web]\nimage = 'web:v1'\n");
    let evaluation = evaluate(&set, &config, &deployer(), Source::Apply);
    insta::assert_snapshot!(report::render_test(&evaluation, 1, 0, 2));
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
        ).verdict;
        for set in [&shuffled, &from_hash] {
            let verdict = evaluate(
                &PolicySet::compile(set).unwrap(),
                &config,
                &deployer(),
                Source::Apply,
            ).verdict;
            prop_assert_eq!(&verdict, &expected);
        }
    }
}

// ---------------------------------------------------------------------------
// Fill-only defaults (#1118)
// ---------------------------------------------------------------------------

const TEAM_LABEL: &str = r#"
[policy.team-label]
match = "app.namespace == 'payments'"

[policy.team-label.default]
env.TEAM = "payments"
deploy.max_unavailable = 1
"#;

/// A filled field as `(policy, field, value)`.
type Fill = (String, String, String);

/// The filled fields as `(policy, field, value)` triples.
fn fills(filled: &[Filled]) -> Vec<Fill> {
    filled
        .iter()
        .map(|fill| {
            (
                fill.policy.clone(),
                fill.field.clone(),
                fill.value.to_string(),
            )
        })
        .collect()
}

fn judge(policies_toml: &str, app_toml: &str) -> Judgement {
    let set = compile(policies_toml);
    let config = apps(&format!("[app.web]\n{app_toml}\n"));
    let spec = &config.app["web"];
    judge_app(
        &set,
        &app_id_of("web", spec),
        spec,
        &deployer(),
        Source::Apply,
    )
}

/// The defaults table: one row per kind of fill.
#[test]
fn the_default_table() {
    let fill = |policy: &str, field: &str, value: &str| {
        (policy.to_string(), field.to_string(), value.to_string())
    };
    let rows: Vec<(&str, &str, Vec<Fill>)> = vec![
        // A default fills a field the app leaves out, key by key.
        (
            TEAM_LABEL,
            "image = 'w:1'\nnamespace = 'payments'",
            vec![
                fill("team-label", "deploy.max_unavailable", "1"),
                fill("team-label", "env.TEAM", "\"payments\""),
            ],
        ),
        // `match` decides, as it does for deny.
        (TEAM_LABEL, "image = 'w:1'", vec![]),
        // A value the app sets wins; its other keys are left alone.
        (
            TEAM_LABEL,
            "image = 'w:1'\nnamespace = 'payments'\nenv = { TEAM = 'mine', OWNER = 'me' }\n[app.web.deploy]\nmax_unavailable = 3",
            vec![],
        ),
        // An existing table gains only the keys it lacks.
        (
            TEAM_LABEL,
            "image = 'w:1'\nnamespace = 'payments'\nenv = { OWNER = 'me' }\n[app.web.deploy]\nmax_surge = 2",
            vec![
                fill("team-label", "deploy.max_unavailable", "1"),
                fill("team-label", "env.TEAM", "\"payments\""),
            ],
        ),
        // Two defaults for one field: the first by name fills it.
        (
            "[policy.b-second.default]\nmemory = '1Gi'\n[policy.a-first.default]\nmemory = '256Mi'\n",
            "image = 'w:1'",
            vec![fill("a-first", "memory", "\"256Mi\"")],
        ),
        // A match sees what the policies before it filled.
        (
            "[policy.a-port.default]\nport = 8080\n[policy.b-health]\nmatch = 'has(app.port)'\n[policy.b-health.default]\nhealth = { path = '/healthz' }\n",
            "image = 'w:1'",
            vec![
                fill("a-port", "port", "8080"),
                fill("b-health", "health.path", "\"/healthz\""),
            ],
        ),
    ];
    for (policies_toml, app_toml, expected) in rows {
        let judgement = judge(policies_toml, app_toml);
        assert!(
            judgement.verdict.is_allowed(),
            "{app_toml}: {:?}",
            judgement.verdict
        );
        assert_eq!(fills(&judgement.filled), expected, "{app_toml}");
    }
}

#[test]
fn a_filled_field_reads_as_the_policy_that_filled_it() {
    let judgement = judge(TEAM_LABEL, "image = 'w:1'\nnamespace = 'payments'");
    assert_eq!(
        judgement.filled[1].to_string(),
        "policy team-label fills env.TEAM = \"payments\" on app payments/web"
    );
    assert_eq!(
        judgement.spec.env["TEAM"],
        crate::config::EnvValue::Plain("payments".into())
    );
}

/// Validation and every deny see the spec with its defaults filled in.
#[test]
fn deny_and_validation_see_the_filled_spec() {
    let policies_toml = r#"
        [policy.a-limits.default]
        memory = "256Mi"
        [policy.z-needs-limits]
        deny = "!has(app.memory)"
    "#;
    assert!(judge(policies_toml, "image = 'w:1'").verdict.is_allowed());

    // Port 0 has the right type, so it commits, but no app may use it.
    let judgement = judge("[policy.zero.default]\nport = 0\n", "image = 'w:1'");
    assert_eq!(judgement.verdict.denials.len(), 1);
    let denial = &judgement.verdict.denials[0];
    assert_eq!(denial.policy, "zero");
    assert!(
        denial
            .message
            .starts_with("the app isn't valid once its defaults are filled in"),
        "{denial}"
    );
}

#[test]
fn a_policy_with_only_a_default_never_denies() {
    let judgement = judge("[policy.fill.default]\nmemory = '256Mi'\n", "image = 'w:1'");
    assert!(judgement.verdict.denials.is_empty());
    assert!(judgement.verdict.warnings.is_empty());
}

/// Every category of bad `default` is refused when it's committed.
#[test]
fn every_kind_of_bad_default_is_refused_at_commit() {
    let cases: Vec<(&str, ErrorCheck)> = vec![
        ("[policy.rule]\nmatch = 'true'\n", |e| {
            matches!(e, PolicyError::NothingToDo)
        }),
        ("[policy.rule]\ndefault = {}\n", |e| {
            matches!(
                e,
                PolicyError::Empty {
                    field: PolicyField::Default
                }
            )
        }),
        (
            "[policy.rule.default]\nmemroy = '1Gi'\n",
            |e| matches!(e, PolicyError::DefaultField { name, .. } if name == "memroy"),
        ),
        (
            "[policy.rule.default]\nnamespace = 'prod'\n",
            |e| matches!(e, PolicyError::DefaultField { name, .. } if name == "namespace"),
        ),
        (
            "[policy.rule.default]\nreplicas = 3\n",
            |e| matches!(e, PolicyError::DefaultField { name, .. } if name == "replicas"),
        ),
        // A default never grants what the caller's token gates.
        (
            "[policy.rule.default]\nexec = '/bin/sh'\n",
            |e| matches!(e, PolicyError::DefaultField { name, .. } if name == "exec"),
        ),
        (
            "[policy.rule.default]\nruntime.sysctls = { 'net.core.somaxconn' = '4096' }\n",
            |e| matches!(e, PolicyError::DefaultField { name, .. } if name == "runtime"),
        ),
        // Only the owner's token or an admin claims an app (#1125).
        (
            "[policy.rule.default]\nowner = 'db-failover'\n",
            |e| matches!(e, PolicyError::DefaultField { name, .. } if name == "owner"),
        ),
        (
            "[policy.rule.default]\nsidecar = []\n",
            |e| matches!(e, PolicyError::DefaultField { name, .. } if name == "sidecar"),
        ),
        (
            "[policy.rule.default]\ndeploy.max_unavaliable = 1\n",
            |e| matches!(e, PolicyError::InvalidDefault { error } if error.contains("max_unavaliable")),
        ),
        ("[policy.rule.default]\nport = 'eighty'\n", |e| {
            matches!(e, PolicyError::InvalidDefault { .. })
        }),
        ("[policy.rule.default]\nenv.TEAM = 7\n", |e| {
            matches!(e, PolicyError::InvalidDefault { .. })
        }),
    ];
    for (toml, expected) in cases {
        let config: Config = toml::from_str(toml).unwrap();
        let (name, policy) = config.policy.iter().next().unwrap();
        let error = check_policy(name, policy).unwrap_err();
        assert!(expected(&error), "{toml}: {error}");
        assert!(config.validate().is_err(), "{toml}");
    }
    let huge = format!(
        "[policy.rule.default]\nenv.BIG = '{}'\n",
        "x".repeat(MAX_DEFAULT_BYTES)
    );
    let config: Config = toml::from_str(&huge).unwrap();
    assert!(matches!(
        check_policy("rule", &config.policy["rule"]),
        Err(PolicyError::TooLong {
            field: PolicyField::Default,
            ..
        })
    ));
}

#[test]
fn admit_writes_the_filled_spec_and_fences_it_with_the_declared_one() {
    let config = apps(&format!(
        "{TEAM_LABEL}\n[app.web]\nimage = 'web:v1'\nnamespace = 'payments'\n[app.api]\nimage = 'api:v1'\n"
    ));
    let admitted = admit(
        &BTreeMap::new(),
        0,
        &config,
        &deployer(),
        Source::Apply,
        config_to_desired_writes(&config),
    )
    .unwrap();
    let web = AppId::new("web", "payments");
    assert_eq!(admitted.defaulted.len(), 1);
    assert_eq!(admitted.defaulted[&web].declared, config.app["web"]);
    assert_eq!(admitted.filled.len(), 2);
    let mut fenced_apps = 0;
    for write in &admitted.writes {
        let RaftRequest::PolicyFenced {
            change, declared, ..
        } = write
        else {
            continue;
        };
        let RaftRequest::AppSpec { app_id, spec } = change.as_ref() else {
            panic!("expected an app write, got {change:?}");
        };
        fenced_apps += 1;
        if *app_id == web {
            assert_eq!(
                spec.env["TEAM"],
                crate::config::EnvValue::Plain("payments".into())
            );
            assert_eq!(declared.as_deref(), Some(&config.app["web"]));
        } else {
            assert_eq!(declared, &None, "an app no default touched carries none");
            assert!(spec.env.is_empty());
        }
    }
    assert_eq!(fenced_apps, 2);
}

#[test]
fn the_test_report_lists_filled_fields() {
    let set = compile(&format!("{TEAM_LABEL}\n{LIMITS_AND_LATEST}"));
    let config = apps(
        r#"
        [app.web]
        image = "web:v1"
        namespace = "payments"
        "#,
    );
    let evaluation = evaluate(&set, &config, &deployer(), Source::Apply);
    insta::assert_snapshot!(report::render_test(&evaluation, 1, 0, 3));
}

/// An app for the default property tests: which of the fields the
/// defaults name it sets itself.
#[derive(Debug, Clone)]
struct SometimesSet {
    namespace: &'static str,
    memory: Option<&'static str>,
    team: Option<&'static str>,
    owner: Option<&'static str>,
    port: Option<u16>,
    max_unavailable: Option<u32>,
}

fn sometimes_set() -> impl Strategy<Value = SometimesSet> {
    (
        prop_oneof![Just("default"), Just("payments")],
        proptest::option::of(prop_oneof![Just("128Mi"), Just("2Gi")]),
        proptest::option::of(prop_oneof![Just("mine"), Just("theirs")]),
        proptest::option::of(Just("me")),
        proptest::option::of(1u16..9000),
        proptest::option::of(0u32..5),
    )
        .prop_map(
            |(namespace, memory, team, owner, port, max_unavailable)| SometimesSet {
                namespace,
                memory,
                team,
                owner,
                port,
                max_unavailable,
            },
        )
}

fn app_from(app: &SometimesSet) -> AppSpec {
    let mut toml = format!("image = 'w:1'\nnamespace = '{}'\n", app.namespace);
    if let Some(memory) = app.memory {
        toml.push_str(&format!("memory = '{memory}'\n"));
    }
    if let Some(port) = app.port {
        toml.push_str(&format!("port = {port}\n"));
    }
    let env: Vec<String> = [("TEAM", app.team), ("OWNER", app.owner)]
        .iter()
        .filter_map(|(key, value)| value.map(|value| format!("{key} = '{value}'")))
        .collect();
    if !env.is_empty() {
        toml.push_str(&format!("env = {{ {} }}\n", env.join(", ")));
    }
    if let Some(max_unavailable) = app.max_unavailable {
        toml.push_str(&format!(
            "deploy = {{ max_unavailable = {max_unavailable} }}\n"
        ));
    }
    toml::from_str(&toml).unwrap()
}

/// Default policies that overlap on purpose: two fill `memory`, two fill
/// `env.TEAM`, and some only match `payments`.
const OVERLAPPING_DEFAULTS: [(&str, Option<&str>, &str); 4] = [
    (
        "a-team",
        Some("app.namespace == 'payments'"),
        "env.TEAM = 'payments'\ndeploy.max_unavailable = 1",
    ),
    ("b-limits", None, "memory = '256Mi'\nport = 8080"),
    (
        "c-other-team",
        None,
        "env.TEAM = 'platform'\nenv.OWNER = 'sre'",
    ),
    (
        "d-big",
        Some("app.namespace == 'payments'"),
        "memory = '1Gi'",
    ),
];

fn default_policy(match_expression: Option<&str>, default: &str) -> PolicySpec {
    let table: toml::Table = toml::from_str(default).unwrap();
    PolicySpec {
        match_expression: match_expression.map(str::to_string),
        deny: None,
        message: None,
        action: PolicyAction::Deny,
        applies_to: PolicyTarget::Apps,
        default: Some(
            serde_json::to_value(table)
                .unwrap()
                .as_object()
                .unwrap()
                .clone(),
        ),
    }
}

/// Whether every value `declared` holds is in `effective`, unchanged.
fn contained(declared: &serde_json::Value, effective: &serde_json::Value) -> bool {
    match (declared, effective) {
        (serde_json::Value::Object(declared), serde_json::Value::Object(effective)) => {
            declared.iter().all(|(key, value)| {
                value.is_null()
                    || effective
                        .get(key)
                        .is_some_and(|kept| contained(value, kept))
            })
        }
        _ => declared == effective,
    }
}

proptest! {
    /// Defaulting twice is defaulting once: the filled spec has nothing
    /// left to fill.
    #[test]
    fn defaulting_is_idempotent(app in sometimes_set()) {
        let specs: BTreeMap<String, PolicySpec> = OVERLAPPING_DEFAULTS
            .iter()
            .map(|(name, matching, default)| (name.to_string(), default_policy(*matching, default)))
            .collect();
        let set = PolicySet::compile(&specs).unwrap();
        let declared = app_from(&app);
        let app_id = app_id_of("web", &declared);
        let once = judge_app(&set, &app_id, &declared, &deployer(), Source::Apply);
        let twice = judge_app(&set, &app_id, &once.spec, &deployer(), Source::Apply);
        prop_assert_eq!(&twice.spec, &once.spec);
        prop_assert!(twice.filled.is_empty(), "{:?}", twice.filled);
    }

    /// A default never changes a field the user set: every value of the
    /// declared spec is in the filled one, unchanged, and no field the
    /// user set is reported as filled.
    #[test]
    fn a_default_never_changes_a_field_the_user_set(app in sometimes_set()) {
        let specs: BTreeMap<String, PolicySpec> = OVERLAPPING_DEFAULTS
            .iter()
            .map(|(name, matching, default)| (name.to_string(), default_policy(*matching, default)))
            .collect();
        let set = PolicySet::compile(&specs).unwrap();
        let declared = app_from(&app);
        let judgement = judge_app(&set, &app_id_of("web", &declared), &declared, &deployer(), Source::Apply);
        let declared_json = serde_json::to_value(&declared).unwrap();
        let effective_json = serde_json::to_value(&judgement.spec).unwrap();
        prop_assert!(contained(&declared_json, &effective_json), "{declared_json} vs {effective_json}");
        for fill in &judgement.filled {
            let path: Vec<&str> = fill.field.split('.').collect();
            let user_value = path
                .iter()
                .try_fold(&declared_json, |value, key| value.get(*key));
            prop_assert!(user_value.is_none_or(serde_json::Value::is_null), "{} was the user's", fill.field);
        }
    }

    /// The filled spec doesn't depend on the order the policies were
    /// declared or inserted in: they fill in name order.
    #[test]
    fn default_order_does_not_depend_on_map_order(
        app in sometimes_set(),
        order in Just(vec![0usize, 1, 2, 3]).prop_shuffle(),
    ) {
        let mut shuffled = BTreeMap::new();
        let mut hashed = std::collections::HashMap::new();
        for index in &order {
            let (name, matching, default) = OVERLAPPING_DEFAULTS[*index];
            shuffled.insert(name.to_string(), default_policy(matching, default));
            hashed.insert(name.to_string(), default_policy(matching, default));
        }
        let in_order: BTreeMap<String, PolicySpec> = OVERLAPPING_DEFAULTS
            .iter()
            .map(|(name, matching, default)| (name.to_string(), default_policy(*matching, default)))
            .collect();
        let from_hash: BTreeMap<String, PolicySpec> = hashed.into_iter().collect();
        let declared = app_from(&app);
        let app_id = app_id_of("web", &declared);
        let expected = judge_app(&PolicySet::compile(&in_order).unwrap(), &app_id, &declared, &deployer(), Source::Apply);
        for specs in [&shuffled, &from_hash] {
            let judgement = judge_app(&PolicySet::compile(specs).unwrap(), &app_id, &declared, &deployer(), Source::Apply);
            prop_assert_eq!(&judgement, &expected);
        }
        // The first policy by name wins a field two of them fill.
        if app.memory.is_none() {
            prop_assert!(expected.filled.iter().any(|fill| fill.field == "memory" && fill.policy == "b-limits"));
        }
        if app.team.is_none() && app.namespace == "payments" {
            prop_assert!(expected.filled.iter().any(|fill| fill.field == "env.TEAM" && fill.policy == "a-team"));
        }
    }
}

/// #1118: the fence carries the declared spec whenever the stored one
/// differs from it, by a default or by an image bound to a digest, and
/// not when they're the same.
#[test]
fn fence_carries_the_declared_spec_only_when_the_stored_one_differs() {
    let config = apps("[app.web]\nimage = 'web:v1'\n[app.api]\nimage = 'api:v1'\n");
    let mut bound = config.clone();
    bound.app.get_mut("web").unwrap().image = Some(format!("web:v1@sha256:{}", "a".repeat(64)));
    let fenced = fence(config_to_desired_writes(&bound), 3, &declared_apps(&config));
    for write in &fenced {
        let RaftRequest::PolicyFenced {
            change, declared, ..
        } = write
        else {
            panic!("expected a fence, got {write:?}");
        };
        let RaftRequest::AppSpec { app_id, .. } = change.as_ref() else {
            panic!("expected an app write");
        };
        match app_id.name.as_str() {
            "web" => assert_eq!(declared.as_deref(), Some(&config.app["web"])),
            _ => assert_eq!(declared, &None),
        }
    }
}

// ---------------------------------------------------------------------------
// Jobs, the audit action and `relish policy audit` (#1119)
// ---------------------------------------------------------------------------

/// A decision as `(policy, action, kind and workload)`.
type WorkloadDecision = (String, PolicyAction, String);

/// Every decision in `verdict`: denials, warnings, then audit findings.
fn workload_decisions(verdict: &Verdict) -> Vec<WorkloadDecision> {
    verdict
        .denials
        .iter()
        .chain(&verdict.warnings)
        .chain(&verdict.audits)
        .map(|finding| {
            (
                finding.policy.clone(),
                finding.action,
                format!(
                    "{} {}/{}",
                    finding.kind.as_str(),
                    finding.workload.namespace,
                    finding.workload.name
                ),
            )
        })
        .collect()
}

const WORKLOADS: &str = r#"
[app.web]
image = "web:latest"

[job.nightly]
image = "report:latest"
schedule = "0 3 * * *"
"#;

/// The jobs table: which policies judge which workloads, and how each
/// sees it.
#[test]
fn what_a_policy_applies_to_decides_what_it_judges() {
    let decision =
        |policy: &str, action, workload: &str| (policy.to_string(), action, workload.to_string());
    let rows: Vec<(&str, Vec<WorkloadDecision>)> = vec![
        // A policy without `applies_to` judges apps, as before.
        (
            "deny = \"app.image.endsWith(':latest')\"",
            vec![decision("rule", PolicyAction::Deny, "app default/web")],
        ),
        // A jobs policy sees the job as `job`, and only jobs.
        (
            "applies_to = 'jobs'\ndeny = \"job.image.endsWith(':latest') && has(job.schedule)\"",
            vec![decision("rule", PolicyAction::Deny, "job default/nightly")],
        ),
        // A policy for both sees each as `workload`.
        (
            "applies_to = 'both'\ndeny = \"workload.image.endsWith(':latest')\"",
            vec![
                decision("rule", PolicyAction::Deny, "app default/web"),
                decision("rule", PolicyAction::Deny, "job default/nightly"),
            ],
        ),
        // An apps policy may read `workload` too.
        (
            "deny = \"workload.name == 'web'\"",
            vec![decision("rule", PolicyAction::Deny, "app default/web")],
        ),
        // `audit` records, and refuses nothing.
        (
            "applies_to = 'both'\naction = 'audit'\ndeny = \"!has(workload.memory)\"",
            vec![
                decision("rule", PolicyAction::Audit, "app default/web"),
                decision("rule", PolicyAction::Audit, "job default/nightly"),
            ],
        ),
    ];
    for (policy, expected) in rows {
        let set = compile(&format!("[policy.rule]\n{policy}\n"));
        let evaluation = evaluate(&set, &apps(WORKLOADS), &deployer(), Source::Apply);
        assert_eq!(
            workload_decisions(&evaluation.verdict),
            expected,
            "{policy}"
        );
    }
}

#[test]
fn an_audit_finding_never_refuses_or_warns() {
    let set = compile(
        "[policy.limits]\naction = 'audit'\ndeny = '!has(app.memory)'\nmessage = 'set a memory limit'\n",
    );
    let verdict = evaluate(&set, &apps(WORKLOADS), &deployer(), Source::Apply).verdict;
    assert!(verdict.is_allowed());
    assert!(verdict.warnings.is_empty());
    assert_eq!(
        verdict.audits[0].to_string(),
        "policy limits audits app default/web: set a memory limit"
    );
}

#[test]
fn a_denied_job_is_named_as_a_job() {
    let config = apps(&format!(
        "[policy.pinned]\napplies_to = 'jobs'\ndeny = \"job.image.endsWith(':latest')\"\nmessage = 'pin a version'\n{WORKLOADS}"
    ));
    let error = admit(
        &BTreeMap::new(),
        0,
        &config,
        &deployer(),
        Source::Apply,
        Vec::new(),
    )
    .unwrap_err();
    assert_eq!(
        error.to_string(),
        "policy pinned denies job default/nightly: pin a version"
    );
}

/// What a policy may read depends on what it applies to, and a default
/// belongs to an apps policy only.
#[test]
fn every_kind_of_bad_job_policy_is_refused_at_commit() {
    let cases: Vec<(&str, ErrorCheck)> = vec![
        (
            "deny = \"job.image == 'x'\"",
            |e| matches!(e, PolicyError::UnknownVariable { variable, .. } if variable == "job"),
        ),
        (
            "applies_to = 'jobs'\ndeny = \"app.image == 'x'\"",
            |e| matches!(e, PolicyError::UnknownVariable { variable, .. } if variable == "app"),
        ),
        (
            "applies_to = 'both'\ndeny = \"app.image == 'x'\"",
            |e| matches!(e, PolicyError::UnknownVariable { variable, .. } if variable == "app"),
        ),
        (
            "applies_to = 'both'\ndeny = 'workload.port == 80'",
            |e| matches!(e, PolicyError::UnknownField { variable, name, .. } if variable == "workload" && name == "port"),
        ),
        (
            "applies_to = 'jobs'\ndeny = 'job.replicas == 2'",
            |e| matches!(e, PolicyError::UnknownField { name, .. } if name == "replicas"),
        ),
        (
            "applies_to = 'jobs'\n[policy.rule.default]\nmemory = '1Gi'",
            |e| matches!(e, PolicyError::DefaultOutsideApps { .. }),
        ),
    ];
    for (body, expected) in cases {
        let config: Config = toml::from_str(&format!("[policy.rule]\n{body}\n")).unwrap();
        let error = check_policy("rule", &config.policy["rule"]).unwrap_err();
        assert!(expected(&error), "{body}: {error}");
    }
    let error = check_policy(
        "rule",
        &toml::from_str::<Config>("[policy.rule]\ndeny = \"job.image == 'x'\"\n")
            .unwrap()
            .policy["rule"],
    )
    .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("a policy that applies to apps sees only app, workload, caller and source"),
        "{error}"
    );
}

/// `JOB_FIELDS` must name exactly the keys a `[job.*]` table takes, plus
/// `name`, read from serde's own list as `APP_FIELDS` is.
#[test]
fn job_fields_match_the_job_spec() {
    let error = toml::from_str::<crate::config::JobSpec>("zzz = 1")
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
    let mut fields: Vec<&str> = JOB_FIELDS.to_vec();
    fields.sort();
    assert_eq!(keys, fields);
}

/// `WORKLOAD_FIELDS` is exactly where apps and jobs meet, less `runtime`,
/// which is a word both use for different things: an app's kernel knobs
/// (#1122), a job's execution state.
#[test]
fn workload_fields_are_the_fields_apps_and_jobs_share() {
    let mut shared: Vec<&str> = APP_FIELDS
        .iter()
        .copied()
        .filter(|field| JOB_FIELDS.contains(field) && *field != "runtime")
        .collect();
    shared.sort();
    let mut fields: Vec<&str> = WORKLOAD_FIELDS.to_vec();
    fields.sort();
    assert_eq!(shared, fields);
}

fn committed(name: &str, toml: &str, gitops_owned: bool) -> audit::CommittedApp {
    audit::CommittedApp {
        id: AppId::new(name, "default"),
        spec: toml::from_str(toml).unwrap(),
        gitops_owned,
        controller: None,
    }
}

/// #1125: an app a controller owns is judged as the controller's token,
/// since only that token writes it. A policy that lets only the controller
/// write in a namespace passes the controller's app and flags another.
#[test]
fn the_audit_judges_a_controller_owned_app_as_its_controller() {
    let policies = policies(
        r#"
        [policy.only-the-controller]
        action = "audit"
        deny = "caller.name != 'db-failover'"
        "#,
    );
    let mut owned = committed("db", "image = 'pg:17'\nowner = 'db-failover'", false);
    owned.controller = Some(Caller::token("db-failover", ApiRole::Deployer));
    let unowned = committed("web", "image = 'web:v1'", false);
    let audit = audit::audit_workloads(&policies, 1, &[owned, unowned], &[]);
    assert_eq!(
        workload_decisions(&audit.verdict),
        vec![(
            "only-the-controller".into(),
            PolicyAction::Audit,
            "app default/web".into()
        )]
    );
}

/// #1125: the audit finds the controller's caller from the token its
/// `owner` names, role included; without that token it falls back to the
/// cluster's own `audit` caller.
#[test]
fn the_audit_reads_the_controllers_role_from_its_token() {
    let mut desired = crate::council::types::DesiredState {
        policies: policies(
            "[policy.deployers-only]\naction = \"audit\"\ndeny = \"caller.role != 'deployer'\"\n",
        ),
        ..Default::default()
    };
    for (name, owner) in [("db", "db-failover"), ("orphan", "revoked")] {
        let mut spec: AppSpec = toml::from_str("image = 'pg:17'").unwrap();
        spec.owner = Some(owner.to_string());
        desired.apps.insert(AppId::new(name, "default"), spec);
    }
    let token = crate::sesame::token::create_token(
        "db-failover",
        ApiRole::Deployer,
        crate::sesame::types::TokenScope::default(),
        None,
    )
    .unwrap();
    desired.security_state.api_tokens.push(token.token);
    let audit = audit::audit(&desired);
    assert_eq!(
        workload_decisions(&audit.verdict),
        vec![(
            "deployers-only".into(),
            PolicyAction::Audit,
            "app default/orphan".into()
        )]
    );
}

/// The audit judges committed apps and jobs as an apply would, by the
/// policies in force, and says nothing about what passes.
#[test]
fn the_audit_lists_what_the_policies_would_now_say() {
    let policies = policies(
        r#"
        [policy.no-latest]
        applies_to = "both"
        deny = "workload.image.endsWith(':latest')"
        message = "pin a version"

        [policy.limits]
        action = "audit"
        deny = "!has(app.memory)"

        [policy.not-from-git]
        action = "warn"
        deny = "source == 'gitops'"
        "#,
    );
    let apps_list = vec![
        committed("api", "image = 'api:v2'\nmemory = '256Mi'", false),
        committed("web", "image = 'web:latest'", true),
    ];
    let jobs = vec![(
        AppId::new("nightly", "default"),
        toml::from_str::<crate::config::JobSpec>("image = 'report:latest'\nschedule = '0 3 * * *'")
            .unwrap(),
    )];
    let audit = audit::audit_workloads(&policies, 7, &apps_list, &jobs);
    assert_eq!(
        (audit.generation, audit.policies, audit.apps, audit.jobs),
        (7, 3, 2, 1)
    );
    assert_eq!(
        workload_decisions(&audit.verdict),
        vec![
            (
                "no-latest".into(),
                PolicyAction::Deny,
                "app default/web".into()
            ),
            (
                "no-latest".into(),
                PolicyAction::Deny,
                "job default/nightly".into()
            ),
            // GitOps owns web, so it's judged as a GitOps sync.
            (
                "not-from-git".into(),
                PolicyAction::Warn,
                "app default/web".into()
            ),
            (
                "limits".into(),
                PolicyAction::Audit,
                "app default/web".into()
            ),
        ]
    );
    insta::assert_snapshot!(report::render_audit(&audit));
}

#[test]
fn an_audit_with_nothing_to_report_says_so() {
    let clean = audit::audit_workloads(
        &policies("[policy.no-latest]\ndeny = \"app.image.endsWith(':latest')\"\n"),
        2,
        &[committed("api", "image = 'api:v2'", false)],
        &[],
    );
    assert!(clean.verdict.is_empty());
    assert_eq!(
        report::render_audit(&clean),
        "Checked 1 app(s) against 1 policy(ies) at generation 2.\n\nEvery committed app and job passes.\n"
    );
    let none = audit::audit_workloads(&BTreeMap::new(), 0, &[], &[]);
    assert_eq!(
        report::render_audit(&none),
        "No policies (generation 0): nothing to audit.\n"
    );
    let mut broken = BTreeMap::new();
    broken.insert("old".to_string(), spec(None, "app.name ==="));
    let audit = audit::audit_workloads(
        &broken,
        1,
        &[committed("api", "image = 'api:v2'", false)],
        &[],
    );
    assert!(
        audit
            .broken
            .as_deref()
            .is_some_and(|reason| reason.contains("relish policy delete old"))
    );
}
