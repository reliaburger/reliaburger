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

That normal command now writes `require_mtls = true` into the generated
`reliaburger.toml`. The first Bun therefore loads the identity and refuses to
start cluster mode without it. You don't generate keys and then quietly run
plaintext because you missed one Boolean. If you really need plaintext for an
isolated experiment, you must say `--development-plaintext`; the generated
file and both commands warn you. `relish dev create` makes the same explicit
trade for its local Lima network because that provisioner doesn't enrol a
separate identity in every VM yet. Subtle, it isn't.

The generated master key and security-bootstrap JSON both land as `0600`.
That second file used to land as `0644`, while Bun's loader quite reasonably
refused to read it. The result was security with a certain slapstick quality:
the bootstrap command generated a file the agent rejected. The end-to-end
config test now guards both halves of the contract.

Under the hood:

1. Generate a 256-bit master secret (for HKDF key wrapping)
2. Generate Root CA (ECDSA P-256, self-signed, 10-year lifetime)
3. Generate Node CA, Workload CA, Ingress CA (each signed by Root, 5-year lifetime)
4. Generate an `age` keypair for secret encryption
5. Wrap all intermediate CA private keys with the master secret
6. Issue the first node's certificate (signed by Node CA, 1-year lifetime)
7. Seal the root CA private key with the `age` public key, write to disk
8. Delete the root CA private key from memory

Notice what step 8 does *not* do: mint a join token. An earlier version did,
and printed it in the summary. We took it out, and the reason is worth a
paragraph because it's the whole point of the next section. A join token now
names the one node it may enrol, and `init` cannot know the id of a node you
haven't added yet. So it mints nothing and tells you how to mint one once the
cluster is up.

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

  To enrol another node, start this one, then mint a token bound
  to that node's id:
    relish join-token create --node-id <node-id>
```

## Join tokens and node certificates

New nodes join by presenting a join token:

```bash
$ relish join --token rbrg_join_1_a7f3b9c2... --node-id node-02 \
    --ca-fingerprint sha256:0123... https://10.0.1.5:9117
```

Port 9117 is the member's agent API. Port 9443 is the gossip seed that belongs
in the joining node's `[cluster].join` configuration; pointing the HTTPS client
at it can't work.

The token is a 256-bit random value, SHA-256 hashed for storage. The cluster never stores the plaintext — only the hash goes into Raft. When a new node presents a token, the council hashes it and compares against stored hashes. If it matches, isn't expired, hasn't been consumed, and names the very node id in the request, the council marks it as consumed and issues a node certificate.

There's a subtlety in *who* issues the certificate. A brand-new node has nothing: no CA, no replicated state, no way to validate anyone. So it can't issue its own cert. Instead it asks an existing member. `relish join` POSTs to that member's `/v1/cluster/join` endpoint, which validates the token and signs the joiner a certificate, returning a bundle with the new cert plus the Node CA and Root CA certificates the joiner needs to verify future peers. The joiner writes all of that into its identity directory (the same `sesame::identity_store` layout the bootstrap node uses) and restarts. On the way back up it loads the identity and its listeners come up speaking mTLS.

Read that again, though, and one word should worry you: *returns*. An earlier version of this flow had the issuer generate the joiner's keypair and post the private key back in the bundle. That's a private key travelling over the wire and sitting in the issuer's memory — a key that's supposed to be the one thing only the joiner ever holds. So we changed it. The joiner now generates its own keypair locally and sends only a **CSR** (a certificate signing request: a public key plus the identity it's asking to be certified for, signed by the matching private key to prove possession). The issuer signs the CSR and returns just the leaf certificate. The private key never leaves the joining node. This is the same shape as the workload-identity flow from Chapter 10, and for the same reason: a key you never transmit is a key you can't leak in transit.

The issuer takes exactly one thing from the CSR — its public key. Every other field of the certificate, the node-id SAN in particular, it rebuilds from the id *it* validated. And validation is the load-bearing word: the token is minted for one id, and the request must ask for exactly that id. A token created for `node-05` cannot obtain a `node-01` certificate, no matter what the CSR or the request body claims — the mismatch is refused before the CA key is ever touched. So a joiner that stuffs a rival node's id into its CSR gets nothing; the request is a request, not an instruction, and it's a request the token has to have authorised in the first place.

That endpoint is one of the few public routes: a joiner has no bearer token yet, so the join token in the request body *is* the credential. Which raises an obvious question — if the joiner can't verify anyone yet, how does it know it's talking to the real cluster and not an imposter handing out a poisoned CA? It doesn't, on first contact. This is trust-on-first-use, the same model as SSH. The mitigation is the fingerprint: `relish join` prints the `sha256:` fingerprint of the root CA it received, and `--ca-fingerprint sha256:...` refuses the bundle if it doesn't match. Compare it against what `relish init` printed and an imposter is caught before a single byte is written to disk. The token's 15-minute, single-use lifetime keeps the replay window small.

So how do you add a node? Mint a credential for it, explicitly, naming the id
it will enrol as:

```sh
JOIN_TOKEN="$(relish --ca-cert root-ca.crt --token "$ADMIN_TOKEN" \
  join-token create --node-id node-02 --ttl 15m)"
```

`relish token create` still means an **API bearer token**. `relish join-token
create` means a short-lived credential that can enrol exactly one node. The
hyphen is doing useful security work here: operators and scripts can't quietly
confuse two secrets with very different powers.

`--node-id` is mandatory, and it's the crux of the impersonation guard. The
token isn't a blank cheque that enrols *a* node; it's bound to *this* node id,
and the join handler refuses any request that asks for a different one. Without
that binding, an Admin who minted a token for a throwaway worker could — or
could be tricked into letting someone — turn it into a certificate for a
council member's id, which the bound Raft verifier from later in this chapter
would then accept as that member. Binding the id at mint time closes the gap:
the token can only ever enrol the node it was written for.

The CLI parser turns `15m` into seconds and rejects anything below one second
or above one hour. The Admin-only handler generates the random plaintext,
commits a `JoinToken` containing only its SHA-256 hash, expiry, and bound node
id to Raft, and prints the plaintext after that write succeeds. If the request
lands on a follower during an election it returns an error without the
plaintext. Point the retry at the current leader. No orphan token, no
optimistic success, no creative archaeology in the Raft data directory. Nice
and boring.

After that, every internal TCP connection between cluster nodes uses mutual TLS. Raft and reporting require node certificates. Peer API clients present the same identity too, although the API listener also accepts certificate-less Relish and browser connections because those authenticate with a bearer token or session cookie. Both peer sides verify the Node CA chain and the live revocation list. A plain TCP connection to one of these TLS ports gets rejected immediately.

### Where the node's certificate actually lives

For a long time, the honest answer was: nowhere. `initialize_cluster()` issued the first node's certificate and returned it, and the caller printed a summary and dropped it on the floor. The join path was worse — the council issued a certificate for the new node and then threw it away. All the PKI machinery worked; the output just never landed anywhere a listener could load it from. We only noticed when we sat down to wire mTLS into the Raft listener and asked the obvious question: which file do I read the key from?

The fix is `sesame::identity_store`, a small module that owns one directory:

```text
{data_dir}/identity/
  node.crt      # this node's certificate (PEM)
  node.key      # this node's private key (PEM, mode 0600)
  node-ca.crt   # the intermediate that signed it
  root-ca.crt   # the trust anchor
  meta.json     # node_id, serial, CA generation, validity window
```

`relish init` now writes this for the bootstrap node, fills the `[security]` section of the generated `reliaburger.toml` so `bun` knows where to look, and enables the mTLS requirement in that same file. Two details are worth stealing for your own projects. First, the private key is written with `atomic_write_mode(path, data, Some(0o600))` — the permission is set on the temp file *before* the rename, so there is never a moment when the key sits at its final path world-readable. Second, `load()` returns `Result<Option<NodeIdentity>, _>`, not a bare `Result`. A missing identity isn't an error; it's a state ("this node hasn't enrolled yet") that the caller matches on explicitly. Rust makes the three outcomes — loaded, absent, corrupt — impossible to conflate, which is exactly what you want for a file that decides whether your listeners speak TLS.

`init` also prints the root CA fingerprint (`sha256:...`). Keep it. When a joiner enrols later, `relish join` prints the fingerprint of the root CA it received, and those two strings matching is your defence against handing a node's trust to the wrong cluster.

### One token, one certificate

Look back at how the issuer handled a token: check it's unused, mark it consumed, then allocate a serial and sign. Three steps. On a single-threaded toy that's fine. In a real cluster it's a race waiting to happen. Two nodes fire the *same* leaked token at two different members at the same instant. Both members read the replicated state, both see the token as unused, both sail past the check, and now one token has minted two certificates with two serials. Not catastrophic on its own, but it's a duplicate-issuance bug in the part of the system whose entire job is to say "no" precisely.

The fix is to make "check unused, mark consumed, allocate serial" a single indivisible operation. We already have a machine that does exactly one thing at a time in a total order across the whole cluster: the Raft log. So we added one log entry, `ConsumeJoinTokenForIssue`, that does all three inside the state machine:

```rust
RaftRequest::ConsumeJoinTokenForIssue { token_hash } => {
    let Some(token) = /* find by hash */ else {
        return Some(CouncilResponse::Refused { reason: "join token not found".into() });
    };
    if token.consumed {
        return Some(CouncilResponse::Refused { reason: "join token already consumed".into() });
    }
    token.consumed = true;
    let serial = self.state.security_state.next_serial;
    self.state.security_state.next_serial += 1;
    return Some(CouncilResponse::JoinTokenConsumed { serial });
}
```

Because every node applies the log serially, exactly one entry for a given token finds it unconsumed. That entry gets a `JoinTokenConsumed { serial }`; the racer's entry, and every retry, gets `Refused`. The issuer only signs *after* this returns a serial, so a token that lost the race never produces a certificate. One token, one cert — enforced by consensus, not by hope.

The `Option<CouncilResponse>` return is Rust's way of saying "this entry might carry a verdict back to whoever proposed it". Most log entries return `None` (applied, nothing to report). This one returns `Some(...)` so the proposer learns which serial it won, or why it was turned away, without racing to read shared state back — the same pattern the plain serial allocator already used.

One more thing changed while we were in here. The old code hard-coded the issuer's distinguished name as a string literal when it rebuilt the CA for signing. If that string ever drifted from the actual Node CA's subject, the issued leaf wouldn't chain. We now derive it straight from the stored CA certificate with rcgen's `CertificateParams::from_ca_cert_der`, so the issuer field always matches the CA that's really in the trust store.

### Binding a connection to a node

Chapter 10 pins the client verifier to the Node CA: a peer must present a valid, unrevoked certificate that the Node CA issued. That stops outsiders cold. It does *not* stop one cluster node from impersonating another. Node B holds a perfectly valid Node-CA certificate. If B connects to A pretending to be C, the CA-pin check is happy — B's cert is genuine. We just never asked "genuine as *whom*?"

To ask that, the verifier needs two things: a name to expect, and a name inside the certificate to check against. The name inside the certificate is the node-id URI SAN we started issuing, `spiffe://reliaburger/node/<id>`. The name to expect is whichever node we meant to dial. On the Raft transport that's easy — openraft hands the connection factory the target's `CouncilNodeInfo`, which carries the peer's node id — so we build a per-target verifier bound to that id:

```rust
if let Some(expected) = &self.expected_node_id {
    let presented = node_id_from_leaf(end_entity).ok_or(/* no SAN → reject */)?;
    if &presented != expected {
        return Err(/* wrong node → reject */);
    }
}
```

Now a valid certificate for node C, presented when we dialled node A, is rejected: the SAN says C, we expected A. A certificate with no node-id SAN at all is rejected too — under binding, absence is mismatch. The property jumps from "some valid node" to "the specific node I meant to reach". (The reporting transport dials by address without a target id to hand the verifier, so it keeps the CA-pin-plus-CRL guarantee; binding lives where the id is known.)

### Installing the bundle without half-doing it

Each identity file is already written atomically — temp file, then rename — so no single file is ever truncated. But a node's identity is *five* files, and a crash between the second and third rename leaves a directory that looks populated and is quietly broken: a certificate from the new enrolment, say, beside a CA from the old one. Load that and your listeners come up with an identity that doesn't chain.

The cheap, robust fix is a commit marker. `save` clears the marker first, writes all five files, and writes the marker *last*. `load` refuses any directory that has identity files but no marker — that's a crash caught mid-install — and, for good measure, re-checks that the leaf actually chains to the CA chain before handing the identity back:

```rust
if !marker_path.exists() {
    return Err(IdentityStoreError::PartialBundle);
}
// ... read the five files ...
cert::validate_chain(&certificate_der, &node_ca_der, &root_ca_der)
    .map_err(|e| IdentityStoreError::InconsistentBundle { reason: e.to_string() })?;
```

A half-installed bundle now fails loudly at load instead of silently running degraded. It's the filesystem equivalent of the Raft trick above: make the "it's ready" signal a single thing that lands last, so partial states are detectable.

### A connection that never finishes

The accept side of a network listener has a quiet failure mode: a peer connects, sends two of the four length-prefix bytes, and then just... stops. It never sends more, never closes. The `read_exact` for the rest of the frame blocks forever, and the task handling that connection is pinned for the life of the process. Do it a few thousand times and you've exhausted the node without sending a single valid byte.

The frame-size cap doesn't help here — the attacker never claims a large frame, they just never finish a small one. What's missing is a clock. So every accept-side connection on the Raft and reporting listeners now runs its handshake and framed read under a `tokio::time::timeout`:

```rust
let _ = tokio::time::timeout(RAFT_ACCEPT_DEADLINE, async {
    if let Ok(tls_stream) = acceptor.accept(stream).await {
        handle_raft_rpc(tls_stream, raft).await;
    }
}).await;
```

A peer that stalls is dropped when the deadline fires, and the task is freed. `tokio::time::timeout` wraps any future and races it against a timer, returning `Err` if the timer wins — a much better tool than trusting TCP's own timeouts, which are measured in minutes and aren't really yours to set. The outbound side already had a connect timeout; this closes the same gap on the way in.

### A number is a promise, not a fact

That fixed the clock. There were two more bounds missing on the same listener, and both come from the same habit: believing what a stranger tells you.

Here's the original frame read. It looks fine:

```rust
let len = u32::from_be_bytes(len_buf) as usize;
if len > MAX_RAFT_RPC_SIZE {
    return None;
}
let mut payload = vec![0u8; len];
stream.read_exact(&mut payload).await.ok()?;
```

There's a cap, the cap is checked, and an oversized frame is rejected. So what's wrong? `vec![0u8; len]` allocates `len` bytes *before* a single byte of payload has arrived. Snapshots can legitimately be large, so `MAX_RAFT_RPC_SIZE` is 64 MiB. Send four bytes saying "64 MiB incoming" and the node hands you 64 MiB of memory for the privilege. Send four bytes a hundred times and it's 6.4 GiB. The cheapest possible attack: four bytes.

The fix is to let the buffer grow with reality rather than with the claim:

```rust
let mut payload = Vec::new();
let mut chunk = vec![0u8; FRAME_CHUNK_SIZE.min(len.max(1))];
let mut remaining = len;
while remaining > 0 {
    let want = remaining.min(chunk.len());
    stream.read_exact(&mut chunk[..want]).await.ok()?;
    payload.extend_from_slice(&chunk[..want]);
    remaining -= want;
}
```

A liar now costs one 64 KiB chunk. An honest peer sending a real snapshot pays the same total, just in instalments — `Vec` grows geometrically, so the reallocation cost is amortised and nobody notices. The declared length still bounds the loop, so it's not useless; it's just no longer taken as an instruction to allocate.

The third bound is *how many* connections exist at once. Each one is capped in time and in size now, but nothing stopped a peer opening ten thousand of them. So the accept loop holds a semaphore permit for the lifetime of each connection:

```rust
let connections = Arc::new(Semaphore::new(max_connections));
loop {
    let permit = connections.clone().acquire_owned().await?;
    let (stream, _peer) = listener.accept().await?;
    tokio::spawn(async move {
        let _permit = permit;   // released when the task ends
        // …serve the connection…
    });
}
```

Acquiring *before* `accept()` is the part worth pausing on. Excess peers then wait in the kernel's accept backlog, where they cost a socket and nothing of ours — no task, no buffer. Accept first and you've already paid for the connection before deciding you didn't want it.

`let _permit = permit;` inside the task is doing real work despite looking like a no-op. Rust drops a value at the end of the scope that owns it, and dropping a semaphore permit returns it. Moving the permit into the task is what ties "this connection is finished" to "this slot is free" — no release call to forget, no error path that skips it. This is RAII, the same pattern as a `MutexGuard`: the type's `Drop` impl *is* the cleanup, so the compiler's scope rules enforce what a human would otherwise have to remember.

Naming it `_permit` rather than `_` matters too. A bare `_` doesn't bind, so the permit would drop immediately at the top of the task and the cap would silently do nothing — a bug that only shows up under load, which is exactly when you'd rather not be debugging. The test for this leaks a permit on purpose and asserts the *second* connection is never served.

## Gossip HMAC

Gossip uses UDP, which can't do TLS. Instead, we authenticate gossip messages with HMAC-SHA256. The HMAC key is derived from the cluster master secret (which members hold, but outsiders don't). Deriving it from the public Root CA certificate would prove nothing. This proves the sender holds cluster key material without the overhead of TLS on every UDP datagram.

```rust
pub fn derive_gossip_key(master_secret: &[u8; 32]) -> hmac::Key {
    let salt = Salt::new(HKDF_SHA256, b"reliaburger-gossip-hmac-v1");
    let prk = salt.extract(master_secret);
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

The middleware skips authentication for `/v1/health` (so liveness probes still work) and when no user tokens exist yet. That second case is a bootstrap window, not permission to publish an open control plane: Bun only permits it on an IP-literal loopback listener. Wildcard, routable and hostname listeners are refused until the cluster has a real user token.

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

## The perimeter firewall

The eBPF firewall polices app-to-app traffic. The *perimeter* firewall polices the host itself: it keeps the outside world away from ports that only cluster members have any business touching — the gossip/Raft/reporting ports (9443–9445), the management API (9117), and the container host-port range. It's plain nftables, no eBPF, and it's reconciled straight from gossip membership. Every known cluster node gets a blanket `accept`; those ports are `drop`ped for everyone else. Policy stays `accept`, so SSH and whatever else you run on the box is untouched.

```
table ip reliaburger {
  chain input {
    type filter hook input priority 0; policy accept;
    iif "lo" accept
    ip saddr { 10.0.1.1, 10.0.1.2 } accept   # cluster members
    tcp dport 9443 drop
    tcp dport 9444 drop
    tcp dport 9117 drop
    # ...
  }
}
```

That ruleset looks obvious. It took two genuinely nasty bugs to get right, and both are worth your time because they'll bite you in any firewall you ever write.

**Always accept loopback first.** The first version had no `iif "lo" accept`. Everything worked in tests. Then on a real node, every `relish` command hung — `relish nodes`, `relish council`, the lot. No error, just a hang. The reason: `relish` talks to the local agent on `127.0.0.1:9117`, and the `tcp dport 9117 drop` rule doesn't care that the packet came from loopback. The source address is `127.0.0.1`, which isn't a cluster member, so the SYN was dropped on the floor. No RST, no refusal, just silence — which is exactly what a dropped packet looks like from the other end. The fix is one line, and it's the first rule in every sane firewall: accept loopback before you drop anything.

**`nft -f` appends; it doesn't replace.** We reconcile the firewall every time gossip membership changes. The first cut just fed the table definition to `nft -f` each time — and discovered the hard way that feeding `table ip reliaburger { ... }` to `nft -f` *adds* those rules to the existing chain rather than replacing them. So each reconcile stacked another full copy. That would be harmless, except the very first copy was generated at boot, before gossip had found anyone, when the only "member" was the node itself. Its `drop` rules sat at the front of the chain and shadowed the later copy that actually allowed the peers. Result: a node that gossiped happily but refused every peer's Raft connection. The fix is to make the ruleset idempotent — ensure the table exists, delete it, then recreate it, all in one atomic transaction:

```
table ip reliaburger {}
delete table ip reliaburger
table ip reliaburger { chain input { ... } }
```

Now every reconcile leaves exactly one clean copy, no matter how many times it runs.

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

## The keys nobody was holding

Here's an uncomfortable thing we found much later, reviewing our own work. Every primitive in this chapter was implemented, tested, and correct. And almost none of it did anything at runtime.

The reason was mundane. All the CA operations, the OIDC signing, the secret decryption at container startup, need one input: the 32-byte master secret, the `wrapping_ikm`. `relish init` generated it and wrote it to `{cluster}-master.key`. The running agent? It hardcoded `wrapping_ikm: None` with a `// TODO` next to it. So `sign_workload_csr` reached the line where it unwraps the CA key, found `None`, and bailed out with "no wrapping IKM available". The crypto was a beautifully built engine with no fuel line.

Two symptoms followed from the same root. The bootstrap `SecurityState` — the CAs, the age keypair, the OIDC config — was never loaded into Raft either, so `/v1/identity/jwks` returned 503 and `relish token list` had nothing to list. And the dev cluster handed out no key material at all, so there was nothing to load even if we'd wired the loading.

Fixing it is not glamorous, which is exactly why it's worth showing. First, a config section pointing the node at its secrets:

```rust
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct SecuritySection {
    pub master_key_path: Option<PathBuf>,
    pub bootstrap_path: Option<PathBuf>,
}
```

Both fields are `Option<PathBuf>`. A node with neither set still boots — that's the single-node case, no cluster, no PKI. A node with `master_key_path` set loads the key or dies trying. That "or dies trying" matters. A node told to load security material and then quietly booting *without* it is the worst outcome: it looks secure and isn't. So the loader fails closed, and it fails closed on file permissions too:

```rust
let mode = meta.permissions().mode();
// Any group (0o070) or other (0o007) permission bit set is too open.
if mode & 0o077 != 0 {
    return Err(BootstrapError::PermissionsTooOpen { path: path.to_path_buf(), mode: mode & 0o777 });
}
```

A master key that the whole box can read is a leaked master key. We refuse to load one.

The `Option` then flows all the way through. `cluster_params_from_config` turns the two paths into an `Option<[u8; 32]>` and an `Option<Box<SecurityState>>`, and both ride into the runtime on the `ClusterParams` struct. On a genuinely fresh bootstrap — no seeds, and the durable Raft store from Chapter 2 reports itself empty — the node seeds the `SecurityState` into Raft exactly once:

```rust
if params.seeds.is_empty() && store_fresh {
    let mut members = BTreeMap::new();
    members.insert(raft_id, self_info.clone());
    let _ = council.initialize(members).await;

    if let Some(state) = &params.bootstrap_security_state
        && let Err(e) = seed_bootstrap_state(&council, state).await
    {
        eprintln!("cluster: failed to seed security bootstrap state: {e}");
    }
}
```

Notice we lean on the durable store to make this idempotent. We don't track "have I seeded yet?" in a flag. A restarted node has a populated store, `store_fresh` is false, and the whole block is skipped. The `let Some(...) && let Err(...)` is Rust's `let`-chain: bind the state if it's there, run the seed, and only take the branch if it failed. From Raft, the state replicates to every node that joins, so only the bootstrap node ever needs the bootstrap file. The joiners get the master key (to unwrap their own operations) and read the rest out of consensus.

The lesson is one every distributed system eventually teaches. "Implemented and tested" is not "working". A unit test proves a function does what it says when you call it with the right arguments. It says nothing about whether anything ever calls it with the right arguments. The wiring is the system.

## The middleware nobody attached

Loading the keys made the crypto *work*. It didn't make anything *enforced*. We had an `auth_middleware` — a proper axum layer that reads a Bearer token, validates the Argon2 hash, checks the role. We had tokens that now persisted in Raft. And the two were never introduced to each other. The router was built without the layer. `relish` sent no `Authorization` header. You could create a token, admire it, and then watch every unauthenticated request sail straight through.

Turning it on sounds like one line: `.layer(auth_middleware)`. It wasn't, for three reasons worth walking through, because each is a design decision the naive version gets wrong.

**When is the door locked?** The middleware already had a rule: if no tokens exist, allow everything. That reads like a bug ("fails open!") but it's the right default for a system you bootstrap yourself. A brand-new cluster has no tokens, and you need *some* way to create the first one. So the door is open until you create a token, and locks the moment you do. We kept that, but tightened it: the check keys on *user* tokens, not on the store being non-empty, because of the second problem.

**Who authenticates the cluster to itself?** When you ask node A for logs that live partly on node B, A calls B's API. Turn on auth and that internal call gets a 401 — the cluster locks itself out. A needs a credential B will accept. We didn't want to store one (a stored token would make the store non-empty and slam the bootstrap door shut before you'd made a single real token). So the service token is *derived*, not stored:

```rust
pub fn derive_service_token(ikm: &[u8; 32]) -> Result<String, TokenError> {
    let bytes = crypto::hkdf_derive_key(ikm, &SERVICE_TOKEN_SALT, "reliaburger-service-token-v1")?;
    Ok(format!("rbrg_{}", hex::encode(bytes)))
}
```

Every node runs this over the same master key (Stage 3a put it on every node) and gets the same string. The middleware is handed that string and accepts it as a `__system` principal, in constant time, alongside the stored-token check. So the cluster authenticates to itself with a shared secret nobody had to distribute — it falls out of the key they already share. And because it's never in the token store, it doesn't count as a "user token", so the bootstrap door stays open until *you* create the first real one.

**How does the lock notice a new key?** We seeded the middleware's token store once, at startup. But tokens are created at runtime, on the leader, into Raft. A store loaded once at boot never sees them — you'd create a token and enforcement would never engage until a restart. So a small task re-reads the tokens from Raft every few seconds:

```rust
tokio::spawn(async move {
    let mut ticker = tokio::time::interval(Duration::from_secs(5));
    loop {
        tokio::select! {
            _ = refresh_shutdown.cancelled() => break,
            _ = ticker.tick() => refresh_token_store(&refresh_store, &refresh_council).await,
        }
    }
});
```

The `select!` is the idiom for "do this on a timer, but drop everything the moment we're told to shut down": whichever branch's future finishes first wins, and a cancelled `CancellationToken` resolves immediately. The five-second lag between creating a token and it being enforced is real, and deliberate — a poll is far simpler than threading a change-notification out of the Raft state machine, and five seconds is nothing for an operation you do by hand.

Three problems, none of them the crypto. Attaching a middleware is trivial; deciding what happens during bootstrap, how the system talks to itself once locked, and how the lock learns about new keys is the actual work. Same lesson as before, one layer up: the wiring is the system.

## Signing the whispers

The API is behind a lock now. Gossip isn't. Every node shouts SWIM datagrams over UDP — "are you alive?", "I think node-3 is dead" — and anyone on the network can shout back a lie. A forged datagram claiming a healthy node is dead is a denial-of-service with no packets dropped. UDP can't do TLS, so we sign each datagram with an HMAC.

The nice part is we don't need to distribute anything. The gossip key is derived from the master secret every node already loaded in Stage 3a:

```rust
pub fn derive_gossip_key(ikm: &[u8; 32]) -> hmac::Key { /* HKDF over the master key */ }
```

Same master key on every node, same derived key, no key exchange. (The original code derived it from the root CA *certificate* — but a certificate is public, so that authenticated nothing. The master key is the secret; use the secret.)

Signing happens in the transport, not in the forty-odd places that build a message. The `GossipMessage` already had a `hmac: [u8; 32]` field sitting zeroed "until Phase 4". The trick is what you sign over: the message *with that field zeroed*, so sender and receiver compute identical bytes.

```rust
pub fn canonical_bytes(&self) -> Result<Vec<u8>, bincode::Error> {
    let mut canonical = self.clone();
    canonical.hmac = [0u8; 32];   // sign over the message minus the signature slot
    bincode::serialize(&canonical)
}
```

This carries a quiet load-bearing assumption: bincode has to produce the *same* bytes on both nodes. It does, but only because the payload is `Vec`s and scalars. The day someone adds a `HashMap` to a gossip message, its iteration order will differ per process, the bytes will differ, and every node will silently reject every other node's gossip — a cluster that dissolves for no visible reason. So `canonical_bytes` carries a comment that says, in effect: *if you put a map here, make it a `BTreeMap`.* The send path signs; the receive path drops anything that doesn't verify, reusing the exact `continue` that already skipped malformed datagrams. A node with the wrong master key now simply doesn't exist as far as its peers are concerned.

One honest caveat we wrote down rather than hid: during a rolling upgrade, a node that's learned to verify will drop gossip from a peer that hasn't yet learned to sign (it's still sending zeroes). Because signing and verifying ship in the same binary and the key was already on every node, a coordinated restart converges in seconds. If we ever needed zero-downtime upgrades, we'd add an "accept unsigned during the transition" flag. We didn't need it, so we didn't build it — but we said so.

## Verifying the ingredients

The last gap: image signatures. We had a `verify_signature` that chains a signature to the cluster CA, and a config flag `require_signatures`. And in between, a check that only asked *is there a signature?* — never *is it valid?* A forged signature blob passed.

The awkward discovery was that there was nowhere to call the real verifier. The function that would live in a scheduler — read desired state, place replicas, check the image — doesn't exist yet; central scheduling is a later chapter. The only place an image actually becomes a running container today is the agent's local deploy loop. So that's where the gate goes:

```rust
for (app_name, spec) in &config.app {
    if let Err(reason) = self.enforce_image_signature(spec).await {
        let _ = events.send(ApplyEvent::Error { message: reason }).await;
        continue;   // refuse this app; the others still deploy
    }
    // ...deploy...
}
```

`enforce_image_signature` reaches the root CA the same way secret decryption already did (through the council), looks the image up in the replicated manifest catalogue, and — for a Pickle-hosted image — verifies the signature against that CA. External-registry images (nginx from Docker Hub) aren't ours to vouch for, so they pass. A local image with no signature, or a tampered one, is refused. This is deliberately *local* enforcement: each node vouches for what it runs, which is exactly the right guarantee until a central scheduler exists to vouch cluster-wide.

One more thing had to travel with the root CA to make this honest: the CRL. Chaining a signature to the cluster CA proves the signer *was* trusted when the certificate was issued, not that it still is. If a signing key leaks and you revoke its certificate, images it signed before the revocation must stop deploying. So `enforce_image_signature` passes the council's CRL alongside the root CA, and the keyless verifier checks the leaf and every intermediate against it before it even looks at the signature bytes. A revoked certificate anywhere in the chain fails the deploy closed. Revocation that doesn't reach every trust decision isn't revocation; it's a suggestion.

Two different domains — gossip integrity and registry trust — one theme. The primitive was always there; the enforcement point was the missing piece.

## What Rust taught us

Phase 4 is where Rust's ownership model really earns its keep. Cryptographic key material is the poster child for "use after free" and "double use" bugs. In C, you'd need to manually track which functions own which keys and when to zero them. In Rust, ownership rules enforce this automatically.

When we clone a CA's wrapped key for unwrapping, the original stays untouched in the Raft state. When we derive an HKDF key and use it for encryption, the derived key is dropped as soon as the function returns. No dangling references, no use-after-free.

The `ring` crate makes this even stricter. An `aead::LessSafeKey` can't be cloned or serialised. You create it, use it, and it's gone. The type system prevents you from accidentally persisting encryption keys to disk.

## Tests

Cryptography is a dream to unit-test and a nightmare to test any other way. The functions are pure: same input, same output, every time. HKDF is deterministic, AES-GCM round-trips, a tampered ciphertext always fails the tag check. None of it needs a network, a second node, or root. So Phase 4 is almost entirely unit-tested — 119 tests living next to the code in `src/sesame/`.

### What the unit tests prove

The roster maps directly onto the primitives in this chapter:

- **CA hierarchy** — generation and chain validation: a node cert verifies against the Node CA, which verifies against the Root, and a Workload-CA-signed cert does *not* validate as a node cert.
- **HKDF** — determinism (same `ikm`/salt/info gives the same key) and separation (different `info` gives unrelated keys).
- **AES-256-GCM** — encrypt/decrypt round-trips, and the one that matters most: flip a byte of ciphertext and decryption returns an error, never plaintext.
- **Key wrapping** — wrap an intermediate CA key, unwrap it, get the original back; unwrap with the wrong purpose string and it fails.
- **`age` secrets** — encrypt/decrypt, plus namespace isolation (team B's key can't read team A's secret).
- **Argon2id** — hashing and verification, and that the three roles (`Admin`/`Deployer`/`ReadOnly`) gate the right operations.
- **Gossip HMAC**, **mTLS config building**, **Raft log entry encryption**, **eBPF firewall rule resolution** — each gets its own test.

A nice property of testing crypto this way: the tamper-detection and namespace-isolation tests are *negative* tests. They assert that the wrong key fails. Those are the tests that actually tell you the security property holds, not just that the happy path works.

### Running them

No gating here. Crypto needs nothing but a CPU, so the whole chapter runs under a plain:

```sh
cargo test --lib sesame
```

The one thing the unit tests *don't* cover is the full token-and-revocation lifecycle across a running cluster — single-use enforcement in the agent, secret rotation windows, certificate revocation lists. That needs `SecurityState` in Raft, so its integration test (`tests/security_integration.rs`, labelled "Phase 10") arrives with Chapter 10. It drives the `sesame` library directly — no running agent — so it too runs under a plain `cargo test`.

Phase 4 adds 85 tests to the suite, bringing the total to 795.

### Private files need private temporary files too

Writing a credential to `context.tmp`, changing its permissions, and renaming it
sounds atomic. It still has two problems. The temporary file can be readable
before the permission change, and a pre-existing symlink at that predictable
name can redirect the write into another file.

The shared identity writer now uses `tempfile::Builder` to create a unique file
with the requested mode before writing any bytes. It syncs the contents,
atomically replaces the destination, and syncs the containing directory. The
`NamedTempFile` owns cleanup on errors; a successful `persist` transfers ownership
to the destination path. This also makes managed-cluster checkpoints durable.

A regression test plants a temporary symlink beside a context and verifies that
saving the context leaves the other file untouched. The original implementation
failed this test. The identity suite checks that certificate persistence still
works with the safer writer.

### Let a new node reach the enrolment endpoint

Real VM qualification found a circular dependency in the perimeter firewall.
Only gossip members could reach the management and cluster ports. A new node
needed those ports to enrol and become a member. Loopback integration tests,
which deliberately disable the host firewall, couldn't expose that problem.

`security.bootstrap_peers` lists explicit IP addresses that may reach those
ports before membership exists. It grants network reachability, not a cluster
identity: the join endpoint still requires its node-bound token, and internal
transports still enforce their configured authentication. These rules don't
permit traffic to container host ports. Quickstart knows the addresses of its
owned VMs before it configures any node, so it supplies that exact list.

The field uses Rust's `IpAddr` type rather than strings or raw nftables fragments.
Serde rejects malformed addresses while parsing configuration. Rule rendering
separates IPv4 and IPv6 and retains the final drop rules for outsiders. We also
pass the actual configured listener ports into the perimeter instead of assuming
the default ports. Tests assert both the narrow accept rules and the remaining
drops; the managed VM test exercises the enrolment path with the firewall active.


A real VM also caught a less obvious firewall mistake. Linux chose an ephemeral
client port inside our protected container-port range for a registry download.
The reply came back to that port and our input chain dropped it. We now accept
connection-tracked replies before applying the destination-port restrictions.
We deliberately match `ct direction reply`, not every established connection:
an inbound connection must still obey the current peer policy after membership
changes. Both IPv4 and IPv6 tables use the same ordering.

An enrolled node can join gossip before Raft has replicated its API tokens.
Refusing its public listener immediately caused a restart loop which delayed
replication further. Bun now waits up to thirty seconds for the token refresh
before opening that listener. Timeout still fails closed. Tests deliver a token
after startup begins and separately check the empty-store deadline. A shutdown
guard cancels already spawned tasks when startup exits with an error; Rust drops
the guard on both the success and error return paths.

### Certificate lifetimes need a clock, not a calendar date

Issue a certificate at lunchtime with a 90-second lifetime. Converting both ends
of its validity window to year/month/day throws away the entire lifetime: both
ends become midnight. It is already expired. Our original CA and node issuance
paths did exactly that, while stored CA metadata kept a different timestamp.

Issuance now captures one instant, rounds to the whole-second precision encoded
by X.509, and adds the requested lifetime with checked arithmetic. The `time`
crate handles calendar transitions; the handwritten leap-year conversion is gone.
We reject lifetimes that cannot produce a valid, positive window. Stored CA
metadata comes from the same parameters used to sign the certificate. The
regression decodes a real 90-second certificate and compares both the duration
and the root CA's encoded timestamps with its stored metadata.

That fix was a little too exact, though. With `not_before` set to the issuing
second, a joining node whose clock ran one second behind the issuer's received
a certificate it considered "not yet valid". It refused to save it, asked for a
new one five seconds later, and burned another Raft serial each time. Nobody's
clocks agree to the second, so this isn't an edge case.

Every issuance path now backdates `not_before` by `CLOCK_SKEW_BACKDATE` (five
minutes), the same constant workload certificates already used in Chapter 10.
The lifetime still counts from the issuing instant, so a 90-second certificate
stays usable for 90 seconds. We deliberately didn't add leeway to validation:
the verifier stays exact, and the only thing that moved is the start of the
window the issuer signs.

Backdating had one knock-on effect, caught by an existing test. The check that a
leaf never outlives its issuer compared the leaf's `not_before` with the
issuer's window. Once that start moved five minutes back, an ingress CA that had
expired a second ago could still mint a (born-expired) leaf. The check now judges
the issuer at the real issuing instant, and clamps the leaf's backdated start so
it never predates its issuer. The regression issues a root, an intermediate, a node
certificate (both the self-issued and CSR-signed kinds) and an ingress leaf,
then checks each one with a clock sixty seconds in the past.

The constant is declared `pub const CLOCK_SKEW_BACKDATE: Duration = ...;` in
`ca.rs`. A Rust `const` is a compile-time value with no fixed address (closer to
a C `#define` with a type than to a global variable), so sharing it between
modules costs nothing. The workload identity code now imports it with
`use super::ca::CLOCK_SKEW_BACKDATE;`, where `super` means "the parent module",
here `sesame`.

### Bad certificate input is an error, not a panic

A DNS subject alternative name must fit the certificate library's IA5 string
representation. The public issuer previously called `unwrap()` on that conversion,
so a non-ASCII name could panic. It now collects conversions into a
`Result<Vec<_>, _>`: collection stops at the first invalid name and returns its
context to the caller. The common-name SAN follows the same rule instead of
silently disappearing. Requesting a root CA through the intermediate-CA function
also returns an input error. Regression tests exercise both former panic paths.

### A process counter cannot identify certificates across nodes

Restart two ingress nodes using the same CA. If each starts its certificate
counter at the same number, their first leaves share an issuer and serial.
An atomic increment only coordinates threads inside one process. It says
nothing about another node, or tomorrow's restart.

Ingress issuance now asks the operating system for twenty random bytes. It
clears the sign bit and sets the next bit, leaving 158 random bits and a
positive, non-zero value that always occupies twenty bytes. That also keeps
these serials outside the eight-byte range of our Raft-issued node identities.
[RFC 5280 section 4.1.2.2](https://www.rfc-editor.org/rfc/rfc5280#section-4.1.2.2)
sets the positive-serial requirement and the twenty-octet ceiling.

The expression `(serial[0] & 0x3f) | 0x40` uses bitwise AND to retain the six
low bits and bitwise OR to set the next bit. `[0u8; 20]` creates a fixed-size
array of twenty bytes. The random fill returns a `Result`; `?` propagates a
failure instead of falling back to a timestamp or a repeatable counter.
Uniqueness is probabilistic, based on independent cryptographic randomness.
There is no persistent counter to roll back and no synchronous Raft request
inside the TLS handshake. The signed certificate carries its serial; the
resolver owns its cached key and discards that cache at restart.

The regression starts separate processes with the same CA, including a restart
and concurrent peers. It checks the actual signed certificates, their serial
shape and the absence of duplicates. The old counter fails this test.

This does not add ingress CRL distribution. Our cluster revocation API owns
64-bit node identities; it must not truncate a longer ingress serial and claim
to revoke it. Individual ingress-leaf revocation remains unsupported. Certificate
renewal and hot reload are tracked separately under C14, and CA rotation remains
an explicit future capability under F04.


### Send the issuer along with the leaf

A certificate signed by the Ingress CA isn't directly signed by the cluster
root. The client needs the intermediate certificate to connect those two facts.
Our first SNI resolver sent just the leaf. Unit tests proved its signature was
correct, but a real client trusting only the documented root returned
`UnknownIssuer`.

Ingress issuance and the resolver now carry the original root-signed Ingress CA
certificate and send `[leaf, intermediate]` during the handshake. We don't
recreate a self-signed copy of that intermediate from its signing parameters;
that would discard the root's signature. The root itself stays with the client
as its configured trust anchor and needn't travel in the server's chain.

The regression starts a real TLS listener and a client whose trust store contains
only the root. It fails against the leaf-only implementation and succeeds with
the complete chain, checking that the server sends exactly those two certificates.
This is a prerequisite for testing renewal: a fresh certificate must also be one
the documented client can validate.

### A cached leaf still has an expiry date

Open a connection, leave the ingress running for ninety days, then open another.
The original cache returned the same certificate both times. Caching had quietly
turned a validity period into a suggestion.

The resolver now reads the leaf's X.509 validity dates when it caches the key.
Before the midpoint it reuses that key. From the midpoint onwards, the next
handshake issues a replacement and swaps the cache entry. An idle host needs no
background signing: its first returning client triggers renewal, even if the old
leaf expired while nobody was using it. Cache keys use lower-case hostnames.

Signing happens outside the short cache lock. If signing fails, an existing leaf
can still serve until its expiry; an expired or not-yet-valid leaf cannot. The
issuer's actual certificate sets another boundary: we refuse issuance outside
its validity window and cap each leaf at the issuer's expiry. Renewing leaves
cannot extend the lifetime of a CA. CA rotation remains separate work.

`CachedCertificate` owns an `Arc<CertifiedKey>` and a small validity value. Cloning
the `Arc` gives a handshake ownership of the current key without copying its
secret bytes. Replacing the entry doesn't invalidate a connection already using
that key. The lifetime limit for established connections is a separate policy.

The regression uses real TLS connections with a ten-second certificate. It
checks reuse immediately, a different valid leaf after six seconds, and another
valid leaf after eleven idle seconds. Two issuance tests also reject an expired
issuer and prove a leaf cannot outlive it. Operator file replacement and node
transport renewal still have their own implementation and acceptance work.

### Replacing a certificate while the listener stays open

An operator updates `tls_cert_path` first, then `tls_key_path`. Between those
writes, the new certificate and the old key don't match. Installing each file
independently would break every new handshake during that interval.

Wrapper now polls the pair once a second, reads both files on a blocking worker,
and checks that the key matches the leaf and every certificate in the supplied
chain is currently valid. Only then does it publish the complete pair. Missing,
malformed, oversized or mismatched files leave the previous pair in place. The
resolver checks validity again at handshake time, so retaining a broken
replacement doesn't make the old certificate immortal. Repeated identical reload
errors produce one warning until the files recover or the error changes.

A Tokio `watch` channel holds the latest validated pair. Unlike a queue, it
represents one current value: the resolver briefly borrows it, clones the `Arc`
and releases the borrow. The polling future belongs to `BoundProxy::serve`,
alongside the listeners, and stops when that server shuts down. File reads and
key parsing happen in `spawn_blocking`, so neither can stall an async worker.
Each input must be a regular file and fit within one MiB; symlink-based atomic
replacement still works.

The acceptance test runs the real Wrapper listener. It deliberately writes a
mismatched intermediate pair, finishes the replacement, then writes malformed
PEM. New connections observe the latest valid certificate; an existing HTTP
connection keeps working throughout. A separate test rejects expired and
not-yet-valid certificates at startup. This reload doesn't provision certificates
for the operator, and it doesn't yet renew the node identity used by the API,
Raft or reporting.

### A reconnect must check the current certificate

Reloading the key is only half the job. TLS session resumption can establish a
new connection from a previously negotiated session without asking the resolver
for a certificate. Our real reconnect test reused a client configuration,
consumed the server's session tickets and observed exactly that: `Resumed`
instead of a full handshake.

Ingress now disables the server session cache and TLS 1.3 tickets, matching the
node/API policy. Every reconnect performs certificate validation against the
current resolver. Existing connections stay open during renewal or file reload;
their eventual retirement needs its own connection lifetime limit. This costs a
full TLS handshake per connection, a deliberate trade-off for an explicit
certificate lifecycle in 0.1.0.

### Give established connections a retirement date

A WebSocket can keep a TLS session alive long after we replace its certificate.
Timing out the HTTP serving future doesn't solve that: once Hyper accepts the
upgrade, it hands the byte stream to the WebSocket forwarding task and finishes
that future.

API, registry and ingress TLS connections now have a one-hour maximum lifetime.
For ordinary HTTP, the server starts graceful shutdown thirty seconds before
that deadline. HTTP/2 stops admitting new streams; HTTP/1 finishes its current
request. Work still running at the final deadline ends, so clients of long-lived
streams must reconnect. The API TLS handshake itself also has a ten-second
limit. Raft and reporting already exchange one RPC per connection under much
shorter deadlines; they don't need the HTTP retirement policy.

`LifetimeLimitedIo<S>` wraps the stream itself, so the hard limit survives an
upgrade. The `S` type parameter lets the same wrapper hold each listener's TLS
stream. It implements Tokio's `AsyncRead` and `AsyncWrite`: every poll first
checks the deadline, then delegates to the underlying stream. Activity never
moves the deadline. Polling the timer registers a wakeup, which also retires an
idle reader or a writer blocked by a peer that stopped reading.

The timer lives in `Pin<Box<Sleep>>`. `Box` gives it a stable heap allocation;
`Pin` promises not to move the timer while Tokio holds references into its
state. The stream's `Unpin` bound lets us borrow and poll that field normally.
We need no `unsafe` pointer manipulation. Returning `Poll::Pending` means
"wake me when something changes", not "block this thread".

The tests advance Tokio's clock, rather than wait an hour. Three I/O tests prove
that active traffic cannot extend the limit and that blocked reads and writes
wake at expiry. A real HTTPS listener upgrades to WebSocket, echoes bytes,
retires the old stream and still accepts a fresh connection. Bun's actual API
serving function also completes an in-flight request during the drain grace,
then closes that connection. Node certificate issuance and hot replacement
remain the unfinished part of C14.

### Replace a node identity without tearing it in half

Suppose renewal writes `node.crt`, then cannot replace `node.key`. The old store
had already removed its commit marker. Even the old, still-valid identity could
no longer be loaded. A process restart turned a recoverable write failure into
an enrolment problem.

The store now commits a complete `node.bundle.json` in one private atomic
replacement. It contains the leaf, matching key, CA certificates and metadata.
The existing PEM files remain exports, but Bun loads the snapshot. A layout-2
marker requires that snapshot: a missing or corrupt snapshot cannot silently
fall back to older exports. Initial installation still writes the marker last.
A validated layout-1 identity is snapshotted before its exports are replaced,
so importing that layout doesn't create another partial-write window.

Before writing, we verify the chain, issuer names, signed node URI and serial,
and the private key's match to the leaf. Validity comes from X.509 dates, not
from sidecar timestamps. Those checks also run when loading. The snapshot and
key use mode `0600`; the managed installer copies the snapshot before the
marker and retains private permissions. Parse errors report locations without
quoting input that could contain key material.

The snapshot uses the existing unique temporary-file, file-sync, rename and
parent-directory-sync helper. Before replacement, readers see the old complete
identity. After replacement, they see the new complete identity. A directory
sync error after rename can still make durability uncertain; a renewal caller
must treat that as failure and must not publish an unacknowledged write to its
live TLS resolvers. Atomicity prevents a mixture of identities, not every
possible storage failure.

`IdentityBundle` is private to the persistence module. Its serde implementations
encode the complete snapshot without making the public `NodeIdentity` type a
wire format for private keys. Loading moves those owned vectors into a validated
identity. A concurrent-reader test checks that replacements never expose a
half-written pair. Even a lone PEM file from an interrupted first install is an
incomplete identity, never an unenrolled node that may choose plaintext. Other regressions force an export write failure, substitute
an unrelated private key, alter identity metadata and corrupt or remove the
snapshot. Automatic issuance and live transport replacement remain separate
steps.

### Keep TLS configurations, replace their credentials

Rebuilding a certificate file doesn't change a `rustls::ServerConfig` that
already owns the old key. The same problem appears in client configurations.
We need those configurations to ask for the current credentials at each new
handshake.

`LiveNodeIdentity` loads a validated identity and keeps its persistence directory
alongside a Tokio `watch` value containing the identity and signing key together.
Clones share that value. The type implements rustls's server and client
certificate resolver traits, so existing configurations can select a replacement
without rebuilding listeners or dropping established connections.

Replacement validates the new certificate, preserves the node identifier and
trust anchors, and requires a newer signed serial. Replaying the exact current
certificate is harmless. A shared Tokio mutex serialises writers; the blocking
worker owns its guard through disk persistence and publication. Dropping the
requesting future therefore cannot cancel a transaction that has started. A
failed save leaves the live value unchanged.

Why treat expired client and server identities differently? A server resolver can
refuse an expired leaf. Returning `None` from a client resolver means "send no
certificate", which our optional-mTLS API permits for browsers and CLI users.
An expired node mustn't quietly become one of those anonymous TLS clients. Its
resolver continues presenting the configured certificate so the peer rejects
its expiry, matching rustls's static certificate behaviour.

The tests retain existing configurations across replacement and perform real
handshakes for both required and optional client authentication, with bound and
unbound peer verification. They also check invalid replacements, persistence
failure and recovery, concurrent serial ordering and expired client refusal.
For cancellation, a blocking test worker holds publication after the new snapshot
is durable. Cancelling the caller and releasing that hold still publishes the
same identity that disk contains. The authenticated renewal request remains a
separate implementation step.


### Give every transport the same live identity

A replacement in memory only helps if the listeners use it. Bun now loads one
`LiveNodeIdentity` and clones that handle into the API and registry listeners,
Raft and reporting transports, and internal HTTPS clients. Their configurations
keep the existing trust anchors and revocation checks, but ask the shared
resolver for the current certificate at each handshake. Raft also retains its
expected-node check. Client session resumption is disabled so reconnects cannot
silently retain an earlier identity.

The HTTP pool can keep an established connection until its existing lifetime
limit. Once it reconnects, the same `reqwest::Client` presents the replacement.
The registry client's service-token header survives this change too. TLS proves
which node connected; the token still authorises the operation.

Diagnostics read the live snapshot per request. Axum's `Extension<T>` carries
an application value through the router; `Option<Extension<T>>` also permits
plaintext development routers where no node identity exists. This isn't input
from a request header. Bun installs the handle when it constructs the router.
We report the current public serial and expiry, with automatic renewal still
false until the issuance loop exists.

The regressions use actual handshakes and HTTPS requests while retaining the
same configurations and client. A cluster test goes further: replace all three
node identities, revoke every old leaf, then require a new Raft write to reach
all voters and new reports to reach the leader. A stale resolver can no longer
pass just because yesterday's certificate hasn't expired yet.

### A renewed leaf cannot extend its issuer's life

Suppose the Node CA expires in 90 seconds. Issuing the usual one-year node leaf
would advertise a year of validity even though its chain stops working almost
immediately. Both locally generated node certificates and CSR-signed leaves now
cap their expiry at the issuer's expiry, and refuse issuance if the issuer isn't
currently valid.

Joining and renewal reconstruct signing parameters from the stored CA. Those
parameters are useful for the subject and constraints, but we explicitly copy
the original certificate's signed validity dates into them before signing. The
sidecar dates in cluster state don't get to extend that window. The tests shorten
an actual root-signed Node CA while deliberately leaving its sidecar unchanged,
then check all three issuance paths. Separate cases use expired and future-dated
issuers and require refusal. Renewal can't substitute for CA rotation.

### Renew the caller's identity, not a name from its request

A service token proves that a request came from internal automation. It doesn't
say which node sent it. Allowing that token alone to request any node name would
let one compromised node impersonate every other node.

`POST /v1/cluster/renew` therefore requires both the service principal and the
client certificate from the actual TLS connection. Bun places that public leaf
in a typed Axum extension after the handshake. Headers cannot populate it, and
anonymous TLS clients don't receive one. The endpoint takes a CSR and format
compatibility, not a caller-selected node name.

The leader checks the existing leaf against a quorum-confirmed security state:
chain, issuer binding, signed validity, node URI and revocation of the leaf and
its issuers. It then checks that the signed CSR names the same node. The CSR's
public key is the only request-controlled field that reaches the new
certificate; issuance rebuilds the identity and usages server-side.

Why check again when TLS already did? An established connection can survive a
revocation or pass its certificate's expiry. Each renewal request needs current
admission evidence. After allocating a serial through Raft, the issuer reads
current security state and validates again before signing. Serial allocation
may commit even if the request disappears, so a retry can leave a gap in the
sequence. That's harmless. Reusing an allocated serial would not be.

Followers refuse this request. Forwarding it would present the follower's TLS
identity and change who is asking. The renewing node must contact the leader
directly. HTTP admission caps the body at 16 KiB and the operation at ten
seconds; signing runs in a blocking worker with an owned state snapshot.
Concurrent requests get distinct committed serials. Callers rejected at
admission do not spend a serial. The generated private key remains on the
requesting node throughout.

### Let the node own its renewal loop

Bun now starts one renewal owner when it has a live node identity, a council,
resolved member API addresses and the internal service token. Once a second the
owner reads the actual signed validity window. Before the midpoint it waits;
afterwards it generates a fresh local key and CSR and contacts the current
leader directly. It resolves the leader again on every retry, so a failed
request doesn't pin the worker to a former leader.

The renewal HTTP client refuses redirects and opens a new TLS connection for
each attempt. Its connection, request and whole-attempt deadlines bound waiting;
the response body is capped at 64 KiB. A successful response still has to pass
the identity handle's checks for key binding, node and CA continuity and a newer
serial. Only a durable save permits publication. A network error, refusal,
malformed response or failed save leaves the current identity installed and
schedules another attempt after five seconds. An expired identity never gets
an authentication bypass; an operator must re-enrol it.

The owner publishes an enum through a Tokio `watch` channel: starting, valid,
renewing, retrying or expired. Diagnostics hold a receiver, not a copy of the
last result. When the owner exits or panics its sender disappears, and the
receiver reports stopped even if the last stored value was valid. That small
ownership detail prevents a dead background task from claiming healthy renewal
forever. Shutdown cancels pending network work; a persistence transaction that
has already started still owns its completion.

The tests use real TLS and the real renewal endpoint. They show that the worker
waits before the midpoint, retains the old identity after a failed save, rejects
redirects, recovers after server errors and cancels an in-flight request. A
separate test checks that diagnostics distinguish a starting worker from a
stopped one. The cluster acceptance test holds a renewal request at the old
leader, shuts down that council member and requires the worker to reach the new
leader and reload the resulting identity from disk.

The black-box test starts the actual Bun executable with a generated secure
configuration and a leaf already past its midpoint. It waits for a newer serial
on disk, connects with Relish over HTTPS, restarts Bun and checks that the same
renewed leaf is still installed. Malformed responses and valid bundles padded
beyond the response limit must leave both disk and live credentials unchanged.

### Reject lifetimes that the clock cannot represent

A token request with `ttl_days = 0` used to create an already expired credential.
A sufficiently large number could panic while converting days to seconds or
adding that duration to the system clock. Neither request should reach Raft.

We now use `checked_mul` and `SystemTime::checked_add`. Each returns an
`Option`: `Some(value)` on success, `None` when the result cannot be represented.
The `filter` rejects zero, and `and_then` continues only when the previous
calculation succeeded. Invalid lifetimes return HTTP 400 before hashing or
committing a token. Omitting the lifetime still means explicitly non-expiring;
zero is not another spelling of that choice.

The API tests submit both multiplication and clock overflows, then inspect
the committed token store. They also check zero, an ordinary one-day token
and an omitted lifetime. The two regressions failed before the repair.

The central route audit must describe renewal too. Its principal is System,
with the additional TLS peer-certificate requirement enforced by the handler.
CI caught two omissions when we added its request-size limit: the audit table
was missing the new route, and the syntax-tree scanner mistook `.layer(...)`
for an HTTP method. The scanner now follows the receiver through `layer` and
`route_layer`, retaining every explicit HTTP method before and after them.
Unknown constructs still fail the audit instead of silently disappearing.

### Admit the whole workload manifest before changing anything

A token confined to namespace `a` must not deploy a job into namespace `b`.
The apply handler checked apps, then handed jobs to the receiving node without
the same checks. A mixed manifest could even commit its apps before admitting
an unauthorised job.

Apps and jobs now contribute to one borrowed iterator of target name, namespace
and whether the workload executes a host process. `Iterator::chain` visits
the second iterator after the first; it doesn't allocate another manifest or
convert job specifications into app specifications. Every target passes the
token scope and its principal's configured Deploy permission before the first
Raft write or agent command. Both `exec` and `script` additionally check the
configured HostExec permission. Principals without a permission specification
retain the existing role-and-token-scope policy, and the node's host binary
allowlist remains a separate admission boundary.

The API regressions inspect the agent command queue and Raft desired state,
not just HTTP status. Refusal must leave both untouched. Positive tests show
that an in-scope job and an explicitly permitted host job still reach the
agent. A permission check that refuses everything isn't a working permission
system either.

### A deployment must not rewrite its own permission grant

Suppose `ci` may deploy `web`, but may not execute host scripts. If that same
credential can apply `[permission.ci]` with `host-exec`, the restriction buys
us nothing. Namespace quotas have the same problem: the user constrained by
a budget must not be able to remove that budget in the next manifest.

Ordinary permission and namespace declarations now require an unscoped user
administrator. We check this before any part of a mixed manifest changes
state. The internal service identity cannot grant permissions. A test lease
remains a separate, bounded exception: its existing ownership checks allow
only its reserved namespace, and never permission declarations.

The check must survive a network hop. When a follower forwards apply to the
leader, it preserves the user's bearer token or session cookie. Substituting
the node's service token would erase the identity whose permissions the
leader needs to check. The forwarding response also retains the leader's
status and content type, so an authorisation refusal stays a refusal rather
than looking like a successful event stream.

### A scoped administrator is still scoped

An administrator restricted to namespace `team` must not create an unrestricted
administrator token. Otherwise the first call to token creation discards the
boundary that the original credential was meant to enforce.

Token creation, listing and revocation, node join-token creation, cluster secret
rotation and image signing now require an unscoped user administrator. These
operations manage cluster-wide credentials or trust; they have no individual
workload target against which we could check an app or namespace restriction.
Scoped administrators can still use workload routes within their own scope.
The bootstrap path remains governed by authentication middleware, and internal
service credentials still cannot use user-management routes.

The regression goes through the router with both app-scoped and
namespace-scoped administrators. Every global management request must return
403 before changing credential state. An unrestricted administrator then creates,
lists and revokes a token through the same router. That positive path matters:
refusing every request would also prevent escalation, but it would leave us
with a rather unhelpful administration API.

### An administrator's lease override needs global authority

Lease inspection and release usually require the exact credential that created
the lease. Operators also need a way to inspect or clean up an abandoned run.
That override now requires an unscoped administrator, just like global
credential management. Being an administrator for one app or namespace does
not permit inspecting or deleting another credential's lease.

Both API paths had failing regressions: inspection returned 200 and release
returned 204 for a scoped outsider. The repaired tests also exercise the two
legitimate paths, an unscoped operator and the exact scoped Deployer who owns
the lease. Ownership and the administrator override are distinct authorities;
a role check alone cannot stand in for both.

### Retiring the extra PEM parser dependency

Rustls already provides PEM parsing through `rustls::pki_types`. The separate
`rustls-pemfile` crate wraps that parser and is now unmaintained, so we use the
underlying API directly and remove the dependency and its audit exception.
The bounded file reader, certificate validation and reload rules stay in place.

`PemObject` is a trait providing parsing functions on certificate and key
types. Importing the trait lets Rust resolve an associated function such as
`CertificateDer::pem_slice_iter(bytes)`. The type before `::` determines which
PEM sections the iterator accepts. For a private key, `next()` produces
`Option<Result<PrivateKeyDer, Error>>`: there might be no key, or parsing the
next key might fail. `transpose()` turns that into
`Result<Option<PrivateKeyDer>, Error>`, letting `?` propagate a parse error
before we report the separate “no private key” case.

The advisory gate fails with the old dependency when its exception is removed.
After migration it passes without that exception. TLS file-reload and client
certificate tests check the behaviour that matters to users; a shorter
lockfile is useful, but it is not evidence that a TLS connection still works.


### Fetch the recipient, keep the private key on the cluster

You have an API token and a manifest to deploy, but no copy of the directory
created by `relish init`. Encrypting a new environment value should not require
copying cluster key files onto your laptop.

`GET /v1/secret/public-key` returns the cluster's active age public recipient
and its generation. Authenticated readers, including scoped readers, can use
it. A public recipient lets you encrypt a value; it cannot decrypt anything.
The endpoint returns a dedicated `SecretPublicKey` struct with just two fields,
instead of serialising the stored `AgeKeypair` that also contains wrapped
private material. A node without an active cluster key returns 503.

This is a read of the node's applied Raft state. A follower can briefly lag a
rotation, so the response does not promise a quorum-fresh generation. Rotation
keeps retiring keys for decryption until finalisation; if a key is retired
between fetching it and applying a manifest, fetch again and re-encrypt.
Ordinary apply and decryption still enforce their existing checks.

The API regression uses a scoped read-only credential, verifies anonymous
refusal after bootstrap, and checks that the response contains exactly the
public recipient and generation. It rotates the key through Raft and proves
that values sealed with each returned recipient decrypt with that generation.
The catalogue goes further: it fetches through TLS, seals two different age
ciphertexts for the same value, deploys them into a leased OCI workload and
reads their plaintext from the actual container. Unicode and newline bytes
must survive; a neighbouring unencrypted value must remain unchanged.

### A health check must distinguish preparation from execution

A deploy can spend seconds pulling an image. Its instance already exists in the
supervisor, and its egress policy already exists in desired state, but no process
can execute yet. Our live security check treated that expected interval as a
lost policy and stopped the deployment. Hosted Linux qualification exposed the
race; a deterministic regression reproduced a Pending instance becoming Stopped.

Both live monitoring and the reconciliation sweep now allow Pending and Preparing
instances to reach the existing pre-start policy installation step. From
Initialising and Starting onwards, an absent binding still fences the workload.
Existing bindings remain checked during preparation too, so preparing a new
instance does not excuse losing an already-installed policy. The deployment
driver must complete policy installation before changing to an execution state.

The test exercises all eight nonterminal lifecycle states through the agent. It
preserves the two preparation states and requires stopping in each execution or
shutdown state without kernel ownership. The privileged pre-start test also
checks the real cgroup map entries and create-before-start ordering. Its failure
now includes deployment errors, and it joins its agent before finishing.

The eight-state regression fails before the repair and passes afterwards with
all 483 Linux Bun tests (one explicit gate). Strict Clippy passes on both
platforms, and all 24 actual privileged eBPF tests pass in 5.90 seconds.
