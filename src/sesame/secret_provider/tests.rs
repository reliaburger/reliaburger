//! The exec secret provider's protocol, bounds and failures, against real
//! provider scripts in a temporary directory.

use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};

use super::*;

/// A value no error or failure message may ever contain.
const SECRET: &str = "hunter2-do-not-print";

fn name(provider: &str) -> ProviderName {
    ProviderName::try_from(provider.to_string()).unwrap()
}

/// Write an executable `/bin/sh` provider script.
fn script(dir: &Path, file: &str, body: &str) -> PathBuf {
    let path = dir.join(file);
    std::fs::write(&path, format!("#!/bin/sh\n{body}\n")).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

fn providers(binary: &Path, configure: impl FnOnce(&mut SecretProviderConfig)) -> SecretProviders {
    let mut config = SecretProviderConfig::new(binary);
    configure(&mut config);
    SecretProviders::new(BTreeMap::from([(name("vault"), config)]))
}

fn context() -> LaunchContext<'static> {
    LaunchContext {
        namespace: "team-a",
        app: "api",
        instance: "team-a__api-0",
    }
}

fn reference() -> SecretReference {
    SecretReference::parse("PROVIDER[vault:secret/db#password]").unwrap()
}

async fn resolve_with(
    body: &str,
    configure: impl FnOnce(&mut SecretProviderConfig),
) -> Result<String, ProviderError> {
    let dir = tempfile::tempdir().unwrap();
    let binary = script(dir.path(), "provider", body);
    providers(&binary, configure)
        .resolve(&reference(), "DB_PASSWORD", context())
        .await
}

fn assert_secret_free(error: &ProviderError) {
    let message = error.to_string();
    assert!(!message.contains(SECRET), "leaked the secret: {message}");
    assert!(
        !format!("{error:?}").contains(SECRET),
        "leaked the secret: {error:?}"
    );
}

// ---------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------

#[test]
fn a_reference_names_its_provider_and_path() {
    let reference = reference();
    assert_eq!(reference.provider().as_str(), "vault");
    assert_eq!(reference.path(), "secret/db#password");
    assert_eq!(reference.as_str(), "PROVIDER[vault:secret/db#password]");
}

#[test]
fn a_path_may_hold_colons_and_brackets() {
    let reference = SecretReference::parse("PROVIDER[aws-sm:arn:aws:secretsmanager:x[1]]").unwrap();
    assert_eq!(reference.provider().as_str(), "aws-sm");
    assert_eq!(reference.path(), "arn:aws:secretsmanager:x[1]");
}

#[test]
fn malformed_references_are_refused_with_the_reason() {
    let cases = [
        ("PROVIDER[vault:secret", ReferenceError::Unterminated),
        ("PROVIDER[vault]", ReferenceError::MissingSeparator),
        (
            "PROVIDER[Vault:secret]",
            ReferenceError::InvalidProviderName("Vault".into()),
        ),
        (
            "PROVIDER[:secret]",
            ReferenceError::InvalidProviderName(String::new()),
        ),
        ("PROVIDER[vault:]", ReferenceError::EmptyPath),
        ("PROVIDER[vault:a\nb]", ReferenceError::ControlCharacter),
    ];
    for (value, expected) in cases {
        assert_eq!(SecretReference::parse(value), Err(expected), "{value}");
    }
    let long = format!("PROVIDER[vault:{}]", "a".repeat(MAX_PATH_BYTES + 1));
    assert_eq!(
        SecretReference::parse(&long),
        Err(ReferenceError::PathTooLong)
    );
}

#[test]
fn only_values_starting_with_the_prefix_are_references() {
    assert!(SecretReference::is_reference("PROVIDER[vault:x]"));
    assert!(!SecretReference::is_reference("provider[vault:x]"));
    assert!(!SecretReference::is_reference("x PROVIDER[vault:x]"));
    assert!(needs_resolution(&["A=1".into(), "B=PROVIDER[v:x]".into()]));
    assert!(!needs_resolution(&["A=1".into(), "B=ENC[AGE:x]".into()]));
}

// ---------------------------------------------------------------------------
// Node configuration
// ---------------------------------------------------------------------------

#[test]
fn a_provider_table_parses_with_default_bounds() {
    let providers: BTreeMap<ProviderName, SecretProviderConfig> =
        toml::from_str("[vault]\nbinary = \"/usr/local/bin/vault-provider\"\n").unwrap();
    let vault = &providers[&name("vault")];
    assert_eq!(vault.binary, PathBuf::from("/usr/local/bin/vault-provider"));
    assert_eq!(vault.timeout_secs, DEFAULT_TIMEOUT_SECS);
    assert_eq!(vault.max_output_bytes, DEFAULT_MAX_OUTPUT_BYTES);
    vault.validate().unwrap();
}

#[test]
fn a_provider_table_with_a_bad_name_or_field_is_refused() {
    assert!(
        toml::from_str::<BTreeMap<ProviderName, SecretProviderConfig>>(
            "[Vault]\nbinary = \"/bin/true\"\n"
        )
        .is_err()
    );
    assert!(
        toml::from_str::<BTreeMap<ProviderName, SecretProviderConfig>>(
            "[vault]\nbinary = \"/bin/true\"\nargs = [\"x\"]\n"
        )
        .is_err()
    );
}

#[test]
fn a_provider_needs_an_absolute_binary_and_sane_bounds() {
    let relative = SecretProviderConfig::new("vault-provider");
    assert!(relative.validate().unwrap_err().contains("absolute"));
    for timeout_secs in [0, MAX_TIMEOUT_SECS + 1] {
        let config = SecretProviderConfig {
            timeout_secs,
            ..SecretProviderConfig::new("/bin/true")
        };
        assert!(config.validate().unwrap_err().contains("timeout_secs"));
    }
    for max_output_bytes in [0, MAX_OUTPUT_BYTES_CEILING + 1] {
        let config = SecretProviderConfig {
            max_output_bytes,
            ..SecretProviderConfig::new("/bin/true")
        };
        assert!(config.validate().unwrap_err().contains("max_output_bytes"));
    }
}

// ---------------------------------------------------------------------------
// The protocol, against real provider scripts
// ---------------------------------------------------------------------------

#[tokio::test]
async fn a_provider_gets_one_versioned_request_and_its_value_is_the_secret() {
    let dir = tempfile::tempdir().unwrap();
    let request_file = dir.path().join("request.json");
    let binary = script(
        dir.path(),
        "provider",
        &format!(
            "head -n 1 > {}\nprintf '{{\"version\":1,\"value\":\"{SECRET}\"}}'",
            request_file.display()
        ),
    );
    let secret = providers(&binary, |_| {})
        .resolve(&reference(), "DB_PASSWORD", context())
        .await
        .unwrap();
    assert_eq!(secret, SECRET);

    let request: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&request_file).unwrap()).unwrap();
    assert_eq!(
        request,
        serde_json::json!({
            "version": 1,
            "provider": "vault",
            "path": "secret/db#password",
            "namespace": "team-a",
            "app": "api",
            "instance": "team-a__api-0",
            "variable": "DB_PASSWORD",
        })
    );
}

#[tokio::test]
async fn a_provider_sees_only_its_configured_environment() {
    let dir = tempfile::tempdir().unwrap();
    let env_file = dir.path().join("env.txt");
    let binary = script(
        dir.path(),
        "provider",
        &format!(
            "env > {}\nprintf '{{\"version\":1,\"value\":\"x\"}}'",
            env_file.display()
        ),
    );
    providers(&binary, |config| {
        config
            .env
            .insert("VAULT_ADDR".into(), "https://vault.internal:8200".into());
    })
    .resolve(&reference(), "DB_PASSWORD", context())
    .await
    .unwrap();
    let env = std::fs::read_to_string(&env_file).unwrap();
    assert!(
        env.contains("VAULT_ADDR=https://vault.internal:8200"),
        "{env}"
    );
    assert!(env.contains(&format!("PATH={PROVIDER_PATH}")), "{env}");
    assert!(!env.contains("CARGO"), "Bun's environment leaked in: {env}");
}

#[tokio::test]
async fn an_unconfigured_provider_is_refused_without_running_anything() {
    let error = SecretProviders::default()
        .resolve(&reference(), "DB_PASSWORD", context())
        .await
        .unwrap_err();
    assert_eq!(
        error,
        ProviderError::NotConfigured {
            provider: name("vault")
        }
    );
    assert_eq!(
        error.to_string(),
        "secret provider \"vault\" is not configured on this node"
    );
}

#[tokio::test]
async fn a_missing_provider_binary_fails_with_its_path() {
    let error = providers(Path::new("/nonexistent/vault-provider"), |_| {})
        .resolve(&reference(), "DB_PASSWORD", context())
        .await
        .unwrap_err();
    assert!(matches!(error, ProviderError::Spawn { .. }), "{error:?}");
    assert!(
        error.to_string().contains("/nonexistent/vault-provider"),
        "{error}"
    );
}

#[tokio::test]
async fn a_slow_provider_is_killed_with_everything_it_started() {
    let dir = tempfile::tempdir().unwrap();
    let pid_file = dir.path().join("sleep.pid");
    let binary = script(
        dir.path(),
        "provider",
        &format!("sleep 30 &\necho $! > {}\nwait", pid_file.display()),
    );
    let started = std::time::Instant::now();
    let error = providers(&binary, |config| config.timeout_secs = 3)
        .resolve(&reference(), "DB_PASSWORD", context())
        .await
        .unwrap_err();
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "the timeout didn't bound the call"
    );
    assert!(matches!(error, ProviderError::TimedOut { .. }), "{error:?}");
    assert_eq!(
        error.to_string(),
        "secret provider \"vault\" did not answer within 3s and was killed"
    );

    // The provider's own child died with it: it ran in its own group.
    let pid: i32 = std::fs::read_to_string(&pid_file)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let mut alive = true;
    for _ in 0..50 {
        alive = nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None).is_ok();
        if !alive {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(!alive, "the provider's sleep outlived the timeout");
}

#[tokio::test]
async fn a_provider_that_writes_too_much_is_killed_at_the_cap() {
    let error = resolve_with("yes", |config| config.max_output_bytes = 1024)
        .await
        .unwrap_err();
    assert_eq!(
        error,
        ProviderError::OutputTooLarge {
            provider: name("vault"),
            limit: 1024
        }
    );
}

#[tokio::test]
async fn an_answer_just_under_the_cap_is_accepted() {
    let answer = format!("{{\"version\":1,\"value\":\"{SECRET}\"}}");
    let limit = answer.len();
    let secret = resolve_with(&format!("printf '{answer}'"), |config| {
        config.max_output_bytes = limit;
    })
    .await
    .unwrap();
    assert_eq!(secret, SECRET);
}

#[tokio::test]
async fn a_killed_provider_fails_with_its_signal() {
    let error = resolve_with("kill -9 $$", |_| {}).await.unwrap_err();
    assert_eq!(
        error.to_string(),
        "secret provider \"vault\" was killed by signal 9"
    );
}

#[tokio::test]
async fn a_provider_failing_on_a_full_disk_fails_with_its_status() {
    // What a provider does when its cache can't be written: stderr, exit 1.
    let error = resolve_with(
        "echo 'write /var/cache/vault: No space left on device' >&2\nexit 1",
        |_| {},
    )
    .await
    .unwrap_err();
    assert_eq!(
        error.to_string(),
        "secret provider \"vault\" exited with status 1"
    );
}

#[tokio::test]
async fn a_provider_error_answer_is_shown_shortened_to_one_line() {
    let long = "x".repeat(MAX_PROVIDER_MESSAGE_CHARS + 50);
    let error = resolve_with(
        &format!(
            "printf '{{\"version\":1,\"error\":\"permission denied\\\\nfor {long}\"}}'\nexit 2"
        ),
        |_| {},
    )
    .await
    .unwrap_err();
    let ProviderError::Refused { message, .. } = &error else {
        panic!("{error:?}");
    };
    assert!(message.starts_with("permission denied for x"), "{message}");
    assert!(message.ends_with('…'), "{message}");
    assert!(!message.contains('\n'));
}

#[tokio::test]
async fn an_answer_in_another_version_is_refused_without_its_value() {
    let error = resolve_with(
        &format!("printf '{{\"version\":2,\"value\":\"{SECRET}\"}}'"),
        |_| {},
    )
    .await
    .unwrap_err();
    assert_eq!(
        error,
        ProviderError::UnsupportedVersion {
            provider: name("vault"),
            version: 2
        }
    );
    assert_secret_free(&error);
}

#[tokio::test]
async fn malformed_answers_never_echo_what_the_provider_wrote() {
    for body in [
        format!("printf '{SECRET}'"),
        format!("printf '{{\"version\":\"{SECRET}\"}}'"),
        format!("printf '{{\"version\":1,\"value\":[\"{SECRET}\"]}}'"),
        "printf '{\"version\":1}'".to_string(),
    ] {
        let error = resolve_with(&body, |_| {}).await.unwrap_err();
        assert!(
            matches!(error, ProviderError::Malformed { .. }),
            "{body}: {error:?}"
        );
        assert_secret_free(&error);
    }
}

#[tokio::test]
async fn a_value_with_a_failing_exit_is_not_used() {
    let error = resolve_with(
        &format!("printf '{{\"version\":1,\"value\":\"{SECRET}\"}}'\nexit 3"),
        |_| {},
    )
    .await
    .unwrap_err();
    assert_eq!(
        error.to_string(),
        "secret provider \"vault\" exited with status 3"
    );
    assert_secret_free(&error);
}

#[tokio::test]
async fn a_value_with_a_nul_byte_is_refused() {
    let error = resolve_with("printf '{\"version\":1,\"value\":\"a\\\\u0000b\"}'", |_| {})
        .await
        .unwrap_err();
    assert_eq!(
        error,
        ProviderError::InvalidValue {
            provider: name("vault")
        }
    );
}

#[tokio::test]
async fn resolving_an_environment_replaces_only_provider_entries() {
    let dir = tempfile::tempdir().unwrap();
    let binary = script(
        dir.path(),
        "provider",
        &format!("printf '{{\"version\":1,\"value\":\"{SECRET}\"}}'"),
    );
    let env = vec![
        "MODE=plain".to_string(),
        "DB_PASSWORD=PROVIDER[vault:secret/db#password]".to_string(),
        "EMPTY=".to_string(),
    ];
    let resolved = providers(&binary, |_| {})
        .resolve_env(&env, context())
        .await
        .unwrap();
    assert_eq!(
        resolved,
        vec![
            "MODE=plain".to_string(),
            format!("DB_PASSWORD={SECRET}"),
            "EMPTY=".to_string(),
        ]
    );
}

#[tokio::test]
async fn a_failed_entry_names_its_variable_and_never_a_secret() {
    let dir = tempfile::tempdir().unwrap();
    let binary = script(
        dir.path(),
        "provider",
        &format!("printf '{{\"version\":1,\"value\":\"{SECRET}\"}}'"),
    );
    let env = vec![
        "DB_PASSWORD=PROVIDER[vault:secret/db#password]".to_string(),
        "API_KEY=PROVIDER[aws:prod/api]".to_string(),
    ];
    let error = providers(&binary, |_| {})
        .resolve_env(&env, context())
        .await
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        "cannot resolve secret API_KEY: secret provider \"aws\" is not configured on this node"
    );
    assert!(!format!("{error:?}").contains(SECRET));
}
