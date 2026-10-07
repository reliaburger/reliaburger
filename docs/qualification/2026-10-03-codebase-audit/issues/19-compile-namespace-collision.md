# Directory compile silently drops same-named workloads from distinct namespaces

Suggested priority: **P1**. Affects **v0.1.4**; verified at `f4757e7789d3672d21f15ca203031d6604d7e11a`.

Verification: Two-file directory compilation reproduced.

### Problem and impact


`relish compile` keys merged apps and jobs by bare name. Two legitimate namespace-qualified workloads are inserted under the same key, so the later definition replaces the earlier one. The collision warning is explicitly conditional on the namespaces being equal; cross-namespace loss produces no warning. Operators can compile and apply an apparently complete production/staging tree while one workload is absent.

### Reproduction


Create these files, then run `relish compile config/`:

```toml
# config/prod/web.toml
[app.web]
image = "prod:v1"
```

```toml
# config/staging/web.toml
[app.web]
image = "staging:v1"
```

The current-library test compiled two files successfully with zero warnings, but returned one app, `staging/web`. The app map length was 1; the same `insert` implementation affects jobs. The manual explicitly derives each subdirectory’s namespace ([docs/manual/01_deploy-an-app.md:162](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/docs/manual/01_deploy-an-app.md#L162)).

Expected: preserve both distinct workload identities. If the serialized single-config representation cannot express both yet, compilation must fail clearly instead of silently claiming success.

### Evidence and fix direction


[src/relish/compile.rs:247](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/relish/compile.rs#L247) checks for a same-namespace collision, then unconditionally inserts by bare name. Both the app and job loops have this shape. `Config` itself uses bare-name maps, so only changing the warning cannot deliver namespace-preserving output.

Carry namespace-qualified resource identity through directory compilation, serialization and apply. Ensure the fix also covers jobs and builds with namespace ownership rather than introducing a second silent collision elsewhere. If a format change is needed, follow the repository’s pre-1.0 compatibility policy.

### Acceptance criteria


- A prod/staging tree containing two `web` apps emits and applies both, retaining images and namespace identities.
- Cover same-named jobs in distinct namespaces and explicit namespace overrides.
- Ambiguous duplicate definitions within one namespace follow an explicit deterministic policy with visible diagnostics.
- Round-trip the resolved output through parsing and apply; checking only the compiler’s intermediate map is insufficient.

### Existing issue comparison


No matching issue was found. #398 fixed instance ordinals across nodes, not directory compilation’s loss of namespace-qualified desired resources.

### Current implementation snippets

The excerpts below are verbatim from the audited checkout. Links are pinned to that commit.

[src/relish/compile.rs:247–280](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/relish/compile.rs#L247-L280)

```rust

    for (name, spec) in source.app {
        let namespace = spec.namespace.clone();
        if let Some(existing) = target.app.get(&name)
            && existing.namespace == namespace
        {
            collisions.push(format!(
                "duplicate app {:?} in namespace {:?}: the later definition wins",
                name,
                namespace.as_deref().unwrap_or("default")
            ));
        }
        target.app.insert(name, spec);
    }

    for (name, spec) in source.job {
        let namespace = spec.namespace.clone();
        if let Some(existing) = target.job.get(&name)
            && existing.namespace == namespace
        {
            collisions.push(format!(
                "duplicate job {:?} in namespace {:?}: the later definition wins",
                name,
                namespace.as_deref().unwrap_or("default")
            ));
        }
        target.job.insert(name, spec);
    }

    target.namespace.extend(source.namespace);
    target.permission.extend(source.permission);
    target.build.extend(source.build);
    collisions
}
```

[src/relish/compile.rs:219–231](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/relish/compile.rs#L219-L231)

```rust
}

/// Apply a namespace to all apps and jobs in a config that don't
/// already have one set.
fn apply_namespace(config: &mut Config, namespace: &str) {
    for app in config.app.values_mut() {
        if app.namespace.is_none() {
            app.namespace = Some(namespace.to_string());
        }
    }
    for job in config.job.values_mut() {
        if job.namespace.is_none() {
            job.namespace = Some(namespace.to_string());
```
