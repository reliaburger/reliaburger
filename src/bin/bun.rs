//! Bun — the Reliaburger node agent.
//!
//! Runs on every node in the cluster. Manages container lifecycle,
//! health checks, and reports state to the cluster leader.

use std::path::PathBuf;

use clap::Parser;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use reliaburger::bun::agent::BunAgent;
use reliaburger::bun::api;
use reliaburger::config::node::NodeConfig;
use reliaburger::grill::port::PortAllocator;
use reliaburger::grill::{AnyGrill, ProcessGrill, detect_runtime};

#[derive(Parser)]
#[command(name = "bun", version, about = "Reliaburger node agent")]
struct Cli {
    /// Path to node configuration file.
    #[arg(long)]
    config: Option<PathBuf>,

    /// Listen address for the local API.
    #[arg(long, default_value = "127.0.0.1:9117")]
    listen: String,

    /// Runtime to use: auto, process, runc, apple.
    #[arg(long, default_value = "auto")]
    runtime: String,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    println!("bun: reliaburger node agent v{}", env!("CARGO_PKG_VERSION"));

    // Load node config
    let config = if let Some(ref path) = cli.config {
        NodeConfig::from_file(path).map_err(|e| anyhow::anyhow!("failed to load config: {e}"))?
    } else {
        NodeConfig::default()
    };

    // Create port allocator from config
    let port_allocator = PortAllocator::new(
        config.network.port_range.start,
        config.network.port_range.end,
    );

    // Select runtime
    let runtime = select_runtime(&cli.runtime).await?;

    // Create command channel
    let (cmd_tx, cmd_rx) = mpsc::channel(256);

    // Create shutdown token
    let shutdown = CancellationToken::new();

    // Spawn the agent
    let agent_shutdown = shutdown.clone();
    let agent_handle = tokio::spawn(async move {
        let mut agent = BunAgent::new(runtime, port_allocator, cmd_rx, agent_shutdown);
        agent.run().await;
    });

    // Start the API server
    let listener = tokio::net::TcpListener::bind(&cli.listen).await?;
    println!("bun: API server listening on {}", cli.listen);

    let app = api::router(cmd_tx);
    let server_shutdown = shutdown.clone();
    let server_handle = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                server_shutdown.cancelled().await;
            })
            .await
            .ok();
    });

    // Wait for SIGINT or SIGTERM
    let signal_shutdown = shutdown.clone();
    tokio::spawn(async move {
        let ctrl_c = tokio::signal::ctrl_c();
        ctrl_c.await.ok();
        println!("\nbun: received shutdown signal");
        signal_shutdown.cancel();
    });

    // Wait for everything to finish
    let _ = tokio::join!(agent_handle, server_handle);
    println!("bun: shutdown complete");

    Ok(())
}

async fn select_runtime(name: &str) -> anyhow::Result<AnyGrill> {
    match name {
        "auto" => {
            let runtime = detect_runtime().await;
            let kind = match &runtime {
                AnyGrill::Process(_) => "process",
                #[cfg(target_os = "linux")]
                AnyGrill::Runc(_) => "runc",
                #[cfg(target_os = "macos")]
                AnyGrill::Apple(_) => "apple-container",
            };
            println!("bun: auto-detected runtime: {kind}");
            Ok(runtime)
        }
        "process" => {
            println!("bun: using process runtime");
            Ok(AnyGrill::Process(ProcessGrill::new()))
        }
        #[cfg(target_os = "linux")]
        "runc" => {
            let is_rootless = reliaburger::grill::rootless::is_rootless();
            let mode = if is_rootless { "rootless" } else { "root" };
            println!("bun: using runc runtime ({mode})");

            let (bundle_base, image_store, state_dir) = if is_rootless {
                let base = dirs::data_local_dir()
                    .unwrap_or_else(|| std::path::PathBuf::from("/tmp/reliaburger"))
                    .join("reliaburger");
                (
                    base.join("bundles"),
                    reliaburger::grill::ImageStore::new(base.join("images")),
                    reliaburger::grill::rootless::rootless_state_dir(),
                )
            } else {
                let base = std::path::PathBuf::from("/var/lib/reliaburger");
                (
                    base.join("bundles"),
                    reliaburger::grill::ImageStore::new(base.join("images")),
                    std::path::PathBuf::from("/run/reliaburger/runc"),
                )
            };

            Ok(AnyGrill::Runc(reliaburger::grill::runc::RuncGrill::new(
                bundle_base, image_store, is_rootless, state_dir,
            )))
        }
        #[cfg(target_os = "macos")]
        "apple" => {
            println!("bun: using Apple Container runtime");
            Ok(AnyGrill::Apple(
                reliaburger::grill::apple::AppleContainerGrill::new(),
            ))
        }
        other => anyhow::bail!("unknown runtime: {other}"),
    }
}
