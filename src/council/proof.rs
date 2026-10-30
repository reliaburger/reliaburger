//! One leadership proof at a time, shared by every request waiting for one
//! (#1441).
//!
//! A leadership proof (openraft's `ensure_linearizable`) sends a heartbeat
//! to every follower and waits for a quorum to answer. Job, task-array and
//! upgrade handlers each ask for one before they serve a request, and under
//! load every request in flight used to run its own, in parallel, every
//! heartbeat interval for up to 3 s. That multiplied heartbeat traffic just
//! when the followers were already slow to answer it (#1253).
//!
//! [`SharedProof`] runs at most one proof at a time. A caller that arrives
//! while one is running doesn't take that proof's answer: its heartbeats may
//! have left before the caller's request arrived, so they say nothing about
//! leadership at the moment the caller asks. It waits for the next proof
//! instead, and every caller that arrived during the same round shares that
//! one. Each caller waits for at most the end of one round plus one more.

use std::future::Future;

use tokio::sync::watch;

/// Where the shared proof stands.
#[derive(Debug, Clone, Copy, Default)]
struct ProofState {
    /// How many proofs have started. The latest one's generation.
    started: u64,
    /// Whether a proof is running now.
    running: bool,
    /// The latest finished proof: its generation and whether it proved this
    /// node leads.
    finished: Option<(u64, bool)>,
}

/// Runs one leadership proof at a time and shares its answer with every
/// caller that was waiting before it started.
#[derive(Debug)]
pub struct SharedProof {
    state: watch::Sender<ProofState>,
}

impl Default for SharedProof {
    fn default() -> Self {
        Self::new()
    }
}

impl SharedProof {
    /// No proof run yet.
    pub fn new() -> Self {
        Self {
            state: watch::Sender::new(ProofState::default()),
        }
    }

    /// Whether this node leads, according to the first proof that starts
    /// after this call. When no proof is running, this caller runs `prove`
    /// itself; otherwise it waits for the running one to end and shares the
    /// next. Dropping a caller mid-proof is safe: the next waiter runs one.
    pub async fn run<F, Fut>(&self, prove: F) -> bool
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = bool>,
    {
        let mut changes = self.state.subscribe();
        let wanted = changes.borrow_and_update().started + 1;
        let mut prove = Some(prove);
        loop {
            let mut answer = None;
            let mut generation = None;
            // `send_if_modified` holds the write lock for the closure, so
            // checking for an answer and claiming the next proof is atomic.
            self.state.send_if_modified(|state| {
                if let Some((finished, proved)) = state.finished
                    && finished >= wanted
                {
                    answer = Some(proved);
                    return false;
                }
                if state.running {
                    return false;
                }
                state.running = true;
                state.started += 1;
                generation = Some(state.started);
                true
            });
            if let Some(proved) = answer {
                return proved;
            }
            if let Some(generation) = generation
                && let Some(prove) = prove.take()
            {
                let mut round = Round {
                    state: &self.state,
                    generation,
                    proved: None,
                };
                let proved = prove().await;
                round.proved = Some(proved);
                return proved;
            }
            // The sender lives in `self`, so it can't close while we wait.
            if changes.changed().await.is_err() {
                return false;
            }
        }
    }
}

/// A running proof. Dropping it ends the round, so a caller cancelled
/// mid-proof (a request's deadline, say) still lets the waiters go on.
struct Round<'a> {
    state: &'a watch::Sender<ProofState>,
    generation: u64,
    proved: Option<bool>,
}

impl Drop for Round<'_> {
    fn drop(&mut self) {
        let generation = self.generation;
        let proved = self.proved;
        self.state.send_modify(|state| {
            state.running = false;
            if let Some(proved) = proved {
                state.finished = Some((generation, proved));
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Duration;

    use tokio::sync::oneshot;

    use super::*;

    #[tokio::test]
    async fn callers_waiting_on_a_running_proof_share_the_next_one() {
        let shared = Arc::new(SharedProof::new());
        let proofs = Arc::new(AtomicU64::new(0));
        let (release_tx, release_rx) = oneshot::channel::<()>();

        // The first caller starts a proof and holds it open.
        let first = {
            let shared = Arc::clone(&shared);
            let proofs = Arc::clone(&proofs);
            tokio::spawn(async move {
                shared
                    .run(|| async move {
                        proofs.fetch_add(1, Ordering::SeqCst);
                        let _ = release_rx.await;
                        true
                    })
                    .await
            })
        };
        while proofs.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
        // Twenty more arrive while it runs.
        let waiters: Vec<_> = (0..20)
            .map(|_| {
                let shared = Arc::clone(&shared);
                let proofs = Arc::clone(&proofs);
                tokio::spawn(async move {
                    shared
                        .run(|| async move {
                            proofs.fetch_add(1, Ordering::SeqCst);
                            tokio::time::sleep(Duration::from_millis(20)).await;
                            false
                        })
                        .await
                })
            })
            .collect();
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert_eq!(proofs.load(Ordering::SeqCst), 1, "only one proof at a time");
        release_tx.send(()).unwrap();

        assert!(first.await.unwrap());
        for waiter in waiters {
            // They take the second proof's answer, not the first's.
            assert!(!waiter.await.unwrap());
        }
        assert_eq!(proofs.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn a_proof_that_finished_before_a_call_does_not_answer_it() {
        let shared = SharedProof::new();
        assert!(shared.run(|| async { true }).await);
        assert!(!shared.run(|| async { false }).await);
    }

    #[tokio::test]
    async fn a_cancelled_proof_lets_the_waiters_run_their_own() {
        let shared = Arc::new(SharedProof::new());
        let started = Arc::new(AtomicU64::new(0));
        let stuck = {
            let shared = Arc::clone(&shared);
            let started = Arc::clone(&started);
            tokio::spawn(async move {
                shared
                    .run(|| async move {
                        started.fetch_add(1, Ordering::SeqCst);
                        std::future::pending::<bool>().await
                    })
                    .await
            })
        };
        while started.load(Ordering::SeqCst) == 0 {
            tokio::task::yield_now().await;
        }
        let waiter = {
            let shared = Arc::clone(&shared);
            tokio::spawn(async move { shared.run(|| async { true }).await })
        };
        tokio::time::sleep(Duration::from_millis(20)).await;
        stuck.abort();
        let answer = tokio::time::timeout(Duration::from_secs(5), waiter)
            .await
            .expect("the waiter never ran a proof after the first was cancelled")
            .unwrap();
        assert!(answer);
    }
}
