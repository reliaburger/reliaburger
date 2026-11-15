//! The agents `relish agent setup` knows: where each keeps its MCP servers,
//! and the entry it gets.
//!
//! Every path is the agent's user-level config, under the home directory, so
//! one setup covers every project. Every entry starts the same stdio server,
//! [`MCP_COMMAND_LINE`], and says nothing else: no auto-approval, no trust
//! flag, no tool allow-list. Whether a tool call needs a yes stays the
//! agent's own default.

use std::path::{Path, PathBuf};

use schemars::JsonSchema;
use serde::Serialize;
use serde_json::{Value, json};

use super::edit::Format;

/// The name every agent knows the server by.
pub const MCP_SERVER_NAME: &str = "reliaburger";

/// The command every agent runs to start the server: `relish mcp`, the
/// stdio server #1094 adds. It's a bare `relish`, found on `PATH`, so an
/// upgrade that moves the binary doesn't break it.
pub const MCP_COMMAND_LINE: &[&str] = &["relish", "mcp"];

/// One line for the agents whose entries carry a description.
pub const MCP_DESCRIPTION: &str = "Reliaburger: read-only cluster status, diagnosis and logs";

/// The same line for a proposing entry, `relish agent setup --propose`.
pub const MCP_PROPOSE_DESCRIPTION: &str =
    "Reliaburger: cluster status, diagnosis and logs, and changes proposed for a person to approve";

/// What an entry's `relish mcp` offers: the read-only tools, or those and
/// the proposing ones (#1682).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mode {
    /// `relish mcp`: read-only.
    Read,
    /// `relish mcp --propose --namespace NAME`: it may also propose
    /// changes in `namespace`, each one approved by a person or a policy.
    Propose {
        /// The one namespace its deployer agent token is confined to.
        namespace: String,
    },
}

impl Mode {
    /// The arguments after `relish`.
    pub fn args(&self) -> Vec<String> {
        let mut args: Vec<String> = command_and_args()
            .1
            .iter()
            .map(|word| word.to_string())
            .collect();
        if let Mode::Propose { namespace } = self {
            args.extend([
                "--propose".to_string(),
                "--namespace".to_string(),
                namespace.clone(),
            ]);
        }
        args
    }

    /// The whole command line, as setup reports it.
    pub fn command_line(&self) -> Vec<String> {
        let mut line = vec![command_and_args().0.to_string()];
        line.extend(self.args());
        line
    }

    /// The mode whose arguments are `args`, if they're one of setup's.
    fn of_args(args: &Value) -> Option<Mode> {
        let words: Vec<&str> = args
            .as_array()?
            .iter()
            .map(Value::as_str)
            .collect::<Option<_>>()?;
        match words.as_slice() {
            ["mcp"] => Some(Mode::Read),
            ["mcp", "--propose", "--namespace", namespace] => Some(Mode::Propose {
                namespace: namespace.to_string(),
            }),
            _ => None,
        }
    }

    fn description(&self) -> &'static str {
        match self {
            Mode::Read => MCP_DESCRIPTION,
            Mode::Propose { .. } => MCP_PROPOSE_DESCRIPTION,
        }
    }
}

/// An agent `relish agent setup` can wire up.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, JsonSchema, clap::ValueEnum)]
#[serde(rename_all = "kebab-case")]
pub enum Client {
    /// Anthropic's Claude Code.
    ClaudeCode,
    /// OpenAI's Codex CLI.
    Codex,
    /// The Cursor editor.
    Cursor,
    /// Visual Studio Code.
    #[value(name = "vscode")]
    #[serde(rename = "vscode")]
    VsCode,
    /// Block's Goose.
    Goose,
    /// Google's Gemini CLI.
    #[value(name = "gemini")]
    #[serde(rename = "gemini")]
    Gemini,
}

/// Where a home directory's agent configs live.
#[derive(Debug, Clone)]
pub struct Home {
    /// The home directory.
    pub root: PathBuf,
    /// Whether this is macOS, where VS Code keeps its settings under
    /// `Library/Application Support` rather than `.config`.
    pub macos: bool,
}

impl Home {
    /// This user's home, or `None` when it can't be found.
    pub fn current() -> Option<Home> {
        Some(Home {
            root: dirs::home_dir()?,
            macos: cfg!(target_os = "macos"),
        })
    }

    fn join(&self, path: &str) -> PathBuf {
        self.root.join(path)
    }

    fn vscode_user(&self) -> PathBuf {
        if self.macos {
            self.join("Library/Application Support/Code/User")
        } else {
            self.join(".config/Code/User")
        }
    }
}

/// An agent's MCP config: the file, its format, and the key its servers
/// live under.
#[derive(Debug, Clone)]
pub struct ConfigFile {
    /// The file.
    pub path: PathBuf,
    /// Its format.
    pub format: Format,
    /// The top-level key whose map holds the servers.
    pub parent: &'static str,
}

impl Client {
    /// Every agent, in the order setup reports them.
    pub const ALL: &'static [Client] = &[
        Client::ClaudeCode,
        Client::Codex,
        Client::Cursor,
        Client::VsCode,
        Client::Goose,
        Client::Gemini,
    ];

    /// The name a caller types: `claude-code`, `vscode`, …
    pub fn as_str(self) -> &'static str {
        match self {
            Client::ClaudeCode => "claude-code",
            Client::Codex => "codex",
            Client::Cursor => "cursor",
            Client::VsCode => "vscode",
            Client::Goose => "goose",
            Client::Gemini => "gemini",
        }
    }

    /// Whether this agent looks installed: its config directory, or Claude
    /// Code's `~/.claude.json`, exists.
    pub fn detected(self, home: &Home) -> bool {
        let markers = match self {
            Client::ClaudeCode => vec![home.join(".claude"), home.join(".claude.json")],
            Client::Codex => vec![home.join(".codex")],
            Client::Cursor => vec![home.join(".cursor")],
            Client::VsCode => vec![home.vscode_user()],
            Client::Goose => vec![home.join(".config/goose")],
            Client::Gemini => vec![home.join(".gemini")],
        };
        markers.iter().any(|marker| marker.exists())
    }

    /// The file this agent reads its user-level MCP servers from.
    pub fn config(self, home: &Home) -> ConfigFile {
        let (path, format, parent) = match self {
            Client::ClaudeCode => (home.join(".claude.json"), Format::Json, "mcpServers"),
            Client::Codex => (home.join(".codex/config.toml"), Format::Toml, "mcp_servers"),
            Client::Cursor => (home.join(".cursor/mcp.json"), Format::Json, "mcpServers"),
            Client::VsCode => (home.vscode_user().join("mcp.json"), Format::Json, "servers"),
            Client::Goose => (
                home.join(".config/goose/config.yaml"),
                Format::Yaml,
                "extensions",
            ),
            Client::Gemini => (
                home.join(".gemini/settings.json"),
                Format::Json,
                "mcpServers",
            ),
        };
        ConfigFile {
            path,
            format,
            parent,
        }
    }

    /// The read-only entry this agent gets under [`MCP_SERVER_NAME`].
    pub fn entry(self) -> Value {
        self.entry_for(&Mode::Read)
    }

    /// The entry this agent gets under [`MCP_SERVER_NAME`] for `mode`.
    pub fn entry_for(self, mode: &Mode) -> Value {
        let command = command_and_args().0;
        let args = mode.args();
        match self {
            Client::ClaudeCode => {
                json!({"type": "stdio", "command": command, "args": args, "env": {}})
            }
            Client::VsCode => json!({"type": "stdio", "command": command, "args": args}),
            Client::Codex | Client::Cursor | Client::Gemini => {
                json!({"command": command, "args": args})
            }
            // Goose's own `goose configure` writes these fields; `enabled`
            // turns the extension on, it approves nothing.
            Client::Goose => json!({
                "enabled": true,
                "type": "stdio",
                "name": MCP_SERVER_NAME,
                "description": mode.description(),
                "cmd": command,
                "args": args,
                "envs": {},
                "timeout": 300,
            }),
        }
    }

    /// Whether `value` is an entry setup writes for this agent, in either
    /// mode, so switching between `relish mcp` and `relish mcp --propose`
    /// replaces setup's own entry without `--force`.
    pub fn is_own(self, value: &Value) -> bool {
        value
            .get("args")
            .and_then(Mode::of_args)
            .is_some_and(|mode| self.entry_for(&mode) == *value)
    }

    /// Where this agent's personal skills go, for the agents that read
    /// Agent Skills from the home directory.
    pub fn skill(self, home: &Home) -> Option<PathBuf> {
        match self {
            Client::ClaudeCode => Some(
                home.join(".claude/skills")
                    .join(MCP_SERVER_NAME)
                    .join("SKILL.md"),
            ),
            _ => None,
        }
    }
}

fn command_and_args() -> (&'static str, &'static [&'static str]) {
    match MCP_COMMAND_LINE.split_first() {
        Some((command, args)) => (command, args),
        None => ("relish", &[]),
    }
}

/// The server's command line, as setup reports it.
pub fn mcp_command_line() -> Vec<String> {
    MCP_COMMAND_LINE
        .iter()
        .map(|word| word.to_string())
        .collect()
}

/// `path` with `home` shown as `~`, for prose.
pub fn tilde(path: &Path, home: &Home) -> String {
    match path.strip_prefix(&home.root) {
        Ok(rest) => format!("~/{}", rest.display()),
        Err(_) => path.display().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Keys that would let an agent call tools without asking, in any of the
    /// six formats: Gemini's `trust`, Cline's and Roo's `alwaysAllow` and
    /// `autoApprove`, Claude Code's `permissions`, and the like.
    const APPROVAL_KEYS: &[&str] = &[
        "trust",
        "alwaysAllow",
        "always_allow",
        "autoApprove",
        "auto_approve",
        "permissions",
        "allow",
        "allowedTools",
        "allowed_tools",
        "includeTools",
        "bypass",
        "yolo",
    ];

    fn keys(value: &Value, found: &mut Vec<String>) {
        match value {
            Value::Object(map) => {
                for (key, inner) in map {
                    found.push(key.clone());
                    keys(inner, found);
                }
            }
            Value::Array(items) => items.iter().for_each(|item| keys(item, found)),
            _ => {}
        }
    }

    #[test]
    fn no_entry_approves_anything() {
        for client in Client::ALL {
            let entry = client.entry();
            let mut found = Vec::new();
            keys(&entry, &mut found);
            for key in &found {
                assert!(
                    !APPROVAL_KEYS
                        .iter()
                        .any(|bad| bad.eq_ignore_ascii_case(key)),
                    "{client:?} writes {key}"
                );
            }
            assert!(
                !entry.to_string().contains('*'),
                "{client:?} has a wildcard: {entry}"
            );
        }
    }

    #[test]
    fn every_entry_starts_relish_mcp() {
        for client in Client::ALL {
            let entry = client.entry();
            let command = entry
                .get("command")
                .or_else(|| entry.get("cmd"))
                .and_then(Value::as_str);
            assert_eq!(command, Some("relish"), "{client:?}");
            assert_eq!(entry["args"], json!(["mcp"]), "{client:?}");
        }
    }

    #[test]
    fn a_proposing_entry_starts_relish_mcp_propose_in_one_namespace() {
        let mode = Mode::Propose {
            namespace: "shop".to_string(),
        };
        assert_eq!(
            mode.command_line(),
            ["relish", "mcp", "--propose", "--namespace", "shop"]
        );
        for client in Client::ALL {
            let entry = client.entry_for(&mode);
            assert_eq!(
                entry["args"],
                json!(["mcp", "--propose", "--namespace", "shop"]),
                "{client:?}"
            );
            let mut found = Vec::new();
            keys(&entry, &mut found);
            assert!(
                !found.iter().any(|key| APPROVAL_KEYS
                    .iter()
                    .any(|bad| bad.eq_ignore_ascii_case(key))),
                "{client:?} approves something: {entry}"
            );
        }
    }

    #[test]
    fn setup_knows_its_own_entries_in_either_mode_and_nothing_else() {
        let propose = Mode::Propose {
            namespace: "shop".to_string(),
        };
        for client in Client::ALL {
            assert!(client.is_own(&client.entry()), "{client:?}");
            assert!(client.is_own(&client.entry_for(&propose)), "{client:?}");
            let mut edited = client.entry_for(&propose);
            edited["args"] = json!(["mcp", "--propose"]);
            assert!(!client.is_own(&edited), "{client:?}: {edited}");
            let mut extra = client.entry();
            extra["env"] = json!({"RELIABURGER_TOKEN": "rbrg_someone"});
            assert!(!client.is_own(&extra), "{client:?}: {extra}");
            assert!(!client.is_own(&json!({"command": "other-mcp"})));
        }
    }

    #[test]
    fn every_config_is_under_home() {
        let home = Home {
            root: PathBuf::from("/home/someone"),
            macos: false,
        };
        for client in Client::ALL {
            assert!(client.config(&home).path.starts_with(&home.root));
            if let Some(skill) = client.skill(&home) {
                assert!(skill.starts_with(&home.root));
            }
        }
        let mac = Home {
            root: PathBuf::from("/Users/someone"),
            macos: true,
        };
        assert_eq!(
            tilde(&Client::VsCode.config(&mac).path, &mac),
            "~/Library/Application Support/Code/User/mcp.json"
        );
        assert_eq!(
            tilde(&Client::VsCode.config(&home).path, &home),
            "~/.config/Code/User/mcp.json"
        );
    }

    #[test]
    fn names_round_trip_through_clap() {
        for client in Client::ALL {
            let parsed = <Client as clap::ValueEnum>::from_str(client.as_str(), false).unwrap();
            assert_eq!(parsed, *client);
            assert_eq!(
                serde_json::to_value(client).unwrap(),
                json!(client.as_str())
            );
        }
    }
}
