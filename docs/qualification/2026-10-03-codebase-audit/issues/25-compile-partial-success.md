# Directory compile exits successfully and emits an incomplete manifest after workload parse failures

Suggested priority: **P2**. Affects **v0.1.4**; verified at `f4757e7789d3672d21f15ca203031d6604d7e11a`.

Verification: Good plus malformed file returns successful partial config.

### Problem and impact


`relish compile` downgrades an invalid workload file to a warning and still emits the remaining config with a successful exit. Unreadable subdirectories are also skipped in one path. The documented `relish compile config/ > all.toml` followed by `relish apply all.toml` can therefore publish an incomplete application set while CI sees a successful compilation.

A syntax error in one required app is not equivalent to a non-fatal formatting suggestion. Printing a warning to stderr does not let ordinary command pipelines distinguish a complete artifact from a partial one.

### Reproduction and verified result


Create `good.toml` with `[app.good] image = "good:v1"` and `broken.toml` with an unterminated `image = [` under `[app.broken]`. The current public compile function returns `Ok`, one merged file, one app and one warning. The CLI prints that partial TOML and returns `Ok(())`. The retained executable test verifies the successful partial compile; the exit behavior is source-verified.

Expected: fail compilation and avoid emitting a deployable success artifact when an input workload cannot be parsed/read. If partial compilation is useful, require an explicit option and make the incomplete state machine-readable.

### Fix direction and acceptance


Classify parse/read failures as hard errors, aggregate file-specific diagnostics, and validate completeness before writing stdout. Keep warnings for genuinely non-fatal conditions. Treat defaults parsing failures consistently: continuing without required defaults should not masquerade as fully resolved configuration.

- One malformed or unreadable required input makes the CLI exit nonzero and prevents successful partial artifact publication.
- A corrected tree compiles completely with deterministic output.
- A pipeline test asserts the exit code as well as the compiler’s return type.
- Explicit partial mode, if introduced, cannot accidentally look like the normal complete mode.

### Existing issue comparison


No matching issue was found. The code’s existing O10 warnings improve visibility but leave the successful incomplete artifact contract unchanged.

### Current implementation snippets

The excerpts below are verbatim from the audited checkout. Links are pinned to that commit.

[src/relish/compile.rs:106–115](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/relish/compile.rs#L106-L115)

```rust
            }
            Err(e) => {
                warnings.push(format!("{}: {e}", entry_path.display()));
            }
        }
    }

    // Recurse into subdirectories — directory name becomes the namespace
    if let Ok(read_dir) = std::fs::read_dir(dir) {
        let mut subdirs: Vec<PathBuf> = read_dir
```

[src/relish/compile.rs:136–145](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/relish/compile.rs#L136-L145)

```rust
                    // Skip unreadable directories
                }
                Err(e) => return Err(e),
            }
        }
    }

    Ok(CompileResult {
        config: merged,
        merged_from,
```

[src/relish/commands.rs:1365–1388](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/relish/commands.rs#L1365-L1388)

```rust
pub fn compile(path: &Path) -> Result<(), RelishError> {
    let result = super::compile::compile(path)?;

    if !result.warnings.is_empty() {
        for w in &result.warnings {
            eprintln!("warning: {w}");
        }
    }

    let app_count = result.config.app.len();
    let job_count = result.config.job.len();
    let file_count = result.merged_from.len();

    // Serialise the merged config as TOML
    let toml = toml::to_string_pretty(&result.config)
        .map_err(|e| RelishError::FormatFailed(e.to_string()))?;
    print!("{toml}");

    eprintln!("compiled {file_count} file(s): {app_count} app(s), {job_count} job(s)");
    Ok(())
}

/// Show structural diff between two configs.
pub fn diff(path_a: &Path, path_b: Option<&Path>) -> Result<(), RelishError> {
```
