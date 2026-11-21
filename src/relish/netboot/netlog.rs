//! Boot logs over the network (#2149): the listener for the lines a node
//! sends while it boots.
//!
//! With `relish netboot --stream-logs`, the installer writes this server's
//! address to `reliaburger/netlog.conf` on the node's ESP. From then on,
//! `reliaburger-netlog.service` on the node points the kernel's netconsole
//! at that address and sends the kernel log so far, so every kernel line,
//! and the lines of the units that matter during a boot, arrive here as UDP
//! datagrams on [`NETLOG_PORT`].
//!
//! The listener prints each line among `relish netboot`'s other lines as
//! `log <mac>: <line>`, naming the sender by the MAC its own DHCP answers
//! paired with that address ([`Peers`]), or by its address when it never
//! answered that machine. It also appends the line to
//! `<netboot dir>/logs/<mac>.log`, so a lab record can quote it.
//!
//! Nothing here is authenticated: anyone on the LAN can send lines. So the
//! listener only prints and appends, it marks a sender it didn't serve, and
//! it replaces control characters, so a line can't move the cursor or
//! change the terminal's colours.

use std::collections::HashMap;
use std::fmt;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use tokio::io::AsyncWriteExt;
use tokio::net::UdpSocket;

use super::interface::{self, udp_socket};
use super::{InterfaceChoice, Log, MacAddress, NetbootError};

/// The UDP port boot logs arrive on: netconsole's usual one.
pub const NETLOG_PORT: u16 = 6666;

/// The directory under the netboot directory the raw lines go to.
pub const LOGS_DIRECTORY: &str = "logs";

/// The largest datagram the listener reads. netconsole sends at most a
/// few hundred bytes a packet; anything longer is cut there.
const MAX_DATAGRAM: usize = 2048;

/// Who sent a boot log line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sender {
    /// A machine this process answered over DHCP, by its MAC.
    Served(MacAddress),
    /// An address this process never answered.
    NotServed(Ipv4Addr),
}

impl Sender {
    /// The file under `logs/` its lines are appended to.
    pub fn file_name(self) -> String {
        match self {
            Sender::Served(mac) => format!("{mac}.log"),
            Sender::NotServed(address) => format!("{address}.log"),
        }
    }
}

impl fmt::Display for Sender {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Sender::Served(mac) => write!(f, "{mac}"),
            Sender::NotServed(address) => write!(f, "{address} (not served)"),
        }
    }
}

/// The machines this process answered: each one's address and MAC, as its
/// DHCP requests showed them.
#[derive(Debug, Default)]
pub struct Peers {
    by_address: HashMap<Ipv4Addr, MacAddress>,
}

impl Peers {
    /// Remember that `mac` asked from `address`. A later request from the
    /// same address replaces it, since leases move.
    pub fn learn(&mut self, address: Ipv4Addr, mac: MacAddress) {
        if address.is_unspecified() || address.is_broadcast() {
            return;
        }
        self.by_address.insert(address, mac);
    }

    /// Who sent a line from `address`.
    pub fn sender(&self, address: Ipv4Addr) -> Sender {
        match self.by_address.get(&address) {
            Some(mac) => Sender::Served(*mac),
            None => Sender::NotServed(address),
        }
    }
}

/// [`Peers`], shared between the DHCP servers, which learn from their
/// synchronous answer callback, and the listener. A `std` mutex: each
/// holder only reads or inserts one entry and never awaits while holding
/// it.
pub type SharedPeers = Arc<Mutex<Peers>>;

/// Remember that `mac` asked from `address`.
pub fn learn(peers: &SharedPeers, address: Ipv4Addr, mac: MacAddress) {
    // A panic elsewhere can't leave a half-written map entry behind, so a
    // poisoned lock is still usable.
    peers
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .learn(address, mac);
}

fn sender(peers: &SharedPeers, address: Ipv4Addr) -> Sender {
    peers
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .sender(address)
}

/// The lines in one datagram: split at newlines, without empty ones, and
/// with every control character but a tab shown as `?`. Bytes that aren't
/// UTF-8 become U+FFFD.
pub fn lines(datagram: &[u8]) -> Vec<String> {
    String::from_utf8_lossy(datagram)
        .split('\n')
        .map(|line| {
            line.trim_end_matches('\r')
                .chars()
                .map(|c| if c.is_control() && c != '\t' { '?' } else { c })
                .collect::<String>()
        })
        .filter(|line| !line.trim().is_empty())
        .collect()
}

/// How a line prints among the netboot server's other lines.
pub fn describe(sender: Sender, line: &str) -> String {
    format!("log {sender}: {line}")
}

/// The files under `logs/` that lines are appended to, kept open.
struct LogFiles {
    directory: Option<PathBuf>,
    open: HashMap<String, tokio::fs::File>,
    /// A write failed once, and the operator heard why; later failures
    /// stay quiet so the terminal isn't flooded.
    warned: bool,
}

impl LogFiles {
    async fn append(&mut self, sender: Sender, line: &str) -> std::io::Result<()> {
        let Some(directory) = &self.directory else {
            return Ok(());
        };
        let name = sender.file_name();
        if !self.open.contains_key(&name) {
            tokio::fs::create_dir_all(directory).await?;
            let file = tokio::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(directory.join(&name))
                .await?;
            self.open.insert(name.clone(), file);
        }
        if let Some(file) = self.open.get_mut(&name) {
            file.write_all(format!("{line}\n").as_bytes()).await?;
        }
        Ok(())
    }
}

/// Print each line that arrives on `socket` through `log`, naming its
/// sender from `peers`, and append it to `logs/<sender>.log` under
/// `directory` when there is one. Runs until the socket fails.
pub async fn listen(
    socket: UdpSocket,
    peers: SharedPeers,
    directory: Option<PathBuf>,
    log: Log,
) -> std::io::Result<()> {
    let mut files = LogFiles {
        directory: directory.map(|d| d.join(LOGS_DIRECTORY)),
        open: HashMap::new(),
        warned: false,
    };
    let mut buffer = vec![0u8; MAX_DATAGRAM];
    loop {
        let (len, source) = socket.recv_from(&mut buffer).await?;
        let SocketAddr::V4(source) = source else {
            continue;
        };
        let sender = sender(&peers, *source.ip());
        for line in lines(&buffer[..len]) {
            // The file first: once a line has printed, it's on disk too.
            if let Err(error) = files.append(sender, &line).await
                && !files.warned
            {
                files.warned = true;
                log(format!(
                    "log: can't write {}: {error}; boot logs only print from now on",
                    sender.file_name()
                ));
            }
            log(describe(sender, &line));
        }
    }
}

/// `relish machines logs`: only the listener, for a lab where nothing is
/// being served. Runs until Ctrl-C.
pub async fn run_listener(
    choice: &InterfaceChoice,
    directory: Option<PathBuf>,
) -> Result<(), NetbootError> {
    let interface = interface::find(choice)?;
    let socket = udp_socket(
        "the boot log listener",
        SocketAddrV4::new(interface.address, NETLOG_PORT),
        None,
    )?;
    println!(
        "relish machines logs: listening on {} ({}) UDP {NETLOG_PORT}; Ctrl-C stops it",
        interface.name, interface.address
    );
    if let Some(directory) = &directory {
        println!(
            "  appending each machine's lines to {}",
            directory
                .join(LOGS_DIRECTORY)
                .join("<machine>.log")
                .display()
        );
    }
    let log: Log = Arc::new(|line| println!("{line}"));
    let peers = SharedPeers::default();
    tokio::select! {
        _ = tokio::signal::ctrl_c() => {
            println!("relish machines logs: stopped");
            Ok(())
        }
        result = listen(socket, peers, directory, log) => Err(NetbootError::ServerStopped {
            reason: match result {
                Ok(()) => "the boot log listener stopped".to_string(),
                Err(error) => format!("the boot log listener failed: {error}"),
            },
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    const MAC: MacAddress = MacAddress([0x52, 0x54, 0x00, 0x12, 0x34, 0x56]);

    #[test]
    fn a_datagram_splits_into_its_non_empty_lines() {
        assert_eq!(
            lines(b"[    1.234567] r8169 0000:01:00.0 eth0: link up\n"),
            ["[    1.234567] r8169 0000:01:00.0 eth0: link up"]
        );
        assert_eq!(
            lines(b"reliaburger: bun not healthy after 120 s\r\n\nreliaburger: counted boot failed, rebooting\n"),
            [
                "reliaburger: bun not healthy after 120 s",
                "reliaburger: counted boot failed, rebooting"
            ]
        );
        // netconsole can split a long line across packets: each part
        // prints as it comes.
        assert_eq!(lines(b"no newline yet"), ["no newline yet"]);
        assert!(lines(b"").is_empty());
        assert!(lines(b"\n \n\r\n").is_empty());
    }

    #[test]
    fn control_characters_and_bad_utf8_cannot_reach_the_terminal() {
        // An escape sequence that would clear the screen, a bell, and a
        // byte that isn't UTF-8.
        let line = lines(b"\x1b[2Jcleared\x07 \xff\tok");
        assert_eq!(line, ["?[2Jcleared? \u{fffd}\tok"]);
    }

    #[test]
    fn a_served_machine_is_named_by_its_mac_and_others_are_marked() {
        let mut peers = Peers::default();
        let address = Ipv4Addr::new(192, 168, 1, 51);
        peers.learn(address, MAC);
        assert_eq!(peers.sender(address), Sender::Served(MAC));
        let stranger = Ipv4Addr::new(192, 168, 1, 99);
        assert_eq!(peers.sender(stranger), Sender::NotServed(stranger));

        assert_eq!(
            describe(peers.sender(address), "reliaburger: bun healthy"),
            "log 52:54:00:12:34:56: reliaburger: bun healthy"
        );
        assert_eq!(
            describe(peers.sender(stranger), "hello"),
            "log 192.168.1.99 (not served): hello"
        );
        assert_eq!(peers.sender(address).file_name(), "52:54:00:12:34:56.log");
        assert_eq!(peers.sender(stranger).file_name(), "192.168.1.99.log");
    }

    #[test]
    fn a_request_without_an_address_teaches_nothing() {
        let mut peers = Peers::default();
        peers.learn(Ipv4Addr::UNSPECIFIED, MAC);
        peers.learn(Ipv4Addr::BROADCAST, MAC);
        assert_eq!(
            peers.sender(Ipv4Addr::UNSPECIFIED),
            Sender::NotServed(Ipv4Addr::UNSPECIFIED)
        );
    }

    #[test]
    fn a_later_request_from_the_same_address_replaces_the_mac() {
        let mut peers = Peers::default();
        let address = Ipv4Addr::new(192, 168, 1, 51);
        let other = MacAddress([0x52, 0x54, 0x00, 0x12, 0x34, 0x57]);
        peers.learn(address, MAC);
        peers.learn(address, other);
        assert_eq!(peers.sender(address), Sender::Served(other));
    }

    #[tokio::test]
    async fn the_listener_prints_each_line_and_appends_it_to_the_machines_file() {
        let directory = tempfile::tempdir().unwrap();
        let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let listener = socket.local_addr().unwrap();
        let peers = SharedPeers::default();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let log: Log = Arc::new(move |line| {
            let _ = tx.send(line);
        });
        let task = tokio::spawn(listen(
            socket,
            peers.clone(),
            Some(directory.path().to_path_buf()),
            log,
        ));

        let served = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        // The served machine is 127.0.0.1 too, so learn it before it sends,
        // as the DHCP answer would have.
        learn(&peers, Ipv4Addr::LOCALHOST, MAC);
        served
            .send_to(
                b"reliaburger: bun not healthy after 120 s\nreliaburger: counted boot failed, rebooting\n",
                listener,
            )
            .await
            .unwrap();
        let mut printed = Vec::new();
        for _ in 0..2 {
            let line = tokio::time::timeout(Duration::from_secs(5), rx.recv())
                .await
                .unwrap()
                .unwrap();
            printed.push(line);
        }
        assert_eq!(
            printed,
            [
                "log 52:54:00:12:34:56: reliaburger: bun not healthy after 120 s",
                "log 52:54:00:12:34:56: reliaburger: counted boot failed, rebooting",
            ]
        );
        let file = directory
            .path()
            .join(LOGS_DIRECTORY)
            .join("52:54:00:12:34:56.log");
        let written = tokio::fs::read_to_string(&file).await.unwrap();
        assert_eq!(
            written,
            "reliaburger: bun not healthy after 120 s\nreliaburger: counted boot failed, rebooting\n"
        );
        task.abort();
    }

    #[tokio::test]
    async fn without_a_directory_the_listener_only_prints() {
        let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let listener = socket.local_addr().unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let log: Log = Arc::new(move |line| {
            let _ = tx.send(line);
        });
        let task = tokio::spawn(listen(socket, SharedPeers::default(), None, log));
        let stranger = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        stranger.send_to(b"hello\n", listener).await.unwrap();
        let line = tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(line, "log 127.0.0.1 (not served): hello");
        task.abort();
    }
}
