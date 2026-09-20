//! Exclusive durable discovery ownership, independent of runtime adoption.
//!
//! This synchronous store belongs on a blocking worker that retains the journal
//! for the whole operation. Its caller establishes kernel/remote withdrawal and
//! runtime release; a saved record alone cannot establish those external facts.

use std::fs::{File, OpenOptions};
use std::io::{self, Read};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

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
}

impl DiscoveryJournal {
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
        validate_file(&claim)?;
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
        write_checkpoint(&self.directory, &next)?;
        self.inventory = next;
        self.uncertain = false;
        Ok(())
    }
}

fn validate_file(file: &File) -> io::Result<()> {
    let metadata = file.metadata()?;
    if !metadata.is_file()
        || metadata.nlink() != 1
        || metadata.mode() & 0o777 != 0o600
        || metadata.uid() != nix::unistd::geteuid().as_raw()
    {
        return Err(io::Error::other(
            "discovery file is not private and regular",
        ));
    }
    Ok(())
}

fn read_checkpoint(directory: &Path) -> io::Result<DiscoveryInventory> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK)
        .open(directory.join(CHECKPOINT))?;
    validate_file(&file)?;
    if file.metadata()?.len() > LIMIT {
        return Err(io::Error::other("discovery checkpoint exceeds size limit"));
    }
    let mut bytes = Vec::new();
    file.take(LIMIT + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > LIMIT {
        return Err(io::Error::other("discovery checkpoint exceeds size limit"));
    }
    let checkpoint: Checkpoint = serde_json::from_slice(&bytes)?;
    if checkpoint.schema != 1 {
        return Err(io::Error::other("unsupported discovery checkpoint schema"));
    }
    validate(&checkpoint.inventory)?;
    Ok(checkpoint.inventory)
}

fn write_checkpoint(directory: &Path, inventory: &DiscoveryInventory) -> io::Result<()> {
    let bytes = serde_json::to_vec(&Checkpoint {
        schema: 1,
        inventory: inventory.clone(),
    })?;
    if bytes.len() as u64 > LIMIT {
        return Err(io::Error::other("discovery checkpoint exceeds size limit"));
    }
    crate::sesame::identity::atomic_write_mode(&directory.join(CHECKPOINT), &bytes, Some(0o600))
}

fn validate(inventory: &DiscoveryInventory) -> io::Result<()> {
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

    fn inventory() -> DiscoveryInventory {
        let mut services = ServiceMap::new();
        let id = ServiceId::new("default", "api-g5");
        services
            .register(&id, 8080, Some(vec!["default/client".into()]))
            .unwrap();
        DiscoveryInventory {
            services: vec![ServiceOwner { entry: services.resolve(&id).unwrap().clone(), phase: ServicePhase::Owned }],
            references: vec![ReferenceOwner {
                service: id.clone(),
                reference: serde_json::from_value(serde_json::json!({
                    "instance_id": "default__api-g5-0", "generation": "0123456789abcdef0123456789abcdef", "container_index": 2
                })).unwrap(),
                phase: ReferencePhase::Held,
            }],
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
            Some(br#"{"schema":2,"inventory":{"services":[],"references":[]}}"#.as_slice()),
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
