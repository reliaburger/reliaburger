//! What `relish setup --quickstart` and `relish setup --ssh` built, as they
//! print it when they finish.
//!
//! People get a few lines: how long it took, where the app answers, what to
//! run next. `-o json` prints the same facts as one document, so an agent
//! that ran setup can quote what it built (the nodes, the API, the app, how
//! to take it down) instead of scraping the progress lines, which go to
//! standard error then.

use std::{fmt, net::Ipv4Addr, path::PathBuf};

use super::{
    progress::{StageTiming, Timings},
    security::RootBackupFiles,
    state::Operation,
};

/// Which kind of cluster setup built.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ClusterKind {
    /// Lima VMs on this machine (`--quickstart`).
    Laptop,
    /// Linux machines reached over SSH (`--ssh`).
    Servers,
}

/// One node setup built.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct BuiltNode {
    /// The node's name in the cluster.
    pub name: String,
    /// The address peers reach it on, when setup learned one.
    pub address: Option<Ipv4Addr>,
    /// The SSH destination setup used, for a cluster built over SSH.
    pub ssh: Option<String>,
}

/// A command worth running next.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct NextStep {
    /// The command line.
    pub run: String,
    /// What it shows.
    pub why: String,
}

/// How to take the cluster down again.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Teardown {
    /// One command removes the cluster and its credentials.
    Command {
        /// The command line.
        run: String,
    },
    /// No command does it yet; the manual chapter has the steps.
    Manual {
        /// The `relish manual` chapter with the steps.
        chapter: String,
        /// Why there's no command.
        reason: String,
    },
}

/// Everything a finished setup built, as `-o json` prints it.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SetupReport {
    /// The cluster's name.
    pub cluster: String,
    /// Laptop VMs or machines over SSH.
    pub kind: ClusterKind,
    /// The Reliaburger version every node runs.
    pub version: String,
    /// Wall-clock seconds this run took, downloads included.
    pub seconds: f64,
    /// Where the time went, stage by stage.
    pub stages: Vec<StageTiming>,
    /// Every node, in the order setup made them.
    pub nodes: Vec<BuiltNode>,
    /// The HTTPS API `relish` talks to, from the saved context.
    pub api: Option<String>,
    /// The file holding the admin credential every `relish` on this machine
    /// uses. The credential itself is never printed.
    pub context: PathBuf,
    /// Where the demo app answers.
    pub app: String,
    /// The directory holding setup's checkpoints and the cluster's state.
    pub state_directory: PathBuf,
    /// Every step's timing, for a laptop cluster.
    pub timings_file: Option<PathBuf>,
    /// The directory to back up, with the master key, for a cluster built
    /// over SSH.
    pub back_up: Option<PathBuf>,
    /// The only copy of the root CA's key and the identity that opens it,
    /// on this machine, to move offline (#709).
    pub root_backup: Option<RootBackupFiles>,
    /// Commands worth running next. All of them only read.
    pub next: Vec<NextStep>,
    /// How to take the cluster down.
    pub teardown: Teardown,
    /// Things to deal with before the `next` commands work.
    pub warnings: Vec<String>,
}

/// What a laptop setup knows when it finishes.
pub struct LaptopFacts<'a> {
    /// The finished operation.
    pub operation: &'a Operation,
    /// The run's timings.
    pub timings: &'a Timings,
    /// The saved context's API endpoint, when it could be read.
    pub api: Option<String>,
    /// The context file.
    pub context: PathBuf,
    /// Where `timings.json` went.
    pub timings_file: PathBuf,
    /// How to invoke this `relish` (`relish`, or a path when it isn't on
    /// `PATH` yet).
    pub relish: &'a str,
    /// The root CA backup and its identity.
    pub root_backup: Option<RootBackupFiles>,
}

/// What a setup over SSH knows when it finishes.
pub struct ServerFacts<'a> {
    /// The finished operation.
    pub operation: &'a Operation,
    /// The run's timings.
    pub timings: &'a Timings,
    /// The saved context's API endpoint, when it could be read.
    pub api: Option<String>,
    /// The context file.
    pub context: PathBuf,
    /// The security directory to back up.
    pub back_up: PathBuf,
    /// Why `relish` can't reach the API yet, when it can't.
    pub unreachable: Option<String>,
    /// How to invoke this `relish`.
    pub relish: &'a str,
    /// The root CA backup and its identity.
    pub root_backup: Option<RootBackupFiles>,
}

fn nodes(operation: &Operation) -> Vec<BuiltNode> {
    let targets = &operation.state.spec.ssh;
    operation
        .state
        .nodes
        .iter()
        .enumerate()
        .map(|(index, node)| BuiltNode {
            name: node.name.clone(),
            address: node.address,
            ssh: targets.get(index).map(ToString::to_string),
        })
        .collect()
}

/// `0.2.7`, as `relish --version` shows it, rather than the tag `v0.2.7`.
fn version(operation: &Operation) -> String {
    let version = operation.state.spec.version.to_string();
    version.trim_start_matches('v').to_string()
}

fn step(run: String, why: &str) -> NextStep {
    NextStep {
        run,
        why: why.to_string(),
    }
}

impl SetupReport {
    /// The report for a laptop cluster.
    pub fn laptop(facts: LaptopFacts<'_>) -> Self {
        let spec = &facts.operation.state.spec;
        let relish = facts.relish;
        Self {
            cluster: spec.name.clone(),
            kind: ClusterKind::Laptop,
            version: version(facts.operation),
            seconds: facts.timings.total_seconds,
            stages: facts.timings.stages(),
            nodes: nodes(facts.operation),
            api: facts.api,
            context: facts.context,
            app: format!("http://localhost:{}", spec.ingress_port),
            state_directory: facts.operation.directory.clone(),
            timings_file: Some(facts.timings_file),
            back_up: None,
            root_backup: facts.root_backup,
            next: vec![
                step(
                    format!("{relish} manual tour"),
                    "the five-minute tour of this cluster",
                ),
                step(format!("{relish} status"), "every app and node"),
                step(format!("{relish} logs hello"), "the demo app's logs"),
                step(
                    format!("{relish} local status --name {}", spec.name),
                    "each VM and its API",
                ),
            ],
            teardown: Teardown::Command {
                run: format!("{relish} local destroy --name {} --yes", spec.name),
            },
            warnings: Vec::new(),
        }
    }

    /// The report for a cluster built over SSH.
    pub fn servers(facts: ServerFacts<'_>) -> Self {
        let spec = &facts.operation.state.spec;
        let relish = facts.relish;
        let app = spec
            .ssh
            .first()
            .map(|target| format!("http://{}", target.host))
            .unwrap_or_default();
        Self {
            cluster: spec.name.clone(),
            kind: ClusterKind::Servers,
            version: version(facts.operation),
            seconds: facts.timings.total_seconds,
            stages: facts.timings.stages(),
            nodes: nodes(facts.operation),
            api: facts.api,
            context: facts.context,
            app,
            state_directory: facts.operation.directory.clone(),
            timings_file: None,
            back_up: Some(facts.back_up),
            root_backup: facts.root_backup,
            next: vec![
                step(format!("{relish} status"), "every app and node"),
                step(format!("{relish} logs hello"), "the demo app's logs"),
                step(format!("{relish} wtf"), "anything that needs attention"),
            ],
            teardown: Teardown::Manual {
                chapter: "getting-started".to_string(),
                reason: "no command removes a cluster built over SSH yet".to_string(),
            },
            warnings: facts.unreachable.into_iter().collect(),
        }
    }
}

impl fmt::Display for SetupReport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "cluster {} ready in {:.1}s", self.cluster, self.seconds)?;
        writeln!(f, "  app: {}", self.app)?;
        for next in &self.next {
            writeln!(f, "  next: {:<36} ({})", next.run, next.why)?;
        }
        match &self.teardown {
            Teardown::Command { run } => writeln!(f, "  remove it: {run}")?,
            Teardown::Manual { chapter, .. } => {
                writeln!(f, "  remove it: the steps are in `relish manual {chapter}`")?
            }
        }
        if let Some(directory) = &self.back_up {
            writeln!(
                f,
                "  back up {}: it holds the cluster's master key",
                directory.display()
            )?;
        }
        if let Some(root) = &self.root_backup {
            writeln!(
                f,
                "  root CA: only in {}, which {} opens; move both offline, apart from each other",
                root.backup.display(),
                root.identity.display()
            )?;
            if self.kind == ClusterKind::Laptop {
                writeln!(
                    f,
                    "  (on a laptop cluster the root is offline only in name: this machine \
                     is both the operator and the nodes)"
                )?;
            }
        }
        // Last, so it isn't lost above the timings: until it's dealt with,
        // the `next` commands above all fail.
        for warning in &self.warnings {
            writeln!(f, "\nwarning: {warning}")?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::relish::output::{OutputFormat, format_output};
    use crate::relish::quickstart::{
        progress::{Outcome, Stage, StepTiming},
        state::{ClusterSpec, NodePhase},
    };
    use std::path::Path;

    fn timings() -> Timings {
        let timing = |label: &str, stage, start_seconds, seconds, bytes| StepTiming {
            label: label.to_string(),
            stage,
            start_seconds,
            seconds,
            outcome: Outcome::Done,
            bytes,
            note: None,
        };
        Timings {
            total_seconds: 187.25,
            steps: vec![
                timing("check host", Stage::Host, 0.0, 0.5, None),
                timing(
                    "download image",
                    Stage::Download,
                    0.5,
                    60.0,
                    Some(600 << 20),
                ),
                timing("boot VM 1", Stage::Boot, 61.0, 50.0, None),
                timing("configure node 1", Stage::Configure, 111.0, 40.0, None),
                timing("demo answers", Stage::Verify, 151.0, 36.25, None),
            ],
        }
    }

    /// An operation as setup leaves it, in a fixed directory so the
    /// snapshot doesn't depend on a temporary path.
    fn operation(root: &Path, ssh: &[&str]) -> Operation {
        let spec = ClusterSpec {
            name: if ssh.is_empty() { "laptop" } else { "servers" }.to_string(),
            nodes: 3,
            version: "0.2.7".parse().unwrap(),
            api_port: 19117,
            ingress_port: 18080,
            registry_port: 15050,
            ssh: ssh.iter().map(|target| target.parse().unwrap()).collect(),
        };
        let mut operation = Operation::open(root, &spec).unwrap();
        // Real names carry the operation's random identifier.
        for (index, node) in operation.state.nodes.iter_mut().enumerate() {
            node.name = format!("rb-{}-{}", spec.name, index + 1);
            node.address = Some(Ipv4Addr::new(10, 0, 0, index as u8 + 1));
            node.phase = NodePhase::Started;
        }
        operation.directory = PathBuf::from("/home/me/.reliaburger/clusters/laptop");
        operation
    }

    fn laptop_report() -> SetupReport {
        let root = tempfile::tempdir().unwrap();
        let operation = operation(root.path(), &[]);
        let timings = timings();
        SetupReport::laptop(LaptopFacts {
            operation: &operation,
            timings: &timings,
            api: Some("https://127.0.0.1:19117".to_string()),
            context: PathBuf::from("/home/me/.reliaburger/context.json"),
            timings_file: PathBuf::from("/home/me/.reliaburger/clusters/laptop/timings.json"),
            relish: "relish",
            root_backup: Some(RootBackupFiles {
                backup: PathBuf::from(
                    "/home/me/.reliaburger/clusters/laptop/security/root-ca-backup.age",
                ),
                identity: PathBuf::from(
                    "/home/me/.reliaburger/root-backup-keys/root-0123456789abcdef.age-identity",
                ),
            }),
        })
    }

    fn servers_report(unreachable: Option<String>) -> SetupReport {
        let root = tempfile::tempdir().unwrap();
        let mut operation = operation(
            root.path(),
            &["ubuntu@10.0.0.1", "ubuntu@10.0.0.2", "ubuntu@10.0.0.3:2222"],
        );
        operation.directory = PathBuf::from("/home/me/.reliaburger/clusters/servers");
        let timings = timings();
        SetupReport::servers(ServerFacts {
            operation: &operation,
            timings: &timings,
            api: Some("https://10.0.0.1:9117".to_string()),
            context: PathBuf::from("/home/me/.reliaburger/context.json"),
            back_up: PathBuf::from("/home/me/.reliaburger/clusters/servers/security"),
            unreachable,
            relish: "relish",
            root_backup: Some(RootBackupFiles {
                backup: PathBuf::from(
                    "/home/me/.reliaburger/clusters/servers/security/root-ca-backup.age",
                ),
                identity: PathBuf::from(
                    "/home/me/.reliaburger/root-backup-keys/root-fedcba9876543210.age-identity",
                ),
            }),
        })
    }

    #[test]
    fn snapshot_of_a_laptop_setup_as_json() {
        insta::assert_snapshot!(format_output(&laptop_report(), OutputFormat::Json).unwrap());
    }

    #[test]
    fn snapshot_of_a_setup_over_ssh_as_json() {
        insta::assert_snapshot!(format_output(&servers_report(None), OutputFormat::Json).unwrap());
    }

    #[test]
    fn the_json_never_carries_the_admin_credential() {
        let json = format_output(&laptop_report(), OutputFormat::Json).unwrap();
        assert!(!json.contains("token"), "{json}");
        assert!(!json.contains("rbrg_"), "{json}");
    }

    /// #709: both setups say where the root backup and its identity are,
    /// to move offline, and a laptop setup says that's only nominal there.
    #[test]
    fn setup_says_where_the_root_backup_is_and_to_move_it_offline() {
        let servers = servers_report(None).to_string();
        assert!(servers.contains("root-ca-backup.age"), "{servers}");
        assert!(
            servers.contains("root-fedcba9876543210.age-identity"),
            "{servers}"
        );
        assert!(servers.contains("move both offline"), "{servers}");
        assert!(!servers.contains("offline only in name"), "{servers}");
        let laptop = laptop_report().to_string();
        assert!(laptop.contains("move both offline"), "{laptop}");
        assert!(laptop.contains("offline only in name"), "{laptop}");
    }

    #[test]
    fn a_laptop_cluster_is_removed_by_one_command() {
        let report = laptop_report();
        assert_eq!(
            report.teardown,
            Teardown::Command {
                run: "relish local destroy --name laptop --yes".to_string()
            }
        );
        assert!(report.back_up.is_none());
    }

    #[test]
    fn a_cluster_over_ssh_names_its_machines_and_what_to_back_up() {
        let report = servers_report(None);
        let ssh: Vec<_> = report.nodes.iter().map(|node| node.ssh.clone()).collect();
        assert_eq!(
            ssh,
            [
                Some("ubuntu@10.0.0.1".to_string()),
                Some("ubuntu@10.0.0.2".to_string()),
                Some("ubuntu@10.0.0.3:2222".to_string()),
            ]
        );
        assert_eq!(report.app, "http://10.0.0.1");
        assert!(matches!(report.teardown, Teardown::Manual { .. }));
    }

    #[test]
    fn people_see_where_the_app_is_what_to_run_and_how_to_remove_it() {
        let text = laptop_report().to_string();
        assert!(
            text.starts_with("cluster laptop ready in 187.2s\n  app: http://localhost:18080\n"),
            "{text}"
        );
        assert!(text.contains("next: relish manual tour"), "{text}");
        assert!(
            text.contains("remove it: relish local destroy --name laptop --yes"),
            "{text}"
        );
    }

    #[test]
    fn an_unreachable_api_is_the_last_thing_people_read() {
        let text = servers_report(Some("open port 9117".to_string())).to_string();
        assert!(text.ends_with("\nwarning: open port 9117\n"), "{text}");
        assert!(text.contains("back up /home/me/.reliaburger/clusters/servers/security"));
    }
}
