//! Exclusive durable discovery ownership, independent of runtime adoption.
//!
//! This synchronous store belongs on a blocking worker that retains the journal
//! for the whole operation. Its caller establishes kernel/remote withdrawal and
//! runtime release; a saved record alone cannot establish those external facts.

use std::fs::{File, OpenOptions};
use std::io;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::durable::{Access, read_json, validate_file};
use crate::grill::runc_intent::NetworkReference;
use crate::onion::{service_id::ServiceId, service_map::ServiceMap, types::ServiceEntry};

const CHECKPOINT: &str = "discovery.json";
const LIMIT: u64 = 16 * 1024 * 1024;

/// Whether the original service can still have published routes or grants.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ServicePhase {
    /// Publication or its attempted mutation still requires confirmed withdrawal.
    Owned,
    /// The caller confirmed withdrawal; allocation remains reserved until forgotten.
    Withdrawn,
}

/// Exact service allocation and its remaining publication obligation.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServiceOwner {
    /// Original executions for backends without a rootful network address hold.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub executions: std::collections::BTreeMap<String, crate::grill::RuntimeGeneration>,
    /// Original allocated VIP, port, destination identity and attempted backends.
    pub entry: ServiceEntry,
    /// Withdrawal evidence established by the publisher, not inferred by this store.
    pub phase: ServicePhase,
}

/// Whether discovery still prevents release of a runtime address.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReferencePhase {
    /// At least one publication obligation still retains this address.
    Held,
    /// Withdrawal was confirmed; runtime release may be performed or replayed.
    ReleaseAuthorised,
}

/// Original runtime generation associated with its publishing service.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReferenceOwner {
    /// Canonical namespace-qualified service identity.
    pub service: ServiceId,
    /// Original address allocation and runtime generation.
    pub reference: NetworkReference,
    /// Durable permission precedes the runtime's release call.
    pub phase: ReferencePhase,
}

/// Complete local inventory, including attempted publication and pending releases.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiscoveryInventory {
    /// Allocations retained until their entire withdrawal completes.
    pub services: Vec<ServiceOwner>,
    /// Held references and release permissions not yet confirmed by the runtime.
    pub references: Vec<ReferenceOwner>,
    /// Original cluster publication attempts, bound to their enrolled consumer.
    pub consumer: Option<super::consumer_owners::ConsumerOwnership>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Checkpoint {
    schema: u32,
    inventory: DiscoveryInventory,
}

/// One exclusive writer. Drop releases its claim; failed persistence fences writes.
#[derive(Debug)]
pub struct DiscoveryJournal {
    directory: PathBuf,
    inventory: DiscoveryInventory,
    uncertain: bool,
    _claim: File,
    #[cfg(test)]
    write_pause: Option<(
        tokio::sync::oneshot::Sender<()>,
        tokio::sync::oneshot::Receiver<()>,
    )>,
    #[cfg(test)]
    fail_next_write: bool,
}

impl DiscoveryJournal {
    /// Open on a blocking worker; cancellation never leaves I/O on the async runtime.
    pub async fn open_async(directory: &Path) -> io::Result<Self> {
        let directory = directory.to_path_buf();
        tokio::task::spawn_blocking(move || Self::open(&directory))
            .await
            .map_err(io::Error::other)?
    }

    /// Correlate exact runtime address evidence before any recovery mutation.
    /// The caller must exclude concurrent runtime registration and supply its complete inventory.
    pub fn reconcile_runtime_inventory(
        &self,
        launches: &[crate::grill::RuntimeLaunch],
    ) -> io::Result<DiscoveryInventory> {
        if self.inventory.consumer.is_some() {
            return Err(io::Error::other(
                "consumer ownership requires cluster recovery reconciliation",
            ));
        }
        self.reconcile_original_runtime_inventory(launches)
    }

    /// Correlate local runtime ownership after the caller verifies consumer enrolment.
    pub(crate) fn reconcile_consumer_runtime_inventory(
        &self,
        launches: &[crate::grill::RuntimeLaunch],
        identity: &super::consumer_owners::ConsumerIdentity,
    ) -> io::Result<DiscoveryInventory> {
        if self
            .inventory
            .consumer
            .as_ref()
            .is_none_or(|owner| owner.identity != *identity)
        {
            return Err(io::Error::other(
                "original consumer enrolment identity is missing",
            ));
        }
        self.reconcile_original_runtime_inventory(launches)
    }

    fn reconcile_original_runtime_inventory(
        &self,
        launches: &[crate::grill::RuntimeLaunch],
    ) -> io::Result<DiscoveryInventory> {
        use crate::grill::runc_intent::NetworkReferenceState;
        if self.uncertain {
            return Err(io::Error::other("discovery checkpoint is uncertain"));
        }
        let mut by_instance = std::collections::HashMap::new();
        for launch in launches {
            if by_instance.insert(&launch.instance_id, launch).is_some() {
                return Err(io::Error::other("duplicate runtime launch identity"));
            }
            if let Some(
                NetworkReferenceState::Held(reference) | NetworkReferenceState::Released(reference),
            ) = &launch.network_reference
                && (reference.instance_id != launch.instance_id
                    || launch.generation
                        != crate::grill::RuntimeGeneration::runc(reference.generation.as_str()))
            {
                return Err(io::Error::other(
                    "runtime reference belongs to another instance or generation",
                ));
            }
        }
        for owner in &self.inventory.references {
            let launch = by_instance
                .get(&owner.reference.instance_id)
                .ok_or_else(|| io::Error::other("original runtime reference is missing"))?;
            let (reference, released) = match &launch.network_reference {
                Some(NetworkReferenceState::Held(reference)) => (reference, false),
                Some(NetworkReferenceState::Released(reference)) => (reference, true),
                None => return Err(io::Error::other("original runtime reference is missing")),
            };
            if *reference != owner.reference
                || (released && owner.phase != ReferencePhase::ReleaseAuthorised)
            {
                return Err(io::Error::other(
                    "original runtime reference conflicts with discovery ownership",
                ));
            }
            if service_for_launch(&self.inventory, launch)? != owner.service {
                return Err(io::Error::other(
                    "runtime specification conflicts with original service",
                ));
            }
        }
        let mut next = self.inventory.clone();
        for launch in launches {
            let Some(NetworkReferenceState::Held(reference)) = &launch.network_reference else {
                continue;
            };
            if next
                .references
                .iter()
                .any(|owner| owner.reference.instance_id == launch.instance_id)
            {
                continue;
            }
            let service = service_for_launch(&next, launch)?;
            next.references.push(ReferenceOwner {
                service,
                reference: reference.clone(),
                phase: ReferencePhase::Held,
            });
        }
        for service in &next.services {
            for backend in &service.entry.backends {
                let execution_matches = service
                    .executions
                    .get(&backend.instance_id)
                    .zip(by_instance.get(&crate::grill::InstanceId(backend.instance_id.clone())))
                    .is_some_and(|(generation, launch)| {
                        launch.network_reference.is_none()
                            && launch.generation == *generation
                            && launch.spec.port_mapping.is_some_and(|mapping| {
                                mapping.host_port == backend.host_port
                                    && mapping.container_port == service.entry.port
                            })
                            && service_for_launch(&next, launch).is_ok_and(|id| {
                                id.namespace == service.entry.namespace
                                    && id.name == service.entry.app_name
                            })
                    });
                if !execution_matches
                    && !next.references.iter().any(|owner| {
                        owner.reference.instance_id.0 == backend.instance_id
                            && owner.service.namespace == service.entry.namespace
                            && owner.service.name == service.entry.app_name
                            && owner.phase == ReferencePhase::Held
                    })
                {
                    return Err(io::Error::other(
                        "published backend has no original runtime hold",
                    ));
                }
            }
        }
        validate(&next)?;
        validate_transition(&self.inventory, &next)?;
        Ok(next)
    }

    /// Transfer ownership to a write worker and recover it only on acknowledged success.
    /// Cancellation or failure requires reopening and reconciling the complete state.
    pub async fn persist(mut self, next: DiscoveryInventory) -> io::Result<Self> {
        tokio::task::spawn_blocking(move || {
            self.save(next)?;
            Ok(self)
        })
        .await
        .map_err(io::Error::other)?
    }

    /// Open the complete inventory under an exclusive claim. The parent must exist.
    /// Only a newly created directory may initialise ownership.
    /// Missing established state, redirected paths and incompatible schemas refuse.
    pub fn open(directory: &Path) -> io::Result<Self> {
        let fresh = match std::fs::DirBuilder::new().mode(0o700).create(directory) {
            Ok(()) => {
                let parent = directory
                    .parent()
                    .filter(|parent| !parent.as_os_str().is_empty())
                    .unwrap_or(Path::new("."));
                File::open(parent)?.sync_all()?;
                true
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => false,
            Err(error) => return Err(error),
        };
        let metadata = std::fs::symlink_metadata(directory)?;
        if !metadata.is_dir()
            || metadata.mode() & 0o777 != 0o700
            || metadata.uid() != nix::unistd::geteuid().as_raw()
        {
            return Err(io::Error::other("discovery directory is not private"));
        }
        let mut options = OpenOptions::new();
        options
            .read(true)
            .write(true)
            .mode(0o600)
            .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK);
        // Recreating a lost claim would make the next recovery trust it as original.
        let claim = options
            .create_new(fresh)
            .open(directory.join("owner.lock"))?;
        validate_file(&claim, Access::Exclusive)?;
        claim.try_lock().map_err(|error| match error {
            std::fs::TryLockError::WouldBlock => {
                io::Error::new(io::ErrorKind::WouldBlock, "discovery owner is busy")
            }
            std::fs::TryLockError::Error(error) => error,
        })?;
        let inventory = match read_checkpoint(directory) {
            Ok(inventory) if !fresh => inventory,
            Ok(_) => {
                return Err(io::Error::other(
                    "discovery checkpoint lost its original claim",
                ));
            }
            Err(error) if fresh && error.kind() == io::ErrorKind::NotFound => {
                let empty = DiscoveryInventory::default();
                claim.sync_all()?;
                write_checkpoint(directory, &empty)?;
                empty
            }
            Err(error) => return Err(error),
        };
        Ok(Self {
            directory: directory.into(),
            inventory,
            uncertain: false,
            _claim: claim,
            #[cfg(test)]
            write_pause: None,
            #[cfg(test)]
            fail_next_write: false,
        })
    }

    /// Read retained evidence; this does not authorise routing publication.
    pub fn inventory(&self) -> &DiscoveryInventory {
        &self.inventory
    }

    /// Save the entire successor before publication or address release.
    ///
    /// The caller must confirm withdrawal before authorising release and confirm
    /// runtime release before forgetting that permission. Persistence failures
    /// fence this writer until it is dropped and the complete store is reopened.
    /// Directory this journal owns, for reopening after an uncertain write.
    pub(crate) fn directory(&self) -> &Path {
        &self.directory
    }

    /// Refuse a candidate the journal would reject, without touching disk.
    pub(crate) fn check(&self, next: &DiscoveryInventory) -> io::Result<()> {
        validate(next)?;
        validate_transition(&self.inventory, next)
    }

    /// Make the next checkpoint write fail as an I/O error would.
    #[cfg(test)]
    pub(crate) fn fail_next_write(&mut self) {
        self.fail_next_write = true;
    }

    pub fn save(&mut self, next: DiscoveryInventory) -> io::Result<()> {
        if self.uncertain {
            return Err(io::Error::other(
                "discovery persistence is uncertain; reopen to recover",
            ));
        }
        validate(&next)?;
        validate_transition(&self.inventory, &next)?;
        // Keep the original in-memory obligations on any uncertain disk outcome.
        self.uncertain = true;
        #[cfg(test)]
        if let Some((entered, resume)) = self.write_pause.take() {
            let _ = entered.send(());
            resume.blocking_recv().map_err(io::Error::other)?;
        }
        #[cfg(test)]
        if std::mem::take(&mut self.fail_next_write) {
            return Err(io::Error::other("injected checkpoint write failure"));
        }
        write_checkpoint(&self.directory, &next)?;
        self.inventory = next;
        self.uncertain = false;
        Ok(())
    }
}

// Match structured ownership from the original intent, never ambiguous instance text.
fn service_for_launch(
    inventory: &DiscoveryInventory,
    launch: &crate::grill::RuntimeLaunch,
) -> io::Result<ServiceId> {
    let cgroup = launch
        .spec
        .linux
        .host_cgroup_path()
        .ok_or_else(|| io::Error::other("runtime reference has no original cgroup identity"))?;
    let port = launch
        .spec
        .port_mapping
        .ok_or_else(|| io::Error::other("runtime reference has no original service port"))?;
    let mut matches = inventory.services.iter().filter(|owner| {
        owner.phase == ServicePhase::Owned
            && owner.entry.port == port.container_port
            && crate::grill::cgroup::instance_cgroup_path(
                &owner.entry.namespace,
                &owner.entry.app_name,
                &launch.instance_id,
            )
            .is_ok_and(|expected| expected == cgroup)
    });
    let original = matches
        .next()
        .ok_or_else(|| io::Error::other("runtime reference has no matching original service"))?;
    if matches.next().is_some() {
        return Err(io::Error::other(
            "runtime reference has ambiguous service ownership",
        ));
    }
    Ok(ServiceId::new(
        &original.entry.namespace,
        &original.entry.app_name,
    ))
}

fn read_checkpoint(directory: &Path) -> io::Result<DiscoveryInventory> {
    let checkpoint: Checkpoint = read_json(&directory.join(CHECKPOINT), LIMIT, Access::Exclusive)?;
    if checkpoint.schema != 4 {
        return Err(io::Error::other("unsupported discovery checkpoint schema"));
    }
    validate(&checkpoint.inventory)?;
    Ok(checkpoint.inventory)
}

fn write_checkpoint(directory: &Path, inventory: &DiscoveryInventory) -> io::Result<()> {
    let bytes = serde_json::to_vec(&Checkpoint {
        schema: 4,
        inventory: inventory.clone(),
    })?;
    if bytes.len() as u64 > LIMIT {
        return Err(io::Error::other("discovery checkpoint exceeds size limit"));
    }
    crate::sesame::identity::atomic_write_mode(&directory.join(CHECKPOINT), &bytes, Some(0o600))
}

fn validate(inventory: &DiscoveryInventory) -> io::Result<()> {
    if let Some(consumer) = &inventory.consumer {
        consumer.validate()?;
    }
    let entries: Vec<_> = inventory
        .services
        .iter()
        .map(|owner| owner.entry.clone())
        .collect();
    ServiceMap::from_snapshot(&entries).map_err(io::Error::other)?;
    let services: std::collections::HashMap<_, _> = inventory
        .services
        .iter()
        .map(|owner| {
            (
                ServiceId::new(&owner.entry.namespace, &owner.entry.app_name),
                owner,
            )
        })
        .collect();
    if inventory
        .services
        .iter()
        .any(|owner| owner.phase == ServicePhase::Withdrawn && !owner.entry.backends.is_empty())
    {
        return Err(io::Error::other(
            "withdrawn service still contains backends",
        ));
    }
    for owner in &inventory.services {
        if owner.executions.keys().any(|id| {
            !owner
                .entry
                .backends
                .iter()
                .any(|backend| &backend.instance_id == id)
        }) {
            return Err(io::Error::other(
                "execution witness has no published backend",
            ));
        }
    }
    let mut instances = std::collections::HashSet::new();
    let mut allocations = std::collections::HashSet::new();
    for owner in &inventory.references {
        let service = services
            .get(&owner.service)
            .ok_or_else(|| io::Error::other("network reference has no service owner"))?;
        let reference = &owner.reference;
        let generation = reference.generation.as_str();
        let prefix = format!("{}-", owner.service.qualified());
        let suffix = reference.instance_id.0.strip_prefix(&prefix).unwrap_or("");
        let ordinary = suffix
            .parse::<u32>()
            .is_ok_and(|ordinal| ordinal.to_string() == suffix);
        let replacement = suffix
            .strip_prefix('g')
            .and_then(|value| value.split_once('-'))
            .is_some_and(|(generation, ordinal)| {
                generation
                    .parse::<u64>()
                    .is_ok_and(|value| value.to_string() == generation)
                    && ordinal
                        .parse::<u32>()
                        .is_ok_and(|value| value.to_string() == ordinal)
            });
        if (!ordinary && !replacement)
            || generation.len() != 32
            || !generation.bytes().all(|byte| byte.is_ascii_hexdigit())
            || reference.container_index >= 509
            || !instances.insert(&reference.instance_id)
            || !allocations.insert(reference.container_index)
            || (owner.phase == ReferencePhase::ReleaseAuthorised
                && service
                    .entry
                    .backends
                    .iter()
                    .any(|backend| backend.instance_id == reference.instance_id.0))
        {
            return Err(io::Error::other(
                "invalid or conflicting discovery network reference",
            ));
        }
    }
    Ok(())
}

fn validate_transition(previous: &DiscoveryInventory, next: &DiscoveryInventory) -> io::Result<()> {
    super::consumer_owners::validate_transition(
        previous.consumer.as_ref(),
        next.consumer.as_ref(),
    )?;
    let services: std::collections::HashMap<_, _> = next
        .services
        .iter()
        .map(|owner| {
            (
                ServiceId::new(&owner.entry.namespace, &owner.entry.app_name),
                owner,
            )
        })
        .collect();
    for original in &previous.services {
        let id = ServiceId::new(&original.entry.namespace, &original.entry.app_name);
        if let Some(successor) = services.get(&id) {
            for (instance, generation) in &original.executions {
                if successor
                    .entry
                    .backends
                    .iter()
                    .any(|backend| &backend.instance_id == instance)
                    && successor.executions.get(instance) != Some(generation)
                {
                    return Err(io::Error::other(
                        "published execution changed without withdrawal",
                    ));
                }
            }
        }
        match services.get(&id) {
            Some(successor)
                if successor.entry.vip == original.entry.vip
                    && successor.entry.port == original.entry.port
                    && (original.phase == ServicePhase::Owned
                        || successor.phase == ServicePhase::Withdrawn) => {}
            None if original.phase == ServicePhase::Withdrawn => {}
            _ => {
                return Err(io::Error::other(
                    "original service allocation has not retired",
                ));
            }
        }
    }
    let references: std::collections::HashMap<_, _> = next
        .references
        .iter()
        .map(|owner| (&owner.reference.instance_id, owner))
        .collect();
    for original in &previous.references {
        match references.get(&original.reference.instance_id) {
            Some(successor)
                if successor.reference == original.reference
                    && successor.service == original.service
                    && (original.phase == ReferencePhase::Held
                        || successor.phase == ReferencePhase::ReleaseAuthorised) => {}
            None if original.phase == ReferencePhase::ReleaseAuthorised => {}
            _ => {
                return Err(io::Error::other(
                    "original network reference has not retired",
                ));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[tokio::test]
    async fn slow_persistence_does_not_block_the_async_runtime() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("owners");
        let mut journal = DiscoveryJournal::open_async(&path).await.unwrap();
        let (entered, waiting) = tokio::sync::oneshot::channel();
        let (resume, paused) = tokio::sync::oneshot::channel();
        let (heartbeat, observed) = std::sync::mpsc::channel();
        journal.write_pause = Some((entered, paused));
        // A separate thread releases even a broken inline implementation, so
        // the single-thread runtime test fails instead of deadlocking forever.
        let release = std::thread::spawn(move || {
            let responsive = observed
                .recv_timeout(std::time::Duration::from_secs(2))
                .is_ok();
            let _ = resume.send(());
            responsive
        });
        let write = tokio::spawn(journal.persist(inventory()));
        waiting.await.unwrap();
        let _ = heartbeat.send(());
        let journal = write.await.unwrap().unwrap();
        assert_eq!(journal.inventory().references.len(), 1);
        assert!(
            release.join().unwrap(),
            "checkpoint I/O blocked the async runtime"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn cancelled_persistence_retains_its_claim_until_the_worker_finishes() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("owners");
        let mut journal = DiscoveryJournal::open_async(&path).await.unwrap();
        let (entered, waiting) = tokio::sync::oneshot::channel();
        let (resume, paused) = tokio::sync::oneshot::channel();
        journal.write_pause = Some((entered, paused));
        let mut write = tokio::spawn(journal.persist(inventory()));
        waiting.await.unwrap();
        write.abort();
        let cancelled = tokio::time::timeout(std::time::Duration::from_secs(1), &mut write).await;
        let competing = DiscoveryJournal::open_async(&path).await;
        resume.send(()).unwrap();
        if cancelled.is_err() {
            let _ = write.await;
        }
        let recovered = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                match DiscoveryJournal::open_async(&path).await {
                    Ok(journal) => break journal,
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                    }
                    Err(error) => panic!("recovery failed: {error}"),
                }
            }
        })
        .await
        .unwrap();
        assert!(matches!(competing, Err(error) if error.kind() == io::ErrorKind::WouldBlock));
        assert!(
            matches!(cancelled, Ok(Err(error)) if error.is_cancelled()),
            "caller cancellation waited for blocking storage"
        );
        assert_eq!(
            recovered.inventory().references.len(),
            1,
            "cancelled writer lost its durable obligation"
        );
    }

    fn consumer_snapshot(generation: u64, host_port: u16) -> serde_json::Value {
        let catalog = crate::onion::catalog::EndpointCatalog::rebuild([(
            ServiceId::new("default", "remote"),
            8080,
            vec![crate::onion::catalog::CatalogBackend {
                execution: Some(crate::grill::RuntimeExecution {
                    instance_id: crate::grill::InstanceId("default__remote-0".into()),
                    generation: format!("{host_port:064x}").try_into().unwrap(),
                }),
                node_id: "producer".into(),
                node_ip: "192.0.2.10".parse().unwrap(),
                host_port,
                healthy: true,
            }],
        )])
        .unwrap();
        let effective =
            ServiceMap::new().with_cluster_catalog_excluding_node(&catalog, Some("reader"));
        serde_json::json!({"generation": generation, "catalog": catalog,
            "effective_services": effective.resolve_all(), "ingress": []})
    }

    fn consumer_inventory() -> serde_json::Value {
        serde_json::json!({"services": [], "references": [], "consumer": {
            "identity": {"node_id": "reader", "cluster_identity": vec![42_u8; 32]},
            "publications": [consumer_snapshot(1, 30001), consumer_snapshot(2, 30002)],
            "phase": "Withdrawing", "receipts": {}
        }})
    }

    #[test]
    fn consumer_publications_retain_original_generations_and_effective_views_after_reopening() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("owners");
        let expected = consumer_inventory();
        let mut journal = DiscoveryJournal::open(&path).unwrap();
        journal
            .save(serde_json::from_value(expected.clone()).unwrap())
            .unwrap();
        drop(journal);
        let journal = DiscoveryJournal::open(&path).unwrap();
        assert_eq!(serde_json::to_value(journal.inventory()).unwrap(), expected);
        assert!(
            journal.reconcile_runtime_inventory(&[]).is_err(),
            "standalone recovery must not ignore remote consumer ownership"
        );
        assert!(DiscoveryJournal::open(&path).is_err());
        let wire: serde_json::Value =
            serde_json::from_slice(&std::fs::read(path.join(CHECKPOINT)).unwrap()).unwrap();
        assert_eq!(wire["schema"], 4);
    }

    #[test]
    fn consumer_ownership_refuses_lost_rewritten_rebound_and_stale_history_atomically() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("owners");
        let mut journal = DiscoveryJournal::open(&path).unwrap();
        let original = consumer_inventory();
        journal
            .save(serde_json::from_value(original.clone()).unwrap())
            .unwrap();
        let bytes = std::fs::read(path.join(CHECKPOINT)).unwrap();
        let mutations: &[fn(&mut serde_json::Value)] = &[
            |v| v["consumer"] = serde_json::Value::Null,
            |v| v["consumer"]["identity"]["node_id"] = "another-reader".into(),
            |v| v["consumer"]["identity"]["cluster_identity"] = serde_json::json!(vec![43_u8; 32]),
            |v| {
                v["consumer"]["publications"]
                    .as_array_mut()
                    .unwrap()
                    .remove(0);
            },
            |v| {
                v["consumer"]["publications"].as_array_mut().unwrap().pop();
            },
            |v| v["consumer"]["publications"][0] = consumer_snapshot(1, 31001),
            |v| {
                v["consumer"]["publications"]
                    .as_array_mut()
                    .unwrap()
                    .push(consumer_snapshot(1, 30001))
            },
            |v| {
                v["consumer"]["publications"]
                    .as_array_mut()
                    .unwrap()
                    .push(consumer_snapshot(2, 31002))
            },
        ];
        for mutate in mutations {
            let mut candidate = original.clone();
            mutate(&mut candidate);
            assert!(
                journal
                    .save(serde_json::from_value(candidate).unwrap())
                    .is_err()
            );
            assert_eq!(std::fs::read(path.join(CHECKPOINT)).unwrap(), bytes);
            assert_eq!(serde_json::to_value(journal.inventory()).unwrap(), original);
        }
        let mut next = original;
        next["consumer"]["publications"]
            .as_array_mut()
            .unwrap()
            .push(consumer_snapshot(3, 30003));
        journal
            .save(serde_json::from_value(next.clone()).unwrap())
            .unwrap();
        assert_eq!(serde_json::to_value(journal.inventory()).unwrap(), next);
    }

    #[test]
    fn consumer_ownership_rejects_invalid_or_uncorrelated_effective_views_and_capacity_overflow() {
        let mutations: &[fn(&mut serde_json::Value)] = &[
            |v| v["consumer"]["identity"]["node_id"] = "".into(),
            |v| v["consumer"]["publications"][0]["generation"] = 0.into(),
            |v| v["consumer"]["publications"][0]["effective_services"] = serde_json::json!([]),
            |v| v["consumer"]["publications"][0]["effective_services"][0]["port"] = 9090.into(),
            |v| {
                v["consumer"]["publications"][0]["effective_services"][0]["backends"] =
                    serde_json::json!([])
            },
            |v| {
                let mut alias = v["consumer"]["publications"][0]["effective_services"][0].clone();
                alias["app_name"] = "alias".into();
                v["consumer"]["publications"][0]["effective_services"]
                    .as_array_mut()
                    .unwrap()
                    .push(alias);
            },
            |v| {
                v["consumer"]["publications"] =
                    serde_json::json!(vec![consumer_snapshot(1, 30001); 1025])
            },
        ];
        for mutate in mutations {
            let root = tempfile::tempdir().unwrap();
            let path = root.path().join("owners");
            let mut journal = DiscoveryJournal::open(&path).unwrap();
            let bytes = std::fs::read(path.join(CHECKPOINT)).unwrap();
            let mut candidate = consumer_inventory();
            mutate(&mut candidate);
            assert!(
                journal
                    .save(serde_json::from_value(candidate).unwrap())
                    .is_err()
            );
            assert_eq!(std::fs::read(path.join(CHECKPOINT)).unwrap(), bytes);
        }
    }

    #[test]
    fn consumer_retains_local_changes_within_one_generation_and_empty_initial_catalogue() {
        let root = tempfile::tempdir().unwrap();
        let mut journal = DiscoveryJournal::open(&root.path().join("owners")).unwrap();
        let mut next = consumer_inventory();
        next["consumer"]["publications"] = serde_json::json!([{
            "generation": 0, "catalog": {"services": {}}, "effective_services": [], "ingress": []
        }]);
        journal
            .save(serde_json::from_value(next.clone()).unwrap())
            .unwrap();
        let mut publication = consumer_snapshot(1, 30001);
        next["consumer"]["publications"]
            .as_array_mut()
            .unwrap()
            .push(publication.clone());
        journal
            .save(serde_json::from_value(next.clone()).unwrap())
            .unwrap();
        let mut local = ServiceMap::new();
        local
            .register(&ServiceId::new("default", "local"), 9000, None)
            .unwrap();
        publication["effective_services"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::to_value(local.resolve_all()[0]).unwrap());
        next["consumer"]["publications"]
            .as_array_mut()
            .unwrap()
            .push(publication);
        journal
            .save(serde_json::from_value(next.clone()).unwrap())
            .unwrap();
        assert_eq!(serde_json::to_value(journal.inventory()).unwrap(), next);
    }

    #[test]
    fn consumer_bounds_total_exposures_even_below_publication_capacity() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("owners");
        let mut journal = DiscoveryJournal::open(&path).unwrap();
        let before = std::fs::read(path.join(CHECKPOINT)).unwrap();
        let mut publication: super::super::consumer_owners::ConsumerPublication =
            serde_json::from_value(consumer_snapshot(1, 30001)).unwrap();
        let service = publication.catalog.services.values_mut().next().unwrap();
        let backend = service.backends[0].clone();
        service.backends = (0..32)
            .map(|offset| {
                let mut backend = backend.clone();
                backend.host_port += offset;
                backend
            })
            .collect();
        let effective = ServiceMap::new()
            .with_cluster_catalog_excluding_node(&publication.catalog, Some("reader"));
        publication.effective_services = effective.resolve_all().into_iter().cloned().collect();
        let mut next: DiscoveryInventory = serde_json::from_value(consumer_inventory()).unwrap();
        // Each attempt retains two service records and 64 backend records.
        next.consumer.as_mut().unwrap().publications = vec![publication; 993];
        assert!(
            journal
                .save(next)
                .unwrap_err()
                .to_string()
                .contains("exposure capacity")
        );
        assert_eq!(std::fs::read(path.join(CHECKPOINT)).unwrap(), before);
    }

    #[test]
    fn consumer_failed_write_preserves_previous_evidence_and_fences_the_writer() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("owners");
        let mut journal = DiscoveryJournal::open(&path).unwrap();
        let original = consumer_inventory();
        journal
            .save(serde_json::from_value(original.clone()).unwrap())
            .unwrap();
        let mut next = original.clone();
        next["consumer"]["publications"]
            .as_array_mut()
            .unwrap()
            .push(consumer_snapshot(3, 30003));
        std::fs::rename(path.join(CHECKPOINT), path.join("saved.json")).unwrap();
        std::fs::create_dir(path.join(CHECKPOINT)).unwrap();
        assert!(
            journal
                .save(serde_json::from_value(next.clone()).unwrap())
                .is_err()
        );
        assert_eq!(serde_json::to_value(journal.inventory()).unwrap(), original);
        std::fs::remove_dir(path.join(CHECKPOINT)).unwrap();
        std::fs::rename(path.join("saved.json"), path.join(CHECKPOINT)).unwrap();
        assert!(journal.save(serde_json::from_value(next).unwrap()).is_err());
        drop(journal);
        let recovered = DiscoveryJournal::open(&path).unwrap();
        assert_eq!(
            serde_json::to_value(recovered.inventory()).unwrap(),
            original
        );
    }

    #[tokio::test]
    async fn consumer_cancelled_persistence_keeps_the_claim_until_original_evidence_is_written() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("owners");
        let mut journal = DiscoveryJournal::open_async(&path).await.unwrap();
        let original = consumer_inventory();
        let next = serde_json::from_value(original.clone()).unwrap();
        let (entered, waiting) = tokio::sync::oneshot::channel();
        let (resume, paused) = tokio::sync::oneshot::channel();
        journal.write_pause = Some((entered, paused));
        let operation = tokio::spawn(journal.persist(next));
        waiting.await.unwrap();
        operation.abort();
        assert!(operation.await.unwrap_err().is_cancelled());
        assert!(matches!(DiscoveryJournal::open_async(&path).await,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock));
        resume.send(()).unwrap();
        let recovered = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                match DiscoveryJournal::open_async(&path).await {
                    Ok(journal) => break journal,
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        tokio::task::yield_now().await
                    }
                    Err(error) => panic!("recovery failed: {error}"),
                }
            }
        })
        .await
        .unwrap();
        assert_eq!(
            serde_json::to_value(recovered.inventory()).unwrap(),
            original
        );
    }

    fn inventory() -> DiscoveryInventory {
        let mut services = ServiceMap::new();
        let id = ServiceId::new("default", "api-g5");
        services
            .register(&id, 8080, Some(vec!["default/client".into()]))
            .unwrap();
        DiscoveryInventory {
            consumer: None,
            services: vec![ServiceOwner { executions: Default::default(), entry: services.resolve(&id).unwrap().clone(), phase: ServicePhase::Owned }],
            references: vec![ReferenceOwner {
                service: id.clone(),
                reference: serde_json::from_value(serde_json::json!({
                    "instance_id": "default__api-g5-0", "generation": "0123456789abcdef0123456789abcdef", "container_index": 2
                })).unwrap(),
                phase: ReferencePhase::Held,
            }],
        }
    }

    fn runtime_launch(
        reference: crate::grill::runc_intent::NetworkReferenceState,
    ) -> crate::grill::RuntimeLaunch {
        let original = &inventory().references[0];
        let cgroup = crate::grill::cgroup::instance_cgroup_path(
            &original.service.namespace,
            &original.service.name,
            &original.reference.instance_id,
        )
        .unwrap();
        let spec = serde_json::from_value(serde_json::json!({
            "root": {"path": "/fixture", "readonly": true},
            "process": {"args": ["/app"], "env": [], "cwd": "/", "user": {"uid": 0, "gid": 0}},
            "mounts": [], "linux": {"namespaces": [], "cgroupsPath": format!("/{}", cgroup.strip_prefix("/sys/fs/cgroup").unwrap().display())},
            "port_mapping": {"host_port": 20000, "container_port": 8080}
        }))
        .unwrap();
        crate::grill::RuntimeLaunch {
            generation: crate::grill::RuntimeGeneration::runc(
                original.reference.generation.as_str(),
            ),
            instance_id: original.reference.instance_id.clone(),
            spec,
            network_reference: Some(reference),
        }
    }

    #[test]
    fn recovery_correlates_rootless_publications_to_original_execution_generations() {
        let root = tempfile::tempdir().unwrap();
        let mut journal = DiscoveryJournal::open(&root.path().join("owners")).unwrap();
        let mut saved = inventory();
        let reference = saved.references.remove(0).reference;
        let mut launch = runtime_launch(crate::grill::runc_intent::NetworkReferenceState::Held(
            reference,
        ));
        launch.network_reference = None;
        let owner = &mut saved.services[0];
        owner
            .entry
            .backends
            .push(crate::onion::types::BackendInstance {
                instance_id: launch.instance_id.0.clone(),
                node_ip: "127.0.0.1".parse().unwrap(),
                host_port: 20000,
                healthy: true,
                local: false,
            });
        owner
            .executions
            .insert(launch.instance_id.0.clone(), launch.generation.clone());
        journal.save(saved).unwrap();
        journal
            .reconcile_runtime_inventory(std::slice::from_ref(&launch))
            .unwrap();
        let mut changed = launch.clone();
        changed.generation = crate::grill::RuntimeGeneration::runc("replacement");
        assert!(journal.reconcile_runtime_inventory(&[changed]).is_err());
        assert!(journal.reconcile_runtime_inventory(&[]).is_err());
    }

    #[test]
    fn recovery_recovers_a_runtime_hold_saved_before_discovery_acknowledgement() {
        use crate::grill::runc_intent::NetworkReferenceState;
        let root = tempfile::tempdir().unwrap();
        let mut journal = DiscoveryJournal::open(&root.path().join("owners")).unwrap();
        let mut saved = inventory();
        let reference = saved.references.remove(0);
        journal.save(saved).unwrap();
        let recovered = journal
            .reconcile_runtime_inventory(&[runtime_launch(NetworkReferenceState::Held(
                reference.reference.clone(),
            ))])
            .unwrap();
        assert_eq!(
            recovered.references.len(),
            1,
            "original hold was lost between journals"
        );
        assert_eq!(recovered.references[0].reference, reference.reference);
        assert_eq!(recovered.references[0].service, reference.service);
        assert_eq!(recovered.references[0].phase, ReferencePhase::Held);
        assert!(
            journal.inventory().references.is_empty(),
            "correlation mutated durable state before acknowledgement"
        );
    }

    #[test]
    fn recovery_refuses_missing_changed_or_prematurely_released_runtime_ownership() {
        use crate::grill::runc_intent::NetworkReferenceState;
        let root = tempfile::tempdir().unwrap();
        let mut journal = DiscoveryJournal::open(&root.path().join("owners")).unwrap();
        journal.save(inventory()).unwrap();
        let original = inventory().references[0].reference.clone();
        let held = runtime_launch(NetworkReferenceState::Held(original.clone()));
        assert!(
            journal
                .reconcile_runtime_inventory(std::slice::from_ref(&held))
                .is_ok()
        );
        assert!(
            journal.reconcile_runtime_inventory(&[]).is_err(),
            "missing original runtime accepted"
        );
        let mut changed = original.clone();
        changed.container_index += 1;
        assert!(
            journal
                .reconcile_runtime_inventory(&[runtime_launch(NetworkReferenceState::Held(
                    changed
                ))])
                .is_err()
        );
        assert!(
            journal
                .reconcile_runtime_inventory(&[runtime_launch(NetworkReferenceState::Released(
                    original
                ))])
                .is_err(),
            "runtime release had no discovery permission"
        );
        assert!(
            journal
                .reconcile_runtime_inventory(&[held.clone(), held])
                .is_err(),
            "duplicate runtime identities accepted"
        );
    }

    #[test]
    fn recovery_requires_original_service_and_cgroup_for_an_unacknowledged_hold() {
        use crate::grill::runc_intent::NetworkReferenceState;
        let root = tempfile::tempdir().unwrap();
        let mut journal = DiscoveryJournal::open(&root.path().join("owners")).unwrap();
        let mut saved = inventory();
        let reference = saved.references.remove(0).reference;
        let launch = runtime_launch(NetworkReferenceState::Held(reference));
        assert!(
            journal
                .reconcile_runtime_inventory(std::slice::from_ref(&launch))
                .is_err(),
            "missing original service accepted"
        );
        journal.save(saved).unwrap();
        let mut wrong = launch;
        wrong.spec.linux.cgroups_path = Some("/reliaburger/default/api/g5-0".into());
        assert!(
            journal.reconcile_runtime_inventory(&[wrong]).is_err(),
            "ambiguous instance text replaced original service evidence"
        );
    }

    #[test]
    fn recovery_refuses_published_backends_without_their_original_hold() {
        use crate::grill::runc_intent::NetworkReferenceState;
        let root = tempfile::tempdir().unwrap();
        let mut journal = DiscoveryJournal::open(&root.path().join("owners")).unwrap();
        let mut saved = inventory();
        let original = saved.references.remove(0).reference;
        saved.services[0]
            .entry
            .backends
            .push(crate::onion::types::BackendInstance {
                instance_id: original.instance_id.0.clone(),
                node_ip: "10.0.0.4".parse().unwrap(),
                host_port: 8080,
                healthy: true,
                local: false,
            });
        journal.save(saved).unwrap();
        let mut launch = runtime_launch(NetworkReferenceState::Held(original));
        assert!(
            journal
                .reconcile_runtime_inventory(std::slice::from_ref(&launch))
                .is_ok()
        );
        launch.network_reference = None;
        assert!(journal.reconcile_runtime_inventory(&[launch]).is_err());
    }

    #[test]
    fn recovery_refuses_a_fingerprint_from_another_runtime_generation() {
        use crate::grill::runc_intent::NetworkReferenceState;
        let root = tempfile::tempdir().unwrap();
        let mut journal = DiscoveryJournal::open(&root.path().join("owners")).unwrap();
        let saved = inventory();
        journal.save(saved.clone()).unwrap();
        let mut launch = runtime_launch(NetworkReferenceState::Held(
            saved.references[0].reference.clone(),
        ));
        launch.generation = crate::grill::RuntimeGeneration::process("another-private-generation");
        assert!(journal.reconcile_runtime_inventory(&[launch]).is_err());
        assert_eq!(
            serde_json::to_value(journal.inventory()).unwrap(),
            serde_json::to_value(&saved).unwrap()
        );
    }

    #[test]
    fn recovery_preserves_release_permission_until_physical_acknowledgement() {
        use crate::grill::runc_intent::NetworkReferenceState;
        let root = tempfile::tempdir().unwrap();
        let mut journal = DiscoveryJournal::open(&root.path().join("owners")).unwrap();
        let mut saved = inventory();
        saved.references[0].phase = ReferencePhase::ReleaseAuthorised;
        journal.save(saved.clone()).unwrap();
        for state in [
            NetworkReferenceState::Held(saved.references[0].reference.clone()),
            NetworkReferenceState::Released(saved.references[0].reference.clone()),
        ] {
            let recovered = journal
                .reconcile_runtime_inventory(&[runtime_launch(state)])
                .unwrap();
            assert_eq!(
                recovered.references[0].phase,
                ReferencePhase::ReleaseAuthorised
            );
        }
    }

    #[test]
    fn original_allocations_and_release_permissions_survive_reopening() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("owners");
        let mut owner = DiscoveryJournal::open(&path).unwrap();
        let mut original = inventory();
        original.services[0].entry.vip =
            crate::onion::vip::VirtualIP("127.128.12.34".parse().unwrap());
        original.services[0].entry.app_id = u32::from(original.services[0].entry.vip.0);
        owner.save(original.clone()).unwrap();
        assert_eq!(
            std::fs::metadata(path.join(CHECKPOINT))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        drop(owner);
        let mut owner = DiscoveryJournal::open(&path).unwrap();
        assert_eq!(
            serde_json::to_value(owner.inventory()).unwrap(),
            serde_json::to_value(&original).unwrap()
        );
        original.references[0].phase = ReferencePhase::ReleaseAuthorised;
        owner.save(original).unwrap();
        drop(owner);
        let mut owner = DiscoveryJournal::open(&path).unwrap();
        assert_eq!(
            owner.inventory().references[0].phase,
            ReferencePhase::ReleaseAuthorised
        );
        let mut done = owner.inventory().clone();
        done.references.clear();
        done.services[0].phase = ServicePhase::Withdrawn;
        owner.save(done).unwrap();
        owner.save(DiscoveryInventory::default()).unwrap();
    }

    #[test]
    fn concurrent_writers_refuse_until_the_original_claim_is_dropped() {
        let root = tempfile::tempdir().unwrap();
        let owner = DiscoveryJournal::open(&root.path().join("owners")).unwrap();
        assert!(
            matches!(DiscoveryJournal::open(&root.path().join("owners")), Err(error) if error.kind() == io::ErrorKind::WouldBlock)
        );
        drop(owner);
        assert!(DiscoveryJournal::open(&root.path().join("owners")).is_ok());
    }

    #[test]
    fn lost_claim_remains_refused_across_repeated_recovery_attempts() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("owners");
        let mut owner = DiscoveryJournal::open(&path).unwrap();
        owner.save(inventory()).unwrap();
        drop(owner);
        std::fs::remove_file(path.join("owner.lock")).unwrap();
        for _ in 0..3 {
            assert!(DiscoveryJournal::open(&path).is_err());
        }
    }

    #[test]
    fn held_owners_cannot_be_forgotten_replaced_or_reactivated() {
        let root = tempfile::tempdir().unwrap();
        let mut owner = DiscoveryJournal::open(&root.path().join("owners")).unwrap();
        owner.save(inventory()).unwrap();
        assert!(owner.save(DiscoveryInventory::default()).is_err());
        let mut changed = inventory();
        changed.services[0].entry.port += 1;
        assert!(owner.save(changed).is_err());
        let mut changed = inventory();
        changed.references[0].reference.container_index += 1;
        assert!(owner.save(changed).is_err());
        let mut released = inventory();
        released.references[0].phase = ReferencePhase::ReleaseAuthorised;
        owner.save(released).unwrap();
        assert!(owner.save(inventory()).is_err());
    }

    #[test]
    fn conflicting_or_malformed_inventory_is_refused_before_writing() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("owners");
        let mut owner = DiscoveryJournal::open(&path).unwrap();
        owner.save(inventory()).unwrap();
        let original = std::fs::read(path.join(CHECKPOINT)).unwrap();
        let corrupt: &[fn(&mut DiscoveryInventory)] = &[
            |next| next.services.push(next.services[0].clone()),
            |next| next.references.push(next.references[0].clone()),
            |next| next.references[0].service = ServiceId::new("other", "api"),
            |next| next.references[0].reference.instance_id.0 = "default__api-0".into(),
            |next| next.references[0].reference.container_index = 509,
            |next| {
                next.references[0].reference.generation =
                    serde_json::from_str("\"invalid\"").unwrap()
            },
        ];
        for corrupt in corrupt {
            let mut next = inventory();
            corrupt(&mut next);
            assert!(owner.save(next).is_err());
            assert_eq!(std::fs::read(path.join(CHECKPOINT)).unwrap(), original);
        }
    }

    #[test]
    fn missing_corrupt_or_future_checkpoint_never_means_empty_ownership() {
        for bytes in [
            None,
            Some(b"{".as_slice()),
            Some(br#"{"schema":1,"inventory":{"services":[],"references":[]}}"#.as_slice()),
            Some(br#"{"schema":99,"inventory":{"services":[],"references":[]}}"#.as_slice()),
        ] {
            let root = tempfile::tempdir().unwrap();
            let path = root.path().join("owners");
            let mut owner = DiscoveryJournal::open(&path).unwrap();
            owner.save(inventory()).unwrap();
            drop(owner);
            if let Some(bytes) = bytes {
                std::fs::write(path.join(CHECKPOINT), bytes).unwrap();
            } else {
                std::fs::remove_file(path.join(CHECKPOINT)).unwrap();
            }
            assert!(DiscoveryJournal::open(&path).is_err());
        }
    }

    #[test]
    fn failed_publication_fences_the_writer_until_recovery() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("owners");
        let mut owner = DiscoveryJournal::open(&path).unwrap();
        owner.save(inventory()).unwrap();
        std::fs::rename(path.join(CHECKPOINT), path.join("saved.json")).unwrap();
        std::fs::create_dir(path.join(CHECKPOINT)).unwrap();
        assert!(owner.save(inventory()).is_err());
        std::fs::remove_dir(path.join(CHECKPOINT)).unwrap();
        std::fs::rename(path.join("saved.json"), path.join(CHECKPOINT)).unwrap();
        assert!(owner.save(inventory()).is_err());
        drop(owner);
        let mut recovered = DiscoveryJournal::open(&path).unwrap();
        recovered.save(inventory()).unwrap();
    }

    #[test]
    fn redirected_or_nonprivate_storage_is_refused() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("owners");
        let mut owner = DiscoveryJournal::open(&path).unwrap();
        owner.save(inventory()).unwrap();
        drop(owner);
        std::fs::rename(path.join(CHECKPOINT), path.join("saved.json")).unwrap();
        std::os::unix::fs::symlink(path.join("saved.json"), path.join(CHECKPOINT)).unwrap();
        assert!(DiscoveryJournal::open(&path).is_err());
        std::fs::remove_file(path.join(CHECKPOINT)).unwrap();
        std::fs::rename(path.join("saved.json"), path.join(CHECKPOINT)).unwrap();
        std::fs::set_permissions(
            path.join(CHECKPOINT),
            std::fs::Permissions::from_mode(0o644),
        )
        .unwrap();
        assert!(DiscoveryJournal::open(&path).is_err());
    }
}
