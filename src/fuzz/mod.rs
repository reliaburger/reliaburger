//! Fuzz target bodies: one per decoder that reads bytes from another machine (#1398).
//!
//! Each function here takes the raw bytes a peer, a client, an upstream
//! registry or an operator could hand us, runs them through the real decoder,
//! and checks what it can about the result. The `fuzz/` crate wraps each one
//! in a `cargo fuzz` target (nightly Rust and libFuzzer), and the unit tests
//! below run every one over its checked-in seed corpus on stable Rust, so
//! `make ci` exercises the same assertions without a fuzzer.
//!
//! A body panics when it finds a bug: that is how libFuzzer learns an input
//! crashed. Decoding garbage is not a bug, so a refused input returns
//! [`Outcome::Refused`]. Where the decoder has an encoder, an accepted input
//! must survive a round trip: decode, re-encode, decode again, and compare.
//!
//! This module is compiled for the crate's own tests and under the `fuzzing`
//! feature, which only the `fuzz/` crate turns on. No binary carries it.

use std::sync::OnceLock;

use crate::council::fence::RecoveryFence;
use crate::council::network::RaftRpcEnvelope;
use crate::sesame::ca::CaHierarchy;

#[cfg(test)]
mod seeds;
#[cfg(test)]
mod tests;

/// What a target made of one input.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// The decoder accepted the input, and every check on the result held.
    Accepted,
    /// The decoder refused the input, as it should refuse most random bytes.
    Refused,
}

/// One fuzz target: the name `cargo fuzz run` takes, and its body.
pub struct Target {
    /// The `fuzz/fuzz_targets/<name>.rs` file and its corpus directory,
    /// `fuzz/corpus/<name>/`.
    pub name: &'static str,
    /// The decoder the target drives, for the docs and failure messages.
    pub decoder: &'static str,
    /// The target's body.
    pub run: fn(&[u8]) -> Outcome,
}

/// Every fuzz target. The `fuzz/` crate has one binary per entry, and a unit
/// test keeps the two lists the same.
pub const TARGETS: &[Target] = &[
    Target {
        name: "gossip_datagram",
        decoder: "mustard::message::decode_datagram",
        run: gossip_datagram,
    },
    Target {
        name: "raft_rpc_frame",
        decoder: "council::network::decode_and_admit",
        run: raft_rpc_frame,
    },
    Target {
        name: "dns_query",
        decoder: "onion::dns::decode_query",
        run: dns_query,
    },
    Target {
        name: "council_backup",
        decoder: "council::backup::decode_backup",
        run: council_backup,
    },
    Target {
        name: "oci_manifest_push",
        decoder: "pickle::api::parse_pushed_manifest",
        run: oci_manifest_push,
    },
    Target {
        name: "oci_manifest_upstream",
        decoder: "grill::oci_pull::parse_manifest",
        run: oci_manifest_upstream,
    },
    Target {
        name: "join_csr",
        decoder: "sesame::ca::sign_node_csr",
        run: join_csr,
    },
    Target {
        name: "app_config",
        decoder: "config::Config::parse",
        run: app_config,
    },
    Target {
        name: "node_config",
        decoder: "config::node::NodeConfig::parse",
        run: node_config,
    },
];

/// A gossip datagram, as any host that can reach the UDP port sends it.
///
/// Accepted datagrams must re-encode to bytes that decode to the same
/// message. The comparison is on the encoding: postcard's output is
/// canonical for a message (it holds no unordered maps), and
/// `GossipMessage` has no `PartialEq`.
pub fn gossip_datagram(data: &[u8]) -> Outcome {
    use crate::mustard::message::{decode_datagram, encode_datagram};

    let Ok(message) = decode_datagram(data) else {
        return Outcome::Refused;
    };
    let encoded = encode_datagram(&message).expect("a decoded datagram re-encodes");
    assert!(
        encoded.len() <= data.len(),
        "re-encoding grew a {}-byte datagram to {} bytes",
        data.len(),
        encoded.len()
    );
    let again = decode_datagram(&encoded).expect("a re-encoded datagram decodes");
    let reencoded = encode_datagram(&again).expect("a decoded datagram re-encodes");
    assert_eq!(encoded, reencoded, "a datagram changed on its round trip");
    Outcome::Accepted
}

/// A Raft RPC frame's payload, as a peer sends it over TCP.
///
/// An admitted frame is a [`RaftRpcEnvelope`]: re-encoded, it must be
/// admitted again and decode to the same JSON value. Values, not bytes,
/// because a JSON object's key order is not part of what it means.
pub fn raft_rpc_frame(data: &[u8]) -> Outcome {
    use crate::council::network::decode_and_admit;

    // A fresh fence per input: admitting one can fence the receiver.
    if decode_and_admit(data, &RecoveryFence::serving(1)).is_none() {
        return Outcome::Refused;
    }
    let envelope: RaftRpcEnvelope =
        serde_json::from_slice(data).expect("an admitted frame is an envelope");
    let encoded = serde_json::to_vec(&envelope).expect("an envelope re-encodes");
    assert!(
        decode_and_admit(&encoded, &RecoveryFence::serving(1)).is_some(),
        "a re-encoded frame was refused"
    );
    let again: RaftRpcEnvelope =
        serde_json::from_slice(&encoded).expect("a re-encoded frame is an envelope");
    assert_eq!(
        serde_json::to_value(&envelope).expect("an envelope is a JSON value"),
        serde_json::to_value(&again).expect("an envelope is a JSON value"),
        "a Raft frame changed on its round trip"
    );
    Outcome::Accepted
}

/// A DNS query, as any workload sends it to the node's resolver.
///
/// An accepted query must re-encode to a query that is accepted again with
/// the same bytes, and both answers Onion builds from it (an A record and a
/// bare status) must be well-formed responses to it.
pub fn dns_query(data: &[u8]) -> Outcome {
    use crate::onion::dns::{build_a_response, build_status_response, decode_query, parse_query};
    use crate::onion::vip::VirtualIP;
    use hickory_proto::op::{Message, MessageType};

    let Some(message) = decode_query(data) else {
        assert!(
            parse_query(data).is_none(),
            "parse_query took a refused query"
        );
        assert!(build_status_response(data, 3).is_empty());
        return Outcome::Refused;
    };
    let (name, _qtype) = parse_query(data).expect("an accepted query parses");
    assert_eq!(
        name,
        name.to_ascii_lowercase(),
        "a query name kept its case"
    );

    let encoded = message.to_vec().expect("an accepted query re-encodes");
    let again = decode_query(&encoded).expect("a re-encoded query is accepted");
    let reencoded = again.to_vec().expect("an accepted query re-encodes");
    assert_eq!(encoded, reencoded, "a DNS query changed on its round trip");

    let vip = VirtualIP(std::net::Ipv4Addr::new(127, 128, 0, 1));
    for response in [build_a_response(data, vip), build_status_response(data, 3)] {
        assert!(!response.is_empty(), "an accepted query got no answer");
        let response = Message::from_vec(&response).expect("an answer decodes");
        assert_eq!(response.metadata.message_type, MessageType::Response);
        assert_eq!(response.metadata.id, message.metadata.id);
        assert_eq!(response.queries, message.queries);
    }
    Outcome::Accepted
}

/// A council backup file, as an operator hands it to `relish council restore`.
///
/// A decoded envelope must re-encode and decode to an equal envelope. The
/// fuzzer can't forge the GCM tag, so unsealing under any key must fail
/// cleanly rather than panic.
pub fn council_backup(data: &[u8]) -> Outcome {
    use crate::council::backup::{decode_backup, encode_backup, unseal_snapshot};

    let Ok(sealed) = decode_backup(data) else {
        return Outcome::Refused;
    };
    let encoded = encode_backup(&sealed).expect("a decoded backup re-encodes");
    let again = decode_backup(&encoded).expect("a re-encoded backup decodes");
    assert_eq!(sealed, again, "a backup changed on its round trip");
    let _ = unseal_snapshot(&[7u8; 32], &sealed);
    Outcome::Accepted
}

/// A manifest or image index body, as a client pushes it to Pickle.
///
/// The parse is the one `PUT /v2/<name>/manifests/<reference>` does before
/// it looks at the blob store. Every descriptor digest that parses must
/// print back to itself, and the content type Pickle later serves the stored
/// bytes as must be the media type the client declared.
pub fn oci_manifest_push(data: &[u8]) -> Outcome {
    use crate::pickle::api::{
        detect_manifest_content_type, is_supported_media_type, parse_pushed_manifest,
    };
    use crate::pickle::types::Digest;

    let served = detect_manifest_content_type(data);
    let Ok(manifest) = parse_pushed_manifest(data) else {
        return Outcome::Refused;
    };
    if let Some(declared) = manifest.declared_media_type()
        && is_supported_media_type(declared)
    {
        assert_eq!(
            served, declared,
            "a pushed manifest is served as another type"
        );
    }
    for digest in manifest.descriptor_digests() {
        if let Ok(parsed) = Digest::new(digest) {
            assert_eq!(parsed.as_str(), digest, "a digest changed when parsed");
        }
    }
    let _ = manifest.platform_names();
    Outcome::Accepted
}

/// A manifest or image index, as an upstream registry serves it.
///
/// A parsed manifest must serialise and parse back to the same JSON value.
/// Choosing a platform from an index and checking an image's layer sizes
/// must refuse bad input, never panic.
pub fn oci_manifest_upstream(data: &[u8]) -> Outcome {
    use crate::grill::oci_pull::{check_layer_sizes, parse_manifest, platform_descriptor};
    use oci_client::manifest::OciManifest;

    let Ok(manifest) = parse_manifest(data) else {
        return Outcome::Refused;
    };
    match &manifest {
        OciManifest::Image(image) => {
            let _ = check_layer_sizes(image);
        }
        OciManifest::ImageIndex(index) => {
            for architecture in ["amd64", "arm64"] {
                let _ = platform_descriptor(index, architecture);
            }
        }
    }
    let encoded = serde_json::to_vec(&manifest).expect("a parsed manifest re-encodes");
    let again = parse_manifest(&encoded).expect("a re-encoded manifest parses");
    assert_eq!(
        serde_json::to_value(&manifest).expect("a manifest is a JSON value"),
        serde_json::to_value(&again).expect("a manifest is a JSON value"),
        "a manifest changed on its round trip"
    );
    Outcome::Accepted
}

/// The node id every fuzzed join CSR is signed for.
const JOIN_NODE_ID: &str = "fuzz-node";

/// A Node CA, made once per process: generating keys per input would make
/// the target a key-generation benchmark.
fn join_ca() -> &'static CaHierarchy {
    static HIERARCHY: OnceLock<CaHierarchy> = OnceLock::new();
    HIERARCHY.get_or_init(|| {
        crate::sesame::ca::generate_ca_hierarchy("fuzz", b"fuzz-wrapping-ikm")
            .expect("a fuzz CA hierarchy generates")
    })
}

/// A certificate signing request, as a joining node sends it.
///
/// A CSR that the Node CA signs must yield a certificate for the node id the
/// CA chose, never one the CSR asked for, and it must chain to the Node CA.
pub fn join_csr(data: &[u8]) -> Outcome {
    use crate::sesame::ca::{NODE_LEAF_LIFETIME, node_spiffe_uri, sign_node_csr};
    use crate::sesame::types::SerialNumber;

    let hierarchy = join_ca();
    let Ok((certificate_der, _serial)) = sign_node_csr(
        data,
        JOIN_NODE_ID,
        SerialNumber(1),
        NODE_LEAF_LIFETIME,
        &hierarchy.node.signing_keypair,
        &hierarchy.node.certificate_params,
    ) else {
        return Outcome::Refused;
    };
    let (_, certificate) =
        x509_parser::parse_x509_certificate(&certificate_der).expect("a signed CSR is a cert");
    let subject = certificate.subject().to_string();
    assert_eq!(
        subject,
        format!("CN={JOIN_NODE_ID}"),
        "the CSR chose the subject"
    );
    let sans = certificate
        .subject_alternative_name()
        .expect("a node cert's SAN extension parses")
        .expect("a node cert has a SAN extension");
    let uri = node_spiffe_uri(JOIN_NODE_ID);
    assert!(
        sans.value.general_names.iter().all(|name| match name {
            x509_parser::extensions::GeneralName::URI(found) => *found == uri,
            x509_parser::extensions::GeneralName::DNSName(found) => *found == JOIN_NODE_ID,
            _ => false,
        }),
        "the CSR added a SAN: {:?}",
        sans.value.general_names
    );
    crate::sesame::cert::verify_signature(&certificate_der, &hierarchy.node.ca.certificate_der)
        .expect("a signed CSR chains to the Node CA");
    Outcome::Accepted
}

/// An app configuration, as `relish apply` or GitOps hands it to the council.
///
/// A parsed config must serialise back to TOML that parses to an equal
/// config. Validation and template expansion may refuse it, but not panic.
pub fn app_config(data: &[u8]) -> Outcome {
    use crate::config::Config;

    let Ok(text) = std::str::from_utf8(data) else {
        return Outcome::Refused;
    };
    let Ok(config) = Config::parse(text) else {
        return Outcome::Refused;
    };
    let encoded = toml::to_string(&config).expect("a parsed app config re-encodes");
    let again = Config::parse(&encoded).expect("a re-encoded app config parses");
    assert_eq!(config, again, "an app config changed on its round trip");
    let _ = config.validate();
    let _ = config.clone().expand_templates();
    Outcome::Accepted
}

/// A node configuration file, as Bun reads it at start.
///
/// A parsed config must serialise back to TOML that parses to an equal
/// config. Its validation reads the filesystem, so it isn't run here.
pub fn node_config(data: &[u8]) -> Outcome {
    use crate::config::node::NodeConfig;

    let Ok(text) = std::str::from_utf8(data) else {
        return Outcome::Refused;
    };
    let Ok(config) = NodeConfig::parse(text) else {
        return Outcome::Refused;
    };
    let encoded = toml::to_string(&config).expect("a parsed node config re-encodes");
    let again = NodeConfig::parse(&encoded).expect("a re-encoded node config parses");
    assert_eq!(config, again, "a node config changed on its round trip");
    Outcome::Accepted
}
