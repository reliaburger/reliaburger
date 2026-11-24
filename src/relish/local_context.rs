//! Private host credentials for the managed laptop cluster.

use std::io::Read;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::RelishError;
use crate::file_lock::FileLock;

const MAX_CONTEXT_BYTES: u64 = 64 * 1024;

/// Credentials and endpoint belonging to one managed cluster operation.
/// This type deliberately omits `Debug` so diagnostics can't print the bearer.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LocalContext {
    /// Persistence format, currently 1.
    pub schema: u32,
    /// Stable cluster operation identifier, protecting another cluster's context.
    pub owner: String,
    /// Host-accessible HTTPS API endpoint of the node relish asks first:
    /// the first node set up.
    pub endpoint: String,
    /// The other nodes' API endpoints, which relish tries in order when
    /// `endpoint` doesn't answer (#2241). Empty for a cluster this host
    /// reaches through one forward, such as a laptop cluster.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub other_endpoints: Vec<String>,
    /// The endpoint that answered last time, when it wasn't `endpoint`, so
    /// the next command asks it first instead of waiting on a dead node.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_answered: Option<String>,
    /// Administrator bearer, stored only in an owner-readable file.
    pub token: String,
    /// Absolute path to the cluster's public CA certificate.
    pub ca_cert: PathBuf,
    /// Explicit host forwards; an absent origin never implies guest reachability.
    pub service_endpoints: crate::bun::capabilities::ServiceEndpoints,
}

impl LocalContext {
    /// Load an optional context, refusing malformed or exposed credentials.
    pub fn load(path: &Path) -> Result<Option<Self>, RelishError> {
        let file = match std::fs::File::open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if file.metadata()?.permissions().mode() & 0o077 != 0 {
                return Err(RelishError::InitFailed(format!(
                    "context {} contains credentials; restrict its permissions to 0600",
                    path.display()
                )));
            }
        }
        let mut bytes = Vec::new();
        file.take(MAX_CONTEXT_BYTES + 1).read_to_end(&mut bytes)?;
        if bytes.len() as u64 > MAX_CONTEXT_BYTES {
            return Err(RelishError::InitFailed(
                "local context exceeds 64 KiB".to_string(),
            ));
        }
        let context: Self = serde_json::from_slice(&bytes)
            .map_err(|error| RelishError::InitFailed(format!("invalid local context: {error}")))?;
        context.validate()?;
        Ok(Some(context))
    }

    /// Atomically save credentials, without replacing another cluster's
    /// working context. Another cluster's context whose files are gone is
    /// replaced, since nothing can use it any more, and returned so the
    /// caller can say so (#2162).
    pub fn save(&self, path: &Path) -> Result<Option<ReplacedContext>, RelishError> {
        self.validate()?;
        let parent = path.parent().ok_or_else(|| {
            RelishError::InitFailed("context has no parent directory".to_string())
        })?;
        let mut directory = std::fs::DirBuilder::new();
        directory.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            directory.mode(0o700);
        }
        directory.create(parent)?;
        let _lock = lock_context(path)?;
        let replaced = match Self::load(path)? {
            Some(existing) if existing.owner != self.owner => {
                let Some(missing) = existing.missing_file() else {
                    return Err(RelishError::InitFailed(
                        "another managed cluster owns the active context".to_string(),
                    ));
                };
                Some(ReplacedContext {
                    owner: existing.owner.clone(),
                    missing: missing.to_path_buf(),
                })
            }
            _ => None,
        };
        let bytes = serde_json::to_vec_pretty(self).map_err(RelishError::SerialiseJson)?;
        crate::sesame::identity::atomic_write_mode(path, &bytes, Some(0o600))?;
        Ok(replaced)
    }

    /// The file this context needs that no longer exists, if any: its CA
    /// certificate, which lives in the cluster's directory and goes when
    /// that directory is deleted. The token is in the context itself.
    pub fn missing_file(&self) -> Option<&Path> {
        match std::fs::metadata(&self.ca_cert) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Some(&self.ca_cert),
            _ => None,
        }
    }

    /// Remove the active credentials only when they belong to this operation.
    pub fn remove_owned(path: &Path, owner: &str) -> Result<(), RelishError> {
        let _lock = lock_context(path)?;
        if Self::load(path)?.is_some_and(|context| context.owner == owner) {
            std::fs::remove_file(path)?;
            #[cfg(unix)]
            if let Some(parent) = path.parent() {
                std::fs::File::open(parent)?.sync_all()?;
            }
        }
        Ok(())
    }

    /// Connect using this cluster's pinned CA and bearer, with explicit operator
    /// overrides taking precedence over saved values.
    pub fn client(
        &self,
        token: Option<&str>,
        ca_cert: Option<&Path>,
    ) -> Result<super::client::BunClient, RelishError> {
        self.connect(token, ca_cert, probe_endpoint)
            .map(|(client, _)| client)
    }

    /// [`client`](Self::client), aimed at the first of this context's
    /// endpoints that `probe` says answers, and that choice.
    ///
    /// Every endpoint gets the same pinned CA and bearer, so a node that
    /// answers but doesn't prove it belongs to this cluster's CA still fails
    /// the command on its TLS handshake: falling over never weakens the
    /// identity check.
    pub fn connect(
        &self,
        token: Option<&str>,
        ca_cert: Option<&Path>,
        probe: impl FnMut(&str) -> Result<(), super::UnreachableCause>,
    ) -> Result<(super::client::BunClient, EndpointChoice), RelishError> {
        self.validate()?;
        let ca = match ca_cert {
            Some(path) => std::fs::read(path).map_err(|error| {
                RelishError::ClientConfig(format!(
                    "can't read the CA certificate {}: {error}",
                    path.display()
                ))
            })?,
            None => self.read_ca()?,
        };
        let choice = self.choose_endpoint(probe)?;
        let client = super::client::BunClient::new_with_ca(
            &choice.endpoint,
            Some(token.unwrap_or(&self.token)),
            &ca,
        )?
        .with_service_endpoints(self.service_endpoints_for(&choice.endpoint));
        Ok((client, choice))
    }

    /// Every endpoint relish may try, in the order it tries them: the one
    /// that answered last time, then `endpoint`, then `other_endpoints`.
    pub fn endpoints(&self) -> Vec<&str> {
        let listed = std::iter::once(self.endpoint.as_str())
            .chain(self.other_endpoints.iter().map(String::as_str));
        // A remembered endpoint the list no longer holds is ignored, not
        // trusted: someone edited the list since.
        let last = self
            .last_answered
            .as_deref()
            .filter(|last| listed.clone().any(|endpoint| endpoint == *last));
        let mut order: Vec<&str> = Vec::new();
        for endpoint in last.into_iter().chain(listed) {
            if !order.contains(&endpoint) {
                order.push(endpoint);
            }
        }
        order
    }

    /// The first endpoint, in [`endpoints`](Self::endpoints) order, that
    /// `probe` says accepts a connection, with the ones before it that
    /// didn't. A context with one endpoint isn't probed: the command's own
    /// request says why it can't connect. When none answers, the error lists
    /// every endpoint tried and why.
    pub fn choose_endpoint(
        &self,
        mut probe: impl FnMut(&str) -> Result<(), super::UnreachableCause>,
    ) -> Result<EndpointChoice, RelishError> {
        let candidates = self.endpoints();
        if let [only] = candidates.as_slice() {
            return Ok(EndpointChoice {
                endpoint: only.to_string(),
                passed_over: Vec::new(),
            });
        }
        let mut passed_over = Vec::new();
        for endpoint in candidates {
            match probe(endpoint) {
                Ok(()) => {
                    return Ok(EndpointChoice {
                        endpoint: endpoint.to_string(),
                        passed_over,
                    });
                }
                Err(cause) => passed_over.push(super::EndpointAttempt {
                    endpoint: endpoint.to_string(),
                    cause,
                }),
            }
        }
        Err(RelishError::NoEndpointAnswered {
            attempts: passed_over,
        })
    }

    /// Remember that `endpoint` answered, in the context at `path`, so the
    /// next command asks it first. Leaves the file alone when nothing
    /// changes, or when it now holds another cluster's context or no longer
    /// lists `endpoint`.
    pub fn remember_answered(&self, path: &Path, endpoint: &str) -> Result<(), RelishError> {
        let last = (endpoint != self.endpoint).then(|| endpoint.to_string());
        if last == self.last_answered {
            return Ok(());
        }
        let _lock = lock_context(path)?;
        let Some(mut current) = Self::load(path)? else {
            return Ok(());
        };
        if current.owner != self.owner || !current.endpoints().contains(&endpoint) {
            return Ok(());
        }
        current.last_answered = last;
        let bytes = serde_json::to_vec_pretty(&current).map_err(RelishError::SerialiseJson)?;
        crate::sesame::identity::atomic_write_mode(path, &bytes, Some(0o600))?;
        Ok(())
    }

    /// The host forwards for a connection to `endpoint`. A forward on the
    /// same host as `endpoint` (a bare-metal node's registry and ingress)
    /// moves with the API to the node that answered; one on another host
    /// (a laptop's own forwards) stays.
    fn service_endpoints_for(&self, endpoint: &str) -> crate::bun::capabilities::ServiceEndpoints {
        let mut forwards = self.service_endpoints.clone();
        let (Some(home), Some(chosen)) = (url_host(&self.endpoint), url_host(endpoint)) else {
            return forwards;
        };
        if home == chosen {
            return forwards;
        }
        for forward in [
            &mut forwards.registry,
            &mut forwards.ingress_http,
            &mut forwards.ingress_https,
        ] {
            if let Some(origin) = forward
                && let Some(moved) = with_host(origin, home, chosen)
            {
                *origin = moved;
            }
        }
        forwards
    }

    /// This context's host forwards, for a connection pinned to the same
    /// cluster CA.
    ///
    /// `--endpoint` bypasses the context's credentials so an operator can
    /// talk to any node, say node 2 through its own API forward. The
    /// registry and ingress forwards are still how this host reaches that
    /// cluster, and without them relish falls back to the node's own
    /// listener address, which means nothing outside the VM. The CA is the
    /// identity check: a connection that trusts the same root CA talks to
    /// the same cluster. Anything else (no CA, another CA, an unreadable
    /// file) gets no forwards.
    pub fn forwards_for_ca(
        &self,
        connection_ca_pem: Option<&[u8]>,
    ) -> Option<crate::bun::capabilities::ServiceEndpoints> {
        let connection_ca_pem = connection_ca_pem?;
        let context_ca_pem = std::fs::read(&self.ca_cert).ok()?;
        (context_ca_pem == connection_ca_pem).then(|| self.service_endpoints.clone())
    }

    /// This context's own CA certificate, or an error that names the
    /// context and the missing file and says what to do about it.
    fn read_ca(&self) -> Result<Vec<u8>, RelishError> {
        std::fs::read(&self.ca_cert).map_err(|error| {
            if error.kind() != std::io::ErrorKind::NotFound {
                return RelishError::ClientConfig(format!(
                    "can't read the CA certificate {} that the relish context for {} trusts: {error}",
                    self.ca_cert.display(),
                    self.owner
                ));
            }
            RelishError::ContextFileMissing {
                // The path only goes into the message; every caller loaded
                // this context from the default path.
                context: default_path().unwrap_or_else(|_| PathBuf::from("context.json")),
                owner: self.owner.clone(),
                missing: self.ca_cert.clone(),
            }
        })
    }

    fn validate(&self) -> Result<(), RelishError> {
        if self.schema != 1
            || self.owner.is_empty()
            || self.token.is_empty()
            || !self.ca_cert.is_absolute()
        {
            return Err(RelishError::InitFailed(
                "invalid local context schema, owner, token or CA path".to_string(),
            ));
        }
        super::client::validate_endpoint(&self.endpoint).map_err(|error| {
            RelishError::InitFailed(format!("invalid context endpoint: {error}"))
        })?;
        if !self.endpoint.starts_with("https://") {
            return Err(RelishError::InitFailed(
                "managed context requires HTTPS".to_string(),
            ));
        }
        for endpoint in &self.other_endpoints {
            super::client::validate_endpoint(endpoint).map_err(|error| {
                RelishError::InitFailed(format!("invalid context endpoint {endpoint}: {error}"))
            })?;
            if !endpoint.starts_with("https://") {
                return Err(RelishError::InitFailed(format!(
                    "managed context requires HTTPS, and {endpoint} isn't"
                )));
            }
        }
        Ok(())
    }
}

/// How long relish waits for each of a context's endpoints to accept a TCP
/// connection before it tries the next (#2241). A node on the same LAN
/// answers in milliseconds; one whose power is off never does.
pub const PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(1500);

/// Which of a context's endpoints a command uses, and the ones it tried
/// first that didn't answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EndpointChoice {
    /// The endpoint that accepted a connection.
    pub endpoint: String,
    /// The endpoints tried before it, and why each didn't answer.
    pub passed_over: Vec<super::EndpointAttempt>,
}

/// Whether `endpoint` accepts a TCP connection within [`PROBE_TIMEOUT`].
///
/// It's only a connect: the command's own request still does the TLS
/// handshake against the cluster CA, so a probe that reaches the wrong
/// server can't let it through. It blocks, like the rest of reading the
/// context; `relish mcp` builds its clients off the runtime.
pub fn probe_endpoint(endpoint: &str) -> Result<(), super::UnreachableCause> {
    use super::UnreachableCause;
    let url = reqwest::Url::parse(endpoint)
        .map_err(|error| UnreachableCause::Other(format!("invalid endpoint: {error}")))?;
    let addresses = url
        .socket_addrs(|| None)
        .map_err(|_| UnreachableCause::NameResolution)?;
    let mut cause = UnreachableCause::NameResolution;
    for address in addresses {
        match std::net::TcpStream::connect_timeout(&address, PROBE_TIMEOUT) {
            Ok(_) => return Ok(()),
            Err(error) => {
                cause = match error.kind() {
                    std::io::ErrorKind::ConnectionRefused => UnreachableCause::Refused,
                    std::io::ErrorKind::TimedOut | std::io::ErrorKind::WouldBlock => {
                        UnreachableCause::TimedOut
                    }
                    _ => UnreachableCause::Other(error.to_string()),
                }
            }
        }
    }
    Err(cause)
}

/// The host of an `scheme://host[:port][/path]` URL, brackets and all for
/// IPv6, or `None` when it isn't one.
fn url_host(url: &str) -> Option<&str> {
    let (_, rest) = url.split_once("://")?;
    let authority = rest.split('/').next()?;
    let host = if authority.ends_with(']') {
        authority
    } else {
        authority
            .rsplit_once(':')
            .map_or(authority, |(host, _)| host)
    };
    (!host.is_empty()).then_some(host)
}

/// `url` with its host `from` replaced by `to`, or `None` when its host
/// isn't `from`.
fn with_host(url: &str, from: &str, to: &str) -> Option<String> {
    if url_host(url)? != from {
        return None;
    }
    let (scheme, rest) = url.split_once("://")?;
    Some(format!("{scheme}://{to}{}", &rest[from.len()..]))
}

/// Another cluster's context that [`LocalContext::save`] replaced because a
/// file it needs is gone.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReplacedContext {
    /// The cluster the old context was for.
    pub owner: String,
    /// The file it needed that no longer exists.
    pub missing: PathBuf,
}

/// The default managed context path, separate from user shell configuration.
pub fn default_path() -> Result<PathBuf, RelishError> {
    Ok(root_directory()?.join("context.json"))
}

/// Lock the context's lock file. The guard unlocks when dropped, so the next
/// writer straight after isn't refused by a child mid-spawn (#500).
fn lock_context(path: &Path) -> Result<FileLock, RelishError> {
    let lock_path = path.with_extension("lock");
    let mut options = std::fs::OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let lock = FileLock::try_lock(options.open(&lock_path)?)
        .map_err(|error| RelishError::InitFailed(format!("local context is busy: {error}")))?;
    // `relish uninstall` deletes the lock file (under the lock) once no
    // context is left. A writer that opened the file just before that
    // unlink now holds a lock nobody else can see, while a newer writer
    // could lock a fresh file at the same path. Refuse unless the file we
    // locked is still the one the path names.
    if !still_linked(lock.file(), &lock_path)? {
        return Err(RelishError::InitFailed(
            "local context is busy: its lock file was removed; retry".to_string(),
        ));
    }
    Ok(lock)
}

#[cfg(unix)]
fn still_linked(lock: &std::fs::File, lock_path: &Path) -> Result<bool, RelishError> {
    use std::os::unix::fs::MetadataExt;
    let held = lock.metadata()?;
    match std::fs::symlink_metadata(lock_path) {
        Ok(current) => Ok(current.dev() == held.dev() && current.ino() == held.ino()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error.into()),
    }
}

#[cfg(not(unix))]
fn still_linked(_lock: &std::fs::File, _lock_path: &Path) -> Result<bool, RelishError> {
    Ok(true)
}

/// Delete the context's lock file once no context remains beside it.
///
/// Returns `true` if the lock file was removed. The unlink happens while
/// holding the lock, and every locker re-checks that its file is still
/// linked, so no two writers can end up on different lock files. Leaves
/// everything alone (and returns `false`) while a context file exists.
pub fn remove_unused_lock(path: &Path) -> Result<bool, RelishError> {
    let lock_path = path.with_extension("lock");
    if std::fs::symlink_metadata(&lock_path).is_err() {
        return Ok(false);
    }
    let _lock = lock_context(path)?;
    if std::fs::symlink_metadata(path).is_ok() {
        return Ok(false);
    }
    std::fs::remove_file(&lock_path)?;
    Ok(true)
}

/// Managed local state root; an explicit override must be an absolute path.
pub fn root_directory() -> Result<PathBuf, RelishError> {
    if let Some(path) = std::env::var_os("RELIABURGER_HOME") {
        let path = PathBuf::from(path);
        if !path.is_absolute() {
            return Err(RelishError::InitFailed(
                "RELIABURGER_HOME must be absolute".into(),
            ));
        }
        return Ok(path);
    }
    dirs::home_dir()
        .map(|home| home.join(".reliaburger"))
        .ok_or_else(|| {
            RelishError::InitFailed("cannot locate the home directory for local state".into())
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn removing_a_cluster_context_preserves_another_owner() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("context.json");
        context(root.path(), "one").save(&path).unwrap();
        LocalContext::remove_owned(&path, "two").unwrap();
        assert!(path.exists());
        LocalContext::remove_owned(&path, "one").unwrap();
        assert!(!path.exists());
        LocalContext::remove_owned(&path, "one").unwrap();
    }

    #[test]
    fn unused_lock_is_removed_but_a_live_context_keeps_it() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("context.json");
        let lock = root.path().join("context.lock");
        context(root.path(), "one").save(&path).unwrap();
        assert!(lock.exists(), "saving creates the lock beside the context");

        assert!(!remove_unused_lock(&path).unwrap());
        assert!(lock.exists(), "a saved context keeps its lock");

        LocalContext::remove_owned(&path, "one").unwrap();
        assert!(remove_unused_lock(&path).unwrap());
        assert!(!lock.exists());
        assert!(!remove_unused_lock(&path).unwrap(), "already gone");
    }

    /// A child that another thread is forking holds a copy of every open
    /// descriptor until its `exec`, the lock file's included. Each writer
    /// must still release the lock when it returns, or the next one is
    /// refused as busy (#500).
    #[test]
    fn writers_take_turns_while_other_threads_spawn_processes() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};

        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("context.json");
        let spawning = Arc::new(AtomicBool::new(true));
        let spawners: Vec<_> = (0..2)
            .map(|_| {
                let spawning = Arc::clone(&spawning);
                std::thread::spawn(move || {
                    while spawning.load(Ordering::Relaxed) {
                        let _ = std::process::Command::new("true")
                            .stdin(std::process::Stdio::null())
                            .stdout(std::process::Stdio::null())
                            .stderr(std::process::Stdio::null())
                            .status();
                    }
                })
            })
            .collect();
        let refused = (0..100)
            .filter(|_| {
                context(root.path(), "one").save(&path).is_err()
                    || LocalContext::remove_owned(&path, "one").is_err()
                    || remove_unused_lock(&path).is_err()
            })
            .count();
        spawning.store(false, Ordering::Relaxed);
        for spawner in spawners {
            spawner.join().unwrap();
        }
        assert_eq!(refused, 0, "writers refused by a lock nobody holds");
    }

    #[test]
    fn unused_lock_is_left_while_another_process_holds_it() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("context.json");
        let _held = lock_context(&path).unwrap();
        assert!(remove_unused_lock(&path).is_err());
        assert!(root.path().join("context.lock").exists());
    }

    #[cfg(unix)]
    #[test]
    fn a_lock_file_unlinked_or_replaced_is_detected() {
        let root = tempfile::tempdir().unwrap();
        let lock_path = root.path().join("context.lock");
        // A writer that opened the lock file before uninstall unlinked it.
        std::fs::write(&lock_path, "").unwrap();
        let stale = std::fs::File::open(&lock_path).unwrap();
        std::fs::remove_file(&lock_path).unwrap();
        assert!(!still_linked(&stale, &lock_path).unwrap());
        // A fresh file at the same path is a different inode.
        std::fs::write(&lock_path, "").unwrap();
        assert!(!still_linked(&stale, &lock_path).unwrap());
        let fresh = std::fs::File::open(&lock_path).unwrap();
        assert!(still_linked(&fresh, &lock_path).unwrap());
    }

    /// A context for `owner` whose CA certificate exists in `root`.
    fn context(root: &std::path::Path, owner: &str) -> LocalContext {
        let ca_cert = root.join("root-ca.crt");
        if !ca_cert.exists() {
            std::fs::write(&ca_cert, b"cluster ca").unwrap();
        }
        LocalContext {
            schema: 1,
            owner: owner.to_string(),
            endpoint: "https://127.0.0.1:19117".to_string(),
            other_endpoints: Vec::new(),
            last_answered: None,
            token: "rbrg_private".to_string(),
            ca_cert: root.join("root-ca.crt"),
            service_endpoints: Default::default(),
        }
    }

    const NODE_1: &str = "https://10.77.0.11:9117";
    const NODE_2: &str = "https://10.77.0.12:9117";
    const NODE_3: &str = "https://10.77.0.13:9117";

    /// A bare-metal context for three nodes, node 1 first.
    fn three_nodes(root: &std::path::Path) -> LocalContext {
        let mut three = context(root, "bare-metal/lab");
        three.endpoint = NODE_1.to_string();
        three.other_endpoints = vec![NODE_2.to_string(), NODE_3.to_string()];
        three
    }

    /// A probe that answers only for `up`, recording what it was asked.
    fn only_up<'a>(
        up: &'a [&'a str],
        asked: &'a std::cell::RefCell<Vec<String>>,
    ) -> impl FnMut(&str) -> Result<(), crate::relish::UnreachableCause> + 'a {
        move |endpoint| {
            asked.borrow_mut().push(endpoint.to_string());
            if up.contains(&endpoint) {
                Ok(())
            } else {
                Err(crate::relish::UnreachableCause::TimedOut)
            }
        }
    }

    #[test]
    fn with_its_first_endpoint_down_a_three_node_context_still_answers() {
        let root = tempfile::tempdir().unwrap();
        let asked = std::cell::RefCell::new(Vec::new());

        let choice = three_nodes(root.path())
            .choose_endpoint(only_up(&[NODE_2, NODE_3], &asked))
            .unwrap();

        assert_eq!(choice.endpoint, NODE_2);
        assert_eq!(
            choice.passed_over,
            vec![crate::relish::EndpointAttempt {
                endpoint: NODE_1.to_string(),
                cause: crate::relish::UnreachableCause::TimedOut,
            }]
        );
        assert_eq!(
            *asked.borrow(),
            vec![NODE_1, NODE_2],
            "stops at the first that answers"
        );
    }

    #[test]
    fn when_no_endpoint_answers_the_error_lists_every_endpoint_tried() {
        let root = tempfile::tempdir().unwrap();
        let asked = std::cell::RefCell::new(Vec::new());

        let error = three_nodes(root.path())
            .choose_endpoint(only_up(&[], &asked))
            .unwrap_err();

        assert!(matches!(
            &error,
            RelishError::NoEndpointAnswered { attempts } if attempts.len() == 3
        ));
        let message = error.to_string();
        for endpoint in [NODE_1, NODE_2, NODE_3] {
            assert!(message.contains(endpoint), "{message}");
        }
        assert!(
            message.contains("no answer before the connect timeout"),
            "{message}"
        );
    }

    #[test]
    fn a_context_with_one_endpoint_is_not_probed() {
        let root = tempfile::tempdir().unwrap();
        let asked = std::cell::RefCell::new(Vec::new());

        let choice = context(root.path(), "cluster-a")
            .choose_endpoint(only_up(&[], &asked))
            .unwrap();

        assert_eq!(choice.endpoint, "https://127.0.0.1:19117");
        assert!(asked.borrow().is_empty());
    }

    #[test]
    fn the_endpoint_that_answered_last_is_tried_first_then_the_rest_in_order() {
        let root = tempfile::tempdir().unwrap();
        let mut context = three_nodes(root.path());
        assert_eq!(context.endpoints(), vec![NODE_1, NODE_2, NODE_3]);

        context.last_answered = Some(NODE_3.to_string());
        assert_eq!(context.endpoints(), vec![NODE_3, NODE_1, NODE_2]);

        // An endpoint the list no longer holds is ignored.
        context.last_answered = Some("https://10.77.0.99:9117".to_string());
        assert_eq!(context.endpoints(), vec![NODE_1, NODE_2, NODE_3]);
    }

    #[test]
    fn the_node_that_answered_is_remembered_for_the_next_command() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("context.json");
        let context = three_nodes(root.path());
        context.save(&path).unwrap();

        context.remember_answered(&path, NODE_2).unwrap();
        let saved = LocalContext::load(&path).unwrap().unwrap();
        assert_eq!(saved.last_answered.as_deref(), Some(NODE_2));
        assert_eq!(saved.endpoints()[0], NODE_2);

        // Node 1 answering again puts it back first.
        saved.remember_answered(&path, NODE_1).unwrap();
        let saved = LocalContext::load(&path).unwrap().unwrap();
        assert_eq!(saved.last_answered, None);
        assert_eq!(saved.endpoints()[0], NODE_1);
    }

    #[test]
    fn remembering_leaves_another_clusters_context_alone() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("context.json");
        context(root.path(), "cluster-a").save(&path).unwrap();

        three_nodes(root.path())
            .remember_answered(&path, NODE_2)
            .unwrap();

        let saved = LocalContext::load(&path).unwrap().unwrap();
        assert_eq!(saved.owner, "cluster-a");
        assert_eq!(saved.last_answered, None);
    }

    #[test]
    fn forwards_on_the_first_nodes_host_move_to_the_node_that_answered() {
        let root = tempfile::tempdir().unwrap();
        let mut context = three_nodes(root.path());
        context.service_endpoints = crate::bun::capabilities::ServiceEndpoints {
            registry: Some("https://10.77.0.11:5050".to_string()),
            ingress_http: Some("http://10.77.0.11:80".to_string()),
            ingress_https: Some("https://127.0.0.1:8443".to_string()),
        };

        let forwards = context.service_endpoints_for(NODE_2);

        assert_eq!(
            forwards.registry.as_deref(),
            Some("https://10.77.0.12:5050")
        );
        assert_eq!(
            forwards.ingress_http.as_deref(),
            Some("http://10.77.0.12:80")
        );
        assert_eq!(
            forwards.ingress_https.as_deref(),
            Some("https://127.0.0.1:8443"),
            "a forward on another host stays"
        );
        assert_eq!(
            context.service_endpoints_for(NODE_1),
            context.service_endpoints
        );
    }

    #[test]
    fn hosts_are_found_in_ipv4_ipv6_and_named_urls() {
        assert_eq!(url_host("https://10.0.0.1:9117"), Some("10.0.0.1"));
        assert_eq!(url_host("https://[fd00::1]:9117/v1"), Some("[fd00::1]"));
        assert_eq!(url_host("http://[fd00::1]"), Some("[fd00::1]"));
        assert_eq!(url_host("http://wyse-1.lan"), Some("wyse-1.lan"));
        assert_eq!(url_host("not a url"), None);
        assert_eq!(
            with_host("https://[fd00::1]:5050", "[fd00::1]", "[fd00::2]").as_deref(),
            Some("https://[fd00::2]:5050")
        );
        assert_eq!(
            with_host("https://10.0.0.1:5050", "10.0.0.9", "10.0.0.2"),
            None
        );
    }

    #[test]
    fn every_endpoint_of_a_context_must_be_https() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("context.json");
        let mut plaintext = three_nodes(root.path());
        plaintext
            .other_endpoints
            .push("http://10.77.0.14:9117".to_string());
        assert!(plaintext.save(&path).is_err());
    }

    #[test]
    fn a_context_whose_first_node_refuses_connects_to_the_next_that_listens() {
        let root = tempfile::tempdir().unwrap();
        let dead = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let dead_port = dead.local_addr().unwrap().port();
        drop(dead);
        let live = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let live_port = live.local_addr().unwrap().port();
        let hierarchy = crate::sesame::ca::generate_ca_hierarchy("probe", b"ikm").unwrap();
        std::fs::write(
            root.path().join("root-ca.crt"),
            crate::sesame::cert::der_to_pem(&hierarchy.root.ca.certificate_der, "CERTIFICATE"),
        )
        .unwrap();
        let mut context = context(root.path(), "bare-metal/lab");
        context.endpoint = format!("https://127.0.0.1:{dead_port}");
        context.other_endpoints = vec![format!("https://127.0.0.1:{live_port}")];

        let (client, choice) = context.connect(None, None, probe_endpoint).unwrap();

        assert_eq!(client.base_url(), format!("https://127.0.0.1:{live_port}"));
        assert_eq!(choice.passed_over.len(), 1);
        assert_eq!(
            choice.passed_over[0].cause,
            crate::relish::UnreachableCause::Refused
        );
    }

    #[test]
    #[cfg(unix)]
    fn saving_context_does_not_follow_a_preexisting_temporary_symlink() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("context.json");
        let victim = root.path().join("unrelated.txt");
        std::fs::write(&victim, b"keep this file").unwrap();
        std::os::unix::fs::symlink(&victim, path.with_extension("tmp")).unwrap();
        context(root.path(), "cluster-a").save(&path).unwrap();
        assert_eq!(std::fs::read(&victim).unwrap(), b"keep this file");
    }

    #[test]
    fn context_roundtrip_preserves_credentials_privately() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("context.json");
        let original = context(root.path(), "cluster-a");
        original.save(&path).unwrap();
        let loaded = LocalContext::load(&path).unwrap().unwrap();
        assert_eq!(loaded.endpoint, original.endpoint);
        assert_eq!(loaded.token, original.token);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }

    #[test]
    fn setup_cannot_replace_another_clusters_context() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("context.json");
        context(root.path(), "cluster-a").save(&path).unwrap();
        assert!(context(root.path(), "cluster-b").save(&path).is_err());
        assert_eq!(
            LocalContext::load(&path).unwrap().unwrap().owner,
            "cluster-a"
        );
    }

    #[test]
    fn a_context_whose_ca_is_gone_is_replaced_and_reported() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("context.json");
        let old_cluster = tempfile::tempdir().unwrap();
        context(old_cluster.path(), "cluster-a")
            .save(&path)
            .unwrap();
        let missing = old_cluster.path().join("root-ca.crt");
        drop(old_cluster);

        let replaced = context(root.path(), "cluster-b").save(&path).unwrap();

        assert_eq!(
            replaced,
            Some(ReplacedContext {
                owner: "cluster-a".to_string(),
                missing,
            })
        );
        assert_eq!(
            LocalContext::load(&path).unwrap().unwrap().owner,
            "cluster-b"
        );
    }

    #[test]
    fn saving_over_a_working_context_of_the_same_cluster_replaces_nothing_else() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("context.json");
        assert_eq!(context(root.path(), "cluster-a").save(&path).unwrap(), None);
        assert_eq!(context(root.path(), "cluster-a").save(&path).unwrap(), None);
    }

    #[test]
    fn a_context_whose_ca_is_gone_says_which_context_and_file_and_what_to_do() {
        let root = tempfile::tempdir().unwrap();
        let mut stale = context(root.path(), "bare-metal/lab");
        stale.ca_cert = root.path().join("deleted-cluster/root-ca.crt");

        let error = stale.client(None, None).err().unwrap();

        assert!(matches!(error, RelishError::ContextFileMissing { .. }));
        let message = error.to_string();
        assert!(message.contains("bare-metal/lab"), "{message}");
        assert!(
            message.contains(&stale.ca_cert.display().to_string()),
            "{message}"
        );
        assert!(message.contains("context.json"), "{message}");
        assert!(
            message.contains("relish machines claim --create"),
            "{message}"
        );
        assert!(!message.contains("status 0"), "{message}");
    }

    #[test]
    fn a_missing_ca_given_on_the_command_line_is_named() {
        let root = tempfile::tempdir().unwrap();
        let missing = root.path().join("elsewhere.crt");
        let error = context(root.path(), "cluster-a")
            .client(None, Some(&missing))
            .err()
            .unwrap();
        assert!(matches!(error, RelishError::ClientConfig(_)));
        assert!(
            error.to_string().contains(&missing.display().to_string()),
            "{error}"
        );
    }

    #[test]
    fn missing_file_names_only_a_ca_that_is_gone() {
        let root = tempfile::tempdir().unwrap();
        let mut live = context(root.path(), "cluster-a");
        assert_eq!(live.missing_file(), None);
        live.ca_cert = root.path().join("gone.crt");
        assert_eq!(live.missing_file(), Some(live.ca_cert.as_path()));
    }

    #[test]
    fn context_rejects_remote_plaintext_and_unknown_schema() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("context.json");
        let mut invalid = context(root.path(), "cluster-a");
        invalid.endpoint = "http://192.0.2.1:9117".to_string();
        assert!(invalid.save(&path).is_err());
        invalid.endpoint = "https://127.0.0.1:19117".to_string();
        invalid.schema = 2;
        assert!(invalid.save(&path).is_err());
    }

    #[test]
    #[cfg(unix)]
    fn context_refuses_to_use_world_readable_credentials() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("context.json");
        context(root.path(), "cluster-a").save(&path).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(LocalContext::load(&path).is_err());
    }

    #[test]
    fn missing_context_leaves_the_ordinary_local_default_available() {
        let root = tempfile::tempdir().unwrap();
        assert!(
            LocalContext::load(&root.path().join("absent"))
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn forwards_apply_only_to_a_connection_pinned_to_the_same_ca() {
        let root = tempfile::tempdir().unwrap();
        let ca_cert = root.path().join("root-ca.crt");
        std::fs::write(&ca_cert, b"cluster ca").unwrap();
        let mut context = context(root.path(), "cluster-a");
        context.ca_cert = ca_cert;
        context.service_endpoints.registry = Some("https://127.0.0.1:15050".to_string());

        let forwards = context.forwards_for_ca(Some(b"cluster ca")).unwrap();
        assert_eq!(
            forwards.registry.as_deref(),
            Some("https://127.0.0.1:15050")
        );
        assert!(context.forwards_for_ca(Some(b"another ca")).is_none());
        assert!(context.forwards_for_ca(None).is_none());

        context.ca_cert = root.path().join("missing.crt");
        assert!(context.forwards_for_ca(Some(b"cluster ca")).is_none());
    }
}
