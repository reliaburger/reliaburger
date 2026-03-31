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
