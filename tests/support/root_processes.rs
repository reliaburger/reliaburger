//! Teardown that kills only the processes a test started under its root.
//!
//! Tests that run real Bun nodes leave detached owners, Runc commands and
//! helpers behind when they fail, so their teardown scans `/proc` for
//! processes that name the test's root and kills them. The scan sees every
//! process on the host, including whatever runs the tests (a CI runner, a
//! shell, an editor), so the decision of what to signal must stay inside
//! what the test owns (#800):
//!
//! - A process whose command line names one of the test's unique paths was
//!   started for this test.
//! - A process that only holds a file under those paths counts only if it
//!   descends from the test process: anything can open a file in `/tmp`.
//! - A whole process group is killed only when an owned process leads it,
//!   as Bun's detached owners do. Any other owned process is killed alone.
//!
//! Everything else is left running, however it matched.

use std::path::Path;

use nix::unistd::Pid;

/// One process the scan found naming, or holding a file under, a needle.
#[derive(Debug, Clone)]
pub struct Matched {
    /// The process.
    pub pid: Pid,
    /// Its process group, if it could still be read.
    pub group: Option<Pid>,
    /// Whether its command line names a needle.
    pub names_needle: bool,
    /// Whether it is a descendant of the test process.
    pub descends_from_test: bool,
}

/// What teardown does to one matched process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Teardown {
    /// SIGKILL the whole group the process leads.
    Group(Pid),
    /// SIGKILL only this process.
    Process(Pid),
    /// Leave it alone: the test didn't start it.
    Spare,
}

/// Decide what to do with one matched process, given the test's own group.
pub fn decide(process: &Matched, test_group: Pid) -> Teardown {
    if process.pid.as_raw() <= 1 {
        return Teardown::Spare;
    }
    if !process.names_needle && !process.descends_from_test {
        return Teardown::Spare;
    }
    match process.group {
        // An owner leads its own group, so killing the group takes the Runc
        // child it is waiting on too. Bun shares the test's group, and
        // signalling that group would kill the test binary itself.
        Some(group) if group == process.pid && group != test_group => Teardown::Group(group),
        _ => Teardown::Process(process.pid),
    }
}

/// Every process (other than this one) whose command line, or any of whose
/// open files, contains one of `needles`.
pub fn matching(needles: &[Vec<u8>]) -> Vec<Matched> {
    let me = std::process::id() as i32;
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    let names = |bytes: &[u8]| {
        needles
            .iter()
            .any(|needle| bytes.windows(needle.len()).any(|part| part == needle))
    };
    entries
        .flatten()
        .filter_map(|entry| {
            let pid = entry.file_name().to_str()?.parse::<i32>().ok()?;
            // The test itself may hold a file under the root as it unwinds.
            if pid == me {
                return None;
            }
            // A Bun already dying from its `Child`'s kill-on-drop has dropped
            // its memory, so its command line reads empty, but its threads
            // keep its files (the kernel ownership lock among them) a moment
            // longer. Those files still name the root, so match on them too
            // and wait for the exact lock owner (#673).
            let command = std::fs::read(entry.path().join("cmdline")).unwrap_or_default();
            let names_needle = names(&command);
            if !names_needle && !holds_file_under(&entry.path(), &names) {
                return None;
            }
            let pid = Pid::from_raw(pid);
            Some(Matched {
                pid,
                group: nix::unistd::getpgid(Some(pid)).ok(),
                names_needle,
                descends_from_test: descends_from(pid, Pid::from_raw(me)),
            })
        })
        .collect()
}

/// SIGKILL what the test owns among `matched`. Returns the processes it
/// signalled, for the caller to wait on.
pub fn kill_owned(matched: &[Matched]) -> Vec<Pid> {
    use nix::sys::signal::{Signal, kill, killpg};
    let test_group = nix::unistd::getpgrp();
    let mut signalled = Vec::new();
    for process in matched {
        match decide(process, test_group) {
            Teardown::Group(group) => {
                let _ = killpg(group, Signal::SIGKILL);
                signalled.push(process.pid);
            }
            Teardown::Process(pid) => {
                let _ = kill(pid, Signal::SIGKILL);
                signalled.push(pid);
            }
            Teardown::Spare => {
                eprintln!(
                    "test cleanup: sparing process {} outside this test that holds a file under its root",
                    process.pid
                );
            }
        }
    }
    signalled
}

/// Does any thread of the process at `/proc/<pid>` hold an open file whose
/// path matches? Threads share one file table, but once the main thread has
/// exited only the others still show it.
fn holds_file_under(process: &Path, matches: &dyn Fn(&[u8]) -> bool) -> bool {
    use std::os::unix::ffi::OsStrExt;
    std::fs::read_dir(process.join("task"))
        .into_iter()
        .flatten()
        .flatten()
        .flat_map(|thread| {
            std::fs::read_dir(thread.path().join("fd"))
                .into_iter()
                .flatten()
        })
        .flatten()
        .filter_map(|fd| std::fs::read_link(fd.path()).ok())
        .any(|target| matches(target.as_os_str().as_bytes()))
}

/// Whether `ancestor` appears in the parent chain of `pid`.
fn descends_from(pid: Pid, ancestor: Pid) -> bool {
    let mut current = pid.as_raw();
    // A chain longer than this is a loop read from a racing /proc.
    for _ in 0..256 {
        let Some(parent) = parent_of(current) else {
            return false;
        };
        if parent == ancestor.as_raw() {
            return true;
        }
        if parent <= 1 {
            return false;
        }
        current = parent;
    }
    false
}

/// The parent PID from `/proc/<pid>/stat`.
fn parent_of(pid: i32) -> Option<i32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    // The command name may itself hold ") ", so split after the last one.
    let (_, rest) = stat.rsplit_once(") ")?;
    rest.split_whitespace().nth(1)?.parse().ok()
}
