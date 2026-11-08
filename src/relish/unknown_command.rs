//! What `relish` says about a command it doesn't know.
//!
//! clap answers an unknown subcommand with "a similar subcommand exists" and
//! picks the closest name, whatever that command does. For `relish apps` the
//! closest is `apply`, so a read-only agent's natural guess was pointed
//! straight at a write (#1093). Here we keep clap's suggestions only when
//! they read, and otherwise point at the command to start from.
//!
//! The rule is the same for a human and an agent, because relish can't tell
//! them apart: an agent may run the plain CLI and read the same stderr. A
//! typo of a mutating command (`relish aply`) therefore gets no suggestion,
//! just the pointer to `--help`.

use clap::error::{ContextKind, ContextValue, ErrorKind};

/// Top-level commands that only read: neither the command nor any of its
/// subcommands changes the cluster or the files on this machine. A command
/// missing from here is never suggested, so a new one is safe by default.
pub const READ_ONLY_COMMANDS: &[&str] = &[
    "audit",
    "batch-status",
    "compile",
    "diff",
    "events",
    "help",
    "history",
    "inspect",
    "jobs",
    "lint",
    "logs",
    "logs-search",
    "manual",
    "metrics",
    "nodes",
    "path",
    "resolve",
    "routes",
    "status",
    "timeline",
    "top",
    "wtf",
    "agent",
];

/// `relish agent` commands that only read. `report`, `patch` and `setup`
/// post, build or write files, so they are left out.
pub const READ_ONLY_AGENT_COMMANDS: &[&str] = &[
    "context",
    "diagnose",
    "explain",
    "guide",
    "help",
    "logs",
    "manual",
    "schema",
    "source",
    "timeline",
    "upgrade-check",
    "version",
];

/// Subcommand names that only read wherever they appear, such as
/// `relish council status` or `relish batch results`.
pub const READ_ONLY_SUBCOMMANDS: &[&str] = &[
    "backups", "grep", "help", "list", "logs", "results", "search", "show", "status", "tree",
    "watch",
];

/// Where in the command tree an unknown subcommand was typed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Level {
    /// `relish <unknown>`.
    Top,
    /// `relish agent <unknown>`.
    Agent,
    /// Under any other command, such as `relish council <unknown>`.
    Nested,
}

impl Level {
    fn read_only(self) -> &'static [&'static str] {
        match self {
            Level::Top => READ_ONLY_COMMANDS,
            Level::Agent => READ_ONLY_AGENT_COMMANDS,
            Level::Nested => READ_ONLY_SUBCOMMANDS,
        }
    }

    /// Where to start when nothing read-only is close to what was typed.
    fn start_here(self) -> Option<&'static str> {
        match self {
            Level::Top => Some("relish status"),
            Level::Agent => Some("relish agent context"),
            Level::Nested => None,
        }
    }
}

/// Rewrite clap's answer to an unknown subcommand so that it never suggests
/// a command that changes anything. `args` are the command line without the
/// program name.
///
/// Returns the lead for a `relish agent` usage hint: "did you mean ...?"
/// with the read-only suggestions, or where to start when there are none.
/// Returns `None`, and leaves `error` alone, for any other error.
pub fn keep_read_only_suggestions(error: &mut clap::Error, args: &[String]) -> Option<String> {
    if error.kind() != ErrorKind::InvalidSubcommand {
        return None;
    }
    let unknown = match error.get(ContextKind::InvalidSubcommand) {
        Some(ContextValue::String(unknown)) => unknown.clone(),
        _ => return None,
    };
    let level = level_of(args, &unknown);
    let parent = parent_command(args, &unknown);
    let suggested: Vec<String> = match error.get(ContextKind::SuggestedSubcommand) {
        Some(ContextValue::Strings(names)) => names.clone(),
        Some(ContextValue::String(name)) => vec![name.clone()],
        _ => Vec::new(),
    };
    let kept: Vec<String> = suggested
        .into_iter()
        .filter(|name| level.read_only().contains(&name.as_str()))
        .collect();

    if kept.is_empty() {
        error.remove(ContextKind::SuggestedSubcommand);
        if let Some(start) = level.start_here() {
            add_tip(
                error,
                &format!("`{start}` is the place to start; `{parent} --help` lists every command"),
            );
            return Some(format!("start with {start};"));
        }
        return None;
    }
    let lead = kept
        .iter()
        .map(|name| format!("{parent} {name}"))
        .collect::<Vec<_>>()
        .join(", ");
    error.insert(
        ContextKind::SuggestedSubcommand,
        ContextValue::Strings(kept),
    );
    Some(format!("did you mean {lead}?"))
}

/// Add a "tip:" line to the error, after any clap already gave.
fn add_tip(error: &mut clap::Error, tip: &str) {
    let mut tips = match error.get(ContextKind::Suggested) {
        Some(ContextValue::StyledStrs(tips)) => tips.clone(),
        _ => Vec::new(),
    };
    tips.push(tip.to_string().into());
    error.insert(ContextKind::Suggested, ContextValue::StyledStrs(tips));
}

/// The words of `args` that are neither flags nor a global flag's value.
fn positional_words(args: &[String]) -> Vec<&str> {
    let mut words = Vec::new();
    let mut skip_value = false;
    for arg in args {
        if skip_value {
            skip_value = false;
            continue;
        }
        if super::agent::VALUE_FLAGS.contains(&arg.as_str()) {
            skip_value = true;
            continue;
        }
        if !arg.starts_with('-') {
            words.push(arg.as_str());
        }
    }
    words
}

fn level_of(args: &[String], unknown: &str) -> Level {
    match positional_words(args).as_slice() {
        [first, ..] if *first == unknown => Level::Top,
        ["agent", second, ..] if *second == unknown => Level::Agent,
        _ => Level::Nested,
    }
}

/// The command the unknown word was typed under, as `relish council`.
fn parent_command(args: &[String], unknown: &str) -> String {
    let words = positional_words(args);
    let before = words
        .iter()
        .position(|word| *word == unknown)
        .unwrap_or(words.len());
    let mut parent = vec!["relish"];
    parent.extend(&words[..before]);
    parent.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cli() -> clap::Command {
        clap::Command::new("relish")
            .subcommand(clap::Command::new("apply"))
            .subcommand(clap::Command::new("status"))
            .subcommand(
                clap::Command::new("agent")
                    .subcommand(clap::Command::new("context"))
                    .subcommand(clap::Command::new("report")),
            )
            .subcommand(
                clap::Command::new("council")
                    .subcommand(clap::Command::new("status"))
                    .subcommand(clap::Command::new("recover")),
            )
    }

    /// Parse `line` against the small CLI and answer the error it gives.
    fn answer(line: &[&str]) -> (String, Option<String>) {
        let mut error = cli().try_get_matches_from(line).unwrap_err();
        let args: Vec<String> = line[1..].iter().map(|arg| arg.to_string()).collect();
        let lead = keep_read_only_suggestions(&mut error, &args);
        (error.render().to_string(), lead)
    }

    #[test]
    fn a_word_closest_to_a_mutating_command_suggests_where_to_start() {
        let (text, lead) = answer(&["relish", "apps"]);
        assert!(!text.contains("'apply'"), "{text}");
        assert!(
            text.contains("`relish status` is the place to start"),
            "{text}"
        );
        assert_eq!(lead.as_deref(), Some("start with relish status;"));
    }

    #[test]
    fn a_typo_of_a_read_only_command_still_suggests_it() {
        let (text, lead) = answer(&["relish", "statsu"]);
        assert!(
            text.contains("a similar subcommand exists: 'status'"),
            "{text}"
        );
        assert_eq!(lead.as_deref(), Some("did you mean relish status?"));
    }

    #[test]
    fn a_typo_of_a_mutating_command_gets_no_suggestion() {
        let (text, _) = answer(&["relish", "aply"]);
        assert!(!text.contains("'apply'"), "{text}");
        assert!(text.contains("--help` lists every command"), "{text}");
    }

    #[test]
    fn under_agent_only_read_only_agent_commands_are_suggested() {
        let (text, lead) = answer(&["relish", "agent", "contxt"]);
        assert!(text.contains("'context'"), "{text}");
        assert_eq!(lead.as_deref(), Some("did you mean relish agent context?"));
        let (text, lead) = answer(&["relish", "agent", "reprt"]);
        assert!(!text.contains("'report'"), "{text}");
        assert_eq!(lead.as_deref(), Some("start with relish agent context;"));
    }

    #[test]
    fn under_another_command_only_read_only_verbs_are_suggested() {
        let (text, _) = answer(&["relish", "council", "statsu"]);
        assert!(text.contains("'status'"), "{text}");
        let (text, lead) = answer(&["relish", "council", "recovr"]);
        assert!(!text.contains("'recover'"), "{text}");
        assert_eq!(lead, None);
    }

    #[test]
    fn other_errors_are_left_alone() {
        let mut error = cli()
            .try_get_matches_from(["relish", "status", "--bogus"])
            .unwrap_err();
        let before = error.render().to_string();
        assert_eq!(keep_read_only_suggestions(&mut error, &[]), None);
        assert_eq!(error.render().to_string(), before);
    }
}
