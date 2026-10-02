//! Work a turn hands to a task and finishes when the task reports back
//! (#351, stage 3).
//!
//! Some work needs the loop at both ends but not in the middle. An upgrade
//! must stop taking new work before it downloads the next binary, and exec it
//! once the download is staged; the download itself can take minutes. The
//! perimeter firewall is decided from the loop's view of membership, but the
//! `nft` subprocess that applies it can take half a second. Egress allowlists
//! are re-resolved against DNS, which can take seconds a host, and the
//! answers must be applied to the bindings as they are by then. So the turn that
//! starts such work spawns its slow middle into `follow_ups`, and the task's
//! result comes back as a [`FollowUp`] through its own `select!` branch. The
//! loop applies it there, one turn at a time like everything else.
//!
//! The pattern is the restart steps' and the state sweeps' (stage 2), with
//! one enum for every kind of work instead of a `JoinSet` each.

use tokio::sync::oneshot;

use super::{BunAgent, BunError, ContainerState, Grill, InstanceId};

/// The result of work a turn spawned, for the loop to finish.
pub(super) enum FollowUp {
    /// An upgrade or rollback fetched, verified and staged its binary.
    UpgradePrepared(UpgradePreparation),
    /// `nft` applied, or refused, the ruleset for these inputs.
    FirewallApplied {
        inputs: crate::firewall::rules::PerimeterInputs,
        result: Result<(), crate::firewall::rules::FirewallError>,
    },
    /// A kill, pause or resume fault's signals were sent, or weren't.
    Signalled(super::signal_faults::Signalled),
    /// A node-pressure helper started or stopped.
    NodePressure(super::node_pressure_work::PressureDone),
    /// The egress allowlists were re-resolved.
    #[cfg(all(feature = "ebpf", target_os = "linux"))]
    EgressResolved(Vec<super::egress_resolution::Resolution>),
}

/// How a follow-up task ended, as `JoinSet::join_next_with_id` yields it.
pub(super) type FollowUpOutcome = Result<(tokio::task::Id, FollowUp), tokio::task::JoinError>;

/// Which upgrade command a preparation answers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum UpgradeKind {
    /// `UpgradeApply` for this upgrade id.
    Apply { upgrade_id: String },
    /// `UpgradeRollback`.
    Rollback,
}

/// A finished upgrade preparation and the caller waiting on it.
pub(super) struct UpgradePreparation {
    kind: UpgradeKind,
    /// `Ok(None)`: the same upgrade was already in flight on this node.
    prepared:
        Result<Option<crate::upgrade::manager::PreparedUpgrade>, crate::upgrade::UpgradeError>,
    response: oneshot::Sender<Result<(), BunError>>,
}

/// A running workload the upgrade marker must find alive after the swap,
/// before its pid is known.
struct InventoryEntry {
    id: InstanceId,
    namespace: String,
    app_name: String,
}

/// Read each workload's pid. A workload whose runtime reports none is left
/// out, as it always was: the marker can only check pids it has.
async fn read_inventory<G: Grill>(
    grill: &G,
    entries: Vec<InventoryEntry>,
) -> Vec<crate::upgrade::marker::InstanceInventory> {
    let mut inventory = Vec::new();
    for entry in entries {
        let Ok(Some(pid)) = grill.pid(&entry.id).await else {
            continue;
        };
        let replica_index = crate::grill::InstanceIdentity::parse(&entry.id.0)
            .map(|ident| ident.ordinal)
            .unwrap_or(0);
        inventory.push(crate::upgrade::marker::InstanceInventory {
            namespace: entry.namespace,
            app_name: entry.app_name,
            instance_id: replica_index,
            pid,
            full_id: entry.id.0,
        });
    }
    inventory
}

impl<G: Grill + Clone + 'static> BunAgent<G> {
    /// Finish whatever a spawned task reported.
    pub(super) async fn apply_follow_up(&mut self, done: FollowUpOutcome) {
        match done {
            Ok((_, FollowUp::UpgradePrepared(preparation))) => {
                self.finish_upgrade_preparation(preparation).await;
            }
            Ok((_, FollowUp::FirewallApplied { inputs, result })) => {
                self.firewall_applying = None;
                match result {
                    Ok(()) => self.last_firewall_inputs = Some(inputs),
                    Err(error) => eprintln!("warning: firewall reconciliation failed: {error}"),
                }
            }
            Ok((_, FollowUp::Signalled(done))) => self.finish_signal_fault(done),
            Ok((_, FollowUp::NodePressure(done))) => {
                self.finish_node_pressure(done).await;
            }
            #[cfg(all(feature = "ebpf", target_os = "linux"))]
            Ok((_, FollowUp::EgressResolved(resolutions))) => {
                self.egress_resolving = None;
                self.apply_egress_resolutions(resolutions).await;
            }
            Err(error) => {
                // A panicked task drops its caller's answer, which the caller
                // sees as a closed channel; say why here.
                eprintln!("bun: a task the agent loop started failed: {error}");
                if self
                    .upgrade_preparing
                    .as_ref()
                    .is_some_and(|(_, task)| *task == error.id())
                {
                    self.upgrade_preparing = None;
                    self.draining
                        .store(false, std::sync::atomic::Ordering::Relaxed);
                }
                if self.firewall_applying == Some(error.id()) {
                    self.firewall_applying = None;
                }
                if self.egress_resolving == Some(error.id()) {
                    self.egress_resolving = None;
                }
            }
        }
    }

    /// The running workloads an upgrade's marker records, without pids yet.
    fn upgrade_inventory_entries(&self) -> Vec<InventoryEntry> {
        self.supervisor
            .list_instances()
            .into_iter()
            .filter(|instance| !instance.is_job && instance.state == ContainerState::Running)
            .map(|instance| InventoryEntry {
                id: instance.id.clone(),
                namespace: instance.namespace.clone(),
                app_name: instance.app_name.clone(),
            })
            .collect()
    }

    /// Refuse a second upgrade or rollback while one is still preparing.
    /// A re-delivered `UpgradeApply` for the upgrade in flight is answered
    /// `Ok`, as a re-delivery after the marker was written always was.
    fn refuse_concurrent_upgrade(
        &self,
        kind: &UpgradeKind,
        response: oneshot::Sender<Result<(), BunError>>,
    ) -> Option<oneshot::Sender<Result<(), BunError>>> {
        let Some((preparing, _)) = &self.upgrade_preparing else {
            return Some(response);
        };
        let answer = match (preparing, kind) {
            (UpgradeKind::Apply { upgrade_id }, UpgradeKind::Apply { upgrade_id: asked })
                if upgrade_id == asked =>
            {
                Ok(())
            }
            (UpgradeKind::Apply { upgrade_id }, _) => Err(BunError::Upgrade(
                crate::upgrade::UpgradeError::AlreadyInFlight {
                    upgrade_id: upgrade_id.clone(),
                },
            )),
            (UpgradeKind::Rollback, _) => Err(BunError::Upgrade(
                crate::upgrade::UpgradeError::AlreadyInFlight {
                    upgrade_id: "rollback".into(),
                },
            )),
        };
        let _ = response.send(answer);
        None
    }

    /// Node-level upgrade: stop taking new work, then fetch, verify and
    /// stage the binary off the loop (#351, decision 4). The loop answers
    /// and execs when the preparation reports back. On any failure the node
    /// keeps running the current version, undrained.
    pub(super) fn begin_upgrade_apply(
        &mut self,
        directive: crate::upgrade::types::UpgradeDirective,
        response: oneshot::Sender<Result<(), BunError>>,
    ) {
        let Some(manager) = self.upgrade.clone() else {
            let _ = response.send(Err(BunError::UpgradesUnavailable));
            return;
        };
        let kind = UpgradeKind::Apply {
            upgrade_id: directive.upgrade_id.clone(),
        };
        let Some(response) = self.refuse_concurrent_upgrade(&kind, response) else {
            return;
        };
        // Stop taking new work while the swap is in progress. Running
        // workloads are untouched (and survive the exec; see grill).
        self.draining
            .store(true, std::sync::atomic::Ordering::Relaxed);
        let entries = self.upgrade_inventory_entries();
        let grill = self.supervisor.grill().clone();
        let preparing = kind.clone();
        let task = self.follow_ups.spawn(async move {
            let inventory = read_inventory(&grill, entries).await;
            let prepared = manager.prepare(&directive, inventory).await;
            FollowUp::UpgradePrepared(UpgradePreparation {
                kind,
                prepared,
                response,
            })
        });
        self.upgrade_preparing = Some((preparing, task.id()));
    }

    /// Node-level rollback: the same swap, with no download or re-verify.
    /// Staging the old binary and writing the marker still touch the disk,
    /// so they run off the loop too.
    pub(super) fn begin_upgrade_rollback(
        &mut self,
        version: Option<crate::upgrade::BinaryVersion>,
        response: oneshot::Sender<Result<(), BunError>>,
    ) {
        let Some(manager) = self.upgrade.clone() else {
            let _ = response.send(Err(BunError::UpgradesUnavailable));
            return;
        };
        let kind = UpgradeKind::Rollback;
        let Some(response) = self.refuse_concurrent_upgrade(&kind, response) else {
            return;
        };
        self.draining
            .store(true, std::sync::atomic::Ordering::Relaxed);
        let entries = self.upgrade_inventory_entries();
        let grill = self.supervisor.grill().clone();
        let preparing = kind.clone();
        let task = self.follow_ups.spawn(async move {
            let inventory = read_inventory(&grill, entries).await;
            let prepared = manager.prepare_rollback(version, inventory).await.map(Some);
            FollowUp::UpgradePrepared(UpgradePreparation {
                kind,
                prepared,
                response,
            })
        });
        self.upgrade_preparing = Some((preparing, task.id()));
    }

    /// Answer the caller, then exec the staged binary. Only the exec, and
    /// the moment before it that lets the answer flush, stay on the loop.
    async fn finish_upgrade_preparation(&mut self, preparation: UpgradePreparation) {
        self.upgrade_preparing = None;
        let UpgradePreparation {
            kind,
            prepared,
            response,
        } = preparation;
        let Some(manager) = self.upgrade.clone() else {
            let _ = response.send(Err(BunError::UpgradesUnavailable));
            return;
        };
        let prepared = match prepared {
            Ok(Some(prepared)) => prepared,
            Ok(None) => {
                // Same upgrade already in flight: idempotent OK.
                let _ = response.send(Ok(()));
                return;
            }
            Err(error) => {
                self.draining
                    .store(false, std::sync::atomic::Ordering::Relaxed);
                let _ = response.send(Err(BunError::Upgrade(error)));
                return;
            }
        };

        match &kind {
            UpgradeKind::Apply { upgrade_id } => println!(
                "bun: upgrading to {} (upgrade {upgrade_id})",
                prepared.target_version()
            ),
            UpgradeKind::Rollback => {
                println!("bun: rolling back to {}", prepared.target_version())
            }
        }
        // Respond before the point of no return, and give the HTTP layer a
        // moment to flush the response: exec closes every socket.
        let _ = response.send(Ok(()));
        // LOOP-INLINE: 200 ms on purpose, so the answer flushes before exec replaces the process
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;

        // Only returns on failure (the symlink is already reverted then).
        let error = manager.execute(prepared);
        self.draining
            .store(false, std::sync::atomic::Ordering::Relaxed);
        let what = match kind {
            UpgradeKind::Apply { .. } => "upgrade",
            UpgradeKind::Rollback => "rollback",
        };
        eprintln!(
            "bun: {what} exec failed, still on {}: {error}",
            manager.running_version()
        );
    }

    /// Apply `ruleset` for `inputs` with `nft`, off the loop. A test
    /// that stalls the firewall stands in for `nft` entirely, so the harness
    /// never rewrites the host's firewall.
    pub(super) fn spawn_perimeter_apply(
        &mut self,
        ruleset: String,
        inputs: crate::firewall::rules::PerimeterInputs,
    ) {
        #[cfg(test)]
        let stalls = std::sync::Arc::clone(&self.loop_stalls);
        let task = self.follow_ups.spawn(async move {
            #[cfg(test)]
            if stalls.hold(super::LoopStall::Firewall).await {
                return FollowUp::FirewallApplied {
                    inputs,
                    result: Ok(()),
                };
            }
            let result = crate::firewall::rules::apply_ruleset(&ruleset).await;
            FollowUp::FirewallApplied { inputs, result }
        });
        self.firewall_applying = Some(task.id());
    }
}
