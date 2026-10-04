# Registry token verification bypasses bounded Argon2 admission and blocks Tokio workers

Suggested priority: **P1**. Affects **v0.1.4**; verified at `f4757e7789d3672d21f15ca203031d6604d7e11a`.

Verification: Complete synchronous verification call path.

### Problem / impact


Pickle uses synchronous token hashing on Tokio runtime threads for both reads and writes. The API auth path already addresses exactly this resource exhaustion hazard with a cheap shape check, a process-wide four-permit semaphore and `spawn_blocking`. Registry authentication bypasses all three. Even ordinary authenticated OCI requests can block runtime workers; repeated invalid credentials cost one Argon2 verification per stored token and lack the API concurrency bound. Bun hosts scheduling, health, networking and registry on the same Tokio runtime, so registry traffic can interfere with the whole node.

### Verified evidence


- [src/pickle/registry_auth.rs:171](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/pickle/registry_auth.rs#L171): `authenticate_writer` calls synchronous `sesame::auth::authenticate(bearer, &tokens)` inside an async function.
- [src/pickle/registry_auth.rs:214](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/pickle/registry_auth.rs#L214): `authorise_read` does the same. This path is reachable by `GET /v2/` with no upload admission limit.
- [src/sesame/auth.rs:144](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/sesame/auth.rs#L144): `authenticate` calls `token::find_valid_token`; [src/sesame/token.rs:139](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/sesame/token.rs#L139) loops across stored tokens; [src/sesame/token.rs:106](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/sesame/token.rs#L106) performs Argon2 verification synchronously.
- [src/sesame/auth.rs:236](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/sesame/auth.rs#L236): `authenticate_off_lock` is the existing bounded asynchronous verifier. It checks shape, acquires `VERIFY_PERMITS` and uses `spawn_blocking`.
- [src/pickle/api.rs:551](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/pickle/api.rs#L551): four-write admission applies only POST/PATCH/PUT, not GET/HEAD; it is separate from shared token verification anyway.

### Proposed fix / acceptance


Route registry read and write verification through `authenticate_off_lock` with the cloned token snapshot. Keep constant-time internal service-token recognition and existing role/scope checks. Extend the existing shared-permit auth test approach to registry reads and writes: holding all process-wide verification permits must make registry verification wait while unrelated async work proceeds; malformed credentials must be rejected without hash work. Test both Bearer and TLS Basic envelopes and assert unchanged 401/403 decisions.

### Verification limit


No live CPU-exhaustion test was performed. Blocking/concurrency behavior follows directly from the synchronous call chain. Existing registry auth tests confirm the paths but do not assert bounded hashing or timer progress.

### Current implementation snippets

The excerpts below are verbatim from the audited checkout. Links are pinned to that commit.

[src/pickle/registry_auth.rs:167–175](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/pickle/registry_auth.rs#L167-L175)

```rust

    let Some(bearer) = bearer else {
        return Err(WriteDenied::Unauthenticated);
    };
    match crate::sesame::auth::authenticate(bearer, &tokens) {
        Ok(ctx) => {
            // A registry push is a deploy-class mutation.
            if crate::sesame::token::check_role(ctx.role, ApiRole::Deployer).is_ok() {
                Ok(Some(ctx))
```

[src/pickle/registry_auth.rs:208–215](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/pickle/registry_auth.rs#L208-L215)

```rust
    }

    let Some(bearer) = bearer else {
        return Err(WriteDenied::Unauthenticated);
    };
    let tokens = { auth.tokens.read().await.clone() };
    crate::sesame::auth::authenticate(bearer, &tokens).map_err(|_| WriteDenied::Unauthenticated)
}
```

[src/sesame/auth.rs:255–266](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/sesame/auth.rs#L255-L266)

```rust
                "authentication temporarily unavailable".to_string(),
            ));
        }
    };

    let candidate = plaintext.to_string();
    let result = tokio::task::spawn_blocking(move || {
        let _permit = permit;
        authenticate(&candidate, &tokens)
    })
    .await;

```
