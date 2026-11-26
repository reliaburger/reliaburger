//! `relish os`: the appliance OS across the cluster (W6). What each node
//! runs, the newest release, the rollout the leader runs to move the
//! fleet to a version (`crate::os::rollout`), and where each node sends its
//! boot logs (`crate::os::netlog`).

use std::fmt;
use std::path::Path;

use serde::Serialize;

use crate::appliance::settings::ApplianceSettings;
use crate::os::netlog::NetlogTarget;
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
    let client =
        super::image::download_client(super::image::CONNECT_TIMEOUT, super::image::IDLE_TIMEOUT)?;
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

/// What `relish os boot-logs` was asked to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum BootLogsRequest {
    /// Show where each node sends its boot logs.
    Show,
    /// Send them to `target` from now on.
    To { target: NetlogTarget },
    /// Stop sending them.
    Off,
}

impl BootLogsRequest {
    /// The request for `--to` and `--off`: neither shows.
    pub fn from_flags(to: Option<NetlogTarget>, off: bool) -> Self {
        match (to, off) {
            (Some(target), _) => Self::To { target },
            (None, true) => Self::Off,
            (None, false) => Self::Show,
        }
    }
}

/// Why a node with no `/v1/os/boot-logs` route can't answer.
const TOO_OLD: &str = "this node's bun predates relish os boot-logs (0.3.3); upgrade it first";

/// One node's boot logs, as it answered.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum BootLogState {
    /// They go to `target`. `warning` says why a change waits for the next
    /// boot.
    On {
        target: NetlogTarget,
        #[serde(skip_serializing_if = "Option::is_none")]
        warning: Option<String>,
    },
    /// They stay on the machine.
    Off {
        #[serde(skip_serializing_if = "Option::is_none")]
        warning: Option<String>,
    },
    /// The node runs bun on another OS: it has no ESP to keep the setting.
    NotAnAppliance,
    /// The node didn't answer, or couldn't do it.
    Failed { reason: String },
}

impl BootLogState {
    /// The state a node's answer to `GET` or `POST /v1/os/boot-logs` gives.
    pub fn from_answer(answer: Result<serde_json::Value, RelishError>) -> Self {
        let value = match answer {
            Ok(value) => value,
            Err(RelishError::ApiError { status: 409, body })
                if body.contains("isn't an appliance") =>
            {
                return Self::NotAnAppliance;
            }
            // A bun from before 0.3.3 has no such route, and answers a bare
            // 404 (#2323).
            Err(RelishError::ApiError { status: 404, .. }) => {
                return Self::Failed {
                    reason: TOO_OLD.to_string(),
                };
            }
            Err(error) => {
                return Self::Failed {
                    reason: error.to_string(),
                };
            }
        };
        let warning = value["warning"].as_str().map(str::to_string);
        match &value["target"] {
            serde_json::Value::Null => Self::Off { warning },
            target => match serde_json::from_value(target.clone()) {
                Ok(target) => Self::On { target, warning },
                Err(e) => Self::Failed {
                    reason: format!("the answer: {e}"),
                },
            },
        }
    }
}

/// One node in `relish os boot-logs`'s answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct NodeBootLogs {
    pub node_id: String,
    #[serde(flatten)]
    pub state: BootLogState,
}

/// What `relish os boot-logs` answers: what it was asked, and each node.
#[derive(Debug, Clone, Serialize)]
pub struct BootLogsReport {
    pub request: BootLogsRequest,
    pub nodes: Vec<NodeBootLogs>,
}

impl BootLogsReport {
    /// Why the command fails after printing: a change that some node may
    /// not have made, or that no node could.
    pub fn failure(&self) -> Option<String> {
        if self.request == BootLogsRequest::Show {
            return None;
        }
        let failed: Vec<&str> = self
            .nodes
            .iter()
            .filter(|node| matches!(node.state, BootLogState::Failed { .. }))
            .map(|node| node.node_id.as_str())
            .collect();
        if !failed.is_empty() {
            // A node may have written the file and then lost the
            // connection, so its answer doesn't prove nothing changed.
            return Some(format!(
                "boot logs may not have changed on {}; run `relish os boot-logs` to check",
                failed.join(", ")
            ));
        }
        if self
            .nodes
            .iter()
            .all(|node| node.state == BootLogState::NotAnAppliance)
        {
            return Some("no node here is an appliance".to_string());
        }
        None
    }
}

impl fmt::Display for BootLogsReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.request {
            BootLogsRequest::Show => {}
            BootLogsRequest::To { target } => writeln!(
                f,
                "Boot logs go to {target} over UDP, in cleartext, from now on. \
                 `relish machines logs` on that machine shows them.\n"
            )?,
            BootLogsRequest::Off => writeln!(f, "Boot logs stay on each machine from now on.\n")?,
        }
        writeln!(f, "{:<20} BOOT LOGS", "NODE")?;
        for node in &self.nodes {
            let (shown, warning) = match &node.state {
                BootLogState::On { target, warning } => (format!("to {target}"), warning.as_ref()),
                BootLogState::Off { warning } => ("off".to_string(), warning.as_ref()),
                BootLogState::NotAnAppliance => ("not an appliance".to_string(), None),
                BootLogState::Failed { reason } => (format!("failed: {reason}"), None),
            };
            match warning {
                Some(warning) => writeln!(f, "{:<20} {shown} ({warning})", node.node_id)?,
                None => writeln!(f, "{:<20} {shown}", node.node_id)?,
            }
        }
        Ok(())
    }
}

/// `relish os boot-logs [--to <address>[:<port>] | --off] [--node <id>]…`:
/// show or change where each appliance sends its boot logs. Each node's
/// bun keeps the setting in `reliaburger/netlog.conf` on its ESP, which
/// every boot reads.
pub async fn boot_logs(
    request: BootLogsRequest,
    only: &[String],
    output: OutputFormat,
) -> Result<(), RelishError> {
    let client = BunClient::default_local();
    let mut nodes = client.nodes().await?;
    if let Some(missing) = only
        .iter()
        .find(|id| !nodes.iter().any(|node| &node.node_id == *id))
    {
        return Err(failed(&format!("no node {missing} in this cluster")));
    }
    if !only.is_empty() {
        nodes.retain(|node| only.contains(&node.node_id));
    }
    let mut answers = Vec::new();
    for node in nodes {
        let answer = match client.for_node(&node) {
            Ok(node_client) => match request {
                BootLogsRequest::Show => node_client.os_boot_logs().await,
                BootLogsRequest::To { target } => node_client.os_boot_logs_set(Some(target)).await,
                BootLogsRequest::Off => node_client.os_boot_logs_set(None).await,
            },
            Err(error) => Err(error),
        };
        answers.push(NodeBootLogs {
            node_id: node.node_id,
            state: BootLogState::from_answer(answer),
        });
    }
    answers.sort_by(|a, b| a.node_id.cmp(&b.node_id));
    let report = BootLogsReport {
        request,
        nodes: answers,
    };
    print_output(&report, output)?;
    match report.failure() {
        Some(reason) => Err(failed(&reason)),
        None => Ok(()),
    }
}

/// The cluster's `[appliance]` settings, as `relish os settings` shows them: the TOML section itself.
#[derive(Debug, Clone, Serialize)]
pub struct SettingsReport(pub ApplianceSettings);

impl fmt::Display for SettingsReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0.to_toml())?;
        if self.0.console_chaos {
            writeln!(
                f,
                "\nKill mode is on: a hit in a console's invaders game kills that instance."
            )
        } else {
            writeln!(
                f,
                "\nKill mode is off: a hit in a console's invaders game only pops the invader."
            )
        }
    }
}

/// `relish os settings`: the cluster's `[appliance]` settings (#2151).
pub async fn settings(output: OutputFormat) -> Result<(), RelishError> {
    let settings = BunClient::default_local().os_settings().await?;
    print_output(&SettingsReport(settings), output)
}

/// `relish os settings --set FILE`: replace the cluster's `[appliance]` settings with
/// the `[appliance]` section of `path` (cluster admin; audited).
pub async fn set_settings(path: &Path, output: OutputFormat) -> Result<(), RelishError> {
    let text = tokio::fs::read_to_string(path)
        .await
        .map_err(|e| failed(&format!("can't read {}: {e}", path.display())))?;
    let wanted = ApplianceSettings::from_toml(&text)
        .map_err(|e| failed(&format!("{}: {e}", path.display())))?;
    let settings = BunClient::default_local().os_settings_set(&wanted).await?;
    print_output(&SettingsReport(settings), output)
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

    fn target(value: &str) -> NetlogTarget {
        value.parse().unwrap()
    }

    fn api_error(status: u16, body: &str) -> RelishError {
        RelishError::ApiError {
            status,
            body: body.to_string(),
        }
    }

    #[test]
    fn neither_flag_shows_the_setting() {
        assert_eq!(
            BootLogsRequest::from_flags(None, false),
            BootLogsRequest::Show
        );
        assert_eq!(
            BootLogsRequest::from_flags(None, true),
            BootLogsRequest::Off
        );
        assert_eq!(
            BootLogsRequest::from_flags(Some(target("10.0.0.1")), false),
            BootLogsRequest::To {
                target: target("10.0.0.1:6666")
            }
        );
    }

    #[test]
    fn a_nodes_answer_becomes_its_boot_log_state() {
        assert_eq!(
            BootLogState::from_answer(Ok(serde_json::json!({"target": "10.42.0.1:6666"}))),
            BootLogState::On {
                target: target("10.42.0.1"),
                warning: None
            }
        );
        assert_eq!(
            BootLogState::from_answer(Ok(
                serde_json::json!({"target": null, "warning": "boot logs stop at the next boot"})
            )),
            BootLogState::Off {
                warning: Some("boot logs stop at the next boot".into())
            }
        );
        assert_eq!(
            BootLogState::from_answer(Err(api_error(
                409,
                r#"{"error":"this node isn't an appliance, so it has no boot logs to send"}"#
            ))),
            BootLogState::NotAnAppliance
        );
        assert_eq!(
            BootLogState::from_answer(Err(api_error(404, ""))),
            BootLogState::Failed {
                reason: "this node's bun predates relish os boot-logs (0.3.3); upgrade it first"
                    .into()
            },
            "a 0.3.2 bun has no boot-logs route"
        );
        assert!(matches!(
            BootLogState::from_answer(Err(api_error(403, "forbidden"))),
            BootLogState::Failed { .. }
        ));
        assert!(matches!(
            BootLogState::from_answer(Ok(serde_json::json!({"target": "laptop"}))),
            BootLogState::Failed { .. }
        ));
    }

    fn report(request: BootLogsRequest, states: Vec<(&str, BootLogState)>) -> BootLogsReport {
        BootLogsReport {
            request,
            nodes: states
                .into_iter()
                .map(|(node_id, state)| NodeBootLogs {
                    node_id: node_id.into(),
                    state,
                })
                .collect(),
        }
    }

    #[test]
    fn the_boot_log_table_shows_each_node_and_why_a_change_waits() {
        let shown = report(
            BootLogsRequest::To {
                target: target("10.42.0.1"),
            },
            vec![
                (
                    "home-1",
                    BootLogState::On {
                        target: target("10.42.0.1"),
                        warning: None,
                    },
                ),
                (
                    "home-2",
                    BootLogState::On {
                        target: target("10.42.0.1"),
                        warning: Some("systemctl failed; boot logs start at the next boot".into()),
                    },
                ),
                ("laptop", BootLogState::NotAnAppliance),
            ],
        );
        assert_eq!(
            shown.to_string(),
            "Boot logs go to 10.42.0.1:6666 over UDP, in cleartext, from now on. \
             `relish machines logs` on that machine shows them.\n\n\
             NODE                 BOOT LOGS\n\
             home-1               to 10.42.0.1:6666\n\
             home-2               to 10.42.0.1:6666 (systemctl failed; boot logs start at the next boot)\n\
             laptop               not an appliance\n"
        );
        assert_eq!(shown.failure(), None);
        let off = report(
            BootLogsRequest::Show,
            vec![("home-1", BootLogState::Off { warning: None })],
        );
        assert_eq!(
            off.to_string(),
            "NODE                 BOOT LOGS\nhome-1               off\n"
        );
    }

    #[test]
    fn boot_logs_as_data_name_the_request_and_each_nodes_state() {
        let shown = report(
            BootLogsRequest::To {
                target: target("10.42.0.1"),
            },
            vec![
                (
                    "home-1",
                    BootLogState::On {
                        target: target("10.42.0.1"),
                        warning: None,
                    },
                ),
                ("home-2", BootLogState::Off { warning: None }),
                ("laptop", BootLogState::NotAnAppliance),
                (
                    "gone",
                    BootLogState::Failed {
                        reason: "timed out".into(),
                    },
                ),
            ],
        );
        assert_eq!(
            serde_json::to_value(&shown).unwrap(),
            serde_json::json!({
                "request": {"action": "to", "target": "10.42.0.1:6666"},
                "nodes": [
                    {"node_id": "home-1", "state": "on", "target": "10.42.0.1:6666"},
                    {"node_id": "home-2", "state": "off"},
                    {"node_id": "laptop", "state": "not_an_appliance"},
                    {"node_id": "gone", "state": "failed", "reason": "timed out"},
                ]
            })
        );
    }

    #[test]
    fn a_change_fails_when_a_node_missed_it_or_none_could_make_it() {
        let failed = BootLogState::Failed {
            reason: "timed out".into(),
        };
        assert_eq!(
            report(BootLogsRequest::Off, vec![("gone", failed.clone())]).failure(),
            Some(
                "boot logs may not have changed on gone; run `relish os boot-logs` to check".into()
            )
        );
        assert_eq!(
            report(
                BootLogsRequest::Off,
                vec![("laptop", BootLogState::NotAnAppliance)]
            )
            .failure(),
            Some("no node here is an appliance".into())
        );
        // Only showing never fails: an unreachable node is in the table.
        assert_eq!(
            report(BootLogsRequest::Show, vec![("gone", failed)]).failure(),
            None
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
