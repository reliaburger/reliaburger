//! The lock file stays free of crates whose advisories we used to excuse.

/// RUSTSEC-2026-0235: rkyv 0.7. Cargo used to lock it as rust_decimal's
/// optional dependency without building it, and the audit carried an
/// exception guarded by an empty `cargo tree -i rkyv`. The #1026 refresh
/// dropped it from the lock file, so the exception and its guard went too, and
/// it mustn't come back unnoticed.
#[test]
fn lock_file_has_no_rkyv_and_audit_needs_no_exception_for_it() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let lock = std::fs::read_to_string(root.join("Cargo.lock")).unwrap();
    assert!(
        !lock.contains("name = \"rkyv\""),
        "Cargo.lock contains rkyv again"
    );
    let audit = std::fs::read_to_string(root.join(".cargo/audit.toml")).unwrap();
    assert!(
        !audit.contains("\"RUSTSEC-2026-0235\""),
        ".cargo/audit.toml still ignores RUSTSEC-2026-0235"
    );
    let makefile = std::fs::read_to_string(root.join("Makefile")).unwrap();
    assert!(
        !makefile.contains("-i rkyv"),
        "make audit still asks Cargo for the rkyv graph"
    );
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

/// RUSTSEC-2025-0141: bincode is unmaintained, and its final release (3.0.0)
/// is a `compile_error!` tombstone, so there was nothing to upgrade to.
/// Gossip, reporting frames and the Raft metadata moved to postcard instead
/// (#1026), so the lock file must not pull bincode back in and the audit must
/// not need an exception for it.
#[test]
fn lock_file_has_no_bincode_and_audit_needs_no_exception_for_it() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let lock = std::fs::read_to_string(root.join("Cargo.lock")).unwrap();
    assert!(
        !lock.contains("name = \"bincode\""),
        "Cargo.lock still contains bincode"
    );
    let audit = std::fs::read_to_string(root.join(".cargo/audit.toml")).unwrap();
    assert!(
        !audit.contains("\"RUSTSEC-2025-0141\""),
        ".cargo/audit.toml still ignores RUSTSEC-2025-0141"
    );
}

/// RUSTSEC-2024-0370: proc-macro-error is unmaintained. It came in at build
/// time through age 0.10's i18n-embed-fl 0.7. The age bump (#1026, now at
/// 0.11.5) moved to i18n-embed-fl 0.9.4, which uses proc-macro-error2, so the lock
/// file must not pull it back in and the audit needs no exception for it.
#[test]
fn lock_file_has_no_proc_macro_error_and_audit_needs_no_exception_for_it() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let lock = std::fs::read_to_string(root.join("Cargo.lock")).unwrap();
    assert!(
        !lock.contains("name = \"proc-macro-error\""),
        "Cargo.lock contains proc-macro-error again"
    );
    let audit = std::fs::read_to_string(root.join(".cargo/audit.toml")).unwrap();
    assert!(
        !audit.contains("\"RUSTSEC-2024-0370\""),
        ".cargo/audit.toml still ignores RUSTSEC-2024-0370"
    );
}

/// RUSTSEC-2026-0381, -0382 and -0383: older rustls-webpki accepts paths
/// through the distrusted emSign intermediates, whose private keys are public,
/// and older webpki-roots and webpki-root-certs bundle the root they chain to.
/// Every locked copy must be a patched release (#1269), and the audit must not
/// need an exception for any of them.
#[test]
fn lock_file_has_only_webpki_releases_that_distrust_emsign() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let lock = std::fs::read_to_string(root.join("Cargo.lock")).unwrap();
    // The first patched release of each. webpki-roots 0.26.x is a shim that
    // re-exports the 1.x roots (`pub use parent::TLS_SERVER_ROOTS`), so 1.x
    // decides. RustSec's advisory lists the 0.26.11 shim as unaffected on the
    // same basis, provided the 1.x it resolves to is patched.
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
    let audit = std::fs::read_to_string(root.join(".cargo/audit.toml")).unwrap();
    for id in [
        "RUSTSEC-2026-0381",
        "RUSTSEC-2026-0382",
        "RUSTSEC-2026-0383",
    ] {
        assert!(
            !audit.contains(&format!("\"{id}\"")),
            ".cargo/audit.toml ignores {id}"
        );
    }
}

/// RUSTSEC-2026-0387: crypto-bigint before 0.7.6 has secret-dependent branches
/// in operations meant to be constant-time. age 0.12 compiled it in through
/// its P-256 hardware-token recipients, which we never use, so age went back
/// to 0.11.5, which has none of that code. No vulnerable copy may come back,
/// and the audit must not need an exception for it.
#[test]
fn lock_file_has_no_crypto_bigint_with_timing_leaks_and_audit_needs_no_exception_for_it() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let lock = std::fs::read_to_string(root.join("Cargo.lock")).unwrap();
    for package in lock.split("[[package]]") {
        if !package.contains("\nname = \"crypto-bigint\"\n") {
            continue;
        }
        let version: Vec<u64> = package
            .split("\nversion = \"")
            .nth(1)
            .and_then(|rest| rest.split('"').next())
            .unwrap()
            .split('.')
            .map(|part| part.parse().unwrap())
            .collect();
        assert!(
            version.as_slice() >= [0, 7, 6].as_slice(),
            "Cargo.lock locks crypto-bigint {version:?}"
        );
    }
    let audit = std::fs::read_to_string(root.join(".cargo/audit.toml")).unwrap();
    assert!(
        !audit.contains("\"RUSTSEC-2026-0387\""),
        ".cargo/audit.toml ignores RUSTSEC-2026-0387"
    );
}

/// RUSTSEC-2026-0391 and 0395 to 0406: zstd 0.13.3 and zstd-safe 7.3.0. The
/// source bundle (#1570) and rust-embed's include-flate moved to zstd 0.14.3,
/// so the only crates still allowed to depend on the old line are Arrow,
/// Parquet and DataFusion 55, until DataFusion 56 moves them to arrow 60
/// (#1366).
#[test]
fn only_arrow_parquet_and_datafusion_still_lock_zstd_0_13() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let lock = std::fs::read_to_string(root.join("Cargo.lock")).unwrap();
    // zstd 0.13 itself is the one package that depends on zstd-safe 7.
    let excused = [
        "arrow-ipc",
        "parquet",
        "datafusion",
        "datafusion-datasource",
        "zstd",
    ];
    for package in lock.split("[[package]]") {
        let Some(name) = package
            .split("\nname = \"")
            .nth(1)
            .and_then(|rest| rest.split('"').next())
        else {
            continue;
        };
        let old_zstd = package.contains("\"zstd 0.13") || package.contains("\"zstd-safe 7");
        assert!(
            !old_zstd || excused.contains(&name),
            "{name} depends on zstd 0.13 or zstd-safe 7"
        );
        if name == "reliaburger" {
            assert!(
                package.contains("\"zstd 0.14"),
                "reliaburger no longer depends on zstd 0.14"
            );
        }
    }
}

/// The 13 zstd advisories again: Arrow, Parquet and DataFusion 55 still ask
/// for zstd 0.13, but the patched 0.13.4 and zstd-safe 7.4.0 (published 1
/// November 2026) satisfy that, so every locked copy of either line must be a
/// patched release, and the audit must not need an exception for any of them.
#[test]
fn lock_file_has_only_patched_zstd_and_audit_needs_no_exception_for_it() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
    let lock = std::fs::read_to_string(root.join("Cargo.lock")).unwrap();
    // The first patched release of each line Cargo.lock holds.
    let patched = [
        ("zstd", [0, 13, 4]),
        ("zstd", [0, 14, 3]),
        ("zstd-safe", [7, 4, 0]),
        ("zstd-safe", [8, 2, 0]),
    ];
    let mut checked = 0;
    for package in lock.split("[[package]]") {
        for (name, first_patched) in patched {
            if !package.contains(&format!("\nname = \"{name}\"\n")) {
                continue;
            }
            let version: Vec<u64> = package
                .split("\nversion = \"")
                .nth(1)
                .and_then(|rest| rest.split('"').next())
                .unwrap()
                .split('.')
                .map(|part| part.parse().unwrap())
                .collect();
            if version[..2] != first_patched[..2] {
                continue;
            }
            assert!(
                version.as_slice() >= first_patched.as_slice(),
                "Cargo.lock locks {name} {version:?}"
            );
            checked += 1;
        }
    }
    // A line that disappears should take its row out of `patched`, not let
    // the test pass on nothing.
    assert_eq!(checked, patched.len(), "Cargo.lock lost a zstd line");
    let audit = std::fs::read_to_string(root.join(".cargo/audit.toml")).unwrap();
    for id in [
        "0391", "0395", "0396", "0397", "0398", "0399", "0400", "0401", "0402", "0403", "0404",
        "0405", "0406",
    ] {
        assert!(
            !audit.contains(&format!("\"RUSTSEC-2026-{id}\"")),
            ".cargo/audit.toml ignores RUSTSEC-2026-{id}"
        );
    }
}
