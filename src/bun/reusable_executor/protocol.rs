//! Bounded, sequence-bound messages on the host-private executor connection.
use super::ExecutorError;
use crate::grill::oci::OciProcess;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

const FRAME_LIMIT: usize = 65_536;
const STRING_LIMIT: usize = 256;

pub(super) fn command(sequence: u64, process: &OciProcess) -> Result<Vec<u8>, ExecutorError> {
    if sequence == 0
        || process.args.is_empty()
        || process.args.len() > STRING_LIMIT
        || process.env.len() > STRING_LIMIT
        || process.args[0].is_empty()
        || process.cwd.is_empty()
        || process.user.uid >= crate::grill::userns::CONTAINER_ID_COUNT
        || process.user.gid >= crate::grill::userns::CONTAINER_ID_COUNT
    {
        return Err(ExecutorError::Configuration(
            "unsupported command identity or vector",
        ));
    }
    let mut bytes = Vec::with_capacity(256);
    bytes.extend_from_slice(&sequence.to_be_bytes());
    for value in [
        process.args.len() as u32,
        process.env.len() as u32,
        process.user.uid,
        process.user.gid,
    ] {
        bytes.extend_from_slice(&value.to_be_bytes());
    }
    for value in std::iter::once(&process.cwd)
        .chain(&process.args)
        .chain(&process.env)
    {
        if value.contains('\0') || value.len() > FRAME_LIMIT {
            return Err(ExecutorError::Configuration(
                "command string exceeds protocol bounds",
            ));
        }
        bytes.extend_from_slice(&(value.len() as u32).to_be_bytes());
        bytes.extend_from_slice(value.as_bytes());
        if bytes.len() > FRAME_LIMIT {
            return Err(ExecutorError::Configuration(
                "command frame exceeds protocol bounds",
            ));
        }
    }
    if process.env.iter().any(|value| {
        !value
            .split_once('=')
            .is_some_and(|(name, _)| !name.is_empty())
    }) {
        return Err(ExecutorError::Configuration("invalid environment"));
    }
    let mut frame = Vec::with_capacity(bytes.len() + 4);
    frame.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
    frame.extend_from_slice(&bytes);
    Ok(frame)
}

#[derive(Debug, PartialEq)]
pub(super) enum Event {
    Started,
    Output { stream: u8, bytes: Vec<u8> },
    Exited(i32),
    Ready,
    SpawnFailed(u32),
}
pub(super) async fn receive<S: tokio::io::AsyncRead + Unpin>(
    socket: &mut S,
    sequence: u64,
) -> Result<Event, ExecutorError> {
    let kind = socket.read_u8().await?;
    if socket.read_u64().await? != sequence {
        return Err(ExecutorError::Protocol("stale command sequence".into()));
    }
    match kind {
        1 => Ok(Event::Started),
        2 => {
            let stream = socket.read_u8().await?;
            let length = socket.read_u32().await? as usize;
            if !matches!(stream, 1 | 2) || length == 0 || length > 4096 {
                return Err(ExecutorError::Protocol("invalid output frame".into()));
            }
            let mut bytes = vec![0; length];
            socket.read_exact(&mut bytes).await?;
            Ok(Event::Output { stream, bytes })
        }
        3 => Ok(Event::Exited(socket.read_i32().await?)),
        4 => Ok(Event::Ready),
        5 => Ok(Event::SpawnFailed(socket.read_u32().await?)),
        _ => Err(ExecutorError::Protocol("unknown message kind".into())),
    }
}
pub(super) async fn cleanup<S: tokio::io::AsyncWrite + Unpin>(
    socket: &mut S,
    sequence: u64,
) -> Result<(), ExecutorError> {
    socket.write_u8(b'C').await?;
    socket.write_u64(sequence).await?;
    Ok(())
}

#[cfg(target_os = "linux")]
pub(super) fn send_directory(
    socket: &std::os::unix::net::UnixStream,
    directory: &std::fs::File,
) -> Result<(), ExecutorError> {
    use nix::sys::socket::{ControlMessage, MsgFlags, sendmsg};
    use std::os::fd::AsRawFd;
    let descriptors = [directory.as_raw_fd()];
    let marker = [std::io::IoSlice::new(b"F")];
    let result = sendmsg::<()>(
        socket.as_raw_fd(),
        &marker,
        &[ControlMessage::ScmRights(&descriptors)],
        MsgFlags::MSG_NOSIGNAL,
        None,
    )
    .map_err(std::io::Error::from)?;
    if result != 1 {
        return Err(ExecutorError::Protocol(
            "directory transfer was partial".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn stale_and_unbounded_frames_are_refused_before_payload_allocation() {
        let mut stale = &b"\x03\x00\x00\x00\x00\x00\x00\x00\x02"[..];
        assert!(receive(&mut stale, 1).await.is_err());
        let mut huge = &b"\x02\x00\x00\x00\x00\x00\x00\x00\x01\x01\xff\xff\xff\xff"[..];
        assert!(receive(&mut huge, 1).await.is_err());
        let mut exited = &b"\x03\x00\x00\x00\x00\x00\x00\x00\x01\xff\xff\xff\xf7"[..];
        assert_eq!(receive(&mut exited, 1).await.unwrap(), Event::Exited(-9));
    }
    #[tokio::test]
    async fn cleanup_receipt_names_exactly_one_command_sequence() {
        let mut bytes = Vec::new();
        cleanup(&mut bytes, 17).await.unwrap();
        assert_eq!(bytes, [b'C', 0, 0, 0, 0, 0, 0, 0, 17]);
    }
    #[test]
    fn encoding_refuses_privileged_users_embedded_nuls_and_oversized_credentials() {
        let job = toml::from_str("image='fixture:v1'\ncommand=['/bin/true']").unwrap();
        let mut process = crate::grill::oci::generate_job_oci_spec(
            "test",
            "default",
            &job,
            "/sys/fs/cgroup/test",
            None,
        )
        .process;
        assert!(command(1, &process).is_ok());
        process.user.uid = 65536;
        assert!(command(1, &process).is_err());
        process.user.uid = 0;
        process.env = vec!["TOKEN=bad\0value".into()];
        assert!(command(1, &process).is_err());
        process.env = vec![format!("TOKEN={}", "x".repeat(65_536))];
        assert!(command(1, &process).is_err());
    }
}
