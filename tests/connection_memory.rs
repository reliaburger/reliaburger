//! #892: what an open API connection keeps on the heap after its answer.
//!
//! In the 0.2.0 soak the node that served the cluster's API sat at 600-800 MB
//! while its peers sat near 200 MB, and its memory tracked its open
//! connections at about 3 MB each. Lima's port forwarder keeps every
//! forwarded connection open until Bun's 75-second idle timeout, so that node
//! always had a couple of hundred. Each one kept a write buffer as big as the
//! largest answer it had sent: Hyper copied the whole body into it because
//! Bun's stream wrappers hid the socket's vectored writes, and it clears that
//! buffer between requests without shrinking it.
//!
//! This binary counts every heap byte through its own global allocator, so
//! the numbers are what the connections hold, not what the allocator keeps
//! mapped. The counters are process-wide, so each test needs its own
//! process, which is how nextest runs them (`make test`).

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use reliaburger::sesame::connection::{ConnectionTimeouts, serve_http_connection};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

struct Counting;

static LIVE: AtomicUsize = AtomicUsize::new(0);

// SAFETY: every call forwards to the system allocator with the caller's own
// layout and pointer; the counter is bookkeeping only.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: forwarded unchanged from our caller, who upholds `alloc`'s contract.
        let pointer = unsafe { System.alloc(layout) };
        if !pointer.is_null() {
            LIVE.fetch_add(layout.size(), Ordering::Relaxed);
        }
        pointer
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        // SAFETY: `pointer` came from `alloc` above with this same layout.
        unsafe { System.dealloc(pointer, layout) };
        LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// One answer's size: a big status, events or relayed answer.
const ANSWER_BYTES: usize = 4 * 1024 * 1024;

/// Kept-alive connections, each having sent one answer.
const CONNECTIONS: usize = 8;

/// What one idle connection may keep: TLS-free here, so Hyper's read buffer,
/// its connection state and the router's per-connection layers.
const PER_CONNECTION_CEILING: usize = 256 * 1024;

/// Send one keep-alive GET and read its whole answer, leaving the
/// connection open.
async fn get_and_keep_open(socket: &mut tokio::net::TcpStream) {
    socket
        .write_all(b"GET / HTTP/1.1\r\nhost: api\r\n\r\n")
        .await
        .unwrap();
    let mut head = Vec::new();
    while !head.ends_with(b"\r\n\r\n") {
        let mut byte = [0u8; 1];
        socket.read_exact(&mut byte).await.unwrap();
        head.push(byte[0]);
    }
    let head = String::from_utf8(head).unwrap().to_ascii_lowercase();
    let length: usize = head
        .lines()
        .find_map(|line| line.strip_prefix("content-length: "))
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    assert_eq!(length, ANSWER_BYTES);
    let mut remaining = length;
    let mut piece = vec![0u8; 64 * 1024];
    while remaining > 0 {
        let read = socket
            .read(&mut piece[..remaining.min(64 * 1024)])
            .await
            .unwrap();
        assert!(read > 0, "the connection closed mid-answer");
        remaining -= read;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_idle_connection_does_not_keep_a_copy_of_its_largest_answer() {
    // One shared answer: every response is a reference to it, so only a
    // copy made while writing it shows up as new heap.
    let answer = axum::body::Bytes::from(vec![b'x'; ANSWER_BYTES]);
    let router = axum::Router::new().route(
        "/",
        axum::routing::get(move || {
            let answer = answer.clone();
            async move { answer }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let shutdown = tokio_util::sync::CancellationToken::new();
    tokio::spawn({
        let shutdown = shutdown.clone();
        async move {
            loop {
                let Ok((socket, _)) = listener.accept().await else {
                    return;
                };
                tokio::spawn(serve_http_connection(
                    socket,
                    router.clone(),
                    ConnectionTimeouts::PRODUCTION,
                    None,
                    shutdown.clone(),
                ));
            }
        }
    });
    // Warm up: the first connection allocates the runtime's and Hyper's
    // one-off state, which isn't per connection.
    let mut warm = tokio::net::TcpStream::connect(address).await.unwrap();
    get_and_keep_open(&mut warm).await;
    drop(warm);
    tokio::time::sleep(Duration::from_millis(200)).await;

    let before = LIVE.load(Ordering::Relaxed);
    let mut open = Vec::new();
    for _ in 0..CONNECTIONS {
        let mut socket = tokio::net::TcpStream::connect(address).await.unwrap();
        get_and_keep_open(&mut socket).await;
        open.push(socket);
    }
    // Let every server task finish writing and go idle.
    tokio::time::sleep(Duration::from_millis(200)).await;
    let held = LIVE.load(Ordering::Relaxed).saturating_sub(before) / CONNECTIONS;
    shutdown.cancel();

    assert!(
        held < PER_CONNECTION_CEILING,
        "each idle connection holds {held} bytes after a {ANSWER_BYTES}-byte answer"
    );
}
