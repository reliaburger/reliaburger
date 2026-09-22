//! Privileged qualification of disposable test-volume ownership.
#![cfg(target_os = "linux")]

use reliaburger::config::{AppSpec, Config};
use reliaburger::grill::btrfs::VolumeBackend;
use reliaburger::grill::volume::VolumeManager;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn provisioned() {
    assert_eq!(std::env::var("RELIABURGER_BTRFS_TESTS").as_deref(), Ok("1"));
    // SAFETY: geteuid has no pointer arguments or memory preconditions.
    assert_eq!(unsafe { nix::libc::geteuid() }, 0, "requires root");
}
fn spec(size: Option<&str>) -> AppSpec {
    let mut text =
        "[app.web]\nimage = 'test:v1'\n[[app.web.volumes]]\npath = '/data'\n".to_string();
    if let Some(size) = size {
        text.push_str(&format!("size = '{size}'\n"));
    }
    Config::parse(&text).unwrap().app.remove("web").unwrap()
}
fn command(program: &str, args: &[&str]) {
    let output = Command::new(program).args(args).output().unwrap();
    assert!(
        output.status.success(),
        "{program} {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}
struct Mounted(PathBuf);
impl Drop for Mounted {
    fn drop(&mut self) {
        let _ = Command::new("umount").arg(&self.0).status();
    }
}
fn checkpoint(root: &Path) -> PathBuf {
    root.join(".test-storage/rbtest-storage__web.checkpoint")
}

#[test]
#[ignore = "requires root, loop devices and RELIABURGER_BTRFS_TESTS=1"]
fn lease_loop_storage_keeps_busy_mount_then_retries_cleanup() {
    provisioned();
    let root = tempfile::tempdir().unwrap();
    let manager = VolumeManager::new(root.path());
    let app = spec(Some("32Mi"));
    manager
        .prepare_test_storage("rbtest-storage", "web", &app)
        .unwrap();
    let path = root.path().join("rbtest-storage/web/data");
    let guard = Mounted(path.clone());
    assert_eq!(manager.backend_of(&path), Some(VolumeBackend::LoopMount));
    std::fs::write(path.join("marker"), "persist").unwrap();
    VolumeManager::new(root.path())
        .prepare_test_storage("rbtest-storage", "web", &app)
        .unwrap();
    let mut holder = Command::new("sleep")
        .arg("60")
        .current_dir(&path)
        .stdin(Stdio::null())
        .spawn()
        .unwrap();
    let refused = manager.retire_test_storage("rbtest-storage", "web");
    let kept = checkpoint(root.path()).exists()
        && path.with_extension("img").exists()
        && path.join("marker").exists();
    holder.kill().unwrap();
    holder.wait().unwrap();
    assert!(refused.is_err(), "busy filesystem was retired");
    assert!(kept, "failed retirement discarded storage ownership");
    assert!(
        manager
            .prepare_test_storage("rbtest-storage", "web", &app)
            .is_err(),
        "retiring storage was reused"
    );
    manager
        .retire_test_storage("rbtest-storage", "web")
        .unwrap();
    std::mem::forget(guard); // Successful retirement already unmounted it.
    assert!(!path.exists());
    assert!(!path.with_extension("img").exists());
    assert!(!checkpoint(root.path()).exists());
    manager
        .retire_test_storage("rbtest-storage", "web")
        .unwrap();
}

#[test]
#[ignore = "requires root, Btrfs tools and RELIABURGER_BTRFS_TESTS=1"]
fn lease_btrfs_storage_deletes_subvolume_and_preserves_other_namespace() {
    provisioned();
    let scratch = tempfile::tempdir().unwrap();
    let image = scratch.path().join("btrfs.img");
    let root = scratch.path().join("mnt");
    std::fs::create_dir(&root).unwrap();
    command("truncate", &["-s", "256M", image.to_str().unwrap()]);
    command("mkfs.btrfs", &["-q", image.to_str().unwrap()]);
    command(
        "mount",
        &[
            "-i",
            "-o",
            "loop",
            image.to_str().unwrap(),
            root.to_str().unwrap(),
        ],
    );
    let _mount = Mounted(root.clone());
    let manager = VolumeManager::new(&root);
    manager
        .prepare_test_storage("rbtest-storage", "web", &spec(None))
        .unwrap();
    let path = root.join("rbtest-storage/web/data");
    assert_eq!(
        manager.backend_of(&path),
        Some(VolumeBackend::BtrfsSubvolume)
    );
    std::fs::write(path.join("marker"), "remove").unwrap();
    let ordinary = manager
        .create_managed_volume("default", "web", Path::new("/data"), None)
        .unwrap();
    std::fs::write(ordinary.join("marker"), "preserve").unwrap();
    manager
        .retire_test_storage("rbtest-storage", "web")
        .unwrap();
    assert!(!path.exists());
    assert!(!checkpoint(&root).exists());
    assert_eq!(
        std::fs::read_to_string(ordinary.join("marker")).unwrap(),
        "preserve"
    );
}

#[test]
#[ignore = "requires root, mount tools and RELIABURGER_BTRFS_TESTS=1"]
fn lease_unowned_nested_mount_keeps_storage_until_removed() {
    provisioned();
    let root = tempfile::tempdir().unwrap();
    let manager = VolumeManager::new(root.path());
    manager
        .prepare_test_storage("rbtest-storage", "web", &spec(None))
        .unwrap();
    let path = root.path().join("rbtest-storage/web/data");
    let nested = path.join("unowned");
    std::fs::create_dir(&nested).unwrap();
    command(
        "mount",
        &["-i", "-t", "tmpfs", "none", nested.to_str().unwrap()],
    );
    let guard = Mounted(nested.clone());
    std::fs::write(nested.join("marker"), "preserve").unwrap();
    assert!(
        manager
            .retire_test_storage("rbtest-storage", "web")
            .is_err()
    );
    assert!(checkpoint(root.path()).exists());
    assert_eq!(
        std::fs::read_to_string(nested.join("marker")).unwrap(),
        "preserve"
    );
    drop(guard);
    manager
        .retire_test_storage("rbtest-storage", "web")
        .unwrap();
    assert!(!path.exists());
}

#[test]
#[ignore = "requires root, loop devices and RELIABURGER_BTRFS_TESTS=1"]
fn lease_loop_storage_survives_owner_process_death() {
    provisioned();
    const CHILD_ROOT: &str = "RELIABURGER_STORAGE_CRASH_TEST_ROOT";
    if let Some(root) = std::env::var_os(CHILD_ROOT) {
        let root = PathBuf::from(root);
        VolumeManager::new(&root)
            .prepare_test_storage("rbtest-storage", "web", &spec(Some("32Mi")))
            .unwrap();
        std::fs::write(root.join("ready"), "mounted").unwrap();
        std::thread::sleep(std::time::Duration::from_secs(60));
        panic!("parent did not kill the storage owner");
    }
    let root = tempfile::tempdir().unwrap();
    let mut child = Command::new(std::env::current_exe().unwrap())
        .args([
            "--ignored",
            "--exact",
            "lease_loop_storage_survives_owner_process_death",
        ])
        .env(CHILD_ROOT, root.path())
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    let path = root.path().join("rbtest-storage/web/data");
    let guard = Mounted(path.clone());
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    while !root.path().join("ready").exists() {
        if child.try_wait().unwrap().is_some() {
            panic!("storage owner exited before provisioning");
        }
        if std::time::Instant::now() >= deadline {
            child.kill().unwrap();
            child.wait().unwrap();
            panic!("storage owner did not provision its mount");
        }
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    child.kill().unwrap();
    assert!(!child.wait().unwrap().success());
    assert!(checkpoint(root.path()).exists());
    let manager = VolumeManager::new(root.path());
    manager
        .prepare_test_storage("rbtest-storage", "web", &spec(Some("32Mi")))
        .unwrap();
    std::fs::write(path.join("after-recovery"), "works").unwrap();
    manager
        .retire_test_storage("rbtest-storage", "web")
        .unwrap();
    std::mem::forget(guard);
    assert!(!path.exists());
    assert!(!path.with_extension("img").exists());
    assert!(!checkpoint(root.path()).exists());
}
