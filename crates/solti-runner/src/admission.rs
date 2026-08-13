//! # Runner build admission
//!
//! [`RunnerBuildAdmission`] bounds managed runner construction.
//! [`BuildScope`] carries one admitted build path through composing runners.

use std::{
    collections::{HashMap, VecDeque},
    fmt,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};

use thiserror::Error;
use tokio::sync::{Notify, Semaphore};

use crate::BuildCancellation;

/// Invalid runner-build admission limits.
#[derive(Clone, Copy, Debug, Eq, Error, PartialEq)]
#[non_exhaustive]
pub enum BuildAdmissionConfigError {
    /// A required positive limit was zero.
    #[error("{field} must be greater than zero")]
    Zero {
        /// Stable field name.
        field: &'static str,
    },
    /// A limit exceeded Tokio's semaphore capacity.
    #[error("{field} must not exceed semaphore_max_permits")]
    ExceedsSemaphoreMaximum {
        /// Stable field name.
        field: &'static str,
    },
}

/// Shared admission capability for one managed runner hierarchy.
///
/// Root admission atomically reserves one global slot and one slot for the
/// selected runner. Nested builds reuse the outer global slot and reserve only
/// the selected runner's slot. An unavailable resource never causes a waiter
/// to hold the other resource.
///
/// Fitting nested waiters have progress priority because their outer builds
/// already own global slots. Fitting roots follow. Within each class, the
/// earliest fitting waiter is admitted; an infeasible waiter does not block a
/// later request whose complete resource set is available.
#[derive(Clone)]
pub struct RunnerBuildAdmission {
    inner: Arc<AdmissionInner>,
}

impl RunnerBuildAdmission {
    /// Creates admission with explicit global and per-runner limits.
    ///
    /// # Errors
    ///
    /// Returns [`BuildAdmissionConfigError::Zero`] when either limit is zero.
    /// Returns [`BuildAdmissionConfigError::ExceedsSemaphoreMaximum`] when a
    /// limit exceeds [`Semaphore::MAX_PERMITS`].
    pub fn new(
        global_limit: usize,
        per_runner_limit: usize,
    ) -> Result<Self, BuildAdmissionConfigError> {
        validate_limit("global_limit", global_limit)?;
        validate_limit("per_runner_limit", per_runner_limit)?;
        Ok(Self {
            inner: Arc::new(AdmissionInner {
                global_limit,
                per_runner_limit,
                state: Mutex::new(AdmissionState::default()),
            }),
        })
    }

    pub(crate) async fn enter_root(
        &self,
        runner: &str,
        cancellation: &BuildCancellation,
    ) -> Result<BuildScope, EnterBuildError> {
        let runner: Arc<str> = Arc::from(runner);
        let permit = self
            .inner
            .acquire(AdmissionKind::Root, Arc::clone(&runner), cancellation)
            .await?;
        Ok(BuildScope {
            admission: Some(self.clone()),
            ancestry: vec![runner],
            _permit: Some(permit),
        })
    }
}

impl fmt::Debug for RunnerBuildAdmission {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RunnerBuildAdmission")
            .field("global_limit", &self.inner.global_limit)
            .field("per_runner_limit", &self.inner.per_runner_limit)
            .finish_non_exhaustive()
    }
}

fn validate_limit(field: &'static str, value: usize) -> Result<(), BuildAdmissionConfigError> {
    if value == 0 {
        return Err(BuildAdmissionConfigError::Zero { field });
    }
    if value > Semaphore::MAX_PERMITS {
        return Err(BuildAdmissionConfigError::ExceedsSemaphoreMaximum { field });
    }
    Ok(())
}

#[derive(Default)]
struct AdmissionState {
    global_in_use: usize,
    per_runner_in_use: HashMap<Arc<str>, usize>,
    waiters: VecDeque<Arc<AdmissionWaiter>>,
}

struct AdmissionInner {
    global_limit: usize,
    per_runner_limit: usize,
    state: Mutex<AdmissionState>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum AdmissionKind {
    Root,
    Nested,
}

struct AdmissionWaiter {
    kind: AdmissionKind,
    runner: Arc<str>,
    granted: AtomicBool,
    ready: Notify,
}

impl AdmissionInner {
    async fn acquire(
        self: &Arc<Self>,
        kind: AdmissionKind,
        runner: Arc<str>,
        cancellation: &BuildCancellation,
    ) -> Result<AdmissionPermit, EnterBuildError> {
        if cancellation.is_cancelled() {
            return Err(EnterBuildError::Cancelled);
        }

        let waiter = Arc::new(AdmissionWaiter {
            kind,
            runner,
            granted: AtomicBool::new(false),
            ready: Notify::new(),
        });
        {
            let mut state = self.lock_state();
            state.waiters.push_back(Arc::clone(&waiter));
            self.grant_waiters(&mut state);
        }
        let mut registration = WaiterRegistration {
            admission: Arc::clone(self),
            waiter: Arc::clone(&waiter),
            active: true,
        };

        tokio::select! {
            biased;
            _ = cancellation.cancelled() => Err(EnterBuildError::Cancelled),
            () = wait_until_granted(&waiter) => {
                registration.active = false;
                Ok(AdmissionPermit {
                    admission: Arc::clone(self),
                    kind,
                    runner: Arc::clone(&waiter.runner),
                })
            }
        }
    }

    fn lock_state(&self) -> std::sync::MutexGuard<'_, AdmissionState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    fn grant_waiters(&self, state: &mut AdmissionState) {
        loop {
            let Some(index) = self.next_fitting_waiter(state) else {
                return;
            };
            let waiter = Arc::clone(&state.waiters[index]);

            if waiter.kind == AdmissionKind::Root {
                state.global_in_use += 1;
            }
            *state
                .per_runner_in_use
                .entry(Arc::clone(&waiter.runner))
                .or_default() += 1;
            let granted = state
                .waiters
                .remove(index)
                .expect("the selected runner admission waiter is queued");
            debug_assert!(Arc::ptr_eq(&granted, &waiter));
            let was_granted = waiter.granted.swap(true, Ordering::Release);
            debug_assert!(!was_granted, "a waiter must be granted at most once");
            waiter.ready.notify_one();
        }
    }

    fn next_fitting_waiter(&self, state: &AdmissionState) -> Option<usize> {
        [AdmissionKind::Nested, AdmissionKind::Root]
            .into_iter()
            .find_map(|kind| {
                state
                    .waiters
                    .iter()
                    .position(|waiter| waiter.kind == kind && self.can_grant(state, waiter))
            })
    }

    fn can_grant(&self, state: &AdmissionState, waiter: &AdmissionWaiter) -> bool {
        let runner_in_use = state
            .per_runner_in_use
            .get(&waiter.runner)
            .copied()
            .unwrap_or_default();
        runner_in_use < self.per_runner_limit
            && (waiter.kind == AdmissionKind::Nested || state.global_in_use < self.global_limit)
    }

    fn cancel_waiter(&self, waiter: &AdmissionWaiter) {
        let mut state = self.lock_state();
        if waiter.granted.swap(false, Ordering::AcqRel) {
            self.release_reservation(&mut state, waiter.kind, &waiter.runner);
        } else if let Some(index) = state
            .waiters
            .iter()
            .position(|queued| std::ptr::eq(queued.as_ref(), waiter))
        {
            state.waiters.remove(index);
        }
        self.grant_waiters(&mut state);
    }

    fn release(&self, kind: AdmissionKind, runner: &Arc<str>) {
        let mut state = self.lock_state();
        self.release_reservation(&mut state, kind, runner);
        self.grant_waiters(&mut state);
    }

    fn release_reservation(
        &self,
        state: &mut AdmissionState,
        kind: AdmissionKind,
        runner: &Arc<str>,
    ) {
        if kind == AdmissionKind::Root {
            state.global_in_use = state
                .global_in_use
                .checked_sub(1)
                .expect("root admission must own one global reservation");
        }
        let remove_runner = {
            let in_use = state
                .per_runner_in_use
                .get_mut(runner)
                .expect("admission permit runner must be reserved");
            *in_use = in_use
                .checked_sub(1)
                .expect("runner admission reservation must not underflow");
            *in_use == 0
        };
        if remove_runner {
            state.per_runner_in_use.remove(runner);
        }
    }
}

async fn wait_until_granted(waiter: &AdmissionWaiter) {
    loop {
        let notified = waiter.ready.notified();
        if waiter.granted.load(Ordering::Acquire) {
            return;
        }
        notified.await;
    }
}

struct WaiterRegistration {
    admission: Arc<AdmissionInner>,
    waiter: Arc<AdmissionWaiter>,
    active: bool,
}

impl Drop for WaiterRegistration {
    fn drop(&mut self) {
        if self.active {
            self.admission.cancel_waiter(&self.waiter);
        }
    }
}

struct AdmissionPermit {
    admission: Arc<AdmissionInner>,
    kind: AdmissionKind,
    runner: Arc<str>,
}

impl Drop for AdmissionPermit {
    fn drop(&mut self) {
        self.admission.release(self.kind, &self.runner);
    }
}

/// Opaque admission path passed through one runner build.
///
/// A composing runner passes this value by mutable reference to
/// [`RunnerCatalog::build_scoped_with_cancellation`](crate::RunnerCatalog::build_scoped_with_cancellation).
/// The scope is intentionally not cloneable: one outer build has one ordered
/// nested-build path.
#[must_use]
pub struct BuildScope {
    admission: Option<RunnerBuildAdmission>,
    ancestry: Vec<Arc<str>>,
    _permit: Option<AdmissionPermit>,
}

impl BuildScope {
    /// Creates a scope without admission limits for a direct runner call.
    ///
    /// Prefer [`RunnerRouter::build`](crate::RunnerRouter::build) for direct
    /// unmanaged builds. Core-managed and composing builds receive a scope
    /// from the router and must not replace it.
    pub fn unmanaged(runner: &str) -> Self {
        Self {
            admission: None,
            ancestry: vec![Arc::from(runner)],
            _permit: None,
        }
    }

    pub(crate) async fn enter_child(
        &mut self,
        runner: &str,
        cancellation: &BuildCancellation,
    ) -> Result<Self, EnterBuildError> {
        if self
            .ancestry
            .iter()
            .any(|ancestor| ancestor.as_ref() == runner)
        {
            return Err(EnterBuildError::Recursive);
        }

        let runner: Arc<str> = Arc::from(runner);
        let permit = match &self.admission {
            Some(admission) => Some(
                admission
                    .inner
                    .acquire(AdmissionKind::Nested, Arc::clone(&runner), cancellation)
                    .await?,
            ),
            None => None,
        };
        let mut ancestry = self.ancestry.clone();
        ancestry.push(runner);
        Ok(Self {
            admission: self.admission.clone(),
            ancestry,
            _permit: permit,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum EnterBuildError {
    Cancelled,
    Recursive,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{future::Future, task::Poll};

    #[test]
    fn limits_are_checked_before_coordinator_creation() {
        for (actual, field) in [
            (RunnerBuildAdmission::new(0, 1).unwrap_err(), "global_limit"),
            (
                RunnerBuildAdmission::new(1, 0).unwrap_err(),
                "per_runner_limit",
            ),
        ] {
            assert_eq!(actual, BuildAdmissionConfigError::Zero { field });
        }
        assert_eq!(
            RunnerBuildAdmission::new(usize::MAX, 1).unwrap_err(),
            BuildAdmissionConfigError::ExceedsSemaphoreMaximum {
                field: "global_limit"
            }
        );
        assert_eq!(
            RunnerBuildAdmission::new(1, usize::MAX).unwrap_err(),
            BuildAdmissionConfigError::ExceedsSemaphoreMaximum {
                field: "per_runner_limit"
            }
        );
    }

    #[tokio::test]
    async fn infeasible_root_cannot_block_a_nested_runner_needed_by_global_owner() {
        let admission = RunnerBuildAdmission::new(1, 1).unwrap();
        let cancellation = BuildCancellation::new();
        let mut outer = admission.enter_root("chain", &cancellation).await.unwrap();

        let direct_cancellation = BuildCancellation::new();
        let mut direct = Box::pin(admission.enter_root("leaf", &direct_cancellation));
        assert_pending(&mut direct, "the outer build owns the global slot").await;

        let mut nested = Box::pin(outer.enter_child("leaf", &cancellation));
        let child = poll_ready(
            &mut nested,
            "an infeasible root must not block the nested leaf",
        )
        .await
        .unwrap();
        drop(child);
        drop(nested);
        drop(outer);

        let direct_scope = direct.await.unwrap();
        drop(direct_scope);
    }

    #[tokio::test]
    async fn blocked_runner_roots_do_not_reserve_idle_global_slots() {
        let admission = RunnerBuildAdmission::new(2, 1).unwrap();
        let cancellation = BuildCancellation::new();
        let active_x = admission.enter_root("x", &cancellation).await.unwrap();

        let pending_cancellation = BuildCancellation::new();
        let mut pending_x = Box::pin(admission.enter_root("x", &pending_cancellation));
        assert_pending(&mut pending_x, "runner x is at its limit").await;

        let active_y = admission.enter_root("y", &cancellation).await.unwrap();
        drop(active_y);
        drop(active_x);
        let next_x = pending_x.await.unwrap();
        drop(next_x);
    }

    #[tokio::test]
    async fn cancelling_a_nested_waiter_releases_its_queue_position() {
        let admission = RunnerBuildAdmission::new(2, 1).unwrap();
        let cancellation = BuildCancellation::new();
        let mut first_outer = admission
            .enter_root("outer-a", &cancellation)
            .await
            .unwrap();
        let first_leaf = first_outer
            .enter_child("leaf", &cancellation)
            .await
            .unwrap();
        let mut second_outer = admission
            .enter_root("outer-b", &cancellation)
            .await
            .unwrap();

        let (cancel_handle, waiter_cancellation) = BuildCancellation::pair();
        let mut waiter = Box::pin(second_outer.enter_child("leaf", &waiter_cancellation));
        assert_pending(&mut waiter, "the first nested build owns the leaf slot").await;
        cancel_handle.cancel();
        assert!(matches!(waiter.await, Err(EnterBuildError::Cancelled)));

        drop(first_leaf);
        let next_leaf = second_outer
            .enter_child("leaf", &cancellation)
            .await
            .unwrap();
        drop(next_leaf);
    }

    async fn assert_pending<F: Future + Unpin>(future: &mut F, message: &str) {
        std::future::poll_fn(|cx| match std::pin::Pin::new(&mut *future).poll(cx) {
            Poll::Pending => Poll::Ready(()),
            Poll::Ready(_) => panic!("{message}"),
        })
        .await;
    }

    async fn poll_ready<F: Future + Unpin>(future: &mut F, message: &str) -> F::Output {
        std::future::poll_fn(|cx| match std::pin::Pin::new(&mut *future).poll(cx) {
            Poll::Ready(output) => Poll::Ready(output),
            Poll::Pending => panic!("{message}"),
        })
        .await
    }
}
