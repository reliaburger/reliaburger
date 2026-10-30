//! A read-only TFTP server, just big enough for PXE.
//!
//! PXE firmware only speaks TFTP, so iPXE and its first script come this
//! way; everything bigger goes over HTTP. The protocol is RFC 1350 with
//! option negotiation (RFC 2347): `blksize` (RFC 2348) for bigger blocks,
//! `tsize` and `timeout` (RFC 2349). Each transfer gets its own socket on a
//! fresh port, as RFC 1350 asks, and sends one block at a time, resending
//! it when the acknowledgement doesn't come.
//!
//! [`Packet`] parses and encodes, [`negotiate`] answers the options and
//! [`Transfer`] is the sender's state machine; none of them touch the
//! network. [`serve`] is the loop around them.

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use tokio::net::UdpSocket;

/// The TFTP port.
pub const TFTP_PORT: u16 = 69;

/// The block size when the client asks for none (RFC 1350).
pub const DEFAULT_BLOCK_SIZE: u16 = 512;

/// The largest block we agree to: a 1500-byte Ethernet frame less the IP,
/// UDP and TFTP headers, so blocks are never fragmented.
pub const MAX_BLOCK_SIZE: u16 = 1468;

/// Seconds to wait for an acknowledgement when the client asks for no timeout.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(2);

/// How many times a block is resent before the transfer is abandoned.
pub const DEFAULT_RETRIES: u32 = 5;

/// TFTP error codes (RFC 1350, RFC 2347).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorCode {
    /// 0: see the message.
    Undefined,
    /// 1: no such file.
    FileNotFound,
    /// 2: access violation (we refuse writes).
    AccessViolation,
    /// 4: illegal TFTP operation.
    IllegalOperation,
    /// 8: the client refused our option acknowledgement.
    OptionsRefused,
    /// Any other code.
    Other(u16),
}

impl From<u16> for ErrorCode {
    fn from(code: u16) -> Self {
        match code {
            0 => ErrorCode::Undefined,
            1 => ErrorCode::FileNotFound,
            2 => ErrorCode::AccessViolation,
            4 => ErrorCode::IllegalOperation,
            8 => ErrorCode::OptionsRefused,
            other => ErrorCode::Other(other),
        }
    }
}

impl From<ErrorCode> for u16 {
    fn from(code: ErrorCode) -> Self {
        match code {
            ErrorCode::Undefined => 0,
            ErrorCode::FileNotFound => 1,
            ErrorCode::AccessViolation => 2,
            ErrorCode::IllegalOperation => 4,
            ErrorCode::OptionsRefused => 8,
            ErrorCode::Other(other) => other,
        }
    }
}

/// One TFTP packet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Packet {
    /// RRQ: read `filename`, with options as (name, value) in order.
    ReadRequest {
        filename: String,
        mode: String,
        options: Vec<(String, String)>,
    },
    /// WRQ: a write, which this server refuses.
    WriteRequest { filename: String },
    /// DATA: one block of the file.
    Data { block: u16, data: Vec<u8> },
    /// ACK: the client has `block`.
    Ack { block: u16 },
    /// ERROR: either side gives up.
    Error { code: ErrorCode, message: String },
    /// OACK: the options the server accepted.
    OptionAck { options: Vec<(String, String)> },
}

/// Why a datagram isn't a TFTP packet.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PacketError {
    #[error("too short")]
    TooShort,
    #[error("unknown opcode {0}")]
    UnknownOpcode(u16),
    #[error("a string isn't NUL-terminated UTF-8")]
    BadString,
    #[error("an option has no value")]
    OddOptions,
}

impl Packet {
    /// Parse one datagram.
    pub fn parse(bytes: &[u8]) -> Result<Packet, PacketError> {
        let (opcode, body) = split_u16(bytes).ok_or(PacketError::TooShort)?;
        match opcode {
            1 | 2 => {
                let mut strings = strings(body)?.into_iter();
                let filename = strings.next().ok_or(PacketError::TooShort)?;
                let mode = strings.next().ok_or(PacketError::TooShort)?;
                if opcode == 2 {
                    return Ok(Packet::WriteRequest { filename });
                }
                let rest: Vec<String> = strings.collect();
                if !rest.len().is_multiple_of(2) {
                    return Err(PacketError::OddOptions);
                }
                let options = rest
                    .chunks(2)
                    .map(|pair| (pair[0].clone(), pair[1].clone()))
                    .collect();
                Ok(Packet::ReadRequest {
                    filename,
                    mode,
                    options,
                })
            }
            3 => {
                let (block, data) = split_u16(body).ok_or(PacketError::TooShort)?;
                Ok(Packet::Data {
                    block,
                    data: data.to_vec(),
                })
            }
            4 => {
                let (block, _) = split_u16(body).ok_or(PacketError::TooShort)?;
                Ok(Packet::Ack { block })
            }
            5 => {
                let (code, rest) = split_u16(body).ok_or(PacketError::TooShort)?;
                // Some clients leave out the message's terminating NUL.
                let message = rest.strip_suffix(&[0]).unwrap_or(rest);
                Ok(Packet::Error {
                    code: code.into(),
                    message: String::from_utf8_lossy(message).into_owned(),
                })
            }
            6 => {
                let strings = strings(body)?;
                if !strings.len().is_multiple_of(2) {
                    return Err(PacketError::OddOptions);
                }
                let options = strings
                    .chunks(2)
                    .map(|pair| (pair[0].clone(), pair[1].clone()))
                    .collect();
                Ok(Packet::OptionAck { options })
            }
            other => Err(PacketError::UnknownOpcode(other)),
        }
    }

    /// Encode for the wire.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        match self {
            Packet::ReadRequest {
                filename,
                mode,
                options,
            } => {
                out.extend_from_slice(&1u16.to_be_bytes());
                push_string(&mut out, filename);
                push_string(&mut out, mode);
                for (name, value) in options {
                    push_string(&mut out, name);
                    push_string(&mut out, value);
                }
            }
            Packet::WriteRequest { filename } => {
                out.extend_from_slice(&2u16.to_be_bytes());
                push_string(&mut out, filename);
                push_string(&mut out, "octet");
            }
            Packet::Data { block, data } => {
                out.extend_from_slice(&3u16.to_be_bytes());
                out.extend_from_slice(&block.to_be_bytes());
                out.extend_from_slice(data);
            }
            Packet::Ack { block } => {
                out.extend_from_slice(&4u16.to_be_bytes());
                out.extend_from_slice(&block.to_be_bytes());
            }
            Packet::Error { code, message } => {
                out.extend_from_slice(&5u16.to_be_bytes());
                out.extend_from_slice(&u16::from(*code).to_be_bytes());
                push_string(&mut out, message);
            }
            Packet::OptionAck { options } => {
                out.extend_from_slice(&6u16.to_be_bytes());
                for (name, value) in options {
                    push_string(&mut out, name);
                    push_string(&mut out, value);
                }
            }
        }
        out
    }
}

fn split_u16(bytes: &[u8]) -> Option<(u16, &[u8])> {
    let (head, rest) = bytes.split_first_chunk::<2>()?;
    Some((u16::from_be_bytes(*head), rest))
}

fn strings(bytes: &[u8]) -> Result<Vec<String>, PacketError> {
    let Some(body) = bytes.strip_suffix(&[0]) else {
        return Err(PacketError::BadString);
    };
    body.split(|b| *b == 0)
        .map(|s| String::from_utf8(s.to_vec()).map_err(|_| PacketError::BadString))
        .collect()
}

fn push_string(out: &mut Vec<u8>, value: &str) {
    out.extend_from_slice(value.as_bytes());
    out.push(0);
}

/// The options agreed for one transfer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Negotiated {
    /// Bytes per DATA block.
    pub block_size: u16,
    /// How long to wait for each acknowledgement.
    pub timeout: Duration,
    /// The options to acknowledge, in the client's order; empty means no
    /// OACK, and the transfer starts straight with block 1.
    pub acknowledged: Vec<(String, String)>,
}

/// Answer a read request's options for a file of `size` bytes, accepting
/// block sizes up to `max_block_size`. Unknown or invalid options are left
/// out of the OACK, which RFC 2347 says means "not accepted".
pub fn negotiate(requested: &[(String, String)], size: u64, max_block_size: u16) -> Negotiated {
    let mut agreed = Negotiated {
        block_size: DEFAULT_BLOCK_SIZE,
        timeout: DEFAULT_TIMEOUT,
        acknowledged: Vec::new(),
    };
    for (name, value) in requested {
        let lower = name.to_ascii_lowercase();
        match lower.as_str() {
            "blksize" => {
                if let Ok(asked @ 8..=65464) = value.parse::<u16>() {
                    agreed.block_size = asked.min(max_block_size);
                    agreed
                        .acknowledged
                        .push((name.clone(), agreed.block_size.to_string()));
                }
            }
            "tsize" => agreed.acknowledged.push((name.clone(), size.to_string())),
            "timeout" => {
                if let Ok(seconds @ 1..=255) = value.parse::<u8>() {
                    agreed.timeout = Duration::from_secs(u64::from(seconds));
                    agreed.acknowledged.push((name.clone(), value.clone()));
                }
            }
            _ => {}
        }
    }
    agreed
}

/// What the transfer wants done next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Step {
    /// Send this packet and wait for the next acknowledgement.
    Send(Vec<u8>),
    /// Nothing to do: a stray or duplicate packet. Keep waiting.
    Wait,
    /// The client has every block.
    Done,
    /// The client sent an ERROR.
    ClientAborted {
        code: ErrorCode,
        message: String,
        /// True if it came in answer to our OACK: normal for UEFI firmware,
        /// which asks for `tsize` alone, then asks again for the file.
        after_options: bool,
    },
    /// The retries ran out.
    GaveUp {
        /// True if no block was ever acknowledged: the client went away
        /// after the OACK, which is also normal.
        after_options: bool,
    },
}

/// One file being sent: the sender's half of RFC 1350's lock-step.
#[derive(Debug)]
pub struct Transfer {
    data: Arc<[u8]>,
    block_size: usize,
    /// The block awaiting acknowledgement; 0 while the OACK is.
    current: u64,
    last_sent: Vec<u8>,
    retries: u32,
    max_retries: u32,
}

impl Transfer {
    /// Start sending `data`, returning the transfer and its first packet:
    /// the OACK if there are options to acknowledge, else block 1.
    pub fn start(
        data: Arc<[u8]>,
        negotiated: &Negotiated,
        max_retries: u32,
    ) -> (Transfer, Vec<u8>) {
        let mut transfer = Transfer {
            data,
            block_size: usize::from(negotiated.block_size.max(1)),
            current: 0,
            last_sent: Vec::new(),
            retries: 0,
            max_retries,
        };
        let first = if negotiated.acknowledged.is_empty() {
            transfer.next_block()
        } else {
            Packet::OptionAck {
                options: negotiated.acknowledged.clone(),
            }
            .encode()
        };
        transfer.last_sent = first.clone();
        (transfer, first)
    }

    /// How many DATA blocks the file takes. A file that fills its last
    /// block exactly ends with an empty one, so the client knows it's over.
    pub fn total_blocks(&self) -> u64 {
        self.data.len() as u64 / self.block_size as u64 + 1
    }

    /// React to a packet from the client.
    pub fn on_packet(&mut self, packet: &Packet) -> Step {
        match packet {
            // Block numbers wrap at 65535; compare on the wire's 16 bits.
            Packet::Ack { block } if *block == self.current as u16 => {
                if self.current == self.total_blocks() {
                    return Step::Done;
                }
                self.retries = 0;
                let next = self.next_block();
                self.last_sent = next.clone();
                Step::Send(next)
            }
            Packet::Error { code, message } => Step::ClientAborted {
                code: *code,
                message: message.clone(),
                after_options: self.current == 0,
            },
            // An old ACK, sent again because ours crossed it: resending
            // here would double every later block (the Sorcerer's
            // Apprentice bug, RFC 1123 4.2.3.1). The timeout resends.
            _ => Step::Wait,
        }
    }

    /// The acknowledgement didn't come in time.
    pub fn on_timeout(&mut self) -> Step {
        self.retries += 1;
        if self.retries > self.max_retries {
            return Step::GaveUp {
                after_options: self.current == 0,
            };
        }
        Step::Send(self.last_sent.clone())
    }

    fn next_block(&mut self) -> Vec<u8> {
        self.current += 1;
        let start = ((self.current - 1) as usize).saturating_mul(self.block_size);
        let start = start.min(self.data.len());
        let end = start.saturating_add(self.block_size).min(self.data.len());
        Packet::Data {
            block: self.current as u16,
            data: self.data[start..end].to_vec(),
        }
        .encode()
    }
}

/// The files the TFTP server hands out, by the name clients ask for.
pub type TftpFiles = HashMap<String, Arc<[u8]>>;

/// Settings for the TFTP server; tests shorten them.
#[derive(Debug, Clone, Copy)]
pub struct TftpSettings {
    /// The largest block size agreed.
    pub max_block_size: u16,
    /// Resends per block before giving up.
    pub retries: u32,
    /// The wait for an acknowledgement when the client sets no timeout.
    pub default_timeout: Duration,
}

impl Default for TftpSettings {
    fn default() -> Self {
        TftpSettings {
            max_block_size: MAX_BLOCK_SIZE,
            retries: DEFAULT_RETRIES,
            default_timeout: DEFAULT_TIMEOUT,
        }
    }
}

/// What to do with a request on the main port.
#[derive(Debug, Clone, PartialEq)]
pub enum Request {
    /// Send this file, with these options.
    Send {
        filename: String,
        data: Arc<[u8]>,
        negotiated: Negotiated,
    },
    /// Answer with this ERROR packet.
    Refuse(Packet),
    /// Not a request; ignore it.
    Ignore,
}

/// Decide what to do with a datagram on the TFTP port.
pub fn route(bytes: &[u8], files: &TftpFiles, settings: &TftpSettings) -> Request {
    let refuse = |code, message: &str| {
        Request::Refuse(Packet::Error {
            code,
            message: message.to_string(),
        })
    };
    match Packet::parse(bytes) {
        Ok(Packet::ReadRequest {
            filename,
            mode,
            options,
        }) => {
            if !mode.eq_ignore_ascii_case("octet") {
                return refuse(ErrorCode::Undefined, "only octet mode is served");
            }
            // Some firmware asks for "/boot.ipxe".
            let name = filename.trim_start_matches('/');
            let Some(data) = files.get(name) else {
                return refuse(ErrorCode::FileNotFound, "file not found");
            };
            let mut negotiated = negotiate(&options, data.len() as u64, settings.max_block_size);
            if !negotiated
                .acknowledged
                .iter()
                .any(|(n, _)| n.eq_ignore_ascii_case("timeout"))
            {
                negotiated.timeout = settings.default_timeout;
            }
            Request::Send {
                filename: name.to_string(),
                data: data.clone(),
                negotiated,
            }
        }
        Ok(Packet::WriteRequest { .. }) => refuse(ErrorCode::AccessViolation, "read-only server"),
        _ => Request::Ignore,
    }
}

/// How a transfer ended, for the log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Every block acknowledged.
    Sent { bytes: usize, block_size: u16 },
    /// The client stopped after the OACK: a size probe, not a failure.
    Probed,
    /// The client sent an ERROR mid-transfer.
    Aborted { message: String },
    /// No acknowledgement after every retry.
    TimedOut,
}

/// Serve TFTP read requests on `socket` until it fails, starting one task
/// per transfer. Each finished transfer goes to `log` with the client's
/// address and the file name.
pub async fn serve<L>(
    socket: UdpSocket,
    files: Arc<TftpFiles>,
    settings: TftpSettings,
    log: L,
) -> std::io::Result<()>
where
    L: Fn(SocketAddr, &str, Outcome) + Clone + Send + 'static,
{
    let local = socket.local_addr()?.ip();
    let mut buffer = vec![0u8; 1500];
    loop {
        let (len, peer) = socket.recv_from(&mut buffer).await?;
        match route(&buffer[..len], &files, &settings) {
            Request::Send {
                filename,
                data,
                negotiated,
            } => {
                let log = log.clone();
                tokio::spawn(async move {
                    let outcome = send(local, peer, data, &negotiated, settings.retries).await;
                    log(peer, &filename, outcome);
                });
            }
            Request::Refuse(error) => {
                let _ = socket.send_to(&error.encode(), peer).await;
            }
            Request::Ignore => {}
        }
    }
}

/// Send one file to `peer` from a new socket on `local`.
async fn send(
    local: IpAddr,
    peer: SocketAddr,
    data: Arc<[u8]>,
    negotiated: &Negotiated,
    retries: u32,
) -> Outcome {
    let bytes = data.len();
    let Ok(socket) = UdpSocket::bind(SocketAddr::new(local, 0)).await else {
        return Outcome::TimedOut;
    };
    // Connected: the kernel drops datagrams from anyone else.
    if socket.connect(peer).await.is_err() {
        return Outcome::TimedOut;
    }
    let (mut transfer, first) = Transfer::start(data, negotiated, retries);
    let mut pending = Some(first);
    let mut buffer = vec![0u8; 1500];
    loop {
        if let Some(packet) = pending.take()
            && socket.send(&packet).await.is_err()
        {
            return Outcome::TimedOut;
        }
        let step = match tokio::time::timeout(negotiated.timeout, socket.recv(&mut buffer)).await {
            Err(_) => transfer.on_timeout(),
            Ok(Err(_)) => transfer.on_timeout(),
            Ok(Ok(len)) => match Packet::parse(&buffer[..len]) {
                Ok(packet) => transfer.on_packet(&packet),
                Err(_) => Step::Wait,
            },
        };
        match step {
            Step::Send(packet) => pending = Some(packet),
            Step::Wait => {}
            Step::Done => {
                return Outcome::Sent {
                    bytes,
                    block_size: negotiated.block_size,
                };
            }
            Step::ClientAborted {
                after_options: true,
                ..
            }
            | Step::GaveUp {
                after_options: true,
            } => return Outcome::Probed,
            Step::ClientAborted { message, .. } => return Outcome::Aborted { message },
            Step::GaveUp { .. } => return Outcome::TimedOut,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(n, v)| (n.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn a_read_request_with_options_parses_and_round_trips() {
        let bytes = b"\x00\x01boot.ipxe\x00octet\x00blksize\x001468\x00tsize\x000\x00";
        let packet = Packet::parse(bytes).unwrap();
        assert_eq!(
            packet,
            Packet::ReadRequest {
                filename: "boot.ipxe".into(),
                mode: "octet".into(),
                options: options(&[("blksize", "1468"), ("tsize", "0")]),
            }
        );
        assert_eq!(packet.encode(), bytes);
    }

    #[test]
    fn every_packet_kind_round_trips() {
        for packet in [
            Packet::Data {
                block: 65535,
                data: vec![1, 2, 3],
            },
            Packet::Data {
                block: 1,
                data: vec![],
            },
            Packet::Ack { block: 7 },
            Packet::Error {
                code: ErrorCode::FileNotFound,
                message: "file not found".into(),
            },
            Packet::OptionAck {
                options: options(&[("tsize", "4096")]),
            },
            Packet::WriteRequest {
                filename: "x".into(),
            },
        ] {
            assert_eq!(Packet::parse(&packet.encode()).unwrap(), packet);
        }
    }

    #[test]
    fn malformed_packets_are_refused() {
        assert_eq!(Packet::parse(b""), Err(PacketError::TooShort));
        assert_eq!(Packet::parse(b"\x00"), Err(PacketError::TooShort));
        assert_eq!(
            Packet::parse(b"\x00\x09"),
            Err(PacketError::UnknownOpcode(9))
        );
        assert_eq!(Packet::parse(b"\x00\x01file"), Err(PacketError::BadString));
        assert_eq!(
            Packet::parse(b"\x00\x01file\x00"),
            Err(PacketError::TooShort)
        );
        assert_eq!(
            Packet::parse(b"\x00\x01f\x00octet\x00blksize\x00"),
            Err(PacketError::OddOptions)
        );
        assert_eq!(Packet::parse(b"\x00\x04\x00"), Err(PacketError::TooShort));
        assert_eq!(
            Packet::parse(b"\x00\x01\xff\x00octet\x00"),
            Err(PacketError::BadString)
        );
    }

    #[test]
    fn an_error_without_its_trailing_nul_still_parses() {
        assert_eq!(
            Packet::parse(b"\x00\x05\x00\x08aborted").unwrap(),
            Packet::Error {
                code: ErrorCode::OptionsRefused,
                message: "aborted".into()
            }
        );
    }

    #[test]
    fn blksize_is_capped_tsize_answered_and_timeout_echoed() {
        let agreed = negotiate(
            &options(&[("BLKSIZE", "65464"), ("tsize", "0"), ("timeout", "4")]),
            1_000_000,
            MAX_BLOCK_SIZE,
        );
        assert_eq!(agreed.block_size, 1468);
        assert_eq!(agreed.timeout, Duration::from_secs(4));
        assert_eq!(
            agreed.acknowledged,
            options(&[("BLKSIZE", "1468"), ("tsize", "1000000"), ("timeout", "4")])
        );
    }

    #[test]
    fn a_smaller_blksize_is_taken_as_asked() {
        let agreed = negotiate(&options(&[("blksize", "1024")]), 10, MAX_BLOCK_SIZE);
        assert_eq!(agreed.block_size, 1024);
        assert_eq!(agreed.acknowledged, options(&[("blksize", "1024")]));
    }

    #[test]
    fn invalid_and_unknown_options_are_left_out() {
        let agreed = negotiate(
            &options(&[
                ("blksize", "7"),
                ("blksize", "65465"),
                ("timeout", "0"),
                ("timeout", "256"),
                ("windowsize", "4"),
                ("multicast", ""),
            ]),
            10,
            MAX_BLOCK_SIZE,
        );
        assert_eq!(agreed.block_size, DEFAULT_BLOCK_SIZE);
        assert_eq!(agreed.timeout, DEFAULT_TIMEOUT);
        assert!(agreed.acknowledged.is_empty());
    }

    fn data(len: usize) -> Arc<[u8]> {
        (0..len)
            .map(|i| (i % 251) as u8)
            .collect::<Vec<u8>>()
            .into()
    }

    fn plain(block_size: u16) -> Negotiated {
        Negotiated {
            block_size,
            timeout: DEFAULT_TIMEOUT,
            acknowledged: Vec::new(),
        }
    }

    fn data_block(bytes: &[u8]) -> (u16, Vec<u8>) {
        match Packet::parse(bytes).unwrap() {
            Packet::Data { block, data } => (block, data),
            other => panic!("not data: {other:?}"),
        }
    }

    fn sent(step: Step) -> Vec<u8> {
        match step {
            Step::Send(bytes) => bytes,
            other => panic!("expected a packet, got {other:?}"),
        }
    }

    #[test]
    fn without_options_the_transfer_starts_with_block_one_and_ends_on_a_short_block() {
        let (mut transfer, first) = Transfer::start(data(1000), &plain(512), 3);
        assert_eq!(data_block(&first), (1, data(1000)[..512].to_vec()));
        let second = sent(transfer.on_packet(&Packet::Ack { block: 1 }));
        assert_eq!(data_block(&second), (2, data(1000)[512..].to_vec()));
        assert_eq!(transfer.on_packet(&Packet::Ack { block: 2 }), Step::Done);
    }

    #[test]
    fn a_file_filling_its_last_block_ends_with_an_empty_one() {
        let (mut transfer, _) = Transfer::start(data(1024), &plain(512), 3);
        assert_eq!(transfer.total_blocks(), 3);
        sent(transfer.on_packet(&Packet::Ack { block: 1 }));
        let last = sent(transfer.on_packet(&Packet::Ack { block: 2 }));
        assert_eq!(data_block(&last), (3, vec![]));
        assert_eq!(transfer.on_packet(&Packet::Ack { block: 3 }), Step::Done);
    }

    #[test]
    fn an_empty_file_is_one_empty_block() {
        let (mut transfer, first) = Transfer::start(data(0), &plain(512), 3);
        assert_eq!(data_block(&first), (1, vec![]));
        assert_eq!(transfer.on_packet(&Packet::Ack { block: 1 }), Step::Done);
    }

    #[test]
    fn with_options_the_oack_comes_first_and_ack_zero_starts_the_data() {
        let negotiated = negotiate(&options(&[("blksize", "1468")]), 3000, MAX_BLOCK_SIZE);
        let (mut transfer, first) = Transfer::start(data(3000), &negotiated, 3);
        assert_eq!(
            Packet::parse(&first).unwrap(),
            Packet::OptionAck {
                options: options(&[("blksize", "1468")])
            }
        );
        let block = sent(transfer.on_packet(&Packet::Ack { block: 0 }));
        assert_eq!(data_block(&block).1.len(), 1468);
    }

    #[test]
    fn a_duplicate_ack_waits_instead_of_resending() {
        let (mut transfer, _) = Transfer::start(data(2000), &plain(512), 3);
        sent(transfer.on_packet(&Packet::Ack { block: 1 }));
        assert_eq!(transfer.on_packet(&Packet::Ack { block: 1 }), Step::Wait);
        assert_eq!(transfer.on_packet(&Packet::Ack { block: 9 }), Step::Wait);
        assert_eq!(
            transfer.on_packet(&Packet::Data {
                block: 1,
                data: vec![]
            }),
            Step::Wait
        );
    }

    #[test]
    fn a_timeout_resends_the_last_packet_until_the_retries_run_out() {
        let (mut transfer, first) = Transfer::start(data(2000), &plain(512), 2);
        assert_eq!(sent(transfer.on_timeout()), first);
        assert_eq!(sent(transfer.on_timeout()), first);
        assert_eq!(
            transfer.on_timeout(),
            Step::GaveUp {
                after_options: false
            }
        );
    }

    #[test]
    fn an_acknowledged_block_resets_the_retries() {
        let (mut transfer, _) = Transfer::start(data(2000), &plain(512), 1);
        sent(transfer.on_timeout());
        let second = sent(transfer.on_packet(&Packet::Ack { block: 1 }));
        assert_eq!(sent(transfer.on_timeout()), second);
    }

    #[test]
    fn a_client_that_stops_after_the_oack_is_a_probe_not_a_failure() {
        let negotiated = negotiate(&options(&[("tsize", "0")]), 3000, MAX_BLOCK_SIZE);
        let (mut transfer, _) = Transfer::start(data(3000), &negotiated, 0);
        assert_eq!(
            transfer.on_packet(&Packet::Error {
                code: ErrorCode::OptionsRefused,
                message: "".into()
            }),
            Step::ClientAborted {
                code: ErrorCode::OptionsRefused,
                message: "".into(),
                after_options: true
            }
        );
        assert_eq!(
            transfer.on_timeout(),
            Step::GaveUp {
                after_options: true
            }
        );
    }

    #[test]
    fn block_numbers_wrap_past_65535() {
        let (mut transfer, _) = Transfer::start(data(70_000), &plain(1), 0);
        for block in 1..=65535u32 {
            sent(transfer.on_packet(&Packet::Ack {
                block: block as u16,
            }));
        }
        // Block 65536 went out as 0, and its ACK says 0.
        let next = sent(transfer.on_packet(&Packet::Ack { block: 0 }));
        assert_eq!(data_block(&next).0, 1);
    }

    fn files() -> TftpFiles {
        TftpFiles::from([("boot.ipxe".to_string(), data(10))])
    }

    #[test]
    fn requests_for_served_files_are_sent_and_others_refused() {
        let settings = TftpSettings::default();
        let rrq = |name: &str, mode: &str| {
            Packet::ReadRequest {
                filename: name.into(),
                mode: mode.into(),
                options: vec![],
            }
            .encode()
        };
        assert!(matches!(
            route(&rrq("/boot.ipxe", "OCTET"), &files(), &settings),
            Request::Send { ref filename, .. } if filename == "boot.ipxe"
        ));
        assert!(matches!(
            route(&rrq("../etc/passwd", "octet"), &files(), &settings),
            Request::Refuse(Packet::Error {
                code: ErrorCode::FileNotFound,
                ..
            })
        ));
        assert!(matches!(
            route(&rrq("boot.ipxe", "netascii"), &files(), &settings),
            Request::Refuse(Packet::Error {
                code: ErrorCode::Undefined,
                ..
            })
        ));
        let write = Packet::WriteRequest {
            filename: "boot.ipxe".into(),
        };
        assert!(matches!(
            route(&write.encode(), &files(), &settings),
            Request::Refuse(Packet::Error {
                code: ErrorCode::AccessViolation,
                ..
            })
        ));
        assert_eq!(
            route(b"\x00\x04\x00\x01", &files(), &settings),
            Request::Ignore
        );
        assert_eq!(route(b"junk", &files(), &settings), Request::Ignore);
    }

    /// A client that asks for `name` with `blksize`, acknowledges every
    /// block, and returns the file and the block size it was given.
    async fn download(server: SocketAddr, name: &str, blksize: &str) -> (Vec<u8>, usize) {
        let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let request = Packet::ReadRequest {
            filename: name.into(),
            mode: "octet".into(),
            options: options(&[("blksize", blksize), ("tsize", "0")]),
        };
        client.send_to(&request.encode(), server).await.unwrap();
        let mut buffer = vec![0u8; 70_000];
        let mut file = Vec::new();
        let mut block_size = 512;
        loop {
            let (len, from) =
                tokio::time::timeout(Duration::from_secs(5), client.recv_from(&mut buffer))
                    .await
                    .unwrap()
                    .unwrap();
            assert_ne!(from, server, "data must come from the transfer's own port");
            match Packet::parse(&buffer[..len]).unwrap() {
                Packet::OptionAck { options } => {
                    let agreed = options.iter().find(|(n, _)| n == "blksize").unwrap();
                    block_size = agreed.1.parse().unwrap();
                    client
                        .send_to(&Packet::Ack { block: 0 }.encode(), from)
                        .await
                        .unwrap();
                }
                Packet::Data { block, data } => {
                    file.extend_from_slice(&data);
                    client
                        .send_to(&Packet::Ack { block }.encode(), from)
                        .await
                        .unwrap();
                    if data.len() < block_size {
                        return (file, block_size);
                    }
                }
                other => panic!("unexpected {other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn a_client_downloads_a_file_over_loopback_with_a_negotiated_block_size() {
        let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let server = socket.local_addr().unwrap();
        let contents = data(100_000);
        let files = Arc::new(TftpFiles::from([(
            "ipxe-x86_64.efi".to_string(),
            contents.clone(),
        )]));
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let log = move |peer: SocketAddr, name: &str, outcome: Outcome| {
            let _ = tx.send((peer, name.to_string(), outcome));
        };
        let task = tokio::spawn(serve(socket, files, TftpSettings::default(), log));

        let (file, block_size) = download(server, "ipxe-x86_64.efi", "65464").await;
        assert_eq!(block_size, 1468);
        assert_eq!(&file[..], &contents[..]);
        let (_, name, outcome) = rx.recv().await.unwrap();
        assert_eq!(name, "ipxe-x86_64.efi");
        assert_eq!(
            outcome,
            Outcome::Sent {
                bytes: 100_000,
                block_size: 1468
            }
        );
        task.abort();
    }

    #[tokio::test]
    async fn a_client_that_vanishes_after_the_oack_counts_as_a_probe() {
        let socket = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let server = socket.local_addr().unwrap();
        let files = Arc::new(files());
        let settings = TftpSettings {
            retries: 1,
            default_timeout: Duration::from_millis(20),
            ..TftpSettings::default()
        };
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let log = move |_: SocketAddr, _: &str, outcome: Outcome| {
            let _ = tx.send(outcome);
        };
        let task = tokio::spawn(serve(socket, files, settings, log));
        let client = UdpSocket::bind("127.0.0.1:0").await.unwrap();
        let request = Packet::ReadRequest {
            filename: "boot.ipxe".into(),
            mode: "octet".into(),
            options: options(&[("tsize", "0")]),
        };
        client.send_to(&request.encode(), server).await.unwrap();
        assert_eq!(rx.recv().await.unwrap(), Outcome::Probed);
        task.abort();
    }
}
