//! Run the compiled `relish` without the developer's cluster (#2290).
//!
//! `relish` finds its cluster through `RELIABURGER_ENDPOINT`,
//! `RELIABURGER_TOKEN` and `RELIABURGER_CA_CERT`, and through the context
//! file under `RELIABURGER_HOME` (default `~/.reliaburger`). A test that
//! inherits them asks whatever cluster the developer last used: when that
//! cluster is switched off, every command waits for it before going on, and
//! a test that runs dozens of commands passes nextest's time limit.
//!
//! Set this per `Command`, never for the whole of `make ci`: the installer
//! tests in `scripts/release/test_package.py` need the real home.

#![allow(dead_code)]

use std::path::Path;
use std::process::Command;

/// The variables that point `relish` at a cluster.
pub const CLUSTER_VARIABLES: [&str; 3] = [
    "RELIABURGER_ENDPOINT",
    "RELIABURGER_TOKEN",
    "RELIABURGER_CA_CERT",
];

/// Point `command` at `home` (an empty directory the test owns) for its
/// local state and drop the cluster variables, so it sees no cluster the
/// test didn't give it. A `tokio::process::Command` passes `as_std_mut()`.
/// Variables the test sets after this call still win.
pub fn isolate_relish<'a>(command: &'a mut Command, home: &Path) -> &'a mut Command {
    command.env("RELIABURGER_HOME", home);
    for variable in CLUSTER_VARIABLES {
        command.env_remove(variable);
    }
    command
}

/// The compiled `relish`, isolated with [`isolate_relish`].
pub fn relish_command(home: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_relish"));
    isolate_relish(&mut command, home);
    command
}
