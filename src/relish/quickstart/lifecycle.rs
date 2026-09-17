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
    let lima = Lima::new(executable, Duration::from_secs(120));
    for (index, node) in operation.state.nodes.iter().enumerate() {
        let status = lima.status(&node.name).await?;
        match action {
            Action::Status => {
                println!("{}: {}", node.name, status.as_deref().unwrap_or("Missing"));
                if status.as_deref() == Some("Running") {
                    let bootstrap = bootstrap.as_ref().context("cluster bootstrap is missing")?;
                    let client = bootstrap.client(&format!(
                        "https://127.0.0.1:{}",
                        operation.state.spec.api_port + index as u16
                    ))?;
                    match crate::relish::readiness::wait_for_node(&client, Duration::from_secs(3))
                        .await
                    {
                        Ok(()) => println!("  API ready"),
                        Err(error) => println!("  API not ready: {error}"),
                    }
                }
            }
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
                }
            }
            Action::Destroy => {
                if status.is_some() {
                    lima.command(&["delete", "--force", &node.name]).await?;
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
