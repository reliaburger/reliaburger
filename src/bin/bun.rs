//! Bun — the Reliaburger node agent.
//!
//! Runs on every node in the cluster. Manages container lifecycle,
//! health checks, and reports state to the cluster leader.

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    println!("bun: reliaburger node agent v{}", env!("CARGO_PKG_VERSION"));
    todo!("Phase 1")
}
