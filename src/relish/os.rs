//! `relish os`: the appliance OS across the cluster (W6). What each node
//! runs, the newest release, and the rollout the leader runs to move the
//! fleet to a version (`crate::os::rollout`).

use std::fmt;

use serde::Serialize;

use crate::os::rollout::{OsNodePhase, OsRollout, OsRolloutPhase, OsUpdateState};
use crate::relish::RelishError;
use crate::relish::client::BunClient;
use crate::relish::image::ChannelTrust;
use crate::relish::output::{OutputFormat, print_output, progress};

/// One node's OS, as `relish os list` shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct NodeOs {
    pub node_id: String,
    /// `None` if the node isn't an appliance or didn't answer.
    pub version: Option<String>,
    pub update: OsUpdateState,
}

/// The newest release the channel names, checked against `trust`: the
/// release keys, or a lab build's key given with `--key`.
pub async fn newest(channel_url: &str, trust: &ChannelTrust) -> Result<String, RelishError> {
    if let Some(warning) = trust.warning() {
        eprintln!("relish os: {warning}");
    }
    let client = crate::http_client::builder()
        .user_agent("relish")
        .build()
        .map_err(|e| failed(&e.to_string()))?;
    let channel = super::image::fetch(&client, channel_url).await?;
    let signature = super::image::fetch(&client, &format!("{channel_url}.sig")).await?;
    Ok(trust.verify(&channel, &signature)?.version)
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

/// What `relish os list` answers: the newest release and each node.
#[derive(Debug, Clone, Serialize)]
pub struct OsList {
    /// `None` if the channel couldn't be read.
    pub newest: Option<String>,
    pub nodes: Vec<NodeOs>,
}

impl fmt::Display for OsList {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&render_list(self.newest.as_deref(), &self.nodes))
    }
}

/// One rollout as `relish os upgrade` and `resume` answer it: the
/// rollout's own record as data, the table for people.
#[derive(Debug, Clone, Serialize)]
#[serde(transparent)]
pub struct RolloutReport(pub OsRollout);

impl fmt::Display for RolloutReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&render_rollout(&self.0))
    }
}

/// What `relish os status` answers: the rollout in progress, and the last
/// one that finished.
#[derive(Debug, Clone, Serialize)]
pub struct OsStatus {
    pub active: Option<OsRollout>,
    pub last: Option<OsRollout>,
}

impl fmt::Display for OsStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match (&self.active, &self.last) {
            (Some(rollout), _) => f.write_str(&render_rollout(rollout)),
            (None, Some(last)) => {
                writeln!(f, "No OS rollout in progress. The last one:\n")?;
                f.write_str(&render_rollout(last))
            }
            (None, None) => writeln!(f, "No OS rollout in progress, and none has run."),
        }
    }
}

/// What `relish os abort` answers: the rollout it stopped.
#[derive(Debug, Clone, Serialize)]
pub struct Aborted {
    pub aborted: String,
}

impl fmt::Display for Aborted {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            f,
            "Aborted OS rollout {}. Nodes keep the version they're on.",
            self.aborted
        )
    }
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
pub async fn list(
    channel_url: &str,
    trust: &ChannelTrust,
    output: OutputFormat,
) -> Result<(), RelishError> {
    let newest = match newest(channel_url, trust).await {
        Ok(version) => Some(version),
        Err(error) => {
            eprintln!("warning: {error}");
            None
        }
    };
    let nodes = node_versions(&BunClient::default_local()).await?;
    print_output(&OsList { newest, nodes }, output)
}

/// `relish os upgrade [VERSION]`: the newest release unless named. Only
/// the newest is read from the channel, so `trust` matters only then; the
/// nodes check the release itself against their own image's key.
pub async fn upgrade(
    version: Option<String>,
    channel_url: &str,
    trust: &ChannelTrust,
    allow_downgrade: bool,
    output: OutputFormat,
) -> Result<(), RelishError> {
    let version = match version {
        Some(version) => version,
        None => newest(channel_url, trust).await?,
    };
    let answer = BunClient::default_local()
        .os_rollout_start(&version, channel_url, allow_downgrade)
        .await?;
    let rollout: OsRollout =
        serde_json::from_value(answer).map_err(|e| failed(&format!("the answer: {e}")))?;
    print_output(&RolloutReport(rollout), output)?;
    progress(output, "\nFollow it with `relish os status`.");
    Ok(())
}

/// `relish os status`.
pub async fn status(output: OutputFormat) -> Result<(), RelishError> {
    let answer = BunClient::default_local().os_rollout().await?;
    let active: Option<OsRollout> = serde_json::from_value(answer["active"].clone())
        .map_err(|e| failed(&format!("the answer: {e}")))?;
    let history: Vec<OsRollout> =
        serde_json::from_value(answer["history"].clone()).unwrap_or_default();
    print_output(
        &OsStatus {
            active,
            last: history.into_iter().last(),
        },
        output,
    )
}

/// `relish os resume`.
pub async fn resume(output: OutputFormat) -> Result<(), RelishError> {
    let answer = BunClient::default_local().os_rollout_resume().await?;
    let rollout: OsRollout =
        serde_json::from_value(answer).map_err(|e| failed(&format!("the answer: {e}")))?;
    print_output(&RolloutReport(rollout), output)
}

/// `relish os abort`.
pub async fn abort(output: OutputFormat) -> Result<(), RelishError> {
    let answer = BunClient::default_local().os_rollout_abort().await?;
    let aborted = answer["aborted"]
        .as_str()
        .ok_or_else(|| failed("the answer names no rollout"))?;
    print_output(
        &Aborted {
            aborted: aborted.to_string(),
        },
        output,
    )
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
    fn the_list_as_data_keeps_each_nodes_update() {
        let list = OsList {
            newest: None,
            nodes: vec![NodeOs {
                node_id: "home-3".into(),
                version: Some("2026.41.0".into()),
                update: OsUpdateState::Staging {
                    target: "2026.42.0".into(),
                },
            }],
        };
        let json = serde_json::to_value(&list).unwrap();
        assert_eq!(json["newest"], serde_json::Value::Null);
        assert_eq!(json["nodes"][0]["node_id"], "home-3");
        assert_eq!(json["nodes"][0]["version"], "2026.41.0");
        assert_eq!(
            json["nodes"][0]["update"],
            serde_json::to_value(&list.nodes[0].update).unwrap()
        );
    }

    #[test]
    fn a_status_with_no_rollout_is_two_nulls_as_data_and_a_sentence_for_people() {
        let status = OsStatus {
            active: None,
            last: None,
        };
        assert_eq!(
            serde_json::to_value(&status).unwrap(),
            serde_json::json!({"active": null, "last": null})
        );
        assert_eq!(
            status.to_string(),
            "No OS rollout in progress, and none has run.\n"
        );
    }

    #[test]
    fn a_rollout_report_is_the_rollouts_own_record() {
        let rollout = crate::os::rollout::plan(
            "os-1",
            "2026.42.0",
            "https://example/os-channel.json",
            "home-1",
            vec![("home-1".into(), "a".into(), true, "2026.41.0".into())],
            0,
        );
        assert_eq!(
            serde_json::to_value(RolloutReport(rollout.clone())).unwrap(),
            serde_json::to_value(&rollout).unwrap()
        );
        assert_eq!(
            serde_json::to_value(Aborted {
                aborted: "os-1".into()
            })
            .unwrap(),
            serde_json::json!({"aborted": "os-1"})
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
