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
pub const LEADER_CONFIRMATION: std::time::Duration = std::time::Duration::from_secs(3);

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
    /// No request handler under `src/bun/api/` decides with a single round.
    /// A background loop may, if the line before says why, with `(#1393)`.
    #[test]
    fn request_handlers_confirm_leadership_rather_than_trust_one_round() {
        let api = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/bun/api");
        let mut found = Vec::new();
        for entry in std::fs::read_dir(&api).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().is_none_or(|e| e != "rs") || path.ends_with("tests.rs") {
                continue;
            }
            let source = std::fs::read_to_string(&path).unwrap();
            let lines: Vec<&str> = source.lines().collect();
            for (index, line) in lines.iter().enumerate() {
                let explained = index > 0 && lines[index - 1].contains("(#1393)");
                if line.contains(".is_leader()") && !explained {
                    found.push(format!("{}:{}: {}", path.display(), index + 1, line.trim()));
                }
            }
        }
        assert!(
            found.is_empty(),
            "use bun::leadership::leads on request paths (#1393):\n{}",
            found.join("\n")
        );
    }
}
