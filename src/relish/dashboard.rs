//! Read-only browser access to the authenticated Bun dashboard.

use std::net::SocketAddr;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{Method, StatusCode, header};
use axum::response::{IntoResponse, Response};
use futures_util::StreamExt;
use rand::RngCore;
use tokio_util::sync::CancellationToken;

use super::{RelishError, client::BunClient};

#[derive(Clone)]
struct DashboardSession {
    client: BunClient,
    authority: String,
    origin: String,
    launch_token: String,
    session_token: String,
    cookie_name: String,
    launch_used: Arc<AtomicBool>,
    shutdown: CancellationToken,
}

impl DashboardSession {
    fn new(client: BunClient, address: SocketAddr) -> Self {
        let token = || {
            let mut bytes = [0u8; 32];
            rand::rngs::OsRng.fill_bytes(&mut bytes);
            hex::encode(bytes)
        };
        Self {
            client,
            authority: address.to_string(),
            origin: format!("http://{address}"),
            launch_token: token(),
            session_token: token(),
            cookie_name: format!("rb_dashboard_{}", address.port()),
            launch_used: Arc::new(AtomicBool::new(false)),
            shutdown: CancellationToken::new(),
        }
    }
}

fn router(session: DashboardSession) -> Router {
    Router::new().fallback(proxy).with_state(session)
}

async fn proxy(State(session): State<DashboardSession>, request: Request) -> Response {
    let headers = request.headers();
    if request.uri().authority().is_some()
        || headers
            .get(header::HOST)
            .and_then(|value| value.to_str().ok())
            != Some(session.authority.as_str())
        || headers
            .get(header::ORIGIN)
            .is_some_and(|value| value.as_bytes() != session.origin.as_bytes())
        || headers
            .get("sec-fetch-site")
            .is_some_and(|value| value == "cross-site")
    {
        return (
            StatusCode::FORBIDDEN,
            "dashboard requests must originate on its loopback address",
        )
            .into_response();
    }
    if !matches!(*request.method(), Method::GET | Method::HEAD) {
        return (
            StatusCode::METHOD_NOT_ALLOWED,
            "the browser dashboard connection is read-only; use Relish for changes",
        )
            .into_response();
    }
    let launch_path = format!("/_reliaburger/open/{}", session.launch_token);
    if request.uri().path() == launch_path {
        if request.method() != Method::GET || session.launch_used.swap(true, Ordering::AcqRel) {
            return StatusCode::FORBIDDEN.into_response();
        }
        let cookie = format!(
            "{}={}; HttpOnly; SameSite=Strict; Path=/",
            session.cookie_name, session.session_token
        );
        return (
            StatusCode::SEE_OTHER,
            [
                (header::SET_COOKIE, cookie),
                (header::LOCATION, "/".to_string()),
                (header::CACHE_CONTROL, "no-store".to_string()),
                (header::REFERRER_POLICY, "no-referrer".to_string()),
            ],
        )
            .into_response();
    }
    let expected = format!("{}={}", session.cookie_name, session.session_token);
    let authenticated = headers
        .get(header::COOKIE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|cookie| cookie.split(';').any(|part| part.trim() == expected));
    if !authenticated {
        return (
            StatusCode::FORBIDDEN,
            "open the link printed by relish dashboard",
        )
            .into_response();
    }
    // Concatenation keeps every request on the configured Bun endpoint;
    // joining a URL supplied by the browser could replace its authority.
    let target = format!("{}{}", session.client.base_url(), request.uri());
    let client = match session.client.http() {
        Ok(client) => client,
        Err(error) => return (StatusCode::BAD_GATEWAY, error.to_string()).into_response(),
    };
    let mut upstream = client.request(request.method().clone(), target);
    if let Some(accept) = headers.get(header::ACCEPT) {
        upstream = upstream.header(header::ACCEPT, accept);
    }
    // Browser cookies and Authorization never reach Bun. The configured
    // client's bearer remains in this process and its CA-pinned connection.
    let response = match tokio::time::timeout(Duration::from_secs(15), upstream.send()).await {
        Ok(Ok(response)) => response,
        Ok(Err(error)) => return (StatusCode::BAD_GATEWAY, error.to_string()).into_response(),
        Err(_) => {
            return (
                StatusCode::GATEWAY_TIMEOUT,
                "Bun did not answer the dashboard request",
            )
                .into_response();
        }
    };
    if response.status().is_redirection() {
        return (
            StatusCode::BAD_GATEWAY,
            "Bun redirected the dashboard request; check the endpoint and credentials",
        )
            .into_response();
    }
    let status = response.status();
    let content_type = response.headers().get(header::CONTENT_TYPE).cloned();
    let body = Body::from_stream(
        response
            .bytes_stream()
            .take_until(session.shutdown.cancelled_owned()),
    );
    let mut response = (status, body).into_response();
    if let Some(value) = content_type {
        response.headers_mut().insert(header::CONTENT_TYPE, value);
    }
    for (name, value) in [
        (header::CACHE_CONTROL, "no-store"),
        (header::REFERRER_POLICY, "no-referrer"),
        (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
        (header::X_FRAME_OPTIONS, "DENY"),
    ] {
        response
            .headers_mut()
            .insert(name, axum::http::HeaderValue::from_static(value));
    }
    response
}

/// Open a temporary, read-only loopback dashboard using the current CLI context.
/// The one-use browser link exchanges an ephemeral nonce for a private cookie.
pub async fn run(port: u16, no_open: bool) -> Result<(), RelishError> {
    let client = BunClient::default_local();
    client.health().await?;
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await?;
    let session = DashboardSession::new(client, listener.local_addr()?);
    let url = format!(
        "{}/_reliaburger/open/{}",
        session.origin, session.launch_token
    );
    println!("dashboard: {url}\nread-only browser session; Ctrl-C to stop");
    if !no_open {
        #[cfg(target_os = "macos")]
        let program = "open";
        #[cfg(not(target_os = "macos"))]
        let program = "xdg-open";
        if let Err(error) = tokio::process::Command::new(program)
            .arg(&url)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
        {
            eprintln!("could not open the browser: {error}; use the printed link");
        }
    }
    let shutdown = session.shutdown.clone();
    let signal = shutdown.clone();
    let watcher = tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        signal.cancel();
    });
    let result = axum::serve(listener, router(session))
        .with_graceful_shutdown(shutdown.clone().cancelled_owned())
        .await;
    shutdown.cancel();
    watcher.abort();
    result?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    #[tokio::test]
    async fn proxy_uses_saved_credentials_streams_content_and_refuses_redirects() {
        use http_body_util::BodyExt;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let upstream = axum::Router::new()
            .route(
                "/ui/test",
                axum::routing::get(|headers: axum::http::HeaderMap| async move {
                    assert_eq!(headers["authorization"], "Bearer upstream-only");
                    assert!(!headers.contains_key("cookie"));
                    (
                        [(header::CONTENT_TYPE, "text/html")],
                        "<p>dashboard works</p>",
                    )
                }),
            )
            .route(
                "/redirect",
                axum::routing::get(|| async {
                    (
                        StatusCode::FOUND,
                        [(header::LOCATION, "http://127.0.0.1:9/")],
                    )
                }),
            );
        let server = tokio::spawn(async move {
            axum::serve(listener, upstream).await.unwrap();
        });
        let session = DashboardSession::new(
            BunClient::new_with_token(&format!("http://{address}"), Some("upstream-only")),
            "127.0.0.1:18117".parse().unwrap(),
        );
        let cookie = format!("{}={}", session.cookie_name, session.session_token);
        let app = router(session);
        let request = |path: &str| {
            Request::builder()
                .uri(path)
                .header("host", "127.0.0.1:18117")
                .header("cookie", &cookie)
                .header("authorization", "Bearer browser-supplied")
                .body(Body::empty())
                .unwrap()
        };
        let response = app.clone().oneshot(request("/ui/test")).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(response.headers()["referrer-policy"], "no-referrer");
        let body = response.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(&body[..], b"<p>dashboard works</p>");
        assert_eq!(
            app.oneshot(request("/redirect")).await.unwrap().status(),
            StatusCode::BAD_GATEWAY
        );
        server.abort();
    }

    fn session() -> DashboardSession {
        DashboardSession::new(
            super::super::client::BunClient::new_with_token(
                "http://127.0.0.1:9",
                Some("upstream-only"),
            ),
            "127.0.0.1:18117".parse().unwrap(),
        )
    }

    #[tokio::test]
    async fn browser_launch_is_single_use_and_sets_a_private_session_cookie() {
        let session = session();
        let path = format!("/_reliaburger/open/{}", session.launch_token);
        let app = router(session);
        let request = || {
            Request::builder()
                .uri(&path)
                .header("host", "127.0.0.1:18117")
                .body(Body::empty())
                .unwrap()
        };
        let response = app.clone().oneshot(request()).await.unwrap();
        assert_eq!(response.status(), StatusCode::SEE_OTHER);
        let cookie = response.headers()["set-cookie"].to_str().unwrap();
        assert!(cookie.contains("HttpOnly"));
        assert!(cookie.contains("SameSite=Strict"));
        assert!(!cookie.contains("upstream-only"));
        assert_eq!(
            app.oneshot(request()).await.unwrap().status(),
            StatusCode::FORBIDDEN
        );
    }

    #[tokio::test]
    async fn dashboard_rejects_missing_sessions_rebinding_origins_and_mutations() {
        let session = session();
        let cookie = format!("{}={}", session.cookie_name, session.session_token);
        let app = router(session);
        for (method, host, origin, auth, expected) in [
            ("GET", "127.0.0.1:18117", None, false, StatusCode::FORBIDDEN),
            (
                "GET",
                "attacker.example:18117",
                None,
                true,
                StatusCode::FORBIDDEN,
            ),
            (
                "GET",
                "127.0.0.1:18117",
                Some("https://attacker.example"),
                true,
                StatusCode::FORBIDDEN,
            ),
            (
                "POST",
                "127.0.0.1:18117",
                None,
                true,
                StatusCode::METHOD_NOT_ALLOWED,
            ),
        ] {
            let mut request = Request::builder()
                .method(method)
                .uri("/")
                .header("host", host);
            if auth {
                request = request.header("cookie", &cookie);
            }
            if let Some(origin) = origin {
                request = request.header("origin", origin);
            }
            assert_eq!(
                app.clone()
                    .oneshot(request.body(Body::empty()).unwrap())
                    .await
                    .unwrap()
                    .status(),
                expected
            );
        }
    }
}
