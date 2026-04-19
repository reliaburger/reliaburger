//! Data types for the Sesame security subsystem.
//!
//! All persistent security state — CAs, node certificates, API tokens,
//! age keypairs, join tokens — is defined here. These types are stored
//! in Raft (via serde) and referenced throughout the codebase.

use std::fmt;
use std::time::SystemTime;

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Serial numbers
// ---------------------------------------------------------------------------

/// A certificate serial number. Monotonically increasing, assigned by the CA.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SerialNumber(pub u64);

impl fmt::Display for SerialNumber {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "0x{:02x}", self.0)
    }
}

// ---------------------------------------------------------------------------
// CA hierarchy
// ---------------------------------------------------------------------------

/// Which CA in the hierarchy this represents.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum CaRole {
    Root,
    Node,
    Workload,
    Ingress,
}

impl fmt::Display for CaRole {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CaRole::Root => write!(f, "Root"),
            CaRole::Node => write!(f, "Node"),
            CaRole::Workload => write!(f, "Workload"),
            CaRole::Ingress => write!(f, "Ingress"),
        }
    }
}

/// A private key encrypted with an HKDF-derived wrapping key (AES-256-GCM).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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

/// One CA in the hierarchy (root, node, workload, or ingress).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
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
    /// When this CA certificate was issued.
    pub not_before: SystemTime,
    /// When this CA certificate expires.
    pub not_after: SystemTime,
    /// The parent CA's serial (None for the root CA).
    pub issuer_serial: Option<SerialNumber>,
    /// Generation counter, incremented on `relish ca rotate`.
    pub generation: u64,
}

// ---------------------------------------------------------------------------
// Node certificate
// ---------------------------------------------------------------------------

/// A certificate issued to a cluster node for inter-node mTLS.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NodeCertificate {
    /// The node's unique identifier (used as CN in the certificate).
    pub node_id: String,
    /// DER-encoded X.509 certificate, signed by the Node CA.
    pub certificate_der: Vec<u8>,
    /// DER-encoded private key (stored on the node, not in Raft).
    #[serde(skip)]
    pub private_key_der: Vec<u8>,
    /// Serial number assigned by the Node CA.
    pub serial: SerialNumber,
    /// Certificate validity start.
    pub not_before: SystemTime,
    /// Certificate validity end (default: 1 year from issuance).
    pub not_after: SystemTime,
    /// The Node CA generation that signed this certificate.
    pub ca_generation: u64,
}

// ---------------------------------------------------------------------------
// SPIFFE identity
// ---------------------------------------------------------------------------

/// A SPIFFE URI identifying a workload.
/// Format: `spiffe://CLUSTER/ns/NAMESPACE/app/APP_NAME`
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct SpiffeUri {
    /// The trust domain (cluster name).
    pub trust_domain: String,
    /// The namespace containing the workload.
    pub namespace: String,
    /// The workload type (app or job).
    pub workload_type: WorkloadType,
    /// The workload name.
    pub name: String,
}

impl SpiffeUri {
    /// Renders the full URI string.
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

impl fmt::Display for SpiffeUri {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.to_uri())
    }
}

/// Whether the workload is a long-running app or a batch job.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum WorkloadType {
    App,
    Job,
}

// ---------------------------------------------------------------------------
// Workload identity
// ---------------------------------------------------------------------------

/// The full identity bundle for a running workload instance.
///
/// Lives in worker memory and on the workload's tmpfs mount — never
/// serialised to Raft. The private key must not leave the worker node.
#[derive(Debug, Clone)]
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

// ---------------------------------------------------------------------------
// OIDC signing configuration
// ---------------------------------------------------------------------------

/// OIDC signing configuration for workload identity JWTs.
///
/// Stored in Raft as part of `SecurityState`. The Ed25519 private key
/// is wrapped with the same HKDF mechanism used for CA private keys.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct OidcSigningConfig {
    /// Ed25519 private key for signing JWTs, wrapped with AES-256-GCM.
    pub signing_key_wrapped: WrappedKey,
    /// Ed25519 public key bytes (published via JWKS endpoint).
    pub public_key_der: Vec<u8>,
    /// Key ID for the JWKS entry.
    pub key_id: String,
    /// The issuer URL (e.g., "https://prod.reliaburger.dev").
    pub issuer: String,
}

/// Claims embedded in a workload identity JWT.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
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
    /// Workload namespace.
    #[serde(rename = "reliaburger.dev/namespace")]
    pub namespace: String,
    /// Workload app or job name.
    #[serde(rename = "reliaburger.dev/app")]
    pub app: String,
    /// Cluster name (trust domain).
    #[serde(rename = "reliaburger.dev/cluster")]
    pub cluster: String,
    /// Node ID where the workload runs.
    #[serde(rename = "reliaburger.dev/node")]
    pub node: String,
    /// Instance ID within the app.
    #[serde(rename = "reliaburger.dev/instance")]
    pub instance: String,
}

// ---------------------------------------------------------------------------
// API tokens
// ---------------------------------------------------------------------------

/// The role granted to an API token.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ApiRole {
    /// Full access: deploy, stop, create tokens, manage secrets.
    Admin,
    /// Deploy and stop apps, view status.
    Deployer,
    /// View status, logs, resolve only.
    ReadOnly,
}

impl fmt::Display for ApiRole {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ApiRole::Admin => write!(f, "admin"),
            ApiRole::Deployer => write!(f, "deployer"),
            ApiRole::ReadOnly => write!(f, "read-only"),
        }
    }
}

/// Scope restrictions on an API token.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct TokenScope {
    /// If set, token can only act on these app names.
    pub apps: Option<Vec<String>>,
    /// If set, token can only act within these namespaces.
    pub namespaces: Option<Vec<String>>,
}

impl TokenScope {
    /// Returns true if this scope allows the given app in the given namespace.
    pub fn allows(&self, app: &str, namespace: &str) -> bool {
        let app_ok = self
            .apps
            .as_ref()
            .is_none_or(|apps| apps.iter().any(|a| a == app));
        let ns_ok = self
            .namespaces
            .as_ref()
            .is_none_or(|nss| nss.iter().any(|n| n == namespace));
        app_ok && ns_ok
    }
}

/// An API token for human or CI access to the cluster.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ApiToken {
    /// Human-readable name for the token.
    pub name: String,
    /// Argon2id hash of the token secret.
    pub token_hash: Vec<u8>,
    /// Salt used for hashing.
    pub token_salt: Vec<u8>,
    /// The role granted to this token.
    pub role: ApiRole,
    /// Optional scope restrictions.
    pub scope: TokenScope,
    /// When the token expires.
    pub expires_at: Option<SystemTime>,
    /// When the token was created.
    pub created_at: SystemTime,
}

// ---------------------------------------------------------------------------
// Age keypair (secret encryption)
// ---------------------------------------------------------------------------

/// The scope of an age keypair.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum AgeKeyScope {
    /// Cluster-wide default keypair.
    ClusterWide,
    /// Namespace-scoped keypair.
    Namespace(String),
}

/// An age keypair used for encrypting/decrypting secrets.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AgeKeypair {
    /// The scope of this keypair.
    pub scope: AgeKeyScope,
    /// The age public key (safe to distribute).
    pub public_key: String,
    /// The age private key, wrapped with HKDF-derived key.
    pub private_key_wrapped: WrappedKey,
    /// Generation counter, incremented on `relish secret rotate`.
    pub generation: u64,
    /// Whether this key is read-only (old generation, kept for decryption
    /// of not-yet-re-encrypted secrets during rotation).
    #[serde(default)]
    pub read_only: bool,
}

// ---------------------------------------------------------------------------
// Join token
// ---------------------------------------------------------------------------

/// Node attestation mode required for joining.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum AttestationMode {
    /// No additional attestation beyond the join token.
    None,
    /// Require TPM 2.0 attestation quote during join.
    Tpm,
    /// Require a pre-issued client certificate from an external CA.
    Certificate,
}

/// A one-time-use join token for adding a node to the cluster.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JoinToken {
    /// SHA-256 hash of the token value (plaintext is never stored).
    pub token_hash: [u8; 32],
    /// When the token expires (default: 15 minutes from creation).
    pub expires_at: SystemTime,
    /// Whether the token has been consumed.
    pub consumed: bool,
    /// Node attestation mode required for this token.
    pub attestation_mode: AttestationMode,
}

// ---------------------------------------------------------------------------
// Certificate revocation list
// ---------------------------------------------------------------------------

/// The cluster's certificate revocation list, distributed via gossip.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Crl {
    /// Revoked certificate entries.
    pub entries: Vec<CrlEntry>,
    /// Monotonically increasing version, incremented on every update.
    pub version: u64,
    /// When this CRL was last updated.
    pub updated_at: SystemTime,
}

impl Default for Crl {
    fn default() -> Self {
        Self {
            entries: Vec::new(),
            version: 0,
            updated_at: SystemTime::UNIX_EPOCH,
        }
    }
}

/// A single revoked certificate.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CrlEntry {
    /// Serial number of the revoked certificate.
    pub serial: SerialNumber,
    /// Which CA issued the revoked certificate.
    pub issuer: CaRole,
    /// When the certificate was revoked.
    pub revoked_at: SystemTime,
    /// Human-readable reason (e.g. "node-07 compromised").
    pub reason: String,
}

// ---------------------------------------------------------------------------
// Cluster security state
// ---------------------------------------------------------------------------

/// The full security state stored in Raft.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct SecurityState {
    /// The CA hierarchy (root + intermediates).
    pub certificate_authorities: Vec<CertificateAuthority>,
    /// Age keypairs for secret encryption.
    pub age_keypairs: Vec<AgeKeypair>,
    /// API tokens (hashed).
    pub api_tokens: Vec<ApiToken>,
    /// Outstanding join tokens.
    pub join_tokens: Vec<JoinToken>,
    /// Next serial number to assign.
    pub next_serial: u64,
    /// OIDC signing configuration for workload identity JWTs.
    #[serde(default)]
    pub oidc_signing_config: Option<OidcSigningConfig>,
    /// Certificate revocation list.
    #[serde(default)]
    pub crl: Crl,
}

impl SecurityState {
    /// Get the CA for a given role.
    pub fn get_ca(&self, role: CaRole) -> Option<&CertificateAuthority> {
        self.certificate_authorities
            .iter()
            .find(|ca| ca.role == role)
    }

    /// Get the cluster-wide age keypair.
    pub fn cluster_age_keypair(&self) -> Option<&AgeKeypair> {
        self.age_keypairs
            .iter()
            .find(|kp| kp.scope == AgeKeyScope::ClusterWide)
    }

    /// Get a namespace-scoped age keypair.
    pub fn namespace_age_keypair(&self, namespace: &str) -> Option<&AgeKeypair> {
        self.age_keypairs
            .iter()
            .find(|kp| kp.scope == AgeKeyScope::Namespace(namespace.to_string()))
    }

    /// Allocate the next serial number.
    pub fn next_serial(&mut self) -> SerialNumber {
        let serial = SerialNumber(self.next_serial);
        self.next_serial += 1;
        serial
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serial_number_display_hex() {
        assert_eq!(SerialNumber(1).to_string(), "0x01");
        assert_eq!(SerialNumber(255).to_string(), "0xff");
        assert_eq!(SerialNumber(4096).to_string(), "0x1000");
    }

    #[test]
    fn ca_role_display() {
        assert_eq!(CaRole::Root.to_string(), "Root");
        assert_eq!(CaRole::Node.to_string(), "Node");
    }

    #[test]
    fn spiffe_uri_format() {
        let uri = SpiffeUri {
            trust_domain: "prod".to_string(),
            namespace: "default".to_string(),
            workload_type: WorkloadType::App,
            name: "api".to_string(),
        };
        assert_eq!(uri.to_uri(), "spiffe://prod/ns/default/app/api");
    }

    #[test]
    fn spiffe_uri_job_format() {
        let uri = SpiffeUri {
            trust_domain: "staging".to_string(),
            namespace: "batch".to_string(),
            workload_type: WorkloadType::Job,
            name: "migrate".to_string(),
        };
        assert_eq!(uri.to_uri(), "spiffe://staging/ns/batch/job/migrate");
    }

    #[test]
    fn token_scope_allows_unrestricted() {
        let scope = TokenScope::default();
        assert!(scope.allows("any-app", "any-namespace"));
    }

    #[test]
    fn token_scope_restricts_by_app() {
        let scope = TokenScope {
            apps: Some(vec!["api".to_string(), "web".to_string()]),
            namespaces: None,
        };
        assert!(scope.allows("api", "production"));
        assert!(!scope.allows("database", "production"));
    }

    #[test]
    fn token_scope_restricts_by_namespace() {
        let scope = TokenScope {
            apps: None,
            namespaces: Some(vec!["production".to_string()]),
        };
        assert!(scope.allows("api", "production"));
        assert!(!scope.allows("api", "staging"));
    }

    #[test]
    fn security_state_next_serial_increments() {
        let mut state = SecurityState::default();
        assert_eq!(state.next_serial(), SerialNumber(0));
        assert_eq!(state.next_serial(), SerialNumber(1));
        assert_eq!(state.next_serial(), SerialNumber(2));
    }

    #[test]
    fn jwt_claims_serde_custom_field_names() {
        let claims = WorkloadJwtClaims {
            iss: "https://prod.reliaburger.dev".to_string(),
            sub: "spiffe://prod/ns/default/app/api".to_string(),
            aud: vec!["spiffe://prod".to_string()],
            exp: 1700000000,
            iat: 1699996400,
            namespace: "default".to_string(),
            app: "api".to_string(),
            cluster: "prod".to_string(),
            node: "node-01".to_string(),
            instance: "api-g1234-0".to_string(),
        };
        let json = serde_json::to_string(&claims).unwrap();
        assert!(json.contains("\"reliaburger.dev/namespace\":\"default\""));
        assert!(json.contains("\"reliaburger.dev/app\":\"api\""));
        assert!(json.contains("\"reliaburger.dev/cluster\":\"prod\""));
        assert!(json.contains("\"reliaburger.dev/node\":\"node-01\""));
        assert!(json.contains("\"reliaburger.dev/instance\":\"api-g1234-0\""));

        // Round-trip
        let decoded: WorkloadJwtClaims = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, claims);
    }

    #[test]
    fn security_state_default_has_no_oidc_config() {
        let state = SecurityState::default();
        assert!(state.oidc_signing_config.is_none());
    }
}
