/// Config compilation for Reliaburger.
///
/// Walks a directory of TOML files, discovers `_defaults.toml` files,
/// merges defaults into each app/job spec, and returns a single resolved
/// `Config`. Directory names become namespaces.
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::config::Config;

use super::RelishError;

/// Result of compiling a config directory.
#[derive(Debug)]
pub struct CompileResult {
    /// The merged configuration.
    pub config: Config,
    /// Files that were successfully merged.
    pub merged_from: Vec<PathBuf>,
    /// Warnings (e.g. parse errors in individual files).
    pub warnings: Vec<String>,
}

/// Compile a config file or directory into a single resolved `Config`.
///
/// If `path` is a file, parses it directly. If a directory, walks it
/// recursively, discovers `_defaults.toml`, merges defaults into each
/// config, and combines everything into one `Config`.
pub fn compile(path: &Path) -> Result<CompileResult, RelishError> {
    if path.is_file() {
        return compile_single_file(path);
    }

    if !path.is_dir() {
        return Err(RelishError::Io(std::io::Error::new(
            std::io::ErrorKind::NotFound,
            format!("{} is not a file or directory", path.display()),
        )));
    }

    compile_directory(path)
}

/// Compile a single TOML file.
fn compile_single_file(path: &Path) -> Result<CompileResult, RelishError> {
    let config = Config::from_file(path)?;
    Ok(CompileResult {
        config,
        merged_from: vec![path.to_path_buf()],
        warnings: Vec::new(),
    })
}

/// Compile a directory of TOML files.
fn compile_directory(dir: &Path) -> Result<CompileResult, RelishError> {
    compile_directory_with_defaults(dir, None)
}

/// Compile a directory, inheriting defaults from the parent if the
/// directory doesn't have its own `_defaults.toml`.
fn compile_directory_with_defaults(
    dir: &Path,
    parent_defaults: Option<&BTreeMap<String, toml::Value>>,
) -> Result<CompileResult, RelishError> {
    let mut merged = Config::default();
    let mut merged_from = Vec::new();
    let mut warnings = Vec::new();

    // Load defaults: own file takes priority, fall back to parent's
    let own_defaults = load_defaults(dir);
    let defaults = own_defaults.as_ref().or(parent_defaults);

    // Process all .toml files in this directory (except _defaults.toml)
    let entries = collect_toml_files(dir)?;

    for entry_path in &entries {
        let filename = entry_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("");
        if filename == "_defaults.toml" {
            continue;
        }

        match Config::from_file(entry_path) {
            Ok(mut file_config) => {
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

                merge_into(&mut merged, file_config);
                merged_from.push(entry_path.clone());
            }
            Err(e) => {
                warnings.push(format!("{}: {e}", entry_path.display()));
            }
        }
    }

    // Recurse into subdirectories — directory name becomes the namespace
    if let Ok(read_dir) = std::fs::read_dir(dir) {
        let mut subdirs: Vec<PathBuf> = read_dir
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.is_dir())
            .collect();
        subdirs.sort();

        for subdir in subdirs {
            match compile_directory_with_defaults(&subdir, defaults) {
                Ok(mut sub_result) => {
                    // Apply the subdirectory name as namespace
                    if let Some(ns) = subdir.file_name().and_then(|n| n.to_str()) {
                        apply_namespace(&mut sub_result.config, ns);
                    }
                    merge_into(&mut merged, sub_result.config);
                    merged_from.extend(sub_result.merged_from);
                    warnings.extend(sub_result.warnings);
                }
                Err(RelishError::Io(_)) => {
                    // Skip unreadable directories
                }
                Err(e) => return Err(e),
            }
        }
    }

    Ok(CompileResult {
        config: merged,
        merged_from,
        warnings,
    })
}

/// Collect all .toml files in a directory (non-recursive, sorted).
fn collect_toml_files(dir: &Path) -> Result<Vec<PathBuf>, RelishError> {
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.is_file() && p.extension().is_some_and(|ext| ext == "toml"))
        .collect();
    files.sort();
    Ok(files)
}

/// Load `_defaults.toml` from a directory, if present.
fn load_defaults(dir: &Path) -> Option<BTreeMap<String, toml::Value>> {
    let defaults_path = dir.join("_defaults.toml");
    if !defaults_path.is_file() {
        return None;
    }
    let content = std::fs::read_to_string(&defaults_path).ok()?;
    toml::from_str(&content).ok()
}

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

/// Derive namespace from the path relative to the root directory.
/// If the file is directly in the root, returns None.
fn derive_namespace(root: &Path, file: &Path) -> Option<String> {
    let parent = file.parent()?;
    if parent == root {
        return None;
    }
    parent.file_name()?.to_str().map(String::from)
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
        }
    }
}

/// Merge `source` into `target`, appending all resources.
fn merge_into(target: &mut Config, source: Config) {
    target.app.extend(source.app);
    target.job.extend(source.job);
    target.namespace.extend(source.namespace);
    target.permission.extend(source.permission);
    target.build.extend(source.build);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn write_file(dir: &Path, name: &str, content: &str) {
        fs::write(dir.join(name), content).unwrap();
    }

    #[test]
    fn compile_single_file_parses() {
        let dir = TempDir::new().unwrap();
        write_file(
            dir.path(),
            "app.toml",
            r#"
            [app.web]
            image = "myapp:v1"
            "#,
        );

        let result = compile(&dir.path().join("app.toml")).unwrap();
        assert_eq!(result.config.app.len(), 1);
        assert!(result.config.app.contains_key("web"));
        assert_eq!(result.merged_from.len(), 1);
        assert!(result.warnings.is_empty());
    }

    #[test]
    fn compile_merges_defaults_toml() {
        let dir = TempDir::new().unwrap();
        write_file(
            dir.path(),
            "_defaults.toml",
            r#"
            image = "base:v1"
            "#,
        );
        write_file(
            dir.path(),
            "app.toml",
            r#"
            [app.web]
            replicas = 3
            "#,
        );

        let result = compile(dir.path()).unwrap();
        let web = &result.config.app["web"];
        assert_eq!(
            web.image.as_deref(),
            Some("base:v1"),
            "default image should be applied"
        );
    }

    #[test]
    fn compile_defaults_dont_override_explicit() {
        let dir = TempDir::new().unwrap();
        write_file(
            dir.path(),
            "_defaults.toml",
            r#"
            image = "base:v1"
            "#,
        );
        write_file(
            dir.path(),
            "app.toml",
            r#"
            [app.web]
            image = "custom:v2"
            "#,
        );

        let result = compile(dir.path()).unwrap();
        let web = &result.config.app["web"];
        assert_eq!(
            web.image.as_deref(),
            Some("custom:v2"),
            "explicit image should not be overridden"
        );
    }

    #[test]
    fn compile_directory_namespace_inheritance() {
        let dir = TempDir::new().unwrap();
        let subdir = dir.path().join("backend");
        fs::create_dir(&subdir).unwrap();

        write_file(
            &subdir,
            "app.toml",
            r#"
            [app.api]
            image = "api:v1"
            "#,
        );

        let result = compile(dir.path()).unwrap();
        let api = &result.config.app["api"];
        assert_eq!(
            api.namespace.as_deref(),
            Some("backend"),
            "subdirectory name should become namespace"
        );
    }

    #[test]
    fn compile_invalid_file_skipped_with_warning() {
        let dir = TempDir::new().unwrap();
        write_file(dir.path(), "bad.toml", "this is not valid toml [[[");
        write_file(
            dir.path(),
            "good.toml",
            r#"
            [app.web]
            image = "myapp:v1"
            "#,
        );

        let result = compile(dir.path()).unwrap();
        assert_eq!(result.config.app.len(), 1, "valid file should be parsed");
        assert_eq!(result.warnings.len(), 1, "bad file should produce warning");
        assert!(result.warnings[0].contains("bad.toml"));
    }

    #[test]
    fn compile_multiple_files_merged() {
        let dir = TempDir::new().unwrap();
        write_file(
            dir.path(),
            "apps.toml",
            r#"
            [app.web]
            image = "web:v1"
            "#,
        );
        write_file(
            dir.path(),
            "jobs.toml",
            r#"
            [job.migrate]
            image = "migrate:v1"
            "#,
        );

        let result = compile(dir.path()).unwrap();
        assert_eq!(result.config.app.len(), 1);
        assert_eq!(result.config.job.len(), 1);
        assert_eq!(result.merged_from.len(), 2);
    }

    #[test]
    fn compile_nonexistent_path_errors() {
        let result = compile(Path::new("/nonexistent/path/nothing.toml"));
        assert!(result.is_err());
    }
}
