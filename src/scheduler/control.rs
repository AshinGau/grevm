//! One-shot execution lifecycle and public scheduler control flow.

use super::{Scheduler, ordered_commit::CommittedPrefixEnd};
use crate::{AbortReason, GrevmError, ParallelState, TxExecutionOutcome};
use revm::DatabaseRef;
use revm_context::result::EVMError;
use std::{
    sync::atomic::{AtomicU8, Ordering},
    time::Instant,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
enum ExecutionPhase {
    Ready = 0,
    Running = 1,
    Completed = 2,
}

impl ExecutionPhase {
    fn from_raw(value: u8) -> Self {
        match value {
            0 => Self::Ready,
            1 => Self::Running,
            2 => Self::Completed,
            _ => unreachable!("invalid scheduler execution phase"),
        }
    }
}

/// One-shot lifecycle shared by every public execution entry point.
///
/// Dropping the execution permit completes the lifecycle after success, error, or unwinding, so a
/// scheduler can never be restarted with partially consumed state.
#[derive(Debug)]
pub(super) struct ExecutionLifecycle(AtomicU8);

impl ExecutionLifecycle {
    pub(super) fn new() -> Self {
        Self(AtomicU8::new(ExecutionPhase::Ready as u8))
    }

    fn try_start(&self) -> Result<ExecutionPermit<'_>, ExecutionPhase> {
        self.0
            .compare_exchange(
                ExecutionPhase::Ready as u8,
                ExecutionPhase::Running as u8,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .map(|_| ExecutionPermit(self))
            .map_err(ExecutionPhase::from_raw)
    }
}

#[must_use = "the execution permit must be held until the execution attempt finishes"]
pub(super) struct ExecutionPermit<'a>(&'a ExecutionLifecycle);

impl Drop for ExecutionPermit<'_> {
    fn drop(&mut self) {
        self.0.0.store(ExecutionPhase::Completed as u8, Ordering::Release);
    }
}

impl<DB> Scheduler<DB>
where
    DB: DatabaseRef + Send + Sync,
    DB::Error: Clone + Send + Sync + 'static,
{
    /// Take the ordered transaction outcomes and the corresponding `ParallelState`.
    ///
    /// Before execution this returns an empty outcome list and untouched state. After an error it
    /// returns the successfully committed prefix; state and outcomes always describe that same
    /// prefix.
    #[must_use]
    pub fn take_result_and_state(self) -> (Vec<TxExecutionOutcome>, ParallelState<DB>) {
        (self.results.into_inner(), self.state.into_inner())
    }

    /// Execute using the scheduler's unified runtime configuration.
    ///
    /// # Errors
    ///
    /// Returns an error if this scheduler has already started, or if execution encounters a
    /// database, fatal EVM, or scheduler invariant error. Invalid transactions are skipped.
    pub fn execute(&self) -> Result<(), GrevmError<DB::Error>> {
        self.parallel_execute(None)
    }

    /// Execute with an optional per-call concurrency override.
    ///
    /// New integrations should configure [`crate::GrevmConfig::concurrency_level`] and call
    /// [`Self::execute`].
    ///
    /// # Errors
    ///
    /// Returns an error if this scheduler has already started, or if execution encounters a
    /// database, fatal EVM, or scheduler invariant error. Invalid transactions are skipped.
    ///
    /// # Panics
    ///
    /// Panics if `concurrency_level` is zero.
    pub fn parallel_execute(
        &self,
        concurrency_level: Option<usize>,
    ) -> Result<(), GrevmError<DB::Error>> {
        let concurrency_level = concurrency_level.unwrap_or(self.config.concurrency_level);
        assert!(concurrency_level > 0, "grevm concurrency level must be greater than zero");
        let _execution = self.begin_execution()?;
        self.run_measured(|started| self.parallel_execute_inner(concurrency_level, started))
    }

    pub(super) fn begin_execution(&self) -> Result<ExecutionPermit<'_>, GrevmError<DB::Error>> {
        let txid = self.scheduler_ctx.committed_idx().min(self.block_size.saturating_sub(1));
        self.execution.try_start().map_err(|phase| GrevmError {
            txid,
            error: EVMError::Custom(format!(
                "a Scheduler can execute only once; create a new Scheduler for each block \
                 (current phase: {phase:?})"
            )),
        })
    }

    pub(super) fn run_measured(
        &self,
        execute: impl FnOnce(Instant) -> Result<(), GrevmError<DB::Error>>,
    ) -> Result<(), GrevmError<DB::Error>> {
        let started = Instant::now();
        self.metrics.record_block_start(self.block_size);
        let result = execute(started);
        self.metrics.record_validation_resets(self.scheduler_ctx.validation_reset_count());
        self.metrics.record_total_time(started.elapsed());
        self.metrics.report();
        result
    }

    pub(super) fn post_execute(
        &self,
        committed: CommittedPrefixEnd,
    ) -> Result<(), GrevmError<DB::Error>> {
        // `committed` is the authoritative committed boundary. Abort metadata selects whether the
        // remaining suffix is replayed or an unrecoverable error is returned.
        if self.is_aborted() {
            match self.abort_reason.get() {
                Some(AbortReason::FatalEvmError(txid)) => {
                    let error = self.tx_results.get(*txid).and_then(|result| {
                        result
                            .lock()
                            .as_ref()
                            .and_then(|result| result.execute_result.as_ref().err().cloned())
                    });
                    if let Some(error) = error {
                        return Err(GrevmError { txid: *txid, error });
                    }

                    // Losing the execution error is itself a parallel scheduler inconsistency.
                    // The committed prefix remains authoritative, so replay the suffix.
                    return self.fallback_after_parallel_error(
                        committed,
                        *txid,
                        "fatal execution abort has no matching transaction error",
                    );
                }
                // Parallel execution normally returns the commit-thread error before this branch.
                // Keeping it in the abort reason makes `post_execute` correct on its own as well.
                Some(AbortReason::CommitError(error)) => return Err(error.clone()),
                Some(AbortReason::ParallelError { txid, message }) => {
                    return self.fallback_after_parallel_error(committed, *txid, message);
                }
                Some(AbortReason::SelfDestructed | AbortReason::FallbackSequential) => {
                    return self.replay_uncommitted_suffix(committed);
                }
                None => {
                    return self.fallback_after_parallel_error(
                        committed,
                        self.scheduler_ctx.committed_idx(),
                        "parallel execution aborted without a reason",
                    );
                }
            }
        }
        Ok(())
    }

    pub(super) fn abort(&self, abort_reason: AbortReason<DB::Error>) {
        // Preserve the first abort cause. Publish it before the release-store so acquire readers
        // that observe `abort` can also observe the reason.
        self.abort_reason.get_or_init(|| abort_reason);
        self.abort.store(true, Ordering::Release);
        self.finality_wait.notify();
        self.commit_wait.notify();
    }

    #[inline]
    pub(super) fn is_aborted(&self) -> bool {
        self.abort.load(Ordering::Acquire)
    }
}
