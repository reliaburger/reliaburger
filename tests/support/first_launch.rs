//! Pay an executable's one-time first-launch cost before a timed phase.
//!
//! On macOS the first `exec` of a newly written executable waits for
//! `syspolicyd` to assess it, unless the launching terminal has the Developer
//! Tools permission (CI runners and agent shells don't). The wait happens in
//! the child after `posix_spawn` has already returned, so it lands inside
//! whatever deadline the test put around the child. Assessments are
//! serialised host-wide: a freshly linked ~490 MB debug `relish` took 4 to 8
//! seconds alone and more than 15 seconds while two other fresh binaries were
//! being assessed (#573). A small fresh script waits behind them too.
//!
//! [`first_launch`] runs the executable once, outside the test's behavioural
//! deadline, and reports the time as its own phase. The first launch of each
//! path in a test process is the only one that does any work; later calls
//! return at once. Fixtures that need several copies of one script should
//! write it once and hard-link it: a hard link shares the assessed file, while
//! a copy is a new file that gets assessed again.
//!
//! Included with `#[path = "../support/first_launch.rs"] mod first_launch;`.
#![allow(dead_code)]

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Generous bound for one first launch. It stays below nextest's 60-second
/// termination so an assessment queue that never drains fails here, with a
/// message naming the phase, rather than as an anonymous slow test.
const FIRST_LAUNCH_BOUND: Duration = Duration::from_secs(45);

/// Paths already launched by this test process.
static LAUNCHED: Mutex<BTreeSet<PathBuf>> = Mutex::new(BTreeSet::new());

/// Run each `(program, args)` once, all at the same time, and wait for every
/// one to exit, so a later timed launch of the same files measures only the
/// programs. Starting them together means they wait in the host's assessment
/// queue side by side, not one after another. Exit statuses are ignored: the
/// warm-up proves each file can start, not what it does. Panics with the
/// elapsed time if a program can't start or doesn't exit within the bound,
/// after killing and reaping every child it started.
pub fn first_launch(launches: &[(&Path, &[&str])]) {
    let started = Instant::now();
    let mut running = Vec::new();
    let mut failures = Vec::new();
    for (program, args) in launches {
        // A std mutex is fine here: it guards a set lookup in a synchronous
        // helper and is never held across a launch or an `.await`.
        if LAUNCHED.lock().unwrap().contains(*program) {
            continue;
        }
        match Command::new(program)
            .args(*args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(child) => running.push((*program, child)),
            Err(error) => failures.push(format!("{} did not start: {error}", program.display())),
        }
    }
    while failures.is_empty() && !running.is_empty() {
        let mut still_running = Vec::new();
        for (program, mut child) in running {
            match child.try_wait() {
                Ok(Some(_)) => {
                    eprintln!(
                        "first-launch program={} elapsed={:?}",
                        program.display(),
                        started.elapsed()
                    );
                    LAUNCHED.lock().unwrap().insert(program.to_path_buf());
                }
                Ok(None) => still_running.push((program, child)),
                Err(error) => {
                    failures.push(format!(
                        "{} could not be polled: {error}",
                        program.display()
                    ));
                    still_running.push((program, child));
                }
            }
        }
        running = still_running;
        if !running.is_empty() && started.elapsed() > FIRST_LAUNCH_BOUND {
            failures.push(format!(
                "first launches did not exit within {FIRST_LAUNCH_BOUND:?} (host executable \
                 assessment or a stuck program)"
            ));
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    if failures.is_empty() {
        return;
    }
    // Own every child through kill and reap before failing the test.
    for (program, mut child) in running {
        let kill = child.kill();
        let reap = child.wait();
        failures.push(format!(
            "{} still running: kill={kill:?} reap={reap:?}",
            program.display()
        ));
    }
    panic!(
        "first launch failed after {:?}: {failures:?}",
        started.elapsed()
    );
}
