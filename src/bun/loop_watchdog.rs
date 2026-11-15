//! A watchdog that catches the agent loop stalled and says which thread
//! held it (#1707).
//!
//! The loop meter ([`super::loop_meter`]) only learns a turn was slow when
//! the turn ends, and by then whatever held the loop has let go. #1707's
//! turns overran their 500 ms deadline by about 700 ms: the deadline's timer
//! fired, but nothing polled the loop's task until much later. The likely
//! culprit is a tokio worker blocked in a filesystem syscall with the loop's
//! task sitting in its LIFO slot, where no other worker can steal it.
//!
//! To catch that in the act, the loop stamps a [`LoopHeartbeat`] as each
//! turn begins and ends, and a dedicated `std::thread` (not a tokio task: a
//! task would be stuck behind the same blocked worker) checks the stamp every
//! 100 ms. When a turn has run longer than the threshold (750 ms by default,
//! `[node] loop_stall_snapshot_ms`), it reads every thread of the process
//! from `/proc/self/task/<tid>/` (name, state, wait channel, the syscall in
//! flight and, for threads that aren't sleeping, the kernel stack) and
//! writes one marked block to stderr, which the journal keeps:
//!
//! ```text
//! bun: agent loop stalled 812 ms in follow_up (consumer_sync/journal), 312 ms past its turn deadline; thread snapshot (pid 4242, 3 threads):
//! bun:   tid 4250 tokio-runtime-w D wchan=jbd2_log_wait_commit syscall=openat(0xffffff9c, 0xffff8000, 0x80241) [ran the agent loop]
//! bun:     jbd2_log_wait_commit+0xc4/0x120
//! bun:   tid 4251 tokio-runtime-w S wchan=futex_wait_queue syscall=futex(0xaaaa0000, 0x189, 0x0)
//! bun:   tid 4260 loop-watchdog R [this watchdog]
//! bun: end of thread snapshot
//! ```
//!
//! One stall gets one snapshot, however long it lasts; the watchdog re-arms
//! once the loop finishes that turn, and takes at most
//! [`MAX_SNAPSHOTS_PER_HOUR`]. Userspace backtraces of other threads are left
//! out on purpose: getting one means interrupting each thread with a signal
//! and walking its stack in a handler, which is `unsafe`, async-signal-unsafe
//! and could itself wedge the process. The kernel side is what #1707 needs:
//! which thread is in which syscall, and what it waits for.

use std::collections::VecDeque;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use super::loop_meter::LoopBranch;

/// A turn that has run this long counts as a stall, unless the node config
/// says otherwise.
pub const DEFAULT_STALL_THRESHOLD: Duration = Duration::from_millis(750);

/// How often the watchdog thread looks at the heartbeat.
pub const POLL_INTERVAL: Duration = Duration::from_millis(100);

/// At most this many snapshots in any hour. A stall past the limit still
/// gets a one-line note.
pub const MAX_SNAPSHOTS_PER_HOUR: usize = 10;

/// Threads listed in one snapshot, at most.
const MAX_THREADS: usize = 256;

/// Kernel stack frames kept per thread.
const MAX_STACK_FRAMES: usize = 12;

/// One snapshot's size cap, so a process with hundreds of threads can't
/// flood the journal.
const MAX_SNAPSHOT_BYTES: usize = 32 * 1024;

/// The prefix every line of a snapshot carries, like Bun's other lines.
const LINE_PREFIX: &str = "bun: ";

/// The line that closes a snapshot. The soak harness cuts blocks out of the
/// journal between the opening line's "thread snapshot" and this.
pub const SNAPSHOT_END: &str = "bun: end of thread snapshot";

const HOUR_MS: u64 = 3_600_000;

/// What the agent loop is doing, stamped where the watchdog thread can read
/// it without waiting for the loop.
pub struct LoopHeartbeat {
    origin: Instant,
    /// When the turn in progress began, in milliseconds since `origin` plus
    /// one; 0 between turns.
    turn_started_ms: AtomicU64,
    /// The turn's [`LoopBranch`], as its index.
    branch: AtomicU8,
    /// The Linux thread id that began the turn; 0 where there's no tid.
    loop_thread: AtomicU32,
    /// What the turn handles, for the snapshot's first line. A `&'static
    /// str` can't go in an atomic, so it takes a `std` mutex: the loop holds
    /// it only to store a pointer, the watchdog only to copy one, and neither
    /// holds it across an `await`.
    detail: std::sync::Mutex<Option<&'static str>>,
}

impl Default for LoopHeartbeat {
    fn default() -> Self {
        Self::new()
    }
}

/// The turn in progress, as the watchdog reads it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TurnStamp {
    /// When the turn began, on the heartbeat's clock.
    pub started_ms: u64,
    /// Which branch of the loop runs it.
    pub branch: LoopBranch,
    /// What it handles.
    pub detail: Option<&'static str>,
    /// The thread that began it, or 0.
    pub loop_thread: u32,
}

impl LoopHeartbeat {
    /// A heartbeat with no turn in progress.
    pub fn new() -> Self {
        Self {
            origin: Instant::now(),
            turn_started_ms: AtomicU64::new(0),
            branch: AtomicU8::new(0),
            loop_thread: AtomicU32::new(0),
            detail: std::sync::Mutex::new(None),
        }
    }

    /// The heartbeat's clock: milliseconds since it was made, plus one, so
    /// a stamp is never 0.
    pub fn now_ms(&self) -> u64 {
        u64::try_from(self.origin.elapsed().as_millis())
            .unwrap_or(u64::MAX - 1)
            .saturating_add(1)
    }

    /// A turn of `branch` begins on this thread.
    pub fn begin(&self, branch: LoopBranch, detail: Option<&'static str>) {
        if let Ok(mut slot) = self.detail.lock() {
            *slot = detail;
        }
        self.branch.store(branch as u8, Ordering::Relaxed);
        self.loop_thread
            .store(current_thread_id(), Ordering::Relaxed);
        self.turn_started_ms.store(self.now_ms(), Ordering::Release);
    }

    /// The turn in progress has ended.
    pub fn finish(&self) {
        self.turn_started_ms.store(0, Ordering::Release);
    }

    /// The turn in progress, or `None` between turns.
    pub fn current(&self) -> Option<TurnStamp> {
        let started_ms = self.turn_started_ms.load(Ordering::Acquire);
        if started_ms == 0 {
            return None;
        }
        let branch = LoopBranch::from_index(self.branch.load(Ordering::Relaxed))?;
        let detail = self.detail.lock().ok().and_then(|detail| *detail);
        Some(TurnStamp {
            started_ms,
            branch,
            detail,
            loop_thread: self.loop_thread.load(Ordering::Relaxed),
        })
    }
}

/// The calling thread's Linux thread id, as `/proc/self/task` names it.
#[cfg(target_os = "linux")]
fn current_thread_id() -> u32 {
    thread_local! {
        // gettid is a syscall; a thread's id never changes, so ask once.
        static TID: u32 = u32::try_from(nix::unistd::gettid().as_raw()).unwrap_or(0);
    }
    TID.with(|tid| *tid)
}

#[cfg(not(target_os = "linux"))]
fn current_thread_id() -> u32 {
    0
}

/// What the watchdog does about one look at the heartbeat.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// No turn, or a turn within the threshold.
    Healthy,
    /// A new stall: take a snapshot. `age_ms` is how long the turn has run.
    Capture { age_ms: u64 },
    /// A new stall past the hourly limit: note it, without a snapshot.
    RateLimited { age_ms: u64 },
    /// The stall already handled is still going on.
    AlreadyReported,
}

/// Decides, from the time and the turn in progress, when a stall starts and
/// whether it gets a snapshot. The clock is passed in, so tests drive it.
#[derive(Debug)]
pub struct IncidentTracker {
    threshold_ms: u64,
    max_per_hour: usize,
    /// The start of the stalled turn already handled, until the loop moves on.
    reported_turn: Option<u64>,
    /// When recent snapshots were taken, oldest first.
    captures: VecDeque<u64>,
}

impl IncidentTracker {
    /// A tracker that calls a turn longer than `threshold` a stall and takes
    /// at most `max_per_hour` snapshots in any hour.
    pub fn new(threshold: Duration, max_per_hour: usize) -> Self {
        Self {
            threshold_ms: u64::try_from(threshold.as_millis()).unwrap_or(u64::MAX),
            max_per_hour,
            reported_turn: None,
            captures: VecDeque::new(),
        }
    }

    /// Look at the turn that began at `turn_started_ms` (`None` between
    /// turns) at `now_ms`.
    pub fn observe(&mut self, now_ms: u64, turn_started_ms: Option<u64>) -> Verdict {
        let Some(started) = turn_started_ms else {
            self.reported_turn = None;
            return Verdict::Healthy;
        };
        let age_ms = now_ms.saturating_sub(started);
        if age_ms < self.threshold_ms {
            self.reported_turn = None;
            return Verdict::Healthy;
        }
        if self.reported_turn == Some(started) {
            return Verdict::AlreadyReported;
        }
        self.reported_turn = Some(started);
        while self
            .captures
            .front()
            .is_some_and(|taken| now_ms.saturating_sub(*taken) >= HOUR_MS)
        {
            self.captures.pop_front();
        }
        if self.captures.len() >= self.max_per_hour {
            return Verdict::RateLimited { age_ms };
        }
        self.captures.push_back(now_ms);
        Verdict::Capture { age_ms }
    }
}

/// How the watchdog runs.
#[derive(Debug, Clone, Copy)]
pub struct WatchdogSettings {
    /// A turn longer than this is a stall.
    pub threshold: Duration,
    /// How often the heartbeat is checked.
    pub poll: Duration,
    /// Snapshots in any hour, at most.
    pub max_per_hour: usize,
    /// The turn's own deadline, so the snapshot can say how late the loop is.
    pub turn_deadline: Duration,
}

impl WatchdogSettings {
    /// The production settings with `threshold` as the stall threshold.
    pub fn with_threshold(threshold: Duration) -> Self {
        Self {
            threshold,
            poll: POLL_INTERVAL,
            max_per_hour: MAX_SNAPSHOTS_PER_HOUR,
            turn_deadline: super::agent::TURN_RUNTIME_BUDGET,
        }
    }
}

/// The running watchdog thread. Dropping the handle tells the thread to
/// stop at its next look; [`LoopWatchdog::stop`] also waits for it.
pub struct LoopWatchdog {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl LoopWatchdog {
    /// Start watching `heartbeat`, writing snapshots to stderr.
    pub fn spawn_to_stderr(
        heartbeat: Arc<LoopHeartbeat>,
        settings: WatchdogSettings,
    ) -> std::io::Result<Self> {
        Self::spawn(heartbeat, settings, |block| eprintln!("{block}"))
    }

    /// Start watching `heartbeat`, handing each snapshot block to `sink`.
    pub fn spawn(
        heartbeat: Arc<LoopHeartbeat>,
        settings: WatchdogSettings,
        mut sink: impl FnMut(&str) + Send + 'static,
    ) -> std::io::Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let thread = std::thread::Builder::new()
            .name("loop-watchdog".to_string())
            .spawn(move || {
                let mut tracker = IncidentTracker::new(settings.threshold, settings.max_per_hour);
                while !thread_stop.load(Ordering::Relaxed) {
                    std::thread::sleep(settings.poll);
                    let turn = heartbeat.current();
                    let verdict =
                        tracker.observe(heartbeat.now_ms(), turn.map(|turn| turn.started_ms));
                    if let Some(block) = report(verdict, turn, &settings) {
                        sink(&block);
                    }
                }
            })?;
        Ok(Self {
            stop,
            thread: Some(thread),
        })
    }

    /// Stop the thread and wait for it to finish.
    pub fn stop(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for LoopWatchdog {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

/// What the watchdog writes for `verdict`, if anything.
fn report(
    verdict: Verdict,
    turn: Option<TurnStamp>,
    settings: &WatchdogSettings,
) -> Option<String> {
    let turn = turn?;
    let deadline_ms = u64::try_from(settings.turn_deadline.as_millis()).unwrap_or(u64::MAX);
    let stalled = |age_ms: u64| {
        format!(
            "{LINE_PREFIX}agent loop stalled {age_ms} ms in {}, {} ms past its turn deadline",
            describe_turn(&turn),
            age_ms.saturating_sub(deadline_ms)
        )
    };
    match verdict {
        Verdict::Healthy | Verdict::AlreadyReported => None,
        Verdict::RateLimited { age_ms } => Some(format!(
            "{}; thread snapshot skipped: {} already this hour",
            stalled(age_ms),
            settings.max_per_hour
        )),
        Verdict::Capture { age_ms } => {
            let threads = read_threads(Path::new("/proc/self/task"));
            Some(render(
                &stalled(age_ms),
                std::process::id(),
                threads,
                turn.loop_thread,
                current_thread_id(),
            ))
        }
    }
}

fn describe_turn(turn: &TurnStamp) -> String {
    match turn.detail {
        Some(detail) => format!("{} ({detail})", turn.branch.label()),
        None => turn.branch.label().to_string(),
    }
}

/// One thread of the process, read from `/proc/self/task/<tid>/`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadSnapshot {
    /// The thread id.
    pub tid: u32,
    /// The thread's name (`comm`), at most 15 bytes.
    pub name: String,
    /// The scheduler state from `stat`: `R`, `S`, `D` and so on.
    pub state: Option<char>,
    /// The kernel function the thread sleeps in, if it sleeps.
    pub wait_channel: Option<String>,
    /// The syscall in flight, described.
    pub syscall: Option<String>,
    /// The kernel stack's frames, or why it couldn't be read.
    pub stack: Result<Vec<String>, String>,
}

/// Every thread under `task_directory` (normally `/proc/self/task`), in tid
/// order, or why the directory couldn't be read.
pub fn read_threads(task_directory: &Path) -> Result<Vec<ThreadSnapshot>, String> {
    let entries = std::fs::read_dir(task_directory).map_err(|error| describe_error(&error))?;
    let mut tids: Vec<u32> = entries
        .filter_map(|entry| entry.ok()?.file_name().to_str()?.parse().ok())
        .collect();
    tids.sort_unstable();
    Ok(tids
        .into_iter()
        .map(|tid| read_thread(&task_directory.join(tid.to_string()), tid))
        .collect())
}

fn read_thread(directory: &Path, tid: u32) -> ThreadSnapshot {
    let read = |name: &str| std::fs::read_to_string(directory.join(name));
    ThreadSnapshot {
        tid,
        name: read("comm")
            .map(|name| name.trim_end().to_string())
            .unwrap_or_else(|_| "?".to_string()),
        state: read("stat").ok().as_deref().and_then(parse_state),
        wait_channel: read("wchan").ok().as_deref().and_then(parse_wait_channel),
        syscall: read("syscall").ok().as_deref().map(describe_syscall),
        stack: read("stack")
            .map(|stack| parse_stack(&stack))
            .map_err(|error| describe_error(&error)),
    }
}

fn describe_error(error: &std::io::Error) -> String {
    match error.kind() {
        std::io::ErrorKind::PermissionDenied => "permission denied (needs root)".to_string(),
        std::io::ErrorKind::NotFound => "not available".to_string(),
        _ => error.to_string(),
    }
}

/// The state letter from a `stat` line. The thread name in parentheses may
/// itself hold spaces or `)`, so the state is the first field after the last
/// `)`.
pub fn parse_state(stat: &str) -> Option<char> {
    let (_, rest) = stat.rsplit_once(')')?;
    rest.trim_start().chars().next()
}

/// A `wchan` entry, or `None` for a thread that isn't waiting (`0`).
pub fn parse_wait_channel(wchan: &str) -> Option<String> {
    let wchan = wchan.trim();
    (!wchan.is_empty() && wchan != "0").then(|| wchan.to_string())
}

/// A `syscall` entry in words: `running`, `not in a syscall` (`-1 sp pc`),
/// or the call's name and first three arguments.
pub fn describe_syscall(raw: &str) -> String {
    let mut fields = raw.split_whitespace();
    let Some(number) = fields.next() else {
        return "unknown".to_string();
    };
    if number == "running" {
        return "running".to_string();
    }
    let Ok(number) = number.parse::<libc::c_long>() else {
        return format!("unknown ({})", raw.trim());
    };
    if number < 0 {
        return "not in a syscall".to_string();
    }
    let name = syscall_name(number)
        .map(str::to_string)
        .unwrap_or_else(|| format!("syscall_{number}"));
    let arguments: Vec<&str> = fields.take(3).collect();
    format!("{name}({})", arguments.join(", "))
}

/// The kernel stack's frames, without the `[<0>] ` address prefix the
/// kernel hides behind, at most [`MAX_STACK_FRAMES`] of them.
pub fn parse_stack(stack: &str) -> Vec<String> {
    let frames: Vec<&str> = stack
        .lines()
        .map(|line| {
            line.split_once("] ")
                .map_or(line, |(_, frame)| frame)
                .trim()
        })
        .filter(|frame| !frame.is_empty())
        .collect();
    let mut kept: Vec<String> = frames
        .iter()
        .take(MAX_STACK_FRAMES)
        .map(|frame| frame.to_string())
        .collect();
    if frames.len() > MAX_STACK_FRAMES {
        kept.push(format!("... {} more", frames.len() - MAX_STACK_FRAMES));
    }
    kept
}

/// Whether a snapshot shows `thread`'s kernel stack. A sleeping thread's
/// wait channel and syscall say enough, and most threads sleep; the stack
/// is for threads in uninterruptible sleep (`D`), running, or the one that
/// began the stalled turn.
fn shows_stack(thread: &ThreadSnapshot, loop_thread: u32) -> bool {
    thread.tid == loop_thread || !matches!(thread.state, Some('S') | Some('I'))
}

/// The snapshot block: `header`, one line per thread (with the kernel stack
/// where [`shows_stack`] says), and the end marker, at most
/// [`MAX_SNAPSHOT_BYTES`] before the marker.
pub fn render(
    header: &str,
    pid: u32,
    threads: Result<Vec<ThreadSnapshot>, String>,
    loop_thread: u32,
    own_thread: u32,
) -> String {
    let threads = match threads {
        Ok(threads) => threads,
        Err(reason) => {
            return format!(
                "{header}; thread snapshot (pid {pid}):\n{LINE_PREFIX}  threads unreadable: {reason}\n{SNAPSHOT_END}"
            );
        }
    };
    let mut block = format!(
        "{header}; thread snapshot (pid {pid}, {} threads):",
        threads.len()
    );
    let mut lines = Vec::new();
    for thread in threads.iter().take(MAX_THREADS) {
        lines.push(thread_line(thread, loop_thread, own_thread));
        if thread.tid == own_thread || !shows_stack(thread, loop_thread) {
            continue;
        }
        match &thread.stack {
            Ok(frames) => lines.extend(frames.iter().map(|frame| format!("  {frame}"))),
            Err(reason) => lines.push(format!("  stack unreadable: {reason}")),
        }
    }
    if threads.len() > MAX_THREADS {
        lines.push(format!("... {} more threads", threads.len() - MAX_THREADS));
    }
    for line in lines {
        if block.len() + line.len() > MAX_SNAPSHOT_BYTES {
            block.push_str(&format!("\n{LINE_PREFIX}  ... snapshot truncated"));
            break;
        }
        block.push('\n');
        block.push_str(LINE_PREFIX);
        block.push_str("  ");
        block.push_str(&line);
    }
    block.push('\n');
    block.push_str(SNAPSHOT_END);
    block
}

fn thread_line(thread: &ThreadSnapshot, loop_thread: u32, own_thread: u32) -> String {
    let mut line = format!(
        "tid {} {} {}",
        thread.tid,
        thread.name,
        thread.state.unwrap_or('?')
    );
    if thread.tid == own_thread {
        line.push_str(" [this watchdog]");
        return line;
    }
    if let Some(wait_channel) = &thread.wait_channel {
        line.push_str(&format!(" wchan={wait_channel}"));
    }
    if let Some(syscall) = &thread.syscall {
        line.push_str(&format!(" syscall={syscall}"));
    }
    if thread.tid == loop_thread {
        line.push_str(" [ran the agent loop]");
    }
    line
}

/// The name of syscall `number` on this architecture, for the calls a
/// blocked thread is likely to be in.
#[cfg(target_os = "linux")]
fn syscall_name(number: libc::c_long) -> Option<&'static str> {
    const NAMES: &[(libc::c_long, &str)] = &[
        (libc::SYS_read, "read"),
        (libc::SYS_write, "write"),
        (libc::SYS_pread64, "pread64"),
        (libc::SYS_pwrite64, "pwrite64"),
        (libc::SYS_close, "close"),
        (libc::SYS_openat, "openat"),
        (libc::SYS_fsync, "fsync"),
        (libc::SYS_fdatasync, "fdatasync"),
        (libc::SYS_sync, "sync"),
        (libc::SYS_syncfs, "syncfs"),
        (libc::SYS_sync_file_range, "sync_file_range"),
        (libc::SYS_fallocate, "fallocate"),
        (libc::SYS_ftruncate, "ftruncate"),
        (libc::SYS_renameat, "renameat"),
        (libc::SYS_renameat2, "renameat2"),
        (libc::SYS_unlinkat, "unlinkat"),
        (libc::SYS_mkdirat, "mkdirat"),
        (libc::SYS_newfstatat, "newfstatat"),
        (libc::SYS_statx, "statx"),
        (libc::SYS_getdents64, "getdents64"),
        (libc::SYS_flock, "flock"),
        (libc::SYS_fcntl, "fcntl"),
        (libc::SYS_ioctl, "ioctl"),
        (libc::SYS_futex, "futex"),
        (libc::SYS_epoll_pwait, "epoll_pwait"),
        (libc::SYS_ppoll, "ppoll"),
        (libc::SYS_nanosleep, "nanosleep"),
        (libc::SYS_clock_nanosleep, "clock_nanosleep"),
        (libc::SYS_sched_yield, "sched_yield"),
        (libc::SYS_socket, "socket"),
        (libc::SYS_bind, "bind"),
        (libc::SYS_connect, "connect"),
        (libc::SYS_accept4, "accept4"),
        (libc::SYS_sendto, "sendto"),
        (libc::SYS_recvfrom, "recvfrom"),
        (libc::SYS_sendmsg, "sendmsg"),
        (libc::SYS_recvmsg, "recvmsg"),
        (libc::SYS_wait4, "wait4"),
        (libc::SYS_waitid, "waitid"),
        (libc::SYS_execve, "execve"),
        (libc::SYS_clone3, "clone3"),
        (libc::SYS_mmap, "mmap"),
        (libc::SYS_munmap, "munmap"),
        (libc::SYS_madvise, "madvise"),
        (libc::SYS_mount, "mount"),
        (libc::SYS_umount2, "umount2"),
        (libc::SYS_setns, "setns"),
        (libc::SYS_unshare, "unshare"),
        (libc::SYS_bpf, "bpf"),
        (libc::SYS_io_uring_enter, "io_uring_enter"),
    ];
    NAMES
        .iter()
        .find(|(known, _)| *known == number)
        .map(|(_, name)| *name)
}

#[cfg(not(target_os = "linux"))]
fn syscall_name(_number: libc::c_long) -> Option<&'static str> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const THRESHOLD: Duration = Duration::from_millis(750);

    fn tracker() -> IncidentTracker {
        IncidentTracker::new(THRESHOLD, 3)
    }

    #[test]
    fn no_turn_in_progress_is_healthy() {
        assert_eq!(tracker().observe(10_000, None), Verdict::Healthy);
    }

    #[test]
    fn a_turn_within_the_threshold_is_healthy() {
        assert_eq!(tracker().observe(1_749, Some(1_000)), Verdict::Healthy);
    }

    #[test]
    fn a_turn_past_the_threshold_is_captured_once() {
        let mut tracker = tracker();
        assert_eq!(
            tracker.observe(1_750, Some(1_000)),
            Verdict::Capture { age_ms: 750 }
        );
        assert_eq!(
            tracker.observe(1_850, Some(1_000)),
            Verdict::AlreadyReported
        );
        assert_eq!(
            tracker.observe(9_000, Some(1_000)),
            Verdict::AlreadyReported
        );
    }

    #[test]
    fn the_watchdog_rearms_once_the_stalled_turn_ends() {
        let mut tracker = tracker();
        assert!(matches!(
            tracker.observe(2_000, Some(1_000)),
            Verdict::Capture { .. }
        ));
        assert_eq!(tracker.observe(2_100, None), Verdict::Healthy);
        assert_eq!(
            tracker.observe(3_000, Some(2_200)),
            Verdict::Capture { age_ms: 800 }
        );
    }

    #[test]
    fn a_new_stalled_turn_is_a_new_incident_even_unseen_between() {
        // The loop finished the stalled turn and began another, which also
        // stalled, between two looks.
        let mut tracker = tracker();
        assert!(matches!(
            tracker.observe(2_000, Some(1_000)),
            Verdict::Capture { .. }
        ));
        assert_eq!(
            tracker.observe(3_000, Some(2_050)),
            Verdict::Capture { age_ms: 950 }
        );
    }

    #[test]
    fn snapshots_past_the_hourly_limit_are_rate_limited() {
        let mut tracker = tracker();
        for start in [0, 10_000, 20_000] {
            assert!(matches!(
                tracker.observe(start + 1_000, Some(start + 1)),
                Verdict::Capture { .. }
            ));
            tracker.observe(start + 1_100, None);
        }
        assert_eq!(
            tracker.observe(31_000, Some(30_001)),
            Verdict::RateLimited { age_ms: 999 }
        );
        // Still one verdict per incident.
        assert_eq!(
            tracker.observe(31_100, Some(30_001)),
            Verdict::AlreadyReported
        );
    }

    #[test]
    fn the_rate_limit_frees_a_slot_an_hour_after_a_snapshot() {
        let mut tracker = tracker();
        for start in [0, 10_000, 20_000] {
            tracker.observe(start + 1_000, Some(start + 1));
            tracker.observe(start + 1_100, None);
        }
        assert!(matches!(
            tracker.observe(HOUR_MS + 999, Some(HOUR_MS)),
            Verdict::RateLimited { .. }
        ));
        tracker.observe(HOUR_MS + 1_100, None);
        assert!(matches!(
            tracker.observe(HOUR_MS + 2_000, Some(HOUR_MS + 1_200)),
            Verdict::Capture { .. }
        ));
    }

    #[test]
    fn the_heartbeat_reads_the_turn_in_progress() {
        let heartbeat = LoopHeartbeat::new();
        assert_eq!(heartbeat.current(), None);
        heartbeat.begin(LoopBranch::FollowUp, Some("consumer_sync/journal"));
        let turn = heartbeat.current().unwrap();
        assert_eq!(turn.branch, LoopBranch::FollowUp);
        assert_eq!(turn.detail, Some("consumer_sync/journal"));
        assert!(turn.started_ms >= 1);
        assert_eq!(turn.loop_thread, current_thread_id());
        heartbeat.finish();
        assert_eq!(heartbeat.current(), None);
    }

    #[test]
    fn the_state_is_read_after_a_thread_name_holding_a_parenthesis() {
        assert_eq!(parse_state("4250 (tokio-runtime-w) D 1 4242"), Some('D'));
        assert_eq!(parse_state("4251 (odd) name) S 1 4242"), Some('S'));
        assert_eq!(parse_state("garbage"), None);
    }

    #[test]
    fn a_zero_wait_channel_means_not_waiting() {
        assert_eq!(parse_wait_channel("0"), None);
        assert_eq!(parse_wait_channel(""), None);
        assert_eq!(
            parse_wait_channel("jbd2_log_wait_commit"),
            Some("jbd2_log_wait_commit".to_string())
        );
    }

    #[test]
    fn a_syscall_entry_is_described_in_words() {
        assert_eq!(describe_syscall("running\n"), "running");
        assert_eq!(
            describe_syscall("-1 0xffffc0000 0xaaaa0000\n"),
            "not in a syscall"
        );
        assert_eq!(describe_syscall(""), "unknown");
        let described = describe_syscall("99999 0x1 0x2 0x3 0x4 0x5 0x6 0x7 0x8\n");
        assert_eq!(described, "syscall_99999(0x1, 0x2, 0x3)");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_syscall_number_is_named_on_linux() {
        let raw = format!("{} 0x5 0x0 0x0 0x0 0x0 0x0 0x0 0x0", libc::SYS_fsync);
        assert_eq!(describe_syscall(&raw), "fsync(0x5, 0x0, 0x0)");
    }

    #[test]
    fn a_kernel_stack_loses_its_prefix_and_is_capped() {
        let frames: String = (0..20)
            .map(|frame| format!("[<0>] frame_{frame}+0x10/0x20\n"))
            .collect();
        let parsed = parse_stack(&frames);
        assert_eq!(parsed.len(), MAX_STACK_FRAMES + 1);
        assert_eq!(parsed[0], "frame_0+0x10/0x20");
        assert_eq!(parsed[MAX_STACK_FRAMES], "... 8 more");
    }

    /// Lay out a fake `/proc/self/task` with one directory per thread.
    fn fixture_task(root: &Path, tid: u32, files: &[(&str, &str)]) {
        let directory = root.join(tid.to_string());
        std::fs::create_dir_all(&directory).unwrap();
        for (name, content) in files {
            std::fs::write(directory.join(name), content).unwrap();
        }
    }

    fn fixture() -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        fixture_task(
            root.path(),
            4250,
            &[
                ("comm", "tokio-runtime-w\n"),
                ("stat", "4250 (tokio-runtime-w) D 1 4242 4242 0 -1\n"),
                ("wchan", "jbd2_log_wait_commit"),
                (
                    "syscall",
                    "56 0xffffff9c 0xffff8000 0x80241 0x1a4 0x0 0x0 0xffffc000 0xaaaa0000\n",
                ),
                (
                    "stack",
                    "[<0>] jbd2_log_wait_commit+0xc4/0x120\n[<0>] ext4_sync_file+0x1f0/0x3a0\n",
                ),
            ],
        );
        fixture_task(
            root.path(),
            4251,
            &[
                ("comm", "tokio-runtime-w\n"),
                ("stat", "4251 (tokio-runtime-w) S 1 4242\n"),
                ("wchan", "futex_wait_queue"),
                ("syscall", "98 0xaaaa0000 0x189 0x0 0x0 0x0 0x0 0x1 0x2\n"),
                ("stack", "[<0>] futex_wait_queue+0x60/0x90\n"),
            ],
        );
        // No stack file: as for a thread whose stack the kernel won't show.
        fixture_task(
            root.path(),
            4260,
            &[
                ("comm", "loop-watchdog\n"),
                ("stat", "4260 (loop-watchdog) R 1 4242\n"),
                ("wchan", "0"),
                ("syscall", "running\n"),
            ],
        );
        std::fs::create_dir_all(root.path().join("not-a-tid")).unwrap();
        root
    }

    #[test]
    fn threads_are_read_from_a_task_directory_in_tid_order() {
        let root = fixture();
        let threads = read_threads(root.path()).unwrap();
        let tids: Vec<u32> = threads.iter().map(|thread| thread.tid).collect();
        assert_eq!(tids, vec![4250, 4251, 4260]);
        let blocked = &threads[0];
        assert_eq!(blocked.name, "tokio-runtime-w");
        assert_eq!(blocked.state, Some('D'));
        assert_eq!(
            blocked.wait_channel.as_deref(),
            Some("jbd2_log_wait_commit")
        );
        assert_eq!(
            blocked.stack,
            Ok(vec![
                "jbd2_log_wait_commit+0xc4/0x120".to_string(),
                "ext4_sync_file+0x1f0/0x3a0".to_string()
            ])
        );
        assert_eq!(threads[2].stack, Err("not available".to_string()));
        assert_eq!(threads[2].syscall.as_deref(), Some("running"));
    }

    #[test]
    fn an_unreadable_task_directory_is_reported_not_fatal() {
        let root = tempfile::tempdir().unwrap();
        let missing = read_threads(&root.path().join("missing"));
        assert_eq!(missing, Err("not available".to_string()));
        let block = render(
            "bun: agent loop stalled 800 ms in command",
            7,
            missing,
            0,
            0,
        );
        assert_eq!(
            block,
            "bun: agent loop stalled 800 ms in command; thread snapshot (pid 7):\n\
             bun:   threads unreadable: not available\n\
             bun: end of thread snapshot"
        );
    }

    #[test]
    fn a_snapshot_shows_stacks_only_for_threads_that_are_not_sleeping() {
        let root = fixture();
        let threads = read_threads(root.path()).unwrap();
        let block = render(
            "bun: agent loop stalled 812 ms in follow_up (consumer_sync/journal), 312 ms past its turn deadline",
            4242,
            Ok(threads),
            4250,
            4260,
        );
        let lines: Vec<&str> = block.lines().collect();
        assert_eq!(
            lines[0],
            "bun: agent loop stalled 812 ms in follow_up (consumer_sync/journal), \
             312 ms past its turn deadline; thread snapshot (pid 4242, 3 threads):"
        );
        assert!(
            lines[1].starts_with("bun:   tid 4250 tokio-runtime-w D wchan=jbd2_log_wait_commit")
        );
        assert!(lines[1].ends_with("[ran the agent loop]"));
        assert_eq!(lines[2], "bun:     jbd2_log_wait_commit+0xc4/0x120");
        assert_eq!(lines[3], "bun:     ext4_sync_file+0x1f0/0x3a0");
        assert!(lines[4].starts_with("bun:   tid 4251 tokio-runtime-w S wchan=futex_wait_queue"));
        assert_eq!(lines[5], "bun:   tid 4260 loop-watchdog R [this watchdog]");
        assert_eq!(lines[6], SNAPSHOT_END);
        assert_eq!(lines.len(), 7);
    }

    #[test]
    fn the_loop_threads_stack_is_shown_even_while_it_sleeps() {
        let root = fixture();
        let threads = read_threads(root.path()).unwrap();
        let block = render("bun: stalled", 1, Ok(threads), 4251, 4260);
        assert!(block.contains("bun:     futex_wait_queue+0x60/0x90"));
    }

    #[test]
    fn a_huge_snapshot_is_truncated_but_still_closed() {
        let threads: Vec<ThreadSnapshot> = (0..400)
            .map(|tid| ThreadSnapshot {
                tid,
                name: "tokio-runtime-w".to_string(),
                state: Some('D'),
                wait_channel: Some("io_schedule".to_string()),
                syscall: Some("fsync(0x5, 0x0, 0x0)".to_string()),
                stack: Ok((0..MAX_STACK_FRAMES)
                    .map(|frame| format!("frame_{frame}+0x10/0x20"))
                    .collect()),
            })
            .collect();
        let block = render("bun: stalled", 1, Ok(threads), 0, u32::MAX);
        assert!(block.len() <= MAX_SNAPSHOT_BYTES + 200);
        assert!(block.contains("... snapshot truncated"));
        assert!(block.ends_with(SNAPSHOT_END));
    }

    #[test]
    fn a_rate_limited_stall_gets_one_line_without_a_snapshot() {
        let turn = TurnStamp {
            started_ms: 1,
            branch: LoopBranch::Command,
            detail: Some("status"),
            loop_thread: 0,
        };
        let settings = WatchdogSettings {
            threshold: THRESHOLD,
            poll: POLL_INTERVAL,
            max_per_hour: 10,
            turn_deadline: Duration::from_millis(500),
        };
        let line = report(Verdict::RateLimited { age_ms: 900 }, Some(turn), &settings).unwrap();
        assert_eq!(
            line,
            "bun: agent loop stalled 900 ms in command (status), 400 ms past its turn deadline; \
             thread snapshot skipped: 10 already this hour"
        );
        assert_eq!(
            report(Verdict::AlreadyReported, Some(turn), &settings),
            None
        );
        assert_eq!(report(Verdict::Healthy, Some(turn), &settings), None);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn this_process_threads_are_readable_from_proc() {
        let threads = read_threads(Path::new("/proc/self/task")).unwrap();
        let own = current_thread_id();
        let me = threads.iter().find(|thread| thread.tid == own).unwrap();
        assert_eq!(me.state, Some('R'));
        assert!(!me.name.is_empty());
    }
}
