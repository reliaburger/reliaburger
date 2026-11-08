//! The agent loop's stall watchdog against a real blocked tokio worker
//! (#1707).
//!
//! A task begins a "turn" on the heartbeat, then blocks its worker thread
//! in a synchronous sleep for longer than the stall threshold, as a worker
//! stuck in a filesystem syscall would. The watchdog runs on its own
//! `std::thread`, so it sees the stall while the worker is still stuck, and
//! must write exactly one snapshot naming that thread.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use reliaburger::bun::loop_meter::LoopBranch;
use reliaburger::bun::loop_watchdog::{
    LoopHeartbeat, LoopWatchdog, SNAPSHOT_END, WatchdogSettings,
};

const BLOCKED_WORKER: &str = "stall-worker";

#[test]
fn a_blocked_worker_gets_exactly_one_snapshot_naming_its_thread() {
    let heartbeat = Arc::new(LoopHeartbeat::new());
    let blocks = Arc::new(Mutex::new(Vec::<String>::new()));
    let sink_blocks = Arc::clone(&blocks);
    let watchdog = LoopWatchdog::spawn(
        Arc::clone(&heartbeat),
        WatchdogSettings::with_threshold(Duration::from_millis(750)),
        move |block| sink_blocks.lock().unwrap().push(block.to_string()),
    )
    .unwrap();

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .thread_name(BLOCKED_WORKER)
        .enable_all()
        .build()
        .unwrap();
    let turn_heartbeat = Arc::clone(&heartbeat);
    runtime.block_on(async move {
        tokio::spawn(async move {
            turn_heartbeat.begin(LoopBranch::FollowUp, Some("consumer_sync/journal"));
            // Deliberately blocks the worker, as the bug under hunt does.
            std::thread::sleep(Duration::from_millis(1_500));
            turn_heartbeat.finish();
        })
        .await
        .unwrap();
    });
    // A healthy stretch after the stall: no second snapshot.
    std::thread::sleep(Duration::from_millis(400));
    watchdog.stop();

    let blocks = blocks.lock().unwrap();
    assert_eq!(blocks.len(), 1, "one snapshot per stall: {blocks:#?}");
    let block = &blocks[0];
    eprintln!("{block}");
    let first = block.lines().next().unwrap();
    assert!(
        first.starts_with("bun: agent loop stalled ")
            && first.contains(" ms in follow_up (consumer_sync/journal), ")
            && first.contains("; thread snapshot (pid "),
        "{first}"
    );
    assert!(block.ends_with(SNAPSHOT_END));

    #[cfg(target_os = "linux")]
    {
        let loop_line = block
            .lines()
            .find(|line| line.ends_with("[ran the agent loop]"))
            .unwrap_or_else(|| panic!("no thread marked as the loop's: {block}"));
        assert!(
            loop_line.contains(&format!(" {BLOCKED_WORKER} S ")),
            "{loop_line}"
        );
        assert!(loop_line.contains("nanosleep("), "{loop_line}");
        assert!(
            block.contains(" loop-watchdog R [this watchdog]"),
            "{block}"
        );
    }
}
