//! `relish mcp`'s read-only token against a stand-in cluster API.

use std::sync::Arc;

use axum::http::{HeaderMap, Method, StatusCode, Uri};
use tokio::sync::Mutex;

use super::*;

/// One request the stand-in saw: method, path, bearer and body.
#[derive(Debug, Clone)]
struct Seen {
    method: Method,
    path: String,
    bearer: String,
    body: serde_json::Value,
}

/// A cluster API that knows an admin bearer and a read-only one, and can be
/// told to refuse token creation.
struct Cluster {
    url: String,
    seen: Arc<Mutex<Vec<Seen>>>,
}

const ADMIN: &str = "rbrg_context_admin";
const READER: &str = "rbrg_context_reader";

impl Cluster {
    async fn start(refuse_creation: bool) -> Cluster {
        let seen: Arc<Mutex<Vec<Seen>>> = Arc::default();
        let minted = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let record = seen.clone();
        let app = axum::Router::new().fallback(
            move |method: Method, uri: Uri, headers: HeaderMap, body: String| {
                let record = record.clone();
                let minted = minted.clone();
                async move {
                    let bearer = headers
                        .get(axum::http::header::AUTHORIZATION)
                        .and_then(|value| value.to_str().ok())
                        .and_then(|value| value.strip_prefix("Bearer "))
                        .unwrap_or_default()
                        .to_string();
                    let body: serde_json::Value =
                        serde_json::from_str(&body).unwrap_or(serde_json::Value::Null);
                    record.lock().await.push(Seen {
                        method: method.clone(),
                        path: uri.path().to_string(),
                        bearer: bearer.clone(),
                        body,
                    });
                    let admin = bearer == ADMIN;
                    match (method, uri.path(), admin) {
                        (_, _, false) if bearer == READER => {
                            (StatusCode::FORBIDDEN, "requires admin role".to_string())
                        }
                        (_, _, false) => (StatusCode::UNAUTHORIZED, "invalid token".to_string()),
                        (Method::GET, "/v1/token/list", true) => {
                            (StatusCode::OK, r#"{"tokens":[]}"#.to_string())
                        }
                        (Method::POST, "/v1/token/create", true) if refuse_creation => (
                            StatusCode::FORBIDDEN,
                            r#"{"error":"policy forbids new tokens"}"#.to_string(),
                        ),
                        (Method::POST, "/v1/token/create", true) => {
                            let n = minted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                            (
                                StatusCode::OK,
                                format!(
                                    r#"{{"token":"rbrg_minted_{n}","expires_at":{}}}"#,
                                    now() + 86_400
                                ),
                            )
                        }
                        (Method::POST, "/v1/token/revoke", true) => {
                            (StatusCode::OK, r#"{"message":"revoked"}"#.to_string())
                        }
                        _ => (StatusCode::NOT_FOUND, "no such thing".to_string()),
                    }
                }
            },
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        Cluster {
            url: format!("http://{address}"),
            seen,
        }
    }

    fn client(&self, token: &str) -> BunClient {
        BunClient::new_with_token(&self.url, Some(token))
    }

    async fn seen(&self) -> Vec<Seen> {
        self.seen.lock().await.clone()
    }

    async fn creations(&self) -> Vec<Seen> {
        self.seen()
            .await
            .into_iter()
            .filter(|seen| seen.path == "/v1/token/create")
            .collect()
    }
}

fn scope(namespace: Option<&str>) -> Scope {
    Scope {
        owner: "laptop".to_string(),
        endpoint: "https://127.0.0.1:19117".to_string(),
        namespace: namespace.map(str::to_string),
    }
}

fn stored(name: &str, token: &str, expires_at: Option<u64>) -> StoredToken {
    StoredToken {
        schema: 1,
        owner: "laptop".to_string(),
        endpoint: "https://127.0.0.1:19117".to_string(),
        namespace: None,
        name: name.to_string(),
        token: token.to_string(),
        expires_at,
    }
}

fn read_only(credential: &Credential) -> &str {
    match credential {
        Credential::ReadOnly(token) => token,
        Credential::ContextNotAdmin => panic!("expected a read-only token"),
    }
}

#[cfg(unix)]
fn mode(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

#[tokio::test]
async fn an_admin_context_mints_a_scoped_read_only_token_with_a_ttl_and_stores_it() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("mcp-token.json");
    let cluster = Cluster::start(false).await;
    let (credential, obtained) = obtain(&path, &scope(None), &cluster.client(ADMIN), now(), false)
        .await
        .unwrap();
    assert_eq!(obtained, Obtained::Minted);
    assert_eq!(read_only(&credential), "rbrg_minted_0");

    let creations = cluster.creations().await;
    assert_eq!(creations.len(), 1);
    assert_eq!(creations[0].method, Method::POST);
    assert_eq!(creations[0].bearer, ADMIN, "made with the context's token");
    let body = &creations[0].body;
    assert_eq!(body["role"], "read-only");
    assert_eq!(body["ttl_days"], TOKEN_TTL_DAYS);
    assert_eq!(body["no_expiry"], false);
    assert!(
        body["name"].as_str().unwrap().starts_with("relish-mcp-"),
        "{body}"
    );

    let saved = StoredToken::load(&path).unwrap().unwrap();
    assert_eq!(saved.token, "rbrg_minted_0");
    assert!(saved.usable(&scope(None), now()));
    #[cfg(unix)]
    assert_eq!(mode(&path), 0o600, "the context's own mode");
}

#[tokio::test]
async fn a_namespace_confines_the_token_and_gets_its_own_file() {
    let directory = tempfile::tempdir().unwrap();
    let context = directory.path().join("context.json");
    let path = path_beside(&context, Some("payments")).unwrap();
    assert_eq!(path, directory.path().join("mcp-token-payments.json"));
    let cluster = Cluster::start(false).await;
    obtain(
        &path,
        &scope(Some("payments")),
        &cluster.client(ADMIN),
        now(),
        false,
    )
    .await
    .unwrap();
    assert_eq!(
        cluster.creations().await[0].body["namespaces"],
        serde_json::json!(["payments"])
    );
    for bad in ["../etc", "Payments", "", "-x", "a/b"] {
        assert!(
            matches!(
                path_beside(&context, Some(bad)),
                Err(CredentialError::BadNamespace(_))
            ),
            "{bad:?}"
        );
    }
}

#[tokio::test]
async fn a_usable_stored_token_is_reused_without_asking_the_cluster() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("mcp-token.json");
    stored("relish-mcp-1", "rbrg_kept", Some(now() + 3_600))
        .save(&path)
        .unwrap();
    let cluster = Cluster::start(false).await;
    let (credential, obtained) = obtain(&path, &scope(None), &cluster.client(ADMIN), now(), false)
        .await
        .unwrap();
    assert_eq!(obtained, Obtained::Reused);
    assert_eq!(read_only(&credential), "rbrg_kept");
    assert!(cluster.seen().await.is_empty(), "no round trip");
}

#[tokio::test]
async fn a_read_only_context_mints_nothing() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("mcp-token.json");
    let cluster = Cluster::start(false).await;
    let (credential, obtained) = obtain(&path, &scope(None), &cluster.client(READER), now(), false)
        .await
        .unwrap();
    assert_eq!(obtained, Obtained::NotNeeded);
    assert!(credential == Credential::ContextNotAdmin);
    assert!(cluster.creations().await.is_empty());
    assert!(!path.exists());
}

#[tokio::test]
async fn an_expired_stored_token_is_replaced_and_the_old_one_revoked() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("mcp-token.json");
    stored("relish-mcp-old", "rbrg_expired", Some(now() - 1))
        .save(&path)
        .unwrap();
    let cluster = Cluster::start(false).await;
    let (credential, obtained) = obtain(&path, &scope(None), &cluster.client(ADMIN), now(), false)
        .await
        .unwrap();
    assert_eq!(obtained, Obtained::Minted);
    assert_eq!(read_only(&credential), "rbrg_minted_0");
    assert_eq!(
        StoredToken::load(&path).unwrap().unwrap().token,
        "rbrg_minted_0"
    );
    let revoked: Vec<_> = cluster
        .seen()
        .await
        .into_iter()
        .filter(|seen| seen.path == "/v1/token/revoke")
        .map(|seen| seen.body["name"].clone())
        .collect();
    assert_eq!(revoked, [serde_json::json!("relish-mcp-old")]);
}

#[tokio::test]
async fn a_token_about_to_expire_or_from_another_cluster_is_not_reused() {
    let cluster = Cluster::start(false).await;
    let soon = stored("a", "rbrg_soon", Some(now() + 60));
    let mut elsewhere = stored("b", "rbrg_elsewhere", Some(now() + 3_600));
    elsewhere.owner = "another-cluster".to_string();
    for token in [soon, elsewhere] {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("mcp-token.json");
        token.save(&path).unwrap();
        let (_, obtained) = obtain(&path, &scope(None), &cluster.client(ADMIN), now(), false)
            .await
            .unwrap();
        assert_eq!(obtained, Obtained::Minted, "{}", token.token);
    }
}

/// A token the cluster rejected (revoked, say) is renewed, not swapped for
/// the admin token.
#[tokio::test]
async fn renewing_replaces_a_rejected_token_with_a_new_read_only_one() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("mcp-token.json");
    stored("relish-mcp-revoked", "rbrg_revoked", Some(now() + 3_600))
        .save(&path)
        .unwrap();
    let cluster = Cluster::start(false).await;
    let (credential, obtained) = obtain(&path, &scope(None), &cluster.client(ADMIN), now(), true)
        .await
        .unwrap();
    assert_eq!(obtained, Obtained::Minted);
    assert_eq!(read_only(&credential), "rbrg_minted_0");
}

#[tokio::test]
async fn a_cluster_that_refuses_token_creation_is_a_clear_error_and_stores_nothing() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("mcp-token.json");
    let cluster = Cluster::start(true).await;
    let error = obtain(&path, &scope(None), &cluster.client(ADMIN), now(), false)
        .await
        .err()
        .expect("a refusal is an error");
    assert!(
        matches!(error, CredentialError::Refused { status: 403, .. }),
        "{error}"
    );
    let message = error.to_string();
    assert!(message.contains("policy forbids new tokens"), "{message}");
    assert!(
        message.contains("won't use the context's admin token"),
        "{message}"
    );
    assert!(!path.exists());
}

#[tokio::test]
async fn an_unreachable_cluster_is_an_error_too() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("mcp-token.json");
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    drop(listener);
    let admin = BunClient::new_with_token(&format!("http://{address}"), Some(ADMIN));
    let error = obtain(&path, &scope(None), &admin, now(), false)
        .await
        .err()
        .expect("no cluster, no token");
    assert!(matches!(error, CredentialError::Unreachable(_)), "{error}");
}

#[test]
fn a_token_file_others_can_read_is_refused() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("mcp-token.json");
    stored("a", "rbrg_a", None).save(&path).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(StoredToken::load(&path).is_err());
    }
}

#[tokio::test]
async fn removing_revokes_the_token_and_deletes_the_file() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("mcp-token.json");
    stored("relish-mcp-gone", "rbrg_gone", None)
        .save(&path)
        .unwrap();
    let cluster = Cluster::start(false).await;
    assert!(remove(&path, &cluster.client(ADMIN)).await.unwrap());
    assert!(!path.exists());
    assert!(!remove(&path, &cluster.client(ADMIN)).await.unwrap());
    let revoked: Vec<_> = cluster
        .seen()
        .await
        .into_iter()
        .filter(|seen| seen.path == "/v1/token/revoke")
        .collect();
    assert_eq!(revoked.len(), 1);
}
