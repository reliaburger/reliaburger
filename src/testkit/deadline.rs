//! One absolute deadline inherited by every operation in a test case.

use std::future::Future;
use std::time::Duration;

use tokio::time::Instant;

/// A non-zero absolute deadline on Tokio's monotonic clock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Deadline {
    expires_at: Instant,
    budget_ms: u64,
}

/// Failure to construct or enforce a [`Deadline`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DeadlineError {
    #[error("deadline budget must be greater than zero")]
    ZeroBudget,
    #[error("deadline has already elapsed")]
    Expired,
    #[error("{operation} exceeded its {budget_ms} ms deadline")]
    Exceeded { operation: String, budget_ms: u64 },
}

impl Deadline {
    /// Start a deadline with a non-zero budget.
    pub fn after(budget: Duration) -> Result<Self, DeadlineError> {
        if budget.is_zero() {
            return Err(DeadlineError::ZeroBudget);
        }
        Ok(Self {
            expires_at: Instant::now() + budget,
            budget_ms: budget.as_millis().try_into().unwrap_or(u64::MAX),
        })
    }

    /// Derive a child which can never outlive its parent.
    pub fn child(self, maximum: Duration) -> Result<Self, DeadlineError> {
        if maximum.is_zero() {
            return Err(DeadlineError::ZeroBudget);
        }
        let now = Instant::now();
        let remaining = self.expires_at.saturating_duration_since(now);
        if remaining.is_zero() {
            return Err(DeadlineError::Expired);
        }
        let expires_at = self.expires_at.min(now + maximum);
        Ok(Self {
            expires_at,
            budget_ms: expires_at
                .saturating_duration_since(now)
                .as_millis()
                .try_into()
                .unwrap_or(u64::MAX),
        })
    }

    /// Absolute Tokio instant used by polling and network calls.
    pub fn expires_at(self) -> Instant {
        self.expires_at
    }

    /// Original budget used in timeout evidence.
    pub fn budget_ms(self) -> u64 {
        self.budget_ms
    }

    /// Remaining monotonic budget.
    pub fn remaining(self) -> Duration {
        self.expires_at.saturating_duration_since(Instant::now())
    }

    /// Run a future without allowing it to exceed this deadline.
    pub async fn run<T, F>(&self, operation: &str, future: F) -> Result<T, DeadlineError>
    where
        F: Future<Output = T>,
    {
        tokio::time::timeout_at(self.expires_at, future)
            .await
            .map_err(|_| DeadlineError::Exceeded {
                operation: operation.to_string(),
                budget_ms: self.budget_ms,
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_budget_is_rejected() {
        assert_eq!(
            Deadline::after(Duration::ZERO),
            Err(DeadlineError::ZeroBudget)
        );
    }

    #[tokio::test(start_paused = true)]
    async fn child_never_outlives_parent() {
        let parent = Deadline::after(Duration::from_secs(5)).unwrap();
        let child = parent.child(Duration::from_secs(30)).unwrap();
        assert_eq!(child.expires_at(), parent.expires_at());
        assert!(matches!(
            child.run("probe", std::future::pending::<()>()).await,
            Err(DeadlineError::Exceeded { .. })
        ));
    }
}
