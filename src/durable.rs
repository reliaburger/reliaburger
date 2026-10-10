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
    validate_metadata(&file.metadata()?, access)
}

fn validate_metadata(metadata: &std::fs::Metadata, access: Access) -> io::Result<()> {
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

fn open_record(path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK)
        .open(path)
}

/// Records are replaced by atomic rename, and a reader that doesn't hold the
/// writer's lock can open the old file just before the rename. That file then
/// has no links left, which isn't tampering (a hard-linked copy has two or
/// more): open the path again to read the replacement. Bounded, so a path
/// that keeps changing still fails validation rather than spinning.
fn open_validated(
    mut file: File,
    path: &Path,
    access: Access,
    mut before_validation: impl FnMut(&File),
) -> io::Result<File> {
    let mut replacements = 0;
    loop {
        before_validation(&file);
        // One snapshot decides both replacement and privacy. Rechecking link
        // count in validate_file would race another rename after this check.
        let metadata = file.metadata()?;
        if metadata.nlink() == 0 {
            if replacements == 8 {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "record replaced too often while reading",
                ));
            }
            file = open_record(path)?;
            replacements += 1;
        } else {
            validate_metadata(&metadata, access)?;
            return Ok(file);
        }
    }
}

/// Read a whole record of at most `limit` bytes.
///
/// A missing file is `NotFound`; a symlink, wrong file type or wrong privacy is
/// `InvalidData`; a record over the limit is `FileTooLarge`.
pub(crate) fn read_bounded(path: &Path, limit: u64, access: Access) -> io::Result<Vec<u8>> {
    let context =
        |error: io::Error| io::Error::new(error.kind(), format!("{}: {error}", path.display()));
    let file = open_validated(open_record(path)?, path, access, |_| {}).map_err(context)?;
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

    #[test]
    fn a_record_replaced_after_opening_is_read_again_not_refused() {
        use std::os::unix::fs::OpenOptionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("route.json");
        let write = |name: &str, body: &str| {
            let file = dir.path().join(name);
            std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&file)
                .unwrap();
            std::fs::write(&file, body).unwrap();
            file
        };
        std::fs::rename(write("first", "old"), &path).unwrap();
        // A lock-free reader opens the record...
        let opened = super::open_record(&path).unwrap();
        // ...and the writer atomically replaces it before validation.
        std::fs::rename(write("second", "new"), &path).unwrap();
        assert!(super::validate_file(&opened, super::Access::Exclusive).is_err());
        let current =
            super::open_validated(opened, &path, super::Access::Exclusive, |_| {}).unwrap();
        super::validate_file(&current, super::Access::Exclusive).unwrap();
        assert_eq!(std::io::read_to_string(current).unwrap(), "new");
    }

    /// Reading again only follows an atomic replacement. The file it finds
    /// still has to pass every check a first open would.
    #[test]
    fn a_replacement_read_again_is_still_refused_when_unsafe() {
        use std::os::unix::fs::OpenOptionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("route.json");
        let write = |name: &str, body: &str, mode: u32| {
            let file = dir.path().join(name);
            std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(mode)
                .open(&file)
                .unwrap();
            std::fs::write(&file, body).unwrap();
            file
        };
        let replaced_by = |replacement: &Path| {
            std::fs::rename(write("old", "1", 0o600), &path).unwrap();
            let opened = super::open_record(&path).unwrap();
            std::fs::rename(replacement, &path).unwrap();
            super::open_validated(opened, &path, super::Access::Exclusive, |_| {}).and_then(
                |file| super::validate_file(&file, super::Access::Exclusive).map(|_| file),
            )
        };
        let symlink = dir.path().join("symlink");
        std::os::unix::fs::symlink(write("target", "1", 0o600), &symlink).unwrap();
        assert!(replaced_by(&symlink).is_err(), "followed a symlink");
        let linked = write("linked", "1", 0o600);
        std::fs::hard_link(&linked, dir.path().join("second-link")).unwrap();
        assert!(replaced_by(&linked).is_err(), "accepted a hard link");
        let shared = write("shared", "1", 0o644);
        assert!(replaced_by(&shared).is_err(), "accepted mode 0644");
        let corrupt = replaced_by(&write("corrupt", "{not json", 0o600)).unwrap();
        assert!(serde_json::from_reader::<_, u32>(corrupt).is_err());
        // Removed without a replacement: the record is gone, not tampered with.
        std::fs::rename(write("removed", "1", 0o600), &path).unwrap();
        let opened = super::open_record(&path).unwrap();
        std::fs::remove_file(&path).unwrap();
        let error =
            super::open_validated(opened, &path, super::Access::Exclusive, |_| {}).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
    }

    use super::*;

    fn write(path: &Path, bytes: &[u8], mode: u32) {
        let _ = std::fs::remove_file(path);
        std::fs::write(path, bytes).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    #[test]
    fn replacement_after_the_preliminary_check_is_retried_at_validation() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("route.json");
        let replacement = dir.path().join("replacement.json");
        write(&path, b"1", 0o600);
        write(&replacement, b"2", 0o600);
        let opened = open_record(&path).unwrap();
        let mut inspected = 0;
        let current = open_validated(opened, &path, Access::Exclusive, |_| {
            if inspected == 0 {
                std::fs::rename(&replacement, &path).unwrap();
            }
            inspected += 1;
        })
        .unwrap();
        assert_eq!(std::io::read_to_string(current).unwrap(), "2");
    }

    #[test]
    fn continuous_replacement_stops_after_eight_reopens() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("route.json");
        write(&path, b"1", 0o600);
        let mut inspections = 0;
        let error = open_validated(
            open_record(&path).unwrap(),
            &path,
            Access::Exclusive,
            |_| {
                let replacement = dir.path().join(format!("replacement-{inspections}"));
                write(&replacement, b"2", 0o600);
                std::fs::rename(replacement, &path).unwrap();
                inspections += 1;
            },
        )
        .unwrap_err();
        assert_eq!(inspections, 9);
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
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
