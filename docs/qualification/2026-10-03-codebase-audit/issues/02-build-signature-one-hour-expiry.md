# Build signer expiry breaks later builds and redeployment of cluster-signed images after one hour

Suggested priority: **P1**. Affects **v0.1.4**; verified at `f4757e7789d3672d21f15ca203031d6604d7e11a`.

Verification: Full source path; existing clock-injected expiry test.

### Problem / impact


The built-in code-signing identity uses the same one-hour certificate lifetime as runtime workload identities. The leader caches that identity forever without checking expiry or rotating it. Every image signature is then checked against certificate validity at current wall-clock time.

Within one hour after the namespace's first build signer is provisioned, images built under it stop passing `require_signatures` enforcement. Later builds on the same leader use the expired cached signer and fail the local signature self-check. Already built images cannot be newly deployed, rescheduled to a replacement worker, or restarted through a fresh deploy after expiry. Restarting the leader may mint a new signer for later builds but does not repair signatures already embedded in existing image metadata. This affects cluster-generated keyless signatures; externally signed images have no leaf certificate and are unaffected.

### Verified evidence


- [src/bun/build_runner.rs:937](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/bun/build_runner.rs#L937): `get_or_provision_build_signer` returns any existing cached identity without expiry inspection. No refresh/removal path exists for this cache.
- [src/bun/build_runner.rs:946](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/bun/build_runner.rs#L946): provisioning calls `CouncilNode::sign_workload_csr` with `CertUsage::CodeSigning`.
- [src/council/node.rs:525](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/council/node.rs#L525): all usage types go through `identity::validate_and_sign_csr`.
- [src/sesame/identity.rs:185](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/sesame/identity.rs#L185): `params.not_after = now + WORKLOAD_CERT_LIFETIME`; `WORKLOAD_CERT_LIFETIME` at line 21 is 3,600 seconds and the function does not distinguish signing lifetime.
- [src/bun/build_runner.rs:994](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/bun/build_runner.rs#L994): `sign_pushed_image` verifies locally against current trust before attaching the signature, so expired cache reuse produces a signing failure.
- [src/pickle/signing.rs:280](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/pickle/signing.rs#L280): keyless verification passes `SystemTime::now()` to `verify_keyless_at`.
- [src/pickle/signing.rs:318](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/pickle/signing.rs#L318): verification validates every certificate's lifetime at that current time.
- [src/meat/scheduler.rs:432](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/meat/scheduler.rs#L432): enforcement calls that verifier, and [src/bun/agent/launch.rs:292](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/bun/agent/launch.rs#L292) repeats it before node launch.
- [src/pickle/signing.rs:689](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/pickle/signing.rs#L689): existing `verify_keyless_rejects_an_expired_chain` explicitly asserts expiration makes the previously valid signature invalid. It uses ten years, masking the production leaf's actual one-hour lifetime.
- Existing cache test `get_or_provision_build_signer` reuse is immediate only, with no expiry scenario ([src/bun/build_runner.rs:1903](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/bun/build_runner.rs#L1903)).

### Proposed fix / acceptance


Give artifact signing an explicit lifecycle and durable verification semantics separate from runtime mTLS leaves. Renew the cached signing identity before it expires, and choose a supported strategy that keeps previously signed image digests deployable after leaf expiry while respecting revocation and rejecting signatures made without valid authority. Simply checking the existing unsigned `signed_at` value is not sufficient evidence of historical signing time; use an authenticated timestamp/attestation or another well-defined trust mechanism. Alternatively, a longer-lived dedicated signing credential plus documented maintenance policy could be an interim solution, with expiry handling still required.

Inject clock/expiry in regression tests: provision signer, advance just beyond its one-hour validity, request another build/signature, and require fresh valid signing authority; sign an image before expiry, verify/deploy it after the old leaf expires, and require the supported retained-artifact behavior. Revoked and cryptographically invalid signatures must still be refused. Cover cache reuse, leader restart, build tracking terminal state, and image redeploy/reschedule.

### Verification limit


No one-hour live cluster soak was run. Exact expiry follows directly from the issued leaf's hard-coded lifetime, cached reuse and the current-time verifier. The existing injected-clock expired-chain test demonstrates the relevant refusal semantics.

### Current implementation snippets

The excerpts below are verbatim from the audited checkout. Links are pinned to that commit.

[src/bun/build_runner.rs:930–939](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/bun/build_runner.rs#L930-L939)

```rust
    cache: &tokio::sync::Mutex<HashMap<String, BuildSigner>>,
    council: &crate::council::CouncilNode,
    trust_domain: &str,
    namespace: &str,
    node_name: &str,
) -> Result<BuildSigner, String> {
    let mut guard = cache.lock().await;
    if let Some(existing) = guard.get(namespace) {
        return Ok(existing.clone());
    }
```

[src/sesame/identity.rs:19–22](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/sesame/identity.rs#L19-L22)

```rust

/// Workload certificate lifetime: 1 hour.
pub const WORKLOAD_CERT_LIFETIME: Duration = Duration::from_secs(3600);

```

[src/sesame/identity.rs:182–186](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/sesame/identity.rs#L182-L186)

```rust
    // Exact validity window: a one-hour certificate is valid for one hour
    // (plus the skew backdate), not until midnight.
    params.not_before = time::OffsetDateTime::from(now - CLOCK_SKEW_BACKDATE);
    params.not_after = time::OffsetDateTime::from(now + WORKLOAD_CERT_LIFETIME);

```

[src/pickle/signing.rs:270–282](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/pickle/signing.rs#L270-L282)

```rust
    sig: &ImageSignature,
    digest: &Digest,
    root_ca_cert_der: &[u8],
    crl: Option<&crate::sesame::types::Crl>,
) -> Result<(), SigningError> {
    verify_keyless_at(
        sig,
        digest,
        root_ca_cert_der,
        crl,
        std::time::SystemTime::now(),
    )
}
```

[src/pickle/signing.rs:315–319](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/pickle/signing.rs#L315-L319)

```rust
    // Full chain: every adjacent signature + issuer binding, chaining to the
    // trust anchor, and every cert valid at `at`.
    crate::sesame::cert::validate_chain_at(chain, root_ca_cert_der, at)
        .map_err(|e| SigningError::ChainVerifyFailed(e.to_string()))?;

```
