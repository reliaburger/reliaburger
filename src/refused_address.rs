//! A loopback address that refuses connections, for tests (#1844).
//!
//! The obvious way to get one, binding port 0 and dropping the listener,
//! frees the port, and a test running in parallel can be given it and answer
//! for the "dead" peer. Holding a bound socket that never listens doesn't
//! work either: Linux refuses a connection to it, but macOS drops the SYN,
//! so the client waits for its timeout.
//!
//! Port 1 has neither problem. It sits below every ephemeral range (Linux
//! hands out 32768-60999, macOS 49152-65535), so binding port 0 can never
//! be given it, and nothing in the suite binds it by number. A connection
//! to it is refused at once on both.

use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};

/// A loopback address every connection to is refused.
pub(crate) fn refused_address() -> SocketAddr {
    SocketAddr::V4(SocketAddrV4::new(Ipv4Addr::LOCALHOST, 1))
}

/// [`refused_address`] as an `http://` URL.
pub(crate) fn refused_url() -> String {
    format!("http://{}", refused_address())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_connection_to_the_refused_address_is_refused_at_once() {
        let started = std::time::Instant::now();
        let error = tokio::net::TcpStream::connect(refused_address())
            .await
            .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::ConnectionRefused);
        assert!(started.elapsed() < std::time::Duration::from_secs(1));
    }

    #[test]
    fn an_ephemeral_port_is_never_the_refused_address() {
        // What a dropped port-0 listener could be handed to someone else.
        for _ in 0..64 {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            assert_ne!(listener.local_addr().unwrap(), refused_address());
        }
    }
}
