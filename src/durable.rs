//! Bounded, symlink-refusing reads of node-local durable records.
//!
//! Every ownership journal and checkpoint on a node is read the same way: open
//! without following a final symlink, insist on a regular file with the expected
//! privacy, refuse anything larger than the record's limit, then read at most
//! one byte past that limit so a file growing underneath us is still caught.
//! Callers keep their own schema and identity checks.

use std::fs::{File, OpenOptions};
use std::io::{self, Read};
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::Path;

use serde::de::DeserializeOwned;

/// Who may have written a record file, checked on the open descriptor.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Access {
    /// Any regular file.
    Regular,
    /// Owned by this user, with no group or other permission bits.
    OwnerOnly,
    /// Owned by this user, mode exactly 0600, and a single hard link.
    Exclusive,
}

/// Refuse an open file that isn't a regular file with the given privacy.
pub(crate) fn validate_file(file: &File, access: Access) -> io::Result<()> {
    let metadata = file.metadata()?;
    let owned = || metadata.uid() == nix::unistd::geteuid().as_raw();
    let valid = metadata.is_file()
        && match access {
            Access::Regular => true,
            Access::OwnerOnly => owned() && metadata.mode() & 0o077 == 0,
            Access::Exclusive => {
                owned() && metadata.mode() & 0o777 == 0o600 && metadata.nlink() == 1
            }
        };
    if !valid {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            match access {
                Access::Regular => "expected a regular file",
                Access::OwnerOnly => "expected a regular file private to this user",
                Access::Exclusive => "expected a single-link mode 0600 file owned by this user",
            },
        ));
    }
    Ok(())
}

/// Refuse a path that isn't a real directory owned by this user with no group
/// or other permission bits. A symlink is refused rather than followed.
pub(crate) fn validate_directory(path: &Path) -> io::Result<()> {
    let metadata = std::fs::symlink_metadata(path)?;
    if !metadata.is_dir()
        || metadata.uid() != nix::unistd::geteuid().as_raw()
        || metadata.mode() & 0o077 != 0
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{} is not a private directory", path.display()),
        ));
    }
    Ok(())
}

/// Read a whole record of at most `limit` bytes.
///
/// A missing file is `NotFound`; a symlink, wrong file type or wrong privacy is
/// `InvalidData`; a record over the limit is `FileTooLarge`.
pub(crate) fn read_bounded(path: &Path, limit: u64, access: Access) -> io::Result<Vec<u8>> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK)
        .open(path)?;
    let context =
        |error: io::Error| io::Error::new(error.kind(), format!("{}: {error}", path.display()));
    validate_file(&file, access).map_err(context)?;
    let too_large = || {
        io::Error::new(
            io::ErrorKind::FileTooLarge,
            format!("{} exceeds {limit} bytes", path.display()),
        )
    };
    if file.metadata()?.len() > limit {
        return Err(too_large());
    }
    let mut bytes = Vec::new();
    file.take(limit + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > limit {
        return Err(too_large());
    }
    Ok(bytes)
}

/// Read and parse a JSON record with [`read_bounded`]'s checks.
pub(crate) fn read_json<T: DeserializeOwned>(
    path: &Path,
    limit: u64,
    access: Access,
) -> io::Result<T> {
    Ok(serde_json::from_slice(&read_bounded(path, limit, access)?)?)
}

/// Like [`read_json`], but a missing file is `None`. Every other failure,
/// including a dangling symlink, still refuses.
pub(crate) fn read_json_if_exists<T: DeserializeOwned>(
    path: &Path,
    limit: u64,
    access: Access,
) -> io::Result<Option<T>> {
    match read_json(path, limit, access) {
        Ok(value) => Ok(Some(value)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    fn write(path: &Path, bytes: &[u8], mode: u32) {
        let _ = std::fs::remove_file(path);
        std::fs::write(path, bytes).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    #[test]
    fn reads_a_record_exactly_at_the_limit() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("record.json");
        write(&path, b"[1,2]", 0o600);
        let value: Vec<u8> = read_json(&path, 5, Access::Exclusive).unwrap();
        assert_eq!(value, vec![1, 2]);
    }

    #[test]
    fn refuses_a_record_one_byte_over_the_limit() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("record.json");
        write(&path, b"[1,2] ", 0o600);
        let error = read_bounded(&path, 5, Access::Regular).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::FileTooLarge);
    }

    #[test]
    fn missing_record_is_none_only_when_asked() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("absent.json");
        let absent: Option<u32> = read_json_if_exists(&path, 64, Access::Regular).unwrap();
        assert!(absent.is_none());
        let error = read_json::<u32>(&path, 64, Access::Regular).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
    }

    #[test]
    fn refuses_a_symlink_even_to_a_valid_record() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target.json");
        write(&target, b"1", 0o600);
        let link = dir.path().join("link.json");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(read_json::<u32>(&link, 64, Access::Regular).is_err());
        assert!(read_json_if_exists::<u32>(&link, 64, Access::Regular).is_err());
    }

    #[test]
    fn refuses_a_directory_in_place_of_a_record() {
        let dir = tempfile::tempdir().unwrap();
        let error = read_bounded(dir.path(), 64, Access::Regular).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn privacy_levels_refuse_what_they_should() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("record.json");
        write(&path, b"1", 0o644);
        assert!(read_json::<u32>(&path, 64, Access::Regular).is_ok());
        assert!(read_json::<u32>(&path, 64, Access::OwnerOnly).is_err());
        write(&path, b"1", 0o400);
        assert!(read_json::<u32>(&path, 64, Access::OwnerOnly).is_ok());
        assert!(read_json::<u32>(&path, 64, Access::Exclusive).is_err());
        write(&path, b"1", 0o600);
        std::fs::hard_link(&path, dir.path().join("second.json")).unwrap();
        assert!(read_json::<u32>(&path, 64, Access::OwnerOnly).is_ok());
        assert!(read_json::<u32>(&path, 64, Access::Exclusive).is_err());
    }

    #[test]
    fn refuses_malformed_json_as_invalid_data() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("record.json");
        write(&path, b"{not json", 0o600);
        let error = read_json::<u32>(&path, 64, Access::Regular).unwrap_err();
        assert!(matches!(
            error.kind(),
            io::ErrorKind::InvalidData | io::ErrorKind::UnexpectedEof
        ));
    }

    #[test]
    fn directory_must_be_private_and_real() {
        let dir = tempfile::tempdir().unwrap();
        let private = dir.path().join("private");
        std::fs::create_dir(&private).unwrap();
        std::fs::set_permissions(&private, std::fs::Permissions::from_mode(0o700)).unwrap();
        validate_directory(&private).unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&private, &link).unwrap();
        assert!(validate_directory(&link).is_err());
        std::fs::set_permissions(&private, std::fs::Permissions::from_mode(0o750)).unwrap();
        assert!(validate_directory(&private).is_err());
    }
}
