//! Process-wide resource coordination for concurrent GREVM schedulers.

use parking_lot::{Condvar, Mutex};
use std::{
    collections::VecDeque,
    fmt,
    num::NonZeroUsize,
    sync::{Arc, OnceLock},
    time::Duration,
};

const CANCELLATION_POLL_INTERVAL: Duration = Duration::from_millis(10);
const PARALLEL_COORDINATOR_COUNT: usize = 2;
const MIN_PARALLEL_WORKER_COUNT: usize = 2;
const MIN_PARALLEL_ROLE_COUNT: usize = PARALLEL_COORDINATOR_COUNT + MIN_PARALLEL_WORKER_COUNT;

/// A shared limit on active GREVM execution roles.
///
/// Parallel schedulers consume one slot per speculative worker plus one slot for finality and one
/// for ordered commit. Sequential execution consumes one slot. Clones coordinate through the same
/// FIFO budget, so integrations can bound overlapping payload builds, validation, and historical
/// execution without coupling the limit to any one caller.
///
/// Synchronous nested execution with the same budget is not supported. A database callback, state
/// hook, or custom precompile must not wait for another scheduler sharing this budget because the
/// outer scheduler retains its permit until the callback returns. Use a separately budgeted inner
/// execution only when the embedding application can account for the combined CPU limit.
#[derive(Clone)]
pub struct ExecutionResources {
    inner: Arc<ExecutionResourcesInner>,
}

impl ExecutionResources {
    /// Returns the process-wide default budget.
    ///
    /// Its capacity is the logical parallelism reported by the operating system, falling back to
    /// one. Every call returns a handle to the same budget.
    pub fn process_default() -> Self {
        static PROCESS_DEFAULT: OnceLock<ExecutionResources> = OnceLock::new();
        PROCESS_DEFAULT
            .get_or_init(|| {
                let capacity = std::thread::available_parallelism().unwrap_or(NonZeroUsize::MIN);
                Self::dedicated(capacity)
            })
            .clone()
    }

    /// Creates an independent budget with `max_active_roles` slots.
    ///
    /// Dedicated budgets are useful when an embedding application already partitions CPU capacity
    /// between subsystems or when execution must be isolated in tests.
    pub fn dedicated(max_active_roles: NonZeroUsize) -> Self {
        let capacity = max_active_roles.get();
        Self {
            inner: Arc::new(ExecutionResourcesInner {
                capacity,
                state: Mutex::new(ResourceState {
                    available: capacity,
                    next_waiter: 0,
                    waiters: VecDeque::new(),
                }),
                available: Condvar::new(),
            }),
        }
    }

    /// Returns the maximum number of active execution roles coordinated by this budget.
    pub fn capacity(&self) -> NonZeroUsize {
        NonZeroUsize::new(self.inner.capacity).expect("execution resource capacity is non-zero")
    }

    pub(crate) fn acquire(&self, desired_parallel_workers: Option<usize>) -> ExecutionAllocation {
        self.acquire_inner(desired_parallel_workers, None)
            .expect("execution resource acquisition without cancellation cannot interrupt")
    }

    pub(crate) fn acquire_with_cancellation<C>(
        &self,
        desired_parallel_workers: Option<usize>,
        cancelled: C,
    ) -> Option<ExecutionAllocation>
    where
        C: Fn() -> bool,
    {
        self.acquire_inner(desired_parallel_workers, Some(&cancelled))
    }

    fn acquire_inner(
        &self,
        desired_parallel_workers: Option<usize>,
        cancelled: Option<&dyn Fn() -> bool>,
    ) -> Option<ExecutionAllocation> {
        if cancelled.is_some_and(|cancelled| cancelled()) {
            return None
        }

        let mut waiter = {
            let mut state = self.inner.state.lock();
            let waiter = state.next_waiter;
            state.next_waiter =
                state.next_waiter.checked_add(1).expect("execution resource waiter id exhausted");
            state.waiters.push_back(waiter);
            QueuedWaiter { inner: self.inner.clone(), id: waiter, queued: true }
        };

        loop {
            if cancelled.is_some_and(|cancelled| cancelled()) {
                return None
            }

            let mut state = self.inner.state.lock();
            if state.waiters.front() == Some(&waiter.id) && state.available > 0 {
                let (slots, workers) =
                    allocation_for(self.inner.capacity, state.available, desired_parallel_workers);
                state.available -= slots;
                let queued = state.waiters.pop_front();
                debug_assert_eq!(queued, Some(waiter.id));
                waiter.queued = false;
                self.inner.available.notify_all();
                drop(state);

                let permit = ResourcePermit { inner: self.inner.clone(), slots };
                return Some(match workers {
                    Some(workers) => ExecutionAllocation::Parallel { workers, _permit: permit },
                    None => ExecutionAllocation::Sequential { _permit: permit },
                })
            }

            if cancelled.is_some() {
                self.inner.available.wait_for(&mut state, CANCELLATION_POLL_INTERVAL);
            } else {
                self.inner.available.wait(&mut state);
            }
            drop(state);
        }
    }

    #[cfg(test)]
    pub(crate) fn available(&self) -> usize {
        self.inner.state.lock().available
    }

    #[cfg(test)]
    pub(crate) fn waiter_count(&self) -> usize {
        self.inner.state.lock().waiters.len()
    }
}

impl Default for ExecutionResources {
    fn default() -> Self {
        Self::process_default()
    }
}

impl fmt::Debug for ExecutionResources {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ExecutionResources")
            .field("capacity", &self.inner.capacity)
            .finish_non_exhaustive()
    }
}

pub(crate) enum ExecutionAllocation {
    Sequential { _permit: ResourcePermit },
    Parallel { workers: NonZeroUsize, _permit: ResourcePermit },
}

impl ExecutionAllocation {
    pub(crate) fn retain_sequential_role(&mut self) {
        if let Self::Parallel { _permit, .. } = self {
            _permit.retain(1);
        }
    }
}

struct ExecutionResourcesInner {
    capacity: usize,
    state: Mutex<ResourceState>,
    available: Condvar,
}

struct ResourceState {
    available: usize,
    next_waiter: u64,
    waiters: VecDeque<u64>,
}

pub(crate) struct ResourcePermit {
    inner: Arc<ExecutionResourcesInner>,
    slots: usize,
}

struct QueuedWaiter {
    inner: Arc<ExecutionResourcesInner>,
    id: u64,
    queued: bool,
}

impl Drop for QueuedWaiter {
    fn drop(&mut self) {
        if !self.queued {
            return
        }
        let mut state = self.inner.state.lock();
        if let Some(position) = state.waiters.iter().position(|queued| *queued == self.id) {
            state.waiters.remove(position);
            self.inner.available.notify_all();
        }
    }
}

impl Drop for ResourcePermit {
    fn drop(&mut self) {
        let mut state = self.inner.state.lock();
        state.available = state
            .available
            .checked_add(self.slots)
            .expect("execution resource permit count overflowed");
        debug_assert!(state.available <= self.inner.capacity);
        self.inner.available.notify_all();
    }
}

impl ResourcePermit {
    fn retain(&mut self, slots: usize) {
        debug_assert!(slots > 0 && slots <= self.slots);
        let released = self.slots - slots;
        if released == 0 {
            return
        }
        self.slots = slots;
        let mut state = self.inner.state.lock();
        state.available = state
            .available
            .checked_add(released)
            .expect("execution resource permit count overflowed");
        debug_assert!(state.available <= self.inner.capacity);
        self.inner.available.notify_all();
    }
}

fn allocation_for(
    capacity: usize,
    available: usize,
    desired_parallel_workers: Option<usize>,
) -> (usize, Option<NonZeroUsize>) {
    let Some(desired_workers) = desired_parallel_workers else { return (1, None) };
    if desired_workers < MIN_PARALLEL_WORKER_COUNT ||
        capacity < MIN_PARALLEL_ROLE_COUNT ||
        available < MIN_PARALLEL_ROLE_COUNT
    {
        return (1, None)
    }

    let roles =
        desired_workers.saturating_add(PARALLEL_COORDINATOR_COUNT).min(capacity).min(available);
    let workers = roles - PARALLEL_COORDINATOR_COUNT;
    (roles, NonZeroUsize::new(workers))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        sync::{
            Arc,
            atomic::{AtomicBool, AtomicUsize, Ordering},
            mpsc,
        },
        thread,
        time::{Duration, Instant},
    };

    fn wait_until(mut predicate: impl FnMut() -> bool) -> bool {
        let deadline = Instant::now() + Duration::from_secs(1);
        while !predicate() {
            if Instant::now() >= deadline {
                return false
            }
            thread::yield_now();
        }
        true
    }

    #[test]
    fn parallel_allocation_is_clamped_to_capacity() {
        let resources = ExecutionResources::dedicated(NonZeroUsize::new(4).unwrap());
        let mut allocation = resources.acquire(Some(usize::MAX));

        let ExecutionAllocation::Parallel { workers, .. } = allocation else {
            panic!("four roles must allow parallel execution")
        };
        assert_eq!(workers.get(), 2);
        assert_eq!(resources.available(), 0);
        allocation.retain_sequential_role();
        assert_eq!(resources.available(), 3);
        drop(allocation);
        assert_eq!(resources.available(), 4);
    }

    #[test]
    fn small_budgets_and_single_worker_requests_are_sequential() {
        for capacity in 1..=3 {
            let resources = ExecutionResources::dedicated(NonZeroUsize::new(capacity).unwrap());
            assert!(matches!(resources.acquire(Some(8)), ExecutionAllocation::Sequential { .. }));
        }

        let resources = ExecutionResources::dedicated(NonZeroUsize::new(8).unwrap());
        assert!(matches!(resources.acquire(Some(1)), ExecutionAllocation::Sequential { .. }));
    }

    #[test]
    fn cancellation_removes_a_waiter_and_releases_the_next_one() {
        let resources = ExecutionResources::dedicated(NonZeroUsize::MIN);
        let mut first = Some(resources.acquire(None));
        let cancelled = Arc::new(AtomicBool::new(false));
        let (result_tx, result_rx) = mpsc::channel();
        let (next_tx, next_rx) = mpsc::channel();

        let failures = thread::scope(|scope| {
            let mut failures = Vec::new();
            let waiting_resources = resources.clone();
            let waiting_cancelled = cancelled.clone();
            scope.spawn(move || {
                let allocation = waiting_resources
                    .acquire_with_cancellation(None, || waiting_cancelled.load(Ordering::Acquire));
                result_tx.send(allocation.is_none()).unwrap();
            });

            if !wait_until(|| resources.waiter_count() == 1) {
                failures.push("first waiter did not enter the queue");
                cancelled.store(true, Ordering::Release);
                drop(first.take());
                return failures
            }
            let next_resources = resources.clone();
            scope.spawn(move || {
                let _allocation = next_resources.acquire(None);
                next_tx.send(()).unwrap();
            });
            if !wait_until(|| resources.waiter_count() == 2) {
                failures.push("second waiter did not enter the queue");
                cancelled.store(true, Ordering::Release);
                drop(first.take());
                return failures
            }

            cancelled.store(true, Ordering::Release);
            match result_rx.recv_timeout(Duration::from_secs(1)) {
                Ok(true) => {}
                Ok(false) => failures.push("cancelled waiter acquired a permit"),
                Err(_) => failures.push("cancelled waiter did not return"),
            }
            if !wait_until(|| resources.waiter_count() == 1) {
                failures.push("cancelled waiter remained queued");
            }
            let next_completed_early = match next_rx.try_recv() {
                Ok(()) => {
                    failures.push("next waiter acquired before the held permit was released");
                    true
                }
                Err(mpsc::TryRecvError::Empty) => false,
                Err(mpsc::TryRecvError::Disconnected) => {
                    failures.push("next waiter disconnected before acquiring a permit");
                    true
                }
            };
            drop(first.take());
            if !next_completed_early && next_rx.recv_timeout(Duration::from_secs(1)).is_err() {
                failures.push("next waiter did not acquire the released permit");
            }
            failures
        });

        assert!(failures.is_empty(), "{}", failures.join("; "));
        assert_eq!(resources.available(), 1);
    }

    #[test]
    fn panicking_cancellation_check_does_not_strand_the_fifo() {
        let resources = ExecutionResources::dedicated(NonZeroUsize::MIN);
        let _first = resources.acquire(None);
        let polls = AtomicUsize::new(0);

        let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            resources.acquire_with_cancellation(None, || {
                if polls.fetch_add(1, Ordering::Relaxed) == 0 {
                    false
                } else {
                    panic!("injected cancellation panic")
                }
            })
        }));

        assert!(panic.is_err());
        assert_eq!(resources.waiter_count(), 0);
    }

    #[test]
    fn queued_allocations_are_fifo() {
        let resources = ExecutionResources::dedicated(NonZeroUsize::MIN);
        let mut first = Some(resources.acquire(None));
        let (order_tx, order_rx) = mpsc::channel();
        let (release_first_tx, release_first_rx) = mpsc::channel();

        let failures = thread::scope(|scope| {
            let mut failures = Vec::new();
            let first_resources = resources.clone();
            let first_order = order_tx.clone();
            scope.spawn(move || {
                let allocation = first_resources.acquire(None);
                first_order.send(1).unwrap();
                release_first_rx.recv().unwrap();
                drop(allocation);
            });
            if !wait_until(|| resources.waiter_count() == 1) {
                failures.push("first waiter did not enter the queue");
                drop(first.take());
                let _ = release_first_tx.send(());
                return failures
            }

            let second_resources = resources.clone();
            scope.spawn(move || {
                let _allocation = second_resources.acquire(None);
                order_tx.send(2).unwrap();
            });
            if !wait_until(|| resources.waiter_count() == 2) {
                failures.push("second waiter did not enter the queue");
                drop(first.take());
                let _ = release_first_tx.send(());
                return failures
            }

            drop(first.take());
            match order_rx.recv_timeout(Duration::from_secs(1)) {
                Ok(1) => {}
                Ok(_) => failures.push("second waiter acquired before the first waiter"),
                Err(_) => failures.push("first waiter did not acquire the released permit"),
            }
            if release_first_tx.send(()).is_err() {
                failures.push("first waiter exited before its release signal");
            }
            match order_rx.recv_timeout(Duration::from_secs(1)) {
                Ok(2) => {}
                Ok(_) => failures.push("unexpected waiter acquisition order"),
                Err(_) => failures.push("second waiter did not acquire after the first released"),
            }
            failures
        });

        assert!(failures.is_empty(), "{}", failures.join("; "));
    }
}
