//! Durable provisioning and retirement of disposable, lease-owned test storage.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::{VolumeError, VolumeManager};
use crate::grill::btrfs::VolumeBackend;

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct OwnedVolume {
    backend: VolumeBackend,
    ready: bool,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Journal {
    schema: u32,
    namespace: String,
    app: String,
    retiring: bool,
    #[serde(deserialize_with = "unique_volumes")]
    volumes: BTreeMap<PathBuf, OwnedVolume>,
}

fn unique_volumes<'de, D>(deserializer: D) -> Result<BTreeMap<PathBuf, OwnedVolume>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    struct Unique;
    impl<'de> serde::de::Visitor<'de> for Unique {
        type Value = BTreeMap<PathBuf, OwnedVolume>;
        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("unique owned volume paths")
        }
        fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
        where
            A: serde::de::MapAccess<'de>,
        {
            let mut volumes = BTreeMap::new();
            while let Some((path, volume)) = map.next_entry()? {
                if volumes.insert(path, volume).is_some() {
                    return Err(serde::de::Error::custom("duplicate owned volume path"));
                }
                if volumes.len() > 128 {
                    return Err(serde::de::Error::custom("too many owned volumes"));
                }
            }
            Ok(volumes)
        }
    }
    deserializer.deserialize_map(Unique)
}

fn overlapping(left: &Path, right: &Path) -> bool {
    let left_artifacts = [
        left.to_path_buf(),
        left.with_extension("img"),
        VolumeManager::sidecar_path(left),
    ];
    let right_artifacts = [
        right.to_path_buf(),
        right.with_extension("img"),
        VolumeManager::sidecar_path(right),
    ];
    left_artifacts.iter().any(|left| {
        right_artifacts
            .iter()
            .any(|right| left.starts_with(right) || right.starts_with(left))
    })
}

fn refuse(message: impl Into<String>) -> VolumeError {
    VolumeError::Ownership(message.into())
}

fn validate_names(namespace: &str, app: &str) -> Result<(), VolumeError> {
    if !crate::testkit::lease::valid_test_namespace(namespace)
        || !crate::config::valid_workload_label(app)
    {
        return Err(refuse(
            "storage retirement requires an owned test namespace and valid app",
        ));
    }
    Ok(())
}

fn relative_mount(path: &Path) -> Result<&Path, VolumeError> {
    if path == Path::new("/")
        || !path.is_absolute()
        || path
            .components()
            .any(|part| matches!(part, std::path::Component::ParentDir))
    {
        return Err(VolumeError::PathTraversal(path.display().to_string()));
    }
    path.strip_prefix("/")
        .map_err(|error| refuse(error.to_string()))
}

/// Check only descendants of the operator-selected, canonical storage root.
fn checked_path(root: &Path, path: &Path) -> Result<(), VolumeError> {
    let relative = path
        .strip_prefix(root)
        .map_err(|error| refuse(error.to_string()))?;
    let mut current = root.to_path_buf();
    for component in relative.components() {
        if !matches!(component, std::path::Component::Normal(_)) {
            return Err(refuse("invalid owned storage path"));
        }
        current.push(component);
        match std::fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(refuse(format!(
                    "owned storage path is a symlink: {}",
                    current.display()
                )));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

fn journal_path(root: &Path, namespace: &str, app: &str) -> PathBuf {
    root.join(".test-storage")
        .join(format!("{namespace}__{app}.checkpoint"))
}

fn roots(root: &Path, namespace: &str, app: &str) -> [PathBuf; 3] {
    [
        root.join(namespace).join(app),
        root.join(".config").join(namespace).join(app),
        root.join(".snapshots").join(namespace).join(app),
    ]
}

fn load(root: &Path, namespace: &str, app: &str) -> Result<Option<Journal>, VolumeError> {
    let path = journal_path(root, namespace, app);
    checked_path(root, &path)?;
    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK);
    }
    let file = match options.open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    const MAX_BYTES: u64 = 1024 * 1024;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.len() > MAX_BYTES {
        return Err(refuse("invalid test storage checkpoint file"));
    }
    let mut bytes = Vec::new();
    file.take(MAX_BYTES + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > MAX_BYTES {
        return Err(refuse("test storage checkpoint is too large"));
    }
    let journal: Journal =
        serde_json::from_slice(&bytes).map_err(|error| refuse(error.to_string()))?;
    if journal.schema != 1
        || journal.namespace != namespace
        || journal.app != app
        || journal.volumes.len() > 128
    {
        return Err(refuse("invalid test storage checkpoint identity or schema"));
    }
    for (index, path) in journal.volumes.keys().enumerate() {
        relative_mount(path)?;
        if journal
            .volumes
            .keys()
            .skip(index + 1)
            .any(|other| overlapping(path, other))
        {
            return Err(refuse("overlapping test storage checkpoint paths"));
        }
    }
    Ok(Some(journal))
}

fn persist(root: &Path, journal: &Journal) -> Result<(), VolumeError> {
    let path = journal_path(root, &journal.namespace, &journal.app);
    checked_path(root, &path)?;
    let parent = root.join(".test-storage");
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&parent)?;
    }
    #[cfg(not(unix))]
    std::fs::create_dir_all(&parent)?;
    let bytes = serde_json::to_vec(journal).map_err(|error| refuse(error.to_string()))?;
    crate::sesame::identity::atomic_write_mode(&path, &bytes, Some(0o600))?;
    std::fs::File::open(root)?.sync_all()?;
    Ok(())
}

fn exists(path: &Path) -> Result<bool, VolumeError> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

fn claim(root: &Path, namespace: &str, app: &str) -> Result<Journal, VolumeError> {
    validate_names(namespace, app)?;
    if let Some(journal) = load(root, namespace, app)? {
        if journal.retiring {
            return Err(refuse("test storage retirement is pending"));
        }
        return Ok(journal);
    }
    for path in roots(root, namespace, app) {
        checked_path(root, &path)?;
        if exists(&path)? {
            return Err(refuse(format!(
                "refusing pre-existing unowned test storage: {}",
                path.display()
            )));
        }
    }
    let journal = Journal {
        schema: 1,
        namespace: namespace.into(),
        app: app.into(),
        retiring: false,
        volumes: BTreeMap::new(),
    };
    persist(root, &journal)?;
    Ok(journal)
}

/// Called off the runtime, under the agent's per-workload deployment guard.
pub(super) fn prepare(
    manager: &VolumeManager,
    namespace: &str,
    app: &str,
    spec: &crate::config::app::AppSpec,
) -> Result<(), VolumeError> {
    validate_names(namespace, app)?;
    std::fs::create_dir_all(&manager.volumes_dir)?;
    let root = manager.volumes_dir.canonicalize()?;
    let mut journal = claim(&root, namespace, app)?;
    for volume in spec.volumes.iter().filter(|volume| volume.source.is_none()) {
        let relative = relative_mount(&volume.path)?;
        let path = root.join(namespace).join(app).join(relative);
        checked_path(&root, &path)?;
        let size = volume
            .size
            .as_deref()
            .map(|size| {
                crate::config::types::parse_resource_value(size)
                    .map_err(|error| VolumeError::InvalidSize(error.to_string()))
            })
            .transpose()?;
        if let Some(owned) = journal.volumes.get(&volume.path) {
            if !owned.ready {
                return Err(refuse(
                    "interrupted test volume provisioning requires lease retirement",
                ));
            }
            if manager.backend_of(&path) != Some(owned.backend) || !path.is_dir() {
                return Err(refuse(
                    "test volume backend or directory no longer matches its ownership",
                ));
            }
            checked_path(&root, &VolumeManager::sidecar_path(&path))?;
            if owned.backend == VolumeBackend::LoopMount {
                checked_path(&root, &path.with_extension("img"))?;
                ensure_loop_mount(&path)?;
            }
            continue;
        }
        if journal.volumes.len() >= 128 {
            return Err(refuse("test volume ownership limit reached"));
        }
        for previous in journal.volumes.keys() {
            let previous = root
                .join(namespace)
                .join(app)
                .join(relative_mount(previous)?);
            if overlapping(&path, &previous) {
                return Err(refuse(
                    "test volume paths or provisioning artifacts overlap",
                ));
            }
        }
        for artifact in [
            &path,
            &path.with_extension("img"),
            &VolumeManager::sidecar_path(&path),
        ] {
            checked_path(&root, artifact)?;
            if exists(artifact)? {
                return Err(refuse(format!(
                    "refusing pre-existing volume artifact: {}",
                    artifact.display()
                )));
            }
        }
        let backend = crate::grill::btrfs::select_backend(
            crate::grill::btrfs::is_btrfs(&root),
            cfg!(target_os = "linux") && super::is_root(),
            size.is_some(),
        );
        journal.volumes.insert(
            volume.path.clone(),
            OwnedVolume {
                backend,
                ready: false,
            },
        );
        persist(&root, &journal)?;
        #[cfg(test)]
        if let Some(inject) = manager.before_test_provision {
            inject(&journal_path(&root, namespace, app), &path)?;
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        match backend {
            VolumeBackend::Plain => std::fs::create_dir_all(&path)?,
            VolumeBackend::BtrfsSubvolume => {
                run(
                    "btrfs",
                    &["subvolume".as_ref(), "create".as_ref(), path.as_os_str()],
                )?;
                if let Some(size) = size {
                    run(
                        "btrfs",
                        &["quota".as_ref(), "enable".as_ref(), root.as_os_str()],
                    )?;
                    run(
                        "btrfs",
                        &[
                            "qgroup".as_ref(),
                            "limit".as_ref(),
                            size.to_string().as_ref(),
                            path.as_os_str(),
                        ],
                    )?;
                }
            }
            VolumeBackend::LoopMount => {
                let image = path.with_extension("img");
                let size = size.ok_or_else(|| refuse("loop volume has no size"))?;
                std::fs::create_dir_all(&path)?;
                let file = std::fs::OpenOptions::new()
                    .create_new(true)
                    .write(true)
                    .open(&image)?;
                file.set_len(size)?;
                file.sync_all()?;
                run(
                    "mkfs.ext4",
                    &["-F".as_ref(), "-q".as_ref(), image.as_os_str()],
                )?;
                ensure_loop_mount(&path)?;
            }
        }
        manager.write_backend(&path, backend)?;
        let mut directory = Some(path.as_path());
        while let Some(current) = directory {
            std::fs::File::open(current)?.sync_all()?;
            if current == root {
                break;
            }
            directory = current.parent();
        }
        journal
            .volumes
            .get_mut(&volume.path)
            .ok_or_else(|| refuse("missing volume claim"))?
            .ready = true;
        persist(&root, &journal)?;
    }
    // OCI generation writes only within this previously claimed config root.
    let config_root = root.join(".config").join(namespace).join(app);
    checked_path(&root, &config_root)?;
    for config in spec
        .config_file
        .iter()
        .filter(|config| config.content.is_some())
    {
        let filename = config
            .path
            .file_name()
            .ok_or_else(|| refuse("inline configuration has no file name"))?;
        let path = config_root.join(filename);
        checked_path(&root, &path)?;
        if exists(&path)? && !std::fs::symlink_metadata(&path)?.is_file() {
            return Err(refuse("inline configuration is not a regular file"));
        }
    }
    Ok(())
}

/// Commands run only in spawn_blocking; all writes retain their durable intent.
fn run(program: &str, arguments: &[&std::ffi::OsStr]) -> Result<(), VolumeError> {
    use std::io::{Seek, SeekFrom};
    use std::process::{Command, Stdio};
    let mut diagnostics = tempfile::tempfile()?;
    let mut command = Command::new(program);
    command
        .args(arguments)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(diagnostics.try_clone()?);
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::process::CommandExt;
        let parent = std::process::id();
        // SAFETY: only async-signal-safe Linux syscalls run between fork and exec.
        // The synchronous spawning thread remains alive until this child exits;
        // rechecking the parent closes death before PR_SET_PDEATHSIG was armed.
        unsafe {
            command.pre_exec(move || {
                if nix::libc::prctl(nix::libc::PR_SET_PDEATHSIG, nix::libc::SIGKILL) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if nix::libc::getppid() as u32 != parent {
                    return Err(std::io::Error::other("storage command owner exited"));
                }
                Ok(())
            });
        }
    }
    let mut child = command.spawn()?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if std::time::Instant::now() >= deadline {
            child.kill()?;
            child.wait()?;
            return Err(refuse(format!("{program} timed out")));
        }
        // This function runs on a blocking thread, never a Tokio worker.
        std::thread::sleep(std::time::Duration::from_millis(10));
    };
    if !status.success() {
        diagnostics.seek(SeekFrom::Start(0))?;
        let mut bytes = Vec::new();
        diagnostics.take(4096).read_to_end(&mut bytes)?;
        return Err(refuse(format!(
            "{program} failed: {}",
            String::from_utf8_lossy(&bytes)
        )));
    }
    Ok(())
}

#[derive(Debug)]
struct Mount {
    path: PathBuf,
    #[cfg(target_os = "linux")]
    device: String,
    #[cfg(target_os = "linux")]
    filesystem: String,
}

#[cfg(target_os = "linux")]
fn mounts() -> Result<Vec<Mount>, VolumeError> {
    use std::os::unix::ffi::OsStringExt;
    let mut result = Vec::new();
    for line in std::fs::read_to_string("/proc/self/mountinfo")?.lines() {
        let (before, after) = line
            .split_once(" - ")
            .ok_or_else(|| refuse("invalid mount inventory"))?;
        let fields: Vec<_> = before.split_whitespace().collect();
        let encoded = fields
            .get(4)
            .ok_or_else(|| refuse("missing mount path"))?
            .as_bytes();
        let mut decoded = Vec::new();
        let mut index = 0;
        while index < encoded.len() {
            if encoded[index] == b'\\' && index + 3 < encoded.len() {
                let digits = &encoded[index + 1..index + 4];
                if !digits.iter().all(|digit| (b'0'..=b'7').contains(digit)) {
                    return Err(refuse("invalid mount path escape"));
                }
                let value = (digits[0] - b'0') as u16 * 64
                    + (digits[1] - b'0') as u16 * 8
                    + (digits[2] - b'0') as u16;
                decoded.push(u8::try_from(value).map_err(|_| refuse("invalid mount path escape"))?);
                index += 4;
            } else {
                decoded.push(encoded[index]);
                index += 1;
            }
        }
        result.push(Mount {
            path: std::ffi::OsString::from_vec(decoded).into(),
            device: fields
                .get(2)
                .ok_or_else(|| refuse("missing mount device"))?
                .to_string(),
            filesystem: after
                .split_whitespace()
                .next()
                .ok_or_else(|| refuse("missing mount filesystem"))?
                .into(),
        });
    }
    Ok(result)
}

#[cfg(target_os = "macos")]
fn mounts() -> Result<Vec<Mount>, VolumeError> {
    use std::os::unix::ffi::OsStringExt;
    // SAFETY: a null buffer asks only for the number of mounted filesystems.
    let count = unsafe { nix::libc::getfsstat(std::ptr::null_mut(), 0, nix::libc::MNT_NOWAIT) };
    if count < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let mut capacity = count as usize + 16;
    for _ in 0..4 {
        if capacity > 4096 {
            return Err(refuse("mount inventory is too large"));
        }
        let mut buffer: Vec<std::mem::MaybeUninit<nix::libc::statfs>> =
            Vec::with_capacity(capacity);
        buffer.resize_with(capacity, std::mem::MaybeUninit::uninit);
        let bytes = i32::try_from(capacity * std::mem::size_of::<nix::libc::statfs>())
            .map_err(|_| refuse("mount inventory is too large"))?;
        // SAFETY: buffer has capacity fully sized for `bytes`, correct statfs
        // alignment, and remains alive until the syscall finishes writing it.
        let written = unsafe {
            nix::libc::getfsstat(buffer.as_mut_ptr().cast(), bytes, nix::libc::MNT_NOWAIT)
        };
        if written < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        if written as usize >= capacity {
            capacity *= 2;
            continue;
        }
        let mut mounts = Vec::new();
        for entry in &buffer[..written as usize] {
            // SAFETY: getfsstat initialised exactly the returned number of
            // statfs records, and this entry lies within that returned prefix.
            let entry = unsafe { entry.assume_init_ref() };
            let end = entry
                .f_mntonname
                .iter()
                .position(|byte| *byte == 0)
                .ok_or_else(|| refuse("unterminated mount path"))?;
            let bytes = entry.f_mntonname[..end]
                .iter()
                .map(|byte| *byte as u8)
                .collect();
            mounts.push(Mount {
                path: std::ffi::OsString::from_vec(bytes).into(),
            });
        }
        return Ok(mounts);
    }
    Err(refuse("mount inventory changed during inspection"))
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn mounts() -> Result<Vec<Mount>, VolumeError> {
    Err(refuse(
        "test storage retirement is unsupported on this platform",
    ))
}

#[cfg(target_os = "linux")]
fn verify_loop(mount: &Mount, image: &Path) -> Result<(), VolumeError> {
    let source =
        std::fs::read_to_string(format!("/sys/dev/block/{}/loop/backing_file", mount.device))?;
    if mount.filesystem != "ext4"
        || Path::new(source.trim()).canonicalize()? != image.canonicalize()?
    {
        return Err(refuse(
            "mounted filesystem does not match the owned loop image",
        ));
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn verify_loop(_mount: &Mount, _image: &Path) -> Result<(), VolumeError> {
    Err(refuse("loop-backed test storage requires Linux"))
}

fn ensure_loop_mount(path: &Path) -> Result<(), VolumeError> {
    if !cfg!(target_os = "linux") {
        return Err(refuse("loop-backed test storage requires Linux"));
    }
    let image = path.with_extension("img");
    if !std::fs::symlink_metadata(&image)?.is_file() {
        return Err(refuse("loop image is not a regular file"));
    }
    if let Some(mount) = mounts()?.iter().find(|mount| mount.path == path) {
        return verify_loop(mount, &image);
    }
    // util-linux's internal-only option avoids untracked filesystem helpers.
    run(
        "mount",
        &[
            "-i".as_ref(),
            "-o".as_ref(),
            "loop".as_ref(),
            image.as_os_str(),
            path.as_os_str(),
        ],
    )?;
    let inventory = mounts()?;
    let mount = inventory
        .iter()
        .find(|mount| mount.path == path)
        .ok_or_else(|| refuse("loop mount was not observed"))?;
    verify_loop(mount, &image)
}

fn remove_file(path: &Path) -> Result<(), VolumeError> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn remove_directory(path: &Path) -> Result<(), VolumeError> {
    match std::fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

pub(super) fn retire(
    manager: &VolumeManager,
    namespace: &str,
    app: &str,
) -> Result<(), VolumeError> {
    validate_names(namespace, app)?;
    let root = match manager.volumes_dir.canonicalize() {
        Ok(root) => root,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    let owned_roots = roots(&root, namespace, app);
    for path in &owned_roots {
        checked_path(&root, path)?;
    }
    let Some(mut journal) = load(&root, namespace, app)? else {
        if owned_roots
            .iter()
            .map(|path| exists(path))
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .any(|exists| exists)
        {
            return Err(refuse(
                "test storage exists without an ownership checkpoint",
            ));
        }
        let directory = root.join(".test-storage");
        if exists(&directory)? {
            std::fs::File::open(directory)?.sync_all()?;
        }
        return Ok(());
    };
    if exists(&owned_roots[2])? {
        return Err(refuse(
            "test snapshots are not supported; unexpected snapshot storage retained",
        ));
    }
    journal.retiring = true;
    persist(&root, &journal)?;
    let inventory = mounts()?;
    for mount in inventory
        .iter()
        .filter(|mount| owned_roots.iter().any(|root| mount.path.starts_with(root)))
    {
        let volume = journal
            .volumes
            .iter()
            .find(|(path, volume)| {
                volume.backend == VolumeBackend::LoopMount
                    && relative_mount(path)
                        .ok()
                        .is_some_and(|path| owned_roots[0].join(path) == mount.path)
            })
            .ok_or_else(|| {
                refuse(format!(
                    "unowned mount beneath test storage: {}",
                    mount.path.display()
                ))
            })?;
        let path = owned_roots[0].join(relative_mount(volume.0)?);
        verify_loop(mount, &path.with_extension("img"))?;
    }
    for (mount_path, owned) in &journal.volumes {
        let path = owned_roots[0].join(relative_mount(mount_path)?);
        checked_path(&root, &path)?;
        if owned.backend == VolumeBackend::LoopMount {
            if inventory.iter().any(|mount| mount.path == path) {
                run("umount", &[path.as_os_str()])?;
                if mounts()?.iter().any(|mount| mount.path == path) {
                    return Err(refuse("test volume remains mounted"));
                }
            }
            let image = path.with_extension("img");
            checked_path(&root, &image)?;
            remove_file(&image)?;
        } else if owned.backend == VolumeBackend::BtrfsSubvolume && exists(&path)? {
            run(
                "btrfs",
                &["subvolume".as_ref(), "delete".as_ref(), path.as_os_str()],
            )?;
        }
    }
    for path in &owned_roots[..2] {
        remove_directory(path)?;
        if let Some(parent) = path.parent() {
            if exists(parent)? {
                std::fs::File::open(parent)?.sync_all()?;
            }
            match std::fs::remove_dir(parent) {
                Ok(()) => {
                    if let Some(grandparent) = parent.parent() {
                        std::fs::File::open(grandparent)?.sync_all()?;
                    }
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::NotFound | std::io::ErrorKind::DirectoryNotEmpty
                    ) => {}
                Err(error) => return Err(error.into()),
            }
        }
    }
    remove_file(&journal_path(&root, namespace, app))?;
    std::fs::File::open(root.join(".test-storage"))?.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn spec(paths: &[&str]) -> crate::config::app::AppSpec {
        let mut text = String::from("[app.web]\nimage = 'test:v1'\n");
        for path in paths {
            text.push_str(&format!("[[app.web.volumes]]\npath = '{path}'\n"));
        }
        crate::config::Config::parse(&text)
            .unwrap()
            .app
            .remove("web")
            .unwrap()
    }
    fn checkpoint(root: &Path) -> PathBuf {
        root.join(".test-storage/rbtest-storage__web.checkpoint")
    }
    #[test]
    fn failed_provisioning_keeps_durable_intent_before_creating_storage() {
        let root = tempfile::tempdir().unwrap();
        let mut manager = VolumeManager::new(root.path());
        manager.before_test_provision = Some(|journal, path| {
            assert!(!path.exists());
            let state: serde_json::Value =
                serde_json::from_slice(&std::fs::read(journal).unwrap()).unwrap();
            assert_eq!(state["volumes"]["/data"]["ready"], false);
            Err(VolumeError::Ownership(
                "injected provisioning interruption".into(),
            ))
        });
        assert!(
            manager
                .prepare_test_storage("rbtest-storage", "web", &spec(&["/data"]))
                .is_err()
        );
        let recovered = VolumeManager::new(root.path());
        assert!(
            recovered
                .prepare_test_storage("rbtest-storage", "web", &spec(&["/data"]))
                .is_err()
        );
        recovered
            .retire_test_storage("rbtest-storage", "web")
            .unwrap();
        assert!(!checkpoint(root.path()).exists());
    }
    #[cfg(unix)]
    #[test]
    fn symlinked_test_namespace_refuses_without_touching_external_data() {
        let root = tempfile::tempdir().unwrap();
        let external = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(external.path().join("web/data")).unwrap();
        std::fs::write(external.path().join("web/data/keep"), "preserve").unwrap();
        std::os::unix::fs::symlink(external.path(), root.path().join("rbtest-storage")).unwrap();
        let manager = VolumeManager::new(root.path());
        assert!(
            manager
                .prepare_test_storage("rbtest-storage", "web", &spec(&["/data"]))
                .is_err()
        );
        assert!(
            manager
                .retire_test_storage("rbtest-storage", "web")
                .is_err()
        );
        assert_eq!(
            std::fs::read_to_string(external.path().join("web/data/keep")).unwrap(),
            "preserve"
        );
    }

    #[test]
    fn owned_storage_survives_reopen_and_retires_without_touching_host_sources() {
        let root = tempfile::tempdir().unwrap();
        let external = tempfile::tempdir().unwrap();
        let marker = external.path().join("keep");
        std::fs::write(&marker, "user data").unwrap();
        let mut app = spec(&["/data"]);
        let mut bind = app.volumes[0].clone();
        bind.path = "/host".into();
        bind.source = Some(external.path().into());
        app.volumes.push(bind);
        VolumeManager::new(root.path())
            .prepare_test_storage("rbtest-storage", "web", &app)
            .unwrap();
        let data = root.path().join("rbtest-storage/web/data/marker");
        std::fs::write(&data, "persist").unwrap();
        let reopened = VolumeManager::new(root.path());
        reopened
            .prepare_test_storage("rbtest-storage", "web", &app)
            .unwrap();
        assert_eq!(std::fs::read_to_string(&data).unwrap(), "persist");
        assert!(reopened.provisioned_apps().is_empty());
        reopened
            .retire_test_storage("rbtest-storage", "web")
            .unwrap();
        reopened
            .retire_test_storage("rbtest-storage", "web")
            .unwrap();
        assert!(!data.exists());
        assert!(!checkpoint(root.path()).exists());
        assert_eq!(std::fs::read_to_string(marker).unwrap(), "user data");
    }
    #[test]
    fn unowned_or_ordinary_storage_refuses_before_deletion() {
        let root = tempfile::tempdir().unwrap();
        let absent = root.path().join("absent");
        assert!(
            VolumeManager::new(&absent)
                .prepare_test_storage("default", "web", &spec(&["/data"]))
                .is_err()
        );
        assert!(!absent.exists());
        let manager = VolumeManager::new(root.path());
        for namespace in ["default", "rbtest-storage"] {
            let data = root.path().join(namespace).join("web/data");
            std::fs::create_dir_all(&data).unwrap();
            std::fs::write(data.join("keep"), "preserve").unwrap();
            assert!(
                manager
                    .prepare_test_storage(namespace, "web", &spec(&["/data"]))
                    .is_err()
            );
            assert!(manager.retire_test_storage(namespace, "web").is_err());
            assert_eq!(
                std::fs::read_to_string(data.join("keep")).unwrap(),
                "preserve"
            );
        }
        assert!(!checkpoint(root.path()).exists());
    }
    #[test]
    fn interrupted_storage_provisioning_refuses_reuse_but_can_retire() {
        let root = tempfile::tempdir().unwrap();
        let manager = VolumeManager::new(root.path());
        manager
            .prepare_test_storage("rbtest-storage", "web", &spec(&["/data"]))
            .unwrap();
        let path = checkpoint(root.path());
        let mut state: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
        state["volumes"]["/data"]["ready"] = false.into();
        std::fs::write(&path, serde_json::to_vec(&state).unwrap()).unwrap();
        let reopened = VolumeManager::new(root.path());
        assert!(
            reopened
                .prepare_test_storage("rbtest-storage", "web", &spec(&["/data"]))
                .is_err()
        );
        assert!(path.exists());
        reopened
            .retire_test_storage("rbtest-storage", "web")
            .unwrap();
        assert!(!path.exists());
    }
    #[test]
    fn corrupt_or_duplicate_storage_claims_retain_data() {
        for bytes in [
            "not json",
            r#"{"schema":1,"namespace":"rbtest-storage","app":"web","retiring":false,"volumes":{"/data":{"backend":"plain","ready":true},"/data":{"backend":"plain","ready":true}}}"#,
        ] {
            let root = tempfile::tempdir().unwrap();
            let manager = VolumeManager::new(root.path());
            manager
                .prepare_test_storage("rbtest-storage", "web", &spec(&["/data"]))
                .unwrap();
            let path = checkpoint(root.path());
            std::fs::write(&path, bytes).unwrap();
            assert!(
                manager
                    .retire_test_storage("rbtest-storage", "web")
                    .is_err()
            );
            assert!(root.path().join("rbtest-storage/web/data").exists());
            assert!(path.exists());
        }
    }
    #[cfg(unix)]
    #[test]
    fn owned_config_symlink_refuses_before_overwriting_external_data() {
        let root = tempfile::tempdir().unwrap();
        let external = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(external.path(), "preserve").unwrap();
        let manager = VolumeManager::new(root.path());
        let mut app = spec(&[]);
        app.config_file.push(crate::config::ConfigFileSpec {
            path: "/etc/example.conf".into(),
            source: None,
            content: Some("overwrite".into()),
        });
        manager
            .prepare_test_storage("rbtest-storage", "web", &app)
            .unwrap();
        let directory = root.path().join(".config/rbtest-storage/web");
        std::fs::create_dir_all(&directory).unwrap();
        std::os::unix::fs::symlink(external.path(), directory.join("example.conf")).unwrap();
        assert!(
            manager
                .prepare_test_storage("rbtest-storage", "web", &app)
                .is_err()
        );
        assert_eq!(
            std::fs::read_to_string(external.path()).unwrap(),
            "preserve"
        );
        manager
            .retire_test_storage("rbtest-storage", "web")
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(external.path()).unwrap(),
            "preserve"
        );
    }
    #[test]
    fn overlapping_owned_volume_artifacts_refuse() {
        for paths in [
            ["/data", "/data/nested"],
            ["/data", "/data.img"],
            ["/data.ext", "/data.other"],
        ] {
            let root = tempfile::tempdir().unwrap();
            let manager = VolumeManager::new(root.path());
            assert!(
                manager
                    .prepare_test_storage("rbtest-storage", "web", &spec(&paths))
                    .is_err()
            );
            manager
                .retire_test_storage("rbtest-storage", "web")
                .unwrap();
        }
    }
    #[test]
    fn unexpected_snapshots_keep_ownership_until_operator_repairs_storage() {
        let root = tempfile::tempdir().unwrap();
        let manager = VolumeManager::new(root.path());
        manager
            .prepare_test_storage("rbtest-storage", "web", &spec(&["/data"]))
            .unwrap();
        let unexpected = root.path().join(".snapshots/rbtest-storage/web");
        std::fs::create_dir_all(&unexpected).unwrap();
        assert!(
            manager
                .retire_test_storage("rbtest-storage", "web")
                .is_err()
        );
        assert!(checkpoint(root.path()).exists());
        assert!(root.path().join("rbtest-storage/web/data").exists());
        std::fs::remove_dir(&unexpected).unwrap();
        manager
            .retire_test_storage("rbtest-storage", "web")
            .unwrap();
    }
}
