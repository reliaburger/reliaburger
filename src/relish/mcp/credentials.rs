//! `relish mcp`'s own read-only token (#1730).
//!
//! The saved local context holds the cluster's admin token, and an agent's
//! MCP connection should never carry that. So when `relish mcp` (or `relish
//! agent setup --yes`) finds an admin context, it uses the admin token once,
//! to create a read-only token with a one-day lifetime, and keeps that
//! beside the context in `mcp-token.json` (or `mcp-token-<namespace>.json`
//! for `--namespace`), with the context's own file mode, 0600. Only `relish
//! mcp` and `relish agent setup` read that file. From then on `relish mcp`
//! sends the read-only token. When it has expired, or the cluster rejects
//! it, a new one is made. Nothing here ever hands the admin token to a tool:
//! a cluster that won't make a read-only token is an error, not a reason to
//! fall back.
//!
//! Making a token happens under a file lock beside the token file
//! (`mcp-token.lock`), so several `relish mcp` processes starting together
//! make one token between them: the first makes it, the rest find it saved.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::file_lock::{FileLock, FileLockError};
use crate::relish::RelishError;
use crate::relish::client::{BunClient, TokenAccess, TokenLifetime, TokenRequest};

/// How long a read-only token for MCP lives: a day, as the manual's
/// `relish token create --ttl-days 1` suggests.
pub const TOKEN_TTL_DAYS: u64 = 1;

/// A token this close to its expiry is renewed rather than used.
const RENEW_MARGIN_SECONDS: u64 = 5 * 60;

/// How long each request to the cluster may take here.
const CLUSTER_TIMEOUT: Duration = Duration::from_secs(10);

/// Largest token file `load` reads.
const MAX_FILE_BYTES: u64 = 16 * 1024;

/// How long a server waits for another one that's making the token: longer
/// than that one's probe, create and revoke together.
const LOCK_TIMEOUT: Duration = Duration::from_secs(4 * CLUSTER_TIMEOUT.as_secs());

/// How every token `relish mcp` makes is named: this, then the time and the
/// process id. `relish token list`, `relish wtf` and `relish agent
/// diagnose` know them by it and don't warn that they expire, because
/// `relish mcp` renews them itself.
pub const TOKEN_NAME_PREFIX: &str = "relish-mcp-";

/// Whether `name` is one of the tokens `relish mcp` makes. The name is the
/// only mark a token carries; a token someone else names this way is
/// treated the same.
pub fn made_by_relish_mcp(name: &str) -> bool {
    name.starts_with(TOKEN_NAME_PREFIX)
}

/// Why `relish mcp` has no read-only token to send. `Clone`, so `relish
/// mcp` can answer with a refusal it remembers.
#[derive(Debug, Clone, thiserror::Error)]
pub enum CredentialError {
    /// The cluster answered the token request with an error.
    #[error(
        "the cluster refused to create a read-only token for relish mcp (HTTP {status}: {body}); \
         relish mcp won't use the context's admin token instead. Make one with `relish token \
         create --name agent --role read-only --ttl-days 1` and pass it as RELIABURGER_TOKEN"
    )]
    Refused {
        /// The HTTP status.
        status: u16,
        /// What the cluster said.
        body: String,
    },
    /// The cluster rejected the context's own token (401).
    #[error(
        "the cluster rejected the context's token, so relish mcp can't make a read-only one; \
         check `relish status`"
    )]
    ContextRejected,
    /// No answer from the cluster.
    #[error(
        "couldn't reach the cluster to get a read-only token for relish mcp: {0}; to use one \
         you made yourself, make it with `relish token create --name agent --role read-only \
         --ttl-days 1` and pass it as RELIABURGER_TOKEN"
    )]
    Unreachable(String),
    /// The stored token file couldn't be read or written.
    #[error("relish mcp's read-only token at {path}: {reason}")]
    Storage {
        /// The file.
        path: PathBuf,
        /// What went wrong.
        reason: String,
    },
    /// A `--namespace` that can't be part of a file name.
    #[error("namespace {0:?} isn't a valid namespace name")]
    BadNamespace(String),
}

/// The cluster and scope a stored token belongs to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scope {
    /// The context's `owner`, so another cluster's token is never reused.
    pub owner: String,
    /// The context's endpoint.
    pub endpoint: String,
    /// `relish mcp --namespace`: the token is confined to it.
    pub namespace: Option<String>,
}

/// The read-only token as stored on disk. No `Debug`, so diagnostics can't
/// print the secret.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StoredToken {
    /// File format, currently 1.
    pub schema: u32,
    /// The context `owner` it was made under.
    pub owner: String,
    /// The endpoint it was made at.
    pub endpoint: String,
    /// The namespace it's confined to, if any.
    pub namespace: Option<String>,
    /// Its name in `relish token list`.
    pub name: String,
    /// The secret.
    pub token: String,
    /// Its expiry, Unix seconds.
    pub expires_at: Option<u64>,
}

impl StoredToken {
    /// Whether this token belongs to `scope` and has time left at `now`.
    pub fn usable(&self, scope: &Scope, now: u64) -> bool {
        self.schema == 1
            && self.owner == scope.owner
            && self.endpoint == scope.endpoint
            && self.namespace == scope.namespace
            && !self.token.is_empty()
            && self
                .expires_at
                .is_none_or(|expiry| expiry > now.saturating_add(RENEW_MARGIN_SECONDS))
    }

    /// Load a stored token, refusing a file others can read, as the context
    /// does.
    pub fn load(path: &Path) -> Result<Option<Self>, CredentialError> {
        let storage = |reason: String| CredentialError::Storage {
            path: path.to_path_buf(),
            reason,
        };
        let file = match std::fs::File::open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(storage(error.to_string())),
        };
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = file
                .metadata()
                .map_err(|error| storage(error.to_string()))?
                .permissions()
                .mode();
            if mode & 0o077 != 0 {
                return Err(storage(
                    "it holds a credential; restrict its permissions to 0600".to_string(),
                ));
            }
        }
        let mut bytes = Vec::new();
        file.take(MAX_FILE_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(|error| storage(error.to_string()))?;
        if bytes.len() as u64 > MAX_FILE_BYTES {
            return Err(storage("it's larger than 16 KiB".to_string()));
        }
        serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|error| storage(error.to_string()))
    }

    /// Save it atomically with mode 0600 in a 0700 directory, as the context
    /// is saved.
    pub fn save(&self, path: &Path) -> Result<(), CredentialError> {
        let storage = |reason: String| CredentialError::Storage {
            path: path.to_path_buf(),
            reason,
        };
        create_directory(path).map_err(|error| storage(error.to_string()))?;
        let bytes = serde_json::to_vec_pretty(self).map_err(|error| storage(error.to_string()))?;
        crate::sesame::identity::atomic_write_mode(path, &bytes, Some(0o600))
            .map_err(|error| storage(error.to_string()))
    }
}

/// Make the directory `path` goes in, 0700 like the context's.
fn create_directory(path: &Path) -> std::io::Result<()> {
    let Some(parent) = path.parent() else {
        return Ok(());
    };
    let mut directory = std::fs::DirBuilder::new();
    directory.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        directory.mode(0o700);
    }
    directory.create(parent)
}

/// The JSON a token file holds, as `relish agent setup` previews it: the
/// stored token's, or the one a new token would get at `now`, with the
/// secret replaced by [`crate::redact::REDACTED`].
pub fn shape(scope: &Scope, stored: Option<&StoredToken>, now: u64) -> String {
    let mut preview = stored.cloned().unwrap_or_else(|| StoredToken {
        schema: 1,
        owner: scope.owner.clone(),
        endpoint: scope.endpoint.clone(),
        namespace: scope.namespace.clone(),
        name: token_name(now),
        token: String::new(),
        expires_at: Some(now.saturating_add(TOKEN_TTL_DAYS * 24 * 60 * 60)),
    });
    preview.token = crate::redact::REDACTED.to_string();
    // Strings, numbers and options only: serialising it can't fail.
    serde_json::to_string_pretty(&preview).unwrap_or_default()
}

/// The name for a token made at `now` by this process.
fn token_name(now: u64) -> String {
    format!("{TOKEN_NAME_PREFIX}{now}-{}", std::process::id())
}

/// Every token file beside `context`: `mcp-token.json` and each
/// `mcp-token-<namespace>.json`, sorted by name. None when the directory
/// isn't there.
pub fn files_beside(context: &Path) -> Result<Vec<PathBuf>, CredentialError> {
    let Some(directory) = context.parent() else {
        return Ok(Vec::new());
    };
    let storage = |error: std::io::Error| CredentialError::Storage {
        path: directory.to_path_buf(),
        reason: error.to_string(),
    };
    let entries = match std::fs::read_dir(directory) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(storage(error)),
    };
    let mut files = Vec::new();
    for entry in entries {
        let name = entry.map_err(storage)?.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let token_file = name == "mcp-token.json"
            || name
                .strip_prefix("mcp-token-")
                .and_then(|rest| rest.strip_suffix(".json"))
                .is_some_and(valid_namespace);
        if token_file {
            files.push(context.with_file_name(name));
        }
    }
    files.sort();
    Ok(files)
}

/// Where the token for `namespace` lives, beside `context`.
pub fn path_beside(context: &Path, namespace: Option<&str>) -> Result<PathBuf, CredentialError> {
    let name = match namespace {
        None => "mcp-token.json".to_string(),
        Some(namespace) if valid_namespace(namespace) => format!("mcp-token-{namespace}.json"),
        Some(namespace) => return Err(CredentialError::BadNamespace(namespace.to_string())),
    };
    Ok(context.with_file_name(name))
}

/// A namespace name as Reliaburger accepts one: lowercase letters, digits
/// and `-`, starting with a letter or digit. Anything else could escape the
/// directory as part of a file name.
fn valid_namespace(namespace: &str) -> bool {
    !namespace.is_empty()
        && namespace.len() <= 63
        && namespace
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        && !namespace.starts_with('-')
}

/// What `relish mcp` should send.
#[derive(Clone, PartialEq, Eq)]
pub enum Credential {
    /// This read-only token.
    ReadOnly(String),
    /// The context's own token, which the cluster says isn't admin: nothing
    /// to mint.
    ContextNotAdmin,
}

/// What [`obtain`] did, for `relish agent setup` to report.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Obtained {
    /// A stored token was still good.
    Reused,
    /// A new token was made and stored.
    Minted,
    /// The context's token isn't admin, so nothing was made.
    NotNeeded,
}

/// The read-only token for `scope`: the stored one while it's usable,
/// otherwise a new one made with `admin`, the context's own client.
///
/// `rejected` is a token the cluster has just turned away (revoked, or
/// expired early): a stored token that's this one isn't used. A replaced
/// token is revoked, best effort, so they don't pile up in `relish token
/// list`.
///
/// Reusing a stored token reads only the file. Making one takes the lock
/// beside it first and looks again, so a server that waited for another
/// uses the token that one made.
pub async fn obtain(
    path: &Path,
    scope: &Scope,
    admin: &BunClient,
    now: u64,
    rejected: Option<&str>,
) -> Result<(Credential, Obtained), CredentialError> {
    let reusable = |stored: &StoredToken| {
        stored.usable(scope, now) && rejected.is_none_or(|rejected| stored.token != rejected)
    };
    if let Some(stored) = StoredToken::load(path)?.filter(reusable) {
        return Ok((Credential::ReadOnly(stored.token), Obtained::Reused));
    }
    let _lock = lock(path).await?;
    let stored = StoredToken::load(path)?;
    if let Some(stored) = stored.as_ref().filter(|stored| reusable(stored)) {
        return Ok((Credential::ReadOnly(stored.token.clone()), Obtained::Reused));
    }
    match bounded(admin.token_access()).await? {
        TokenAccess::Admin => {}
        TokenAccess::Lesser => return Ok((Credential::ContextNotAdmin, Obtained::NotNeeded)),
        TokenAccess::Rejected => return Err(CredentialError::ContextRejected),
    }
    let name = token_name(now);
    let request = TokenRequest {
        name: name.clone(),
        role: "read-only".to_string(),
        apps: None,
        namespaces: scope.namespace.clone().map(|namespace| vec![namespace]),
        lifetime: TokenLifetime::Days(TOKEN_TTL_DAYS),
        inherit_permissions: false,
    };
    let issued = bounded(admin.token_create(&request)).await?;
    let fresh = StoredToken {
        schema: 1,
        owner: scope.owner.clone(),
        endpoint: scope.endpoint.clone(),
        namespace: scope.namespace.clone(),
        name,
        token: issued.token.clone(),
        expires_at: issued.expires_at,
    };
    fresh.save(path)?;
    // Revoked by name, so only a token made at this cluster: a name from
    // another one could be someone else's here.
    let replaced = stored.filter(|old| {
        old.owner == scope.owner && old.endpoint == scope.endpoint && old.name != fresh.name
    });
    if let Some(old) = replaced {
        let _ = tokio::time::timeout(CLUSTER_TIMEOUT, admin.token_revoke(&old.name)).await;
    }
    Ok((Credential::ReadOnly(issued.token), Obtained::Minted))
}

/// Whether the cluster still accepts `token`, asked with the same cheap
/// admin-only read as the role probe: `Some(false)` for a 401, `Some(true)`
/// for any other verdict, `None` when there's no answer.
pub async fn accepted(admin: &BunClient, token: &str) -> Option<bool> {
    let probe = admin.with_token(token);
    match tokio::time::timeout(CLUSTER_TIMEOUT, probe.token_access()).await {
        Ok(Ok(TokenAccess::Rejected)) => Some(false),
        Ok(Ok(TokenAccess::Admin | TokenAccess::Lesser)) => Some(true),
        Ok(Err(_)) | Err(_) => None,
    }
}

/// Take the stored token out: revoke it, best effort, and delete the file
/// and its lock. Returns whether there was one.
pub async fn remove(path: &Path, admin: &BunClient) -> Result<bool, CredentialError> {
    let lock = lock(path).await?;
    let Some(stored) = StoredToken::load(path)? else {
        return Ok(false);
    };
    let _ = tokio::time::timeout(CLUSTER_TIMEOUT, admin.token_revoke(&stored.name)).await;
    let storage = |path: &Path, error: std::io::Error| CredentialError::Storage {
        path: path.to_path_buf(),
        reason: error.to_string(),
    };
    std::fs::remove_file(path).map_err(|error| storage(path, error))?;
    let lock_path = lock_path(path);
    lock.remove(&lock_path)
        .map_err(|error| storage(&lock_path, error))?;
    Ok(true)
}

/// The lock file beside the token file `path`.
fn lock_path(path: &Path) -> PathBuf {
    path.with_extension("lock")
}

/// Take the lock beside `path`, waiting for another server holding it.
async fn lock(path: &Path) -> Result<FileLock, CredentialError> {
    let lock_path = lock_path(path);
    let storage = {
        let lock_path = lock_path.clone();
        move |reason: String| CredentialError::Storage {
            path: lock_path.clone(),
            reason,
        }
    };
    let waiting = storage.clone();
    // `lock_within` waits by polling on its thread, so not on a worker.
    tokio::task::spawn_blocking(move || lock_blocking(&lock_path))
        .await
        .map_err(|error| storage(error.to_string()))?
        .map_err(|error| match error {
            FileLockError::Busy => waiting(format!(
                "another relish mcp has held it for {} s",
                LOCK_TIMEOUT.as_secs()
            )),
            FileLockError::Io(error) => waiting(error.to_string()),
        })
}

fn lock_blocking(lock_path: &Path) -> Result<FileLock, FileLockError> {
    create_directory(lock_path)?;
    // `remove` deletes the lock file while holding it; a waiter that had
    // opened the old one then finds it unlinked and opens the new one.
    loop {
        let mut options = std::fs::OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let lock = FileLock::lock_within(options.open(lock_path)?, LOCK_TIMEOUT)?;
        if lock.still_names(lock_path)? {
            return Ok(lock);
        }
    }
}

/// One cluster request, bounded, with its failure sorted into a refusal
/// (the cluster answered no) or no answer at all.
async fn bounded<T>(
    request: impl std::future::Future<Output = Result<T, RelishError>>,
) -> Result<T, CredentialError> {
    match tokio::time::timeout(CLUSTER_TIMEOUT, request).await {
        Ok(Ok(value)) => Ok(value),
        Ok(Err(RelishError::ApiError { status, body })) if status != 0 => {
            Err(CredentialError::Refused { status, body })
        }
        Ok(Err(error)) => Err(CredentialError::Unreachable(error.to_string())),
        Err(_) => Err(CredentialError::Unreachable(format!(
            "no answer within {} s",
            CLUSTER_TIMEOUT.as_secs()
        ))),
    }
}

/// Seconds since the Unix epoch.
pub fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests;
