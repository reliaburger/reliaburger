# CLI rejects permission and build manifests that refer to an already-created cluster namespace

Suggested priority: **P2**. Affects **v0.1.4**; verified at `f4757e7789d3672d21f15ca203031d6604d7e11a`.

Verification: Same config refused locally and accepted by validate_against reproduced.

### Problem and impact


The server supports a permission/build config targeting a namespace created by an earlier apply, but `relish apply` validates the manifest locally before contacting the server. Its bare `Config::validate` sees only namespace declarations in that one manifest and rejects the existing cluster namespace as unknown. This prevents the normal split-file workflow from reaching the server’s correct cluster-aware validator.

### Reproduction


First create namespace `prod` in the cluster. Then apply this separate file:

```toml
[permission.reader]
actions = ["logs"]
namespaces = ["prod"]
```

The retained current-library test shows `config.validate()` refuses this with an unknown-namespace error, while `config.validate_against(&["prod"])` accepts exactly the same config. `load_manifest` invokes the former before `apply_with_client` can send any request. Build blocks follow the same namespace validation logic; this report does not claim every separate build command uses this loader.

Expected: local syntax/intrinsic validation followed by the server’s authoritative namespace check, or a client check against fetched known namespaces.

### Evidence and fix direction


[src/relish/commands.rs:39](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/relish/commands.rs#L39) calls bare validation in `load_manifest`; [src/config/validate.rs:52](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/config/validate.rs#L52) validates permissions and builds against only the current config. The cluster route deliberately calls `validate_against` with committed namespaces at [src/bun/api/apply.rs:502](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/bun/api/apply.rs#L502).

Separate intrinsic validation from checks requiring server context, without weakening server-side rejection of nonexistent namespaces. Dry-run should explain when namespace existence could not be verified offline. Do not require redeclaring an existing namespace in every file; that can accidentally replace its quota spec.

### Acceptance criteria


- CLI apply reaches the cluster and accepts the separate permission/build file when its namespace exists.
- A truly missing namespace is still refused clearly.
- Existing namespace quotas are not reset by the workaround or fix.
- Test the real client-to-server path, plus offline lint/preview behavior and leader forwarding.

### Existing issue comparison


No matching issue was found. The server’s explicit separate-file support is already implemented; this is a remaining client-side obstruction.

### Current implementation snippets

The excerpts below are verbatim from the audited checkout. Links are pinned to that commit.

[src/relish/commands.rs:39–47](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/relish/commands.rs#L39-L47)

```rust
async fn load_manifest(source: &super::manifest::ManifestSource) -> Result<Config, RelishError> {
    let loaded = super::manifest::load(source).await?;
    if let Some(report) = &loaded.migration_report {
        eprint!("{report}");
        eprintln!();
    }
    loaded.config.validate()?;
    Ok(loaded.config)
}
```

[src/config/validate.rs:50–61](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/config/validate.rs#L50-L61)

```rust
        // Permissions and builds may reference namespaces declared in the
        // same file. Apply passes the already-committed desired-state
        // namespaces through `validate_against`; a bare `validate` only
        // knows about namespaces in this config.
        let declared: Vec<String> = self.namespace.keys().cloned().collect();
        for (name, perm) in &self.permission {
            validate_permission(name, perm, &declared)?;
        }
        for (name, build) in &self.build {
            validate_build(name, build, &declared)?;
        }
        Ok(())
```

[src/bun/api/apply.rs:502–516](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/bun/api/apply.rs#L502-L516)

```rust
    // namespace is rejected before any write lands.
    let known_namespaces: Vec<String> = council
        .desired_state()
        .await
        .namespaces
        .keys()
        .cloned()
        .collect();
    if let Err(e) = config.validate_against(&known_namespaces) {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({ "error": e.to_string() })),
        )
            .into_response();
    }
```
