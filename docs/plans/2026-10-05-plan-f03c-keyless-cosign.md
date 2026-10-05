# Plan: F03c, keyless cosign and Sigstore bundles (later)

*Written 5 October 2026 for [#619](https://github.com/reliaburger/reliaburger/issues/619) (F03c), milestone "Later". [F03a](2026-10-02-plan-f03-upstream-image-trust.md) ships in 0.1.6 with key-based cosign in the classic `.sig` layout only. Its format check (#597) found that the signed public images people run are all signed keyless, and that cosign 3 writes a Sigstore bundle as an OCI referrer by default. The maintainer decided on 5 October 2026 to plan keyless verification for a later release. This is that plan. (F03b, taking the master key's authority off workers, is a separate plan.)*

## Where we are

0.1.6 answers one question: did one of *our* keys sign these bytes?

- **The verifier** (`src/pickle/cosign.rs`) reads the classic layout: the tag `sha256-<hex>.sig` in the image's repository, whose `simplesigning` layers each carry a payload naming a digest and a base64 ECDSA P-256 signature in the `dev.cosignproject.cosign/signature` annotation. `verify_signature` checks the signature with `ring` first, then requires the payload to name the bound digest. It ignores every other annotation.
- **The policy** (`UpstreamTrustRule` in `src/config/node.rs`) has `match`, `require_signatures` and `cosign_keys` (PEM P-256), in `node.toml` only. Startup refuses `require_signatures = true` without keys.
- **The deploy gate** (#611): Bun's loop builds a `pickle::trust::CosignCheck` for an image whose rule wants a signature, and `answer_after_cosign` runs it on its own task with a 60 s deadline. Signatures come through `SignatureSource`: the pull-through cache (`ClusterSource::cosign_signature`, which stores the `.sig` image as a cached tag in the catalogue) or the registry directly. An image without a bound digest is refused.
- **Fixtures** (`tests/fixtures/cosign/`) are real `cosign sign --key` output, made with `--new-bundle-format=false --tlog-upload=false`: no certificate, no Rekor entry.

What's missing is everything a public image needs. A keyless signature has no key to list. Instead it carries a Fulcio certificate, valid for ten minutes, that binds an ephemeral key to an OIDC identity (a GitHub workflow, a Google service account), and a Rekor transparency-log entry proving the signature was logged while that certificate was valid. Checking it means trusting Sigstore's roots, matching the identity against policy, and checking time against the log, not against the node's clock.

### The format check, again (5 October 2026)

We repeated #597's check with `curl` against each registry's API: resolve the tag, then ask for `sha256-<hex>.sig`, the referrers API (`GET /v2/<repo>/referrers/<digest>`) and the referrers fallback tag (`sha256-<hex>`, no suffix).

| Image | Digest | `.sig` | Referrers API | Fallback tag | Signer identity (SAN, issuer) |
|-------|--------|--------|---------------|--------------|-------------------------------|
| `cgr.dev/chainguard/static:latest` | `fe55470f…` | yes, 1 layer | supported, empty | none | `https://github.com/chainguard-images/images/.github/workflows/release.yaml@refs/heads/main`, GitHub Actions |
| `gcr.io/distroless/static-debian12:latest` | `d75cdd72…` | yes, 1 layer | supported, empty | none | `keyless@distroless.iam.gserviceaccount.com`, Google |
| `registry.k8s.io/kube-apiserver:v1.34.1` | `b9d7c117…` | yes, 2 layers | supported, empty | none | `krel-staging@k8s-releng-prod.iam.gserviceaccount.com`, Google |
| `ghcr.io/fluxcd/source-controller:v1.7.0` | `233e0a1e…` | yes, 1 layer | unsupported (404) | none | `https://github.com/fluxcd/source-controller/.github/workflows/release.yml@refs/tags/v1.7.0`, GitHub Actions |
| `ghcr.io/sigstore/cosign/cosign:v3.0.2` | `b29487e4…` | yes, 2 layers | unsupported (404) | see below | keyless, plus one layer signed with a key |
| `ghcr.io/sigstore/cosign/cosign:v3.1.3` | `9e5c2f2e…` | **no** | unsupported (404) | **2 bundles** | `keyless@projectsigstore.iam.gserviceaccount.com`, Google; plus one key-signed bundle |
| `ghcr.io/kyverno/kyverno:v1.15.2` | `16e71077…` | no (`.att` only) | unsupported (404) | none | attestations only |
| `docker.io/library/nginx:latest` | `abe47724…` | no | supported, empty | none | not signed |

What that tells us:

1. **The classic layout is still most of what's out there,** and it's all keyless. Each `.sig` layer carries `dev.sigstore.cosign/certificate` (the Fulcio leaf, PEM), `dev.sigstore.cosign/chain` (intermediate and root) and `dev.sigstore.cosign/bundle`: a Rekor *signed entry timestamp* (SET) plus the entry it covers (`body`, `integratedTime`, `logIndex`, `logID`). There's no inclusion proof in the classic layout, only the SET.
2. **The bundle has arrived.** cosign's own release image switched between v3.0.2 and v3.1.3. Each signature is an OCI artifact manifest with `artifactType: application/vnd.dev.sigstore.bundle.v0.3+json`, an empty config, one layer holding the bundle JSON, and a `subject` naming the signed image index. The bundle is a DSSE envelope around an in-toto statement (`predicateType: https://sigstore.dev/cosign/sign/v1`) whose `subject` names the same digest. It carries the Fulcio leaf alone (v0.3 drops the chain), a Rekor entry with *both* the SET and an inclusion proof with a signed checkpoint, and an RFC 3161 timestamp from `timestamp.sigstore.dev`.
3. **GHCR has no referrers API,** so cosign writes the fallback tag `sha256-<hex>`, an index listing the bundle manifests. Docker Hub, `gcr.io`, `cgr.dev` and `registry.k8s.io` answer the API.
4. **GHCR answers a missing fallback tag with the image itself.** Asking `ghcr.io/sigstore/cosign/cosign` for `sha256-b29487e4…` (v3.0.2, which has no bundles) returned the v3.0.2 image index, with that digest. A reader that trusts "whatever the fallback tag returns is a referrers list" would read a multi-platform image as five referrers. Every referrer must be checked for `artifactType` and a `subject` equal to the bound digest.
5. **Certificates outlive nothing.** The Kubernetes certificate expired on 9 September 2025, ten minutes after it was issued. Checking validity against the node's clock would refuse every keyless image ever published, so validity must be checked at the time Rekor (or a timestamp authority) vouches for.
6. **Identities vary per release.** Flux's SAN names the tag (`@refs/tags/v1.7.0`), so an exact match would need editing every upgrade. A policy needs a prefix form at least.
7. **Key-signed bundles exist too:** one of cosign v3.1.3's two bundles has `verificationMaterial.publicKey` instead of a certificate. That's what `cosign sign --key` writes by default from cosign 3, so 0.1.6's key-based users meet the bundle as soon as they upgrade cosign.

The book's ch10 table, written for #597, says `ghcr.io/sigstore/cosign/cosign` publishes `.sig`; that was true of v2.4.1 and isn't of v3.1.3. K5 corrects it.

## What we'll build

### K0. A spike on the crate, with a decision gate

Before any production code, a two-day spike on a branch that adds `sigstore-verify` and `sigstore-trust-root` (see "Crates" below) and verifies the checked-in fixtures from K1 offline: the Chainguard and Kubernetes classic signatures converted to bundles (K3), and cosign's v3.1.3 bundle as is. It records the new crates, the clean build time and the release binary's growth. If the classic conversion doesn't verify, or the cost is out of line, we stop and bring the numbers back to the maintainer before K2.

### K1. Fixtures

Real signatures, fetched once with `curl` and checked in under `tests/fixtures/sigstore/`, so every test runs offline:

- **Classic keyless:** the `.sig` manifest and payload blobs for one pinned digest each of `cgr.dev/chainguard/static` and `registry.k8s.io/kube-apiserver:v1.34.1` (two layers, and a certificate long expired).
- **Bundle keyless:** cosign v3.1.3's fallback-tag index, both bundle manifests and both bundle blobs.
- **Trust roots:** the Sigstore public-good `trusted_root.json` as of the fetch date, and a second, unrelated root (Sigstore's staging root) for the "wrong root" tests.
- **The GHCR quirk:** the image index GHCR returns for a missing fallback tag.
- A `README.md` in the directory says where each file came from, the date, and the `curl` commands, so the fixtures can be refreshed.

### K2. Identity policy and trust roots in `node.toml`

Each rule takes keyless identities alongside `cosign_keys`. A signature is accepted if it verifies under any of them.

```toml
[images.trust_policy.sigstore]
# Optional. Without it, the Sigstore public-good root built into this release.
trusted_root = "/etc/reliaburger/sigstore/trusted_root.json"

[[images.trust_policy.upstream]]
match = "cgr.dev/chainguard/*"
require_signatures = true

[[images.trust_policy.upstream.keyless]]
certificate_identity = "https://github.com/chainguard-images/images/.github/workflows/release.yaml@refs/heads/main"
certificate_oidc_issuer = "https://token.actions.githubusercontent.com"

[[images.trust_policy.upstream]]
match = "ghcr.io/fluxcd/*"
require_signatures = true

[[images.trust_policy.upstream.keyless]]
# A trailing * is a prefix, as in `match`; anything else is exact.
certificate_identity = "https://github.com/fluxcd/source-controller/.github/workflows/release.yml@refs/tags/*"
certificate_oidc_issuer = "https://token.actions.githubusercontent.com"
```

- **Both fields are required,** and the issuer is always exact. An identity without an issuer is meaningless: anyone can get a certificate for `keyless@…` from an issuer they control, if the verifier doesn't pin the issuer.
- **Identity** is the certificate's SAN (a URI for workflows, an email for service accounts), compared as a string. The issuer is the Fulcio extension `1.3.6.1.4.1.57264.1.8` (falling back to the older `.1.1`).
- **Startup refuses** a rule with `require_signatures = true` and neither keys nor identities, a keyless entry missing either field, a bare `*` identity, and a `trusted_root` file that doesn't parse. As today, a typo refuses the whole config rather than quietly shrinking the trusted set.
- **Trust roots stay off the API.** Identities and the trusted root live in `node.toml`, never in Raft or an API request, for the same reason as `cosign_keys` (`security-sesame.md` §5.10): an API token mustn't be able to widen what a node trusts.

**The trusted root.** Sigstore publishes `trusted_root.json` (Fulcio CAs, Rekor log keys, CT log keys, timestamp authorities, each with a `validFor` window) through a TUF repository at `tuf-repo-cdn.sigstore.dev`. The plan:

- **Built in:** each release embeds the public-good `trusted_root.json` current at release time. `sigstore-trust-root` carries one; we pin our own copy in the repository so the bytes are reviewed, and the release checklist refreshes it with the TUF client in `sigstore-trust-root`'s `update-embedded-roots` example (run by a person, not by nodes).
- **Override:** `trusted_root` in `node.toml` points at a file, for a newer root than the release's, a private Sigstore deployment, or an air-gapped site that fetched it elsewhere.
- **Rotation:** Sigstore rotates by *adding* entries with new `validFor` windows (the current root has two Rekor logs, `rekor.sigstore.dev` and `log2025-1.rekor.sigstore.dev`, and two Fulcio CAs). Old signatures keep verifying under old entries. A node with an older root fails closed on signatures from a newer log or CA, naming the log ID it doesn't know, and the fix is the override file or an upgrade.
- **No TUF client on nodes in F03c.** A node refreshing TUF on its own needs the network, a writable cache, and a story for the frequently expiring timestamp and snapshot metadata, all for keys that change a few times a year. See open question 2.

### K3. Keyless verification of the classic `.sig` layout

The fetch is today's; the verification grows. For each `simplesigning` layer:

1. **With a certificate annotation**, build a v0.1 Sigstore bundle from the layer: `messageSignature` (the signature over the payload bytes), `x509CertificateChain` (the leaf and chain annotations), and one Rekor entry from `dev.sigstore.cosign/bundle` (`canonicalizedBody`, `integratedTime`, `logIndex`, `logId`, and the SET as `inclusionPromise`). That's the shape cosign itself verifies a classic signature as, and it lets one verifier serve both layouts.
2. **Verify the bundle** against the trusted root, with the payload bytes as the artefact:
   - the leaf chains to a Fulcio CA in the root whose `validFor` covers the signing time;
   - the leaf's embedded SCT verifies under a CT log key in the root;
   - the SET verifies under the Rekor key named by `logId`, over the canonical entry, and the entry's body names this signature, this certificate and this payload's hash;
   - the leaf was valid (`notBefore` ≤ t ≤ `notAfter`) at t = `integratedTime`, the time the SET signs. The node's clock plays no part;
   - the signature verifies under the leaf's key.
3. **Match the identity:** the leaf's SAN and issuer against the rule's `keyless` entries.
4. **Then parse the payload,** as today, and require `critical.image.docker-manifest-digest` to equal the bound digest.

A layer without a certificate goes through 0.1.6's key path unchanged, so a `.sig` image mixing both (cosign v3.0.2's) works. The refusal lists each layer's reason, as `NotVerified` does today, with the identity found when it didn't match: "signed by `https://github.com/evil/…`, issuer GitHub Actions; this rule trusts …" is the line an operator needs.

### K4. Sigstore bundles as OCI referrers

Find the bundles for a bound digest:

1. **Referrers API:** `GET /v2/<repo>/referrers/<digest>?artifactType=application/vnd.dev.sigstore.bundle.v0.3%2Bjson`. A 404 means the registry doesn't support it, so:
2. **Fallback tag:** fetch the manifest at tag `sha256-<hex>` (OCI distribution 1.1's referrers tag schema).
3. **Filter, never trust the listing:** keep descriptors whose `artifactType` is a Sigstore bundle media type we read (v0.1 to v0.3), fetch each manifest by digest, and require its `subject.digest` to equal the bound digest. A fallback "tag" that is really the image (the GHCR case) has no bundle descriptors and yields nothing. Each manifest's single layer is the bundle; cap its size (bundles are 4 to 8 KiB; refuse past 64 KiB, as payloads are capped today) and the number of referrers read (the same 1024 as `MAX_SIGNATURE_LAYERS`).
4. **Verify the bundle,** with the bound digest as the artefact: the same checks as K3, plus
   - **DSSE:** the envelope's signature over the pre-authentication encoding, and the in-toto statement's `subject` naming the bound digest, with `predicateType` `https://sigstore.dev/cosign/sign/v1`. Attestations (SLSA provenance, SBOMs) also live as bundles; they aren't signatures and don't count;
   - **inclusion proof:** an RFC 6962 audit path from the entry's leaf hash to the checkpoint's root hash, and the checkpoint's signature under the log key;
   - **signing time:** the SET's `integratedTime` and, where present, the RFC 3161 timestamp's time, each verified under the root; a Rekor v2 entry has no SET, so its time comes from the timestamp alone.
   - a key-signed bundle (`publicKey` instead of a certificate) verifies under the rule's `cosign_keys`.
5. **Where it's fetched from:** the referrers list straight from the registry every time, since it changes when someone signs again; manifests and bundle blobs by digest through the pull-through cache when it's on, since they're content-addressed and the cache already stores digest-pinned blobs.

**Order of looking:** a rule accepts an image when *any* signature in *either* layout verifies. Bun asks for referrers and `.sig` in parallel, under the same 60 s deadline. With neither present, the refusal names both places it looked.

**Air-gapped:** verification itself needs no network. The bundle carries its certificate, SET, inclusion proof, checkpoint and timestamp, and the trusted root is a file. What does need the network is fetching the bundle, which is the same as fetching the image: a mirror that copies referrers (`cosign copy`, `oras cp -r`) or the `.sig` tag alongside the image works.

### K5. Docs that match

- Manual `10_security.md` "Signed images": keyless rules, finding a signer's identity (`cosign verify` prints it, and the plan's table shows four), the trusted root and its override, and what a refusal looks like.
- Book ch10: a section on keyless trust, written around the fixtures. Why the certificate is only ten minutes long, why time comes from the log, what an inclusion proof shows. The #597 table gains the bundle findings above.
- `registry-pickle.md` §5.6 and `security-sesame.md` §5.10: the Sigstore trusted root as a node-held trust root, alongside cosign keys.
- `cosign.rs`'s module docs: the scope line that says keyless is out of scope.

## Crates

The verification above is a lot of cryptographic surface: X.509 chains on P-256 and P-384, SCTs (which mean re-encoding the leaf's TBS without its SCT extension), RFC 8785 canonical JSON for SETs, RFC 6962 Merkle proofs, signed-note checkpoints, CMS for RFC 3161 timestamps, DSSE, and three bundle versions. Done ourselves, that's roughly two to three thousand lines of security-critical parsing, each part with a conformance story to get right, plus keeping up with Sigstore's format changes (Rekor v2 arrived in 2025, bundle v0.3 is cosign 3's default).

There are two Rust implementations, both from the Sigstore organisation:

| | `sigstore` (sigstore-rs) | `sigstore-verify` + `sigstore-trust-root` (sigstore-rust) |
|---|---|---|
| Version | 0.14.0, May 2026; "an experimental crate" | 0.14.0, 29 September 2026; first release November 2025, fourteen minor versions since |
| Scope | everything, including cosign's OCI layout | verification only; split crates (`-types`, `-bundle`, `-crypto`, `-merkle`, `-rekor`, `-tsa`, `-trust-root`, `-tuf`) |
| Formats | classic cosign, bundles | bundles v0.1 to v0.3, Rekor v1 and v2, SCTs, RFC 3161, identity policy |
| Crypto | `aws-lc-rs` | `aws-lc-rs` |
| Pulls in | `oci-client` 0.17 (we have `oci-distribution` 0.11), `openidconnect`, `tough`, `reqwest` 0.13, `scrypt`, … | `x509-cert`, `der`, `spki`, `cms`, `jiff`, `tls_codec`, `serde_json_canonicalizer`; TUF and `tokio` only behind the default `tuf` feature |
| MSRV | not declared | 1.86 (ours is 1.97) |
| Audit | none we could find | none we could find |

What the dependency tree already holds matters here: `aws-lc-rs` (through `rustls` and `object_store`), `rustls-webpki` 0.103, `reqwest` 0.13 and `tracing` are all in our `Cargo.lock` today. So `sigstore-verify` with `sigstore-trust-root`'s default features off adds about twenty small crates to a 712-package lock, no new C library, and no network code. Its policy API (`VerificationPolicy`, `IdentityMatcher`) matches identities exactly; our prefix form runs on top, from the identity and issuer it returns.

**Recommendation: `sigstore-verify` and `sigstore-trust-root` (no default features), pinned to an exact version,** with our own code for what's ours: fetching the `.sig` and referrers from registries and the cache, converting a classic layer into a v0.1 bundle, the identity prefix match, and requiring the signed digest to be the bound one. CLAUDE.md's "don't reinvent what exists" applies squarely: this is the Sigstore project's own verifier, and the simplest code we could own is a thin layer over it. The old `sigstore` crate is the wrong choice: it calls itself experimental, duplicates our OCI client and brings an OIDC stack we'd never call.

The risks, and what we do about them:

- **It's young and pre-1.0.** We pin `=0.14.x`, upgrade on purpose, and K1's real-signature fixtures are the regression net that makes an upgrade safe to take.
- **No published audit.** Neither implementation has one we could find. The K0 spike reads the verification path (`verify.rs` and `verify_impl/`, about 3,200 lines) and records anything it skips, and the weekly `make audit` (RustSec) covers the new crates like every other.
- **Two crypto backends.** Our code uses `ring`; the crate uses `aws-lc-rs`, which we already build. That costs nothing new, but it's worth saying in the book.

## Tests first

- **Policy (K2):** table tests for identity matching (exact, prefix, a prefix that must not match a longer path, email against URI SANs, issuer always exact); startup refusals for each malformed rule, a bare `*`, and an unreadable `trusted_root`; the built-in root used when there's no override.
- **Classic keyless (K3), against the K1 fixtures, offline:**
  - Chainguard and Kubernetes signatures verify under the fixture root with their real identities;
  - the Kubernetes one verifies even though its certificate expired a year before the test runs;
  - refusals: another identity, another issuer, the right identity with the issuer of a different provider, the staging root instead of production, a payload naming another digest, a flipped byte in the payload, the signature, the SET, and the `integratedTime` (which breaks the SET, so the time can't be moved without detection);
  - a mixed `.sig` image (a keyed layer and a keyless one) accepted by either kind of rule.
- **Bundles (K4), offline:**
  - cosign v3.1.3's keyless bundle verifies via the fallback tag, and via a stand-in registry that answers the referrers API with the same descriptors;
  - refusals: a referrer whose `subject` is another digest, a DSSE statement whose subject differs from the manifest's `subject`, an attestation bundle (another `predicateType`), a flipped hash in the inclusion proof, a checkpoint signed by an unknown key, a bundle past the size cap, more referrers than the cap;
  - the GHCR quirk: the fallback tag answering with the image index yields no bundles and a refusal naming both places looked.
- **Time and roots, synthetic:** cases real data can't express, built with `rcgen` and a test trusted root: a leaf whose window doesn't cover `integratedTime`, a CA entry whose `validFor` has ended before the signing time, and a Rekor log ID the root doesn't list.
- **End to end:** `tests/suite/pickle_cluster.rs` serves the fixtures from the in-process registry and checks both layouts, directly and through the pull-through cache; Bun agent tests deploy a keyless-signed image under a matching rule and refuse it, by name and with the signer's identity, under a rule for another identity.

## Order and size

K0 first (two days), then K1 and K2 together (about three days), K3 (about a week), K4 (about a week), and K5 alongside each, as with F03a: one PR per step, stacked, each with its book section. Three to four weeks in all. K3 before K4 because the classic layout is still what most public images publish; K4 can't wait much longer, since cosign 3 users publish nothing else.

## Compatibility

The new settings are node configuration (`node.toml`), which isn't wire or durable state, and `CosignCheck` never leaves the node. Bundles and their manifests go through the cache as digest-pinned entries, which the catalogue already stores. So the plan as written changes no format and needs no `protocol` or `state` bump. If the maintainer prefers the cache to hold referrer lists too (open question 4), that's a new kind of catalogue entry and bumps `state`.

## Open questions for the maintainer

1. **The crate.** Take `sigstore-verify` (the recommendation), or implement the minimum ourselves on `ring` and `x509-parser` and accept the two to three thousand lines? The K0 spike reports the real cost before anything is merged.
2. **Refreshing the trusted root.** Built-in plus a `node.toml` file (this plan), or should nodes run a TUF client and refresh it themselves? The alternative middle ground is a command (`reliaburger sigstore fetch-root`) that does the TUF update where there's network and writes the file.
3. **Identity patterns.** Exact plus a trailing-`*` prefix (this plan, matching `match`), or full regular expressions as cosign's `--certificate-identity-regexp` allows? Regexes are more expressive and easier to get dangerously wrong.
4. **Referrer lists and the cache.** Read referrers straight from the registry every time (this plan), or cache the list too, so nodes behind the cache never ask upstream, at the cost of a `state` bump and staleness when an image is re-signed?
5. **Requiring transparency.** This plan requires a Rekor entry (SET or inclusion proof) for every keyless signature, as cosign does. Should a key-signed bundle without one (`--tlog-upload=false`, what 0.1.6's fixture uses) still pass, as it does today?
6. **More certificate checks.** Fulcio certificates also carry the source repository, ref and workflow as extensions (`1.3.6.1.4.1.57264.1.12` and friends). Match on those too in F03c, or leave them for when someone asks?
