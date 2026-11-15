//! Undo everything the gym did, on every way out.
//!
//! The [`Ledger`] records each thing the gym creates the moment the cluster
//! confirms it: a fault with its node and id, an app, the solver's token.
//! [`clean_up`] undoes them and then checks the cluster agrees, taking each
//! item off the ledger only once it's gone. The runner calls it after every
//! run, whatever happened in the run: an answer, an error, a solver that
//! crashed or ran out of time, Ctrl-C, or a panic it caught.
//!
//! What if cleanup itself can't finish, or never runs because the process
//! is going down? Then the ledger's `Drop` prints the exact `relish`
//! commands that would finish it. Rust runs `Drop` when a value goes out of
//! scope, including while a panic unwinds the stack, which makes it the
//! last line of defence (it can't make network calls: `Drop` isn't `async`,
//! and the runtime may be gone). And for a `kill -9`, which runs nothing at
//! all, every fault carries a TTL and the token expires in a day.

use std::collections::BTreeSet;
use std::time::Duration;

use serde::Serialize;

use super::cluster::{GymCluster, PlacedFault};

/// Everything the gym created that still needs undoing.
#[derive(Debug)]
pub struct Ledger {
    namespace: String,
    faults: Vec<PlacedFault>,
    apps: BTreeSet<String>,
    token: Option<String>,
}

impl Ledger {
    /// An empty ledger for one namespace.
    pub fn new(namespace: &str) -> Self {
        Self {
            namespace: namespace.to_string(),
            faults: Vec::new(),
            apps: BTreeSet::new(),
            token: None,
        }
    }

    /// Record an app the gym is about to apply. It's recorded before the
    /// request, since an apply that times out may still land.
    pub fn app(&mut self, app: &str) {
        self.apps.insert(app.to_string());
    }

    /// Record faults the cluster placed.
    pub fn faults(&mut self, faults: &[PlacedFault]) {
        self.faults.extend_from_slice(faults);
    }

    /// Record the solver's token, once created.
    pub fn token(&mut self, name: &str) {
        self.token = Some(name.to_string());
    }

    /// Whether nothing is left to undo.
    pub fn is_empty(&self) -> bool {
        self.faults.is_empty() && self.apps.is_empty() && self.token.is_none()
    }

    /// The `relish` commands that would finish the cleanup by hand.
    pub fn manual_commands(&self) -> Vec<String> {
        let namespace = &self.namespace;
        let mut commands: Vec<String> = self
            .faults
            .iter()
            .map(|fault| match &fault.node {
                Some(node) => format!("relish fault clear {} --node {node}", fault.id),
                None => format!("relish fault clear {}", fault.id),
            })
            .collect();
        for app in &self.apps {
            commands.push(format!("relish fault clear {app} --namespace {namespace}"));
        }
        for app in &self.apps {
            commands.push(format!("relish delete {app} --namespace {namespace}"));
        }
        if let Some(token) = &self.token {
            commands.push(format!("relish token revoke {token}"));
        }
        commands
    }
}

impl Drop for Ledger {
    fn drop(&mut self) {
        if self.is_empty() {
            return;
        }
        // The last resort: cleanup didn't finish, so say how to finish it.
        eprintln!(
            "relish gym: cleanup in namespace `{}` didn't finish; run these to finish it:",
            self.namespace
        );
        for command in self.manual_commands() {
            eprintln!("  {command}");
        }
    }
}

/// What cleanup did, for the report.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct CleanupReport {
    /// Each fault cleared, as `id@node`.
    pub faults_cleared: Vec<String>,
    /// Each app deleted.
    pub apps_deleted: Vec<String>,
    /// The token revoked, if there was one.
    pub token_revoked: Option<String>,
    /// What went wrong on the way (an already-expired fault shows up here
    /// as a clear the cluster didn't need).
    pub errors: Vec<String>,
    /// The cluster shows no gym fault and no gym app in the namespace, and
    /// the token is revoked.
    pub verified: bool,
    /// What was still there when cleanup gave up.
    pub left_behind: Vec<String>,
}

/// How cleanup waits for the cluster to agree.
#[derive(Debug, Clone, Copy)]
pub struct CleanupPace {
    /// How long to wait for deleted apps to go.
    pub timeout: Duration,
    /// How often to look.
    pub poll: Duration,
}

impl Default for CleanupPace {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(120),
            poll: Duration::from_secs(2),
        }
    }
}

fn fault_name(fault: &PlacedFault) -> String {
    match &fault.node {
        Some(node) => format!("{}@{node}", fault.id),
        None => fault.id.to_string(),
    }
}

/// Undo everything on the ledger, then check the cluster agrees.
///
/// Every step runs whatever the one before it did: a fault that won't clear
/// doesn't stop the apps being deleted or the token being revoked.
pub async fn clean_up(
    cluster: &impl GymCluster,
    ledger: &mut Ledger,
    pace: CleanupPace,
) -> CleanupReport {
    let namespace = ledger.namespace.clone();
    let mut report = CleanupReport::default();

    for fault in &ledger.faults {
        match cluster.clear(fault).await {
            Ok(()) => report.faults_cleared.push(fault_name(fault)),
            // A kill is over the moment it lands, and a fault may have
            // expired: the listing below is what decides.
            Err(error) => report
                .errors
                .push(format!("clear fault {}: {error}", fault_name(fault))),
        }
    }
    // The backstop: anything on a gym app in this namespace, including a
    // fault whose answer never reached us.
    for app in &ledger.apps {
        if let Err(error) = cluster.clear_app(&namespace, app).await {
            report
                .errors
                .push(format!("clear faults on {app}: {error}"));
        }
    }
    for app in &ledger.apps {
        match cluster.delete(&namespace, app).await {
            Ok(()) => report.apps_deleted.push(app.clone()),
            Err(error) => report.errors.push(format!("delete {app}: {error}")),
        }
    }
    if let Some(token) = ledger.token.clone() {
        match cluster.revoke_token(&token).await {
            Ok(()) => {
                report.token_revoked = Some(token);
                ledger.token = None;
            }
            Err(error) => report.errors.push(format!("revoke token {token}: {error}")),
        }
    }

    let deadline = tokio::time::Instant::now() + pace.timeout;
    loop {
        let left = left_behind(cluster, ledger).await;
        match left {
            Ok(left) if left.is_empty() => {
                ledger.faults.clear();
                ledger.apps.clear();
                break;
            }
            Ok(left) => report.left_behind = left,
            Err(error) => report.left_behind = vec![format!("couldn't check: {error}")],
        }
        if tokio::time::Instant::now() >= deadline {
            break;
        }
        tokio::time::sleep(pace.poll).await;
    }
    if ledger.faults.is_empty() && ledger.apps.is_empty() {
        report.left_behind.clear();
    }
    report.verified = ledger.is_empty();
    report
}

/// What the cluster still shows of the ledger: gym faults and gym apps in
/// the namespace.
async fn left_behind(
    cluster: &impl GymCluster,
    ledger: &Ledger,
) -> Result<Vec<String>, super::cluster::ClusterError> {
    let namespace = ledger.namespace.as_str();
    let mut left: Vec<String> = cluster
        .active_faults()
        .await?
        .into_iter()
        .filter(|fault| {
            fault.namespace.as_deref() == Some(namespace) && ledger.apps.contains(&fault.app)
        })
        .map(|fault| {
            format!(
                "fault {} ({}) on {}",
                fault_name(&fault),
                fault.kind,
                fault.app
            )
        })
        .collect();
    let workloads = cluster.workloads(namespace).await?;
    left.extend(
        workloads
            .into_iter()
            .filter(|name| ledger.apps.contains(name))
            .map(|name| format!("app {name}")),
    );
    Ok(left)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_manual_commands_name_every_fault_app_and_token() {
        let mut ledger = Ledger::new("staging");
        ledger.app("gym-api");
        ledger.faults(&[PlacedFault {
            id: 4,
            node: Some("node-1".into()),
            app: "gym-api".into(),
            namespace: Some("staging".into()),
            kind: "pause".into(),
        }]);
        ledger.token("gym-solver-1");
        assert_eq!(
            ledger.manual_commands(),
            [
                "relish fault clear 4 --node node-1",
                "relish fault clear gym-api --namespace staging",
                "relish delete gym-api --namespace staging",
                "relish token revoke gym-solver-1",
            ]
        );
        // Nothing to undo here: this test only checks the text.
        ledger.faults.clear();
        ledger.apps.clear();
        ledger.token = None;
    }

    #[test]
    fn an_empty_ledger_has_nothing_to_say() {
        let ledger = Ledger::new("staging");
        assert!(ledger.is_empty());
        assert!(ledger.manual_commands().is_empty());
    }
}
