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

mod stall_guard {
    use reliaburger::sesame::connection::{
        ConnectionTimeouts, StallGuardedIo, configure_accepted_socket,
    };
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[tokio::test(start_paused = true)]
    async fn a_connection_that_never_sends_a_byte_times_out() {
        let (stream, _peer) = tokio::io::duplex(64);
        let mut stream =
            StallGuardedIo::new(stream, Duration::from_secs(5), Duration::from_secs(60));
        let result = tokio::time::timeout(Duration::from_secs(6), stream.read_u8()).await;
        assert_eq!(
            result.unwrap().unwrap_err().kind(),
            std::io::ErrorKind::TimedOut
        );
    }

    #[tokio::test(start_paused = true)]
    async fn after_the_first_byte_reads_wait_as_long_as_they_need() {
        let (stream, mut peer) = tokio::io::duplex(64);
        let mut stream =
            StallGuardedIo::new(stream, Duration::from_secs(5), Duration::from_secs(60));
        peer.write_all(b"G").await.unwrap();
        assert_eq!(stream.read_u8().await.unwrap(), b'G');
        let quiet = tokio::time::timeout(Duration::from_secs(600), stream.read_u8()).await;
        assert!(
            quiet.is_err(),
            "an established connection has no read deadline"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_write_that_makes_no_progress_fails_after_the_stall_limit() {
        let (stream, _peer) = tokio::io::duplex(1);
        let mut stream =
            StallGuardedIo::new(stream, Duration::from_secs(5), Duration::from_secs(60));
        let result = tokio::time::timeout(Duration::from_secs(61), stream.write_all(b"xx")).await;
        assert_eq!(
            result.unwrap().unwrap_err().kind(),
            std::io::ErrorKind::TimedOut
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_slow_reader_that_keeps_draining_is_not_a_stall() {
        let (stream, mut peer) = tokio::io::duplex(1);
        let mut stream =
            StallGuardedIo::new(stream, Duration::from_secs(5), Duration::from_secs(60));
        let writer = async {
            stream.write_all(&[b'x'; 5]).await.unwrap();
        };
        let reader = async {
            for _ in 0..5 {
                tokio::time::sleep(Duration::from_secs(50)).await;
                peer.read_u8().await.unwrap();
            }
        };
        // Over four minutes in total, but never more than 50 s without progress.
        tokio::join!(writer, reader);
    }

    #[tokio::test]
    async fn accepted_sockets_get_tcp_keepalive() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let _client = tokio::net::TcpStream::connect(listener.local_addr().unwrap())
            .await
            .unwrap();
        let (accepted, _) = listener.accept().await.unwrap();
        configure_accepted_socket(&accepted, &ConnectionTimeouts::PRODUCTION).unwrap();
        assert!(socket2::SockRef::from(&accepted).keepalive().unwrap());
    }
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
