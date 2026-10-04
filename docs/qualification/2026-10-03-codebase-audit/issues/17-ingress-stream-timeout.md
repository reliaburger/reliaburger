# Ingress aborts healthy SSE and download streams after 30 seconds

Suggested priority: **P2**. Affects **v0.1.4**; verified at `f4757e7789d3672d21f15ca203031d6604d7e11a`.

Verification: Active 40-second backend stream aborted at 30 seconds.

### Problem

Both Wrapper reqwest clients impose a 30-second total request timeout, which also applies while consuming the response body. A healthy SSE feed or long download is aborted at 30 seconds even if it continues producing data. The streamed-body path explicitly claims support for SSE and large downloads.

### Evidence

- [src/wrapper/proxy.rs:191](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/wrapper/proxy.rs#L191) and `:197`: `.timeout(Duration::from_secs(30))` on both clients.
- [src/wrapper/proxy.rs:702–717](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/wrapper/proxy.rs#L702-L717): SSE/large-download comment and the response-body streaming path.

### Reproduction

`evidence/network.rs` uses real `bind_proxy` with a backend sending one valid `text/event-stream` chunk per second for 40 seconds. The test client reads until EOF/error.

Actual verified output:

```text
stream: error after 30.002430958s, chunks=30, error=error decoding response body
```
Expected: all 40 chunks are delivered and an actively streaming response can remain connected beyond 30 seconds.

### Suggested fix / acceptance

Separate connection/header deadlines from an active body's lifetime; apply any desired stream idle deadline to inactivity, while respecting drain/cancellation. Cover both normal and fresh clients, a stream active beyond 30 seconds, stalled upstreams and bounded cancellation. This is distinct from #369's deferred WebSocket close-handshake/ACME work.

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

[src/wrapper/proxy.rs:700–717](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/wrapper/proxy.rs#L700-L717)

```rust
                }

                // Stream the backend's body instead of buffering it whole, so
                // SSE, gRPC and large downloads flow with backpressure and don't
                // pin the whole response in memory (ING3).
                //
                // The response owns its permit; a bounded upstream pump owns
                // the drain guard and observes cancellation even when the
                // client stops polling its body (ING2/DEP5/§5.5). Only this
                // backend's drain may hold or cancel the stream (T1.7).
                let mut drain_guard = drain_guard;
                if let Some(guard) = &mut drain_guard {
                    guard.keep_only(idx);
                }
                let terminate: Vec<_> = terminate.into_iter().nth(idx).into_iter().collect();
                let stream =
                    guarded_body_stream(resp.bytes_stream(), permit, drain_guard, terminate);
                return response
```
