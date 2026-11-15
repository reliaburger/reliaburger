//! Seed inputs for each fuzz target, built from the same fixtures the
//! decoders' unit tests use.
//!
//! Every seed here must be accepted by its target: the unit tests check
//! that, so the round-trip assertions run on real input in `make ci`, not
//! only on whatever a fuzzer finds. `make fuzz-corpus-seed` writes them
//! into `fuzz/corpus/<target>/`, where they start libFuzzer off.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::Path;

use crate::council::network::{RaftRpc, RaftRpcEnvelope};
use crate::meat::NodeId;
use crate::mustard::message::{
    DirectoryExtension, GossipMessage, GossipPayload, LeaderHint, MembershipUpdate, encode_datagram,
};
use crate::mustard::state::NodeState;

/// One named seed input.
pub(super) struct Seed {
    /// The file name under `fuzz/corpus/<target>/`.
    pub(super) name: String,
    /// The bytes.
    pub(super) bytes: Vec<u8>,
}

fn seed(name: impl Into<String>, bytes: Vec<u8>) -> Seed {
    Seed {
        name: name.into(),
        bytes,
    }
}

/// The seeds for the target called `target`.
pub(super) fn for_target(target: &str) -> Vec<Seed> {
    match target {
        "gossip_datagram" => gossip(),
        "raft_rpc_frame" => raft(),
        "dns_query" => dns(),
        "council_backup" => backup(),
        "oci_manifest_push" => pushed_manifests(),
        "oci_manifest_upstream" => upstream_manifests(),
        "join_csr" => csrs(),
        "app_config" => config_files(&["phase-1", "phase-8", "templates", "policies"]),
        "node_config" => config_files(&["node"]),
        other => panic!("no seeds for fuzz target {other}"),
    }
}

fn address(port: u16) -> SocketAddr {
    SocketAddr::from(([127, 0, 0, 1], port))
}

fn update(node: &str, state: NodeState) -> MembershipUpdate {
    MembershipUpdate {
        node_id: NodeId::new(node),
        address: address(9000),
        state,
        incarnation: 3,
    }
}

/// A message with a fixed sequence number, so the seed's bytes are stable.
fn message(payload: GossipPayload) -> GossipMessage {
    let mut message = GossipMessage::new(NodeId::new("sender"), 5, payload);
    message.sequence = 42;
    message
}

fn gossip() -> Vec<Seed> {
    let ping = message(GossipPayload::Ping {
        updates: vec![update("target", NodeState::Alive)],
    });
    let ping_req = message(GossipPayload::PingReq {
        target: NodeId::new("target"),
        requester: NodeId::new("requester"),
        updates: vec![update("suspect", NodeState::Suspect)],
    });
    let ack = message(GossipPayload::Ack {
        updates: vec![update("gone", NodeState::Dead)],
        relayed_for: Some(NodeId::new("target")),
    });
    let sync = message(GossipPayload::Sync {
        entries: vec![update("a", NodeState::Alive), update("b", NodeState::Left)],
        wants_reply: true,
    });
    let mut with_directory = message(GossipPayload::Ping { updates: vec![] });
    with_directory.extension = Some(DirectoryExtension {
        node_id: NodeId::new("sender"),
        api_address: address(9443),
        reporting_address: address(9444),
        leader: Some(LeaderHint {
            node_id: NodeId::new("leader"),
            term: 7,
            recovery_epoch: 0,
            api_address: address(9443),
            reporting_address: address(9444),
        }),
        labels: BTreeMap::from([("zone".to_string(), "a".to_string())]),
        disk_pressured: false,
        recovery_epoch: None,
        recovery_id: None,
        jobs: None,
        hmac: [0u8; 32],
    });
    [
        ("ping", ping),
        ("ping-req", ping_req),
        ("ack", ack),
        ("sync", sync),
        ("ping-with-directory", with_directory),
    ]
    .into_iter()
    .map(|(name, message)| {
        seed(
            name,
            encode_datagram(&message).expect("seed datagram encodes"),
        )
    })
    .collect()
}

fn envelope(rpc: RaftRpc) -> Vec<u8> {
    serde_json::to_vec(&RaftRpcEnvelope {
        compatibility: crate::compatibility::CURRENT,
        sender_recovery_epoch: 1,
        sender_recovery_id: None,
        rpc,
    })
    .expect("seed envelope encodes")
}

fn raft() -> Vec<Seed> {
    use crate::council::types::RaftRequest;
    use openraft::raft::{AppendEntriesRequest, InstallSnapshotRequest, VoteRequest};
    use openraft::{CommittedLeaderId, Entry, EntryPayload, LogId, SnapshotMeta, Vote};

    let log_id = |index| LogId::new(CommittedLeaderId::new(1, 7), index);
    let vote = VoteRequest::new(Vote::new(1, 7), Some(log_id(0)));
    let append = AppendEntriesRequest {
        vote: Vote::new_committed(1, 7),
        prev_log_id: Some(log_id(0)),
        entries: vec![
            Entry {
                log_id: log_id(1),
                payload: EntryPayload::Blank,
            },
            Entry {
                log_id: log_id(2),
                payload: EntryPayload::Normal(RaftRequest::ConfigSet {
                    key: "cluster.name".to_string(),
                    value: "fuzz".to_string(),
                }),
            },
        ],
        leader_commit: Some(log_id(1)),
    };
    let snapshot = InstallSnapshotRequest {
        vote: Vote::new_committed(1, 7),
        meta: SnapshotMeta {
            last_log_id: Some(log_id(2)),
            last_membership: Default::default(),
            snapshot_id: "1-7-2".to_string(),
        },
        offset: 0,
        data: b"{}".to_vec(),
        done: true,
    };
    vec![
        seed("vote", envelope(RaftRpc::Vote(vote))),
        seed("append-entries", envelope(RaftRpc::AppendEntries(append))),
        seed(
            "install-snapshot",
            envelope(RaftRpc::InstallSnapshot(snapshot)),
        ),
    ]
}

/// A query packet laid out by hand, as the DNS unit tests build them.
fn dns_packet(name: &str, qtype: u16, additional: &[u8]) -> Vec<u8> {
    let mut packet = vec![0x12, 0x34, 0x01, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00];
    packet.extend_from_slice(&u16::from(!additional.is_empty()).to_be_bytes());
    for label in name.split('.') {
        packet.push(label.len() as u8);
        packet.extend_from_slice(label.as_bytes());
    }
    packet.push(0);
    packet.extend_from_slice(&qtype.to_be_bytes());
    packet.extend_from_slice(&[0x00, 0x01]);
    packet.extend_from_slice(additional);
    packet
}

fn dns() -> Vec<Seed> {
    // An EDNS0 OPT record: root name, type 41, a 1232-byte payload, no options.
    let edns = [0x00, 0x00, 0x29, 0x04, 0xd0, 0, 0, 0, 0, 0, 0];
    vec![
        seed("a-internal", dns_packet("redis.internal", 1, &[])),
        seed("a-namespaced", dns_packet("api.payments.internal", 1, &[])),
        seed("aaaa-internal", dns_packet("Redis.INTERNAL", 28, &[])),
        seed("a-external", dns_packet("api.stripe.com", 1, &[])),
        seed("a-edns", dns_packet("redis.internal", 1, &edns)),
    ]
}

fn backup() -> Vec<Seed> {
    use crate::council::backup::{BackupConfig, encode_backup, seal_snapshot};

    let sealed = seal_snapshot(&[5u8; 32], br#"{"apps":{}}"#, &BackupConfig::default())
        .expect("seed backup seals");
    vec![seed(
        "sealed-snapshot",
        encode_backup(&sealed).expect("seed backup encodes"),
    )]
}

const ZERO_DIGEST: &str = "sha256:0000000000000000000000000000000000000000000000000000000000000000";
const ONE_DIGEST: &str = "sha256:1111111111111111111111111111111111111111111111111111111111111111";

fn json(value: serde_json::Value) -> Vec<u8> {
    serde_json::to_vec_pretty(&value).expect("seed JSON encodes")
}

fn image_manifest(media_type: &str) -> serde_json::Value {
    serde_json::json!({
        "schemaVersion": 2,
        "mediaType": media_type,
        "config": {
            "mediaType": "application/vnd.oci.image.config.v1+json",
            "digest": ZERO_DIGEST,
            "size": 2
        },
        "layers": [{
            "mediaType": "application/vnd.oci.image.layer.v1.tar+gzip",
            "digest": ONE_DIGEST,
            "size": 32
        }]
    })
}

fn image_index(media_type: &str) -> serde_json::Value {
    serde_json::json!({
        "schemaVersion": 2,
        "mediaType": media_type,
        "manifests": [
            {
                "mediaType": "application/vnd.oci.image.manifest.v1+json",
                "digest": ZERO_DIGEST,
                "size": 400,
                "platform": {"os": "linux", "architecture": "amd64"}
            },
            {
                "mediaType": "application/vnd.oci.image.manifest.v1+json",
                "digest": ONE_DIGEST,
                "size": 400,
                "platform": {"os": "linux", "architecture": "arm64", "variant": "v8"}
            }
        ]
    })
}

fn pushed_manifests() -> Vec<Seed> {
    let mut untyped = image_manifest("");
    untyped
        .as_object_mut()
        .expect("a manifest is an object")
        .remove("mediaType");
    vec![
        seed(
            "oci-manifest",
            json(image_manifest("application/vnd.oci.image.manifest.v1+json")),
        ),
        seed(
            "docker-manifest",
            json(image_manifest(
                "application/vnd.docker.distribution.manifest.v2+json",
            )),
        ),
        seed(
            "oci-index",
            json(image_index("application/vnd.oci.image.index.v1+json")),
        ),
        seed("untyped-manifest", json(untyped)),
    ]
}

fn upstream_manifests() -> Vec<Seed> {
    vec![
        seed(
            "oci-manifest",
            json(image_manifest("application/vnd.oci.image.manifest.v1+json")),
        ),
        seed(
            "docker-manifest-list",
            json(image_index(
                "application/vnd.docker.distribution.manifest.list.v2+json",
            )),
        ),
        seed(
            "oci-index",
            json(image_index("application/vnd.oci.image.index.v1+json")),
        ),
    ]
}

fn csrs() -> Vec<Seed> {
    use crate::sesame::ca::create_node_csr;

    ["node-1", "node-evil"]
        .into_iter()
        .map(|node| {
            let (csr_der, _key) = create_node_csr(node).expect("seed CSR generates");
            seed(node, csr_der)
        })
        .collect()
}

/// Examples in another format: a Smoker scenario (`relish fault scenario`)
/// and a task-array manifest (`relish batch submit`).
const NOT_CONFIG: [&str; 2] = [
    "container-chaos-scenario.toml",
    "container-shared-runc-array.toml",
];

/// Every `*.toml` file in the named directories under `examples/`, except
/// those [`NOT_CONFIG`] names.
fn config_files(directories: &[&str]) -> Vec<Seed> {
    let examples = Path::new(env!("CARGO_MANIFEST_DIR")).join("examples");
    let mut seeds = Vec::new();
    for directory in directories {
        let mut paths: Vec<_> = std::fs::read_dir(examples.join(directory))
            .expect("examples directory reads")
            .map(|entry| entry.expect("examples entry reads").path())
            .filter(|path| {
                path.extension()
                    .is_some_and(|extension| extension == "toml")
            })
            .filter(|path| {
                !NOT_CONFIG
                    .iter()
                    .any(|name| path.file_name().is_some_and(|found| found == *name))
            })
            .collect();
        paths.sort();
        for path in paths {
            let name = path
                .file_name()
                .expect("an example has a file name")
                .to_string_lossy()
                .into_owned();
            seeds.push(seed(
                name,
                std::fs::read(&path).expect("example file reads"),
            ));
        }
    }
    seeds
}
