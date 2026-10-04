//! Typed directory defaults shared by configuration resolution paths.
use std::collections::BTreeMap;

use serde::Deserialize;

use super::app::DeploySpec;
use super::{Config, EnvValue, ResourceRange};

/// Only advertised defaults are accepted; unsupported keys never disappear.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub(crate) struct WorkloadDefaults {
    image: Option<String>,
    #[serde(with = "super::types::memory_range")]
    memory: Option<ResourceRange>,
    #[serde(with = "super::types::cpu_range")]
    cpu: Option<ResourceRange>,
    env: BTreeMap<String, EnvValue>,
    deploy: Option<DeploySpec>,
}

impl WorkloadDefaults {
    /// Child scalars and individual nested keys override inherited defaults.
    pub(crate) fn inherit(&self, parent: Option<&Self>) -> Self {
        let Some(parent) = parent else {
            return self.clone();
        };
        let mut env = parent.env.clone();
        env.extend(self.env.clone());
        Self {
            image: self.image.clone().or_else(|| parent.image.clone()),
            memory: self.memory.or(parent.memory),
            cpu: self.cpu.or(parent.cpu),
            env,
            deploy: merge_deploy(self.deploy.as_ref(), parent.deploy.as_ref()),
        }
    }

    /// Explicit workload fields win, including zero and false deploy settings.
    pub(crate) fn apply(&self, config: &mut Config) {
        for app in config.app.values_mut() {
            if app.image.is_none() && app.exec.is_none() && app.script.is_none() {
                app.image.clone_from(&self.image);
            }
            app.memory = app.memory.or(self.memory);
            app.cpu = app.cpu.or(self.cpu);
            let mut env = self.env.clone();
            env.extend(std::mem::take(&mut app.env));
            app.env = env;
            app.deploy = merge_deploy(app.deploy.as_ref(), self.deploy.as_ref());
        }
        for job in config.job.values_mut() {
            if job.image.is_none() && job.exec.is_none() && job.script.is_none() {
                job.image.clone_from(&self.image);
            }
            job.memory = job.memory.or(self.memory);
            job.cpu = job.cpu.or(self.cpu);
            let mut env = self.env.clone();
            env.extend(std::mem::take(&mut job.env));
            job.env = env;
        }
    }
}

fn merge_deploy(
    explicit: Option<&DeploySpec>,
    inherited: Option<&DeploySpec>,
) -> Option<DeploySpec> {
    match (explicit, inherited) {
        (None, inherited) => inherited.cloned(),
        (Some(explicit), None) => Some(explicit.clone()),
        (Some(explicit), Some(inherited)) => Some(DeploySpec {
            strategy: explicit
                .strategy
                .clone()
                .or_else(|| inherited.strategy.clone()),
            max_surge: explicit.max_surge.or(inherited.max_surge),
            max_unavailable: explicit.max_unavailable.or(inherited.max_unavailable),
            drain_timeout: explicit
                .drain_timeout
                .clone()
                .or_else(|| inherited.drain_timeout.clone()),
            health_timeout: explicit
                .health_timeout
                .clone()
                .or_else(|| inherited.health_timeout.clone()),
            auto_rollback: explicit.auto_rollback.or(inherited.auto_rollback),
        }),
    }
}
