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

Our first version of the signer got this subtly, dangerously wrong. It checked that the expected URI SAN was *present* in the CSR, then handed the whole CSR to `signed_by()` — which signs the SAN list exactly as the requester submitted it. Spot the problem? A compromised node could send a CSR containing the expected URI *plus* another workload's SPIFFE URI, or a DNS name like `payments.internal`, and the council would cheerfully sign all of them. The presence check passes; the smuggled names ride along.

The fix is a rule worth framing: **never sign what the requester asked for — sign what the server decided.** The only thing the signer takes from the CSR is its public key. Everything else, the SAN list above all, gets rebuilt server-side from the identity the council expects:

```rust
pub fn validate_and_sign_csr(
    csr_der: &[u8],
    expected_spiffe_uri: &SpiffeUri,
    serial: SerialNumber,
    workload_ca_keypair: &KeyPair,
    workload_ca_params: &CertificateParams,
    now: SystemTime,
) -> Result<Vec<u8>, IdentityError> {
    let csr_params = CertificateSigningRequestParams::from_der(&csr_der_ref)?;

    // The CSR must claim the identity the council expects. This is a
    // request check only — the claimed SAN list is never signed as-is.
    let has_matching_uri = csr_params.params.subject_alt_names.iter().any(|san| {
        matches!(san, SanType::URI(uri) if uri.as_str() == expected_uri)
    });
    if !has_matching_uri {
        return Err(IdentityError::CsrValidationFailed(...));
    }

    // Rebuild the certificate contents server-side.
    let mut params = CertificateParams::default();
    params.subject_alt_names = vec![SanType::URI(expected_uri.try_into()?)];
    // ... DN, key usages, serial ...
    params.not_before = time::OffsetDateTime::from(now - CLOCK_SKEW_BACKDATE);
    params.not_after = time::OffsetDateTime::from(now + WORKLOAD_CERT_LIFETIME);

    let signed = params.signed_by(&csr_params.public_key, &ca_cert, workload_ca_keypair)?;
    Ok(signed.der().to_vec())
}
```

The `rcgen` crate gives us the CSR round-trip. We enabled the `x509-parser` feature on rcgen to get `CertificateSigningRequestParams::from_der()`, which parses the incoming CSR and extracts the public key and SANs. The three-argument `signed_by(public_key, issuer, issuer_key)` on `CertificateParams` is the key move: it builds a certificate from *our* parameters around *their* public key. The worker still has the only copy of the private key, the council has the CA key, and neither side ever sees the other's.

The regression test (`smuggled_csr_sans_are_not_signed`) hand-builds a hostile CSR with two extra SANs, signs it, parses the issued DER, and asserts the SAN set contains exactly one URI — the expected one.

### Certificates expire fast (and exactly)

Workload certificates live for 1 hour. That's deliberate. Short-lived certificates mean that if a credential is stolen, the window for misuse is tiny. Compare this with Kubernetes ServiceAccount tokens, which historically had no expiry at all.

There was a second bug hiding in that sentence, though. The original signer converted `now` and `now + 1 hour` to *calendar dates* with a hand-rolled days-since-epoch helper, because that's the shape `rcgen::date_time_ymd()` wants. A certificate issued at 14:00 got `not_before` and `not_after` both equal to that day's midnight — a "one-hour" certificate that was either valid for the rest of the day or already expired, depending on which side of midnight the maths fell. The fix was to stop converting at all: rcgen's validity fields are `time::OffsetDateTime` values, and the `time` crate implements `From<SystemTime>`, so `time::OffsetDateTime::from(now + WORKLOAD_CERT_LIFETIME)` gives second-precision timestamps directly. The hand-rolled date helpers went in the bin, which is the best kind of fix.

Two details worth copying. The signer takes `now` as a parameter instead of calling `SystemTime::now()` inside — an injected clock, so the test can pin a fixed instant and assert the exact window (`one_hour_certificate_window_is_exact_not_calendar_dates`). And `not_before` is backdated by five minutes (`CLOCK_SKEW_BACKDATE`), because the verifying node's clock rarely agrees with the signer's to the second, and a freshly issued certificate that's "not yet valid" is a miserable bug to chase.

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
    key.pem       — the private key (never left this node, owner-only)
    ca.pem        — Workload CA + Root CA chain
    bundle.pem    — cert + CA chain concatenated
    token         — OIDC JWT (owner-only)
    meta.json     — rotation metadata (no secrets)
```

This is a bind mount from the host into the container. The OCI spec adds it automatically:

```rust
let identity_host_dir = crate::sesame::identity::instance_identity_dir(
    &volumes_dir, instance_id,
);
mounts.push(OciMount {
    destination: PathBuf::from("/run/reliaburger/identity"),
    source: Some(identity_host_dir),
    mount_type: Some("bind".to_string()),
    options: vec!["bind".to_string(), "ro".to_string()],
});
```

The directory starts empty. Files appear after the CSR round-trip completes. On rotation, files are updated atomically (write to `.tmp`, rename). A workload that needs mTLS should watch for `cert.pem` to appear, then reload on change.

For process workloads (ProcessGrill), there's no container to mount into. Instead, the `RELIABURGER_IDENTITY_DIR` environment variable points to the host path.

### One directory per instance, cradle to grave

The host side of that mount deserves its own story, because the first version got the granularity wrong. Identity files landed in `{volumes}/.identity/{namespace}/{app}` — one directory per *app*. Run two replicas of `web` and they share the directory; each CSR round-trip overwrites the other replica's private key and JWT. A rolling redeploy dropped the in-memory rotation entry with the old instance, and an adopted workload came back with `identity: None`, so its certificate silently ran out. Key material also survived `relish stop`, sitting on disk with nothing attached to it.

The fix is one idea applied at every point in the lifecycle: **the identity directory belongs to the instance, not the app.** `{volumes}/.identity/{instance_id}`, created before the container, destroyed with it.

- **Before create.** The agent prepares the directory at the same pre-start seam where it programs egress: the bind-mount source must exist before `runc create`, and this is where the backing mount goes. On Linux, running as root, the directory is a dedicated size-bounded tmpfs (`mode=0700`), so private keys never touch persistent storage. On macOS or rootless it's a plain `0700` directory — a documented gap, same honesty rule as the network chapter's non-runc runtimes.
- **After start.** The CSR round-trip writes the files. The key and token are `0600`, and under root they're chowned to the container's runtime UID (65534) so the workload — not just root — can read its own owner-only key. A `meta.json` sidecar records the SPIFFE URI and the rotation schedule; it holds no secrets.
- **On stop, redeploy, rollback.** The directory (and its tmpfs) goes with the instance. A rolling redeploy provisions the new instance's identity, then removes the retired instance's directory; a failed redeploy removes the half-born instance's directory during rollback.
- **On adoption.** When a new `bun` process adopts workloads from a previous one (a crash restart, a self-upgrade), it rebuilds each `WorkloadIdentity` from the per-instance directory — the PEM files plus `meta.json`. The adopted workload keeps its certificate *and its rotation schedule*: the next rotation fires when the pre-restart one would have, not thirty minutes after whenever the process happened to restart. Rotation state lives on disk, not in process memory. Directories with no live owner (including old app-scoped layouts) get swept at the same moment.

That last point is the general lesson. Any timer you keep only in memory is a timer that resets on restart, and for credentials "resets" means "quietly never fires" (identity was `None`; nothing rotates `None`). If a schedule matters across restarts, it has to be derivable from disk.

### What we built

The crypto library layer is complete and tested:
- `src/sesame/identity.rs` — CSR creation (worker), validation + signing (council), identity bundles, per-instance delivery (tmpfs-backed on Linux root), rotation state machine
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

Two details of the lookup were tightened in Phase 12b (finding IMG1). First, the policy key is the canonical repository path as pushed — `team/app` matches `team/app`, not a basename-stripped `app` — so a nested repository can't slip past its own policy, and an external image whose last path segment happens to match a signed local name isn't checked against the wrong entry. Second, verification returns the manifest *digest* it verified, and the agent deploys that digest-pinned reference (`myapp@sha256:…`) instead of the tag. A tag is mutable; someone can re-point it between the moment you verify and the moment the runtime pulls. The signature covers the digest, so the digest is what runs.

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

### Turning mTLS on: the mode matrix

Persisting identities and building handshake-true configs is nothing until a real listener uses them. The Raft RPC transport is the first to get wrapped. Whether it does is decided by a small matrix, evaluated once at startup, driven by two inputs: is there an identity on disk, and is `[security] require_mtls` set?

| `require_mtls` | identity on disk | what happens |
|---|---|---|
| false | absent | plaintext, as before |
| false | present | plaintext, with a warning to set `require_mtls` |
| true | present | Raft RPC requires client certs; peers dialled over mTLS |
| true | absent, bootstrap node | refuse to start — run `relish init` |
| true | absent, joiner | refuse to start — run `relish join`, then restart |

The two "refuse to start" rows are the important ones. A node that is told to require mTLS but has no certificate cannot speak the internal transport, so rather than limp along in a half-secured state it stops with the exact command that fixes it. This is the deliberate cost of the restart-after-join model: a joiner enrols (writes its identity to disk), then restarts, and on the second boot the identity is present and the matrix lands on the mTLS row. One extra restart per node, once, in exchange for never running in an ambiguous partial-TLS state.

Wiring it through the runtime is mechanical once the pieces from earlier in the chapter exist. `cluster::runtime::start` builds a `TlsAcceptor` from `build_mtls_server_config` and a `TlsConnector` from `build_mtls_client_config`, both sharing one `CrlHandle`. The acceptor goes to `serve_raft_rpc`; the connector goes to the network factory via `new_tls`. The frame read/write functions became generic over `AsyncRead + AsyncWrite` so the exact same code serves a plain `TcpStream` or a `tokio_rustls` stream — the only fork is one `match` on whether TLS is configured. And the `CrlHandle` is handed back to `bun`, whose existing token-refresh ticker now also copies the latest CRL from Raft into it every five seconds, so a `RevokeCertificate` takes effect on a peer's next handshake without anyone restarting anything.

The reporting transport is the second internal listener, and because it was built to the same shape as the Raft transport, extending it is almost copy-paste: `TcpReportingTransport::bind_tls` takes the same optional acceptor and connector, its accept loop and framing became generic over the stream type exactly as the Raft codec did, and `cluster::runtime` hands it the identical acceptor and connector it built for Raft. One `CrlHandle`, one identity, three listeners all revoking in lockstep. The lesson worth keeping: when two transports share a shape, securing the second is a fraction of the work of securing the first. It pays to make your byte-shovelling code generic over the stream before you have a second stream type, not after.

### The third listener is a different animal

The agent API is the third listener, and it does not fit the Raft mould. Raft and reporting are node-to-node: both ends are cluster members with certificates, so requiring a client cert is exactly right. But the API is also reached by the `relish` CLI and by browsers, and neither of those has a node certificate. Require a client cert there and you've locked out every human operator.

So the API gets its own server config, `build_api_server_config`, built with `WebPkiClientVerifier::builder(...).allow_unauthenticated()`: it *offers* client-cert auth (a peer node that presents one is verified and CRL-checked exactly as on the Raft transport) but does not *require* it. A connection with no client cert completes the handshake and falls through to the bearer-token / session-cookie layer from the previous chapter. One subtlety cost a test run: wrapping `WebPkiClientVerifier` in our own `RevocationCheckingClientVerifier` silently reverted the "optional" part, because the `ClientCertVerifier` trait defaults `client_auth_mandatory()` to `true` and our wrapper hadn't overridden it. The fix is to delegate that method (and `offer_client_auth`) to the inner verifier so the API config's "unauthenticated is fine" and the Raft config's "cert required" both flow through. The lesson: when you wrap a trait object, audit *every* defaulted method, not just the ones you meant to change.

Serving axum over TLS needs a hand-rolled accept loop — `axum::serve` takes a plain listener — but it's the same shape the ingress proxy already uses: accept, hand the TCP stream to a per-connection task, run the TLS handshake there (so a slow handshaker can't wedge the accept loop), then feed the decrypted stream to hyper.

The other half is the clients. Turning on API TLS breaks the cluster instantly if the callers keep dialling `http://peer:9117` — placement reconcilers, metric and log fan-out, batch dispatch, build delegation, upgrade directives all talk to peer APIs. Rather than scatter scheme-and-client logic across five subsystems, they share one `ClusterHttp` value: a scheme (`http` or `https`) plus a reqwest client that trusts the cluster CA. `bun` builds it once from the identity at startup — `ClusterHttp::secure(...)` when mTLS is on, `ClusterHttp::plaintext()` otherwise — and hands it to the API state, the reconciler and the upgrade orchestrator. Every peer URL is now `cluster_http.url(authority, path)`, so the scheme is decided in exactly one place. The clients present no client certificate: node-to-node calls still authenticate with the service token, which now simply rides inside TLS instead of over the wire in the clear. And because a node cert's SAN is the node id rather than its IP, that CA-trusting client (like the CLI's `--ca-cert` mode) skips hostname verification — the CA pin is the guarantee, not the name.

The Pickle registry is deliberately *not* on this list. It's a fourth listener with its own exposure story — it serves content-addressed blobs and needs an auth story of its own — so it stays plaintext-and-firewalled for now, tracked as a separate piece of work. Three of the four internal listeners speak mTLS; the fourth is honest about not yet doing so.

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

## Token management

API tokens live in `SecurityState.api_tokens` and are managed through Raft. `relish token create` generates a token (Argon2id-hashed before storage), `relish token list` shows active tokens via the `/v1/token/list` endpoint, and `relish token revoke` removes a token via `/v1/token/revoke`.

Both list and revoke endpoints read from or write to the council's security state directly. The list endpoint formats each token's name, role, and creation timestamp. The revoke endpoint writes a `RevokeApiToken` command to Raft, which removes the token from all council replicas immediately.

## Join token validation

When a new node runs `relish join --token <token> <addr>`, the join handler on the receiving agent now validates the token against SecurityState. The flow:

1. Agent reads SecurityState from the council
2. Calls `sesame::join::validate_and_issue()` — checks the token hash, verifies it's not expired or consumed, issues a node certificate signed by the Node CA
3. Writes `ConsumeJoinToken` to Raft to mark the token as used (preventing replay)
4. Returns the new node's certificate and CA chain

This closes the loop from Phase 4's PKI infrastructure — join tokens now actually work end-to-end.

## Secret key rotation

Secret encryption uses age keypairs stored in SecurityState. Each keypair has a `scope` (cluster-wide or namespace), a `generation` counter, and a `read_only` flag.

Rotation happens in two steps:

**Step 1: `relish secret rotate`** generates a new age keypair (generation N+1) and marks the current keypair (generation N) as `read_only = true`. Both keys now exist in SecurityState. The cluster encrypts new secrets with generation N+1 but can still decrypt old secrets encrypted with generation N.

**Step 2: `relish secret rotate --finalize`** removes all `read_only` keypairs. After this, only generation N+1 exists. Old secrets encrypted with generation N become undecryptable, so the operator must re-encrypt them first.

The Raft commands:

```rust
RotateSecretKey { scope, new_keypair }   // mark old as read-only, add new
FinalizeSecretRotation { scope }          // delete read-only keypairs
```

This dual-key window means rotation is never a cliff. You start it, re-encrypt your secrets at your own pace, then finalise when ready.

### Three invariants that keep the window safe

The dual-key idea is simple, but it only holds if three things are true at once. Getting any one of them wrong turns rotation from "never a cliff" into a data-loss bug.

**Encryption picks the *active* key, not the first one.** After step 1 the vector holds two cluster-wide keypairs: generation N (read-only) and generation N+1. A naïve "find the keypair for this scope" returns whichever comes first, which after a `push` is still the old, retiring key. New secrets would then be sealed under a key you are about to delete. So `active_age_keypair` filters out the read-only keys and takes the highest generation, falling back to the newest of any generation only if every key is read-only:

```rust
pub fn active_age_keypair(&self, scope: &AgeKeyScope) -> Option<&AgeKeypair> {
    self.age_keypairs
        .iter()
        .filter(|kp| &kp.scope == scope && !kp.read_only)
        .max_by_key(|kp| kp.generation)
        .or_else(|| {
            self.age_keypairs
                .iter()
                .filter(|kp| &kp.scope == scope)
                .max_by_key(|kp| kp.generation)
        })
}
```

`filter` keeps only the matching keys, `max_by_key` picks the newest generation, and `or_else` runs the fallback closure only when the first search found nothing. The `&kp.scope == scope` compares two references; Rust dereferences both sides for you when the type derives `PartialEq`.

**Decryption tries every live generation.** A workload sealed under generation N must still open during the window, even though the cluster now encrypts under N+1. The agent asks for *all* keypairs of the scope, newest first, and tries each until one decrypts:

```rust
identities
    .iter()
    .find_map(|id| decrypt_secret(encrypted, id).ok())
```

`find_map` returns the first `Some` and stops. `decrypt_secret(...).ok()` turns a `Result` into an `Option`, so a wrong-key failure just moves on to the next identity instead of aborting. Newest-first ordering means the common case (a freshly re-encrypted secret) matches on the first try.

**Finalise refuses to empty a scope.** `FinalizeSecretRotation` deletes the read-only keys, but only after checking that an active replacement survives:

```rust
let has_active = self.state.security_state.age_keypairs
    .iter()
    .any(|kp| kp.scope == *scope && !kp.read_only);
if has_active {
    self.state.security_state.age_keypairs
        .retain(|kp| kp.scope != *scope || !kp.read_only);
}
```

Without the guard, a stray finalise on a scope that only holds read-only keys would `retain` nothing and leave the scope with no usable key, making every secret sealed under it permanently undecryptable. The guard now answers with a `Refused` response rather than a silent no-op, so the operator learns *why* nothing happened.

The API mirrors the same fail-safe instinct: a malformed body to `/v1/secret/rotate` returns `400`, never a silent default rotation. Mutating cluster key state should take a deliberate request, not a typo.

### Verify before you retire

The three invariants keep the *window* safe. But the window still ended on trust: finalise deleted the old key as soon as an active replacement existed, taking the operator's word that every stored secret had been re-encrypted. Finalise early — before re-applying an app whose config still carries generation-N ciphertext — and that secret is bricked the moment the key is gone. A guard that trusts the operator isn't a guard.

The awkward bit: you can't look at an `ENC[AGE:...]` value and tell which key sealed it. Age ciphertext deliberately doesn't disclose its recipient. So the state machine records it as metadata at the only moment it's knowable — write time. Applying an app spec encrypts new values against the scope's *active* key, so the `AppSpec` entry records the active generation for each encrypted env value:

```rust
pub struct SecretSeal {
    pub scope: AgeKeyScope,
    pub generation: u64,
}
// SecurityState:
pub secret_seals: BTreeMap<String, SecretSeal>,  // "namespace/app/ENV_KEY"
```

A `BTreeMap` rather than a `HashMap`, because it serialises in key order — snapshots stay byte-deterministic across nodes. Finalise now walks every stored app, and any encrypted value whose recorded generation is older than the newest — or that has no record at all, which is what state persisted before this field existed looks like — blocks the retirement and gets named in the error:

```
409 cannot finalise secret rotation: secrets still sealed under an old
generation (re-encrypt and re-apply them first): default/web/DB_PASSWORD
```

The operator's re-encrypt step (encrypt against the new public key, `relish apply`) re-records the seal at the new generation, and the finalise goes through. The `#[serde(default)]` on the field is doing quiet compatibility work here: old snapshots load with an empty map, their secrets count as "unknown generation", and the system fails towards *keeping* the key — the recoverable direction.

Is the record proof? No — it's the write-time assumption made explicit. Re-apply an unchanged config and the seal updates without any real re-encryption. What it catches is the case that actually eats data: finalising while a secret demonstrably hasn't been touched since the old generation.

One more rule rounds it off: **one rotation at a time.** A second `RotateSecretKey` while a read-only key still exists for the scope is refused (finalise or re-encrypt first); a duplicate delivery of the *same* rotation is recognised by its generation number and accepted idempotently. Both refusals travel as a new `CouncilResponse::Refused { reason }` variant — appended to the enum, never inserted, because responses cross the bincode-encoded wire where variants are identified by index.

## Certificate revocation

Sometimes you need to revoke a certificate before it expires. A node gets compromised, a workload's key leaks, or you rotate a CA. The Certificate Revocation List (CRL) tracks which serial numbers are no longer trusted.

The CRL lives in `SecurityState`, replicated through Raft:

```rust
pub struct Crl {
    pub entries: Vec<CrlEntry>,
    pub version: u64,
    pub updated_at: SystemTime,
}

pub struct CrlEntry {
    pub serial: SerialNumber,
    pub issuer: CaRole,
    pub revoked_at: SystemTime,
    pub reason: String,
}
```

When the council leader processes a `RevokeCertificate` command, it appends an entry and bumps the version. Any node can read the CRL from `council.security_state()` and check whether a peer's certificate serial appears in the list.

The check itself is simple:

```rust
pub fn check_crl(serial: SerialNumber, crl: &Crl) -> Result<(), CertError> {
    if let Some(entry) = crl.entries.iter().find(|e| e.serial == serial) {
        return Err(CertError::Revoked { serial, reason: entry.reason.clone() });
    }
    Ok(())
}
```

CRLs are small (one entry per revoked cert, most clusters have few revocations). They propagate through Raft replication, so all council members have the same CRL. Worker nodes read it from their assigned council parent.

### Wiring the CRL into TLS handshakes

For a long time `check_crl()` had exactly one caller: its own tests. Making revocation real meant getting it into the TLS handshake path, and that surfaced three problems worth walking through.

First, a design choice. rustls's `WebPkiClientVerifier` can enforce revocation natively, but it wants DER-encoded X.509 CRL files. We could generate those with rcgen — except every rebuild would need the Node CA private key (an unwrap of Raft-held material), plus a signing and expiry lifecycle for an artifact whose source of truth is already replicated and instantly fresh. So instead the verifiers share a `CrlHandle`, an `Arc<RwLock<Crl>>` that every handshake re-reads and a background ticker refreshes from `SecurityState`. Note it's `std::sync::RwLock`, not tokio's: rustls verifier traits are synchronous, and the critical section is a `Vec` scan. The "never `std::sync::Mutex` in async code" rule is about holding locks across `.await` points; there are none here.

Second, a bug the old tests couldn't see. Our node certificates are signed by the Node CA *intermediate*, but the original config builders presented only the leaf and trusted only the Root CA. WebPKI can't build that path — every real handshake would have failed. The old tests only *constructed* configs, so they passed for months. The rewritten tests run actual handshakes over an in-memory `tokio::io::duplex` pipe, and the builders now present `[leaf, node-ca]`.

There's a related wrinkle on the client side. Peer nodes are dialled by gossip IP and their certificates carry no DNS SANs, so rustls's default hostname verification would reject every peer. The client uses a custom `PinnedChainServerVerifier` instead: validate the presented certificate against the *pinned* cluster CAs, check the CRL, skip the hostname. The property we need is "this peer holds a valid, unrevoked Node CA certificate", and that's exactly what it checks — no more, no less.

Third, the one the new tests caught on their first run. `crl_updates_apply_to_new_handshakes_without_rebuilding_configs` revokes a client's serial through the shared handle, reconnects, and expects a refusal. It got a success. TLS 1.3 session resumption: the client had a ticket from its first connection, and a resumed session skips client-certificate verification entirely. A freshly revoked node could have reconnected forever on old tickets. The server config now disables resumption (`NoServerSessionStorage`, zero tickets) — cluster connections are few and long-lived, so paying for a full handshake each time is nothing next to a revocation that doesn't revoke.

## Egress DNS resolution

Apps can restrict outbound connections to specific destinations via `[egress] allow = ["api.stripe.com:443"]`. The initial resolution happens at deploy time using synchronous DNS. But hostnames can change IPs.

We added an async re-resolution function that uses `tokio::net::lookup_host()` instead of blocking `ToSocketAddrs`. The agent can call this periodically (e.g. every 5 minutes in the health tick) to detect IP changes and update the BPF egress maps.

The key difference from the sync version: it doesn't block the tokio runtime, so it can run alongside health checks and identity rotation without stalling the event loop.

### Actually enforcing it

For a long time that resolution went nowhere. The parser worked, the async re-resolver worked, `egress_to_bpf_entries` built the map entries — but nothing wrote them to the kernel, so `[egress] allow = […]` was a comment. Wiring it in raises one genuinely new problem: **which cgroup?**

The eBPF `connect4` hook, when it sees a non-VIP destination, asks `bpf_get_current_cgroup_id()` — the cgroup of whichever process is calling `connect()` — and looks that id up in `egress_enabled_map`. If enforcement is on for that cgroup, the destination must be present in `egress_map` or the connection is refused. So the userspace side has to know each instance's cgroup id, and it has to be *the same number the kernel will produce*.

On cgroup v2 that number is the inode of the instance's cgroup directory. We read `/proc/<pid>/cgroup` — the `0::<path>` line is the v2 path — join it under `/sys/fs/cgroup`, and `stat` the directory. Its `st_ino` is exactly what `bpf_get_current_cgroup_id()` returns. (This is our first time reaching for `MetadataExt::ino()` — the Unix-specific extension trait that exposes the raw inode number that portable `std::fs::Metadata` hides.)

The rest is bookkeeping. When an instance reaches Running, the agent resolves its cgroup id, resolves the allowlist (DNS off the event loop via `spawn_blocking`), writes an `egress_map` entry per destination, and flips `egress_enabled_map` for the cgroup. When the instance stops, it clears the enable flag *and* deletes each allow entry. We first shipped this skipping the delete, reasoning that a fresh cgroup always gets a unique id so stale entries are inert — but the kernel *recycles* cgroup ids, and a future instance that lands on a recycled id would inherit a departed workload's allowlist. So now we scrub both.

Two subtleties took a second pass to get right, and both were the same shape of bug: enforcement that silently isn't there.

**Rolling redeploy.** The fresh-deploy path programmed egress. The rolling-redeploy path — which creates replacement instances inline, for speed — quietly didn't, so every *redeployed* workload ran with no egress rules at all. A workload you never redeploy is enforced; the moment you ship a new version, enforcement evaporates. The fix funnels both paths through one `finish_instance_networking` helper (backend map, namespace firewall, egress together), so a step the fresh path performs can't be dropped on the redeploy path. The same helper now also runs on crash-restart, which likewise hands the instance a fresh cgroup.

**Failing closed.** The original code bailed on the first error — DNS didn't resolve, a map write failed — and returned having enforced nothing. That's *fail-open*: the instance runs with unrestricted egress, the exact opposite of what `[egress] allow` asked for. Now, once we know egress is configured and eBPF is loaded, we enable enforcement *first* (an enabled flag with no allow entries denies everything) and only then open destinations one at a time. A failure anywhere after that point leaves the cgroup denying all egress, and the periodic re-resolve fills the allowlist back in once DNS recovers. Deny-by-default is only a safe default if the failure path defaults to it too.

Two honesty notes. First, this is eBPF-only: a build without the feature, or a non-Linux host, warns loudly that `[egress]` is *not* being enforced rather than silently allowing everything (that was deliberate — see the deferred-nftables discussion below). Second, verifying it needs the kernel. The `egress_denied_by_default_allowed_when_listed` test in `tests/ebpf.rs` runs in the Lima VM: it loads the real program, allows a single `127.0.0.1:port` for the *test's own* cgroup (the process doing the connecting, so the ids line up), and asserts the listed port connects while an unlisted one is refused with `EPERM`. As with the connect rewrite in Chapter 3, `EPERM` — not `ECONNREFUSED` — is what a BPF `return 0` becomes.

### The IPv6 bypass

Here's an uncomfortable question we had to ask about all of the above: what happens when the workload calls `connect()` on an IPv6 socket?

The answer, for longer than we'd like to admit, was "nothing". The policy lived in the `cgroup/connect4` hook, which the kernel only invokes for IPv4 connects. The parser made it worse: DNS results were filtered to A records, so even the allowlist itself quietly threw away the AAAA half of a dual-stack service. Put together, a v4-only allowlist wasn't a policy. It was a suggestion — any workload with an IPv6 route could reach anything it liked, over v6.

The fix has three parts. The parser now keeps AAAA records and accepts explicit v6 entries (`[2001:db8::1]:443` — brackets required, because in `2001:db8::1:443` you can't tell the port from the last address group). A new `cgroup/connect6` program mirrors the enforcement logic against an `egress6_map`. And the v4-mapped form gets special handling: a dual-stack socket connecting to an IPv4 server goes through *connect6* carrying `::ffff:a.b.c.d`, so the hook unwraps the mapped address and judges it against the v4 policy. Miss that and you've just built a third door.

There's a policy decision hiding in the failure mode. If the kernel refuses to attach connect6 (it shouldn't — the hook predates our 5.7 minimum — but hardware surprises you), what do you do with an app that has an allowlist? Loading "degraded" and enforcing v4-only means shipping the exact bug we just fixed. So the agent refuses the deploy instead: an allowlist you can trivially bypass is not enforced, and we'd rather say so at deploy time than pretend. The Lima test `v4_only_allowlist_no_longer_bypassed_over_ipv6` pins the regression: the same v6 connect that used to succeed must now come back `EPERM`.

### CIDR entries and LPM tries

`allow = ["10.0.0.0/8:5432"]` used to be an error ("CIDR notation not yet supported"). The reason is structural: `egress_map` is a hash map, and a hash map can only answer exact-match questions. "Is 10.1.2.3 inside 10.0.0.0/8?" is not a lookup, it's a prefix match.

Kernels solved this long ago for routing tables: `BPF_MAP_TYPE_LPM_TRIE`, a longest-prefix-match trie. Its key is `{ u32 prefixlen; u8 data[] }` — you insert `10.0.0.0` with a prefix length, and a lookup with a full-length key returns the entry with the longest matching prefix. On the Rust side aya exposes this as `LpmTrie<_, K, V>`, with keys built via `Key::new(prefix_len, data)`. The `K` here is just `[u8; 12]` — a plain byte array, because the trie matches raw bits and we want full control of their layout.

Why 12 bytes? Our tries are per-cgroup, so the key data is the cgroup id (8 bytes) followed by the address (4 for v4, 16 for v6), and the prefix length is `64 + cidr_prefix`: the cgroup id is always fully matched, then the address prefix. One trap: the trie compares bytes from the front, so both sides must serialise the cgroup id identically. We write it big-endian on both sides — the C hook byte by byte, the Rust side with `to_be_bytes()`.

Ports don't prefix-match, so they live in the value: a fixed array of up to eight allowed ports the hook scans (a BPF-friendly bounded loop). And longest-prefix matching has a sharp edge — with `10.0.0.0/8:443` and `10.1.0.0/16:80` both configured, a connect to `10.1.2.3:443` matches only the more specific /16 entry and would be denied, even though the /8 allows it. Userspace fixes this at programming time: `merge_cidr_ports` folds the ports of every enclosing prefix into each more specific entry, so the single entry the trie returns carries the union. It's a pure function, tested on macOS; more than eight merged ports on one prefix is a config error, reported at deploy time rather than mis-enforced at runtime.

The parser grew the matching syntax: `10.0.0.0/8:443` and `[2001:db8::/32]:443`, prefix lengths bounds-checked per family. A network address with host bits set (`10.1.2.3/8`) is rejected with the normalised form in the error message — silently reinterpreting a typo'd firewall rule is how you allow things nobody asked for.

### Closing the start window

One more gap, and this one is about *time* rather than address families. Enforcement was programmed after `start`: the agent needed a pid to find the instance's cgroup id, and a pid needs a running process. But the connect hook treats "cgroup not in `egress_enabled_map`" as "no policy, allow everything". So every instance began life with a window — process running, maps not yet written — in which it could connect anywhere. A malicious image doesn't politely wait for your enforcement to arrive; the first thing in `ENTRYPOINT` runs inside that window.

The obvious fix is "program the maps before start", which immediately raises the question the pid used to answer: what's the cgroup id of a process that doesn't exist yet? Our runc runtime deliberately uses `runc run` (create, start, wait and delete in one child — it's the only way the runc CLI hands you the exit code), so there's no created-but-idle container to inspect either. The answer turned out to be pleasingly simple: the cgroup id *is* the inode of the cgroup directory, and nothing says the runtime has to be the one that creates that directory. The agent now does `create_dir_all` on the instance's `cgroupsPath` itself, stats the inode, programs the enforcement flag and allow entries against it, and only then lets the runtime start the workload — which joins the existing directory, inode untouched. Create → program → start. The window is gone.

Failure handling is stricter here than post-start, because we still *can* refuse: if the maps can't be programmed, the created container is removed and the deploy fails, rather than starting a workload ahead of its policy. (A transient DNS failure is gentler — enforcement goes on with an empty allowlist, deny-all, and the re-resolver fills it in.) A `Grill` method, `honours_cgroup_path()`, gates the whole path: root-mode runc says yes; ProcessGrill and Apple Container say no, because they don't place workloads into our cgroup tree at all. For those runtimes the pid-based post-start path still applies and the window still exists — that's a documented limitation of process-style runtimes, not something we paper over.

Proving the ordering in a test is neat: the Lima test deploys through a mock grill whose `pid()` returns `None`, so the post-start path *cannot* have programmed anything. If the enforcement flag is set for the cgroup directory's inode after the deploy, only the pre-start path can have put it there.

### Sweeping against kernel truth

Everything so far maintains the maps *transactionally*: deploy writes, stop deletes. But the kernel's map state and the agent's in-memory picture can still drift — Bun restarts and adopts running workloads (their bindings were in the dead process's memory), a transient map write fails, an unclean shutdown leaves entries behind for cgroup ids the kernel will happily recycle.

So the agent now treats the kernel as something to *reconcile against*, not just write to. Every `[ebpf] sweep_interval_secs` (default 60), it enumerates the enforcement flags and allow entries actually in the kernel, compares them with the cgroups its live instances expect, and acts on the difference: state belonging to no live instance is scrubbed across all four maps plus the flag; live instances that lost their binding (the adoption case) get egress reprogrammed from their stored spec; and every live binding's entries are rewritten — inserts are idempotent, and that's what heals an entry a failed write left missing. Stale `cgroup_namespace_map` keys get the same treatment. A sweep with nothing to do logs nothing; you only hear from it when it found drift, which is exactly when you want to hear from it.

## What we learned

**A JWT is three base64url segments, not a dependency.** We minted OIDC tokens in about 30 lines using `ring` (which we already had for Ed25519) and `base64`. Pulling in a JWT crate would have added a dependency and its transitive tree to encode `header.claims.signature`. When the format is this simple and you already own the crypto, write the 30 lines.

**Short-lived certificates need a grace period, or availability suffers.** One-hour workload certs keep the blast radius of a stolen key tiny. But a strict one-hour expiry means a council outage at the wrong moment kills every workload. The four-hour grace period is the release valve: security from the short lifetime, availability from the grace window. You usually can't have one property for free; here we paid for it with a few lines of rotation-state logic.

**Keyless signing fell out of a format coincidence.** The workload identity keypair `rcgen` generates is PKCS#8 DER, which is exactly what `ring::signature::EcdsaKeyPair::from_pkcs8` wants. No conversion, no glue. We didn't design that — we noticed it, wrote a test to prove it (`sign_with_workload_identity_keypair`), and got keyless image signing almost for free.

**The registry accepts everything; the scheduler enforces trust.** Putting signature enforcement at *schedule* time rather than *push* time means a missing signature never breaks your CI pipeline. Unsigned images land in Pickle and simply sit there, visible but unschedulable. Pushing the policy check to the latest possible moment kept two concerns from tangling.

**Sign what the server decided, not what the requester asked for.** Our CSR signer checked that the expected SAN was present, then signed the requester's SAN list wholesale — smuggled names included. Any signing service faces this shape of bug: validation that inspects the request and then signs the request anyway. The safe pattern takes only the public key from the request and rebuilds everything else from server-side expectations.

**Credential lifecycles must be derivable from disk.** Rotation schedules that lived only in agent memory reset (or vanished) on every restart; identity directories keyed by app instead of instance meant replicas overwrote each other's keys. Both fixes are the same idea — the instance is the unit of identity, and its on-disk directory is the source of truth for what it holds and when it rotates.

## Tests

Like Chapter 4, this is crypto, so it's almost all pure functions and almost all unit tests — nothing here needs root, a network, or a second node. But Phase 10 is also where the *lifecycle* tests live, the ones that drive a feature end to end through the `sesame` library.

### Unit tests

Image signing (`src/pickle/signing.rs`) is the clearest example of testing a security property through its negatives. Alongside `sign_and_verify_round_trip`, the suite asserts the *failures*: `verify_keyless_wrong_digest_fails`, `verify_keyless_wrong_ca_fails`, `verify_external_key_untrusted_fails`, `verify_external_key_wrong_digest_fails`. A signature scheme that only proves "a valid signature verifies" proves nothing; the value is in "a tampered digest, a foreign CA, or an untrusted key all fail". There's also `sign_with_workload_identity_keypair`, the test that nails down the PKCS#8 coincidence above. The identity and OIDC modules (`sesame/identity.rs`, `sesame/oidc.rs`) carry their own unit tests for CSR creation, SAN validation, JWT minting and verification, and the rotation state machine — including the hostile ones: `smuggled_csr_sans_are_not_signed` and `one_hour_certificate_window_is_exact_not_calendar_dates` (with `check_validity_accepts_fresh_and_rejects_expired_workload_cert` pinning the window through an injected clock).

The per-instance lifecycle has its own regression suite spread across the layers it touches: `identity_mount_source_is_per_instance_not_per_app` (OCI spec), `two_replicas_of_one_app_get_distinct_identity_dirs_and_keys` (the overwrite bug), `deploy_prepares_and_stop_removes_per_instance_identity_dirs` and `rolling_redeploy_leaves_only_live_instances_identity_dirs` (agent lifecycle), and `adoption_restores_identity_and_rotation_schedule_from_disk` plus `adoption_sweeps_orphaned_identity_dirs` (restart safety). The tmpfs backing itself only shows up under Linux as root, so `identity_dir_is_tmpfs_backed_under_root` self-skips elsewhere and runs in the Lima rig. On the rotation side, the state machine's verify-before-retire behaviour is pinned by `finalize_refused_while_a_secret_is_sealed_under_an_old_generation`, `finalize_succeeds_after_secrets_re_encrypted_under_the_new_generation`, `concurrent_second_rotation_refused_until_finalised`, `same_generation_rotation_retry_is_idempotent`, and the compatibility fixture `legacy_secret_without_generation_metadata_blocks_finalize`; the API tests assert the refusals surface as `409`s.

### Integration tests — the lifecycles

Two integration files drive whole features through the library, no running agent required:

- **`tests/security_integration.rs`** is the lifecycle suite: `join_token_single_use_enforced` and `join_token_expiry_enforced` (the loop we left open in Chapter 4), `secret_rotation_dual_key_window` and `secret_rotation_finalize_drops_old_key` (the dual-key window), and `crl_revoked_cert_rejected` / `crl_valid_cert_allowed` / `crl_empty_allows_all` (revocation).
- **`tests/identity_demo.rs`** doubles as a runnable walkthrough. Each test is a stage of the SPIFFE story — `demo_cluster_init_and_ca_hierarchy`, `demo_csr_generation_and_signing`, `demo_oidc_jwt`, `demo_identity_bundle_and_tmpfs`, `demo_rotation_state_machine` — and they print as they go.

### Running them

No gating — crypto needs only a CPU:

```sh
cargo test --lib sesame pickle::signing      # unit tests
cargo test --test security_integration        # token / rotation / CRL lifecycle
cargo test --test identity_demo -- --nocapture # the SPIFFE walkthrough, printed
```

The `--nocapture` on the last one is the point: it turns the test into a guided tour of cluster init, CSR signing, JWT minting, and rotation, printed to your terminal.

## What we deferred

**TPM sealing** binds the master secret to specific hardware via the TPM chip's Platform Configuration Registers. If someone steals a disk, the master key is useless on different hardware. This is important for production hardening, but requires a TPM 2.0 device and the `tss-esapi` crate (Linux only). We've deferred it to v2.

## What we built

Phase 10 adds a complete security layer on top of the Phase 4 PKI foundation:

- Every workload instance gets a SPIFFE X.509 certificate and OIDC JWT automatically, with exact validity windows and server-rebuilt SANs
- Identity lives in a per-instance directory (tmpfs-backed on Linux root), created before start, removed with the instance, and restored — schedule and all — across agent restarts
- Images are signed (keyless or cosign-compatible) and verified by the scheduler
- SecurityState (CAs, tokens, keypairs, CRL, secret seals) is replicated through Raft
- The agent provisions identity during deploy and rotates certificates every 30 minutes
- API tokens are managed via `relish token list/revoke`
- Secret keys rotate with a dual-key transition window, one rotation at a time, and finalise verifies every stored secret is re-sealed before the old key is retired
- The CRL tracks revoked certificates
- Egress DNS re-resolves asynchronously

The next chapter tackles advanced observability.
