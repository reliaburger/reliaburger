//! How a request handler decides whether this node leads the council.
//!
//! `CouncilNode::is_leader` is one `ensure_linearizable` round, and each
//! follower gets a single heartbeat interval to answer it. Under load both
//! followers can answer one round late, and a settled leader that read that
//! as "I'm a follower" would forward the request to itself, which
//! `leader_api_url` refuses with a 503 (#1009, #1253, #1393). So a request
//! handler asks [`leads`] instead: a leader gets [`LEADER_CONFIRMATION`] to
//! prove it, and a follower answers at once.
//!
//! Background loops may still call `is_leader`: a loop that skips a tick
//! tries again on the next one.

use crate::council::CouncilNode;

use super::api::ApiState;

/// How long a leader gets to prove it leads before it serves a request
/// itself.
pub use crate::council::LEADER_CONFIRMATION;

/// True if this node leads, as a request handler must know it before it
/// writes to Raft or answers from its own state. A follower returns false
/// at once, without waiting.
pub async fn leads(council: &CouncilNode) -> bool {
    council.confirm_leadership(LEADER_CONFIRMATION).await
}

/// The council to forward a request to, or `None` when this node has proved
/// it leads (or runs without a council) and may serve the request itself.
///
/// A follower's replica can lag, and a node back from a stop may hold a log
/// older than the run, so only a confirmed leader may say "not found".
pub async fn council_to_forward_to(state: &ApiState) -> Option<&CouncilNode> {
    let council = state.council.as_deref()?;
    if leads(council).await {
        None
    } else {
        Some(council)
    }
}

#[cfg(test)]
mod tests {
    /// Every `.rs` file under `dir`, leaving out test-only files.
    fn production_sources(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            let name = path.file_name().unwrap().to_string_lossy().to_string();
            if path.is_dir() {
                if name != "tests" {
                    production_sources(&path, out);
                }
            } else if name.ends_with(".rs") && !name.contains("tests") {
                out.push(path);
            }
        }
    }

    /// Each production line under `src/{dir}` that contains `call`, as
    /// `path:line: code`, unless the line before contains `explained_by`.
    /// A file's inline test module is skipped.
    fn calls_in(dir: &str, call: &str, explained_by: Option<&str>) -> Vec<String> {
        let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut sources = Vec::new();
        production_sources(&manifest.join("src").join(dir), &mut sources);
        let mut found = Vec::new();
        for path in sources {
            let source = std::fs::read_to_string(&path).unwrap();
            let lines: Vec<&str> = source.lines().collect();
            for (index, line) in lines.iter().enumerate() {
                let test_module = line.trim() == "#[cfg(test)]"
                    && lines
                        .get(index + 1)
                        .is_some_and(|next| next.trim_start().starts_with("mod "));
                if test_module {
                    break;
                }
                let explained = index > 0
                    && explained_by.is_some_and(|marker| lines[index - 1].contains(marker));
                if line.contains(call) && !explained {
                    let path = path.strip_prefix(manifest).unwrap_or(&path);
                    found.push(format!("{}:{}: {}", path.display(), index + 1, line.trim()));
                }
            }
        }
        found
    }

    /// No request handler in `src/bun/` decides with a single round: not
    /// only the API modules, since the job, batch, task-array and build
    /// routes live beside them. A background loop may, if the line before
    /// says why, with `(#1393)`.
    #[test]
    fn request_handlers_confirm_leadership_rather_than_trust_one_round() {
        let found = calls_in("bun", ".is_leader()", Some("(#1393)"));
        assert!(
            found.is_empty(),
            "use bun::leadership::leads on request paths (#1393):\n{}",
            found.join("\n")
        );
    }

    /// The files that may read the council with one leadership round, how
    /// many such calls each holds, and why. Anything else uses
    /// `security_state_confirmed`. The count is exact, so a new call in an
    /// allowed file fails the test too, rather than riding on the entry.
    const SINGLE_ROUND_READS_ALLOWED: &[(&str, usize, &str)] = &[
        (
            "src/council/node.rs",
            3,
            "is_leader, security_state_linearizable and \
             desired_state_linearizable: the one-round reads themselves, which \
             confirm_leadership and security_state_confirmed build on",
        ),
        (
            "src/council/backup.rs",
            1,
            "backup_tick runs in a loop: a failed read skips one export, and \
             the next tick tries again",
        ),
    ];

    /// A linearizable read is a leadership round too: `security_state_linearizable`
    /// and its kin fail when the followers answer one heartbeat late, and a
    /// request path then answers 503 (#1937, the CA rotation routes). Only
    /// the files in [`SINGLE_ROUND_READS_ALLOWED`] may make one, and only as
    /// many as it says.
    #[test]
    fn request_paths_confirm_leadership_before_a_linearizable_read() {
        let found = calls_in("", "_linearizable()", None);
        let mut wrong = Vec::new();
        for call in &found {
            if !SINGLE_ROUND_READS_ALLOWED
                .iter()
                .any(|(path, ..)| call.starts_with(&format!("{path}:")))
            {
                wrong.push(call.clone());
            }
        }
        for (path, allowed, _) in SINGLE_ROUND_READS_ALLOWED {
            let calls: Vec<&String> = found
                .iter()
                .filter(|call| call.starts_with(&format!("{path}:")))
                .collect();
            if calls.len() != *allowed {
                wrong.push(format!(
                    "{path} makes {} one-round reads, and the allowlist says {allowed}: {calls:?}",
                    calls.len()
                ));
            }
        }
        assert!(
            wrong.is_empty(),
            "use CouncilNode::security_state_confirmed on request paths, or add \
             the call to SINGLE_ROUND_READS_ALLOWED with why (#1937):\n{}",
            wrong.join("\n")
        );
    }
}
