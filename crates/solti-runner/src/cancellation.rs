//! # Runner build cancellation
//!
//! [`BuildCancellation`] is the read-only signal observed by a runner.
//! [`BuildCancellationHandle`] owns the corresponding cancellation capability.

use std::fmt;

use tokio_util::sync::CancellationToken;

/// Read-only cancellation signal for one [`Runner`](crate::Runner) build.
///
/// A runner should pass clones to child futures that participate in the build.
/// Build work must remain owned by the returned future. Dropping that future
/// must not leave background work running.
///
/// The caller that owns the build creates a signal and its matching
/// [`BuildCancellationHandle`] with [`Self::pair`]. A runner receives only this
/// signal and therefore cannot cancel its own build.
///
/// ```compile_fail
/// use solti_runner::BuildCancellation;
///
/// fn cancel_from_runner(signal: &BuildCancellation) {
///     signal.cancel();
/// }
/// ```
#[derive(Clone, Default)]
#[must_use]
pub struct BuildCancellation {
    token: CancellationToken,
}

impl BuildCancellation {
    /// Creates an independent signal without an owner handle.
    ///
    /// Use [`Self::pair`] when another task must be able to request
    /// cancellation.
    pub fn new() -> Self {
        Self {
            token: CancellationToken::new(),
        }
    }

    /// Creates an owner handle and its matching read-only signal.
    pub fn pair() -> (BuildCancellationHandle, Self) {
        let signal = Self::new();
        let handle = BuildCancellationHandle {
            token: signal.token.clone(),
        };
        (handle, signal)
    }

    /// Returns whether cancellation was requested.
    pub fn is_cancelled(&self) -> bool {
        self.token.is_cancelled()
    }

    /// Waits until cancellation is requested.
    pub async fn cancelled(&self) {
        self.token.cancelled().await;
    }
}

impl fmt::Debug for BuildCancellation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BuildCancellation")
            .field("is_cancelled", &self.is_cancelled())
            .finish()
    }
}

/// Owner handle for one [`BuildCancellation`] signal.
///
/// Core retains this handle while a reconciliation is active. Custom callers
/// can retain it while passing the matching signal to
/// [`RunnerRouter::build_with_cancellation`](crate::RunnerRouter::build_with_cancellation)
/// or
/// [`RunnerCatalog::build_with_cancellation`](crate::RunnerCatalog::build_with_cancellation).
#[derive(Clone)]
#[must_use]
pub struct BuildCancellationHandle {
    token: CancellationToken,
}

impl BuildCancellationHandle {
    /// Requests cancellation.
    ///
    /// The operation is idempotent and wakes every waiter.
    pub fn cancel(&self) {
        self.token.cancel();
    }

    /// Returns whether cancellation was requested.
    pub fn is_cancelled(&self) -> bool {
        self.token.is_cancelled()
    }
}

impl fmt::Debug for BuildCancellationHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("BuildCancellationHandle")
            .field("is_cancelled", &self.is_cancelled())
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn clones_observe_one_idempotent_signal() {
        let (handle, cancellation) = BuildCancellation::pair();
        let waiter = cancellation.clone();

        handle.cancel();
        handle.cancel();

        waiter.cancelled().await;
        assert!(waiter.is_cancelled());
        assert!(handle.is_cancelled());
    }

    #[test]
    fn independent_signal_has_no_cancellation_owner() {
        let cancellation = BuildCancellation::new();

        assert!(!cancellation.is_cancelled());
    }
}
