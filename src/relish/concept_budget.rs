//! The concept budget: how much an agent has to learn before it can drive
//! Reliaburger (#1681).
//!
//! Three things make up the platform's surface: the resource kinds
//! `relish apply` accepts, the `relish` commands and the flags on them. This
//! module counts them from the real definitions (serde's field list for
//! [`crate::config::Config`] and clap's command tree) and compares the
//! counts with [`BUDGET_FILE`]. Any difference fails: growth needs a
//! deliberate budget edit with a reason, and shrinkage asks for the budget to
//! come down, so it only ever ratchets towards what we ship.

use std::collections::BTreeSet;

use clap::{ArgAction, Command};
use serde::de::{self, DeserializeOwned, Deserializer, Visitor};
use serde::{Deserialize, Serialize};

/// The checked-in budget, relative to the repository root.
pub const BUDGET_FILE: &str = "docs/testing/concept-budget.json";

/// What one command tree adds to the concept count.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommandCounts {
    /// Subcommands at every depth, hidden ones included; the root and clap's
    /// generated `help` are left out.
    pub commands: usize,
    /// How many of `commands` are hidden, themselves or through a parent.
    pub hidden_commands: usize,
    /// Named arguments (with a long or short name) on every command. A
    /// global flag counts once, on the command that declares it.
    pub flags: usize,
    /// Distinct flag names across the tree: `--namespace` on thirty
    /// commands counts once.
    pub distinct_flags: usize,
}

/// Count the commands and flags under `root`.
///
/// Pass the tree straight from `CommandFactory::command()`, before clap
/// builds it: building copies every global flag into every subcommand and
/// adds the generated `help` command and `--help`/`--version` flags.
pub fn count_commands(root: &Command) -> CommandCounts {
    let mut counts = CommandCounts {
        commands: 0,
        hidden_commands: 0,
        flags: 0,
        distinct_flags: 0,
    };
    let mut names = BTreeSet::new();
    walk(root, false, &mut counts, &mut names);
    counts.distinct_flags = names.len();
    counts
}

fn walk(command: &Command, hidden: bool, counts: &mut CommandCounts, names: &mut BTreeSet<String>) {
    for arg in command.get_arguments() {
        if arg.is_positional() || is_generated(arg.get_action()) {
            continue;
        }
        counts.flags += 1;
        if let Some(name) = flag_name(arg) {
            names.insert(name);
        }
    }
    for sub in command.get_subcommands() {
        if sub.get_name() == "help" {
            continue;
        }
        let sub_hidden = hidden || sub.is_hide_set();
        counts.commands += 1;
        if sub_hidden {
            counts.hidden_commands += 1;
        }
        walk(sub, sub_hidden, counts, names);
    }
}

fn is_generated(action: &ArgAction) -> bool {
    matches!(
        action,
        ArgAction::Help | ArgAction::HelpShort | ArgAction::HelpLong | ArgAction::Version
    )
}

fn flag_name(arg: &clap::Arg) -> Option<String> {
    match (arg.get_long(), arg.get_short()) {
        (Some(long), _) => Some(format!("--{long}")),
        (None, Some(short)) => Some(format!("-{short}")),
        (None, None) => None,
    }
}

/// The top-level field names of a struct that derives `Deserialize`, in
/// declaration order and under their serde names (`use`, not `uses`).
///
/// `None` when `T` doesn't deserialise as a struct. Serde's derive hands the
/// field list to [`Deserializer::deserialize_struct`]; a deserialiser that
/// records it and refuses everything else reads it without any input.
pub fn struct_field_names<T: DeserializeOwned>() -> Option<&'static [&'static str]> {
    let mut probe = FieldProbe { fields: None };
    // The probe always answers with an error once it has the names.
    let _ = T::deserialize(&mut probe);
    probe.fields
}

struct FieldProbe {
    fields: Option<&'static [&'static str]>,
}

impl<'de> Deserializer<'de> for &mut FieldProbe {
    type Error = de::value::Error;

    fn deserialize_any<V: Visitor<'de>>(self, _visitor: V) -> Result<V::Value, Self::Error> {
        Err(de::Error::custom("not a struct"))
    }

    fn deserialize_struct<V: Visitor<'de>>(
        self,
        _name: &'static str,
        fields: &'static [&'static str],
        _visitor: V,
    ) -> Result<V::Value, Self::Error> {
        self.fields = Some(fields);
        Err(de::Error::custom("field names recorded"))
    }

    serde::forward_to_deserialize_any! {
        bool i8 i16 i32 i64 i128 u8 u16 u32 u64 u128 f32 f64 char str string
        bytes byte_buf option unit unit_struct newtype_struct seq tuple
        tuple_struct map enum identifier ignored_any
    }
}

/// Every count the budget tracks, measured from this build.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConceptCounts {
    /// Top-level tables `relish apply` accepts (`[app.*]`, `[job.*]`, ...).
    pub resource_kinds: usize,
    /// The command tree's counts.
    pub commands: CommandCounts,
}

/// One tracked count's budget and why it is that number.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BudgetEntry {
    /// The count this build must match.
    pub budget: usize,
    /// Why the budget is this value: what the last raise or cut bought.
    pub reason: String,
}

/// The checked-in budget, one entry per tracked count.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConceptBudget {
    /// Resource kinds `relish apply` accepts.
    pub resource_kinds: BudgetEntry,
    /// `relish` commands and subcommands, hidden ones included.
    pub commands: BudgetEntry,
    /// Flags on every command, counted per command.
    pub flags: BudgetEntry,
    /// Distinct flag names.
    pub distinct_flags: BudgetEntry,
}

impl ConceptBudget {
    /// Parse the budget file's JSON.
    pub fn parse(json: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(json)
    }

    /// Compare `measured` with the budget. Empty when they match; otherwise
    /// one line per count that moved or lacks a reason, saying what to do.
    pub fn compare(&self, measured: &ConceptCounts) -> Vec<String> {
        let tracked = [
            (
                "resource_kinds",
                measured.resource_kinds,
                &self.resource_kinds,
            ),
            ("commands", measured.commands.commands, &self.commands),
            ("flags", measured.commands.flags, &self.flags),
            (
                "distinct_flags",
                measured.commands.distinct_flags,
                &self.distinct_flags,
            ),
        ];
        let mut problems = Vec::new();
        for (name, count, entry) in tracked {
            if entry.reason.trim().is_empty() {
                problems.push(format!(
                    "{name}: `reason` is empty in {BUDGET_FILE}; say why the budget is {}",
                    entry.budget
                ));
            }
            if count > entry.budget {
                problems.push(format!(
                    "{name}: {count} is over its budget of {}. Every new concept is something \
                     an agent has to learn. If the growth is deliberate, raise `{name}.budget` \
                     to {count} in {BUDGET_FILE} in the same pull request and say why in \
                     `{name}.reason`; otherwise fold it into something that exists",
                    entry.budget
                ));
            } else if count < entry.budget {
                problems.push(format!(
                    "{name}: {count} is under its budget of {}. Lower `{name}.budget` to {count} \
                     in {BUDGET_FILE} and update `{name}.reason`, so the budget ratchets down",
                    entry.budget
                ));
            }
        }
        problems
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Arg;

    fn tree() -> Command {
        Command::new("tool")
            .arg(Arg::new("token").long("token").global(true))
            .subcommand(
                Command::new("get")
                    .arg(Arg::new("name"))
                    .arg(Arg::new("namespace").long("namespace").short('n'))
                    .arg(Arg::new("watch").short('w').action(ArgAction::SetTrue)),
            )
            .subcommand(
                Command::new("ca")
                    .subcommand(
                        Command::new("rotate")
                            .arg(Arg::new("namespace").long("namespace"))
                            .arg(Arg::new("force").long("force").hide(true)),
                    )
                    .subcommand(Command::new("backup")),
            )
            .subcommand(
                Command::new("internal")
                    .hide(true)
                    .subcommand(Command::new("probe")),
            )
    }

    fn entry(budget: usize) -> BudgetEntry {
        BudgetEntry {
            budget,
            reason: "today's surface".to_string(),
        }
    }

    fn budget(kinds: usize, commands: usize, flags: usize, distinct: usize) -> ConceptBudget {
        ConceptBudget {
            resource_kinds: entry(kinds),
            commands: entry(commands),
            flags: entry(flags),
            distinct_flags: entry(distinct),
        }
    }

    fn measured() -> ConceptCounts {
        ConceptCounts {
            resource_kinds: 3,
            commands: count_commands(&tree()),
        }
    }

    #[test]
    fn nested_and_hidden_commands_count_and_the_root_does_not() {
        let counts = count_commands(&tree());
        assert_eq!(counts.commands, 6);
        assert_eq!(counts.hidden_commands, 2, "internal and its child");
    }

    #[test]
    fn hidden_flags_count_and_positionals_do_not() {
        // --token, --namespace, -w, --namespace, --force.
        assert_eq!(count_commands(&tree()).flags, 5);
    }

    #[test]
    fn a_flag_repeated_across_commands_is_one_distinct_name() {
        // --token, --namespace, -w, --force.
        assert_eq!(count_commands(&tree()).distinct_flags, 4);
    }

    #[test]
    fn a_built_tree_adds_help_that_does_not_count() {
        let mut built = Command::new("tool")
            .version("1")
            .subcommand(Command::new("get"));
        built.build();
        assert!(built.get_subcommands().any(|sub| sub.get_name() == "help"));
        let counts = count_commands(&built);
        assert_eq!(counts.commands, 1);
        assert_eq!(counts.flags, 0, "--help and --version are clap's");
    }

    #[test]
    fn struct_field_names_follow_serde_renames() {
        #[derive(Deserialize)]
        #[allow(dead_code)]
        struct Tables {
            app: u8,
            #[serde(rename = "use")]
            uses: u8,
        }
        assert_eq!(struct_field_names::<Tables>(), Some(&["app", "use"][..]));
        assert_eq!(struct_field_names::<String>(), None);
    }

    #[test]
    fn a_count_equal_to_its_budget_passes() {
        assert!(budget(3, 6, 5, 4).compare(&measured()).is_empty());
    }

    #[test]
    fn growth_fails_and_says_how_to_raise_the_budget_deliberately() {
        let problems = budget(3, 5, 5, 4).compare(&measured());
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].starts_with("commands: 6 is over its budget of 5"));
        assert!(problems[0].contains("raise `commands.budget` to 6"));
        assert!(problems[0].contains(BUDGET_FILE));
        assert!(problems[0].contains("same pull request"));
        assert!(problems[0].contains("`commands.reason`"));
    }

    #[test]
    fn shrinkage_fails_until_the_budget_comes_down() {
        let problems = budget(4, 6, 5, 4).compare(&measured());
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].starts_with("resource_kinds: 3 is under its budget of 4"));
        assert!(problems[0].contains("Lower `resource_kinds.budget` to 3"));
    }

    #[test]
    fn a_budget_without_a_reason_fails() {
        let mut without = budget(3, 6, 5, 4);
        without.flags.reason = "  ".to_string();
        let problems = without.compare(&measured());
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].starts_with("flags: `reason` is empty"));
    }

    #[test]
    fn the_budget_file_refuses_unknown_and_missing_counts() {
        let entry = r#"{"budget": 1, "reason": "r"}"#;
        let full = format!(
            r#"{{"resource_kinds": {entry}, "commands": {entry}, "flags": {entry}, "distinct_flags": {entry}}}"#
        );
        assert!(ConceptBudget::parse(&full).is_ok());
        let missing = format!(r#"{{"resource_kinds": {entry}}}"#);
        assert!(ConceptBudget::parse(&missing).is_err());
        let extra = full.replacen('{', &format!(r#"{{"fields": {entry}, "#), 1);
        assert!(ConceptBudget::parse(&extra).is_err());
    }
}
