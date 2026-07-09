//! WebSocket upgrade detection and proxying.
//!
//! Detects HTTP upgrade requests for WebSocket, forwards the upgrade
//! handshake to the backend, and if the backend responds with 101,
//! switches to raw bidirectional TCP proxying. Wrapper does not
//! inspect or modify WebSocket frames — it operates as a transparent
//! TCP proxy after the upgrade.

use std::net::SocketAddr;

use axum::body::Body;
use axum::http::{Request, StatusCode, header};
use axum::response::{IntoResponse, Response};
use hyper_util::rt::TokioIo;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// Cap on the backend's handshake response head we buffer before giving up.
const MAX_HANDSHAKE_HEAD: usize = 16 * 1024;

/// Check whether a request is a WebSocket upgrade.
///
/// A valid WebSocket upgrade has both `Connection: Upgrade` (or
/// containing "upgrade" as a token) and `Upgrade: websocket`.
pub fn is_websocket_upgrade(req: &Request<Body>) -> bool {
    let has_upgrade_connection = req
        .headers()
        .get(header::CONNECTION)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.to_lowercase().contains("upgrade"));

    let has_websocket_upgrade = req
        .headers()
        .get(header::UPGRADE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.eq_ignore_ascii_case("websocket"));

    has_upgrade_connection && has_websocket_upgrade
}

/// Handle a WebSocket upgrade by connecting to the backend, relaying the
/// handshake, and then transparently proxying bytes in both directions.
///
/// Forwards the backend's own `101` response (including its
/// `Sec-WebSocket-Accept` header) to the client, then splices the client's
/// upgraded connection to the backend socket with
/// [`tokio::io::copy_bidirectional`]. Returns `502 Bad Gateway` if the
/// backend refuses the upgrade or the connection fails.
pub async fn handle_websocket_upgrade(mut req: Request<Body>, backend: SocketAddr) -> Response {
    // Capture the client-side upgrade future before consuming the request.
    // It resolves to the raw client stream once our 101 is written.
    let client_upgrade = hyper::upgrade::on(&mut req);

    // Connect to the backend and relay the handshake as raw HTTP/1.1.
    let mut backend_stream = match TcpStream::connect(backend).await {
        Ok(s) => s,
        Err(_) => return StatusCode::BAD_GATEWAY.into_response(),
    };
    let upgrade_request = build_upgrade_request(&req, backend);
    if backend_stream
        .write_all(upgrade_request.as_bytes())
        .await
        .is_err()
    {
        return StatusCode::BAD_GATEWAY.into_response();
    }

    // Read the backend's handshake response head (up to the blank line).
    // `leftover` is any payload the backend sent past the head — it must be
    // forwarded to the client before the bidirectional splice begins.
    let (head, leftover) = match read_http_head(&mut backend_stream).await {
        Some(pair) => pair,
        None => return StatusCode::BAD_GATEWAY.into_response(),
    };

    let Some(handshake) = parse_handshake(&head) else {
        return StatusCode::BAD_GATEWAY.into_response();
    };
    if !handshake.is_switching_protocols {
        return StatusCode::BAD_GATEWAY.into_response();
    }

    // Rebuild the backend's 101 for the client, carrying its handshake
    // headers (crucially `Sec-WebSocket-Accept`) so the client accepts.
    let mut builder = Response::builder().status(StatusCode::SWITCHING_PROTOCOLS);
    for (name, value) in &handshake.headers {
        builder = builder.header(name, value);
    }
    let response = match builder.body(Body::empty()) {
        Ok(r) => r,
        Err(_) => return StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    };

    // Once the client connection upgrades, splice it to the backend.
    tokio::spawn(async move {
        let upgraded = match client_upgrade.await {
            Ok(u) => u,
            Err(_) => return,
        };
        let mut client_io = TokioIo::new(upgraded);
        if !leftover.is_empty() && client_io.write_all(&leftover).await.is_err() {
            return;
        }
        let _ = tokio::io::copy_bidirectional(&mut client_io, &mut backend_stream).await;
    });

    response
}

/// The parsed backend handshake response.
struct Handshake {
    is_switching_protocols: bool,
    headers: Vec<(String, String)>,
}

/// Parse a raw HTTP/1.1 response head into its status and end-to-end headers.
///
/// Drops hop-by-hop framing headers we regenerate (`Connection`/`Upgrade` are
/// kept — they're part of the WebSocket handshake the client must see).
fn parse_handshake(head: &str) -> Option<Handshake> {
    let mut lines = head.split("\r\n");
    let status_line = lines.next()?;
    let is_switching_protocols = status_line.starts_with("HTTP/1.1 101");

    let mut headers = Vec::new();
    for line in lines {
        if line.is_empty() {
            break;
        }
        let (name, value) = line.split_once(':')?;
        headers.push((name.trim().to_string(), value.trim().to_string()));
    }
    Some(Handshake {
        is_switching_protocols,
        headers,
    })
}

/// Read from `stream` until the end of the HTTP head (`\r\n\r\n`).
///
/// Returns the head as a string and any bytes read past it, or `None` on I/O
/// error, EOF before the head completes, or an oversized head.
async fn read_http_head(stream: &mut TcpStream) -> Option<(String, Vec<u8>)> {
    let mut buf = Vec::with_capacity(1024);
    let mut chunk = [0u8; 1024];
    loop {
        let n = stream.read(&mut chunk).await.ok()?;
        if n == 0 {
            return None; // EOF before the head completed
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(pos) = find_head_end(&buf) {
            let head = String::from_utf8_lossy(&buf[..pos]).into_owned();
            let leftover = buf[pos + 4..].to_vec();
            return Some((head, leftover));
        }
        if buf.len() > MAX_HANDSHAKE_HEAD {
            return None;
        }
    }
}

/// Index of the `\r\n\r\n` that terminates an HTTP head, if present.
fn find_head_end(buf: &[u8]) -> Option<usize> {
    buf.windows(4).position(|w| w == b"\r\n\r\n")
}

/// Build a raw HTTP/1.1 upgrade request string to send to the backend.
fn build_upgrade_request(req: &Request<Body>, backend: SocketAddr) -> String {
    let method = req.method().as_str();
    let path = req
        .uri()
        .path_and_query()
        .map(|pq| pq.as_str())
        .unwrap_or("/");

    let mut request = format!("{method} {path} HTTP/1.1\r\nHost: {backend}\r\n");

    for (name, value) in req.headers() {
        if name == header::HOST {
            continue; // Already set above
        }
        if let Ok(v) = value.to_str() {
            request.push_str(&format!("{}: {v}\r\n", name.as_str()));
        }
    }
    request.push_str("\r\n");
    request
}

/// Construct a WebSocket Close frame.
///
/// The frame format is:
/// - 1 byte: 0x88 (FIN + opcode 8 = Close)
/// - 1 byte: payload length (2 for just the status code)
/// - 2 bytes: status code in network byte order
///
/// Status 1001 = "Going Away" (used during connection draining).
pub fn build_close_frame(status: u16) -> Vec<u8> {
    vec![
        0x88,                  // FIN + opcode Close
        0x02,                  // payload length = 2
        (status >> 8) as u8,   // status high byte
        (status & 0xFF) as u8, // status low byte
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_ws_request() -> Request<Body> {
        Request::builder()
            .uri("/ws")
            .header(header::CONNECTION, "Upgrade")
            .header(header::UPGRADE, "websocket")
            .header("Sec-WebSocket-Key", "dGhlIHNhbXBsZSBub25jZQ==")
            .header("Sec-WebSocket-Version", "13")
            .body(Body::empty())
            .unwrap()
    }

    #[test]
    fn is_websocket_upgrade_detects_headers() {
        let req = make_ws_request();
        assert!(is_websocket_upgrade(&req));
    }

    #[test]
    fn rejects_missing_upgrade_header() {
        let req = Request::builder()
            .uri("/ws")
            .header(header::CONNECTION, "keep-alive")
            .body(Body::empty())
            .unwrap();
        assert!(!is_websocket_upgrade(&req));
    }

    #[test]
    fn rejects_missing_connection_upgrade() {
        let req = Request::builder()
            .uri("/ws")
            .header(header::UPGRADE, "websocket")
            .body(Body::empty())
            .unwrap();
        assert!(!is_websocket_upgrade(&req));
    }

    #[test]
    fn detects_case_insensitive_websocket() {
        let req = Request::builder()
            .uri("/ws")
            .header(header::CONNECTION, "upgrade")
            .header(header::UPGRADE, "WebSocket")
            .body(Body::empty())
            .unwrap();
        assert!(is_websocket_upgrade(&req));
    }

    #[test]
    fn detects_connection_with_multiple_values() {
        // Connection header can contain multiple tokens
        let req = Request::builder()
            .uri("/ws")
            .header(header::CONNECTION, "keep-alive, Upgrade")
            .header(header::UPGRADE, "websocket")
            .body(Body::empty())
            .unwrap();
        assert!(is_websocket_upgrade(&req));
    }

    #[test]
    fn close_frame_construction() {
        let frame = build_close_frame(1001);
        assert_eq!(frame.len(), 4);
        assert_eq!(frame[0], 0x88); // FIN + Close opcode
        assert_eq!(frame[1], 0x02); // payload length
        // 1001 = 0x03E9
        assert_eq!(frame[2], 0x03); // high byte
        assert_eq!(frame[3], 0xE9); // low byte
    }

    #[test]
    fn close_frame_going_away() {
        let frame = build_close_frame(1001);
        let status = u16::from_be_bytes([frame[2], frame[3]]);
        assert_eq!(status, 1001);
    }

    #[test]
    fn build_upgrade_request_format() {
        let req = make_ws_request();
        let backend: SocketAddr = "10.0.2.2:8080".parse().unwrap();
        let raw = build_upgrade_request(&req, backend);

        assert!(raw.starts_with("GET /ws HTTP/1.1\r\n"));
        assert!(raw.contains("Host: 10.0.2.2:8080\r\n"));
        assert!(raw.contains("upgrade: websocket\r\n"));
        assert!(raw.contains("sec-websocket-key: dGhlIHNhbXBsZSBub25jZQ==\r\n"));
        assert!(raw.ends_with("\r\n\r\n"));
    }
}
