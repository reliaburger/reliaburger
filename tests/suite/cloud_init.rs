//! The cloud-init user data in the agent playbook (#1579).
//!
//! Nobody runs this file in CI: a provider boots it. So the test holds it to
//! what `relish setup --ssh` and the installer really need, from their own
//! code: the user it creates is the one the playbook's setup line logs in
//! as, with passwordless sudo and key-only login; its packages are exactly
//! the ones setup installs; it carries no secret; and the install and setup
//! lines next to it use flags that exist.

use std::process::Command;

use reliaburger::relish::quickstart::server::PACKAGES;

use super::website::read;

const CHAPTER: &str = "docs/manual/16_working-with-an-ai-agent.md";

/// The YAML between the chapter's cloud-init markers, without its fence.
fn user_data() -> String {
    let chapter = read(CHAPTER);
    let start = chapter
        .find("<!-- agent-cloud-init:start -->")
        .expect("the chapter has no cloud-init start marker");
    let end = chapter
        .find("<!-- agent-cloud-init:end -->")
        .expect("the chapter has no cloud-init end marker");
    let block = &chapter[start..end];
    let body = block
        .split_once("```yaml\n")
        .expect("the cloud-init block isn't a yaml fence")
        .1;
    body.rsplit_once("```")
        .expect("the cloud-init fence isn't closed")
        .0
        .to_string()
}

fn parsed() -> serde_json::Value {
    yaml_serde::from_str(&user_data()).expect("the cloud-init block is valid YAML")
}

/// The playbook's command line that starts with `prefix`.
fn playbook_line(prefix: &str) -> String {
    read(CHAPTER)
        .lines()
        .find(|line| line.starts_with(prefix))
        .unwrap_or_else(|| panic!("the playbook has no line starting {prefix:?}"))
        .to_string()
}

#[test]
fn the_user_data_is_cloud_config() {
    // cloud-init ignores user data whose first line isn't exactly this.
    assert_eq!(user_data().lines().next(), Some("#cloud-config"));
}

#[test]
fn the_user_logs_in_with_a_key_and_sudo_needs_no_password() {
    let value = parsed();
    let users = value["users"].as_array().expect("users is a list");
    assert_eq!(users[0], "default", "keep the image's own user too");
    let user = &users[1];
    assert_eq!(user["sudo"], "ALL=(ALL) NOPASSWD:ALL");
    assert_eq!(user["lock_passwd"], true, "no password login");
    let keys = user["ssh_authorized_keys"].as_array().unwrap();
    assert_eq!(keys.len(), 1);
    assert!(keys[0].as_str().unwrap().starts_with("ssh-ed25519 "));
}

#[test]
fn setup_logs_in_as_the_user_the_user_data_creates() {
    let name = parsed()["users"][1]["name"].as_str().unwrap().to_string();
    let line = playbook_line("relish setup --ssh ");
    let machines = line.split_whitespace().nth(3).unwrap();
    for machine in machines.split(',') {
        assert_eq!(
            machine.split_once('@').map(|(user, _)| user),
            Some(name.as_str()),
            "{line}"
        );
    }
    let wait = read(CHAPTER);
    assert!(
        wait.contains(&format!("`ssh {name}@<address> cloud-init status --wait`")),
        "the playbook waits for cloud-init as {name}"
    );
}

#[test]
fn the_packages_are_exactly_the_ones_setup_installs() {
    let packages: Vec<String> = parsed()["packages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|package| package.as_str().unwrap().to_string())
        .collect();
    assert_eq!(packages, PACKAGES);
}

#[test]
fn the_user_data_carries_no_secret() {
    let text = user_data().to_lowercase();
    for secret in [
        "token",
        "master",
        "private key",
        "rbrg_",
        "password:",
        "join",
    ] {
        assert!(!text.contains(secret), "the user data mentions {secret:?}");
    }
}

#[test]
fn the_install_line_uses_a_flag_the_installer_has() {
    let line = playbook_line("curl -fsSL https://reliaburger.com/install.sh");
    let flag = line.split_whitespace().last().unwrap();
    let installer = read("scripts/release/install.sh.in");
    assert!(
        installer.contains(&format!("      {flag})")),
        "the installer has no {flag} option"
    );
}

#[test]
fn the_setup_lines_parse_with_the_real_cli() {
    for prefix in ["relish setup --quickstart", "relish setup --ssh "] {
        let line = playbook_line(prefix);
        let output = Command::new(env!("CARGO_BIN_EXE_relish"))
            .args(line.split_whitespace().skip(1))
            .env("RELISH_PARSE_ONLY", "1")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{line}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
