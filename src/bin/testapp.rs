//! TestApp — a configurable HTTP server for demos and testing.
//!
//! Wraps the library's `TestApp` as a standalone binary. Useful for
//! running with ProcessGrill via the `command` field in app configs.
//!
//! `bun testapp` runs the same code, so a cluster node needs nothing but
//! `bun` on its PATH. This binary stays for local demos and for configs that
//! already reference it.
//!
//! Modes:
//!   healthy           — always returns 200
//!   unhealthy-after N — returns 200 for N requests, then 500
//!   hang              — accepts connections, never responds
//!   exit-after N      — exits cleanly after N requests
//!   slow DELAY_MS     — responds after a delay
//!   alloc MIB         — holds MIB megabytes resident, otherwise healthy

use clap::Parser;
use reliaburger::bun::testapp::{TestApp, parse_mode};

#[derive(Parser)]
#[command(name = "testapp", version, about = "Configurable test HTTP server")]
struct Cli {
    /// Behaviour: healthy, unhealthy-after, hang, exit-after, slow, alloc.
    #[arg(long, default_value = "healthy")]
    mode: String,

    /// Port to listen on.
    #[arg(long, default_value = "8080")]
    port: u16,

    /// Request count for unhealthy-after and exit-after.
    #[arg(long, default_value = "5")]
    count: u32,

    /// Delay in milliseconds for slow mode.
    #[arg(long, default_value = "3000")]
    delay: u64,

    /// Resident megabytes for alloc mode.
    #[arg(long, default_value = "64")]
    alloc_mib: usize,
}

#[tokio::main]
async fn main() {
    let cli = Cli::parse();

    // One parser, shared with `bun testapp`. The two used to have separate
    // match arms and had already drifted — this binary silently lacked
    // `exit-after`, which the library had supported for some time.
    let mode = match parse_mode(&cli.mode, cli.count, cli.delay, cli.alloc_mib) {
        Ok(mode) => mode,
        Err(message) => {
            eprintln!("testapp: {message}");
            std::process::exit(1);
        }
    };

    let app = TestApp::start_on_port(mode, cli.port).await;
    println!(
        "testapp: listening on 0.0.0.0:{} (mode: {})",
        app.port(),
        cli.mode
    );

    tokio::signal::ctrl_c().await.ok();
    println!("\ntestapp: shutting down");
    app.shutdown();
}
