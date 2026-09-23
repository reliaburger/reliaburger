//! Deadline behaviour shared by TLS API, ingress and upgraded connections.

use reliaburger::sesame::connection::LifetimeLimitedIo;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

#[tokio::test(start_paused = true)]
async fn traffic_before_expiry_succeeds_but_does_not_extend_connection_lifetime() {
    let (stream, mut peer) = tokio::io::duplex(64);
    let mut stream = LifetimeLimitedIo::new(stream, Duration::from_secs(10));
    tokio::time::advance(Duration::from_secs(9)).await;
    stream.write_all(b"x").await.unwrap();
    assert_eq!(peer.read_u8().await.unwrap(), b'x');
    peer.write_all(b"y").await.unwrap();
    assert_eq!(stream.read_u8().await.unwrap(), b'y');
    tokio::time::advance(Duration::from_secs(1)).await;
    assert_eq!(
        stream.write_all(b"z").await.unwrap_err().kind(),
        std::io::ErrorKind::TimedOut
    );
    assert_eq!(
        stream.flush().await.unwrap_err().kind(),
        std::io::ErrorKind::TimedOut
    );
    assert_eq!(
        stream.shutdown().await.unwrap_err().kind(),
        std::io::ErrorKind::TimedOut
    );
}

#[tokio::test(start_paused = true)]
async fn an_idle_reader_wakes_at_the_connection_deadline() {
    let (stream, _peer) = tokio::io::duplex(64);
    let mut stream = LifetimeLimitedIo::new(stream, Duration::from_secs(10));
    let result = tokio::time::timeout(Duration::from_secs(11), stream.read_u8()).await;
    assert!(
        result.is_ok(),
        "the lifetime timer must wake a pending read"
    );
    assert_eq!(
        result.unwrap().unwrap_err().kind(),
        std::io::ErrorKind::TimedOut
    );
}

#[tokio::test(start_paused = true)]
async fn a_blocked_writer_wakes_at_the_connection_deadline() {
    let (stream, _peer) = tokio::io::duplex(1);
    let mut stream = LifetimeLimitedIo::new(stream, Duration::from_secs(10));
    let result = tokio::time::timeout(Duration::from_secs(11), stream.write_all(b"xx")).await;
    assert!(
        result.is_ok(),
        "the lifetime timer must wake a blocked writer"
    );
    assert_eq!(
        result.unwrap().unwrap_err().kind(),
        std::io::ErrorKind::TimedOut
    );
}
