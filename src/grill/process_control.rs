//! Durable client of the foreground owner. All filesystem/socket operations run
//! on blocking workers except exec sockets, whose lifetime carries cancellation.
//! Cancellation never cancels an in-flight lifecycle mutation.

use std::fs::{File, OpenOptions};
use std::io;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::{Duration, Instant};

use ring::rand::{SecureRandom, SystemRandom};

use super::InstanceId;
use super::oci::OciSpec;
use super::process_owner::{self, OwnerPhase, OwnerRecord, ProcessLaunch};

#[derive(Debug, Clone)]
pub(crate) struct ProcessControl {
    root: PathBuf,
    executable: PathBuf,
}

impl ProcessControl {
    pub(crate) fn new(root: PathBuf, executable: PathBuf) -> Self {
        Self { root, executable }
    }

    fn directory(&self, id: &InstanceId) -> io::Result<PathBuf> {
        if id.0.is_empty()
            || id.0.len() > 200
            || !id
                .0
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"_-".contains(&byte))
        {
            return Err(io::Error::other("invalid process instance identity"));
        }
        Ok(self.root.join(&id.0))
    }

    fn load(&self, id: &InstanceId) -> io::Result<OwnerRecord> {
        let directory = self.directory(id)?;
        validate_directory(&self.root)?;
        validate_directory(&directory)?;
        let record = process_owner::load(&directory)?;
        let launch = record
            .launch
            .as_ref()
            .ok_or_else(|| io::Error::other("missing process launch intent"))?;
        if launch.instance_id != *id
            || record.command != command(&launch.spec)
            || record.environment != environment(&launch.spec)
        {
            return Err(io::Error::other(
                "process intent conflicts with instance or command",
            ));
        }
        Ok(record)
    }

    async fn run<T: Send + 'static>(
        &self,
        id: &InstanceId,
        operation: impl FnOnce(Self, InstanceId) -> io::Result<T> + Send + 'static,
    ) -> io::Result<T> {
        let this = self.clone();
        let id = id.clone();
        tokio::task::spawn_blocking(move || operation(this, id))
            .await
            .map_err(io::Error::other)?
    }

    pub(crate) async fn inventory(&self) -> io::Result<Vec<super::RuntimeLaunch>> {
        let this = self.clone();
        tokio::task::spawn_blocking(move || {
            match std::fs::symlink_metadata(&this.root) {
                Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
                Err(error) => return Err(error),
                Ok(_) => validate_directory(&this.root)?,
            }
            let mut launches = Vec::new();
            for entry in std::fs::read_dir(&this.root)? {
                let entry = entry?;
                let name = entry
                    .file_name()
                    .into_string()
                    .map_err(|_| io::Error::other("non-UTF-8 process launch identity"))?;
                if name.starts_with(".preparing-") && entry.file_type()?.is_dir() {
                    // Publication precedes owner launch. An abandoned staging
                    // directory has never granted permission to execute.
                    continue;
                }
                let instance_id = InstanceId(name);
                let record = this.load(&instance_id)?;
                let launch = record
                    .launch
                    .ok_or_else(|| io::Error::other("missing process launch intent"))?;
                launches.push(super::RuntimeLaunch {
                    instance_id,
                    spec: launch.spec,
                });
            }
            launches.sort_by(|left, right| left.instance_id.0.cmp(&right.instance_id.0));
            Ok(launches)
        })
        .await
        .map_err(io::Error::other)?
    }

    pub(crate) async fn prepare(&self, id: &InstanceId, spec: &OciSpec) -> io::Result<()> {
        let previous_nonce = self
            .run(id, |this, id| {
                match std::fs::symlink_metadata(this.directory(&id)?) {
                    Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
                    Err(error) => Err(error),
                    Ok(_) => this.load(&id).map(|record| Some(record.nonce)),
                }
            })
            .await?;
        let spec = spec.clone();
        self.run(id, move |this, id| {
            let directory = this.directory(&id)?;
            if let Some(parent) = this.root.parent() {
                create_parent_directories(parent)?;
            }
            create_directory(&this.root)?;
            let mut nonce = [0u8; 16];
            SystemRandom::new()
                .fill(&mut nonce)
                .map_err(|_| io::Error::other("cannot generate process capability"))?;
            let record = OwnerRecord {
                schema: 2,
                nonce: hex::encode(nonce),
                command: command(&spec),
                environment: environment(&spec),
                phase: OwnerPhase::Prepared,
                launch: Some(ProcessLaunch {
                    instance_id: id.clone(),
                    spec,
                }),
            };
            match std::fs::symlink_metadata(&directory) {
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    if previous_nonce.is_some() {
                        return Err(io::Error::other(
                            "process generation disappeared before preparation",
                        ));
                    }
                    // No helper accepts an unpublished temporary directory.
                    // Keep the operation lock through publication and sync so
                    // another adapter cannot start an undurable generation.
                    let temporary = tempfile::Builder::new()
                        .prefix(".preparing-")
                        .permissions(std::fs::Permissions::from_mode(0o700))
                        .tempdir_in(&this.root)?;
                    let _operation = operation_lock(temporary.path())?;
                    process_owner::persist(temporary.path(), &record)?;
                    std::fs::rename(temporary.path(), &directory)?;
                    let _unpublished_path = temporary.keep();
                    File::open(&this.root)?.sync_all()
                }
                Err(error) => Err(error),
                Ok(_) => {
                    validate_directory(&directory)?;
                    let _operation = operation_lock(&directory)?;
                    let _owner = wait_for_owner_lock(&directory)?;
                    let previous = this.load(&id)?;
                    if previous_nonce.as_deref() != Some(previous.nonce.as_str()) {
                        return Err(io::Error::other(
                            "process generation changed before preparation",
                        ));
                    }
                    if !matches!(
                        previous.phase,
                        OwnerPhase::Retired { .. } | OwnerPhase::Cancelled
                    ) {
                        return Err(io::Error::other(
                            "previous process generation has not retired",
                        ));
                    }
                    remove_control_socket(&directory, &previous)?;
                    process_owner::persist(&directory, &record)
                }
            }
        })
        .await
    }

    pub(crate) async fn start(&self, id: &InstanceId) -> io::Result<()> {
        // Cancellation can leave a blocking mutation queued behind another
        // operation. Bind its authority before queueing that mutation, so it
        // cannot activate a successor prepared by the intervening lock holder.
        // Cancellation during this first, read-only operation grants no launch.
        let nonce = self.record(id).await?.nonce;
        self.run(id, move |this, id| {
            let directory = this.directory(&id)?;
            let _operation = operation_lock(&directory)?;
            let record = this.load(&id)?;
            if record.nonce != nonce {
                return Err(io::Error::other(
                    "process launch generation changed before start",
                ));
            }
            // A prior preparer could have died after rename but before its
            // directory sync. Re-establish publication before any execution.
            File::open(&this.root)?.sync_all()?;
            if !matches!(record.phase, OwnerPhase::Prepared) {
                return Err(io::Error::other("process generation is not prepared"));
            }
            let output = OpenOptions::new()
                .create(true)
                .append(true)
                .mode(0o600)
                .custom_flags(nix::libc::O_NOFOLLOW)
                .open(directory.join("owner.log"))?;
            let mut child = tokio::process::Command::new(&this.executable)
                .args(["__process-owner", "--detach", "--directory"])
                .arg(&directory)
                .arg("--generation")
                .arg(&record.nonce)
                .process_group(0)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(output)
                .kill_on_drop(false)
                .spawn()?;
            // A long-lived helper must not depend on a waiter that Bun loses
            // during exec. Reap the short bootstrapper before returning start.
            let mut launch_status = None;
            let deadline = Instant::now() + Duration::from_secs(15);
            loop {
                if launch_status.is_none() {
                    launch_status = child.try_wait()?;
                }
                if launch_status.is_some_and(|status| !status.success()) {
                    return Err(io::Error::other(
                        "process owner bootstrapper failed; launch intent retained",
                    ));
                }
                let current = this.load(&id)?;
                if current.nonce != record.nonce {
                    return Err(io::Error::other("process launch generation changed"));
                }
                match current.phase {
                    OwnerPhase::Running { .. }
                    | OwnerPhase::Retiring { .. }
                    | OwnerPhase::Retired { .. }
                        if launch_status.is_some() =>
                    {
                        return Ok(());
                    }
                    OwnerPhase::Cancelled => {
                        return Err(io::Error::other("process launch was cancelled"));
                    }
                    _ => {}
                }
                if Instant::now() >= deadline {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "process owner did not start; launch intent retained",
                    ));
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        })
        .await
    }

    pub(crate) async fn record(&self, id: &InstanceId) -> io::Result<OwnerRecord> {
        self.run(id, |this, id| this.load(&id)).await
    }

    pub(crate) async fn status(&self, id: &InstanceId) -> io::Result<OwnerRecord> {
        self.run(id, |this, id| {
            let record = this.finish_retirement(&id)?;
            if !matches!(record.phase, OwnerPhase::Running { .. }) { return Ok(record); }
            let result = request(&this.directory(&id)?, &record, "status");
            // The owner may commit completion and remove its socket between
            // reading the record and connecting. Re-read that positive proof.
            let current = this.finish_retirement(&id)?;
            if current.nonce != record.nonce { return Err(io::Error::other("process generation changed during inspection")); }
            if matches!(current.phase, OwnerPhase::Retired { .. }) { return Ok(current); }
            let response = result?;
            let phase: OwnerPhase = serde_json::from_value(response.get("phase").cloned()
                .ok_or_else(|| io::Error::other("owner returned no phase"))?)?;
            if !matches!((phase, &record.phase), (OwnerPhase::Running { pid: live }, OwnerPhase::Running { pid: recorded }) if live == *recorded) {
                return Err(io::Error::other("owner returned conflicting process identity"));
            }
            Ok(current)
        }).await
    }

    pub(crate) async fn signal(&self, id: &InstanceId, force: bool) -> io::Result<()> {
        let nonce = self.record(id).await?.nonce;
        self.run(id, move |this, id| {
            let directory = this.directory(&id)?;
            let _operation = operation_lock(&directory)?;
            if this.load(&id)?.nonce != nonce {
                return Err(io::Error::other(
                    "process generation changed before signalling",
                ));
            }
            let mut record = this.finish_retirement(&id)?;
            if matches!(record.phase, OwnerPhase::Prepared)
                && let Ok(_owner) = process_owner::lock_owner(&directory)
            {
                record = this.load(&id)?;
                if matches!(record.phase, OwnerPhase::Prepared) {
                    // Locking fences a helper still waiting to start. Its
                    // subsequent reload sees Cancelled, never user code.
                    record.phase = OwnerPhase::Cancelled;
                    process_owner::persist(&directory, &record)?;
                    remove_control_socket(&directory, &record)?;
                    return Ok(());
                }
            }
            if matches!(
                record.phase,
                OwnerPhase::Retired { .. } | OwnerPhase::Cancelled
            ) {
                return Ok(());
            }
            let result = request(
                &directory,
                &record,
                if force { "kill" } else { "terminate" },
            );
            let current = this.finish_retirement(&id)?;
            if current.nonce != record.nonce {
                return Err(io::Error::other(
                    "process generation changed during signalling",
                ));
            }
            if matches!(current.phase, OwnerPhase::Retired { .. }) {
                return Ok(());
            }
            if result?["accepted"] != true {
                return Err(io::Error::other("owner did not accept signal"));
            }
            Ok(())
        })
        .await
    }

    fn finish_retirement(&self, id: &InstanceId) -> io::Result<OwnerRecord> {
        let directory = self.directory(id)?;
        let mut record = self.load(id)?;
        let deadline = Instant::now() + Duration::from_secs(2);
        while matches!(record.phase, OwnerPhase::Retiring { .. }) {
            match process_owner::lock_owner(&directory) {
                Ok(_owner) => {
                    record = self.load(id)?;
                    if matches!(record.phase, OwnerPhase::Retiring { .. }) {
                        process_owner::complete_retirement(&directory, &mut record)?;
                    }
                    return Ok(record);
                }
                Err(error)
                    if error.kind() == io::ErrorKind::WouldBlock && Instant::now() < deadline =>
                {
                    std::thread::sleep(Duration::from_millis(10));
                    record = self.load(id)?;
                }
                Err(error) => return Err(error),
            }
        }
        Ok(record)
    }

    /// Run through the durable owner; dropping this future cancels its socket.
    pub(crate) async fn exec(&self, id: &InstanceId, command: &[String]) -> io::Result<String> {
        use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};
        let record = self.record(id).await?;
        if !matches!(record.phase, OwnerPhase::Running { .. }) {
            return Err(io::Error::other("instance is not running"));
        }
        let path = process_owner::socket_path(&self.directory(id)?, &record);
        let request = format!(
            "{}\n",
            serde_json::json!({"nonce": record.nonce, "action": "exec", "command": command})
        );
        if request.len() > process_owner::exec::REQUEST_LIMIT {
            return Err(io::Error::other("exec request exceeds size limit"));
        }
        // Keep this socket in the caller's async future. Cancellation or Bun
        // death closes it, allowing the independent owner to retire execution.
        tokio::time::timeout(Duration::from_secs(300), async {
            let mut socket = tokio::net::UnixStream::connect(path).await?;
            socket.write_all(request.as_bytes()).await?;
            let mut bytes = Vec::new();
            tokio::io::BufReader::new(socket)
                .take(process_owner::exec::RESPONSE_LIMIT + 1)
                .read_until(b'\n', &mut bytes)
                .await?;
            if bytes.len() as u64 > process_owner::exec::RESPONSE_LIMIT
                || bytes.last() != Some(&b'\n')
            {
                return Err(io::Error::other("invalid exec response size"));
            }
            let response: serde_json::Value = serde_json::from_slice(&bytes)?;
            if let Some(error) = response.get("error") {
                return Err(io::Error::other(format!("process exec refused: {error}")));
            }
            response
                .get("output")
                .and_then(|value| value.as_str())
                .map(str::to_owned)
                .ok_or_else(|| io::Error::other("process exec returned no output"))
        })
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "process exec timed out"))?
    }

    pub(crate) fn log_stem(&self, id: &InstanceId) -> io::Result<PathBuf> {
        Ok(self.directory(id)?.join("output"))
    }
}

fn command(spec: &OciSpec) -> Vec<String> {
    if spec.process.args.is_empty() {
        vec!["sleep".into(), "86400".into()]
    } else {
        spec.process.args.clone()
    }
}

fn environment(spec: &OciSpec) -> std::collections::BTreeMap<String, String> {
    spec.process
        .env
        .iter()
        .filter_map(|value| value.split_once('='))
        .map(|(key, value)| (key.to_owned(), value.to_owned()))
        .collect()
}

fn validate_directory(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::MetadataExt;
    let metadata = std::fs::symlink_metadata(path)?;
    if !metadata.is_dir()
        || metadata.uid() != nix::unistd::geteuid().as_raw()
        || metadata.mode() & 0o077 != 0
    {
        return Err(io::Error::other(
            "invalid private process ownership directory",
        ));
    }
    Ok(())
}

fn create_parent_directories(path: &Path) -> io::Result<()> {
    match std::fs::metadata(path) {
        Ok(metadata) if metadata.is_dir() => Ok(()),
        Ok(_) => Err(io::Error::other(
            "process ownership parent is not a directory",
        )),
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            if let Some(parent) = path
                .parent()
                .filter(|parent| !parent.as_os_str().is_empty())
            {
                create_parent_directories(parent)?;
            }
            create_directory(path)
        }
        Err(error) => Err(error),
    }
}

fn create_directory(path: &Path) -> io::Result<()> {
    match std::fs::DirBuilder::new().mode(0o700).create(path) {
        Ok(()) => {
            File::open(path)?.sync_all()?;
            if let Some(parent) = path
                .parent()
                .filter(|parent| !parent.as_os_str().is_empty())
            {
                File::open(parent)?.sync_all()?;
            }
        }
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error),
    }
    validate_directory(path)
}

fn operation_lock(directory: &Path) -> io::Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(nix::libc::O_NOFOLLOW)
        .open(directory.join("client.lock"))?;
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        match file.try_lock() {
            Ok(()) => break,
            Err(std::fs::TryLockError::WouldBlock) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(std::fs::TryLockError::WouldBlock) => {
                return Err(io::Error::new(
                    io::ErrorKind::WouldBlock,
                    "process operation is busy",
                ));
            }
            Err(std::fs::TryLockError::Error(error)) => return Err(error),
        }
    }
    Ok(file)
}

fn wait_for_owner_lock(directory: &Path) -> io::Result<File> {
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        match process_owner::lock_owner(directory) {
            Ok(lock) => return Ok(lock),
            Err(error)
                if error.kind() == io::ErrorKind::WouldBlock && Instant::now() < deadline =>
            {
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(error) => return Err(error),
        }
    }
}

fn remove_control_socket(directory: &Path, record: &OwnerRecord) -> io::Result<()> {
    let socket = process_owner::socket_path(directory, record);
    let parent = socket
        .parent()
        .ok_or_else(|| io::Error::other("invalid socket path"))?;
    match process_owner::validate_socket_directory(parent) {
        Ok(()) => {
            process_owner::remove_socket(&socket)?;
            std::fs::remove_dir(parent)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn request(directory: &Path, record: &OwnerRecord, action: &str) -> io::Result<serde_json::Value> {
    let path = process_owner::socket_path(directory, record);
    process_owner::validate_socket_directory(
        path.parent()
            .ok_or_else(|| io::Error::other("invalid socket path"))?,
    )?;
    let request = format!(
        "{}\n",
        serde_json::json!({"nonce": record.nonce, "action": action})
    );
    // This function runs on a blocking worker, but the socket uses the existing
    // Tokio reactor so a full Unix listen backlog cannot block connect forever.
    let bytes = tokio::runtime::Handle::current().block_on(async move {
        use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt};
        tokio::time::timeout(Duration::from_secs(2), async move {
            let mut socket = tokio::net::UnixStream::connect(path).await?;
            socket.write_all(request.as_bytes()).await?;
            let mut bytes = Vec::new();
            tokio::io::BufReader::new(socket)
                .take(4097)
                .read_until(b'\n', &mut bytes)
                .await?;
            if bytes.len() > 4096 || bytes.last() != Some(&b'\n') {
                return Err(io::Error::other("invalid process owner response size"));
            }
            Ok(bytes)
        })
        .await
        .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "process owner request timed out"))?
    })?;
    let response: serde_json::Value = serde_json::from_slice(&bytes)?;
    if let Some(error) = response.get("error") {
        return Err(io::Error::other(format!("process owner refused: {error}")));
    }
    Ok(response)
}
