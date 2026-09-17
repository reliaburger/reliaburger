//! Managed cluster creation, secure enrolment and application verification.

use super::{
    artifacts,
    download::Downloader,
    provision,
    security::{self, Bootstrap},
    state::{ClusterSpec, NodePhase, Operation},
};
use crate::{
    bun::agent::CouncilStatus,
    relish::{client::BunClient, local_context::LocalContext},
    upgrade::BinaryVersion,
};
use anyhow::{Context, Result, bail};
use futures_util::{StreamExt, stream::FuturesUnordered};
use std::{
    path::{Path, PathBuf},
    time::Duration,
};

/// Choices for creating or resuming one laptop cluster.
pub struct Options {
    /// Stable cluster name.
    pub name: String,
    /// One or three VMs.
    pub nodes: usize,
    /// First host API port.
    pub api_port: u16,
    /// Browser-facing HTTP ingress port.
    pub ingress_port: u16,
    /// Explicit development-only directory containing prebuilt Linux bun and relish.
    pub development_binaries: Option<PathBuf>,
}

/// Create or resume a cluster under one five-minute deadline, preserving checkpoints.
pub async fn run(options: Options) -> Result<()> {
    let root = crate::relish::local_context::root_directory()?;
    let spec = ClusterSpec {
        name: options.name,
        nodes: options.nodes,
        version: env!("CARGO_PKG_VERSION").parse::<BinaryVersion>()?,
        api_port: options.api_port,
        ingress_port: options.ingress_port,
    };
    let operation_root = root.clone();
    let (mut operation, bootstrap, _setup_lock) =
        tokio::task::spawn_blocking(move || -> Result<_> {
            let operation = Operation::open(&operation_root, &spec)?;
            let mut lock_options = std::fs::OpenOptions::new();
            lock_options.read(true).write(true).create(true).truncate(false);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                lock_options.mode(0o600);
            }
            let setup_lock = lock_options.open(operation_root.join("setup.lock"))?;
            setup_lock.try_lock().context("another managed setup is using this state directory")?;
            if LocalContext::load(&operation_root.join("context.json"))?
                .is_some_and(|context| context.owner != operation.state.id)
            {
                bail!("another cluster owns the active context; use a separate RELIABURGER_HOME for a second cluster");
            }
            let bootstrap = security::prepare(&operation)?;
            Ok((operation, bootstrap, setup_lock))
        })
        .await??;
    super::preflight::socket_paths(&root, operation.state.nodes.iter().map(|node| &node.name))?;
    let started = std::time::Instant::now();
    let result = tokio::time::timeout(
        Duration::from_secs(300),
        provision_cluster(
            &root,
            &mut operation,
            &bootstrap,
            options.development_binaries.as_deref(),
        ),
    )
    .await;
    match result {
        Ok(Ok(())) => {
            println!(
                "cluster {} ready in {:.1}s",
                operation.state.spec.name,
                started.elapsed().as_secs_f64()
            );
            println!(
                "  app: http://localhost:{}",
                operation.state.spec.ingress_port
            );
            println!("  next: relish nodes; relish status; relish logs hello");
            println!(
                "  lifecycle: relish local status|stop|start|destroy --name {}",
                operation.state.spec.name
            );
            Ok(())
        }
        Ok(Err(error)) => Err(error.context(format!(
            "setup stopped; checkpoints are in {}. Re-run the same setup command to resume",
            operation.directory.display()
        ))),
        Err(_) => bail!(
            "setup exceeded five minutes; progress is saved in {}. Re-run the same setup command to resume",
            operation.directory.display()
        ),
    }
}

async fn provision_cluster(
    root: &Path,
    operation: &mut Operation,
    bootstrap: &Bootstrap,
    development: Option<&Path>,
) -> Result<()> {
    let spec = operation.state.spec.clone();
    let cache = root.join("cache");
    tokio::fs::create_dir_all(&cache).await?;
    super::preflight::host(root).await?;
    let downloader = Downloader::new(Duration::from_secs(180))?;
    println!("preparing verified Linux image and tooling");
    let binaries = async {
        if let Some(directory) = development {
            eprintln!(
                "development binaries selected explicitly; this run does not qualify a published release"
            );
            let bun = tokio::fs::canonicalize(directory.join("bun")).await?;
            let relish = tokio::fs::canonicalize(directory.join("relish")).await?;
            return Ok::<_, anyhow::Error>((bun, relish));
        }
        tokio::try_join!(
            artifacts::binary(&cache, &spec.version, "bun", &downloader),
            artifacts::binary(&cache, &spec.version, "relish", &downloader)
        )
    };
    let image = async {
        if development.is_some() {
            let image = artifacts::guest_image(std::env::consts::ARCH)?;
            let path = cache.join(&image.asset);
            downloader
                .fetch(&image.url, &image.sha256, &path, 2 * 1024 * 1024 * 1024)
                .await?;
            Ok::<_, anyhow::Error>(path)
        } else {
            artifacts::image(&cache, &spec.version, &downloader).await
        }
    };
    let (lima, image, (bun, relish)) =
        tokio::try_join!(artifacts::tooling(root, &downloader), image, binaries)?;
    println!("starting {} Linux VM(s)", spec.nodes);
    let mut statuses = Vec::new();
    for node in &operation.state.nodes {
        statuses.push(lima.status(&node.name).await?);
    }
    let to_start = statuses
        .iter()
        .filter(|status| status.as_deref() != Some("Running"))
        .count();
    let to_create = statuses.iter().filter(|status| status.is_none()).count();
    super::preflight::resources(to_start, to_create, root).await?;
    let mut boots = FuturesUnordered::new();
    let nodes = operation.state.nodes.clone();
    for (index, node) in nodes.iter().enumerate() {
        let lima = lima.clone();
        let node = node.clone();
        let config_path = operation.directory.join(format!("{}.yaml", node.name));
        let yaml = provision::vm_config(
            image.to_str().context("non-UTF-8 image cache path")?,
            std::env::consts::ARCH,
            spec.api_port + index as u16,
            (index == 0).then_some(spec.ingress_port),
        )?;
        tokio::fs::write(&config_path, yaml).await?;
        let status = statuses[index].clone();
        let api_port = spec.api_port + index as u16;
        let ingress_port = (index == 0).then_some(spec.ingress_port);
        let boot = async move {
            if status.as_deref() != Some("Running") {
                let mut ports = vec![api_port];
                ports.extend(ingress_port);
                super::preflight::ports(&ports).await?;
            }
            match status.as_deref() {
                None if node.phase != NodePhase::Planned => bail!(
                    "owned VM {} disappeared; refusing to create a replacement cluster implicitly",
                    node.name
                ),
                None => {
                    lima.command(&[
                        "start",
                        "--tty=false",
                        &format!("--name={}", node.name),
                        config_path.to_str().context("invalid VM config path")?,
                    ])
                    .await?;
                }
                Some("Running") => {}
                Some("Stopped") => {
                    lima.command(&["start", "--tty=false", &node.name]).await?;
                }
                Some(status) => bail!("VM {} is in unexpected state {status}", node.name),
            }
            lima.wait_for_guest(&node.name).await?;
            Ok::<_, anyhow::Error>((index, lima.address(&node.name).await?))
        };
        // Lima creates its shared SSH key on first boot. Initialise once before
        // starting peers, otherwise concurrent ssh-keygen calls can overwrite it.
        if index == 0 {
            record_boot(operation, boot.await?).await?;
        } else {
            boots.push(boot);
        }
    }
    while let Some(result) = boots.next().await {
        record_boot(operation, result?).await?;
    }
    let peers: Vec<_> = operation
        .state
        .nodes
        .iter()
        .filter_map(|node| node.address)
        .collect();
    let first_address = operation.state.nodes[0]
        .address
        .context("bootstrap VM has no address")?;
    let endpoint = format!("https://127.0.0.1:{}", spec.api_port);
    let client = bootstrap.client(&endpoint)?;
    for index in 0..spec.nodes {
        let node = operation.state.nodes[index].clone();
        if node.phase != NodePhase::Started {
            println!("configuring {}", node.name);
            lima.install(&node.name, &bun, "/usr/local/bin/bun", true)
                .await?;
            lima.install(&node.name, &relish, "/usr/local/bin/relish", true)
                .await?;
            let config = provision::node_config(
                &spec.name,
                &node.name,
                node.address.context("VM has no address")?,
                (index > 0).then_some(first_address),
                &peers,
            )?;
            let config_path = operation.directory.join(format!("{}.toml", node.name));
            tokio::fs::write(&config_path, config).await?;
            lima.install(
                &node.name,
                &config_path,
                "/etc/reliaburger/node.toml",
                false,
            )
            .await?;
            lima.install(
                &node.name,
                &bootstrap.directory.join("master.key"),
                "/etc/reliaburger/master.key",
                false,
            )
            .await?;
            if index == 0 {
                lima.install(
                    &node.name,
                    &bootstrap.directory.join("security-bootstrap.json"),
                    "/etc/reliaburger/security-bootstrap.json",
                    false,
                )
                .await?;
                // Copy the commit marker last, just as the local identity store does.
                for file in [
                    "node.crt",
                    "node.key",
                    "node-ca.crt",
                    "root-ca.crt",
                    "meta.json",
                    "bundle.committed",
                ] {
                    lima.install(
                        &node.name,
                        &bootstrap.directory.join("identity").join(file),
                        &format!("/etc/reliaburger/identity/{file}"),
                        false,
                    )
                    .await?;
                }
            }
            operation.state.nodes[index].phase = NodePhase::Configured;
            operation.save_async().await?;
            if index > 0 {
                let enrolled = lima
                    .command(&[
                        "shell",
                        &node.name,
                        "sudo",
                        "test",
                        "-f",
                        "/etc/reliaburger/identity/bundle.committed",
                    ])
                    .await
                    .is_ok();
                if !enrolled {
                    let token = client.join_token_create(&node.name, 300).await?;
                    let token_path = operation.directory.join("join.token");
                    crate::sesame::identity::atomic_write_mode(
                        &token_path,
                        token.as_bytes(),
                        Some(0o600),
                    )?;
                    let result = async {
                        lima.install(
                            &node.name,
                            &token_path,
                            "/etc/reliaburger/join.token",
                            false,
                        )
                        .await?;
                        lima.command(&[
                            "shell",
                            &node.name,
                            "sudo",
                            "/usr/local/bin/relish",
                            "join",
                            "--token-file",
                            "/etc/reliaburger/join.token",
                            "--node-id",
                            &node.name,
                            "--identity-dir",
                            "/etc/reliaburger/identity",
                            "--ca-fingerprint",
                            &bootstrap.root_fingerprint,
                            &format!("https://{first_address}:9117"),
                        ])
                        .await?;
                        Ok::<_, anyhow::Error>(())
                    }
                    .await;
                    let _ = tokio::fs::remove_file(&token_path).await;
                    let _ = lima
                        .command(&[
                            "shell",
                            &node.name,
                            "sudo",
                            "rm",
                            "-f",
                            "/etc/reliaburger/join.token",
                        ])
                        .await;
                    result?;
                }
            }
            operation.state.nodes[index].phase = NodePhase::Enrolled;
            operation.save_async().await?;
            let service_path = operation.directory.join("reliaburger.service");
            tokio::fs::write(&service_path, provision::SERVICE).await?;
            lima.install(
                &node.name,
                &service_path,
                "/etc/systemd/system/reliaburger.service",
                false,
            )
            .await?;
            lima.command(&["shell", &node.name, "sudo", "systemctl", "daemon-reload"])
                .await?;
            lima.command(&[
                "shell",
                &node.name,
                "sudo",
                "systemctl",
                "enable",
                "--now",
                "reliaburger.service",
            ])
            .await?;
            operation.state.nodes[index].phase = NodePhase::Started;
            operation.save_async().await?;
        }
        lima.command(&[
            "shell",
            &node.name,
            "sudo",
            "systemctl",
            "start",
            "reliaburger.service",
        ])
        .await?;
        let node_client = bootstrap.client(&format!(
            "https://127.0.0.1:{}",
            spec.api_port + index as u16
        ))?;
        crate::relish::readiness::wait_for_node(&node_client, Duration::from_secs(45)).await?;
        let version: BinaryVersion = node_client.node_version().await?.parse()?;
        if version != spec.version {
            bail!(
                "VM {} is running {version}, expected {}",
                node.name,
                spec.version
            );
        }
    }
    println!("checking quorum and deploying the hello container");
    let names: Vec<_> = operation
        .state
        .nodes
        .iter()
        .map(|node| node.name.clone())
        .collect();
    wait_for_quorum(&client, &names).await?;
    let demo = demo_config();
    tokio::fs::write(operation.directory.join("hello.toml"), &demo).await?;
    client.apply(&crate::config::Config::parse(&demo)?).await?;
    probe_demo(spec.ingress_port).await?;
    let context = LocalContext {
        schema: 1,
        owner: operation.state.id.clone(),
        endpoint,
        token: tokio::fs::read_to_string(bootstrap.directory.join("admin.token")).await?,
        ca_cert: bootstrap.directory.join("identity/root-ca.crt"),
    };
    let path = root.join("context.json");
    tokio::task::spawn_blocking(move || context.save(&path)).await??;
    Ok(())
}

async fn record_boot(
    operation: &mut Operation,
    (index, address): (usize, std::net::Ipv4Addr),
) -> Result<()> {
    let node = &mut operation.state.nodes[index];
    if node.address.is_some_and(|previous| previous != address) && node.phase == NodePhase::Started
    {
        bail!(
            "VM {} changed its shared address; refusing to resume with stale peer configuration",
            node.name
        );
    }
    node.address = Some(address);
    if node.phase == NodePhase::Planned {
        node.phase = NodePhase::Created;
    }
    operation.save_async().await?;
    Ok(())
}

fn quorum_ready(names: &[String], council: &CouncilStatus) -> bool {
    council
        .leader
        .as_ref()
        .is_some_and(|leader| names.contains(leader))
        && council.members.len() == names.len()
        && names
            .iter()
            .all(|name| council.members.iter().any(|member| &member.name == name))
}

pub(super) async fn wait_for_quorum(client: &BunClient, names: &[String]) -> Result<()> {
    tokio::time::timeout(Duration::from_secs(45), async {
        loop {
            if let (Ok(council), Ok(nodes)) = (client.council().await, client.nodes().await)
                && quorum_ready(names, &council)
                && names.iter().all(|name| {
                    nodes
                        .iter()
                        .any(|node| &node.node_id == name && node.state == "alive")
                })
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    })
    .await
    .context("owned nodes did not form a healthy council quorum")?;
    Ok(())
}

fn demo_config() -> String {
    format!(
        r#"[app.hello]
image = "{}"
command = ["sh", "-c", "mkdir -p /tmp/www; printf 'Reliaburger is running\\n' > /tmp/www/index.html; exec httpd -f -p 8080 -h /tmp/www"]
port = 8080
replicas = 1
[app.hello.health]
path = "/"
interval = 1
threshold_healthy = 1
[app.hello.ingress]
host = "localhost"
"#,
        crate::testkit::PINNED_TEST_WORKLOAD_IMAGE.replacen(
            "docker.io/library/",
            "public.ecr.aws/docker/library/",
            1
        )
    )
}

async fn probe_demo(port: u16) -> Result<()> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(3))
        .build()?;
    tokio::time::timeout(Duration::from_secs(45), async {
        loop {
            if let Ok(response) = client
                .get(format!("http://127.0.0.1:{port}/"))
                .header("Host", "localhost")
                .send()
                .await
                && response.status().is_success()
                && response
                    .text()
                    .await
                    .is_ok_and(|body| body.trim() == "Reliaburger is running")
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
    })
    .await
    .context("hello container did not answer through the host ingress port")?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bun::agent::{CouncilMemberInfo, CouncilStatus};

    #[test]
    fn readiness_requires_every_owned_voter_and_an_owned_leader() {
        let names = vec!["one".into(), "two".into(), "three".into()];
        let mut council = CouncilStatus {
            leader: Some("one".into()),
            ..Default::default()
        };
        assert!(!quorum_ready(&names, &council));
        council.members = names
            .iter()
            .enumerate()
            .map(|(index, name)| CouncilMemberInfo {
                raft_id: index as u64,
                name: name.clone(),
                address: "192.168.104.1:9444".into(),
            })
            .collect();
        assert!(quorum_ready(&names, &council));
        council.leader = Some("foreign".into());
        assert!(!quorum_ready(&names, &council));
    }

    #[test]
    fn demo_is_a_pinned_container_with_health_and_browser_ingress() {
        let config = crate::config::Config::parse(&demo_config()).unwrap();
        let app = &config.app["hello"];
        assert!(
            app.image
                .as_ref()
                .unwrap()
                .starts_with("public.ecr.aws/docker/library/busybox@sha256:")
        );
        assert_eq!(app.ingress.as_ref().unwrap().host, "localhost");
        assert_eq!(app.health.as_ref().unwrap().path, "/");
    }
}
