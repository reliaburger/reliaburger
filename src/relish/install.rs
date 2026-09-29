//! Where the installer puts `relish`, and how to name it in instructions.
//!
//! `install.sh` keeps the binary in `<RELIABURGER_HOME>/bin` (by default
//! `~/.reliaburger/bin`) and, when `~/.local/bin` is already on `PATH`, links
//! `~/.local/bin/relish` to it. Otherwise it prints the line that adds the
//! store to `PATH`. Until the user opens a new shell, `relish` may not
//! resolve, so printed next steps name the full path instead.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

/// How to invoke the running binary from a shell: `relish` when the first
/// `relish` on `PATH` is this executable, otherwise its full path, quoted
/// for a POSIX shell when it needs to be.
pub fn invocation() -> String {
    match std::env::current_exe() {
        Ok(executable) => invocation_for(&executable, std::env::var_os("PATH").as_deref()),
        Err(_) => "relish".to_string(),
    }
}

fn invocation_for(executable: &Path, path: Option<&OsStr>) -> String {
    let running = std::fs::canonicalize(executable).unwrap_or_else(|_| executable.to_path_buf());
    let resolved = path
        .and_then(|path| first_on_path(path, "relish"))
        .and_then(|found| std::fs::canonicalize(found).ok());
    if resolved.as_deref() == Some(running.as_path()) {
        "relish".to_string()
    } else {
        shell_quote(&executable.to_string_lossy())
    }
}

/// The first executable file called `name` in a `PATH`-style list, the way a
/// shell's command lookup finds it.
fn first_on_path(path: &OsStr, name: &str) -> Option<PathBuf> {
    std::env::split_paths(path)
        .filter(|directory| directory.is_absolute())
        .map(|directory| directory.join(name))
        .find(|candidate| is_executable(candidate))
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

/// Quote `text` so a POSIX shell reads it back as one word.
pub(crate) fn shell_quote(text: &str) -> String {
    let plain = !text.is_empty()
        && text
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "/._-+:@%,=".contains(c));
    if plain {
        text.to_string()
    } else {
        format!("'{}'", text.replace('\'', r"'\''"))
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn executable(path: &Path) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn joined(directories: &[&Path]) -> std::ffi::OsString {
        std::env::join_paths(directories).unwrap()
    }

    #[test]
    fn names_relish_when_path_finds_this_binary() {
        let root = tempfile::tempdir().unwrap();
        let store = root.path().join("store/bin");
        executable(&store.join("relish"));
        let path = joined(&[Path::new("/nonexistent"), &store]);
        assert_eq!(invocation_for(&store.join("relish"), Some(&path)), "relish");
    }

    #[test]
    fn names_relish_through_the_local_bin_link() {
        let root = tempfile::tempdir().unwrap();
        let store = root.path().join("store/bin");
        let local = root.path().join("local/bin");
        executable(&store.join("relish"));
        std::fs::create_dir_all(&local).unwrap();
        std::os::unix::fs::symlink(store.join("relish"), local.join("relish")).unwrap();
        let path = joined(&[&local]);
        assert_eq!(invocation_for(&store.join("relish"), Some(&path)), "relish");
    }

    #[test]
    fn prints_the_full_path_when_relish_is_not_on_path() {
        let root = tempfile::tempdir().unwrap();
        let store = root.path().join("store/bin");
        executable(&store.join("relish"));
        let path = joined(&[Path::new("/nonexistent")]);
        let expected = store.join("relish").to_string_lossy().into_owned();
        assert_eq!(
            invocation_for(&store.join("relish"), Some(&path)),
            shell_quote(&expected)
        );
        assert_eq!(
            invocation_for(&store.join("relish"), None),
            shell_quote(&expected)
        );
    }

    #[test]
    fn prints_the_full_path_when_another_relish_comes_first() {
        let root = tempfile::tempdir().unwrap();
        let store = root.path().join("store/bin");
        let other = root.path().join("other/bin");
        executable(&store.join("relish"));
        executable(&other.join("relish"));
        let path = joined(&[&other, &store]);
        assert_ne!(invocation_for(&store.join("relish"), Some(&path)), "relish");
    }

    #[test]
    fn skips_files_that_are_not_executable_and_relative_entries() {
        let root = tempfile::tempdir().unwrap();
        let store = root.path().join("store/bin");
        let data = root.path().join("data");
        executable(&store.join("relish"));
        std::fs::create_dir_all(&data).unwrap();
        std::fs::write(data.join("relish"), "not a program").unwrap();
        let path = joined(&[Path::new("relative"), &data, &store]);
        assert_eq!(first_on_path(&path, "relish"), Some(store.join("relish")));
    }

    #[test]
    fn quotes_paths_a_shell_would_split() {
        assert_eq!(
            shell_quote("/home/a/.reliaburger/bin/relish"),
            "/home/a/.reliaburger/bin/relish"
        );
        assert_eq!(
            shell_quote("/Users/Jo Bloggs/.reliaburger/bin/relish"),
            "'/Users/Jo Bloggs/.reliaburger/bin/relish'"
        );
        assert_eq!(shell_quote("/tmp/it's"), r"'/tmp/it'\''s'");
    }
}
