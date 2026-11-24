//! An app's spec, read back through the API (#1322).
//!
//! A controller that owns an app needs to see what the cluster holds for
//! it: its image, its resources, its replicas. The diagnostics read carries
//! counts and placements but none of that, so `GET
//! /v1/apps/{app}/{namespace}/spec` answers with the spec itself, the one
//! the cluster runs and, where it kept one, the one that was declared.
//!
//! The spec is where an app's secrets live, so the answer leaves them out.
//! Every env value goes, plain or encrypted, in the app and in each init
//! container and sidecar, because no pattern can tell a password from a
//! feature flag. A provider reference stays: it names where a secret
//! lives, not the secret. Every config file's inline content goes too.
//! The commands, arguments and scripts keep their words, with anything the
//! redaction rules recognise as a secret (a token, a key, a URL's password)
//! taken out. The answer lists every field it redacted.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::config::{AppSpec, EnvValue};
use crate::council::types::DesiredState;
use crate::meat::types::AppId;
use crate::redact::{REDACTED, redact_secrets};

/// What `GET /v1/apps/{app}/{namespace}/spec` answers.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AppSpecView {
    /// The app's name.
    pub app: String,
    /// Its namespace.
    pub namespace: String,
    /// The controller token that owns it (#1125), if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    /// Whether GitOps owns it.
    #[serde(default)]
    pub gitops_owned: bool,
    /// The spec the cluster runs: the declared one with any policy
    /// defaults filled in (#1118) and each image tag bound to its digest.
    pub spec: AppSpec,
    /// The spec as it was declared, when the cluster kept one apart from
    /// the one it runs. Absent when the two are the same, or when a
    /// rollback wrote the spec.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub declared: Option<AppSpec>,
    /// Every field whose value this answer leaves out, as a dotted path
    /// such as `spec.env.DB_PASSWORD` or `declared.sidecar.proxy.env.TOKEN`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub redacted: Vec<String>,
}

/// The view of `id` in `desired`, with its secrets redacted, or `None`
/// when the cluster has no such app.
pub fn app_spec_view(desired: &DesiredState, id: &AppId) -> Option<AppSpecView> {
    let stored = desired.apps.get(id)?;
    let mut redacted = Vec::new();
    let spec = redact_spec(stored, "spec", &mut redacted);
    let declared = desired
        .declared_apps
        .get(id)
        .filter(|declared| *declared != stored)
        .map(|declared| redact_spec(declared, "declared", &mut redacted));
    Some(AppSpecView {
        app: id.name.clone(),
        namespace: id.namespace.clone(),
        owner: stored.owner.clone(),
        gitops_owned: desired.gitops_owned.apps.contains(id),
        spec,
        declared,
        redacted,
    })
}

/// `spec` with its secrets left out, each redacted field's path, under
/// `prefix`, pushed onto `redacted`.
fn redact_spec(spec: &AppSpec, prefix: &str, redacted: &mut Vec<String>) -> AppSpec {
    let mut spec = spec.clone();
    redact_env(&mut spec.env, &format!("{prefix}.env"), redacted);
    redact_words(&mut spec.command, &format!("{prefix}.command"), redacted);
    redact_words(&mut spec.args, &format!("{prefix}.args"), redacted);
    redact_words(&mut spec.pre_stop, &format!("{prefix}.pre_stop"), redacted);
    if let Some(script) = &mut spec.script {
        redact_text(script, &format!("{prefix}.script"), redacted);
    }
    // A probe or route path can carry a query string, `?token=…`.
    if let Some(health) = &mut spec.health {
        redact_text(&mut health.path, &format!("{prefix}.health.path"), redacted);
    }
    if let Some(path) = spec
        .ingress
        .as_mut()
        .and_then(|ingress| ingress.path.as_mut())
    {
        redact_text(path, &format!("{prefix}.ingress.path"), redacted);
    }
    for file in &mut spec.config_file {
        if file.content.is_some() {
            file.content = Some(REDACTED.to_string());
            redacted.push(format!(
                "{prefix}.config_file.{}.content",
                file.path.display()
            ));
        }
    }
    for (index, init) in spec.init.iter_mut().enumerate() {
        let path = format!("{prefix}.init.{index}");
        redact_env(&mut init.env, &format!("{path}.env"), redacted);
        redact_words(&mut init.command, &format!("{path}.command"), redacted);
    }
    for sidecar in &mut spec.sidecar {
        let path = format!("{prefix}.sidecar.{}", sidecar.name);
        redact_env(&mut sidecar.env, &format!("{path}.env"), redacted);
        redact_words(&mut sidecar.command, &format!("{path}.command"), redacted);
        redact_words(&mut sidecar.args, &format!("{path}.args"), redacted);
    }
    spec
}

/// Replace every plain and encrypted value in `env` with [`REDACTED`],
/// keeping provider references.
fn redact_env(env: &mut BTreeMap<String, EnvValue>, prefix: &str, redacted: &mut Vec<String>) {
    for (key, value) in env.iter_mut() {
        if matches!(value, EnvValue::Provider(_)) {
            continue;
        }
        *value = EnvValue::Plain(REDACTED.to_string());
        redacted.push(format!("{prefix}.{key}"));
    }
}

/// Take any secret the redaction rules recognise out of each word, and
/// redact a whole word when the word before it names a secret: the value
/// of `--password hunter2`, `--db-token abc` or `DB_PASSWORD hunter2`.
///
/// A short flag with its value joined on, such as MySQL's `-phunter2`,
/// isn't caught: `p` names a password only to MySQL, and redacting every
/// word that starts with `-p` would take out `-port` too.
fn redact_words(words: &mut [String], prefix: &str, redacted: &mut Vec<String>) {
    let mut after_secret_flag = false;
    for (index, word) in words.iter_mut().enumerate() {
        let path = format!("{prefix}.{index}");
        let names_secret = names_a_secret(word);
        if after_secret_flag {
            *word = REDACTED.to_string();
            redacted.push(path);
        } else {
            redact_text(word, &path, redacted);
        }
        after_secret_flag = names_secret;
    }
}

/// Whether `word` is a flag or key naming a secret whose value is the next
/// word: `--password`, `-token` or `DB_PASSWORD`, with no `=` (a word with
/// one carries its own value, which [`redact_text`] takes out).
fn names_a_secret(word: &str) -> bool {
    !word.contains('=') && crate::redact::is_secret_key(word.trim_start_matches('-'))
}

/// Take any secret the redaction rules recognise out of `text`, naming
/// `path` in `redacted` when one was there.
fn redact_text(text: &mut String, path: &str, redacted: &mut Vec<String>) {
    let clean = redact_secrets(text);
    if clean != *text {
        *text = clean;
        redacted.push(path.to_string());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn desired_with(manifest: &str) -> (DesiredState, AppId) {
        let config = crate::config::Config::parse(manifest).unwrap();
        let (name, spec) = config.app.into_iter().next().unwrap();
        let id = AppId::new(&name, spec.namespace.as_deref().unwrap_or("default"));
        let mut desired = DesiredState::default();
        desired.apps.insert(id.clone(), spec);
        (desired, id)
    }

    #[test]
    fn every_env_value_is_redacted_but_a_provider_reference_stays() {
        let (desired, id) = desired_with(
            "[app.web]\nimage = 'web:v1'\n\
             [app.web.env]\nLOG_LEVEL = 'debug'\nDB_PASSWORD = 'hunter2'\n\
             SEALED = 'ENC[AGE:abc123]'\nAPI_KEY = 'PROVIDER[vault:kv/web]'\n",
        );
        let view = app_spec_view(&desired, &id).unwrap();
        let env = &view.spec.env;
        assert_eq!(env["LOG_LEVEL"], EnvValue::Plain(REDACTED.into()));
        assert_eq!(env["DB_PASSWORD"], EnvValue::Plain(REDACTED.into()));
        assert_eq!(env["SEALED"], EnvValue::Plain(REDACTED.into()));
        assert!(matches!(env["API_KEY"], EnvValue::Provider(_)));
        assert_eq!(
            view.redacted,
            [
                "spec.env.DB_PASSWORD",
                "spec.env.LOG_LEVEL",
                "spec.env.SEALED"
            ]
        );
        let json = serde_json::to_string(&view).unwrap();
        assert!(
            !json.contains("hunter2") && !json.contains("abc123"),
            "{json}"
        );
    }

    #[test]
    fn sidecar_and_init_env_and_config_file_content_are_redacted() {
        let (desired, id) = desired_with(
            "[app.web]\nimage = 'web:v1'\n\
             [[app.web.config_file]]\npath = '/etc/web.conf'\ncontent = 'password=hunter2'\n\
             [[app.web.init]]\nimage = 'migrate:v1'\ncommand = ['migrate']\nenv = { DB_URL = 'postgres://u:hunter2@db/x' }\n\
             [[app.web.sidecar]]\nname = 'proxy'\nimage = 'proxy:v1'\nenv = { TOKEN = 'hunter2' }\n",
        );
        let view = app_spec_view(&desired, &id).unwrap();
        let json = serde_json::to_string(&view).unwrap();
        assert!(!json.contains("hunter2"), "{json}");
        assert_eq!(
            view.redacted,
            [
                "spec.config_file./etc/web.conf.content",
                "spec.init.0.env.DB_URL",
                "spec.sidecar.proxy.env.TOKEN",
            ]
        );
        assert_eq!(view.spec.image.as_deref(), Some("web:v1"));
        assert_eq!(view.spec.init[0].command, ["migrate"]);
    }

    #[test]
    fn a_secret_in_a_command_is_taken_out_and_the_rest_kept() {
        let token = format!("rbrg_{}", "a".repeat(40));
        let (desired, id) = desired_with(&format!(
            "[app.web]\nimage = 'web:v1'\ncommand = ['serve', '--token', '{token}']\n"
        ));
        let view = app_spec_view(&desired, &id).unwrap();
        assert_eq!(view.spec.command[0], "serve");
        assert_eq!(view.spec.command[1], "--token");
        assert!(
            !view.spec.command[2].contains(&token),
            "{:?}",
            view.spec.command
        );
        assert_eq!(view.redacted, ["spec.command.2"]);
    }

    #[test]
    fn the_declared_spec_is_shown_only_when_it_differs() {
        let (mut desired, id) = desired_with("[app.web]\nimage = 'web:v1'\n");
        assert!(app_spec_view(&desired, &id).unwrap().declared.is_none());
        let declared = desired.apps[&id].clone();
        desired.declared_apps.insert(id.clone(), declared.clone());
        assert!(app_spec_view(&desired, &id).unwrap().declared.is_none());
        // A policy default filled in the memory the app left out.
        let (filled, _) = desired_with("[app.web]\nimage = 'web:v1'\nmemory = '256Mi'\n");
        desired.apps = filled.apps;
        let view = app_spec_view(&desired, &id).unwrap();
        assert!(view.spec.memory.is_some());
        assert_eq!(view.declared, Some(declared));
    }

    #[test]
    fn a_secret_passed_as_the_next_argument_is_redacted() {
        let (desired, id) = desired_with(
            "[app.web]\nimage = 'web:v1'\n\
             command = ['serve', '--password', 'hunter2', '--port', '8080']\n\
             args = ['--db-token', 'abc123', 'DB_PASSWORD', 'hunter3']\n\
             pre_stop = ['drain', '--api-key', 'k3y']\n\
             [[app.web.init]]\nimage = 'migrate:v1'\ncommand = ['migrate', '--secret', 's3cret']\n\
             [[app.web.sidecar]]\nname = 'proxy'\nimage = 'proxy:v1'\n\
             command = ['proxy', '-token', 't0ken']\nargs = ['--access-key', 'AKIA']\n",
        );
        let view = app_spec_view(&desired, &id).unwrap();
        assert_eq!(
            view.spec.command,
            ["serve", "--password", REDACTED, "--port", "8080"]
        );
        assert_eq!(
            view.spec.args,
            ["--db-token", REDACTED, "DB_PASSWORD", REDACTED]
        );
        assert_eq!(view.spec.pre_stop, ["drain", "--api-key", REDACTED]);
        assert_eq!(view.spec.init[0].command, ["migrate", "--secret", REDACTED]);
        assert_eq!(view.spec.sidecar[0].command, ["proxy", "-token", REDACTED]);
        assert_eq!(view.spec.sidecar[0].args, ["--access-key", REDACTED]);
        let json = serde_json::to_string(&view).unwrap();
        for secret in [
            "hunter2", "hunter3", "abc123", "k3y", "s3cret", "t0ken", "AKIA",
        ] {
            assert!(!json.contains(secret), "{secret} leaked: {json}");
        }
        assert!(view.redacted.contains(&"spec.command.2".to_string()));
        assert!(view.redacted.contains(&"spec.args.1".to_string()));
        assert!(
            view.redacted
                .contains(&"spec.sidecar.proxy.command.2".to_string())
        );
    }

    #[test]
    fn a_secret_joined_to_its_flag_is_redacted_and_the_next_word_kept() {
        let (desired, id) = desired_with(
            "[app.web]\nimage = 'web:v1'\n\
             command = ['serve', '--password=hunter2', '--db-token=abc123', '--port', '8080']\n",
        );
        let view = app_spec_view(&desired, &id).unwrap();
        let json = serde_json::to_string(&view).unwrap();
        assert!(
            !json.contains("hunter2") && !json.contains("abc123"),
            "{json}"
        );
        assert_eq!(view.spec.command[3..], ["--port", "8080"]);
    }

    #[test]
    fn a_token_in_a_health_or_ingress_path_is_redacted() {
        let token = format!("rbrg_{}", "b".repeat(40));
        let (desired, id) = desired_with(&format!(
            "[app.web]\nimage = 'web:v1'\nport = 8080\n\
             [app.web.health]\npath = '/healthz?token={token}'\n\
             [app.web.ingress]\nhost = 'web.example.com'\npath = '/hook?api_key=hunter2'\n"
        ));
        let view = app_spec_view(&desired, &id).unwrap();
        let json = serde_json::to_string(&view).unwrap();
        assert!(
            !json.contains(&token) && !json.contains("hunter2"),
            "{json}"
        );
        assert!(
            view.spec
                .health
                .unwrap()
                .path
                .starts_with("/healthz?token=")
        );
        assert!(view.redacted.contains(&"spec.health.path".to_string()));
        assert!(view.redacted.contains(&"spec.ingress.path".to_string()));
    }

    #[test]
    fn an_unknown_app_has_no_view() {
        let (desired, _) = desired_with("[app.web]\nimage = 'web:v1'\n");
        assert!(app_spec_view(&desired, &AppId::new("db", "default")).is_none());
    }
}
