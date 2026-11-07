//! `relish mcp` as a client starts it: the real binary over stdio, pointed
//! at a stand-in cluster API that records every request.
//!
//! The point is the safety claim: every tool `tools/list` offers can be
//! called, and none of them sends the cluster anything but a read. The
//! tool list is derived from `relish agent`, so a command that lands later is
//! covered here without a change.

use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::State;
use axum::http::{Method, StatusCode, Uri};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout};
use tokio::sync::Mutex;

type Requests = Arc<Mutex<Vec<(Method, String)>>>;

/// A cluster API that answers 404 to everything and remembers what it saw.
async fn stand_in_cluster() -> (String, Requests) {
    let requests: Requests = Arc::default();
    let app = axum::Router::new()
        .fallback(
            |State(requests): State<Requests>, method: Method, uri: Uri| async move {
                requests.lock().await.push((method, uri.to_string()));
                (StatusCode::NOT_FOUND, "no such thing")
            },
        )
        .with_state(requests.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (format!("http://{address}"), requests)
}

fn relish(home: &std::path::Path) -> tokio::process::Command {
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_relish"));
    command
        .env("RELIABURGER_HOME", home)
        .env_remove("RELIABURGER_TOKEN")
        .env_remove("RELIABURGER_CA_CERT")
        .env_remove("RELIABURGER_ENDPOINT")
        .kill_on_drop(true);
    command
}

struct McpProcess {
    _child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
}

impl McpProcess {
    async fn start(endpoint: &str, home: &std::path::Path) -> McpProcess {
        let mut child = relish(home)
            .args(["--endpoint", endpoint, "mcp"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        let mut process = McpProcess {
            _child: child,
            stdin,
            stdout,
            next_id: 1,
        };
        let answer = process
            .request(
                "initialize",
                json!({
                    "protocolVersion": "2025-06-18",
                    "capabilities": {},
                    "clientInfo": {"name": "suite", "version": "1"}
                }),
            )
            .await;
        assert_eq!(answer["result"]["serverInfo"]["name"], "reliaburger");
        process
            .send(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}))
            .await;
        process
    }

    async fn send(&mut self, message: Value) {
        self.stdin
            .write_all(format!("{message}\n").as_bytes())
            .await
            .unwrap();
        self.stdin.flush().await.unwrap();
    }

    async fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.next_id;
        self.next_id += 1;
        self.send(json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
            .await;
        loop {
            let mut line = String::new();
            tokio::time::timeout(Duration::from_secs(60), self.stdout.read_line(&mut line))
                .await
                .expect("relish mcp answers within 60 s")
                .unwrap();
            assert!(!line.is_empty(), "relish mcp hung up");
            // Every line on stdout must be protocol: a stray print would
            // break a real client.
            let message: Value = serde_json::from_str(&line)
                .unwrap_or_else(|error| panic!("not JSON-RPC ({error}): {line}"));
            if message["id"] == id {
                return message;
            }
        }
    }
}

/// Arguments that satisfy `schema`'s required properties.
fn sample_arguments(schema: &Value) -> Value {
    let mut arguments = serde_json::Map::new();
    let required = schema["required"].as_array().cloned().unwrap_or_default();
    for name in required {
        let name = name.as_str().unwrap();
        let property = &schema["properties"][name];
        let value = match property["type"].as_str() {
            Some("string") => match property["enum"].as_array() {
                Some(choices) => choices[0].clone(),
                None => json!(match name {
                    "pattern" => "fn main",
                    "target" => "Cargo.toml",
                    _ => "web",
                }),
            },
            Some("array") => json!(["deploy"]),
            Some("integer") => property.get("default").cloned().unwrap_or(json!(1)),
            Some("boolean") => json!(false),
            other => panic!("{name}: no sample for {other:?}"),
        };
        arguments.insert(name.to_string(), value);
    }
    Value::Object(arguments)
}

#[tokio::test]
async fn no_tool_sends_the_cluster_anything_but_a_read() {
    let home = tempfile::tempdir().unwrap();
    let (endpoint, requests) = stand_in_cluster().await;
    let mut mcp = McpProcess::start(&endpoint, home.path()).await;

    let listed = mcp.request("tools/list", json!({})).await;
    let tools = listed["result"]["tools"].as_array().unwrap().clone();
    assert!(!tools.is_empty(), "{listed}");
    for tool in &tools {
        let name = tool["name"].as_str().unwrap();
        assert_eq!(tool["annotations"]["readOnlyHint"], true, "{name}");
        let arguments = sample_arguments(&tool["inputSchema"]);
        let answer = mcp
            .request("tools/call", json!({"name": name, "arguments": arguments}))
            .await;
        let envelope = &answer["result"]["structuredContent"];
        assert_eq!(
            envelope["schema"], "relish.agent/v1",
            "{name} answers with an envelope: {answer}"
        );
    }

    let requests = requests.lock().await.clone();
    assert!(
        !requests.is_empty(),
        "at least one tool asks the cluster something, or this test proves nothing"
    );
    let writes: Vec<_> = requests
        .iter()
        .filter(|(method, _)| method != Method::GET && method != Method::HEAD)
        .collect();
    assert!(writes.is_empty(), "tools sent writes: {writes:?}");
}

#[tokio::test]
async fn list_honours_toolsets_and_refuses_an_unknown_one() {
    let home = tempfile::tempdir().unwrap();
    let output = relish(home.path())
        .args(["mcp", "--list", "--toolsets", "source"])
        .output()
        .await
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let tools: Value = serde_json::from_slice(&output.stdout).unwrap();
    let names: Vec<&str> = tools
        .as_array()
        .unwrap()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap())
        .collect();
    assert!(!names.is_empty());
    assert!(
        names.iter().all(|name| name.starts_with("source_")),
        "{names:?}"
    );

    let output = relish(home.path())
        .args(["mcp", "--list", "--toolsets", "nonsense"])
        .output()
        .await
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("no toolset is called \"nonsense\""),
        "{stderr}"
    );
}
