//! Advisory file locks that release when their guard is dropped.
//!
//! Every file lock in Reliaburger goes through [`FileLock`]. The standard
//! library's `File::try_lock` is an `flock` on Unix, and an `flock` belongs to
//! the open file description, not to our descriptor. A child process that
//! another thread is halfway through spawning holds a copy of every descriptor
//! until its `exec` closes the close-on-exec ones, so closing our file alone
//! can leave the lock held by that copy for a moment. The next lock taken
//! straight after was refused as busy: #285, #497, #500, #519 and #606 were
//! all this. [`FileLock`]'s `Drop` unlocks before the file closes, which
//! releases the lock for every copy at once.
//!
//! The descriptor is also close-on-exec, so no child keeps a copy past its
//! `exec`. The standard library opens every file that way; the guard checks
//! anyway, because a `File` can also come from a raw descriptor.
//!
//! `source_rule` (a test) fails if a lock is taken anywhere else in `src/`,
//! and `clippy.toml` forbids `File`'s lock methods outside this module.

use std::fs::File;
use std::io;
use std::os::fd::AsRawFd;
use std::time::{Duration, Instant};

use nix::fcntl::{FcntlArg, FdFlag, fcntl};

/// How often [`FileLock::lock_within`] retries a busy lock.
const RETRY_INTERVAL: Duration = Duration::from_millis(10);

/// Why a lock wasn't taken.
#[derive(Debug, thiserror::Error)]
pub enum FileLockError {
    /// Another holder has the lock.
    #[error("file lock is held by another owner")]
    Busy,
    /// Locking failed for some other reason.
    #[error(transparent)]
    Io(#[from] io::Error),
}

impl From<FileLockError> for io::Error {
    /// A busy lock becomes [`io::ErrorKind::WouldBlock`], so callers that
    /// retry on that kind keep working.
    fn from(error: FileLockError) -> Self {
        match error {
            FileLockError::Busy => io::Error::new(io::ErrorKind::WouldBlock, error),
            FileLockError::Io(error) => error,
        }
    }
}

/// An exclusive `flock` on an open file, released when dropped.
///
/// Keep the guard alive for as long as the lock must be held. Dropping it
/// unlocks first, then closes the file.
#[derive(Debug)]
#[must_use = "dropping a FileLock releases the lock straight away"]
pub struct FileLock {
    file: File,
}

// The only code in `src/` allowed to call `File`'s lock methods.
#[allow(clippy::disallowed_methods)]
impl FileLock {
    /// Take an exclusive lock on `file` without waiting.
    ///
    /// Returns [`FileLockError::Busy`] if another holder has it. The file is
    /// made close-on-exec first if it isn't already.
    pub fn try_lock(file: File) -> Result<Self, FileLockError> {
        close_on_exec(&file)?;
        // The guard below exists before anything can fail, so its `Drop`
        // always unlocks.
        match file.try_lock() {
            Ok(()) => Ok(Self { file }),
            Err(std::fs::TryLockError::WouldBlock) => Err(FileLockError::Busy),
            Err(std::fs::TryLockError::Error(error)) => Err(FileLockError::Io(error)),
        }
    }

    /// Take an exclusive lock on `file`, retrying a busy lock until `timeout`
    /// has passed.
    ///
    /// Blocks the calling thread, so async callers run it in
    /// `spawn_blocking`. Polling rather than a blocking `flock` keeps the
    /// wait bounded.
    pub fn lock_within(file: File, timeout: Duration) -> Result<Self, FileLockError> {
        let deadline = Instant::now() + timeout;
        close_on_exec(&file)?;
        loop {
            match file.try_lock() {
                Ok(()) => return Ok(Self { file }),
                Err(std::fs::TryLockError::WouldBlock) if Instant::now() < deadline => {
                    std::thread::sleep(RETRY_INTERVAL);
                }
                Err(std::fs::TryLockError::WouldBlock) => return Err(FileLockError::Busy),
                Err(std::fs::TryLockError::Error(error)) => return Err(FileLockError::Io(error)),
            }
        }
    }

    /// The locked file, for reading its metadata or syncing it.
    pub fn file(&self) -> &File {
        &self.file
    }

    /// Whether `path` still names the locked file.
    ///
    /// A lock file that may be deleted (see [`Self::remove`]) needs this
    /// check after every lock: a holder that opened the old file before it
    /// was unlinked would otherwise lock a file nobody else can find, beside
    /// a newcomer locking its replacement. On `false` the caller drops this
    /// lock and opens the path again.
    pub fn still_names(&self, path: &std::path::Path) -> io::Result<bool> {
        use std::os::unix::fs::MetadataExt;
        let held = self.file.metadata()?;
        match std::fs::symlink_metadata(path) {
            Ok(named) => Ok(named.dev() == held.dev() && named.ino() == held.ino()),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(error),
        }
    }

    /// Delete the lock file at `path` while still holding it, then release.
    ///
    /// Safe only when every locker of `path` checks [`Self::still_names`]
    /// after locking: a waiter that opened this file then finds it unlinked
    /// and retries against a fresh one.
    pub fn remove(self, path: &std::path::Path) -> io::Result<()> {
        if self.still_names(path)? {
            match std::fs::remove_file(path) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
        }
        drop(self);
        Ok(())
    }
}

#[allow(clippy::disallowed_methods)]
impl Drop for FileLock {
    fn drop(&mut self) {
        // Nothing useful can be done with a failed unlock: closing the
        // descriptor straight after still releases the lock eventually.
        let _ = self.file.unlock();
    }
}

/// Set `FD_CLOEXEC` on `file` unless it's already set.
fn close_on_exec(file: &File) -> io::Result<()> {
    let descriptor = file.as_raw_fd();
    let flags = FdFlag::from_bits_truncate(fcntl(descriptor, FcntlArg::F_GETFD)?);
    if !flags.contains(FdFlag::FD_CLOEXEC) {
        fcntl(descriptor, FcntlArg::F_SETFD(flags | FdFlag::FD_CLOEXEC))?;
    }
    Ok(())
}

/// Threads that spawn `true` in a loop until dropped, for tests that take a
/// lock beside process spawning: each spawn copies every open descriptor
/// into a child that holds it until its `exec`.
#[cfg(test)]
pub(crate) struct SpawningThreads {
    spawning: std::sync::Arc<std::sync::atomic::AtomicBool>,
    threads: Vec<std::thread::JoinHandle<()>>,
}

#[cfg(test)]
impl SpawningThreads {
    /// Start `count` threads spawning processes.
    pub(crate) fn start(count: usize) -> Self {
        use std::sync::atomic::{AtomicBool, Ordering};
        let spawning = std::sync::Arc::new(AtomicBool::new(true));
        let threads = (0..count)
            .map(|_| {
                let spawning = std::sync::Arc::clone(&spawning);
                std::thread::spawn(move || {
                    while spawning.load(Ordering::Relaxed) {
                        let _ = std::process::Command::new("true")
                            .stdin(std::process::Stdio::null())
                            .stdout(std::process::Stdio::null())
                            .stderr(std::process::Stdio::null())
                            .status();
                    }
                })
            })
            .collect();
        Self { spawning, threads }
    }
}

#[cfg(test)]
impl Drop for SpawningThreads {
    fn drop(&mut self) {
        self.spawning
            .store(false, std::sync::atomic::Ordering::Relaxed);
        for thread in self.threads.drain(..) {
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
mod source_rule;

#[cfg(test)]
mod tests {
    use super::*;

    fn open(path: &std::path::Path) -> File {
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .unwrap()
    }

    #[test]
    fn a_held_lock_refuses_another_holder_until_dropped() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("held.lock");
        let held = FileLock::try_lock(open(&path)).unwrap();
        assert!(matches!(
            FileLock::try_lock(open(&path)),
            Err(FileLockError::Busy)
        ));
        drop(held);
        let _retaken = FileLock::try_lock(open(&path)).unwrap();
    }

    #[test]
    fn a_waiter_on_a_removed_lock_file_sees_it_no_longer_names_the_path() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("held.lock");
        let held = FileLock::try_lock(open(&path)).unwrap();
        assert!(held.still_names(&path).unwrap());
        // A waiter opened the old file before the holder deleted it.
        let stale = open(&path);
        held.remove(&path).unwrap();
        assert!(!path.exists());
        let stale = FileLock::try_lock(stale).unwrap();
        assert!(!stale.still_names(&path).unwrap());
        let fresh = FileLock::try_lock(open(&path)).unwrap();
        assert!(fresh.still_names(&path).unwrap());
        assert!(!stale.still_names(&path).unwrap());
    }

    #[test]
    fn a_busy_lock_becomes_a_would_block_io_error() {
        let error = io::Error::from(FileLockError::Busy);
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
    }

    #[test]
    fn lock_within_gives_up_after_its_timeout() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("held.lock");
        let _held = FileLock::try_lock(open(&path)).unwrap();
        let started = Instant::now();
        let result = FileLock::lock_within(open(&path), Duration::from_millis(50));
        assert!(matches!(result, Err(FileLockError::Busy)));
        assert!(started.elapsed() >= Duration::from_millis(50));
    }

    #[test]
    fn lock_within_takes_a_lock_released_while_it_waits() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("held.lock");
        let held = FileLock::try_lock(open(&path)).unwrap();
        let releaser = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(30));
            drop(held);
        });
        let _taken = FileLock::lock_within(open(&path), Duration::from_secs(5)).unwrap();
        releaser.join().unwrap();
    }

    #[test]
    fn a_descriptor_opened_without_close_on_exec_gets_it() {
        let directory = tempfile::tempdir().unwrap();
        let file = open(&directory.path().join("inherited.lock"));
        fcntl(file.as_raw_fd(), FcntlArg::F_SETFD(FdFlag::empty())).unwrap();
        let lock = FileLock::try_lock(file).unwrap();
        let flags =
            FdFlag::from_bits_truncate(fcntl(lock.file().as_raw_fd(), FcntlArg::F_GETFD).unwrap());
        assert!(flags.contains(FdFlag::FD_CLOEXEC));
    }

    /// The bug the guard exists for: a lock closed without unlocking stays
    /// held by any child mid-spawn, so taking it again straight after a drop
    /// was refused.
    #[test]
    fn a_dropped_lock_is_retaken_while_other_threads_spawn_processes() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("contended.lock");
        let _spawning = SpawningThreads::start(2);
        let refused = (0..1000)
            .filter(|_| FileLock::try_lock(open(&path)).is_err())
            .count();
        assert_eq!(refused, 0, "locks refused although nobody held them");
    }
}
