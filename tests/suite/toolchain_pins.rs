//! The devcontainer, the mutants job and the Copilot set-up use the toolchain
//! CI tests with (#1584).
//!
//! Each of them pins its own copy of the Rust version, the nextest version and
//! the packages the build needs, because each is installed by a different
//! tool: a Dockerfile, a GitHub Action, an apt line. A copy that falls behind
//! still builds, just not the way CI does, and an agent then reports "tests
//! pass" from a toolchain nobody else runs. These tests read the files and
//! fail on the first one that drifts.

use std::collections::BTreeMap;

use serde_json::Value;

use super::website::{read, repository};

const DOCKERFILE: &str = ".devcontainer/Dockerfile";
const DEVCONTAINER: &str = ".devcontainer/devcontainer.json";
const CI: &str = ".github/workflows/ci.yml";
const MUTANTS: &str = ".github/workflows/mutants.yml";
const COPILOT: &str = ".github/workflows/copilot-setup-steps.yml";

/// The default of `ARG <name>=<value>` in the devcontainer's Dockerfile.
fn dockerfile_arg(name: &str) -> String {
    let prefix = format!("ARG {name}=");
    read(DOCKERFILE)
        .lines()
        .find_map(|line| line.strip_prefix(&prefix).map(str::to_owned))
        .unwrap_or_else(|| panic!("{DOCKERFILE} has no {prefix}"))
}

/// Every package the Dockerfile's `apt-get install` names.
fn dockerfile_packages() -> Vec<String> {
    let text = read(DOCKERFILE).replace("\\\n", " ");
    let line = text
        .lines()
        .find(|line| line.contains("apt-get install"))
        .expect("the Dockerfile installs packages with apt-get");
    let after = line.split("apt-get install").nth(1).unwrap();
    let packages = after.split("&&").next().unwrap();
    packages
        .split_whitespace()
        .filter(|word| !word.starts_with('-'))
        .map(str::to_owned)
        .collect()
}

fn workflow(path: &str) -> Value {
    yaml_serde::from_str(&read(path)).unwrap_or_else(|error| panic!("{path}: {error}"))
}

fn job<'a>(workflow: &'a Value, name: &str) -> &'a Value {
    workflow["jobs"]
        .get(name)
        .unwrap_or_else(|| panic!("no job {name}"))
}

fn steps(job: &Value) -> &[Value] {
    job["steps"].as_array().expect("a job with steps")
}

/// The version after `@` of every step that uses `action`.
fn action_versions(job: &Value, action: &str) -> Vec<String> {
    steps(job)
        .iter()
        .filter_map(|step| step["uses"].as_str())
        .filter_map(|uses| uses.strip_prefix(&format!("{action}@")))
        .map(str::to_owned)
        .collect()
}

/// The single Rust a job installs with dtolnay/rust-toolchain.
fn rust_version(job: &Value) -> String {
    let versions = action_versions(job, "dtolnay/rust-toolchain");
    assert_eq!(
        versions.len(),
        1,
        "one Rust toolchain per job: {versions:?}"
    );
    versions[0].clone()
}

/// Tool name to version, from every taiki-e/install-action `tool:` list.
fn installed_tools(job: &Value) -> BTreeMap<String, String> {
    steps(job)
        .iter()
        .filter(|step| {
            step["uses"]
                .as_str()
                .is_some_and(|uses| uses.starts_with("taiki-e/install-action@"))
        })
        .filter_map(|step| step["with"]["tool"].as_str())
        .flat_map(|tools| tools.split(','))
        .filter_map(|tool| tool.trim().split_once('@'))
        .map(|(name, version)| (name.to_owned(), version.to_owned()))
        .collect()
}

/// The packages a job's step named `step` installs with apt-get.
fn apt_packages(job: &Value, step: &str) -> Vec<String> {
    let script = steps(job)
        .iter()
        .find(|candidate| candidate["name"] == step)
        .and_then(|found| found["run"].as_str())
        .unwrap_or_else(|| panic!("no step {step:?}"));
    script
        .lines()
        .filter_map(|line| line.split("apt-get install").nth(1))
        .flat_map(str::split_whitespace)
        .filter(|word| !word.starts_with('-'))
        .map(str::to_owned)
        .collect()
}

fn nextest_pin() -> String {
    let config: toml::Table = read(".config/nextest.toml").parse().unwrap();
    config["nextest-version"].as_str().unwrap().to_owned()
}

#[test]
fn the_devcontainer_builds_with_the_rust_ci_tests_with() {
    let ci = workflow(CI);
    let rust = dockerfile_arg("RUST_VERSION");
    for name in ["portable", "lint", "source-bundle", "linux"] {
        assert_eq!(
            rust_version(job(&ci, name)),
            rust,
            "{DOCKERFILE}'s RUST_VERSION must match ci.yml's {name} job"
        );
    }
}

#[test]
fn the_devcontainer_and_ci_install_the_nextest_the_repository_pins() {
    let pin = nextest_pin();
    assert_eq!(dockerfile_arg("NEXTEST_VERSION"), pin);
    let ci = workflow(CI);
    let mut checked = 0;
    for (name, job) in ci["jobs"].as_object().unwrap() {
        if let Some(version) = installed_tools(job).get("cargo-nextest") {
            assert_eq!(
                version, &pin,
                "ci.yml's {name} job installs another nextest"
            );
            checked += 1;
        }
    }
    assert!(checked > 0, "ci.yml installs nextest somewhere");
}

#[test]
fn the_devcontainer_has_every_package_the_portable_build_installs() {
    let packages = dockerfile_packages();
    let portable = apt_packages(
        job(&workflow(CI), "portable"),
        "Prepare portable build host",
    );
    assert!(!portable.is_empty());
    // The runner image already has these; a bare Ubuntu image doesn't. A C
    // compiler and libc.a for build.rs's `cc -static`, and Python 3.11 or
    // later for `make test-ci-scripts`.
    for package in portable
        .iter()
        .map(String::as_str)
        .chain(["build-essential", "python3", "git"])
    {
        assert!(
            packages.iter().any(|installed| installed == package),
            "{DOCKERFILE} doesn't install {package}: {packages:?}"
        );
    }
    assert!(
        read(DOCKERFILE).contains("FROM ubuntu:24.04\n"),
        "the devcontainer is the runners' Ubuntu (ubuntu-24.04)"
    );
}

#[test]
fn the_devcontainer_builds_its_dockerfile_as_the_image_s_own_user() {
    let config: Value = serde_json::from_str(&read(DEVCONTAINER)).unwrap();
    let dockerfile = config["build"]["dockerfile"].as_str().unwrap();
    assert!(
        repository()
            .join(".devcontainer")
            .join(dockerfile)
            .is_file(),
        "devcontainer.json builds {dockerfile}, which doesn't exist"
    );
    // Portable tests check file permissions, which root ignores.
    let user = config["remoteUser"].as_str().unwrap();
    assert_ne!(user, "root");
    let last_user = read(DOCKERFILE)
        .lines()
        .rev()
        .find_map(|line| line.strip_prefix("USER "))
        .map(str::to_owned);
    assert_eq!(
        last_user.as_deref(),
        Some(user),
        "the Dockerfile installs the toolchain as remoteUser"
    );
}

#[test]
fn the_mutants_job_tests_with_the_devcontainer_s_toolchain() {
    let mutants = workflow(MUTANTS);
    let job = job(&mutants, "mutants");
    assert_eq!(rust_version(job), dockerfile_arg("RUST_VERSION"));
    let tools = installed_tools(job);
    assert_eq!(tools.get("cargo-nextest"), Some(&nextest_pin()));
    assert_eq!(
        tools.get("cargo-mutants"),
        Some(&dockerfile_arg("MUTANTS_VERSION")),
        "a missed mutant must reproduce in the devcontainer"
    );
    let config: toml::Table = read(".cargo/mutants.toml").parse().unwrap();
    assert_eq!(config["test_tool"].as_str(), Some("nextest"));
}

#[test]
fn the_mutants_job_stays_off_the_pull_request_s_critical_path() {
    let mutants = workflow(MUTANTS);
    // Its own workflow, so ci.yml's evidence gate, the required check,
    // never waits for it.
    assert!(!read(CI).contains("mutants"));
    let job = job(&mutants, "mutants");
    assert!(
        job["runs-on"].as_str().unwrap().starts_with("ubuntu-"),
        "macOS runners are the scarce ones"
    );
    assert!(
        job.get("timeout-minutes").is_some(),
        "a bounded job: {job:#}"
    );
    let paths: Vec<&str> = mutants["on"]["pull_request"]["paths"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert!(
        paths.contains(&"src/**"),
        "pull requests run it only for code: {paths:?}"
    );
    assert_eq!(
        mutants["concurrency"]["cancel-in-progress"],
        "${{ github.event_name == 'pull_request' }}"
    );
    // A missed mutant (2) or a timeout (3) is a finding, never a red check.
    let step = steps(job)
        .iter()
        .find(|step| step["name"] == "Test the mutants")
        .unwrap();
    let test = step["run"].as_str().unwrap();
    assert!(test.contains("0|2|3) ;;"), "{test}");
    assert!(test.contains("--in-place") && test.contains("--sharding round-robin"));
    // Run 42213963003 hit the job's limit, so no summary and no artefact:
    // the step stops itself first, and reports what it finished.
    assert!(
        test.contains("timeout --signal=INT") && test.contains("124) echo \"MUTANTS_STOPPED"),
        "{test}"
    );
    // rust-cache sets CARGO_INCREMENTAL=0, which made every mutant a full
    // rebuild of the crate (7.5 minutes each on that run).
    assert_eq!(step["env"]["CARGO_INCREMENTAL"], "1");
}

#[test]
fn copilot_setup_steps_match_the_devcontainer() {
    let copilot = workflow(COPILOT);
    // Copilot's coding agent runs only a job with exactly this name, for at
    // most 59 minutes.
    let job = job(&copilot, "copilot-setup-steps");
    assert!(job["timeout-minutes"].as_u64().unwrap() <= 59);
    assert_eq!(rust_version(job), dockerfile_arg("RUST_VERSION"));
    assert_eq!(
        installed_tools(job).get("cargo-nextest"),
        Some(&nextest_pin())
    );
    let packages = dockerfile_packages();
    for package in apt_packages(job, "Prepare portable build host") {
        assert!(packages.contains(&package), "{DOCKERFILE} lacks {package}");
    }
}
