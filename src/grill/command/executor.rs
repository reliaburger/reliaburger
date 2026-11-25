//! Command execution shared by direct adapters and claimed OCI generations.

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use futures_util::future::BoxFuture;
use tokio::sync::{Mutex, OwnedMutexGuard};

use super::{COMMAND_TIMEOUT, CommandOutput, CommandState};
use crate::grill::runc_intent::{IntentCommands, IntentPhase, RuntimeRole};

/// How long a role exec keeps asking a busy owner whether its role runs.
/// Each ask has its own 2 s deadline, so this covers many of them; it stays
/// well under an exec's own 300 s default, and sits between the 15 s that
/// #2297 gives a command's start and the 60 s that #2292 gives a stop.
const EXEC_ADMISSION_TIMEOUT: Duration = Duration::from_secs(30);

/// The first pause before asking a late owner again; it doubles each time.
const LATE_ANSWER_BACKOFF_START: Duration = Duration::from_millis(10);

/// The longest pause between two asks.
const LATE_ANSWER_BACKOFF_LIMIT: Duration = Duration::from_millis(200);

/// How much of a seal's timeout its asking leaves for the drain. Retiring
/// a launcher sends one force-kill through its owner (one 2 s request
/// deadline), then reads the owner's Retired record and makes a durable
/// write; the extra second covers those two on a busy disk.
const DRAIN_RESERVE: Duration = Duration::from_secs(3);

/// Whether a role observation failed only because an owner's answer was
/// lost or late, so that asking again may succeed.
fn transient_owner_error(error: &io::Error) -> bool {
    error
        .get_ref()
        .and_then(|inner| inner.downcast_ref::<super::CommandError>())
        .is_some_and(super::CommandError::is_transient)
}

/// The pause before asking a late owner again: 10 ms, doubling to 200 ms.
struct LateAnswerBackoff(Duration);

impl LateAnswerBackoff {
    fn new() -> Self {
        Self(LATE_ANSWER_BACKOFF_START)
    }

    async fn pause(&mut self) {
        tokio::time::sleep(self.0).await;
        self.0 = (self.0 * 2).min(LATE_ANSWER_BACKOFF_LIMIT);
    }
}

/// Ask through `ask`, and ask again while the owner's answer is lost or
/// late, as exec admission does. Any other error returns at once. It has
/// no bound of its own: the caller runs it under a timeout, as a seal's
/// drain does (#2328). While it is asking again, `late` holds the last late
/// answer, so a caller whose timeout cuts it short can say why; once an
/// ask answers, `late` is cleared. Each ask must be safe to repeat.
pub(crate) async fn ask_while_late<T, F, Fut>(
    late: &mut Option<io::Error>,
    mut ask: F,
) -> io::Result<T>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = io::Result<T>>,
{
    let mut backoff = LateAnswerBackoff::new();
    loop {
        match ask().await {
            Err(error) if transient_owner_error(&error) => {
                *late = Some(error);
                backoff.pause().await;
            }
            answer => {
                *late = None;
                return answer;
            }
        }
    }
}

/// Executes a bounded short runtime command without shell interpolation.
pub trait RuntimeCommandExecutor: Send + Sync {
    /// Return actual exit evidence and captured output, or retain uncertainty as an error.
    fn output(
        &self,
        program: &str,
        arguments: &[&str],
    ) -> impl std::future::Future<Output = io::Result<CommandOutput>> + Send;
}

/// Direct CLI execution for standalone callers without durable runtime ownership.
#[derive(Debug, Clone, Copy)]
pub struct DirectCommandExecutor;

impl RuntimeCommandExecutor for DirectCommandExecutor {
    async fn output(&self, program: &str, arguments: &[&str]) -> io::Result<CommandOutput> {
        let output = tokio::time::timeout(
            COMMAND_TIMEOUT,
            tokio::process::Command::new(program)
                .args(arguments)
                .kill_on_drop(true)
                .output(),
        )
        .await
        .map_err(|_| {
            io::Error::new(
                io::ErrorKind::TimedOut,
                "runtime command exceeded five seconds",
            )
        })??;
        Ok(CommandOutput {
            exit_code: output.status.code(),
            stdout: output.stdout,
            stderr: output.stderr,
        })
    }
}

/// Shared handles to one exclusive generation's short-command collection.
///
/// Each worker retains the mutex and generation claim through completion. Clones
/// created before sealing cannot acquire cleanup authority or reopen admission.
/// An execution error consumes the live claim; recover its durable generation
/// explicitly before retrying cleanup. No error becomes proof of absence.
#[derive(Debug, Clone)]
pub struct ClaimedCommandExecutor {
    commands: Arc<Mutex<Option<IntentCommands>>>,
    cleanup: bool,
}

impl ClaimedCommandExecutor {
    /// Open normal command admission for an already claimed generation.
    pub fn new(commands: IntentCommands) -> Self {
        Self {
            commands: Arc::new(Mutex::new(Some(commands))),
            cleanup: false,
        }
    }

    /// Read original intent and phase while retaining this generation's claim.
    pub async fn intent(&self) -> io::Result<crate::grill::runc_intent::RuntimeIntent> {
        let guard = self.commands.lock().await;
        guard
            .as_ref()
            .ok_or_else(unavailable)?
            .record()
            .cloned()
            .ok_or_else(unavailable)
    }

    /// Start a bound long-lived role while retaining the shared generation claim.
    pub async fn start_role(
        &self,
        role: RuntimeRole,
        program: &Path,
        arguments: &[String],
        environment: &BTreeMap<String, String>,
    ) -> io::Result<()> {
        if self.cleanup {
            return Err(io::Error::other(
                "cleanup handle cannot start runtime roles",
            ));
        }
        let program = program.to_owned();
        let arguments = arguments.to_vec();
        let environment = environment.clone();
        let mut guard = self.commands.clone().lock_owned().await;
        tokio::spawn(async move {
            let current = guard.as_ref().ok_or_else(unavailable)?;
            if !current
                .record()
                .is_some_and(|record| record.phase == IntentPhase::Owned)
            {
                return Err(io::Error::other("runtime role admission is sealed"));
            }
            let current = guard.take().ok_or_else(unavailable)?;
            *guard = Some(
                current
                    .start_role(role, &program, &arguments, &environment)
                    .await?,
            );
            Ok(())
        })
        .await
        .map_err(io::Error::other)?
    }

    /// Observe the exact durable role binding without signalling recovered PIDs.
    pub async fn role_state(&self, role: RuntimeRole) -> io::Result<Option<CommandState>> {
        let guard = self.commands.lock().await;
        guard
            .as_ref()
            .ok_or_else(unavailable)?
            .role_state(role)
            .await
    }

    /// [`role_state`](Self::role_state), asking again while the role's
    /// owner answers late, for up to `bound` (#2328). One ask checks every
    /// role, one owner request each, then reads `role` once more, and each
    /// request has a 2 s deadline, so an ask that starts just before
    /// `bound` runs out can overrun it by that much: 6 s with a rootless
    /// network helper. A zero `bound` asks once. Each ask takes the adapter
    /// mutex afresh. Any other refusal, like an owner that died while its
    /// workload runs, returns at once.
    pub async fn role_state_patiently(
        &self,
        role: RuntimeRole,
        bound: Duration,
    ) -> io::Result<Option<CommandState>> {
        let (guard, state) = self
            .ask_patiently(bound, |commands| Box::pin(commands.role_state(role)))
            .await?;
        drop(guard);
        Ok(state)
    }

    /// Observe the bound network helper after cancelling one that an earlier
    /// process bound but never started; see
    /// [`IntentCommands::settle_network_helper`](crate::grill::runc_intent::IntentCommands::settle_network_helper).
    pub async fn settle_network_helper(&self) -> io::Result<Option<CommandState>> {
        if self.cleanup {
            return Err(io::Error::other(
                "cleanup handle cannot settle runtime roles",
            ));
        }
        let guard = self.commands.lock().await;
        guard
            .as_ref()
            .ok_or_else(unavailable)?
            .settle_network_helper()
            .await
    }

    /// Run auxiliary work without holding the adapter mutex during its lifetime.
    /// Cancellation closes the caller's owner socket; sealing can retire the role
    /// concurrently and waits for its accepted auxiliary children to disappear.
    ///
    /// Admission asks the role's owner whether the role runs, and each ask
    /// has a short deadline. When a busy owner answers late, admission asks
    /// again for up to [`EXEC_ADMISSION_TIMEOUT`], so a loaded host delays
    /// the exec instead of failing it (#2305). Each attempt takes the adapter
    /// mutex afresh: a seal can get in between attempts, and the next
    /// attempt then refuses, because admission is sealed or because a failed
    /// seal left the claim for recovery.
    pub async fn exec_role(
        &self,
        role: RuntimeRole,
        command: &[String],
    ) -> io::Result<crate::grill::ExecOutput> {
        if self.cleanup {
            return Err(io::Error::other(
                "cleanup handle cannot execute runtime roles",
            ));
        }
        let (guard, execution) = self
            .ask_patiently(EXEC_ADMISSION_TIMEOUT, |commands| {
                Box::pin(commands.execution(role))
            })
            .await?;
        drop(guard);
        // Keep the socket future in this caller, so cancellation reaches the owner.
        execution.execute(command).await
    }

    /// Locate original role logs through validated generation ownership.
    pub async fn role_log_stem(&self, role: RuntimeRole) -> io::Result<Option<PathBuf>> {
        let guard = self.commands.lock().await;
        guard
            .as_ref()
            .ok_or_else(unavailable)?
            .role_log_stem(role)
            .await
    }

    /// Fence normal handles and return a cleanup handle after positive command draining.
    ///
    /// The seal first checks every role with its owner, and each ask has a
    /// short deadline. A busy owner that answers late is asked again, as
    /// exec admission does (#2320), until [`DRAIN_RESERVE`] before `timeout`
    /// runs out; the drain gets what's left. Each ask takes the adapter
    /// mutex afresh. If no answer comes in time, or one comes too late to
    /// leave the drain its reserve, the seal returns `TimedOut` with the
    /// claim as it was, so the caller can seal again. Any other refusal,
    /// like an owner that died while its workload runs, is the seal's own:
    /// it fences admission and fails at once, leaving the claim for recovery.
    ///
    /// The asking and the drain stay inside `timeout`. Persisting the fence
    /// before the drain is a durable write that no deadline bounds, so a
    /// slow disk can still push a seal past it.
    pub async fn seal(&self, timeout: Duration) -> io::Result<Self> {
        let started = tokio::time::Instant::now();
        let deadline = started + timeout;
        let asking = timeout.saturating_sub(DRAIN_RESERVE);
        // Dropping this mid-attempt is safe: an attempt's observations are
        // idempotent, and dropping it releases the mutex or its place in
        // the queue for it.
        let answered = tokio::time::timeout_at(
            started + asking,
            self.ask_patiently(asking, |commands| {
                Box::pin(async move {
                    match commands.validate_roles().await {
                        Err(error) if transient_owner_error(&error) => Err(error),
                        // The seal checks the roles again and reports this itself.
                        _ => Ok(()),
                    }
                })
            }),
        )
        .await
        .map_err(|_| seal_unanswered(asking))?;
        let (mut guard, ()) = answered?;
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining < DRAIN_RESERVE {
            return Err(seal_unanswered(asking));
        }
        let commands = self.commands.clone();
        tokio::spawn(async move {
            let current = guard.take().ok_or_else(unavailable)?;
            *guard = Some(current.seal(remaining).await?);
            Ok(Self {
                commands,
                cleanup: true,
            })
        })
        .await
        .map_err(io::Error::other)?
    }

    /// Ask the claim through `ask`, and ask again while the owner's answer
    /// is lost or late, for up to `bound`. Each attempt takes the adapter
    /// mutex and drops it before pausing, so other callers get in between
    /// two asks. Returns the answer with the mutex of the attempt that got
    /// it, still held. Any other error returns at once.
    async fn ask_patiently<T>(
        &self,
        bound: Duration,
        ask: impl Fn(&IntentCommands) -> BoxFuture<'_, io::Result<T>>,
    ) -> io::Result<(OwnedMutexGuard<Option<IntentCommands>>, T)> {
        let started = tokio::time::Instant::now();
        let mut backoff = LateAnswerBackoff::new();
        let mut attempts: u32 = 0;
        loop {
            attempts += 1;
            let guard = self.commands.clone().lock_owned().await;
            let answer = match guard.as_ref() {
                Some(commands) => ask(commands).await,
                None => Err(unavailable()),
            };
            match answer {
                Ok(value) => return Ok((guard, value)),
                Err(error) if transient_owner_error(&error) => {
                    if started.elapsed() >= bound {
                        return Err(io::Error::new(
                            io::ErrorKind::TimedOut,
                            format!(
                                "process owner didn't answer for {bound:?} ({attempts} attempts): {error}"
                            ),
                        ));
                    }
                }
                Err(error) => return Err(error),
            }
            drop(guard);
            backoff.pause().await;
        }
    }

    /// Save a network reference without cancellation releasing the generation claim.
    pub async fn retain_network(&self, container_index: u16) -> io::Result<()> {
        if self.cleanup {
            return Err(io::Error::other(
                "cleanup cannot acquire a network reference",
            ));
        }
        let mut guard = self.commands.clone().lock_owned().await;
        tokio::spawn(async move {
            let current = guard.take().ok_or_else(unavailable)?;
            *guard = Some(current.retain_network(container_index).await?);
            Ok(())
        })
        .await
        .map_err(io::Error::other)?
    }

    /// Keep the claim through a durable, generation-matched release receipt.
    pub async fn release_network(
        &self,
        reference: crate::grill::runc_intent::NetworkReference,
    ) -> io::Result<()> {
        let mut guard = self.commands.clone().lock_owned().await;
        tokio::spawn(async move {
            let current = guard.take().ok_or_else(unavailable)?;
            *guard = Some(current.release_network(reference).await?);
            Ok(())
        })
        .await
        .map_err(io::Error::other)?
    }

    /// Release the generation only after the caller has confirmed resource absence.
    pub async fn finish(&self, exit_code: Option<i32>) -> io::Result<()> {
        if !self.cleanup {
            return Err(io::Error::other(
                "normal runtime handle cannot finish cleanup",
            ));
        }
        let mut guard = self.commands.clone().lock_owned().await;
        tokio::spawn(async move {
            guard
                .take()
                .ok_or_else(unavailable)?
                .finish(exit_code)
                .await
        })
        .await
        .map_err(io::Error::other)?
    }
}

impl RuntimeCommandExecutor for ClaimedCommandExecutor {
    async fn output(&self, program: &str, arguments: &[&str]) -> io::Result<CommandOutput> {
        let program = program.to_owned();
        let arguments: Vec<String> = arguments.iter().map(|value| (*value).to_owned()).collect();
        let cleanup = self.cleanup;
        let mut guard = self.commands.clone().lock_owned().await;
        tokio::spawn(async move {
            let current = guard.as_ref().ok_or_else(unavailable)?;
            let admitted = current.record().is_some_and(|record| {
                if cleanup {
                    record.phase == IntentPhase::Retiring
                } else {
                    record.phase == IntentPhase::Owned
                }
            });
            // A stale clone's refusal must leave the cleanup handle intact.
            if !admitted {
                return Err(io::Error::other("runtime command admission is sealed"));
            }
            let current = guard.take().ok_or_else(unavailable)?;
            let (current, output) = if cleanup {
                current
                    .run_cleanup(
                        Path::new(&program),
                        &arguments,
                        &BTreeMap::new(),
                        COMMAND_TIMEOUT,
                    )
                    .await?
            } else {
                current
                    .run(
                        Path::new(&program),
                        &arguments,
                        &BTreeMap::new(),
                        COMMAND_TIMEOUT,
                    )
                    .await?
            };
            *guard = Some(current);
            Ok(output)
        })
        .await
        .map_err(io::Error::other)?
    }
}

/// A seal that got no answer in time to drain; it leaves the claim intact.
fn seal_unanswered(asking: Duration) -> io::Error {
    io::Error::new(
        io::ErrorKind::TimedOut,
        format!(
            "process owner didn't answer within {asking:?} with time left to drain; the claim is kept, seal again"
        ),
    )
}

fn unavailable() -> io::Error {
    io::Error::other("runtime command claim is unavailable; recover the recorded generation")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grill::command::CommandError;

    fn wrapped(error: CommandError) -> io::Error {
        io::Error::other(error)
    }

    #[test]
    fn a_late_owner_answer_is_transient() {
        let late = io::Error::new(io::ErrorKind::TimedOut, "process owner request timed out");
        assert!(transient_owner_error(&wrapped(CommandError::Io(late))));
    }

    #[test]
    fn a_missing_record_is_not_transient() {
        let missing = io::Error::new(io::ErrorKind::NotFound, "no owner record");
        assert!(!transient_owner_error(&wrapped(CommandError::Io(missing))));
    }

    #[test]
    fn a_command_that_overran_its_wait_is_not_transient() {
        let overran = CommandError::TimedOut {
            command: crate::grill::command::CommandId(crate::grill::InstanceId("command-0".into())),
            timeout: Duration::from_secs(1),
        };
        assert!(!transient_owner_error(&wrapped(overran)));
    }

    fn late() -> io::Error {
        let late = io::Error::new(io::ErrorKind::TimedOut, "process owner request timed out");
        wrapped(CommandError::Io(late))
    }

    // These await the helper directly, with no `tokio::spawn`: a paused
    // clock advances only while the test's own future is idle.
    #[tokio::test(start_paused = true)]
    async fn ask_while_late_asks_again_after_a_late_answer() {
        let started = tokio::time::Instant::now();
        let mut asks = 0;
        let mut last_late = None;
        let answer = ask_while_late(&mut last_late, || {
            asks += 1;
            let ask = asks;
            async move { if ask < 3 { Err(late()) } else { Ok(ask) } }
        })
        .await;
        assert_eq!(answer.unwrap(), 3);
        // The 10 ms pause, then the doubled 20 ms one.
        assert_eq!(started.elapsed(), Duration::from_millis(30));
        assert!(last_late.is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn ask_while_late_keeps_the_last_late_answer_for_a_timeout() {
        let mut last_late = None;
        let cut = tokio::time::timeout(
            Duration::from_secs(1),
            ask_while_late(&mut last_late, || async { Err::<(), _>(late()) }),
        )
        .await;
        assert!(cut.is_err());
        assert!(last_late.unwrap().to_string().contains("timed out"));
    }

    #[tokio::test(start_paused = true)]
    async fn ask_while_late_returns_any_other_error_at_once() {
        let started = tokio::time::Instant::now();
        let mut asks = 0;
        let mut last_late = None;
        let answer: io::Result<()> = ask_while_late(&mut last_late, || {
            asks += 1;
            async {
                Err(io::Error::other(
                    "process owner died while its workload still runs",
                ))
            }
        })
        .await;
        assert!(answer.unwrap_err().to_string().contains("owner died"));
        assert_eq!(asks, 1);
        assert_eq!(started.elapsed(), Duration::ZERO);
        assert!(last_late.is_none());
    }

    #[test]
    fn a_bare_timeout_is_not_an_owner_answer() {
        let bare = io::Error::new(io::ErrorKind::TimedOut, "timed out");
        assert!(!transient_owner_error(&bare));
    }
}
