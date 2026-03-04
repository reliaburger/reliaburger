//! Relish — the Reliaburger CLI.
//!
//! Command-line interface for managing a Reliaburger cluster.
//! Launches a TUI when invoked with no arguments.

use std::path::PathBuf;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "relish", version, about = "Reliaburger CLI")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Apply configuration from a file or directory.
    Apply {
        /// Path to a TOML config file or directory.
        path: PathBuf,
    },
    /// Show cluster and app status.
    Status,
    /// Stream logs from an app or job.
    Logs {
        /// App or job name.
        name: String,
    },
    /// Execute a command inside a running container.
    Exec {
        /// App name.
        app: String,
        /// Command to run.
        #[arg(trailing_var_arg = true)]
        command: Vec<String>,
    },
    /// Show detailed info about an app, node, or job.
    Inspect {
        /// Resource name.
        name: String,
    },
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Command::Apply { path: _ } => todo!("Phase 1"),
        Command::Status => todo!("Phase 1"),
        Command::Logs { name: _ } => todo!("Phase 1"),
        Command::Exec { app: _, command: _ } => todo!("Phase 1"),
        Command::Inspect { name: _ } => todo!("Phase 1"),
    }
}
