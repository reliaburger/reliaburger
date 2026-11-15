//! Where `relish` and `bun` keep files under a user's home directory.
//!
//! The `dirs` crate answers "where is this user's home, data or cache
//! directory?". A `dirs` release that changed one of those answers on macOS
//! or Linux would silently move users' contexts, tokens, dev clusters and
//! fallback stores. These tests pin the path each function we call returns,
//! and the paths we build on top of them, against a throwaway `HOME`.
//!
//! `HOME` is process-wide, and changing it in a running test binary races
//! every other test, so a parent test re-runs this binary with a fresh
//! environment and reads what the fixture prints.
#![cfg(any(target_os = "macos", target_os = "linux"))]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

const FIXTURE_HOME: &str = "RELIABURGER_USER_DIRECTORIES_FIXTURE";

/// Runs the fixture with `HOME` set to `home` and the XDG overrides given
/// (all others cleared), and returns what it printed as `name -> path`.
fn user_directories(home: &Path, xdg: &[(&str, &Path)]) -> BTreeMap<String, PathBuf> {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args([
            "--exact",
            "user_directories::user_directories_fixture",
            "--ignored",
            "--nocapture",
            "--test-threads=1",
        ])
        .env(FIXTURE_HOME, "1")
        .env("HOME", home)
        .env_remove("RELIABURGER_HOME")
        .env_remove("XDG_DATA_HOME")
        .env_remove("XDG_CACHE_HOME")
        .env_remove("XDG_CONFIG_HOME");
    for (name, value) in xdg {
        command.env(name, value);
    }
    let output = command.output().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "fixture failed: {stdout}{}",
        String::from_utf8_lossy(&output.stderr)
    );
    // libtest prints "test <name> ... " without a newline before the
    // fixture's own output, so the marker can start mid-line.
    let paths: BTreeMap<String, PathBuf> = stdout
        .lines()
        .filter_map(|line| line.split_once("user-directory ").map(|(_, rest)| rest))
        .filter_map(|line| line.split_once('='))
        .map(|(name, path)| (name.to_string(), PathBuf::from(path)))
        .collect();
    assert!(!paths.is_empty(), "fixture printed no paths: {stdout}");
    paths
}

#[test]
#[ignore = "subprocess fixture for user directory paths"]
fn user_directories_fixture() {
    if std::env::var_os(FIXTURE_HOME).is_none() {
        return;
    }
    let print = |name: &str, path: Option<PathBuf>| {
        if let Some(path) = path {
            println!("user-directory {name}={}", path.display());
        }
    };
    print("home", dirs::home_dir());
    print("data_local", dirs::data_local_dir());
    print("cache", dirs::cache_dir());
    print(
        "relish_root",
        reliaburger::relish::local_context::root_directory().ok(),
    );
    print(
        "node_data",
        Some(reliaburger::relish::setup::default_data_dir()),
    );
}

#[test]
fn home_directory_is_the_home_environment_variable() {
    let home = tempfile::tempdir().unwrap();
    let paths = user_directories(home.path(), &[]);
    assert_eq!(paths["home"], home.path());
}

#[test]
fn relish_state_and_node_data_live_in_dot_reliaburger_under_home() {
    let home = tempfile::tempdir().unwrap();
    let paths = user_directories(home.path(), &[]);
    assert_eq!(paths["relish_root"], home.path().join(".reliaburger"));
    assert_eq!(paths["node_data"], home.path().join(".reliaburger"));
}

#[cfg(target_os = "macos")]
#[test]
fn macos_data_and_cache_directories_live_under_library() {
    let home = tempfile::tempdir().unwrap();
    let paths = user_directories(home.path(), &[]);
    assert_eq!(
        paths["data_local"],
        home.path().join("Library/Application Support")
    );
    assert_eq!(paths["cache"], home.path().join("Library/Caches"));
}

#[cfg(target_os = "macos")]
#[test]
fn macos_ignores_xdg_overrides() {
    let home = tempfile::tempdir().unwrap();
    let elsewhere = tempfile::tempdir().unwrap();
    let paths = user_directories(
        home.path(),
        &[
            ("XDG_DATA_HOME", elsewhere.path()),
            ("XDG_CACHE_HOME", elsewhere.path()),
        ],
    );
    assert_eq!(
        paths["data_local"],
        home.path().join("Library/Application Support")
    );
    assert_eq!(paths["cache"], home.path().join("Library/Caches"));
}

#[cfg(target_os = "linux")]
#[test]
fn linux_data_and_cache_directories_follow_the_xdg_defaults() {
    let home = tempfile::tempdir().unwrap();
    let paths = user_directories(home.path(), &[]);
    assert_eq!(paths["data_local"], home.path().join(".local/share"));
    assert_eq!(paths["cache"], home.path().join(".cache"));
}

#[cfg(target_os = "linux")]
#[test]
fn linux_honours_absolute_xdg_overrides() {
    let home = tempfile::tempdir().unwrap();
    let data = tempfile::tempdir().unwrap();
    let cache = tempfile::tempdir().unwrap();
    let paths = user_directories(
        home.path(),
        &[
            ("XDG_DATA_HOME", data.path()),
            ("XDG_CACHE_HOME", cache.path()),
        ],
    );
    assert_eq!(paths["data_local"], data.path());
    assert_eq!(paths["cache"], cache.path());
}
