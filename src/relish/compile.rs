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
    /// Non-fatal duplicate-definition warnings; incomplete trees are errors.
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
    let own_defaults = load_defaults(dir)?;
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

                for collision in merge_into(&mut merged, file_config)? {
                    warnings.push(format!("{}: {collision}", entry_path.display()));
                }
                merged_from.push(entry_path.clone());
            }
            Err(e) => {
                return Err(RelishError::FormatFailed(format!(
                    "{}: {e}",
                    entry_path.display()
                )));
            }
        }
    }

    // Recurse into subdirectories — directory name becomes the namespace
    {
        let read_dir = std::fs::read_dir(dir)?;
        let mut subdirs = Vec::new();
        for entry in read_dir {
            let path = entry?.path();
            let metadata = std::fs::metadata(&path).map_err(|error| {
                RelishError::FormatFailed(format!("{}: {error}", path.display()))
            })?;
            if metadata.is_dir() {
                subdirs.push(path);
            }
        }
        subdirs.sort();

        for subdir in subdirs {
            let mut sub_result = compile_directory_with_defaults(&subdir, defaults)?;
            // Apply the subdirectory name as namespace.
            if let Some(ns) = subdir.file_name().and_then(|n| n.to_str()) {
                apply_namespace(&mut sub_result.config, ns);
            }
            for collision in merge_into(&mut merged, sub_result.config)? {
                warnings.push(format!("{}: {collision}", subdir.display()));
            }
            merged_from.extend(sub_result.merged_from);
            warnings.extend(sub_result.warnings);
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
    let mut files = Vec::new();
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if path
            .extension()
            .is_some_and(|extension| extension == "toml")
        {
            let metadata = std::fs::metadata(&path).map_err(|error| {
                RelishError::FormatFailed(format!("{}: {error}", path.display()))
            })?;
            if metadata.is_file() {
                files.push(path);
            }
        }
    }
    files.sort();
    Ok(files)
}

/// Read defaults strictly: errors must not turn a tree into a partial manifest.
fn load_defaults(dir: &Path) -> Result<Option<BTreeMap<String, toml::Value>>, RelishError> {
    let path = dir.join("_defaults.toml");
    match std::fs::symlink_metadata(&path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(RelishError::FormatFailed(format!(
                "{}: {error}",
                path.display()
            )));
        }
        Ok(_) => {}
    }
    let content = std::fs::read_to_string(&path)
        .map_err(|error| RelishError::FormatFailed(format!("{}: {error}", path.display())))?;
    toml::from_str(&content)
        .map(Some)
        .map_err(|error| RelishError::FormatFailed(format!("{}: {error}", path.display())))
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

/// Apply a namespace to all apps, jobs and builds in a config that don't
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
    for build in config.build.values_mut() {
        if build.namespace.is_none() {
            build.namespace = Some(namespace.to_string());
        }
    }
}

/// Merge resources, refusing identities that the bare-name maps cannot express.
/// Same-namespace duplicates retain the existing last-file-wins warning policy.
fn merge_into(target: &mut Config, source: Config) -> Result<Vec<String>, RelishError> {
    let mut collisions = Vec::new();

    for (name, spec) in source.app {
        if let Some(existing) = target.app.get(&name) {
            check_namespace_collision(
                "app",
                &name,
                existing.namespace.as_deref(),
                spec.namespace.as_deref(),
            )?;
            collisions.push(format!(
                "duplicate app {name:?} in namespace {:?}: the later definition wins",
                spec.namespace.as_deref().unwrap_or("default")
            ));
        }
        target.app.insert(name, spec);
    }
    for (name, spec) in source.job {
        if let Some(existing) = target.job.get(&name) {
            check_namespace_collision(
                "job",
                &name,
                existing.namespace.as_deref(),
                spec.namespace.as_deref(),
            )?;
            collisions.push(format!(
                "duplicate job {name:?} in namespace {:?}: the later definition wins",
                spec.namespace.as_deref().unwrap_or("default")
            ));
        }
        target.job.insert(name, spec);
    }
    for (name, spec) in source.build {
        if let Some(existing) = target.build.get(&name) {
            check_namespace_collision(
                "build",
                &name,
                existing.namespace.as_deref(),
                spec.namespace.as_deref(),
            )?;
            collisions.push(format!(
                "duplicate build {name:?} in namespace {:?}: the later definition wins",
                spec.namespace.as_deref().unwrap_or("default")
            ));
        }
        target.build.insert(name, spec);
    }
    target.namespace.extend(source.namespace);
    target.permission.extend(source.permission);
    Ok(collisions)
}

fn check_namespace_collision(
    kind: &str,
    name: &str,
    existing: Option<&str>,
    incoming: Option<&str>,
) -> Result<(), RelishError> {
    let existing = existing.unwrap_or("default");
    let incoming = incoming.unwrap_or("default");
    if existing != incoming {
        return Err(RelishError::FormatFailed(format!(
            "cannot compile {kind}.{name} from namespaces {existing:?} and {incoming:?} into one manifest: use distinct resource names or apply separate manifests"
        )));
    }
    Ok(())
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

    #[test]
    fn cross_namespace_workloads_cannot_be_silently_overwritten() {
        for kind in ["app", "job", "build"] {
            let dir = TempDir::new().unwrap();
            for (file, namespace) in [("a.toml", "prod"), ("b.toml", "staging")] {
                let fields = if kind == "build" {
                    "context = \".\"\ndestination = \"pickle://web:1\"\n"
                } else {
                    "image = \"image:1\"\n"
                };
                write_file(
                    dir.path(),
                    file,
                    &format!("[{kind}.web]\nnamespace = \"{namespace}\"\n{fields}"),
                );
            }
            let error = compile(dir.path())
                .expect_err("both namespaces cannot fit a bare-name map")
                .to_string();
            assert!(
                error.contains("web") && error.contains("prod") && error.contains("staging"),
                "{error}"
            );
        }
    }

    #[test]
    fn directory_namespaces_cannot_lose_same_named_workloads() {
        for kind in ["app", "job", "build"] {
            let dir = TempDir::new().unwrap();
            for namespace in ["prod", "staging"] {
                let subdir = dir.path().join(namespace);
                fs::create_dir(&subdir).unwrap();
                let fields = if kind == "build" {
                    "context = \".\"\ndestination = \"pickle://web:1\"\n"
                } else {
                    "image = \"image:1\"\n"
                };
                write_file(&subdir, "web.toml", &format!("[{kind}.web]\n{fields}"));
            }
            let error = compile(dir.path()).unwrap_err().to_string();
            assert!(
                error.contains("prod") && error.contains("staging"),
                "{error}"
            );
        }
    }

    #[test]
    fn malformed_defaults_refuse_the_entire_compile() {
        let dir = TempDir::new().unwrap();
        write_file(dir.path(), "_defaults.toml", "image = \"not closed\n");
        write_file(dir.path(), "a.toml", "[app.web]\nimage = \"x:1\"\n");
        let error = compile(dir.path()).unwrap_err().to_string();
        assert!(error.contains("_defaults.toml"), "{error}");
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
    fn malformed_workload_files_refuse_the_entire_compile() {
        let dir = TempDir::new().unwrap();
        write_file(dir.path(), "bad.toml", "this is not valid toml [[[");
        write_file(dir.path(), "good.toml", "[app.web]\nimage = \"web:1\"\n");
        let error = compile(dir.path())
            .expect_err("partial manifests must not be emitted")
            .to_string();
        assert!(error.contains("bad.toml"), "{error}");
    }

    #[test]
    fn invalid_files_in_nested_directories_refuse_the_entire_compile() {
        let dir = TempDir::new().unwrap();
        fs::create_dir(dir.path().join("prod")).unwrap();
        write_file(&dir.path().join("prod"), "bad.toml", "[app.web\n");
        write_file(dir.path(), "good.toml", "[app.api]\nimage = \"api:1\"\n");
        assert!(
            compile(dir.path())
                .unwrap_err()
                .to_string()
                .contains("bad.toml")
        );
    }

    #[cfg(unix)]
    #[test]
    fn unreadable_symlink_input_is_an_error() {
        let dir = TempDir::new().unwrap();
        std::os::unix::fs::symlink(dir.path().join("missing"), dir.path().join("bad.toml"))
            .unwrap();
        let error = compile(dir.path()).unwrap_err().to_string();
        assert!(error.contains("bad.toml"), "{error}");
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
