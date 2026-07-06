# Phase 13 Implementation Plan: Relish TUI ("A Room with a View")

**Date:** July 2026. **Executor note:** this plan is prescriptive; follow it step by step. All paths are relative to the repo root. Verified against the codebase as of 2026-07-06 (`src/bin/relish.rs` ~1292 lines, `src/bun/api.rs` ~2894 lines, no ratatui/crossterm/tungstenite deps present, `insta` in dev-deps but unused).

## Scope decisions (already made — do not relitigate)

1. Entry points: both `relish tui` and bare `relish` (no arguments) launch the TUI.
2. The `:` command palette is navigation-only (no CLI passthrough this phase).
3. This phase includes thin server endpoints: `GET /v1/events` (backed by a new in-memory EventStore) and `GET /v1/jobs`.
4. Live streaming uses WebSocket (`/v1/ws/logs/...`, `/v1/ws/events`); the existing SSE endpoints stay untouched for the CLI.

## 0. Verified ground truth (trust this; do not re-derive)

- **CLI today**: `src/bin/relish.rs` has `struct Cli { output, token, #[command(subcommand)] command: Command }` — the subcommand is **required**, so bare `relish` currently errors. There is no `tui` subcommand.
- **Client**: `BunClient` in `src/relish/client.rs` (reqwest, bearer token via `resolve_token()` → `--token` flag then `RELIABURGER_TOKEN` env, base URL `http://127.0.0.1:9117`, `default_local()`). Errors: `RelishError` (thiserror) in `src/relish/mod.rs`.
- **API**: axum 0.8 router in `src/bun/api.rs::router()` (many params, `#[allow(clippy::too_many_arguments)]`). Protected routes sit behind `crate::sesame::auth::auth_middleware` via `route_layer`. Handlers reach the agent via `AgentCommand` over `state.cmd_tx` + `oneshot` (see `gather_statuses()`, api.rs ~1213).
- **Endpoints the TUI uses as-is**: `/v1/status` (`Vec<InstanceStatus>`), `/v1/status/{app}/{ns}`, `/v1/cluster/nodes` (`Vec<NodeStatus>`), `/v1/cluster/council` (`CouncilStatus`), `/v1/routes` (`Vec<RouteInfo>`: `host, path, app_name, healthy_backends, total_backends, websocket`), `/v1/alerts` (`{"alerts": [AlertStatus]}`), `/v1/deploys/history/{app}`, `/v1/metrics/app/{app}/{ns}`, `/v1/logs/{app}/{ns}?follow=true` (SSE — untouched).
- **Key types**: `InstanceStatus { id, app_name, namespace, state: String, restart_count, host_port, pid }`, `NodeStatus { node_id, address, state, incarnation, is_council, is_leader, labels }`, `CouncilStatus { members, leader, term, last_applied_log, app_count }` — all in `src/bun/agent.rs`.
- **No event store exists** anywhere. Ketchup stores only log lines (`LogEntry { timestamp, stream, line }`). We build the ring buffer (§4).
- **Jobs have no read API**: the agent runs `[job.*]` workloads (`WorkloadInstance.is_job` in `src/bun/supervisor.rs`; fields `app_name, namespace, state: ContainerState, restart_count, created_at: Instant, is_job, image`). `/v1/batch` POST returns 501. `BatchTracker` (`src/meat/batch_tracker.rs`) is lib-only, NOT wired — do **not** build `/v1/jobs` on it; read supervisor instances.
- **`AgentCommand::FollowLogs`** exists (`src/bun/agent.rs:100`), used by the SSE follow branch (`src/bun/api.rs:403`) — the WS log handler mirrors this.
- **TestHarness** (`tests/integration.rs:20`): real `BunAgent` + `api::router()` on an ephemeral port, `BunClient` against it, `CancellationToken` shutdown in `Drop`. Base for TUI integration tests.
- **Conventions**: tests first; `make ci` (fmt-check, clippy -D warnings, test) before every commit; no `unwrap` in production code; thiserror; tokio-only sync primitives; CancellationToken shutdown; British English prose; the book chapter `docs/book/13-a-room-with-a-view.md` (currently a stub) is written alongside each stage; `docs/progress.md` updated per stage; ask the user before each commit.

## 1. Crate and dependency changes

`Cargo.toml`:

```toml
[dependencies]
# change existing line:
axum = { version = "0.8", features = ["ws"] }
# add:
ratatui = "0.29"
crossterm = { version = "0.28", features = ["event-stream"] }
tokio-tungstenite = "0.26"
```

Why these versions:

- **axum `ws`**: not in the default feature set; enables `axum::extract::ws::{WebSocketUpgrade, WebSocket, Message}`.
- **ratatui 0.29**: stable line; its crossterm backend pins **crossterm 0.28**, so we pin 0.28 explicitly to keep `KeyEvent`/`Event` the same types ratatui's `CrosstermBackend` uses. Do NOT use ratatui 0.30-alpha.
- **crossterm `event-stream`**: provides `crossterm::event::EventStream` (a futures Stream) so input joins `tokio::select!` without a blocking thread.
- **tokio-tungstenite 0.26**: matches the tungstenite version axum 0.8's `ws` feature uses, keeping one copy in the tree. No TLS feature needed (`ws://` to localhost). Connect via `connect_async` on a hand-built request so we can attach `Authorization: Bearer` (`IntoClientRequest`).
- **insta**: already present in dev-deps; use plain string snapshots.

After editing: `cargo build`, then `cargo tree -d | grep -i tungstenite` — exactly one tungstenite version. If axum has since moved to 0.27, match it (this is the one place the executor may adjust a version; re-verify with `cargo tree -d`).

## 2. Module layout

New module `src/relish/tui/` (add `pub mod tui;` to `src/relish/mod.rs`). One responsibility per file; nothing over ~350 lines.

```
src/relish/tui/
  mod.rs           # pub async fn run() -> Result<(), RelishError>; wires guard + event loop
  terminal.rs      # TerminalGuard: RAII raw-mode/alternate-screen + chained panic hook
  app.rs           # TuiApp state, ViewStack ops, update(msg) reducer — NO ratatui imports
  msg.rs           # Msg, DataUpdate, StreamItem enums — the only inter-task vocabulary
  keys.rs          # pure fn key_to_msg(view, mode, KeyEvent) -> Option<Msg>; KEYBINDINGS const
  data.rs          # DataProvider trait, ProviderError, HttpDataProvider (wraps BunClient), pollers
  stream.rs        # WS follow tasks (logs, events): connect, forward into Msg channel, reconnect
  palette.rs       # PaletteState + parse_command(":apps" | ":logs <app>" | ":quit" | ...)
  theme.rs         # style constants, severity colours
  fixtures.rs      # TestScenario mock data + MockDataProvider (used by tests)
  views/
    mod.rs         # pub fn render(frame, &TuiApp): size guard + dispatch on view_stack.last(); status bar
    dashboard.rs   # header, apps table, nodes strip, recent events, alerts
    apps.rs        # apps list view
    app_detail.rs  # tabs: Overview/Instances/Logs/Metrics/Deploys/Config
    nodes.rs       # nodes list + node detail
    jobs.rs        # jobs list + job detail
    events.rs      # event stream view (+ filter bar)
    logs.rs        # multiplexed log view (+ follow toggle, filter)
    routes.rs      # routes list + route detail
    search.rs      # search view
    help.rs        # keybinding reference rendered from keys::KEYBINDINGS
    widgets.rs     # shared: styled table builder, "terminal too small" screen, header/status bar
```

Server side: new `src/bun/events.rs` (+ `pub mod events;` in `src/bun/mod.rs`), edits to `src/bun/agent.rs`, `src/bun/api.rs`, `src/bin/bun.rs`.

**Hard rule**: `app.rs`, `msg.rs`, `keys.rs`, `palette.rs`, `data.rs` must not import ratatui — state and logic stay renderer-free so unit tests never need a terminal. Only `views/*`, `terminal.rs`, `mod.rs` touch rendering types (`keys.rs` imports only `crossterm::event::{KeyCode, KeyEvent, KeyModifiers}`).

## 3. Core architecture

### 3.1 State

```rust
// app.rs
#[derive(Debug, Clone, PartialEq)]
pub enum View {
    Dashboard,
    Apps,
    AppDetail { app: String, namespace: String, tab: DetailTab },
    Nodes,
    NodeDetail { node: String },
    Jobs,
    JobDetail { name: String, namespace: String },
    Events,
    Logs { app: Option<(String, String)> },   // (app, namespace); None = picker
    Routes,
    RouteDetail { host: String },
    Search,
    Help,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DetailTab { Overview, Instances, Logs, Metrics, Deploys, Config }

/// Everything the TUI knows about the cluster. Plain data, all fetched.
#[derive(Debug, Clone, Default)]
pub struct ClusterData {
    pub instances: Vec<InstanceStatus>,          // /v1/status
    pub nodes: Vec<NodeStatus>,                  // /v1/cluster/nodes
    pub council: Option<CouncilStatus>,          // /v1/cluster/council
    pub alerts: Vec<serde_json::Value>,          // /v1/alerts (AlertStatus not exported cleanly; keep JSON)
    pub routes: Vec<RouteInfo>,                  // /v1/routes
    pub jobs: Vec<JobStatus>,                    // /v1/jobs (new)
    pub events: VecDeque<ClusterEvent>,          // seed /v1/events, live via WS
    pub deploy_history: HashMap<String, Vec<serde_json::Value>>, // per app, fetched on detail entry
    pub app_metrics: Option<MetricsQueryResult>, // fetched while Metrics tab open
    pub last_updated: Option<u64>,               // epoch seconds of last successful poll
}

#[derive(Debug, Clone)]
pub enum Connection { Connected, Disconnected { since_epoch: u64, last_error: String } }

pub struct TuiApp {
    pub view_stack: Vec<View>,          // never empty; [Dashboard] at bottom
    pub data: ClusterData,
    pub connection: Connection,
    pub now_epoch: u64,                 // advanced by Msg::Tick; NEVER SystemTime::now() in render
    pub apps_cursor: usize,
    pub nodes_cursor: usize,
    pub jobs_cursor: usize,
    pub routes_cursor: usize,
    pub events_scroll: usize,
    pub log_lines: VecDeque<LogLine>,   // capped at 10_000
    pub log_follow: bool,
    pub search: SearchState,            // input: String, results: Vec<SearchHit>, cursor
    pub filter: Option<String>,         // '/' search-within-view for the active list view
    pub palette: Option<PaletteState>,  // Some while ':' palette open
    pub status_message: Option<(String, Level)>,
    pub should_quit: bool,
    pub pending: Vec<Effect>,           // side-effect requests drained by the event loop
}
```

`Effect` keeps the reducer pure and unit-testable — the reducer asks the loop to do IO:

```rust
#[derive(Debug, Clone, PartialEq)]
pub enum Effect {
    RefreshAll,                                       // 'r' key
    OpenLogStream { app: String, namespace: String }, // entering Logs view / Logs tab
    CloseLogStream,
    FetchDeployHistory { app: String },
    FetchAppMetrics { app: String, namespace: String },
}
```

### 3.2 Messages

```rust
// msg.rs
pub enum Msg {
    Key(crossterm::event::KeyEvent),
    Resize(u16, u16),
    Tick,                                  // 100ms cadence; drives render + now_epoch
    Data(DataUpdate),
    Stream(StreamItem),
}

pub enum DataUpdate {
    Status(Result<Vec<InstanceStatus>, ProviderError>),
    Nodes(Result<Vec<NodeStatus>, ProviderError>),
    Council(Result<CouncilStatus, ProviderError>),
    Alerts(Result<Vec<serde_json::Value>, ProviderError>),
    Routes(Result<Vec<RouteInfo>, ProviderError>),
    Jobs(Result<Vec<JobStatus>, ProviderError>),
    EventsSeed(Result<Vec<ClusterEvent>, ProviderError>),
    DeployHistory { app: String, result: Result<Vec<serde_json::Value>, ProviderError> },
    AppMetrics(Result<MetricsQueryResult, ProviderError>),
}

pub enum StreamItem {
    LogLine(LogLine),                      // { instance: String, line: String }
    Event(ClusterEvent),
    StreamDown { what: StreamKind, error: String },
    StreamUp { what: StreamKind },
}
```

**Error/disconnect policy (implement exactly)**: every `Err` in a `DataUpdate` sets `connection = Disconnected { .. }` and leaves existing data untouched (stale-but-visible); every `Ok` sets `Connected` and bumps `last_updated`. The status bar shows `DISCONNECTED (retrying) — data as of HH:MM:SS`. Polling never stops while disconnected; the next success self-heals.

### 3.3 DataProvider trait

Async-fn-in-trait is not dyn-compatible → **generics, not `Box<dyn>`**:

```rust
// data.rs
#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    #[error("agent unreachable: {0}")] Unreachable(String),
    #[error("api error {status}: {body}")] Api { status: u16, body: String },
    #[error("websocket: {0}")] WebSocket(String),
}

pub trait DataProvider: Send + Sync + 'static {
    fn status(&self) -> impl Future<Output = Result<Vec<InstanceStatus>, ProviderError>> + Send;
    fn nodes(&self) -> impl Future<Output = Result<Vec<NodeStatus>, ProviderError>> + Send;
    fn council(&self) -> impl Future<Output = Result<CouncilStatus, ProviderError>> + Send;
    fn alerts(&self) -> impl Future<Output = Result<Vec<serde_json::Value>, ProviderError>> + Send;
    fn routes(&self) -> impl Future<Output = Result<Vec<RouteInfo>, ProviderError>> + Send;
    fn jobs(&self) -> impl Future<Output = Result<Vec<JobStatus>, ProviderError>> + Send;
    fn recent_events(&self, limit: usize) -> impl Future<Output = Result<Vec<ClusterEvent>, ProviderError>> + Send;
    fn deploy_history(&self, app: &str) -> impl Future<Output = Result<Vec<serde_json::Value>, ProviderError>> + Send;
    fn app_metrics(&self, app: &str, ns: &str) -> impl Future<Output = Result<MetricsQueryResult, ProviderError>> + Send;
    /// Run until `cancel` fires, pushing lines into `tx`. Reconnects internally.
    fn follow_logs(&self, app: String, ns: String, tail: usize,
                   tx: mpsc::Sender<StreamItem>, cancel: CancellationToken)
                   -> impl Future<Output = ()> + Send;
    fn follow_events(&self, tx: mpsc::Sender<StreamItem>, cancel: CancellationToken)
                   -> impl Future<Output = ()> + Send;
}

pub struct HttpDataProvider { pub client: BunClient }      // each method = one BunClient call
pub struct MockDataProvider { pub scenario: TestScenario } // fixtures.rs; canned data instantly
```

(Fallback if `impl Future` in traits fights the executor: `#[allow(async_fn_in_trait)]` + plain `async fn` — acceptable since we only use generics; the `+ Send` form above is the safe default.)

New `BunClient` methods (mirror the existing `nodes()`/`routes()` pattern in `src/relish/client.rs`): `alerts()`, `jobs()`, `events(limit)`, `deploy_history(app)`, `app_metrics(app, ns)`, plus `ws_logs(app, ns, tail)` / `ws_events()` (§4.4). Add `RelishError::WebSocket(String)`.

### 3.4 Event loop and terminal lifecycle

```rust
// mod.rs — the only place that owns the real terminal
pub async fn run() -> Result<(), RelishError> {
    let client = BunClient::default_local();
    let provider = Arc::new(HttpDataProvider { client });
    let mut guard = TerminalGuard::new()?;          // raw mode + alt screen + panic hook
    let result = event_loop(provider, guard.terminal_mut()).await;
    drop(guard);                                    // explicit restore BEFORE printing errors
    result
}

async fn event_loop<P: DataProvider, B: ratatui::backend::Backend>(
    provider: Arc<P>, terminal: &mut Terminal<B>,
) -> Result<(), RelishError> {
    let (tx, mut rx) = mpsc::channel::<Msg>(256);
    let cancel = CancellationToken::new();
    spawn_pollers(provider.clone(), tx.clone(), cancel.clone()); // 5s lists, 2s metrics (gated)
    // spawn provider.follow_events(...) once — lives for the session
    let mut input = crossterm::event::EventStream::new();
    let mut tick = tokio::time::interval(Duration::from_millis(100)); // 10 FPS
    tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut app = TuiApp::new();
    let mut log_cancel: Option<CancellationToken> = None;

    loop {
        tokio::select! {
            _ = tick.tick() => {
                app.now_epoch = unix_now();
                terminal.draw(|f| views::render(f, &app)).map_err(RelishError::Io)?;
            }
            Some(msg) = rx.recv() => app.update(msg),
            Some(Ok(ev)) = input.next() => match ev {
                crossterm::event::Event::Key(k) if k.kind == KeyEventKind::Press =>
                    app.update(Msg::Key(k)),
                crossterm::event::Event::Resize(w, h) => app.update(Msg::Resize(w, h)),
                _ => {}
            },
        }
        for effect in app.pending.drain(..) { /* spawn one-shot fetch / manage log_cancel */ }
        if app.should_quit { cancel.cancel(); return Ok(()); }
    }
}
```

Rules the executor must follow:

- **Render only on tick** (10 FPS ceiling). Key handling mutates state; the next tick (≤100 ms) paints it.
- **Pollers**: one task per cadence. The 5 s task calls `status`, `nodes`, `council`, `alerts`, `routes`, `jobs` with `tokio::join!`, one `Msg::Data` per result. The 2 s metrics task runs only while a Metrics tab is open — gate via `tokio::sync::watch::Receiver<Option<(String, String)>>` set from the effect handler.
- **`Effect::RefreshAll`** → `tokio::sync::Notify` poked at the 5 s poller ("poll now").
- **Log streams**: `Effect::OpenLogStream` cancels any previous `log_cancel`, makes a fresh token, spawns `provider.follow_logs(...)`. `Effect::CloseLogStream` cancels on leaving the view.
- **TerminalGuard** (terminal.rs): `new()` enables raw mode, enters the alternate screen, installs a panic hook that restores the terminal then chains the previous hook. `Drop` disables raw mode + leaves the alternate screen. Never call `std::process::exit` inside the TUI (skips destructors).

**Entry points** (`src/bin/relish.rs`): change `command: Command` to `command: Option<Command>`, add a `/// Launch the interactive terminal UI.` `Tui` variant, and dispatch:

```rust
let result = match cli.command {
    None | Some(Command::Tui) => reliaburger::relish::tui::run().await,
    Some(Command::Apply { ref path }) => commands::apply(path, cli.output).await,
    // ... wrap every existing arm in Some(...)
};
```

`relish --help` / `--version` keep working (clap handles them before dispatch). This is a mechanical wrap of ~40 match arms — one commit, no other changes, so the diff reviews cleanly.

## 4. Server-side additions (bun)

### 4.1 Event store — `src/bun/events.rs` (new, ~150 LOC + tests)

No event storage exists; build the minimal honest in-memory store:

```rust
pub const EVENT_BUFFER_CAP: usize = 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClusterEvent {
    pub sequence: u64,
    pub timestamp: u64,                 // unix seconds
    pub kind: EventKind,
    pub severity: EventSeverity,
    pub app: Option<String>,
    pub namespace: Option<String>,
    pub node: Option<String>,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum EventKind { Deploy, Restart, Health, Stop, JobCompleted, JobFailed, Alert, Fault }

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EventSeverity { Info, Warning, Critical }

pub struct EventStore {
    buffer: VecDeque<ClusterEvent>,
    next_sequence: u64,
    live: tokio::sync::broadcast::Sender<ClusterEvent>,   // capacity 256
}
// new(), record(...) -> u64 (push_back, pop_front at cap, broadcast — ignore send error),
// recent(limit, app, min_severity) -> Vec<ClusterEvent>, subscribe() -> broadcast::Receiver
```

Shared as `Arc<tokio::sync::RwLock<EventStore>>` (matches the other stores in `ApiState`).

**Emission points**: give `BunAgent` a field `events: Option<Arc<RwLock<EventStore>>>` (setter `with_event_store(...)`, same pattern as other optional subsystems) and a private `async fn record_event(&self, ...)` that no-ops when `None`. Emit from these verified sites in `src/bun/agent.rs`:

| Existing code location | Event |
|---|---|
| deploy path (~agent.rs:937, `ApplyEvent::Complete`/`Error` sends) | `Deploy` info "deployed app X (n instances)" / `Deploy` critical on error |
| restart logic (health-check restart + `check_jobs` retry, ~agent.rs:1821+) | `Restart` warning "instance X restarted (attempt n)" |
| health transition to `Unhealthy` | `Health` warning |
| `AgentCommand::Stop` handling | `Stop` info |
| `check_jobs` terminal success / failure | `JobCompleted` info / `JobFailed` warning |

That is the entire Phase 13 emit list. Node join/leave and alert-firing events are out of scope (gossip and alert evaluation don't run inside the agent task) — note this in the book as deferred.

### 4.2 `GET /v1/events` + `GET /v1/ws/events`

Add `events: Option<Arc<RwLock<EventStore>>>` to `ApiState` and as a new `api::router()` parameter (extend the existing `#[allow(clippy::too_many_arguments)]`; update ALL callers — `grep -rn "api::router(" src tests`).

- `events_handler`: `Query { limit, app, severity }` → `store.recent(limit.unwrap_or(100), ...)` → `Json({"events": [...]})`. When `state.events` is `None`, return `{"events": []}`.
- `ws_events_handler`: `WebSocketUpgrade` → on_upgrade → replay last 50 as JSON text frames, then `tokio::select!` over `broadcast::Receiver::recv()` (on `Err(Lagged(_))` → `continue`, on `Closed` → return) and `socket.recv()` (client gone → return).

Both routes go in the **protected** block (bearer middleware applies to the upgrade GET — WS clients must send `Authorization`).

### 4.3 `GET /v1/jobs`

New wire type in `src/bun/agent.rs` next to `InstanceStatus`:

```rust
/// Status of a job (run-to-completion) instance, as returned by /v1/jobs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobStatus {
    pub name: String,          // WorkloadInstance.app_name
    pub namespace: String,
    pub instance_id: String,
    pub image: String,
    pub state: String,         // same rendering as InstanceStatus.state
    pub restart_count: u32,
    pub age_seconds: u64,      // now - created_at
}
```

**Honesty rule**: do NOT invent `succeeded/failed/duration/schedule` semantics the supervisor doesn't track. Expose the raw `ContainerState` string exactly as `/v1/status` does, plus `restart_count` and `age_seconds`. The TUI labels states, it does not reinterpret them. Cron `schedule` lives in `JobSpec` config held only during deploy — out of scope; note in the book.

Add `AgentCommand::JobStatus { response: oneshot::Sender<Vec<JobStatus>> }`, handled where `AgentCommand::Status` is handled, filtering supervisor instances on `is_job`. Handler `jobs_handler` copies the `gather_statuses` oneshot pattern. Route in the protected block.

### 4.4 `GET /v1/ws/logs/{app}/{namespace}?tail=N`

Mirror the existing SSE follow branch (api.rs:392–409) over WS: on upgrade, send `AgentCommand::FollowLogs { app_name, namespace, tail, lines: lines_tx }`, then `tokio::select!`: forward each received line as `Message::Text`, and return when `socket.recv()` shows the client is gone (dropping `lines_rx` stops the agent-side forwarder). Existing SSE endpoints untouched; `relish logs -f` keeps using them.

**Client side** (`src/relish/client.rs`):

```rust
pub async fn ws_connect(&self, path_and_query: &str) -> Result<WsStream, RelishError> {
    let ws_url = format!("{}{}", self.base_url.replacen("http", "ws", 1), path_and_query);
    let mut request = ws_url.into_client_request()
        .map_err(|e| RelishError::WebSocket(e.to_string()))?;
    if let Some(t) = &self.token {
        request.headers_mut().insert("Authorization",
            format!("Bearer {t}").parse().map_err(|_| RelishError::WebSocket("bad token".into()))?);
    }
    let (stream, _resp) = tokio_tungstenite::connect_async(request).await
        .map_err(|e| RelishError::WebSocket(e.to_string()))?;
    Ok(stream)
}
pub async fn ws_logs(&self, app: &str, ns: &str, tail: usize) -> Result<WsStream, RelishError> {
    self.ws_connect(&format!("/v1/ws/logs/{app}/{ns}?tail={tail}")).await
}
pub async fn ws_events(&self) -> Result<WsStream, RelishError> { self.ws_connect("/v1/ws/events").await }
```

Wrinkle: `BunClient` currently buries the bearer token in reqwest default headers. Store `token: Option<String>` as a field at construction (minor refactor of `new_with_token`) so WS can reuse it — preferred over re-calling `resolve_token()`.

`stream.rs` reconnect loop (backs `HttpDataProvider::follow_logs/follow_events`): connect → forward `Message::Text` frames as `StreamItem` (parse JSON for events; raw line for logs) → on error/close send `StreamItem::StreamDown`, sleep 5 s inside `tokio::select!` with `cancel.cancelled()`, retry → `StreamItem::StreamUp` after reconnect.

## 5. Per-view specifications

Global chrome on every view: header bar (`N nodes · M apps · leader <id>` from `nodes`/`council` + connection indicator) and bottom nav bar (`[a]pps [n]odes [j]obs [e]vents [l]ogs [r]outes [s]earch [?]help [:]cmd [q]uit`, contextual in detail views). Frames smaller than 80×24 render only the "terminal too small — resize to at least 80x24" screen (widgets.rs).

| View | Key | Data source | Layout / columns | Extra keys | Empty state | Error state |
|---|---|---|---|---|---|---|
| **Dashboard** | start / root | cached `status`, `nodes`, `council`, `alerts`, `events` | apps table (NAME, NS, READY n/m, STATUS, RESTARTS) aggregated by (app, ns); compact nodes strip (ID, STATE, COUNCIL★/LEADER); last 8 events; alerts line | `Enter` → AppDetail; letters navigate; `q` quits | "no apps deployed — relish apply \<path\>" | stale data + red `DISCONNECTED` chip |
| **Apps** | `a` | `status` aggregated | NAME, NS, READY, STATUS (worst instance state), RESTARTS, PORTS | ↑↓, `Enter`, `/` filter, `r` refresh | "no apps" | same |
| **App detail** | `Enter` | per tab | tab bar `Overview│Instances│Logs│Metrics│Deploys│Config`; `Tab`/`Shift-Tab` cycle | `Esc`/`q` back | — | per-tab notice |
| — Overview | | `status` + newest DeployHistoryEntry | image (or "unknown"), ready count, state summary, restart total | | | |
| — Instances | | `status` filtered | ID, STATE, RESTARTS, HOST PORT, PID | ↑↓ | "no instances" | |
| — Logs | | WS `/v1/ws/logs/{app}/{ns}?tail=100` | scrolling tail, instance-prefixed, colour per instance; pin-to-bottom in follow mode | `f` follow, PgUp/PgDn, Home/End | "waiting for logs…" | "stream disconnected — reconnecting" banner |
| — Metrics | | `/v1/metrics/app/{app}/{ns}` every 2 s | `Sparkline` for CPU + memory from `MetricsQueryResult.data`; render warnings | | "no metrics recorded" | |
| — Deploys | | `/v1/deploys/history/{app}` on tab entry | TIME, IMAGE, RESULT, STEPS n/m | | "no deploy history" | |
| — Config | | `status` + deploy history | honest subset: image, host ports, restart counts; note "full resolved config is not exposed by the API" | | | |
| **Nodes** | `n` | `nodes` + `council` | ID, ADDRESS, STATE, INCARNATION, COUNCIL, LEADER★, LABELS | `Enter` → detail | "single-node mode — no gossip peers" | |
| **Node detail** | `Enter` | selected `NodeStatus` + matching `council.members` | key/value panel; council term/last-applied if member | `Esc` | — | |
| **Jobs** | `j` | `/v1/jobs` | NAME, NS, STATE, RESTARTS, AGE, IMAGE | `Enter` → detail (key/value) | "no jobs" | |
| **Events** | `e` | seed `GET /v1/events?limit=100`, live WS | newest-at-bottom: TIME, SEV (coloured), KIND, APP, MESSAGE; auto-scroll unless scrolled up | `/` filter, PgUp/PgDn, Home/End | "no events yet" | stream-down banner, list stays |
| **Logs** | `l` | app picker (from `status`) → WS logs | picker first (`Logs { app: None }`), then same widget as Logs tab | `Enter` pick, `f` follow, `/` client-side substring filter | "no apps to stream" | reconnect banner |
| **Routes** | `r` | `routes` | HOST, PATH, APP, BACKENDS healthy/total (coloured), WS? | `Enter` → detail (key/value) | "no ingress routes configured" | |
| **Search** | `s` | in-memory over `data` (apps, nodes, jobs, routes, event messages) | input line + `KIND NAME CONTEXT` results; case-insensitive substring, prefix matches ranked first (no fuzzy crate) | type, ↑↓, `Enter` jumps to detail, `Esc` | "type to search" | |
| **Help** | `?` | `keys::KEYBINDINGS` const | scrollable two-column table generated from the dispatcher's own table — cannot drift | ↑↓/PgUp/PgDn | — | — |
| **Palette** | `:` | — | one-line input overlay at bottom | `Enter` run, `Esc` cancel. Commands: `:apps` `:nodes` `:jobs` `:events` `:logs [app]` `:routes` `:search` `:help` `:quit`/`:q` | — | unknown → status_message error |

Key dispatch precedence in `update(Msg::Key)`: palette open → palette; Search view → search input; `/`-filter input active → filter; else `keys::key_to_msg(view, key)`. `q` pops the stack and quits only from Dashboard; `Esc` pops (no-op at Dashboard). `KEYBINDINGS: &[(&str, &str, &str)]` (key, context, description) is the single source of truth for dispatch AND the Help view.

## 6. Testing strategy

### 6.1 Snapshot tests (unit, `#[cfg(test)]` in each `views/*.rs`)

Helper in `fixtures.rs`:

```rust
/// Render at fixed size, return the buffer as plain text (cell symbols only —
/// styles deliberately ignored so colour tweaks don't invalidate snapshots).
pub fn render_to_string(app: &TuiApp, width: u16, height: u16) -> String {
    let backend = ratatui::backend::TestBackend::new(width, height);
    let mut terminal = ratatui::Terminal::new(backend).expect("test terminal");
    terminal.draw(|f| crate::relish::tui::views::render(f, app)).expect("draw");
    let buffer = terminal.backend().buffer();
    let mut out = String::new();
    for y in 0..buffer.area.height {
        for x in 0..buffer.area.width {
            out.push_str(buffer[(x, y)].symbol());
        }
        out.push('\n');
    }
    out
}
```

`TestScenario` variants: `Empty`, `HealthyCluster` (3 apps × 2 instances, 3 nodes, 2 routes, 1 job, 6 events), `DegradedApp` (one `unhealthy` app, restarts > 0, critical event), `ManyApps` (30 apps — exercises scroll). All fixture timestamps are fixed constants (e.g. `now_epoch = 1_750_000_000`); `TuiApp::with_test_data(scenario)` sets `now_epoch` to match — **renders must be byte-deterministic**.

Every test: `insta::assert_snapshot!("dashboard_healthy", render_to_string(&app, 120, 40));` — always explicit snapshot names (auto-names churn when tests move). Snapshots in `src/relish/tui/views/snapshots/*.snap`; commit them. Required coverage: dashboard (empty/healthy/degraded), apps (healthy/many/filtered), app detail (all 6 tabs), nodes + node detail, jobs, events, logs (with buffered lines), routes + route detail, search (empty + query), help, terminal-too-small at 60×15.

Workflow: first run fails "new snapshot" → `cargo insta accept` → re-run green → **read the .snap files** to confirm they look right → commit.

### 6.2 Navigation and reducer tests (unit, `app.rs`/`keys.rs`)

Pure state tests, no terminal:

```rust
fn key(c: char) -> Msg { Msg::Key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)) }

#[test]
fn apps_view_opens_and_esc_returns_to_dashboard() {
    let mut app = TuiApp::with_test_data(TestScenario::HealthyCluster);
    app.update(key('a'));
    assert_eq!(app.view_stack.last(), Some(&View::Apps));
    app.update(Msg::Key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)));
    assert_eq!(app.view_stack.last(), Some(&View::Dashboard));
}
```

Cover: every nav key from Dashboard; Enter drill-in for apps/nodes/jobs/routes; Tab cycling through all six tabs with wrap; `q` pops vs quits; palette open/parse/dispatch (`:logs web` → `View::Logs { app: Some(..) }`, namespace resolved from cached status, default `"default"`); `/` filter narrows cursor bounds; error `DataUpdate` → `Disconnected` while data preserved; `StreamItem::LogLine` caps buffer at 10 000; `Effect` emission (entering Logs tab pushes `OpenLogStream`).

### 6.3 Data-fetch tests (unit, `data.rs`/`fixtures.rs`)

`MockDataProvider` returns scenario data: call each provider method, feed the `DataUpdate` into `app.update`, assert `ClusterData` populated, snapshot one render. This satisfies the roadmap's "mock API responses render correctly" without HTTP.

### 6.4 Integration tests — `tests/tui.rs` (new)

**No PTY needed** (and none should be used): the state/terminal split means tests drive `TuiApp` + `HttpDataProvider` against TestHarness's real HTTP API and render into `TestBackend`. This exercises everything except raw-mode setup (covered by `TerminalGuard` by construction; a PTY test would be flaky in CI). Copy `TestHarness` from `tests/integration.rs`, updating the `api::router(...)` call for the new `events` parameter and giving `BunAgent` an `EventStore`.

The five roadmap tests:

1. **`tui_launches_renders_dashboard_and_navigates`** — harness + 1-app apply; fetch via `HttpDataProvider`, feed into `TuiApp`; rendered output contains the app name + node header; `'a'` → `View::Apps`; render shows table headers.
2. **`app_detail_shows_instance_count_and_health`** — app with `replicas = 2`; Apps → Enter → Instances tab; rendered output shows 2 instance rows in `running` state; `app.data.instances` filtered count == 2.
3. **`log_view_streams_new_lines_in_real_time`** — proc-grill app printing to stdout; spawn `provider.follow_logs(...)`; receive `StreamItem`s in a 10 s `tokio::time::timeout` loop into `app.update`; assert `log_lines` grows and the rendered Logs view contains a known line. Covers the WS endpoint end-to-end.
4. **`search_filters_across_views`** — two apps ("web", "worker"); Search; type "web"; results contain web, not worker; Enter → `View::AppDetail { app: "web", .. }`.
5. **`help_lists_all_keybindings`** — render Help at 120×40 scrolling through; every `KEYBINDINGS` key string appears in the concatenated pages.

Plus plain HTTP integration tests for the new endpoints: `/v1/events` shows a deploy event after apply; `/v1/jobs` lists a deployed job; `/v1/ws/events` replays and streams; WS auth (connect without token against a token-store router → rejected — mirror `tests/security_integration.rs` construction).

## 7. Staged execution plan

Every stage: write listed tests first (red) → implement (green) → `make ci` → write the book section → update `docs/progress.md` → ask the user, then commit. LOC estimates exclude tests/snapshots.

| # | Stage | Tests first | Implementation | Book section (same commit) | Commit message | Est. LOC |
|---|---|---|---|---|---|---|
| 1 | Skeleton: deps, entry points, terminal guard, empty dashboard | snapshots: dashboard empty + too-small; nav: `q` quits, stack starts at Dashboard | Cargo.toml deps; `Option<Command>` + `Tui` variant; `tui/{mod,terminal,app,msg,keys,theme,fixtures}.rs`; `views/{mod,dashboard,widgets}.rs` minimal; `render_to_string` | "The shape of a TUI": immediate-mode rendering; RAII/`Drop` terminal guard; panic hooks; why bare `relish` launches it | `relish tui: skeleton, terminal guard, and entry points` | ~500 |
| 2 | Event loop, Msg/Effect reducer, DataProvider + mock | reducer tests (tick, ok/err → Connected/Disconnected, effect emission); mock-provider fetch | `data.rs` trait + Mock + Http provider (existing BunClient methods only); `event_loop` select!/tick/pollers; effect drain | "One loop to rule them all": `select!` recap; single-owner state vs `Arc<Mutex>`; async-fn-in-trait, generics not `dyn` | `relish tui: event loop, message reducer, and data provider` | ~450 |
| 3 | Dashboard + Apps with real data | snapshots: dashboard healthy/degraded, apps healthy/many/filtered; nav: `a`, Enter, `/` | `BunClient::alerts()`; client-side instance aggregation; full dashboard + apps render; status bar; `r` refresh | "First light": Layout/Constraint; tables from borrowed state; stale-data-on-disconnect decision | `relish tui: dashboard and apps views` | ~400 |
| 4 | App detail (Overview/Instances/Deploys/Config), nodes, routes | snapshots: 4 tabs, nodes(+detail), routes(+detail); nav: Tab cycling, drill-ins | `BunClient::deploy_history()`; `app_detail.rs`, `nodes.rs`, `routes.rs`; `Effect::FetchDeployHistory` | "Drilling down": enums for tab state, exhaustive `match` as UI router | `relish tui: app detail tabs, nodes and routes views` | ~450 |
| 5 | Server: EventStore + emissions + `/v1/events` + `/v1/jobs` | unit: ring cap/sequence/filter/subscribe; integration: apply → deploy event; job config → `/v1/jobs` row | `src/bun/events.rs`; agent field + 5–6 emit sites; `JobStatus` + `AgentCommand::JobStatus`; handlers + routes; router signature (all callers) | "What just happened?": why an in-memory ring buffer; `VecDeque`; `broadcast` and lag semantics | `bun: cluster event store, /v1/events and /v1/jobs endpoints` | ~400 |
| 6 | Server WS + client WS | integration: ws_events replay+live; ws_logs receives stdout; WS auth rejected without token | axum `ws` feature; `ws_logs_handler`, `ws_events_handler`; `BunClient::{ws_connect,ws_logs,ws_events}`; `RelishError::WebSocket`; token field on BunClient | "Upgrading the connection": WS upgrade mechanics; WS vs SSE and why both exist | `bun+relish: websocket streaming for logs and events` | ~350 |
| 7 | Events, Logs, Jobs views + stream tasks + reconnect | snapshots: events, logs, jobs (+details); reducer: log ring cap, StreamDown banner; integration test #3 | `stream.rs` reconnect loops; `events.rs`, `logs.rs`, `jobs.rs`; `BunClient::{events,jobs}`; Logs tab wiring; app picker | "Live data": backpressure, bounded buffers, reconnect under `CancellationToken` | `relish tui: live events, logs and jobs views` | ~500 |
| 8 | Metrics tab, search, help, palette | snapshots: metrics, search, help; nav: palette parse table incl. unknown cmd; integration tests #4, #5 | `BunClient::app_metrics`; sparkline tab + 2 s gated poll; `search.rs` + jump; `help.rs` from KEYBINDINGS; `palette.rs` | "Finding things": matching without a fuzzy crate; one keybinding table for render + dispatch | `relish tui: metrics sparklines, search, help and command palette` | ~450 |
| 9 | Integration hardening + roadmap test sweep | integration tests #1, #2; disconnect test (kill harness → Disconnected render) | fixes shaken out; `tests/tui.rs` harness finalised | "Testing a screen": TestBackend philosophy; why no PTY; snapshot discipline | `relish tui: integration test suite for the roadmap milestones` | ~200 |
| 10 | Docs, progress, chapter polish | full `make ci`; `cargo insta test` clean | README + docs/README (`relish`/`relish tui` usage, keybindings table); progress.md Phase 13 all ticked; roadmap milestone note | Chapter intro + "What we learned" + final pass for AI-tell transitions / British English | `docs: Phase 13 TUI documentation and book chapter` | ~50 + prose |

Total ≈ 3,350 production LOC + ~1,500 test LOC + snapshots. Each stage leaves `make ci` green — stages 5–6 are server-only; stage 1's entry-point change is safe because `tui::run()` fails cleanly with an agent-unreachable error (printed AFTER the terminal guard restores the screen).

## 8. Book chapter outline — `docs/book/13-a-room-with-a-view.md`

One section per stage; Rust concepts on first appearance, framed for C/Python/Go readers:

1. **A room with a view** — why a TUI, the k9s heritage, the two-mode binary. *(stage 1)*
2. **Owning the terminal** — raw mode, alternate screen; RAII and `Drop` vs Go's `defer`/C's manual cleanup; panic hooks and why a panicking TUI must restore the terminal before printing the backtrace. *(1)*
3. **Immediate-mode rendering** — redraw everything at 10 FPS vs retained/DOM UIs; `Frame`, `Layout`, `Constraint`; why this is cheap. *(1, 3)*
4. **One loop, one owner** — `tokio::select!` with four arms; state owned by a single task, channels not mutexes ("share memory by communicating", enforced by the borrow checker); the Msg/Effect reducer and why the pure shape makes tests trivial. *(2)*
5. **Traits without boxes** — async fn in traits, dyn-compatibility, why generics + monomorphisation (same pattern as `WorkloadSupervisor<G: Grill>`); the MockDataProvider seam. *(2)*
6. **Views as enums** — the view stack as `Vec<View>`, exhaustive `match` as router; contrast with class-hierarchy widget trees; tabs as a fieldless `Copy` enum. *(3–4)*
7. **What just happened? An event store in 150 lines** — what the cluster already records (nothing, honestly); `VecDeque` ring buffer; `tokio::sync::broadcast` and lagged receivers; where the emit hooks live. *(5)*
8. **Upgrading the connection** — HTTP → WS upgrade mechanics; SSE vs WS trade-offs and why both now exist; the bearer header on the upgrade request. *(6)*
9. **Streams that survive** — reconnect loops, `CancellationToken` trees, bounded channels as backpressure; capping the log buffer. *(7)*
10. **The colon key** — a tiny command language, parsing without a parser crate. *(8)*
11. **Testing a screen** — `TestBackend` renders cells into a buffer; insta snapshots (first use in the project: `.snap` files, `cargo insta accept`, review discipline); driving the reducer with synthetic `KeyEvent`s; why no PTY. *(1, 9)*
12. **What we learned** — determinism via `now_epoch` injection; clippy `-D warnings` on UI code; what we'd change (event persistence in Ketchup — deferred). *(10)*

## 9. Risks and pitfalls (explicit, for the executor)

1. **Never block the runtime.** No `crossterm::event::read()`/`poll()` anywhere — only `EventStream` (needs the `event-stream` feature). No OS threads for input. No `std::thread::sleep` in async code.
2. **No `unwrap`/`expect` in production code.** `terminal.draw` errors map into `RelishError`. `unwrap` is fine in `#[cfg(test)]` and `fixtures.rs` test constructors only.
3. **Snapshot determinism.** All rendered times derive from `app.now_epoch` minus fixture timestamps. Zero `SystemTime::now`/`Instant::now` under `views/` (grep to confirm). Compare cell symbols only, never `Buffer` debug output (styles churn).
4. **Insta discipline.** Explicit snapshot names; `cargo insta accept` then READ the .snap; commit `.snap`; never commit `.snap.new`; add nothing to `.gitignore`.
5. **Raw-mode leaks.** Every early return goes through `TerminalGuard::drop`. Never `std::process::exit` inside the TUI. Install the panic hook before the loop; chain the previous hook.
6. **`relish --help` regression.** After `Option<Command>`: verify `relish --help` (exit 0), `relish --version`, `relish status` still work; bare `relish` with no agent prints the error AFTER the guard restores the terminal.
7. **Router signature change** (stage 5) breaks every `api::router(` caller: `src/bin/bun.rs` + several `tests/*.rs` — `grep -rn "api::router(" src tests`, fix all in the same commit.
8. **WS version drift.** `cargo tree -d | grep -i tungstenite` must show one version; align `tokio-tungstenite` with axum's transitive pin.
9. **broadcast lag.** `recv()` returning `Err(Lagged(n))` → `continue`, never `return`, or slow terminals lose their event stream permanently.
10. **Clippy traps**: `clippy::large_enum_variant` on `View`/`Msg` (box the metrics payload only if it fires); key dispatch as `match` on `(view, key.code)` tuples avoids collapsible-if lints; render from `&TuiApp`, borrow into `Row`s, `.to_string()` only for computed cells.
11. **Don't fake data.** Jobs view shows raw supervisor state strings; Config tab states what the API doesn't expose; events come only from the wired emit sites. No synthetic "success rate"/"schedule" columns — leave them out and note it in the book.
12. **Streaming tests need real time**: 10 s `tokio::time::timeout` loops, never bare `sleep(fixed)`; do NOT use `start_paused` for tests hitting real sockets (see also the project's known `start_paused` + `tokio::spawn` pitfall).
13. **Auth on WS**: the upgrade GET passes through the bearer middleware. TestHarness uses the auth-disabled path; add one test with a token store to prove WS + `Authorization` works.
14. **Render only on tick** — never render inside the key handler too, or behaviour differs between iterations and CPU sits at 100%.

## Deviation from the design doc

`docs/design/cli-relish.md` §4 says "WebSocket connections for live logs/events/exec" — we implement WS for logs and events; **exec streaming stays out of scope** for Phase 13 (the existing `/v1/exec` is request/response). Note this in the book's "What we learned".
