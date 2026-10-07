# Ingress follows backend redirects and loses the original status, Location and cookies

Suggested priority: **P1**. Affects **v0.1.4**; verified at `f4757e7789d3672d21f15ca203031d6604d7e11a`.

Verification: Real production proxy with loopback backend reproduced.

### Problem

Wrapper uses reqwest's default redirect policy. A backend's HTTP redirect is followed inside the proxy instead of being returned to the client. The original status, Location and Set-Cookie are lost. Login/OAuth and other redirect-based applications therefore behave incorrectly. Relative redirect requests also bypass a fresh ingress routing decision, while absolute redirects cause the bun to make an outbound request to the redirected origin.

### Evidence

- [src/wrapper/proxy.rs:190–200](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/wrapper/proxy.rs#L190-L200): both the normal and fresh-connection clients use `Client::builder()` without `redirect(Policy::none())`.
- [src/wrapper/proxy.rs:653](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/wrapper/proxy.rs#L653): sends through that client.
- [src/wrapper/proxy.rs:680](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/wrapper/proxy.rs#L680): forwards the final response status, after reqwest has followed any redirects.

### Reproduction

`evidence/network.rs` starts a real production `bind_proxy` and a loopback backend. The backend `/redirect` responds `302`, `Location: /landing`, `Set-Cookie: login=nonce; Path=/`; `/landing` responds `200` and `landing`. The external test client disables redirects.

Actual verified output:

```text
redirect: status=200 OK location=None set-cookie=None body=landing
```
Expected: the ingress client receives the backend's 302, Location and cookie, and the backend receives no `/landing` request until the external client follows it.

### Suggested fix / acceptance

Set `reqwest::redirect::Policy::none()` on both proxy clients. Verify 301/302/303/307/308, relative/absolute locations, cookie preservation and method/body preservation for POST 307/308. An absolute redirect target must not be contacted by the ingress proxy.

### Current implementation snippets

The excerpts below are verbatim from the audited checkout. Links are pinned to that commit.

[src/wrapper/proxy.rs:188–201](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/wrapper/proxy.rs#L188-L201)

```rust
    shutdown: CancellationToken,
) -> Result<BoundProxy, WrapperError> {
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .pool_max_idle_per_host(32)
        .pool_idle_timeout(UPSTREAM_POOL_IDLE_TIMEOUT)
        .build()
        .map_err(|e| WrapperError::ProxyFailed(format!("failed to build http client: {e}")))?;
    let fresh_client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .pool_max_idle_per_host(0)
        .build()
        .map_err(|e| WrapperError::ProxyFailed(format!("failed to build http client: {e}")))?;

```

[src/wrapper/proxy.rs:649–662](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/wrapper/proxy.rs#L649-L662)

```rust
    // connection failure*. A backend that answered (even with a 5xx) may have
    // side effects, so its request is never replayed against another instance;
    // a connection that never opened is always safe to retry (§ retry).
    for (idx, (_cand_id, cand_addr)) in candidates.iter().enumerate() {
        let upstream_uri = match build_upstream_uri(cand_addr, &parts.uri) {
            Some(u) => u,
            None => continue,
        };

        let (upstream_req, body_sent) = upstream.build(&state.client, &upstream_uri);
        let mut sent = tokio::select! {
            biased;
            _ = super::draining::wait_for_termination(&terminate) => return StatusCode::SERVICE_UNAVAILABLE.into_response(),
            result = upstream_req.send() => result,
```

[src/wrapper/proxy.rs:678–690](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/wrapper/proxy.rs#L678-L690)

```rust
            };
        }
        match sent {
            Ok(resp) => {
                let status =
                    StatusCode::from_u16(resp.status().as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
                let mut response = Response::builder().status(status);

                // Copy end-to-end response headers only. Drop hop-by-hop headers
                // and the upstream framing headers (`Content-Length` /
                // `Transfer-Encoding`): the response body streams below, so hyper
                // re-frames it and copying the upstream length would mismatch.
                let conn_tokens = connection_tokens(resp.headers());
```
