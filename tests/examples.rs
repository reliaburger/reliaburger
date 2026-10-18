//! Every example config under `examples/` must pass a `relish` dry run.
//!
//! Scenario files (those with `[[step]]` tables) go through
//! `relish fault scenario --dry-run`; everything else, Kubernetes YAML
//! included, through `relish apply --dry-run`. Neither needs a running agent.
//! Node configs under `examples/node/` must parse as `NodeConfig`.

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
        // `examples/node/` holds node configs for `bun --config`, not
        // workloads: they must parse as one.
        if config.starts_with(root.join("examples/node")) {
            if let Err(error) = reliaburger::config::NodeConfig::parse(&contents) {
                failures.push(format!(
                    "{}: not a node config: {error}",
                    config.strip_prefix(root).unwrap().display()
                ));
            }
            continue;
        }
        // The GitOps repository is one tree, with `_defaults.toml` files
        // and directory namespaces: it's compiled and checked as a whole by
        // `gitops_example_repository_compiles_and_passes_a_dry_run`.
        if config.starts_with(root.join("examples/gitops")) {
            continue;
        }
        let arguments: &[&str] = if contents.lines().any(|line| line.trim() == "[[cohort]]") {
            &["batch", "submit", path, "--dry-run"]
        } else if contents.lines().any(|line| line.trim() == "[[step]]") {
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

/// The runtime prefixes CLAUDE.md's example naming convention allows.
const RUNTIME_PREFIXES: [&str; 4] = ["proc-", "container-", "apple-", "runc-"];

#[test]
fn example_names_follow_the_runtime_prefix_convention() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut offenders = Vec::new();
    for entry in std::fs::read_dir(root.join("examples")).unwrap() {
        let directory = entry.unwrap().path();
        let is_phase = directory
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("phase-"));
        if !directory.is_dir() || !is_phase {
            continue;
        }
        for file in std::fs::read_dir(&directory).unwrap() {
            let file = file.unwrap().path();
            let name = file.file_name().unwrap().to_str().unwrap().to_string();
            if !RUNTIME_PREFIXES
                .iter()
                .any(|prefix| name.starts_with(prefix))
            {
                offenders.push(file.strip_prefix(root).unwrap().display().to_string());
            }
        }
    }
    offenders.sort();
    assert!(
        offenders.is_empty(),
        "examples without a runtime prefix ({}): {offenders:?}",
        RUNTIME_PREFIXES.join(", ")
    );
}

/// Every app name a shipped example defines: TOML `[app.*]` tables and
/// Kubernetes Deployments.
fn shipped_app_names(root: &Path) -> std::collections::BTreeSet<String> {
    let mut configs = Vec::new();
    example_configs(&root.join("examples"), &mut configs);
    let mut names = std::collections::BTreeSet::new();
    for config in configs {
        let contents = std::fs::read_to_string(&config).unwrap();
        if config
            .extension()
            .is_some_and(|extension| extension == "yaml")
        {
            for document in yaml_serde::Deserializer::from_str(&contents) {
                let value: yaml_serde::Value = serde::Deserialize::deserialize(document).unwrap();
                if value["kind"].as_str() == Some("Deployment")
                    && let Some(name) = value["metadata"]["name"].as_str()
                {
                    names.insert(name.to_string());
                }
            }
        } else if let Ok(parsed) = reliaburger::config::Config::parse(&contents) {
            names.extend(parsed.app.keys().cloned());
        }
    }
    names
}

#[test]
fn chaos_scenario_example_targets_apps_a_shipped_example_defines() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let scenario = root.join("examples/phase-8/container-chaos-scenario.toml");
    let output = Command::new(env!("CARGO_BIN_EXE_relish"))
        .args(["fault", "scenario", scenario.to_str().unwrap(), "--dry-run"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "the scenario's dry run failed:\n{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let parsed: reliaburger::smoker::types::ScriptedScenario =
        toml::from_str(&std::fs::read_to_string(&scenario).unwrap()).unwrap();
    let apps = shipped_app_names(root);
    let unknown: Vec<&str> = parsed
        .steps
        .iter()
        .flat_map(|step| std::iter::once(&step.target).chain(step.from.as_ref()))
        .map(String::as_str)
        .filter(|target| !apps.contains(*target))
        .collect();
    assert!(
        unknown.is_empty(),
        "scenario steps name apps no example defines: {unknown:?} (known: {apps:?})"
    );

    let contents = std::fs::read_to_string(&scenario).unwrap();
    assert!(
        contents.contains("--acknowledge"),
        "the scenario's run line must pass --acknowledge, which every injection needs"
    );
}

#[test]
fn build_example_contexts_hold_a_dockerfile() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut configs = Vec::new();
    example_configs(&root.join("examples"), &mut configs);
    let mut builds = 0;
    let mut missing = Vec::new();
    for config in configs.iter().filter(|config| {
        config
            .extension()
            .is_some_and(|extension| extension == "toml")
    }) {
        let Ok(parsed) =
            reliaburger::config::Config::parse(&std::fs::read_to_string(config).unwrap())
        else {
            continue;
        };
        for (name, spec) in &parsed.build {
            builds += 1;
            // `relish build` resolves a relative context against the
            // directory of the file that declares it.
            let context = config.parent().unwrap().join(&spec.context);
            if !context.join(&spec.dockerfile).is_file() {
                missing.push(format!(
                    "{} [build.{name}]: no {} in {}",
                    config.strip_prefix(root).unwrap().display(),
                    spec.dockerfile,
                    context.display()
                ));
            }
        }
    }
    assert!(
        builds >= 2,
        "expected the burger and phase-8 builds, found {builds}"
    );
    assert!(
        missing.is_empty(),
        "build contexts without a Dockerfile:\n{}",
        missing.join("\n")
    );
}

#[test]
fn process_examples_run_no_images() {
    // A process-only node refuses an image app (#914), so a `proc-*` example
    // that names one can't run as its header says.
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut configs = Vec::new();
    example_configs(&root.join("examples"), &mut configs);
    let mut offenders = Vec::new();
    for config in &configs {
        let name = config.file_name().unwrap().to_str().unwrap();
        if !name.starts_with("proc-") {
            continue;
        }
        let parsed =
            reliaburger::config::Config::parse(&std::fs::read_to_string(config).unwrap()).unwrap();
        let apps = parsed
            .app
            .iter()
            .filter(|(_, app)| app.image.is_some())
            .map(|(workload, _)| format!("{name}: app.{workload}"));
        let jobs = parsed
            .job
            .iter()
            .filter(|(_, job)| job.image.is_some())
            .map(|(workload, _)| format!("{name}: job.{workload}"));
        offenders.extend(apps.chain(jobs));
    }
    assert!(
        offenders.is_empty(),
        "proc-* examples with an image: {offenders:?}"
    );
}

#[test]
fn gitops_example_repository_compiles_and_passes_a_dry_run() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let repository = root.join("examples/gitops/repository");
    assert!(repository.is_dir(), "no example GitOps repository");
    let compiled = Command::new(env!("CARGO_BIN_EXE_relish"))
        .args(["compile", repository.to_str().unwrap()])
        .output()
        .unwrap();
    assert!(
        compiled.status.success(),
        "relish compile failed:\n{}",
        String::from_utf8_lossy(&compiled.stderr)
    );
    let merged_text = String::from_utf8(compiled.stdout).unwrap();
    let parsed = reliaburger::config::Config::parse(&merged_text).unwrap();
    let namespaces: std::collections::BTreeSet<_> = parsed
        .app
        .values()
        .filter_map(|app| app.namespace.clone())
        .collect();
    assert!(
        namespaces.len() >= 2,
        "the repository should show directory namespaces, got {namespaces:?}"
    );

    let directory = tempfile::tempdir().unwrap();
    let merged = directory.path().join("all.toml");
    std::fs::write(&merged, &merged_text).unwrap();
    let dry_run = Command::new(env!("CARGO_BIN_EXE_relish"))
        .args(["apply", merged.to_str().unwrap(), "--dry-run"])
        .output()
        .unwrap();
    assert!(
        dry_run.status.success(),
        "the compiled repository failed a dry run:\n{}{}",
        String::from_utf8_lossy(&dry_run.stdout),
        String::from_utf8_lossy(&dry_run.stderr)
    );
}
