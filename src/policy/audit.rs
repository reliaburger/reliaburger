//! `relish policy audit` (#1119): which committed apps and jobs the
//! policies in force would now refuse, warn about or record.
//!
//! A new policy never touches running work. An app judged under last
//! week's policies keeps running under them until it's applied again. This
//! module answers the question that leaves open: what would happen if it
//! were? It judges every committed workload the way an apply would, and
//! changes nothing. Kubernetes users will know the idea from Gatekeeper's
//! audit, here without a controller: it runs when someone asks.

use std::collections::BTreeMap;

use crate::config::{AppSpec, JobSpec, PolicySpec};
use crate::council::types::DesiredState;
use crate::meat::types::AppId;

use super::{Caller, PolicySet, Source, Verdict, judge_app, judge_job};

/// What `GET /v1/policies/audit` answers.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PolicyAudit {
    /// The policy generation the audit judged under.
    pub generation: u64,
    /// How many policies are in force.
    pub policies: usize,
    /// How many committed apps were judged.
    pub apps: usize,
    /// How many committed job definitions were judged.
    pub jobs: usize,
    /// What the policies said about them.
    pub verdict: Verdict,
    /// Set when a policy in force no longer compiles: nothing could be
    /// judged until an admin replaces or deletes it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub broken: Option<String>,
}

/// One committed app, as an audit judges it.
#[derive(Debug, Clone)]
pub struct CommittedApp {
    /// The app's identity.
    pub id: AppId,
    /// Its spec as declared (#1118), or as stored when the cluster kept no
    /// declared spec.
    pub spec: AppSpec,
    /// Whether GitOps owns it, so it's judged as a GitOps sync would.
    pub gitops_owned: bool,
}

/// Judge every committed app and job in `desired` by the policies in force.
///
/// An app is judged as an apply would judge it: its declared spec (#1118)
/// where the cluster kept one, filled in by the defaults, validated, then
/// every `deny`. A job is judged from its stored definition, its schedule
/// put back, with its image read back as the tag it was bound from.
pub fn audit(desired: &DesiredState) -> PolicyAudit {
    let mut apps: Vec<CommittedApp> = desired
        .apps
        .iter()
        .map(|(id, stored)| {
            // Without a declared spec (a rollback wrote this one), the
            // stored image is read back as the tag it was bound from.
            let spec = desired.declared_apps.get(id).cloned().unwrap_or_else(|| {
                let mut spec = stored.clone();
                spec.image = spec.image.as_deref().map(unbound);
                spec
            });
            CommittedApp {
                id: id.clone(),
                spec,
                gitops_owned: desired.gitops_owned.apps.contains(id),
            }
        })
        .collect();
    apps.sort_by(|left, right| left.id.cmp(&right.id));
    let jobs: Vec<(AppId, JobSpec)> = desired
        .task_arrays
        .jobs()
        .definitions()
        .filter_map(|(key, record)| {
            let (namespace, name) = key.split_once('/')?;
            let mut spec = record.definition.template.clone();
            spec.schedule = record
                .definition
                .cron
                .as_ref()
                .map(|cron| cron.expression.clone());
            spec.image = spec.image.as_deref().map(unbound);
            Some((AppId::new(name, namespace), spec))
        })
        .collect();
    audit_workloads(&desired.policies, desired.policy_generation, &apps, &jobs)
}

/// Judge `apps` and `jobs` by `policies`, at `generation`. An app GitOps
/// owns is judged as a GitOps sync, by the caller `gitops`; any other
/// workload as the cluster's own `audit` caller, with the source `apply`.
pub fn audit_workloads(
    policies: &BTreeMap<String, PolicySpec>,
    generation: u64,
    apps: &[CommittedApp],
    jobs: &[(AppId, JobSpec)],
) -> PolicyAudit {
    let mut audit = PolicyAudit {
        generation,
        policies: policies.len(),
        apps: apps.len(),
        jobs: jobs.len(),
        ..PolicyAudit::default()
    };
    if policies.is_empty() {
        return audit;
    }
    let set = match PolicySet::compile(policies) {
        Ok(set) => set,
        Err(error) => {
            audit.broken = Some(error.to_string());
            return audit;
        }
    };
    for app in apps {
        let (caller, source) = if app.gitops_owned {
            (Caller::gitops(), Source::GitOps)
        } else {
            (Caller::audit(), Source::Apply)
        };
        let judgement = judge_app(&set, &app.id, &app.spec, &caller, source);
        audit.verdict.extend(judgement.verdict);
    }
    for (id, spec) in jobs {
        let verdict = judge_job(&set, id, spec, &Caller::audit(), Source::Apply);
        audit.verdict.extend(verdict);
    }
    audit
}

/// An image reference as it was written before the leader bound its tag to
/// a digest: `web:v1@sha256:…` reads back as `web:v1`. A reference pinned
/// by digest alone, with no tag, stays as it is.
pub fn unbound(image: &str) -> String {
    match image.split_once('@') {
        Some((tagged, _)) if has_tag(tagged) => tagged.to_string(),
        _ => image.to_string(),
    }
}

/// Whether `reference` ends in a `:tag`, not a registry port.
fn has_tag(reference: &str) -> bool {
    let last = reference.rsplit('/').next().unwrap_or(reference);
    last.contains(':')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bound_tag_reads_back_as_the_tag() {
        let digest = format!("sha256:{}", "a".repeat(64));
        assert_eq!(unbound(&format!("web:v1@{digest}")), "web:v1");
        assert_eq!(
            unbound(&format!("registry:5000/team/web:latest@{digest}")),
            "registry:5000/team/web:latest"
        );
        assert_eq!(
            unbound(&format!("registry:5000/web@{digest}")),
            format!("registry:5000/web@{digest}")
        );
        assert_eq!(unbound("web:v1"), "web:v1");
    }
}
