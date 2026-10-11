//! Teardown kills only what a test started under its root (#800).
//!
//! The scan behind it reads every process on the host. On a CI runner the
//! host includes the runner, so a process outside the test that merely
//! holds a file under the root must survive, and its group with it.

use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use nix::unistd::Pid;

use super::root_processes::{self, Matched, Teardown};

fn matched(pid: i32, group: i32, names_needle: bool, descends_from_test: bool) -> Matched {
    Matched {
        pid: Pid::from_raw(pid),
        group: Some(Pid::from_raw(group)),
        names_needle,
        descends_from_test,
    }
}

const TEST_GROUP: i32 = 100;

#[test]
fn an_owner_leading_its_own_group_loses_the_whole_group() {
    let owner = matched(200, 200, true, false);
    assert_eq!(
        root_processes::decide(&owner, Pid::from_raw(TEST_GROUP)),
        Teardown::Group(Pid::from_raw(200))
    );
}

#[test]
fn a_process_in_the_tests_own_group_is_killed_alone() {
    let bun = matched(201, TEST_GROUP, true, true);
    assert_eq!(
        root_processes::decide(&bun, Pid::from_raw(TEST_GROUP)),
        Teardown::Process(Pid::from_raw(201))
    );
}

#[test]
fn a_named_process_in_someone_elses_group_is_killed_but_not_its_group() {
    // Before #800 this killed group 300 and everything in it.
    let member = matched(202, 300, true, false);
    assert_eq!(
        root_processes::decide(&member, Pid::from_raw(TEST_GROUP)),
        Teardown::Process(Pid::from_raw(202))
    );
}

#[test]
fn a_dying_descendant_that_only_holds_a_file_is_killed() {
    // A Bun whose memory is gone reads an empty command line (#673).
    let dying = matched(203, TEST_GROUP, false, true);
    assert_eq!(
        root_processes::decide(&dying, Pid::from_raw(TEST_GROUP)),
        Teardown::Process(Pid::from_raw(203))
    );
}

#[test]
fn an_outside_process_that_only_holds_a_file_is_spared() {
    for group in [204, 300, TEST_GROUP] {
        let outsider = matched(204, group, false, false);
        assert_eq!(
            root_processes::decide(&outsider, Pid::from_raw(TEST_GROUP)),
            Teardown::Spare,
            "group {group}"
        );
    }
}

#[test]
fn init_is_never_signalled() {
    let init = matched(1, 1, true, true);
    assert_eq!(
        root_processes::decide(&init, Pid::from_raw(TEST_GROUP)),
        Teardown::Spare
    );
}

fn alive(pid: Pid) -> bool {
    // A zombie has exited; only its parent's wait is missing.
    std::fs::read_to_string(format!("/proc/{pid}/stat"))
        .ok()
        .and_then(|stat| {
            stat.rsplit_once(") ")
                .map(|(_, rest)| !rest.starts_with('Z'))
        })
        .unwrap_or(false)
}

fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(10);
    while !done() {
        assert!(Instant::now() < deadline, "timed out waiting: {what}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// The PID of the process whose command line is exactly `argv`.
fn find_by_command(argv: &[&str]) -> Option<Pid> {
    let wanted: Vec<u8> = argv
        .iter()
        .flat_map(|arg| [arg.as_bytes(), b"\0"].concat())
        .collect();
    std::fs::read_dir("/proc")
        .ok()?
        .flatten()
        .find_map(|entry| {
            let pid = entry.file_name().to_str()?.parse::<i32>().ok()?;
            (std::fs::read(entry.path().join("cmdline")).ok()? == wanted)
                .then(|| Pid::from_raw(pid))
        })
}

/// The first child of `parent`, from `/proc/<parent>/task/<parent>/children`.
fn child_of(parent: u32) -> Option<Pid> {
    std::fs::read_to_string(format!("/proc/{parent}/task/{parent}/children"))
        .ok()?
        .split_whitespace()
        .next()?
        .parse()
        .ok()
        .map(Pid::from_raw)
}

#[test]
fn teardown_kills_the_tests_owner_group_and_spares_an_outsider_holding_a_file_under_the_root() {
    let root = tempfile::tempdir().unwrap();
    let held = root.path().join("held");
    std::fs::write(&held, "").unwrap();
    let mut needle = root.path().as_os_str().as_encoded_bytes().to_vec();
    needle.push(b'/');

    // An outsider: started through `setsid -f`, whose own process exits at
    // once, so the sleeper is reparented away from this test. It leads its
    // own session and group and holds a file under the root, but its
    // command line names nothing of the root's.
    let marker = format!("{}", 7000 + std::process::id() % 1000);
    let status = Command::new("setsid")
        .args([
            "-f",
            "sh",
            "-c",
            "exec 3<\"$1\"; exec sleep \"$2\"",
            "outsider",
        ])
        .arg(&held)
        .arg(&marker)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap();
    assert!(status.success());
    let mut outsider = None;
    wait_until("the outsider to start", || {
        outsider = find_by_command(&["sleep", &marker]);
        outsider.is_some_and(|pid| {
            std::fs::read_dir(format!("/proc/{pid}/fd"))
                .into_iter()
                .flatten()
                .flatten()
                .any(|fd| std::fs::read_link(fd.path()).is_ok_and(|target| target == held))
        })
    });
    let outsider = outsider.unwrap();
    assert_eq!(nix::unistd::getpgid(Some(outsider)).unwrap(), outsider);

    // A test-owned owner: it leads its own group, names the root on its
    // command line and runs a child in that group, like a Runc command.
    let mut owner = Command::new("sh")
        .args(["-c", "sleep 300; true", "owner"])
        .arg(root.path().join("owner"))
        .process_group(0)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let owner_pid = Pid::from_raw(owner.id() as i32);
    let mut owner_child = None;
    wait_until("the owner's child", || {
        owner_child = child_of(owner.id());
        owner_child.is_some()
    });
    let owner_child = owner_child.unwrap();

    let found = root_processes::matching(&[needle]);
    let pids: Vec<Pid> = found.iter().map(|process| process.pid).collect();
    assert!(pids.contains(&owner_pid), "{found:?}");
    assert!(pids.contains(&outsider), "{found:?}");
    root_processes::kill_owned(&found);

    let _ = owner.wait();
    wait_until("the owner's group to die", || !alive(owner_child));
    // Give a wrong group kill time to land before checking the outsider.
    std::thread::sleep(Duration::from_millis(200));
    let survived = alive(outsider);
    let _ = nix::sys::signal::kill(outsider, nix::sys::signal::Signal::SIGKILL);
    assert!(survived, "teardown killed a process outside the test");
}

#[test]
fn a_process_started_through_setsid_does_not_descend_from_the_test() {
    // Guard the premise of the test above: `setsid -f` really does leave
    // the sleeper outside this test's tree.
    let marker = format!("{}", 8000 + std::process::id() % 1000);
    let status = Command::new("setsid")
        .args(["-f", "sleep", &marker])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap();
    assert!(status.success());
    let mut outsider = None;
    wait_until("the outsider to start", || {
        outsider = find_by_command(&["sleep", &marker]);
        outsider.is_some()
    });
    let outsider = outsider.unwrap();
    let in_tree = root_processes::matching(&[format!("sleep\0{marker}").into_bytes()])
        .iter()
        .any(|process| process.pid == outsider && process.descends_from_test);
    let _ = nix::sys::signal::kill(outsider, nix::sys::signal::Signal::SIGKILL);
    assert!(!in_tree);
}
