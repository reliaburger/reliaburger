//! A loopback registry that serves blobs slowly, for tests that must
//! interrupt a real cold image pull partway through (#740).
//!
//! It sits in front of the pinned-image mirror (`make test-linux` serves
//! one; see `src/testkit/pinned_images.rs`), the way a mirror sits in front
//! of a public registry. Manifests pass at full speed, so a pull resolves
//! its image straight away. Blob bodies trickle out at a fixed rate, so the
//! pull then spends seconds fetching its layers, and a test can cancel it,
//! or let its deadline expire, while bytes are still arriving. The bytes
//! are the mirror's own, so the client still verifies every digest.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

use axum::body::{Body, Bytes};
use axum::extract::{Request, State};
use axum::http::{Method, StatusCode, header};
use axum::response::Response;
use futures_util::StreamExt;
use tokio_util::sync::CancellationToken;

/// Bytes sent per write. Each one waits its share of the rate first.
const CHUNK: usize = 8 << 10;

/// A running slow registry. Dropping it stops the server.
pub struct SlowRegistry {
    /// `host:port` to name as the mirror.
    pub address: String,
    shared: Arc<Shared>,
    shutdown: CancellationToken,
}

struct Shared {
    upstream: String,
    client: reqwest::Client,
    bytes_per_second: u64,
    /// Blob bytes sent so far, across every response.
    blob_bytes: AtomicU64,
    /// Blob responses still sending their body.
    blobs_in_flight: AtomicUsize,
}

/// Counts one blob response as in flight until its body ends or is dropped.
struct InFlight(Arc<Shared>);

impl Drop for InFlight {
    fn drop(&mut self) {
        self.0.blobs_in_flight.fetch_sub(1, Ordering::SeqCst);
    }
}

impl Drop for SlowRegistry {
    fn drop(&mut self) {
        self.shutdown.cancel();
    }
}

impl SlowRegistry {
    /// Serve `upstream` (a plain-HTTP registry's `host:port`) on loopback,
    /// with each blob body limited to `bytes_per_second`.
    pub async fn start(upstream: String, bytes_per_second: u64) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap().to_string();
        let shared = Arc::new(Shared {
            upstream,
            client: reqwest::Client::builder().no_proxy().build().unwrap(),
            bytes_per_second: bytes_per_second.max(1),
            blob_bytes: AtomicU64::new(0),
            blobs_in_flight: AtomicUsize::new(0),
        });
        let app = axum::Router::new()
            .fallback(forward)
            .with_state(Arc::clone(&shared));
        let shutdown = CancellationToken::new();
        let stop = shutdown.clone();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app)
                .with_graceful_shutdown(stop.cancelled_owned())
                .await;
        });
        Self {
            address,
            shared,
            shutdown,
        }
    }

    /// Blob bytes sent so far.
    pub fn blob_bytes(&self) -> u64 {
        self.shared.blob_bytes.load(Ordering::SeqCst)
    }

    /// Blob responses that have started and not yet finished sending.
    pub fn blobs_in_flight(&self) -> usize {
        self.shared.blobs_in_flight.load(Ordering::SeqCst)
    }

    /// Wait until more than `bytes` of blob data has gone out, so a pull is
    /// past its manifests and config and into a layer.
    pub async fn wait_for_blob_bytes(&self, bytes: u64, within: Duration) {
        tokio::time::timeout(within, async {
            while self.blob_bytes() <= bytes {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .unwrap_or_else(|_| {
            panic!(
                "the slow registry sent only {} blob bytes in {within:?}",
                self.blob_bytes()
            )
        });
    }
}

/// Answer one request from the upstream registry: manifests and the version
/// check at full speed, blob bodies at the configured rate.
async fn forward(State(shared): State<Arc<Shared>>, request: Request) -> Response {
    let path = request.uri().path().to_owned();
    let method = request.method().clone();
    let mut upstream = shared
        .client
        .request(method.clone(), format!("http://{}{path}", shared.upstream));
    if let Some(accept) = request.headers().get(header::ACCEPT) {
        upstream = upstream.header(header::ACCEPT, accept);
    }
    let Ok(answer) = upstream.send().await else {
        return status(StatusCode::BAD_GATEWAY);
    };
    let mut response = Response::builder().status(answer.status().as_u16());
    for name in [
        header::CONTENT_TYPE,
        header::CONTENT_LENGTH,
        "docker-content-digest".parse().unwrap(),
    ] {
        if let Some(value) = answer.headers().get(&name) {
            response = response.header(name, value.as_bytes());
        }
    }
    let blob = path.contains("/blobs/") && answer.status().is_success();
    if method == Method::HEAD {
        return response.body(Body::empty()).unwrap();
    }
    if !blob {
        let body = answer.bytes().await.unwrap_or_default();
        return response.body(Body::from(body)).unwrap();
    }
    shared.blobs_in_flight.fetch_add(1, Ordering::SeqCst);
    let in_flight = InFlight(Arc::clone(&shared));
    let pause = Duration::from_secs_f64(CHUNK as f64 / shared.bytes_per_second as f64);
    // Split whatever the upstream sends into fixed chunks and pace them.
    // The guard rides in the stream, so a client that hangs up mid-body
    // ends the response's in-flight count as surely as a finished one.
    let chunks = answer.bytes_stream().flat_map(|piece| {
        let pieces: Vec<Result<Bytes, std::io::Error>> = match piece {
            Ok(bytes) => bytes
                .chunks(CHUNK)
                .map(|chunk| Ok(Bytes::copy_from_slice(chunk)))
                .collect(),
            Err(error) => vec![Err(std::io::Error::other(error))],
        };
        futures_util::stream::iter(pieces)
    });
    let paced = chunks.then(move |chunk| {
        let shared = Arc::clone(&in_flight.0);
        async move {
            tokio::time::sleep(pause).await;
            if let Ok(bytes) = &chunk {
                shared
                    .blob_bytes
                    .fetch_add(bytes.len() as u64, Ordering::SeqCst);
            }
            chunk
        }
    });
    response.body(Body::from_stream(paced)).unwrap()
}

fn status(code: StatusCode) -> Response {
    let mut response = Response::new(Body::empty());
    *response.status_mut() = code;
    response
}
