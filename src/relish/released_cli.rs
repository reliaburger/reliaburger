//! What the published relish accepts, so the docs can't promise more.
//!
//! The installer fetches a fixed release (`docs/website/install.sh`), but the
//! site, the README and the five-minute tour are written on the development
//! branch, next to commands that release doesn't have yet. A visitor who
//! types one of those gets an unknown-subcommand error. To catch that, we keep
//! a snapshot of the release's command line in
//! `scripts/release/released-cli.tsv`, written from the published binary's
//! own help by `scripts/release/released_cli.py`, and check every advertised
//! `relish …` line against it.
//!
//! The check is a small model of clap's parser: subcommands, positionals,
//! options with their aliases and the values clap lists for them. It's
//! deliberately lenient about placeholders (`<FILE>`, `NODE`), because docs
//! write those where a person types their own value.

use std::collections::{HashMap, HashSet};

/// The command line of one published relish release.
#[derive(Debug, Clone)]
pub struct ReleasedCli {
    version: String,
    commands: HashSet<String>,
    arguments: HashMap<String, Vec<String>>,
    options: HashMap<String, Vec<ReleasedOption>>,
}

/// One option of one command, under every alias it has.
#[derive(Debug, Clone)]
struct ReleasedOption {
    aliases: Vec<String>,
    takes_value: bool,
    choices: Vec<String>,
}

/// Why a snapshot file couldn't be read.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ReleasedCliError {
    #[error("line {line} of the released-CLI snapshot is malformed: {text:?}")]
    Malformed { line: usize, text: String },
    #[error("the released-CLI snapshot has no version line")]
    MissingVersion,
}

/// Why the published release wouldn't accept a command line.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum Rejection {
    #[error("not a relish command line")]
    NotRelish,
    #[error("{version} has no command `{command}`")]
    UnknownCommand { version: String, command: String },
    #[error("{version}'s `{command}` has no option {option}")]
    UnknownOption {
        version: String,
        command: String,
        option: String,
    },
    #[error("{version}'s `{command} {option}` doesn't take {value:?}")]
    UnknownValue {
        version: String,
        command: String,
        option: String,
        value: String,
    },
    #[error("{version}'s `{command}` takes no argument {argument:?}")]
    UnexpectedArgument {
        version: String,
        command: String,
        argument: String,
    },
}

const ROOT: &str = "relish";

impl ReleasedCli {
    /// Reads the tab-separated snapshot `released_cli.py` writes.
    pub fn parse(text: &str) -> Result<Self, ReleasedCliError> {
        let mut version = None;
        let mut commands = HashSet::new();
        let mut arguments: HashMap<String, Vec<String>> = HashMap::new();
        let mut options: HashMap<String, Vec<ReleasedOption>> = HashMap::new();
        for (index, line) in text.lines().enumerate() {
            if line.trim().is_empty() || line.starts_with('#') {
                continue;
            }
            let fields: Vec<&str> = line.split('\t').collect();
            match fields.as_slice() {
                ["version", value] => version = Some(value.to_string()),
                ["command", path] => {
                    commands.insert(path.to_string());
                }
                ["argument", path, name] => arguments
                    .entry(path.to_string())
                    .or_default()
                    .push(name.to_string()),
                ["option", path, aliases, value, choices] => options
                    .entry(path.to_string())
                    .or_default()
                    .push(ReleasedOption {
                        aliases: aliases.split(',').map(str::to_string).collect(),
                        takes_value: !value.is_empty(),
                        choices: choices
                            .split(',')
                            .filter(|choice| !choice.is_empty())
                            .map(str::to_string)
                            .collect(),
                    }),
                _ => {
                    return Err(ReleasedCliError::Malformed {
                        line: index + 1,
                        text: line.to_string(),
                    });
                }
            }
        }
        Ok(Self {
            version: version.ok_or(ReleasedCliError::MissingVersion)?,
            commands,
            arguments,
            options,
        })
    }

    /// The release's tag, such as `v0.1.6`.
    pub fn version(&self) -> &str {
        &self.version
    }

    /// How docs label a command this release lacks: `0.2.0, not in 0.1.6`.
    pub fn unreleased_label(&self, upcoming_version: &str) -> String {
        format!(
            "{upcoming_version}, not in {}",
            self.version.trim_start_matches('v')
        )
    }

    /// Whether this release would accept `line`, a `relish …` command as the
    /// docs show it. Placeholders such as `<FILE>`, `[NODE]` or `NAME` stand
    /// for any value.
    pub fn check(&self, line: &str) -> Result<(), Rejection> {
        let words = split_words(line);
        let Some((first, rest)) = words.split_first() else {
            return Err(Rejection::NotRelish);
        };
        if first != ROOT {
            return Err(Rejection::NotRelish);
        }
        let mut path = ROOT.to_string();
        let mut positionals = 0;
        let mut index = 0;
        while index < rest.len() {
            let word = &rest[index];
            index += 1;
            if word == "--" {
                break;
            }
            if word.starts_with('-') && word.len() > 1 {
                let (flag, inline) = match word.split_once('=') {
                    Some((flag, value)) => (flag, Some(value.to_string())),
                    None => (word.as_str(), None),
                };
                let option = self.option(&path, flag)?;
                if !option.takes_value {
                    continue;
                }
                let value = match inline {
                    Some(value) => Some(value),
                    None => {
                        index += 1;
                        rest.get(index - 1).cloned()
                    }
                };
                if let Some(value) = value {
                    self.check_value(&path, flag, option, &value)?;
                }
                continue;
            }
            let child = format!("{path} {word}");
            if positionals == 0 && self.commands.contains(&child) {
                path = child;
                continue;
            }
            self.check_positional(&path, word, positionals)?;
            positionals += 1;
        }
        Ok(())
    }

    fn option(&self, path: &str, flag: &str) -> Result<&ReleasedOption, Rejection> {
        [path, ROOT]
            .iter()
            .filter_map(|owner| self.options.get(*owner))
            .flatten()
            .find(|option| option.aliases.iter().any(|alias| alias == flag))
            .ok_or_else(|| Rejection::UnknownOption {
                version: self.version.clone(),
                command: path.to_string(),
                option: flag.to_string(),
            })
    }

    fn check_value(
        &self,
        path: &str,
        flag: &str,
        option: &ReleasedOption,
        value: &str,
    ) -> Result<(), Rejection> {
        if option.choices.is_empty()
            || is_placeholder(value)
            || option.choices.iter().any(|choice| choice == value)
        {
            return Ok(());
        }
        Err(Rejection::UnknownValue {
            version: self.version.clone(),
            command: path.to_string(),
            option: flag.to_string(),
            value: value.to_string(),
        })
    }

    fn check_positional(&self, path: &str, word: &str, taken: usize) -> Result<(), Rejection> {
        let declared = self.arguments.get(path).map(Vec::as_slice).unwrap_or(&[]);
        let has_children = self
            .commands
            .iter()
            .any(|command| command.starts_with(&format!("{path} ")));
        if declared.is_empty() && has_children {
            if is_placeholder(word) {
                return Ok(());
            }
            return Err(Rejection::UnknownCommand {
                version: self.version.clone(),
                command: format!("{path} {word}"),
            });
        }
        let fits = taken < declared.len()
            || declared
                .last()
                .is_some_and(|argument| argument.ends_with("..."));
        if fits {
            return Ok(());
        }
        Err(Rejection::UnexpectedArgument {
            version: self.version.clone(),
            command: path.to_string(),
            argument: word.to_string(),
        })
    }
}

/// `<FILE>`, `[NODE]`, `<a|b>` or an upper-case word like `NAME` or `NODE_ID`.
fn is_placeholder(word: &str) -> bool {
    word.starts_with('<')
        || word.starts_with('[')
        || (word.chars().any(|c| c.is_ascii_uppercase())
            && word
                .chars()
                .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_'))
}

/// Splits a command line into words the way a shell would for the simple
/// lines docs show: whitespace separates, single or double quotes group.
fn split_words(line: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut in_word = false;
    let mut quote = None;
    for character in line.chars() {
        match (quote, character) {
            (Some(open), c) if c == open => quote = None,
            (Some(_), c) => current.push(c),
            (None, '\'' | '"') => {
                quote = Some(character);
                in_word = true;
            }
            (None, c) if c.is_whitespace() => {
                if in_word {
                    words.push(std::mem::take(&mut current));
                    in_word = false;
                }
            }
            (None, c) => {
                current.push(c);
                in_word = true;
            }
        }
    }
    if in_word {
        words.push(current);
    }
    words
}

#[cfg(test)]
mod tests {
    use super::*;

    const SNAPSHOT: &str = "\
# a comment
version\tv0.1.6
command\trelish
command\trelish apply
command\trelish bench
command\trelish fault
command\trelish fault delay
command\trelish help
command\trelish manual
command\trelish manual examples
command\trelish batch
argument\trelish apply\t[PATH_OR_URL]
argument\trelish batch\t<PATH>
argument\trelish fault delay\t<TARGET>
argument\trelish fault delay\t<DELAY>
argument\trelish help\t[COMMAND]...
argument\trelish manual\t[CHAPTER]
option\trelish\t--output\t<OUTPUT>\thuman,json,yaml
option\trelish apply\t-f,--file\t<PATH_OR_URL>\t
option\trelish apply\t--dry-run\t\t
option\trelish bench\t--quick\t\t
option\trelish fault delay\t--duration\t<DURATION>\t
";

    fn released() -> ReleasedCli {
        ReleasedCli::parse(SNAPSHOT).unwrap()
    }

    #[test]
    fn the_version_line_names_the_release() {
        assert_eq!(released().version(), "v0.1.6");
        assert_eq!(released().unreleased_label("0.2.0"), "0.2.0, not in 0.1.6");
    }

    #[test]
    fn released_commands_with_options_and_arguments_are_accepted() {
        let cli = released();
        for line in [
            "relish apply -f https://reliaburger.com/demo/podinfo.yaml",
            "relish apply --file=x.yaml --dry-run",
            "relish fault delay redis 300ms --duration 2m",
            "relish --output json apply x.toml",
            "relish apply x.toml --output yaml",
            "relish manual tour",
            "relish manual examples",
            "relish help COMMAND",
            "relish bench --quick",
        ] {
            assert_eq!(cli.check(line), Ok(()), "{line}");
        }
    }

    #[test]
    fn placeholders_stand_for_any_value() {
        let cli = released();
        assert_eq!(cli.check("relish apply <PATH_OR_URL>"), Ok(()));
        assert_eq!(cli.check("relish --output <OUTPUT> apply"), Ok(()));
        assert_eq!(cli.check("relish fault <KIND>"), Ok(()));
    }

    #[test]
    fn a_command_the_release_lacks_is_rejected_by_name() {
        assert_eq!(
            released().check("relish jobs"),
            Err(Rejection::UnknownCommand {
                version: "v0.1.6".to_string(),
                command: "relish jobs".to_string(),
            })
        );
        assert!(matches!(
            released().check("relish fault frobnicate redis"),
            Err(Rejection::UnknownCommand { .. })
        ));
    }

    #[test]
    fn an_option_the_release_lacks_is_rejected() {
        assert_eq!(
            released().check("relish bench --scenario jobs-shared-containers"),
            Err(Rejection::UnknownOption {
                version: "v0.1.6".to_string(),
                command: "relish bench".to_string(),
                option: "--scenario".to_string(),
            })
        );
    }

    #[test]
    fn a_value_outside_the_listed_choices_is_rejected() {
        assert!(matches!(
            released().check("relish --output toml status"),
            Err(Rejection::UnknownValue { .. })
        ));
    }

    #[test]
    fn a_subcommand_where_the_release_takes_one_path_is_an_extra_argument() {
        // 0.1.6's `batch` takes a manifest path and has no `submit`.
        assert_eq!(
            released().check("relish batch submit jobs.toml"),
            Err(Rejection::UnexpectedArgument {
                version: "v0.1.6".to_string(),
                command: "relish batch".to_string(),
                argument: "jobs.toml".to_string(),
            })
        );
    }

    #[test]
    fn quoted_arguments_are_one_word() {
        assert_eq!(released().check("relish apply 'my file.toml'"), Ok(()));
    }

    #[test]
    fn a_line_that_isnt_relish_is_refused() {
        assert_eq!(released().check("curl http://x"), Err(Rejection::NotRelish));
        assert_eq!(released().check(""), Err(Rejection::NotRelish));
    }

    #[test]
    fn a_malformed_or_versionless_snapshot_is_refused() {
        assert_eq!(
            ReleasedCli::parse("version\tv1\nnonsense\n").unwrap_err(),
            ReleasedCliError::Malformed {
                line: 2,
                text: "nonsense".to_string()
            }
        );
        assert_eq!(
            ReleasedCli::parse("command\trelish\n").unwrap_err(),
            ReleasedCliError::MissingVersion
        );
    }
}
