//! Power-cut qualification for storage that promises durability.
//!
//! Two fixtures, each run in two phases by
//! `scripts/release/qualify-storage-power-cut.sh` on a disposable Lima VM:
//!
//! - the log/metrics exporter (its `_export_checkpoint.json`, the
//!   cross-process `_export_checkpoint.lock`, and export-then-prune);
//! - the standalone `LocalLeaseStore` (private unique temporaries plus file
//!   and directory syncs);
//! - council backups, which upload a new backup and then prune older ones.
//!
//! `prepare` starts worker processes (re-executions of this test binary) that
//! keep mutating durable state. Every operation they finish is appended to an
//! fsynced ledger before the next one starts. `prepare` returns while the
//! workers are still running, the driver cuts the VM's power at a random
//! moment, and `verify` runs on the next boot. It checks that every operation
//! a ledger acknowledged survived, and that the recovered state is usable.
#![cfg(target_os = "linux")]

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use rand::Rng;
use reliaburger::bun::disk_pressure::check_and_relieve;
use reliaburger::council::backup::{
    BackupConfig, BackupStore, decode_backup, encode_backup, seal_snapshot, unseal_snapshot,
};
use reliaburger::ketchup::export::{CHECKPOINT_FILENAME, ExportCheckpoint, export_logs};
use reliaburger::ketchup::log_store::LogStore;
use reliaburger::ketchup::remote_query::query_remote;
use reliaburger::ketchup::types::LogStream;
use reliaburger::testkit::lease::{LocalLeaseStore, TestLease, TestLeaseState, now_unix_millis};
use sha2::{Digest, Sha256};

const DIRECTORY: &str = "RELIABURGER_POWER_CUT_DIRECTORY";
const PHASE: &str = "RELIABURGER_REBOOT_PHASE";
const ROLE: &str = "RELIABURGER_POWER_CUT_ROLE";
const SCRIPT: &str = "scripts/release/qualify-storage-power-cut.sh";

const EXPORT_TEST: &str = "actual_power_cut_preserves_acknowledged_log_exports";
const LEASE_TEST: &str = "actual_power_cut_preserves_acknowledged_lease_operations";

/// Node prefix the exporters write under, as Bun's node id would be.
const NODE: &str = "node-power-cut";
const EXPORTERS: usize = 3;
const LINES_PER_FILE: usize = 40;
/// Small enough that the pruning exporters delete sources every few files.
const PRESSURE_BYTES: u64 = 16 * 1024;

/// Realistic names: Bun keeps several lease stores side by side in one directory.
const LEASE_STORES: [&str; 3] = [
    "test-leases.json",
    "node-test-leases.json",
    "soak-test-leases.json",
];
const LEASE_OWNER: &str = "token:power-cut";
const LEASE_LIFETIME_MS: u64 = 3_600_000;
const MAX_LIVE_LEASES: usize = 8;

// ---------------------------------------------------------------------------
// Shared fixture plumbing
// ---------------------------------------------------------------------------

fn fixture_directory() -> PathBuf {
    // A missing variable means an automated runner picked this up by
    // mistake. Passing would claim power-cut evidence nobody collected.
    let directory = std::env::var(DIRECTORY).unwrap_or_else(|_| {
        panic!("{DIRECTORY} unset: run this only through {SCRIPT}, which cuts the VM's power")
    });
    PathBuf::from(directory)
}

fn boot_id() -> String {
    std::fs::read_to_string("/proc/sys/kernel/random/boot_id")
        .unwrap()
        .trim()
        .to_string()
}

fn sync_directory(path: &Path) {
    std::fs::File::open(path).unwrap().sync_all().unwrap();
}

fn sha256(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// Append-only record of finished operations, synced line by line.
struct Ledger(std::fs::File);

impl Ledger {
    fn open(path: &Path) -> Self {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .unwrap();
        Self(file)
    }

    fn record(&mut self, line: &str) {
        self.0.write_all(format!("{line}\n").as_bytes()).unwrap();
        self.0.sync_data().unwrap();
    }
}

/// Complete ledger lines. A final line the power cut tore was never
/// acknowledged, so only text up to the last newline counts.
fn ledger_lines(path: &Path) -> Vec<String> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(error) => panic!("read ledger {}: {error}", path.display()),
    };
    let complete = match bytes.iter().rposition(|byte| *byte == b'\n') {
        Some(end) => &bytes[..end],
        None => &[][..],
    };
    let text = std::str::from_utf8(complete)
        .unwrap_or_else(|error| panic!("synced ledger {} is corrupt: {error}", path.display()));
    assert!(
        !text.contains('\0'),
        "synced ledger {} contains zeroed bytes",
        path.display()
    );
    text.lines()
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect()
}

/// Start one worker: this test binary again, detached into its own process
/// group so it outlives the prepare phase's shell session.
#[expect(
    clippy::zombie_processes,
    reason = "workers must outlive prepare; init reaps them once prepare exits"
)]
fn spawn_worker(test: &str, directory: &Path, role: &str) {
    use std::os::unix::process::CommandExt;
    let log = std::fs::File::create(directory.join(format!("{role}.log"))).unwrap();
    let child = Command::new(std::env::current_exe().unwrap())
        .args([
            "--ignored",
            "--exact",
            test,
            "--nocapture",
            "--test-threads=1",
        ])
        .env(DIRECTORY, directory)
        .env(ROLE, role)
        .env_remove(PHASE)
        .stdin(Stdio::null())
        .stdout(log.try_clone().unwrap())
        .stderr(log)
        .process_group(0)
        .spawn()
        .unwrap();
    std::fs::write(
        directory.join(format!("{role}.pid")),
        child.id().to_string(),
    )
    .unwrap();
}

/// Record the boot before any worker starts, so verify can prove the kernel
/// really went away.
fn begin_prepare(directory: &Path) {
    let proof = directory.join("proof.json");
    assert!(
        !proof.exists(),
        "never overwrite earlier power-cut evidence"
    );
    std::fs::write(
        &proof,
        serde_json::to_vec(&serde_json::json!({"boot": boot_id()})).unwrap(),
    )
    .unwrap();
    std::fs::File::open(&proof).unwrap().sync_all().unwrap();
    sync_directory(directory);
}

async fn wait_until(description: &str, mut ready: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(60), async {
        while !ready() {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("workers never reached {description}"));
}

/// The prepare phase finished: workers are live and acknowledging.
fn finish_prepare(directory: &Path) {
    let marker = directory.join("prepared");
    std::fs::write(&marker, boot_id()).unwrap();
    std::fs::File::open(&marker).unwrap().sync_all().unwrap();
    sync_directory(directory);
}

/// The kernel that runs verify must not be the one prepare recorded.
fn assert_rebooted(directory: &Path) -> String {
    let proof: serde_json::Value =
        serde_json::from_slice(&std::fs::read(directory.join("proof.json")).unwrap()).unwrap();
    let boot = boot_id();
    assert_ne!(
        proof["boot"].as_str().unwrap(),
        boot,
        "this is not an actual power cut: the kernel is the same"
    );
    assert!(
        directory.join("prepared").exists(),
        "prepare never confirmed its workers were acknowledging"
    );
    boot
}

fn finish_verify(directory: &Path, boot: &str, summary: serde_json::Value) {
    std::fs::write(
        directory.join("summary.json"),
        serde_json::to_vec_pretty(&summary).unwrap(),
    )
    .unwrap();
    std::fs::write(directory.join("verified-boot"), boot).unwrap();
    println!("{summary}");
}

fn phase() -> String {
    std::env::var(PHASE).unwrap_or_else(|_| panic!("{PHASE} unset: run this through {SCRIPT}"))
}

// ---------------------------------------------------------------------------
// C03: exporter checkpoint, cross-process lock and export-then-prune
// ---------------------------------------------------------------------------

struct ExportLayout {
    source: PathBuf,
    staging: PathBuf,
    reference: PathBuf,
    destination: PathBuf,
    ledgers: PathBuf,
}

impl ExportLayout {
    fn new(root: &Path) -> Self {
        Self {
            source: root.join("source"),
            staging: root.join("staging"),
            reference: root.join("reference"),
            destination: root.join("destination"),
            ledgers: root.join("ledgers"),
        }
    }

    fn destination_url(&self) -> String {
        format!("file://{}", self.destination.display())
    }

    /// Where the exporter puts one generation of a source file.
    fn object(&self, name: &str, sha: &str) -> PathBuf {
        self.destination.join(NODE).join(format!("{sha}-{name}"))
    }
}

/// Flush Parquet through `LogStore` into a private staging directory, keep a
/// synced reference copy, then publish it into the exported directory with an
/// atomic rename, the way Bun's own flush publishes a finished file.
async fn run_generator(layout: &ExportLayout) {
    let mut ledger = Ledger::open(&layout.ledgers.join("generator.ledger"));
    let mut store = LogStore::new(layout.staging.clone());
    for sequence in 0u64.. {
        for line in 0..LINES_PER_FILE {
            store.append_at(
                1_700_000_000 + sequence,
                "power-cut",
                "default",
                LogStream::Stdout,
                &format!("file {sequence} line {line}"),
            );
        }
        store.flush().await.unwrap();
        let mut staged: Vec<_> = std::fs::read_dir(&layout.staging)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .filter(|path| {
                path.extension()
                    .is_some_and(|extension| extension == "parquet")
            })
            .collect();
        assert_eq!(staged.len(), 1, "one flush must stage exactly one file");
        let staged = staged.pop().unwrap();
        let name = staged.file_name().unwrap().to_str().unwrap().to_string();
        let bytes = std::fs::read(&staged).unwrap();
        let sha = sha256(&bytes);
        let reference = layout.reference.join(&name);
        std::fs::write(&reference, &bytes).unwrap();
        std::fs::File::open(&reference).unwrap().sync_all().unwrap();
        sync_directory(&layout.reference);
        ledger.record(&format!("generate {name} {sha}"));
        std::fs::rename(&staged, layout.source.join(&name)).unwrap();
        sync_directory(&layout.source);
        ledger.record(&format!("publish {name}"));
        let pause = rand::thread_rng().gen_range(20..120);
        tokio::time::sleep(Duration::from_millis(pause)).await;
    }
}

/// Exporter 0 behaves like `relish logs-export` (export only); the others
/// behave like Bun's disk-pressure tick (export, then prune).
async fn run_exporter(layout: &ExportLayout, index: usize) {
    let mut ledger = Ledger::open(&layout.ledgers.join(format!("exporter-{index}.ledger")));
    let destination = layout.destination_url();
    let mut checkpoint = ExportCheckpoint::default();
    let mut acknowledged = BTreeSet::new();
    loop {
        let outcome = if index == 0 {
            export_logs(&layout.source, &destination, NODE, &mut checkpoint)
                .await
                .map(|_| ())
                .map_err(|error| error.to_string())
        } else {
            let result = check_and_relieve(
                &layout.source,
                Some(&destination),
                NODE,
                &mut checkpoint,
                PRESSURE_BYTES,
                0,
            )
            .await;
            match result.export_error {
                None => Ok(()),
                Some(error) => Err(error),
            }
        };
        match outcome {
            Ok(()) => {
                for id in &checkpoint.exported_files {
                    if acknowledged.insert(id.clone()) {
                        ledger.record(&format!("ack {id}"));
                    }
                }
            }
            // Another exporter holds the cross-process lock; that is the contract.
            Err(error) if error.contains("busy") => {}
            Err(error) => panic!("exporter {index} failed: {error}"),
        }
        let pause = rand::thread_rng().gen_range(5..60);
        tokio::time::sleep(Duration::from_millis(pause)).await;
    }
}

/// Generated files: name → (sha256, whether the publish rename was acknowledged).
fn generated_files(layout: &ExportLayout) -> BTreeMap<String, (String, bool)> {
    let mut files = BTreeMap::new();
    for line in ledger_lines(&layout.ledgers.join("generator.ledger")) {
        let fields: Vec<&str> = line.split(' ').collect();
        match fields.as_slice() {
            ["generate", name, sha] => {
                files.insert(name.to_string(), (sha.to_string(), false));
            }
            ["publish", name] => {
                files
                    .get_mut(*name)
                    .unwrap_or_else(|| panic!("publish without generate: {line}"))
                    .1 = true;
            }
            _ => panic!("unexpected generator ledger line {line:?}"),
        }
    }
    files
}

fn acknowledged_exports(layout: &ExportLayout) -> BTreeSet<String> {
    let mut acknowledged = BTreeSet::new();
    for index in 0..EXPORTERS {
        for line in ledger_lines(&layout.ledgers.join(format!("exporter-{index}.ledger"))) {
            let id = line
                .strip_prefix("ack ")
                .unwrap_or_else(|| panic!("unexpected exporter ledger line {line:?}"));
            acknowledged.insert(id.to_string());
        }
    }
    acknowledged
}

/// The destination must hold exactly the reference bytes of one durable id.
fn assert_exported(layout: &ExportLayout, id: &str, claim: &str) {
    let (name, sha) = id
        .split_once('@')
        .unwrap_or_else(|| panic!("malformed durable id {id:?}"));
    let reference = std::fs::read(layout.reference.join(name))
        .unwrap_or_else(|error| panic!("{claim} {id} has no reference copy: {error}"));
    assert_eq!(sha256(&reference), sha, "reference copy of {name} is torn");
    let object = layout.object(name, sha);
    let exported = std::fs::read(&object)
        .unwrap_or_else(|error| panic!("{claim} {id} is missing from the destination ({error})"));
    assert!(
        exported == reference,
        "{claim} {id} is torn at the destination: {} bytes, expected {}",
        exported.len(),
        reference.len()
    );
}

fn count_entries(directory: &Path, matches: impl Fn(&str) -> bool) -> usize {
    match std::fs::read_dir(directory) {
        Ok(entries) => entries
            .filter(|entry| {
                matches(
                    entry
                        .as_ref()
                        .unwrap()
                        .file_name()
                        .to_str()
                        .unwrap_or_default(),
                )
            })
            .count(),
        Err(_) => 0,
    }
}

async fn prepare_exports(root: &Path, layout: &ExportLayout) {
    begin_prepare(root);
    for directory in [
        &layout.source,
        &layout.staging,
        &layout.reference,
        &layout.destination,
        &layout.ledgers,
    ] {
        std::fs::create_dir_all(directory).unwrap();
    }
    sync_directory(root);
    spawn_worker(EXPORT_TEST, root, "generator");
    for index in 0..EXPORTERS {
        spawn_worker(EXPORT_TEST, root, &format!("exporter-{index}"));
    }
    wait_until("acknowledged exports", || {
        generated_files(layout).len() >= 3 && acknowledged_exports(layout).len() >= 2
    })
    .await;
    finish_prepare(root);
}

async fn verify_exports(root: &Path, layout: &ExportLayout) {
    let boot = assert_rebooted(root);
    let generated = generated_files(layout);
    let acknowledged = acknowledged_exports(layout);

    // The authoritative checkpoint is readable, not torn.
    let bytes = std::fs::read(layout.source.join(CHECKPOINT_FILENAME))
        .expect("checkpoint missing after acknowledged exports");
    let checkpoint: ExportCheckpoint =
        serde_json::from_slice(&bytes).expect("checkpoint torn by the power cut");

    // Every export an exporter acknowledged is at the destination, byte for byte.
    for id in &acknowledged {
        assert_exported(layout, id, "acknowledged export");
    }
    // Pruning trusts the checkpoint, so every id it lists must be durable too.
    for id in &checkpoint.exported_files {
        assert_exported(layout, id, "checkpointed export");
    }
    // Nothing left the source without reaching the destination.
    let mut pruned = 0;
    for (name, (sha, published)) in &generated {
        match std::fs::read(layout.source.join(name)) {
            Ok(bytes) => assert_eq!(&sha256(&bytes), sha, "source {name} is torn"),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound && *published => {
                pruned += 1;
                assert_exported(layout, &format!("{name}@{sha}"), "pruned source");
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => panic!("read source {name}: {error}"),
        }
    }

    // The cross-process lock died with its holders and is reusable.
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(layout.source.join("_export_checkpoint.lock"))
        .expect("export lock file missing");
    lock.try_lock()
        .expect("export lock still held after the power cut");
    drop(lock);

    // A fresh exporter (with a stale in-memory view) exports new data.
    let mut store = LogStore::new(layout.source.clone());
    for line in 0..LINES_PER_FILE {
        store.append_at(
            1_800_000_000,
            "power-cut",
            "default",
            LogStream::Stdout,
            &format!("after the cut line {line}"),
        );
    }
    store.flush().await.unwrap();
    let mut fresh = ExportCheckpoint::default();
    let result = export_logs(&layout.source, &layout.destination_url(), NODE, &mut fresh)
        .await
        .expect("fresh export after the power cut failed");
    assert!(
        result.files_exported >= 1,
        "fresh export did not ship new data"
    );
    for id in &fresh.exported_files {
        let (name, sha) = id.split_once('@').unwrap();
        let source = std::fs::read(layout.source.join(name)).unwrap();
        assert_eq!(std::fs::read(layout.object(name, sha)).unwrap(), source);
    }

    // Every object at the destination is readable Parquet.
    let node_directory = layout.destination.join(NODE);
    let objects = count_entries(&node_directory, |name| name.ends_with(".parquet"));
    let rows = query_remote(
        node_directory.to_str().unwrap(),
        "SELECT timestamp, app, namespace, stream, line FROM logs",
    )
    .await
    .expect("exported Parquet is unreadable after the power cut");
    assert_eq!(rows.len(), objects * LINES_PER_FILE);

    finish_verify(
        root,
        &boot,
        serde_json::json!({
            "fixture": "exporter",
            "generated": generated.len(),
            "published": generated.values().filter(|(_, published)| *published).count(),
            "acknowledged": acknowledged.len(),
            "checkpointed": checkpoint.exported_files.len(),
            "pruned": pruned,
            "destination_objects": objects,
            "staged_leftovers": count_entries(&node_directory, |name| name.contains('#')),
            "stale_temporaries": count_entries(&layout.source, |name| name.starts_with(".reliaburger-")),
        }),
    );
}

/// Two-phase fixture: the driver must actually cut the disposable VM's power.
#[tokio::test]
#[ignore = "run only through scripts/release/qualify-storage-power-cut.sh on a disposable VM"]
async fn actual_power_cut_preserves_acknowledged_log_exports() {
    let root = fixture_directory();
    let layout = ExportLayout::new(&root);
    if let Ok(role) = std::env::var(ROLE) {
        match role.strip_prefix("exporter-") {
            Some(index) => run_exporter(&layout, index.parse().unwrap()).await,
            None if role == "generator" => run_generator(&layout).await,
            None => panic!("unknown exporter worker role {role}"),
        }
        return;
    }
    match phase().as_str() {
        "prepare" => prepare_exports(&root, &layout).await,
        "verify" => verify_exports(&root, &layout).await,
        other => panic!("invalid power-cut phase {other}"),
    }
}

// ---------------------------------------------------------------------------
// C11: LocalLeaseStore create/renew/release
// ---------------------------------------------------------------------------

/// One lease-store mutation, as recorded in a worker's ledger.
#[derive(Debug, Clone, PartialEq, Eq)]
enum LeaseOperation {
    Create { id: String, expires: u64 },
    Renew { id: String, expires: u64 },
    Clean { id: String },
    Release { id: String },
}

impl LeaseOperation {
    fn encode(&self) -> String {
        match self {
            Self::Create { id, expires } => format!("create {id} {expires}"),
            Self::Renew { id, expires } => format!("renew {id} {expires}"),
            Self::Clean { id } => format!("clean {id}"),
            Self::Release { id } => format!("release {id}"),
        }
    }

    fn decode(text: &str) -> Self {
        let fields: Vec<&str> = text.split(' ').collect();
        match fields.as_slice() {
            ["create", id, expires] => Self::Create {
                id: id.to_string(),
                expires: expires.parse().unwrap(),
            },
            ["renew", id, expires] => Self::Renew {
                id: id.to_string(),
                expires: expires.parse().unwrap(),
            },
            ["clean", id] => Self::Clean { id: id.to_string() },
            ["release", id] => Self::Release { id: id.to_string() },
            _ => panic!("unexpected lease operation {text:?}"),
        }
    }
}

/// What the store must say about one lease: whether cleanup has begun, and
/// its expiry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct LeaseView {
    cleaning: bool,
    expires: u64,
}

type LeaseModel = BTreeMap<String, LeaseView>;

fn apply(model: &mut LeaseModel, operation: &LeaseOperation) {
    match operation {
        LeaseOperation::Create { id, expires } => {
            let previous = model.insert(
                id.clone(),
                LeaseView {
                    cleaning: false,
                    expires: *expires,
                },
            );
            assert!(previous.is_none(), "{id} created twice");
        }
        LeaseOperation::Renew { id, expires } => model.get_mut(id).unwrap().expires = *expires,
        LeaseOperation::Clean { id } => model.get_mut(id).unwrap().cleaning = true,
        LeaseOperation::Release { id } => {
            model.remove(id).unwrap();
        }
    }
}

fn choose_operation(model: &LeaseModel, worker: usize, next: &mut u64) -> LeaseOperation {
    let mut rng = rand::thread_rng();
    let cleaning: Vec<&String> = model
        .iter()
        .filter(|(_, v)| v.cleaning)
        .map(|(k, _)| k)
        .collect();
    let active: Vec<&String> = model
        .iter()
        .filter(|(_, v)| !v.cleaning)
        .map(|(k, _)| k)
        .collect();
    if !cleaning.is_empty() && rng.gen_bool(0.5) {
        let id = cleaning[rng.gen_range(0..cleaning.len())].clone();
        return LeaseOperation::Release { id };
    }
    if active.len() < 3 || (model.len() < MAX_LIVE_LEASES && rng.gen_bool(0.3)) {
        *next += 1;
        return LeaseOperation::Create {
            id: format!("w{worker}-{next}"),
            expires: now_unix_millis() + LEASE_LIFETIME_MS,
        };
    }
    let id = active[rng.gen_range(0..active.len())].clone();
    if rng.gen_bool(0.7) {
        *next += 1;
        LeaseOperation::Renew {
            id,
            expires: now_unix_millis() + LEASE_LIFETIME_MS + *next,
        }
    } else {
        LeaseOperation::Clean { id }
    }
}

async fn perform(store: &LocalLeaseStore, operation: &LeaseOperation) {
    let now = now_unix_millis();
    match operation {
        LeaseOperation::Create { id, expires } => {
            let lease = TestLease::new(
                id.clone(),
                LEASE_OWNER.to_string(),
                "power-cut".to_string(),
                format!("rbtest-{id}"),
                now,
                *expires,
            )
            .unwrap();
            store.create(lease).await.unwrap();
        }
        LeaseOperation::Renew { id, expires } => {
            store.renew(id, LEASE_OWNER, now, *expires).await.unwrap();
        }
        LeaseOperation::Clean { id } => {
            store.begin_cleanup(id, Some(LEASE_OWNER)).await.unwrap();
        }
        LeaseOperation::Release { id } => store.finish_cleanup(id).await.unwrap(),
    }
}

async fn run_lease_worker(root: &Path, worker: usize) {
    let store = LocalLeaseStore::open(root.join("leases").join(LEASE_STORES[worker]))
        .await
        .unwrap();
    let mut ledger = Ledger::open(&root.join(format!("ledgers/lease-{worker}.ledger")));
    let mut model = LeaseModel::new();
    let mut next = 0;
    loop {
        let operation = choose_operation(&model, worker, &mut next);
        ledger.record(&format!("begin {}", operation.encode()));
        perform(&store, &operation).await;
        ledger.record(&format!("ack {}", operation.encode()));
        apply(&mut model, &operation);
        let pause = rand::thread_rng().gen_range(2..30);
        tokio::time::sleep(Duration::from_millis(pause)).await;
    }
}

/// Replay a worker's ledger: the state every acknowledged operation implies,
/// plus the state if the one operation in flight at the cut also landed.
fn replay(lines: &[String]) -> (LeaseModel, Option<LeaseModel>, usize) {
    let mut acknowledged = LeaseModel::new();
    let mut in_flight: Option<LeaseOperation> = None;
    let mut count = 0;
    for line in lines {
        if let Some(text) = line.strip_prefix("begin ") {
            assert!(in_flight.is_none(), "two operations in flight: {line}");
            in_flight = Some(LeaseOperation::decode(text));
        } else if let Some(text) = line.strip_prefix("ack ") {
            let operation = LeaseOperation::decode(text);
            assert_eq!(
                in_flight.take(),
                Some(operation.clone()),
                "ack without begin"
            );
            apply(&mut acknowledged, &operation);
            count += 1;
        } else {
            panic!("unexpected lease ledger line {line:?}");
        }
    }
    let landed = in_flight.map(|operation| {
        let mut model = acknowledged.clone();
        apply(&mut model, &operation);
        model
    });
    (acknowledged, landed, count)
}

async fn observed(store: &LocalLeaseStore) -> LeaseModel {
    store
        .snapshot()
        .await
        .unwrap()
        .into_iter()
        .map(|(id, lease)| {
            let view = LeaseView {
                cleaning: matches!(lease.state, TestLeaseState::Cleaning { .. }),
                expires: lease.expires_at_unix_ms,
            };
            (id, view)
        })
        .collect()
}

fn acknowledged_lease_operations(root: &Path, worker: usize) -> usize {
    ledger_lines(&root.join(format!("ledgers/lease-{worker}.ledger")))
        .iter()
        .filter(|line| line.starts_with("ack "))
        .count()
}

async fn prepare_leases(root: &Path) {
    begin_prepare(root);
    std::fs::create_dir_all(root.join("leases")).unwrap();
    std::fs::create_dir_all(root.join("ledgers")).unwrap();
    sync_directory(root);
    for worker in 0..LEASE_STORES.len() {
        spawn_worker(LEASE_TEST, root, &format!("lease-{worker}"));
    }
    wait_until("acknowledged lease operations", || {
        (0..LEASE_STORES.len()).all(|worker| acknowledged_lease_operations(root, worker) >= 4)
    })
    .await;
    finish_prepare(root);
}

async fn verify_leases(root: &Path) {
    let boot = assert_rebooted(root);
    let mut acknowledged_total = 0;
    let mut in_flight_landed = 0;
    let mut live = 0;
    for (worker, name) in LEASE_STORES.iter().enumerate() {
        let lines = ledger_lines(&root.join(format!("ledgers/lease-{worker}.ledger")));
        let (acknowledged, landed, count) = replay(&lines);
        acknowledged_total += count;
        let path = root.join("leases").join(name);
        let store = LocalLeaseStore::open(path.clone())
            .await
            .unwrap_or_else(|error| panic!("{name} did not reopen cleanly: {error}"));
        let state = observed(&store).await;
        // A private temporary renamed over the wrong store would carry
        // another worker's leases.
        let prefix = format!("w{worker}-");
        assert!(
            state.keys().all(|id| id.starts_with(&prefix)),
            "{name} holds another store's leases: {:?}",
            state.keys().collect::<Vec<_>>()
        );
        if state != acknowledged {
            assert_eq!(
                Some(&state),
                landed.as_ref(),
                "{name} lost or invented an acknowledged operation"
            );
            in_flight_landed += 1;
        }
        live += state.len();

        // The reopened store still accepts and persists new work.
        let id = format!("w{worker}-verify");
        let expires = now_unix_millis() + LEASE_LIFETIME_MS;
        for operation in [
            LeaseOperation::Create {
                id: id.clone(),
                expires,
            },
            LeaseOperation::Renew {
                id: id.clone(),
                expires: expires + 1,
            },
            LeaseOperation::Clean { id: id.clone() },
            LeaseOperation::Release { id: id.clone() },
        ] {
            perform(&store, &operation).await;
        }
        drop(store);
        let reopened = LocalLeaseStore::open(path).await.unwrap();
        assert_eq!(observed(&reopened).await, state);
    }
    let unexpected: Vec<String> = std::fs::read_dir(root.join("leases"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| !LEASE_STORES.contains(&name.as_str()) && !name.starts_with(".reliaburger-"))
        .collect();
    assert!(
        unexpected.is_empty(),
        "unexpected lease files {unexpected:?}"
    );
    finish_verify(
        root,
        &boot,
        serde_json::json!({
            "fixture": "leases",
            "acknowledged": acknowledged_total,
            "in_flight_landed": in_flight_landed,
            "live_leases": live,
            "stale_temporaries": count_entries(&root.join("leases"), |name| name.starts_with(".reliaburger-")),
        }),
    );
}

/// Two-phase fixture: the driver must actually cut the disposable VM's power.
#[tokio::test]
#[ignore = "run only through scripts/release/qualify-storage-power-cut.sh on a disposable VM"]
async fn actual_power_cut_preserves_acknowledged_lease_operations() {
    let root = fixture_directory();
    if let Ok(role) = std::env::var(ROLE) {
        let worker = role
            .strip_prefix("lease-")
            .and_then(|index| index.parse().ok())
            .unwrap_or_else(|| panic!("unknown lease worker role {role}"));
        run_lease_worker(&root, worker).await;
        return;
    }
    match phase().as_str() {
        "prepare" => prepare_leases(&root).await,
        "verify" => verify_leases(&root).await,
        other => panic!("invalid power-cut phase {other}"),
    }
}

// ---------------------------------------------------------------------------
// Council backups: upload, then prune older backups
// ---------------------------------------------------------------------------

const BACKUP_TEST: &str = "actual_power_cut_preserves_acknowledged_council_backups";
const BACKUP_KEY: [u8; 32] = [7; 32];
/// Small, so pruning deletes an older backup on almost every tick.
const BACKUP_RETAIN: usize = 3;
/// Backup keys are millisecond timestamps; a fixed base keeps them unique.
const BACKUP_EPOCH_MS: u64 = 1_790_000_000_000;

fn backup_directory(root: &Path) -> PathBuf {
    root.join("backups")
}

fn backup_url(root: &Path) -> String {
    format!("file://{}", backup_directory(root).display())
}

fn backup_time(sequence: u64) -> std::time::SystemTime {
    std::time::UNIX_EPOCH + Duration::from_millis(BACKUP_EPOCH_MS + sequence)
}

fn backup_payload(sequence: u64) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({"sequence": sequence})).unwrap()
}

/// Seal a fresh backup, upload it, then prune, the way the leader's backup
/// tick does, recording each step in the ledger once it has returned.
async fn run_backup_worker(root: &Path) {
    let store = BackupStore::from_url(&backup_url(root)).unwrap();
    let mut ledger = Ledger::open(&root.join("ledgers/backup.ledger"));
    let mut sequence = 0u64;
    loop {
        let sealed = seal_snapshot(
            &BACKUP_KEY,
            &backup_payload(sequence),
            &BackupConfig::default(),
        )
        .unwrap();
        let bytes = encode_backup(&sealed).unwrap();
        ledger.record(&format!("begin {sequence}"));
        store.put(&sealed, backup_time(sequence)).await.unwrap();
        ledger.record(&format!("put {sequence} {}", sha256(&bytes)));
        store.prune(BACKUP_RETAIN).await.unwrap();
        ledger.record(&format!("prune {sequence}"));
        sequence += 1;
        let pause = rand::thread_rng().gen_range(2..30);
        tokio::time::sleep(Duration::from_millis(pause)).await;
    }
}

/// Acknowledged uploads (sequence → SHA-256) and the upload in flight, if any.
fn acknowledged_backups(root: &Path) -> (BTreeMap<u64, String>, Option<u64>) {
    let mut uploads = BTreeMap::new();
    let mut in_flight = None;
    for line in ledger_lines(&root.join("ledgers/backup.ledger")) {
        let fields: Vec<&str> = line.split(' ').collect();
        match fields.as_slice() {
            ["begin", sequence] => in_flight = Some(sequence.parse().unwrap()),
            ["put", sequence, sha] => {
                let sequence: u64 = sequence.parse().unwrap();
                assert_eq!(in_flight.take(), Some(sequence), "put without begin");
                uploads.insert(sequence, sha.to_string());
            }
            ["prune", _] => {}
            _ => panic!("unexpected backup ledger line {line:?}"),
        }
    }
    (uploads, in_flight)
}

/// Every backup object at the destination, by sequence, plus leftover
/// staging files from uploads the cut interrupted.
fn stored_backups(root: &Path) -> (BTreeMap<u64, Vec<u8>>, usize) {
    let mut backups = BTreeMap::new();
    let mut staged = 0;
    for entry in std::fs::read_dir(backup_directory(root)).unwrap() {
        let entry = entry.unwrap();
        let name = entry.file_name().to_string_lossy().into_owned();
        let Some(millis) = name
            .strip_prefix("council-")
            .and_then(|rest| rest.strip_suffix(".backup"))
        else {
            staged += 1;
            continue;
        };
        let sequence = millis.parse::<u64>().unwrap() - BACKUP_EPOCH_MS;
        backups.insert(sequence, std::fs::read(entry.path()).unwrap());
    }
    (backups, staged)
}

fn assert_backup_intact(sequence: u64, bytes: &[u8]) {
    let sealed = decode_backup(bytes).unwrap_or_else(|error| {
        panic!("backup {sequence} is torn ({} bytes): {error}", bytes.len())
    });
    let payload = unseal_snapshot(&BACKUP_KEY, &sealed)
        .unwrap_or_else(|error| panic!("backup {sequence} does not unseal: {error}"));
    assert_eq!(
        payload,
        backup_payload(sequence),
        "backup {sequence} holds another state"
    );
}

async fn prepare_backups(root: &Path) {
    begin_prepare(root);
    std::fs::create_dir_all(backup_directory(root)).unwrap();
    std::fs::create_dir_all(root.join("ledgers")).unwrap();
    sync_directory(root);
    spawn_worker(BACKUP_TEST, root, "backup");
    wait_until("acknowledged backups", || {
        acknowledged_backups(root).0.len() > BACKUP_RETAIN + 2
    })
    .await;
    finish_prepare(root);
}

async fn verify_backups(root: &Path) {
    let boot = assert_rebooted(root);
    let (uploads, in_flight) = acknowledged_backups(root);
    let (stored, staged) = stored_backups(root);

    // Nothing at the destination is torn, and each acknowledged backup holds
    // exactly the bytes that were uploaded.
    for (sequence, bytes) in &stored {
        assert_backup_intact(*sequence, bytes);
        match uploads.get(sequence) {
            Some(sha) => assert_eq!(&sha256(bytes), sha, "backup {sequence} changed"),
            None => assert_eq!(
                Some(*sequence),
                in_flight,
                "backup {sequence} was never uploaded"
            ),
        }
    }

    // Pruning keeps the newest backups. An upload that landed without its
    // acknowledgement may have pruned one more acknowledged backup, no more.
    let newest = *uploads.keys().last().unwrap();
    let unacknowledged = stored.keys().filter(|sequence| **sequence > newest).count();
    for sequence in uploads.keys().rev().take(BACKUP_RETAIN - unacknowledged) {
        assert!(
            stored.contains_key(sequence),
            "acknowledged backup {sequence} is missing, though pruning must keep it"
        );
    }

    // Recovery reads the newest backup, and the store keeps working.
    let store = BackupStore::from_url(&backup_url(root)).unwrap();
    let latest = store
        .latest()
        .await
        .expect("newest backup is unreadable after the power cut")
        .expect("no backup survived the power cut");
    let newest_stored = *stored.keys().last().unwrap();
    assert_eq!(
        unseal_snapshot(&BACKUP_KEY, &latest).unwrap(),
        backup_payload(newest_stored)
    );
    let fresh = newest_stored + 1_000;
    let sealed = seal_snapshot(
        &BACKUP_KEY,
        &backup_payload(fresh),
        &BackupConfig::default(),
    )
    .unwrap();
    store.put(&sealed, backup_time(fresh)).await.unwrap();
    store.prune(BACKUP_RETAIN).await.unwrap();
    let latest = store.latest().await.unwrap().unwrap();
    assert_eq!(
        unseal_snapshot(&BACKUP_KEY, &latest).unwrap(),
        backup_payload(fresh)
    );

    finish_verify(
        root,
        &boot,
        serde_json::json!({
            "fixture": "backups",
            "acknowledged": uploads.len(),
            "present": stored.len(),
            "in_flight_landed": unacknowledged,
            "staged_leftovers": staged,
        }),
    );
}

/// Two-phase fixture: the driver must actually cut the disposable VM's power.
#[tokio::test]
#[ignore = "run only through scripts/release/qualify-storage-power-cut.sh on a disposable VM"]
async fn actual_power_cut_preserves_acknowledged_council_backups() {
    let root = fixture_directory();
    if let Ok(role) = std::env::var(ROLE) {
        assert_eq!(role, "backup", "unknown backup worker role");
        run_backup_worker(&root).await;
        return;
    }
    match phase().as_str() {
        "prepare" => prepare_backups(&root).await,
        "verify" => verify_backups(&root).await,
        other => panic!("invalid power-cut phase {other}"),
    }
}
