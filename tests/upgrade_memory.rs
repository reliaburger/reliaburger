//! #892: what a self-upgrade costs the heap of the node that runs it.
//!
//! In the 0.2.0 soak the kernel killed Bun on a 2 GiB node seconds after an
//! upgrade started: the node fetched the 114 MB binary from its own registry
//! and held it several times over (the registry's read of the blob, the
//! client's response body, a copy of that, and the copy each check read),
//! on top of the memory the node already used.
//!
//! A candidate binary now goes to a private file as it arrives and every
//! check reads it from there, so preparing an upgrade needs the same heap
//! for a large binary as for a small one. This binary counts every heap byte
//! through its own global allocator, so the numbers are the upgrade's
//! allocations, not whatever the allocator keeps mapped.
//!
//! The counters are process-wide, so each test needs its own process, which
//! is how nextest runs them (`make test`).

use std::alloc::{GlobalAlloc, Layout, System};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

use reliaburger::config::node::UpgradeSection;
use reliaburger::upgrade::BinaryVersion;
use reliaburger::upgrade::manager::UpgradeManager;
use reliaburger::upgrade::signing::{self, SignatureEnvelope};
use reliaburger::upgrade::store::BinaryStore;
use reliaburger::upgrade::types::{BinarySource, UpgradeDirective};

struct Counting;

static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

// SAFETY: every call forwards to the system allocator with the caller's own
// layout and pointer; the counters are bookkeeping only.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: forwarded unchanged from our caller, who upholds `alloc`'s contract.
        let pointer = unsafe { System.alloc(layout) };
        if !pointer.is_null() {
            let live = LIVE.fetch_add(layout.size(), Ordering::Relaxed) + layout.size();
            PEAK.fetch_max(live, Ordering::Relaxed);
        }
        pointer
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        // SAFETY: `pointer` came from `alloc` above with this same layout.
        unsafe { System.dealloc(pointer, layout) };
        LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// The candidate's size: big enough that holding it once is unmistakable.
const BINARY_BYTES: usize = 64 * 1024 * 1024;

/// The heap an upgrade may use on top of what was live before it, whatever
/// the binary's size: buffers, a client, a child process, never the binary.
const HEAP_CEILING: usize = 4 * 1024 * 1024;

fn v(s: &str) -> BinaryVersion {
    s.parse().unwrap()
}

struct Fixture {
    _dir: tempfile::TempDir,
    manager: UpgradeManager,
    binary_dir: PathBuf,
    release_pkcs8: Vec<u8>,
    external_pkcs8: Vec<u8>,
    /// Where the candidate is written for a local-file upgrade, or served from.
    candidate: PathBuf,
}

/// A manager over a temp binary dir with throwaway keys and a fake running
/// bun-v0.1.0, as the manager's own unit tests build it.
fn fixture() -> Fixture {
    let dir = tempfile::tempdir().unwrap();
    let binary_dir = dir.path().join("bin");
    std::fs::create_dir_all(&binary_dir).unwrap();
    let (release_pkcs8, release_public) = signing::generate_keypair().unwrap();
    let (external_pkcs8, external_public) = signing::generate_keypair().unwrap();
    let store = BinaryStore::new(binary_dir.clone(), "bun".to_string());
    let stub = SignatureEnvelope {
        schema: 1,
        sha256: signing::sha256_hex(b"old binary"),
        embedded: String::new(),
        external: None,
    };
    store.stage(&v("0.1.0"), b"old binary", &stub).unwrap();
    store.activate(&v("0.1.0")).unwrap();
    let config = UpgradeSection {
        external_signing_key: Some(signing::encode_public_key(&external_public)),
        binary_dir: Some(binary_dir.clone()),
        release_keys_override: Some(vec![signing::encode_public_key(&release_public)]),
        ..UpgradeSection::default()
    };
    let manager = UpgradeManager::new(
        &config,
        &dir.path().join("data"),
        &binary_dir.join("bun"),
        v("0.1.0"),
        vec!["bun".to_string()],
    )
    .unwrap();
    let candidate = dir.path().join("incoming").join("bun-v0.2.0");
    std::fs::create_dir_all(candidate.parent().unwrap()).unwrap();
    Fixture {
        _dir: dir,
        manager,
        binary_dir,
        release_pkcs8,
        external_pkcs8,
        candidate,
    }
}

/// A release-sized candidate that answers `--compatibility` like this
/// build: a shell script padded with a comment. Written to `path`, signed,
/// and dropped, so none of it is live when the measurement starts.
fn write_candidate(fixture: &Fixture, path: &Path) -> (String, String, String) {
    let formats = serde_json::to_string(&reliaburger::compatibility::CURRENT).unwrap();
    let mut binary = format!("#!/bin/sh\nprintf '%s' '{formats}'\nexit 0\n").into_bytes();
    binary.resize(BINARY_BYTES, b'#');
    std::fs::write(path, &binary).unwrap();
    (
        signing::sha256_hex(&binary),
        signing::sign(&fixture.release_pkcs8, &binary).unwrap(),
        signing::sign(&fixture.external_pkcs8, &binary).unwrap(),
    )
}

fn directive(
    signed: (String, String, String),
    source: BinarySource,
    network: bool,
) -> UpgradeDirective {
    let (sha256, embedded, external) = signed;
    UpgradeDirective {
        upgrade_id: "memory".to_string(),
        target_version: v("0.2.0"),
        binary_sha256: sha256,
        embedded_signature: embedded,
        external_signature: Some(external),
        source,
        network_provenance: network,
        allow_downgrade: false,
    }
}

/// A registry that serves the file at `path` for every request, streamed
/// from disk in small pieces, so it adds nothing big to the heap itself.
async fn file_registry(path: PathBuf) -> String {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap().to_string();
    tokio::spawn(async move {
        loop {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let path = path.clone();
            tokio::spawn(async move {
                let mut request = vec![0u8; 8192];
                let _ = socket.read(&mut request).await;
                let mut file = tokio::fs::File::open(&path).await.unwrap();
                let length = file.metadata().await.unwrap().len();
                let head = format!(
                    "HTTP/1.1 200 OK\r\ncontent-length: {length}\r\nconnection: close\r\n\r\n"
                );
                let _ = socket.write_all(head.as_bytes()).await;
                let _ = tokio::io::copy(&mut file, &mut socket).await;
            });
        }
    });
    address
}

/// Run `work` and return the most heap it had live at once above what was
/// live when it started.
async fn peak_during<F: std::future::Future>(work: F) -> (F::Output, usize) {
    let before = LIVE.load(Ordering::Relaxed);
    PEAK.store(before, Ordering::Relaxed);
    let output = work.await;
    let peak = PEAK.load(Ordering::Relaxed).saturating_sub(before);
    (output, peak)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn preparing_a_registry_upgrade_never_holds_the_binary_in_memory() {
    let fixture = fixture();
    let signed = write_candidate(&fixture, &fixture.candidate);
    let registry = file_registry(fixture.candidate.clone()).await;
    let directive = directive(
        signed,
        BinarySource::Pickle {
            registry_address: registry,
        },
        true,
    );

    let (prepared, peak) = peak_during(fixture.manager.prepare(&directive, vec![])).await;

    assert!(prepared.unwrap().is_some());
    assert!(
        peak < HEAP_CEILING,
        "preparing a {BINARY_BYTES}-byte upgrade peaked {peak} bytes above its start"
    );
    let staged = fixture.binary_dir.join("bun-v0.2.0");
    assert_eq!(
        std::fs::metadata(&staged).unwrap().len(),
        BINARY_BYTES as u64
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_leaders_candidate_check_never_holds_the_binary_in_memory() {
    let fixture = fixture();
    let signed = write_candidate(&fixture, &fixture.candidate);
    let registry = file_registry(fixture.candidate.clone()).await;
    let directive = directive(
        signed,
        BinarySource::Pickle {
            registry_address: registry,
        },
        true,
    );

    let (checked, peak) = peak_during(fixture.manager.check_candidate(&directive)).await;

    checked.unwrap();
    assert!(
        peak < HEAP_CEILING,
        "checking a {BINARY_BYTES}-byte candidate peaked {peak} bytes above its start"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn preparing_a_local_file_upgrade_never_holds_the_binary_in_memory() {
    let fixture = fixture();
    let signed = write_candidate(&fixture, &fixture.candidate);
    let directive = directive(
        signed,
        BinarySource::LocalFile {
            path: fixture.candidate.clone(),
        },
        false,
    );

    let (prepared, peak) = peak_during(fixture.manager.prepare(&directive, vec![])).await;

    assert!(prepared.unwrap().is_some());
    assert!(
        peak < HEAP_CEILING,
        "preparing a {BINARY_BYTES}-byte local upgrade peaked {peak} bytes above its start"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn preparing_a_rollback_never_holds_the_binary_in_memory() {
    let fixture = fixture();
    // A signed version older than the running 0.1.0, in the store with its
    // envelope, so the rollback re-verifies it before probing it.
    let older = fixture.binary_dir.join("bun-v0.0.9");
    let (sha256, embedded, _) = write_candidate(&fixture, &older);
    SignatureEnvelope {
        schema: 1,
        sha256,
        embedded,
        external: None,
    }
    .store(&fixture.binary_dir.join("bun-v0.0.9.sig"))
    .unwrap();

    let (prepared, peak) = peak_during(
        fixture
            .manager
            .prepare_rollback(Some(v("0.0.9")), Vec::new()),
    )
    .await;

    assert!(prepared.unwrap().is_some());
    assert!(
        peak < HEAP_CEILING,
        "preparing a rollback to a {BINARY_BYTES}-byte binary peaked {peak} bytes above its start"
    );
}
