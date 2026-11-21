//! How long an `exec` may run, and how its deadlines nest (#2128).
//!
//! Four layers bound one exec, and each must give up later than the one
//! inside it, so the innermost layer that can say *why* answers first:
//!
//! 1. The agent (this module) runs the exec under the caller's timeout,
//!    capped at [`MAX_EXEC_TIMEOUT`]. When it fires, the API answers `504`
//!    with an error naming the app and the timeout.
//! 2. The owner-socket wait in `grill::process_control` sits in the same
//!    future, so it's only a backstop for callers that don't bound it
//!    themselves; it's set just above the agent's cap.
//! 3. The exec owner's own deadline in `grill::process_owner::exec` retires
//!    the command if Bun stays connected but stops asking; it's above both.
//!    Dropping the agent's future closes the socket first, which cancels the
//!    command long before this deadline.
//! 4. The `relish` client waits the requested timeout plus
//!    [`CLIENT_EXEC_MARGIN`], so it hears the agent's `504` instead of
//!    timing out on its own with nothing to say.

use std::future::Future;
use std::time::Duration;

use super::BunError;

/// The longest an exec may run, and the timeout of one that asks for none.
///
/// `relish exec app -- some-long-command` is a person waiting at a terminal
/// for a one-shot answer, so five minutes keeps the limit it always had.
/// The test kit asks for far less (#2128).
pub const MAX_EXEC_TIMEOUT: Duration = Duration::from_secs(300);

/// How much longer the client waits than the timeout it asked the agent for.
///
/// The agent's answer has to cross the API and the network after its
/// deadline fires; 30 s leaves room for a loaded node.
pub const CLIENT_EXEC_MARGIN: Duration = Duration::from_secs(30);

/// The timeout an exec runs under, from the request's `timeout_seconds`.
///
/// No request means [`MAX_EXEC_TIMEOUT`]; a request is held between one
/// second and that cap.
pub fn exec_timeout(requested_seconds: Option<u64>) -> Duration {
    match requested_seconds {
        None => MAX_EXEC_TIMEOUT,
        Some(seconds) => Duration::from_secs(seconds.max(1)).min(MAX_EXEC_TIMEOUT),
    }
}

/// How long the client waits for the answer to an exec that asked for
/// `requested` (or for none).
pub fn client_exec_wait(requested: Option<Duration>) -> Duration {
    exec_timeout(requested.map(|timeout| timeout.as_secs())) + CLIENT_EXEC_MARGIN
}

/// Run `exec` for `namespace/app` under `timeout`, answering
/// [`BunError::ExecTimeout`] when it doesn't finish in time. Dropping the
/// unfinished future closes an owner socket, which cancels the command.
pub(super) async fn run_exec_within<F>(
    exec: F,
    app_name: &str,
    namespace: &str,
    timeout: Duration,
) -> Result<crate::grill::ExecOutput, BunError>
where
    F: Future<Output = Result<crate::grill::ExecOutput, crate::grill::GrillError>>,
{
    match tokio::time::timeout(timeout, exec).await {
        Ok(result) => result.map_err(BunError::from),
        Err(_) => Err(BunError::ExecTimeout {
            app_name: app_name.to_string(),
            namespace: namespace.to_string(),
            seconds: timeout.as_secs(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exec_timeout_defaults_to_and_is_capped_at_the_maximum() {
        assert_eq!(exec_timeout(None), MAX_EXEC_TIMEOUT);
        assert_eq!(exec_timeout(Some(60)), Duration::from_secs(60));
        assert_eq!(exec_timeout(Some(0)), Duration::from_secs(1));
        assert_eq!(exec_timeout(Some(u64::MAX)), MAX_EXEC_TIMEOUT);
    }

    /// Each layer gives up after the one inside it, so the agent's named
    /// timeout is what the caller hears.
    #[test]
    fn exec_deadlines_nest_inside_the_clients_wait() {
        let socket = crate::grill::OWNER_SOCKET_EXEC_TIMEOUT;
        let owner = crate::grill::process_owner::exec::OWNER_EXEC_DEADLINE;
        assert!(
            MAX_EXEC_TIMEOUT < socket,
            "agent cap must fire before the socket backstop"
        );
        assert!(
            socket < owner,
            "socket backstop must fire before the owner retires"
        );
        assert!(
            owner < client_exec_wait(None),
            "client must outwait every server layer"
        );
        for requested in [1, 60, 299, 300, 10_000] {
            let requested = Duration::from_secs(requested);
            assert!(
                exec_timeout(Some(requested.as_secs())) < client_exec_wait(Some(requested)),
                "{requested:?}"
            );
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_hanging_exec_fails_with_a_named_timeout() {
        let started = tokio::time::Instant::now();
        let result = run_exec_within(
            std::future::pending(),
            "registry-web",
            "rbtest-x",
            Duration::from_secs(60),
        )
        .await;
        assert_eq!(started.elapsed(), Duration::from_secs(60));
        let error = result.unwrap_err();
        assert!(
            matches!(error, BunError::ExecTimeout { seconds: 60, .. }),
            "{error:?}"
        );
        let message = error.to_string();
        assert!(message.contains("rbtest-x/registry-web"), "{message}");
        assert!(message.contains("timed out after 60s"), "{message}");
    }
}
