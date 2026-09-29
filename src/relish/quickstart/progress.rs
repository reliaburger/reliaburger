//! Live setup progress: one line per step, and where the time went.
//!
//! On a terminal the lines update in place; anywhere else (a CI log, a pipe)
//! each step prints a line when it starts and another when it finishes.

use std::{
    io::Write,
    sync::{
        Arc, OnceLock,
        atomic::{AtomicU32, AtomicU64, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};

/// Broad phase of setup, used to group the final summary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Stage {
    /// Host prerequisites.
    Host,
    /// Tooling, guest image and binary downloads.
    Download,
    /// Creating or starting the VMs.
    Boot,
    /// Installing, enrolling and starting nodes.
    Configure,
    /// Quorum, demo workload and ingress probe.
    Verify,
}

impl Stage {
    const ALL: [Stage; 5] = [
        Stage::Host,
        Stage::Download,
        Stage::Boot,
        Stage::Configure,
        Stage::Verify,
    ];

    fn label(self) -> &'static str {
        match self {
            Stage::Host => "host checks",
            Stage::Download => "downloads",
            Stage::Boot => "VM boot",
            Stage::Configure => "node setup",
            Stage::Verify => "cluster checks",
        }
    }
}

/// How a step ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// The step completed.
    Done,
    /// The step returned an error.
    Failed,
    /// Setup stopped (an error elsewhere, or a deadline) while it ran.
    Interrupted,
}

#[derive(Debug, Clone)]
struct Finished {
    at: Instant,
    outcome: Outcome,
}

/// Transfer counters for a download step.
#[derive(Debug, Default)]
struct Transfer {
    /// Bytes already on disk before this run, for example a resumed partial.
    resumed: OnceLock<u64>,
    /// Expected size, when the server said.
    total: OnceLock<u64>,
    /// Bytes on disk now, including `resumed`.
    bytes: AtomicU64,
    /// Bytes received during this run, across every attempt. The speed is
    /// worked out from these, so neither a resumed partial nor a restart
    /// distorts it.
    fresh: AtomicU64,
    /// How many times a dropped transfer has been retried in this run.
    retries: AtomicU32,
    /// Where the latest retry picked up, 0 when it had to start again.
    retry_offset: AtomicU64,
}

#[derive(Debug)]
struct StepState {
    label: String,
    stage: Stage,
    started: Instant,
    transfer: Transfer,
    note: OnceLock<String>,
    finished: OnceLock<Finished>,
}

/// A handle to one running step. Cloning it shares the same step.
#[derive(Debug, Clone)]
pub struct Step(Arc<StepState>);

impl Step {
    /// Record the start of a transfer: `resumed` bytes are already present.
    pub fn begin_transfer(&self, resumed: u64, total: Option<u64>) {
        let _ = self.0.transfer.resumed.set(resumed);
        if let Some(total) = total {
            let _ = self.0.transfer.total.set(total);
        }
        self.0.transfer.bytes.store(resumed, Ordering::Relaxed);
    }

    /// Count newly received bytes.
    pub fn add_bytes(&self, count: u64) {
        self.0.transfer.bytes.fetch_add(count, Ordering::Relaxed);
        self.0.transfer.fresh.fetch_add(count, Ordering::Relaxed);
    }

    /// Record retry number `attempt` of a dropped transfer, which carries
    /// on from `offset` bytes (0 when the server made it start again).
    pub fn retry(&self, attempt: u32, offset: u64, total: Option<u64>) {
        let _ = self.0.transfer.resumed.set(0);
        if let Some(total) = total {
            let _ = self.0.transfer.total.set(total);
        }
        self.0
            .transfer
            .retry_offset
            .store(offset, Ordering::Relaxed);
        self.0.transfer.retries.store(attempt, Ordering::Relaxed);
        self.0.transfer.bytes.store(offset, Ordering::Relaxed);
    }

    /// Attach a short explanation, such as "cached". The first one sticks.
    pub fn note(&self, note: &str) {
        let _ = self.0.note.set(note.to_owned());
    }

    /// Mark the step complete.
    pub fn done(&self) {
        self.finish(Outcome::Done);
    }

    /// Mark the step complete with a short explanation.
    pub fn done_with(&self, note: &str) {
        self.note(note);
        self.done();
    }

    /// Mark the step failed.
    pub fn fail(&self) {
        self.finish(Outcome::Failed);
    }

    /// Mark the step done or failed from a result, passing the result through.
    pub fn record<T, E>(&self, result: Result<T, E>) -> Result<T, E> {
        match &result {
            Ok(_) => self.done(),
            Err(_) => self.fail(),
        }
        result
    }

    fn finish(&self, outcome: Outcome) {
        // The first outcome wins; later calls (say, a failure after done) are ignored.
        let _ = self.0.finished.set(Finished {
            at: Instant::now(),
            outcome,
        });
    }
}

enum Message {
    Add(Step),
    Note(String),
    Close,
}

/// The progress display for one setup run.
pub struct Progress {
    sender: mpsc::Sender<Message>,
    renderer: Option<std::thread::JoinHandle<Vec<Step>>>,
    started: Instant,
}

impl Progress {
    /// Draw on standard output, updating in place only on a real terminal.
    pub fn stdout() -> Self {
        use std::io::IsTerminal;
        let interactive = std::io::stdout().is_terminal()
            && std::env::var_os("TERM").is_some_and(|term| term != "dumb");
        Self::new(std::io::stdout(), interactive)
    }

    fn new<W: Write + Send + 'static>(output: W, interactive: bool) -> Self {
        let (sender, receiver) = mpsc::channel();
        // Terminal writes can block, so the renderer gets its own thread
        // instead of borrowing a Tokio worker.
        let renderer = std::thread::spawn(move || render(output, interactive, &receiver));
        Self {
            sender,
            renderer: Some(renderer),
            started: Instant::now(),
        }
    }

    /// Start a new step and show it.
    pub fn step(&self, stage: Stage, label: impl Into<String>) -> Step {
        let step = Step(Arc::new(StepState {
            label: label.into(),
            stage,
            started: Instant::now(),
            transfer: Transfer::default(),
            note: OnceLock::new(),
            finished: OnceLock::new(),
        }));
        let _ = self.sender.send(Message::Add(step.clone()));
        step
    }

    /// Print a message above the progress lines.
    pub fn note(&self, message: impl Into<String>) {
        let _ = self.sender.send(Message::Note(message.into()));
    }

    /// Stop drawing and return the timings of every step.
    pub async fn finish(mut self) -> Timings {
        let _ = self.sender.send(Message::Close);
        let steps = match self.renderer.take() {
            Some(renderer) => tokio::task::spawn_blocking(move || renderer.join().ok())
                .await
                .ok()
                .flatten()
                .unwrap_or_default(),
            None => Vec::new(),
        };
        Timings::from_steps(self.started, Instant::now(), &steps)
    }
}

impl Drop for Progress {
    fn drop(&mut self) {
        let _ = self.sender.send(Message::Close);
    }
}

const TICK: Duration = Duration::from_millis(125);

fn render<W: Write>(
    mut output: W,
    interactive: bool,
    receiver: &mpsc::Receiver<Message>,
) -> Vec<Step> {
    let mut steps: Vec<Step> = Vec::new();
    // Non-interactive output prints each finish once.
    let mut reported = 0_usize;
    let mut reported_finished: Vec<bool> = Vec::new();
    let mut drawn = 0_usize;
    loop {
        let message = receiver.recv_timeout(TICK);
        let closing = matches!(
            message,
            Ok(Message::Close) | Err(mpsc::RecvTimeoutError::Disconnected)
        );
        let mut notes = Vec::new();
        match message {
            Ok(Message::Add(step)) => steps.push(step),
            Ok(Message::Note(note)) => notes.push(note),
            _ => {}
        }
        // Drain whatever else arrived, so a burst of steps draws once.
        let mut close_seen = closing;
        while let Ok(message) = receiver.try_recv() {
            match message {
                Message::Add(step) => steps.push(step),
                Message::Note(note) => notes.push(note),
                Message::Close => close_seen = true,
            }
        }
        let now = Instant::now();
        if close_seen {
            for step in &steps {
                // Anything still running when setup ends was cut short.
                let _ = step.0.finished.set(Finished {
                    at: now,
                    outcome: Outcome::Interrupted,
                });
            }
        }
        if interactive {
            let width = crossterm::terminal::size()
                .map(|(columns, _)| usize::from(columns))
                .unwrap_or(100);
            let height = crossterm::terminal::size()
                .map(|(_, rows)| usize::from(rows))
                .unwrap_or(40);
            let mut frame = String::new();
            if drawn > 0 {
                frame.push_str(&format!("\x1b[{drawn}A"));
            }
            for note in &notes {
                frame.push_str(&format!("\r\x1b[2K{note}\n"));
            }
            // Draw only what fits, or the cursor can't climb back up.
            let visible = height.saturating_sub(2).max(1);
            let first = steps.len().saturating_sub(visible);
            for step in &steps[first..] {
                frame.push_str(&format!("\r\x1b[2K{}\n", line(step, now, width)));
            }
            drawn = steps.len() - first;
            let _ = output.write_all(frame.as_bytes());
        } else {
            for note in &notes {
                let _ = writeln!(output, "{note}");
            }
            for step in &steps[reported..] {
                let _ = writeln!(output, "... {}", step.0.label);
            }
            reported = steps.len();
            reported_finished.resize(steps.len(), false);
            for (step, reported) in steps.iter().zip(reported_finished.iter_mut()) {
                if !*reported && step.0.finished.get().is_some() {
                    *reported = true;
                    let _ = writeln!(output, "{}", line(step, now, usize::MAX));
                }
            }
        }
        let _ = output.flush();
        if close_seen {
            return steps;
        }
    }
}

/// Human-readable size with binary units.
fn bytes(count: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KiB", "MiB", "GiB"];
    let mut value = count as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{count} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// One display line: status, label, transfer detail and elapsed seconds.
fn line(step: &Step, now: Instant, width: usize) -> String {
    let state = &step.0;
    let finished = state.finished.get();
    let end = finished.map_or(now, |finished| finished.at);
    let elapsed = end.saturating_duration_since(state.started);
    let status = match finished.map(|finished| finished.outcome) {
        None => "[ .. ]",
        Some(Outcome::Done) => "[ ok ]",
        Some(Outcome::Failed) => "[FAIL]",
        Some(Outcome::Interrupted) => "[stop]",
    };
    let mut detail = String::new();
    if let Some(resumed) = state.transfer.resumed.get() {
        let received = state.transfer.bytes.load(Ordering::Relaxed);
        detail = match state.transfer.total.get() {
            Some(total) => format!("{} / {}", bytes(received), bytes(*total)),
            None => bytes(received),
        };
        let seconds = elapsed.as_secs_f64();
        let fresh = state.transfer.fresh.load(Ordering::Relaxed);
        if seconds >= 0.5 && fresh > 0 {
            detail.push_str(&format!("  {}/s", bytes((fresh as f64 / seconds) as u64)));
        }
        let retries = state.transfer.retries.load(Ordering::Relaxed);
        if retries > 0 {
            match state.transfer.retry_offset.load(Ordering::Relaxed) {
                0 => detail.push_str(&format!("  restarted (retry {retries})")),
                offset => {
                    detail.push_str(&format!("  resumed at {} (retry {retries})", bytes(offset)))
                }
            }
        } else if *resumed > 0 {
            detail.push_str(&format!("  resumed at {}", bytes(*resumed)));
        }
    }
    if let Some(note) = state.note.get() {
        if !detail.is_empty() {
            detail.push_str("  ");
        }
        detail.push_str(note);
    }
    let text = format!(
        "{status} {:<28} {:>7.1}s  {detail}",
        state.label,
        elapsed.as_secs_f64()
    );
    let text = text.trim_end();
    if text.chars().count() > width {
        text.chars().take(width.saturating_sub(1)).collect()
    } else {
        text.to_owned()
    }
}

/// Timing of one step, as saved and summarised.
#[derive(Debug, Clone, serde::Serialize)]
pub struct StepTiming {
    /// What the step did.
    pub label: String,
    /// Which part of setup it belongs to.
    pub stage: Stage,
    /// Seconds from the start of setup to the start of this step.
    pub start_seconds: f64,
    /// How long the step took.
    pub seconds: f64,
    /// How it ended.
    pub outcome: Outcome,
    /// Bytes transferred during this run, for downloads.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bytes: Option<u64>,
    /// Extra context, such as "cached".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// Every step's timing for one setup run.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Timings {
    /// Wall-clock seconds for the whole run.
    pub total_seconds: f64,
    /// Steps in the order they started.
    pub steps: Vec<StepTiming>,
}

impl Timings {
    fn from_steps(started: Instant, now: Instant, steps: &[Step]) -> Self {
        let steps = steps
            .iter()
            .map(|step| {
                let state = &step.0;
                let finished = state.finished.get();
                let end = finished.map_or(now, |finished| finished.at);
                StepTiming {
                    label: state.label.clone(),
                    stage: state.stage,
                    start_seconds: state
                        .started
                        .saturating_duration_since(started)
                        .as_secs_f64(),
                    seconds: end.saturating_duration_since(state.started).as_secs_f64(),
                    outcome: finished.map_or(Outcome::Interrupted, |finished| finished.outcome),
                    bytes: state
                        .transfer
                        .resumed
                        .get()
                        .map(|_| state.transfer.fresh.load(Ordering::Relaxed)),
                    note: state.note.get().cloned(),
                }
            })
            .collect();
        Self {
            total_seconds: now.saturating_duration_since(started).as_secs_f64(),
            steps,
        }
    }

    /// Wall-clock time per stage. Steps in a stage overlap (three VMs boot
    /// at once), so a stage's time is its first start to its last finish,
    /// not the sum of its steps.
    pub fn summary(&self) -> String {
        let mut text = format!(
            "where the time went ({:.1}s in total):\n",
            self.total_seconds
        );
        for stage in Stage::ALL {
            let steps: Vec<_> = self
                .steps
                .iter()
                .filter(|step| step.stage == stage)
                .collect();
            if steps.is_empty() {
                continue;
            }
            let start = steps
                .iter()
                .map(|step| step.start_seconds)
                .fold(f64::INFINITY, f64::min);
            let end = steps
                .iter()
                .map(|step| step.start_seconds + step.seconds)
                .fold(0.0, f64::max);
            let span = end - start;
            let transferred: u64 = steps.iter().filter_map(|step| step.bytes).sum();
            let mut row = format!("  {:<16} {:>7.1}s", stage.label(), span);
            if transferred > 0 && span > 0.0 {
                row.push_str(&format!(
                    "  {} at {}/s",
                    bytes(transferred),
                    bytes((transferred as f64 / span) as u64)
                ));
            }
            text.push_str(&row);
            text.push('\n');
        }
        text
    }

    /// Every step on its own line, for `--timings`.
    pub fn table(&self) -> String {
        let mut text = String::new();
        for step in &self.steps {
            text.push_str(&format!(
                "  {:>7.1}s +{:>6.1}s  {:<28} {:?}{}\n",
                step.seconds,
                step.start_seconds,
                step.label,
                step.outcome,
                step.note
                    .as_deref()
                    .map(|note| format!(" ({note})"))
                    .unwrap_or_default()
            ));
        }
        text
    }
}

/// Steps for other modules' tests, without a renderer.
#[cfg(test)]
pub(super) mod tests_support {
    use super::*;

    /// A step that isn't drawn anywhere.
    pub fn detached_step() -> Step {
        Step(Arc::new(StepState {
            label: "test".into(),
            stage: Stage::Download,
            started: Instant::now(),
            transfer: Transfer::default(),
            note: OnceLock::new(),
            finished: OnceLock::new(),
        }))
    }

    /// Bytes on disk according to the step.
    pub fn bytes(step: &Step) -> u64 {
        step.0.transfer.bytes.load(Ordering::Relaxed)
    }

    /// The latest retry number the step showed.
    pub fn retries(step: &Step) -> u32 {
        step.0.transfer.retries.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A writer the test can read after the renderer thread returns it.
    #[derive(Clone, Default)]
    struct Shared(Arc<std::sync::Mutex<Vec<u8>>>);

    impl Write for Shared {
        fn write(&mut self, buffer: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buffer);
            Ok(buffer.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl Shared {
        fn text(&self) -> String {
            String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
        }
    }

    fn step(label: &str, stage: Stage, started: Instant) -> Step {
        Step(Arc::new(StepState {
            label: label.into(),
            stage,
            started,
            transfer: Transfer::default(),
            note: OnceLock::new(),
            finished: OnceLock::new(),
        }))
    }

    #[test]
    fn download_line_shows_bytes_total_and_speed() {
        let start = Instant::now();
        let download = step("download guest image", Stage::Download, start);
        download.begin_transfer(0, Some(600 * 1024 * 1024));
        download.add_bytes(300 * 1024 * 1024);
        let text = line(&download, start + Duration::from_secs(10), 200);
        assert!(text.starts_with("[ .. ] download guest image"), "{text}");
        assert!(text.contains("300.0 MiB / 600.0 MiB"), "{text}");
        assert!(text.contains("30.0 MiB/s"), "{text}");
        assert!(text.contains("10.0s"), "{text}");
    }

    #[test]
    fn resumed_bytes_do_not_inflate_the_speed() {
        let start = Instant::now();
        let download = step("download guest image", Stage::Download, start);
        download.begin_transfer(500 * 1024 * 1024, Some(600 * 1024 * 1024));
        download.add_bytes(10 * 1024 * 1024);
        let text = line(&download, start + Duration::from_secs(10), 200);
        assert!(text.contains("510.0 MiB / 600.0 MiB"), "{text}");
        assert!(text.contains("  1.0 MiB/s"), "{text}");
        assert!(text.contains("resumed at 500.0 MiB"), "{text}");
    }

    #[test]
    fn a_retried_download_shows_where_it_resumed_and_which_retry_it_is() {
        let start = Instant::now();
        let download = step("download guest image", Stage::Download, start);
        download.begin_transfer(0, Some(600 * 1024 * 1024));
        download.add_bytes(100 * 1024 * 1024);
        download.retry(2, 100 * 1024 * 1024, Some(600 * 1024 * 1024));
        download.add_bytes(20 * 1024 * 1024);
        let text = line(&download, start + Duration::from_secs(10), 200);
        assert!(text.contains("120.0 MiB / 600.0 MiB"), "{text}");
        assert!(text.contains("  12.0 MiB/s"), "{text}");
        assert!(text.contains("resumed at 100.0 MiB (retry 2)"), "{text}");
        download.retry(3, 0, None);
        download.add_bytes(10 * 1024 * 1024);
        let text = line(&download, start + Duration::from_secs(10), 200);
        assert!(text.contains("10.0 MiB / 600.0 MiB"), "{text}");
        assert!(text.contains("  13.0 MiB/s"), "{text}");
        assert!(text.contains("restarted (retry 3)"), "{text}");
        let timings = Timings::from_steps(start, start, &[download]);
        assert_eq!(timings.steps[0].bytes, Some(130 * 1024 * 1024));
    }

    #[test]
    fn finished_lines_freeze_their_elapsed_time_and_keep_notes() {
        let start = Instant::now();
        let cached = step("install Lima 2.1.0", Stage::Download, start);
        cached.done_with("cached");
        let text = line(&cached, start + Duration::from_secs(60), 200);
        assert!(text.starts_with("[ ok ]"), "{text}");
        assert!(text.contains("cached"), "{text}");
        assert!(!text.contains("60.0s"), "{text}");
        let failed = step("boot VM 1", Stage::Boot, start);
        failed.fail();
        failed.done();
        assert!(line(&failed, start, 200).starts_with("[FAIL]"));
    }

    #[test]
    fn long_lines_are_cut_to_the_terminal_width() {
        let start = Instant::now();
        let long = step(&"x".repeat(100), Stage::Boot, start);
        assert_eq!(line(&long, start, 40).chars().count(), 39);
    }

    #[tokio::test]
    async fn plain_output_prints_each_start_and_finish_once() {
        let output = Shared::default();
        let progress = Progress::new(output.clone(), false);
        let first = progress.step(Stage::Host, "check host");
        first.done();
        progress.note("development binaries selected");
        let _unfinished = progress.step(Stage::Boot, "boot VM 1");
        let timings = progress.finish().await;
        let text = output.text();
        assert_eq!(text.matches("check host").count(), 2, "{text}");
        assert!(text.contains("... check host"), "{text}");
        assert!(text.contains("[ ok ] check host"), "{text}");
        assert!(text.contains("development binaries selected"), "{text}");
        assert!(text.contains("[stop] boot VM 1"), "{text}");
        assert!(!text.contains('\x1b'), "{text}");
        assert_eq!(timings.steps.len(), 2);
        assert_eq!(timings.steps[1].outcome, Outcome::Interrupted);
    }

    #[tokio::test]
    async fn terminal_output_redraws_in_place() {
        let output = Shared::default();
        let progress = Progress::new(output.clone(), true);
        let boot = progress.step(Stage::Boot, "boot VM 1");
        std::thread::sleep(TICK * 3);
        boot.done();
        progress.finish().await;
        let text = output.text();
        // Later frames move the cursor back up over the earlier ones.
        assert!(text.contains("\x1b[1A"), "{text:?}");
        assert!(text.contains("[ ok ] boot VM 1"), "{text:?}");
    }

    #[test]
    fn summary_counts_overlapping_steps_once_per_stage() {
        let timing = |label: &str, stage, start_seconds, seconds, bytes| StepTiming {
            label: label.into(),
            stage,
            start_seconds,
            seconds,
            outcome: Outcome::Done,
            bytes,
            note: None,
        };
        let timings = Timings {
            total_seconds: 100.0,
            steps: vec![
                timing(
                    "download image",
                    Stage::Download,
                    0.0,
                    20.0,
                    Some(200 * 1024 * 1024),
                ),
                timing("download bun", Stage::Download, 0.0, 5.0, Some(0)),
                timing("boot VM 1", Stage::Boot, 20.0, 50.0, None),
                timing("boot VM 2", Stage::Boot, 22.0, 52.0, None),
                timing("boot VM 3", Stage::Boot, 22.0, 49.0, None),
            ],
        };
        let summary = timings.summary();
        assert!(summary.contains("100.0s in total"), "{summary}");
        assert!(
            summary.contains("downloads           20.0s  200.0 MiB at 10.0 MiB/s"),
            "{summary}"
        );
        assert!(summary.contains("VM boot             54.0s"), "{summary}");
        assert!(!summary.contains("node setup"), "{summary}");
        let json = serde_json::to_value(&timings).unwrap();
        assert_eq!(json["steps"][2]["stage"], "boot");
        assert!(json["steps"][2].get("bytes").is_none());
    }
}
