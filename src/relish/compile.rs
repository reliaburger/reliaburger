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
    let (own_defaults, defaults_warning) = load_defaults(dir);
    if let Some(warning) = defaults_warning {
        warnings.push(warning);
    }
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

                for collision in merge_into(&mut merged, file_config) {
                    warnings.push(format!("{}: {collision}", entry_path.display()));
                }
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
                    for collision in merge_into(&mut merged, sub_result.config) {
                        warnings.push(format!("{}: {collision}", subdir.display()));
                    }
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
///
/// Returns the defaults and any reason they could not be loaded (O10). The
/// `.ok()?` this replaced turned an unreadable or malformed defaults file
/// into "there are no defaults" — so a typo in `_defaults.toml` didn't fail
/// the compile, it silently dropped the default image from every app in the
/// directory and let the error surface much later as a missing field.
fn load_defaults(dir: &Path) -> (Option<BTreeMap<String, toml::Value>>, Option<String>) {
    let defaults_path = dir.join("_defaults.toml");
    if !defaults_path.is_file() {
        return (None, None);
    }
    let content = match std::fs::read_to_string(&defaults_path) {
        Ok(content) => content,
        Err(e) => {
            return (
                None,
                Some(format!("{}: unreadable: {e}", defaults_path.display())),
            );
        }
    };
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
///
/// Returns a warning for every resource the merge *overwrote* (O10). The
/// maps are keyed by name, so `extend` silently replaced a same-named app
/// from an earlier file — split your apps across two files, name one twice
/// by accident, and `compile` would emit one of them with no hint that the
/// other ever existed. Two apps of the same name in *different* namespaces
/// are legitimate (DEP1) and are not reported.
#[must_use]
fn merge_into(target: &mut Config, source: Config) -> Vec<String> {
    let mut collisions = Vec::new();

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

    /// O10: the maps are keyed by name, so `extend` silently replaced a
    /// same-named app from an earlier file. `compile` emitted one of them
    /// and said nothing about the other.
    #[test]
    fn duplicate_app_names_in_one_namespace_are_reported() {
        let dir = TempDir::new().unwrap();
        write_file(dir.path(), "a.toml", "[app.web]\nimage = \"first:1\"\n");
        write_file(dir.path(), "b.toml", "[app.web]\nimage = \"second:1\"\n");

        let result = compile(dir.path()).unwrap();
        assert!(
            result.warnings.iter().any(|w| w.contains("duplicate app")),
            "a silently overwritten app produced no warning: {:?}",
            result.warnings
        );
        // Last file still wins — the fix is visibility, not new semantics.
        assert_eq!(result.config.app["web"].image.as_deref(), Some("second:1"));
    }

    /// Two apps of the same name in different namespaces have been
    /// legitimate since DEP1, and must not be reported as a collision.
    #[test]
    fn same_app_name_in_two_namespaces_is_not_a_duplicate() {
        let dir = TempDir::new().unwrap();
        fs::create_dir(dir.path().join("team-a")).unwrap();
        fs::create_dir(dir.path().join("team-b")).unwrap();
        write_file(
            &dir.path().join("team-a"),
            "web.toml",
            "[app.web]\nimage = \"a:1\"\n",
        );
        write_file(
            &dir.path().join("team-b"),
            "web.toml",
            "[app.web]\nimage = \"b:1\"\n",
        );

        let result = compile(dir.path()).unwrap();
        assert!(
            !result.warnings.iter().any(|w| w.contains("duplicate app")),
            "namespaced apps were reported as duplicates: {:?}",
            result.warnings
        );
    }

    /// O10: a malformed `_defaults.toml` used to be indistinguishable from
    /// no defaults at all, so the error surfaced later as a missing field.
    #[test]
    fn a_malformed_defaults_file_is_reported() {
        let dir = TempDir::new().unwrap();
        write_file(dir.path(), "_defaults.toml", "image = \"not closed\n");
        write_file(dir.path(), "a.toml", "[app.web]\nimage = \"x:1\"\n");

        let result = compile(dir.path()).unwrap();
        assert!(
            result
                .warnings
                .iter()
                .any(|w| w.contains("_defaults.toml") && w.contains("invalid TOML")),
            "a malformed defaults file was swallowed: {:?}",
            result.warnings
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
