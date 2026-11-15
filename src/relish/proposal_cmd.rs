//! `relish proposal` and `relish undo`: the people's side of agent
//! proposals (#1682).
//!
//! `relish proposal show` prints a proposal in a fixed order: the cluster's
//! summary, reversibility and the undo command, policy findings, the blast
//! radius, the field diff, and last the author's reason, under a heading
//! that says whose words they are. `approve` prints the same and asks to
//! confirm; a lossy or irreversible proposal asks for the app's name typed
//! back. In JSON mode nothing is asked, so `--yes` is required.

use std::io::{BufRead, Write};

use crate::proposal::view::{ProposalListing, ProposalView};

use super::RelishError;
use super::client::BunClient;
use super::output::{OutputFormat, print_output};

/// `relish proposal list`: every proposal the caller's scope reaches.
pub async fn proposal_list(output: OutputFormat) -> Result<(), RelishError> {
    let listing = BunClient::default_local()
        .proposals()
        .await
        .map_err(plain_error)?;
    print_output(&Listing(listing), output)
}

/// `relish proposal show ID`.
pub async fn proposal_show(id: u64, output: OutputFormat) -> Result<(), RelishError> {
    let view = BunClient::default_local()
        .proposal(id)
        .await
        .map_err(plain_error)?;
    print_output(&Shown(view), output)
}

/// `relish proposal approve ID`: show it, confirm, approve. It applies in
/// the same step.
pub async fn proposal_approve(id: u64, yes: bool, output: OutputFormat) -> Result<(), RelishError> {
    let client = BunClient::default_local();
    let view = client.proposal(id).await.map_err(plain_error)?;
    if output == OutputFormat::Human {
        print!("{}", render_proposal(&view));
    }
    confirm_decision(&view, "approve", yes, output).await?;
    let decided = client.approve_proposal(id).await.map_err(plain_error)?;
    print_output(&Decided(decided), output)
}

/// `relish proposal reject ID --reason ...`.
pub async fn proposal_reject(
    id: u64,
    reason: &str,
    output: OutputFormat,
) -> Result<(), RelishError> {
    let decided = BunClient::default_local()
        .reject_proposal(id, reason)
        .await
        .map_err(plain_error)?;
    print_output(&Decided(decided), output)
}

/// `relish undo ID`: put an applied proposal back, if nothing has changed
/// the apps since.
pub async fn undo(id: u64, yes: bool, output: OutputFormat) -> Result<(), RelishError> {
    let client = BunClient::default_local();
    let view = client.proposal(id).await.map_err(plain_error)?;
    if output == OutputFormat::Human {
        println!("Undo proposal {id}: {}.", view.undo.effect);
    }
    confirm_decision(&view, "undo", yes, output).await?;
    let undone = client.undo_proposal(id).await.map_err(plain_error)?;
    print_output(&Decided(undone), output)
}

/// Ask before deciding: `y` for an exact proposal, the app's name for a
/// lossy or irreversible one. `--yes` skips it; JSON mode needs `--yes`.
async fn confirm_decision(
    view: &ProposalView,
    verb: &str,
    yes: bool,
    output: OutputFormat,
) -> Result<(), RelishError> {
    if yes {
        return Ok(());
    }
    if output != OutputFormat::Human {
        return Err(RelishError::InvalidFlag {
            flag: "yes".to_string(),
            reason: format!(
                "-o json asks no questions; pass --yes to {verb} proposal {}",
                view.id
            ),
        });
    }
    let typed_name = view.reversibility != "exact";
    let expected = view
        .blast_radius
        .apps
        .first()
        .and_then(|app| app.split_once('/'))
        .map(|(_, name)| name.to_string())
        .unwrap_or_default();
    let prompt = if typed_name {
        format!(
            "This is {} to undo. Type the app's name ({expected}) to {verb}: ",
            if view.reversibility == "none" {
                "impossible"
            } else {
                "lossy"
            }
        )
    } else {
        format!("{verb} proposal {}? [y/N] ", view.id)
    };
    let reply = tokio::task::spawn_blocking(move || {
        print!("{prompt}");
        let _ = std::io::stdout().flush();
        let mut line = String::new();
        std::io::stdin().lock().read_line(&mut line).map(|_| line)
    })
    .await
    .map_err(|error| RelishError::Io(std::io::Error::other(error)))?
    .map_err(RelishError::Io)?;
    let reply = reply.trim();
    let agreed = if typed_name {
        reply == expected
    } else {
        matches!(reply, "y" | "Y" | "yes")
    };
    if agreed {
        Ok(())
    } else {
        Err(RelishError::InvalidFlag {
            flag: "yes".to_string(),
            reason: format!("not confirmed; proposal {} is unchanged", view.id),
        })
    }
}

/// The API's own sentence for a refusal, not its JSON.
fn plain_error(error: RelishError) -> RelishError {
    let RelishError::ApiError { status, body } = &error else {
        return error;
    };
    match serde_json::from_str::<serde_json::Value>(body) {
        Ok(answer) => match answer["error"].as_str() {
            Some(message) => RelishError::ApiError {
                status: *status,
                body: message.to_string(),
            },
            None => error,
        },
        Err(_) => error,
    }
}

#[derive(Debug, serde::Serialize)]
#[serde(transparent)]
struct Listing(ProposalListing);

impl std::fmt::Display for Listing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", render_proposal_list(&self.0.proposals))
    }
}

#[derive(Debug, serde::Serialize)]
#[serde(transparent)]
struct Shown(ProposalView);

impl std::fmt::Display for Shown {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", render_proposal(&self.0))
    }
}

#[derive(Debug, serde::Serialize)]
#[serde(transparent)]
struct Decided(ProposalView);

impl std::fmt::Display for Decided {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let view = &self.0;
        match view.state.as_str() {
            "applied" => {
                writeln!(f, "proposal {} applied", view.id)?;
                if let Some(command) = &view.undo.command {
                    writeln!(f, "undo: {command}")?;
                }
                Ok(())
            }
            "failed" => writeln!(
                f,
                "proposal {} failed and changed nothing: {}",
                view.id,
                view.outcome.as_deref().unwrap_or("no reason given")
            ),
            state => writeln!(f, "proposal {} {state}", view.id),
        }
    }
}

/// The table `relish proposal list` prints. A policy's approval shows in
/// the BY column as `policy:<name>`.
pub fn render_proposal_list(proposals: &[ProposalView]) -> String {
    if proposals.is_empty() {
        return "No proposals.\n".to_string();
    }
    let decided_by = |view: &ProposalView| {
        view.approval
            .approved_by
            .clone()
            .or_else(|| view.approval.rejected_by.clone())
            .unwrap_or_else(|| "-".to_string())
    };
    let width = |header: &str, column: &dyn Fn(&ProposalView) -> usize| {
        proposals
            .iter()
            .map(column)
            .max()
            .unwrap_or(0)
            .max(header.len())
    };
    let author_width = width("AUTHOR", &|view| view.author.len());
    let by_width = width("BY", &|view| decided_by(view).len());
    let mut out = format!(
        "{:>4}  {:9}  {:8}  {:author_width$}  {:by_width$}  SUMMARY\n",
        "ID", "STATE", "KIND", "AUTHOR", "BY"
    );
    for view in proposals {
        let summary = view
            .summary
            .iter()
            .find(|line| !line.starts_with("UNDO IS") && !line.starts_with("CAN'T"))
            .or(view.summary.first())
            .cloned()
            .unwrap_or_default();
        out.push_str(&format!(
            "{:>4}  {:9}  {:8}  {:author_width$}  {:by_width$}  {summary}\n",
            view.id,
            view.state,
            view.kind,
            view.author,
            decided_by(view)
        ));
    }
    out
}

/// A proposal in the fixed order a person decides by.
pub fn render_proposal(view: &ProposalView) -> String {
    let mut out = format!(
        "Proposal {} ({}, {}) by {} ({})\n",
        view.id, view.kind, view.state, view.author, view.author_kind
    );
    if view.state == "pending" {
        out.push_str(&format!("Expires in {}s.\n", view.expires_in_seconds));
    }
    if let Some(outcome) = &view.outcome {
        out.push_str(&format!("Outcome: {outcome}\n"));
    }
    if let Some(by) = &view.approval.approved_by {
        out.push_str(&format!("Approved by {by}.\n"));
    }
    out.push('\n');
    for line in &view.summary {
        out.push_str(&format!("{line}\n"));
    }
    out.push_str(&format!(
        "\nReversibility: {}. Undo: {} ({}).\n",
        view.reversibility,
        view.undo.command.as_deref().unwrap_or("none"),
        view.undo.effect
    ));
    if !view.findings.is_empty() {
        out.push_str("\nPolicy findings:\n");
        for finding in &view.findings {
            out.push_str(&format!("  {finding}\n"));
        }
    }
    let radius = &view.blast_radius;
    out.push_str("\nBlast radius:\n");
    out.push_str(&format!("  apps: {}\n", radius.apps.join(", ")));
    out.push_str(&format!(
        "  nodes: {}\n",
        if radius.nodes.is_empty() {
            "none yet".to_string()
        } else {
            radius.nodes.join(", ")
        }
    ));
    out.push_str(&format!(
        "  instances: {} replaced, {} started, {} stopped\n",
        radius.instances_replaced, radius.instances_started, radius.instances_stopped
    ));
    // The scheduler's dry run, for the instances it starts (#1682).
    if !radius.new_nodes_forecast.is_empty() {
        out.push_str(&format!(
            "  forecast: new instances on {}\n",
            radius.new_nodes_forecast.join(", ")
        ));
    }
    if radius.unplaced_forecast > 0 {
        out.push_str(&format!(
            "  forecast: can't place {}{}\n",
            radius.unplaced_forecast,
            radius
                .forecast_note
                .as_deref()
                .map(|note| format!(" ({note})"))
                .unwrap_or_default()
        ));
    } else if let Some(note) = &radius.forecast_note {
        out.push_str(&format!("  forecast: none ({note})\n"));
    }
    if !radius.routes.is_empty() {
        out.push_str(&format!("  routes: {}\n", radius.routes.join(", ")));
    }
    if !radius.services.is_empty() {
        out.push_str(&format!("  services: {}\n", radius.services.join(", ")));
    }
    for unknown in &radius.unknown {
        out.push_str(&format!("  unknown: {unknown}\n"));
    }
    if !view.diff.is_empty() {
        out.push_str("\nChanges:\n");
        for change in &view.diff {
            out.push_str(&format!(
                "  {} {}: {} -> {}\n",
                change.app,
                change.field,
                change.from.as_deref().unwrap_or("(none)"),
                change.to.as_deref().unwrap_or("(none)")
            ));
        }
    }
    out.push_str(&format!(
        "\nThe {}'s reason (its own words, unverified):\n  {}\n",
        if view.author_kind == "agent" {
            "agent"
        } else {
            "author"
        },
        view.reason
    ));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn view() -> ProposalView {
        serde_json::from_value(serde_json::json!({
            "id": 17, "state": "pending", "kind": "scale", "undo_of": null,
            "namespace": "shop", "author": "holmes", "author_kind": "agent",
            "summary": [
                "Scale shop/web from 3 to 5 replicas.",
                "Touches 1 app on 2 nodes (node-a, node-b); starts 2 instances, forecast on node-c.",
                "Serves https://shop.example.com/ and the service web.shop.",
                "Undo: `relish undo 17` puts web back to 3 replicas."
            ],
            "reversibility": "exact",
            "undo": {"command": "relish undo 17", "effect": "puts web back to 3 replicas"},
            "findings": ["policy limits warns about app shop/web: no memory limit"],
            "blast_radius": {"apps": ["shop/web"], "nodes": ["node-a", "node-b"],
                "new_nodes_forecast": ["node-c"], "instances_replaced": 0, "instances_started": 2,
                "instances_stopped": 0, "routes": ["shop.example.com/"],
                "services": ["web.shop"], "unknown": []},
            "diff": [{"app": "shop/web", "field": "replicas", "from": "3", "to": "5"}],
            "approval": {"needs": "human", "approved_by": null, "rejected_by": null, "undone_by": null},
            "leases": ["app:shop/web"], "revisions": [],
            "created_at_unix_ms": 1, "expires_at_unix_ms": 2, "expires_in_seconds": 3540,
            "outcome": null, "compacted": false,
            "reason": "p99 over the SLO for 10 minutes"
        }))
        .unwrap()
    }

    #[test]
    fn show_prints_the_clusters_words_first_and_the_agents_last() {
        let shown = render_proposal(&view());
        insta::assert_snapshot!(shown);
        let summary = shown.find("Scale shop/web").unwrap();
        let reason = shown.find("its own words, unverified").unwrap();
        assert!(summary < reason);
    }

    #[test]
    fn the_list_marks_a_policy_approval() {
        let mut approved = view();
        approved.id = 18;
        approved.state = "applied".to_string();
        approved.approval.approved_by = Some("policy:agents-scale-shop".to_string());
        assert_eq!(render_proposal_list(&[]), "No proposals.\n");
        insta::assert_snapshot!(render_proposal_list(&[approved, view()]));
    }

    #[test]
    fn a_refusal_prints_the_apis_sentence() {
        let error = plain_error(RelishError::ApiError {
            status: 403,
            body: r#"{"error":"holmes made proposal 17 and can't approve it","code":"forbidden"}"#
                .to_string(),
        });
        assert_eq!(
            error.to_string(),
            "API error (status 403): holmes made proposal 17 and can't approve it"
        );
    }

    #[tokio::test]
    async fn json_mode_needs_yes() {
        let error = confirm_decision(&view(), "approve", false, OutputFormat::Json)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("--yes"), "{error}");
        assert!(
            confirm_decision(&view(), "approve", true, OutputFormat::Json)
                .await
                .is_ok()
        );
    }
}
