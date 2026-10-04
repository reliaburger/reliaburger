# GitOps webhook returns 202 on followers but discards the trigger before leader sync

Suggested priority: **P2**. Affects **v0.1.4**; verified at `f4757e7789d3672d21f15ca203031d6604d7e11a`.

Verification: API queue, per-node startup and leader-gated runner traced.

### Problem and unsupported claim


Each council member configured for GitOps exposes a webhook queue and validates a signed delivery locally. The handler enqueues a local nudge and returns `202` saying sync is queued. A follower’s local sync loop consumes that nudge, sees it is not leader and discards it. It never forwards the trigger to the leader.

A stable webhook endpoint can therefore stop providing instant deployment after leadership changes while continuing to acknowledge deliveries successfully. Periodic polling can eventually notice the commit, but accepted webhook delivery does not trigger the promised immediate leader sync ([docs/whitepaper.md:679](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/docs/whitepaper.md#L679)).

### Reproduction / verification


On a configured council with a long poll interval, deliver a correctly signed fresh webhook to a follower. Observe `202` from that follower and no leader fetch before its next poll. Repeat after leader handover with the same fixed endpoint. This is a proposed multi-node regression; no live council was launched for this case.

The parent independently checked all three production stages: [src/bin/bun.rs:2233](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/bin/bun.rs#L2233) creates a queue/runner on every configured council member; [src/bun/api/gitops.rs:20](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/bun/api/gitops.rs#L20) admits only into that local queue; [src/lettuce/runner.rs:64](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/lettuce/runner.rs#L64) receives the signal and then immediately continues on a nonleader. No forwarding path intervenes.

### Fix direction and acceptance


Route accepted deliveries to the current leader or persist a cluster-visible pending sync trigger. Preserve the raw signed payload/header verification and replay/rate guarantees when forwarding; avoid consuming a delivery ID permanently before leader admission succeeds. Return a retryable failure when no coordinator can accept it.

- A valid follower delivery causes prompt leader sync with polling intentionally delayed.
- Leader handover does not silently discard acknowledged deliveries.
- No leader/unreachable leader produces a retryable result unless the nudge is durably retained.
- Replay and rate-limit behavior remain correct across retries and forwarding.

### Existing issue comparison


No matching issue was found. #297 fixes replay registration before local rate admission, not leader routing and acknowledged-but-discarded triggers.

### Current implementation snippets

The excerpts below are verbatim from the audited checkout. Links are pinned to that commit.

[src/bin/bun.rs:2233–2253](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/bin/bun.rs#L2233-L2253)

```rust
    let gitops_webhook_tx =
        if let (Some(gitops), Some(council)) = (config.gitops.clone(), api_council.clone()) {
            let (webhook_tx, webhook_rx) = mpsc::channel::<()>(16);
            if let Some(secret) = gitops.webhook_secret.as_deref() {
                gitops_webhook_validator = Some(std::sync::Arc::new(tokio::sync::Mutex::new(
                    reliaburger::lettuce::webhook::WebhookValidator::new(
                        secret,
                        gitops.webhook_rate_limit,
                    ),
                )));
            }
            reliaburger::lettuce::runner::spawn_gitops_sync(
                council,
                gitops,
                webhook_rx,
                config.storage.data.clone(),
                shutdown.clone(),
            );
            println!("bun: gitops sync loop started");
            Some(webhook_tx)
        } else {
```

[src/bun/api/gitops.rs:67–80](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/bun/api/gitops.rs#L67-L80)

```rust
        // GitHub/Gitea sign the body: `X-Hub-Signature-256: sha256=<hex>`.
        guard.validate(&body, signature, delivery_id.as_deref(), &branch)
    };
    drop(guard);

    match result {
        Ok(_) => {
            permit.send(());
            (
                StatusCode::ACCEPTED,
                Json(serde_json::json!({ "message": "sync queued" })),
            )
                .into_response()
        }
```

[src/lettuce/runner.rs:60–74](https://github.com/reliaburger/reliaburger/blob/f4757e7789d3672d21f15ca203031d6604d7e11a/src/lettuce/runner.rs#L60-L74)

```rust
                _ = shutdown.cancelled() => break,
                _ = ticker.tick() => {}
                signal = webhook_rx.recv() => {
                    if signal.is_none() {
                        break; // sender dropped
                    }
                    // Drain any queued webhook signals so a burst
                    // collapses into a single sync.
                    while webhook_rx.try_recv().is_ok() {}
                }
            }

            if !council.is_leader().await {
                continue;
            }
```
