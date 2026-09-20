//! Restore durable policy authority before adopting surviving workloads.

use super::{BunAgent, BunError, Grill};
use crate::bun::egress_owners::PolicyPhase;
use crate::grill::{RuntimeLaunch, records::InstanceRecord};

impl<G: Grill + Clone + 'static> BunAgent<G> {
    /// Validate the complete policy inventory before runtime recovery mutates owners.
    pub(super) async fn restore_egress_owners(
        &mut self,
        records: &[InstanceRecord],
        launches: Option<&[RuntimeLaunch]>,
    ) -> Result<(), BunError> {
        let Some(directory) = self.records_dir.clone() else {
            return Ok(());
        };
        let owners =
            tokio::task::spawn_blocking(move || crate::bun::egress_owners::load(&directory))
                .await
                .map_err(|error| BunError::AdoptionState(error.to_string()))?
                .map_err(|error| BunError::AdoptionState(error.to_string()))?;
        for record in records {
            if record
                .app_spec
                .as_ref()
                .and_then(|spec| spec.egress.as_ref())
                .is_some_and(|policy| !policy.allow.is_empty())
                && !owners.contains_key(&crate::grill::InstanceId(record.instance_id.clone()))
            {
                return Err(BunError::AdoptionState(format!(
                    "protected instance {} has no policy ownership or retirement evidence",
                    record.instance_id
                )));
            }
        }
        for (id, owner) in &owners {
            if owner.runtime != self.supervisor.grill().runtime_kind() {
                return Err(BunError::AdoptionState(format!(
                    "egress owner {id} belongs to another runtime"
                )));
            }
            let record = records.iter().find(|record| record.instance_id == id.0);
            if let Some(record) = record {
                if record.oci_spec != owner.original_spec
                    || record
                        .app_spec
                        .as_ref()
                        .and_then(|spec| spec.egress.as_ref())
                        .is_none_or(|policy| policy.allow != owner.allow)
                {
                    return Err(BunError::AdoptionState(format!(
                        "egress owner {id} conflicts with adoption input"
                    )));
                }
            } else if owner.phase == PolicyPhase::Owned && launches.is_none() {
                return Err(BunError::AdoptionState(format!(
                    "egress owner {id} has no complete runtime inventory"
                )));
            }
            if let Some(launches) = launches
                && owner.phase == PolicyPhase::Owned
                && !launches
                    .iter()
                    .any(|launch| launch.instance_id == *id && launch.spec == owner.original_spec)
            {
                return Err(BunError::AdoptionState(format!(
                    "egress owner {id} conflicts with original runtime intent"
                )));
            }
        }
        #[cfg(all(feature = "ebpf", target_os = "linux"))]
        {
            let mut owners = owners;
            for owner in owners.values_mut() {
                if owner.phase == PolicyPhase::Owned {
                    owner.resolved = Self::resolve_owned_egress(&owner.allow).await;
                }
            }
            self.egress_bindings = owners;
            self.egress_store_uncertain = false;
            let mut retained = self.egress_bindings.clone();
            retained.retain(|id, owner| {
                owner.phase == PolicyPhase::Owned
                    || records.iter().any(|record| record.instance_id == id.0)
            });
            if retained.len() != self.egress_bindings.len() {
                self.persist_egress_owners(retained.clone()).await?;
                self.egress_bindings = retained;
            }
        }
        #[cfg(not(all(feature = "ebpf", target_os = "linux")))]
        if !owners.is_empty() {
            return Err(BunError::AdoptionState(
                "durable egress owners require Linux eBPF support".into(),
            ));
        }
        Ok(())
    }

    #[cfg(all(feature = "ebpf", target_os = "linux"))]
    /// Publish original ownership, fencing later mutations after uncertain writes.
    pub(super) async fn persist_egress_owners(
        &mut self,
        owners: std::collections::HashMap<
            crate::grill::InstanceId,
            crate::bun::egress_owners::EgressBinding,
        >,
    ) -> Result<(), BunError> {
        if self.egress_store_uncertain {
            return Err(BunError::AdoptionState(
                "egress ownership persistence is uncertain; restart to recover the checkpoint"
                    .into(),
            ));
        }
        let Some(directory) = self.records_dir.clone() else {
            return Ok(());
        };
        // Set this before awaiting the worker. An error after rename/fsync may
        // have changed durable state; no later mutation may overwrite it blindly.
        self.egress_store_uncertain = true;
        tokio::task::spawn_blocking(move || crate::bun::egress_owners::persist(&directory, owners))
            .await
            .map_err(|error| BunError::AdoptionState(error.to_string()))?
            .map_err(|error| BunError::AdoptionState(error.to_string()))?;
        self.egress_store_uncertain = false;
        Ok(())
    }

    #[cfg(all(feature = "ebpf", target_os = "linux"))]
    /// Forget positive policy-retirement evidence after adoption metadata is gone.
    pub(super) async fn forget_retired_egress_owner(
        &mut self,
        id: &crate::grill::InstanceId,
    ) -> Result<(), BunError> {
        let Some(owner) = self.egress_bindings.get(id) else {
            return Ok(());
        };
        if owner.phase != PolicyPhase::Retired {
            return Err(BunError::AdoptionState(format!(
                "egress owner {id} has not confirmed kernel retirement"
            )));
        }
        let mut remaining = self.egress_bindings.clone();
        remaining.remove(id);
        self.persist_egress_owners(remaining).await?;
        self.egress_bindings.remove(id);
        Ok(())
    }

    #[cfg(not(all(feature = "ebpf", target_os = "linux")))]
    /// Forget positive policy-retirement evidence after adoption metadata is gone.
    pub(super) async fn forget_retired_egress_owner(
        &mut self,
        _id: &crate::grill::InstanceId,
    ) -> Result<(), BunError> {
        Ok(())
    }

    #[cfg(all(feature = "ebpf", target_os = "linux"))]
    /// Resolve within a bounded wait, denying all destinations on failure.
    pub(super) async fn resolve_owned_egress(
        allow: &[String],
    ) -> Vec<crate::sesame::egress::EgressDestination> {
        let allow = allow.to_vec();
        match tokio::time::timeout(
            std::time::Duration::from_secs(5),
            tokio::task::spawn_blocking(move || {
                crate::sesame::egress::resolve_egress_entries(&allow)
            }),
        )
        .await
        {
            Ok(Ok(Ok(resolved))) => resolved,
            _ => {
                eprintln!(
                    "sesame: egress resolution unavailable; retaining deny-all until re-resolution"
                );
                Vec::new()
            }
        }
    }

    #[cfg(all(feature = "ebpf", target_os = "linux"))]
    /// Require retained ownership and enforcement before publishing a live adopter.
    pub(super) async fn restore_live_egress(
        &mut self,
        id: &crate::grill::InstanceId,
        record: &InstanceRecord,
    ) -> Result<(), BunError> {
        let protected = record
            .app_spec
            .as_ref()
            .and_then(|spec| spec.egress.as_ref())
            .is_some_and(|policy| !policy.allow.is_empty());
        if !protected {
            return Ok(());
        }
        let fail = |reason: &str| {
            BunError::AdoptionState(format!("cannot restore egress for {id}: {reason}"))
        };
        let binding = self
            .egress_bindings
            .get(id)
            .ok_or_else(|| fail("original policy ownership is missing"))?;
        let (boot_id, cgroup) = {
            let path = binding
                .original_spec
                .linux
                .host_cgroup_path()
                .ok_or_else(|| fail("original cgroup path is missing"))?;
            tokio::task::spawn_blocking(move || {
                Ok::<_, std::io::Error>((
                    crate::bun::egress_owners::boot_id()?,
                    crate::sesame::egress::cgroup_id_of_path(&path),
                ))
            })
            .await
            .map_err(|error| BunError::AdoptionState(error.to_string()))?
            .map_err(|error| BunError::AdoptionState(error.to_string()))?
        };
        if binding.phase != PolicyPhase::Owned
            || binding.boot_id != boot_id
            || cgroup != Some(binding.cgroup_id)
            || !self.supervisor.grill().honours_cgroup_path()
        {
            return Err(fail("original runtime cgroup identity is unavailable"));
        }
        let handle = self
            .onion_ebpf
            .clone()
            .ok_or_else(|| fail("kernel data path is unavailable"))?;
        let mut ebpf = handle.lock().await;
        if !ebpf.is_attached()
            || !ebpf.connect6_attached()
            || !ebpf.sendmsg4_attached()
            || !ebpf.sendmsg6_attached()
            || !crate::sesame::egress::egress_enforced(&mut ebpf.bpf, binding.cgroup_id)
                .map_err(|error| BunError::AdoptionState(error.to_string()))?
        {
            return Err(fail("original enforcement is unavailable"));
        }
        let cgroup_id = binding.cgroup_id;
        drop(ebpf);
        self.reprogram_cgroup_egress(cgroup_id, None)
            .await
            .map_err(|error| BunError::AdoptionState(error.to_string()))
    }

    #[cfg(not(all(feature = "ebpf", target_os = "linux")))]
    /// Require retained ownership and enforcement before publishing a live adopter.
    pub(super) async fn restore_live_egress(
        &mut self,
        _id: &crate::grill::InstanceId,
        record: &InstanceRecord,
    ) -> Result<(), BunError> {
        if record
            .app_spec
            .as_ref()
            .and_then(|spec| spec.egress.as_ref())
            .is_some_and(|policy| !policy.allow.is_empty())
        {
            return Err(BunError::AdoptionState(
                "protected adoption requires Linux eBPF support".into(),
            ));
        }
        Ok(())
    }
}
