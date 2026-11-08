//! `relish apps`: one row per app, problems first.
//!
//! The list `relish status` used to print, moved here so it can grow
//! filters for large clusters. `relish apps` rolls each app's instances up
//! with the same [`app_state`] the cluster summary and `relish agent
//! context` use; `relish apps NAME` lists that app's instances, and
//! `relish apps --instances` every instance, as the old `status` table did.

use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::future::Future;
use std::time::Duration;

use futures_util::StreamExt as _;
use serde::Serialize;

use super::client::BunClient;
use super::{OutputFormat, RelishError};
use crate::bun::agent::{ClusterInstanceStatus, NodeStatus};
use crate::bun::diagnostics::DesiredAppEvidence;
use crate::bun::summary::{AppState, app_state};

/// How long each node gets to answer before it's reported as not answering.
pub const NODE_TIMEOUT: Duration = Duration::from_secs(3);

/// How many nodes are asked at once.
const FAN_OUT: usize = 32;

/// The flags `relish apps` takes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AppsArgs {
    /// List this app's instances instead of the apps.
    pub name: Option<String>,
    /// Only this namespace.
    pub namespace: Option<String>,
    /// Only apps that aren't running and ready.
    pub unhealthy: bool,
    /// Only apps with an instance on this node.
    pub node: Option<String>,
    /// List every instance of every app.
    pub instances: bool,
}

/// One app, rolled up from its instances.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AppRow {
    /// The app's namespace.
    pub namespace: String,
    /// The app's name.
    pub app: String,
    /// How it's doing, from the shared rollup.
    pub state: AppState,
    /// Replicas it wants.
    pub desired: u32,
    /// Replicas running (and so passing their health checks).
    pub running: u32,
    /// Restarts across its instances.
    pub restarts: u32,
    /// The nodes its instances are on.
    pub nodes: Vec<String>,
    /// Its oldest instance's age, in seconds; `None` with no instance.
    pub age_seconds: Option<u64>,
    /// Why it isn't fully placed or running, when the cluster knows.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Every node's instances, and each node that didn't answer with why.
#[derive(Debug, Clone, Default)]
pub struct Instances {
    /// The instances of the nodes that answered.
    pub rows: Vec<ClusterInstanceStatus>,
    /// `(node, why)` for each node that didn't.
    pub unanswered: Vec<(String, String)>,
}

/// Run `relish apps` against the configured endpoint.
pub async fn run(args: AppsArgs, output: OutputFormat) -> Result<(), RelishError> {
    run_with_client(&args, output, &BunClient::default_local()).await
}

/// Run `relish apps` with an explicit client.
pub async fn run_with_client(
    args: &AppsArgs,
    output: OutputFormat,
    client: &BunClient,
) -> Result<(), RelishError> {
    let instances = collect_instances(client).await?;
    for (node, why) in &instances.unanswered {
        eprintln!("warning: node {node} didn't answer ({why}); its instances aren't listed");
    }
    if args.instances || args.name.is_some() {
        // The council's reasons are extra detail: a list without them still
        // answers.
        let desired = client.desired_apps().await.unwrap_or_default();
        let rows = filter_instances(&instances.rows, args);
        let desired: Vec<DesiredAppEvidence> = desired
            .into_iter()
            .filter(|app| app_matches(&app.app, &app.namespace, args))
            .collect();
        return print(&rows, output, || render_instances(&rows, &desired));
    }
    let desired = client.desired_apps().await?;
    let rows = filter_apps(rollup(&desired, &instances), args);
    print(&rows, output, || render_apps(&rows, &desired))
}

fn print<T: Serialize>(
    value: &T,
    output: OutputFormat,
    human: impl FnOnce() -> String,
) -> Result<(), RelishError> {
    match output {
        OutputFormat::Human => print!("{}", human()),
        OutputFormat::Json => println!(
            "{}",
            serde_json::to_string_pretty(value).map_err(RelishError::SerialiseJson)?
        ),
        OutputFormat::Yaml => print!(
            "{}",
            yaml_serde::to_string(value).map_err(RelishError::SerialiseYaml)?
        ),
    }
    Ok(())
}

/// Ask every node gossip doesn't count as gone, through the entry node's
/// relay, `NODE_TIMEOUT` each. Returns each answer with its node, and each
/// node that didn't answer with why.
pub(crate) async fn ask_each_node<T, F, Fut>(
    client: &BunClient,
    nodes: &[NodeStatus],
    ask: F,
) -> (Vec<(String, T)>, Vec<(String, String)>)
where
    F: Fn(BunClient) -> Fut,
    Fut: Future<Output = Result<T, RelishError>>,
{
    let requests = nodes.iter().filter(|node| !node.is_down()).map(|node| {
        let name = node.node_id.clone();
        let answer = client.via_node(&name).map(&ask);
        async move {
            let answer = match answer {
                Ok(request) => match tokio::time::timeout(NODE_TIMEOUT, request).await {
                    Ok(Ok(value)) => Ok(value),
                    Ok(Err(error)) => Err(error.to_string()),
                    Err(_) => Err(format!("no answer in {} s", NODE_TIMEOUT.as_secs())),
                },
                Err(error) => Err(error.to_string()),
            };
            (name, answer)
        }
    });
    let mut answers = futures_util::stream::iter(requests).buffer_unordered(FAN_OUT);
    let mut answered = Vec::new();
    let mut unanswered = Vec::new();
    while let Some((name, answer)) = answers.next().await {
        match answer {
            Ok(value) => answered.push((name, value)),
            Err(why) => unanswered.push((name, why)),
        }
    }
    answered.sort_by(|a, b| a.0.cmp(&b.0));
    unanswered.sort();
    (answered, unanswered)
}

/// Every instance on every node that answered. A standalone node answers
/// for itself.
pub async fn collect_instances(client: &BunClient) -> Result<Instances, RelishError> {
    let nodes = client.nodes().await?;
    if nodes.is_empty() {
        return Ok(Instances {
            rows: client.cluster_status().await?,
            unanswered: Vec::new(),
        });
    }
    let (answered, unanswered) =
        ask_each_node(client, &nodes, |node| async move { node.status().await }).await;
    let mut rows: Vec<ClusterInstanceStatus> = answered
        .into_iter()
        .flat_map(|(node, instances)| {
            instances
                .into_iter()
                .map(move |instance| ClusterInstanceStatus {
                    node: node.clone(),
                    instance,
                })
        })
        .collect();
    rows.sort_by(|a, b| {
        (&a.node, &a.instance.namespace, &a.instance.id).cmp(&(
            &b.node,
            &b.instance.namespace,
            &b.instance.id,
        ))
    });
    Ok(Instances { rows, unanswered })
}

/// One row per app the council wants, worst first, then by namespace and
/// name.
pub fn rollup(desired: &[DesiredAppEvidence], instances: &Instances) -> Vec<AppRow> {
    let nodes_unanswered = !instances.unanswered.is_empty();
    let mut rows: Vec<AppRow> = desired
        .iter()
        .map(|app| {
            let own: Vec<&ClusterInstanceStatus> = instances
                .rows
                .iter()
                .filter(|row| {
                    row.instance.app_name == app.app && row.instance.namespace == app.namespace
                })
                .collect();
            let running = own
                .iter()
                .filter(|row| row.instance.state == "running")
                .count();
            let running = u32::try_from(running).unwrap_or(u32::MAX);
            let nodes: BTreeSet<&str> = own.iter().map(|row| row.node.as_str()).collect();
            AppRow {
                namespace: app.namespace.clone(),
                app: app.app.clone(),
                state: app_state(app.desired_replicas, running, nodes_unanswered, false),
                desired: app.desired_replicas,
                running,
                restarts: own.iter().map(|row| row.instance.restart_count).sum(),
                nodes: nodes.into_iter().map(str::to_string).collect(),
                age_seconds: own.iter().filter_map(|row| row.instance.age_seconds).max(),
                reason: reason(app, &own),
            }
        })
        .collect();
    rows.sort_by(|a, b| (a.state, &a.namespace, &a.app).cmp(&(b.state, &b.namespace, &b.app)));
    rows
}

/// Why an app isn't fully placed or running, as the council or a node's
/// runtime says.
fn reason(app: &DesiredAppEvidence, instances: &[&ClusterInstanceStatus]) -> Option<String> {
    if let Some(blocked) = &app.blocked {
        return Some(format!("not placed, blocked: {blocked}"));
    }
    if let Some(failure) = &app.rollout_failed {
        return Some(format!(
            "RolloutFailed on {}: {}",
            failure.node, failure.reason
        ));
    }
    if let Some(home) = &app.volume_home_away {
        return Some(format!("waits for {home}, which holds its volume"));
    }
    if let Some(short) = &app.unschedulable {
        return Some(format!("unschedulable: {short}"));
    }
    instances.iter().find_map(|row| {
        row.instance
            .reason
            .as_ref()
            .map(|why| format!("{} failed on {}: {why}", row.instance.id, row.node))
    })
}

fn app_matches(app: &str, namespace: &str, args: &AppsArgs) -> bool {
    args.name.as_deref().is_none_or(|name| name == app)
        && args.namespace.as_deref().is_none_or(|ns| ns == namespace)
}

/// `rows` narrowed by `--namespace`, `--unhealthy` and `--node`.
pub fn filter_apps(rows: Vec<AppRow>, args: &AppsArgs) -> Vec<AppRow> {
    rows.into_iter()
        .filter(|row| {
            args.namespace
                .as_deref()
                .is_none_or(|ns| ns == row.namespace)
        })
        .filter(|row| !args.unhealthy || is_unhealthy(row))
        .filter(|row| {
            args.node
                .as_deref()
                .is_none_or(|node| row.nodes.iter().any(|n| n == node))
        })
        .collect()
}

/// Anything not running and ready: down, degraded, troubled or unknown.
pub fn is_unhealthy(row: &AppRow) -> bool {
    row.state < AppState::Running
}

/// Instances narrowed by NAME, `--namespace`, `--node` and `--unhealthy`
/// (an instance that isn't running).
pub fn filter_instances(
    rows: &[ClusterInstanceStatus],
    args: &AppsArgs,
) -> Vec<ClusterInstanceStatus> {
    rows.iter()
        .filter(|row| app_matches(&row.instance.app_name, &row.instance.namespace, args))
        .filter(|row| args.node.as_deref().is_none_or(|node| node == row.node))
        .filter(|row| !args.unhealthy || row.instance.state != "running")
        .cloned()
        .collect()
}

/// A compact age: `40s`, `12m`, `5h`, `29d`; `-` when unknown.
fn age_cell(seconds: Option<u64>) -> String {
    match seconds {
        None => "-".to_string(),
        Some(s) if s >= 86_400 => format!("{}d", s / 86_400),
        Some(s) if s >= 3_600 => format!("{}h", s / 3_600),
        Some(s) if s >= 60 => format!("{}m", s / 60),
        Some(s) => format!("{s}s"),
    }
}

/// The nodes column: up to three names, then how many more.
fn nodes_cell(nodes: &[String]) -> String {
    match nodes.len() {
        0 => "-".to_string(),
        n if n <= 3 => nodes.join(","),
        n => format!("{},+{}", nodes[..3].join(","), n - 3),
    }
}

/// The `relish apps` table, then one line for each listed app that says
/// why it isn't fully running.
pub fn render_apps(rows: &[AppRow], desired: &[DesiredAppEvidence]) -> String {
    if desired.is_empty() {
        return "no apps deployed\n".to_string();
    }
    if rows.is_empty() {
        return "no apps match\n".to_string();
    }
    let mut out = format!(
        "{:<14} {:<20} {:<10} {:<7} {:<8} {:<24} {}\n",
        "NAMESPACE", "APP", "STATE", "READY", "RESTARTS", "NODES", "AGE"
    );
    for row in rows {
        let _ = writeln!(
            out,
            "{:<14} {:<20} {:<10} {:<7} {:<8} {:<24} {}",
            row.namespace,
            row.app,
            row.state.word(),
            format!("{}/{}", row.running, row.desired),
            row.restarts,
            nodes_cell(&row.nodes),
            age_cell(row.age_seconds)
        );
    }
    let notes: Vec<String> = rows
        .iter()
        .filter_map(|row| {
            row.reason
                .as_ref()
                .map(|why| format!("{} (namespace {}) {why}", row.app, row.namespace))
        })
        .collect();
    if !notes.is_empty() {
        out.push('\n');
        for note in notes {
            let _ = writeln!(out, "{note}");
        }
    }
    out
}

/// One row per instance, as `relish apps NAME` and `relish apps
/// --instances` list them, then why any app isn't fully placed.
pub fn render_instances(
    statuses: &[ClusterInstanceStatus],
    desired: &[DesiredAppEvidence],
) -> String {
    let mut output = String::new();
    if statuses.is_empty() {
        output.push_str("no instances\n");
    } else {
        let _ = writeln!(
            output,
            "{:<24} {:<20} {:<15} {:<12} {:<10} {:<10} {:<8} AGE",
            "NODE", "INSTANCE", "APP", "NAMESPACE", "STATE", "PID", "RESTARTS"
        );
        for row in statuses {
            let s = &row.instance;
            let _ = writeln!(
                output,
                "{:<24} {:<20} {:<15} {:<12} {:<10} {:<10} {:<8} {}",
                row.node,
                s.id,
                s.app_name,
                s.namespace,
                s.state,
                super::commands::pid_cell(s),
                s.restart_count,
                age_cell(s.age_seconds)
            );
        }
    }
    let blocked: Vec<_> = desired
        .iter()
        .filter_map(|app| app.blocked.as_ref().map(|reason| (app, reason)))
        .collect();
    let waiting: Vec<_> = desired
        .iter()
        .filter_map(|app| app.volume_home_away.as_ref().map(|home| (app, home)))
        .collect();
    let failed: Vec<_> = desired
        .iter()
        .filter_map(|app| app.rollout_failed.as_ref().map(|failure| (app, failure)))
        .collect();
    let short: Vec<_> = desired
        .iter()
        .filter_map(|app| app.unschedulable.as_ref().map(|short| (app, short)))
        .collect();
    let refused: Vec<_> = statuses
        .iter()
        .filter_map(|row| row.instance.reason.as_ref().map(|reason| (row, reason)))
        .collect();
    if !blocked.is_empty()
        || !waiting.is_empty()
        || !failed.is_empty()
        || !short.is_empty()
        || !refused.is_empty()
    {
        output.push('\n');
    }
    // #881: the runtime refused the launch, so waiting won't start it.
    for (row, reason) in refused {
        let _ = writeln!(
            output,
            "{} ({}, namespace {}) failed on {}: {reason}",
            row.instance.id, row.instance.app_name, row.instance.namespace, row.node
        );
    }
    for (app, reason) in blocked {
        let _ = writeln!(
            output,
            "{} (namespace {}) is not placed, blocked: {reason}",
            app.app, app.namespace
        );
    }
    // #423: the app isn't lost, it waits for the node holding its data.
    for (app, home) in waiting {
        let _ = writeln!(
            output,
            "{} (namespace {}) waits for {home}, which holds its volume and is out of the cluster",
            app.app, app.namespace
        );
    }
    // #692: the leader won't try this spec again until a different one is applied.
    for (app, failure) in failed {
        let _ = writeln!(
            output,
            "{} (namespace {}) RolloutFailed on {}: {}; apply a different spec or roll back",
            app.app, app.namespace, failure.node, failure.reason
        );
    }
    // #702: the scheduler placed what fits and says why the rest has no node.
    for (app, short) in short {
        let _ = writeln!(
            output,
            "{} (namespace {}) is unschedulable: {short}",
            app.app, app.namespace
        );
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    fn evidence(app: &str, replicas: u32) -> DesiredAppEvidence {
        DesiredAppEvidence {
            owner: None,
            app: app.to_string(),
            namespace: "prod".to_string(),
            desired_replicas: replicas,
            scheduled_replicas: 0,
            placements: Default::default(),
            service_port: None,
            blocked: None,
            volume_home_away: None,
            unschedulable: None,
            rollout_failed: None,
            volume_homes: Vec::new(),
        }
    }

    fn instance(node: &str, app: &str, index: u32, state: &str) -> ClusterInstanceStatus {
        ClusterInstanceStatus {
            node: node.to_string(),
            instance: crate::bun::agent::InstanceStatus {
                id: format!("prod__{app}-{index}"),
                app_name: app.to_string(),
                namespace: "prod".to_string(),
                state: state.to_string(),
                restart_count: index,
                host_port: None,
                exit_code: None,
                pid: Some(100 + index),
                runtime_unknown: false,
                status_age_ms: None,
                age_seconds: Some(3_600 * u64::from(index + 1)),
                reason: None,
            },
        }
    }

    fn cluster() -> (Vec<DesiredAppEvidence>, Instances) {
        let desired = vec![
            evidence("web", 3),
            evidence("api", 2),
            evidence("db", 1),
            evidence("batch", 0),
        ];
        let instances = Instances {
            rows: vec![
                instance("node-1", "web", 0, "running"),
                instance("node-2", "web", 1, "running"),
                instance("node-3", "web", 2, "running"),
                instance("node-1", "api", 0, "running"),
                instance("node-2", "api", 1, "failed"),
            ],
            unanswered: Vec::new(),
        };
        (desired, instances)
    }

    #[test]
    fn apps_roll_up_their_instances_with_problems_first() {
        let (desired, instances) = cluster();
        let rows = rollup(&desired, &instances);
        let order: Vec<(&str, AppState)> = rows.iter().map(|r| (r.app.as_str(), r.state)).collect();
        assert_eq!(
            order,
            [
                ("db", AppState::Down),
                ("api", AppState::Degraded),
                ("web", AppState::Running),
                ("batch", AppState::Stopped),
            ]
        );
        let web = &rows[2];
        assert_eq!((web.running, web.desired, web.restarts), (3, 3, 3));
        assert_eq!(web.nodes, ["node-1", "node-2", "node-3"]);
        assert_eq!(web.age_seconds, Some(3 * 3_600), "the oldest instance");
        insta::assert_snapshot!(render_apps(&rows, &desired));
    }

    #[test]
    fn a_node_that_did_not_answer_makes_short_apps_unknown() {
        let (desired, mut instances) = cluster();
        instances.unanswered = vec![("node-3".to_string(), "timed out".to_string())];
        let rows = rollup(&desired, &instances);
        let api = rows.iter().find(|r| r.app == "api").unwrap();
        assert_eq!(api.state, AppState::Unknown);
    }

    #[test]
    fn unhealthy_keeps_only_apps_that_are_not_running_and_ready() {
        let (desired, instances) = cluster();
        let args = AppsArgs {
            unhealthy: true,
            ..AppsArgs::default()
        };
        let rows = filter_apps(rollup(&desired, &instances), &args);
        let names: Vec<&str> = rows.iter().map(|r| r.app.as_str()).collect();
        assert_eq!(names, ["db", "api"]);
    }

    #[test]
    fn node_and_namespace_filters_narrow_the_list() {
        let (desired, instances) = cluster();
        let on_node_3 = AppsArgs {
            node: Some("node-3".to_string()),
            ..AppsArgs::default()
        };
        let rows = filter_apps(rollup(&desired, &instances), &on_node_3);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].app, "web");
        let elsewhere = AppsArgs {
            namespace: Some("other".to_string()),
            ..AppsArgs::default()
        };
        assert!(filter_apps(rollup(&desired, &instances), &elsewhere).is_empty());
        assert_eq!(render_apps(&[], &desired), "no apps match\n");
        assert_eq!(render_apps(&[], &[]), "no apps deployed\n");
    }

    #[test]
    fn apps_name_lists_that_apps_instances() {
        let (_, instances) = cluster();
        let args = AppsArgs {
            name: Some("api".to_string()),
            ..AppsArgs::default()
        };
        let rows = filter_instances(&instances.rows, &args);
        assert_eq!(rows.len(), 2);
        insta::assert_snapshot!(render_instances(&rows, &[]));
        let unhealthy = AppsArgs {
            unhealthy: true,
            ..args
        };
        assert_eq!(filter_instances(&instances.rows, &unhealthy).len(), 1);
    }

    #[test]
    fn json_rows_carry_state_ready_and_nodes() {
        let (desired, instances) = cluster();
        let rows = rollup(&desired, &instances);
        let value = serde_json::to_value(&rows).unwrap();
        assert_eq!(value[0]["app"], "db");
        assert_eq!(value[0]["state"], "down");
        assert_eq!(value[2]["nodes"][2], "node-3");
        assert_eq!(value[2]["running"], 3);
    }

    #[test]
    fn a_long_node_list_shows_three_and_a_count() {
        let nodes: Vec<String> = (1..=5).map(|n| format!("n{n}")).collect();
        assert_eq!(nodes_cell(&nodes), "n1,n2,n3,+2");
        assert_eq!(nodes_cell(&[]), "-");
        assert_eq!(age_cell(Some(59)), "59s");
        assert_eq!(age_cell(Some(2 * 86_400)), "2d");
    }

    /// #326: an app the namespace quota keeps off every node has no
    /// instance row, so the list names it and says why.
    #[test]
    fn instances_name_an_over_quota_app_and_why() {
        let blocked = DesiredAppEvidence {
            blocked: Some(crate::meat::quota::QuotaError::CpuExceeded {
                namespace: "prod".to_string(),
                current: 0,
                requested: 1600,
                limit: 1000,
            }),
            ..evidence("greedy", 2)
        };
        let output = render_instances(&[], &[evidence("fine", 1), blocked]);
        assert_eq!(
            output,
            "no instances\n\n\
             greedy (namespace prod) is not placed, blocked: \
             namespace \"prod\" would exceed CPU quota: 0+1600 > 1000m\n"
        );
        let rows = rollup(
            &[evidence("fine", 0), {
                let mut greedy = evidence("greedy", 2);
                greedy.blocked = Some(crate::meat::quota::QuotaError::CpuExceeded {
                    namespace: "prod".to_string(),
                    current: 0,
                    requested: 1600,
                    limit: 1000,
                });
                greedy
            }],
            &Instances::default(),
        );
        assert_eq!(
            rows[0].reason.as_deref(),
            Some("not placed, blocked: namespace \"prod\" would exceed CPU quota: 0+1600 > 1000m")
        );
    }

    /// #692: an app whose rollout failed shows `RolloutFailed`, where and
    /// why, and how to clear it.
    #[test]
    fn instances_name_an_app_whose_rollout_failed() {
        let failed = DesiredAppEvidence {
            rollout_failed: Some(crate::meat::deploy_types::RolloutFailure {
                fingerprint: "abc".to_string(),
                node: "node-3".to_string(),
                reason: "web-g2-0 failed its health check".to_string(),
                at_unix_ms: 1,
            }),
            ..evidence("web", 3)
        };
        let output = render_instances(&[], &[evidence("fine", 1), failed.clone()]);
        assert_eq!(
            output,
            "no instances\n\n\
             web (namespace prod) RolloutFailed on node-3: web-g2-0 failed its health check; \
             apply a different spec or roll back\n"
        );
        let rows = rollup(std::slice::from_ref(&failed), &Instances::default());
        assert!(render_apps(&rows, &[failed]).contains(
            "web (namespace prod) RolloutFailed on node-3: web-g2-0 failed its health check"
        ));
    }

    /// #423: an app waiting for the node that holds its volume says which
    /// node it waits for.
    #[test]
    fn instances_name_a_volume_app_waiting_for_its_home_node() {
        let waiting = DesiredAppEvidence {
            volume_home_away: Some("node-2".to_string()),
            ..evidence("db", 1)
        };
        let output = render_instances(&[], &[evidence("fine", 1), waiting]);
        assert_eq!(
            output,
            "no instances\n\n\
             db (namespace prod) waits for node-2, which holds its volume and is out of the \
             cluster\n"
        );
    }

    /// #702: an app placed only in part says how many replicas found a
    /// node and why the rest didn't.
    #[test]
    fn instances_say_how_much_of_a_short_app_is_placed_and_why() {
        let short = DesiredAppEvidence {
            unschedulable: Some(crate::meat::types::Unschedulable {
                placed: 2,
                wanted: 3,
                reason: crate::meat::types::UnplacedReason::NoRoom,
            }),
            ..evidence("web", 3)
        };
        let output = render_instances(&[], &[evidence("fine", 1), short]);
        assert_eq!(
            output,
            "no instances\n\n\
             web (namespace prod) is unschedulable: placed 2 of 3 replicas: \
             no node that can run it has room\n"
        );
    }

    /// #881: an instance the runtime refused to launch says why under the
    /// table, so a missing binary isn't just a `failed` cell.
    #[test]
    fn instances_name_one_that_could_not_launch_and_why() {
        let mut row = instance("node-1", "ghost", 0, "failed");
        row.instance.id = "default__ghost-0".to_string();
        row.instance.namespace = "default".to_string();
        row.instance.reason = Some("binary not found: /opt/ghost/server".to_string());
        let output = render_instances(std::slice::from_ref(&row), &[]);
        assert!(
            output.ends_with(
                "\ndefault__ghost-0 (ghost, namespace default) failed on node-1: \
                 binary not found: /opt/ghost/server\n"
            ),
            "{output}"
        );
        let mut ghost = evidence("ghost", 1);
        ghost.namespace = "default".to_string();
        let rows = rollup(
            &[ghost],
            &Instances {
                rows: vec![row],
                unanswered: Vec::new(),
            },
        );
        assert_eq!(
            rows[0].reason.as_deref(),
            Some("default__ghost-0 failed on node-1: binary not found: /opt/ghost/server")
        );
    }

    #[tokio::test]
    async fn apps_returns_agent_unreachable() {
        let err = run_with_client(
            &AppsArgs::default(),
            OutputFormat::Human,
            &BunClient::new("http://127.0.0.1:1"),
        )
        .await
        .unwrap_err();
        assert!(
            matches!(err, RelishError::AgentUnreachable { .. }),
            "got: {err:?}"
        );
    }
}
