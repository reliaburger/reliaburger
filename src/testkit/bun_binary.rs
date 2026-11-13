//! The Bun binary the runtime tests hand their runtimes as its owner.
//!
//! `make test-linux` names it in [`BUN_BINARY_ENV`], in cargo's real target
//! directory. It used to name `target/debug/bun` in the checkout, which
//! doesn't exist when `CARGO_TARGET_DIR` points elsewhere, as it does for
//! parallel agents. Every runtime command then failed with ENOENT in about
//! 30 ms, and the run read like a burst of flaky runc failures (#1859). So a
//! named binary that isn't there fails the test at once, naming the path.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

/// Names the Bun binary under test.
pub const BUN_BINARY_ENV: &str = "RELIABURGER_BUN_BINARY";

/// The Bun binary under test: the one [`BUN_BINARY_ENV`] names, or else the
/// one cargo builds beside this test binary.
///
/// A named binary must exist; the error names the path. The one beside the
/// test binary isn't checked: portable tests build runtimes with an owner
/// they never run, and Bun needn't be built for them.
pub fn bun_binary() -> Result<PathBuf, String> {
    let executable =
        std::env::current_exe().map_err(|error| format!("can't find this test binary: {error}"))?;
    resolve(std::env::var_os(BUN_BINARY_ENV), &executable)
}

fn resolve(named: Option<OsString>, executable: &Path) -> Result<PathBuf, String> {
    let Some(named) = named else {
        // Test binaries run from target/<profile>/deps; Bun is built in
        // target/<profile>.
        return executable
            .parent()
            .and_then(Path::parent)
            .map(|profile| profile.join("bun"))
            .ok_or_else(|| format!("{} has no build directory", executable.display()));
    };
    let path = PathBuf::from(named);
    if path.is_file() {
        Ok(path)
    } else {
        Err(format!(
            "{BUN_BINARY_ENV} names {}, which doesn't exist. Build bun first, and \
             point {BUN_BINARY_ENV} into cargo's target directory: CARGO_TARGET_DIR \
             if it's set, or target/ in the checkout (#1859)",
            path.display()
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_named_binary_that_exists_is_used() {
        let directory = tempfile::tempdir().unwrap();
        let bun = directory.path().join("bun");
        std::fs::write(&bun, b"").unwrap();
        assert_eq!(
            resolve(Some(bun.clone().into()), Path::new("/x/debug/deps/t")),
            Ok(bun)
        );
    }

    #[test]
    fn a_named_binary_that_is_missing_is_refused_by_its_path() {
        let directory = tempfile::tempdir().unwrap();
        let missing = directory.path().join("target/debug/bun");
        let error =
            resolve(Some(missing.clone().into()), Path::new("/x/debug/deps/t")).unwrap_err();
        assert!(error.contains(&missing.display().to_string()), "{error}");
        assert!(error.contains("CARGO_TARGET_DIR"), "{error}");
    }

    #[test]
    fn a_named_directory_is_no_binary() {
        let directory = tempfile::tempdir().unwrap();
        assert!(resolve(Some(directory.path().into()), Path::new("/x/debug/deps/t")).is_err());
    }

    #[test]
    fn without_a_name_the_build_beside_the_test_binary_is_named() {
        assert_eq!(
            resolve(None, Path::new("/elsewhere/target/debug/deps/runtime-0abc")),
            Ok(PathBuf::from("/elsewhere/target/debug/bun"))
        );
    }
}
