//! A cooperative per-request deadline.
//!
//! The REST handlers are blocking: once one is inside `block_in_place` nothing
//! outside can cancel it. Instead the expensive loops — the body fetches behind
//! a script history or a spender scan, and the per-transaction loops in the
//! handlers — check a deadline between iterations and give up when it passes.

use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Deadline {
    expires_at: Option<Instant>,
}

impl Deadline {
    pub fn after(timeout: Duration) -> Self {
        Self {
            expires_at: (!timeout.is_zero()).then(|| Instant::now() + timeout),
        }
    }

    /// No deadline: used by callers that are not serving a request.
    pub fn never() -> Self {
        Self { expires_at: None }
    }

    pub fn expired(&self) -> bool {
        self.expires_at.is_some_and(|at| Instant::now() >= at)
    }

    pub fn remaining(&self) -> Option<Duration> {
        self.expires_at
            .map(|at| at.saturating_duration_since(Instant::now()))
    }
}

impl Default for Deadline {
    fn default() -> Self {
        Self::never()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_zero_timeout_means_no_deadline() {
        assert!(!Deadline::after(Duration::ZERO).expired());
        assert!(!Deadline::never().expired());
        assert_eq!(Deadline::never().remaining(), None);
    }

    #[test]
    fn an_elapsed_deadline_reports_expired() {
        let deadline = Deadline::after(Duration::from_nanos(1));
        std::thread::sleep(Duration::from_millis(2));
        assert!(deadline.expired());
        assert_eq!(deadline.remaining(), Some(Duration::ZERO));
    }
}
