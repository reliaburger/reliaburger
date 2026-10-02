//! Raising an app's replica count without rolling the replicas it runs.
//!
//! The placement reconciler hands this node its share of an app as the app's
//! spec with `replicas` set to that share. When another node dies, a
//! survivor's share grows by one and nothing else changes. Rolling every
//! replica for that would stop the healthy ones, and on a cluster that has
//! just lost a node their address release waits for the lost node's view
//! lease, so they linger as stopped instances for a minute (#346). A deploy
//! that only raises the count starts the new replicas beside the old ones.

use super::{BunAgent, Grill};
use crate::config::Replicas;
use crate::config::app::AppSpec;
use crate::grill::state::ContainerState;

impl<G: Grill + Clone + 'static> BunAgent<G> {
    /// The ordinals deploying `spec` as `ordinals` adds beside the replicas
    /// already running, when the deploy changes nothing but a higher replica
    /// count and every running replica's ordinal is still among `ordinals`.
    ///
    /// `None` means the deploy must roll as usual: the app is new, its spec
    /// changed, its count didn't grow, a running replica's ordinal is no
    /// longer assigned here, or not every replica it runs is running (a
    /// stopped app, a crash-looping replica, a rollout that failed halfway).
    /// Call it before the new spec is stored.
    pub(super) fn replicas_to_add_in_place(
        &self,
        app_name: &str,
        namespace: &str,
        spec: &AppSpec,
        ordinals: &[u32],
    ) -> Option<Vec<u32>> {
        let key = (app_name.to_string(), namespace.to_string());
        let previous = self.deployed_specs.get(&key)?;
        let (running, _) = replicas_added(previous, spec)?;
        let instances: Vec<_> = self
            .supervisor
            .list_instances()
            .into_iter()
            .filter(|instance| {
                !instance.is_job
                    && instance.app_name == app_name
                    && instance.namespace == namespace
                    && !self.deferred_retirements.contains(&instance.id)
            })
            .collect();
        let all_running = instances
            .iter()
            .all(|instance| instance.state == ContainerState::Running && !instance.retry_pending);
        if !all_running || instances.len() != running as usize {
            return None;
        }
        let mut running_ordinals = std::collections::BTreeSet::new();
        for instance in &instances {
            let ordinal = crate::grill::InstanceIdentity::parse(&instance.id.0)?.ordinal;
            if !ordinals.contains(&ordinal) || !running_ordinals.insert(ordinal) {
                return None;
            }
        }
        let added: Vec<u32> = ordinals
            .iter()
            .copied()
            .filter(|ordinal| !running_ordinals.contains(ordinal))
            .collect();
        (!added.is_empty()).then_some(added)
    }
}

/// When `wanted` is `previous` with more fixed replicas and nothing else
/// changed: the previous count and how many more `wanted` asks for.
fn replicas_added(previous: &AppSpec, wanted: &AppSpec) -> Option<(u32, u32)> {
    let (Replicas::Fixed(before), Replicas::Fixed(after)) = (previous.replicas, wanted.replicas)
    else {
        return None;
    };
    if after <= before {
        return None;
    }
    let mut grown = previous.clone();
    grown.replicas = wanted.replicas;
    (grown == *wanted).then_some((before, after - before))
}

#[cfg(test)]
mod tests {
    use super::replicas_added;
    use crate::config::Replicas;
    use crate::config::app::AppSpec;

    fn spec(image: &str, replicas: u32) -> AppSpec {
        toml::from_str(&format!("image = {image:?}\nreplicas = {replicas}\n")).unwrap()
    }

    #[test]
    fn only_a_higher_count_adds_replicas() {
        assert_eq!(
            replicas_added(&spec("web:v1", 1), &spec("web:v1", 3)),
            Some((1, 2))
        );
        assert_eq!(replicas_added(&spec("web:v1", 3), &spec("web:v1", 3)), None);
        assert_eq!(replicas_added(&spec("web:v1", 3), &spec("web:v1", 2)), None);
    }

    #[test]
    fn any_other_change_rolls() {
        assert_eq!(replicas_added(&spec("web:v1", 1), &spec("web:v2", 2)), None);
        let mut daemon = spec("web:v1", 1);
        daemon.replicas = Replicas::DaemonSet;
        assert_eq!(replicas_added(&spec("web:v1", 1), &daemon), None);
        assert_eq!(replicas_added(&daemon, &spec("web:v1", 2)), None);
    }
}
