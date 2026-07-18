//! Black-box contracts for the compiled `relish` executable.
//!
//! Parser unit tests are useful, but they cannot catch broken exit codes,
//! stdout/stderr routing, or a binary that was wired to the wrong handler.

use std::process::{Command, Output};

fn run(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_relish"))
        .args(args)
        .env_remove("RELIABURGER_TOKEN")
        .env_remove("RELIABURGER_CA_CERT")
        .output()
        .unwrap()
}

#[test]
fn version_reports_the_compiled_binary() {
    let output = run(&["--version"]);
    assert!(output.status.success());
    assert_eq!(String::from_utf8_lossy(&output.stdout), "relish 0.1.0\n");
    assert!(output.stderr.is_empty());
}

#[test]
fn invalid_output_format_exits_with_clap_error() {
    let output = run(&["--output", "xml", "status"]);
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("invalid value 'xml'"));
}

#[test]
fn apply_dry_run_succeeds_without_an_agent() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("app.toml");
    std::fs::write(
        &config,
        r#"
[app.web]
image = "proc-grill:image-ignored"
command = ["true"]
"#,
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_relish"))
        .arg("apply")
        .arg(&config)
        .arg("--dry-run")
        .env_remove("RELIABURGER_TOKEN")
        .env_remove("RELIABURGER_CA_CERT")
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("web"), "{stdout}");
    assert!(stdout.contains("dry run"), "{stdout}");
    assert!(output.stderr.is_empty());
}

#[test]
fn missing_apply_file_exits_nonzero_and_uses_stderr() {
    let output = run(&["apply", "/definitely/missing/reliaburger.toml", "--dry-run"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("error:"));
}

#[test]
fn endpoint_environment_rejects_remote_plaintext_before_dispatch() {
    let output = Command::new(env!("CARGO_BIN_EXE_relish"))
        .arg("status")
        .env("RELIABURGER_ENDPOINT", "http://192.0.2.10:9117")
        .env_remove("RELIABURGER_TOKEN")
        .env_remove("RELIABURGER_CA_CERT")
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("must use HTTPS"));
}
