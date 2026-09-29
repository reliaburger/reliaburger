//! App metrics without a Prometheus install, end to end.
//!
//! A real Bun (process runtime) runs the test app, which serves Prometheus
//! text on `/metrics`. The app declares `metrics = {}`, so Bun's scrape loop
//! collects it, and the compiled `relish metrics` reads it back through the
//! API, labelled with the instance and node.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};

use crate::bun_process;
use bun_process::{
    WAIT, assert_success, reserve_address, run_relish, spawn_bun_with_port_retry, wait_for_relish,
    wait_for_relish_output, write_portable_node_config,
};

/// Send one request to the app and read its answer.
fn hit(port: u16) {
    let Ok(mut stream) = TcpStream::connect(("127.0.0.1", port)) else {
        return;
    };
    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
    let _ = stream.write_all(b"GET / HTTP/1.1\r\nHost: test\r\n\r\n");
    let mut buf = [0u8; 256];
    let _ = stream.read(&mut buf);
}

#[test]
fn relish_metrics_shows_samples_scraped_from_each_instance() {
    let root = tempfile::tempdir().unwrap();
    let (mut bun, address) = spawn_bun_with_port_retry(false, || {
        let config = write_portable_node_config(root.path());
        // Scrape every second so the test needn't wait ten.
        let mut text = std::fs::read_to_string(&config).unwrap();
        text.push_str("\n[metrics]\napp_scrape_interval_secs = 1\n");
        std::fs::write(&config, text).unwrap();
        (config, reserve_address(), root.path().join("bun.log"))
    });
    let endpoint = format!("http://{address}");
    wait_for_relish(&mut bun, &["--endpoint", &endpoint, "status"]);

    let port = reserve_address().port();
    let manifest = root.path().join("metrics-app.toml");
    std::fs::write(
        &manifest,
        format!(
            r#"
[app.web]
image = "proc-grill:image-ignored"
command = [{testapp:?}, "--port", "{port}"]
port = {port}
metrics = {{}}
"#,
            testapp = env!("CARGO_BIN_EXE_testapp"),
        ),
    )
    .unwrap();
    let apply = run_relish(&["--endpoint", &endpoint, "apply", manifest.to_str().unwrap()]);
    assert_success(&apply, "apply the metrics app");

    // Keep some traffic flowing while the scrapes land, so the counter moves.
    let deadline = Instant::now() + WAIT;
    let overview = loop {
        hit(port);
        let output = run_relish(&["--endpoint", &endpoint, "metrics", "web"]);
        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        if output.status.success()
            && stdout.contains("http_requests_total")
            && stdout.contains("/s")
        {
            break stdout;
        }
        bun.assert_running();
        assert!(
            Instant::now() < deadline,
            "relish metrics never showed a request rate\nstdout={stdout}\nstderr={}\nbun log={}",
            String::from_utf8_lossy(&output.stderr),
            std::fs::read_to_string(&bun.log_path).unwrap_or_default()
        );
        std::thread::sleep(Duration::from_millis(250));
    };
    assert!(
        overview.contains("http_request_duration_seconds") && overview.contains("histogram"),
        "{overview}"
    );
    assert!(overview.contains("up"), "{overview}");

    let detail = wait_for_relish_output(
        &mut bun,
        &[
            "--endpoint",
            &endpoint,
            "metrics",
            "web",
            "--name",
            "http_requests_total",
        ],
        "default__web-0",
    );
    let detail = String::from_utf8_lossy(&detail.stdout);
    assert!(detail.contains("RATE/S"), "{detail}");

    let json = run_relish(&[
        "--endpoint",
        &endpoint,
        "--output",
        "json",
        "metrics",
        "web",
        "--name",
        "up",
    ]);
    assert_success(&json, "relish metrics --output json");
    let rows: serde_json::Value = serde_json::from_slice(&json.stdout).unwrap();
    let row = &rows.as_array().unwrap()[0];
    assert_eq!(row["instance"], "default__web-0", "{rows}");
    assert!(
        row["node"].as_str().unwrap().starts_with("first-run-"),
        "{rows}"
    );
    assert_eq!(row["latest"], 1.0, "the scrape must be up: {rows}");
}
