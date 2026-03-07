//! TestApp — a configurable HTTP server for demos and testing.
//!
//! Wraps the library's `TestApp` as a standalone binary. Useful for
//! running with ProcessGrill via the `command` field in app configs.
//!
//! Modes:
//!   healthy           — always returns 200
//!   unhealthy-after N — returns 200 for N requests, then 500
//!   hang              — accepts connections, never responds
//!   slow DELAY_MS     — responds after a delay

use clap::Parser;
use reliaburger::bun::testapp::{TestApp, TestAppMode};
use std::time::Duration;

#[derive(Parser)]
#[command(name = "testapp", version, about = "Configurable test HTTP server")]
struct Cli {
    /// Behaviour mode: healthy, unhealthy-after, hang, slow.
    #[arg(long, default_value = "healthy")]
    mode: String,

    /// Port to listen on.
    #[arg(long, default_value = "8080")]
    port: u16,

    /// Request count for unhealthy-after mode.
    #[arg(long, default_value = "5")]
    count: u32,

    /// Delay in milliseconds for slow mode.
    #[arg(long, default_value = "3000")]
    delay: u64,
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    let mode = match cli.mode.as_str() {
        "healthy" => TestAppMode::Healthy,
        "unhealthy-after" => TestAppMode::UnhealthyAfter(cli.count),
        "hang" => TestAppMode::Hang,
        "slow" => TestAppMode::Slow(Duration::from_millis(cli.delay)),
        other => {
            eprintln!("unknown mode: {other}");
            eprintln!("valid modes: healthy, unhealthy-after, hang, slow");
            std::process::exit(1);
        }
    };

    let app = TestApp::start_on_port(mode, cli.port).await;
    println!(
        "testapp: listening on 127.0.0.1:{} (mode: {})",
        app.port(),
        cli.mode
    );

    tokio::signal::ctrl_c().await.ok();
    println!("\ntestapp: shutting down");
    app.shutdown();
}
