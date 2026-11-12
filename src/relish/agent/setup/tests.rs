//! `relish agent setup` against a temporary home directory.

use std::collections::BTreeMap;

use clap::Parser;
use serde_json::{Value, json};

use super::*;

/// A home directory in a temporary directory, deleted when dropped.
struct TestHome {
    _dir: tempfile::TempDir,
    home: Home,
}

impl TestHome {
    fn new() -> TestHome {
        let dir = tempfile::tempdir().unwrap();
        // macOS's temporary directory is behind a symlink; resolve it so the
        // paths setup reports start with the root the tests strip.
        let root = fs::canonicalize(dir.path()).unwrap();
        TestHome {
            _dir: dir,
            home: Home { root, macos: true },
        }
    }

    fn path(&self, relative: &str) -> PathBuf {
        self.home.root.join(relative)
    }

    fn write(&self, relative: &str, text: &str) {
        let path = self.path(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    fn mkdir(&self, relative: &str) {
        fs::create_dir_all(self.path(relative)).unwrap();
    }

    fn read(&self, relative: &str) -> String {
        fs::read_to_string(self.path(relative)).unwrap()
    }

    /// Every file under the home, relative path to contents.
    fn files(&self) -> BTreeMap<String, String> {
        let mut files = BTreeMap::new();
        let mut pending = vec![self.home.root.clone()];
        while let Some(directory) = pending.pop() {
            for entry in fs::read_dir(&directory).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    pending.push(path);
                } else {
                    let relative = path.strip_prefix(&self.home.root).unwrap();
                    files.insert(
                        relative.display().to_string(),
                        fs::read_to_string(&path).unwrap(),
                    );
                }
            }
        }
        files
    }

    /// Every directory under the home, relative.
    fn directories(&self) -> Vec<String> {
        let mut found = Vec::new();
        let mut pending = vec![self.home.root.clone()];
        while let Some(directory) = pending.pop() {
            for entry in fs::read_dir(&directory).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    let relative = path.strip_prefix(&self.home.root).unwrap();
                    found.push(relative.display().to_string());
                    pending.push(path);
                }
            }
        }
        found.sort();
        found
    }

    /// A home with every agent installed and some config of their own.
    fn with_every_agent() -> TestHome {
        let home = TestHome::new();
        home.write(
            ".claude.json",
            "{\n  \"numStartups\": 12,\n  \"mcpServers\": {\n    \"github\": {\n      \"command\": \"gh-mcp\",\n      \"env\": {\n        \"GITHUB_TOKEN\": \"ghp_not_for_the_diff\"\n      }\n    }\n  },\n  \"projects\": {}\n}\n",
        );
        home.write(
            ".claude/settings.json",
            "{\"permissions\": {\"allow\": [\"Bash(ls:*)\"]}}\n",
        );
        home.write(
            ".codex/config.toml",
            "# Codex\nmodel = \"gpt-5\"\n\n[mcp_servers.docs]\ncommand = \"docs-mcp\"\n",
        );
        home.mkdir(".cursor");
        home.mkdir("Library/Application Support/Code/User");
        home.write(
            ".config/goose/config.yaml",
            "GOOSE_PROVIDER: anthropic\nextensions:\n  developer:\n    enabled: true\n    type: builtin\n    name: developer\n",
        );
        home.write(".gemini/settings.json", "{\n  \"theme\": \"Dracula\"\n}\n");
        home
    }
}

#[derive(Debug, Parser)]
struct TestCli {
    #[command(flatten)]
    options: SetupOptions,
}

fn options(line: &str) -> SetupOptions {
    let mut argv = vec!["setup"];
    argv.extend(line.split_whitespace());
    TestCli::try_parse_from(argv).unwrap().options
}

const SKILL: &str =
    "---\nname: reliaburger\ndescription: test skill\n---\n\nRun relish agent context first.\n";

fn build() -> BuildInfo {
    BuildInfo {
        version: "0.2.7".to_string(),
        commit: None,
        dirty: None,
        source_digest: None,
    }
}

fn run_in(home: &TestHome, line: &str, skill: Option<&str>) -> Value {
    run_with_token(home, line, skill, None)
}

fn run_with_token(
    home: &TestHome,
    line: &str,
    skill: Option<&str>,
    token: Option<TokenPlan>,
) -> Value {
    use crate::relish::client::TokenOrigin;
    let origin = if token.is_some() {
        TokenOrigin::ContextAdmin
    } else {
        TokenOrigin::Absent
    };
    run_with_facts(
        home,
        line,
        skill,
        token,
        TokenFacts {
            origin,
            access: None,
        },
    )
}

fn run_with_facts(
    home: &TestHome,
    line: &str,
    skill: Option<&str>,
    token: Option<TokenPlan>,
    facts: TokenFacts,
) -> Value {
    let envelope = answer(
        &options(line),
        &home.home,
        skill,
        token,
        facts,
        build(),
        format!("relish agent setup {line}").trim_end(),
    );
    let text = serde_json::to_string(&envelope).unwrap();
    let root = home.home.root.display().to_string();
    serde_json::from_str(&text.replace(&root, "$HOME")).unwrap()
}

fn statuses(answer: &Value) -> Vec<(String, String, String)> {
    let mut found = Vec::new();
    for agent in answer["data"]["agents"].as_array().unwrap() {
        for change in agent["changes"].as_array().unwrap() {
            found.push((
                agent["agent"].as_str().unwrap().to_string(),
                change["kind"].as_str().unwrap().to_string(),
                change["status"].as_str().unwrap().to_string(),
            ));
        }
    }
    found
}

fn named_paths(answer: &Value, home: &TestHome) -> Vec<String> {
    let mut paths = Vec::new();
    for agent in answer["data"]["agents"].as_array().unwrap() {
        for change in agent["changes"].as_array().unwrap() {
            let path = change["path"].as_str().unwrap();
            let relative = path.strip_prefix("$HOME/").unwrap();
            assert!(home.path(relative).starts_with(&home.home.root));
            paths.push(relative.to_string());
        }
    }
    paths
}

#[test]
fn without_yes_nothing_is_written_and_the_answer_is_the_diff() {
    let home = TestHome::with_every_agent();
    let before = home.files();
    let directories = home.directories();
    let answer = run_in(&home, "", Some(SKILL));
    assert_eq!(home.files(), before, "a preview writes nothing");
    assert_eq!(home.directories(), directories, "nor makes a directory");
    assert_eq!(answer["data"]["applied"], false);
    insta::assert_json_snapshot!(answer);
}

#[test]
fn dry_run_and_print_write_nothing_either() {
    let home = TestHome::with_every_agent();
    let before = home.files();
    for line in [
        "--dry-run",
        "--print",
        "--dry-run --remove",
        "--dry-run --force",
    ] {
        let answer = run_in(&home, line, Some(SKILL));
        assert_eq!(answer["data"]["applied"], false, "{line}");
        assert_eq!(home.files(), before, "{line}");
    }
}

#[test]
fn dry_run_and_yes_together_are_a_usage_error() {
    assert!(TestCli::try_parse_from(["setup", "--dry-run", "--yes"]).is_err());
    assert!(TestCli::try_parse_from(["setup", "--print", "--yes"]).is_err());
    assert!(TestCli::try_parse_from(["setup", "nonsense"]).is_err());
    let parsed = options("cursor vscode --yes");
    assert_eq!(parsed.agents, [Client::Cursor, Client::VsCode]);
    assert!(parsed.yes);
}

#[test]
fn yes_writes_only_the_paths_the_answer_names() {
    let home = TestHome::with_every_agent();
    let before = home.files();
    let answer = run_in(&home, "--yes", Some(SKILL));
    assert_eq!(answer["data"]["applied"], true);
    let named = named_paths(&answer, &home);
    let after = home.files();
    for (path, text) in &after {
        if before.get(path) != Some(text) {
            assert!(
                named.contains(path),
                "wrote {path}, which the answer doesn't name"
            );
        }
    }
    for path in before.keys() {
        assert!(after.contains_key(path), "{path} disappeared");
    }
    assert!(
        !after.keys().any(|path| path.ends_with(".relish-setup.tmp")),
        "{after:?}"
    );
    assert_eq!(
        after[".claude/settings.json"], before[".claude/settings.json"],
        "Claude Code's permissions are never touched"
    );
    let every_change_written = answer["data"]["agents"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|agent| agent["changes"].as_array().unwrap())
        .all(|change| change["written"] == true);
    assert!(every_change_written, "{answer}");
    insta::assert_json_snapshot!(
        "files_after_setup",
        after
            .iter()
            .filter(|(path, text)| before.get(*path) != Some(*text))
            .collect::<BTreeMap<_, _>>()
    );
}

#[test]
fn a_second_run_changes_nothing() {
    let home = TestHome::with_every_agent();
    run_in(&home, "--yes", Some(SKILL));
    let once = home.files();
    let answer = run_in(&home, "--yes", Some(SKILL));
    assert_eq!(home.files(), once);
    for (agent, kind, status) in statuses(&answer) {
        assert_eq!(status, "unchanged", "{agent} {kind}");
    }
    assert_eq!(
        answer["warnings"],
        json!([]),
        "nothing was written, so nothing to restart"
    );
    assert_eq!(answer["next"], json!([]));
}

#[test]
fn remove_undoes_setup_byte_for_byte_where_the_format_allows() {
    let home = TestHome::with_every_agent();
    let before = home.files();
    run_in(&home, "--yes", Some(SKILL));
    let answer = run_in(&home, "--remove --yes", Some(SKILL));
    assert_eq!(answer["data"]["action"], "remove");
    let after = home.files();
    for path in [
        ".claude.json",
        ".codex/config.toml",
        ".config/goose/config.yaml",
        ".gemini/settings.json",
    ] {
        assert_eq!(after[path], before[path], "{path}");
    }
    assert!(!after.contains_key(".claude/skills/reliaburger/SKILL.md"));
    assert!(
        !home.path(".claude/skills/reliaburger").exists(),
        "the skill's empty directory goes too"
    );
    // A file setup created is left as an empty object.
    assert_eq!(after[".cursor/mcp.json"], "{}\n");
    let again = run_in(&home, "--remove --yes", Some(SKILL));
    for (agent, kind, status) in statuses(&again) {
        assert_eq!(status, "unchanged", "{agent} {kind}");
    }
}

#[test]
fn someone_elses_entry_is_left_alone_unless_forced() {
    let home = TestHome::new();
    let theirs = "{\n  \"mcpServers\": {\n    \"reliaburger\": {\n      \"command\": \"/opt/rb/relish\",\n      \"args\": [\"mcp\", \"--no-logs\"]\n    }\n  }\n}\n";
    home.write(".cursor/mcp.json", theirs);
    let answer = run_in(&home, "cursor --yes", None);
    assert_eq!(
        statuses(&answer),
        [("cursor".into(), "mcp_server".into(), "conflict".into())]
    );
    assert_eq!(home.read(".cursor/mcp.json"), theirs);
    assert!(
        answer["warnings"][0]
            .as_str()
            .unwrap()
            .contains("~/.cursor/mcp.json already has a mcpServers.reliaburger entry"),
        "{answer}"
    );
    let removed = run_in(&home, "cursor --remove --yes", None);
    assert_eq!(statuses(&removed)[0].2, "conflict");
    assert_eq!(home.read(".cursor/mcp.json"), theirs);

    let forced = run_in(&home, "cursor --yes --force", None);
    assert_eq!(statuses(&forced)[0].2, "replace");
    let written: Value = serde_json::from_str(&home.read(".cursor/mcp.json")).unwrap();
    assert_eq!(
        written["mcpServers"]["reliaburger"],
        json!({"command": "relish", "args": ["mcp"]})
    );
}

#[test]
fn a_users_own_skill_is_left_alone_unless_forced() {
    let home = TestHome::new();
    home.write(".claude/skills/reliaburger/SKILL.md", "my own notes\n");
    let answer = run_in(&home, "claude-code --yes", Some(SKILL));
    let skill = &statuses(&answer)[1];
    assert_eq!((skill.1.as_str(), skill.2.as_str()), ("skill", "conflict"));
    assert_eq!(
        home.read(".claude/skills/reliaburger/SKILL.md"),
        "my own notes\n"
    );
    let removed = run_in(&home, "claude-code --remove --yes", Some(SKILL));
    assert_eq!(statuses(&removed)[1].2, "conflict");
    assert!(home.path(".claude/skills/reliaburger/SKILL.md").exists());
}

#[test]
fn a_file_that_cant_be_rewritten_safely_is_left_alone() {
    let home = TestHome::new();
    let jsonc = "{\n  // mine\n  \"servers\": {}\n}\n";
    home.write("Library/Application Support/Code/User/mcp.json", jsonc);
    let yaml = "# my goose\nextensions: {}\n";
    home.write(".config/goose/config.yaml", yaml);
    let answer = run_in(&home, "--yes", None);
    let found = statuses(&answer);
    assert_eq!(
        found,
        [
            ("vscode".into(), "mcp_server".into(), "unreadable".into()),
            ("goose".into(), "mcp_server".into(), "unreadable".into()),
        ]
    );
    assert_eq!(
        home.read("Library/Application Support/Code/User/mcp.json"),
        jsonc
    );
    assert_eq!(home.read(".config/goose/config.yaml"), yaml);
    let note = answer["data"]["agents"][3]["changes"][0]["note"]
        .as_str()
        .unwrap();
    assert!(note.contains("put this under \"reliaburger\""), "{note}");
}

#[test]
fn agents_that_arent_installed_are_listed_but_untouched() {
    let home = TestHome::new();
    home.mkdir(".gemini");
    let answer = run_in(&home, "--yes", None);
    assert_eq!(
        statuses(&answer),
        [("gemini".into(), "mcp_server".into(), "add".into())]
    );
    assert_eq!(
        home.files().keys().collect::<Vec<_>>(),
        [".gemini/settings.json"]
    );
    let agents = answer["data"]["agents"].as_array().unwrap();
    assert_eq!(agents.len(), Client::ALL.len());
    assert_eq!(agents[0]["detected"], false);
}

#[test]
fn a_named_agent_is_set_up_even_when_it_wasnt_found() {
    let home = TestHome::new();
    let answer = run_in(&home, "codex --yes", None);
    assert_eq!(
        statuses(&answer),
        [("codex".into(), "mcp_server".into(), "add".into())]
    );
    assert_eq!(
        home.read(".codex/config.toml"),
        "[mcp_servers.reliaburger]\nargs = [\"mcp\"]\ncommand = \"relish\"\n"
    );
}

#[test]
fn finding_no_agent_says_how_to_name_one() {
    let home = TestHome::new();
    let answer = run_in(&home, "", None);
    assert_eq!(statuses(&answer), []);
    assert!(
        answer["warnings"][0]
            .as_str()
            .unwrap()
            .contains("relish agent setup cursor"),
        "{answer}"
    );
    assert_eq!(answer["next"], json!([]));
}

#[test]
fn the_diff_shows_no_neighbouring_lines() {
    let home = TestHome::with_every_agent();
    let answer = run_in(&home, "claude-code", None);
    let diff = answer["data"]["agents"][0]["changes"][0]["diff"]
        .as_str()
        .unwrap();
    assert!(!diff.contains("ghp_not_for_the_diff"), "{diff}");
    assert!(!diff.contains("github"), "{diff}");
    assert!(
        diff.lines()
            .filter(|line| !line.starts_with("---") && !line.starts_with("+++"))
            .all(|line| line.starts_with('+') || line.starts_with("@@")),
        "only added lines: {diff}"
    );
}

#[test]
fn the_preview_offers_the_same_command_with_yes() {
    let home = TestHome::with_every_agent();
    let answer = run_in(&home, "cursor --dry-run", None);
    assert_eq!(
        answer["next"],
        json!([{
            "run": "relish agent setup cursor --yes",
            "why": "make exactly these changes, once a human has read the diffs",
            "read_only": false,
        }])
    );
}

#[test]
fn a_file_changed_after_setup_read_it_is_not_overwritten() {
    let home = TestHome::new();
    home.write(".gemini/settings.json", "{}\n");
    let mut planned = plan_server(
        Client::Gemini,
        &home.home,
        &Mode::Read,
        Action::Install,
        false,
    );
    home.write(".gemini/settings.json", "{\"theme\": \"new\"}\n");
    apply(&mut planned);
    assert!(!planned.change.written);
    assert!(
        planned
            .change
            .note
            .as_deref()
            .unwrap()
            .contains("changed while setup was working")
    );
    assert_eq!(home.read(".gemini/settings.json"), "{\"theme\": \"new\"}\n");
}

#[cfg(unix)]
#[test]
fn a_private_file_stays_private() {
    use std::os::unix::fs::PermissionsExt;
    let home = TestHome::new();
    home.write(".claude.json", "{}\n");
    fs::set_permissions(home.path(".claude.json"), fs::Permissions::from_mode(0o600)).unwrap();
    run_in(&home, "claude-code --yes", None);
    let mode = fs::metadata(home.path(".claude.json"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(mode & 0o777, 0o600);
}

#[cfg(unix)]
#[test]
fn a_symlinked_config_stays_a_symlink() {
    let home = TestHome::new();
    home.write("dotfiles/gemini.json", "{}\n");
    home.mkdir(".gemini");
    std::os::unix::fs::symlink(
        home.path("dotfiles/gemini.json"),
        home.path(".gemini/settings.json"),
    )
    .unwrap();
    let answer = run_in(&home, "gemini --yes", None);
    assert_eq!(
        answer["data"]["agents"][5]["changes"][0]["path"],
        "$HOME/dotfiles/gemini.json"
    );
    assert!(
        fs::symlink_metadata(home.path(".gemini/settings.json"))
            .unwrap()
            .file_type()
            .is_symlink()
    );
    assert!(home.read("dotfiles/gemini.json").contains("reliaburger"));
}

#[test]
fn the_written_configs_approve_nothing() {
    let home = TestHome::with_every_agent();
    run_in(&home, "--yes", Some(SKILL));
    for (path, text) in home.files() {
        if path.starts_with(".claude/settings") {
            continue;
        }
        for word in [
            "trust",
            "alwaysAllow",
            "autoApprove",
            "allow",
            "yolo",
            "\"*\"",
        ] {
            assert!(!text.contains(word), "{path} contains {word}: {text}");
        }
    }
}

// ---------------------------------------------------------------------------
// relish mcp's read-only token (#1730).

/// What a fake token plan's `apply` was asked to do.
type TokenCalls = std::sync::Arc<std::sync::Mutex<Vec<(Action, PathBuf)>>>;

/// The preview a real plan shows, from the credentials module.
fn preview() -> String {
    let scope = crate::relish::mcp::credentials::Scope {
        owner: "laptop".to_string(),
        endpoint: "https://127.0.0.1:19117".to_string(),
        namespace: None,
        purpose: crate::relish::mcp::credentials::Purpose::Read,
    };
    crate::relish::mcp::credentials::shape(&scope, None, 1_790_000_000)
}

fn token_file(home: &TestHome, name: &str, stored: bool) -> TokenFile {
    TokenFile {
        path: home.path(&format!(".reliaburger/{name}")),
        stored,
        preview: preview(),
        note: None,
    }
}

/// A token plan over `files` whose `apply` records what it was asked to do
/// and answers with `outcome`.
fn token_plan_over(
    files: Vec<TokenFile>,
    outcome: Result<bool, String>,
) -> (TokenPlan, TokenCalls) {
    let calls: TokenCalls = Default::default();
    let record = calls.clone();
    let plan = TokenPlan {
        files,
        apply: Box::new(move |action, path: &std::path::Path| {
            record.lock().unwrap().push((action, path.to_path_buf()));
            outcome.clone()
        }),
    };
    (plan, calls)
}

fn token_plan(
    home: &TestHome,
    stored: bool,
    outcome: Result<bool, String>,
) -> (TokenPlan, TokenCalls) {
    token_plan_over(vec![token_file(home, "mcp-token.json", stored)], outcome)
}

fn actions(calls: &TokenCalls) -> Vec<Action> {
    calls
        .lock()
        .unwrap()
        .iter()
        .map(|(action, _)| *action)
        .collect()
}

#[test]
fn an_admin_context_shows_the_read_only_token_in_the_preview_and_makes_nothing() {
    let home = TestHome::with_every_agent();
    let (plan, calls) = token_plan(&home, false, Ok(true));
    let answer = run_with_token(&home, "", Some(SKILL), Some(plan));
    let token = &answer["data"]["mcp_tokens"][0];
    assert_eq!(token["kind"], "mcp_token");
    assert_eq!(token["status"], "add");
    assert_eq!(token["written"], false);
    assert!(calls.lock().unwrap().is_empty(), "a preview mints nothing");
    assert!(
        answer["next"][0]["run"]
            .as_str()
            .unwrap()
            .ends_with("--yes")
    );
}

/// #1763 review: the diff showed a sentence as the file's content, though
/// the file holds JSON with the secret in it.
#[test]
fn the_token_diff_shows_the_files_json_with_the_secret_redacted() {
    let home = TestHome::with_every_agent();
    let (plan, _) = token_plan(&home, false, Ok(true));
    let answer = run_with_token(&home, "", Some(SKILL), Some(plan));
    let diff = answer["data"]["mcp_tokens"][0]["diff"].as_str().unwrap();
    assert!(diff.contains(r#"+  "token": "[redacted]""#), "{diff}");
    assert!(diff.contains(r#"+  "owner": "laptop""#), "{diff}");
    assert!(!diff.contains("relish mcp sends it instead"), "{diff}");
}

#[test]
fn yes_makes_the_read_only_token() {
    let home = TestHome::with_every_agent();
    let (plan, calls) = token_plan(&home, false, Ok(true));
    let answer = run_with_token(&home, "--yes", Some(SKILL), Some(plan));
    assert_eq!(answer["data"]["mcp_tokens"][0]["written"], true);
    assert_eq!(actions(&calls), [Action::Install]);
}

#[test]
fn a_stored_token_is_left_alone() {
    let home = TestHome::with_every_agent();
    let (plan, calls) = token_plan(&home, true, Ok(true));
    let answer = run_with_token(&home, "--yes", Some(SKILL), Some(plan));
    assert_eq!(answer["data"]["mcp_tokens"][0]["status"], "unchanged");
    assert!(calls.lock().unwrap().is_empty());
}

/// #1763 review: a stored token the cluster had revoked was reported
/// `unchanged`. The plan marks it not stored, says why, and `--yes`
/// replaces it.
#[test]
fn a_stored_token_the_cluster_rejects_is_replaced_and_says_why() {
    let home = TestHome::with_every_agent();
    let mut file = token_file(&home, "mcp-token.json", false);
    file.note = Some("the cluster rejects the stored token".to_string());
    let (plan, calls) = token_plan_over(vec![file], Ok(true));
    let answer = run_with_token(&home, "--yes", Some(SKILL), Some(plan));
    let token = &answer["data"]["mcp_tokens"][0];
    assert_eq!(token["status"], "add");
    assert_eq!(token["written"], true);
    assert!(
        token["note"]
            .as_str()
            .unwrap()
            .contains("rejects the stored token"),
        "{token}"
    );
    assert_eq!(actions(&calls), [Action::Install]);
}

#[test]
fn without_an_admin_context_there_is_no_token_to_make() {
    let home = TestHome::with_every_agent();
    let answer = run_in(&home, "--yes", Some(SKILL));
    assert_eq!(answer["data"]["mcp_tokens"], serde_json::json!([]));
}

#[test]
fn a_context_the_cluster_says_isnt_admin_makes_no_token() {
    let home = TestHome::with_every_agent();
    let (plan, _) = token_plan(&home, false, Ok(false));
    let answer = run_with_token(&home, "--yes", Some(SKILL), Some(plan));
    let token = &answer["data"]["mcp_tokens"][0];
    assert_eq!(token["written"], false);
    assert!(
        token["note"].as_str().unwrap().contains("isn't admin"),
        "{token}"
    );
}

#[test]
fn a_cluster_that_refuses_the_token_is_a_clear_warning() {
    let home = TestHome::with_every_agent();
    let refusal = "the cluster refused to create a read-only token for relish mcp (HTTP 403: no)";
    let (plan, _) = token_plan(&home, false, Err(refusal.to_string()));
    let answer = run_with_token(&home, "--yes", Some(SKILL), Some(plan));
    let token = &answer["data"]["mcp_tokens"][0];
    assert_eq!(token["written"], false);
    assert!(token["note"].as_str().unwrap().contains(refusal), "{token}");
    let warnings = answer["warnings"].to_string();
    assert!(warnings.contains(refusal), "{warnings}");
    assert!(warnings.contains("never the admin one"), "{warnings}");
}

#[test]
fn remove_takes_the_token_back_out() {
    let home = TestHome::with_every_agent();
    home.write(".reliaburger/mcp-token.json", "{}");
    let (plan, calls) = token_plan(&home, true, Ok(true));
    let answer = run_with_token(&home, "--remove --yes", Some(SKILL), Some(plan));
    assert_eq!(answer["data"]["mcp_tokens"][0]["status"], "remove");
    assert_eq!(answer["data"]["mcp_tokens"][0]["written"], true);
    assert_eq!(actions(&calls), [Action::Remove]);
}

/// #1763 review: `--remove` left `mcp-token-<namespace>.json` and its
/// token behind. Every file is in the preview and every one is removed.
#[test]
fn remove_takes_every_namespace_token_out_too() {
    let home = TestHome::with_every_agent();
    let files = vec![
        token_file(&home, "mcp-token.json", true),
        token_file(&home, "mcp-token-payments.json", true),
    ];
    let (plan, _) = token_plan_over(files, Ok(true));
    let preview = run_with_token(&home, "--remove", Some(SKILL), Some(plan));
    let tokens = preview["data"]["mcp_tokens"].as_array().unwrap();
    assert_eq!(tokens.len(), 2, "{tokens:?}");
    assert!(
        tokens[1]["diff"]
            .as_str()
            .unwrap()
            .contains("mcp-token-payments.json"),
        "{tokens:?}"
    );

    let files = vec![
        token_file(&home, "mcp-token.json", true),
        token_file(&home, "mcp-token-payments.json", true),
    ];
    let (plan, calls) = token_plan_over(files, Ok(true));
    let answer = run_with_token(&home, "--remove --yes", Some(SKILL), Some(plan));
    for token in answer["data"]["mcp_tokens"].as_array().unwrap() {
        assert_eq!(token["status"], "remove", "{token}");
        assert_eq!(token["written"], true, "{token}");
    }
    let removed: Vec<PathBuf> = calls
        .lock()
        .unwrap()
        .iter()
        .map(|(_, path)| path.clone())
        .collect();
    assert_eq!(
        removed,
        [
            home.path(".reliaburger/mcp-token.json"),
            home.path(".reliaburger/mcp-token-payments.json")
        ]
    );
}

// ---------------------------------------------------------------------------
// relish agent setup --propose (#1682).

fn explicit(access: Option<crate::relish::client::TokenAccess>) -> TokenFacts {
    TokenFacts {
        origin: crate::relish::client::TokenOrigin::Explicit,
        access,
    }
}

#[test]
fn propose_without_a_namespace_is_refused_and_writes_nothing() {
    let home = TestHome::with_every_agent();
    let before = home.files();
    let (plan, calls) = token_plan(&home, false, Ok(true));
    let answer = run_with_token(&home, "--propose --yes", Some(SKILL), Some(plan));
    assert_eq!(answer["ok"], false, "{answer}");
    assert_eq!(answer["error"]["code"], "usage", "{answer}");
    let message = answer["error"]["message"].as_str().unwrap();
    assert!(message.contains("--propose needs --namespace"), "{message}");
    assert!(message.contains("every namespace"), "{message}");
    assert_eq!(home.files(), before, "a refusal writes nothing");
    assert!(calls.lock().unwrap().is_empty(), "nor makes a token");
}

#[test]
fn a_namespace_needs_propose_and_propose_doesnt_remove() {
    assert!(TestCli::try_parse_from(["setup", "--namespace", "shop"]).is_err());
    assert!(
        TestCli::try_parse_from(["setup", "--propose", "--namespace", "shop", "--remove"]).is_err()
    );
    let parsed = options("--propose --namespace shop");
    assert_eq!(
        parsed.mode().unwrap(),
        Mode::Propose {
            namespace: "shop".to_string()
        }
    );
    let bad = options("--propose --namespace ../etc");
    assert_eq!(bad.mode().unwrap_err().code, ErrorCode::Usage);
}

#[test]
fn propose_wires_relish_mcp_propose_in_one_namespace() {
    let home = TestHome::with_every_agent();
    let answer = run_in(&home, "--propose --namespace shop --yes", Some(SKILL));
    assert_eq!(answer["ok"], true, "{answer}");
    assert_eq!(
        answer["data"]["server"]["command"],
        json!(["relish", "mcp", "--propose", "--namespace", "shop"])
    );
    let claude: Value = serde_json::from_str(&home.read(".claude.json")).unwrap();
    assert_eq!(
        claude["mcpServers"]["reliaburger"]["args"],
        json!(["mcp", "--propose", "--namespace", "shop"])
    );
    let codex = home.read(".codex/config.toml");
    assert!(
        codex.contains(r#"args = ["mcp", "--propose", "--namespace", "shop"]"#),
        "{codex}"
    );
    // Running it again changes nothing.
    let again = run_in(&home, "--propose --namespace shop --yes", Some(SKILL));
    assert!(
        statuses(&again)
            .iter()
            .all(|(_, _, status)| status == "unchanged"),
        "{again}"
    );
}

#[test]
fn switching_to_and_from_propose_replaces_setups_own_entry_without_force() {
    let home = TestHome::with_every_agent();
    run_in(&home, "--yes", Some(SKILL));
    let answer = run_in(&home, "--propose --namespace shop --yes", Some(SKILL));
    let servers: Vec<String> = statuses(&answer)
        .into_iter()
        .filter(|(_, kind, _)| kind == "mcp_server")
        .map(|(_, _, status)| status)
        .collect();
    assert!(!servers.is_empty());
    assert!(servers.iter().all(|status| status == "replace"), "{answer}");
    // Another namespace, and back to read-only, are setup's own too.
    let answer = run_in(&home, "--propose --namespace payments --yes", Some(SKILL));
    assert!(
        !statuses(&answer)
            .iter()
            .any(|(_, _, status)| status == "conflict"),
        "{answer}"
    );
    let answer = run_in(&home, "--yes", Some(SKILL));
    assert!(
        !statuses(&answer)
            .iter()
            .any(|(_, _, status)| status == "conflict"),
        "{answer}"
    );
    let claude: Value = serde_json::from_str(&home.read(".claude.json")).unwrap();
    assert_eq!(claude["mcpServers"]["reliaburger"]["args"], json!(["mcp"]));
}

#[test]
fn remove_takes_a_proposing_entry_out() {
    let home = TestHome::with_every_agent();
    let before = home.read(".claude.json");
    run_in(&home, "--propose --namespace shop --yes", Some(SKILL));
    let answer = run_in(&home, "--remove --yes", Some(SKILL));
    assert!(
        !statuses(&answer)
            .iter()
            .any(|(_, _, status)| status == "conflict"),
        "{answer}"
    );
    assert_eq!(home.read(".claude.json"), before);
}

#[test]
fn propose_makes_the_deployer_agent_token_through_the_plan() {
    let home = TestHome::with_every_agent();
    let (plan, calls) = token_plan_over(
        vec![token_file(&home, "mcp-propose-token-shop.json", false)],
        Ok(true),
    );
    let answer = run_with_token(
        &home,
        "--propose --namespace shop --yes",
        Some(SKILL),
        Some(plan),
    );
    let token = &answer["data"]["mcp_tokens"][0];
    assert_eq!(token["written"], true, "{answer}");
    assert!(
        token["path"]
            .as_str()
            .unwrap()
            .ends_with("mcp-propose-token-shop.json")
    );
    assert_eq!(actions(&calls), [Action::Install]);
}

#[test]
fn propose_with_a_token_setup_didnt_make_warns_it_may_be_a_persons() {
    let home = TestHome::with_every_agent();
    let answer = run_with_facts(
        &home,
        "--propose --namespace shop",
        Some(SKILL),
        None,
        explicit(Some(crate::relish::client::TokenAccess::Lesser)),
    );
    let warnings = answer["warnings"].to_string();
    assert!(warnings.contains("can't tell whether"), "{warnings}");
    assert!(
        warnings.contains("--agent --role deployer --namespaces shop"),
        "{warnings}"
    );
    assert!(!warnings.contains("admin one"), "{warnings}");
}

#[test]
fn an_admin_token_given_to_setup_is_warned_about() {
    let home = TestHome::with_every_agent();
    let admin = explicit(Some(crate::relish::client::TokenAccess::Admin));
    let answer = run_with_facts(
        &home,
        "--propose --namespace shop",
        Some(SKILL),
        None,
        admin,
    );
    let warnings = answer["warnings"].to_string();
    assert!(warnings.contains("is an admin one"), "{warnings}");
    let answer = run_with_facts(&home, "", Some(SKILL), None, admin);
    let warnings = answer["warnings"].to_string();
    assert!(warnings.contains("is an admin one"), "{warnings}");
    assert!(warnings.contains("--role read-only"), "{warnings}");
}

#[test]
fn propose_from_a_context_that_isnt_admin_warns_the_proposals_are_a_persons() {
    let home = TestHome::with_every_agent();
    let (plan, _) = token_plan_over(
        vec![token_file(&home, "mcp-propose-token-shop.json", false)],
        Ok(false),
    );
    let answer = run_with_token(
        &home,
        "--propose --namespace shop --yes",
        Some(SKILL),
        Some(plan),
    );
    let warnings = answer["warnings"].to_string();
    assert!(warnings.contains("isn't an admin one"), "{warnings}");
    assert!(warnings.contains("that person's"), "{warnings}");
}
