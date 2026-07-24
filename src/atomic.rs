use std::sync::atomic::{AtomicUsize, Ordering};

/// A read-only view of a cursor published by a single writer.
///
/// Keeping this view separate from the owning cursor prevents readers from accidentally acquiring
/// publication authority.
#[derive(Debug, Clone, Copy)]
#[repr(transparent)]
pub(crate) struct PublishedCursorReader<'a>(&'a AtomicUsize);

impl<'a> PublishedCursorReader<'a> {
    #[inline]
    pub(crate) fn new(cursor: &'a AtomicUsize) -> Self {
        Self(cursor)
    }

    #[inline]
    pub(crate) fn get(&self) -> usize {
        self.0.load(Ordering::Acquire)
    }
}

trait RewindableAtomic {
    fn load(&self, ordering: Ordering) -> usize;
    fn compare_exchange_weak(
        &self,
        current: usize,
        new: usize,
        success: Ordering,
        failure: Ordering,
    ) -> Result<usize, usize>;
    fn fetch_min(&self, value: usize, ordering: Ordering) -> usize;
}

impl RewindableAtomic for AtomicUsize {
    #[inline]
    fn load(&self, ordering: Ordering) -> usize {
        AtomicUsize::load(self, ordering)
    }

    #[inline]
    fn compare_exchange_weak(
        &self,
        current: usize,
        new: usize,
        success: Ordering,
        failure: Ordering,
    ) -> Result<usize, usize> {
        AtomicUsize::compare_exchange_weak(self, current, new, success, failure)
    }

    #[inline]
    fn fetch_min(&self, value: usize, ordering: Ordering) -> usize {
        AtomicUsize::fetch_min(self, value, ordering)
    }
}

#[inline]
fn claim_before(cursor: &impl RewindableAtomic, limit: usize) -> Option<usize> {
    loop {
        let current = cursor.load(Ordering::Acquire);
        if current >= limit {
            return None;
        }
        if cursor
            .compare_exchange_weak(current, current + 1, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            return Some(current);
        }
    }
}

#[inline]
fn rewind(cursor: &impl RewindableAtomic, value: usize) -> usize {
    cursor.fetch_min(value, Ordering::AcqRel)
}

/// A concurrently claimed cursor that can be rewound when earlier work becomes eligible again.
#[derive(Debug)]
#[repr(transparent)]
pub(crate) struct RewindableCursor(AtomicUsize);

impl RewindableCursor {
    #[inline]
    pub(crate) fn new(value: usize) -> Self {
        Self(AtomicUsize::new(value))
    }

    #[inline]
    pub(crate) fn get(&self) -> usize {
        self.0.load(Ordering::Acquire)
    }

    #[inline]
    pub(crate) fn claim_before(&self, limit: usize) -> Option<usize> {
        claim_before(&self.0, limit)
    }

    /// Rewind to `value`, returning the previous cursor position.
    #[inline]
    pub(crate) fn rewind(&self, value: usize) -> usize {
        rewind(&self.0, value)
    }
}

/// A low-contention work cursor whose internal position may transiently pass its limit.
///
/// Returned claims are always bounded. Overshoot is acceptable because this cursor is only a
/// scheduling hint; task state is synchronized separately by its mutex.
#[derive(Debug)]
#[repr(transparent)]
pub(crate) struct SpeculativeWorkCursor(AtomicUsize);

impl SpeculativeWorkCursor {
    #[inline]
    pub(crate) fn new(value: usize) -> Self {
        Self(AtomicUsize::new(value))
    }

    #[inline]
    pub(crate) fn get(&self) -> usize {
        self.0.load(Ordering::Relaxed)
    }

    #[inline]
    pub(crate) fn claim_before(&self, limit: usize) -> Option<usize> {
        if self.get() >= limit {
            return None;
        }
        let claimed = self.0.fetch_add(1, Ordering::Relaxed);
        (claimed < limit).then_some(claimed)
    }

    #[inline]
    pub(crate) fn rewind(&self, value: usize) {
        self.0.fetch_min(value, Ordering::Relaxed);
    }
}

/// A `usize` whose value is observed only for diagnostics or after worker synchronization.
#[derive(Debug, Default)]
#[repr(transparent)]
pub(crate) struct RelaxedUsize(AtomicUsize);

impl RelaxedUsize {
    #[inline]
    pub(crate) fn increment(&self) {
        self.add(1);
    }

    #[inline]
    pub(crate) fn add(&self, value: usize) {
        self.0.fetch_add(value, Ordering::Relaxed);
    }

    #[inline]
    pub(crate) fn set(&self, value: usize) {
        self.0.store(value, Ordering::Relaxed);
    }

    #[inline]
    pub(crate) fn get(&self) -> usize {
        self.0.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    impl RewindableAtomic for loom::sync::atomic::AtomicUsize {
        fn load(&self, ordering: Ordering) -> usize {
            loom::sync::atomic::AtomicUsize::load(self, ordering)
        }

        fn compare_exchange_weak(
            &self,
            current: usize,
            new: usize,
            success: Ordering,
            failure: Ordering,
        ) -> Result<usize, usize> {
            loom::sync::atomic::AtomicUsize::compare_exchange_weak(
                self, current, new, success, failure,
            )
        }

        fn fetch_min(&self, value: usize, ordering: Ordering) -> usize {
            let mut current = self.load(Ordering::Acquire);
            while value < current {
                match self.compare_exchange_weak(current, value, ordering, Ordering::Acquire) {
                    Ok(previous) => return previous,
                    Err(observed) => current = observed,
                }
            }
            current
        }
    }

    #[test]
    fn rewindable_cursor_never_claims_past_its_limit() {
        let cursor = RewindableCursor::new(0);
        let claimed = std::sync::Mutex::new(Vec::new());
        std::thread::scope(|scope| {
            for _ in 0..8 {
                scope.spawn(|| {
                    while let Some(index) = cursor.claim_before(100) {
                        claimed.lock().unwrap().push(index);
                    }
                });
            }
        });

        let mut claimed = claimed.into_inner().unwrap();
        claimed.sort_unstable();
        assert_eq!(claimed, (0..100).collect::<Vec<_>>());
        assert_eq!(cursor.get(), 100);
    }

    #[test]
    fn rewindable_cursor_can_reissue_work() {
        let cursor = RewindableCursor::new(3);
        assert_eq!(cursor.rewind(1), 3);
        assert_eq!(cursor.claim_before(3), Some(1));
        assert_eq!(cursor.claim_before(3), Some(2));
        assert_eq!(cursor.claim_before(3), None);
    }

    #[test]
    fn rewind_concurrent_with_claim_reissues_every_rewound_index() {
        loom::model(|| {
            use loom::{
                sync::{
                    Arc,
                    atomic::{AtomicUsize, Ordering},
                },
                thread,
            };

            const NO_CLAIM: usize = usize::MAX;

            let cursor = Arc::new(AtomicUsize::new(1));
            let concurrent_claim = Arc::new(AtomicUsize::new(NO_CLAIM));

            let claim_cursor = Arc::clone(&cursor);
            let claimed = Arc::clone(&concurrent_claim);
            let claim_thread = thread::spawn(move || {
                if let Some(index) = claim_before(claim_cursor.as_ref(), 2) {
                    claimed.store(index, Ordering::Relaxed);
                }
            });

            let rewind_cursor = Arc::clone(&cursor);
            let rewind_thread = thread::spawn(move || {
                rewind(rewind_cursor.as_ref(), 0);
            });

            claim_thread.join().unwrap();
            rewind_thread.join().unwrap();

            let mut claims = Vec::new();
            let concurrent = concurrent_claim.load(Ordering::Relaxed);
            if concurrent != NO_CLAIM {
                claims.push(concurrent);
            }
            while let Some(index) = claim_before(cursor.as_ref(), 2) {
                claims.push(index);
            }

            assert!(claims.iter().all(|&index| index < 2));
            assert!(claims.contains(&0), "rewound index must become claimable");
            assert!(claims.contains(&1), "the original position must remain claimable");
        });
    }

    #[test]
    fn speculative_work_cursor_never_returns_an_out_of_range_claim() {
        let cursor = SpeculativeWorkCursor::new(0);
        std::thread::scope(|scope| {
            for _ in 0..8 {
                scope.spawn(|| {
                    while let Some(index) = cursor.claim_before(10) {
                        assert!(index < 10);
                    }
                });
            }
        });
        assert!(cursor.get() >= 10);
    }
}
