# GitOps rejects _defaults.toml and ignores directory-derived namespaces

Suggested priority: **P2**. Affects **v0.1.4**; verified at `f4757e7789d3672d21f15ca203031d6604d7e11a`.

Verification: Production execute_sync on local Git repository reproduced.

### Problem and unsupported claim


Lettuce’s directory loader differs from `relish compile`. It parses every TOML file as a complete `Config`, including `_defaults.toml`, and never performs defaults inheritance or derives namespaces from directory names. A tree documented for ordinary deployment therefore fails GitOps sync when it contains defaults, or targets `default` instead of the intended directory namespace when it does not.

The whitepaper says Lettuce works with directory trees natively immediately after describing shared defaults ([docs/whitepaper.md:1050](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/docs/whitepaper.md#L1050)). This is a concrete deployment incompatibility, not a request for a new configuration format.

### Reproduction and verified result


Commit a local test repository with `_defaults.toml` containing `image = "web:v1"` and `web.toml` containing `[app.web]` and `replicas = 2`. Execute the production `execute_sync` with empty current state. It returns `SyncResult::Failure`; the `_defaults.toml` file error names unknown field `image`. Ordinary `relish compile` accepts this tree and supplies the image.

For the namespace path, place `[app.web] image = "web:v1"` in `prod/web.toml`, with no explicit namespace. The source path leaves namespace unset and the diff resolves it to `default`; ordinary directory compilation derives `prod`. This second case was source-verified rather than separately executed.

### Evidence and fix direction


[src/lettuce/sync.rs:257](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/lettuce/sync.rs#L257) loops over all files with `Config::parse`; it only merges resource maps. [src/lettuce/diff.rs:212](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/lettuce/diff.rs#L212) resolves unset namespace to `default`. The compiler’s defaults and namespace operations live separately in [src/relish/compile.rs:90](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/relish/compile.rs#L90).

Share a deterministic directory compilation contract between CLI and GitOps, operating on the checked-out commit’s in-memory contents without bypassing signed-commit checks. Treat parsing/inheritance errors as sync refusal before desired-state mutation. Correct existing wrongly targeted resources through explicit reviewable namespace changes.

### Acceptance criteria


- The same tree yields equivalent namespace-qualified resolved specs in manual and GitOps paths, including defaults and overrides.
- `_defaults.toml` is treated as defaults, not as an invalid app config.
- Distinct namespaces containing same-named resources remain distinct or are explicitly refused until representable.
- Signed-script policy still inspects the effective configuration; invalid defaults never cause partial application.

### Existing issue comparison


No matching issue was found. #305 repairs unchanged-commit drift; it does not reconcile these two loaders’ semantics.

### Current implementation snippets

The excerpts below are verbatim from the audited checkout. Links are pinned to that commit.

[src/lettuce/sync.rs:257–281](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/lettuce/sync.rs#L257-L281)

```rust
    for (path, content) in ordered {
        let file_config = match Config::parse(content) {
            Ok(config) => config,
            Err(e) => {
                errors.insert(path.clone(), e.to_string());
                continue;
            }
        };

        // A resource named in two files is ambiguous: report it against
        // this later-sorted file and let the earlier definition stand,
        // rather than silently letting hash order pick a winner.
        if let Some(duplicate) = first_duplicate(&merged, &file_config) {
            errors.insert(
                path.clone(),
                format!("duplicate resource {duplicate} already declared in an earlier file"),
            );
            continue;
        }

        merged.app.extend(file_config.app);
        merged.job.extend(file_config.job);
        merged.namespace.extend(file_config.namespace);
        merged.permission.extend(file_config.permission);
        merged.build.extend(file_config.build);
```

[src/lettuce/diff.rs:207–215](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/lettuce/diff.rs#L207-L215)

```rust
/// The `AppId` a git-declared app resolves to.
///
/// Mirrors `config_to_desired_writes`: the app's own `namespace` field,
/// defaulting to `default`. Keeping the two derivations identical is what
/// makes GitOps and manual apply converge on the same identity.
fn app_id_for(name: &str, spec: &AppSpec) -> AppId {
    let namespace = spec.namespace.clone().unwrap_or_else(|| "default".into());
    AppId::new(name, namespace)
}
```

[src/relish/compile.rs:90–105](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/relish/compile.rs#L90-L105)

```rust
                // Apply defaults: merge default fields into apps/jobs
                // that don't have them set
                if let Some(defaults_toml) = defaults {
                    apply_defaults(&mut file_config, defaults_toml);
                }

                // Derive namespace from subdirectory name relative to root
                let namespace = derive_namespace(dir, entry_path);
                if let Some(ref ns) = namespace {
                    apply_namespace(&mut file_config, ns);
                }

                for collision in merge_into(&mut merged, file_config) {
                    warnings.push(format!("{}: {collision}", entry_path.display()));
                }
                merged_from.push(entry_path.clone());
```
