/// Drain-tracker cost on the ingress request path (#2455).
///
/// Every proxied request captures its backend's drain count on the
/// process-wide `SharedDrains` mutex and releases it from a spawned task
/// when it ends, whether or not anything is draining. This measures that
/// pair with 8 tasks on an 8-worker runtime, all on backends that aren't
/// draining, against the same loop on bare atomics.
///
/// - `capture_and_release/mutex`: one `increment_connections` and one
///   `decrement_connections` per request, the shape since #2469 (the
///   proxy captures only the backend it tries).
/// - `capture_and_release/mutex_spawned_release`: the same, with the
///   release on a spawned task as `DrainGuard::drop` does it.
/// - `capture_and_release/atomics`: one `fetch_add` and one `fetch_sub`.
///
/// The issue's threshold: contention matters if the mutex pair caps below
/// about a million requests a second on 8 threads.
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use reliaburger::wrapper::draining::{DrainTracker, SharedDrains};

const TASKS: u64 = 8;

/// The backends each task's requests go to: three, as a small app has.
const BACKENDS: [&str; 3] = ["default__web-0", "default__web-1", "default__web-2"];

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(TASKS as usize)
        .enable_all()
        .build()
        .expect("tokio runtime")
}

/// Run `iters` requests split over [`TASKS`] tasks and return how long they took.
fn run<F, Fut>(runtime: &tokio::runtime::Runtime, iters: u64, request: F) -> Duration
where
    F: Fn(u64) -> Fut + Clone + Send + 'static,
    Fut: std::future::Future<Output = ()> + Send,
{
    runtime.block_on(async move {
        let per_task = iters.div_ceil(TASKS);
        let started = Instant::now();
        let mut tasks = Vec::new();
        for task in 0..TASKS {
            let request = request.clone();
            tasks.push(tokio::spawn(async move {
                for i in 0..per_task {
                    request(task + i).await;
                }
            }));
        }
        for task in tasks {
            task.await.expect("bench task");
        }
        started.elapsed()
    })
}

fn capture_and_release(c: &mut Criterion) {
    let runtime = runtime();
    let mut group = c.benchmark_group("capture_and_release");
    group.throughput(Throughput::Elements(1));

    group.bench_function("mutex", |b| {
        b.iter_custom(|iters| {
            let drains = SharedDrains::new(DrainTracker::new(tokio::sync::mpsc::channel(8).0));
            run(&runtime, iters, move |n| {
                let drains = drains.clone();
                async move {
                    let id = BACKENDS[(n % 3) as usize];
                    drains.increment_connections(id).await;
                    drains.decrement_connections(id).await;
                }
            })
        })
    });

    group.bench_function("mutex_spawned_release", |b| {
        b.iter_custom(|iters| {
            let drains = SharedDrains::new(DrainTracker::new(tokio::sync::mpsc::channel(8).0));
            run(&runtime, iters, move |n| {
                let drains = drains.clone();
                async move {
                    let id = BACKENDS[(n % 3) as usize];
                    drains.increment_connections(id).await;
                    let release = tokio::spawn(async move {
                        drains.decrement_connections(id).await;
                    });
                    // Awaited so the measured time includes the release.
                    let _ = release.await;
                }
            })
        })
    });

    group.bench_function("atomics", |b| {
        b.iter_custom(|iters| {
            let counters: Arc<[AtomicU64; 3]> = Arc::new(Default::default());
            run(&runtime, iters, move |n| {
                let counters = Arc::clone(&counters);
                async move {
                    let counter = &counters[(n % 3) as usize];
                    counter.fetch_add(1, Ordering::Relaxed);
                    counter.fetch_sub(1, Ordering::Relaxed);
                }
            })
        })
    });

    group.finish();
}

criterion_group!(benches, capture_and_release);
criterion_main!(benches);
