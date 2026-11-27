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
    DirectoryExtension, GossipMessage, GossipPayload, LeaderHint, MembershipUpdate, SequenceFloor,
    encode_datagram,
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
        "task_manifest" => task_manifests(),
        "chaos_scenario" => scenario_files(),
        "council_backup_state" => council_states(),
        "api_bearer" => bearers(),
        "ingress_request" => ingress_requests(),
        "tftp_packet" => tftp_packets(),
        "dhcp_request" => dhcp_requests(),
        "netboot_disk_report" => disk_reports(),
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
        floors: vec![SequenceFloor {
            node_id: NodeId::new("departed"),
            incarnation: 2,
            sequence: 41,
            expires_in_secs: 3600,
        }],
        wants_reply: true,
        known_members: 2,
    });
    let stale = message(GossipPayload::Stale {
        held_incarnation: 4,
        held_sequence: 40,
        updates: vec![update("c", NodeState::Suspect)],
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
        leader_age_ms: 0,
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
        ("stale", stale),
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
        seed("council-voters", envelope(RaftRpc::CouncilVoters)),
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

/// A backup sealed under the key the `council_backup` target tries, with a
/// fixed salt and nonce, so the seed is the same bytes on every run and the
/// target's unseal gets past the tag check. `seal_snapshot` picks both at
/// random, as it must.
fn backup() -> Vec<Seed> {
    use crate::council::backup::{
        BACKUP_ENVELOPE_VERSION, BACKUP_SEAL_INFO, SealedBackup, encode_backup, unseal_snapshot,
    };
    use ring::aead::{AES_256_GCM, Aad, LessSafeKey, Nonce, UnboundKey};

    let master_key = super::BACKUP_FUZZ_KEY;
    let payload = br#"{"apps":{}}"#;
    let (hkdf_salt, nonce) = ([3u8; 32], [9u8; 12]);
    let key = crate::sesame::crypto::hkdf_derive_key(&master_key, &hkdf_salt, BACKUP_SEAL_INFO)
        .expect("the seed key derives");
    let key = LessSafeKey::new(UnboundKey::new(&AES_256_GCM, &key).expect("a 32-byte key"));
    let mut ciphertext = payload.to_vec();
    key.seal_in_place_append_tag(
        Nonce::assume_unique_for_key(nonce),
        Aad::empty(),
        &mut ciphertext,
    )
    .expect("the seed seals");
    let sealed = SealedBackup {
        version: BACKUP_ENVELOPE_VERSION,
        hkdf_salt,
        hkdf_info: BACKUP_SEAL_INFO.to_string(),
        nonce,
        ciphertext,
    };
    assert_eq!(
        unseal_snapshot(&master_key, &sealed).expect("the seed unseals"),
        payload,
        "the seed isn't sealed the way a backup is"
    );
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

/// Join CSRs signed with Ed25519 keys from fixed seeds. Ed25519 signatures
/// are deterministic, so each seed is the same bytes on every run; an ECDSA
/// key, as `create_node_csr` makes, signs with a random nonce.
fn csrs() -> Vec<Seed> {
    use crate::sesame::ca::node_spiffe_uri;
    use rcgen::{CertificateParams, DistinguishedName, DnType, KeyPair, SanType};

    // PKCS#8 v1 for an Ed25519 key (RFC 8410): this prefix, then the seed.
    const PKCS8_PREFIX: [u8; 16] = [
        0x30, 0x2e, 0x02, 0x01, 0x00, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x04, 0x22, 0x04,
        0x20,
    ];
    [("node-1", 1u8), ("node-evil", 2u8)]
        .into_iter()
        .map(|(node, key_seed)| {
            let mut pkcs8 = PKCS8_PREFIX.to_vec();
            pkcs8.extend_from_slice(&[key_seed; 32]);
            let key_pair = KeyPair::try_from(pkcs8.as_slice()).expect("a fixed Ed25519 key");
            let mut params = CertificateParams::default();
            let mut name = DistinguishedName::new();
            name.push(DnType::CommonName, node);
            params.distinguished_name = name;
            params.subject_alt_names = vec![SanType::URI(
                node_spiffe_uri(node).try_into().expect("a SPIFFE URI"),
            )];
            let csr = params
                .serialize_request(&key_pair)
                .expect("seed CSR serialises");
            seed(node, csr.der().to_vec())
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

fn task_manifests() -> Vec<Seed> {
    let cohort = |name: &str, count: u32| {
        serde_json::json!({
            "name": name,
            "count": count,
            "template": {"runtime": "process", "exec": "/bin/true"},
        })
    };
    vec![
        seed(
            "one-cohort",
            json(serde_json::json!({"name": "mixed", "cohort": [cohort("small", 1)]})),
        ),
        seed(
            "two-cohorts",
            json(serde_json::json!({
                "name": "render",
                "namespace": "media",
                "cohort": [cohort("thumbnails", 100), cohort("posters", 3)],
            })),
        ),
    ]
}

fn scenario_files() -> Vec<Seed> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("examples/phase-8/container-chaos-scenario.toml");
    vec![seed(
        "container-chaos-scenario",
        std::fs::read(&path).expect("the example scenario reads"),
    )]
}

fn council_states() -> Vec<Seed> {
    use crate::council::types::DesiredState;

    let empty = DesiredState::default();
    let mut configured = DesiredState::default();
    configured.config.insert("heartbeat".into(), "1".into());
    [("empty", empty), ("with-config", configured)]
        .into_iter()
        .map(|(name, state)| {
            seed(
                name,
                serde_json::to_vec(&state).expect("seed state encodes"),
            )
        })
        .collect()
}

fn ingress_requests() -> Vec<Seed> {
    [
        ("api", "app.example\n/api/v1/orders?page=2\n"),
        (
            "api-with-port",
            "APP.example:8080\n/api\nrb_session=abc; theme=dark",
        ),
        ("api-prefix-only", "app.example\n/apiary\n"),
        ("tls-redirect", "tls.example\n/basket?item=1\n"),
        (
            "absolute-form",
            "tls.example:80\nhttp://tls.example/checkout\n",
        ),
        ("ipv6", "[::1]:8080\n/\n"),
        ("unknown-host", "other.example\n/\nsession=1"),
    ]
    .into_iter()
    .map(|(name, text)| seed(name, text.as_bytes().to_vec()))
    .collect()
}

fn tftp_packets() -> Vec<Seed> {
    use crate::relish::netboot::tftp::{ErrorCode, Packet};

    let options = |pairs: &[(&str, &str)]| -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(name, value)| (name.to_string(), value.to_string()))
            .collect()
    };
    let read = |filename: &str, pairs: &[(&str, &str)]| Packet::ReadRequest {
        filename: filename.to_string(),
        mode: "octet".to_string(),
        options: options(pairs),
    };
    [
        ("read", read("boot.ipxe", &[])),
        (
            "read-with-options",
            read(
                "ipxe-x86_64.efi",
                &[("blksize", "1468"), ("tsize", "0"), ("timeout", "5")],
            ),
        ),
        ("read-small-blocks", read("/boot.ipxe", &[("BLKSIZE", "8")])),
        ("read-missing", read("pxelinux.0", &[])),
        (
            "write",
            Packet::WriteRequest {
                filename: "boot.ipxe".to_string(),
            },
        ),
        ("ack", Packet::Ack { block: 1 }),
        (
            "data",
            Packet::Data {
                block: 2,
                data: b"#!ipxe\n".to_vec(),
            },
        ),
        (
            "error",
            Packet::Error {
                code: ErrorCode::OptionsRefused,
                message: "tsize only".to_string(),
            },
        ),
        (
            "option-ack",
            Packet::OptionAck {
                options: options(&[("blksize", "512")]),
            },
        ),
    ]
    .into_iter()
    .map(|(name, packet)| seed(name, packet.encode()))
    .collect()
}

fn dhcp_requests() -> Vec<Seed> {
    use dhcproto::v4::{Architecture, DhcpOption, HType, Message, MessageType, Opcode, OptionCode};

    let request = |kind: MessageType, class: &[u8], arch: Option<u16>| {
        let mut message = Message::default();
        message
            .set_opcode(Opcode::BootRequest)
            .set_htype(HType::Eth)
            .set_xid(0x1234_5678)
            .set_chaddr(&[0x52, 0x54, 0x00, 0x12, 0x34, 0x56]);
        let options = message.opts_mut();
        options.insert(DhcpOption::MessageType(kind));
        options.insert(DhcpOption::ClassIdentifier(class.to_vec()));
        if let Some(arch) = arch {
            options.insert(DhcpOption::ClientSystemArchitecture(Architecture::from(
                arch,
            )));
        }
        message
    };
    let pxe = b"PXEClient:Arch:00007:UNDI:003016";
    let mut with_uuid = request(MessageType::Discover, pxe, Some(11));
    with_uuid
        .opts_mut()
        .insert(DhcpOption::ClientMachineIdentifier(vec![0; 17]));
    let mut ipxe = request(MessageType::Request, pxe, Some(7));
    ipxe.opts_mut()
        .insert(DhcpOption::UserClass(b"iPXE".to_vec()));
    // A boot item (vendor sub-option 71), which a reply on 4011 echoes.
    ipxe.opts_mut().insert(DhcpOption::VendorExtensions(vec![
        71, 4, 0x80, 0x00, 0, 0, 255,
    ]));
    let mut to_us = request(MessageType::Request, pxe, Some(7));
    to_us
        .opts_mut()
        .insert(DhcpOption::ServerIdentifier(std::net::Ipv4Addr::new(
            192, 168, 1, 20,
        )));
    let mut plain = request(MessageType::Discover, b"android-dhcp-14", None);
    plain.opts_mut().remove(OptionCode::ClassIdentifier);
    [
        ("pxe-x86-64", request(MessageType::Discover, pxe, Some(7))),
        ("pxe-arm64-uuid", with_uuid),
        (
            "http-boot",
            request(MessageType::Discover, b"HTTPClient:Arch:00016", Some(16)),
        ),
        ("ipxe-boot-item", ipxe),
        ("request-to-us", to_us),
        ("pxe-bios", request(MessageType::Discover, pxe, Some(0))),
        ("ordinary", plain),
    ]
    .into_iter()
    .map(|(name, message)| {
        seed(
            name,
            crate::relish::netboot::dhcp::encode(&message).expect("a seed request encodes"),
        )
    })
    .collect()
}

fn disk_reports() -> Vec<Seed> {
    [
        (
            "blank",
            r#"NAME="/dev/mmcblk0" TYPE="disk" SIZE="7818182656" PTTYPE="" FSTYPE="" LABEL="" PARTLABEL="""#,
        ),
        (
            "used",
            concat!(
                r#"NAME="/dev/mmcblk0" TYPE="disk" SIZE="7818182656" PTTYPE="gpt" FSTYPE="" LABEL="" PARTLABEL="""#,
                "\n",
                r#"NAME="/dev/mmcblk0p1" TYPE="part" SIZE="536870912" PTTYPE="gpt" FSTYPE="vfat" LABEL="EFI" PARTLABEL="EFI System""#,
                "\n",
                r#"NAME="/dev/mmcblk0p2" TYPE="part" SIZE="6442450944" PTTYPE="gpt" FSTYPE="ext4" LABEL="ThinOS" PARTLABEL="""#,
                "\n",
            ),
        ),
        (
            "escaped-label",
            concat!(
                r#"NAME="/dev/sda" TYPE="disk" SIZE="1000000000" PTTYPE="dos" FSTYPE="" LABEL="" PARTLABEL="""#,
                "\n",
                r#"NAME="/dev/sda1" TYPE="part" SIZE="1" PTTYPE="dos" FSTYPE="ntfs" LABEL="My\x20\x22Data\x22\x1b" PARTLABEL="""#,
            ),
        ),
        (
            "whole-disk-filesystem",
            r#"NAME="/dev/sda" TYPE="disk" SIZE="8000000000" PTTYPE="" FSTYPE="ext4" LABEL="data" PARTLABEL="""#,
        ),
    ]
    .into_iter()
    .map(|(name, text)| seed(name, text.as_bytes().to_vec()))
    .collect()
}

fn bearers() -> Vec<Seed> {
    use super::{BEARER_AGENT, BEARER_HUMAN, BEARER_SERVICE, JWT_CLUSTER, JWT_ISSUER};

    let claims = serde_json::json!({
        "iss": JWT_ISSUER,
        "sub": "spiffe://fuzz/ns/default/app/web",
        "aud": [crate::sesame::oidc::api_audience(JWT_CLUSTER)],
        "exp": 4_102_444_800u64,
        "iat": 1_700_000_000u64,
        "reliaburger.dev/namespace": "default",
        "reliaburger.dev/app": "web",
        "reliaburger.dev/cluster": JWT_CLUSTER,
        "reliaburger.dev/node": "node-1",
        "reliaburger.dev/instance": "web-0",
    });
    let wrong = format!("{}0", &BEARER_HUMAN[..BEARER_HUMAN.len() - 1]);
    [
        ("person", BEARER_HUMAN.to_string()),
        ("agent", BEARER_AGENT.to_string()),
        ("service", BEARER_SERVICE.to_string()),
        ("wrong-secret", wrong),
        ("workload-jwt", seed_jwt(&claims)),
    ]
    .into_iter()
    .map(|(name, bearer)| seed(name, format!("Bearer {bearer}").into_bytes()))
    .collect()
}

/// A workload JWT for the API, signed with [`super::jwt_signing_key`], issued at
/// a fixed time long past, so it's refused for its age: a seed has to be the
/// same bytes on every run, and a JWT that verifies today wouldn't be.
fn seed_jwt(claims: &serde_json::Value) -> String {
    use base64::Engine as _;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;

    let header = serde_json::json!({"alg": "EdDSA", "typ": "JWT", "kid": super::JWT_KEY_ID});
    let input = format!(
        "{}.{}",
        URL_SAFE_NO_PAD.encode(header.to_string()),
        URL_SAFE_NO_PAD.encode(claims.to_string())
    );
    let signature = super::jwt_signing_key().sign(input.as_bytes());
    format!("{input}.{}", URL_SAFE_NO_PAD.encode(signature.as_ref()))
}
