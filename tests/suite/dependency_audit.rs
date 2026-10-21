//! The audit exception for rkyv is valid only while Cargo cannot build it.
#![cfg(unix)]

use std::process::Command;

#[test]
fn audit_refuses_an_active_or_uninspectable_rkyv_dependency() {
    let dir = tempfile::tempdir().unwrap();
    let cargo = dir.path().join("cargo-fixture");
    // Shared, already-launched scripts rather than new files (#582).
    crate::first_launch::install_script(
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
    );
    crate::first_launch::install_script(&dir.path().join("date"), "#!/bin/sh\necho 20260918\n");
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

/// RUSTSEC-2024-0449: unsafe-libyaml is unmaintained. relish reads and writes
/// YAML through yaml_serde, which uses the maintained libyaml-rs port, so the
/// lock file must not pull unsafe-libyaml (or the deprecated serde_yaml) back
/// in, and the audit must not need an exception for it (#817).
#[test]
fn lock_file_has_no_unsafe_libyaml_and_audit_needs_no_exception_for_it() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let lock = std::fs::read_to_string(root.join("Cargo.lock")).unwrap();
    for banned in ["unsafe-libyaml", "serde_yaml"] {
        assert!(
            !lock.contains(&format!("name = \"{banned}\"")),
            "Cargo.lock still contains {banned}"
        );
    }
    let audit = std::fs::read_to_string(root.join(".cargo/audit.toml")).unwrap();
    assert!(
        !audit.contains("\"RUSTSEC-2024-0449\""),
        ".cargo/audit.toml still ignores RUSTSEC-2024-0449"
    );
}

/// RUSTSEC-2026-0381, -0382 and -0383: older rustls-webpki accepts paths
/// through the distrusted emSign intermediates, whose private keys are public,
/// and older webpki-roots and webpki-root-certs bundle the root they chain to.
/// Every locked copy must be a patched release (#1269).
#[test]
fn lock_file_has_only_webpki_releases_that_distrust_emsign() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let lock = std::fs::read_to_string(root.join("Cargo.lock")).unwrap();
    // The first patched release of each. webpki-roots 0.26.x is a shim that
    // re-exports the 1.x roots (`pub use parent::TLS_SERVER_ROOTS`), so 1.x
    // decides; `.cargo/audit.toml` ignores RUSTSEC-2026-0381 for the shim on
    // that basis.
    let patched = [
        ("rustls-webpki", [0, 103, 16]),
        ("webpki-roots", [1, 0, 10]),
        ("webpki-root-certs", [1, 0, 10]),
    ];
    for (name, first_patched) in patched {
        let versions: Vec<Vec<u64>> = lock
            .split("[[package]]")
            .filter(|package| package.contains(&format!("\nname = \"{name}\"\n")))
            .filter_map(|package| package.split("\nversion = \"").nth(1))
            .filter_map(|rest| rest.split('"').next())
            .map(|version| {
                version
                    .split('.')
                    .map(|part| part.parse().unwrap())
                    .collect()
            })
            .collect();
        assert!(!versions.is_empty(), "Cargo.lock has no {name}");
        for version in versions {
            let shim = name == "webpki-roots" && version[0] == 0;
            assert!(
                shim || version.as_slice() >= first_patched.as_slice(),
                "Cargo.lock locks {name} {version:?}"
            );
        }
    }
}
