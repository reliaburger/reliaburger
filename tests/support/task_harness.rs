use std::time::Duration;

use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

/// Owns every task started by an integration-test harness.
///
/// Tests using this guard run on Tokio's multi-thread runtime. That lets the
/// destructor wait while the other worker drives graceful shutdown, including
/// ProcessGrill child termination. The bounded abort is a last resort for a
/// broken shutdown path, not the normal cleanup mechanism.
pub struct TestTasks {
    shutdown: CancellationToken,
    tasks: Vec<JoinHandle<()>>,
}

impl TestTasks {
    pub fn new(shutdown: CancellationToken, tasks: Vec<JoinHandle<()>>) -> Self {
        Self { shutdown, tasks }
    }
}

impl Drop for TestTasks {
    fn drop(&mut self) {
        self.shutdown.cancel();

        for mut task in self.tasks.drain(..) {
            let completed = tokio::task::block_in_place(|| {
                tokio::runtime::Handle::current().block_on(async {
                    // An agent task stops its workloads before it ends, for
                    // up to `shutdown_completion_bound()`.
                    tokio::time::timeout(
                        reliaburger::bun::agent::shutdown_completion_bound()
                            + Duration::from_secs(5),
                        &mut task,
                    )
                    .await
                    .is_ok()
                })
            });
            if !completed {
                task.abort();
            }
        }
    }
}
