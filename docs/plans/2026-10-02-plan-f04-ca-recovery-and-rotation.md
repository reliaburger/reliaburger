# Plan: F04, CA recovery and rotation (0.1.4)

*Written 2 October 2026 for [#362](https://github.com/reliaburger/reliaburger/issues/362) (F04), part of [0.1.4: operations security](https://github.com/reliaburger/reliaburger/milestone/8). The [completion plan](archive/2026-09-17-codebase-completion-plan.md#f04--add-supported-ca-recovery-and-rotation-operations) asks to "recover an encrypted CA backup in an isolated cluster, rotate trust without losing valid workloads, and refuse invalid/expired authority. Define any grace policy explicitly." Master-key rotation (G5) is a separate item. So is the council/worker key split (F03b), without which no rotation contains a compromised node.*

## Where we are

**The hierarchy** (`src/sesame/ca.rs`, `src/sesame/init.rs`):
- A root (10 years) signs three intermediates (5 years): Node, Workload and Ingress. Leaf lifetimes are a year for nodes, 90 days for ingress, and 1 hour (renewed at 30 minutes) for workloads. All keys are ECDSA P-256.
- `relish init` writes:
  - the master key;
  - `security-bootstrap.json` (the whole `SecurityState`, intermediate keys wrapped with the master key);
  - `<cluster>-root-ca.age`, the root's private key sealed to the generation-0 cluster age key. Raft never holds the root key (`private_key_wrapped: None`).
- `CertificateAuthority.generation` exists and is never incremented.
- `SecurityState::get_ca(role)` returns the first CA of a role, so two of a role would be ambiguous.

**Trust is single everywhere in the node:**
- the mTLS server's `RootCertStore` holds the one Node CA (the anchor, PKI2);
- the client verifier pins one Node CA and one root;
- `validate_peer` on renewal and join, image keyless signatures (`pickle/signing.rs`) and workload `ca.pem` each use one CA and one root;
- `NodeIdentity` holds one `node_ca_der` and one `root_ca_der`, and `LiveNodeIdentity::replace` refuses a renewal that changes either;
- the ingress resolver is built once at startup.

Only `relish`'s client already accepts a bundle of several roots.

**Backups and recovery:**
- Council backups seal the whole desired state (wrapped keys included) with a key derived from the master key.
- `relish council recover` restores it under the same master key. It doesn't check the CA state it restores: expiry, key against certificate, root against what nodes pin.
- Nothing reads `root-ca.age`; `unseal_with_age` is a primitive with no caller.
- **There's a hazard today:** `relish secret rotate --finalize` drops read-only age keys, and the root backup is sealed to generation 0, which the seal records (`secret_seals`) don't know about. After the first finalised rotation, only an old `security-bootstrap.json` or council backup can open the root backup. R0 below fixes that first.

**Grace:**
- `identity::extend_grace_period` is never called, and couldn't extend a signed certificate anyway.
- `security-sesame.md` §5.3 step 7 describes a 4-hour grace extension that doesn't exist.

**Revocation:**
- The CRL is checked on every handshake, keyed by serial and CA role.
- Ingress serials are random and outside it.
- Nothing in production issues `RevokeCertificate`.

## What we'll build

### R0. Keep the root backup openable (a fix, first)

- **Finalising a cluster-wide secret rotation keeps the generation-0 cluster key**, read-only, because `relish init` seals the root backup to it.
- `relish secret rotate --finalize` says so, and the manual's rotation section says which file it protects.
- **Test:** rotate, re-seal, finalise, and the root backup still opens with what's left in the state.

### R1. Several CAs per role

- A CA gets a state (`Active` or `Retiring { until }`), and `get_ca` becomes `active_ca(role)` plus `trusted_cas(role)`.
- New Raft requests, appended to the enum:
  - `CaRotationBegin { role, ca }`;
  - `CaRotationFinalize { role }`.
- They follow secret rotation's pattern: one rotation per role at a time, idempotent per generation, and a finalise that refuses while anything still depends on the retiring CA.
- **Tests:**
  - stacked rotations are refused;
  - finalise is refused while a live leaf chains to the retiring CA;
  - `generation` increments.

### R2. Trust bundles in every verifier

- `NodeIdentity` carries the trusted Node CAs and roots, and every verifier tries each:
  - the mTLS server's store and the client verifier;
  - renewal and join `validate_peer`;
  - image signatures;
  - the workload `ca.pem`, which becomes a bundle.
- Bun's security refresh pushes the trusted set the way it pushes the CRL, and the ingress resolver reloads when the Ingress CA changes.
- `LiveNodeIdentity::replace` accepts a new trust set when it's what the council's state says, and refuses anything else.
- **Tests:**
  - two nodes, one holding a leaf from the old Node CA and one from the new, complete mTLS both ways during the window;
  - a leaf from a CA that isn't trusted is refused;
  - the trust set reaches a running node without a restart.

### R3. A root backup the operator holds

- `relish ca backup --out <file>`, run on a node with the master key. It writes the root key and certificate plus metadata (cluster, trust domain, fingerprint, expiry), sealed to a passphrase or an operator's age recipient, not to a cluster key that rotates.
- `relish ca verify <file>` checks offline that:
  - the key matches the certificate;
  - it hasn't expired;
  - the fingerprint is the cluster's.
- **Tests:**
  - round trip;
  - a wrong passphrase;
  - a key that doesn't match its certificate;
  - an expired root;
  - another cluster's backup is refused by fingerprint.

### R4. Rotating an intermediate

`relish ca rotate --role node|workload|ingress --root-backup <file>`:
1. **Begin:** the CLI unseals the root locally and signs a new intermediate whose key the cluster generated (the cluster sends a CSR, so the root key never leaves the operator's machine). The council begins the rotation: both CAs are trusted, and new leaves come from the new one.
2. **Re-issue:**
   - workload leaves move within their hour;
   - node leaves are re-issued early, by an ordered renewal of every node;
   - ingress leaves on their next renewal, or on demand.
3. **Finalise:** refused until every node has acknowledged the new trust set and no live leaf chains to the retiring CA (`ca_generation` on node leaves). Then the old CA is retired.

**Tests:**
- unit tests for each step's refusals;
- a cluster test in `tests/` that rotates the Node CA on three nodes while an app serves traffic. No replica restarts, and mTLS never fails.

### R5. Rotating the root

- `relish ca rotate --root --root-backup <file>` creates a new root. The old root cross-signs it, so verifiers that walk to the root (relish, image signatures, the join fingerprint) accept either during the window. Then each intermediate is re-issued under the new root (R4's flow, with the Node CA trust window), and the new root's backup is written as R3 does.
- The join fingerprint changes, and `relish join-token create` prints the new one.
- **Tests:**
  - a cluster test that rotates the root and joins a node with the new fingerprint;
  - an image signed under the old root still verifies until finalise, and is re-signed or refused after it, depending on question 3.

### R6. Restoring into an isolated cluster

- `relish ca restore --root-backup <file>` initialises a new cluster from an existing root, with new intermediates, a new master key and the same trust domain. It refuses an expired, mismatched or foreign authority.
- **The test the completion plan names:** back up, restore into an isolated cluster, verify that a workload certificate from the old cluster chains to the restored root, and refuse an expired backup.

### R7. The grace policy, stated

- A retiring CA stays trusted until the longest leaf it signed could still be valid: a year for Node, 90 days for Ingress, an hour for Workload.
  - R4 shortens the Node window by re-issuing early, and finalise checks that rather than waiting the year out.
- `extend_grace_period` is removed. A certificate's validity is what was signed, and an expired workload certificate means renew or stop.
- `security-sesame.md` §5.3 and §5.8 are rewritten to match.

## Order

R0 now, on its own. Then R1 → R2 → R3 → R4 (intermediates), which is the useful core. R5, R6 and R7 come after. R1 and R2 change the state format (bump `src/compatibility.rs`, no migration).

## Open questions for the maintainer

1. **Where the root key is used.** This plan keeps it off the cluster: the CLI unseals the operator's backup and signs CSRs the cluster made. Is that acceptable (the operator needs the backup file at hand for every rotation), or should the root stay on the council, sealed?
2. **Intermediates first?** R4 before R5 is our recommendation. Node leaves last a year, so R4 has to re-issue them early rather than wait.
3. **Images signed under a retired root.** Should they be re-signed during the window, kept verifiable by keeping the retired root for image verification only, or refused after finalise?
4. **Restore semantics.** Same trust domain and OIDC issuer (workloads' external trust continues), or a fresh identity that only reuses the root?
5. **Backups.** Is a passphrase acceptable as the default seal for `ca backup`, or should it require an operator age recipient?

## Decisions (maintainer, 2 October 2026)

The recommendations were approved as written:

1. **The root key stays with the operator.** The CLI signs CSRs the cluster made, and the root key never sits on the cluster.
2. **Intermediates first.** R1–R4 are 0.1.5; R5–R7 (root rotation, restore, grace policy) come later.
3. **Images signed under a retired root:** that root stays trusted for image verification only, until it expires. A re-sign command comes later.
4. **Restore:** keep the trust domain and OIDC issuer; issue a new master key and new intermediates.
5. **Backups:** a passphrase by default, `--recipient` for an age key.

R0 shipped in 0.1.4 (#472).

R3 (the operator-held root backup, `relish ca backup` and `relish ca verify`) is in 0.1.6.
