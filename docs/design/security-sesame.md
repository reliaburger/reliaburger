# Sesame -- Security, PKI & Identity

## 1. Overview

Sesame is Reliaburger's built-in PKI, identity, and security layer. It provides the cryptographic foundation that every other subsystem depends on: mutual TLS between cluster nodes, workload identity certificates for every app and job, API authentication for human operators and CI systems, secret encryption at rest, Raft log encryption, and network-level firewalling.

Sesame is not a separate binary or sidecar. It is compiled into the single `reliaburger` binary and activated during `relish init`. Every security primitive -- certificate authorities, certificate signing, token management, secret encryption, firewall rule generation -- is handled by code paths within the existing node roles (council and worker).

**Core responsibilities:**

- **CA hierarchy.** A root CA and three intermediate CAs (Node, Workload, Ingress), each scoped to a single purpose.
- **Node authentication.** Join tokens, mTLS certificate issuance, optional TPM attestation.
- **Workload identity.** SPIFFE-compatible X.509 certificates and OIDC JWTs for every workload, issued via a CSR model, rotated automatically.
- **API authentication.** Scoped tokens with roles, TTLs, and rate limits. Optional OIDC integration with external identity providers.
- **Secret encryption.** Asymmetric age encryption for secrets checked into git. Namespace-scoped keypairs for multi-tenant isolation.
- **Data at rest encryption.** AES-256-GCM encryption of the Raft log, with HKDF key derivation and optional TPM sealing.
- **Network security.** nftables perimeter firewall, eBPF inter-app firewall, egress allowlists, namespace isolation -- all managed automatically by Bun.
- **Certificate revocation.** CRL distribution for long-lived node certificates.

**Design principles:**

1. **Zero-configuration security.** A fresh cluster has mTLS between all nodes, workload identity for all apps, namespace isolation, deny-by-default egress, and encrypted Raft logs -- with no operator configuration.
2. **Separation of privilege.** Worker nodes never hold CA private keys. They can only obtain certificates for workloads they are scheduled to run.
3. **Data plane survives control plane failures.** Existing certificates, firewall rules, and secrets continue working during council outages. Grace period extensions prevent hard cliffs.
4. **Short-lived credentials by default.** Workload certificates live 1 hour, rotated every 30 minutes. API tokens default to 90-day TTL. Short lifetimes reduce the blast radius of credential theft.

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
| **Wrapper** (ingress) | Consumes Ingress CA certificates for `tls = "cluster"` routes. Triggers certificate renewal through Bun. |
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

The root CA private key is used **only** during `relish init` and `relish ca rotate --root`. After signing the three intermediates, the root private key is encrypted with the cluster's age public key, written to a sealed backup file on the admin's machine, and deleted from all cluster nodes. No cluster node holds the root CA private key during normal operation.

All three intermediate CAs chain to the same root, so a single trust anchor (the root CA certificate) is sufficient for any verifier.

### 3.2 Key Distribution Model

```
+-------------------------------------------------------------------+
|                        Council Nodes                               |
|                                                                    |
|  Raft Log (encrypted at rest with AES-256-GCM)                    |
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
|  NO CA private keys. NO age private key. NO OIDC signing key.     |
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

    /// Generation counter, incremented on `relish ca rotate`.
    pub generation: u64,
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
    pub not_after: SystemTime,  // default: 1 year from issuance

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

    /// When the token expires. Default: 90 days from creation.
    pub expires_at: SystemTime,

    /// When the token was created.
    pub created_at: SystemTime,

    /// Last time the token was used (updated on each API request).
    pub last_used: Option<SystemTime>,

    /// Per-token rate limit (requests per second). Default: 100.
    pub rate_limit_rps: u32,

    /// If this token is being rotated, the old token hash that is still
    /// valid during the grace period.
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

/// Grace period state during token rotation.
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

    /// Namespace-scoped keypair (opt-in via `secret_key = true`).
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

    /// Admin CIDR ranges allowed to access management ports.
    pub admin_cidrs: Vec<IpNet>,

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

    /// Audience: always includes "spiffe://CLUSTER", plus any per-app audiences.
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
14. Generate a one-time join token and output it to stderr only.
15. Start accepting connections with mTLS.

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

  Join token (valid 15 minutes, single use):
    rbrg_join_1_a7f3b9c2e1d4...
```

### 5.2 Node Join (`relish join`)

1. The joining node connects to the specified cluster node over TLS (server-authenticated only at this stage, using the root CA certificate that was provided alongside the join token or downloaded via a trust-on-first-use pinning step).
2. The joining node presents the join token.
3. The cluster node forwards the token to the council leader via Raft.
4. The leader validates: token is not expired, token is not already consumed, marks the token as consumed via a Raft write.
5. If `node_attestation = "tpm"`: the joining node presents a TPM attestation quote. The leader verifies it against the pre-registered endorsement keys.
6. If `node_attestation = "certificate"`: the joining node presents a client certificate. The leader verifies it against the configured external CA trust store.
7. The leader generates a node certificate signed by the Node CA (1-year validity) with the node's unique ID as the CN.
8. The signed certificate, the Node CA certificate, the Workload CA certificate chain, and the root CA certificate are sent to the joining node.
9. The joining node stores the certificate and keys, enables mTLS, and begins participating in Mustard gossip.

### 5.3 Workload Certificate Rotation

Bun on each worker node maintains a rotation schedule for every running workload:

1. **Initial issuance.** When Bun starts a workload, it immediately generates a keypair and sends a CSR to its nearest council parent.
2. **Validation.** The council member checks that the requesting node (identified by its mTLS node certificate CN) is scheduled to run the workload (verified against Meat's scheduling state).
3. **Signing.** The council member signs the certificate with the Workload CA, sets a 1-hour lifetime, and returns it.
4. **Writing.** Bun writes `cert.pem`, `key.pem`, `ca.pem`, `bundle.pem`, and `token` (OIDC JWT) to the workload's tmpfs mount at `/var/run/reliaburger/identity/`.
5. **Rotation timer.** Bun schedules the next rotation for 30 minutes later (half the certificate lifetime).
6. **Pre-fetch.** At the 30-minute mark, Bun generates a new keypair, sends a new CSR, receives the new certificate, and atomically writes the new files to the tmpfs mount.
7. **Grace period.** If the CSR fails because the council is unreachable, Bun keeps the current certificate and extends its local validity window by up to 4 additional hours (configurable). The extension is logged as a security event, `relish wtf` warns about it, and an alert fires.

**OIDC JWT minting** happens at the same time as certificate issuance. The council member constructs the JWT with the workload's SPIFFE URI as the `sub` claim, the cluster's OIDC issuer as `iss`, default audience `spiffe://CLUSTER`, plus any per-app audiences from the `[app.NAME.identity]` config. The JWT is signed with the Ed25519 OIDC signing key and returned alongside the signed X.509 certificate.

### 5.4 API Token Lifecycle

**Creation:**

```bash
$ relish token create --name ci-deploy --role deployer \
    --apps "web,api" --namespaces "production" --ttl 90d
```

1. Generate a 256-bit cryptographically random token secret.
2. Hash the secret with Argon2id (salt generated per-token).
3. Store the hash, salt, role, scope, TTL, and rate limit in Raft.
4. Return the plaintext token to the user. It is never stored in plaintext.

**Rotation:**

```bash
$ relish token rotate ci-deploy
```

1. Generate a new token secret and hash.
2. Store the new hash alongside the old hash with a grace period expiry (default 24 hours).
3. During the grace period, both old and new tokens are accepted.
4. After the grace period, the old hash is deleted.

**Expiry:** Expired tokens are automatically revoked. A background sweep on the council leader checks for expired tokens every hour and removes them from Raft state.

**Rate limiting:** Each API request checks the token's `rate_limit_rps`. A token-keyed sliding window counter (in-memory on the API-serving node) tracks request counts. Exceeding the limit returns HTTP 429 with a `Retry-After` header.

### 5.5 Secret Encrypt/Decrypt

**Encryption (client-side, offline):**

```bash
$ relish secret encrypt --pubkey age1qy8m5kz... "my-secret-value"
ENC[AGE:YWdlLWVuY3J5cHRpb24...]
```

> **Status:** `relish secret pubkey` and `relish secret encrypt` are implemented. `relish secret rotate` requires SecurityState in Raft (same dependency as token list/revoke) and is deferred.

The `relish` CLI uses the age public key to encrypt. No cluster access required. The ciphertext is embedded in the TOML app configuration and checked into git.

**Decryption (Bun, at workload start):**

1. Bun reads the app configuration (from Lettuce/git or direct deploy).
2. For each env var value matching `ENC[AGE:...]`, Bun requests decryption from the council.
3. The council decrypts using the age private key (cluster-wide or namespace-scoped).
4. The plaintext is returned over the mTLS channel.
5. Bun injects the plaintext as an environment variable. It is never written to disk.
6. A decryption audit event is logged: which secret, which app, which node, timestamp.

**Namespace-scoped keys:** When `secret_key = true` is set for a namespace, `relish init` (or `relish namespace create`) generates a separate age keypair for that namespace. The private key is stored in Raft, wrapped with HKDF. Bun on nodes running workloads in that namespace can request decryption only for secrets encrypted with that namespace's key. Compromise of one namespace's key does not expose other namespaces' secrets.

**Key rotation (`relish secret rotate`):**

1. Generate a new age keypair.
2. Store the new keypair in Raft, marking the old keypair as `read_only = true`.
3. The cluster now accepts ciphertexts encrypted with either key.
4. The operator (or CI) re-encrypts all secrets with the new public key and commits to git.
5. Once all `ENC[AGE:...]` values use the new key, the operator runs `relish secret rotate --finalize` to delete the old keypair.

### 5.6 Raft Log Encryption

On each council node:

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

**Intermediate CA rotation (`relish ca rotate`):**

1. Generate a new intermediate CA keypair (for whichever CA is being rotated, or all three).
2. Sign the new intermediate with the root CA. (The root CA private key is needed only for this step; it is decrypted from the sealed backup provided by the operator.)
3. Store the new intermediate CA in Raft alongside the old one.
4. **Dual-signing period begins:** both old and new intermediate CAs are trusted. The new intermediate is used for all new certificate issuance.
5. Over time, all existing certificates expire and are re-issued under the new intermediate.
6. Once all certificates issued by the old intermediate have expired (or been re-issued), the old intermediate is revoked and removed.

**Root CA rotation (`relish ca rotate --root`):**

1. The operator provides the sealed root CA backup file.
2. The old root CA key is decrypted.
3. A new root CA keypair is generated.
4. New intermediate CAs are generated and signed by the new root.
5. The old root cross-signs the new root (creating a cross-certificate for transition).
6. During the transition period, both old and new root CAs are trusted.
7. The new root CA key is sealed and backed up. The old root CA key is discarded.

### 5.9 Egress Allowlist DNS Resolution

When Bun processes an app's `[app.NAME.egress]` block:

1. For each hostname in the `allow` list, Bun resolves it via multiple upstream DNS servers.
2. Bun compares responses across DNS servers. If responses diverge, a security event is logged (possible DNS poisoning).
3. The resolved IP addresses are inserted into the per-app nftables set (e.g., `egress_api`).
4. Bun refreshes DNS resolution every 10 seconds.
5. When a hostname's resolved IP changes, the nftables set is updated and an audit event is logged.
6. CIDR-based entries are inserted directly without DNS resolution.

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
# Default TTL for new tokens. Default: 90 days.
default_ttl = "90d"

# Default rate limit (requests per second) for new tokens. Default: 100.
default_rate_limit = 100

# Grace period during token rotation. Default: 24 hours.
rotation_grace_period = "24h"
```

### 6.4 Secret Encryption

```toml
# Namespace-scoped secret keys (opt-in per namespace).
[namespace.team-payments]
secret_key = true    # generate a separate age keypair for this namespace
```

### 6.5 Network Security

```toml
[cluster]
# Default egress policy for apps without an [egress] block.
# Options: "deny" (default, recommended), "allow" (escape hatch for migration).
default_egress = "deny"

# Admin CIDR ranges allowed to access management ports.
# These are added to the admin_cidrs nftables set.
admin_cidrs = ["10.0.0.0/8", "192.168.1.0/24"]
```

```toml
# Per-app egress allowlist.
[app.api.egress]
allow = [
    "*.amazonaws.com:443",
    "api.stripe.com:443",
    "db.example.com:5432",
]

# Per-app inbound firewall (eBPF layer).
[app.payment-service.firewall]
allow_from = ["app.api", "app.admin"]
```

### 6.6 OIDC Configuration

```toml
[security.oidc]
# The OIDC issuer URL published in the discovery document.
# Default: derived from the cluster's API endpoint.
issuer = "https://reliaburger.prod.example.com"

# Per-app audience configuration.
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

1. Immediately run `relish ca rotate` for the compromised CA.
2. Revoke all certificates issued by the compromised CA.
3. The dual-signing period ensures existing legitimate certificates continue working during the transition.
4. Investigate how the key was exfiltrated. Council nodes should have restricted access, TPM sealing, and encrypted Raft logs.

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
- **Semi-trusted:** Council nodes. They hold sensitive key material but are hardened (minimal attack surface, TPM sealing, encrypted Raft logs).
- **Untrusted:** Worker nodes. They may be compromised. The system is designed so that a compromised worker node has limited blast radius.
- **Untrusted:** Network between nodes. All inter-node communication is mTLS-authenticated and encrypted.
- **Untrusted:** External network. Default-deny egress, nftables perimeter rules, Wrapper-only ingress.

### 8.2 Compromised Worker Node

**What the attacker gains:**

- The node's own node certificate and private key (can impersonate this specific node).
- Workload certificates for workloads currently running on this node (1-hour lifetime).
- Plaintext secret values for workloads running on this node (in-memory only, not on disk).
- The ability to send CSRs for workloads scheduled on this node.

**What the attacker cannot do:**

- Obtain certificates for workloads on other nodes (CSR validation checks Meat's scheduling state).
- Forge certificates for arbitrary workloads (no CA private keys on worker nodes).
- Decrypt secrets for workloads in other namespaces (namespace-scoped keys, decryption happens on council).
- Access the age private key, CA private keys, or OIDC signing key (stored only on council nodes).
- Modify the Raft log or cluster state (requires council consensus).
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
2. Rotate all intermediate CAs: `relish ca rotate`.
3. Rotate the age keypair: `relish secret rotate`.
4. Rotate the OIDC signing keypair (re-minting all JWTs).
5. Rotate all API tokens.
6. Audit all cluster activity during the compromise window.
7. Investigate the attack vector and harden council node access.

**Prevention:**

- Council nodes should have the smallest possible attack surface.
- TPM sealing ensures key material cannot be extracted even with disk access.
- Encrypted Raft logs protect against offline forensic access.
- Council nodes should be on a restricted management network.

### 8.4 Join Token Theft

See Section 7.3. The short TTL (15 minutes), single-use property, and optional TPM attestation limit the blast radius. The token is the only shared-secret moment in the entire cluster lifecycle.

### 8.5 DNS Poisoning of Egress Allowlists

**Scenario:** An attacker poisons DNS responses for a hostname in an app's egress allowlist, causing Bun to add a malicious IP to the nftables set.

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
- The eBPF programs are attached at the cgroup level (`BPF_CGROUP_INET4_CONNECT`, `BPF_CGROUP_UDP4_SENDMSG`), which intercepts all socket operations regardless of application behaviour.
- Raw socket creation requires `CAP_NET_RAW`, which is dropped from the container's capability set.
- The nftables perimeter layer provides defense-in-depth: even if a workload bypasses eBPF, the nftables rules on the destination node enforce cluster perimeter policy.

### 8.7 Confused Deputy (OIDC Token Replay)

**Scenario:** A JWT token intended for one service is intercepted and replayed against a different service.

**Mitigation:** Every JWT includes a default audience of `spiffe://CLUSTER_NAME`. The verifying service must check that it is the intended audience in the `aud` claim. For cloud IAM federation, per-app audiences (e.g., `sts.amazonaws.com`) are explicitly configured, and the cloud provider validates the audience before issuing temporary credentials.

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

- **nftables perimeter test:** From outside the cluster, attempt to connect to management ports and app ports. Verify that connections are rejected unless originating from `admin_cidrs` or cluster nodes.
- **eBPF firewall test:** Deploy two apps in the same namespace with `allow_from` restrictions. Verify that unauthorized apps receive `ECONNREFUSED`. Verify that authorised apps connect successfully. Verify that apps in different namespaces cannot communicate without explicit cross-namespace rules.
- **Egress allowlist test:** Deploy an app with an `egress` block. Verify that connections to allowed destinations succeed and connections to disallowed destinations are dropped. Verify DNS resolution refresh by changing the DNS record and confirming the nftables set updates within 10 seconds.
- **`relish firewall test` integration:** Verify that the `--from` / `--to` diagnostic command accurately reports whether a connection would be permitted.

### 10.4 Join Token Security

- **Expiry test:** Create a join token, wait for the TTL to expire, attempt to use it. Verify rejection.
- **Single-use test:** Create a join token, use it to join a node, attempt to use the same token for a second node. Verify rejection.
- **TPM attestation test:** Enable `node_attestation = "tpm"`, attempt to join with a valid token but without a trusted TPM endorsement key. Verify rejection.

### 10.5 Secret Encryption Round-Trip

- **Encrypt/decrypt test:** Encrypt a value with the cluster's public key. Deploy an app referencing the encrypted value. Verify that the workload receives the correct plaintext as an env var.
- **Namespace isolation test:** Encrypt a value with namespace A's public key. Attempt to use it in namespace B's app. Verify decryption failure.
- **Key rotation test:** Encrypt values with key generation N. Run `relish secret rotate`. Verify that old ciphertexts still decrypt (old key is read-only). Encrypt new values with generation N+1. Run `relish secret rotate --finalize`. Verify that old ciphertexts no longer decrypt.

### 10.6 CRL Distribution

- **Revocation test:** Join a node, revoke its certificate, attempt communication from the revoked node. Verify rejection on all other nodes.
- **Propagation timing test:** Revoke a certificate and measure the time until all nodes have the updated CRL. Verify sub-2-second propagation for clusters up to 200 nodes.
- **Stale CRL test:** Disconnect a node before CRL distribution, then reconnect. Verify that the node receives the updated CRL on reconnection.

### 10.7 Audit Logging

- **Token audit test:** Use an API token to perform operations. Verify that `relish token list` shows the correct `last_used` timestamp.
- **Secret decryption audit test:** Deploy an app with encrypted secrets. Verify that `relish events --type secret` shows the decryption events with correct app, node, and timestamp.
- **Egress DNS change audit test:** Change the DNS record for a hostname in an egress allowlist. Verify that an audit event is logged when Bun updates the nftables set.

---

## 11. Prior Art

### 11.1 Kubernetes

**RBAC and ServiceAccount tokens:** Kubernetes uses Role-Based Access Control with ServiceAccount tokens mounted into pods. Tokens were originally long-lived JWTs (never expired), which was a known security weakness. Kubernetes 1.22+ introduced bound service account tokens with audience, expiry, and object binding. Reliaburger's approach is similar in spirit (short-lived, scoped tokens) but simpler: there is no RBAC policy engine, just three roles (admin, deployer, read-only) with optional app/namespace scoping. This covers the common case without the complexity of Kubernetes' Role/ClusterRole/RoleBinding hierarchy.

**PKI:** Kubernetes uses a single CA by default (though kubeadm supports front-proxy CA and etcd CA separately). The kubelet certificate rotation was added later and requires explicit opt-in. Reliaburger's three-intermediate-CA hierarchy is stricter from day one, and workload certificate rotation is automatic and mandatory.

**NetworkPolicy:** Kubernetes NetworkPolicy requires a CNI plugin that supports it (Calico, Cilium, etc.). Many clusters run without any NetworkPolicy enforcement. The policy model is namespace-scoped and uses label selectors. Reliaburger's approach is fundamentally different: namespace isolation is enforced by default (no configuration required), per-app rules use eBPF at the socket level (not packet filtering), and egress is deny-by-default. There is no separate "policy controller" -- the enforcement is built into Bun and Onion.

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

Reliaburger's Ingress CA serves a similar purpose to cert-manager's self-signed/CA issuer for internal certificates. For public-facing TLS, Wrapper supports ACME (Let's Encrypt) directly. The workload identity certificate rotation is analogous to cert-manager's Certificate resources but is fully automatic and requires no CRDs or annotations.

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
| **jsonwebtoken** | JWT creation and validation for OIDC workload identity tokens. | Used for minting JWTs on council nodes and validating JWTs from external OIDC providers. |
| **webpki** | Certificate chain validation and trust anchor management. | Used alongside rustls for verifying certificate chains against the root CA trust anchor. |
| **pem** | PEM encoding/decoding for certificates and keys. | Used for writing certificate files to workload identity mounts. |
| **argon2** | Password hashing for API token storage. | Argon2id variant. Used for hashing API token secrets before Raft storage. |
| **tss-esapi** | TPM 2.0 API bindings (optional, for TPM attestation and key sealing). | Conditional compilation: only included when `tpm` feature is enabled. |
| **nftables** (nftnl-rs or nft crate) | Programmatic nftables rule management. | Used by Bun to manage the `reliaburger` nftables table, sets, and chains. |

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
