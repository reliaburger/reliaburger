//! Real Bun listeners must publish the ports the kernel actually assigned.

use std::time::Duration;

#[tokio::test]
async fn bun_publishes_reachable_ephemeral_service_endpoints() {
    let root = tempfile::tempdir().unwrap();
    let mut node = reliaburger::config::NodeConfig::default();
    node.storage.data = root.path().join("data");
    node.storage.images = root.path().join("images");
    node.storage.logs = root.path().join("logs");
    node.storage.metrics = root.path().join("metrics");
    node.storage.volumes = root.path().join("volumes");
    node.images.registry_port = 0;
    node.ingress.enabled = true;
    node.ingress.http_port = 0;
    node.ingress.https_port = 0;
    let path = root.path().join("node.toml");
    std::fs::write(&path, toml::to_string_pretty(&node).unwrap()).unwrap();
    let log = std::fs::File::create(root.path().join("bun.log")).unwrap();
    let mut child = tokio::process::Command::new(env!("CARGO_BIN_EXE_bun"))
        .args([
            "--runtime",
            "process",
            "--listen",
            "127.0.0.1:0",
            "--config",
        ])
        .arg(path)
        .stdout(log.try_clone().unwrap())
        .stderr(log)
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let address: std::net::SocketAddr = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let output = std::fs::read_to_string(root.path().join("bun.log")).unwrap();
            assert!(child.try_wait().unwrap().is_none(), "bun exited: {output}");
            if let Some(address) = output.lines().find_map(|line| {
                line.strip_prefix("bun: API server listening on ")?
                    .parse()
                    .ok()
            }) {
                break address;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .unwrap();
    assert_ne!(address.port(), 0);
    let client =
        reliaburger::relish::client::BunClient::new_with_token(&format!("http://{address}"), None);
    let report = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            assert!(
                child.try_wait().unwrap().is_none(),
                "bun exited: {}",
                std::fs::read_to_string(root.path().join("bun.log")).unwrap()
            );
            if let Ok(report) = client.capabilities().await {
                break report;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .unwrap();
    let http = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(3))
        .build()
        .unwrap();
    for (origin, path, expected) in [
        (report.service_endpoints.registry.unwrap(), "/v2/", 200),
        (
            report.service_endpoints.ingress_http.unwrap(),
            "/missing",
            404,
        ),
    ] {
        let mut url = url::Url::parse(&origin).unwrap();
        assert_ne!(url.port().unwrap(), 0);
        url.set_host(Some("127.0.0.1")).unwrap();
        url.set_path(path);
        assert_eq!(
            http.get(url).send().await.unwrap().status().as_u16(),
            expected
        );
    }
    let https = url::Url::parse(&report.service_endpoints.ingress_https.unwrap()).unwrap();
    let port = https.port().unwrap();
    assert_ne!(port, 0);
    tokio::net::TcpStream::connect((std::net::Ipv4Addr::LOCALHOST, port))
        .await
        .unwrap();
    child.kill().await.unwrap();
    child.wait().await.unwrap();
}
