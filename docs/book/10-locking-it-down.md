# Locking It Down

Phase 4 gave us the cryptographic foundation: a four-tier CA hierarchy, mTLS between nodes, API tokens with Argon2id hashing, secret encryption with age, and Raft log encryption at rest. That's solid infrastructure. But there's a piece missing that any serious distributed system needs: workload identity.

When your API server calls your database, how does the database know it's really the API server? When your payment processor talks to an external cloud service, how does it prove who it is? In Kubernetes, you'd reach for ServiceAccounts, IRSA, or Workload Identity Federation. We're going to build the equivalent from scratch.

## Workload identity

Every workload in Reliaburger gets its own cryptographic identity. Not a shared secret, not a static password, but a proper X.509 certificate with a SPIFFE URI that says exactly what the workload is. Plus an OIDC JWT token for talking to cloud providers.

The design is zero-configuration. Deploy an app, it gets an identity. No annotations, no sidecar injectors, no admission webhooks.

### SPIFFE URIs

SPIFFE (Secure Production Identity Framework for Everyone) gives us a standard way to name workloads. A SPIFFE URI looks like this:

```
spiffe://prod/ns/default/app/api
```

The trust domain is your cluster name. Then namespace, workload type (app or job), and name. Simple, hierarchical, unambiguous. Two workloads with different URIs are different identities. Same URI means same identity.

We already defined the `SpiffeUri` type back in Phase 4:

```rust
pub struct SpiffeUri {
    pub trust_domain: String,
    pub namespace: String,
    pub workload_type: WorkloadType,
    pub name: String,
}

impl SpiffeUri {
    pub fn to_uri(&self) -> String {
        let kind = match self.workload_type {
            WorkloadType::App => "app",
            WorkloadType::Job => "job",
        };
        format!(
            "spiffe://{}/ns/{}/{}/{}",
            self.trust_domain, self.namespace, kind, self.name
        )
    }
}
```

### The CSR model

Here's the key insight: the private key never leaves the worker node. The worker generates a keypair, sends a Certificate Signing Request (CSR) to the council, and the council signs it with the Workload CA. The CSR contains the public key and the SPIFFE URI, but not the private key.

This is the same model that HTTPS certificate authorities use. You generate a key on your server, send a CSR, and get back a signed certificate. The CA never sees your private key.

```rust
pub fn create_workload_csr(
    spiffe_uri: &SpiffeUri,
) -> Result<(Vec<u8>, Vec<u8>), IdentityError> {
    let key_pair = KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256)
        .map_err(|e| IdentityError::KeyGenFailed(e.to_string()))?;
    let private_key_der = key_pair.serialize_der();

    let uri_string = spiffe_uri.to_uri();

    let mut params = CertificateParams::default();
    // CN = SPIFFE URI
    let mut dn = DistinguishedName::new();
    dn.push(DnType::CommonName, &uri_string);
    params.distinguished_name = dn;
    params.is_ca = IsCa::NoCa;

    // The SPIFFE URI goes in the Subject Alternative Name as a URI type
    let uri_san: SanType = SanType::URI(uri_string.try_into()?);
    params.subject_alt_names = vec![uri_san];

    let csr = params.serialize_request(&key_pair)?;
    Ok((csr.der().to_vec(), private_key_der))
}
```

Two things to notice about the Rust here.

First, `rcgen::SanType::URI`. X.509 certificates have a field called Subject Alternative Name (SAN) that can hold DNS names, email addresses, IP addresses, or URIs. SPIFFE uses URI SANs. The `rcgen` crate's `SanType` enum has a variant for each of these, and `SanType::URI` produces the correct ASN.1 tag (tag 6 in the GeneralName CHOICE, per RFC 5280). We don't need to handle ASN.1 encoding manually.

Second, the function returns `(csr_der, private_key_der)` as two separate byte vectors. The CSR goes over the wire. The private key stays put. Rust's ownership model makes this separation explicit in the type system. You can't accidentally send the private key because it's a separate `Vec<u8>` that you'd have to deliberately move.

### Council-side validation

When the council receives a CSR, it doesn't just blindly sign it. It validates:

1. The CSR's URI SAN matches the workload the council expects (based on the scheduler's placement decisions)
2. The requesting node is actually supposed to be running this workload

```rust
pub fn validate_and_sign_csr(
    csr_der: &[u8],
    expected_spiffe_uri: &SpiffeUri,
    serial: SerialNumber,
    workload_ca_keypair: &KeyPair,
    workload_ca_params: &CertificateParams,
) -> Result<Vec<u8>, IdentityError> {
    let csr_params = CertificateSigningRequestParams::from_der(&csr_der_ref)?;

    // Validate the SPIFFE URI SAN
    let expected_uri = expected_spiffe_uri.to_uri();
    let has_matching_uri = csr_params.params.subject_alt_names.iter().any(|san| {
        matches!(san, SanType::URI(uri) if uri.as_str() == expected_uri)
    });
    if !has_matching_uri {
        return Err(IdentityError::CsrValidationFailed(...));
    }

    // Sign with Workload CA, 1-hour lifetime
    let signed = csr_params.signed_by(&ca_cert, workload_ca_keypair)?;
    Ok(signed.der().to_vec())
}
```

The `rcgen` crate gives us the CSR round-trip. We enabled the `x509-parser` feature on rcgen to get `CertificateSigningRequestParams::from_der()`, which parses the incoming CSR and extracts the public key and SANs. Then `signed_by()` creates a new certificate using the CSR's public key (not a new keypair), signed by the Workload CA.

This pattern matters: the worker has the private key, the council has the CA key, and neither side ever sees the other's key.

### Certificates expire fast

Workload certificates live for 1 hour. That's deliberate. Short-lived certificates mean that if a credential is stolen, the window for misuse is tiny. Compare this with Kubernetes ServiceAccount tokens, which historically had no expiry at all.

Rotation happens at the 30-minute mark (half the lifetime). The worker generates a fresh keypair, sends a new CSR, gets a new certificate. The old one is replaced atomically on the tmpfs mount. The workload doesn't need to do anything.

```rust
pub enum RotationState {
    Valid,           // Fresh, no action needed
    NeedsRotation,   // 30 min passed, time to re-CSR
    GracePeriod,     // Council unreachable, extended validity
    Expired,         // Hard expired, workload must stop
}
```

If the council is unreachable when rotation is due, the worker extends the certificate's local validity by up to 4 hours. That gives 5 hours total (1-hour cert + 4-hour grace) before things break. Long enough to survive most outages. The grace period is tracked locally and logged as a security event.

### OIDC JWTs

X.509 certificates are great for mTLS between services. But when your workload needs to talk to AWS, GCP, or Azure, you need an OIDC token. Cloud providers support Workload Identity Federation: "I'll trust tokens signed by your OIDC issuer."

We generate an Ed25519 signing keypair during `relish init` and store it (wrapped) alongside the CA hierarchy. When a workload gets its certificate, it also gets a JWT signed with this key.

```rust
pub fn mint_jwt(
    claims: &WorkloadJwtClaims,
    config: &OidcSigningConfig,
    wrapping_ikm: &[u8],
) -> Result<String, OidcError> {
    let pkcs8_bytes = crypto::unwrap_key(wrapping_ikm, &config.signing_key_wrapped)?;
    let key_pair = Ed25519KeyPair::from_pkcs8(&pkcs8_bytes)?;

    let header = json!({"alg": "EdDSA", "typ": "JWT", "kid": config.key_id});
    let header_b64 = URL_SAFE_NO_PAD.encode(header.to_string().as_bytes());
    let claims_b64 = URL_SAFE_NO_PAD.encode(serde_json::to_string(claims)?.as_bytes());

    let signing_input = format!("{header_b64}.{claims_b64}");
    let sig = key_pair.sign(signing_input.as_bytes());
    let sig_b64 = URL_SAFE_NO_PAD.encode(sig.as_ref());

    Ok(format!("{signing_input}.{sig_b64}"))
}
```

We didn't add a JWT crate for this. JWT is three base64url segments joined by dots. We already have `ring` for Ed25519 signing and `base64` for encoding. About 30 lines of code instead of another dependency.

The JWT claims include the SPIFFE URI as the subject, the cluster name, namespace, node, and instance ID. External verifiers use the JWKS endpoint (which publishes the Ed25519 public key in RFC 8037 OKP format) to validate these tokens.

### Identity delivery

The workload sees its identity at `/run/reliaburger/identity/`:

```
/run/reliaburger/identity/
    cert.pem      — the workload's X.509 certificate
    key.pem       — the private key (never left this node)
    ca.pem        — Workload CA + Root CA chain
    bundle.pem    — cert + CA chain concatenated
    token         — OIDC JWT
```

This is a bind mount from the host into the container. The OCI spec adds it automatically:

```rust
mounts.push(OciMount {
    destination: PathBuf::from("/run/reliaburger/identity"),
    source: Some(identity_host_dir),
    mount_type: Some("bind".to_string()),
    options: vec!["bind".to_string(), "ro".to_string()],
});
```

The directory starts empty. Files appear after the CSR round-trip completes. On rotation, files are updated atomically (write to `.tmp`, rename). A workload that needs mTLS should watch for `cert.pem` to appear, then reload on change.

For process workloads (ProcessGrill), there's no container to mount into. Instead, the `RELIABURGER_IDENTITY_DIR` environment variable points to the host path.

### What we built

The crypto library layer is complete and tested:
- `src/sesame/identity.rs` — CSR creation (worker), validation + signing (council), identity bundles, tmpfs delivery, rotation state machine
- `src/sesame/oidc.rs` — Ed25519 keypair generation, JWT minting and verification, JWKS endpoint response
- `src/sesame/types.rs` — `WorkloadIdentity`, `OidcSigningConfig`, `WorkloadJwtClaims`
- `src/sesame/init.rs` — OIDC keypair generation during cluster bootstrap

The integration hooks are in place:
- `WorkloadInstance` has `identity` and `identity_mount` fields
- OCI spec includes the `/run/reliaburger/identity/` bind mount
- `SecurityState` has `oidc_signing_config` for Raft storage

The full agent-to-council CSR flow (where the agent automatically requests and installs certificates during deploy) depends on `SecurityState` being accessible through the council, which we'll wire up alongside the "SecurityState in Raft" item later in this phase.

## Image signing

Workload identity gives every running process a certificate. Image signing answers a different question: is this container image what we think it is?

You build an image in CI, push it to Pickle, and deploy it. Between push and deploy, someone could swap the image for a compromised one (supply chain attack). Image signing prevents this. The image gets a cryptographic signature at build time. Before scheduling, the scheduler checks the signature. No signature, no deployment.

### Two signing methods

We support two approaches because different teams have different workflows.

**Keyless signing** uses the build job's workload identity. The build job already has an ECDSA P-256 keypair (from its SPIFFE certificate). After pushing the image, it signs the manifest digest with that key and attaches the certificate chain. Verification follows the chain back to the cluster's root CA. No signing keys to manage, rotate, or protect. The keypair is ephemeral -- it exists only for the lifetime of the build job.

**External key signing** works like cosign. Your CI system signs with a long-lived ECDSA P-256 key. You register the public key in the cluster's trust policy. The scheduler verifies incoming signatures against those registered keys.

```rust
pub enum SigningMethod {
    Keyless { issuer: String, identity: String },
    ExternalKey { key_id: String },
}
```

### Signing with ring

The actual signature uses `ring::signature::EcdsaKeyPair` with P-256 SHA-256. The message is the manifest's digest string -- the same `sha256:abc...` string that identifies the image.

```rust
pub fn sign_manifest_digest(
    digest: &Digest,
    private_key_pkcs8: &[u8],
) -> Result<Vec<u8>, SigningError> {
    let key_pair = EcdsaKeyPair::from_pkcs8(
        &signature::ECDSA_P256_SHA256_ASN1_SIGNING,
        private_key_pkcs8,
        &SystemRandom::new(),
    )?;
    let sig = key_pair.sign(&SystemRandom::new(), digest.as_str().as_bytes())?;
    Ok(sig.as_ref().to_vec())
}
```

A nice property of the design: the workload identity's ECDSA P-256 keypair (generated by `rcgen` in `create_workload_csr`) is PKCS#8 DER, which is exactly what `ring::signature::EcdsaKeyPair::from_pkcs8` expects. No format conversion needed. We verified this with a test that generates a workload CSR keypair and then signs a digest with it -- it works because both rcgen and ring use the same PKCS#8 encoding.

### Verification

For keyless signatures, verification has two steps:

1. Verify the certificate chain (leaf cert -> Workload CA -> Root CA). We reuse `sesame::cert::verify_signature()` from Phase 4.
2. Extract the public key from the leaf certificate and verify the ECDSA signature with `ring::signature::UnparsedPublicKey`.

For external key signatures, verification checks that the public key appears in the trust policy's `keys` list, then verifies the signature.

### Trust policy

Signature enforcement is opt-in via node configuration:

```toml
[images.trust_policy]
require_signatures = true
keys = ["MFkwEwYHKoZIzj0CAQ..."]  # base64-encoded ECDSA P-256 public keys
```

When `require_signatures` is `true`, the scheduler calls `check_image_schedulable()` before placement. If the image exists in Pickle's manifest catalog without a signature, scheduling is rejected. Images from external registries (Docker Hub, GHCR) are not checked -- they're not in the catalog.

This design means pushes never fail due to missing signatures. Your CI pipeline keeps working. But unsigned images sit in Pickle, waiting. They're visible in `relish images` but unschedulable until signed. The separation is clean: the registry accepts everything; the scheduler enforces trust.

### Raft integration

Signatures attach to manifests via an `AttachSignature` Raft command. This is a separate operation from `ManifestCommit` because signing happens after push (the build job pushes first, then signs). The state machine adds the signature to the existing manifest entry:

```rust
RaftRequest::AttachSignature(attach) => {
    self.state.manifest_catalog.apply_attach_signature(attach);
}
```

Once written to Raft, the signature is replicated to all council nodes. The scheduler reads it directly from `DesiredState` without re-verifying -- first-write wins.

## SecurityState in Raft

The CA hierarchy, API tokens, join tokens, age keypairs, and OIDC signing config all live in a single `SecurityState` struct. During `relish init`, this struct is generated alongside a 32-byte master secret. The master secret wraps all private keys using HKDF + AES-256-GCM. The struct itself (with its wrapped keys) is safe to replicate, but the master secret must stay off the wire.

### Two files from init

When you run `relish init`, two sensitive files appear alongside the node config:

```
mycluster-master.key              # hex-encoded 32-byte master secret (0o600)
mycluster-security-bootstrap.json # full SecurityState as JSON
```

The master key file is the crown jewel. Lose it and you can't unwrap any CA private key, which means you can't sign new node certificates, workload certificates, or JWTs. Back it up alongside the sealed root CA file.

The bootstrap file is a one-time transfer mechanism. When `bun` starts for the first time, it loads the JSON, writes a `SecurityStateInit` command to Raft, and deletes the file. After that, SecurityState lives in Raft and replicates to every council node automatically.

### How it fits into Raft

`SecurityState` is a field on `DesiredState`, the struct that the Raft state machine maintains:

```rust
pub struct DesiredState {
    pub apps: HashMap<AppId, AppSpec>,
    pub scheduling: HashMap<AppId, Vec<Placement>>,
    pub manifest_catalog: ManifestCatalog,
    // ... other fields ...
    #[serde(default)]
    pub security_state: SecurityState,
}
```

The `#[serde(default)]` annotation means old Raft snapshots (from before this field existed) deserialise cleanly with an empty `SecurityState`. No migration needed.

Six new `RaftRequest` variants handle security state mutations:

- `SecurityStateInit` -- initial bootstrap from the JSON file
- `CreateJoinToken` / `ConsumeJoinToken` -- join token lifecycle
- `CreateApiToken` / `RevokeApiToken` -- API token management
- `AllocateSerial` -- monotonically incrementing certificate serial counter

Every mutation goes through Raft consensus, so all council nodes see the same sequence of token creations, revocations, and serial allocations. No two nodes can accidentally issue the same serial number.

### The master secret stays in memory

Council nodes load the master secret from the key file at startup and hold it in memory:

```rust
pub struct CouncilNode {
    raft: Raft<TypeConfig>,
    state_machine: CouncilStateMachine,
    wrapping_ikm: Option<[u8; 32]>,  // in-memory only
}
```

When a node needs to sign a workload CSR or issue a join certificate, it reads the wrapped CA private key from `SecurityState` (in Raft), unwraps it with the in-memory master secret, performs the cryptographic operation, and discards the unwrapped key. The master secret never appears in Raft, never crosses the network, and never touches disk except in the original key file.
