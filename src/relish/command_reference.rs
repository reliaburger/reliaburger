//! The README's "Everything relish can do" section, rendered from the CLI.
//!
//! The clap definitions in `src/bin/relish.rs` are the one source of truth
//! for relish's commands and their one-line help. This module walks that
//! command tree and writes it out as Markdown, grouped by what an operator is
//! trying to do, with each group linking to the manual chapter that explains
//! it properly. A test in the relish binary compares the result with the
//! marked region of `README.md`, so the list can't drift from the CLI.

use std::fmt::Write as _;

use clap::{Arg, ArgAction, Command};

/// Opens the generated region of `README.md`.
pub const START_MARKER: &str = "<!-- relish-commands:start -->";

/// Closes the generated region of `README.md`.
pub const END_MARKER: &str = "<!-- relish-commands:end -->";

/// The command that rewrites the region, quoted in every staleness failure.
pub const REGENERATE_COMMAND: &str = "make readme-commands";

/// A manual chapter a group of commands links to.
#[derive(Debug, Clone, Copy)]
pub struct Guide {
    /// Link text.
    pub label: &'static str,
    /// Path relative to the repository root.
    pub path: &'static str,
}

/// A set of top-level commands that belong together in the README.
#[derive(Debug, Clone, Copy)]
pub struct CommandGroup {
    /// Heading shown above the commands.
    pub title: &'static str,
    /// Chapters to read for depth.
    pub guides: &'static [Guide],
    /// Top-level command names, in display order.
    pub commands: &'static [&'static str],
}

/// Every visible top-level relish command, grouped for the README.
///
/// A new command must be added here, or the README test fails and says so.
pub const GROUPS: &[CommandGroup] = &[
    CommandGroup {
        title: "Set up and run a cluster",
        guides: &[
            Guide {
                label: "getting started",
                path: "docs/manual/00_getting-started.md",
            },
            Guide {
                label: "cluster basics",
                path: "docs/manual/02_cluster-basics.md",
            },
        ],
        commands: &[
            "setup",
            "local",
            "init",
            "join",
            "join-token",
            "nodes",
            "council",
            "decommission-node",
            "uninstall",
        ],
    },
    CommandGroup {
        title: "Deploy and manage apps",
        guides: &[Guide {
            label: "deploy an app",
            path: "docs/manual/01_deploy-an-app.md",
        }],
        commands: &[
            "apply",
            "status",
            "inspect",
            "exec",
            "deploy",
            "cancel-deploy",
            "history",
            "rollback",
            "stop",
            "delete",
            "batch",
            "batch-status",
        ],
    },
    CommandGroup {
        title: "Work with config files",
        guides: &[
            Guide {
                label: "deploy an app",
                path: "docs/manual/01_deploy-an-app.md",
            },
            Guide {
                label: "coming from Kubernetes",
                path: "docs/manual/09_kubernetes.md",
            },
        ],
        commands: &["lint", "fmt", "compile", "diff", "import", "export"],
    },
    CommandGroup {
        title: "Networking",
        guides: &[Guide {
            label: "networking and ingress",
            path: "docs/manual/03_networking.md",
        }],
        commands: &["resolve", "routes"],
    },
    CommandGroup {
        title: "Watch what's running",
        guides: &[Guide {
            label: "observability",
            path: "docs/manual/04_observability.md",
        }],
        commands: &[
            "tui",
            "dashboard",
            "top",
            "metrics",
            "logs",
            "logs-export",
            "logs-search",
        ],
    },
    CommandGroup {
        title: "Diagnose and test",
        guides: &[Guide {
            label: "diagnostics",
            path: "docs/manual/07_diagnostics.md",
        }],
        commands: &["wtf", "path", "test", "bench"],
    },
    CommandGroup {
        title: "Break things on purpose",
        guides: &[Guide {
            label: "chaos",
            path: "docs/manual/05_chaos.md",
        }],
        commands: &["fault"],
    },
    CommandGroup {
        title: "Security and access",
        guides: &[Guide {
            label: "security and access",
            path: "docs/manual/10_security.md",
        }],
        commands: &["token", "secret", "sign"],
    },
    CommandGroup {
        title: "Images and volumes",
        guides: &[Guide {
            label: "images and volumes",
            path: "docs/manual/11_images-and-volumes.md",
        }],
        commands: &["images", "build", "snapshot"],
    },
    CommandGroup {
        title: "Upgrades",
        guides: &[Guide {
            label: "operations",
            path: "docs/manual/12_operations.md",
        }],
        commands: &["upgrade"],
    },
    CommandGroup {
        title: "Learn",
        guides: &[Guide {
            label: "under the hood",
            path: "docs/manual/06_under-the-hood.md",
        }],
        commands: &["manual", "source"],
    },
    CommandGroup {
        title: "Contributor tools",
        guides: &[Guide {
            label: "dev cluster",
            path: "docs/README.md#dev-cluster",
        }],
        commands: &["dev"],
    },
];

/// Why the command reference couldn't be rendered or placed.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum CommandReferenceError {
    #[error(
        "relish command {0:?} is in no README group; add it to GROUPS in src/relish/command_reference.rs"
    )]
    Ungrouped(String),
    #[error(
        "README group {group:?} lists {command:?}, which relish does not have; remove it from GROUPS"
    )]
    UnknownCommand {
        group: &'static str,
        command: &'static str,
    },
    #[error("the document has no {START_MARKER} ... {END_MARKER} region")]
    MissingMarkers,
}

/// Renders the collapsible command reference for `cli`, grouped by `groups`.
///
/// Hidden commands and clap's built-in `help` are left out. Every other
/// top-level command must appear in exactly one group.
pub fn render(cli: &Command, groups: &[CommandGroup]) -> Result<String, CommandReferenceError> {
    let visible: Vec<&Command> = cli
        .get_subcommands()
        .filter(|command| is_documented(command))
        .collect();

    for command in &visible {
        let name = command.get_name();
        if !groups.iter().any(|group| group.commands.contains(&name)) {
            return Err(CommandReferenceError::Ungrouped(name.to_string()));
        }
    }

    let mut out = String::new();
    out.push_str("<details>\n<summary><strong>Everything relish can do</strong></summary>\n\n");
    out.push_str(
        "Run `relish` with no command for the terminal UI. `relish help COMMAND` \
         (or `--help` anywhere) lists every flag, and the \
         [reference](docs/manual/13_reference.md) covers global flags, \
         environment variables and exit codes. This list is generated from \
         relish's own command definitions; run `",
    );
    out.push_str(REGENERATE_COMMAND);
    out.push_str("` after changing them.\n");

    for group in groups {
        let links: Vec<String> = group
            .guides
            .iter()
            .map(|guide| format!("[{}]({})", guide.label, guide.path))
            .collect();
        // Writing to a String can't fail.
        let _ = write!(out, "\n**{}** ({})\n\n", group.title, links.join(", "));
        for &name in group.commands {
            let command = visible
                .iter()
                .find(|command| command.get_name() == name)
                .ok_or(CommandReferenceError::UnknownCommand {
                    group: group.title,
                    command: name,
                })?;
            render_command(&mut out, command, &format!("relish {name}"), 0);
        }
    }

    out.push_str("\n</details>\n");
    Ok(out)
}

/// Replaces the text between the markers in `document` with `section`.
pub fn replace_region(document: &str, section: &str) -> Result<String, CommandReferenceError> {
    let start = document
        .find(START_MARKER)
        .ok_or(CommandReferenceError::MissingMarkers)?;
    let body_start = start + START_MARKER.len();
    let end = document[body_start..]
        .find(END_MARKER)
        .ok_or(CommandReferenceError::MissingMarkers)?
        + body_start;
    Ok(format!(
        "{}\n{}{}",
        &document[..body_start],
        section,
        &document[end..]
    ))
}

fn is_documented(command: &Command) -> bool {
    !command.is_hide_set() && command.get_name() != "help"
}

fn render_command(out: &mut String, command: &Command, path: &str, depth: usize) {
    let indent = "  ".repeat(depth);
    let about = command
        .get_about()
        .map(|about| about.to_string())
        .unwrap_or_default();
    let synopsis = synopsis(command, path);
    if about.is_empty() {
        let _ = writeln!(out, "{indent}- `{synopsis}`");
    } else {
        let _ = writeln!(out, "{indent}- `{synopsis}`: {about}");
    }
    for sub in command.get_subcommands().filter(|sub| is_documented(sub)) {
        render_command(out, sub, &format!("{path} {}", sub.get_name()), depth + 1);
    }
}

/// The command path followed by its required options and its positionals,
/// in the same shape clap's usage line uses.
fn synopsis(command: &Command, path: &str) -> String {
    let mut parts = vec![path.to_string()];
    for arg in command.get_arguments() {
        if arg.is_hide_set() || arg.is_positional() || !arg.is_required_set() {
            continue;
        }
        let flag = match (arg.get_long(), arg.get_short()) {
            (Some(long), _) => format!("--{long}"),
            (None, Some(short)) => format!("-{short}"),
            (None, None) => continue,
        };
        if arg.get_action().takes_values() {
            parts.push(format!("{flag} <{}>{}", value_name(arg), ellipsis(arg)));
        } else {
            parts.push(flag);
        }
    }
    for arg in command.get_positionals() {
        if arg.is_hide_set() {
            continue;
        }
        let choices: Vec<String> = arg
            .get_possible_values()
            .iter()
            .filter(|value| !value.is_hide_set())
            .map(|value| value.get_name().to_string())
            .collect();
        let name = if choices.is_empty() {
            value_name(arg)
        } else {
            choices.join("|")
        };
        if arg.is_required_set() {
            parts.push(format!("<{name}>{}", ellipsis(arg)));
        } else {
            parts.push(format!("[{name}]{}", ellipsis(arg)));
        }
    }
    parts.join(" ")
}

fn ellipsis(arg: &Arg) -> &'static str {
    let repeats = matches!(arg.get_action(), ArgAction::Append)
        || arg
            .get_num_args()
            .is_some_and(|range| range.max_values() > 1);
    if repeats { "..." } else { "" }
}

fn value_name(arg: &Arg) -> String {
    arg.get_value_names()
        .and_then(|names| names.first())
        .map(|name| name.to_string())
        .unwrap_or_else(|| arg.get_id().as_str().to_uppercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    const TEST_GROUPS: &[CommandGroup] = &[
        CommandGroup {
            title: "Apps",
            guides: &[Guide {
                label: "deploy an app",
                path: "docs/manual/01_deploy-an-app.md",
            }],
            commands: &["status", "exec"],
        },
        CommandGroup {
            title: "Chaos",
            guides: &[Guide {
                label: "chaos",
                path: "docs/manual/05_chaos.md",
            }],
            commands: &["fault"],
        },
    ];

    fn cli() -> Command {
        Command::new("relish")
            .subcommand(Command::new("status").about("Show cluster and app status."))
            .subcommand(
                Command::new("exec")
                    .about("Execute a command inside a running container.")
                    .arg(Arg::new("app").required(true))
                    .arg(Arg::new("command").action(ArgAction::Append))
                    .arg(Arg::new("namespace").long("namespace")),
            )
            .subcommand(
                Command::new("fault")
                    .about("Inject faults.")
                    .subcommand(
                        Command::new("kill")
                            .about("Kill instances.")
                            .arg(Arg::new("target").required(true))
                            .arg(
                                Arg::new("acknowledge")
                                    .long("acknowledge")
                                    .required(true)
                                    .action(ArgAction::SetTrue),
                            )
                            .arg(
                                Arg::new("node")
                                    .long("node")
                                    .required(true)
                                    .value_name("NODE_ID")
                                    .action(ArgAction::Append),
                            ),
                    )
                    .subcommand(
                        Command::new("mode")
                            .about("Pick a mode.")
                            .arg(Arg::new("mode").value_parser(["fast", "slow"])),
                    ),
            )
            .subcommand(Command::new("__internal").hide(true))
    }

    #[test]
    fn each_group_is_headed_and_links_to_its_manual_chapter() {
        let out = render(&cli(), TEST_GROUPS).unwrap();
        assert!(out.contains("\n**Apps** ([deploy an app](docs/manual/01_deploy-an-app.md))\n"));
        assert!(out.contains("\n**Chaos** ([chaos](docs/manual/05_chaos.md))\n"));
        assert!(out.find("**Apps**").unwrap() < out.find("**Chaos**").unwrap());
    }

    #[test]
    fn the_section_collapses_behind_a_summary() {
        let out = render(&cli(), TEST_GROUPS).unwrap();
        assert!(out.starts_with("<details>\n<summary>"));
        assert!(out.ends_with("</details>\n"));
        assert!(out.contains(REGENERATE_COMMAND));
    }

    #[test]
    fn commands_show_their_about_text_in_group_order() {
        let out = render(&cli(), TEST_GROUPS).unwrap();
        let status = out
            .find("- `relish status`: Show cluster and app status.")
            .unwrap();
        let exec = out.find("- `relish exec").unwrap();
        assert!(status < exec);
    }

    #[test]
    fn positionals_show_required_optional_and_repeated_shapes() {
        let out = render(&cli(), TEST_GROUPS).unwrap();
        assert!(out.contains(
            "- `relish exec <APP> [COMMAND]...`: Execute a command inside a running container."
        ));
    }

    #[test]
    fn subcommands_nest_under_their_parent_with_required_and_repeated_options() {
        let out = render(&cli(), TEST_GROUPS).unwrap();
        assert!(out.contains(
            "- `relish fault`: Inject faults.\n  - `relish fault kill --acknowledge --node <NODE_ID>... <TARGET>`: Kill instances.\n"
        ));
    }

    #[test]
    fn positional_choices_are_listed_inline() {
        let out = render(&cli(), TEST_GROUPS).unwrap();
        assert!(out.contains("- `relish fault mode [fast|slow]`: Pick a mode."));
    }

    #[test]
    fn hidden_commands_and_help_are_left_out() {
        let with_help = cli().subcommand(Command::new("help").about("Print help."));
        let out = render(&with_help, TEST_GROUPS).unwrap();
        assert!(!out.contains("__internal"));
        assert!(!out.contains("- `relish help"));
    }

    #[test]
    fn a_command_in_no_group_is_refused_by_name() {
        let extra = cli().subcommand(Command::new("wtf").about("Diagnose."));
        assert_eq!(
            render(&extra, TEST_GROUPS),
            Err(CommandReferenceError::Ungrouped("wtf".to_string()))
        );
    }

    #[test]
    fn a_group_naming_a_missing_command_is_refused() {
        let groups = &[CommandGroup {
            title: "Apps",
            guides: &[],
            commands: &["status", "exec", "fault", "gone"],
        }];
        assert_eq!(
            render(&cli(), groups),
            Err(CommandReferenceError::UnknownCommand {
                group: "Apps",
                command: "gone",
            })
        );
    }

    #[test]
    fn replacing_the_region_keeps_everything_outside_the_markers() {
        let document = format!("intro\n{START_MARKER}\nold\n{END_MARKER}\noutro\n");
        let updated = replace_region(&document, "new\n").unwrap();
        assert_eq!(
            updated,
            format!("intro\n{START_MARKER}\nnew\n{END_MARKER}\noutro\n")
        );
        assert_eq!(replace_region(&updated, "new\n").unwrap(), updated);
    }

    #[test]
    fn a_document_without_markers_is_refused() {
        assert_eq!(
            replace_region("no markers here", "new\n"),
            Err(CommandReferenceError::MissingMarkers)
        );
        assert_eq!(
            replace_region(&format!("{START_MARKER} but no end"), "new\n"),
            Err(CommandReferenceError::MissingMarkers)
        );
    }
}
