//! Lifecycle commands restricted to the resources in a locked operation record.

use super::{
    lima::Lima,
    security,
    state::{ClusterState, Operation},
};
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

/// Which of a cluster's VMs a lifecycle command acts on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeSelection {
    /// Every owned VM.
    All,
    /// One VM, by its position (0-based) in the saved state.
    One(usize),
}

impl NodeSelection {
    fn indexes(self, count: usize) -> Vec<usize> {
        match self {
            Self::All => (0..count).collect(),
            Self::One(index) => vec![index],
        }
    }
}

/// Resolve a node selector to a position in the saved state.
///
/// Accepts the node's name as `relish nodes` shows it (which is its VM name),
/// its number from 1, or `node-N`, the form the tutorial uses.
pub fn select_node(state: &ClusterState, selector: &str) -> Result<usize> {
    let number = selector
        .strip_prefix("node-")
        .unwrap_or(selector)
        .parse::<usize>()
        .ok();
    let found = match number {
        Some(number) => (1..=state.nodes.len())
            .contains(&number)
            .then(|| number - 1),
        None => state.nodes.iter().position(|node| node.name == selector),
    };
    found.with_context(|| {
        let names: Vec<String> = state
            .nodes
            .iter()
            .enumerate()
            .map(|(index, node)| format!("{} ({})", index + 1, node.name))
            .collect();
        format!(
            "no node {selector:?} in this cluster; pick one of {}",
            names.join(", ")
        )
    })
}

/// Why stopping one node needs `--yes`, one sentence per reason.
///
/// Node 1 carries every host forward: the CLI endpoint, the ingress and the
/// registry. Stopping it is a fair experiment (the others keep running and
/// elect a new leader), but afterwards every `relish` command on the host
/// fails, which reads like a broken cluster unless you asked for it. Stopping
/// a node that would leave fewer than a majority running costs the council
/// its quorum. Both are allowed; neither should happen by accident.
pub fn stop_consequences(state: &ClusterState, index: usize, running: &[bool]) -> Vec<String> {
    let count = state.nodes.len();
    if count < 2 || !running.get(index).copied().unwrap_or(false) {
        return Vec::new();
    }
    let name = &state.nodes[index].name;
    let mut reasons = Vec::new();
    if index == 0 {
        reasons.push(format!(
            "node 1 ({name}) carries the CLI endpoint (127.0.0.1:{}), the ingress \
             (127.0.0.1:{}) and the registry forward. The other nodes keep running, \
             but relish and the ingress can't reach them until you run \
             `relish local start 1`",
            state.spec.api_port, state.spec.ingress_port
        ));
    }
    let still_running = running
        .iter()
        .enumerate()
        .filter(|(other, running)| *other != index && **running)
        .count();
    let majority = count / 2 + 1;
    if still_running < majority {
        reasons.push(format!(
            "stopping {name} leaves {still_running} of {count} nodes running; the council \
             needs {majority} for a quorum, so the cluster stops accepting changes until \
             a node starts again"
        ));
    }
    reasons
}

/// Stop the selected VMs. Stopping one node asks for `confirmed` when
/// [`stop_consequences`] finds a reason to.
pub async fn stop(
    lima: &Lima,
    operation: &Operation,
    selection: NodeSelection,
    confirmed: bool,
) -> Result<()> {
    let nodes = &operation.state.nodes;
    let mut running = Vec::with_capacity(nodes.len());
    for node in nodes {
        running.push(lima.status(&node.name).await?.as_deref() == Some("Running"));
    }
    if let NodeSelection::One(index) = selection {
        let reasons = stop_consequences(&operation.state, index, &running);
        if !reasons.is_empty() && !confirmed {
            bail!("{}\npass --yes to stop it anyway", reasons.join("\n"));
        }
        if !running[index] {
            println!("{} is not running", nodes[index].name);
            return Ok(());
        }
    }
    for index in selection.indexes(nodes.len()) {
        if running[index] {
            lima.command(&["stop", &nodes[index].name]).await?;
            println!("stopped {}", nodes[index].name);
        }
    }
    Ok(())
}

/// Boot the selected VMs and start their Reliaburger service. Readiness is
/// the caller's to check.
pub async fn boot(lima: &Lima, operation: &Operation, selection: NodeSelection) -> Result<()> {
    let nodes = &operation.state.nodes;
    for index in selection.indexes(nodes.len()) {
        let name = &nodes[index].name;
        let status = lima.status(name).await?.with_context(|| {
            format!("owned VM {name} is missing; setup will not implicitly replace a lost cluster")
        })?;
        if status != "Running" {
            lima.command(&["start", "--tty=false", name]).await?;
        }
        lima.command(&[
            "shell",
            name,
            "sudo",
            "systemctl",
            "start",
            "reliaburger.service",
        ])
        .await?;
    }
    Ok(())
}

/// Operate only on saved resource names, holding the same lock as setup.
///
/// `node` narrows `start` and `stop` to one VM; see [`select_node`].
pub async fn run(action: Action, name: &str, node: Option<&str>, confirmed: bool) -> Result<()> {
    if action == Action::Destroy && !confirmed {
        bail!("destroy removes this cluster's VMs and data permanently; pass --yes to confirm");
    }
    if node.is_some() && !matches!(action, Action::Start | Action::Stop) {
        bail!("only `relish local start` and `relish local stop` take a node");
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
    let selection = match node {
        Some(selector) => NodeSelection::One(select_node(&operation.state, selector)?),
        None => NodeSelection::All,
    };
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
        // Report the policy the first node actually serves, not the one setup
        // meant to write, so an edited node.toml shows up here.
        if nodes
            .first()
            .is_some_and(|node| node.condition == NodeCondition::Ready)
        {
            let client = bootstrap.client(&format!(
                "https://127.0.0.1:{}",
                operation.state.spec.api_port
            ))?;
            match tokio::time::timeout(Duration::from_secs(3), client.capabilities()).await {
                Ok(Ok(report)) => println!(
                    "{}",
                    super::provision::describe_test_policy(&report.test_policy)
                ),
                Ok(Err(error)) => println!("fault policy: unknown ({error})"),
                Err(_) => println!("fault policy: unknown (capabilities timed out)"),
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
    match action {
        Action::Status => {} // Handled by the structured observation path above.
        Action::Stop => stop(&lima, &operation, selection, confirmed).await?,
        Action::Destroy => {
            for node in &operation.state.nodes {
                if lima.status(&node.name).await?.is_some() {
                    lima.command(&["delete", "--force", &node.name]).await?;
                    println!("removed {}", node.name);
                }
            }
        }
        Action::Start => {
            boot(&lima, &operation, selection).await?;
            let bootstrap = bootstrap.as_ref().context("cluster bootstrap is missing")?;
            for index in selection.indexes(operation.state.nodes.len()) {
                let client = bootstrap.client(&format!(
                    "https://127.0.0.1:{}",
                    operation.state.spec.api_port + index as u16
                ))?;
                crate::relish::readiness::wait_for_node(&client, Duration::from_secs(45)).await?;
                if selection != NodeSelection::All {
                    println!("started {}", operation.state.nodes[index].name);
                }
            }
            // One node rejoins a running council by itself; a whole-cluster
            // start has to prove the council formed again.
            if selection == NodeSelection::All {
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

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::relish::quickstart::state::ClusterSpec;
    use std::os::unix::fs::PermissionsExt;

    /// A stand-in `limactl`: it appends each argument list to `calls.log`
    /// and answers `list --json` from `list.json`.
    struct FakeLima {
        directory: tempfile::TempDir,
        lima: Lima,
    }

    impl FakeLima {
        fn new() -> Self {
            let directory = tempfile::tempdir().unwrap();
            let script = directory.path().join("limactl");
            let log = directory.path().join("calls.log");
            let list = directory.path().join("list.json");
            std::fs::write(
                &script,
                format!(
                    "#!/bin/sh\nprintf '%s\\n' \"$*\" >> '{}'\nif [ \"$1\" = list ]; then cat '{}'; fi\n",
                    log.display(),
                    list.display()
                ),
            )
            .unwrap();
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
            let lima = Lima::new(script, Duration::from_secs(5));
            Self { directory, lima }
        }

        fn set_states(&self, operation: &Operation, states: &[&str]) {
            let lines: Vec<String> = operation
                .state
                .nodes
                .iter()
                .zip(states)
                .map(|(node, state)| format!(r#"{{"name":"{}","status":"{state}"}}"#, node.name))
                .collect();
            std::fs::write(self.directory.path().join("list.json"), lines.join("\n")).unwrap();
        }

        /// Every call except the `list` reads.
        fn actions(&self) -> Vec<String> {
            std::fs::read_to_string(self.directory.path().join("calls.log"))
                .unwrap_or_default()
                .lines()
                .filter(|line| !line.starts_with("list"))
                .map(str::to_string)
                .collect()
        }
    }

    fn operation(root: &std::path::Path) -> Operation {
        Operation::open(
            root,
            &ClusterSpec {
                name: "laptop".to_string(),
                nodes: 3,
                version: "v0.1.0".parse().unwrap(),
                api_port: 19117,
                ingress_port: 18080,
                registry_port: 15050,
            },
        )
        .unwrap()
    }

    #[test]
    fn a_node_is_named_by_number_by_node_n_or_by_its_name() {
        let root = tempfile::tempdir().unwrap();
        let operation = operation(root.path());
        let third = operation.state.nodes[2].name.clone();
        for selector in ["3", "node-3", third.as_str()] {
            assert_eq!(select_node(&operation.state, selector).unwrap(), 2);
        }
        for selector in ["0", "4", "node-9", "rb-somebody-else-1", ""] {
            let error = select_node(&operation.state, selector)
                .unwrap_err()
                .to_string();
            assert!(error.contains(&third), "{error}");
        }
    }

    #[tokio::test]
    async fn stopping_one_node_stops_only_that_vm() {
        let root = tempfile::tempdir().unwrap();
        let operation = operation(root.path());
        let fake = FakeLima::new();
        fake.set_states(&operation, &["Running", "Running", "Running"]);

        stop(&fake.lima, &operation, NodeSelection::One(2), false)
            .await
            .unwrap();
        assert_eq!(
            fake.actions(),
            vec![format!("stop {}", operation.state.nodes[2].name)]
        );
    }

    #[tokio::test]
    async fn stopping_node_one_needs_yes_because_it_carries_the_host_forwards() {
        let root = tempfile::tempdir().unwrap();
        let operation = operation(root.path());
        let fake = FakeLima::new();
        fake.set_states(&operation, &["Running", "Running", "Running"]);

        let error = stop(&fake.lima, &operation, NodeSelection::One(0), false)
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("CLI endpoint (127.0.0.1:19117)"), "{error}");
        assert!(error.contains("--yes"), "{error}");
        assert!(fake.actions().is_empty(), "{:?}", fake.actions());

        stop(&fake.lima, &operation, NodeSelection::One(0), true)
            .await
            .unwrap();
        assert_eq!(
            fake.actions(),
            vec![format!("stop {}", operation.state.nodes[0].name)]
        );
    }

    #[tokio::test]
    async fn stopping_a_second_node_needs_yes_because_quorum_goes_with_it() {
        let root = tempfile::tempdir().unwrap();
        let operation = operation(root.path());
        let fake = FakeLima::new();
        fake.set_states(&operation, &["Running", "Running", "Stopped"]);

        let error = stop(&fake.lima, &operation, NodeSelection::One(1), false)
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("leaves 1 of 3 nodes running"), "{error}");
        assert!(fake.actions().is_empty());
    }

    #[tokio::test]
    async fn a_stopped_node_is_reported_rather_than_stopped_again() {
        let root = tempfile::tempdir().unwrap();
        let operation = operation(root.path());
        let fake = FakeLima::new();
        fake.set_states(&operation, &["Running", "Running", "Stopped"]);

        stop(&fake.lima, &operation, NodeSelection::One(2), false)
            .await
            .unwrap();
        assert!(fake.actions().is_empty(), "{:?}", fake.actions());
    }

    #[tokio::test]
    async fn stopping_the_whole_cluster_needs_no_confirmation() {
        let root = tempfile::tempdir().unwrap();
        let operation = operation(root.path());
        let fake = FakeLima::new();
        fake.set_states(&operation, &["Running", "Stopped", "Running"]);

        stop(&fake.lima, &operation, NodeSelection::All, false)
            .await
            .unwrap();
        let names = &operation.state.nodes;
        assert_eq!(
            fake.actions(),
            vec![
                format!("stop {}", names[0].name),
                format!("stop {}", names[2].name),
            ]
        );
    }

    #[tokio::test]
    async fn starting_one_node_boots_only_that_vm_and_its_service() {
        let root = tempfile::tempdir().unwrap();
        let operation = operation(root.path());
        let fake = FakeLima::new();
        fake.set_states(&operation, &["Running", "Stopped", "Running"]);

        boot(&fake.lima, &operation, NodeSelection::One(1))
            .await
            .unwrap();
        let name = &operation.state.nodes[1].name;
        assert_eq!(
            fake.actions(),
            vec![
                format!("start --tty=false {name}"),
                format!("shell {name} sudo systemctl start reliaburger.service"),
            ]
        );
    }

    #[tokio::test]
    async fn only_start_and_stop_take_a_node() {
        for action in [Action::Status, Action::Destroy] {
            let error = run(action, "laptop", Some("3"), true)
                .await
                .unwrap_err()
                .to_string();
            assert!(error.contains("take a node"), "{error}");
        }
    }
}
