//! Key-based cosign signatures, in the classic `.sig` tag layout (F03 U3, #361).
//!
//! `cosign sign --key` stores its signature for `repo@sha256:<hex>` as a
//! second image in the same repository, tagged `sha256-<hex>.sig`. Each layer
//! of that image with media type [`SIMPLE_SIGNING_MEDIA_TYPE`] is a small JSON
//! payload naming the signed digest:
//!
//! ```json
//! {"critical":{"identity":{"docker-reference":"ghcr.io/acme/web"},
//!   "image":{"docker-manifest-digest":"sha256:5a90…"},
//!   "type":"cosign container image signature"},"optional":null}
//! ```
//!
//! and the layer's [`SIGNATURE_ANNOTATION`] holds a base64 ECDSA P-256
//! signature (ASN.1 DER, SHA-256) over the payload's exact bytes. A signature
//! counts when some layer verifies under one of the trusted keys *and* its
//! payload names the digest the image was bound to. The signature is checked
//! before the payload is parsed, so nothing untrusted reaches the JSON parser.
//!
//! Keyless signatures (a Fulcio certificate and a Rekor proof in the same
//! layer's other annotations) and the newer Sigstore bundle stored as an OCI
//! referrer are out of scope: this module answers "did one of our keys sign
//! these bytes", and nothing else.

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use ring::signature::{ECDSA_P256_SHA256_ASN1, UnparsedPublicKey};
use serde::Deserialize;

use super::types::{Digest, LayerDescriptor};
use super::upstream::{UpstreamRegistry, UpstreamRoot};
use crate::grill::image::ImageReference;

/// Media type of a cosign signature payload layer.
pub const SIMPLE_SIGNING_MEDIA_TYPE: &str = "application/vnd.dev.cosign.simplesigning.v1+json";

/// Layer annotation holding the base64 signature over the payload.
pub const SIGNATURE_ANNOTATION: &str = "dev.cosignproject.cosign/signature";

/// The `critical.type` every cosign image signature payload carries.
const PAYLOAD_TYPE: &str = "cosign container image signature";

/// DER prefix of a SubjectPublicKeyInfo for an uncompressed P-256 point:
/// `SEQUENCE { SEQUENCE { id-ecPublicKey, prime256v1 }, BIT STRING { 0x04 … } }`.
/// Every P-256 public key cosign writes starts with exactly these bytes, and
/// the 65-byte point follows.
const P256_SPKI_PREFIX: [u8; 26] = [
    0x30, 0x59, 0x30, 0x13, 0x06, 0x07, 0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01, 0x06, 0x08, 0x2a,
    0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07, 0x03, 0x42, 0x00,
];

/// Length of an uncompressed P-256 point: `0x04`, then X and Y.
const P256_POINT_LEN: usize = 65;

/// Signature payloads larger than this are refused before they're read whole.
/// Real payloads are a few hundred bytes.
const MAX_PAYLOAD_BYTES: u64 = 64 * 1024;

/// Most payload layers one signature image may carry. Each signing run adds
/// one; a repository re-signed daily for years still fits.
const MAX_SIGNATURE_LAYERS: usize = 1024;

/// Why a cosign signature check refused an image.
#[derive(Debug, thiserror::Error)]
pub enum CosignError {
    #[error("invalid cosign public key: {0}")]
    InvalidKey(String),
    #[error("no cosign signature at {reference}: {reason}")]
    SignatureUnavailable { reference: String, reason: String },
    #[error("cosign signature {reference} is malformed: {reason}")]
    Malformed { reference: String, reason: String },
    #[error("no cosign signature for {digest} verifies under the trusted keys ({})", reasons.join("; "))]
    NotVerified {
        digest: String,
        reasons: Vec<String>,
    },
}

/// A trusted cosign public key: an ECDSA P-256 point, as `cosign.pub` holds it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CosignKey {
    point: Vec<u8>,
}

impl CosignKey {
    /// Read a `-----BEGIN PUBLIC KEY-----` PEM document (what
    /// `cosign generate-key-pair` writes to `cosign.pub`).
    pub fn from_pem(text: &str) -> Result<Self, CosignError> {
        let block = pem::parse(text).map_err(|e| CosignError::InvalidKey(e.to_string()))?;
        if block.tag() != "PUBLIC KEY" {
            return Err(CosignError::InvalidKey(format!(
                "expected a \"PUBLIC KEY\" PEM block, found {:?}",
                block.tag()
            )));
        }
        let der = block.contents();
        match der.strip_prefix(&P256_SPKI_PREFIX[..]) {
            Some(point) if point.len() == P256_POINT_LEN && point[0] == 0x04 => Ok(Self {
                point: point.to_vec(),
            }),
            _ => Err(CosignError::InvalidKey(
                "not an uncompressed ECDSA P-256 key (cosign's default; RSA and Ed25519 keys aren't supported)"
                    .to_string(),
            )),
        }
    }

    /// Read every key of a rule's `cosign_keys`, refusing the whole list if
    /// one is bad: a typo shouldn't quietly shrink the trusted set.
    pub fn parse_all(pems: &[String]) -> Result<Vec<Self>, CosignError> {
        pems.iter().map(|pem| Self::from_pem(pem)).collect()
    }

    fn verifies(&self, payload: &[u8], signature: &[u8]) -> bool {
        UnparsedPublicKey::new(&ECDSA_P256_SHA256_ASN1, &self.point)
            .verify(payload, signature)
            .is_ok()
    }
}

/// The tag cosign stores a digest's signature under: `sha256-<hex>.sig`.
pub fn signature_tag(digest: &Digest) -> String {
    format!("sha256-{}.sig", digest.hex())
}

/// Where the signature for `image` at `digest` lives: the same registry and
/// repository, at [`signature_tag`].
pub fn signature_reference(image: &ImageReference, digest: &Digest) -> ImageReference {
    ImageReference {
        registry: image.registry.clone(),
        repository: image.repository.clone(),
        tag: signature_tag(digest),
    }
}

/// One payload layer of a signature image, before its bytes are fetched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignatureLayer {
    pub digest: Digest,
    pub size: u64,
    /// The base64 signature from the layer's annotation.
    pub signature: String,
}

impl SignatureLayer {
    /// The descriptor a registry client fetches this layer's blob by.
    pub fn descriptor(&self) -> LayerDescriptor {
        LayerDescriptor {
            digest: self.digest.clone(),
            size: self.size,
            media_type: SIMPLE_SIGNING_MEDIA_TYPE.to_string(),
            platform: None,
        }
    }

    /// Pair the layer with its fetched bytes, which must hash to its digest.
    pub fn with_payload(self, payload: Vec<u8>) -> Result<SignedPayload, String> {
        let actual = super::store::compute_sha256(&payload);
        if actual != self.digest {
            return Err(format!(
                "payload {} hashes to {}",
                self.digest.as_str(),
                actual.as_str()
            ));
        }
        Ok(SignedPayload {
            payload,
            signature: self.signature,
        })
    }
}

/// A payload and the signature over it, ready for [`verify_signature`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignedPayload {
    pub payload: Vec<u8>,
    pub signature: String,
}

#[derive(Deserialize)]
struct SignatureManifest {
    #[serde(default)]
    layers: Vec<ManifestLayer>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ManifestLayer {
    media_type: String,
    digest: String,
    size: u64,
    #[serde(default)]
    annotations: std::collections::BTreeMap<String, String>,
}

/// The payload layers of a signature image's manifest.
///
/// Layers of another media type, and payload layers without a signature
/// annotation, are skipped: they aren't key-based signatures.
pub fn signature_layers(manifest: &[u8]) -> Result<Vec<SignatureLayer>, String> {
    let manifest: SignatureManifest =
        serde_json::from_slice(manifest).map_err(|e| format!("not an image manifest: {e}"))?;
    if manifest.layers.len() > MAX_SIGNATURE_LAYERS {
        return Err(format!(
            "{} layers, more than {MAX_SIGNATURE_LAYERS}",
            manifest.layers.len()
        ));
    }
    let mut layers = Vec::new();
    for layer in manifest.layers {
        if layer.media_type != SIMPLE_SIGNING_MEDIA_TYPE {
            continue;
        }
        let Some(signature) = layer.annotations.get(SIGNATURE_ANNOTATION) else {
            continue;
        };
        if layer.size > MAX_PAYLOAD_BYTES {
            return Err(format!(
                "payload {} is {} bytes, more than {MAX_PAYLOAD_BYTES}",
                layer.digest, layer.size
            ));
        }
        layers.push(SignatureLayer {
            digest: Digest::new(&layer.digest).map_err(|e| e.to_string())?,
            size: layer.size,
            signature: signature.clone(),
        });
    }
    Ok(layers)
}

/// Fetch the signature payloads for `image` at `digest` straight from its
/// registry. A missing `.sig` tag, or a registry that won't answer, is
/// [`CosignError::SignatureUnavailable`]: with `require_signatures` on,
/// either one refuses the image.
pub async fn fetch_signature(
    registry: &dyn UpstreamRegistry,
    image: &ImageReference,
    digest: &Digest,
) -> Result<Vec<SignedPayload>, CosignError> {
    let reference = signature_reference(image, digest);
    let unavailable = |reason: String| CosignError::SignatureUnavailable {
        reference: reference.full_reference(),
        reason,
    };
    let malformed = |reason: String| CosignError::Malformed {
        reference: reference.full_reference(),
        reason,
    };
    let manifest = match registry
        .fetch_root(&reference)
        .await
        .map_err(|e| unavailable(e.to_string()))?
    {
        UpstreamRoot::Image(manifest) => manifest,
        UpstreamRoot::Index(_) => return Err(malformed("an image index, not a manifest".into())),
    };
    let mut payloads = Vec::new();
    for layer in signature_layers(&manifest.manifest_bytes).map_err(malformed)? {
        let bytes = registry
            .fetch_blob(&reference, &layer.descriptor())
            .await
            .map_err(|e| unavailable(e.to_string()))?;
        payloads.push(layer.with_payload(bytes).map_err(malformed)?);
    }
    Ok(payloads)
}

#[derive(Deserialize)]
struct Payload {
    critical: Critical,
}

#[derive(Deserialize)]
struct Critical {
    image: PayloadImage,
    #[serde(rename = "type")]
    kind: String,
}

#[derive(Deserialize)]
struct PayloadImage {
    #[serde(rename = "docker-manifest-digest")]
    docker_manifest_digest: String,
}

/// Accept when some payload's signature verifies under one of `keys` and the
/// payload names `bound`, the digest the image was bound to.
///
/// The refusal lists why each payload failed, so an operator can tell a
/// wrong key from a signature for another digest.
pub fn verify_signature(
    bound: &Digest,
    payloads: &[SignedPayload],
    keys: &[CosignKey],
) -> Result<(), CosignError> {
    let mut reasons = Vec::new();
    if payloads.is_empty() {
        reasons.push("the signature image has no signed payloads".to_string());
    }
    if keys.is_empty() {
        reasons.push("the rule lists no cosign_keys".to_string());
    }
    for (index, signed) in payloads.iter().enumerate() {
        match check_payload(bound, signed, keys) {
            Ok(()) => return Ok(()),
            Err(reason) => reasons.push(format!("payload {}: {reason}", index + 1)),
        }
    }
    Err(CosignError::NotVerified {
        digest: bound.as_str().to_string(),
        reasons,
    })
}

fn check_payload(bound: &Digest, signed: &SignedPayload, keys: &[CosignKey]) -> Result<(), String> {
    let signature = BASE64
        .decode(signed.signature.trim())
        .map_err(|_| "the signature isn't base64".to_string())?;
    if !keys
        .iter()
        .any(|key| key.verifies(&signed.payload, &signature))
    {
        return Err("signature doesn't verify under any trusted key".to_string());
    }
    // Signed by a trusted key, so the bytes are the signer's; now read them.
    let payload: Payload = serde_json::from_slice(&signed.payload)
        .map_err(|e| format!("signed payload isn't a cosign payload: {e}"))?;
    if payload.critical.kind != PAYLOAD_TYPE {
        return Err(format!(
            "signed payload has type {:?}, not {PAYLOAD_TYPE:?}",
            payload.critical.kind
        ));
    }
    let named = payload.critical.image.docker_manifest_digest;
    if named != bound.as_str() {
        return Err(format!("signed payload names {named}, not this image"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pickle::types::PickleError;
    use crate::pickle::upstream::{UpstreamFuture, UpstreamManifest};

    // Real `cosign sign --key` output (cosign v3.1.3, classic `.sig` layout):
    // see tests/fixtures/cosign/ for how it was produced.
    const PAYLOAD: &[u8] = include_bytes!("../../tests/fixtures/cosign/payload.json");
    const SIGNATURE: &str = include_str!("../../tests/fixtures/cosign/signature.b64");
    const PUBLIC_KEY: &str = include_str!("../../tests/fixtures/cosign/cosign.pub");
    const OTHER_KEY: &str = include_str!("../../tests/fixtures/cosign/other.pub");
    const SIGNATURE_MANIFEST: &[u8] =
        include_bytes!("../../tests/fixtures/cosign/signature-manifest.json");
    const SIGNATURE_CONFIG: &[u8] =
        include_bytes!("../../tests/fixtures/cosign/signature-config.json");
    const SIGNED_DIGEST: &str =
        "sha256:5a90fa845f2397b0d429dd19ae5f64aaf0007eaea308c935e7cab63a8c820cce";

    fn signed_digest() -> Digest {
        Digest::new(SIGNED_DIGEST).unwrap()
    }

    fn fixture() -> SignedPayload {
        SignedPayload {
            payload: PAYLOAD.to_vec(),
            signature: SIGNATURE.to_string(),
        }
    }

    fn key(pem: &str) -> CosignKey {
        CosignKey::from_pem(pem).unwrap()
    }

    fn refusal(result: Result<(), CosignError>) -> String {
        match result {
            Err(error @ CosignError::NotVerified { .. }) => error.to_string(),
            other => panic!("expected a refusal, got {other:?}"),
        }
    }

    #[test]
    fn a_real_cosign_signature_verifies_under_its_key() {
        verify_signature(&signed_digest(), &[fixture()], &[key(PUBLIC_KEY)]).unwrap();
    }

    #[test]
    fn any_trusted_key_may_sign() {
        verify_signature(
            &signed_digest(),
            &[fixture()],
            &[key(OTHER_KEY), key(PUBLIC_KEY)],
        )
        .unwrap();
    }

    #[test]
    fn a_signature_under_another_key_is_refused() {
        let message = refusal(verify_signature(
            &signed_digest(),
            &[fixture()],
            &[key(OTHER_KEY)],
        ));
        assert!(
            message.contains("doesn't verify under any trusted key"),
            "{message}"
        );
    }

    #[test]
    fn a_payload_for_another_digest_is_refused() {
        let other = Digest::new(&format!("sha256:{}", "0".repeat(64))).unwrap();
        let message = refusal(verify_signature(&other, &[fixture()], &[key(PUBLIC_KEY)]));
        assert!(
            message.contains(SIGNED_DIGEST),
            "names what was signed: {message}"
        );
        assert!(message.contains("not this image"), "{message}");
    }

    #[test]
    fn a_tampered_payload_is_refused() {
        // Point the payload at another digest without re-signing it.
        let tampered = String::from_utf8(PAYLOAD.to_vec())
            .unwrap()
            .replace("5a90fa84", "5a90fa85");
        assert_ne!(tampered.as_bytes(), PAYLOAD);
        let signed = SignedPayload {
            payload: tampered.into_bytes(),
            signature: SIGNATURE.to_string(),
        };
        let bound = Digest::new(&SIGNED_DIGEST.replace("5a90fa84", "5a90fa85")).unwrap();
        let message = refusal(verify_signature(&bound, &[signed], &[key(PUBLIC_KEY)]));
        assert!(message.contains("doesn't verify"), "{message}");
    }

    #[test]
    fn a_signature_image_with_no_payloads_is_refused() {
        let message = refusal(verify_signature(&signed_digest(), &[], &[key(PUBLIC_KEY)]));
        assert!(message.contains("no signed payloads"), "{message}");
    }

    #[test]
    fn a_rule_with_no_keys_refuses_everything() {
        let message = refusal(verify_signature(&signed_digest(), &[fixture()], &[]));
        assert!(message.contains("no cosign_keys"), "{message}");
    }

    #[test]
    fn one_good_payload_among_bad_ones_is_enough() {
        let junk = SignedPayload {
            payload: b"{}".to_vec(),
            signature: "not base64!".to_string(),
        };
        verify_signature(&signed_digest(), &[junk, fixture()], &[key(PUBLIC_KEY)]).unwrap();
    }

    #[test]
    fn signature_tag_follows_cosigns_layout() {
        assert_eq!(
            signature_tag(&signed_digest()),
            "sha256-5a90fa845f2397b0d429dd19ae5f64aaf0007eaea308c935e7cab63a8c820cce.sig"
        );
        let image = ImageReference::parse("ghcr.io/acme/web:1.2").unwrap();
        let reference = signature_reference(&image, &signed_digest());
        assert_eq!(reference.registry, "ghcr.io");
        assert_eq!(reference.repository, "acme/web");
        assert_eq!(reference.tag, signature_tag(&signed_digest()));
    }

    #[test]
    fn signature_layers_reads_cosigns_manifest() {
        let layers = signature_layers(SIGNATURE_MANIFEST).unwrap();
        assert_eq!(layers.len(), 1);
        assert_eq!(
            layers[0].digest,
            crate::pickle::store::compute_sha256(PAYLOAD)
        );
        assert_eq!(layers[0].size, PAYLOAD.len() as u64);
        assert_eq!(layers[0].signature, SIGNATURE);
    }

    #[test]
    fn signature_layers_skips_other_layers_and_unsigned_payloads() {
        let manifest = serde_json::json!({
            "schemaVersion": 2,
            "layers": [
                {"mediaType": "application/vnd.oci.image.layer.v1.tar+gzip",
                 "digest": SIGNED_DIGEST, "size": 1,
                 "annotations": {SIGNATURE_ANNOTATION: "x"}},
                {"mediaType": SIMPLE_SIGNING_MEDIA_TYPE, "digest": SIGNED_DIGEST, "size": 1},
            ],
        });
        let layers = signature_layers(manifest.to_string().as_bytes()).unwrap();
        assert!(layers.is_empty());
    }

    #[test]
    fn signature_layers_refuses_an_oversized_payload() {
        let manifest = serde_json::json!({
            "schemaVersion": 2,
            "layers": [{"mediaType": SIMPLE_SIGNING_MEDIA_TYPE, "digest": SIGNED_DIGEST,
                        "size": MAX_PAYLOAD_BYTES + 1,
                        "annotations": {SIGNATURE_ANNOTATION: "x"}}],
        });
        assert!(signature_layers(manifest.to_string().as_bytes()).is_err());
    }

    #[test]
    fn a_payload_that_does_not_match_its_layer_digest_is_refused() {
        let layer = signature_layers(SIGNATURE_MANIFEST).unwrap().remove(0);
        let error = layer.with_payload(b"something else".to_vec()).unwrap_err();
        assert!(error.contains("hashes to"), "{error}");
    }

    #[test]
    fn keys_that_are_not_p256_public_keys_are_refused() {
        let private = "-----BEGIN PRIVATE KEY-----\nAAAA\n-----END PRIVATE KEY-----\n";
        assert!(matches!(
            CosignKey::from_pem(private),
            Err(CosignError::InvalidKey(_))
        ));
        let not_p256 = pem::encode(&pem::Pem::new(
            "PUBLIC KEY",
            vec![0x30, 0x03, 0x02, 0x01, 0x00],
        ));
        assert!(matches!(
            CosignKey::from_pem(&not_p256),
            Err(CosignError::InvalidKey(_))
        ));
        assert!(CosignKey::from_pem("not pem").is_err());
        assert!(CosignKey::parse_all(&[PUBLIC_KEY.to_string(), "not pem".to_string()]).is_err());
        assert_eq!(
            CosignKey::parse_all(&[PUBLIC_KEY.to_string(), OTHER_KEY.to_string()])
                .unwrap()
                .len(),
            2
        );
    }

    /// A registry holding the fixture's signature image, or nothing at all.
    struct Registry {
        signed: bool,
    }

    impl UpstreamRegistry for Registry {
        fn head_manifest_digest<'a>(
            &'a self,
            _image: &'a ImageReference,
        ) -> UpstreamFuture<'a, Digest> {
            unimplemented!("signature checks fetch, they don't HEAD")
        }

        fn fetch_manifest<'a>(
            &'a self,
            _image: &'a ImageReference,
        ) -> UpstreamFuture<'a, UpstreamManifest> {
            unimplemented!("signature checks read the root")
        }

        fn fetch_root<'a>(&'a self, image: &'a ImageReference) -> UpstreamFuture<'a, UpstreamRoot> {
            let answer = if self.signed && image.tag == signature_tag(&signed_digest()) {
                Ok(UpstreamRoot::Image(UpstreamManifest {
                    digest: crate::pickle::store::compute_sha256(SIGNATURE_MANIFEST),
                    manifest_bytes: SIGNATURE_MANIFEST.to_vec(),
                    config: LayerDescriptor {
                        digest: crate::pickle::store::compute_sha256(SIGNATURE_CONFIG),
                        size: SIGNATURE_CONFIG.len() as u64,
                        media_type: "application/vnd.oci.image.config.v1+json".into(),
                        platform: None,
                    },
                    config_bytes: SIGNATURE_CONFIG.to_vec(),
                    layers: vec![],
                }))
            } else {
                Err(PickleError::ReplicationFailed(format!(
                    "manifest unknown: {}",
                    image.tag
                )))
            };
            Box::pin(async move { answer })
        }

        fn fetch_blob<'a>(
            &'a self,
            _image: &'a ImageReference,
            layer: &'a LayerDescriptor,
        ) -> UpstreamFuture<'a, Vec<u8>> {
            let answer = if layer.digest == crate::pickle::store::compute_sha256(PAYLOAD) {
                Ok(PAYLOAD.to_vec())
            } else {
                Err(PickleError::BlobNotFound(layer.digest.clone()))
            };
            Box::pin(async move { answer })
        }
    }

    fn image() -> ImageReference {
        ImageReference::parse("ghcr.io/acme/web:1.2").unwrap()
    }

    #[tokio::test]
    async fn fetch_signature_reads_the_sig_tag_and_its_payloads() {
        let payloads = fetch_signature(&Registry { signed: true }, &image(), &signed_digest())
            .await
            .unwrap();
        assert_eq!(payloads, vec![fixture()]);
        verify_signature(&signed_digest(), &payloads, &[key(PUBLIC_KEY)]).unwrap();
    }

    #[tokio::test]
    async fn a_missing_sig_tag_is_refused() {
        let error = fetch_signature(&Registry { signed: false }, &image(), &signed_digest())
            .await
            .unwrap_err();
        assert!(
            matches!(error, CosignError::SignatureUnavailable { .. }),
            "{error:?}"
        );
        let message = error.to_string();
        assert!(
            message.contains(&signature_tag(&signed_digest())),
            "{message}"
        );
    }

    #[tokio::test]
    async fn the_sig_tag_for_another_digest_is_not_this_images_signature() {
        let other = Digest::new(&format!("sha256:{}", "1".repeat(64))).unwrap();
        let error = fetch_signature(&Registry { signed: true }, &image(), &other)
            .await
            .unwrap_err();
        assert!(
            matches!(error, CosignError::SignatureUnavailable { .. }),
            "{error:?}"
        );
    }
}
