//! Lifecycle commands restricted to the resources in a locked operation record.

use super::{lima::Lima, security, state::Operation};
use crate::relish::local_context::{self, LocalContext};
use anyhow::{Context, Result, bail};
use std::time::Duration;

/// Supported actions for an existing managed cluster.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Print VM status and live API readiness.
    Status,
    /// Boot the owned VMs and check the authenticated APIs.
    Start,
    /// Shut down the owned VMs without removing their data.
    Stop,
    /// Permanently remove the owned VMs and their credentials.
    Destroy,
}

/// Observed state of one managed node, independent of its saved provisioning phase.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum NodeCondition {
    /// VM and authenticated critical-subsystem checks succeeded.
    Ready,
    /// The owned VM no longer exists.
    Missing,
    /// Lima reports a state other than Running.
    NotRunning { vm_state: String },
    /// The running VM did not prove API and critical-subsystem readiness.
    ApiNotReady { reason: String },
    /// VM state could not be observed.
    Unknown { reason: String },
}

/// Structured status for one exact VM owned by this managed operation.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct NodeStatus {
    /// Saved, validated VM name.
    pub name: String,
    /// Fresh VM and API evidence.
    pub condition: NodeCondition,
}

/// Inspect every owned node, retaining failures as explicit observations.
pub async fn collect_status(
    lima: &Lima,
    operation: &Operation,
    bootstrap: &security::Bootstrap,
) -> Vec<NodeStatus> {
    let mut nodes = Vec::new();
    for (index, node) in operation.state.nodes.iter().enumerate() {
        let condition = match lima.status(&node.name).await {
            Err(error) => NodeCondition::Unknown {
                reason: error.to_string(),
            },
            Ok(None) => NodeCondition::Missing,
            Ok(Some(state)) if state != "Running" => NodeCondition::NotRunning { vm_state: state },
            Ok(Some(_)) => {
                let probe = async {
                    let client = bootstrap.client(&format!(
                        "https://127.0.0.1:{}",
                        operation.state.spec.api_port + index as u16
                    ))?;
                    crate::relish::readiness::wait_for_node(&client, Duration::from_secs(3)).await
                }
                .await;
                match probe {
                    Ok(()) => NodeCondition::Ready,
                    Err(error) => NodeCondition::ApiNotReady {
                        reason: error.to_string(),
                    },
                }
            }
        };
        nodes.push(NodeStatus {
            name: node.name.clone(),
            condition,
        });
    }
    nodes
}

/// Operate only on saved resource names, holding the same lock as setup.
pub async fn run(action: Action, name: &str, confirmed: bool) -> Result<()> {
    if action == Action::Destroy && !confirmed {
        bail!("destroy removes this cluster's VMs and data permanently; pass --yes to confirm");
    }
    let root = local_context::root_directory()?;
    let operation_root = root.clone();
    let name = name.to_owned();
    let (operation, bootstrap) = tokio::task::spawn_blocking(move || -> Result<_> {
        let operation = Operation::load(&operation_root, &name)?;
        let bootstrap = if matches!(action, Action::Status | Action::Start) {
            Some(security::existing(&operation)?)
        } else {
            None
        };
        Ok((operation, bootstrap))
    })
    .await??;
    let executable = root.join("tools/lima-2.1.0/bin/limactl");
    if !executable.exists() {
        bail!("managed Lima is missing; resume setup to reinstall it");
    }
    let lima = Lima::new(executable, Duration::from_secs(120)).with_home(root.join("lima"));
    if action == Action::Status {
        let bootstrap = bootstrap.as_ref().context("cluster bootstrap is missing")?;
        let nodes = collect_status(&lima, &operation, bootstrap).await;
        let mut unhealthy = 0;
        for node in &nodes {
            let detail = match &node.condition {
                NodeCondition::Ready => "Running; API ready".to_owned(),
                NodeCondition::Missing => "Missing".to_owned(),
                NodeCondition::NotRunning { vm_state } => vm_state.clone(),
                NodeCondition::ApiNotReady { reason } => {
                    format!("Running; API not ready: {reason}")
                }
                NodeCondition::Unknown { reason } => format!("Unknown: {reason}"),
            };
            println!("{}: {detail}", node.name);
            if node.condition != NodeCondition::Ready {
                unhealthy += 1;
            }
        }
        if nodes.is_empty() || unhealthy > 0 {
            bail!(
                "managed cluster is not ready: {unhealthy} of {} nodes unhealthy or unknown",
                nodes.len()
            );
        }
        return Ok(());
    }
    for node in &operation.state.nodes {
        let status = lima.status(&node.name).await?;
        match action {
            Action::Status => {} // Handled by the structured observation path above.
            Action::Start => {
                status.context(
                    "owned VM is missing; setup will not implicitly replace a lost cluster",
                )?;
                lima.command(&["start", "--tty=false", &node.name]).await?;
                lima.command(&[
                    "shell",
                    &node.name,
                    "sudo",
                    "systemctl",
                    "start",
                    "reliaburger.service",
                ])
                .await?;
            }
            Action::Stop => {
                if status.as_deref() == Some("Running") {
                    lima.command(&["stop", &node.name]).await?;
                    println!("stopped {}", node.name);
                }
            }
            Action::Destroy => {
                if status.is_some() {
                    lima.command(&["delete", "--force", &node.name]).await?;
                    println!("removed {}", node.name);
                }
            }
        }
    }
    if action == Action::Start {
        let bootstrap = bootstrap.as_ref().context("cluster bootstrap is missing")?;
        for index in 0..operation.state.nodes.len() {
            let client = bootstrap.client(&format!(
                "https://127.0.0.1:{}",
                operation.state.spec.api_port + index as u16
            ))?;
            crate::relish::readiness::wait_for_node(&client, Duration::from_secs(45)).await?;
        }
    }
    if action == Action::Start {
        let bootstrap = bootstrap.as_ref().context("cluster bootstrap is missing")?;
        let client = bootstrap.client(&format!(
            "https://127.0.0.1:{}",
            operation.state.spec.api_port
        ))?;
        let names = operation
            .state
            .nodes
            .iter()
            .map(|node| node.name.clone())
            .collect::<Vec<_>>();
        super::runner::wait_for_quorum(&client, &names).await?;
        println!("cluster quorum ready");
    }
    if action == Action::Destroy {
        tokio::task::spawn_blocking(move || -> Result<()> {
            LocalContext::remove_owned(&root.join("context.json"), &operation.state.id)?;
            // Preserve the lock inode so a concurrent setup can't lock a different file.
            for entry in std::fs::read_dir(&operation.directory)? {
                let entry = entry?;
                if entry.file_name() == "operation.lock" {
                    continue;
                }
                if entry.file_type()?.is_dir() {
                    std::fs::remove_dir_all(entry.path())?;
                } else {
                    std::fs::remove_file(entry.path())?;
                }
            }
            Ok(())
        })
        .await??;
    }
    Ok(())
}
