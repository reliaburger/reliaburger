//! Relish — the Reliaburger CLI.
//!
//! Command-line interface for managing a Reliaburger cluster.
//! Launches a TUI when invoked with no arguments.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use reliaburger::relish::OutputFormat;
use reliaburger::relish::commands;

#[derive(Parser)]
#[command(name = "relish", version, about = "Reliaburger CLI")]
struct Cli {
    /// Output format: human, json, or yaml.
    #[arg(long, default_value = "human", global = true)]
    output: OutputFormat,

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
        /// Show only the last N lines.
        #[arg(long)]
        tail: Option<usize>,
        /// Follow log output (stream new lines as they appear).
        #[arg(long, short = 'f')]
        follow: bool,
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
    /// Stop all instances of an app.
    Stop {
        /// App name.
        app: String,
    },
    /// Initialise a new project with starter config files.
    Init {
        /// Directory to create config files in.
        #[arg(default_value = ".")]
        dir: PathBuf,
    },
    /// List cluster nodes and their gossip state.
    Nodes,
    /// Show council (Raft) composition and status.
    Council,
    /// Join an existing cluster.
    Join {
        /// Join token (validated in Phase 4).
        #[arg(long)]
        token: String,
        /// Address of an existing cluster member (gossip endpoint).
        addr: String,
    },
    /// Resolve a service name to its VIP and backends.
    Resolve {
        /// Service name (e.g. "redis").
        name: String,
    },
    /// Run chaos testing scenarios or manage fault injections.
    Chaos {
        /// Scenario or action: council-partition, worker-isolation, status, heal.
        action: String,
    },
    /// Manage a local dev cluster (Lima VMs).
    Dev {
        #[command(subcommand)]
        action: DevAction,
    },
}

#[derive(Subcommand)]
enum DevAction {
    /// Create a new dev cluster.
    Create {
        /// Number of nodes.
        #[arg(long, default_value = "3")]
        nodes: usize,
        /// CPUs per node.
        #[arg(long, default_value = "2")]
        cpus: usize,
        /// Memory per node (e.g. "2GiB").
        #[arg(long, default_value = "2GiB")]
        memory: String,
        /// Cluster name.
        #[arg(long, default_value = "default")]
        name: String,
    },
    /// Show dev cluster status.
    Status {
        /// Cluster name.
        #[arg(default_value = "default")]
        name: String,
    },
    /// Open a shell on a node.
    Shell {
        /// Node name (e.g. reliaburger-1).
        node: String,
    },
    /// Stop a dev cluster (VMs stay on disk).
    Stop {
        /// Cluster name.
        #[arg(default_value = "default")]
        name: String,
    },
    /// Start a stopped dev cluster.
    Start {
        /// Cluster name.
        #[arg(default_value = "default")]
        name: String,
    },
    /// Destroy a dev cluster (delete all VMs).
    Destroy {
        /// Cluster name.
        #[arg(default_value = "default")]
        name: String,
    },
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();

    let result = match cli.command {
        Command::Apply { ref path } => commands::apply(path, cli.output).await,
        Command::Status => commands::status().await,
        Command::Logs {
            ref name,
            tail,
            follow,
        } => commands::logs(name, tail, follow).await,
        Command::Exec {
            ref app,
            ref command,
        } => commands::exec(app, command).await,
        Command::Inspect { ref name } => commands::inspect(name).await,
        Command::Stop { ref app } => commands::stop(app).await,
        Command::Init { ref dir } => commands::init(dir),
        Command::Nodes => commands::nodes(cli.output).await,
        Command::Council => commands::council(cli.output).await,
        Command::Join {
            ref token,
            ref addr,
        } => commands::join(token, addr).await,
        Command::Resolve { ref name } => commands::resolve(name).await,
        Command::Chaos { ref action } => commands::chaos(action).await,
        Command::Dev { action } => match &action {
            DevAction::Create {
                nodes,
                cpus,
                memory,
                name,
            } => reliaburger::relish::dev::create(name, *nodes, *cpus, memory).await,
            DevAction::Status { name } => reliaburger::relish::dev::status(name).await,
            DevAction::Shell { node } => reliaburger::relish::dev::shell(node).await,
            DevAction::Stop { name } => reliaburger::relish::dev::stop(name).await,
            DevAction::Start { name } => reliaburger::relish::dev::start(name).await,
            DevAction::Destroy { name } => reliaburger::relish::dev::destroy(name).await,
        },
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    fn parse(args: &[&str]) -> Result<Cli, clap::Error> {
        Cli::try_parse_from(args)
    }

    #[test]
    fn parse_apply_command() {
        let cli = parse(&["relish", "apply", "config.toml"]).unwrap();
        assert!(
            matches!(cli.command, Command::Apply { ref path } if path.to_str() == Some("config.toml"))
        );
    }

    #[test]
    fn parse_status_command() {
        let cli = parse(&["relish", "status"]).unwrap();
        assert!(matches!(cli.command, Command::Status));
    }

    #[test]
    fn parse_exec_with_trailing_args() {
        let cli = parse(&["relish", "exec", "web", "sh", "-c", "ls"]).unwrap();
        match cli.command {
            Command::Exec { app, command } => {
                assert_eq!(app, "web");
                assert_eq!(command, vec!["sh", "-c", "ls"]);
            }
            _ => panic!("expected Exec command"),
        }
    }

    #[test]
    fn output_flag_json() {
        let cli = parse(&["relish", "--output", "json", "status"]).unwrap();
        assert_eq!(cli.output, OutputFormat::Json);
    }

    #[test]
    fn output_flag_yaml() {
        let cli = parse(&["relish", "--output", "yaml", "status"]).unwrap();
        assert_eq!(cli.output, OutputFormat::Yaml);
    }

    #[test]
    fn default_output_is_human() {
        let cli = parse(&["relish", "status"]).unwrap();
        assert_eq!(cli.output, OutputFormat::Human);
    }

    #[test]
    fn parse_init_command() {
        let cli = parse(&["relish", "init"]).unwrap();
        assert!(matches!(cli.command, Command::Init { ref dir } if dir.to_str() == Some(".")));
    }

    #[test]
    fn parse_init_with_dir() {
        let cli = parse(&["relish", "init", "/tmp/myproject"]).unwrap();
        assert!(
            matches!(cli.command, Command::Init { ref dir } if dir.to_str() == Some("/tmp/myproject"))
        );
    }

    #[test]
    fn parse_nodes_command() {
        let cli = parse(&["relish", "nodes"]).unwrap();
        assert!(matches!(cli.command, Command::Nodes));
    }

    #[test]
    fn parse_council_command() {
        let cli = parse(&["relish", "council"]).unwrap();
        assert!(matches!(cli.command, Command::Council));
    }

    #[test]
    fn parse_join_command() {
        let cli = parse(&["relish", "join", "--token", "abc123", "10.0.1.5:9443"]).unwrap();
        match cli.command {
            Command::Join { token, addr } => {
                assert_eq!(token, "abc123");
                assert_eq!(addr, "10.0.1.5:9443");
            }
            _ => panic!("expected Join command"),
        }
    }

    #[test]
    fn parse_join_missing_token_rejected() {
        let result = parse(&["relish", "join", "10.0.1.5:9443"]);
        assert!(result.is_err());
    }

    #[test]
    fn invalid_output_format_rejected() {
        let result = parse(&["relish", "--output", "csv", "status"]);
        assert!(result.is_err());
    }

    #[test]
    fn parse_resolve_command() {
        let cli = parse(&["relish", "resolve", "redis"]).unwrap();
        assert!(matches!(cli.command, Command::Resolve { ref name } if name == "redis"));
    }

    #[test]
    fn parse_stop_command() {
        let cli = parse(&["relish", "stop", "web"]).unwrap();
        assert!(matches!(cli.command, Command::Stop { ref app } if app == "web"));
    }

    #[test]
    fn parse_logs_with_tail() {
        let cli = parse(&["relish", "logs", "web", "--tail", "10"]).unwrap();
        match cli.command {
            Command::Logs { name, tail, follow } => {
                assert_eq!(name, "web");
                assert_eq!(tail, Some(10));
                assert!(!follow);
            }
            _ => panic!("expected Logs command"),
        }
    }

    #[test]
    fn parse_logs_with_follow_short() {
        let cli = parse(&["relish", "logs", "web", "-f"]).unwrap();
        match cli.command {
            Command::Logs { name, tail, follow } => {
                assert_eq!(name, "web");
                assert_eq!(tail, None);
                assert!(follow);
            }
            _ => panic!("expected Logs command"),
        }
    }

    #[test]
    fn parse_logs_with_follow_and_tail() {
        let cli = parse(&["relish", "logs", "web", "--follow", "--tail", "5"]).unwrap();
        match cli.command {
            Command::Logs { name, tail, follow } => {
                assert_eq!(name, "web");
                assert_eq!(tail, Some(5));
                assert!(follow);
            }
            _ => panic!("expected Logs command"),
        }
    }

    #[test]
    fn parse_dev_create_defaults() {
        let cli = parse(&["relish", "dev", "create"]).unwrap();
        match cli.command {
            Command::Dev {
                action:
                    DevAction::Create {
                        nodes,
                        cpus,
                        memory,
                        name,
                    },
            } => {
                assert_eq!(nodes, 3);
                assert_eq!(cpus, 2);
                assert_eq!(memory, "2GiB");
                assert_eq!(name, "default");
            }
            _ => panic!("expected Dev Create command"),
        }
    }

    #[test]
    fn parse_dev_create_custom() {
        let cli = parse(&[
            "relish", "dev", "create", "--nodes", "5", "--cpus", "4", "--memory", "4GiB", "--name",
            "big",
        ])
        .unwrap();
        match cli.command {
            Command::Dev {
                action:
                    DevAction::Create {
                        nodes,
                        cpus,
                        memory,
                        name,
                    },
            } => {
                assert_eq!(nodes, 5);
                assert_eq!(cpus, 4);
                assert_eq!(memory, "4GiB");
                assert_eq!(name, "big");
            }
            _ => panic!("expected Dev Create command"),
        }
    }

    #[test]
    fn parse_dev_destroy() {
        let cli = parse(&["relish", "dev", "destroy"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Dev {
                action: DevAction::Destroy { .. }
            }
        ));
    }

    #[test]
    fn parse_dev_shell() {
        let cli = parse(&["relish", "dev", "shell", "reliaburger-1"]).unwrap();
        match cli.command {
            Command::Dev {
                action: DevAction::Shell { node },
            } => assert_eq!(node, "reliaburger-1"),
            _ => panic!("expected Dev Shell command"),
        }
    }
}
