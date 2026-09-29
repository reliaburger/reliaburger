//! Every example config under `examples/` must pass a `relish` dry run.
//!
//! Scenario files (those with `[[step]]` tables) go through
//! `relish fault scenario --dry-run`; everything else, Kubernetes YAML
//! included, through `relish apply --dry-run`. Neither needs a running agent.

use std::path::{Path, PathBuf};
use std::process::Command;

fn example_configs(directory: &Path, found: &mut Vec<PathBuf>) {
    for entry in std::fs::read_dir(directory).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            example_configs(&path, found);
        } else if path.extension().is_some_and(|extension| {
            extension == "toml" || (cfg!(feature = "kubernetes") && extension == "yaml")
        }) {
            found.push(path);
        }
    }
}

#[test]
fn every_example_config_passes_a_dry_run() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut configs = Vec::new();
    example_configs(&root.join("examples"), &mut configs);
    configs.sort();
    assert!(!configs.is_empty(), "no example configs found");

    let mut failures = Vec::new();
    for config in &configs {
        let contents = std::fs::read_to_string(config).unwrap();
        let path = config.to_str().unwrap();
        let arguments: &[&str] = if contents.lines().any(|line| line.trim() == "[[step]]") {
            &["fault", "scenario", path, "--dry-run"]
        } else {
            &["apply", path, "--dry-run"]
        };
        let output = Command::new(env!("CARGO_BIN_EXE_relish"))
            .args(arguments)
            .current_dir(root)
            .output()
            .unwrap();
        if !output.status.success() {
            failures.push(format!(
                "{}:\n{}{}",
                config.strip_prefix(root).unwrap().display(),
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr)
            ));
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {} examples failed a dry run:\n\n{}",
        failures.len(),
        configs.len(),
        failures.join("\n")
    );
}
