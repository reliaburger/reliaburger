//! Persistent kernel policy with exclusive recovery authority.

use std::collections::BTreeSet;
use std::fs::{File, OpenOptions};
use std::io::{self, Read};
use std::os::fd::{AsFd, AsRawFd, OwnedFd};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use aya::programs::CgroupSockAddr;
use serde::{Deserialize, Serialize};

use super::{OnionEbpf, REQUIRED_MAPS, REQUIRED_PROGRAMS, check_prerequisites};

#[path = "ownership/kernel.rs"]
mod kernel;

const ATTACH_TYPES: [u32; 4] = [10, 11, 14, 15];

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Manifest {
    version: u32,
    boot_id: String,
    state_directory: PathBuf,
    pin_directory: PathBuf,
    cgroup_path: PathBuf,
    cgroup_id: u64,
    phase: Phase,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
enum Phase {
    Preparing,
    Active,
    Retiring,
    Retired,
}

struct OwnedLink {
    descriptor: OwnedFd,
    program_id: u32,
    attach_type: u32,
}

pub(super) struct Ownership {
    lock: File,
    manifest: Manifest,
    links: Vec<OwnedLink>,
}

impl Drop for Ownership {
    fn drop(&mut self) {
        // Pins keep kernel enforcement alive. Explicit unlock also prevents a
        // forked pre-exec child from retaining the recovery claim accidentally.
        let _ = self.lock.unlock();
    }
}

fn private_directory(path: &Path) -> io::Result<bool> {
    let fresh = match std::fs::DirBuilder::new().mode(0o700).create(path) {
        Ok(()) => true,
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => false,
        Err(error) => return Err(error),
    };
    let metadata = std::fs::symlink_metadata(path)?;
    if !metadata.is_dir()
        || metadata.uid() != nix::unistd::geteuid().as_raw()
        || metadata.mode() & 0o777 != 0o700
    {
        return Err(io::Error::other(
            "invalid private kernel ownership directory",
        ));
    }
    Ok(fresh)
}

fn claim(
    cgroup_path: &Path,
    state_directory: &Path,
    pin_directory: &Path,
) -> io::Result<Ownership> {
    // Validate volatile prerequisites before establishing durable ownership.
    private_directory(pin_directory)?;
    let pin_directory = std::fs::canonicalize(pin_directory)?;
    let filesystem = nix::sys::statfs::statfs(&pin_directory).map_err(io::Error::other)?;
    if filesystem.filesystem_type().0 != 0xcafe4a11 {
        return Err(io::Error::other(
            "kernel policy pins require a mounted bpffs",
        ));
    }
    let cgroup_path = std::fs::canonicalize(cgroup_path)?;
    let cgroup_id = crate::sesame::egress::cgroup_id_of_path(&cgroup_path)
        .ok_or_else(|| io::Error::other("cannot identify ownership cgroup"))?;
    let boot_id = crate::grill::process_owner::current_boot_id()?
        .ok_or_else(|| io::Error::other("kernel boot identity is unavailable"))?;
    let fresh = private_directory(state_directory)?;
    let state_directory = std::fs::canonicalize(state_directory)?;
    let lock = OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(fresh)
        .truncate(false)
        .mode(0o600)
        .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK)
        .open(state_directory.join("owner.lock"))?;
    let metadata = lock.metadata()?;
    if !metadata.is_file()
        || metadata.uid() != nix::unistd::geteuid().as_raw()
        || metadata.mode() & 0o777 != 0o600
        || metadata.nlink() != 1
    {
        return Err(io::Error::other("invalid kernel ownership lock"));
    }
    lock.try_lock().map_err(io::Error::other)?;
    if fresh {
        lock.sync_all()?;
        File::open(&state_directory)?.sync_all()?;
        if let Some(parent) = state_directory.parent() {
            File::open(parent)?.sync_all()?;
        }
    }
    let mut manifest = Manifest {
        version: 3,
        boot_id,
        cgroup_id,
        cgroup_path,
        state_directory,
        pin_directory,
        phase: Phase::Preparing,
    };
    let path = manifest.state_directory.join("owner.json");
    match OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK)
        .open(&path)
    {
        Ok(file) => {
            let metadata = file.metadata()?;
            if !metadata.is_file()
                || metadata.len() > 16384
                || metadata.uid() != nix::unistd::geteuid().as_raw()
                || metadata.mode() & 0o777 != 0o600
            {
                return Err(io::Error::other("invalid kernel ownership manifest"));
            }
            let mut bytes = Vec::new();
            file.take(16385).read_to_end(&mut bytes)?;
            if bytes.len() > 16384 {
                return Err(io::Error::other("oversized kernel ownership manifest"));
            }
            let original: Manifest = serde_json::from_slice(&bytes)?;
            manifest.phase = original.phase;
            if original.version != manifest.version
                || !crate::grill::process_owner::valid_boot_id(&original.boot_id)
                || original.state_directory != manifest.state_directory
                || original.pin_directory != manifest.pin_directory
                || original.cgroup_path != manifest.cgroup_path
            {
                return Err(io::Error::other("kernel ownership configuration changed"));
            }
            if original.boot_id == manifest.boot_id {
                if original.cgroup_id != manifest.cgroup_id {
                    return Err(io::Error::other("kernel ownership cgroup changed"));
                }
            } else {
                // A changed, positively read boot UUID proves the old kernel is
                // gone. It does not authorise touching objects in the new kernel.
                if std::fs::read_dir(&manifest.pin_directory)?
                    .next()
                    .transpose()?
                    .is_some()
                {
                    return Err(io::Error::other(
                        "prior-boot kernel ownership has live pins",
                    ));
                }
                if matches!(original.phase, Phase::Preparing | Phase::Active) {
                    manifest.phase = Phase::Preparing;
                }
                // Commit the new boot before creating anything. Interrupted
                // recreation resumes as Preparing under the same exclusive lock.
                crate::sesame::identity::atomic_write_mode(
                    &path,
                    &serde_json::to_vec(&manifest)?,
                    Some(0o600),
                )?;
            }
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            if !fresh {
                return Err(io::Error::other(
                    "original kernel ownership manifest is missing",
                ));
            }
            if std::fs::read_dir(&manifest.pin_directory)?
                .next()
                .transpose()?
                .is_some()
            {
                return Err(io::Error::other(
                    "kernel pins have no original ownership manifest",
                ));
            }
            crate::sesame::identity::atomic_write_mode(
                &path,
                &serde_json::to_vec(&manifest)?,
                Some(0o600),
            )?;
        }
        Err(error) => return Err(error),
    }
    let allowed: BTreeSet<String> = REQUIRED_MAPS
        .iter()
        .map(|name| (*name).to_owned())
        .chain(REQUIRED_PROGRAMS.iter().map(|name| format!("{name}_link")))
        .collect();
    let mut present = BTreeSet::new();
    for entry in std::fs::read_dir(&manifest.pin_directory)? {
        let entry = entry?;
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| io::Error::other("non-UTF-8 kernel pin name"))?;
        if !allowed.contains(&name) || !entry.file_type()?.is_file() {
            return Err(io::Error::other("unexpected kernel ownership entry"));
        }
        present.insert(name);
    }
    let has_link = present.iter().any(|name| name.ends_with("_link"));
    if (manifest.phase == Phase::Active && present != allowed)
        || (has_link && REQUIRED_MAPS.iter().any(|name| !present.contains(*name)))
    {
        return Err(io::Error::other(
            "active kernel ownership inventory is incomplete",
        ));
    }
    Ok(Ownership {
        lock,
        manifest,
        links: Vec::new(),
    })
}

impl Ownership {
    pub(super) fn attached(&self, index: usize) -> bool {
        let Some(link) = self.links.get(index) else {
            return false;
        };
        let path = self
            .manifest
            .pin_directory
            .join(format!("{}_link", REQUIRED_PROGRAMS[index]));
        let Ok(pinned) = kernel::open_link(&path) else {
            return false;
        };
        let Ok(pinned) = kernel::link_info(&pinned) else {
            return false;
        };
        kernel::link_info(&link.descriptor).is_ok_and(|info| {
            info.id == pinned.id
                && info.cgroup_id == self.manifest.cgroup_id
                && info.attach_type == link.attach_type
                && info.program_id == link.program_id
        })
    }

    fn save(&self) -> io::Result<()> {
        crate::sesame::identity::atomic_write_mode(
            &self.manifest.state_directory.join("owner.json"),
            &serde_json::to_vec(&self.manifest)?,
            Some(0o600),
        )
    }

    pub(super) fn detach(&mut self) -> io::Result<()> {
        self.manifest.phase = Phase::Retiring;
        self.save()?;
        // No more in-process references may outlive pin removal. The durable
        // retiring phase permits resuming a prefix of confirmed unlinks.
        self.links.clear();
        for (index, name) in REQUIRED_PROGRAMS.iter().enumerate() {
            let path = self.manifest.pin_directory.join(format!("{name}_link"));
            let descriptor = match kernel::open_link(&path) {
                Ok(descriptor) => descriptor,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error),
            };
            let info = kernel::link_info(&descriptor)?;
            if info.cgroup_id != self.manifest.cgroup_id || info.attach_type != ATTACH_TYPES[index]
            {
                return Err(io::Error::other("kernel link changed before detachment"));
            }
            std::fs::remove_file(path)?;
            drop(descriptor);
        }
        for name in REQUIRED_MAPS {
            match std::fs::remove_file(self.manifest.pin_directory.join(name)) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
        }
        self.manifest.phase = Phase::Retired;
        self.save()
    }
}

pub(super) fn retire(
    cgroup_path: &Path,
    state_directory: &Path,
    pin_directory: &Path,
) -> io::Result<()> {
    let mut ownership = claim(cgroup_path, state_directory, pin_directory)?;
    ownership.detach()
}

pub(super) fn load(
    program_directory: Option<&Path>,
    cgroup_path: &Path,
    state_directory: &Path,
    pin_directory: &Path,
) -> io::Result<OnionEbpf> {
    check_prerequisites().map_err(io::Error::other)?;
    let mut ownership = claim(cgroup_path, state_directory, pin_directory)?;
    if matches!(ownership.manifest.phase, Phase::Retiring | Phase::Retired) {
        return Err(io::Error::other("kernel ownership is retiring or retired"));
    }
    let mut loader = aya::EbpfLoader::new();
    loader.map_pin_path(&ownership.manifest.pin_directory);
    let mut bpf = match program_directory {
        Some(directory) => loader.load_file(directory.join("onion_connect_owned.bpf.o")),
        None => loader.load(aya::include_bytes_aligned!(concat!(
            env!("OUT_DIR"),
            "/onion_connect_owned.bpf.o"
        ))),
    }
    .map_err(io::Error::other)?;
    let names: BTreeSet<_> = bpf.maps().map(|(name, _)| name.to_owned()).collect();
    if names
        != REQUIRED_MAPS
            .iter()
            .map(|name| (*name).to_owned())
            .collect()
    {
        return Err(io::Error::other(
            "owned kernel object has an unexpected map inventory",
        ));
    }
    let mut map_ids = BTreeSet::new();
    for name in REQUIRED_MAPS {
        let map = match bpf.map(name) {
            Some(aya::maps::Map::HashMap(map) | aya::maps::Map::LpmTrie(map)) => map,
            _ => return Err(io::Error::other("owned kernel map has an unexpected type")),
        };
        let info = map.info().map_err(io::Error::other)?;
        let pinned = aya::maps::MapInfo::from_pin(ownership.manifest.pin_directory.join(name))
            .map_err(io::Error::other)?;
        if info.id() != pinned.id() {
            return Err(io::Error::other("kernel map does not match its pin"));
        }
        let (kind, key, value, capacity) = match name {
            "backend_map" => (1, 8, 272, 65534),
            "firewall_map" => (1, 16, 4, 262144),
            "cgroup_namespace_map" | "egress_enabled_map" => (1, 8, 4, 65536),
            "egress_map" => (1, 16, 4, 65536),
            "egress6_map" => (1, 32, 4, 65536),
            "egress_cidr4_map" => (11, 16, 20, 65536),
            "egress_cidr6_map" => (11, 28, 20, 65536),
            "fault_connect_map" => (1, 16, 32, 4096),
            _ => return Err(io::Error::other("unknown persistent map ABI")),
        };
        if info.map_type().map_err(io::Error::other)? as u32 != kind
            || info.key_size() != key
            || info.value_size() != value
            || info.max_entries() != capacity
            || info.map_flags() != 1
        {
            return Err(io::Error::other(format!(
                "kernel map {name} has an incompatible ABI"
            )));
        }
        map_ids.insert(info.id());
    }
    let cgroup = File::open(&ownership.manifest.cgroup_path)?;
    for (index, name) in REQUIRED_PROGRAMS.iter().enumerate() {
        let program: &mut CgroupSockAddr = bpf
            .program_mut(name)
            .ok_or_else(|| io::Error::other("owned kernel program is absent"))?
            .try_into()
            .map_err(io::Error::other)?;
        program.load().map_err(io::Error::other)?;
        let program_info = program.info().map_err(io::Error::other)?;
        let mut new_maps = program_info
            .map_ids()
            .map_err(io::Error::other)?
            .ok_or_else(|| io::Error::other("program map inventory is unavailable"))?;
        new_maps.sort_unstable();
        if new_maps.is_empty() || new_maps.iter().any(|id| !map_ids.contains(id)) {
            return Err(io::Error::other("program references unowned kernel maps"));
        }
        let pin = ownership
            .manifest
            .pin_directory
            .join(format!("{name}_link"));
        if pin.exists() {
            let descriptor = kernel::open_link(&pin)?;
            let link = kernel::link_info(&descriptor)?;
            if link.cgroup_id != ownership.manifest.cgroup_id
                || link.attach_type != ATTACH_TYPES[index]
            {
                return Err(io::Error::other(
                    "kernel link targets a different cgroup or hook",
                ));
            }
            let deadline = Instant::now() + Duration::from_secs(5);
            let mut previous = None;
            for info in aya::programs::loaded_programs() {
                if Instant::now() >= deadline {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "kernel program inventory timed out",
                    ));
                }
                if let Ok(info) = info
                    && info.id() == link.program_id
                {
                    previous = Some(info);
                    break;
                }
            }
            let previous =
                previous.ok_or_else(|| io::Error::other("pinned link program is unavailable"))?;
            let mut previous_maps = previous
                .map_ids()
                .map_err(io::Error::other)?
                .ok_or_else(|| io::Error::other("previous program map inventory is unavailable"))?;
            previous_maps.sort_unstable();
            if previous_maps != new_maps {
                return Err(io::Error::other(
                    "pinned program does not own the expected maps",
                ));
            }
            let old_fd = previous.fd().map_err(io::Error::other)?;
            kernel::replace_link(
                &descriptor,
                program.fd().map_err(io::Error::other)?.as_fd().as_raw_fd(),
                old_fd.as_fd().as_raw_fd(),
            )?;
        } else {
            let descriptor = kernel::create_link(
                program.fd().map_err(io::Error::other)?.as_fd().as_raw_fd(),
                cgroup.as_raw_fd(),
                ATTACH_TYPES[index],
            )?;
            kernel::pin_link(&descriptor, &pin)?;
        }
        let descriptor = kernel::open_link(&pin)?;
        ownership.links.push(OwnedLink {
            descriptor,
            program_id: program_info.id(),
            attach_type: ATTACH_TYPES[index],
        });
        if !ownership.attached(index) {
            return Err(io::Error::other("kernel did not confirm the owned link"));
        }
    }
    ownership.manifest.phase = Phase::Active;
    crate::sesame::identity::atomic_write_mode(
        &ownership.manifest.state_directory.join("owner.json"),
        &serde_json::to_vec(&ownership.manifest)?,
        Some(0o600),
    )?;
    Ok(OnionEbpf {
        _cgroup_path: cgroup_path.to_owned(),
        _connect_link_id: None,
        _connect6_link_id: None,
        _sendmsg4_link_id: None,
        _sendmsg6_link_id: None,
        bpf,
        attached: true,
        connect6_attached: true,
        sendmsg4_attached: true,
        sendmsg6_attached: true,
        ownership: Some(ownership),
    })
}
