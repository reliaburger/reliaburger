//! `relish os`: the appliance OS across the cluster (W6). What each node
//! runs, the newest release, and the rollout the leader runs to move the
//! fleet to a version (`crate::os::rollout`).

use crate::os::rollout::{OsNodePhase, OsRollout, OsRolloutPhase, OsUpdateState};
use crate::relish::RelishError;
use crate::relish::client::BunClient;

/// One node's OS, as `relish os list` shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NodeOs {
    pub node_id: String,
    /// `None` if the node isn't an appliance or didn't answer.
    pub version: Option<String>,
    pub update: OsUpdateState,
}

/// The newest release the channel names, checked against the release keys.
pub async fn newest(channel_url: &str) -> Result<String, RelishError> {
    let keys = crate::upgrade::keys::release_keys(&Default::default())
        .map_err(|e| failed(&e.to_string()))?;
    let client = reqwest::Client::builder()
        .user_agent("relish")
        .build()
        .map_err(|e| failed(&e.to_string()))?;
    let channel = super::image::fetch(&client, channel_url).await?;
    let signature = super::image::fetch(&client, &format!("{channel_url}.sig")).await?;
    let channel = crate::os::OsChannel::verified(&channel, &signature, &keys)
        .map_err(|e| failed(&e.to_string()))?;
    Ok(channel.version)
}

/// Ask every node what it runs.
pub async fn node_versions(client: &BunClient) -> Result<Vec<NodeOs>, RelishError> {
    let mut nodes = Vec::new();
    for node in client.nodes().await? {
        let answer = match client.for_node(&node) {
            Ok(node_client) => node_client.version_json().await.ok(),
            Err(_) => None,
        };
        let probe = answer
            .as_ref()
            .map(|value| crate::os::rollout::probe_from_version(value, true));
        nodes.push(NodeOs {
            node_id: node.node_id,
            version: probe.as_ref().and_then(|p| p.os_version.clone()),
            update: probe.map(|p| p.update).unwrap_or_default(),
        });
    }
    nodes.sort_by(|a, b| a.node_id.cmp(&b.node_id));
    Ok(nodes)
}

/// The `relish os list` table.
pub fn render_list(newest: Option<&str>, nodes: &[NodeOs]) -> String {
    let mut out = match newest {
        Some(version) => format!("Newest OS release: {version}\n\n"),
        None => "Newest OS release: unknown (the channel couldn't be read)\n\n".to_string(),
    };
    out.push_str(&format!("{:<20} {:<12} {}\n", "NODE", "OS", "NOTE"));
    for node in nodes {
        let note = match (&node.update, &node.version) {
            (OsUpdateState::Staging { target }, _) => format!("staging {target}"),
            (OsUpdateState::Rebooting { target }, _) => format!("rebooting into {target}"),
            (OsUpdateState::Failed { target, reason }, _) => {
                format!("update to {target} failed: {reason}")
            }
            (_, None) => "not an appliance, or not answering".to_string(),
            (_, Some(version)) if Some(version.as_str()) != newest && newest.is_some() => {
                "update available".to_string()
            }
            _ => String::new(),
        };
        out.push_str(
            format!(
                "{:<20} {:<12} {note}",
                node.node_id,
                node.version.as_deref().unwrap_or("-")
            )
            .trim_end(),
        );
        out.push('\n');
    }
    out
}

/// A rollout, as `relish os status` shows it.
pub fn render_rollout(rollout: &OsRollout) -> String {
    let phase = match &rollout.phase {
        OsRolloutPhase::Running => "running".to_string(),
        OsRolloutPhase::Paused { reason } => format!(
            "paused: {reason}\n  `relish os resume` retries that node; `relish os abort` stops"
        ),
        OsRolloutPhase::Completed => "complete".to_string(),
        OsRolloutPhase::Aborted { reason } => format!("aborted: {reason}"),
    };
    let mut out = format!(
        "OS rollout {} to {}: {phase}\n\n{:<20} {:<8} {:<12} {}\n",
        rollout.rollout_id, rollout.target, "NODE", "ROLE", "FROM", "PHASE"
    );
    for node in &rollout.nodes {
        let phase = match &node.phase {
            OsNodePhase::Pending => "waiting".to_string(),
            OsNodePhase::Draining => "moving workloads off".to_string(),
            OsNodePhase::Updating => "updating".to_string(),
            OsNodePhase::Done => "done".to_string(),
            OsNodePhase::Skipped => "on the target already".to_string(),
            OsNodePhase::Failed { reason } => format!("failed: {reason}"),
        };
        out.push_str(&format!(
            "{:<20} {:<8} {:<12} {phase}\n",
            node.node_id,
            if node.council { "council" } else { "worker" },
            node.from_version
        ));
    }
    out
}

/// `relish os list`.
pub async fn list(channel_url: &str) -> Result<(), RelishError> {
    let newest = match newest(channel_url).await {
        Ok(version) => Some(version),
        Err(error) => {
            eprintln!("warning: {error}");
            None
        }
    };
    let nodes = node_versions(&BunClient::default_local()).await?;
    print!("{}", render_list(newest.as_deref(), &nodes));
    Ok(())
}

/// `relish os upgrade [VERSION]`: the newest release unless named.
pub async fn upgrade(
    version: Option<String>,
    channel_url: &str,
    allow_downgrade: bool,
) -> Result<(), RelishError> {
    let version = match version {
        Some(version) => version,
        None => newest(channel_url).await?,
    };
    let answer = BunClient::default_local()
        .os_rollout_start(&version, channel_url, allow_downgrade)
        .await?;
    let rollout: OsRollout =
        serde_json::from_value(answer).map_err(|e| failed(&format!("the answer: {e}")))?;
    print!("{}", render_rollout(&rollout));
    println!("\nFollow it with `relish os status`.");
    Ok(())
}

/// `relish os status`.
pub async fn status() -> Result<(), RelishError> {
    let answer = BunClient::default_local().os_rollout().await?;
    let active: Option<OsRollout> = serde_json::from_value(answer["active"].clone())
        .map_err(|e| failed(&format!("the answer: {e}")))?;
    let history: Vec<OsRollout> =
        serde_json::from_value(answer["history"].clone()).unwrap_or_default();
    match (active, history.last()) {
        (Some(rollout), _) => print!("{}", render_rollout(&rollout)),
        (None, Some(last)) => {
            println!("No OS rollout in progress. The last one:\n");
            print!("{}", render_rollout(last));
        }
        (None, None) => println!("No OS rollout in progress, and none has run."),
    }
    Ok(())
}

/// `relish os resume`.
pub async fn resume() -> Result<(), RelishError> {
    let answer = BunClient::default_local().os_rollout_resume().await?;
    let rollout: OsRollout =
        serde_json::from_value(answer).map_err(|e| failed(&format!("the answer: {e}")))?;
    print!("{}", render_rollout(&rollout));
    Ok(())
}

/// `relish os abort`.
pub async fn abort() -> Result<(), RelishError> {
    let answer = BunClient::default_local().os_rollout_abort().await?;
    println!(
        "Aborted OS rollout {}. Nodes keep the version they're on.",
        answer["aborted"].as_str().unwrap_or("?")
    );
    Ok(())
}

fn failed(message: &str) -> RelishError {
    RelishError::InitFailed(message.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_list_shows_who_is_behind_and_who_is_busy() {
        let nodes = [
            NodeOs {
                node_id: "home-1".into(),
                version: Some("2026.42.0".into()),
                update: OsUpdateState::Idle,
            },
            NodeOs {
                node_id: "home-2".into(),
                version: Some("2026.41.0".into()),
                update: OsUpdateState::Idle,
            },
            NodeOs {
                node_id: "home-3".into(),
                version: Some("2026.41.0".into()),
                update: OsUpdateState::Staging {
                    target: "2026.42.0".into(),
                },
            },
            NodeOs {
                node_id: "laptop".into(),
                version: None,
                update: OsUpdateState::Idle,
            },
        ];
        assert_eq!(
            render_list(Some("2026.42.0"), &nodes),
            "Newest OS release: 2026.42.0\n\n\
             NODE                 OS           NOTE\n\
             home-1               2026.42.0\n\
             home-2               2026.41.0    update available\n\
             home-3               2026.41.0    staging 2026.42.0\n\
             laptop               -            not an appliance, or not answering\n"
        );
    }

    #[test]
    fn a_paused_rollout_says_why_and_what_to_do() {
        let mut rollout = crate::os::rollout::plan(
            "os-1",
            "2026.42.0",
            "https://example/os-channel.json",
            "home-1",
            vec![
                ("home-1".into(), "a".into(), true, "2026.41.0".into()),
                ("home-4".into(), "b".into(), false, "2026.41.0".into()),
            ],
            0,
        );
        rollout.nodes[0].phase = OsNodePhase::Failed {
            reason: "booted 2026.41.0 instead".into(),
        };
        rollout.phase = OsRolloutPhase::Paused {
            reason: "home-4: booted 2026.41.0 instead".into(),
        };
        assert_eq!(
            render_rollout(&rollout),
            "OS rollout os-1 to 2026.42.0: paused: home-4: booted 2026.41.0 instead\n  \
             `relish os resume` retries that node; `relish os abort` stops\n\n\
             NODE                 ROLE     FROM         PHASE\n\
             home-4               worker   2026.41.0    failed: booted 2026.41.0 instead\n\
             home-1               council  2026.41.0    waiting\n"
        );
    }
}
