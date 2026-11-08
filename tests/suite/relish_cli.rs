//! Black-box contracts for the compiled `relish` executable.
//!
//! Parser unit tests are useful, but they cannot catch broken exit codes,
//! stdout/stderr routing, or a binary that was wired to the wrong handler.

use std::process::{Command, Output};

fn run(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_relish"))
        .args(args)
        .env_remove("RELIABURGER_TOKEN")
        .env_remove("RELIABURGER_CA_CERT")
        .output()
        .unwrap()
}

#[test]
fn version_reports_the_compiled_binary() {
    let output = run(&["--version"]);
    assert!(output.status.success());
    let version = env!("CARGO_PKG_VERSION");
    let expected = match reliaburger::upgrade::version::build_commit() {
        Some(commit) => format!("relish {version} ({})\n", &commit[..7]),
        None => format!("relish {version}\n"),
    };
    assert_eq!(String::from_utf8_lossy(&output.stdout), expected);
    assert!(output.stderr.is_empty());
}

/// Run `relish` the way an agent's shell does: no terminal on stdin or
/// stdout. Gives up after 20 s, so a reader that hangs fails the test
/// instead of the run.
fn run_without_a_terminal(args: &[&str]) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_relish"))
        .args(args)
        .env_remove("RELIABURGER_TOKEN")
        .env_remove("RELIABURGER_CA_CERT")
        .env_remove("RELIABURGER_ENDPOINT")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
    while child.try_wait().unwrap().is_none() {
        if std::time::Instant::now() > deadline {
            child.kill().unwrap();
            panic!("relish {args:?} hung without a terminal");
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    child.wait_with_output().unwrap()
}

/// #1569: the TUI and the two readers are full-screen. From an agent's shell
/// they used to error obscurely or hang; now they stop at once and say what
/// to run instead.
#[test]
fn full_screen_commands_fail_fast_without_a_terminal() {
    for (args, hint) in [
        (&[][..], "relish agent guide"),
        (&["tui"], "relish agent guide"),
        (&["manual"], "relish agent manual"),
        (&["manual", "tour"], "relish agent manual"),
        (&["source"], "relish agent source"),
        (&["source", "ebpf"], "relish agent source"),
    ] {
        let output = run_without_a_terminal(args);
        assert_eq!(output.status.code(), Some(1), "relish {args:?}");
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("needs a terminal"),
            "relish {args:?}: {stderr}"
        );
        assert!(stderr.contains(hint), "relish {args:?}: {stderr}");
        assert!(output.stdout.is_empty(), "relish {args:?}");
    }
}

#[test]
fn invalid_output_format_exits_with_clap_error() {
    let output = run(&["--output", "xml", "status"]);
    assert_eq!(output.status.code(), Some(2));
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("invalid value 'xml'"));
}

#[test]
fn apply_dry_run_succeeds_without_an_agent() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("app.toml");
    std::fs::write(
        &config,
        r#"
[app.web]
script = "exec true"
"#,
    )
    .unwrap();

    let output = Command::new(env!("CARGO_BIN_EXE_relish"))
        .arg("apply")
        .arg(&config)
        .arg("--dry-run")
        .env_remove("RELIABURGER_TOKEN")
        .env_remove("RELIABURGER_CA_CERT")
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("web"), "{stdout}");
    assert!(stdout.contains("dry run"), "{stdout}");
    assert!(output.stderr.is_empty());
}

#[test]
fn missing_apply_file_exits_nonzero_and_uses_stderr() {
    let output = run(&["apply", "/definitely/missing/reliaburger.toml", "--dry-run"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("error:"));
}

#[test]
fn endpoint_environment_rejects_remote_plaintext_before_dispatch() {
    let output = Command::new(env!("CARGO_BIN_EXE_relish"))
        .arg("status")
        .env("RELIABURGER_ENDPOINT", "http://192.0.2.10:9117")
        .env_remove("RELIABURGER_TOKEN")
        .env_remove("RELIABURGER_CA_CERT")
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("must use HTTPS"));
}

/// #1695: the MCPB bundle passes its optional endpoint setting as
/// `RELIABURGER_ENDPOINT`, empty when the user left it blank. Blank means
/// "use the current context", not "an invalid URL".
#[test]
fn blank_endpoint_environment_falls_back_to_the_current_context() {
    let home = tempfile::tempdir().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_relish"))
        .arg("status")
        .env("RELIABURGER_HOME", home.path())
        .env("RELIABURGER_ENDPOINT", "  ")
        .env("RELIABURGER_TOKEN", "")
        .env_remove("RELIABURGER_CA_CERT")
        .output()
        .unwrap();
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!stderr.contains("invalid endpoint URL"), "{stderr}");
    assert!(!stderr.contains("endpoint URL must"), "{stderr}");
}

/// #1695: a blank `RELIABURGER_TOKEN` sends no bearer at all, the same as
/// an unset one, rather than an empty `Bearer ` header.
#[tokio::test]
async fn blank_token_environment_sends_no_bearer() {
    use axum::{Router, http::HeaderMap};
    use std::sync::{Arc, Mutex};

    let seen: Arc<Mutex<Vec<Option<String>>>> = Arc::default();
    let record = seen.clone();
    let app = Router::new().fallback(move |headers: HeaderMap| {
        let record = record.clone();
        async move {
            record.lock().unwrap().push(
                headers
                    .get(axum::http::header::AUTHORIZATION)
                    .map(|value| value.to_str().unwrap_or("<binary>").to_string()),
            );
            axum::http::StatusCode::SERVICE_UNAVAILABLE
        }
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    // Pay `relish`'s first launch before the 60 s bound starts (#1149).
    crate::bun_process::first_launch_binaries();
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(60),
        tokio::process::Command::new(env!("CARGO_BIN_EXE_relish"))
            .arg("status")
            .env("RELIABURGER_ENDPOINT", &endpoint)
            .env("RELIABURGER_TOKEN", " ")
            .env_remove("RELIABURGER_CA_CERT")
            .kill_on_drop(true)
            .output(),
    )
    .await
    .unwrap()
    .unwrap();
    server.abort();
    let _ = server.await;

    let seen = seen.lock().unwrap();
    assert!(
        !seen.is_empty(),
        "relish never reached the stub: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(seen.iter().all(Option::is_none), "{seen:?}");
}

#[test]
fn offline_log_export_refuses_an_unreadable_checkpoint_before_upload() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("source");
    let dest = dir.path().join("dest");
    std::fs::create_dir(&source).unwrap();
    std::fs::write(source.join("logs.parquet"), b"exported bytes").unwrap();
    std::fs::create_dir(source.join("_export_checkpoint.json")).unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_relish"))
        .args(["logs-export", "--source"])
        .arg(&source)
        .arg("--dest")
        .arg(&dest)
        .env("RELIABURGER_HOME", dir.path().join("home"))
        .env_remove("RELIABURGER_ENDPOINT")
        .env_remove("RELIABURGER_TOKEN")
        .env_remove("RELIABURGER_CA_CERT")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1));
    assert_eq!(
        std::fs::read(source.join("logs.parquet")).unwrap(),
        b"exported bytes"
    );
    assert!(
        !dest.exists(),
        "invalid checkpoint must be refused before upload"
    );
    assert!(String::from_utf8_lossy(&output.stderr).contains("checkpoint"));
    assert!(
        output.stdout.is_empty(),
        "must not report a complete successful export"
    );
}

#[test]
fn offline_log_export_persists_progress_between_runs() {
    let dir = tempfile::tempdir().unwrap();
    let source = dir.path().join("source");
    std::fs::create_dir(&source).unwrap();
    std::fs::write(source.join("logs.parquet"), b"exported bytes").unwrap();
    for expected in ["exported 1 file(s)", "no new files to export"] {
        let output = Command::new(env!("CARGO_BIN_EXE_relish"))
            .args(["logs-export", "--source"])
            .arg(&source)
            .arg("--dest")
            .arg(dir.path().join("dest"))
            .env("RELIABURGER_HOME", dir.path().join("home"))
            .output()
            .unwrap();
        assert!(output.status.success(), "{:?}", output);
        assert!(String::from_utf8_lossy(&output.stdout).contains(expected));
    }
}

#[cfg(unix)]
#[test]
fn offline_log_export_rejects_a_non_utf8_destination() {
    use std::os::unix::ffi::OsStringExt;
    let dir = tempfile::tempdir().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_relish"))
        .args(["logs-export", "--source"])
        .arg(dir.path())
        .arg("--dest")
        .arg(std::ffi::OsString::from_vec(vec![0xff]))
        .env("RELIABURGER_HOME", dir.path().join("home"))
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).contains("UTF-8"));
}

#[test]
fn offline_log_export_rejects_a_missing_source() {
    let dir = tempfile::tempdir().unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_relish"))
        .args(["logs-export", "--source"])
        .arg(dir.path().join("missing"))
        .arg("--dest")
        .arg(dir.path().join("dest"))
        .env("RELIABURGER_HOME", dir.path().join("home"))
        .output()
        .unwrap();
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
}

#[tokio::test]
async fn cancel_deploy_waits_for_terminal_evidence_and_refuses_unknown_success() {
    use axum::{
        Json, Router,
        routing::{get, post},
    };
    for outcome in ["cancelled", "unknown"] {
        let accepted = serde_json::json!({
            "id": "deploy-test", "phase": "deploying_apps", "outcome": null,
            "started_at": 1, "phase_changed_at": 1, "finished_at": null,
            "cancellation_requested_at": 2, "targets": [], "current_target": null,
            "message": "cancellation requested",
        });
        let mut finished = accepted.clone();
        finished["phase"] = "finished".into();
        finished["outcome"] = outcome.into();
        finished["finished_at"] = 3.into();
        finished["message"] = "worker finished".into();
        let app = Router::new()
            .route(
                "/v1/deploys/operations/deploy-test/cancel",
                post(move || async move { (axum::http::StatusCode::ACCEPTED, Json(accepted)) }),
            )
            .route(
                "/v1/deploys/operations",
                get(move || async move {
                    Json(serde_json::json!({"active_deploys": [], "history": [finished]}))
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        // Pay `relish`'s first launch before the 60 s bound starts (#1149).
        crate::bun_process::first_launch_binaries();
        let output = tokio::time::timeout(
            std::time::Duration::from_secs(60),
            tokio::process::Command::new(env!("CARGO_BIN_EXE_relish"))
                .args(["--output", "json", "cancel-deploy", "deploy-test"])
                .env("RELIABURGER_ENDPOINT", endpoint)
                .env_remove("RELIABURGER_TOKEN")
                .env_remove("RELIABURGER_CA_CERT")
                .kill_on_drop(true)
                .output(),
        )
        .await
        .unwrap()
        .unwrap();
        server.abort();
        let _ = server.await;
        assert_eq!(
            output.status.success(),
            outcome == "cancelled",
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let record: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(record["outcome"], outcome);
        assert_eq!(record["phase"], "finished");
    }
}

/// #697: `relish image delete repo:tag` finds the registry through the
/// agent's capabilities and sends the OCI tag deletion there.
#[tokio::test]
async fn image_delete_parses_repo_and_tag() {
    use axum::{
        Json, Router,
        extract::{OriginalUri, State},
        routing::{delete, get},
    };
    use std::sync::{Arc, Mutex};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let mut capabilities = reliaburger::bun::capabilities::ClusterCapabilities::default();
    capabilities.service_endpoints.registry = Some(endpoint.clone());
    let deleted: Arc<Mutex<Vec<String>>> = Arc::default();
    let app =
        Router::new()
            .route(
                "/v1/capabilities",
                get(move || async move { Json(capabilities) }),
            )
            .route(
                "/v2/{*rest}",
                delete(
                    |State(deleted): State<Arc<Mutex<Vec<String>>>>,
                     OriginalUri(uri): OriginalUri| async move {
                        deleted.lock().unwrap().push(uri.path().to_string());
                        axum::http::StatusCode::ACCEPTED
                    },
                ),
            )
            .with_state(deleted.clone());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    // Pay `relish`'s first launch before the 60 s bound starts (#1149).
    crate::bun_process::first_launch_binaries();
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(60),
        tokio::process::Command::new(env!("CARGO_BIN_EXE_relish"))
            .args(["image", "delete", "localhost:5050/team/web:v1"])
            .env("RELIABURGER_ENDPOINT", &endpoint)
            .env_remove("RELIABURGER_TOKEN")
            .env_remove("RELIABURGER_CA_CERT")
            .kill_on_drop(true)
            .output(),
    )
    .await
    .unwrap()
    .unwrap();
    server.abort();
    let _ = server.await;

    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        *deleted.lock().unwrap(),
        vec!["/v2/team/web/manifests/v1".to_string()]
    );
    // A bare name is refused before anything is sent.
    let refused = run(&[
        "--endpoint",
        "http://127.0.0.1:1",
        "images",
        "delete",
        "web",
    ]);
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("repository:tag"));
}

#[test]
fn job_rerun_rejects_non_job_manifests_before_contacting_the_agent() {
    let directory = tempfile::tempdir().unwrap();
    let manifest = directory.path().join("apps.toml");
    std::fs::write(&manifest, "[app.web]\nimage = 'test:v1'\n").unwrap();
    let output = run(&[
        "--endpoint",
        "http://127.0.0.1:1",
        "apply",
        manifest.to_str().unwrap(),
        "--rerun-jobs",
    ]);
    assert!(!output.status.success());
    assert!(String::from_utf8_lossy(&output.stderr).contains("only non-scheduled jobs"));
}

/// Z1.4: `relish apply -f app.yaml` imports Kubernetes YAML in memory,
/// prints the migration report to stderr and applies the converted
/// config to the agent.
#[cfg(feature = "kubernetes")]
#[tokio::test]
async fn apply_file_flag_applies_kubernetes_yaml_to_the_agent() {
    use axum::{
        Router,
        routing::{get, post},
    };
    let applied = std::sync::Arc::new(tokio::sync::Mutex::new(None::<String>));
    let captured = applied.clone();
    let complete = serde_json::json!({
        "type": "Complete", "created": 2, "instances": ["default__web-0", "default__web-1"],
    });
    let app = Router::new()
        .route(
            "/v1/health",
            get(|| async { axum::Json(serde_json::json!({"status": "ok"})) }),
        )
        .route(
            "/v1/apply",
            post(move |body: String| {
                let captured = captured.clone();
                let complete = complete.clone();
                async move {
                    *captured.lock().await = Some(body);
                    (
                        [("content-type", "text/event-stream")],
                        format!("data: {complete}\n\n"),
                    )
                }
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

    let directory = tempfile::tempdir().unwrap();
    let manifest = directory.path().join("web.yaml");
    std::fs::write(
        &manifest,
        r#"apiVersion: apps/v1
kind: Deployment
metadata:
  name: web
spec:
  replicas: 2
  template:
    spec:
      containers:
      - name: web
        image: nginx:1
        args: ["-g", "daemon off;"]
        ports:
        - containerPort: 80
---
apiVersion: v1
kind: Service
metadata:
  name: web
spec:
  ports:
  - port: 8080
    targetPort: 80
"#,
    )
    .unwrap();

    // Pay `relish`'s first launch before the 60 s bound starts (#1149).
    crate::bun_process::first_launch_binaries();
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(60),
        tokio::process::Command::new(env!("CARGO_BIN_EXE_relish"))
            .args(["apply", "-f", manifest.to_str().unwrap()])
            .env("RELIABURGER_ENDPOINT", &endpoint)
            .env_remove("RELIABURGER_TOKEN")
            .env_remove("RELIABURGER_CA_CERT")
            .kill_on_drop(true)
            .output(),
    )
    .await
    .unwrap()
    .unwrap();
    server.abort();
    let _ = server.await;

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "stderr: {stderr}");
    assert!(String::from_utf8_lossy(&output.stdout).contains("deployed 2 instance(s)"));
    assert!(stderr.contains("Deployment/web → [app.web]"), "{stderr}");
    assert!(
        stderr.contains("port 8080 forwards to container port 80"),
        "the migration report must reach the operator: {stderr}"
    );

    let body = applied
        .lock()
        .await
        .clone()
        .expect("agent received the apply");
    let config = reliaburger::config::Config::parse(&body).unwrap();
    let web = &config.app["web"];
    assert_eq!(web.image.as_deref(), Some("nginx:1"));
    assert_eq!(web.args, ["-g", "daemon off;"]);
    assert_eq!(web.port, Some(80));
    assert_eq!(web.replicas, reliaburger::config::Replicas::Fixed(2));
}

/// #703: `--strict` is sold as a CI gate, so a dropped resource must fail it
/// as surely as an approximated one.
#[cfg(feature = "kubernetes")]
#[test]
fn import_strict_fails_when_a_resource_is_dropped() {
    let dir = tempfile::tempdir().unwrap();
    let manifest = dir.path().join("dropped.yaml");
    std::fs::write(
        &manifest,
        r#"
apiVersion: networking.k8s.io/v1
kind: NetworkPolicy
metadata:
  name: deny-all
spec:
  podSelector: {}
---
apiVersion: v1
kind: PersistentVolumeClaim
metadata:
  name: data
spec:
  accessModes: ["ReadWriteOnce"]
"#,
    )
    .unwrap();
    let path = manifest.to_str().unwrap();

    let lenient = run(&["import", "-f", path]);
    assert!(
        lenient.status.success(),
        "{}",
        String::from_utf8_lossy(&lenient.stderr)
    );

    let strict = run(&["import", "--strict", "-f", path]);
    assert_eq!(strict.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&strict.stderr);
    assert!(stderr.contains("NetworkPolicy/deny-all"), "{stderr}");
    assert!(stderr.contains("2 dropped"), "{stderr}");
}

#[cfg(feature = "kubernetes")]
#[test]
fn import_strict_passes_a_manifest_that_converts_cleanly() {
    let dir = tempfile::tempdir().unwrap();
    let manifest = dir.path().join("clean.yaml");
    std::fs::write(
        &manifest,
        r#"
apiVersion: apps/v1
kind: Deployment
metadata:
  name: web
spec:
  template:
    metadata:
      labels:
        app: web
    spec:
      containers:
      - name: web
        image: web:v1
        ports:
        - containerPort: 8080
---
apiVersion: v1
kind: Service
metadata:
  name: web
spec:
  selector:
    app: web
  ports:
  - port: 8080
"#,
    )
    .unwrap();
    let output = run(&["import", "--strict", "-f", manifest.to_str().unwrap()]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("[app.web]"));
}

/// #1132: `export --strict` is the same CI gate as `import --strict`: a job
/// whose egress rules Kubernetes can't carry must fail it, and the error says
/// how many lines of each kind the report has.
#[cfg(feature = "kubernetes")]
#[test]
fn export_strict_fails_when_a_field_is_not_exported() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("job.toml");
    std::fs::write(
        &config,
        r#"
[job.fetch]
image = "fetcher:v1"

[job.fetch.egress]
allow = ["api.example.com:443"]
"#,
    )
    .unwrap();
    let path = config.to_str().unwrap();

    let lenient = run(&["export", "-f", path]);
    assert!(
        lenient.status.success(),
        "{}",
        String::from_utf8_lossy(&lenient.stderr)
    );
    assert!(String::from_utf8_lossy(&lenient.stderr).contains("[job.fetch.egress]"));

    let strict = run(&["export", "--strict", "-f", path]);
    assert_eq!(strict.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&strict.stderr);
    assert!(stderr.contains("[job.fetch.egress]"), "{stderr}");
    assert!(stderr.contains("1 unsupported"), "{stderr}");
}

#[cfg(feature = "kubernetes")]
#[test]
fn export_strict_passes_a_config_that_exports_cleanly() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("cron.toml");
    std::fs::write(
        &config,
        "[job.cleanup]\nimage = \"cleanup:v1\"\nschedule = \"0 3 * * *\"\noverlap = \"allow\"\n",
    )
    .unwrap();
    let output = run(&["export", "--strict", "-f", config.to_str().unwrap()]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("concurrencyPolicy: Allow"));
}

/// #703: exporting used to keep `command` only, so Redis came back without
/// `--maxmemory 64mb --save "" --appendonly no`.
#[cfg(feature = "kubernetes")]
#[test]
fn podinfo_import_then_export_round_trips_args() {
    let podinfo = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/examples/kubernetes/podinfo.yaml"
    );
    let imported = run(&["import", "-f", podinfo]);
    assert!(
        imported.status.success(),
        "{}",
        String::from_utf8_lossy(&imported.stderr)
    );
    let dir = tempfile::tempdir().unwrap();
    let toml = dir.path().join("podinfo.toml");
    std::fs::write(&toml, &imported.stdout).unwrap();

    let exported = run(&["export", "-f", toml.to_str().unwrap()]);
    assert!(
        exported.status.success(),
        "{}",
        String::from_utf8_lossy(&exported.stderr)
    );
    let yaml = String::from_utf8_lossy(&exported.stdout);
    let redis = yaml
        .split("\n---\n")
        .filter_map(|document| yaml_serde::from_str::<yaml_serde::Value>(document).ok())
        .find(|document| {
            document["kind"].as_str() == Some("Deployment")
                && document["metadata"]["name"].as_str() == Some("redis")
        })
        .unwrap_or_else(|| panic!("no redis Deployment in:\n{yaml}"));
    let args: Vec<&str> = redis["spec"]["template"]["spec"]["containers"][0]["args"]
        .as_sequence()
        .unwrap_or_else(|| panic!("redis lost its args:\n{yaml}"))
        .iter()
        .filter_map(|arg| arg.as_str())
        .collect();
    assert_eq!(
        args,
        [
            "redis-server",
            "--maxmemory",
            "64mb",
            "--maxmemory-policy",
            "allkeys-lru",
            "--save",
            "",
            "--appendonly",
            "no",
        ]
    );
}

#[test]
fn apply_refuses_plain_http_manifest_urls() {
    let output = run(&["apply", "-f", "http://example.com/app.yaml", "--dry-run"]);
    assert_eq!(output.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("only https:// URLs are accepted"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn directory_compile_refuses_partial_output_when_an_input_is_invalid() {
    for invalid_file in ["bad.toml", "_defaults.toml"] {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("good.toml"),
            "[app.web]\nimage = \"web:1\"\n",
        )
        .unwrap();
        std::fs::write(dir.path().join(invalid_file), "this is not valid TOML [[[").unwrap();
        let output = run(&["compile", dir.path().to_str().unwrap()]);
        assert_eq!(output.status.code(), Some(1), "{output:?}");
        assert!(
            output.stdout.is_empty(),
            "partial manifest escaped on stdout"
        );
        assert!(String::from_utf8_lossy(&output.stderr).contains(invalid_file));
    }
}

/// Submit `jobs` (TOML) with the compiled `relish batch` against `endpoint`
/// and return the admitted batch id, checking the key went to stderr.
async fn relish_batch(endpoint: &str, jobs: &str) -> u64 {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("jobs.toml");
    std::fs::write(&path, jobs).unwrap();
    let endpoint = endpoint.to_string();
    let output = tokio::task::spawn_blocking(move || {
        Command::new(env!("CARGO_BIN_EXE_relish"))
            .arg("batch")
            .arg(&path)
            .env("RELIABURGER_ENDPOINT", endpoint)
            .env_remove("RELIABURGER_TOKEN")
            .env_remove("RELIABURGER_CA_CERT")
            .output()
            .unwrap()
    })
    .await
    .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "stdout: {stdout}\nstderr: {stderr}"
    );
    assert!(stderr.contains("--idempotency-key"), "{stderr}");
    stdout
        .split_whitespace()
        .nth(1)
        .and_then(|word| word.parse().ok())
        .unwrap_or_else(|| panic!("no batch id in {stdout:?}"))
}

/// A TOML job's `task_timeout_secs` reaches the cluster: `relish batch`
/// submits jobs that would sleep for a minute, and the cluster stops each
/// after one second instead of letting it run for ever.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn hung_toml_job_times_out() {
    let harness = crate::batch::Harness::start().await;
    let hang = "runtime = \"process\"\nexec = \"/bin/sleep\"\ncommand = [\"60\"]\ntask_timeout_secs = 1\nmax_attempts = 1\n";
    // The elapsed-time check below times the jobs, not `relish`'s first
    // launch, so pay that before the clock starts (#1149).
    crate::bun_process::first_launch_binaries();
    let started = std::time::Instant::now();

    // A job that is safe to repeat counts the timeout as a failed attempt.
    let id = relish_batch(
        &harness.base_url,
        &format!("[job.hang]\n{hang}replay_unknown = true\n"),
    )
    .await;
    let summary = harness.wait_done(id, 30).await;
    assert_eq!(summary["failed"], 1, "{summary}");
    assert_eq!(summary["succeeded"], 0, "{summary}");

    // By default a timed-out attempt is an unknown outcome: the command is
    // stopped, and the run waits for `relish batch replay` instead of retrying.
    let id = relish_batch(&harness.base_url, &format!("[job.hang-once]\n{hang}")).await;
    let http = reqwest::Client::new();
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        let summary: serde_json::Value = http
            .get(format!("{}/v1/batch/{id}", harness.base_url))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let cohort = &summary["cohorts"][0];
        if cohort["unknown_owners"]
            .as_array()
            .is_some_and(|owners| !owners.is_empty())
            && cohort["active_commands"] == 0
        {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "the timeout never stopped the command: {summary}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    assert!(started.elapsed() < std::time::Duration::from_secs(30));
}

/// Run `relish agent ...` the way an agent's tool runner does: no terminal,
/// nothing on stdin, no connection settings in the environment.
fn run_agent(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_relish"))
        .args(args)
        .stdin(std::process::Stdio::null())
        .env_remove("RELIABURGER_TOKEN")
        .env_remove("RELIABURGER_CA_CERT")
        .env_remove("RELIABURGER_ENDPOINT")
        .env_remove("TERM")
        .output()
        .unwrap()
}

/// stdout as the one JSON document an agent command prints.
fn agent_envelope(output: &Output) -> serde_json::Value {
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(stdout.matches('\n').count(), 1, "one line: {stdout}");
    let value: serde_json::Value = serde_json::from_str(&stdout).expect("stdout is JSON");
    assert_eq!(value["schema"], "relish.agent/v1");
    value
}

#[test]
fn agent_version_answers_without_a_terminal_or_stdin() {
    let output = run_agent(&["agent", "version"]);
    assert_eq!(output.status.code(), Some(0));
    assert!(output.stderr.is_empty(), "{output:?}");
    let value = agent_envelope(&output);
    assert_eq!(value["ok"], true);
    assert_eq!(value["command"], "version");
    assert_eq!(value["data"]["version"], env!("CARGO_PKG_VERSION"));
}

#[test]
fn agent_ignores_the_output_flag() {
    let output = run_agent(&["-o", "yaml", "agent", "version"]);
    assert_eq!(output.status.code(), Some(0));
    assert_eq!(agent_envelope(&output)["ok"], true);
}

#[test]
fn agent_usage_errors_are_envelopes_on_stdout_with_exit_2() {
    for args in [
        &["agent", "version", "--bogus"][..],
        &["agent", "nonsense"],
        &["agent"],
        &["agent", "version", "--max-bytes", "10"],
        &["--endpoint", "http://192.0.2.10:9117", "agent", "version"],
    ] {
        let output = run_agent(args);
        assert_eq!(output.status.code(), Some(2), "{args:?}");
        assert!(output.stderr.is_empty(), "{args:?}: {output:?}");
        let value = agent_envelope(&output);
        assert_eq!(value["ok"], false, "{args:?}");
        assert_eq!(value["error"]["code"], "usage", "{args:?}");
    }
}

#[test]
fn agent_refuses_a_plaintext_remote_endpoint_from_the_environment_with_an_envelope() {
    let output = Command::new(env!("CARGO_BIN_EXE_relish"))
        .args(["agent", "version"])
        .stdin(std::process::Stdio::null())
        .env("RELIABURGER_ENDPOINT", "http://192.0.2.10:9117")
        .env_remove("RELIABURGER_TOKEN")
        .env_remove("RELIABURGER_CA_CERT")
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    let value = agent_envelope(&output);
    assert_eq!(value["error"]["code"], "usage");
    assert!(
        value["error"]["message"]
            .as_str()
            .unwrap()
            .contains("must use HTTPS"),
        "{value}"
    );
}

#[test]
fn agent_help_is_ordinary_help() {
    let output = run_agent(&["agent", "--help"]);
    assert_eq!(output.status.code(), Some(0));
    assert!(String::from_utf8_lossy(&output.stdout).contains("--max-bytes"));
}

/// #1574: the real binary makes a workspace from its own source, and `diff`
/// sees an edit to it. Building and testing are covered with fake cargos in
/// the unit tests; this proves the wiring and the real bundle.
#[test]
fn agent_patch_init_then_diff_shows_the_edit() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("fix");
    let dir_text = dir.to_str().unwrap();
    let output = run_agent(&["agent", "patch", "init", dir_text]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let init = agent_envelope(&output);
    assert_eq!(init["command"], "patch init");
    assert_eq!(init["data"]["branch"], "patch");
    assert!(dir.join("src/relish/agent/patch.rs").is_file());

    let path = dir.join("src/relish/agent/patch.rs");
    let mut text = std::fs::read_to_string(&path).unwrap();
    text.push_str("// a fix\n");
    std::fs::write(&path, text).unwrap();
    let output = run_agent(&["agent", "patch", "diff", dir_text]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let diff = agent_envelope(&output);
    assert_eq!(diff["command"], "patch diff");
    assert_eq!(
        diff["data"]["files"],
        serde_json::json!([{
            "path": "src/relish/agent/patch.rs",
            "status": "modified",
            "insertions": 1,
            "deletions": 0
        }])
    );
    assert!(diff["data"]["diff"].as_str().unwrap().contains("+// a fix"));
    assert_eq!(diff["data"]["build_changes"], serde_json::json!([]));
}

/// #1691: an agent with only `relish` hands `patch apply` a diff whose hunk
/// counts are wrong, inline as one argument starting `---`; it applies,
/// and `patch diff` prints it with headers git wrote. A hunk that doesn't
/// match is a usage error that names it.
#[test]
fn agent_patch_apply_takes_a_miscounted_diff_and_diff_prints_a_correct_one() {
    let root = tempfile::tempdir().unwrap();
    let dir = root.path().join("fix");
    let dir_text = dir.to_str().unwrap();
    let output = run_agent(&["agent", "patch", "init", dir_text]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");

    let source = std::fs::read_to_string(dir.join("src/relish/agent/patch.rs")).unwrap();
    let first = source.lines().next().unwrap();
    let miscounted = format!(
        "--- a/src/relish/agent/patch.rs\n+++ b/src/relish/agent/patch.rs\n@@ -1,5 +1,9 @@\n-{first}\n+//! Patched by an agent.\n"
    );
    let output = run_agent(&["agent", "patch", "apply", dir_text, &miscounted]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    let applied = agent_envelope(&output);
    assert_eq!(applied["command"], "patch apply");
    assert_eq!(applied["data"]["files"][0]["hunks"][0]["recounted"], true);

    let output = run_agent(&["agent", "patch", "diff", dir_text]);
    let diff = agent_envelope(&output);
    let text = diff["data"]["diff"].as_str().unwrap();
    assert!(text.contains("@@ -1,4 +1,4 @@"), "{text}");
    assert!(text.contains("+//! Patched by an agent."), "{text}");

    let wrong = "--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1 +1 @@\n-no such line\n+x\n";
    let output = run_agent(&["agent", "patch", "apply", dir_text, wrong]);
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    let refused = agent_envelope(&output);
    assert_eq!(refused["error"]["code"], "usage");
    assert!(
        refused["error"]["message"]
            .as_str()
            .unwrap()
            .contains("hunk 1 of src/lib.rs"),
        "{refused}"
    );
}

#[test]
fn agent_patch_on_a_directory_that_is_not_a_workspace_is_not_found() {
    let root = tempfile::tempdir().unwrap();
    let output = run_agent(&["agent", "patch", "diff", root.path().to_str().unwrap()]);
    assert_eq!(output.status.code(), Some(5));
    let value = agent_envelope(&output);
    assert_eq!(value["error"]["code"], "not_found");
}

#[test]
fn agent_schema_stays_within_a_small_budget() {
    let output = run_agent(&["agent", "schema", "--max-bytes", "1024"]);
    assert!(output.stdout.len() <= 1024, "{} bytes", output.stdout.len());
    let value = agent_envelope(&output);
    // The whole schema is far over 1 KiB and mostly objects, which the
    // budget never trims, so this either trims the command list or says the
    // budget is too small. Both are answers inside the budget.
    if value["ok"] == true {
        assert_eq!(value["truncated"], true);
    } else {
        assert_eq!(output.status.code(), Some(2));
        assert_eq!(value["error"]["code"], "usage");
    }
}

#[test]
fn agent_report_writes_a_redacted_draft_and_posts_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let evidence = dir.path().join("diagnose.json");
    std::fs::write(
        &evidence,
        r#"{"node_id": "wolf4", "detail": "wolf4 at 192.0.2.17 sent rbrg_0123abcd"}"#,
    )
    .unwrap();
    let out = dir.path().join("report");
    let output = run_agent(&[
        "agent",
        "report",
        "--title",
        "wolf4 is reported dead",
        "--what-happened",
        "diagnose said wolf4 was dead",
        "--expected",
        "no finding",
        "--not-tested",
        "more than two nodes",
        "--caught-by",
        "smoker-fault",
        "--finding",
        "node-dead",
        "--evidence",
        evidence.to_str().unwrap(),
        "--out",
        out.to_str().unwrap(),
    ]);
    assert_eq!(output.status.code(), Some(0), "{output:?}");
    assert!(output.stderr.is_empty(), "{output:?}");
    let value = agent_envelope(&output);
    assert_eq!(value["command"], "report");
    assert_eq!(
        value["data"]["title"],
        "agent: [unverified] node-1 is reported dead"
    );
    let post = value["next"].as_array().unwrap().last().unwrap().clone();
    assert_eq!(post["read_only"], false);
    assert!(
        post["run"]
            .as_str()
            .unwrap()
            .starts_with("gh issue create ")
    );
    for name in ["issue.md", "bundle.json", "MANIFEST.json"] {
        let text = std::fs::read_to_string(out.join(name)).unwrap();
        for planted in ["wolf4", "192.0.2.17", "rbrg_0123abcd"] {
            assert!(!text.contains(planted), "{planted} in {name}");
        }
    }
}
