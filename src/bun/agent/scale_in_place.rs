//! Changing an app's share on this node without rolling the replicas it keeps.
//!
//! The placement reconciler hands this node its share of an app as the app's
//! spec with `replicas` set to that share and `ordinals` naming the replicas.
//! When another node dies, a survivor's share grows by one. When the leader
//! moves replicas around, say through an upgrade walk, one ordinal can leave
//! while another arrives. Neither changes the version. Rolling every replica
//! for that would stop the healthy ones, and on a cluster that has just lost
//! a node their address release waits for the lost node's view lease, so they
//! linger as stopped instances for a minute (#346). On a full node the roll
//! can't even start, because a rolling replacement needs room beside the old
//! replica (#1111). So a deploy that changes only the share retires the
//! departing replicas and starts the arriving ones, and leaves the rest alone.

use std::collections::BTreeSet;

use super::{BunAgent, Grill};
use crate::config::Replicas;
use crate::config::app::AppSpec;
use crate::grill::InstanceId;
use crate::grill::state::ContainerState;

/// What a same-version change of this node's share does to the replicas it
/// runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct InPlaceChange {
    /// How many replicas to start beside the ones that stay.
    pub(super) add: u32,
    /// The running replicas whose ordinals left this node's share.
    pub(super) retire: Vec<InstanceId>,
}

impl<G: Grill + Clone + 'static> BunAgent<G> {
    /// How deploying `spec` changes the replicas this node runs, when the
    /// deploy changes nothing but this node's share of the same version.
    ///
    /// An empty change is the spec this node already runs, sent again after
    /// a capacity refusal here: the refused deploy changed nothing, and the
    /// leader has since moved the replicas it couldn't start, so there is
    /// nothing to add and nothing to roll (#971).
    ///
    /// `None` means the deploy must roll as usual: the app is new, its spec
    /// changed, its share didn't, or the replicas it runs aren't exactly the
    /// previous share, all running (a stopped app, a crash-looping replica,
    /// a rollout that failed halfway). Call it before the new spec is stored.
    pub(super) fn share_change_in_place(
        &self,
        app_name: &str,
        namespace: &str,
        spec: &AppSpec,
    ) -> Option<InPlaceChange> {
        let key = (app_name.to_string(), namespace.to_string());
        let previous = self.deployed_specs.get(&key)?;
        let change = share_change(previous, spec)?;
        let unchanged = change.arriving.is_empty() && change.departing.is_empty();
        if unchanged && !self.capacity_refusals.contains_key(&key) {
            return None;
        }
        let instances: Vec<_> = self
            .supervisor
            .list_instances()
            .into_iter()
            .filter(|instance| {
                instance.app_name == app_name
                    && instance.namespace == namespace
                    && !self.deferred_retirements.contains(&instance.id)
            })
            .collect();
        let all_running = instances
            .iter()
            .all(|instance| instance.state == ContainerState::Running && !instance.retry_pending);
        let running: BTreeSet<u32> = instances
            .iter()
            .filter_map(|instance| crate::grill::InstanceIdentity::parse(&instance.id.0))
            .map(|identity| identity.ordinal)
            .collect();
        let before: BTreeSet<u32> = previous.replica_ordinals().into_iter().collect();
        // One running instance per ordinal of the previous share, no more.
        if !all_running || instances.len() != before.len() || running != before {
            return None;
        }
        let retire = instances
            .iter()
            .filter(|instance| {
                crate::grill::InstanceIdentity::parse(&instance.id.0)
                    .is_some_and(|identity| change.departing.contains(&identity.ordinal))
            })
            .map(|instance| instance.id.clone())
            .collect();
        Some(InPlaceChange {
            add: change.arriving.len() as u32,
            retire,
        })
    }
}

/// The ordinals that join and leave this node's share between `previous`
/// and `wanted`, each in ascending order.
#[derive(Debug, PartialEq)]
pub(super) struct ShareChange {
    pub(super) arriving: Vec<u32>,
    pub(super) departing: Vec<u32>,
}

/// When `wanted` is `previous` with nothing changed but the fixed replica
/// count and the ordinals it assigns: which ordinals arrive and which leave.
/// `None` for any other change, which must roll.
fn share_change(previous: &AppSpec, wanted: &AppSpec) -> Option<ShareChange> {
    let (Replicas::Fixed(_), Replicas::Fixed(_)) = (previous.replicas, wanted.replicas) else {
        return None;
    };
    let mut same_version = previous.clone();
    same_version.replicas = wanted.replicas;
    same_version.ordinals.clone_from(&wanted.ordinals);
    if same_version != *wanted {
        return None;
    }
    let before: BTreeSet<u32> = previous.replica_ordinals().into_iter().collect();
    let after: BTreeSet<u32> = wanted.replica_ordinals().into_iter().collect();
    Some(ShareChange {
        arriving: after.difference(&before).copied().collect(),
        departing: before.difference(&after).copied().collect(),
    })
}

/// `previous` cut down to the replicas still running after `retired` left:
/// the spec this node runs when a share change stops partway.
pub(super) fn share_without(previous: &AppSpec, retired: &[InstanceId]) -> AppSpec {
    let gone: BTreeSet<u32> = retired
        .iter()
        .filter_map(|id| crate::grill::InstanceIdentity::parse(&id.0))
        .map(|identity| identity.ordinal)
        .collect();
    if gone.is_empty() {
        return previous.clone();
    }
    let kept: Vec<u32> = previous
        .replica_ordinals()
        .into_iter()
        .filter(|ordinal| !gone.contains(ordinal))
        .collect();
    let mut share = previous.clone();
    share.replicas = Replicas::Fixed(kept.len() as u32);
    share.ordinals = Some(kept);
    share
}

#[cfg(test)]
mod tests {
    use super::{ShareChange, share_change, share_without};
    use crate::config::Replicas;
    use crate::config::app::AppSpec;
    use crate::grill::InstanceId;

    fn spec(image: &str, replicas: u32) -> AppSpec {
        toml::from_str(&format!("image = {image:?}\nreplicas = {replicas}\n")).unwrap()
    }

    fn assigned(ordinals: &[u32]) -> AppSpec {
        let mut spec = spec("web:v1", 1);
        spec.replicas = Replicas::Fixed(ordinals.len() as u32);
        spec.ordinals = Some(ordinals.to_vec());
        spec
    }

    fn change(arriving: &[u32], departing: &[u32]) -> Option<ShareChange> {
        Some(ShareChange {
            arriving: arriving.to_vec(),
            departing: departing.to_vec(),
        })
    }

    #[test]
    fn a_higher_count_starts_the_new_replicas() {
        assert_eq!(
            share_change(&spec("web:v1", 1), &spec("web:v1", 3)),
            change(&[1, 2], &[])
        );
    }

    #[test]
    fn a_grown_assignment_starts_only_the_arriving_ordinal() {
        assert_eq!(
            share_change(&assigned(&[0, 2]), &assigned(&[0, 1, 2])),
            change(&[1], &[])
        );
    }

    #[test]
    fn a_shrunk_assignment_retires_only_the_departing_ordinal() {
        assert_eq!(
            share_change(&assigned(&[0, 1, 2]), &assigned(&[0, 2])),
            change(&[], &[1])
        );
        assert_eq!(
            share_change(&spec("web:v1", 3), &spec("web:v1", 2)),
            change(&[], &[2])
        );
    }

    #[test]
    fn a_swapped_assignment_retires_and_starts_without_rolling() {
        assert_eq!(
            share_change(&assigned(&[0, 2]), &assigned(&[1, 3])),
            change(&[1, 3], &[0, 2])
        );
    }

    #[test]
    fn one_ordinal_out_and_one_in_keeps_the_one_that_stays() {
        assert_eq!(
            share_change(&assigned(&[0, 2]), &assigned(&[0, 3])),
            change(&[3], &[2])
        );
    }

    #[test]
    fn an_empty_share_that_gains_a_replica_starts_it() {
        assert_eq!(
            share_change(&assigned(&[]), &assigned(&[4])),
            change(&[4], &[])
        );
    }

    #[test]
    fn the_same_share_in_another_order_changes_nothing() {
        assert_eq!(
            share_change(&assigned(&[2, 0]), &assigned(&[0, 2])),
            change(&[], &[])
        );
    }

    #[test]
    fn a_version_change_rolls_even_when_the_share_moves() {
        let mut moved = assigned(&[0, 3]);
        moved.image = Some("web:v2".into());
        assert_eq!(share_change(&assigned(&[0, 2]), &moved), None);
        assert_eq!(share_change(&spec("web:v1", 1), &spec("web:v2", 2)), None);
    }

    #[test]
    fn a_daemon_set_always_rolls() {
        let mut daemon = spec("web:v1", 1);
        daemon.replicas = Replicas::DaemonSet;
        assert_eq!(share_change(&spec("web:v1", 1), &daemon), None);
        assert_eq!(share_change(&daemon, &spec("web:v1", 2)), None);
    }

    /// A share change that stops after retiring a replica records the
    /// share that still runs, so the retry starts only what's missing.
    #[test]
    fn a_share_cut_short_records_only_the_replicas_still_running() {
        let retired = [InstanceId("default__web-2".into())];
        assert_eq!(share_without(&assigned(&[0, 2]), &retired), assigned(&[0]));
        assert_eq!(share_without(&assigned(&[0, 2]), &[]), assigned(&[0, 2]));
        assert_eq!(
            share_change(
                &share_without(&assigned(&[0, 2]), &retired),
                &assigned(&[0, 3])
            ),
            change(&[3], &[])
        );
    }
}
