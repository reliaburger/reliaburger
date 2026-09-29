//! Durable client of the foreground owner. All filesystem/socket operations run
//! on blocking workers except exec sockets, whose lifetime carries cancellation.
//! Cancellation never cancels an in-flight lifecycle mutation.

use std::fs::{File, OpenOptions};
use std::io;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use ring::rand::{SecureRandom, SystemRandom};

use super::InstanceId;
use super::oci::OciSpec;
use super::process_owner::{self, OwnerPhase, OwnerRecord, ProcessLaunch};
use crate::durable::validate_directory;

mod prune;

#[derive(Debug, Clone)]
pub(crate) struct ProcessControl {
    root: PathBuf,
    executable: PathBuf,
    inventory_reader: super::inventory::InventoryReader,
    /// Blocking owner operations still running, shared by every clone.
    in_flight: Arc<AtomicUsize>,
}

/// Counts one blocking operation for as long as its closure exists, so a
/// caller that drops the future does not end the count early.
struct InFlight(Arc<AtomicUsize>);

impl InFlight {
    fn enter(counter: &Arc<AtomicUsize>) -> Self {
        counter.fetch_add(1, Ordering::SeqCst);
        Self(Arc::clone(counter))
    }
}

impl Drop for InFlight {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

impl ProcessControl {
    pub(crate) fn new(root: PathBuf, executable: PathBuf) -> Self {
        Self {
            root,
            executable,
            inventory_reader: Default::default(),
            in_flight: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// Blocking owner operations that have not finished, including ones
    /// whose caller was cancelled. See `ProcessGrill::owner_operations_in_flight`.
    pub(crate) fn operations_in_flight(&self) -> usize {
        self.in_flight.load(Ordering::SeqCst)
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
        let launch = &record.launch;
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
        let in_flight = InFlight::enter(&self.in_flight);
        tokio::task::spawn_blocking(move || {
            let _in_flight = in_flight;
            operation(this, id)
        })
        .await
        .map_err(io::Error::other)?
    }

    pub(crate) async fn inventory(&self) -> io::Result<Vec<super::RuntimeLaunch>> {
        let this = self.clone();
        self.inventory_reader
            .read(async move {
                tokio::task::spawn_blocking(move || {
                    match std::fs::symlink_metadata(&this.root) {
                        Err(error) if error.kind() == io::ErrorKind::NotFound => {
                            return Ok(Vec::new());
                        }
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
                        let launch = record.launch;
                        launches.push(super::RuntimeLaunch {
                            generation: super::RuntimeGeneration::process(&record.nonce),
                            instance_id,
                            spec: launch.spec,
                            network_reference: None,
                        });
                    }
                    launches.sort_by(|left, right| left.instance_id.0.cmp(&right.instance_id.0));
                    Ok(launches)
                })
                .await
                .map_err(io::Error::other)?
            })
            .await
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
                schema: 3,
                boot_id: process_owner::current_boot_id()?
                    .ok_or_else(|| io::Error::other("kernel boot identity unavailable"))?,
                nonce: hex::encode(nonce),
                command: command(&spec),
                environment: environment(&spec),
                phase: OwnerPhase::Prepared,
                launch: ProcessLaunch {
                    instance_id: id.clone(),
                    spec,
                },
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
                    remove_control_socket(&previous)?;
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
            if process_owner::from_previous_boot(&record)? {
                return Err(io::Error::other(
                    "process generation belongs to a previous kernel boot",
                ));
            }
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
            let mut attempt = 1;
            loop {
                let record = this.finish_retirement(&id)?;
                if !matches!(record.phase, OwnerPhase::Running { .. }) {
                    return Ok(record);
                }
                let result = request(&record, "status");
                // The owner may commit completion and remove its socket between
                // reading the record and connecting. Re-read that positive proof.
                let current = this.finish_retirement(&id)?;
                if current.nonce != record.nonce {
                    return Err(io::Error::other(
                        "process generation changed during inspection",
                    ));
                }
                if matches!(current.phase, OwnerPhase::Retired { .. }) {
                    return Ok(current);
                }
                // finish_retirement just failed to take the owner lock, so the
                // owner is alive. A dropped connection only means it closed
                // this client, never that the workload is gone: ask again.
                let response = match result {
                    Err(error) if dropped_connection(&error) && attempt < OWNER_REQUEST_ATTEMPTS => {
                        attempt += 1;
                        std::thread::sleep(Duration::from_millis(20));
                        continue;
                    }
                    result => result?,
                };
                let phase: OwnerPhase = serde_json::from_value(
                    response
                        .get("phase")
                        .cloned()
                        .ok_or_else(|| io::Error::other("owner returned no phase"))?,
                )?;
                if !matches!((phase, &record.phase), (OwnerPhase::Running { pid: live }, OwnerPhase::Running { pid: recorded }) if live == *recorded)
                {
                    return Err(io::Error::other(
                        "owner returned conflicting process identity",
                    ));
                }
                return Ok(current);
            }
        })
        .await
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
                    remove_control_socket(&record)?;
                    return Ok(());
                }
            }
            if matches!(
                record.phase,
                OwnerPhase::Retired { .. } | OwnerPhase::Cancelled
            ) {
                return Ok(());
            }
            let mut attempt = 1;
            loop {
                let result = request(&record, if force { "kill" } else { "terminate" });
                let current = this.finish_retirement(&id)?;
                if current.nonce != record.nonce {
                    return Err(io::Error::other(
                        "process generation changed during signalling",
                    ));
                }
                if matches!(current.phase, OwnerPhase::Retired { .. }) {
                    return Ok(());
                }
                // As in status: a live owner that dropped this client has not
                // acted on the request, so asking again is safe.
                let response = match result {
                    Err(error)
                        if dropped_connection(&error) && attempt < OWNER_REQUEST_ATTEMPTS =>
                    {
                        attempt += 1;
                        std::thread::sleep(Duration::from_millis(20));
                        continue;
                    }
                    result => result?,
                };
                if response["accepted"] != true {
                    return Err(io::Error::other("owner did not accept signal"));
                }
                return Ok(());
            }
        })
        .await
    }

    fn finish_retirement(&self, id: &InstanceId) -> io::Result<OwnerRecord> {
        let directory = self.directory(id)?;
        let mut record = self.load(id)?;
        if process_owner::from_previous_boot(&record)? {
            // A different kernel cannot retain any old owner or child. Holding
            // the owner lock also fences delayed helpers before updating proof.
            let _owner = process_owner::lock_owner(&directory)?;
            record = self.load(id)?;
            if process_owner::from_previous_boot(&record)? {
                match record.phase {
                    OwnerPhase::Prepared => {
                        record.phase = OwnerPhase::Cancelled;
                        process_owner::persist(&directory, &record)?;
                    }
                    OwnerPhase::Running { .. } => {
                        record.phase = OwnerPhase::Retiring { exit_code: None };
                        process_owner::persist(&directory, &record)?;
                        process_owner::complete_retirement(&directory, &mut record)?;
                    }
                    OwnerPhase::Retiring { .. } => {
                        process_owner::complete_retirement(&directory, &mut record)?;
                    }
                    OwnerPhase::Cancelled | OwnerPhase::Retired { .. } => {}
                }
            }
            return Ok(record);
        }
        if matches!(record.phase, OwnerPhase::Running { .. })
            && let Ok(_owner) = process_owner::lock_owner(&directory)
        {
            // A live owner holds this lock for its whole life, so getting it
            // proves the owner died: a crash, an OOM kill, or systemd stopping
            // Bun's unit. Only its workload's absence permits retirement.
            record = self.load(id)?;
            if let OwnerPhase::Running { pid } = record.phase {
                if !process_owner::process_group_absent(pid)? {
                    return Err(io::Error::other(
                        "process owner died while its workload still runs",
                    ));
                }
                // Nobody observed the exit, so its code stays unknown.
                record.phase = OwnerPhase::Retiring { exit_code: None };
                process_owner::persist(&directory, &record)?;
                process_owner::complete_retirement(&directory, &mut record)?;
            }
            return Ok(record);
        }
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
        use tokio::io::{AsyncBufReadExt, AsyncReadExt};
        let record = self.record(id).await?;
        if !matches!(record.phase, OwnerPhase::Running { .. }) {
            return Err(io::Error::other("instance is not running"));
        }
        let path = process_owner::socket_path(&record);
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
            let socket = deliver_exec_request(&path, &request).await?;
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

fn remove_control_socket(record: &OwnerRecord) -> io::Result<()> {
    let socket = process_owner::socket_path(record);
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

/// Requests made to a live owner before a dropped connection is an error.
const OWNER_REQUEST_ATTEMPTS: u32 = 10;

/// Whether the owner closed or refused this connection before answering.
///
/// A live single-threaded owner drops a client that is slower than its read
/// timeout, and an exiting owner drops its queued connections. Neither says
/// anything about the workload.
fn dropped_connection(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::BrokenPipe
            | io::ErrorKind::ConnectionReset
            | io::ErrorKind::ConnectionRefused
            | io::ErrorKind::NotConnected
            | io::ErrorKind::UnexpectedEof
    )
}

/// Connect to a live owner and hand it an exec request, asking again when the
/// owner drops the connection before the request reaches it.
///
/// The owner serves one client at a time and hangs up on one whose request
/// hasn't arrived within its read timeout. A Bun starved between `connect`
/// and `write` on a loaded host got "Broken pipe" and failed the exec. The
/// owner can't have started a command it never read, so sending again is
/// safe. A failure after the request is sent is never retried: by then the
/// command may be running.
async fn deliver_exec_request(path: &Path, request: &str) -> io::Result<tokio::net::UnixStream> {
    use tokio::io::AsyncWriteExt;
    let mut attempt = 1;
    loop {
        let delivered = async {
            let mut socket = tokio::net::UnixStream::connect(path).await?;
            socket.write_all(request.as_bytes()).await?;
            Ok(socket)
        }
        .await;
        match delivered {
            Err(error) if dropped_connection(&error) && attempt < OWNER_REQUEST_ATTEMPTS => {
                attempt += 1;
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            delivered => return delivered,
        }
    }
}

fn request(record: &OwnerRecord, action: &str) -> io::Result<serde_json::Value> {
    let path = process_owner::socket_path(record);
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
            if bytes.is_empty() {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "process owner closed before responding",
                ));
            }
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, Write};
    use std::os::unix::net::UnixListener;

    fn spec() -> OciSpec {
        use super::super::oci::{OciLinux, OciProcess, OciRoot, OciUser};
        OciSpec {
            root: OciRoot {
                path: "/".into(),
                readonly: false,
            },
            process: OciProcess {
                args: vec!["sleep".into(), "60".into()],
                env: vec![],
                cwd: "/".into(),
                user: OciUser { uid: 0, gid: 0 },
                capabilities: None,
                overrides: None,
            },
            mounts: vec![],
            linux: OciLinux {
                namespaces: vec![],
                resources: None,
                cgroups_path: None,
                uid_mappings: None,
                gid_mappings: None,
            },
            port_mapping: None,
        }
    }

    /// A live owner that hangs up on its first clients without answering, the
    /// way the real one drops a client slower than its read timeout.
    struct ImpatientOwner {
        _lock: File,
        socket_directory: PathBuf,
    }

    impl ImpatientOwner {
        async fn start(control: &ProcessControl, id: &InstanceId, dropped: usize) -> Self {
            control.prepare(id, &spec()).await.unwrap();
            let directory = control.directory(id).unwrap();
            let mut record = control.load(id).unwrap();
            let pid = std::process::id();
            record.phase = OwnerPhase::Running { pid };
            process_owner::persist(&directory, &record).unwrap();
            // Holding the owner lock is what makes an owner live.
            let lock = process_owner::lock_owner(&directory).unwrap();
            let socket = process_owner::socket_path(&record);
            let socket_directory = socket.parent().unwrap().to_path_buf();
            std::fs::DirBuilder::new()
                .mode(0o700)
                .create(&socket_directory)
                .unwrap();
            let listener = UnixListener::bind(&socket).unwrap();
            std::thread::spawn(move || {
                for (index, connection) in listener.incoming().enumerate() {
                    let mut connection = connection.unwrap();
                    if index < dropped {
                        continue;
                    }
                    let mut line = String::new();
                    std::io::BufReader::new(&connection)
                        .read_line(&mut line)
                        .unwrap();
                    let response = serde_json::json!({"phase": {"state": "running", "pid": pid}});
                    writeln!(connection, "{response}").unwrap();
                }
            });
            Self {
                _lock: lock,
                socket_directory,
            }
        }
    }

    impl Drop for ImpatientOwner {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.socket_directory);
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn status_asks_a_live_owner_again_after_it_drops_the_connection() {
        let root = tempfile::tempdir().unwrap();
        let control = ProcessControl::new(root.path().join("owners"), PathBuf::from("/bin/false"));
        let id = InstanceId("default__impatient-0".into());
        let _owner = ImpatientOwner::start(&control, &id, 3).await;
        let record = control.status(&id).await.unwrap();
        assert!(matches!(record.phase, OwnerPhase::Running { .. }));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn exec_request_is_sent_again_until_the_owner_listens() {
        let root = tempfile::tempdir().unwrap();
        let socket = root.path().join("control.sock");
        // A socket file nobody listens on refuses connections, the way an
        // owner's socket does while it is busy dropping a slow client.
        drop(UnixListener::bind(&socket).unwrap());
        let owner = {
            let socket = socket.clone();
            let listening = root.path().join("listening.sock");
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(50));
                // Swap the live socket in with one rename: removing the dead
                // file first opened a window where connect saw NotFound, which
                // correctly means "no owner" and is never retried.
                let listener = UnixListener::bind(&listening).unwrap();
                std::fs::rename(&listening, &socket).unwrap();
                let (connection, _) = listener.accept().unwrap();
                let mut line = String::new();
                std::io::BufReader::new(&connection)
                    .read_line(&mut line)
                    .unwrap();
                line
            })
        };
        deliver_exec_request(&socket, "exec\n").await.unwrap();
        assert_eq!(owner.join().unwrap(), "exec\n");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn exec_request_gives_up_on_an_owner_that_never_listens() {
        let root = tempfile::tempdir().unwrap();
        let socket = root.path().join("control.sock");
        drop(UnixListener::bind(&socket).unwrap());
        let error = deliver_exec_request(&socket, "exec\n").await.unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::ConnectionRefused);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn status_gives_up_on_an_owner_that_never_answers() {
        let root = tempfile::tempdir().unwrap();
        let control = ProcessControl::new(root.path().join("owners"), PathBuf::from("/bin/false"));
        let id = InstanceId("default__silent-0".into());
        let _owner = ImpatientOwner::start(&control, &id, usize::MAX).await;
        // Still no proof of absence: the caller gets an error, not Stopped.
        assert!(control.status(&id).await.is_err());
    }
}
