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
//! paired with that address, or that the machine's first line on each boot
//! (its hello) names, when this process answered that MAC ([`Peers`]). A
//! sender it never answered is named by its address. It also appends the
//! line to `<netboot dir>/logs/<mac>.log`, so a lab record can quote it, or,
//! for a sender it never answered, to `logs/unserved.log` with the address
//! in front. Files are capped at 64 MiB each and 64 open at once, so lines
//! from many forged addresses can't use up relish's file descriptors or the
//! disk.
//!
//! The screen can show less than the file ([`StreamLogs`]). Three machines
//! booting send hundreds of kernel lines each, so `--stream-logs=units`
//! prints only the lines the boot units and this script write, and kernel
//! panics, and leaves the kernel's other lines to the file ([`kind`]).
//!
//! Nothing here is authenticated: anyone on the LAN can send lines, and a
//! hello can name any MAC. So the listener only prints and appends, it
//! marks a sender it didn't serve, and it replaces control characters, so a
//! line can't move the cursor or change the terminal's colours.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use tokio::io::AsyncWriteExt;
use tokio::net::UdpSocket;

use super::interface::{self, udp_socket};
use super::{InterfaceChoice, Log, MacAddress, NetbootError};

/// The UDP port boot logs arrive on: netconsole's usual one.
pub const NETLOG_PORT: u16 = crate::os::netlog::DEFAULT_PORT;

/// The directory under the netboot directory the raw lines go to.
pub const LOGS_DIRECTORY: &str = "logs";

/// The one file under `logs/` for every sender this process never served,
/// each line marked with its address: anyone can send from any number of
/// (forged) addresses, so they don't get a file each.
pub const UNSERVED_LOG: &str = "unserved.log";

/// The files under `logs/` kept open at once. Past this, the one written
/// longest ago is closed, and reopened if its machine sends again.
const MAX_OPEN_LOG_FILES: usize = 64;

/// The most a file under `logs/` grows to; later lines only print. A whole
/// boot's kernel log is a few hundred KiB.
const MAX_LOG_FILE_BYTES: u64 = 64 * 1024 * 1024;

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
            Sender::NotServed(_) => UNSERVED_LOG.to_string(),
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

/// The machines this process answered: their MACs, and the address each
/// one is sending from, as its DHCP requests or its hello showed them.
#[derive(Debug, Default)]
pub struct Peers {
    answered: HashSet<MacAddress>,
    by_address: HashMap<Ipv4Addr, MacAddress>,
    /// The address each MAC's latest DHCP request came from. Only DHCP
    /// sets it, never a hello, which anyone can send.
    asked_from: HashMap<MacAddress, Ipv4Addr>,
}

impl Peers {
    /// Remember that this process answered `mac`, asking from `address`. A
    /// later request from the same address replaces it, since leases move.
    /// A request from no address yet (0.0.0.0) only remembers the MAC.
    pub fn learn(&mut self, address: Ipv4Addr, mac: MacAddress) {
        self.answered.insert(mac);
        if address.is_unspecified() || address.is_broadcast() {
            return;
        }
        self.by_address.insert(address, mac);
        self.asked_from.insert(mac, address);
    }

    /// The address `mac`'s latest DHCP request came from, if it sent one
    /// from an address of its own.
    pub fn asked_from(&self, mac: MacAddress) -> Option<Ipv4Addr> {
        self.asked_from.get(&mac).copied()
    }

    /// A machine at `address` said hello naming `mac` ([`hello_mac`]).
    /// When this process answered that MAC, lines from `address` are its
    /// from now on. The installer and the installed system get their
    /// addresses from the LAN's DHCP server with a client ID of their own,
    /// so the address they send from needn't be the one iPXE asked from.
    /// A MAC this process never answered names nothing.
    pub fn hello(&mut self, address: Ipv4Addr, mac: MacAddress) {
        if self.answered.contains(&mac) {
            self.by_address.insert(address, mac);
        }
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

/// Who sent `line` from `address`, after learning from it if it's a hello.
fn sender(peers: &SharedPeers, address: Ipv4Addr, line: &str) -> Sender {
    let mut peers = peers
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(mac) = hello_mac(line) {
        peers.hello(address, mac);
    }
    peers.sender(address)
}

/// The line `/usr/lib/reliaburger/netlog` sends before anything else on
/// each boot starts with this, then `<address> (<interface> <mac>) to
/// <target>`.
const HELLO_PREFIX: &str = "reliaburger-netlog: streaming from ";

/// The MAC a hello line names, if `line` is one.
pub fn hello_mac(line: &str) -> Option<MacAddress> {
    let rest = line.strip_prefix(HELLO_PREFIX)?;
    let (_, inside) = rest.split_once('(')?;
    let (inside, _) = inside.split_once(')')?;
    let (_interface, mac) = inside.split_once(' ')?;
    mac.parse().ok()
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

/// Which boot log lines print on the screen (`relish netboot
/// --stream-logs[=WHICH]`). Every line goes to the machine's file either
/// way.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, clap::ValueEnum)]
pub enum StreamLogs {
    /// Every line: the kernel's and the boot units'.
    #[default]
    All,
    /// The boot units' lines (the installer, the boot check, bun, the seed,
    /// the clock wait) and the script's own, such as the hello naming the
    /// MAC, plus kernel panics. Other kernel lines go only to the file.
    Units,
}

impl StreamLogs {
    /// Whether `line` prints on the screen.
    pub fn shows(self, line: &str) -> bool {
        match self {
            StreamLogs::All => true,
            StreamLogs::Units => kind(line) != LineKind::Kernel,
        }
    }
}

/// What a boot log line is, as far as the screen cares ([`kind`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LineKind {
    /// A kernel panic, an oops or a `BUG:`: always printed.
    Panic,
    /// A line a boot unit or the netlog script wrote.
    Unit,
    /// Any other line: the kernel's own, or a program's the image doesn't
    /// stream on purpose (systemd's before journald starts, say).
    Kernel,
}

/// How the start of a panic, an oops or a `BUG:` reads, after the
/// timestamp. `Kernel panic` also ends a panic (`---[ end Kernel panic`).
const PANIC_STARTS: [&str; 3] = ["Oops", "BUG:", "Internal error: Oops"];
const PANIC_ANYWHERE: &str = "Kernel panic";

/// What `line` is. The machine sends three shapes of line, and the
/// structure, not the wording, tells them apart:
///
/// - the kernel's, which netconsole and the `dmesg` backlog prefix with
///   the kernel's timestamp, `[    9.412871] `;
/// - a boot unit's, which journald forwards to the kernel log
///   (`StandardOutput=kmsg` in netlog's drop-in) with a syslog tag after
///   the timestamp, `install[412]: `, and the boot check writes to
///   `/dev/kmsg` itself;
/// - the script's own (the hello, the backlog's markers), sent with no
///   timestamp at all.
///
/// Under the timestamp and tag, every line the image's boot units and the
/// netlog script write starts with their own tag: `reliaburger:`, or
/// `reliaburger-<unit>:` (the installer's `reliaburger-install:`, the
/// boot check's `reliaburger-boot-check:`, the hello's
/// `reliaburger-netlog:`), or bun's `bun:`. That tag, as the first word of
/// the message, is what makes a unit line; a kernel line can mention
/// `reliaburger` anywhere else and stays a kernel line.
pub fn kind(line: &str) -> LineKind {
    let message = without_timestamp(line);
    if PANIC_STARTS.iter().any(|start| message.starts_with(start))
        || message.contains(PANIC_ANYWHERE)
    {
        return LineKind::Panic;
    }
    if has_unit_tag(without_syslog_tag(message)) {
        LineKind::Unit
    } else {
        LineKind::Kernel
    }
}

/// `line` without the kernel's `[    9.412871] ` in front, if it has one.
fn without_timestamp(line: &str) -> &str {
    let Some((stamp, rest)) = line
        .strip_prefix('[')
        .and_then(|inside| inside.split_once(']'))
    else {
        return line;
    };
    let is_stamp = stamp
        .trim_start()
        .split_once('.')
        .is_some_and(|(seconds, fraction)| is_digits(seconds) && is_digits(fraction));
    if is_stamp { rest.trim_start() } else { line }
}

/// `message` without the `<identifier>[<pid>]: ` journald puts in front
/// of a line it forwards to the kernel log, if it has one.
fn without_syslog_tag(message: &str) -> &str {
    let Some((tag, rest)) = message.split_once("]: ") else {
        return message;
    };
    match tag.split_once('[') {
        Some((identifier, pid))
            if !identifier.is_empty()
                && !identifier.contains(char::is_whitespace)
                && is_digits(pid) =>
        {
            rest
        }
        _ => message,
    }
}

/// Whether `message` starts with a boot unit's own tag: `reliaburger:`,
/// `reliaburger-<unit>:` or `bun:`.
fn has_unit_tag(message: &str) -> bool {
    let Some((tag, _)) = message.split_once(':') else {
        return false;
    };
    match tag.strip_prefix("reliaburger") {
        Some("") => true,
        Some(rest) => rest.strip_prefix('-').is_some_and(|unit| {
            !unit.is_empty()
                && unit
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        }),
        None => tag == "bun",
    }
}

fn is_digits(text: &str) -> bool {
    !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit())
}

/// How a line prints among the netboot server's other lines.
pub fn describe(sender: Sender, line: &str) -> String {
    format!("log {sender}: {line}")
}

/// One file under `logs/`, open for appending.
struct OpenLog {
    file: tokio::fs::File,
    /// Its length so far.
    length: u64,
    /// When it was last written, by [`LogFiles`]'s count of writes.
    last_write: u64,
}

/// The files under `logs/` that lines are appended to: at most
/// [`MAX_OPEN_LOG_FILES`] open at once, each growing to `max_bytes`.
struct LogFiles {
    directory: Option<PathBuf>,
    open: HashMap<String, OpenLog>,
    /// Files that reached `max_bytes`: their lines only print.
    full: HashSet<String>,
    /// Writes so far, to tell which open file was written longest ago.
    writes: u64,
    /// A write failed once, and the operator heard why; later failures
    /// stay quiet so the terminal isn't flooded.
    warned: bool,
    /// The most a file grows to.
    max_bytes: u64,
}

impl LogFiles {
    fn new(directory: Option<PathBuf>) -> Self {
        Self {
            directory,
            open: HashMap::new(),
            full: HashSet::new(),
            writes: 0,
            warned: false,
            max_bytes: MAX_LOG_FILE_BYTES,
        }
    }

    /// Append `line` to `sender`'s file. True when this line is the first
    /// one dropped because the file is full, so the caller can say so once.
    async fn append(&mut self, sender: Sender, line: &str) -> std::io::Result<bool> {
        let Some(directory) = self.directory.clone() else {
            return Ok(false);
        };
        let name = sender.file_name();
        if self.full.contains(&name) {
            return Ok(false);
        }
        let text = match sender {
            Sender::Served(_) => format!("{line}\n"),
            Sender::NotServed(address) => format!("{address} {line}\n"),
        };
        if !self.open.contains_key(&name) {
            self.close_oldest_if_full();
            tokio::fs::create_dir_all(&directory).await?;
            let file = tokio::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(directory.join(&name))
                .await?;
            let length = file.metadata().await?.len();
            self.open.insert(
                name.clone(),
                OpenLog {
                    file,
                    length,
                    last_write: 0,
                },
            );
        }
        self.writes += 1;
        let Some(open) = self.open.get_mut(&name) else {
            return Ok(false);
        };
        if open.length + text.len() as u64 > self.max_bytes {
            self.open.remove(&name);
            self.full.insert(name);
            return Ok(true);
        }
        open.file.write_all(text.as_bytes()).await?;
        // tokio's File hands the write to a blocking thread and returns
        // before it lands; flush waits for it, so a line that has printed
        // is in the file too.
        open.file.flush().await?;
        open.length += text.len() as u64;
        open.last_write = self.writes;
        Ok(false)
    }

    /// Make room for one more open file by closing the one written longest
    /// ago. It opens again, appending, if its machine sends another line.
    fn close_oldest_if_full(&mut self) {
        if self.open.len() < MAX_OPEN_LOG_FILES {
            return;
        }
        let oldest = self
            .open
            .iter()
            .min_by_key(|(_, open)| open.last_write)
            .map(|(name, _)| name.clone());
        if let Some(name) = oldest {
            self.open.remove(&name);
        }
    }
}

/// Append each line that arrives on `socket` to its sender's file under
/// `directory` when there is one, and print the ones `shown` shows through
/// `log`, naming the sender from `peers`. Runs until the socket fails.
pub async fn listen(
    socket: UdpSocket,
    peers: SharedPeers,
    directory: Option<PathBuf>,
    shown: StreamLogs,
    log: Log,
) -> std::io::Result<()> {
    let mut files = LogFiles::new(directory.map(|d| d.join(LOGS_DIRECTORY)));
    let mut buffer = vec![0u8; MAX_DATAGRAM];
    loop {
        let (len, source) = socket.recv_from(&mut buffer).await?;
        let SocketAddr::V4(source) = source else {
            continue;
        };
        for line in lines(&buffer[..len]) {
            let sender = sender(&peers, *source.ip(), &line);
            // The file first: once a line has printed, it's on disk too.
            match files.append(sender, &line).await {
                Ok(true) => log(format!(
                    "log: {} reached {} MiB; its lines only print from now on",
                    sender.file_name(),
                    files.max_bytes / (1024 * 1024)
                )),
                Ok(false) => {}
                Err(error) if !files.warned => {
                    files.warned = true;
                    log(format!(
                        "log: can't write {}: {error}; boot logs only print from now on",
                        sender.file_name()
                    ));
                }
                Err(_) => {}
            }
            if shown.shows(&line) {
                log(describe(sender, &line));
            }
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
        result = listen(socket, peers, directory, StreamLogs::All, log) => Err(NetbootError::ServerStopped {
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
        assert_eq!(peers.sender(stranger).file_name(), "unserved.log");
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

    /// The line `/usr/lib/reliaburger/netlog` sends first on every boot.
    const HELLO: &str = "reliaburger-netlog: streaming from 192.168.1.77 (enp1s0 52:54:00:12:34:56) to 192.168.1.20:6666";

    #[test]
    fn the_hello_line_names_the_senders_mac() {
        assert_eq!(hello_mac(HELLO), Some(MAC));
        // An interface without a MAC, another line, or a line that only
        // looks like one.
        assert_eq!(
            hello_mac("reliaburger-netlog: streaming from 10.0.0.5 (eth0 ) to 10.0.0.1:6666"),
            None
        );
        assert_eq!(hello_mac("reliaburger: bun healthy"), None);
        assert_eq!(
            hello_mac(
                "[ 1.0] netlog[371]: reliaburger-netlog: streaming from 10.0.0.5 (eth0 52:54:00:12:34:56) to 10.0.0.1:6666"
            ),
            None
        );
        assert_eq!(
            hello_mac("reliaburger-netlog: streaming from 10.0.0.5 (eth0 nonsense) to x"),
            None
        );
    }

    #[test]
    fn a_machine_with_a_new_address_is_named_by_the_mac_in_its_hello() {
        // The installer and the installed system ask the router for an
        // address with a client ID of their own, so they can get a
        // different one from the one iPXE asked relish from. Their hello
        // names the MAC relish answered, so it's the same machine.
        let mut peers = Peers::default();
        peers.learn(Ipv4Addr::new(192, 168, 1, 51), MAC);
        let installer = Ipv4Addr::new(192, 168, 1, 77);
        assert_eq!(peers.sender(installer), Sender::NotServed(installer));
        peers.hello(installer, MAC);
        assert_eq!(peers.sender(installer), Sender::Served(MAC));
    }

    #[test]
    fn a_mac_answered_before_the_machine_had_an_address_still_counts() {
        // The firmware's first request comes from 0.0.0.0.
        let mut peers = Peers::default();
        peers.learn(Ipv4Addr::UNSPECIFIED, MAC);
        let address = Ipv4Addr::new(192, 168, 1, 77);
        peers.hello(address, MAC);
        assert_eq!(peers.sender(address), Sender::Served(MAC));
    }

    #[test]
    fn a_hello_naming_a_mac_this_server_never_answered_names_nothing() {
        // `relish machines logs` answers nobody, so every sender stays
        // marked, whatever its hello says.
        let mut peers = Peers::default();
        let stranger = Ipv4Addr::new(192, 168, 1, 99);
        peers.hello(stranger, MAC);
        assert_eq!(peers.sender(stranger), Sender::NotServed(stranger));
    }

    #[tokio::test]
    async fn the_hello_names_the_lines_that_follow_it() {
        let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let listener = socket.local_addr().unwrap();
        let peers = SharedPeers::default();
        // relish answered the machine's firmware before it had an address.
        learn(&peers, Ipv4Addr::UNSPECIFIED, MAC);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let log: Log = Arc::new(move |line| {
            let _ = tx.send(line);
        });
        let task = tokio::spawn(listen(socket, peers, None, StreamLogs::All, log));
        let machine = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        machine
            .send_to(format!("{HELLO}\n").as_bytes(), listener)
            .await
            .unwrap();
        machine
            .send_to(b"reliaburger-install: installed\n", listener)
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
                format!("log 52:54:00:12:34:56: {HELLO}"),
                "log 52:54:00:12:34:56: reliaburger-install: installed".to_string(),
            ]
        );
        task.abort();
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
            StreamLogs::All,
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

    /// Anyone on the LAN can send a line, from as many (forged) source
    /// addresses as it likes. A file and an open handle each would run
    /// relish, and the netboot servers in the same process, out of file
    /// descriptors. Lines from senders this process never served share one
    /// file, each marked with its address.
    #[tokio::test]
    async fn lines_from_addresses_this_server_never_answered_share_one_bounded_file() {
        let directory = tempfile::tempdir().unwrap();
        let mut files = LogFiles::new(Some(directory.path().to_path_buf()));
        for n in 0..300u16 {
            let address = Ipv4Addr::new(10, 0, (n / 256) as u8, (n % 256) as u8);
            files.append(Sender::NotServed(address), "x").await.unwrap();
        }
        assert!(files.open.len() <= MAX_OPEN_LOG_FILES);
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 1);
        let shared = std::fs::read_to_string(directory.path().join("unserved.log")).unwrap();
        assert!(shared.starts_with("10.0.0.0 x\n10.0.0.1 x\n"), "{shared}");

        // Served machines each have a file, but not each an open handle.
        for n in 0..100u8 {
            let mac = MacAddress([0x52, 0x54, 0x00, 0x00, 0x01, n]);
            files.append(Sender::Served(mac), "y").await.unwrap();
        }
        assert!(files.open.len() <= MAX_OPEN_LOG_FILES);
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 101);
        // A machine whose file was closed is appended to again.
        let first = MacAddress([0x52, 0x54, 0x00, 0x00, 0x01, 0]);
        files.append(Sender::Served(first), "z").await.unwrap();
        let written =
            std::fs::read_to_string(directory.path().join(Sender::Served(first).file_name()))
                .unwrap();
        assert_eq!(written, "y\nz\n");
    }

    #[tokio::test]
    async fn a_log_file_stops_growing_at_its_cap() {
        let directory = tempfile::tempdir().unwrap();
        let mut files = LogFiles::new(Some(directory.path().to_path_buf()));
        files.max_bytes = 1000;
        for _ in 0..100 {
            files
                .append(Sender::Served(MAC), &"a".repeat(49))
                .await
                .unwrap();
        }
        let file = directory.path().join(Sender::Served(MAC).file_name());
        let length = std::fs::metadata(&file).unwrap().len();
        assert!(length <= 1000, "{length}");
        assert!(length >= 950, "it filled up to the cap: {length}");
    }

    // `--stream-logs=units` (#2149 follow-up): what the screen keeps.

    /// A boot as it arrives: the hello and the backlog markers the script
    /// sends itself, the kernel's lines, the installer's and bun's lines as
    /// journald forwards them, the boot check's own kmsg lines, and a panic.
    const MIXED: &[&str] = &[
        HELLO,
        "reliaburger-netlog: the kernel log so far, 812 lines",
        "[    0.000000] Linux version 6.8.0-49-generic (buildd@lcy02-amd64-103)",
        "[    1.204113] systemd[1]: Detected architecture x86-64.",
        "[    2.881204] r8169 0000:01:00.0 enp1s0: Link is Up - 1Gbps/Full",
        "reliaburger-netlog: end of the kernel log so far",
        "[    9.412871] install[412]: reliaburger-install: target /dev/mmcblk0, 7818 MB",
        "[   10.000001] EXT4-fs (mmcblk0p3): mounted filesystem",
        "[   31.554310] reliaburger-boot-check: reliaburger: bun healthy on OS 2026.47.228",
        "[   32.100200] start[880]: reliaburger: bun 0.3.3 from the image is now active (was none)",
        "[   33.010101] start[880]: bun: listening on 10.42.0.11:9443",
        "[   40.123456] audit: type=1400 audit(1700000000.000:2): apparmor=\"STATUS\"",
        "[   41.000000] BUG: kernel NULL pointer dereference, address: 0000000000000008",
        "[   41.000100] Oops: 0002 [#1] PREEMPT SMP NOPTI",
        "[   41.000200] RIP: 0010:netlog_demo+0x10/0x20",
        "[   41.000300] Kernel panic - not syncing: Fatal exception",
    ];

    #[test]
    fn the_units_and_the_script_are_unit_lines() {
        for line in [
            HELLO,
            "reliaburger-netlog: the kernel log so far, 812 lines",
            "[    9.412871] install[412]: reliaburger-install: target /dev/mmcblk0, 7818 MB",
            "[   31.554310] reliaburger-boot-check: reliaburger: bun healthy on OS 2026.47.228",
            "[   32.100200] start[880]: reliaburger: bun 0.3.3 from the image is now active",
            "[   33.010101] start[880]: bun: listening on 10.42.0.11:9443",
            // Before journald, or from a test, without a timestamp or a tag.
            "reliaburger: counted boot failed, rebooting",
        ] {
            assert_eq!(kind(line), LineKind::Unit, "{line}");
        }
    }

    #[test]
    fn kernel_lines_are_kernel_lines_whatever_they_mention() {
        for line in [
            "[    0.000000] Linux version 6.8.0-49-generic",
            "[    1.204113] systemd[1]: Detected architecture x86-64.",
            "[    1.204113] systemd[1]: Starting reliaburger.service - Reliaburger bun...",
            "[    2.881204] r8169 0000:01:00.0 enp1s0: Link is Up - 1Gbps/Full",
            "[    3.000000] reliaburger_fw: loaded",
            "[    3.000000] some text that says reliaburger: in the middle",
            // A netconsole fragment: the rest of a line cut across packets.
            "Full",
            // Not a tag: no space before the bracket, or no digits in it.
            "[    4.000000] start[x]: reliaburger: no pid",
            "[    4.000000] [reliaburger: odd",
        ] {
            assert_eq!(kind(line), LineKind::Kernel, "{line}");
        }
    }

    #[test]
    fn panics_and_oopses_are_panics() {
        for line in [
            "[   41.000000] BUG: kernel NULL pointer dereference, address: 0000000000000008",
            "[   41.000100] Oops: 0002 [#1] PREEMPT SMP NOPTI",
            "[   41.000100] Internal error: Oops: 96000004 [#1] SMP",
            "[   41.000300] Kernel panic - not syncing: Fatal exception",
            "[   41.000400] ---[ end Kernel panic - not syncing: Fatal exception ]---",
            "Kernel panic - not syncing: VFS: Unable to mount root fs",
        ] {
            assert_eq!(kind(line), LineKind::Panic, "{line}");
        }
    }

    #[test]
    fn all_shows_every_line_and_units_hides_only_the_kernels() {
        for line in MIXED {
            assert!(StreamLogs::All.shows(line), "{line}");
            assert_eq!(
                StreamLogs::Units.shows(line),
                kind(line) != LineKind::Kernel,
                "{line}"
            );
        }
        // The hello names the MAC on screen, so the lines after it do too.
        assert!(StreamLogs::Units.shows(HELLO));
        assert!(!StreamLogs::Units.shows("[    2.881204] r8169 0000:01:00.0 enp1s0: Link is Up"));
    }

    #[test]
    fn units_keeps_the_boot_story_on_screen() {
        let sender = Sender::Served(MAC);
        let screen: Vec<String> = MIXED
            .iter()
            .filter(|line| StreamLogs::Units.shows(line))
            .map(|line| describe(sender, line))
            .collect();
        insta::assert_snapshot!("netlog_units_screen", screen.join("\n"));
    }

    /// Send [`MIXED`] to a listener showing `shown`, and return what it
    /// printed and what it wrote to the machine's file.
    async fn listen_to_mixed(shown: StreamLogs) -> (Vec<String>, String) {
        let directory = tempfile::tempdir().unwrap();
        let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let listener = socket.local_addr().unwrap();
        let peers = SharedPeers::default();
        learn(&peers, Ipv4Addr::LOCALHOST, MAC);
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let log: Log = Arc::new(move |line| {
            let _ = tx.send(line);
        });
        let task = tokio::spawn(listen(
            socket,
            peers,
            Some(directory.path().to_path_buf()),
            shown,
            log,
        ));
        let machine = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        // One datagram, so loopback can't reorder or drop part of it.
        machine
            .send_to(format!("{}\n", MIXED.join("\n")).as_bytes(), listener)
            .await
            .unwrap();
        // The last line is a panic, which every mode prints, and each line
        // is in the file before it prints: once it has printed, the file
        // holds everything.
        let last = describe(Sender::Served(MAC), MIXED[MIXED.len() - 1]);
        let mut printed = Vec::new();
        while printed.last() != Some(&last) {
            let line = tokio::time::timeout(Duration::from_secs(5), rx.recv())
                .await
                .unwrap()
                .unwrap();
            printed.push(line);
        }
        task.abort();
        let file = directory
            .path()
            .join(LOGS_DIRECTORY)
            .join("52:54:00:12:34:56.log");
        let written = tokio::fs::read_to_string(&file).await.unwrap();
        (printed, written)
    }

    #[tokio::test]
    async fn every_line_goes_to_the_file_whatever_the_screen_shows() {
        let everything = format!("{}\n", MIXED.join("\n"));
        for shown in [StreamLogs::All, StreamLogs::Units] {
            let (printed, written) = listen_to_mixed(shown).await;
            assert_eq!(written, everything, "{shown:?}");
            let expected: Vec<String> = MIXED
                .iter()
                .filter(|line| shown.shows(line))
                .map(|line| describe(Sender::Served(MAC), line))
                .collect();
            assert_eq!(printed, expected, "{shown:?}");
        }
    }

    #[tokio::test]
    async fn without_a_directory_the_listener_only_prints() {
        let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let listener = socket.local_addr().unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let log: Log = Arc::new(move |line| {
            let _ = tx.send(line);
        });
        let task = tokio::spawn(listen(
            socket,
            SharedPeers::default(),
            None,
            StreamLogs::All,
            log,
        ));
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
