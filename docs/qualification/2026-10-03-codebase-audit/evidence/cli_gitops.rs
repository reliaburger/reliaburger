use std::collections::{BTreeMap, HashMap};
use std::process::Command;
use reliaburger::config::Config;
use reliaburger::lettuce::{git::GitRepo, sync::execute_sync, types::{GitOpsConfig, SyncResult}};
use reliaburger::relish::{compile::compile, plan::{generate_plan, CurrentResource, PlanAction}};

#[test]
fn same_named_apps_in_distinct_namespaces_are_silently_lost() {
    let dir = tempfile::tempdir().unwrap();
    for ns in ["prod", "staging"] {
        std::fs::create_dir(dir.path().join(ns)).unwrap();
        std::fs::write(dir.path().join(ns).join("web.toml"), format!("[app.web]\nimage = '{ns}:v1'\n")).unwrap();
    }
    let result = compile(dir.path()).unwrap();
    assert_eq!(result.merged_from.len(), 2);
    assert!(result.warnings.is_empty());
    assert_eq!(result.config.app.len(), 1);
    assert_eq!(result.config.app["web"].namespace.as_deref(), Some("staging"));
}

#[test]
fn shared_defaults_drop_env_memory_and_deploy_settings() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("_defaults.toml"), "image = 'web:v1'\nmemory = '256Mi'\n[env]\nMODE = 'prod'\n[deploy]\nmax_unavailable = 0\n").unwrap();
    std::fs::write(dir.path().join("web.toml"), "[app.web]\nreplicas = 2\n").unwrap();
    let result = compile(dir.path()).unwrap();
    assert!(result.warnings.is_empty());
    let app = &result.config.app["web"];
    assert_eq!(app.image.as_deref(), Some("web:v1"));
    assert!(app.memory.is_none());
    assert!(app.env.is_empty());
}

#[test]
fn dry_run_reports_replica_port_env_changes_unchanged() {
    let config = Config::parse("[app.web]\nimage = 'web:v1'\nreplicas = 9\nport = 9999\n[app.web.env]\nMODE = 'changed'\n").unwrap();
    let current = vec![CurrentResource {resource:"app.web".into(), image:Some("web:v1".into())}];
    let plan = generate_plan(&config, Some(&current));
    assert_eq!(plan.entries[0].action, PlanAction::Unchanged);
    assert_eq!(plan.to_update, 0);
}

fn git(path: &std::path::Path, args: &[&str]) {
    assert!(Command::new("git").args(args).current_dir(path)
        .env("GIT_AUTHOR_NAME", "audit").env("GIT_AUTHOR_EMAIL", "audit@example.invalid")
        .env("GIT_COMMITTER_NAME", "audit").env("GIT_COMMITTER_EMAIL", "audit@example.invalid")
        .output().unwrap().status.success());
}

fn sync_files(files: &[(&str, &str)]) -> reliaburger::lettuce::sync::SyncOutcome {
    let source = tempfile::tempdir().unwrap();
    git(source.path(), &["init", "-q", "-b", "main"]);
    for (path, content) in files {
        let dest = source.path().join(path);
        std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
        std::fs::write(dest, content).unwrap();
    }
    git(source.path(), &["add", "."]);
    git(source.path(), &["-c", "commit.gpgsign=false", "commit", "-q", "-m", "audit"]);
    let clone_parent = tempfile::tempdir().unwrap();
    let url = source.path().to_str().unwrap();
    let repo = GitRepo::clone_or_open(url, &clone_parent.path().join("bare"), "main").unwrap();
    let config = GitOpsConfig {repo:url.into(), branch:"main".into(), path:"/".into(), poll_interval_secs:30, require_signed_commits:false, trusted_signing_keys:vec![], webhook_secret:None, webhook_rate_limit:10};
    execute_sync(&repo, &config, &HashMap::new(), &BTreeMap::new(), &BTreeMap::new(), &[], None)
}

#[test]
fn gitops_successfully_ignores_jobs_including_migration_gates() {
    let result = sync_files(&[("apps.toml", "[app.web]\nimage = 'web:v1'\n[job.migrate]\nimage = 'migrate:v1'\nrun_before = ['app.web']\n")]);
    assert_eq!(result.result, SyncResult::Success);
    assert_eq!(result.changes.len(), 1);
    assert_eq!(result.diff_summary.unwrap().added, 1);
}

#[test]
fn gitops_cannot_read_the_documented_defaults_directory() {
    let result = sync_files(&[("_defaults.toml", "image = 'web:v1'\n"), ("web.toml", "[app.web]\nreplicas = 2\n")]);
    assert!(matches!(result.result, SyncResult::Failure {..}));
    assert!(result.file_errors["_defaults.toml"].contains("unknown field"));
}

#[test]
fn cli_validation_rejects_permissions_for_an_existing_cluster_namespace() {
    let config = Config::parse("[permission.reader]\nactions = ['logs']\nnamespaces = ['prod']\n").unwrap();
    assert!(config.validate().unwrap_err().to_string().contains("unknown namespace"));
    assert!(config.validate_against(&["prod".into()]).is_ok());
}

#[test]
fn directory_compile_returns_success_after_dropping_an_invalid_workload_file() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("good.toml"), "[app.good]\nimage = 'good:v1'\n").unwrap();
    std::fs::write(dir.path().join("broken.toml"), "[app.broken]\nimage = [\n").unwrap();
    let result = compile(dir.path()).unwrap();
    assert_eq!(result.config.app.len(), 1);
    assert_eq!(result.merged_from.len(), 1);
    assert_eq!(result.warnings.len(), 1);
}
