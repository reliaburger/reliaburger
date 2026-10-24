use super::*;
use crate::config::Replicas;
use proptest::prelude::*;

const POSTGRES: &str = r#"
[template.postgres]
params = { name = "string", size = "string", replicas = { type = "int", default = 1 } }
body = """
[app.${name}]
image = "postgres:17"
replicas = ${replicas}
[[app.${name}.volumes]]
path = "/var/lib/postgresql/data"
size = "${size}"
"""
"#;

fn expanded(toml: &str) -> Result<Config, TemplateError> {
    let mut config = Config::parse(toml).unwrap();
    config.expand_templates().map(|()| config)
}

fn refused(toml: &str) -> TemplateError {
    expanded(toml).expect_err("the config should be refused")
}

// --- parsing ---------------------------------------------------------------

#[test]
fn a_template_parses_with_short_and_full_parameters() {
    let config = Config::parse(POSTGRES).unwrap();
    let template = &config.template["postgres"];
    assert_eq!(template.params["name"].kind, ParamType::String);
    assert_eq!(template.params["name"].default, None);
    assert_eq!(template.params["replicas"].kind, ParamType::Int);
    assert_eq!(
        template.params["replicas"].default,
        Some(ParamValue::Int(1))
    );
    assert!(template.body.contains("[app.${name}]"));
}

#[test]
fn a_use_parses_its_template_and_typed_values() {
    let config = Config::parse(
        "[use.orders-db]\ntemplate = 'postgres'\nname = 'orders-db'\nreplicas = 3\nbackups = true\n",
    )
    .unwrap();
    let instance = &config.uses["orders-db"];
    assert_eq!(instance.template, "postgres");
    assert_eq!(
        instance.values,
        BTreeMap::from([
            ("backups".to_string(), ParamValue::Bool(true)),
            ("name".to_string(), ParamValue::String("orders-db".into())),
            ("replicas".to_string(), ParamValue::Int(3)),
        ])
    );
}

#[test]
fn malformed_templates_and_uses_fail_to_parse_with_the_reason() {
    for (toml, expected) in [
        (
            "[template.t]\nbody = ''\nparams = { a = 'strng' }\n",
            "unknown parameter type",
        ),
        (
            "[template.t]\nbody = ''\nparams = { a = { type = 'int', default = 'x' } }\n",
            "is a string, not an int",
        ),
        (
            "[template.t]\nbody = ''\nparams = { a = { type = 'int', defualt = 1 } }\n",
            "params",
        ),
        ("[template.t]\nparams = {}\n", "body"),
        ("[template.t]\nbody = ''\nextra = 1\n", "extra"),
        ("[use.x]\nname = 'a'\n", "needs `template"),
        ("[use.x]\ntemplate = 3\n", "it's a string"),
        (
            "[use.x]\ntemplate = 't'\nsize = 1.5\n",
            "a string, an integer or a boolean",
        ),
    ] {
        let error = Config::parse(toml).unwrap_err().to_string();
        assert!(error.contains(expected), "{toml}: {error}");
    }
}

#[test]
fn templates_and_uses_round_trip_through_toml_and_json() {
    let config = Config::parse(&format!(
        "{POSTGRES}\n[use.orders-db]\ntemplate = 'postgres'\nname = 'orders-db'\nsize = '50Gi'\n"
    ))
    .unwrap();
    let toml = toml::to_string(&config).unwrap();
    assert_eq!(Config::parse(&toml).unwrap(), config);
    let json = serde_json::to_string(&config).unwrap();
    assert_eq!(serde_json::from_str::<Config>(&json).unwrap(), config);
}

// --- expansion -------------------------------------------------------------

#[test]
fn a_use_expands_to_an_ordinary_app_with_its_provenance() {
    let config = expanded(&format!(
        "{POSTGRES}\n[use.orders-db]\ntemplate = 'postgres'\nname = 'orders-db'\nsize = '50Gi'\n"
    ))
    .unwrap();
    assert!(config.template.is_empty() && config.uses.is_empty());
    let app = &config.app["orders-db"];
    assert_eq!(app.image.as_deref(), Some("postgres:17"));
    assert_eq!(app.replicas, Replicas::Fixed(1), "the default fills in");
    assert_eq!(app.volumes[0].size.as_deref(), Some("50Gi"));
    assert_eq!(
        app.provenance,
        Some(Provenance {
            template: "postgres".into(),
            instance: "orders-db".into(),
        })
    );
}

#[test]
fn two_uses_of_one_template_make_two_apps() {
    let config = expanded(&format!(
        "{POSTGRES}
[use.orders]
template = 'postgres'
name = 'orders-db'
size = '50Gi'
replicas = 3

[use.users]
template = 'postgres'
name = 'users-db'
size = '5Gi'

[app.web]
image = 'web:v1'
"
    ))
    .unwrap();
    assert_eq!(
        config.app.keys().collect::<Vec<_>>(),
        ["orders-db", "users-db", "web"]
    );
    assert_eq!(config.app["orders-db"].replicas, Replicas::Fixed(3));
    assert_eq!(config.app["web"].provenance, None);
}

#[test]
fn a_body_can_hold_jobs_and_they_carry_provenance_too() {
    let config = expanded(
        r#"
[template.migrated]
params = { name = "string" }
body = """
[app.${name}]
image = "${name}:v1"
[job.${name}-migrate]
image = "${name}:v1"
command = ["migrate"]
run_before = ["app.${name}"]
"""

[use.shop]
template = "migrated"
name = "shop"
"#,
    )
    .unwrap();
    assert_eq!(config.job["shop-migrate"].run_before, vec!["app.shop"]);
    assert_eq!(
        config.job["shop-migrate"].provenance,
        config.app["shop"].provenance
    );
}

#[test]
fn a_config_without_templates_is_unchanged() {
    let before = Config::parse("[app.web]\nimage = 'web:v1'\n").unwrap();
    let mut after = before.clone();
    after.expand_templates().unwrap();
    assert_eq!(after, before);
}

#[test]
fn a_value_inside_quotes_is_escaped_so_it_cannot_add_tables() {
    let config = expanded(
        r#"
[template.t]
params = { name = "string", greeting = "string" }
body = """
[app.${name}]
image = "x:1"
env.GREETING = "${greeting}"
"""

[use.u]
template = "t"
name = "web"
greeting = "hi\"\n[app.evil]\nimage = \"evil:1\"\n#"
"#,
    )
    .unwrap();
    assert_eq!(config.app.keys().collect::<Vec<_>>(), ["web"]);
    assert_eq!(
        config.app["web"].env["GREETING"].as_str(),
        "hi\"\n[app.evil]\nimage = \"evil:1\"\n#"
    );
}

#[test]
fn integers_and_booleans_go_in_bare_and_strings_stay_strings() {
    let config = expanded(
        r#"
[template.t]
params = { port = "int", debug = "bool" }
body = """
[app.web]
image = "x:1"
port = ${port}
env.PORT = "${port}"
env.DEBUG = '${debug}'
"""

[use.u]
template = "t"
port = 8080
debug = false
"#,
    )
    .unwrap();
    let web = &config.app["web"];
    assert_eq!(web.port, Some(8080));
    assert_eq!(web.env["PORT"].as_str(), "8080");
    assert_eq!(web.env["DEBUG"].as_str(), "false");
}

#[test]
fn a_doubled_dollar_writes_a_literal_placeholder_for_the_shell() {
    let config = expanded(
        r#"
[template.t]
params = { name = "string" }
body = """
[app.${name}]
image = "x:1"
command = ["sh", "-c", "echo $${HOME} $$PATH"]
"""

[use.u]
template = "t"
name = "web"
"#,
    )
    .unwrap();
    assert_eq!(
        config.app["web"].command,
        ["sh", "-c", "echo ${HOME} $$PATH"]
    );
}

#[test]
fn placeholders_in_comments_are_left_alone() {
    let config = expanded(
        r#"
[template.t]
params = { name = "string" }
body = """
# ${anything}, even ${ unclosed
[app.${name}]
image = "x:1"
"""

[use.u]
template = "t"
name = "web"
"#,
    )
    .unwrap();
    assert!(config.app.contains_key("web"));
}

#[test]
fn a_value_that_looks_like_a_placeholder_is_not_expanded_again() {
    let config = expanded(
        r#"
[template.t]
params = { name = "string", note = "string" }
body = """
[app.${name}]
image = "x:1"
env.NOTE = "${note}"
"""

[use.u]
template = "t"
name = "web"
note = "${name}"
"#,
    )
    .unwrap();
    assert_eq!(config.app["web"].env["NOTE"].as_str(), "${name}");
}

#[test]
fn a_literal_string_takes_a_value_without_quotes_or_newlines() {
    let toml = |value: &str| {
        format!(
            "[template.t]\nparams = {{ v = 'string' }}\nbody = \"\"\"\n[app.web]\nimage = 'x:${{v}}'\n\"\"\"\n[use.u]\ntemplate = 't'\nv = {value:?}\n"
        )
    };
    assert_eq!(
        expanded(&toml("1.2")).unwrap().app["web"].image.as_deref(),
        Some("x:1.2")
    );
    let error = refused(&toml("1'2"));
    assert!(
        matches!(&error, TemplateError::Placement { parameter, .. } if parameter == "v"),
        "{error}"
    );
}

// --- refusals --------------------------------------------------------------

fn instance(values: &str) -> String {
    format!("{POSTGRES}\n[use.orders-db]\ntemplate = 'postgres'\n{values}\n")
}

#[test]
fn an_unknown_template_is_refused() {
    let error = refused("[use.x]\ntemplate = 'nope'\n");
    assert_eq!(
        error,
        TemplateError::UnknownTemplate {
            instance: "x".into(),
            template: "nope".into(),
        }
    );
    assert_eq!(error.to_string(), "use.x: there's no template \"nope\"");
}

#[test]
fn an_unknown_parameter_is_refused() {
    let error = refused(&instance("name = 'db'\nsize = '1Gi'\nsise = '2Gi'"));
    assert!(
        matches!(&error, TemplateError::UnknownParameter { parameter, .. } if parameter == "sise"),
        "{error}"
    );
}

#[test]
fn a_missing_required_parameter_is_refused() {
    let error = refused(&instance("name = 'db'"));
    assert_eq!(
        error.to_string(),
        "use.orders-db: template \"postgres\" needs a value for \"size\""
    );
}

#[test]
fn a_value_of_the_wrong_type_is_refused() {
    let error = refused(&instance("name = 'db'\nsize = '1Gi'\nreplicas = '3'"));
    assert_eq!(
        error.to_string(),
        "use.orders-db: parameter \"replicas\" of template \"postgres\" is an int, not a string"
    );
}

#[test]
fn an_expanded_name_that_clashes_with_a_declared_app_is_refused() {
    let error = refused(&instance(
        "name = 'web'\nsize = '1Gi'\n\n[app.web]\nimage = 'web:v1'",
    ));
    assert_eq!(
        error,
        TemplateError::Clash {
            instance: "orders-db".into(),
            resource: "app.web".into(),
            other: "a declared app".into(),
        }
    );
}

#[test]
fn two_uses_that_expand_to_one_name_are_refused() {
    let error = refused(&format!(
        "{POSTGRES}
[use.a]
template = 'postgres'
name = 'db'
size = '1Gi'
[use.b]
template = 'postgres'
name = 'db'
size = '2Gi'
"
    ));
    assert_eq!(
        error.to_string(),
        "use.b expands to app.db, which use.a already declares"
    );
}

#[test]
fn a_template_that_uses_or_declares_another_template_is_refused() {
    for table in [
        "use.inner]\ntemplate = 'postgres'",
        "template.inner]\nbody = ''",
    ] {
        let error = refused(&format!(
            "{POSTGRES}
[template.outer]
body = \"\"\"
[{table}
\"\"\"
[use.x]
template = 'outer'
"
        ));
        assert!(matches!(error, TemplateError::Nested { .. }), "{error}");
    }
}

#[test]
fn a_body_with_anything_but_apps_and_jobs_is_refused() {
    let error =
        refused("[template.t]\nbody = \"[policy.p]\\ndeny = 'false'\"\n[use.x]\ntemplate = 't'\n");
    assert!(
        matches!(&error, TemplateError::BodyTable { table, .. } if table == "policy.p"),
        "{error}"
    );
}

#[test]
fn a_placeholder_no_parameter_resolves_is_refused_even_if_unused() {
    let error = refused(
        "[template.t]\nparams = { name = 'string' }\nbody = \"[app.${name}]\\nimage = '${imgae}'\"\n",
    );
    assert_eq!(
        error,
        TemplateError::UnresolvedPlaceholder {
            template: "t".into(),
            placeholder: "imgae".into(),
        }
    );
}

#[test]
fn malformed_templates_are_refused() {
    for (toml, expected) in [
        ("[template.Bad_Name]\nbody = ''\n", "lowercase DNS label"),
        (
            "[template.t]\nparams = { template = 'string' }\nbody = ''\n",
            "names the template",
        ),
        (
            "[template.t]\nparams = { '9lives' = 'string' }\nbody = ''\n",
            "parameter names",
        ),
        ("[template.t]\nbody = '[app.${x'\n", "unclosed"),
        ("[template.t]\nbody = '[app.${}]'\n", "empty"),
    ] {
        let error = refused(toml);
        assert!(error.to_string().contains(expected), "{toml}: {error}");
    }
}

#[test]
fn a_use_with_a_bad_name_is_refused() {
    let error = refused(&format!(
        "{POSTGRES}\n[use.Orders]\ntemplate = 'postgres'\nname = 'db'\nsize = '1Gi'\n"
    ));
    assert!(matches!(error, TemplateError::InvalidUse { .. }), "{error}");
}

#[test]
fn a_string_outside_quotes_must_be_a_plain_key() {
    let error = refused(&instance("name = 'web]\\n[app.evil'\nsize = '1Gi'"));
    assert!(
        matches!(&error, TemplateError::Placement { parameter, .. } if parameter == "name"),
        "{error}"
    );
}

#[test]
fn a_body_that_is_not_valid_toml_names_the_use_and_template() {
    let error = refused(
        "[template.t]\nparams = { v = 'string' }\nbody = \"[app.web]\\nimage = '${v}\"\n[use.u]\ntemplate = 't'\nv = 'x'\n",
    );
    assert!(
        error
            .to_string()
            .starts_with("use.u: template \"t\" doesn't expand to valid config"),
        "{error}"
    );
}

#[test]
fn validation_refuses_a_config_whose_uses_were_never_expanded() {
    let config = Config::parse(&instance("name = 'db'\nsize = '1Gi'")).unwrap();
    let error = config.validate().unwrap_err().to_string();
    assert!(error.contains("use.orders-db"), "{error}");
}

// --- properties ------------------------------------------------------------

/// Any string without a `$`, so no value can itself look like a placeholder,
/// and without a `[`, so none reads as an `ENC[` or `PROVIDER[` reference.
fn value_strategy() -> impl Strategy<Value = String> {
    "[^$\\[]{0,24}"
}

fn property_toml(name: &str, quoted: &str, count: i64, flag: bool) -> String {
    let body = r#"
[app.${name}]
image = "x:1"
replicas = ${count}
env.QUOTED = "${quoted}"
env.MULTI = """${quoted}"""
env.FLAG = '${flag}'
command = ["echo", "${quoted}", "${count}"]
"#;
    format!(
        "[template.t]\nparams = {{ name = 'string', quoted = 'string', count = 'int', flag = 'bool' }}\nbody = {body:?}\n[use.u]\ntemplate = 't'\nname = {name:?}\nquoted = {}\ncount = {count}\nflag = {flag}\n",
        toml::Value::String(quoted.to_string())
    )
}

proptest! {
    /// Expansion is deterministic, leaves no placeholder behind, and every
    /// value comes back out of the parsed app exactly as it went in.
    #[test]
    fn expansion_is_deterministic_and_leaves_no_placeholders(
        name in "[a-z][a-z0-9-]{0,10}[a-z0-9]",
        quoted in value_strategy(),
        count in 1i64..1000,
        flag in any::<bool>(),
    ) {
        let toml = property_toml(&name, &quoted, count, flag);
        let first = expanded(&toml).unwrap();
        let second = expanded(&toml).unwrap();
        prop_assert_eq!(&first, &second);

        let rendered = toml::to_string(&first).unwrap();
        prop_assert!(!rendered.contains(OPEN), "{}", rendered);

        let app = &first.app[&name];
        prop_assert_eq!(app.replicas, Replicas::Fixed(count as u32));
        // A multi-line basic string trims one newline straight after the
        // opening quotes, as TOML says; the rest of the value is exact.
        prop_assert_eq!(app.env["QUOTED"].as_str(), quoted.as_str());
        let flag_text = flag.to_string();
        prop_assert_eq!(app.env["FLAG"].as_str(), flag_text.as_str());
        prop_assert_eq!(&app.command[1], &quoted);
        prop_assert_eq!(&app.command[2], &count.to_string());
    }
}
