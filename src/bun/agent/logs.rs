//! Log reads that run off the agent loop (#351, stage 3).
//!
//! `relish logs` used to read every instance's whole capture into one
//! `String` inside a turn, and `relish logs -f --tail N` pushed the tail
//! into the API's 64-slot channel from the loop. A 56 MB capture (#278's)
//! held every caller for hundreds of milliseconds, and a client that stopped
//! reading (`| less`, a stuck proxy) held them for as long as it liked. Now
//! the loop only picks which instances to read, which is in memory, and a
//! task does the reading and the sending.

use tokio::sync::{mpsc, oneshot};

use super::{BunAgent, BunError, Grill, InstanceId, tail_lines};

/// Every instance's capture, one after another. More than one instance gets
/// a `==> id <==` header each, like `tail` over several files.
async fn read_captures<G: Grill>(grill: &G, instance_ids: &[InstanceId]) -> String {
    let mut all_logs = String::new();
    for id in instance_ids {
        let logs = grill.logs(id).await.unwrap_or_default();
        if logs.is_empty() {
            continue;
        }
        if instance_ids.len() > 1 {
            all_logs.push_str(&format!("==> {id} <==\n"));
        }
        all_logs.push_str(&logs);
        if !logs.ends_with('\n') {
            all_logs.push('\n');
        }
    }
    all_logs
}

/// Send the last `tail` lines of each capture, then follow every instance
/// until the client goes away. The tail is sent first and in instance order,
/// so a follow never interleaves live lines into it.
async fn send_tail_then_follow<G: Grill + Clone + 'static>(
    grill: G,
    instance_ids: Vec<InstanceId>,
    tail: Option<usize>,
    label: Option<String>,
    lines: mpsc::Sender<String>,
) {
    let prefix = |id: &InstanceId| {
        label
            .as_deref()
            .map(|node| format!("[{node} {}] ", id.0))
            .unwrap_or_default()
    };
    if let Some(n) = tail {
        for id in &instance_ids {
            let logs = grill.logs(id).await.unwrap_or_default();
            let prefix = prefix(id);
            for line in tail_lines(&logs, n).lines() {
                if lines.send(format!("{prefix}{line}")).await.is_err() {
                    return;
                }
            }
        }
    }

    // One follow per instance, each streaming through its own channel, so a
    // labelled follow can stamp every line with its node and instance.
    for id in instance_ids {
        let prefix = prefix(&id);
        let (instance_tx, mut instance_rx) =
            mpsc::channel::<crate::ketchup::types::CapturedLine>(64);
        let follower = grill.clone();
        tokio::spawn(async move {
            // A live follow shows the instance's whole capture.
            follower
                .follow_logs(&id, instance_tx, &Default::default())
                .await;
        });
        let tx = lines.clone();
        tokio::spawn(async move {
            while let Some(captured) = instance_rx.recv().await {
                if tx.send(format!("{prefix}{}", captured.line)).await.is_err() {
                    return;
                }
            }
        });
    }
}

impl<G: Grill + Clone + 'static> BunAgent<G> {
    /// Instances of `app_name` in `namespace`, or `AppNotFound`.
    fn app_instance_ids(
        &self,
        app_name: &str,
        namespace: &str,
    ) -> Result<Vec<InstanceId>, BunError> {
        let ids: Vec<InstanceId> = self
            .supervisor
            .list_instances()
            .into_iter()
            .filter(|instance| instance.app_name == app_name && instance.namespace == namespace)
            .map(|instance| instance.id.clone())
            .collect();
        if ids.is_empty() {
            return Err(BunError::AppNotFound {
                app_name: app_name.to_string(),
                namespace: namespace.to_string(),
            });
        }
        Ok(ids)
    }

    /// Answer a `Logs` command from a task: every instance's capture, cut to
    /// the last `tail` lines when asked.
    pub(super) fn spawn_logs_read(
        &self,
        app_name: &str,
        namespace: &str,
        tail: Option<usize>,
        response: oneshot::Sender<Result<String, BunError>>,
    ) {
        let instance_ids = match self.app_instance_ids(app_name, namespace) {
            Ok(ids) => ids,
            Err(error) => {
                let _ = response.send(Err(error));
                return;
            }
        };
        let grill = self.supervisor.grill().clone();
        tokio::spawn(async move {
            let logs = read_captures(&grill, &instance_ids).await;
            let logs = match tail {
                Some(n) => tail_lines(&logs, n),
                None => logs,
            };
            let _ = response.send(Ok(logs));
        });
    }

    /// Start a `FollowLogs` stream from a task, of every instance of the app
    /// or only `instance`. An app with no instances to follow closes the
    /// stream at once, as it always has.
    pub(super) fn spawn_logs_follow(
        &self,
        app_name: &str,
        namespace: &str,
        tail: Option<usize>,
        instance: Option<String>,
        label: Option<String>,
        lines: mpsc::Sender<String>,
    ) {
        let Ok(mut instance_ids) = self.app_instance_ids(app_name, namespace) else {
            return;
        };
        if let Some(instance) = &instance {
            instance_ids.retain(|id| &id.0 == instance);
        }
        tokio::spawn(send_tail_then_follow(
            self.supervisor.grill().clone(),
            instance_ids,
            tail,
            label,
            lines,
        ));
    }
}
