//! The audit exception for rkyv is valid only while Cargo cannot build it.
#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::process::Command;

#[test]
fn audit_refuses_an_active_or_uninspectable_rkyv_dependency() {
    let dir = tempfile::tempdir().unwrap();
    let cargo = dir.path().join("cargo-fixture");
    std::fs::write(
        &cargo,
        r#"#!/bin/sh
case "$1" in
  tree)
    case "$RKYV_FIXTURE" in
      active) echo 'rkyv v0.7.45' ;;
      unavailable) echo 'cannot resolve dependencies' >&2; exit 23 ;;
    esac
    ;;
  audit) exit 0 ;;
  *) exit 24 ;;
esac
"#,
    )
    .unwrap();
    std::fs::set_permissions(&cargo, std::fs::Permissions::from_mode(0o755)).unwrap();
    let date = dir.path().join("date");
    std::fs::write(&date, "#!/bin/sh\necho 20260918\n").unwrap();
    std::fs::set_permissions(&date, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut paths = vec![dir.path().to_path_buf()];
    paths.extend(std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    ));
    for (mode, success) in [
        ("inactive", true),
        ("active", false),
        ("unavailable", false),
    ] {
        let output = Command::new("make")
            .current_dir(env!("CARGO_MANIFEST_DIR"))
            .arg("audit")
            .arg(format!("CARGO={}", cargo.display()))
            .env("PATH", std::env::join_paths(&paths).unwrap())
            .env("RKYV_FIXTURE", mode)
            .output()
            .unwrap();
        assert_eq!(
            output.status.success(),
            success,
            "mode={mode}\nstdout={}\nstderr={}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
}
