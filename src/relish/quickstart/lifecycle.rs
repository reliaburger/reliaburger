//! Lifecycle commands restricted to the resources in a locked operation record.

use super::{
    lima::{Lima, POWER_OFF_DEADLINE},
    security,
    state::{ClusterState, Operation},
};
use crate::relish::local_context::{self, LocalContext};
use crate::relish::output::{OutputFormat, print_output};
use anyhow::{Context, Result, bail};
use std::time::Duration;

/// Supported actions for an existing managed cluster.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
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

/// Stop the selected VMs, recording each in `changes`. Stopping one node
/// asks for `confirmed` when [`stop_consequences`] finds a reason to.
pub async fn stop(
    lima: &Lima,
    operation: &Operation,
    selection: NodeSelection,
    confirmed: bool,
    changes: &mut LocalChanges,
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
            changes.record(&nodes[index].name, NodeResult::NotRunning);
            return Ok(());
        }
    }
    for index in selection.indexes(nodes.len()) {
        if running[index] {
            lima.power_off(&nodes[index].name, POWER_OFF_DEADLINE)
                .await?;
            changes.record(&nodes[index].name, NodeResult::Stopped);
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
/// `relish local` drives VMs. Applying it to someone's real machines, least of
/// all `destroy`, would be a much bigger thing than it says.
fn refuse_ssh_cluster(spec: &super::state::ClusterSpec) -> Result<()> {
    if spec.ssh.is_empty() {
        return Ok(());
    }
    bail!(
        "cluster {} runs on machines reached over SSH, and `relish local` only manages VMs; \
         use `relish status`, and systemctl on the machines themselves",
        spec.name
    )
}

/// What `relish local status` saw, as `-o json` prints it.
#[derive(Debug, Clone, serde::Serialize)]
pub struct LocalStatus {
    /// The managed cluster's name.
    pub cluster: String,
    /// Every owned node, in the saved order.
    pub nodes: Vec<NodeStatus>,
    /// The fault policy the first node serves, when it could be read.
    pub fault_policy: Option<String>,
    /// Every node is ready.
    pub ready: bool,
}

impl std::fmt::Display for LocalStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for node in &self.nodes {
            let detail = match &node.condition {
                NodeCondition::Ready => "Running; API ready".to_owned(),
                NodeCondition::Missing => "Missing".to_owned(),
                NodeCondition::NotRunning { vm_state } => vm_state.clone(),
                NodeCondition::ApiNotReady { reason } => {
                    format!("Running; API not ready: {reason}")
                }
                NodeCondition::Unknown { reason } => format!("Unknown: {reason}"),
            };
            writeln!(f, "{}: {detail}", node.name)?;
        }
        if let Some(policy) = &self.fault_policy {
            writeln!(f, "{policy}")?;
        }
        Ok(())
    }
}

/// What a lifecycle command did to one node.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeResult {
    /// `stop` shut it down.
    Stopped,
    /// `stop` found it already down and left it.
    NotRunning,
    /// `start` booted it and its API answered.
    Started,
    /// `destroy` deleted its VM.
    Removed,
}

/// One node in a [`LocalChanges`] report.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct NodeChange {
    pub name: String,
    pub result: NodeResult,
}

impl std::fmt::Display for NodeChange {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let name = &self.name;
        match self.result {
            NodeResult::Stopped => write!(f, "stopped {name}"),
            NodeResult::NotRunning => write!(f, "{name} is not running"),
            NodeResult::Started => write!(f, "started {name}"),
            NodeResult::Removed => write!(f, "removed {name}"),
        }
    }
}

/// What `relish local start`, `stop` or `destroy` did. People see each
/// change as it happens; `-o json` gets the whole report at the end.
#[derive(Debug, Clone, serde::Serialize)]
pub struct LocalChanges {
    /// The managed cluster's name.
    pub cluster: String,
    pub action: Action,
    /// The nodes the command changed, or found already as asked.
    pub nodes: Vec<NodeChange>,
    /// After starting the whole cluster: the council formed again.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub quorum_ready: Option<bool>,
    #[serde(skip)]
    narrate: bool,
}

impl LocalChanges {
    /// An empty report; with `narrate`, each change prints as it's recorded.
    pub fn new(cluster: &str, action: Action, narrate: bool) -> Self {
        Self {
            cluster: cluster.to_string(),
            action,
            nodes: Vec::new(),
            quorum_ready: None,
            narrate,
        }
    }

    fn record(&mut self, name: &str, result: NodeResult) {
        let change = NodeChange {
            name: name.to_string(),
            result,
        };
        if self.narrate {
            println!("{change}");
        }
        self.nodes.push(change);
    }

    fn quorum_ready(&mut self) {
        if self.narrate {
            println!("cluster quorum ready");
        }
        self.quorum_ready = Some(true);
    }
}

impl std::fmt::Display for LocalChanges {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for change in &self.nodes {
            writeln!(f, "{change}")?;
        }
        if self.quorum_ready == Some(true) {
            writeln!(f, "cluster quorum ready")?;
        }
        Ok(())
    }
}

/// Remove a destroyed cluster's files: everything in its directory but the
/// operation lock, and the identity that opened its root backup, which
/// lives in another directory (#709). The cluster and its root go together.
fn forget_cluster_files(
    directory: &std::path::Path,
    identity: Option<&std::path::Path>,
) -> Result<()> {
    if let Some(identity) = identity
        && let Err(error) = std::fs::remove_file(identity)
        && error.kind() != std::io::ErrorKind::NotFound
    {
        return Err(error.into());
    }
    // Preserve the lock inode so a concurrent setup can't lock a different file.
    for entry in std::fs::read_dir(directory)? {
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
}

/// Run a lifecycle action on the managed cluster `name`, printing in
/// `output`.
pub async fn run(
    action: Action,
    name: &str,
    node: Option<&str>,
    confirmed: bool,
    output: OutputFormat,
) -> Result<()> {
    if action == Action::Destroy && !confirmed {
        bail!("destroy removes this cluster's VMs and data permanently; pass --yes to confirm");
    }
    if node.is_some() && !matches!(action, Action::Start | Action::Stop) {
        bail!("only `relish local start` and `relish local stop` take a node");
    }
    let root = local_context::root_directory()?;
    let operation_root = root.clone();
    let cluster = name.to_owned();
    let name = name.to_owned();
    let (operation, bootstrap) = tokio::task::spawn_blocking(move || -> Result<_> {
        let operation = Operation::load(&operation_root, &name)?;
        refuse_ssh_cluster(&operation.state.spec)?;
        let bootstrap = match action {
            Action::Status | Action::Start => Some(security::existing(&operation)?),
            // The identity that opens the root backup lives outside the
            // cluster directory; find it before the directory goes (#709).
            Action::Destroy => security::existing(&operation).ok(),
            _ => None,
        };
        Ok((operation, bootstrap))
    })
    .await??;
    let executable = super::artifacts::managed_limactl(&root);
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
        let unhealthy = nodes
            .iter()
            .filter(|node| node.condition != NodeCondition::Ready)
            .count();
        // Report the policy the first node actually serves, not the one setup
        // meant to write, so an edited node.toml shows up here.
        let mut fault_policy = None;
        if nodes
            .first()
            .is_some_and(|node| node.condition == NodeCondition::Ready)
        {
            let client = bootstrap.client(&format!(
                "https://127.0.0.1:{}",
                operation.state.spec.api_port
            ))?;
            fault_policy = Some(
                match tokio::time::timeout(Duration::from_secs(3), client.capabilities()).await {
                    Ok(Ok(report)) => super::provision::describe_test_policy(&report.test_policy),
                    Ok(Err(error)) => format!("fault policy: unknown ({error})"),
                    Err(_) => "fault policy: unknown (capabilities timed out)".to_string(),
                },
            );
        }
        let total = nodes.len();
        let status = LocalStatus {
            cluster,
            ready: total > 0 && unhealthy == 0,
            nodes,
            fault_policy,
        };
        print_output(&status, output)?;
        if !status.ready {
            bail!(
                "managed cluster is not ready: {unhealthy} of {total} nodes unhealthy or unknown"
            );
        }
        return Ok(());
    }
    let mut changes = LocalChanges::new(&cluster, action, output == OutputFormat::Human);
    match action {
        Action::Status => {} // Handled by the structured observation path above.
        Action::Stop => stop(&lima, &operation, selection, confirmed, &mut changes).await?,
        Action::Destroy => {
            for node in &operation.state.nodes {
                if lima.status(&node.name).await?.is_some() {
                    lima.command(&["delete", "--force", &node.name]).await?;
                    changes.record(&node.name, NodeResult::Removed);
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
                changes.record(&operation.state.nodes[index].name, NodeResult::Started);
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
                changes.quorum_ready();
            }
        }
    }

    if action == Action::Destroy {
        let identity = bootstrap
            .as_ref()
            .and_then(|bootstrap| bootstrap.root_backup.as_ref())
            .map(|files| files.identity.clone());
        tokio::task::spawn_blocking(move || -> Result<()> {
            LocalContext::remove_owned(&root.join("context.json"), &operation.state.id)?;
            forget_cluster_files(&operation.directory, identity.as_deref())
        })
        .await??;
    }
    if !changes.narrate {
        print_output(&changes, output)?;
    }
    Ok(())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn local_commands_leave_machines_reached_over_ssh_alone() {
        let spec = super::super::state::ClusterSpec {
            name: "lab".to_string(),
            nodes: 1,
            version: "v0.1.0".parse().unwrap(),
            api_port: 19117,
            ingress_port: 18080,
            registry_port: 15050,
            ssh: vec!["u@a".parse().unwrap()],
        };
        let error = refuse_ssh_cluster(&spec).unwrap_err().to_string();
        assert!(error.contains("only manages VMs"), "{error}");
        let vms = super::super::state::ClusterSpec {
            ssh: Vec::new(),
            ..spec
        };
        assert!(refuse_ssh_cluster(&vms).is_ok());
    }

    use crate::relish::quickstart::state::ClusterSpec;

    /// A stand-in `limactl`: it appends each argument list to `calls.log`
    /// and answers `list --json` from `list.json`. A guest `systemctl
    /// poweroff` marks that VM `Stopped`, as a real guest would.
    struct FakeLima {
        directory: tempfile::TempDir,
        lima: Lima,
    }

    impl FakeLima {
        fn new() -> Self {
            let directory = tempfile::tempdir().unwrap();
            let script = directory.path().join("limactl.sh");
            // Run through `/bin/sh`, so no new executable is launched and
            // macOS has nothing to assess inside the deadline (#762). The
            // script finds this test's files beside itself.
            std::fs::write(
                &script,
                r#"dir=${0%/*}
printf '%s\n' "$*" >> "$dir/calls.log"
if [ "$1" = list ]; then cat "$dir/list.json"; fi
if [ "$5" = poweroff ]; then
  grep -v "\"$2\"" "$dir/list.json" > "$dir/list.next"
  printf '{"name":"%s","status":"Stopped"}\n' "$2" >> "$dir/list.next"
  mv "$dir/list.next" "$dir/list.json"
fi
"#,
            )
            .unwrap();
            let lima = Lima::shell_script(script, Duration::from_secs(5));
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

    /// The guest-side shutdown `stop` asks each VM for.
    fn power_off(name: &str) -> String {
        format!("shell {name} sudo systemctl poweroff --no-block")
    }

    /// A report that records without printing, as `-o json` uses.
    fn quiet() -> LocalChanges {
        LocalChanges::new("laptop", Action::Stop, false)
    }

    #[tokio::test]
    async fn a_stop_records_which_nodes_it_stopped_and_which_were_down() {
        let root = tempfile::tempdir().unwrap();
        let operation = operation(root.path());
        let fake = FakeLima::new();
        fake.set_states(&operation, &["Running", "Stopped", "Running"]);

        let mut changes = quiet();
        stop(
            &fake.lima,
            &operation,
            NodeSelection::All,
            false,
            &mut changes,
        )
        .await
        .unwrap();
        let names: Vec<_> = changes
            .nodes
            .iter()
            .map(|change| (change.name.as_str(), change.result))
            .collect();
        let nodes = &operation.state.nodes;
        assert_eq!(
            names,
            [
                (nodes[0].name.as_str(), NodeResult::Stopped),
                (nodes[2].name.as_str(), NodeResult::Stopped),
            ]
        );

        let mut changes = quiet();
        stop(
            &fake.lima,
            &operation,
            NodeSelection::One(1),
            false,
            &mut changes,
        )
        .await
        .unwrap();
        assert_eq!(changes.nodes[0].result, NodeResult::NotRunning);
    }

    #[test]
    fn local_changes_as_json_name_the_action_and_each_node() {
        let mut changes = LocalChanges::new("laptop", Action::Start, false);
        changes.record("rb-laptop-1", NodeResult::Started);
        changes.record("rb-laptop-2", NodeResult::Started);
        changes.quorum_ready();
        insta::assert_snapshot!(
            crate::relish::output::format_output(&changes, OutputFormat::Json).unwrap()
        );
    }

    #[test]
    fn local_status_as_json_gives_each_nodes_condition() {
        let status = LocalStatus {
            cluster: "laptop".to_string(),
            nodes: vec![
                NodeStatus {
                    name: "rb-laptop-1".to_string(),
                    condition: NodeCondition::Ready,
                },
                NodeStatus {
                    name: "rb-laptop-2".to_string(),
                    condition: NodeCondition::ApiNotReady {
                        reason: "council not formed".to_string(),
                    },
                },
                NodeStatus {
                    name: "rb-laptop-3".to_string(),
                    condition: NodeCondition::NotRunning {
                        vm_state: "Stopped".to_string(),
                    },
                },
            ],
            fault_policy: Some("fault policy: workload faults allowed".to_string()),
            ready: false,
        };
        insta::assert_snapshot!(
            crate::relish::output::format_output(&status, OutputFormat::Json).unwrap()
        );
        assert_eq!(
            status.to_string(),
            "rb-laptop-1: Running; API ready\n\
             rb-laptop-2: Running; API not ready: council not formed\n\
             rb-laptop-3: Stopped\n\
             fault policy: workload faults allowed\n"
        );
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
                ssh: Vec::new(),
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

        stop(
            &fake.lima,
            &operation,
            NodeSelection::One(2),
            false,
            &mut quiet(),
        )
        .await
        .unwrap();
        assert_eq!(
            fake.actions(),
            vec![power_off(&operation.state.nodes[2].name)]
        );
    }

    #[tokio::test]
    async fn stopping_node_one_needs_yes_because_it_carries_the_host_forwards() {
        let root = tempfile::tempdir().unwrap();
        let operation = operation(root.path());
        let fake = FakeLima::new();
        fake.set_states(&operation, &["Running", "Running", "Running"]);

        let error = stop(
            &fake.lima,
            &operation,
            NodeSelection::One(0),
            false,
            &mut quiet(),
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(error.contains("CLI endpoint (127.0.0.1:19117)"), "{error}");
        assert!(error.contains("--yes"), "{error}");
        assert!(fake.actions().is_empty(), "{:?}", fake.actions());

        stop(
            &fake.lima,
            &operation,
            NodeSelection::One(0),
            true,
            &mut quiet(),
        )
        .await
        .unwrap();
        assert_eq!(
            fake.actions(),
            vec![power_off(&operation.state.nodes[0].name)]
        );
    }

    #[tokio::test]
    async fn stopping_a_second_node_needs_yes_because_quorum_goes_with_it() {
        let root = tempfile::tempdir().unwrap();
        let operation = operation(root.path());
        let fake = FakeLima::new();
        fake.set_states(&operation, &["Running", "Running", "Stopped"]);

        let error = stop(
            &fake.lima,
            &operation,
            NodeSelection::One(1),
            false,
            &mut quiet(),
        )
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

        stop(
            &fake.lima,
            &operation,
            NodeSelection::One(2),
            false,
            &mut quiet(),
        )
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

        stop(
            &fake.lima,
            &operation,
            NodeSelection::All,
            false,
            &mut quiet(),
        )
        .await
        .unwrap();
        let names = &operation.state.nodes;
        assert_eq!(
            fake.actions(),
            vec![power_off(&names[0].name), power_off(&names[2].name),]
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
            let error = run(action, "laptop", Some("3"), true, OutputFormat::Human)
                .await
                .unwrap_err()
                .to_string();
            assert!(error.contains("take a node"), "{error}");
        }
    }

    #[test]
    fn destroy_forgets_the_cluster_and_the_identity_that_opens_its_root() {
        let root = tempfile::tempdir().unwrap();
        let cluster = root.path().join("clusters").join("laptop");
        std::fs::create_dir_all(cluster.join("security")).unwrap();
        std::fs::write(cluster.join("operation.lock"), "").unwrap();
        std::fs::write(
            cluster.join("security").join("root-ca-backup.age"),
            "sealed",
        )
        .unwrap();
        let keys = root.path().join("root-backup-keys");
        std::fs::create_dir(&keys).unwrap();
        let identity = keys.join("root-0123456789abcdef.age-identity");
        let another = keys.join("root-fedcba9876543210.age-identity");
        std::fs::write(&identity, "AGE-SECRET-KEY-1").unwrap();
        std::fs::write(&another, "AGE-SECRET-KEY-1").unwrap();

        forget_cluster_files(&cluster, Some(&identity)).unwrap();
        assert!(!identity.exists());
        assert!(another.exists(), "another cluster's identity was removed");
        let left: Vec<_> = std::fs::read_dir(&cluster)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(left, vec![std::ffi::OsString::from("operation.lock")]);
        // Twice is fine: the identity is already gone.
        forget_cluster_files(&cluster, Some(&identity)).unwrap();
    }
}
