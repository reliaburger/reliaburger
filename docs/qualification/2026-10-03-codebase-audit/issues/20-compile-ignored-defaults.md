# _defaults.toml silently ignores shared environment, memory and deployment settings

Suggested priority: **P2**. Affects **v0.1.4**; verified at `f4757e7789d3672d21f15ca203031d6604d7e11a`.

Verification: Documented defaults fixture compiled; fields absent.

### Problem and unsupported claim


The whitepaper promises `_defaults.toml` values for common environment variables, memory limits and deployment strategy inherited by apps unless overridden ([docs/whitepaper.md:1050](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/docs/whitepaper.md#L1050)). The manual says defaults fill fields an app leaves unset ([docs/manual/01_deploy-an-app.md:163](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/docs/manual/01_deploy-an-app.md#L163)). The compiler parses the defaults as an arbitrary TOML map but implements only the `image` key. Other valid-looking keys are silently discarded.

This can remove expected memory budgets or required environment settings from all apps in a directory while compilation exits successfully and prints no warning.

### Reproduction


```toml
# _defaults.toml
image = "web:v1"
memory = "256Mi"
[env]
MODE = "prod"
[deploy]
max_unavailable = 0
```

```toml
# web.toml
[app.web]
replicas = 2
```

The current compiler returned `image = "web:v1"`, no memory setting, an empty environment and default deployment settings, with no warnings. The retained test directly asserts image/memory/environment behavior; the deployment omission is also established by the only-field implementation below.

### Fix direction and acceptance


Implement typed defaults for the advertised supported fields and define inheritance/override semantics for scalar fields and nested env/deploy tables. Reject unsupported/misspelled defaults keys instead of accepting and ignoring them. Alternatively, explicitly refuse unsupported defaults until implementation and narrow the advertised contract.

- Image, memory, CPU, env and deployment defaults survive resolved output where supported.
- Explicit per-app values win; test partial nested tables, per-directory inheritance and malformed values.
- Unsupported keys fail with the defaults file and key named.
- Round-trip compiled output and verify resource enforcement receives the intended memory/deployment settings.

### Existing issue comparison


No matching implementation issue was found. #300’s five documentation corrections do not cover shared-defaults omission.

### Current implementation snippets

The excerpts below are verbatim from the audited checkout. Links are pinned to that commit.

[src/relish/compile.rs:182–192](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/relish/compile.rs#L182-L192)

```rust
    match toml::from_str(&content) {
        Ok(parsed) => (Some(parsed), None),
        Err(e) => (
            None,
            Some(format!(
                "{}: invalid TOML, defaults not applied: {e}",
                defaults_path.display()
            )),
        ),
    }
}
```

[src/relish/compile.rs:194–209](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/relish/compile.rs#L194-L209)

```rust
/// Apply defaults to a config. For each app, if a field from defaults
/// is missing, inject it. Currently supports the `image` default.
fn apply_defaults(config: &mut Config, defaults: &BTreeMap<String, toml::Value>) {
    let default_image = defaults
        .get("image")
        .and_then(|v| v.as_str())
        .map(String::from);

    for app in config.app.values_mut() {
        if app.image.is_none()
            && let Some(ref img) = default_image
        {
            app.image = Some(img.clone());
        }
    }
}
```
