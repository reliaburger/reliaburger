//! Evidence that instances adopted after a restart or self-upgrade already
//! run their placement.
//!
//! The placement reconciler records an assignment `Pending` before it deploys
//! and `Applied` only once the deploy has finished. A Bun that restarts or
//! upgrades in between comes back to a `Pending` entry for workloads that are
//! running fine, and adopts them. Deploying again would roll every replica
//! (surge-first by default) for nothing. So adoption remembers the spec each
//! adopted app was launched from, and the reconciler asks before it deploys.

use std::collections::{HashMap, HashSet};

use super::{BunAgent, Grill, InstanceId};
use crate::config::Replicas;
use crate::config::app::AppSpec;
use crate::grill::state::ContainerState;

/// What adoption learned about one app's instances.
#[derive(Debug)]
pub(super) struct AdoptedApp {
    /// The spec every adopted instance was launched from, or `None` when
    /// their records disagree or don't say.
    spec: Option<AppSpec>,
    instances: HashSet<InstanceId>,
}

/// Adopted apps by `(name, namespace)`.
pub(super) type AdoptedApps = HashMap<(String, String), AdoptedApp>;

impl<G: Grill + Clone + 'static> BunAgent<G> {
    /// Remember an adopted application instance and the spec its record says
    /// it was launched from. `image` is the instance's own recorded image,
    /// which must agree with that spec.
    pub(super) fn note_adopted_instance(
        &mut self,
        key: &(String, String),
        instance_id: &InstanceId,
        spec: Option<&AppSpec>,
        image: &str,
    ) {
        let spec = spec.filter(|spec| spec.image.as_deref().unwrap_or_default() == image);
        match self.adopted_apps.get_mut(key) {
            Some(adopted) => {
                if adopted.spec.as_ref() != spec {
                    adopted.spec = None;
                }
                adopted.instances.insert(instance_id.clone());
            }
            None => {
                self.adopted_apps.insert(
                    key.clone(),
                    AdoptedApp {
                        spec: spec.cloned(),
                        instances: HashSet::from([instance_id.clone()]),
                    },
                );
            }
        }
    }

    /// Forget adoption evidence for an app this agent deploys again.
    pub(super) fn forget_adopted_app(&mut self, app_name: &str, namespace: &str) {
        self.adopted_apps
            .remove(&(app_name.to_string(), namespace.to_string()));
    }

    /// Whether the app's live instances are exactly the adopted ones, all
    /// running, as many as `spec` asks for, and launched from `spec`.
    pub(super) fn adopted_instances_match(
        &self,
        app_name: &str,
        namespace: &str,
        spec: &AppSpec,
    ) -> bool {
        let key = (app_name.to_string(), namespace.to_string());
        let Some(adopted) = self.adopted_apps.get(&key) else {
            return false;
        };
        let Some(launched) = &adopted.spec else {
            return false;
        };
        let Replicas::Fixed(replicas) = spec.replicas else {
            return false;
        };
        if !launched_as(launched, spec) {
            return false;
        }
        let current: Vec<_> = self
            .supervisor
            .list_instances()
            .into_iter()
            .filter(|instance| instance.app_name == app_name && instance.namespace == namespace)
            .collect();
        current.len() == replicas as usize
            && current.len() == adopted.instances.len()
            && current.iter().all(|instance| {
                adopted.instances.contains(&instance.id)
                    && instance.state == ContainerState::Running
                    && !instance.retry_pending
            })
    }
}

/// Whether an instance launched from `launched` runs `wanted`. A signature
/// check pins a tag to its digest before launch, so a launched image of
/// `repository@digest` matches a wanted `repository:tag`.
fn launched_as(launched: &AppSpec, wanted: &AppSpec) -> bool {
    if launched == wanted {
        return true;
    }
    let (Some(ran), Some(asked)) = (launched.image.as_deref(), wanted.image.as_deref()) else {
        return false;
    };
    if asked.contains('@') {
        return false;
    }
    let (repository, _tag) = crate::meat::scheduler::split_repo_tag(asked);
    if !ran.starts_with(&format!("{repository}@")) {
        return false;
    }
    let mut unpinned = launched.clone();
    unpinned.image = wanted.image.clone();
    unpinned == *wanted
}

#[cfg(test)]
mod tests {
    use super::launched_as;
    use crate::config::app::AppSpec;

    fn spec(image: &str) -> AppSpec {
        toml::from_str(&format!("image = {image:?}\nreplicas = 2\n")).unwrap()
    }

    #[test]
    fn a_pinned_launch_matches_its_tag() {
        let digest = format!("sha256:{}", "a".repeat(64));
        assert!(launched_as(&spec("web:v1"), &spec("web:v1")));
        assert!(launched_as(
            &spec(&format!("web@{digest}")),
            &spec("web:v1")
        ));
        assert!(launched_as(
            &spec(&format!("localhost:5050/web@{digest}")),
            &spec("localhost:5050/web:v1")
        ));
    }

    #[test]
    fn a_different_image_or_setting_does_not_match() {
        let digest = format!("sha256:{}", "a".repeat(64));
        assert!(!launched_as(&spec("web:v1"), &spec("web:v2")));
        assert!(!launched_as(
            &spec(&format!("api@{digest}")),
            &spec("web:v1")
        ));
        let mut more = spec("web:v1");
        more.replicas = crate::config::Replicas::Fixed(3);
        assert!(!launched_as(&spec("web:v1"), &more));
        let mut pinned_elsewhere = spec(&format!("web@{digest}"));
        pinned_elsewhere.port = Some(8080);
        assert!(!launched_as(&pinned_elsewhere, &spec("web:v1")));
    }
}
