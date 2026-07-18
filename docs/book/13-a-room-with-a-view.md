# A Room with a View

You can learn a lot about a cluster by asking it one question at a time. `relish status` tells you what is running. `relish nodes` shows membership. `relish logs -f web` follows one stream. This works, but investigating a wobbling deployment turns into a small collection of terminals and a surprising amount of typing.

So we gave Relish a room with a view.

Running `relish` with no subcommand now opens an interactive terminal interface (TUI). `relish tui` does the same thing explicitly. The ordinary subcommands remain useful for scripts, CI and precise queries. The TUI is for the moment when you want to look around.

## Owning the terminal

A terminal application temporarily changes the terminal it inherited. It enables raw mode, where key presses arrive immediately, and enters the alternate screen, the separate buffer used by programmes such as `less` and `vim`. If we leave either setting behind, the user's shell looks broken. That is a rather impolite failure mode.

`TerminalGuard` owns this temporary state:

```rust
pub struct TerminalGuard {
    terminal: Terminal<CrosstermBackend<Stdout>>,
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = disable_raw_mode();
        let _ = execute!(stdout(), LeaveAlternateScreen);
        let _ = self.terminal.show_cursor();
    }
}
```

`Drop` is Rust's deterministic clean-up hook. It resembles a destructor in C++ and covers the job you might give `defer` in Go or a `finally` block in Python. When a value leaves scope, Rust calls its `drop` method. Early returns and `?` still pass through it.

Panics need one extra precaution. A panic hook restores the terminal before chaining to the previous hook, so the panic message appears on a normal screen. We don't call `process::exit` from the TUI because that would skip destructors entirely. The unexciting clean-up code is doing important work.

## Immediate-mode rendering

Ratatui uses immediate-mode rendering. On every frame we describe the whole screen from the current state. There is no persistent tree of mutable widgets and no DOM to reconcile.

The event loop draws at most ten frames per second:

```rust
let mut tick = tokio::time::interval(Duration::from_millis(100));
tick.set_missed_tick_behavior(MissedTickBehavior::Skip);

tokio::select! {
    _ = tick.tick() => {
        app.now_epoch = unix_now();
        terminal.draw(|frame| views::render(frame, &app))?;
    }
    // input and data arms omitted
}
```

`Layout` divides a `Rect` into the header, current view and status bar. Each view borrows `&TuiApp` and produces text for its section. Borrowing matters here: renderers can read the state, but the type system prevents them from quietly changing it.

Ten complete redraws per second sound wasteful until you remember the scale. Even a large terminal contains only a few thousand cells. Ratatui also computes a diff before writing escape sequences, so unchanged cells stay put.

## One loop, one owner

The TUI has several sources of work: keyboard input, a render timer, HTTP polling and WebSocket streams. We could put the state behind `Arc<Mutex<TuiApp>>` and let every task edit it. That would spread ordering rules across the programme and make rendering race with updates. Fun for all the wrong reasons.

Instead, one task owns `TuiApp`. Background tasks send `Msg` values through a bounded Tokio channel:

```rust
pub enum Msg {
    Key(KeyEvent),
    Resize(u16, u16),
    Tick,
    Data(DataUpdate),
    Stream(StreamItem),
}
```

The reducer applies one message at a time. When it needs I/O, it queues an `Effect`, such as `RefreshAll` or `OpenLogStream`. The event loop drains those requests after the state transition. This keeps the reducer deterministic and lets tests drive it with synthetic key events.

The arrangement is the Rust version of “share memory by communicating”. Ownership makes the slogan concrete: only the event-loop task has `&mut TuiApp`.

## Traits without boxes

The screen doesn't care whether its data came from Bun or a fixture. `DataProvider` captures that boundary:

```rust
pub trait DataProvider: Send + Sync + 'static {
    fn status(
        &self,
    ) -> impl Future<Output = Result<Vec<InstanceStatus>, ProviderError>> + Send;

    // nodes, jobs, events, metrics and streams follow
}
```

An `impl Future` return means “some concrete future type chosen by the implementation”. We use the provider through generics rather than `Box<dyn DataProvider>`. Async methods are not generally object-safe in the same way ordinary trait methods are: the concrete future has a compiler-generated type. Generics let the compiler monomorphise the loop for `HttpDataProvider` or `MockDataProvider`, just as it does for `BunAgent<G: Grill>`.

The mock returns fixed timestamps and canned cluster shapes immediately. The real provider wraps `BunClient`. Both feed the same reducer.

## Views as enums

Navigation is a `Vec<View>`:

```rust
pub enum View {
    Dashboard,
    Apps,
    AppDetail {
        app: String,
        namespace: String,
        tab: DetailTab,
    },
    Nodes,
    Events,
    Logs {
        app: Option<(String, String)>,
    },
    // ...
}
```

The vector is a stack. Opening a detail pushes a value; Escape pops it. The dashboard stays at the bottom, so `q` there quits while `q` in a detail view goes back.

An enum models a closed set of alternatives. Rust requires an exhaustive `match`, so adding a view forces us to decide how rendering and navigation handle it. There is no base widget class and no stringly typed route name to misspell.

`DetailTab` is another enum with six variants. It derives `Copy` because it is a small value with no owned data. Cycling a tab computes the next variant and queues any effect that the new tab needs.

## What just happened?

Before this phase Bun had logs, deploy history and metrics, but no honest cluster event feed. The TUI needed one, so we added the smallest useful store.

`EventStore` keeps 1,024 `ClusterEvent` values in a `VecDeque`. A deque supports efficient insertion at the back and removal from the front, which is exactly a ring buffer's workload. Sequence numbers increase for the lifetime of the Bun process. The store is deliberately in memory: restarting Bun clears the history.

Every recorded event also goes to a Tokio `broadcast` channel. A broadcast sender gives each subscriber its own receiver. A slow receiver can lag behind the fixed channel capacity; the WebSocket handler treats `Lagged` as “skip what you missed and continue”, not as a permanent disconnect.

The agent emits deploy, restart, unhealthy, stop, job-completed and job-failed events. Node membership and alert transitions live outside the agent task, so they remain deferred. We say that plainly rather than inventing data.

The new `/v1/jobs` endpoint follows the same rule. It exposes the supervisor's raw state, restart count and age. It doesn't manufacture a cron schedule or a duration that the supervisor never stored.

## Upgrading the connection

HTTP polling is a good fit for tables. The TUI refreshes cluster lists every five seconds and app metrics every two seconds while the Metrics tab is visible. Logs and events need lower latency, so they use WebSockets.

A WebSocket begins as an HTTP GET with upgrade headers. That first request passes through the same bearer-token middleware as the other protected API routes. Axum then hands the upgraded connection to a task that forwards log lines or serialised events.

The existing command-line log follower still uses Server-Sent Events (SSE). SSE is simple and one-way, which suits `relish logs -f`. WebSockets give the TUI one streaming mechanism for logs and events, plus a clean way to notice that the client has gone away. There is no prize for replacing a working endpoint merely to make the architecture diagram tidier.

## Streams that survive

Networks fail. The provider's stream loop reports `StreamDown`, waits five seconds and reconnects until its `CancellationToken` fires. A successful reconnect sends `StreamUp` and clears the banner.

Cancellation tokens form a tree. The session token stops every background task. Each log view gets a child token; opening another log stream cancels the previous child without ending event streaming or polling.

Both the message channel and server-side log channel are bounded. Bounded channels provide backpressure: when a consumer falls behind, producers wait instead of growing memory forever. The on-screen log history has a separate 10,000-line cap. Old lines fall off the front.

## The colon key

Pressing `:` opens a small command palette. Its language is intentionally tiny:

```text
:apps
:nodes
:jobs
:events
:logs [app]
:routes
:search
:help
:quit
```

`split_whitespace` is enough of a parser. This phase keeps the palette navigation-only, so it cannot quietly become an alternative shell with a second command-dispatch implementation.

Search is similarly modest. It scans cached apps, nodes, jobs, routes and event messages case-insensitively. Prefix matches sort first. We didn't add a fuzzy-matching crate because substring matching solves the current problem and is easy to explain.

## Testing a screen

Ratatui's `TestBackend` renders into an in-memory cell buffer. Tests render at 120 by 40 cells, concatenate the visible symbols and compare the result with an Insta snapshot. Styles are excluded deliberately. Changing a warning colour shouldn't rewrite twenty files.

Fixture time is fixed at `1_750_000_000`. Renderers never call `SystemTime::now()`, so ages and clock values don't drift between runs. This is dependency injection in its smallest form: `now_epoch` is ordinary state.

Reducer tests press synthetic `KeyEvent` values and inspect the view stack or pending effects. Integration tests run a real ProcessGrill agent and API on an ephemeral port, fetch through `HttpDataProvider`, render with `TestBackend`, and exercise WebSocket replay for events and logs. They don't need a pseudo-terminal. Raw-mode correctness stays concentrated in the small RAII guard.

Snapshots are useful only when somebody reads them. A changed snapshot is a review artefact, not a rubber stamp.

## Setting the table

Everything so far assumed a running node. A newcomer doesn't have one. Until now the honest install instructions were "clone the repository, build it, read the docs until the config makes sense". That's a poor first five minutes, so Relish also grew `relish setup`: one guided command that finds or installs `bun`, asks a handful of questions and writes a starter `reliaburger.toml`.

The interesting part is how little new machinery it needed. Phase 14 built a complete pipeline for getting trustworthy binaries onto a machine: release metadata fetching, dual Ed25519 signature verification, and the `BinaryStore` with its atomic symlink swap. Setup reuses all of it. A fresh install is just an upgrade with no previous version, staged into `~/.reliaburger/bin` through exactly the code path a live node uses to replace itself. One deliberate difference: there is no node.toml yet, so no operator signing key exists, and setup verifies the embedded release signature only. That's the same trust an air-gapped upgrade gets. And if a node is already running, setup refuses to swap the binary underneath it and points you at `relish upgrade start` instead, where the markers and automatic revert live.

The questions follow the pattern this whole chapter has been preaching: I/O at the edge, logic pure. `run` reads stdin and prints. Everything it decides is a pure function. `decide_install` compares versions and returns an enum (`NotInstalled`, `UpToDate`, `Upgradable`); `build_node_config` maps a `SetupAnswers` struct onto a full `NodeConfig`; `render_config` emits a minimal TOML file containing only the sections the user actually answered, because a newcomer's first config file should not be a wall of defaults. The test for that pair is one line of intent: parse the rendered file and assert it equals the built config. If the generated file and the node's parser ever disagree, the round-trip test fails before a user ever sees it.

There's no new Rust in any of this, which is rather the point. Struct update syntax (`IngressSection { enabled: true, ..IngressSection::default() }`) carries the whole "mostly defaults, with these overrides" story, and `bool::then_some` turns "the join answer was empty" into an `Option<String>` without an `if`/`else` in sight. By chapter thirteen, the language has stopped being the hard part.

## What we learned

The TUI is mostly a lesson in boundaries. One task owns mutable state. Renderers borrow it. Providers own I/O. Effects cross the line between the two. Once those boundaries were explicit, keyboard tests, HTTP tests and screen tests stopped needing special cases.

The event store is intentionally temporary. A later phase can persist events through Ketchup if operators need history across restarts. Full resolved configuration is also absent because the API doesn't expose it, and exec streaming remains request/response despite the design document's broader WebSocket sketch.

For now, Relish gives you apps, instances, nodes, jobs, routes, events, metrics and live logs in one terminal. No collection of half-forgotten watch commands required. Progress.
