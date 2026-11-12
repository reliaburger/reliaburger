//! `relish agent verify` and `relish verify`: is this cluster exactly what
//! the release says it is?
//!
//! One read-only check, with one verdict:
//!
//! - `verified`: every node answered, every node's `bun` hashes to the
//!   published build for its version and platform, those published bytes
//!   carry a valid signature from a release key compiled into this `relish`,
//!   every node runs the same version, and this `relish` is a published
//!   build too;
//! - `mismatch`: something isn't the published release (a binary that has
//!   drifted, a bad signature, nodes on different versions, a version or a
//!   platform the release never published);
//! - `unknown`: a piece of evidence is missing (the metadata, a node, the
//!   bytes to check a signature over), so nothing is verified.
//!
//! The release key signs a binary's bytes, not their hash, so checking a
//! signature needs the bytes. The command fetches each release artefact the
//! nodes run once, from the URL the metadata names, or reads it from
//! `--artefacts DIR`. The metadata itself isn't signed, and doesn't need to
//! be: a node passes only when its hash equals the hash of bytes that carry
//! the release signature, so a lying metadata host can make the check fail
//! but never pass.
//!
//! `--metadata FILE` turns the network off, for an air-gapped cluster:
//! nothing is fetched, and whatever evidence wasn't handed over is reported
//! as missing.
//!
//! As in `relish agent upgrade-check`, collection ([`collect`]) is separate
//! from the verdict ([`assess`]), so every verdict is tested with evidence
//! built by hand, without a cluster or a network.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use schemars::JsonSchema;
use serde::Serialize;

use super::budget::{self, Rendered};
use super::envelope::{BuildInfo, CommandName, Envelope, NextStep};
use super::upgrade_check::{NodeEvidence, NodeRole, collect_builds};
use crate::relish::client::BunClient;
use crate::relish::{CommandOutcome, OutputFormat, RelishError};
use crate::upgrade::BinaryVersion;
use crate::upgrade::error::UpgradeError;
use crate::upgrade::metadata::{self, PlatformArtifact, ReleaseMetadata};
use crate::upgrade::signing::{self, PublicKey, SignatureEnvelope};

/// Where releases are published: a release's files are under
/// `<this>/<version>/`.
pub const DEFAULT_RELEASE_BASE: &str =
    "https://github.com/reliaburger/reliaburger/releases/download";

/// The repository `gh release download` fetches from, for the offline hint.
const RELEASE_REPOSITORY: &str = "reliaburger/reliaburger";

/// How long fetching one metadata document may take.
const METADATA_TIMEOUT: Duration = Duration::from_secs(20);

/// How long downloading one release binary may take.
const ARTEFACT_TIMEOUT: Duration = Duration::from_secs(300);

/// Characters of a SHA-256 the human table shows.
const SHORT_SHA256_LEN: usize = 12;

/// The flags of `relish agent verify` and `relish verify`.
#[derive(Debug, Clone, clap::Args)]
pub struct VerifyArgs {
    /// Where releases are published; each version's files are read from
    /// URL/<version>/.
    #[arg(long, value_name = "URL", default_value = DEFAULT_RELEASE_BASE, conflicts_with = "metadata")]
    pub release_url: String,
    /// Read bun's release metadata (metadata.json) from this file, and fetch
    /// nothing from the network.
    #[arg(long, value_name = "FILE")]
    pub metadata: Option<PathBuf>,
    /// Read relish's release metadata (cli-metadata.json) from this file.
    #[arg(long, value_name = "FILE")]
    pub cli_metadata: Option<PathBuf>,
    /// Read the release binaries (bun-<platform>) from this directory instead
    /// of downloading them.
    #[arg(long, value_name = "DIR")]
    pub artefacts: Option<PathBuf>,
}

impl VerifyArgs {
    /// Whether the network is off: `--metadata` was given.
    fn offline(&self) -> bool {
        self.metadata.is_some()
    }
}

/// The answer, in one word.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    /// Every node runs the published, signed bytes of one version, and so
    /// does this `relish`.
    Verified,
    /// Something in `reasons` isn't the published release.
    Mismatch,
    /// Some evidence is missing, so nothing is verified; `reasons` says what.
    Unknown,
}

/// What a reason does to the verdict.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ReasonKind {
    // Proof that something isn't the release: `mismatch`.
    Mismatch,
    // A question went unanswered: `unknown`, unless a mismatch decided it.
    MissingEvidence,
}

/// Why the verdict is what it is, from a closed list.
// `//` notes rather than `///` docs keep the schema's code list compact, as
// in `upgrade_check::ReasonCode`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ReasonCode {
    // A node's binary hashes to something other than the published build.
    HashMismatch,
    // A published binary's signature doesn't verify against a release key.
    BadSignature,
    // A published binary doesn't hash to what its metadata says.
    ArtefactMismatch,
    // The live nodes run different versions.
    VersionSkew,
    // The metadata has no build of a node's version for its platform.
    NoArtefact,
    // A node runs a version the metadata doesn't list.
    UnpublishedVersion,
    // This relish isn't the published build of its version.
    RelishMismatch,
    // The release metadata couldn't be fetched or parsed.
    MetadataUnavailable,
    // The cluster didn't answer.
    ClusterUnreachable,
    // A node didn't answer, or is dead or gone.
    NodeUnanswered,
    // A node didn't say which platform it runs on.
    PlatformUnknown,
    // A node didn't report the SHA-256 of its binary.
    HashUnreported,
    // The published bytes weren't available to check the signature over.
    SignatureUnchecked,
    // This relish couldn't be checked: no metadata, or its own bytes unread.
    RelishUnchecked,
}

impl ReasonCode {
    /// What this reason does to the verdict.
    pub fn kind(self) -> ReasonKind {
        match self {
            ReasonCode::HashMismatch
            | ReasonCode::BadSignature
            | ReasonCode::ArtefactMismatch
            | ReasonCode::VersionSkew
            | ReasonCode::NoArtefact
            | ReasonCode::UnpublishedVersion
            | ReasonCode::RelishMismatch => ReasonKind::Mismatch,
            ReasonCode::MetadataUnavailable
            | ReasonCode::ClusterUnreachable
            | ReasonCode::NodeUnanswered
            | ReasonCode::PlatformUnknown
            | ReasonCode::HashUnreported
            | ReasonCode::SignatureUnchecked
            | ReasonCode::RelishUnchecked => ReasonKind::MissingEvidence,
        }
    }
}

/// One reason behind the verdict.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub struct Reason {
    /// Which reason, from a closed list.
    pub code: ReasonCode,
    /// What it does to the verdict.
    pub kind: ReasonKind,
    /// The node it's about, when it's about one.
    pub node: Option<String>,
    /// One line saying what was found.
    pub message: String,
}

impl Reason {
    fn new(code: ReasonCode, node: Option<&str>, message: impl Into<String>) -> Self {
        Reason {
            code,
            kind: code.kind(),
            node: node.map(str::to_string),
            message: message.into(),
        }
    }
}

/// The outcome of one check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum Check {
    // It was checked and it holds.
    Pass,
    // It was checked and it doesn't.
    Fail,
    // It couldn't be checked.
    Unchecked,
}

impl fmt::Display for Check {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Check::Pass => "pass",
            Check::Fail => "FAIL",
            Check::Unchecked => "unchecked",
        })
    }
}

/// One binary, as the check saw it: a node's `bun`, or this `relish`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, JsonSchema)]
pub struct BinaryReport {
    /// The node's id, `this node` for a node without a council, or `relish`
    /// for the CLI running the check.
    pub node: String,
    /// The part the node plays; `null` for `relish`.
    pub role: Option<NodeRole>,
    /// Its membership state: `alive`, `suspect`, `dead` or `left`; `null`
    /// for `relish`.
    pub state: Option<String>,
    /// The version it runs, when it said.
    pub version: Option<String>,
    /// The `{os}-{arch}` it runs on, when it said.
    pub platform: Option<String>,
    /// The SHA-256 of its binary, as it reported it.
    pub sha256: Option<String>,
    /// The SHA-256 the release publishes for that version and platform.
    pub published_sha256: Option<String>,
    /// Whether the two hashes are equal.
    pub hash: Check,
    /// Whether the published bytes carry a valid release signature.
    pub signature: Check,
}

/// `data` for `relish agent verify`; `relish verify -o json` prints it too.
#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct VerifyData {
    /// The answer in one word.
    pub verdict: Verdict,
    /// Every reason found, mismatches first. `verified` has none.
    pub reasons: Vec<Reason>,
    /// Every node the cluster lists, dead ones included.
    pub nodes: Vec<BinaryReport>,
    /// The `relish` running the check.
    pub relish: BinaryReport,
    /// Where each metadata document came from: a URL or a file.
    pub metadata: Vec<String>,
    /// The Ed25519 release keys the signatures were checked against: the
    /// ones compiled into this `relish`.
    pub trusted_keys: Vec<String>,
}

/// What checking one published binary found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ArtefactCheck {
    /// It hashes to the published SHA-256 and a release key signed it.
    Signed,
    /// It hashes to `actual`, not to the published SHA-256.
    HashMismatch {
        /// What the bytes hash to.
        actual: String,
    },
    /// It hashes right, but no release key signed it.
    BadSignature,
}

/// Check `bytes` against what the metadata publishes for them: the SHA-256,
/// then the embedded Ed25519 signature against `keys`. The same check
/// `bun` makes before it runs an upgrade.
pub fn check_artefact(
    bytes: &[u8],
    artefact: &PlatformArtifact,
    keys: &[PublicKey],
) -> ArtefactCheck {
    let envelope = SignatureEnvelope {
        schema: 1,
        sha256: artefact.sha256.clone(),
        embedded: artefact.embedded_signature.clone(),
        external: None,
    };
    // `network = false`: the operator's external signature is about who may
    // install a binary, not where it came from, so it isn't asked for here.
    match signing::verify_binary(bytes, &envelope, keys, None, false) {
        Ok(()) => ArtefactCheck::Signed,
        Err(UpgradeError::HashMismatch { actual, .. }) => ArtefactCheck::HashMismatch { actual },
        Err(_) => ArtefactCheck::BadSignature,
    }
}

/// One metadata document and where it came from.
#[derive(Debug, Clone)]
pub struct MetadataDocument {
    /// A URL or a file.
    pub source: String,
    /// The document, or why it couldn't be read.
    pub document: Result<ReleaseMetadata, String>,
}

/// What this `relish` found out about itself.
#[derive(Debug, Clone)]
pub struct RelishEvidence {
    /// Its version, e.g. `v0.2.8`.
    pub version: BinaryVersion,
    /// Its `{os}-{arch}`.
    pub platform: String,
    /// The SHA-256 of its own executable, or why it couldn't be read.
    pub sha256: Result<String, String>,
    /// `cli-metadata.json`, or why it couldn't be read.
    pub metadata: MetadataDocument,
    /// Its own bytes checked against the published build; `None` when there
    /// was no published build to check against or the bytes couldn't be read.
    pub check: Option<ArtefactCheck>,
}

/// Everything the verdict is made from.
#[derive(Debug, Clone)]
pub struct Evidence {
    /// Every node and what it said about its build, or why the cluster
    /// didn't answer.
    pub cluster: Result<Vec<NodeEvidence>, String>,
    /// bun's metadata: one document per version the nodes run, or the one
    /// `--metadata` named.
    pub metadata: Vec<MetadataDocument>,
    /// Each published `bun` a live node runs, by version and platform, and
    /// what checking its bytes found, or why they couldn't be checked.
    pub artefacts: BTreeMap<(String, String), Result<ArtefactCheck, String>>,
    /// This `relish`.
    pub relish: RelishEvidence,
    /// The trusted keys, as `ed25519:` strings.
    pub trusted_keys: Vec<String>,
}

impl Evidence {
    /// The published `bun` for `version` on `platform`, from whichever
    /// document lists it.
    fn artefact(&self, version: &BinaryVersion, platform: &str) -> Option<&PlatformArtifact> {
        self.documents()
            .find_map(|document| document.artifact_for(version, platform))
    }

    /// Whether some document lists `version`.
    fn publishes(&self, version: &BinaryVersion) -> bool {
        self.documents()
            .any(|document| document.release(version).is_some())
    }

    fn documents(&self) -> impl Iterator<Item = &ReleaseMetadata> {
        self.metadata
            .iter()
            .filter_map(|document| document.document.as_ref().ok())
    }

    fn all_metadata_read(&self) -> bool {
        self.metadata
            .iter()
            .all(|document| document.document.is_ok())
    }
}

fn is_down(node: &NodeEvidence) -> bool {
    matches!(node.state.as_str(), "dead" | "left")
}

/// A node's version, when it said one `relish` can read.
fn node_version(node: &NodeEvidence) -> Option<BinaryVersion> {
    node.build.as_ref().ok()?.version.parse().ok()
}

/// Turn the evidence into the answer and what to run next. Pure, so every
/// verdict is tested without a cluster or a network.
pub fn assess(evidence: &Evidence) -> (VerifyData, Vec<NextStep>) {
    let mut reasons = Vec::new();
    for document in &evidence.metadata {
        if let Err(reason) = &document.document {
            reasons.push(Reason::new(
                ReasonCode::MetadataUnavailable,
                None,
                format!(
                    "couldn't read the release metadata from {}: {reason}",
                    document.source
                ),
            ));
        }
    }
    let nodes = match &evidence.cluster {
        Ok(nodes) => {
            reasons.extend(cluster_reasons(evidence, nodes));
            nodes
                .iter()
                .map(|node| node_report(evidence, node))
                .collect()
        }
        Err(reason) => {
            reasons.push(Reason::new(
                ReasonCode::ClusterUnreachable,
                None,
                format!("the cluster didn't answer: {reason}"),
            ));
            Vec::new()
        }
    };
    let (relish, relish_reasons) = relish_report(&evidence.relish);
    reasons.extend(relish_reasons);
    // Mismatches first: they're what to act on.
    reasons.sort_by_key(|reason| match reason.kind {
        ReasonKind::Mismatch => 0,
        ReasonKind::MissingEvidence => 1,
    });
    let has = |kind| reasons.iter().any(|reason: &Reason| reason.kind == kind);
    let verdict = if has(ReasonKind::Mismatch) {
        Verdict::Mismatch
    } else if has(ReasonKind::MissingEvidence) {
        Verdict::Unknown
    } else {
        Verdict::Verified
    };
    let next = next_steps(evidence, &reasons, verdict);
    let mut sources: Vec<String> = evidence
        .metadata
        .iter()
        .map(|document| document.source.clone())
        .collect();
    sources.push(evidence.relish.metadata.source.clone());
    let data = VerifyData {
        verdict,
        reasons,
        nodes,
        relish,
        metadata: sources,
        trusted_keys: evidence.trusted_keys.clone(),
    };
    (data, next)
}

fn bun_name(platform: &str) -> String {
    format!("bun-{platform}")
}

/// What one node's answer, or silence, says; then each published binary's
/// own check, once each; then the versions across nodes.
fn cluster_reasons(evidence: &Evidence, nodes: &[NodeEvidence]) -> Vec<Reason> {
    let mut reasons = Vec::new();
    if nodes.is_empty() {
        reasons.push(Reason::new(
            ReasonCode::NodeUnanswered,
            None,
            "the cluster listed no nodes",
        ));
    }
    for node in nodes {
        reasons.extend(node_reasons(evidence, node));
    }
    // A build a live node runs whose bytes nobody checked: the collector
    // always records one, but a gap here must never read as a pass.
    let unrecorded: BTreeSet<(String, String)> = nodes
        .iter()
        .filter(|node| !is_down(node))
        .filter_map(|node| {
            let version = node_version(node)?;
            let platform = node.build.as_ref().ok()?.platform.clone()?;
            evidence.artefact(&version, &platform)?;
            let key = (version.to_string(), platform);
            (!evidence.artefacts.contains_key(&key)).then_some(key)
        })
        .collect();
    for (version, platform) in unrecorded {
        reasons.push(Reason::new(
            ReasonCode::SignatureUnchecked,
            None,
            format!(
                "nobody checked the release signature on {} {version}",
                bun_name(&platform)
            ),
        ));
    }
    for ((version, platform), check) in &evidence.artefacts {
        let name = bun_name(platform);
        match check {
            Ok(ArtefactCheck::Signed) => {}
            Ok(ArtefactCheck::HashMismatch { actual }) => {
                let published = version
                    .parse()
                    .ok()
                    .and_then(|version| evidence.artefact(&version, platform))
                    .map(|artefact| artefact.sha256.as_str())
                    .unwrap_or("unknown");
                reasons.push(Reason::new(
                    ReasonCode::ArtefactMismatch,
                    None,
                    format!(
                        "the published {name} {version} hashes to {actual}, but the metadata says {published}"
                    ),
                ));
            }
            Ok(ArtefactCheck::BadSignature) => reasons.push(Reason::new(
                ReasonCode::BadSignature,
                None,
                format!(
                    "the published {name} {version} carries no valid signature from a release key this relish trusts"
                ),
            )),
            Err(reason) => reasons.push(Reason::new(
                ReasonCode::SignatureUnchecked,
                None,
                format!(
                    "couldn't check the release signature on {name} {version}: {reason}; pass --artefacts DIR holding {name}"
                ),
            )),
        }
    }
    let versions: BTreeSet<BinaryVersion> = nodes
        .iter()
        .filter(|node| !is_down(node))
        .filter_map(node_version)
        .collect();
    if versions.len() > 1 {
        let listed: Vec<String> = versions.iter().map(ToString::to_string).collect();
        reasons.push(Reason::new(
            ReasonCode::VersionSkew,
            None,
            format!(
                "the nodes run different versions ({}); a cluster should run one release",
                listed.join(", ")
            ),
        ));
    }
    reasons
}

fn node_reasons(evidence: &Evidence, node: &NodeEvidence) -> Option<Reason> {
    let name = Some(node.node.as_str());
    if is_down(node) {
        return Some(Reason::new(
            ReasonCode::NodeUnanswered,
            name,
            format!(
                "{} is {}, so its binary can't be checked",
                node.node, node.state
            ),
        ));
    }
    let build = match &node.build {
        Ok(build) => build,
        Err(reason) => {
            return Some(Reason::new(
                ReasonCode::NodeUnanswered,
                name,
                format!("{} didn't say which build it runs: {reason}", node.node),
            ));
        }
    };
    let Some(version) = node_version(node) else {
        return Some(Reason::new(
            ReasonCode::UnpublishedVersion,
            name,
            format!(
                "{} reports version {:?}, which isn't a release version",
                node.node, build.version
            ),
        ));
    };
    let Some(platform) = &build.platform else {
        return Some(Reason::new(
            ReasonCode::PlatformUnknown,
            name,
            format!("{} doesn't say which platform it runs on", node.node),
        ));
    };
    if !evidence.publishes(&version) {
        // An unreadable document already said why; only a complete set of
        // documents proves the version was never published.
        return evidence.all_metadata_read().then(|| {
            Reason::new(
                ReasonCode::UnpublishedVersion,
                name,
                format!(
                    "{} runs {version}, which the release metadata doesn't list",
                    node.node
                ),
            )
        });
    }
    let Some(artefact) = evidence.artefact(&version, platform) else {
        return Some(Reason::new(
            ReasonCode::NoArtefact,
            name,
            format!(
                "{} runs {version} on {platform}, and the release has no {} for it",
                node.node,
                bun_name(platform)
            ),
        ));
    };
    let Some(sha256) = &build.binary_sha256 else {
        return Some(Reason::new(
            ReasonCode::HashUnreported,
            name,
            format!(
                "{} doesn't report the SHA-256 of its binary (it runs without its upgrade manager)",
                node.node
            ),
        ));
    };
    (!sha256.eq_ignore_ascii_case(&artefact.sha256)).then(|| {
        Reason::new(
            ReasonCode::HashMismatch,
            name,
            format!(
                "{}'s bun hashes to {sha256}, but the published {} {version} is {}",
                node.node,
                bun_name(platform),
                artefact.sha256
            ),
        )
    })
}

fn node_report(evidence: &Evidence, node: &NodeEvidence) -> BinaryReport {
    let build = node.build.as_ref().ok();
    let platform = build.and_then(|build| build.platform.clone());
    let sha256 = build.and_then(|build| build.binary_sha256.clone());
    let version = node_version(node);
    let artefact = version
        .as_ref()
        .zip(platform.as_deref())
        .and_then(|(version, platform)| evidence.artefact(version, platform));
    let published_sha256 = artefact.map(|artefact| artefact.sha256.clone());
    let live = !is_down(node);
    let hash = match (&sha256, &published_sha256) {
        (Some(ours), Some(theirs)) if live => {
            if ours.eq_ignore_ascii_case(theirs) {
                Check::Pass
            } else {
                Check::Fail
            }
        }
        _ => Check::Unchecked,
    };
    let signature = match version.as_ref().zip(platform.as_ref()) {
        Some((version, platform)) if live && artefact.is_some() => {
            match evidence
                .artefacts
                .get(&(version.to_string(), platform.clone()))
            {
                Some(Ok(ArtefactCheck::Signed)) => Check::Pass,
                Some(Ok(_)) => Check::Fail,
                Some(Err(_)) | None => Check::Unchecked,
            }
        }
        _ => Check::Unchecked,
    };
    BinaryReport {
        node: node.node.clone(),
        role: Some(node.role),
        state: Some(node.state.clone()),
        version: build.map(|build| build.version.clone()),
        platform,
        sha256,
        published_sha256,
        hash,
        signature,
    }
}

fn relish_report(relish: &RelishEvidence) -> (BinaryReport, Vec<Reason>) {
    let version = &relish.version;
    let platform = relish.platform.as_str();
    let name = format!("relish-{platform}");
    let artefact = relish
        .metadata
        .document
        .as_ref()
        .ok()
        .and_then(|document| document.artifact_for(version, platform));
    let mut reasons = Vec::new();
    let mut report = BinaryReport {
        node: "relish".to_string(),
        role: None,
        state: None,
        version: Some(version.to_string()),
        platform: Some(relish.platform.clone()),
        sha256: relish.sha256.as_ref().ok().cloned(),
        published_sha256: artefact.map(|artefact| artefact.sha256.clone()),
        hash: Check::Unchecked,
        signature: Check::Unchecked,
    };
    let unchecked = |message: String| Reason::new(ReasonCode::RelishUnchecked, None, message);
    if let Err(reason) = &relish.sha256 {
        reasons.push(unchecked(format!(
            "couldn't read this relish's own binary: {reason}"
        )));
    }
    match (&relish.metadata.document, artefact) {
        (Err(reason), _) => reasons.push(unchecked(format!(
            "couldn't read relish's release metadata from {}: {reason}; pass --cli-metadata FILE",
            relish.metadata.source
        ))),
        (Ok(_), None) => reasons.push(Reason::new(
            ReasonCode::RelishMismatch,
            None,
            format!(
                "this relish is {version} for {platform}, and the release metadata has no {name} {version}: a development build?"
            ),
        )),
        (Ok(_), Some(_)) => match &relish.check {
            Some(ArtefactCheck::Signed) => {
                report.hash = Check::Pass;
                report.signature = Check::Pass;
            }
            Some(ArtefactCheck::HashMismatch { actual }) => {
                report.hash = Check::Fail;
                reasons.push(Reason::new(
                    ReasonCode::RelishMismatch,
                    None,
                    format!(
                        "this relish hashes to {actual}, not to the published {name} {version}: a development build, or changed since it was installed"
                    ),
                ));
            }
            Some(ArtefactCheck::BadSignature) => {
                report.hash = Check::Pass;
                report.signature = Check::Fail;
                reasons.push(Reason::new(
                    ReasonCode::BadSignature,
                    None,
                    format!(
                        "this relish is the published {name} {version}, but no release key it trusts signed it"
                    ),
                ));
            }
            // When the hash couldn't be read either, the reason above says so.
            None if relish.sha256.is_err() => {}
            None => reasons.push(unchecked(
                "couldn't read this relish's own bytes to check their signature".to_string(),
            )),
        },
    }
    (report, reasons)
}

/// How to hand over the evidence that's missing: the release's files,
/// fetched where there is internet, and the offline flags that read them.
fn next_steps(evidence: &Evidence, reasons: &[Reason], verdict: Verdict) -> Vec<NextStep> {
    let wants_files = reasons.iter().any(|reason| {
        matches!(
            reason.code,
            ReasonCode::MetadataUnavailable
                | ReasonCode::SignatureUnchecked
                | ReasonCode::RelishUnchecked
        )
    });
    if verdict != Verdict::Unknown || !wants_files {
        return Vec::new();
    }
    let version = evidence
        .cluster
        .as_ref()
        .ok()
        .and_then(|nodes| nodes.iter().find_map(node_version))
        .unwrap_or_else(|| evidence.relish.version.clone());
    vec![
        NextStep {
            run: format!(
                "gh release download {version} --repo {RELEASE_REPOSITORY} --pattern metadata.json --pattern cli-metadata.json --pattern 'bun-*' --dir release"
            ),
            why: "fetch the release's metadata and binaries where there is internet; it writes ./release".to_string(),
            read_only: false,
        },
        NextStep {
            run: "relish agent verify --metadata release/metadata.json --cli-metadata release/cli-metadata.json --artefacts release".to_string(),
            why: "verify again from those files, with no network".to_string(),
            read_only: true,
        },
    ]
}

/// The URL of a release file: `<base>/<version>/<file>`.
fn release_file(base: &str, version: &BinaryVersion, file: &str) -> String {
    format!("{}/{version}/{file}", base.trim_end_matches('/'))
}

async fn read_metadata_file(path: &Path) -> MetadataDocument {
    let document = match tokio::fs::read_to_string(path).await {
        Ok(text) => ReleaseMetadata::parse(&text).map_err(|error| error.to_string()),
        Err(error) => Err(error.to_string()),
    };
    MetadataDocument {
        source: path.display().to_string(),
        document,
    }
}

async fn fetch_metadata(url: String) -> MetadataDocument {
    let document = match tokio::time::timeout(METADATA_TIMEOUT, metadata::fetch(&url)).await {
        Ok(fetched) => fetched.map_err(|error| error.to_string()),
        Err(_) => Err(format!("no answer within {}s", METADATA_TIMEOUT.as_secs())),
    };
    MetadataDocument {
        source: url,
        document,
    }
}

/// Download a release binary. Its bytes are checked against the signature
/// before anything trusts them, so the URL may point anywhere.
async fn download(url: &str) -> Result<Vec<u8>, String> {
    let fetch = async {
        let client = crate::http_client::builder()
            .build()
            .map_err(|error| error.to_string())?;
        let response = client
            .get(url)
            .send()
            .await
            .map_err(|error| error.to_string())?;
        if !response.status().is_success() {
            return Err(format!("{url}: status {}", response.status()));
        }
        let bytes = response.bytes().await.map_err(|error| error.to_string())?;
        Ok(bytes.to_vec())
    };
    match tokio::time::timeout(ARTEFACT_TIMEOUT, fetch).await {
        Ok(answer) => answer,
        Err(_) => Err(format!(
            "{url}: not downloaded within {}s",
            ARTEFACT_TIMEOUT.as_secs()
        )),
    }
}

/// Check bytes off the async runtime: hashing and verifying tens of
/// megabytes is CPU work.
async fn check_off_runtime(
    bytes: Vec<u8>,
    artefact: PlatformArtifact,
    keys: Vec<PublicKey>,
) -> Result<ArtefactCheck, String> {
    tokio::task::spawn_blocking(move || check_artefact(&bytes, &artefact, &keys))
        .await
        .map_err(|error| error.to_string())
}

async fn check_published(
    args: &VerifyArgs,
    platform: &str,
    artefact: &PlatformArtifact,
    keys: &[PublicKey],
) -> Result<ArtefactCheck, String> {
    let name = bun_name(platform);
    let bytes = if let Some(directory) = &args.artefacts {
        let path = directory.join(&name);
        tokio::fs::read(&path)
            .await
            .map_err(|error| format!("{}: {error}", path.display()))?
    } else if args.offline() {
        return Err(
            "--metadata turns the network off, so the published bytes weren't fetched".to_string(),
        );
    } else {
        download(&artefact.url).await?
    };
    check_off_runtime(bytes, artefact.clone(), keys.to_vec()).await
}

/// The SHA-256 of the file at `path`, hashed off the runtime.
async fn hash_file(path: PathBuf) -> Result<String, String> {
    tokio::task::spawn_blocking(move || signing::sha256_file(&path))
        .await
        .map_err(|error| error.to_string())?
        .map_err(|error| error.to_string())
}

/// What this `relish`, the executable at `executable`, finds out about
/// itself.
async fn collect_relish(
    args: &VerifyArgs,
    executable: &Path,
    keys: &[PublicKey],
) -> RelishEvidence {
    let version = crate::upgrade::version::compiled_version();
    let platform = metadata::platform_key();
    let metadata = match &args.cli_metadata {
        Some(path) => read_metadata_file(path).await,
        None if args.offline() => MetadataDocument {
            source: "--cli-metadata (not given)".to_string(),
            document: Err("--metadata turns the network off".to_string()),
        },
        None => {
            fetch_metadata(release_file(
                &args.release_url,
                &version,
                "cli-metadata.json",
            ))
            .await
        }
    };
    let sha256 = hash_file(executable.to_path_buf()).await;
    let artefact = metadata
        .document
        .as_ref()
        .ok()
        .and_then(|document| document.artifact_for(&version, &platform))
        .cloned();
    // Only bytes that hash right are worth holding in memory to check the
    // signature over; anything else is already a mismatch.
    let check = match (&sha256, artefact) {
        (Ok(actual), Some(artefact)) if !actual.eq_ignore_ascii_case(&artefact.sha256) => {
            Some(ArtefactCheck::HashMismatch {
                actual: actual.clone(),
            })
        }
        (Ok(_), Some(artefact)) => match tokio::fs::read(executable).await {
            Ok(bytes) => check_off_runtime(bytes, artefact, keys.to_vec()).await.ok(),
            Err(_) => None,
        },
        _ => None,
    };
    RelishEvidence {
        version,
        platform,
        sha256,
        metadata,
        check,
    }
}

/// Collect everything [`assess`] needs: the nodes' builds, the metadata
/// for each version they run, each published binary they run checked
/// against `keys`, and `executable` (this `relish`) checked against its own
/// release. Reads only.
pub async fn collect(
    args: &VerifyArgs,
    client: &BunClient,
    executable: &Path,
    keys: &[PublicKey],
) -> Evidence {
    let cluster = collect_builds(client).await.map(|(_, nodes)| nodes);
    let live: Vec<&NodeEvidence> = match &cluster {
        Ok(nodes) => nodes.iter().filter(|node| !is_down(node)).collect(),
        Err(_) => Vec::new(),
    };
    let versions: BTreeSet<BinaryVersion> =
        live.iter().filter_map(|node| node_version(node)).collect();
    let metadata = match &args.metadata {
        Some(path) => vec![read_metadata_file(path).await],
        None => {
            futures_util::future::join_all(versions.iter().map(|version| {
                fetch_metadata(release_file(&args.release_url, version, "metadata.json"))
            }))
            .await
        }
    };
    let documents: Vec<&ReleaseMetadata> = metadata
        .iter()
        .filter_map(|document| document.document.as_ref().ok())
        .collect();
    let wanted: BTreeMap<(String, String), PlatformArtifact> = live
        .iter()
        .filter_map(|node| {
            let version = node_version(node)?;
            let platform = node.build.as_ref().ok()?.platform.clone()?;
            let artefact = documents
                .iter()
                .find_map(|document| document.artifact_for(&version, &platform))?;
            Some(((version.to_string(), platform), artefact.clone()))
        })
        .collect();
    let mut artefacts = BTreeMap::new();
    for ((version, platform), artefact) in wanted {
        let check = check_published(args, &platform, &artefact, keys).await;
        artefacts.insert((version, platform), check);
    }
    let relish = collect_relish(args, executable, keys).await;
    Evidence {
        cluster,
        metadata,
        artefacts,
        relish,
        trusted_keys: keys.iter().map(signing::encode_public_key).collect(),
    }
}

/// The release keys compiled into this `relish`.
fn compiled_keys() -> Result<Vec<PublicKey>, UpgradeError> {
    crate::upgrade::keys::release_keys(&crate::config::node::UpgradeSection::default())
}

/// Put the verdict in an envelope and render it.
pub fn answer(evidence: &Evidence, build: BuildInfo, max_bytes: usize, rerun: &str) -> Rendered {
    let (data, next) = assess(evidence);
    let mut envelope = Envelope::answered(CommandName::Verify, build, data);
    for step in next {
        envelope = envelope.with_next(step);
    }
    budget::render(&envelope, max_bytes, rerun)
}

/// Run `relish agent verify` against the configured cluster.
pub async fn run(args: &VerifyArgs, max_bytes: usize, rerun: &str) -> Rendered {
    let keys = match compiled_keys() {
        Ok(keys) => keys,
        Err(reason) => {
            let error = super::AgentError::new(
                super::ErrorCode::Internal,
                format!("this relish's release keys don't parse: {reason}"),
            );
            return super::failure(CommandName::Verify, error, max_bytes, rerun);
        }
    };
    let executable = std::env::current_exe().unwrap_or_default();
    let evidence = collect(args, &BunClient::default_local(), &executable, &keys).await;
    answer(&evidence, BuildInfo::current(), max_bytes, rerun)
}

/// How `relish verify` exits for a verdict: `0` verified, `1` mismatch, `3`
/// unknown, as `relish wtf` exits for clean, critical and incomplete.
pub fn outcome(verdict: Verdict) -> CommandOutcome {
    match verdict {
        Verdict::Verified => CommandOutcome::Clean,
        Verdict::Mismatch => CommandOutcome::Problems,
        Verdict::Unknown => CommandOutcome::Warnings,
    }
}

/// Run `relish verify`: the same check, for a person or a script.
pub async fn run_human(
    args: &VerifyArgs,
    output: OutputFormat,
) -> Result<CommandOutcome, RelishError> {
    let keys = compiled_keys()?;
    let executable = std::env::current_exe()?;
    let evidence = collect(args, &BunClient::default_local(), &executable, &keys).await;
    let (data, next) = assess(&evidence);
    let shown = HumanVerify { data, next };
    match output {
        OutputFormat::Human => crate::relish::output::print_output(&shown, output)?,
        OutputFormat::Json | OutputFormat::Yaml => {
            crate::relish::output::print_output(&shown.data, output)?
        }
    }
    Ok(outcome(shown.data.verdict))
}

/// The human answer: the data, and the next steps as lines.
#[derive(Debug, Clone, Serialize)]
pub struct HumanVerify {
    /// What the check found.
    pub data: VerifyData,
    /// What to run next.
    pub next: Vec<NextStep>,
}

impl fmt::Display for VerifyData {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let headline = match self.verdict {
            Verdict::Verified => {
                let version = self
                    .nodes
                    .first()
                    .and_then(|node| node.version.as_deref())
                    .unwrap_or("the release");
                format!(
                    "verified: every node runs the published bun {version}, byte for byte, signed by a release key"
                )
            }
            Verdict::Mismatch => {
                "mismatch: this cluster isn't exactly the published release".to_string()
            }
            Verdict::Unknown => {
                "unknown: some evidence is missing, so nothing is verified".to_string()
            }
        };
        writeln!(f, "{headline}")?;
        writeln!(f)?;
        writeln!(
            f,
            "{:<16} {:<10} {:<15} {:<14} {:<10} SIGNATURE",
            "NODE", "VERSION", "PLATFORM", "SHA256", "HASH"
        )?;
        for binary in self.nodes.iter().chain(std::iter::once(&self.relish)) {
            let short = binary
                .sha256
                .as_deref()
                .map(|sha| sha.get(..SHORT_SHA256_LEN).unwrap_or(sha))
                .unwrap_or("-");
            writeln!(
                f,
                "{:<16} {:<10} {:<15} {:<14} {:<10} {}",
                binary.node,
                binary.version.as_deref().unwrap_or("-"),
                binary.platform.as_deref().unwrap_or("-"),
                short,
                binary.hash.to_string(),
                binary.signature
            )?;
        }
        if !self.reasons.is_empty() {
            writeln!(f)?;
            for reason in &self.reasons {
                let marker = match reason.kind {
                    ReasonKind::Mismatch => "MISMATCH",
                    ReasonKind::MissingEvidence => "UNKNOWN",
                };
                // The code as the JSON spells it, so a person can search for it.
                let code = serde_json::to_value(reason.code)
                    .ok()
                    .and_then(|code| code.as_str().map(str::to_string))
                    .unwrap_or_default();
                writeln!(f, "{marker} [{code}] {}", reason.message)?;
            }
        }
        Ok(())
    }
}

impl fmt::Display for HumanVerify {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.data)?;
        if !self.next.is_empty() {
            writeln!(f)?;
            for step in &self.next {
                writeln!(f, "next: {}", step.run)?;
                writeln!(f, "      {}", step.why)?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::relish::agent::DEFAULT_MAX_BYTES;
    use crate::relish::client::AgentVersion;
    use crate::upgrade::signing::{generate_keypair, sha256_hex, sign};
    use serde_json::{Value, json};

    const BUN: &[u8] = b"the published bun v0.2.7 for linux-x86_64";
    const RELISH: &[u8] = b"the published relish v0.2.7 for this platform";

    /// A release key that signed the fixtures, and the key a relish would
    /// trust.
    struct Keys {
        private: Vec<u8>,
        public: PublicKey,
    }

    fn keys() -> Keys {
        let (private, public) = generate_keypair().unwrap();
        Keys { private, public }
    }

    fn artefact(bytes: &[u8], keys: &Keys) -> PlatformArtifact {
        PlatformArtifact {
            url: "https://releases.example/bun-linux-x86_64".to_string(),
            sha256: sha256_hex(bytes),
            embedded_signature: sign(&keys.private, bytes).unwrap(),
            external_signature: None,
        }
    }

    fn metadata(version: &str, platform: &str, artefact: PlatformArtifact) -> ReleaseMetadata {
        ReleaseMetadata::parse(
            &json!({
                "schema": 1,
                "latest": version,
                "releases": [{"version": version, "platforms": {platform: artefact}}]
            })
            .to_string(),
        )
        .unwrap()
    }

    fn build(version: &str, sha256: Option<&str>) -> AgentVersion {
        serde_json::from_value(json!({
            "version": version,
            "commit": "50596cfe0000000000000000000000000000beef",
            "binary_sha256": sha256,
            "platform": "linux-x86_64",
        }))
        .unwrap()
    }

    fn node(name: &str, role: NodeRole) -> NodeEvidence {
        NodeEvidence {
            node: name.to_string(),
            role,
            state: "alive".to_string(),
            build: Ok(build("v0.2.7", Some(&sha256_hex(BUN)))),
        }
    }

    fn relish_platform() -> String {
        "linux-aarch64".to_string()
    }

    /// Three council nodes on the published v0.2.7, and a published relish:
    /// everything checks out.
    fn verified() -> Evidence {
        let keys = keys();
        let relish_artefact = artefact(RELISH, &keys);
        Evidence {
            cluster: Ok(vec![
                node("node-1", NodeRole::Leader),
                node("node-2", NodeRole::Council),
                node("node-3", NodeRole::Council),
            ]),
            metadata: vec![MetadataDocument {
                source: format!("{DEFAULT_RELEASE_BASE}/v0.2.7/metadata.json"),
                document: Ok(metadata("v0.2.7", "linux-x86_64", artefact(BUN, &keys))),
            }],
            artefacts: BTreeMap::from([(
                ("v0.2.7".to_string(), "linux-x86_64".to_string()),
                Ok(check_artefact(BUN, &artefact(BUN, &keys), &[keys.public])),
            )]),
            relish: RelishEvidence {
                version: "v0.2.7".parse().unwrap(),
                platform: relish_platform(),
                sha256: Ok(sha256_hex(RELISH)),
                metadata: MetadataDocument {
                    source: format!("{DEFAULT_RELEASE_BASE}/v0.2.7/cli-metadata.json"),
                    document: Ok(metadata(
                        "v0.2.7",
                        &relish_platform(),
                        relish_artefact.clone(),
                    )),
                },
                check: Some(check_artefact(RELISH, &relish_artefact, &[keys.public])),
            },
            trusted_keys: vec![signing::encode_public_key(&keys.public)],
        }
    }

    fn verdict(evidence: &Evidence) -> Verdict {
        assess(evidence).0.verdict
    }

    fn codes(evidence: &Evidence) -> Vec<ReasonCode> {
        assess(evidence)
            .0
            .reasons
            .iter()
            .map(|reason| reason.code)
            .collect()
    }

    fn fixture_build() -> BuildInfo {
        BuildInfo {
            version: "0.2.7".to_string(),
            commit: Some("a1b2c3d4e5f60718293a4b5c6d7e8f9012345678".to_string()),
            dirty: None,
            source_digest: None,
        }
    }

    fn rendered(evidence: &Evidence) -> Value {
        let rendered = answer(
            evidence,
            fixture_build(),
            DEFAULT_MAX_BYTES,
            "relish agent verify",
        );
        assert_eq!(
            rendered.exit.code(),
            0,
            "an answer exits 0, whatever it says"
        );
        serde_json::from_str(&rendered.json).unwrap()
    }

    /// Evidence with fixed keys, so a snapshot doesn't change per run: the
    /// signatures are computed, but only the verdicts are kept.
    fn stable(mut evidence: Evidence) -> Evidence {
        evidence.trusted_keys = vec!["ed25519:a-throwaway-test-key".to_string()];
        evidence
    }

    #[test]
    fn a_cluster_on_the_published_signed_bytes_is_verified() {
        let evidence = verified();
        let (data, next) = assess(&evidence);
        assert_eq!(data.verdict, Verdict::Verified, "{:?}", data.reasons);
        assert!(data.reasons.is_empty());
        assert!(next.is_empty());
        for node in &data.nodes {
            assert_eq!(node.hash, Check::Pass, "{}", node.node);
            assert_eq!(node.signature, Check::Pass, "{}", node.node);
            assert_eq!(
                node.published_sha256.as_deref(),
                Some(sha256_hex(BUN).as_str())
            );
        }
        assert_eq!(data.relish.hash, Check::Pass);
        assert_eq!(data.relish.signature, Check::Pass);
    }

    #[test]
    fn a_node_whose_binary_drifted_is_a_mismatch() {
        let mut evidence = verified();
        let drifted = sha256_hex(b"a bun someone rebuilt");
        if let Ok(nodes) = &mut evidence.cluster {
            nodes[1].build = Ok(build("v0.2.7", Some(&drifted)));
        }
        let (data, _) = assess(&evidence);
        assert_eq!(data.verdict, Verdict::Mismatch);
        assert_eq!(codes(&evidence), [ReasonCode::HashMismatch]);
        assert_eq!(data.reasons[0].node.as_deref(), Some("node-2"));
        assert!(data.reasons[0].message.contains(&drifted));
        assert_eq!(data.nodes[1].hash, Check::Fail);
        assert_eq!(data.nodes[0].hash, Check::Pass);
    }

    #[test]
    fn a_version_the_metadata_doesnt_list_is_a_mismatch() {
        let mut evidence = verified();
        if let Ok(nodes) = &mut evidence.cluster {
            for node in nodes.iter_mut() {
                node.build = Ok(build("v0.2.9", Some(&sha256_hex(BUN))));
            }
        }
        assert_eq!(verdict(&evidence), Verdict::Mismatch);
        assert_eq!(codes(&evidence), [ReasonCode::UnpublishedVersion; 3]);
    }

    #[test]
    fn a_version_relish_cant_read_is_unpublished() {
        let mut evidence = verified();
        if let Ok(nodes) = &mut evidence.cluster {
            nodes[0].build = Ok(build("latest", Some(&sha256_hex(BUN))));
        }
        assert_eq!(codes(&evidence), [ReasonCode::UnpublishedVersion]);
    }

    #[test]
    fn a_platform_the_release_has_no_build_for_is_a_mismatch() {
        let mut evidence = verified();
        if let Ok(nodes) = &mut evidence.cluster {
            let mut answer = build("v0.2.7", Some(&sha256_hex(BUN)));
            answer.platform = Some("linux-riscv64".to_string());
            nodes[2].build = Ok(answer);
        }
        assert_eq!(codes(&evidence), [ReasonCode::NoArtefact]);
    }

    #[test]
    fn nodes_on_different_versions_are_a_mismatch() {
        let mut evidence = verified();
        if let Ok(nodes) = &mut evidence.cluster {
            nodes[2].build = Ok(build("v0.2.6", Some("aa")));
        }
        let found = codes(&evidence);
        assert_eq!(verdict(&evidence), Verdict::Mismatch);
        assert!(found.contains(&ReasonCode::VersionSkew), "{found:?}");
    }

    #[test]
    fn a_published_binary_with_a_bad_signature_is_a_mismatch() {
        let mut evidence = verified();
        // Signed by a key this relish doesn't trust.
        let stranger = keys();
        let trusted = keys();
        let check = check_artefact(BUN, &artefact(BUN, &stranger), &[trusted.public]);
        assert_eq!(check, ArtefactCheck::BadSignature);
        evidence.artefacts.insert(
            ("v0.2.7".to_string(), "linux-x86_64".to_string()),
            Ok(check),
        );
        let (data, _) = assess(&evidence);
        assert_eq!(data.verdict, Verdict::Mismatch);
        assert_eq!(codes(&evidence), [ReasonCode::BadSignature]);
        assert!(data.nodes.iter().all(|node| node.signature == Check::Fail));
        assert!(data.nodes.iter().all(|node| node.hash == Check::Pass));
    }

    #[test]
    fn a_published_binary_that_doesnt_hash_to_its_metadata_is_a_mismatch() {
        let keys = keys();
        let tampered = b"a bun swapped on the mirror";
        let check = check_artefact(tampered, &artefact(BUN, &keys), &[keys.public]);
        assert_eq!(
            check,
            ArtefactCheck::HashMismatch {
                actual: sha256_hex(tampered)
            }
        );
        let mut evidence = verified();
        evidence.artefacts.insert(
            ("v0.2.7".to_string(), "linux-x86_64".to_string()),
            Ok(check),
        );
        assert_eq!(codes(&evidence), [ReasonCode::ArtefactMismatch]);
    }

    #[test]
    fn a_garbled_signature_is_a_bad_signature() {
        let keys = keys();
        let mut published = artefact(BUN, &keys);
        published.embedded_signature = "not base64!".to_string();
        assert_eq!(
            check_artefact(BUN, &published, &[keys.public]),
            ArtefactCheck::BadSignature
        );
    }

    #[test]
    fn an_unreachable_node_is_unknown() {
        let mut evidence = verified();
        if let Ok(nodes) = &mut evidence.cluster {
            nodes[2].build = Err("no answer within 10s".to_string());
        }
        let (data, _) = assess(&evidence);
        assert_eq!(data.verdict, Verdict::Unknown);
        assert_eq!(codes(&evidence), [ReasonCode::NodeUnanswered]);
        assert_eq!(data.nodes[2].hash, Check::Unchecked);
    }

    #[test]
    fn a_dead_node_is_unknown_never_verified() {
        let mut evidence = verified();
        if let Ok(nodes) = &mut evidence.cluster {
            nodes[2].state = "dead".to_string();
            nodes[2].build = Err("not asked: the node is dead".to_string());
        }
        assert_eq!(verdict(&evidence), Verdict::Unknown);
        assert_eq!(codes(&evidence), [ReasonCode::NodeUnanswered]);
    }

    #[test]
    fn an_unreachable_cluster_is_unknown() {
        let mut evidence = verified();
        evidence.cluster = Err("bun agent not reachable at localhost:9117".to_string());
        let (data, _) = assess(&evidence);
        assert_eq!(data.verdict, Verdict::Unknown);
        assert_eq!(codes(&evidence), [ReasonCode::ClusterUnreachable]);
        assert!(data.nodes.is_empty());
    }

    #[test]
    fn a_mismatch_outranks_missing_evidence_and_comes_first() {
        let mut evidence = verified();
        if let Ok(nodes) = &mut evidence.cluster {
            nodes[0].build = Err("timed out".to_string());
            nodes[1].build = Ok(build("v0.2.7", Some("beef")));
        }
        assert_eq!(verdict(&evidence), Verdict::Mismatch);
        assert_eq!(
            codes(&evidence),
            [ReasonCode::HashMismatch, ReasonCode::NodeUnanswered]
        );
    }

    /// A way to lose one piece of a verified cluster's evidence.
    type Breakage = fn(&mut Evidence);

    #[test]
    fn missing_evidence_is_never_verified() {
        let gaps: [(&str, Breakage); 6] = [
            ("no metadata", |e| {
                e.metadata[0].document = Err("status 404 Not Found".to_string())
            }),
            ("no hash", |e| {
                if let Ok(nodes) = &mut e.cluster {
                    nodes[0].build = Ok(build("v0.2.7", None));
                }
            }),
            ("no platform", |e| {
                if let Ok(nodes) = &mut e.cluster {
                    let mut answer = build("v0.2.7", Some(&sha256_hex(BUN)));
                    answer.platform = None;
                    nodes[0].build = Ok(answer);
                }
            }),
            ("no published bytes", |e| {
                for check in e.artefacts.values_mut() {
                    *check = Err("offline".to_string());
                }
            }),
            ("no cli metadata", |e| {
                e.relish.metadata.document = Err("offline".to_string())
            }),
            ("relish unread", |e| {
                e.relish.sha256 = Err("permission denied".to_string());
                e.relish.check = None;
            }),
        ];
        for (gap, make) in gaps {
            let mut evidence = verified();
            make(&mut evidence);
            assert_eq!(verdict(&evidence), Verdict::Unknown, "{gap}");
        }
    }

    #[test]
    fn a_signature_nobody_checked_is_never_verified() {
        let mut evidence = verified();
        evidence.artefacts.clear();
        let (data, _) = assess(&evidence);
        assert_eq!(data.verdict, Verdict::Unknown);
        assert_eq!(codes(&evidence), [ReasonCode::SignatureUnchecked]);
        assert!(
            data.nodes
                .iter()
                .all(|node| node.signature == Check::Unchecked)
        );
        let mut evidence = verified();
        evidence.relish.check = None;
        assert_eq!(codes(&evidence), [ReasonCode::RelishUnchecked]);
    }

    #[test]
    fn a_development_relish_is_a_mismatch() {
        let mut evidence = verified();
        let actual = sha256_hex(b"a relish built from a checkout");
        evidence.relish.sha256 = Ok(actual.clone());
        evidence.relish.check = Some(ArtefactCheck::HashMismatch { actual });
        let (data, _) = assess(&evidence);
        assert_eq!(data.verdict, Verdict::Mismatch);
        assert_eq!(codes(&evidence), [ReasonCode::RelishMismatch]);
        assert_eq!(data.relish.hash, Check::Fail);
    }

    #[test]
    fn a_relish_version_the_cli_metadata_doesnt_list_is_a_mismatch() {
        let mut evidence = verified();
        evidence.relish.version = "v0.2.8".parse().unwrap();
        assert_eq!(codes(&evidence), [ReasonCode::RelishMismatch]);
    }

    #[test]
    fn missing_files_point_at_the_offline_flags() {
        let mut evidence = verified();
        evidence.metadata[0].document = Err("dns error".to_string());
        let (data, next) = assess(&evidence);
        assert!(
            data.reasons[0]
                .message
                .contains("couldn't read the release metadata"),
            "{:?}",
            data.reasons
        );
        assert!(next[0].run.starts_with("gh release download v0.2.7 "));
        assert!(!next[0].read_only, "it writes files");
        assert!(next[1].run.contains("--metadata release/metadata.json"));
        assert!(next[1].read_only);
    }

    #[test]
    fn verify_parses_its_flags() {
        use clap::Parser;
        #[derive(Debug, Parser)]
        struct Cli {
            #[command(flatten)]
            args: VerifyArgs,
        }
        let defaults = Cli::try_parse_from(["verify"]).unwrap().args;
        assert_eq!(defaults.release_url, DEFAULT_RELEASE_BASE);
        assert!(!defaults.offline());
        let offline =
            Cli::try_parse_from(["verify", "--metadata", "m.json", "--artefacts", "release"])
                .unwrap()
                .args;
        assert!(offline.offline());
        assert_eq!(offline.artefacts, Some(PathBuf::from("release")));
        assert!(
            Cli::try_parse_from([
                "verify",
                "--metadata",
                "m.json",
                "--release-url",
                "https://x"
            ])
            .is_err(),
            "a file or the network, not both"
        );
    }

    #[test]
    fn a_release_file_sits_under_its_version() {
        let version: BinaryVersion = "v0.2.7".parse().unwrap();
        assert_eq!(
            release_file(
                "https://mirror.example/releases/",
                &version,
                "metadata.json"
            ),
            "https://mirror.example/releases/v0.2.7/metadata.json"
        );
    }

    #[test]
    fn verify_exits_by_its_verdict() {
        assert_eq!(outcome(Verdict::Verified).exit_code(), 0);
        assert_eq!(outcome(Verdict::Mismatch).exit_code(), 1);
        assert_eq!(outcome(Verdict::Unknown).exit_code(), 3);
    }

    /// A fake council of two that answers `/v1/version` through the relay
    /// with the given SHA-256s.
    async fn fake_cluster(hashes: [String; 2]) -> BunClient {
        use axum::extract::Path as UrlPath;
        use axum::routing::get;
        let nodes = json!([
            {"node_id": "node-1", "address": "10.0.0.1:9443", "state": "alive", "incarnation": 1, "is_council": true, "is_leader": true, "labels": {}},
            {"node_id": "node-2", "address": "10.0.0.2:9443", "state": "alive", "incarnation": 1, "is_council": true, "is_leader": false, "labels": {}}
        ]);
        let app = axum::Router::new()
            .route(
                "/v1/upgrade/cluster",
                get(|| async { axum::Json(json!({"active": null, "history": []})) }),
            )
            .route(
                "/v1/cluster/nodes",
                get(move || async move { axum::Json(nodes) }),
            )
            .route(
                "/v1/nodes/{node}/relay/v1/version",
                get(move |UrlPath(node): UrlPath<String>| {
                    let sha = if node == "node-1" {
                        hashes[0].clone()
                    } else {
                        hashes[1].clone()
                    };
                    async move {
                        axum::Json(json!({
                            "version": "v0.2.7",
                            "binary_sha256": sha,
                            "platform": "linux-x86_64",
                        }))
                    }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, app).await });
        BunClient::new_with_token(&origin, None)
    }

    /// Release files on disk, as `gh release download` leaves them: the
    /// metadata, and the bun binary it describes.
    fn release_dir(keys: &Keys) -> tempfile::TempDir {
        let directory = tempfile::tempdir().unwrap();
        let published = metadata("v0.2.7", "linux-x86_64", artefact(BUN, keys));
        std::fs::write(
            directory.path().join("metadata.json"),
            serde_json::to_string(&published).unwrap(),
        )
        .unwrap();
        std::fs::write(directory.path().join("bun-linux-x86_64"), BUN).unwrap();
        directory
    }

    /// The whole path offline: nodes through the relay, the metadata and
    /// the bytes from files, the signature against a throwaway key, and no
    /// network at all.
    #[tokio::test]
    async fn offline_files_verify_the_nodes_and_drift_shows() {
        let keys = keys();
        let directory = release_dir(&keys);
        let relish = directory.path().join("relish");
        std::fs::write(&relish, b"a relish built from a checkout").unwrap();
        let args = VerifyArgs {
            release_url: "https://unused.invalid".to_string(),
            metadata: Some(directory.path().join("metadata.json")),
            cli_metadata: None,
            artefacts: Some(directory.path().to_path_buf()),
        };
        let client = fake_cluster([sha256_hex(BUN), sha256_hex(b"drifted")]).await;
        let evidence = collect(&args, &client, &relish, &[keys.public]).await;
        let (data, _) = assess(&evidence);
        assert_eq!(data.verdict, Verdict::Mismatch);
        assert_eq!(data.nodes[0].hash, Check::Pass);
        assert_eq!(data.nodes[0].signature, Check::Pass);
        assert_eq!(data.nodes[1].hash, Check::Fail);
        assert_eq!(
            data.reasons.iter().map(|r| r.code).collect::<Vec<_>>(),
            [ReasonCode::HashMismatch, ReasonCode::RelishUnchecked]
        );
        assert_eq!(data.reasons[0].node.as_deref(), Some("node-2"));
        assert_eq!(
            data.relish.sha256,
            Some(sha256_hex(b"a relish built from a checkout"))
        );
    }

    #[tokio::test]
    async fn offline_without_the_bytes_leaves_the_signature_unchecked() {
        let keys = keys();
        let directory = release_dir(&keys);
        let relish = directory.path().join("relish");
        std::fs::write(&relish, RELISH).unwrap();
        let args = VerifyArgs {
            release_url: DEFAULT_RELEASE_BASE.to_string(),
            metadata: Some(directory.path().join("metadata.json")),
            cli_metadata: None,
            artefacts: None,
        };
        let client = fake_cluster([sha256_hex(BUN), sha256_hex(BUN)]).await;
        let evidence = collect(&args, &client, &relish, &[keys.public]).await;
        let (data, next) = assess(&evidence);
        assert_eq!(data.verdict, Verdict::Unknown);
        assert!(data.nodes.iter().all(|node| node.hash == Check::Pass));
        assert!(
            data.nodes
                .iter()
                .all(|node| node.signature == Check::Unchecked)
        );
        assert!(
            data.reasons
                .iter()
                .any(|reason| reason.code == ReasonCode::SignatureUnchecked
                    && reason.message.contains("--artefacts")),
            "{:?}",
            data.reasons
        );
        assert!(next[1].run.contains("--artefacts release"));
    }

    #[tokio::test]
    async fn a_published_relish_checks_itself_against_its_own_metadata() {
        let keys = keys();
        let directory = release_dir(&keys);
        let relish = directory.path().join("relish");
        std::fs::write(&relish, RELISH).unwrap();
        let cli = metadata(
            &crate::upgrade::version::compiled_version().to_string(),
            &metadata::platform_key(),
            artefact(RELISH, &keys),
        );
        let cli_path = directory.path().join("cli-metadata.json");
        std::fs::write(&cli_path, serde_json::to_string(&cli).unwrap()).unwrap();
        let args = VerifyArgs {
            release_url: DEFAULT_RELEASE_BASE.to_string(),
            metadata: Some(directory.path().join("metadata.json")),
            cli_metadata: Some(cli_path),
            artefacts: Some(directory.path().to_path_buf()),
        };
        let unanswered = BunClient::new_with_token("http://127.0.0.1:1", None);
        let evidence = collect(&args, &unanswered, &relish, &[keys.public]).await;
        let (data, _) = assess(&evidence);
        assert_eq!(data.relish.hash, Check::Pass);
        assert_eq!(data.relish.signature, Check::Pass);
        assert_eq!(data.verdict, Verdict::Unknown, "the cluster didn't answer");
    }

    // One snapshot per verdict: each is a shape an agent reads.

    #[test]
    fn snapshot_of_a_verified_cluster() {
        insta::assert_json_snapshot!(rendered(&stable(verified())));
    }

    #[test]
    fn snapshot_of_a_drifted_cluster() {
        let mut evidence = verified();
        if let Ok(nodes) = &mut evidence.cluster {
            nodes[1].build = Ok(build("v0.2.7", Some(&sha256_hex(b"a bun someone rebuilt"))));
        }
        insta::assert_json_snapshot!(rendered(&stable(evidence)));
    }

    #[test]
    fn snapshot_of_an_offline_check_without_the_bytes() {
        let mut evidence = verified();
        for check in evidence.artefacts.values_mut() {
            *check = Err(
                "--metadata turns the network off, so the published bytes weren't fetched"
                    .to_string(),
            );
        }
        evidence.metadata[0].source = "release/metadata.json".to_string();
        evidence.relish.metadata = MetadataDocument {
            source: "--cli-metadata (not given)".to_string(),
            document: Err("--metadata turns the network off".to_string()),
        };
        evidence.relish.check = None;
        insta::assert_json_snapshot!(rendered(&stable(evidence)));
    }

    #[test]
    fn snapshot_of_the_human_answer() {
        let mut evidence = verified();
        if let Ok(nodes) = &mut evidence.cluster {
            nodes[1].build = Ok(build("v0.2.7", Some(&sha256_hex(b"a bun someone rebuilt"))));
            nodes[2].state = "dead".to_string();
            nodes[2].build = Err("not asked: the node is dead".to_string());
        }
        let (data, next) = assess(&stable(evidence));
        insta::assert_snapshot!(HumanVerify { data, next }.to_string());
    }
}
