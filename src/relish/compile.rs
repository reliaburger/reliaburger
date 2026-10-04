/// Config compilation for Reliaburger.
///
/// Walks a directory of TOML files, discovers `_defaults.toml` files,
/// merges defaults into each app/job spec, and returns a single resolved
/// `Config`. Directory names become namespaces.
use std::path::{Path, PathBuf};

use crate::config::Config;
use crate::config::defaults::WorkloadDefaults;

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

/// Read the whole tree before resolving it. Any unreadable input refuses output.
fn compile_directory(dir: &Path) -> Result<CompileResult, RelishError> {
    let mut files = std::collections::BTreeMap::new();
    collect_tree(dir, dir, &mut files)?;
    let mut result = compile_sources(&files, DuplicatePolicy::Warn)
        .map_err(|error| RelishError::FormatFailed(error.to_string()))?;
    for path in &mut result.merged_from {
        *path = dir.join(&*path);
    }
    Ok(result)
}

fn collect_tree(
    root: &Path,
    directory: &Path,
    files: &mut std::collections::BTreeMap<PathBuf, String>,
) -> Result<(), RelishError> {
    let mut entries = std::fs::read_dir(directory)?
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<Result<Vec<_>, _>>()?;
    entries.sort();
    for path in entries {
        let metadata = std::fs::metadata(&path)
            .map_err(|error| RelishError::FormatFailed(format!("{}: {error}", path.display())))?;
        if metadata.is_dir() {
            collect_tree(root, &path, files)?;
        } else if metadata.is_file()
            && path
                .extension()
                .is_some_and(|extension| extension == "toml")
        {
            let content = std::fs::read_to_string(&path).map_err(|error| {
                RelishError::FormatFailed(format!("{}: {error}", path.display()))
            })?;
            let relative = path.strip_prefix(root).map_err(|error| {
                RelishError::FormatFailed(format!("{}: {error}", path.display()))
            })?;
            files.insert(relative.to_path_buf(), content);
        }
    }
    Ok(())
}

/// How duplicate definitions in one namespace are handled by the caller.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum DuplicatePolicy {
    /// The interactive compiler reports deterministic overrides.
    Warn,
    /// An unattended reconciler refuses ambiguous desired state.
    Refuse,
}

#[derive(Debug)]
pub(crate) struct TreeError {
    pub path: PathBuf,
    pub message: String,
}

impl std::fmt::Display for TreeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.path.display(), self.message)
    }
}

/// Resolve an immutable snapshot of paths relative to the tree's root.
/// Both filesystem compilation and verified Git commits use these rules.
pub(crate) fn compile_sources(
    files: &std::collections::BTreeMap<PathBuf, String>,
    duplicates: DuplicatePolicy,
) -> Result<CompileResult, TreeError> {
    use std::path::Component;
    for path in files.keys() {
        if path.as_os_str().is_empty()
            || path
                .components()
                .any(|part| !matches!(part, Component::Normal(_)))
        {
            return Err(TreeError {
                path: path.clone(),
                message: "expected a path relative to the configuration root".into(),
            });
        }
    }
    let mut result = CompileResult {
        config: Config::default(),
        merged_from: Vec::new(),
        warnings: Vec::new(),
    };
    resolve_directory(Path::new(""), files, None, duplicates, &mut result)?;
    Ok(result)
}

fn resolve_directory(
    directory: &Path,
    files: &std::collections::BTreeMap<PathBuf, String>,
    parent: Option<&WorkloadDefaults>,
    duplicates: DuplicatePolicy,
    result: &mut CompileResult,
) -> Result<(), TreeError> {
    let defaults_path = directory.join("_defaults.toml");
    let own = files
        .get(&defaults_path)
        .map(|raw| {
            toml::from_str::<WorkloadDefaults>(raw).map_err(|error| TreeError {
                path: defaults_path.clone(),
                message: error.to_string(),
            })
        })
        .transpose()?;
    let resolved = own
        .as_ref()
        .map(|own| own.inherit(parent))
        .or_else(|| parent.cloned());
    for (path, raw) in files
        .iter()
        .filter(|(path, _)| path.parent() == Some(directory))
    {
        if path
            .file_name()
            .is_some_and(|name| name == "_defaults.toml")
        {
            continue;
        }
        let mut config = Config::parse(raw).map_err(|error| TreeError {
            path: path.clone(),
            message: error.to_string(),
        })?;
        if let Some(defaults) = &resolved {
            defaults.apply(&mut config);
        }
        if let Some(namespace) = directory.file_name() {
            let namespace = namespace.to_str().ok_or_else(|| TreeError {
                path: path.clone(),
                message: "directory namespace must be UTF-8".into(),
            })?;
            apply_namespace(&mut config, namespace);
        }
        // Always refuse an identity collision across namespaces first.
        let duplicate = first_duplicate_resource(&result.config, &config);
        let mut next = result.config.clone();
        let warnings = merge_into(&mut next, config).map_err(|error| TreeError {
            path: path.clone(),
            message: error.to_string(),
        })?;
        if duplicates == DuplicatePolicy::Refuse
            && let Some(resource) = duplicate
        {
            return Err(TreeError {
                path: path.clone(),
                message: format!(
                    "duplicate resource {resource} already declared in an earlier file"
                ),
            });
        }
        result.config = next;
        result.warnings.extend(
            warnings
                .into_iter()
                .map(|warning| format!("{}: {warning}", path.display())),
        );
        result.merged_from.push(path.clone());
    }
    let mut children = std::collections::BTreeSet::new();
    for path in files.keys() {
        if let Ok(relative) = path.strip_prefix(directory) {
            let mut parts = relative.components();
            if let (Some(part), Some(_)) = (parts.next(), parts.next()) {
                children.insert(directory.join(part.as_os_str()));
            }
        }
    }
    for child in children {
        resolve_directory(&child, files, resolved.as_ref(), duplicates, result)?;
    }
    Ok(())
}

fn first_duplicate_resource(merged: &Config, incoming: &Config) -> Option<String> {
    for name in incoming.app.keys() {
        if merged.app.contains_key(name) {
            return Some(format!("app.{name}"));
        }
    }
    for name in incoming.job.keys() {
        if merged.job.contains_key(name) {
            return Some(format!("job.{name}"));
        }
    }
    for name in incoming.namespace.keys() {
        if merged.namespace.contains_key(name) {
            return Some(format!("namespace.{name}"));
        }
    }
    for name in incoming.permission.keys() {
        if merged.permission.contains_key(name) {
            return Some(format!("permission.{name}"));
        }
    }
    for name in incoming.build.keys() {
        if merged.build.contains_key(name) {
            return Some(format!("build.{name}"));
        }
    }
    None
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
    #[test]
    fn typed_defaults_preserve_resources_environment_and_partial_deploy_overrides() {
        let dir = TempDir::new().unwrap();
        write_file(
            dir.path(),
            "_defaults.toml",
            r#"
image = "web:1"
memory = "256Mi-512Mi"
cpu = "100m-500m"
[env]
MODE = "prod"
KEEP = "default"
[deploy]
strategy = "rolling"
max_unavailable = 0
auto_rollback = true
"#,
        );
        write_file(
            dir.path(),
            "web.toml",
            r#"
[app.web]
[app.web.env]
KEEP = "explicit"
[app.web.deploy]
auto_rollback = false
"#,
        );
        let result = compile(dir.path()).unwrap();
        let app = &result.config.app["web"];
        assert_eq!(
            app.memory.as_ref().map(|r| r.request),
            Some(256 * 1024 * 1024)
        );
        assert_eq!(app.cpu.as_ref().map(|r| r.request), Some(100));
        assert_eq!(
            app.env["MODE"],
            crate::config::EnvValue::Plain("prod".into())
        );
        assert_eq!(
            app.env["KEEP"],
            crate::config::EnvValue::Plain("explicit".into())
        );
        let deploy = app.deploy.as_ref().unwrap();
        assert_eq!(deploy.max_unavailable, Some(0));
        assert_eq!(deploy.auto_rollback, Some(false));
        assert_eq!(deploy.strategy.as_deref(), Some("rolling"));
        let encoded = toml::to_string(&result.config).unwrap();
        assert_eq!(
            crate::config::Config::parse(&encoded).unwrap(),
            result.config
        );
    }

    #[test]
    fn nested_defaults_merge_parent_tables_and_explicit_scalars_win() {
        let dir = TempDir::new().unwrap();
        fs::create_dir(dir.path().join("prod")).unwrap();
        write_file(
            dir.path(),
            "_defaults.toml",
            "image='parent:1'\nmemory='256Mi'\n[env]\nPARENT='yes'\nVALUE='parent'\n[deploy]\nmax_unavailable=0\nauto_rollback=true\n",
        );
        write_file(
            &dir.path().join("prod"),
            "_defaults.toml",
            "cpu='300m'\n[env]\nVALUE='child'\n[deploy]\nauto_rollback=false\n",
        );
        write_file(
            &dir.path().join("prod"),
            "web.toml",
            "[app.web]\nimage='explicit:1'\nmemory='128Mi'\n[app.web.deploy]\nmax_surge=0\n",
        );
        let result = compile(dir.path()).unwrap();
        let app = &result.config.app["web"];
        assert_eq!(app.image.as_deref(), Some("explicit:1"));
        assert_eq!(
            app.memory.as_ref().map(|r| r.request),
            Some(128 * 1024 * 1024)
        );
        assert_eq!(app.cpu.as_ref().map(|r| r.request), Some(300));
        assert_eq!(
            app.env["PARENT"],
            crate::config::EnvValue::Plain("yes".into())
        );
        assert_eq!(
            app.env["VALUE"],
            crate::config::EnvValue::Plain("child".into())
        );
        let deploy = app.deploy.as_ref().unwrap();
        assert_eq!(deploy.max_surge, Some(0));
        assert_eq!(deploy.max_unavailable, Some(0));
        assert_eq!(deploy.auto_rollback, Some(false));
    }

    #[test]
    fn defaults_reject_unknown_keys_and_invalid_values_with_the_path() {
        for raw in [
            "memroy='256Mi'",
            "memory='nonsense'",
            "cpu='bad'",
            "[deploy]\nmax_unavailble=0",
        ] {
            let dir = TempDir::new().unwrap();
            write_file(dir.path(), "_defaults.toml", raw);
            write_file(dir.path(), "web.toml", "[app.web]\nimage='web:1'\n");
            let error = compile(dir.path())
                .expect_err("unsupported defaults must not disappear")
                .to_string();
            assert!(error.contains("_defaults.toml"), "{error}");
        }
    }
    #[test]
    fn common_defaults_preserve_explicit_host_execution_for_apps_and_jobs() {
        let dir = TempDir::new().unwrap();
        write_file(
            dir.path(),
            "_defaults.toml",
            "image='container:1'\nmemory='64Mi'\ncpu='50m'\n[env]\nMODE='prod'\n",
        );
        write_file(
            dir.path(),
            "native.toml",
            "[app.worker]\nexec='/usr/bin/true'\n[job.migrate]\nexec='/usr/bin/true'\n",
        );
        let result = compile(dir.path()).unwrap();
        let app = &result.config.app["worker"];
        let job = &result.config.job["migrate"];
        assert!(app.image.is_none());
        assert!(job.image.is_none());
        assert_eq!(
            app.memory.as_ref().map(|r| r.request),
            Some(64 * 1024 * 1024)
        );
        assert_eq!(
            job.memory.as_ref().map(|r| r.request),
            Some(64 * 1024 * 1024)
        );
        assert_eq!(job.cpu.as_ref().map(|r| r.request), Some(50));
        assert_eq!(
            job.env["MODE"],
            crate::config::EnvValue::Plain("prod".into())
        );
        result.config.validate().unwrap();
    }
}
