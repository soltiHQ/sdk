//! # Runner build admission
//!
//! [`RunnerBuildAdmission`] bounds managed runner construction.
//! [`BuildScope`] carries one admitted build path through composing runners.
//! One coordinator owns reservations, wait ordering, and nested wait-cycle detection for the complete managed runner hierarchy.

use std::{
    collections::{HashMap, HashSet, VecDeque},
    fmt,
    hash::{Hash, Hasher},
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
/// Root admission atomically reserves one global slot and one slot for the selected runner.
/// Nested builds reuse the outer global slot and reserve only the selected runner's slot.
/// An unavailable resource never causes a waiter to hold the other resource.
///
/// Fitting nested waiters have progress priority because their outer builds already own global slots.
/// Fitting roots follow. Within each class, the earliest fitting waiter is admitted;
/// an infeasible waiter does not block a later request whose complete resource set is available.
///
/// Every root build owns one internal identity.
/// Its nested permits keep that identity.
/// A nested wait is rejected only when every permit that could unblock the waiting roots is held inside the same wait cycle.
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
    /// Returns [`BuildAdmissionConfigError::ExceedsSemaphoreMaximum`] when a limit exceeds [`Semaphore::MAX_PERMITS`].
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
        let owner = BuildOwner::new();
        let permit = self
            .inner
            .acquire(
                AdmissionKind::Root,
                Arc::clone(&runner),
                owner.clone(),
                cancellation,
            )
            .await?;
        Ok(BuildScope {
            admission: Some(self.clone()),
            ancestry: vec![runner],
            owner: Some(owner),
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
    per_runner_in_use: HashMap<Arc<str>, RunnerUsage>,
    waiters: VecDeque<Arc<AdmissionWaiter>>,
}

/// Active reservations for one registered runner.
#[derive(Default)]
struct RunnerUsage {
    /// Total reserved permits.
    in_use: usize,
    /// Reserved permits grouped by root build.
    owners: HashMap<BuildOwner, usize>,
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

/// Pointer-stable identity for one root build.
#[derive(Clone)]
struct BuildOwner(Arc<BuildOwnerIdentity>);

/// Allocation that gives a build owner its identity.
struct BuildOwnerIdentity {
    /// Keeps the identity allocation non-zero-sized.
    _identity: u8,
}

impl BuildOwner {
    /// Creates an identity shared by one root build and all of its nested builds.
    fn new() -> Self {
        Self(Arc::new(BuildOwnerIdentity { _identity: 0 }))
    }
}

impl PartialEq for BuildOwner {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl Eq for BuildOwner {}

impl Hash for BuildOwner {
    fn hash<H: Hasher>(&self, state: &mut H) {
        Arc::as_ptr(&self.0).hash(state);
    }
}

struct AdmissionWaiter {
    kind: AdmissionKind,
    runner: Arc<str>,
    owner: BuildOwner,
    granted: AtomicBool,
    ready: Notify,
}

impl AdmissionInner {
    async fn acquire(
        self: &Arc<Self>,
        kind: AdmissionKind,
        runner: Arc<str>,
        owner: BuildOwner,
        cancellation: &BuildCancellation,
    ) -> Result<AdmissionPermit, EnterBuildError> {
        if cancellation.is_cancelled() {
            return Err(EnterBuildError::Cancelled);
        }

        let waiter = Arc::new(AdmissionWaiter {
            kind,
            runner,
            owner,
            granted: AtomicBool::new(false),
            ready: Notify::new(),
        });
        let mut registration = WaiterRegistration {
            admission: Arc::clone(self),
            waiter: Arc::clone(&waiter),
            active: true,
        };
        let admission_cycle = {
            let mut state = self.lock_state();
            state.waiters.push_back(Arc::clone(&waiter));
            self.grant_waiters(&mut state);
            let detected = kind == AdmissionKind::Nested
                && !waiter.granted.load(Ordering::Acquire)
                && self.admission_cycle_contains(&state, &waiter.owner);
            if detected {
                self.cancel_waiter_locked(&mut state, &waiter);
                self.grant_waiters(&mut state);
                registration.active = false;
            }
            detected
        };
        if admission_cycle {
            return Err(EnterBuildError::AdmissionCycle);
        }

        tokio::select! {
            biased;
            _ = cancellation.cancelled() => Err(EnterBuildError::Cancelled),
            () = wait_until_granted(&waiter) => {
                registration.active = false;
                Ok(AdmissionPermit {
                    admission: Arc::clone(self),
                    kind,
                    runner: Arc::clone(&waiter.runner),
                    owner: waiter.owner.clone(),
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
            let usage = state
                .per_runner_in_use
                .entry(Arc::clone(&waiter.runner))
                .or_default();
            usage.in_use += 1;
            *usage.owners.entry(waiter.owner.clone()).or_default() += 1;
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
            .map(|usage| usage.in_use)
            .unwrap_or_default();
        runner_in_use < self.per_runner_limit
            && (waiter.kind == AdmissionKind::Nested || state.global_in_use < self.global_limit)
    }

    /// Returns whether one nested waiter belongs to a closed admission wait set.
    ///
    /// A set is closed when every permit for each requested runner is held by another waiting owner in the same set.
    /// Scoped routing permits at most one queued nested wait for each root owner.
    fn admission_cycle_contains(&self, state: &AdmissionState, owner: &BuildOwner) -> bool {
        let nested_waits = state
            .waiters
            .iter()
            .filter(|waiter| waiter.kind == AdmissionKind::Nested)
            .map(|waiter| (waiter.owner.clone(), Arc::clone(&waiter.runner)))
            .collect::<HashMap<_, _>>();
        let mut blocked = nested_waits.keys().cloned().collect::<HashSet<_>>();

        loop {
            let candidates = blocked.clone();
            blocked.retain(|candidate| {
                let runner = &nested_waits[candidate];
                state.per_runner_in_use.get(runner).is_some_and(|usage| {
                    usage.in_use >= self.per_runner_limit
                        && usage
                            .owners
                            .keys()
                            .all(|holder| candidates.contains(holder))
                })
            });
            if blocked.len() == candidates.len() {
                return blocked.contains(owner);
            }
        }
    }

    fn cancel_waiter(&self, waiter: &AdmissionWaiter) {
        let mut state = self.lock_state();
        self.cancel_waiter_locked(&mut state, waiter);
        self.grant_waiters(&mut state);
    }

    /// Removes a queued waiter or releases its granted reservation.
    fn cancel_waiter_locked(&self, state: &mut AdmissionState, waiter: &AdmissionWaiter) {
        if waiter.granted.swap(false, Ordering::AcqRel) {
            self.release_reservation(state, waiter.kind, &waiter.runner, &waiter.owner);
        } else if let Some(index) = state
            .waiters
            .iter()
            .position(|queued| std::ptr::eq(queued.as_ref(), waiter))
        {
            state.waiters.remove(index);
        }
    }

    fn release(&self, kind: AdmissionKind, runner: &Arc<str>, owner: &BuildOwner) {
        let mut state = self.lock_state();
        self.release_reservation(&mut state, kind, runner, owner);
        self.grant_waiters(&mut state);
    }

    fn release_reservation(
        &self,
        state: &mut AdmissionState,
        kind: AdmissionKind,
        runner: &Arc<str>,
        owner: &BuildOwner,
    ) {
        if kind == AdmissionKind::Root {
            state.global_in_use = state
                .global_in_use
                .checked_sub(1)
                .expect("root admission must own one global reservation");
        }
        let remove_runner = {
            let usage = state
                .per_runner_in_use
                .get_mut(runner)
                .expect("admission permit runner must be reserved");
            usage.in_use = usage
                .in_use
                .checked_sub(1)
                .expect("runner admission reservation must not underflow");
            let remove_owner = {
                let owner_in_use = usage
                    .owners
                    .get_mut(owner)
                    .expect("admission permit owner must be reserved");
                *owner_in_use = owner_in_use
                    .checked_sub(1)
                    .expect("runner admission owner reservation must not underflow");
                *owner_in_use == 0
            };
            if remove_owner {
                usage.owners.remove(owner);
            }
            usage.in_use == 0
        };
        if remove_runner {
            let removed = state.per_runner_in_use.remove(runner);
            debug_assert!(removed.is_some_and(|usage| usage.owners.is_empty()));
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
    owner: BuildOwner,
}

impl Drop for AdmissionPermit {
    fn drop(&mut self) {
        self.admission.release(self.kind, &self.runner, &self.owner);
    }
}

/// Opaque admission path passed through one runner build.
///
/// A composing runner passes this value by mutable reference to
/// [`RunnerCatalog::build_scoped_with_cancellation`](crate::RunnerCatalog::build_scoped_with_cancellation).
/// The scope is intentionally not cloneable: one outer build has one ordered
/// nested-build path. Managed scopes also carry the root identity used to
/// detect wait cycles across concurrent paths.
#[must_use]
pub struct BuildScope {
    admission: Option<RunnerBuildAdmission>,
    ancestry: Vec<Arc<str>>,
    owner: Option<BuildOwner>,
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
            owner: None,
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
                    .acquire(
                        AdmissionKind::Nested,
                        Arc::clone(&runner),
                        self.owner
                            .clone()
                            .expect("managed build scope must have a root owner"),
                        cancellation,
                    )
                    .await?,
            ),
            None => None,
        };
        let mut ancestry = self.ancestry.clone();
        ancestry.push(runner);
        Ok(Self {
            admission: self.admission.clone(),
            ancestry,
            owner: self.owner.clone(),
            _permit: permit,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum EnterBuildError {
    AdmissionCycle,
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

    #[tokio::test]
    async fn two_root_cycle_rejects_the_closing_waiter_and_recovers_after_unwind() {
        let admission = RunnerBuildAdmission::new(2, 1).unwrap();
        let cancellation = BuildCancellation::new();
        let mut root_a = admission
            .enter_root("runner-a", &cancellation)
            .await
            .unwrap();
        let mut root_b = admission
            .enter_root("runner-b", &cancellation)
            .await
            .unwrap();

        let mut a_to_b = Box::pin(root_a.enter_child("runner-b", &cancellation));
        assert_pending(&mut a_to_b, "runner B is held by the other root").await;

        assert!(matches!(
            root_b.enter_child("runner-a", &cancellation).await,
            Err(EnterBuildError::AdmissionCycle)
        ));

        drop(root_b);
        let child = a_to_b.await.unwrap();
        drop(child);
    }

    #[tokio::test]
    async fn three_root_cycle_rejects_the_closing_waiter_and_recovers_in_order() {
        let admission = RunnerBuildAdmission::new(3, 1).unwrap();
        let cancellation = BuildCancellation::new();
        let mut root_a = admission
            .enter_root("runner-a", &cancellation)
            .await
            .unwrap();
        let mut root_b = admission
            .enter_root("runner-b", &cancellation)
            .await
            .unwrap();
        let mut root_c = admission
            .enter_root("runner-c", &cancellation)
            .await
            .unwrap();

        let mut a_to_b = Box::pin(root_a.enter_child("runner-b", &cancellation));
        assert_pending(&mut a_to_b, "runner B is held by the second root").await;
        let mut b_to_c = Box::pin(root_b.enter_child("runner-c", &cancellation));
        assert_pending(&mut b_to_c, "runner C is held by the third root").await;

        assert!(matches!(
            root_c.enter_child("runner-a", &cancellation).await,
            Err(EnterBuildError::AdmissionCycle)
        ));

        drop(root_c);
        let child_c = b_to_c.await.unwrap();
        drop(child_c);
        drop(root_b);
        let child_b = a_to_b.await.unwrap();
        drop(child_b);
    }

    #[tokio::test]
    async fn cancelled_nested_waiter_leaves_no_cycle_edge() {
        let admission = RunnerBuildAdmission::new(2, 1).unwrap();
        let cancellation = BuildCancellation::new();
        let mut root_a = admission
            .enter_root("runner-a", &cancellation)
            .await
            .unwrap();
        let mut root_b = admission
            .enter_root("runner-b", &cancellation)
            .await
            .unwrap();
        let (cancel_handle, waiter_cancellation) = BuildCancellation::pair();

        let mut a_to_b = Box::pin(root_a.enter_child("runner-b", &waiter_cancellation));
        assert_pending(&mut a_to_b, "runner B is held by the other root").await;
        cancel_handle.cancel();
        assert!(matches!(a_to_b.await, Err(EnterBuildError::Cancelled)));

        let mut b_to_a = Box::pin(root_b.enter_child("runner-a", &cancellation));
        assert_pending(
            &mut b_to_a,
            "the cancelled A-to-B edge must not create a cycle",
        )
        .await;
        drop(root_a);
        let child = b_to_a.await.unwrap();
        drop(child);
    }

    #[tokio::test]
    async fn acyclic_nested_wait_chain_waits_and_recovers() {
        let admission = RunnerBuildAdmission::new(3, 1).unwrap();
        let cancellation = BuildCancellation::new();
        let mut root_a = admission
            .enter_root("runner-a", &cancellation)
            .await
            .unwrap();
        let mut root_b = admission
            .enter_root("runner-b", &cancellation)
            .await
            .unwrap();
        let root_c = admission
            .enter_root("runner-c", &cancellation)
            .await
            .unwrap();

        let mut a_to_b = Box::pin(root_a.enter_child("runner-b", &cancellation));
        assert_pending(&mut a_to_b, "runner B is held by the second root").await;
        let mut b_to_c = Box::pin(root_b.enter_child("runner-c", &cancellation));
        assert_pending(&mut b_to_c, "runner C is held by the third root").await;

        drop(root_c);
        let child_c = b_to_c.await.unwrap();
        drop(child_c);
        drop(root_b);
        let child_b = a_to_b.await.unwrap();
        drop(child_b);
    }

    #[tokio::test]
    async fn active_outside_holder_prevents_a_false_cycle_at_capacity_two() {
        let admission = RunnerBuildAdmission::new(4, 2).unwrap();
        let cancellation = BuildCancellation::new();
        let mut first_root_a = admission
            .enter_root("runner-a", &cancellation)
            .await
            .unwrap();
        let second_root_a = admission
            .enter_root("runner-a", &cancellation)
            .await
            .unwrap();
        let mut first_root_b = admission
            .enter_root("runner-b", &cancellation)
            .await
            .unwrap();
        let second_root_b = admission
            .enter_root("runner-b", &cancellation)
            .await
            .unwrap();

        let mut a_to_b = Box::pin(first_root_a.enter_child("runner-b", &cancellation));
        assert_pending(&mut a_to_b, "both runner B permits are held").await;
        let mut b_to_a = Box::pin(first_root_b.enter_child("runner-a", &cancellation));
        assert_pending(
            &mut b_to_a,
            "the active second B holder can release a permit",
        )
        .await;

        drop(second_root_b);
        let child_b = a_to_b.await.unwrap();
        drop(child_b);
        drop(first_root_a);
        let child_a = b_to_a.await.unwrap();
        drop(child_a);
        drop(second_root_a);
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
