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

use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};

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

/// Why `relish mcp` has no read-only token to send.
#[derive(Debug, thiserror::Error)]
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
    #[error("couldn't reach the cluster to get a read-only token for relish mcp: {0}")]
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
        if let Some(parent) = path.parent() {
            let mut directory = std::fs::DirBuilder::new();
            directory.recursive(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                directory.mode(0o700);
            }
            directory
                .create(parent)
                .map_err(|error| storage(error.to_string()))?;
        }
        let bytes = serde_json::to_vec_pretty(self).map_err(|error| storage(error.to_string()))?;
        crate::sesame::identity::atomic_write_mode(path, &bytes, Some(0o600))
            .map_err(|error| storage(error.to_string()))
    }
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
/// `renew` skips the stored token, for when the cluster has just rejected
/// it (revoked, or expired early). A replaced token is revoked, best effort,
/// so they don't pile up in `relish token list`.
pub async fn obtain(
    path: &Path,
    scope: &Scope,
    admin: &BunClient,
    now: u64,
    renew: bool,
) -> Result<(Credential, Obtained), CredentialError> {
    let stored = StoredToken::load(path)?;
    if !renew && let Some(stored) = stored.as_ref().filter(|stored| stored.usable(scope, now)) {
        return Ok((Credential::ReadOnly(stored.token.clone()), Obtained::Reused));
    }
    match bounded(admin.token_access()).await? {
        TokenAccess::Admin => {}
        TokenAccess::Lesser => return Ok((Credential::ContextNotAdmin, Obtained::NotNeeded)),
        TokenAccess::Rejected => return Err(CredentialError::ContextRejected),
    }
    let name = format!("relish-mcp-{now}-{}", std::process::id());
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
    if let Some(old) = stored.filter(|old| old.owner == scope.owner && old.name != fresh.name) {
        let _ = tokio::time::timeout(CLUSTER_TIMEOUT, admin.token_revoke(&old.name)).await;
    }
    Ok((Credential::ReadOnly(issued.token), Obtained::Minted))
}

/// Take the stored token out: revoke it, best effort, and delete the file.
/// Returns whether there was one.
pub async fn remove(path: &Path, admin: &BunClient) -> Result<bool, CredentialError> {
    let Some(stored) = StoredToken::load(path)? else {
        return Ok(false);
    };
    let _ = tokio::time::timeout(CLUSTER_TIMEOUT, admin.token_revoke(&stored.name)).await;
    std::fs::remove_file(path).map_err(|error| CredentialError::Storage {
        path: path.to_path_buf(),
        reason: error.to_string(),
    })?;
    Ok(true)
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
