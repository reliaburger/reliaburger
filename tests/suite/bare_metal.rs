//! Black-box contracts for the master-key backup prompt in
//! `relish cluster create --bare-metal` and `relish machines claim --create`.

use std::path::Path;
use std::process::{Command, Output};

fn relish(home: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_relish"))
        .args(args)
        .env("HOME", home)
        .env("RELIABURGER_HOME", home.join(".reliaburger"))
        .stdin(std::process::Stdio::null())
        .output()
        .unwrap()
}

fn create(home: &Path, cluster: &Path, extra: &[&str]) -> Output {
    let cluster = cluster.to_str().unwrap();
    let mut args = vec![
        "cluster",
        "create",
        "--bare-metal",
        cluster,
        "--name",
        "home",
        "--operator",
        "192.0.2.10",
    ];
    args.extend_from_slice(extra);
    args.push("d8:9e:f3:00:00:01@192.0.2.51");
    relish(home, &args)
}

#[test]
fn bare_metal_create_without_a_terminal_needs_yes_and_creates_nothing() {
    let home = tempfile::tempdir().unwrap();
    let cluster = home.path().join("home-cluster");
    let output = create(home.path(), &cluster, &[]);
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("--yes"), "{stderr}");
    assert!(stderr.contains("master key"), "{stderr}");
    assert!(!cluster.exists(), "no secrets before the backup is settled");
}

#[test]
fn bare_metal_create_with_yes_reminds_and_carries_on() {
    let home = tempfile::tempdir().unwrap();
    let cluster = home.path().join("home-cluster");
    let output = create(home.path(), &cluster, &["--yes"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "{stdout}{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(stdout.contains("master key"), "{stdout}");
    assert!(stdout.contains("--yes"), "{stdout}");
    assert!(cluster.join("secrets/master.key").exists());
    assert!(cluster.join("fleet.json").exists());
    assert!(home.path().join(".reliaburger/context.json").exists());
}

#[test]
fn claim_create_without_a_terminal_needs_yes_before_contacting_anything() {
    let home = tempfile::tempdir().unwrap();
    let cluster = home.path().join("home-cluster");
    // 192.0.2.0/24 is TEST-NET-1: nothing answers there, so getting past
    // the check would wait on the claim API rather than fail at once.
    let output = relish(
        home.path(),
        &[
            "machines",
            "claim",
            cluster.to_str().unwrap(),
            "--create",
            "--name",
            "home",
            "--operator",
            "192.0.2.10",
            "--trust-lan",
            "192.0.2.51",
        ],
    );
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("--yes"), "{stderr}");
    assert!(!cluster.exists());
}
