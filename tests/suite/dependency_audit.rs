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

/// RUSTSEC-2024-0370: proc-macro-error is unmaintained. It came in at build
/// time through age 0.10's i18n-embed-fl 0.7. The age 0.12 bump (#1026)
/// moved to i18n-embed-fl 0.10, which uses proc-macro-error2, so the lock
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
