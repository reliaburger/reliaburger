//! Shared fixture scripts: one launched file, hard-linked into each test (#582).
#![cfg(unix)]

use crate::first_launch::install_script;
use std::os::unix::fs::MetadataExt;
use std::process::Command;

const BODY: &str = "#!/bin/sh\ndir=${0%/*}\nprintf '%s\\n' \"$*\" >> \"$dir/calls\"\n";

#[test]
fn installed_scripts_are_links_to_one_shared_file() {
    let first = tempfile::tempdir().unwrap();
    let second = tempfile::tempdir().unwrap();
    install_script(&first.path().join("tool"), BODY);
    install_script(&second.path().join("tool"), BODY);
    let first_inode = std::fs::metadata(first.path().join("tool")).unwrap().ino();
    let second_inode = std::fs::metadata(second.path().join("tool")).unwrap().ino();
    assert_eq!(first_inode, second_inode);
}

#[test]
fn a_shared_script_works_on_the_files_beside_the_link_it_was_started_as() {
    let first = tempfile::tempdir().unwrap();
    let second = tempfile::tempdir().unwrap();
    for directory in [&first, &second] {
        install_script(&directory.path().join("tool"), BODY);
    }
    let status = Command::new(first.path().join("tool"))
        .arg("only-first")
        .status()
        .unwrap();
    assert!(status.success());
    assert_eq!(
        std::fs::read_to_string(first.path().join("calls")).unwrap(),
        "only-first\n"
    );
    assert!(!second.path().join("calls").exists());
}

#[test]
fn the_first_launch_argument_skips_the_fixture_body() {
    let directory = tempfile::tempdir().unwrap();
    install_script(&directory.path().join("tool"), BODY);
    let status = Command::new(directory.path().join("tool"))
        .arg("--reliaburger-first-launch")
        .status()
        .unwrap();
    assert!(status.success());
    assert!(!directory.path().join("calls").exists());
}

#[test]
fn different_bodies_get_different_shared_files() {
    let directory = tempfile::tempdir().unwrap();
    install_script(&directory.path().join("one"), BODY);
    install_script(&directory.path().join("two"), "#!/bin/sh\nexit 3\n");
    let one = std::fs::metadata(directory.path().join("one"))
        .unwrap()
        .ino();
    let two = std::fs::metadata(directory.path().join("two"))
        .unwrap()
        .ino();
    assert_ne!(one, two);
    let status = Command::new(directory.path().join("two")).status().unwrap();
    assert_eq!(status.code(), Some(3));
}

#[test]
fn installing_over_an_existing_file_replaces_it_without_writing_through() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("tool");
    install_script(&path, BODY);
    install_script(&path, "#!/bin/sh\nexit 4\n");
    // The first body's shared copy is untouched: installing it again still
    // passes the digest check and runs the original body.
    let again = directory.path().join("again");
    install_script(&again, BODY);
    assert!(Command::new(&again).status().unwrap().success());
    assert_eq!(Command::new(&path).status().unwrap().code(), Some(4));
}
