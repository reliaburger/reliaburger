# Trust No One (Until They Prove It)

In Phase 3 we got our nodes talking. They gossip, they elect leaders, they route traffic. One problem, though: anybody who can reach port 9117 can deploy whatever they like. The gossip protocol trusts every datagram. The Raft log sits on disk in plaintext. The cluster has no idea who's who.

This chapter fixes all of that.

## What we're building

Sesame is Reliaburger's built-in security layer. It's not a sidecar, not a separate binary, not something you bolt on afterwards. It's compiled into the same `bun` binary that runs your containers, and it activates the moment you run `relish init`.

By the end of this chapter, a fresh Reliaburger cluster will have:

- **A CA hierarchy** (root + three intermediate CAs) for signing all certificates
- **Mutual TLS** between every pair of cluster nodes
- **Join tokens** so new nodes prove their right to join
- **API authentication** with scoped, time-limited tokens
- **Secret encryption** so `ENC[AGE:...]` values in your config are decrypted only at runtime
- **Raft log encryption** at rest, so even physical disk access doesn't reveal cluster state
- **eBPF firewall rules** enforcing which apps can talk to which

Zero configuration required. You get all of it by default.

## The CA hierarchy

Every TLS connection needs certificates, and certificates need a Certificate Authority to sign them. We could use Let's Encrypt or some external CA, but that would mean the cluster can't function without internet access and an external dependency. We want the cluster to be self-contained.

So we build our own PKI. The hierarchy looks like this:

```
Root CA (offline after init)
|
+-- Node CA       signs inter-node mTLS certificates
|
+-- Workload CA   signs SPIFFE workload identity certificates
|
+-- Ingress CA    signs TLS certificates for ingress routes
```

The root CA only exists during `relish init`. It signs the three intermediate CAs, then its private key gets encrypted with `age` and written to a backup file. After that, no cluster node holds the root key.

Each intermediate CA has a specific, narrow purpose. The Node CA can only sign node certificates. The Workload CA can only sign workload identity certificates. This separation means that if an attacker compromises a worker node (which never holds any CA keys), they can't forge certificates for other nodes or workloads.

### ECDSA P-256

We use ECDSA P-256 for all certificates. It's fast, produces small signatures, and every TLS implementation supports it. The `rcgen` crate generates the certificates, and `ring` handles the cryptographic operations underneath.

```rust
let key_pair = KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256)?;
```

One line. That's the entire key generation. Rust's type system does the rest — `KeyPair` can only be used in ways that `rcgen` considers safe.

### Key wrapping

The intermediate CA private keys live in the Raft log (so all council members can sign certificates). But you don't want private keys sitting in plaintext, even in consensus state. So we wrap them.

Wrapping means deriving an encryption key via HKDF-SHA256 from a master secret, then encrypting the private key with AES-256-GCM. The wrapped key includes the HKDF salt and nonce — everything a council member needs to unwrap it.

```rust
pub struct WrappedKey {
    pub ciphertext: Vec<u8>,
    pub nonce: [u8; 12],
    pub hkdf_salt: [u8; 32],
    pub hkdf_info: String,
}
```

The `hkdf_info` field binds the derived key to a specific purpose. A key derived with info `"reliaburger-node-ca-wrap-v1"` can't accidentally decrypt something meant for `"reliaburger-workload-ca-wrap-v1"`. This is defence in depth — even if someone gets hold of the master secret, they still need to know which purpose string to use.

## Cluster initialisation

`relish init` is the single most security-critical operation. It generates all root key material in one go.

```bash
$ relish init --cluster-name prod --node-id node-01
```

Under the hood:

1. Generate a 256-bit master secret (for HKDF key wrapping)
2. Generate Root CA (ECDSA P-256, self-signed, 10-year lifetime)
3. Generate Node CA, Workload CA, Ingress CA (each signed by Root, 5-year lifetime)
4. Generate an `age` keypair for secret encryption
5. Wrap all intermediate CA private keys with the master secret
6. Issue the first node's certificate (signed by Node CA, 1-year lifetime)
7. Seal the root CA private key with the `age` public key, write to disk
8. Delete the root CA private key from memory
9. Generate a one-time join token and print it to stderr

The output tells you everything you need:

```
Cluster initialised.

  Cluster name:    prod
  Root CA:         serial 0x01
  Node CA:         serial 0x02
  Workload CA:     serial 0x03
  Ingress CA:      serial 0x04

  IMPORTANT: Back up the sealed root CA key:
    ./prod-root-ca.age

  Losing this file means a full PKI re-bootstrap.

  Join token (valid 15 minutes, single use):
    rbrg_join_1_a7f3b9c2e1d4...
```

## Join tokens and node certificates

New nodes join by presenting a join token:

```bash
$ relish join --token rbrg_join_1_a7f3b9c2... 10.0.1.5:9443
```

The token is a 256-bit random value, SHA-256 hashed for storage. The cluster never stores the plaintext — only the hash goes into Raft. When a new node presents a token, the council hashes it and compares against stored hashes. If it matches, isn't expired, and hasn't been consumed, the council marks it as consumed and issues a node certificate.

But that first token is single-use. It expires after 15 minutes, and once one node has used it, it's gone. How do you add a second node? A tenth?

Any admin with an existing token can generate more:

```bash
$ relish token create --name join-batch --role admin
```

The council writes a new join token to Raft via `generate_new_join_token()`, which takes an explicit TTL. The function is the same one `relish init` uses internally — the only difference is that `init` calls it once automatically, while subsequent tokens are created on demand. Each token is independent: its own 256-bit random value, its own hash, its own expiry. Consume one and the others are unaffected.

After that, every connection between cluster nodes uses mutual TLS. Both sides present their certificates, both sides verify against the Root CA trust anchor. A plain TCP connection to a cluster port gets rejected immediately.

## Gossip HMAC

Gossip uses UDP, which can't do TLS. Instead, we authenticate gossip messages with HMAC-SHA256. The HMAC key is derived from the Root CA certificate (which all cluster members share). This proves the sender is a cluster member without the overhead of TLS on every UDP datagram.

```rust
pub fn derive_gossip_key(root_ca_der: &[u8]) -> hmac::Key {
    let salt = Salt::new(HKDF_SHA256, b"reliaburger-gossip-hmac-v1");
    let prk = salt.extract(root_ca_der);
    // ... derive 256-bit HMAC key
}
```

## API authentication

Every HTTP request to the Bun API now requires a Bearer token:

```
Authorization: Bearer rbrg_a7f3b9c2e1d4...
```

Tokens have three roles:
- **Admin** — full access (deploy, stop, create tokens, manage secrets)
- **Deployer** — deploy and stop apps, view status
- **ReadOnly** — view status, logs, and service resolution

Tokens are hashed with Argon2id before storage. Argon2id is deliberately slow — it's designed to resist GPU-based brute force attacks. The Rust `argon2` crate handles the hashing:

```rust
let argon2 = Argon2::default();
let hash = argon2.hash_password(token.as_bytes(), &salt)?;
```

The middleware skips authentication for `/v1/health` (so liveness probes still work) and when no tokens exist yet (pre-init single-node mode).

## Secret encryption

Application secrets shouldn't live in plaintext in your git repository. Reliaburger uses `age` for asymmetric encryption. You encrypt secrets with the cluster's public key, and only the cluster can decrypt them.

In your app config:
```toml
[env]
DATABASE_URL = "ENC[AGE:YWdlLWVuY3J5cH...]"
```

At container startup, Bun decrypts `ENC[AGE:...]` values and injects the plaintext as environment variables. The decrypted value never touches disk — it goes straight from memory into the container's process environment.

Namespaces can have their own `age` keypairs, so team A's secrets can't be decrypted by team B's workloads.

## Raft log encryption at rest

The Raft log contains everything: CA keys, API token hashes, scheduling state, app configs. Even with key wrapping, we want another layer of protection for the log itself.

Every Raft entry is encrypted with AES-256-GCM before writing to disk. The encryption key is derived from the node's certificate private key via HKDF:

```rust
const RAFT_LOG_HKDF_INFO: &str = "reliaburger-raft-log-encryption-v1";

pub fn derive_log_encryption_key(
    node_private_key_der: &[u8],
    salt: &[u8; 32],
) -> Result<[u8; 32], RaftEncryptionError> {
    crypto::hkdf_derive_key(node_private_key_der, salt, RAFT_LOG_HKDF_INFO)
}
```

Each entry gets a fresh random salt, so identical entries produce different ciphertext. If someone steals the disk, they need the node's private key to decrypt anything.

## eBPF firewall wiring

Phase 3 gave us the eBPF `connect()` hook and the `firewall_map` BPF hash map. Phase 4 populates it.

When you deploy an app with `firewall.allow_from`:

```toml
[app.db.firewall]
allow_from = ["api", "frontend/web"]
```

Bun resolves `"api"` to its cgroup IDs and writes allow rules to the BPF map. The eBPF connect hook checks this map on every `connect()` syscall. If the source cgroup isn't in the map for the destination app, the connection is denied with `EPERM`.

The default behaviour (no `allow_from` specified) permits all apps in the same namespace to connect to each other — namespace isolation without any configuration.

## Under the hood: the crypto primitives

The security layer rests on a handful of cryptographic building blocks. Let's walk through them, because understanding what they do (and what they don't do) matters when you're trusting them with your cluster's secrets.

### HKDF: one secret, many keys

HKDF (HMAC-based Key Derivation Function) is how we turn a single master secret into multiple purpose-specific keys without any of them being related to each other. Two phases: extract (compress the input into a pseudorandom key), then expand (stretch it into the output you need).

```rust
pub fn hkdf_derive_key(ikm: &[u8], salt: &[u8; 32], info: &str) -> Result<[u8; 32], CryptoError> {
    let salt = Salt::new(HKDF_SHA256, salt);
    let prk = salt.extract(ikm);

    let info_bytes = [info.as_bytes()];
    let okm = prk
        .expand(&info_bytes, HkdfLen)
        .map_err(|_| CryptoError::HkdfFailed)?;

    let mut key = [0u8; 32];
    okm.fill(&mut key).map_err(|_| CryptoError::HkdfFailed)?;
    Ok(key)
}
```

The `info` parameter is the magic. Derive a key with `"reliaburger-node-ca-wrap-v1"` and another with `"reliaburger-raft-log-encryption-v1"` from the same master secret and same salt, and you get two completely unrelated 256-bit keys. Even if an attacker recovers one derived key, they learn nothing about the other. The `ring` crate enforces this by requiring a custom type implementing `KeyType` for the output length — another example of Rust's type system preventing mistakes.

### AES-256-GCM: encrypt and authenticate

AES-GCM gives us both confidentiality (the ciphertext is gibberish without the key) and authenticity (any tampering is detected). The `ring` crate expresses this through a two-step API:

```rust
pub fn aes_256_gcm_encrypt(
    key: &[u8; 32],
    plaintext: &[u8],
) -> Result<(Vec<u8>, [u8; 12]), CryptoError> {
    let rng = SystemRandom::new();
    let mut nonce_bytes = [0u8; 12];
    rng.fill(&mut nonce_bytes).map_err(|_| CryptoError::RngFailed)?;

    let unbound_key = UnboundKey::new(&AES_256_GCM, key)
        .map_err(|_| CryptoError::EncryptionFailed)?;
    let sealing_key = LessSafeKey::new(unbound_key);

    let nonce = Nonce::assume_unique_for_key(nonce_bytes);
    let mut in_out = plaintext.to_vec();
    sealing_key
        .seal_in_place_append_tag(nonce, Aad::empty(), &mut in_out)
        .map_err(|_| CryptoError::EncryptionFailed)?;

    Ok((in_out, nonce_bytes))
}
```

Three things worth noticing. First, we generate a fresh random nonce for every encryption. Reusing a nonce with the same key is catastrophic for GCM — it reveals the XOR of two plaintexts. `ring` can't enforce uniqueness at the type level (it would need to track every nonce ever used), but the type is called `Nonce::assume_unique_for_key` to make you think about it.

Second, `seal_in_place_append_tag` modifies the buffer in place — the plaintext becomes ciphertext, and a 16-byte authentication tag is appended. No separate allocation for the ciphertext. This is an optimisation, but it also means the plaintext is gone. You can't accidentally leak it after encryption.

Third, `LessSafeKey` can't be cloned, serialised, or sent across threads. You create it, use it, and it disappears when the function returns. The type system prevents you from accidentally persisting the encryption key alongside the ciphertext.

### Token validation: cheap checks first

The token validation function shows a pattern worth remembering — order your checks from cheapest to most expensive:

```rust
pub fn validate_token(plaintext: &str, stored: &ApiToken) -> Result<(), TokenError> {
    // Check expiry first (cheap: compare two timestamps)
    if let Some(expires_at) = stored.expires_at
        && SystemTime::now() > expires_at
    {
        return Err(TokenError::Expired);
    }

    // Verify Argon2id hash (expensive: deliberately slow)
    let hash_str = String::from_utf8(stored.token_hash.clone())
        .map_err(|_| TokenError::ValidationFailed)?;
    let parsed_hash = PasswordHash::new(&hash_str)
        .map_err(|_| TokenError::ValidationFailed)?;

    Argon2::default()
        .verify_password(plaintext.as_bytes(), &parsed_hash)
        .map_err(|_| TokenError::ValidationFailed)?;

    Ok(())
}
```

Expiry is a nanosecond comparison. Argon2id verification is deliberately slow (tens of milliseconds). By checking expiry first, we reject expired tokens instantly without burning CPU on the hash. This matters under load — an attacker spamming expired tokens costs you almost nothing.

The `if let Some(x) && condition` syntax is relatively new in Rust (stabilised in 1.64). It combines pattern matching with a boolean guard in a single `if` clause. Without it, you'd need a nested `if let` inside an `if`, which is both uglier and harder to read.

### The CA hierarchy generation

The full hierarchy is built in one function, showing how each CA chains to the root:

```rust
pub fn generate_ca_hierarchy(
    cluster_name: &str,
    wrapping_ikm: &[u8],
) -> Result<CaHierarchy, CaError> {
    let root = generate_root_ca(cluster_name, SerialNumber(1))?;

    let node = generate_intermediate_ca(
        CaRole::Node, cluster_name, SerialNumber(2),
        SerialNumber(1),  // parent = root
        &root.signing_keypair, &root.certificate_params,
        wrapping_ikm,
    )?;

    let workload = generate_intermediate_ca(
        CaRole::Workload, cluster_name, SerialNumber(3),
        SerialNumber(1),  // parent = root
        &root.signing_keypair, &root.certificate_params,
        wrapping_ikm,
    )?;

    let ingress = generate_intermediate_ca(
        CaRole::Ingress, cluster_name, SerialNumber(4),
        SerialNumber(1),  // parent = root
        &root.signing_keypair, &root.certificate_params,
        wrapping_ikm,
    )?;

    Ok(CaHierarchy { root, node, workload, ingress })
}
```

All three intermediates sign directly from the root with `BasicConstraints::Constrained(0)`, meaning they can sign end-entity certificates but can't create further sub-CAs. This prevents hierarchy abuse — a compromised Node CA can forge node certificates, but it can't create a rogue Workload CA.

The root keypair is available during this function (we need it to sign the intermediates) but it's not stored anywhere persistent. After `generate_ca_hierarchy` returns, the caller seals the root key with `age` and then drops it from memory. From that point on, the root key only exists encrypted on disk.

## What Rust taught us

Phase 4 is where Rust's ownership model really earns its keep. Cryptographic key material is the poster child for "use after free" and "double use" bugs. In C, you'd need to manually track which functions own which keys and when to zero them. In Rust, ownership rules enforce this automatically.

When we clone a CA's wrapped key for unwrapping, the original stays untouched in the Raft state. When we derive an HKDF key and use it for encryption, the derived key is dropped as soon as the function returns. No dangling references, no use-after-free.

The `ring` crate makes this even stricter. An `aead::LessSafeKey` can't be cloned or serialised. You create it, use it, and it's gone. The type system prevents you from accidentally persisting encryption keys to disk.

## Test count

Phase 4 adds 85 tests to the suite, bringing the total to 795. The new tests cover:

- CA generation and chain validation
- HKDF key derivation determinism
- AES-256-GCM encrypt/decrypt round trips (including tamper detection)
- Key wrapping and unwrapping
- Age secret encryption (including namespace isolation)
- Argon2id token hashing and role checking
- Join token lifecycle (valid, expired, consumed, single-use)
- mTLS config building
- Gossip HMAC authentication
- Secret decryption callback wiring
- Raft log entry encryption
- eBPF firewall rule resolution
