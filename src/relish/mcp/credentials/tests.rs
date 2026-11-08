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
                    let reader = bearer == READER || bearer.starts_with("rbrg_minted_");
                    match (method, uri.path(), admin) {
                        (_, _, false) if reader => {
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
    let (credential, obtained) = obtain(&path, &scope(None), &cluster.client(ADMIN), now(), None)
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
        None,
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
    let (credential, obtained) = obtain(&path, &scope(None), &cluster.client(ADMIN), now(), None)
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
    let (credential, obtained) = obtain(&path, &scope(None), &cluster.client(READER), now(), None)
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
    let (credential, obtained) = obtain(&path, &scope(None), &cluster.client(ADMIN), now(), None)
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
        let (_, obtained) = obtain(&path, &scope(None), &cluster.client(ADMIN), now(), None)
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
    let (credential, obtained) = obtain(
        &path,
        &scope(None),
        &cluster.client(ADMIN),
        now(),
        Some("rbrg_revoked"),
    )
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
    let error = obtain(&path, &scope(None), &cluster.client(ADMIN), now(), None)
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
    let error = obtain(&path, &scope(None), &admin, now(), None)
        .await
        .err()
        .expect("no cluster, no token");
    assert!(matches!(error, CredentialError::Unreachable(_)), "{error}");
    // #1763 review: name the fallback, as a refusal does.
    let message = error.to_string();
    assert!(
        message.contains("relish token create --name agent --role read-only --ttl-days 1"),
        "{message}"
    );
    assert!(message.contains("RELIABURGER_TOKEN"), "{message}");
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

// ---------------------------------------------------------------------------
// Follow-ups from the live-cluster review of #1763.

/// Revoked or rotated behind relish mcp's back, a stored token still looks
/// usable on disk; only the cluster can say it's dead.
#[tokio::test]
async fn the_cluster_says_whether_it_still_accepts_a_token() {
    let cluster = Cluster::start(false).await;
    let admin = cluster.client(ADMIN);
    assert_eq!(accepted(&admin, "rbrg_minted_7").await, Some(true));
    assert_eq!(accepted(&admin, "rbrg_revoked").await, Some(false));
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    drop(listener);
    let nowhere = BunClient::new_with_token(&format!("http://{address}"), Some(ADMIN));
    assert_eq!(accepted(&nowhere, "rbrg_minted_7").await, None, "can't say");
}

/// Two servers saw the same token rejected; the second to get the lock
/// finds the first one's new token and uses it.
#[tokio::test]
async fn renewing_keeps_a_token_another_server_has_already_renewed() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("mcp-token.json");
    stored("relish-mcp-new", "rbrg_renewed", Some(now() + 3_600))
        .save(&path)
        .unwrap();
    let cluster = Cluster::start(false).await;
    let (credential, obtained) = obtain(
        &path,
        &scope(None),
        &cluster.client(ADMIN),
        now(),
        Some("rbrg_revoked"),
    )
    .await
    .unwrap();
    assert_eq!(obtained, Obtained::Reused);
    assert_eq!(read_only(&credential), "rbrg_renewed");
    assert!(cluster.creations().await.is_empty());
}

/// #1763 review: three `relish mcp` processes starting together each made
/// a token, the file kept one and the other two were orphaned.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn servers_starting_together_make_one_token_between_them() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("mcp-token.json");
    let cluster = Arc::new(Cluster::start(false).await);
    let tasks: Vec<_> = (0..3)
        .map(|_| {
            let path = path.clone();
            let cluster = cluster.clone();
            tokio::spawn(async move {
                let (credential, _) =
                    obtain(&path, &scope(None), &cluster.client(ADMIN), now(), None)
                        .await
                        .unwrap();
                read_only(&credential).to_string()
            })
        })
        .collect();
    let mut tokens = Vec::new();
    for task in tasks {
        tokens.push(task.await.unwrap());
    }
    assert_eq!(cluster.creations().await.len(), 1, "one token made");
    assert!(
        tokens.iter().all(|token| token == "rbrg_minted_0"),
        "{tokens:?}"
    );
}

/// The revoke of a replaced token names it by name alone, so a token from
/// another endpoint (another cluster under the same context owner) is left
/// alone: its name could be someone else's here.
#[tokio::test]
async fn a_replaced_token_from_another_endpoint_is_not_revoked_here() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("mcp-token.json");
    let mut elsewhere = stored(
        "relish-mcp-elsewhere",
        "rbrg_elsewhere",
        Some(now() + 3_600),
    );
    elsewhere.endpoint = "https://10.0.0.9:19117".to_string();
    elsewhere.save(&path).unwrap();
    let cluster = Cluster::start(false).await;
    let (_, obtained) = obtain(&path, &scope(None), &cluster.client(ADMIN), now(), None)
        .await
        .unwrap();
    assert_eq!(obtained, Obtained::Minted);
    let revoked: Vec<_> = cluster
        .seen()
        .await
        .into_iter()
        .filter(|seen| seen.path == "/v1/token/revoke")
        .collect();
    assert!(revoked.is_empty(), "{revoked:?}");
}

/// `--remove` must find the namespace files too.
#[test]
fn every_token_file_beside_the_context_is_found() {
    let directory = tempfile::tempdir().unwrap();
    let context = directory.path().join("context.json");
    for name in [
        "context.json",
        "mcp-token.json",
        "mcp-token-payments.json",
        "mcp-token-ops-2.json",
        "mcp-token-Bad.json",
        "mcp-token-.json",
        "mcp-token.lock",
        "other.json",
    ] {
        std::fs::write(directory.path().join(name), "{}").unwrap();
    }
    let found: Vec<String> = files_beside(&context)
        .unwrap()
        .iter()
        .map(|path| path.file_name().unwrap().to_string_lossy().into_owned())
        .collect();
    assert_eq!(
        found,
        [
            "mcp-token-ops-2.json",
            "mcp-token-payments.json",
            "mcp-token.json"
        ]
    );
    let empty = tempfile::tempdir().unwrap();
    assert!(
        files_beside(&empty.path().join("context.json"))
            .unwrap()
            .is_empty()
    );
}

/// `relish agent setup` previews the file: its JSON as written, with the
/// secret replaced.
#[test]
fn a_token_files_preview_is_its_json_with_the_secret_redacted() {
    let held = stored("relish-mcp-1", "rbrg_the_secret", Some(1_900_000_000));
    for preview in [
        shape(&scope(None), Some(&held), now()),
        shape(&scope(None), None, now()),
    ] {
        assert!(!preview.contains("rbrg_the_secret"), "{preview}");
        let json: serde_json::Value = serde_json::from_str(&preview).unwrap();
        assert_eq!(json["token"], crate::redact::REDACTED, "{preview}");
        assert_eq!(json["owner"], "laptop");
        let keys: Vec<&String> = json.as_object().unwrap().keys().collect();
        assert_eq!(
            keys,
            [
                "endpoint",
                "expires_at",
                "name",
                "namespace",
                "owner",
                "schema",
                "token"
            ]
        );
    }
    let at = now();
    let fresh: serde_json::Value = serde_json::from_str(&shape(&scope(None), None, at)).unwrap();
    assert!(
        fresh["name"].as_str().unwrap().starts_with("relish-mcp-"),
        "{fresh}"
    );
    assert_eq!(fresh["expires_at"], at + TOKEN_TTL_DAYS * 86_400);
}

/// The tokens relish mcp makes live a day and renew themselves, so the
/// expiry warning skips them by their name.
#[test]
fn relish_mcp_tokens_are_known_by_their_name() {
    assert!(made_by_relish_mcp("relish-mcp-1762000000-4242"));
    assert!(!made_by_relish_mcp("agent"));
    assert!(!made_by_relish_mcp("my-relish-mcp-1"));
}
