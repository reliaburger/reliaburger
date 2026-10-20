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
//! return at once.
//!
//! nextest runs every test in a process of its own, so a script each test
//! writes for itself is a new file every time, and dozens of them starting
//! together queue one behind another (#582). [`install_script`] keeps one
//! copy of each distinct script in a per-user directory, launches it once,
//! and hard-links it where the test wants it: a hard link is a second name
//! for the already-assessed file, while a copy is a new file that gets
//! assessed again. A script finds its per-test files beside the name it was
//! started as, `${0%/*}`, so one body serves every test.
//!
//! Included with `#[path = "../support/first_launch.rs"] mod first_launch;`
//! from a test binary, or `#[path = "../tests/support/first_launch.rs"]`
//! under `#[cfg(test)]` from the library's unit tests. `bun_process.rs`
//! includes it too, for `bun_process::first_launch_binaries()` (#1149), so a
//! binary that includes `bun_process.rs` reaches this module as
//! `bun_process::first_launch` rather than declaring it a second time.
#![allow(dead_code)]

use std::collections::BTreeSet;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Generous bound for one first launch. It stays below nextest's 60-second
/// termination so an assessment queue that never drains fails here, with a
/// message naming the phase, rather than as an anonymous slow test.
const FIRST_LAUNCH_BOUND: Duration = Duration::from_secs(45);

/// The argument every shared script answers by exiting at once, so it can be
/// launched before any deadline without doing its fixture work.
const FIRST_LAUNCH_ARGUMENT: &str = "--reliaburger-first-launch";

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

/// Put the shell script `body` at `path` as a hard link to the one shared,
/// already-launched copy of that script, replacing whatever was at `path`.
///
/// `body` must start with `#!/bin/sh`. The shared copy is read-only: write
/// per-test data next to the link, never through it.
pub fn install_script(path: &Path, body: &str) {
    let shared = shared_script(body);
    match std::fs::remove_file(path) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => panic!("cannot replace {}: {error}", path.display()),
    }
    if let Err(error) = std::fs::hard_link(&shared, path) {
        // Another filesystem can't hold the link. A copy works, but it is a
        // new file, so it pays a first launch of its own.
        eprintln!(
            "first-launch cannot link {} to {}: {error}; copying",
            path.display(),
            shared.display()
        );
        std::fs::copy(&shared, path).unwrap();
        first_launch(&[(path, &[FIRST_LAUNCH_ARGUMENT])]);
    }
}

/// The shared copy of `body`, named by its digest, launched once.
fn shared_script(body: &str) -> PathBuf {
    use sha2::Digest;
    let rest = body
        .strip_prefix("#!/bin/sh\n")
        .unwrap_or_else(|| panic!("a shared fixture script starts with #!/bin/sh: {body:?}"));
    let script = format!("#!/bin/sh\n[ \"${{1-}}\" = {FIRST_LAUNCH_ARGUMENT} ] && exit 0\n{rest}");
    let digest = sha2::Sha256::digest(script.as_bytes());
    let name: String = digest[..16]
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    let directory = shared_directory();
    let path = directory.join(name);
    if !path.exists() {
        // Write under a private name, then link it into place: the link
        // either creates the file whole or finds another process's copy.
        let staged = tempfile::NamedTempFile::new_in(&directory).unwrap();
        std::fs::write(staged.path(), &script).unwrap();
        std::fs::set_permissions(staged.path(), std::fs::Permissions::from_mode(0o500)).unwrap();
        match std::fs::hard_link(staged.path(), &path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => panic!("cannot publish {}: {error}", path.display()),
        }
    }
    assert_eq!(
        std::fs::read(&path).unwrap(),
        script.as_bytes(),
        "shared fixture script {} no longer matches its digest; a test wrote through a link",
        path.display()
    );
    first_launch(&[(&path, &[FIRST_LAUNCH_ARGUMENT])]);
    path
}

/// A directory only this user can write, in the system temporary directory
/// (so on the same filesystem as `tempfile`'s directories, for hard links).
fn shared_directory() -> PathBuf {
    let uid = nix::unistd::getuid().as_raw();
    let directory = std::env::temp_dir().join(format!("reliaburger-fixture-scripts-{uid}"));
    match std::fs::DirBuilder::new().mode(0o700).create(&directory) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => panic!("cannot create {}: {error}", directory.display()),
    }
    let metadata = std::fs::symlink_metadata(&directory).unwrap();
    assert!(
        metadata.is_dir() && metadata.uid() == uid && metadata.mode() & 0o022 == 0,
        "{} must be a directory this user owns that nobody else can write",
        directory.display()
    );
    directory
}
