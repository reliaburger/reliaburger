# Sesame -- Security, PKI & Identity

## 1. Overview

Sesame is Reliaburger's built-in PKI, identity, and security layer. It provides the cryptographic foundation that every other subsystem depends on: mutual TLS between cluster nodes, workload identity certificates for every app and job, API authentication for human operators and CI systems, secret encryption at rest, and network-level firewalling. Raft log encryption is designed and implemented as a module but is **not yet wired into the durable log** — see the status note in §5.6.

Sesame is not a separate binary or sidecar. It is compiled into the single `reliaburger` binary and activated during `relish init`. Every security primitive -- certificate authorities, certificate signing, token management, secret encryption, firewall rule generation -- is handled by code paths within the existing node roles (council and worker).

**Core responsibilities:**

- **CA hierarchy.** A root CA and three intermediate CAs (Node, Workload, Ingress), each scoped to a single purpose.
- **Node authentication.** Join tokens and mTLS certificate issuance (shipped). TPM attestation is **planned — not yet implemented**: the `AttestationMode::Tpm` variant exists, but there is no TPM code or TPM crate behind it.
- **Workload identity.** SPIFFE-compatible X.509 certificates and OIDC JWTs for every workload, issued via a CSR model, rotated automatically.
- **API authentication.** Scoped tokens with roles, TTLs, and rate limits. Optional OIDC integration with external identity providers.
- **Secret encryption.** Asymmetric age encryption for secrets checked into git. Namespace-scoped keypairs for multi-tenant isolation.
- **Data at rest encryption (planned — implemented but not yet wired).** AES-256-GCM encryption of the Raft log, with HKDF key derivation and optional TPM sealing. The crypto lives in `sesame::raft_encryption` (complete and tested) but has no call sites yet; the durable log currently writes plaintext. TPM sealing is not implemented at all.
- **Network security.** nftables perimeter firewall, eBPF inter-app firewall, egress allowlists, namespace isolation -- all managed automatically by Bun.
- **Certificate revocation.** CRL distribution for long-lived node certificates.

**Design principles:**

1. **Zero-configuration security (target).** A fresh cluster should have mTLS between all nodes, workload identity for all apps, namespace isolation, deny-by-default egress, and encrypted Raft logs. The current egress implementation is narrower: a declared `[app.NAME.egress]` allowlist is deny-by-default and fail-closed, but an app with no block still has unrestricted egress. A cluster-wide default policy remains planned.
2. **Separation of privilege (target — see the note below).** The intended model is that worker nodes never hold CA private keys and can only obtain certificates for workloads they are scheduled to run. **The current implementation does not yet enforce this key split:** every clustered node loads the same cluster master key and its bootstrap security state, from which it can locally unwrap the age private key, the intermediate CA private keys, and the OIDC signing key (see §3.2). A genuine council/worker key separation is planned, not shipped.
3. **Data plane survives control plane failures.** Existing certificates, firewall rules, and secrets continue working during council outages. Grace period extensions prevent hard cliffs.
4. **Short-lived credentials by default.** Workload certificates live 1 hour, rotated every 30 minutes. Deployer and ReadOnly API tokens live 90 days unless created with `--ttl-days` or `--no-expiry`, and `relish token rotate` replaces a secret with a 24-hour overlap (§5.4). Admin tokens are the exception: they get no default expiry, so the cluster can't lock itself out, and `relish wtf` warns about old ones. Short lifetimes reduce the blast radius of credential theft.

---

## 2. Dependencies

| Component | Role in Sesame |
|-----------|---------------|
| **Raft** (council) | Stores all persistent security state: intermediate CA private keys (encrypted), age secret encryption keypairs, API token hashes, CRL entries, OIDC signing keys. All writes to security state go through Raft consensus. |
| **Bun** (worker agent) | Generates workload keypairs, sends CSRs to council, writes signed certificates to workload tmpfs mounts, manages nftables rules, loads eBPF firewall maps, handles secret decryption, rotates node certificates. |
| **Council** (leader/followers) | Holds intermediate CA private keys (in-memory, decrypted from Raft log). Signs CSRs from worker nodes. Validates that CSR subjects match Meat's scheduling state. Distributes CRLs via the reporting tree. |
| **Meat** (scheduler) | Provides the scheduling state that council uses to validate CSRs -- a worker node can only obtain a certificate for a workload that Meat has scheduled onto that node. |
| **Mustard** (gossip) | Propagates the `cluster_nodes` IP set used by nftables perimeter rules. Membership changes trigger Bun to reconcile firewall state. |
| **Onion** (eBPF service discovery) | Hosts the `connect()` interception point where eBPF firewall checks are enforced. The `firewall_map` BPF map is loaded alongside Onion's existing service map. |
| **Wrapper** (ingress) | Reconstructs Ingress CA material on council-enabled ingress nodes and issues per-SNI leaves for `tls = "cluster"` routes (90 days by default). A cached leaf is reissued on the first handshake after its midpoint, measured from the issuing instant. |
| **Lettuce** (GitOps) | Delivers app configurations containing `ENC[AGE:...]` secret values and `firewall`/`egress` blocks to Bun for processing. |

---

## 3. Architecture

### 3.1 CA Hierarchy

```
Root CA (offline after init, signs only intermediate CAs)
|
+-- Node CA         -- signs node certificates for inter-node mTLS
|                      Lifetime: 5 years. Stored encrypted in Raft.
|
+-- Workload CA     -- signs workload identity certificates (SPIFFE)
|                      Lifetime: 5 years. Stored encrypted in Raft.
|
+-- Ingress CA      -- signs certificates for tls = "cluster" ingress routes
                       Lifetime: 5 years. Stored encrypted in Raft.
```

The root CA private key is used **only** during `relish init` and on the operator's own machine, when `relish ca rotate` signs a new intermediate from the operator's backup (and, once it ships, `relish ca rotate --root`; see §5.8). After signing the three intermediates, the root private key is encrypted with the cluster's age public key, written to a sealed backup file on the admin's machine, and deleted from all cluster nodes. No cluster node holds the root CA private key during normal operation.

All three intermediate CAs chain to the same root, so a single trust anchor (the root CA certificate) is sufficient for any verifier.

### 3.2 Key Distribution Model

> **Implementation note (important).** The split drawn below is the *target*
> model. It is **not** how the cluster currently boots. Today every clustered
> node is started with the cluster master key (`load_master_key`) and the
> bootstrap security state, which together let *any* node unwrap the age private
> key, the intermediate CA private keys and the OIDC signing key locally. So the
> "NO CA private keys / NO age private key / NO OIDC signing key" line in the
> Worker Nodes box is aspirational — a real key split is planned work. The
> diagram is retained to show the intended separation, not the shipped one.

```
+-------------------------------------------------------------------+
|                        Council Nodes                               |
|                                                                    |
|  Raft Log (at-rest AES-256-GCM planned; NOT wired — plaintext today) |
|  +--------------------------------------------------------------+ |
|  | Node CA private key (wrapped with HKDF-derived key)          | |
|  | Workload CA private key (wrapped with HKDF-derived key)      | |
|  | Ingress CA private key (wrapped with HKDF-derived key)       | |
|  | Age private key (wrapped with HKDF-derived key)              | |
|  | API token hashes                                             | |
|  | OIDC Ed25519 signing keypair                                 | |
|  | CRL entries                                                  | |
|  +--------------------------------------------------------------+ |
|                                                                    |
|  In-Memory (decrypted on startup)                                  |
|  +--------------------------------------------------------------+ |
|  | Node CA keypair          -- for signing node CSRs             | |
|  | Workload CA keypair      -- for signing workload CSRs         | |
|  | Ingress CA keypair       -- for signing ingress CSRs          | |
|  | Age keypair              -- for decrypting secrets            | |
|  | OIDC signing keypair     -- for minting JWTs                  | |
|  +--------------------------------------------------------------+ |
+-------------------------------------------------------------------+

+-------------------------------------------------------------------+
|                        Worker Nodes                                |
|                                                                    |
|  On Disk (via Bun)                                                 |
|  +--------------------------------------------------------------+ |
|  | Node certificate + private key   (for inter-node mTLS)       | |
|  | Root CA certificate              (trust anchor)               | |
|  | Node CA certificate              (for verifying peer nodes)   | |
|  | Workload CA certificate chain    (for verifying workloads)    | |
|  +--------------------------------------------------------------+ |
|                                                                    |
|  Per-Workload tmpfs (ephemeral, destroyed on stop)                 |
|  +--------------------------------------------------------------+ |
|  | Workload certificate + private key                            | |
|  | CA trust chain (Workload CA + Root CA)                        | |
|  | OIDC JWT token                                                | |
|  +--------------------------------------------------------------+ |
|                                                                    |
|  TARGET: NO CA private keys, age key, or OIDC signing key.        |
|  CURRENT: holds the cluster master key + bootstrap state, so it    |
|  can locally unwrap all of the above (key split not yet enforced). |
+-------------------------------------------------------------------+
```

### 3.3 CSR Flow (Workload Certificate Issuance)

```
Worker Node (Bun)                     Council Node (nearest parent)
     |                                          |
     |  1. Generate keypair locally              |
     |     (per workload instance)               |
     |                                          |
     |  2. Create CSR:                           |
     |     - Subject: SPIFFE URI                 |
     |     - SAN: spiffe://cluster/ns/NS/app/APP|
     |     - Public key from step 1              |
     |                                          |
     |  3. Send CSR over inter-node mTLS ------->|
     |     (authenticated by node certificate)   |
     |                                          |
     |                              4. Validate: |
     |                   - Node cert is valid    |
     |                   - CSR subject matches   |
     |                     Meat's scheduling    |
     |                     state for this node   |
     |                   - Workload IS scheduled |
     |                     on requesting node    |
     |                                          |
     |                              5. Sign cert |
     |                     with Workload CA key  |
     |                     Lifetime: 1 hour      |
     |                                          |
     |  6. Receive signed cert <----------------|
     |                                          |
     |  7. Write to workload tmpfs:              |
     |     /var/run/reliaburger/identity/        |
     |       cert.pem                            |
     |       key.pem                             |
     |       ca.pem                              |
     |       token (OIDC JWT)                    |
     |       bundle.pem                          |
     |                                          |
     |  8. Schedule next rotation                |
     |     (30 min = half of cert lifetime)      |
     |                                          |
```

### 3.4 Node Join Flow

```
Admin                  New Node                  Cluster (any existing node)
  |                       |                              |
  | relish init           |                              |
  | (first node only)     |                              |
  |  -> Generate Root CA  |                              |
  |  -> Generate Node CA, Workload CA, Ingress CA        |
  |  -> Generate age keypair                             |
  |  -> Generate OIDC Ed25519 keypair                    |
  |  -> Generate node cert for self                      |
  |  -> Output join token to stderr                      |
  |                       |                              |
  | relish join --token T |                              |
  | (subsequent nodes)    |                              |
  |                       |  1. Present join token ------>|
  |                       |     (over TLS, server-auth)   |
  |                       |                              |
  |                       |           2. Validate token: |
  |                       |              - Not expired   |
  |                       |              - Not yet used  |
  |                       |              - (optional)    |
  |                       |                TPM attest.   |
  |                       |                              |
  |                       |  3. Receive node cert <------|
  |                       |     signed by Node CA        |
  |                       |                              |
  |                       |  4. All future communication |
  |                       |     uses mTLS with node cert |
  |                       |                              |
```

---

## 4. Data Structures

All structs below are Rust sketch-level definitions. Actual implementations will include `serde` derives, validation logic, and builder patterns where appropriate.

### 4.1 Certificate Authority

```rust
/// Represents one CA in the hierarchy (root, node, workload, or ingress).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CertificateAuthority {
    /// Which CA this is.
    pub role: CaRole,

    /// DER-encoded X.509 certificate.
    pub certificate_der: Vec<u8>,

    /// DER-encoded private key, encrypted with HKDF-derived wrapping key.
    /// None for the root CA on cluster nodes (root key is deleted after init).
    pub private_key_wrapped: Option<WrappedKey>,

    /// Serial number of this CA certificate.
    pub serial: SerialNumber,

    /// When this CA certificate expires.
    pub not_after: SystemTime,

    /// When this CA certificate was issued.
    pub not_before: SystemTime,

    /// The parent CA's serial (None for the root CA).
    pub issuer_serial: Option<SerialNumber>,

    /// Generation counter: 0 at init, one more per rotation of the role
    /// (`RaftRequest::CaRotationBegin`, §5.8).
    pub generation: u64,

    /// `Active` signs and is trusted; `Retiring { until }` is only trusted,
    /// until the rotation is finalised.
    pub state: CaState,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum CaRole {
    Root,
    Node,
    Workload,
    Ingress,
}

/// A private key encrypted with an HKDF-derived wrapping key.
/// The wrapping key is derived from the node's certificate private key,
/// optionally sealed to TPM PCRs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WrappedKey {
    /// AES-256-GCM ciphertext of the private key DER.
    pub ciphertext: Vec<u8>,

    /// 96-bit nonce for AES-256-GCM.
    pub nonce: [u8; 12],

    /// HKDF salt (random, stored alongside ciphertext).
    pub hkdf_salt: [u8; 32],

    /// HKDF info string identifying the purpose of this key.
    pub hkdf_info: String,
}
```

### 4.2 Node Certificate

```rust
/// A certificate issued to a cluster node for inter-node mTLS.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeCertificate {
    /// The node's unique identifier (used as CN in the certificate).
    pub node_id: NodeId,

    /// DER-encoded X.509 certificate, signed by the Node CA.
    pub certificate_der: Vec<u8>,

    /// DER-encoded private key (stored on the node, not in Raft).
    /// Not serialised into Raft state -- kept only on the local node.
    #[serde(skip)]
    pub private_key_der: Vec<u8>,

    /// Serial number assigned by the Node CA.
    pub serial: SerialNumber,

    /// Certificate validity period.
    pub not_before: SystemTime,
    pub not_after: SystemTime,  // default: 1 year from issuance (see §6.1.1)

    /// The Node CA generation that signed this certificate.
    pub ca_generation: u64,
}
```

### 4.3 Workload Identity

```rust
/// The full identity bundle for a running workload instance.
#[derive(Debug)]
pub struct WorkloadIdentity {
    /// The SPIFFE URI for this workload.
    pub spiffe_uri: SpiffeUri,

    /// DER-encoded X.509 certificate, signed by the Workload CA.
    pub certificate_der: Vec<u8>,

    /// DER-encoded private key (generated per-instance, never leaves tmpfs).
    pub private_key_der: Vec<u8>,

    /// PEM-encoded CA trust chain (Workload CA cert + Root CA cert).
    pub ca_chain_pem: String,

    /// OIDC JWT token, signed by the cluster's Ed25519 OIDC signing key.
    pub jwt_token: String,

    /// When this identity was issued.
    pub issued_at: SystemTime,

    /// When the certificate expires (default: 1 hour from issuance).
    pub expires_at: SystemTime,

    /// When the next rotation should occur (default: 30 min from issuance).
    pub next_rotation: SystemTime,

    /// Whether this certificate is operating under a grace period extension.
    pub grace_extended: bool,
}

/// A SPIFFE URI identifying a workload.
/// Format: spiffe://CLUSTER/ns/NAMESPACE/app/APP_NAME
///     or: spiffe://CLUSTER/ns/NAMESPACE/job/JOB_NAME
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SpiffeUri {
    /// The trust domain (cluster name, e.g., "prod").
    pub trust_domain: String,

    /// The namespace containing the workload.
    pub namespace: String,

    /// The workload type (app or job).
    pub workload_type: WorkloadType,

    /// The workload name.
    pub name: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum WorkloadType {
    App,
    Job,
}

impl SpiffeUri {
    /// Renders the full URI string.
    /// Example: "spiffe://prod/ns/default/app/api"
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

The implementation reads the trust domain from `[cluster].name`. `relish init`,
`relish setup` and `relish dev create` persist the requested cluster name into
every generated node config; Bun validates the DNS-style value once at startup
and passes it unchanged into app, job, OIDC and build-signing issuance. The
historical hard-coded `default` domain remains only as the backwards-compatible
config default.

### 4.4 API Token

```rust
/// An API token for human or CI access to the cluster.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiToken {
    /// Human-readable name for the token (e.g., "ci-deploy").
    pub name: String,

    /// Argon2id hash of the token secret.
    pub token_hash: Vec<u8>,

    /// Salt used for hashing.
    pub token_salt: Vec<u8>,

    /// The role granted to this token.
    pub role: ApiRole,

    /// Optional scope restrictions.
    pub scope: TokenScope,

    /// When the token expires; `None` means never. A Deployer or ReadOnly
    /// token created without `--ttl-days` or `--no-expiry` gets the node's
    /// `[security.tokens] default_ttl` (90 days); Admin tokens get none (§5.4).
    pub expires_at: Option<SystemTime>,

    /// When the token's current secret was issued (creation, then each
    /// rotation).
    pub created_at: SystemTime,

    /// Last time the token was used. Shipped, but *not* stored here: each
    /// node keeps it in memory (`sesame::auth::TokenLastUsed`, keyed by the
    /// token's principal id) and `GET /v1/token/list` merges every node's
    /// answer, the latest per token (§5.4).
    pub last_used: Option<SystemTime>,

    /// Per-token rate limit (requests per second). Default: 100.
    pub rate_limit_rps: u32,

    /// After `relish token rotate`, the old secret while it is still
    /// accepted (shipped as `previous_secret: Option<PreviousSecret>`, §5.4).
    pub rotation_grace: Option<RotationGrace>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum ApiRole {
    Admin,
    Deployer,
    ReadOnly,
}

/// Scope restrictions on an API token.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TokenScope {
    /// If set, token can only act on these app names.
    pub apps: Option<Vec<String>>,

    /// If set, token can only act within these namespaces.
    pub namespaces: Option<Vec<String>>,

    /// If set, token can only perform these actions.
    pub actions: Option<Vec<String>>,
}

/// Grace period state during token rotation (shipped as
/// `sesame::types::PreviousSecret { token_hash, token_salt, valid_until }`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RotationGrace {
    /// Hash of the old token that is still accepted.
    pub old_token_hash: Vec<u8>,
    pub old_token_salt: Vec<u8>,

    /// When the old token stops being accepted. Default: 24h after rotation.
    pub grace_expires_at: SystemTime,
}
```

### 4.5 Age Keypair (Secret Encryption)

```rust
/// An age keypair used for encrypting/decrypting secrets.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgeKeypair {
    /// The scope of this keypair.
    pub scope: AgeKeyScope,

    /// The age public key (safe to distribute).
    /// e.g., "age1qy8m5kz..."
    pub public_key: String,

    /// The age private key, wrapped with HKDF-derived key.
    pub private_key_wrapped: WrappedKey,

    /// Generation counter, incremented on `relish secret rotate`.
    pub generation: u64,

    /// Whether this key is read-only (old generation, kept for decryption
    /// of not-yet-re-encrypted secrets during rotation).
    pub read_only: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum AgeKeyScope {
    /// Cluster-wide default keypair.
    ClusterWide,

    /// Namespace-scoped keypair (planned — no code creates one yet; see §5.5).
    Namespace(String),
}
```

### 4.6 Certificate Revocation List

```rust
/// The cluster's certificate revocation list, distributed via the reporting tree.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Crl {
    /// The list of revoked certificate serial numbers.
    pub entries: Vec<CrlEntry>,

    /// Monotonically increasing version, incremented on every CRL update.
    pub version: u64,

    /// When this CRL was last updated.
    pub updated_at: SystemTime,

    /// Signature over the CRL by the issuing CA (for integrity verification).
    pub signature: Vec<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CrlEntry {
    /// Serial number of the revoked certificate.
    pub serial: SerialNumber,

    /// Which CA issued the revoked certificate.
    pub issuer: CaRole,

    /// When the certificate was revoked.
    pub revoked_at: SystemTime,

    /// Human-readable reason (e.g., "node-07 compromised").
    pub reason: String,
}
```

### 4.7 Firewall Rules

```rust
/// A per-app eBPF firewall rule controlling inbound connections.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FirewallRule {
    /// The destination app that this rule protects.
    pub target_app: String,

    /// The target app's namespace.
    pub target_namespace: String,

    /// List of source apps allowed to connect.
    /// If empty, all apps in the same namespace are allowed (default).
    pub allow_from: Vec<AppRef>,
}

/// A reference to an app (possibly in another namespace).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppRef {
    /// App name, e.g., "api".
    pub name: String,

    /// Namespace. Defaults to the target app's namespace.
    pub namespace: Option<String>,
}

/// An egress allowlist rule controlling outbound connections from an app.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EgressRule {
    /// The app this rule applies to.
    pub app_name: String,

    /// The app's namespace.
    pub namespace: String,

    /// Allowed outbound destinations.
    pub allow: Vec<EgressDestination>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum EgressDestination {
    /// A hostname:port pattern (may include wildcards).
    /// e.g., "*.amazonaws.com:443", "api.stripe.com:443"
    HostPort {
        pattern: String,
        port: u16,
    },

    /// A CIDR:port range.
    /// e.g., "10.0.0.0/8:5432"
    CidrPort {
        cidr: IpNet,
        port: u16,
    },
}

/// Resolved egress destination for nftables set insertion.
#[derive(Debug, Clone)]
pub struct ResolvedEgressEntry {
    pub ip: IpAddr,
    pub port: u16,

    /// TTL from DNS resolution; entry is refreshed before expiry.
    pub dns_ttl: Duration,

    /// The original hostname this was resolved from (for audit logging).
    pub source_hostname: Option<String>,
}

/// The nftables state managed by Bun on each node.
#[derive(Debug, Clone)]
pub struct NftablesState {
    /// IP addresses of all cluster nodes (from Mustard gossip).
    pub cluster_nodes: HashSet<IpAddr>,

    /// Operator CIDR ranges allowed to reach the API port (only that port).
    pub operator_cidrs: Vec<IpNet>,

    /// Per-app egress sets (app name -> resolved destinations).
    pub egress_sets: HashMap<String, Vec<ResolvedEgressEntry>>,

    /// Version counter for reconciliation (incremented on every change).
    pub version: u64,
}

/// Entry in the eBPF firewall_map, keyed by destination cgroup ID.
#[derive(Debug, Clone)]
pub struct BpfFirewallEntry {
    /// Cgroup ID of the destination app.
    pub dest_cgroup_id: u64,

    /// Set of source cgroup IDs allowed to connect.
    pub allowed_sources: HashSet<u64>,
}
```

### 4.8 OIDC Structures

```rust
/// The OIDC signing configuration for workload identity JWTs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OidcSigningConfig {
    /// Ed25519 private key for signing JWTs, wrapped.
    pub signing_key_wrapped: WrappedKey,

    /// Ed25519 public key (published via JWKS endpoint).
    pub public_key_der: Vec<u8>,

    /// Key ID for the JWKS entry.
    pub key_id: String,

    /// The issuer URL (e.g., "https://reliaburger.prod.example.com").
    pub issuer: String,
}

/// Claims embedded in a workload identity JWT.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorkloadJwtClaims {
    /// Issuer: the cluster's OIDC endpoint URL.
    pub iss: String,

    /// Subject: the workload's SPIFFE URI.
    pub sub: String,

    /// Audience. Today this is exactly `["spiffe://CLUSTER"]`; per-app
    /// audiences are planned but not yet appended (§6.6).
    pub aud: Vec<String>,

    /// Expiration time (Unix timestamp).
    pub exp: u64,

    /// Issued-at time (Unix timestamp).
    pub iat: u64,

    /// Custom claims.
    #[serde(rename = "reliaburger.dev/namespace")]
    pub namespace: String,

    #[serde(rename = "reliaburger.dev/app")]
    pub app: String,

    #[serde(rename = "reliaburger.dev/cluster")]
    pub cluster: String,

    /// The issuing node's id. NOTE: currently populated with the literal
    /// string "local", not the real node id (planned fix).
    #[serde(rename = "reliaburger.dev/node")]
    pub node: String,

    #[serde(rename = "reliaburger.dev/instance")]
    pub instance: String,
}
```

### 4.9 Join Token

```rust
/// A one-time-use join token for adding a node to the cluster.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JoinToken {
    /// Cryptographically random token value (never stored in plaintext;
    /// only the hash is persisted in Raft).
    pub token_hash: [u8; 32],

    /// When the token expires. Default: 15 minutes from creation.
    pub expires_at: SystemTime,

    /// Whether the token has been consumed.
    pub consumed: bool,

    /// Node attestation mode required for this token.
    pub attestation_mode: AttestationMode,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum AttestationMode {
    /// No additional attestation beyond the join token.
    None,

    /// Require TPM 2.0 attestation quote during join.
    Tpm,

    /// Require a pre-issued client certificate from an external CA.
    Certificate,
}
```

### 4.10 Raft Log Encryption

```rust
/// Encryption state for the Raft log on a council node.
#[derive(Debug)]
pub struct RaftLogEncryption {
    /// AES-256-GCM key derived via HKDF from the node's identity.
    /// This is derived at startup and held in memory only.
    pub log_encryption_key: [u8; 32],

    /// HKDF salt (stored alongside the encrypted Raft log on disk).
    pub hkdf_salt: [u8; 32],

    /// Whether the key derivation is sealed to TPM PCRs.
    pub tpm_sealed: bool,

    /// The TPM PCR values the key is bound to (if tpm_sealed is true).
    pub pcr_values: Option<Vec<PcrValue>>,
}

#[derive(Debug, Clone)]
pub struct PcrValue {
    pub index: u32,
    pub digest: Vec<u8>,
}
```

---

## 5. Operations

### 5.1 Cluster Initialisation (`relish init`)

This is the single most security-critical operation. It generates all root key material.

**Sequence:**

1. Generate a 4096-bit RSA root CA keypair (or Ed25519 if `ca_algorithm = "ed25519"` is configured).
2. Self-sign the root CA certificate with a 10-year validity period.
3. Generate three intermediate CA keypairs (Node CA, Workload CA, Ingress CA).
4. Sign all three intermediate CA certificates with the root CA, each with a 5-year validity period.
5. Generate an age keypair for secret encryption.
6. Generate an Ed25519 keypair for OIDC JWT signing.
7. Generate a node certificate for the first node, signed by the Node CA (1-year validity).
8. Derive the HKDF wrapping key from the first node's certificate private key.
9. Wrap all sensitive keys (intermediate CA private keys, age private key, OIDC signing key) with the wrapping key.
10. Write the wrapped keys to the initial Raft log entry.
11. Encrypt the root CA private key with the cluster's age public key.
12. Write the sealed root CA backup to the admin's filesystem.
13. Delete the root CA private key from memory.
14. Write the first node config with `require_mtls = true`; when Bun starts
    with that file, it refuses to enter cluster mode without the identity and
    brings up the authenticated transports.

The master key and security-bootstrap files are both written with mode `0600`.
Bun enforces that at load time, so a generated bootstrap never relies on a
world-readable interval or asks the operator to repair permissions manually.

This is the normal path. Explicit development generators are the exception:
`relish init --development-plaintext` and `relish dev create` write
`require_mtls = false`, embed a development-only warning in the file and print
the same warning; Bun repeats it at every cluster start. The Lima dev cluster
keeps this temporary limitation because its provisioning path doesn't enrol a
separate node identity in each VM yet. Don't reuse those configs elsewhere.

The transport contract has one deliberate split. Raft and reporting require a
valid, unrevoked node certificate at the TLS handshake. The agent API also
presents a node certificate, and node-to-node clients present theirs, so peer
calls are mutually authenticated. Relish and browsers don't hold node keys,
so the API listener permits an absent client certificate and authorises those
requests with bearer tokens or session cookies. Gossip is UDP and uses the
cluster-master-key-derived HMAC instead of TLS.

**Output to admin:**

```
Cluster initialised.

  Cluster name:    prod
  Root CA:         serial 0x01, expires 2036-02-16
  Node CA:         serial 0x02, expires 2031-02-16
  Workload CA:     serial 0x03, expires 2031-02-16
  Ingress CA:      serial 0x04, expires 2031-02-16

  IMPORTANT: Back up the sealed root CA key:
    ./prod-root-ca.age

  Losing this file means a full PKI re-bootstrap.

  To enrol another node, start this one, then mint a token bound
  to that node's id:
    relish join-token create --node-id <node-id>
```

`init` mints no join token: a token is bound to the one node id it may enrol
(§5.2), and bootstrap cannot know the id of a node you haven't added yet. The
operator mints one after boot with `relish join-token create --node-id`.

### 5.2 Node Join (`relish join`)

After bootstrap, an administrator creates one credential per joining node:

```bash
$ relish --endpoint https://leader.example:9117 \
    --ca-cert root-ca.crt --token "$ADMIN_TOKEN" \
    join-token create --node-id node-02 --ttl 15m
```

`POST /v1/join-token/create` accepts `ttl_seconds` in the bounded range
`1..=3600`, requires the Admin role, generates the secret on the leader and
commits only its SHA-256 hash, expiry and attestation mode through
`RaftRequest::CreateJoinToken`. The response carries the plaintext once. A
follower returns `503` without the plaintext; retrying against the elected
leader cannot create an orphan credential.

1. The joining node connects to the current leader over TLS (server-authenticated only at this stage, using the root CA certificate that was provided alongside the join token or downloaded via a trust-on-first-use pinning step).
2. The joining node presents the join token.
3. The leader validates that the token exists, is not expired and has not already been consumed.
4. One Raft entry atomically marks the token consumed and allocates the certificate serial. A retry or concurrent reuse is refused.
5. If `node_attestation = "tpm"`: the joining node presents a TPM attestation quote, which the leader verifies against the pre-registered endorsement keys. **(Planned — not implemented. The `Tpm` attestation mode is a config/enum placeholder with no TPM code behind it; do not rely on it.)**
6. If `node_attestation = "certificate"`: the joining node presents a client certificate. The leader verifies it against the configured external CA trust store.
7. The leader signs the joining node's CSR with the Node CA. The node's private key never crosses the network.
8. The signed certificate, the Node CA certificate and the root CA certificate are sent to the joining node.
9. The joining node transactionally stores the identity. Provisioning still has to supply its config and cluster master key, and start Bun before it participates in gossip.

### 5.3 Workload Certificate Rotation

Bun on each worker node maintains a rotation schedule for every running workload:

1. **Initial issuance.** When Bun starts a workload, it immediately generates a keypair and sends a CSR to its nearest council parent.
2. **Validation.** The council member checks that the requesting node (identified by its mTLS node certificate CN) is scheduled to run the workload (verified against Meat's scheduling state).
3. **Signing.** The council member signs the certificate with the Workload CA, sets a 1-hour lifetime, and returns it.
4. **Writing.** Bun writes `cert.pem`, `key.pem`, `ca.pem`, `bundle.pem`, and `token` (OIDC JWT) to the workload's tmpfs mount at `/var/run/reliaburger/identity/`.
5. **Rotation timer.** Bun schedules the next rotation for 30 minutes later (half the certificate lifetime).
6. **Pre-fetch.** At the 30-minute mark, Bun generates a new keypair, sends a new CSR, receives the new certificate, and atomically writes the new files to the tmpfs mount.
7. **Grace period.** If the CSR fails because the council is unreachable, Bun keeps the current certificate and extends its local validity window by up to 4 additional hours (configurable). The extension is logged as a security event, `relish wtf` warns about it, and an alert fires.

**Host-side layout.** The mount source is a per-*instance* directory, `{volumes}/.identity/{instance_id}` — never shared between replicas of the same app, so no replica can overwrite another's private key. Bun prepares the directory before the container is created (it is the bind-mount source); on Linux, running as root, it is backed by a dedicated size-bounded tmpfs (`mode=0700`) so key material never touches persistent storage, and the key/token files are chowned to the container's runtime UID. On other platforms it is a plain `0700` directory (documented gap). The directory is removed — and the tmpfs unmounted — when the instance stops, is replaced by a rolling deploy, or is rolled back.

**Restart safety.** Alongside the PEM files, Bun writes a `meta.json` sidecar (SPIFFE URI, issuance/expiry/rotation timestamps — no secrets). After a Bun restart or self-upgrade, workload adoption rebuilds each instance's `WorkloadIdentity` and its rotation schedule from this directory, so rotation continues on the original timetable instead of restarting from `identity: None`. Identity directories with no live owner are swept at adoption.

**Certificate contents.** The council rebuilds every certificate field server-side from the expected identity; the only input taken from the CSR is the public key. Extra SANs in a hostile CSR are never signed. Validity is an exact timestamped window (`now − 5 min` skew backdate to `now + 1 h`), not calendar dates.

**OIDC JWT minting** happens at the same time as certificate issuance. The council member constructs the JWT with the workload's SPIFFE URI as the `sub` claim, the cluster's OIDC issuer as `iss`, and a single audience `spiffe://CLUSTER`. The JWT is signed with the Ed25519 OIDC signing key and returned alongside the signed X.509 certificate.

> **Two claim details differ from the aspirational model.** (1) The `aud` claim
> is exactly `["spiffe://<cluster>"]` today — **per-app audiences from
> `[app.NAME.identity]` are not appended** (that is planned, §6.6). (2) The
> `reliaburger.dev/node` claim is currently the **literal string `"local"`**,
> not the issuing node's id — the CSR-signing path passes `"local"` as the node
> identifier. Treat the per-app-audience and real-node-id stories as planned.

### 5.4 API Token Lifecycle

**Creation:**

```bash
$ relish token create --name ci-deploy --role deployer \
    --apps "web,api" --namespaces "production" --ttl-days 30
```

1. Generate a 256-bit cryptographically random token secret.
2. Hash the secret with Argon2id (salt generated per-token).
3. Store the hash, salt, role, scope, and expiry in Raft.
4. Return the plaintext token to the user, with its expiry. It is never stored
   in plaintext.

**Default lifetime (shipped, F05 I3).** Without `--ttl-days`, a Deployer or
ReadOnly token expires after the answering node's `[security.tokens]
default_ttl` (`"90d"` by default; `"12h"`-style hours and `"none"` are
accepted, and a bad value fails config load). `--no-expiry` opts out
explicitly and conflicts with `--ttl-days`. Admin tokens get no default
expiry (maintainer decision 6): if every Admin expired, nobody could create
the next one, and a non-empty store keeps the bootstrap window shut.
`relish wtf` warns about an Admin token whose secret is older than 90 days,
and `relish wtf` and `relish token list` warn about any token that has
expired or expires within 14 days (`sesame::token::TOKEN_EXPIRY_WARNING`).

**Names with a `[permission]` spec (shipped, decision 3).** Specs are keyed by
token name, so a token created under a name that already has one would
inherit it silently, for example after a revoke and re-create. `POST
/v1/token/create` answers `409` in that case unless the request sets
`inherit_permissions` (`relish token create --inherit-permissions`).

**Scope enforcement (shipped):** `--apps` and `--namespaces` restrict a token
on every route that names an app and namespace (`authorize_scoped`), filter
cluster-wide listings such as `/v1/status` and `/v1/images` to the scope, and
refuse routes that need cluster-wide authority (`require_unscoped`). The Pickle
registry applies the same scope to repositories: a scoped token may push and
(on a routable listener) pull only repositories named `<namespace>/<app>` whose
namespace and app it covers, over Bearer and Basic alike; bare names such as
`api` are refused to it, and `/v1/build` destinations meet the same rule. The
internal service token and unscoped tokens are unaffected. See
`registry-pickle.md` §1.2 for the exact rule. The discovery and chaos reads
filter to the scope as well: `/v1/resolve` and `/v1/resolve/{name}` list only
in-scope services, `/v1/routes` only in-scope ingress routes (`RouteInfo`
carries the route's namespace), `GET /v1/fault` only workload faults on
in-scope apps (`FaultSummary` carries the fault's namespace), and
`GET /v1/build/{id}` answers only for a build whose destination repository
(`BuildRecord::repository`) is in scope. `GET /v1/fault?cluster=true` refuses a
scoped caller, since node faults belong to no tenant. A workload's JWT is a
scoped read-only token, so these are the rules a compromised container meets.

**Permission specs (shipped):** A `[permission.<token-name>]` block
(`config::PermissionSpec`, replicated as `DesiredState.permissions`) is an
additional allow-list on top of role and scope. It can narrow a token but never
widen it. A token with no spec is governed by role and scope alone, and the
internal system principal (node-to-node fan-out) is never gated. A browser
session carries its token's name, so it rides the same spec. The route matrix in
`src/bun/authz.rs` records, per route, the action a spec must grant
(`Route::permission`):

| Gate | Check | Routes |
|------|-------|--------|
| `App(action)` | `authorize_permission` on the path's app and namespace | logs (SSE, entries, cross-node query, WebSocket) → `logs`; app metrics and charts → `metrics`; delete, rollback, snapshot create, restore and delete → `deploy`; stop → `scale`; exec → `exec` |
| `Body(action)` | the same, per app the body names | apply (`deploy`, plus `host-exec` for host commands), deploy cancel (`deploy`); `/v1/build` → `deploy` on the destination repository's app, or cluster-wide for a destination with no namespace; fault inject and clear → `fault` on the target service, cluster-wide for a node fault or a clear that spans tenants, judged against the stored fault for a clear by id |
| `Cluster(action)` | `authorize_cluster_permission`: the action with `apps = ["*"]` and no `namespaces` | `/v1/logs/sql` → `logs`; `/v1/metrics*` store, rollups and cluster queries, `/v1/alerts`, the alerts fragment → `metrics`; secret rotation → `secret-write`; tokens, join tokens, upgrades, elections, decommission, image signing, log export, `[permission]`/`[namespace]` declarations and test leases (create, renew, release) → `admin` |
| `Filtered(action)` | the route answers without the parts the spec doesn't grant | `/v1/top` rows → `metrics` per app; dashboard alert panel → cluster `metrics`; app page charts → `metrics` on that app |

`admin` is a super-grant covering every other action. `secret-read` gates no
route, because no API route returns decrypted secret material: the agent
decrypts `ENC[...]` values straight into the instance environment. Two tests
keep the matrix honest: `every_gated_route_checks_its_permission_action`
statically finds each gated handler's check, and the table test in
`src/bun/api/permission_tests.rs` drives a real request per gated route and per
principal (every role, grant shape, session, scope, system and bootstrap),
asserting 403 exactly where the spec doesn't grant the action. Snapshots,
builds, faults and test leases joined the gated set in 0.2.0 (decision D5 in
[#674](https://github.com/reliaburger/reliaburger/issues/674)), with the new
`fault` action.

**The principal column (0.2.0):** each matrix row also names the least
principal it admits (`Public`, `AnyToken`, `Deployer`, `Admin`, `System`).
`every_route_refuses_callers_below_its_principal` drives every row as seven
callers (a read-only token, a user session, a service-token session, a scoped
Deployer, a Deployer, an Admin and the system principal) and requires a 401 or
403 from every caller ranked below the row, plus entry for a read-only token
on every `AnyToken` row; a second router with a real token store checks that
only `Public` rows answer a request with no credentials. Node-to-node routes
whose JSON message the probe doesn't build are listed exactly and covered by
`every_system_route_requires_the_system_principal`, a source check that their
handlers call `require_system`.

**Sessions and the system principal:** a dashboard session is always
read-only. One opened with the service token keeps the token name `__system`
but has the principal id `session:__system`, and
`sesame::auth::is_system_principal` requires both the name and the id to be
`__system`. `require_system` uses it, so a session never passes a node-to-node
route, whatever token it came from. The session cookie is `HttpOnly`,
`SameSite=Strict` and, when the API serves TLS, `Secure`.

**Bootstrap boundary (shipped):** An empty user-token store leaves protected
API routes open long enough to create the first cluster token. Bun contains
that window to an IP-literal loopback listener (`127.0.0.0/8` or `::1`). It
rejects wildcard and routable addresses, and also rejects hostnames rather than
resolving and checking one address before a later bind. Production Bun creates
the same explicit token store in standalone and clustered modes, so standalone
can't bypass the listener check by omitting council state. The check runs before
runtime, storage or observability startup in standalone mode.

**Rotation (shipped, F05 I3):**

```bash
$ relish token rotate ci-deploy [--grace-hours 24]
```

1. The node that takes `POST /v1/token/rotate {name, grace_hours}` generates a
   new secret and hash, and reads the clock once.
2. It proposes `RaftRequest::RotateApiToken(TokenRotation)`, carrying the new
   hash and salt, `rotated_at`, the new expiry (the token's previous lifetime
   length from now, or none) and `previous_valid_until` (now plus the grace,
   24 hours by default, capped at the old expiry; `None` for a zero grace).
3. The state machine (`sesame::token::apply_rotation`) moves the old hash into
   `ApiToken::previous_secret` and installs the new one. A second rotation
   replaces the previous secret, so at most two secrets work. Unknown names
   and `rbtest-` lease tokens are refused; the last Admin may rotate.
4. Validation tries the current secret, then the previous one until its
   `valid_until`; past that it answers `401 token rotated`. The old secret
   authenticates as its own principal (the digest of its hash), so last use,
   audit events and browser sessions opened with it stay separate, and those
   sessions end when it does.
5. The answering node installs the rotation in its own token store at once;
   the others pick it up on their five-second refresh. The handler records a
   `token.rotated` audit event with the caller and the grace end, never the
   secret. Role, scope and the `[permission]` spec stay with the name.

**Expiry:** A token with a set `expires_at` is rejected at authentication time once that time has passed (a `401 token expired`). A token with no expiry is never rejected on age grounds. Revocation is explicit via `relish token revoke`.

**Expiry sweep (shipped, F05 I2):** every hour (`bun::token_sweep::TOKEN_SWEEP_INTERVAL`) the leader proposes `RaftRequest::SweepExpiredApiTokens { now_unix_ms }`, but only when its own copy of the store has something due, so a quiet cluster writes nothing. The state machine decides what to remove from the entry's `now_unix_ms` alone (`sesame::token::tokens_to_sweep`), so every replica removes the same tokens: those whose `expires_at` plus a 24-hour grace (`EXPIRED_TOKEN_GRACE`) is past. Two rules override that, because an empty store is the bootstrap window in `auth_middleware`: the last Admin is never removed (if every Admin is due, the one that expired most recently stays, ties to the greater name), and the store is never emptied (with no Admin, the most recently expired token stays). The reply, `CouncilResponse::ApiTokensSwept { removed }`, lets the leader record one `token.expired_swept` audit event per token with principal `system`. A store whose every token has expired is not empty, so it still refuses anonymous requests (tested).

**Last use and the listing (shipped, F05 I2):** when `auth_middleware` authenticates a bearer token or a session cookie, it records "now" against the token's principal id in a node-local map. Nothing goes through Raft: a write per request would turn reads into log entries. `GET /v1/token/list` returns each token's name, principal id, role, scope, `created_at`, `expires_at` and `last_used`; the node asked fans out to every live member with `local=true` (allowed to the system principal only in that form; every other caller still needs an unscoped Admin), keeps the latest `last_used` per principal and names silent members in `warnings`. A restarted node forgets its share, so `last_used` can only under-report.

**Rate limiting:** Each API request checks the token's `rate_limit_rps`. A token-keyed sliding window counter (in-memory on the API-serving node) tracks request counts. Exceeding the limit returns HTTP 429 with a `Retry-After` header.

### 5.5 Secret Encrypt/Decrypt

**Encryption (client-side, offline):**

```bash
$ relish secret encrypt --pubkey age1qy8m5kz... "my-secret-value"
ENC[AGE:YWdlLWVuY3J5cHRpb24...]
```

> **Status:** `relish secret pubkey`, `relish secret encrypt` and `relish secret rotate` (start and `--finalize`) are all implemented. Rotation drives `RaftRequest::RotateSecretKey` / `FinalizeSecretRotation` through the council state machine. `--namespace` rotates and finalises one namespace's own key instead of the cluster-wide one (F05 I4, see the namespace-scoped keys note below).

The `relish` CLI uses the age public key to encrypt. No cluster access required. The ciphertext is embedded in the TOML app configuration and checked into git.

**Decryption (Bun, at workload start):**

1. Bun reads the app configuration (from Lettuce/git or direct deploy).
2. For each env var value matching `ENC[AGE:...]`, Bun requests decryption from the council.
3. The council decrypts using the age private key (cluster-wide or namespace-scoped).
4. The plaintext is returned over the mTLS channel.
5. Bun injects the plaintext as an environment variable. The runtime needs the full launch spec on disk to start and adopt the instance, so plaintext reaches disk only in root-only files (mode 0600, in owner-only directories): the runc bundle's `config.json`, deleted when the instance retires; the agent's adoption record, removed with the instance; and the runtime's own launch intent. Retiring the intent cuts every environment entry down to its variable name, so a retired intent keeps no value (`OciSpec::without_environment_values`); recovery compares a spec with an intent's copy through `OciSpec::matches_journal`, which accepts the scrubbed form.
6. A decryption audit event is logged: which secret, which app, which node, timestamp.

**Namespace-scoped keys (F05 I4, opt-in):** `secret_key = true` in `[namespace.X]` gives the namespace its own age keypair, wrapped with HKDF like the cluster key. Raft apply must be deterministic, so the state machine can't generate it: the leader's `bun::namespace_keys` loop finds opted-in namespaces with no key, generates generation 0, decrypts the namespace's stored `ENC[AGE:...]` app values with the cluster-wide keys, seals them again to the new key, and proposes key and values as one `RaftRequest::RotateSecretKey { scope: Namespace(X), resealed, .. }`. Each `ResealedSecret` carries the ciphertext the leader read; the state machine refuses the whole entry if any value changed since, belongs to another namespace, or the scope already has a key, and the leader retries on its next tick. Once a namespace has a key, its values decrypt **only** with that namespace's keys (`SecurityState::decryption_keypairs`): there is no fallback to the cluster-wide key, or a cluster-sealed value would still open there. A namespace without a key uses the cluster-wide keys only. Rotation and finalise are per namespace (`relish secret rotate [--finalize] --namespace X`, `POST /v1/secret/rotate {"namespace": "X"}`), refused for a namespace with no key, and limited to unscoped Admins; `GET /v1/secret/public-key?namespace=X` serves its active public key. Job specs aren't stored desired state, so they aren't re-sealed. **Limitation:** until the master key is split (F03b), every node holds the master key and can unwrap every namespace's key, so the boundary is against other tenants' tokens and workloads, not against a compromised node.

**Key rotation (`relish secret rotate`):**

1. Generate a new age keypair.
2. Store the new keypair in Raft, marking the old keypair as `read_only = true`. Starting a second rotation while one is un-finalised is refused (idempotent retries of the same rotation, deduped on the generation number, are accepted).
3. The cluster now accepts ciphertexts encrypted with either key; new encryption always uses the newest non-read-only generation.
4. The operator (or CI) re-encrypts all secrets with the new public key and commits to git. Each applied `AppSpec` records which generation seals its encrypted values (`SecurityState.secret_seals`, keyed `namespace/app/ENV_KEY`) — age ciphertext does not disclose its recipient, so write time is the only moment this is knowable.
5. Once all `ENC[AGE:...]` values use the new key, the operator runs `relish secret rotate --finalize` to delete the old keypair. Finalise verifies the seal records first: any secret still sealed under an older generation — or with no record at all — refuses the retirement and is named in the error, so the key that can decrypt it is never deleted early.

### 5.6 Raft Log Encryption

> **Status: implemented (with a cluster master key).** `council::durable_log`
> now routes entry values through `sesame::raft_encryption` (HKDF derivation,
> AES-256-GCM seal/open) whenever `open_with_key` is given a cluster master key.
> Encrypted entries carry a one-byte marker and a per-entry salt+nonce; a
> plaintext entry (leading `{`) still decodes, so plaintext and encrypted logs
> interoperate and a keyless/dev cluster stays plaintext. An encrypted entry
> opened without the key errors rather than reading as empty (CP3-safe). Vote and
> log-id metadata remain plaintext bincode (not app data). The HKDF *wrapping* of
> sensitive keys inside the log (step 5) ships as before. TPM sealing (step 2) is
> still not implemented.

On each council node (intended design):

1. At startup, derive the AES-256-GCM encryption key via HKDF:
   - **Input keying material:** the node's certificate private key (DER bytes).
   - **Salt:** a random 32-byte salt stored alongside the Raft log on disk.
   - **Info:** `"reliaburger-raft-log-encryption-v1"`.
2. If a TPM is available, seal the derived key to the current PCR values. The key can only be unsealed on the same hardware with the same boot state.
3. All Raft log writes are encrypted with AES-256-GCM using the derived key. Each entry gets a unique nonce (96-bit counter, never reused).
4. On read, entries are decrypted in memory. The decrypted Raft state exists only in memory.
5. Sensitive keys within the Raft log (age private key, intermediate CA private keys) receive an additional wrapping layer via HKDF, so even if the Raft log encryption is somehow bypassed, these keys remain protected.

### 5.7 CRL Distribution

1. An operator runs `relish ca revoke --node node-07`.
2. The leader adds the node's certificate serial number to the CRL in Raft.
3. The updated CRL is distributed to all nodes via the hierarchical reporting tree (the same tree used for health reporting and scheduling state).
4. Each node caches the CRL in memory.
5. On every inbound mTLS handshake, the node checks the peer's certificate serial against the CRL. If the serial is present, the handshake is rejected.
6. The revoked node is effectively expelled from the cluster. It cannot communicate with any other node. It must re-join via a new join token.

**CRL propagation time:** The reporting tree distributes the CRL within seconds (measured at < 2 seconds in the target cluster size of ~200 nodes). The CRL is small -- just a list of serial numbers and metadata.

### 5.8 CA Rotation

> **Status: intermediates implemented (F04 R1–R4, #362); root rotation planned.** The council state can hold
> several CAs per role. Each has a `CaState` (`Active` or `Retiring { until }`);
> `SecurityState::active_ca(role)` is the one that signs and
> `trusted_cas(role)` is every one a verifier accepts. Two Raft requests drive
> a rotation, the same shape as secret rotation:
>
> - `CaRotationBegin { role, ca }` adds `ca` as the active CA (generation + 1,
>   signed by the active root, wrapped key present) and marks the old one
>   `Retiring` until the longest leaf it could have signed expires. A second
>   rotation of the same role is refused until the first is finalised; a retry
>   of the same generation changes nothing. Root rotation is refused for now.
> - `CaRotationFinalize { role, now_unix_ms }` drops the retiring CA. For the
>   Node CA it's refused while any node's latest leaf
>   (`SecurityState::node_leaves`, recorded when the council allocates the
>   serial) came from the retiring CA; workload and ingress leaves aren't
>   tracked one by one, so for those it's refused until the window ends.
>
> Every verifier trusts the whole set (F04 R2). A node's identity carries a
> `TrustSet` (every trusted Node CA and root); the mTLS server and client
> verifiers, renewal's `validate_peer`, the join bundle and `GET
> /v1/cluster/ca`, keyless image signatures and the workload `ca.pem` all try
> each trusted CA. Bun's security refresh installs the council's trust set
> every five seconds through `LiveNodeIdentity::adopt_trust`, which persists
> it and refuses any set that isn't the council's or that drops the node's own
> issuer; listeners read it on every handshake, so it applies without a
> restart. The ingress resolver rebuilds when the active Ingress CA changes.
>
> The `relish ca` family has `ca backup` and `ca verify` (F04 R3): an
> operator-held root backup sealed to a passphrase or an age recipient, checked
> offline for key match, expiry and fingerprint. `ca rotate` rotates an
> intermediate (F04 R4, below). There is no `ca rotate --root` yet (R5).
> (Certificate *revocation* via the CRL — §5.7, `RaftRequest::RevokeCertificate`
> — is separate and does ship.)

**Intermediate CA rotation (`relish ca rotate --role node|workload|ingress --root-backup <file>`, F04 R4):**

1. **Prepare** (`POST /v1/ca/rotation/prepare {role}`). The leader generates the new P-256 key, wraps it with the master key and proposes `RaftRequest::CaRotationPrepare { role, generation, csr_der, private_key_wrapped }`. Applying it stores a `PendingIntermediate` (one per role; a second prepare replaces the first) and allocates the certificate's serial from `next_serial`, so it can't collide with a revoked serial. Refused for the root, for a role mid-rotation and for a generation other than the active one's plus one. The answer carries the CSR, generation, serial and the active root's fingerprint.
2. **Sign** (on the operator's machine). `relish` opens the R3 backup, checks its fingerprint against the one the council sent, and signs the CSR with `ca::sign_intermediate_csr`: only the CSR's public key is used; name, path length 0, key usages and five-year lifetime are fixed per role and clamped to the root's validity. The root key never reaches the cluster.
3. **Begin** (`POST /v1/ca/rotation/begin {role, certificate_b64}`). The leader checks the certificate with `ca_rotation::intermediate_from_signed` (pending CSR for the role, same public key, allocated serial, CA certificate, signed by the active root), builds the `CertificateAuthority` with the pending wrapped key and proposes `CaRotationBegin`, which clears the pending CSR. Both CAs are trusted; new leaves come from the new one.
4. **Re-issue.**
   - *Node:* each node's renewal worker checks its replica every 5 s. Once its live identity holds exactly the council's trust set it sends `POST /v1/cluster/trust-ack {node_ca_fingerprints}` (node-to-node, TLS-peer authenticated); the leader refuses unless every trusted Node CA is listed, then proposes `AcknowledgeNodeTrust { node_id, generation }`, which raises `NodeLeafRecord::trust_generation` (never lowers it; a join starts at the active generation). When every live node has acknowledged, nodes renew early (`ca_rotation::early_renewal_due`) one at a time in node-id order, each after every earlier node holds a new-CA leaf, or after its slot (`EARLY_RENEWAL_STAGGER`, 60 s per place from the new CA's issue time) if one is stuck.
   - *Workload:* leaves renew every 30 minutes, so they move within the hour.
   - *Ingress:* the resolver reload (R2) re-mints route certificates from the new CA within 5 s.
5. **Finalise** (`POST /v1/ca/rotation/finalize {role}`, `relish ca rotate --role R --finalize`). For the Node CA, refused while any live (not decommissioned) node hasn't acknowledged the active generation or still has a latest leaf from the retiring CA, naming the nodes. For Workload and Ingress, refused until the retiring window ends (an hour, 90 days). Then the retiring CA is removed.

All three admin routes need an unscoped Admin user (never the service principal) with the cluster-wide `admin` permission, and must reach the leader: a follower answers 421 naming it. Each step records an audit event (`ca.rotation_prepared`, `ca.rotation_begun`, `ca.rotation_finalised`).

**Root CA rotation (`relish ca rotate --root`):**

1. The operator provides the sealed root CA backup file.
2. The old root CA key is decrypted.
3. A new root CA keypair is generated.
4. New intermediate CAs are generated and signed by the new root.
5. The old root cross-signs the new root (creating a cross-certificate for transition).
6. During the transition period, both old and new root CAs are trusted.
7. The new root CA key is sealed and backed up. The old root CA key is discarded.

### 5.9 Egress Allowlist Processing

Current status: egress enforcement is opt-in per app. With no `[app.NAME.egress]`
block, Bun allows external egress. Once an app declares a non-empty allowlist,
the following contract applies and there is no warning-only fallback.

When Bun processes an app's `[app.NAME.egress]` block:

1. Each entry is parsed: exact IPv4/IPv6 destinations (`1.2.3.4:443`, `[2001:db8::1]:443`), CIDRs (`10.0.0.0/8:443`, `[2001:db8::/32]:443` — host bits rejected), or hostnames resolved via DNS keeping both A and AAAA records.
2. On runtimes that honour the OCI `cgroupsPath` (currently root-mode runc), Bun creates the instance's cgroup directory itself and programs the eBPF maps against its inode *before* the workload starts (create → program → start). There is no window in which the process runs ahead of its policy. A node without this runtime contract refuses the workload; ProcessGrill, Apple Container, non-eBPF builds and rootless runc don't quietly fall back to post-start programming.
3. Exact destinations go into per-family hash maps (`egress_map`, `egress6_map`); CIDRs into per-family LPM tries with the ports of enclosing prefixes folded into more specific entries.
4. Enforcement requires four live hooks: `connect4` and `connect6` cover connected TCP/UDP sockets, while `sendmsg4` and `sendmsg6` cover unconnected UDP (`sendto()` doesn't invoke a connect hook). If any hook is unavailable, deploys with an allowlist are refused.
5. Bun re-resolves DNS-based entries periodically (about every 5 minutes) and reprograms the maps when a hostname's addresses change.
6. Bun reports the four hooks and pre-start runtime support as a typed live node capability. The scheduler filters policy-bearing workloads using it, and the agent repeats the check immediately before start so a stale report can't open a gap.
7. Every one-second agent tick verifies the live hooks and each protected cgroup's enforcement flag. It repairs a missing flag once and verifies the result. Hook loss, an unreadable map or failed repair stops the affected workload, records the affected app and makes the node unready until all four hooks recover. The slower kernel-truth sweep (`[ebpf] sweep_interval_secs`, default 60) still scrubs stale state and rebuilds all entries.
8. `allow_franchise` remains unimplemented. Bun refuses it explicitly rather than starting a workload with unrestricted cross-cluster egress.

### 5.10 Image Signing Trust Roots

`[images.trust_policy] require_signatures` admits a Pickle-hosted image only
when its attached signature verifies under one of two trust roots:

- **The cluster root CA** (keyless). The per-namespace build signer
  (`spiffe://…/job/build-signer`, a code-signing leaf from the Workload CA)
  signs what `relish build` pushes. No configuration: the chain, validity,
  code-signing EKU, SPIFFE identity and CRL are all checked against state the
  council already holds.
- **Operator keys** (`trust_policy.keys`). Base64 uncompressed ECDSA P-256
  public keys in each node's config file. `relish sign IMAGE --key PATH`
  resolves IMAGE to its manifest digest, signs the digest locally, and sends
  `{digest, public_key, signature}` to `POST /v1/identity/sign` (unscoped
  Admin). The agent checks only that the signature verifies under the key it
  carries, then writes `AttachSignature`; trust is decided at deploy time.
  `relish sign keygen --out PATH` makes a PKCS#8 PEM key (mode 0600, never
  overwrites) and prints the public key line; any unencrypted PKCS#8 P-256
  key from `openssl genpkey` works too.

Why the operator holds the external key rather than the cluster: if the
cluster held a signing key behind `/v1/identity/sign`, any Admin API token
could make any image trusted. With the key on the operator's machine and the
public half in node config, making an image trusted takes the private key and
write access to node config; a stolen API token can only attach signatures
nobody trusts. (The first `relish sign` signed with a key the agent generated
per call and discarded, so no policy could ever list it; that path is gone.)

The manifest carries a single signature slot, so a later `relish sign`
replaces an earlier signature (including a build signer's). Signatures bind
digests, never tags: re-pushing a tag leaves the new digest unsigned.

The signature format is Pickle's own (P-256 over the digest string), not
cosign's. Images from outside Pickle carry no signature Pickle checks yet,
but every apply binds their tags to digests (`nginx:1.27@sha256:…`), so the
bytes that run are the bytes the apply resolved (F03 U1, #361). Which
upstream registries a node runs images from is a third trust root,
`[[images.trust_policy.upstream]]` (`match` a repository or a `*` prefix; the
most specific rule wins) with `[images.trust_policy.upstream_default] allow`
(default `true`; `false` makes the rules an allow-list). The node handling an
apply checks it before contacting any registry, and Bun checks it again before
every deploy where a council catalogue tells Pickle's images from upstream
ones (F03 U2). It stays in `node.toml` for the same reason the keys do:
nothing an API token can change should decide what the cluster trusts. A rule
with `require_signatures = true` and `cosign_keys` (PEM P-256 keys) makes Bun
verify a key-based cosign signature over the bound digest before every
deploy, off the agent loop, reading the `.sig` image through the pull-through
cache when it's on (F03 U3). Either half without the other is refused at
startup.

---

## 6. Configuration

All security configuration lives in the cluster config (applied via `relish apply` or set during `relish init`).

### 6.1 Certificate Lifetimes and Rotation

```toml
[security]
# Workload certificate lifetime. Default: 1 hour.
workload_cert_lifetime = "1h"

# Workload certificate rotation interval (should be < lifetime).
# Default: 30 minutes (half of lifetime).
workload_cert_rotation = "30m"

# Grace period extension when council is unreachable.
# Bun continues using an expired workload cert for up to this duration.
# Default: 4 hours.
cert_grace_period = "4h"

# Node certificate lifetime. Default: 1 year.
node_cert_lifetime = "365d"

# Ingress certificate lifetime. Default: 90 days.
ingress_cert_lifetime = "90d"

# Intermediate CA lifetime. Default: 5 years.
intermediate_ca_lifetime = "5y"

# CA key algorithm. Options: "ecdsa-p256", "ecdsa-p384", "ed25519", "rsa-4096".
# Default: "ecdsa-p256".
ca_algorithm = "ecdsa-p256"
```

The block above is the design sketch. What ships in 0.1.0 is narrower: the
lifetimes are compiled constants in `src/sesame/ca.rs` (`NODE_LEAF_LIFETIME`
one year, `INGRESS_LEAF_LIFETIME` 90 days, workload identity one hour in
`src/sesame/identity.rs`, root CA 10 years, intermediates 5 years), and the
only node-config knob is a development-only override for the two leaf classes.

#### 6.1.1 Soak override: `leaf_lifetime_override_secs`

```toml
[testing]
safety_class = "development"

[security]
leaf_lifetime_override_secs = 3600
```

**Why it exists.** A node leaf renews at six months, so a 24-hour soak would
exercise node renewal zero times. Qualifying renewal under restarts, leader
changes and power cuts needs dozens of renewals per node per day. The override
shortens the node leaf and the cluster-issued ingress leaf to the given number
of seconds; at 3600 a node renews roughly every 27 minutes (half of the
3,900-second signed window, which includes the 300-second backdate).

**Why development only.** A short leaf turns every leader outage longer than
half the lifetime into a fleet-wide expiry, which then needs operator
re-enrolment. That's the right trade in a soak and the wrong one anywhere else,
so `NodeConfig::validate` refuses the key unless `[testing] safety_class` is
`development` (an absent section is `unknown`, which is refused). The value
must be between 600 seconds (`MIN_LEAF_LIFETIME_OVERRIDE`) and 90 days: it
shortens both leaf classes, so it can't exceed the shorter default without
lengthening ingress leaves. Workload identity and the CA lifetimes aren't
affected. Bun refuses to start on a violation, naming the key.

**Where it takes effect.** The lifetime is decided where the leaf is signed:

| Leaf | Signed by | Whose value applies |
|------|-----------|---------------------|
| Node leaf, renewal | council leader (`POST /v1/cluster/renew`, `issue_renewal`) | the leader's |
| Node leaf, join | the member handling the join (`handle_join_issue`) | that member's |
| Node leaf, `relish init` | `relish` on the operator's machine | always one year |
| Ingress leaf | each ingress node for itself (`IngressCertResolver`) | that node's |

**When nodes disagree.** The issuer's value always sets the signed lifetime;
operators should give every node the same value. The renewal worker also treats
its own override as a ceiling (`NodeRenewalWorker::with_leaf_lifetime_ceiling`):
it renews no later than the midpoint of a ceiling-length leaf (for 3600,
1,650 seconds after issue), whatever the signed lifetime. That covers the two
awkward cases:

- A node that gains the override while holding a one-year leaf (including the
  first node's `relish init` leaf) renews within half the override instead of
  in six months, and gets a short leaf from a leader that also carries it.
- A node with the override behind a leader without it gets a one-year leaf
  back. It renews again half a ceiling later, not on the next one-second tick,
  so a mismatch costs one extra renewal per half-override and never a loop.

A node without the override behind a leader with it simply receives short
leaves and renews at their midpoint as usual.

### 6.2 Node Authentication

```toml
[cluster]
# Node attestation mode during join.
# Options: "none" (default), "tpm", "certificate".
node_attestation = "none"

# Join token TTL. Default: 15 minutes.
join_token_ttl = "15m"

# External CA certificate for "certificate" attestation mode.
# Path to a PEM file containing the trusted external CA.
external_ca_path = ""
```

### 6.3 API Tokens

```toml
[security.tokens]
# Default lifetime of new Deployer and ReadOnly tokens: "<n>d", "<n>h" or
# "none". Admin tokens never get one. Default: "90d".
default_ttl = "90d"
```

Only `default_ttl` is parsed. The rotation grace is per call
(`relish token rotate --grace-hours`, 24 by default) rather than a node
setting, and per-token rate limiting is not part of F05.

### 6.4 Secret Encryption

```toml
# Namespace-scoped secret keys (opt-in per namespace, F05 I4).
# The leader creates the namespace's key and re-seals its stored app values
# (§5.5). From then on its values decrypt only with its own key.
[namespace.team-payments]
secret_key = true    # a separate age keypair for this namespace
```

### 6.5 Network Security

```toml
[cluster]
# Planned, not currently parsed or enforced: default policy for apps without
# an [egress] block.
# Options: "deny" (default, recommended), "allow" (escape hatch for migration).
default_egress = "deny"
```

```toml
[security]
# Operator networks allowed through the perimeter to this node's API port
# (`bun --listen`, default 9117) and to no other port. Implemented.
operator_cidrs = ["10.0.0.0/8", "192.168.1.0/24", "2001:db8:1::/48"]
```

`operator_cidrs` sits in `[security]` beside `bootstrap_peers`, the other
perimeter allowlist, and is node-local (it is not replicated through Raft or
gossip). The two lists differ on purpose: a bootstrap peer is a future cluster
member and may reach the API and the gossip, Raft and reporting ports; an
operator network reaches the API port only, because no human client speaks the
cluster protocols. The Pickle registry port is not in the perimeter's drop set
(it relies on its own TLS and authentication), so the list does not mention it.

Validation happens at config load (`NodeConfig::validate`), so Bun refuses to
start rather than silently keeping the operator locked out. Entries are IPv4 or
IPv6 CIDRs, or bare addresses (a single host). A `/0` in either family is
refused with no override: opening the API to every address is never what the
setting is for. A CIDR with host bits set (`192.168.0.17/24`) is refused with
the intended network in the error. Only the parsed, re-serialised form reaches
`nft -f`. The default is empty, which renders no operator rule at all, so the
laptop quickstart (whose API forward arrives on the node's loopback) is
unaffected. Token and mTLS authentication on the API are unchanged; this is a
packet-filter setting only. The list is read at startup; changing it means
restarting Bun.

```toml
# Per-app egress allowlist.
[app.api.egress]
allow = [
    "*.amazonaws.com:443",
    "api.stripe.com:443",
    "db.example.com:5432",
]

# Per-app inbound firewall (eBPF layer).
# Each entry is a string: a bare "app" (same namespace as the target) or
# "namespace/app" for a cross-namespace source. (The AppRef struct in §4.7 is
# the internal representation; the config format is these strings.)
[app.payment-service.firewall]
allow_from = ["api", "admin", "team-web/frontend"]
```

### 6.6 OIDC Configuration

```toml
[security.oidc]
# The OIDC issuer URL published in the discovery document.
# Default: derived from the cluster's API endpoint.
issuer = "https://reliaburger.prod.example.com"

# Per-app audience configuration.
# PLANNED — not yet emitted into JWTs. Minted tokens currently carry only
# aud = ["spiffe://<cluster>"]; these extra audiences are not appended (§5.3).
[app.api.identity]
audiences = ["sts.amazonaws.com"]

[app.data-pipeline.identity]
audiences = ["sts.amazonaws.com", "iam.googleapis.com"]
```

### 6.7 Council Size

```toml
[cluster]
# Number of council (control plane) nodes.
# Must be odd for Raft quorum. Affects CA key replication.
council_size = 3
```

---

## 7. Failure Modes

### 7.1 Council Outage (Certificate Grace Period)

**Scenario:** All council nodes are unavailable. No CSRs can be signed.

**Impact:**

- Running workloads continue operating with their current certificates.
- When a workload's certificate approaches expiry (after the normal 1-hour lifetime), Bun activates the grace period extension, extending local validity by up to 4 hours (configurable).
- Total window before mTLS breaks: 5 hours (1-hour cert lifetime + 4-hour grace).
- New workloads that start during the outage cannot receive identity certificates. They wait for council availability.
- Grace-extended certificates are flagged in the local event log. `relish wtf` warns about them. An alert fires.

**Mitigation:** Council size of 3 or 5 ensures quorum survives single-node or dual-node failures. The 5-hour window gives operators ample time to restore at least one council node.

### 7.2 CA Key Compromise

**Scenario:** An intermediate CA private key is exfiltrated from a council node.

**Impact:**

- **Node CA compromised:** Attacker can forge node certificates and join the cluster as a fake node.
- **Workload CA compromised:** Attacker can forge workload identity certificates and impersonate any app.
- **Ingress CA compromised:** Attacker can forge ingress TLS certificates.
- Compromise of one intermediate CA does not affect the others. A leaked Workload CA key cannot forge node certificates.

**Response:**

1. Immediately run `relish ca rotate` for the compromised CA. **(Planned — CA rotation is not implemented yet, §5.8. Until it ships, the practical response is CRL-revoking affected certificates and, in the worst case, re-bootstrapping the PKI.)**
2. Revoke all certificates issued by the compromised CA (via the CRL, §5.7).
3. The dual-signing period ensures existing legitimate certificates continue working during the transition.
4. Investigate how the key was exfiltrated. Council nodes should have restricted access; TPM sealing and at-rest Raft log encryption are planned hardening (§5.6), not yet active.

**Root CA compromise:** If the sealed root CA backup is stolen, the attacker can sign new intermediate CAs. This requires a full PKI re-bootstrap: `relish init` on a new cluster and migrating workloads.

### 7.3 Join Token Leak

**Scenario:** A join token is accidentally exposed (e.g., CI logs, terminal recording).

**Impact:** An attacker with the token can join a rogue node to the cluster within the token's TTL (default 15 minutes).

**Mitigations:**

- Tokens are single-use: once consumed, a second use is rejected.
- Tokens are short-lived: default 15 minutes.
- Tokens are output to stderr only, never written to disk or structured logs.
- If `node_attestation = "tpm"` is enabled, the token alone is insufficient -- the attacker also needs a trusted TPM.
- If a leak is suspected before the token is consumed: no action needed if the TTL has expired. If the TTL has not expired, generate a new token and do not use the leaked one. There is no explicit "revoke token" command because the token auto-expires.
- If a rogue node has already joined: `relish ca revoke --node <rogue-node>` immediately expels it.

### 7.4 CRL Propagation Delay

**Scenario:** A node's certificate is revoked, but some nodes have not yet received the updated CRL.

**Impact:** The revoked node can still communicate with nodes that have a stale CRL. In the target cluster size (~200 nodes), the reporting tree distributes the CRL in under 2 seconds, so the window is very narrow.

**Mitigations:**

- The reporting tree is the fastest distribution mechanism in the cluster (sub-second for small payloads).
- Nodes that are temporarily unreachable (and thus miss the CRL update) will receive the updated CRL when they reconnect and sync state.
- For critical revocations, `relish ca revoke` outputs the CRL distribution status, confirming which nodes have acknowledged receipt.

### 7.5 Raft Log Encryption Key Loss

> **Applies only once §5.6 ships.** At-rest Raft log encryption and TPM sealing
> are not wired today (the log is plaintext), so this failure mode is about the
> planned design, not current behaviour.

**Scenario:** A council node's disk is moved to different hardware, breaking TPM sealing.

**Impact:** The Raft log cannot be decrypted on the new hardware. The node cannot start.

**Mitigation:** The node must re-join the cluster as a new council member. Raft replication will provide the current state from the other council nodes. The node derives a new encryption key from its new identity. This is by design -- TPM sealing prevents offline disk access.

### 7.6 Age Private Key Loss

**Scenario:** All council nodes are permanently lost, and the age private key was not backed up separately.

**Impact:** All `ENC[AGE:...]` secrets in git become unrecoverable.

**Mitigation:** The age private key is replicated across all council nodes via Raft. Losing all council nodes simultaneously is a catastrophic scenario that also loses all other cluster state. `relish init --import-key` allows bootstrapping a new cluster with a previously exported private key. Operators should maintain offline backups of the age private key for disaster recovery.

---

## 8. Security Considerations

### 8.1 Threat Model

The threat model assumes:

- **Trusted:** The operator who runs `relish init` and has access to the sealed root CA backup.
- **Semi-trusted:** Council nodes. They hold sensitive key material. The intended hardening (minimal attack surface, TPM sealing, encrypted Raft logs) is only partly shipped: TPM sealing and at-rest Raft log encryption are planned, not active (§5.6). Today's protection for keys in the log is the per-key HKDF wrapping, not whole-log encryption.
- **Untrusted:** Worker nodes. They may be compromised. The system is designed so that a compromised worker node has limited blast radius.
- **Untrusted:** Network between nodes. All inter-node communication is mTLS-authenticated and encrypted.
- **Untrusted:** External network. Declared app allowlists are default-deny; apps without an egress block remain unrestricted until the planned cluster default lands. nftables perimeter rules and Wrapper-only ingress cover inbound traffic.

### 8.2 Compromised Worker Node

> **Current blast radius is larger than the target model.** Because every
> clustered node loads the cluster master key and bootstrap security state
> (§3.2), a compromised clustered node can locally unwrap the age private key,
> the intermediate CA private keys and the OIDC signing key. The mitigations
> below describe the *intended* worker/council split, which is planned work; a
> node compromise today should be treated closer to a council compromise (§8.3)
> until the key split ships.

**What the attacker gains (today):**

- The node's own node certificate and private key (can impersonate this specific node).
- Workload certificates for workloads currently running on this node (1-hour lifetime).
- Plaintext secret values for workloads running on this node (in-memory only, not on disk).
- The ability to send CSRs for workloads scheduled on this node.
- The cluster master key and bootstrap state, and therefore the ability to unwrap the age private key, the CA private keys and the OIDC signing key.

**What the target model intends the attacker cannot do (not all enforced today):**

- Obtain certificates for workloads on other nodes (CSR validation checks Meat's scheduling state — this *is* enforced).
- Forge certificates for arbitrary workloads — **not yet guaranteed:** CA private keys are currently derivable on every node.
- Decrypt secrets for other namespaces — **not yet guaranteed:** a namespace with `secret_key = true` has its own age key (§5.5), but every node can still unwrap every namespace's key until the master key is split (F03b).
- Access the age private key, CA private keys, or OIDC signing key — **not yet guaranteed** (see the note above).
- Modify the Raft log or cluster state (requires council consensus — this *is* enforced).
- Bypass nftables perimeter rules on other nodes.

**Response:**

1. `relish ca revoke --node <compromised-node>` -- immediately expels the node.
2. Workload certificates for that node expire within 1 hour (or sooner if council stops renewing them).
3. Rotate any secrets that were exposed to workloads on that node.

### 8.3 Compromised Council Node

**What the attacker gains:**

- All intermediate CA private keys (can forge any certificate).
- The age private key (can decrypt all secrets).
- The OIDC signing key (can forge JWTs).
- The Raft log contents (all cluster state).

**What the attacker cannot do:**

- Forge the root CA (private key is not on any cluster node).
- Act unilaterally if other council nodes are not compromised (Raft requires quorum for writes).
- However, read-only access to the key material is sufficient for forging certificates and decrypting secrets.

**Response:**

1. Isolate the compromised council node immediately.
2. Rotate all intermediate CAs: `relish ca rotate`. **(Planned — not implemented, §5.8.)**
3. Rotate the age keypair: `relish secret rotate`.
4. Rotate the OIDC signing keypair (re-minting all JWTs).
5. Rotate all API tokens.
6. Audit all cluster activity during the compromise window.
7. Investigate the attack vector and harden council node access.

**Prevention:**

- Council nodes should have the smallest possible attack surface.
- TPM sealing would ensure key material cannot be extracted even with disk access (planned — not implemented).
- Encrypted Raft logs would protect against offline forensic access (planned — the encryption module exists but is not wired; the log is plaintext today, §5.6). Per-key HKDF wrapping does protect the age/CA keys stored inside the log.
- Council nodes should be on a restricted management network.

### 8.4 Join Token Theft

See Section 7.3. The short TTL (15 minutes), single-use property, and optional TPM attestation limit the blast radius. The token is the only shared-secret moment in the entire cluster lifecycle.

### 8.5 DNS Poisoning of Egress Allowlists

**Scenario:** An attacker poisons DNS responses for a hostname in an app's egress allowlist, causing Bun to add a malicious IP to the eBPF maps.

**Impact:** The app could connect to an attacker-controlled server instead of the legitimate service.

**Mitigations:**

- Bun resolves via multiple upstream DNS servers and requires consistent answers. Divergent responses are logged as security events.
- When a hostname's resolved IP changes, the event is logged for audit.
- IP-based allowlists (CIDR notation) are immune to DNS poisoning.
- DNSSEC validation can be enabled in the system resolver to cryptographically verify DNS responses.
- For highly sensitive services, use IP-based allowlists instead of hostname-based ones.

### 8.6 eBPF Bypass Attempts

**Scenario:** A compromised workload attempts to bypass the eBPF firewall by manipulating its cgroup, using raw sockets, or exploiting kernel vulnerabilities.

**Mitigations:**

- Workloads run in unprivileged containers without `CAP_NET_RAW`, `CAP_NET_ADMIN`, or `CAP_SYS_ADMIN`.
- Cgroup IDs are assigned by the kernel at container creation and cannot be changed by unprivileged processes.
- The eBPF programs are attached at the cgroup level for IPv4 and IPv6 connect and UDP sendmsg operations (`BPF_CGROUP_INET4_CONNECT`, `BPF_CGROUP_INET6_CONNECT`, `BPF_CGROUP_UDP4_SENDMSG`, `BPF_CGROUP_UDP6_SENDMSG`).
- Raw socket creation requires `CAP_NET_RAW`, which is dropped from the container's capability set.
- The nftables perimeter layer provides defense-in-depth: even if a workload bypasses eBPF, the nftables rules on the destination node enforce cluster perimeter policy.

### 8.7 Confused Deputy (OIDC Token Replay)

**Scenario:** A JWT token intended for one service is intercepted and replayed against a different service.

**Mitigation:** Every JWT includes the audience `spiffe://CLUSTER_NAME`. The verifying service must check that it is the intended audience in the `aud` claim. Per-app audiences for cloud IAM federation (e.g., `sts.amazonaws.com` configured via `[app.NAME.identity]`) are **planned but not yet emitted** — today the `aud` claim carries only `spiffe://CLUSTER_NAME`, so cloud-provider audience federation does not work until that ships (§6.6).

---

## 9. Performance

### 9.1 CSR Round-Trip Latency

The CSR flow (worker generates keypair, sends CSR to council, council validates and signs, returns certificate) adds latency only at certificate rotation time, not on every connection.

| Operation | Expected Latency |
|-----------|-----------------|
| Keypair generation (ECDSA P-256) | < 1 ms |
| CSR creation and serialisation | < 1 ms |
| Network round-trip to council (same datacenter) | 1-5 ms |
| CSR validation (check Meat state) | < 1 ms |
| Certificate signing (ECDSA P-256) | < 1 ms |
| **Total CSR round-trip** | **2-10 ms** |

This occurs once every 30 minutes per workload instance. For a node running 50 workloads, that is 50 CSRs every 30 minutes, or roughly 1.7 CSRs per minute -- negligible load on the council.

### 9.2 CRL Check Overhead

The CRL is a list of revoked serial numbers cached in memory. The check is a hash set lookup on every inbound mTLS handshake.

| Metric | Value |
|--------|-------|
| CRL lookup (hash set, typical size < 100 entries) | < 100 ns |
| Memory overhead per node | < 1 KB for typical CRL sizes |
| Impact on mTLS handshake latency | Unmeasurable (< 0.1% of handshake time) |

### 9.3 nftables Rule Count Limits

The nftables ruleset is kept minimal by design:

| Rule Category | Typical Count |
|---------------|--------------|
| Perimeter rules (input chain) | 5-10 static rules |
| Cluster nodes set | 1 set, N entries (N = cluster size) |
| Admin CIDRs set | 1 set, typically < 10 entries |
| Per-app egress sets | 1 set per app with egress config |
| Per-app egress set entries | Typically < 50 IPs per app |

nftables handles thousands of set entries efficiently (O(1) lookup via hash sets). The `reliaburger` table is reconciled every 30 seconds and on cluster membership changes.

### 9.4 eBPF Firewall Check Cost

The eBPF firewall check happens inside the `connect()` interceptor, alongside the existing Onion service discovery logic.

| Metric | Value |
|--------|-------|
| BPF map lookup (firewall_map, per-connection) | < 200 ns |
| Memory per firewall rule entry | 16 bytes (source cgroup ID + dest cgroup ID) |
| Impact on connection establishment | Unmeasurable in application-level benchmarks |

The eBPF check is a single BPF hash map lookup. It does not allocate memory, does not make syscalls, and does not copy data to userspace. For UDP, the `sendmsg()` interceptor adds the same cost per datagram.

### 9.5 Secret Decryption Overhead

Secret decryption occurs at workload start time, not at runtime.

| Operation | Expected Latency |
|-----------|-----------------|
| age decryption per secret value | < 1 ms |
| Network round-trip to council for decryption | 1-5 ms |
| Typical app with 5-10 secrets | 5-50 ms total at start |

Decrypted values are held in memory and injected as env vars. There is no per-request decryption overhead.

---

## 10. Testing Strategy

### 10.1 PKI Rotation Testing

- **Unit tests:** Verify that `relish ca rotate` generates valid intermediate CAs, that the dual-signing period trusts both old and new CAs, and that certificates issued by the old CA continue validating until expiry.
- **Integration tests:** Spin up a 3-node council cluster. Issue workload certificates. Rotate the Workload CA. Verify that existing workloads continue operating (old certs still valid) and that new CSRs are signed by the new CA.
- **Root rotation test:** Provide the sealed root CA backup, rotate the root, verify cross-signing works, verify all intermediates are re-issued under the new root.

### 10.2 Certificate Expiry Simulation

- **Grace period test:** Start a workload, then make the council unreachable. Verify that the workload certificate enters grace period extension after the normal lifetime. Verify that `relish wtf` reports the grace-extended certificate. Verify that after the grace period (default 5 hours total), the certificate is no longer accepted.
- **Hard expiry test:** Set `cert_grace_period = "0s"` and verify that workloads lose mTLS exactly at the certificate lifetime boundary.
- **Clock skew test:** Simulate clock skew between worker and council nodes. Verify that certificates with `not_before` in the future are rejected, and that near-expiry certificates trigger early rotation.

### 10.3 Firewall Verification

- **nftables perimeter test:** From outside the cluster, attempt to connect to management ports and app ports. Verify that connections are rejected unless originating from cluster nodes, `bootstrap_peers`, or (API port only) `operator_cidrs`. Implemented for `operator_cidrs` as `operator_cidr_reaches_the_api_port_but_not_cluster_ports` in `tests/owned_network.rs`, which applies the real ruleset in a throwaway network namespace.
- **eBPF firewall test:** Deploy two apps in the same namespace with `allow_from` restrictions. Verify that unauthorized apps receive `EPERM`. Verify that authorised apps connect successfully. Verify that apps in different namespaces cannot communicate without explicit cross-namespace rules.
- **Egress allowlist test:** Deploy an app with an `egress` block. Verify that TCP and UDP connections to allowed IPv4/IPv6 destinations succeed and connections to disallowed destinations are dropped. Verify DNS resolution refresh by changing the DNS record and confirming the eBPF maps update.
- **`relish firewall test` integration:** Verify that the `--from` / `--to` diagnostic command accurately reports whether a connection would be permitted.

### 10.4 Join Token Security

- **Expiry test:** Create a join token, wait for the TTL to expire, attempt to use it. Verify rejection.
- **Single-use test:** Create a join token, use it to join a node, attempt to use the same token for a second node. Verify rejection.
- **TPM attestation test:** Enable `node_attestation = "tpm"`, attempt to join with a valid token but without a trusted TPM endorsement key. Verify rejection.

### 10.5 Secret Encryption Round-Trip

- **Encrypt/decrypt test:** Encrypt a value with the cluster's public key. Deploy an app referencing the encrypted value. Verify that the workload receives the correct plaintext as an env var.
- **Namespace isolation test:** Encrypt a value with namespace A's public key. Attempt to use it in namespace B's app. Verify decryption failure. Implemented as `a_value_sealed_for_one_namespace_fails_closed_in_another` and `after_opting_in_a_cluster_sealed_value_fails_closed_in_that_namespace` (`src/bun/agent/tests/namespace_secrets.rs`).
- **Key rotation test:** Encrypt values with key generation N. Run `relish secret rotate`. Verify that old ciphertexts still decrypt (old key is read-only). Encrypt new values with generation N+1. Run `relish secret rotate --finalize`. Verify that old ciphertexts no longer decrypt.

### 10.6 CRL Distribution

- **Revocation test:** Join a node, revoke its certificate, attempt communication from the revoked node. Verify rejection on all other nodes.
- **Propagation timing test:** Revoke a certificate and measure the time until all nodes have the updated CRL. Verify sub-2-second propagation for clusters up to 200 nodes.
- **Stale CRL test:** Disconnect a node before CRL distribution, then reconnect. Verify that the node receives the updated CRL on reconnection.

### 10.7 Audit Logging

- **Token audit test:** Use an API token to perform operations. Verify that `relish token list` shows the correct `last_used` timestamp.
- **Secret decryption audit test:** Deploy an app with encrypted secrets. Verify that `relish events --type secret` shows the decryption events with correct app, node, and timestamp.
- **Egress DNS change audit test:** Change the DNS record for a hostname in an egress allowlist. Verify that an audit event is logged when Bun updates the eBPF maps.

---

## 11. Prior Art

### 11.1 Kubernetes

**RBAC and ServiceAccount tokens:** Kubernetes uses Role-Based Access Control with ServiceAccount tokens mounted into pods. Tokens were originally long-lived JWTs (never expired), which was a known security weakness. Kubernetes 1.22+ introduced bound service account tokens with audience, expiry, and object binding. Reliaburger's approach is similar in spirit (short-lived, scoped tokens) but simpler: there is no RBAC policy engine, just three roles (admin, deployer, read-only) with optional app/namespace scoping. This covers the common case without the complexity of Kubernetes' Role/ClusterRole/RoleBinding hierarchy.

**PKI:** Kubernetes uses a single CA by default (though kubeadm supports front-proxy CA and etcd CA separately). The kubelet certificate rotation was added later and requires explicit opt-in. Reliaburger's three-intermediate-CA hierarchy is stricter from day one, and workload certificate rotation is automatic and mandatory.

**NetworkPolicy:** Kubernetes NetworkPolicy requires a CNI plugin that supports it (Calico, Cilium, etc.). Many clusters run without any NetworkPolicy enforcement. The policy model is namespace-scoped and uses label selectors. Reliaburger's approach is fundamentally different: namespace isolation is enforced by default (no configuration required), and declared per-app egress rules use eBPF at the socket level (not packet filtering). Those declared rules are deny-by-default. The cluster-wide default for apps with no rule is still planned, so we must not describe the current system as globally default-deny. There is no separate "policy controller" -- the enforcement is built into Bun and Onion.

**References:**

- [Kubernetes PKI certificates and requirements](https://kubernetes.io/docs/setup/best-practices/certificates/)
- [Kubernetes RBAC documentation](https://kubernetes.io/docs/reference/access-authn-authz/rbac/)
- [Kubernetes NetworkPolicy](https://kubernetes.io/docs/concepts/services-networking/network-policies/)

### 11.2 SPIFFE and SPIRE

SPIFFE (Secure Production Identity Framework for Everyone) defines the identity format: the `spiffe://` URI scheme, the X.509-SVID (certificate with SPIFFE URI in SAN), and the JWT-SVID. SPIRE is the reference implementation providing a server and per-node agent.

Reliaburger uses the SPIFFE identity format for compatibility -- external systems that trust SPIFFE URIs work with Reliaburger identities. However, Reliaburger does not use SPIRE because the problems SPIRE solves are already handled:

- **Workload attestation:** SPIRE inspects container metadata via the kubelet API. Bun started the container and knows its identity directly.
- **Certificate issuance:** SPIRE server is the CA. Reliaburger has a dedicated Workload CA (intermediate), with council signing CSRs.
- **Certificate rotation:** SPIRE agent rotates via SDS API. Bun writes to tmpfs on a 30-minute schedule.
- **Registration:** SPIRE requires workloads to be registered before receiving identity. Reliaburger assigns identity automatically from app configuration.
- **OIDC federation:** SPIRE requires separate OIDC discovery server configuration. Reliaburger builds it into the cluster API.

**What we borrow:** The `spiffe://` URI format, the X.509-SVID certificate structure, the trust domain concept.

**What we do differently:** No separate SPIRE server or agent binary. No registration step. Identity is automatic for every workload.

**References:**

- [SPIFFE specification](https://spiffe.io/docs/latest/spiffe-about/overview/)
- [SPIFFE ID format](https://github.com/spiffe/spiffe/blob/main/standards/SPIFFE-ID.md)
- [SPIRE documentation](https://spiffe.io/docs/latest/spire-about/)

### 11.3 Consul Connect

HashiCorp Consul Connect provides service mesh with mTLS and intention-based authorisation. It uses a built-in CA (or Vault as an external CA) and issues SPIFFE-compatible certificates. Connect requires sidecar proxies (Envoy) for transparent mTLS.

Reliaburger's approach is similar in using built-in CA and SPIFFE identities, but different in that there are no sidecar proxies. Workloads that want mTLS configure it themselves using the identity files at `/var/run/reliaburger/identity/`. Network-level access control is handled by eBPF and nftables, not by a service mesh proxy.

### 11.4 Istio Security Architecture

Istio provides mTLS between services via Envoy sidecar proxies. Citadel (now istiod) is the CA that issues SPIFFE-compatible certificates. Istio uses the SDS (Secret Discovery Service) API for certificate delivery. Authorisation policies use a declarative model similar to Kubernetes NetworkPolicy but richer (L7 attributes, JWT claims, etc.).

Reliaburger borrows the concept of automatic mTLS identity but avoids the sidecar proxy model entirely. The CSR model (worker generates keypair, council signs) is similar to how Citadel operates, but without the SDS API layer -- Bun writes files directly.

**References:**

- [Istio Security Architecture](https://istio.io/latest/docs/concepts/security/)

### 11.5 cert-manager

cert-manager is a Kubernetes add-on that automates certificate lifecycle management. It supports multiple issuers (Let's Encrypt, Vault, self-signed, etc.) and integrates with Kubernetes Secrets and Ingress resources.

Reliaburger's Ingress CA serves a similar purpose to cert-manager's self-signed/CA issuer for internal certificates. Public-facing certificates currently come from an operator-supplied certificate/key pair. Direct ACME (Let's Encrypt) support remains a possible post-v1 feature. The workload identity certificate rotation is analogous to cert-manager's Certificate resources but is fully automatic and requires no CRDs or annotations.

### 11.6 HashiCorp Vault

Vault provides secret management, PKI, and identity. Its PKI secrets engine can issue X.509 certificates. Its transit engine encrypts data. Its auth methods support many identity providers.

Reliaburger's age-based secret encryption is much simpler than Vault but covers the common case (encrypted secrets in git). Reliaburger's PKI is built-in rather than delegated to Vault. For teams that need Vault's advanced features (dynamic secrets, leasing, audit backends), Reliaburger's workload identity certificates can authenticate to Vault via the cert auth method.

**What we borrow:** The concept of short-lived certificates as a substitute for revocation. The asymmetric encryption model for secrets at rest.

**What we do differently:** No separate Vault server. No dynamic secrets or leasing. Secrets are encrypted in git, not fetched from an API at runtime. Built-in CA instead of Vault PKI engine.

---

## 12. Libraries and Dependencies

| Crate | Purpose | Notes |
|-------|---------|-------|
| **rustls** | TLS implementation for all inter-node mTLS and API TLS. | Pure Rust, no OpenSSL dependency. Supports certificate verification callbacks for CRL checking. |
| **rcgen** | X.509 certificate generation and CSR creation. | Used by `relish init` for CA generation and by Bun for creating workload CSRs. |
| **ring** | Cryptographic primitives: ECDSA key generation, AES-256-GCM encryption/decryption, HKDF key derivation, SHA-256 hashing. | The core crypto library. No unsafe code, constant-time operations. |
| **age** | Asymmetric encryption for secrets (`ENC[AGE:...]` values). | Rust implementation of the age encryption format. Used for secret encryption/decryption and root CA key sealing. |
| **x509-parser** | Parsing and validating X.509 certificates and CRLs. | Used for certificate chain validation, CRL parsing, and `relish ca status` output. |
| **pem** | PEM encoding/decoding for certificates and keys. | Used for writing certificate files to workload identity mounts. |
| **argon2** | Password hashing for API token storage. | Argon2id variant. Used for hashing API token secrets before Raft storage. |

---

## 13. Open Questions

### 13.1 OCSP vs CRL

The current design uses CRL (Certificate Revocation List) for node certificate revocation. OCSP (Online Certificate Status Protocol) is an alternative that provides real-time revocation checking.

**Arguments for staying with CRL:**

- The CRL is small (< 100 entries for typical clusters) and distributed proactively via the reporting tree.
- CRL checks are a local hash set lookup with zero network overhead per handshake.
- The cluster already has a distribution mechanism (reporting tree) that delivers the CRL in under 2 seconds.
- OCSP would require a responder service, adding a new dependency and failure mode.

**Arguments for OCSP:**

- Real-time status: no propagation delay at all.
- Standard protocol: external systems could query the OCSP responder.
- OCSP stapling could be used to avoid the responder being a bottleneck.

**Current decision:** CRL. The reporting tree provides near-real-time distribution, and the simplicity of a local hash set lookup outweighs OCSP's marginal latency improvement. Revisit if external systems need real-time revocation checking.

### 13.2 Hardware Key Storage Without TPM

Not all environments have TPM 2.0. The current fallback (HKDF from the node certificate's private key) protects against offline disk access but not against an attacker with both disk and key material.

**Options under consideration:**

- **PKCS#11 / HSM integration:** Support external HSMs (e.g., YubiHSM, AWS CloudHSM) for CA key storage. This is significantly more complex and environment-specific.
- **Software-based sealed storage:** Use a key derived from multiple inputs (node certificate, cluster secret, and a user-provided passphrase) to approximate the binding that TPM provides.
- **SGX/SEV enclaves:** Use hardware enclaves for key operations on supported hardware. This is highly platform-specific.

**Current decision:** TPM is optional. The non-TPM path provides reasonable security for most environments. HSM integration is deferred until there is concrete user demand.

### 13.3 External CA Integration

Some organisations require that all certificates chain to a corporate root CA rather than a self-signed cluster root.

**Options under consideration:**

- **External root CA:** Allow `relish init --external-ca <cert> --external-key <key>` to use an externally provided root CA instead of generating one. The intermediate CAs would still be managed by Reliaburger.
- **External intermediate CA:** Allow the Node CA, Workload CA, or Ingress CA to be externally managed, with the council acting as a registration authority (RA) rather than a CA.
- **ACME for workload certs:** Use ACME protocol to obtain workload certificates from an external CA. This would require the external CA to support SPIFFE URIs in SANs.

**Current decision:** Deferred. The self-contained PKI is simpler to operate and does not depend on external infrastructure availability. External CA integration will be designed when a concrete use case emerges.

### 13.4 Multi-Cluster Trust Federation

When multiple Reliaburger clusters need to communicate, their workloads need to verify each other's identities. Options include cross-signing root CAs, a shared trust bundle, or a federation server.

**Current decision:** Not in scope for v1. Each cluster is a self-contained trust domain. Cross-cluster communication can use the OIDC federation mechanism (each cluster trusts the other's OIDC endpoint) as a near-term workaround.

### 13.5 Certificate Transparency Logging

Should the cluster maintain a certificate transparency (CT) log of all issued certificates for audit purposes?

**Arguments for:** Complete audit trail of every certificate ever issued. Detect rogue certificate issuance.

**Arguments against:** Additional storage and complexity. Short-lived workload certificates (1 hour, rotated every 30 minutes) would generate a very high volume of CT log entries. The CSR validation already ensures certificates match scheduling state.

**Current decision:** Deferred. The CSR validation mechanism provides the integrity guarantee that CT logging would provide (certificates cannot be issued for unscheduled workloads). A lightweight issuance log (serial number, subject, timestamp) may be added without full CT log infrastructure.
