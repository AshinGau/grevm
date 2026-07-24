mod context;
mod executor;
mod fallback;
mod metrics;
#[cfg(test)]
mod tests;
mod wait;

use crate::{
    AbortReason, GrevmConfig, GrevmError, LocationAndType, MVMemory, ParallelState, ReadVersion,
    Task, TransactionResult, TransactionStatus, TxExecutionOutcome, TxId, TxState, TxVersion,
    async_commit::StateAsyncCommit,
    cache_db::CacheDB,
    delegated_safety::{DelegatedSafetyConfig, ReservePlanner},
    hint::ParallelExecutionHints,
    tx_dependency::TxDependency,
};
use ::metrics::histogram;
use ahash::{AHashMap as HashMap, AHashSet as HashSet};
use alloy_evm::precompiles::DynPrecompile;
use context::SchedulerContext;
use executor::{GrevmExecutor, ParallelTransactionExecutor};
use metrics::ExecuteMetricsCollector;
use parking_lot::{Mutex, MutexGuard};
use revm::DatabaseRef;
use revm_context::{BlockEnv, CfgEnv, TxEnv, result::EVMError};
use revm_primitives::Address;

use std::{
    cmp::max,
    fmt::Debug,
    sync::{
        Arc, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};
use wait::WaitSlot;

const STALL_TIMEOUT: Duration = Duration::from_secs(8);

/// The `Scheduler` struct is responsible for managing the parallel execution of transactions
/// in a block. It coordinates the execution, validation, and finalization of transactions
/// while handling dependencies and conflicts between them.
///
/// # Type Parameters
/// - `DB`: A type that implements the `DatabaseRef` trait, representing the database used for
///   transaction execution.
pub struct Scheduler<DB>
where
    DB: DatabaseRef,
{
    cfg: CfgEnv,
    env: BlockEnv,
    block_size: usize,
    txs: Arc<Vec<TxEnv>>,
    state: Mutex<ParallelState<DB>>,
    results: Mutex<Vec<TxExecutionOutcome>>,
    tx_states: Vec<Mutex<TxState>>,
    tx_results: Vec<Mutex<Option<TransactionResult<DB::Error>>>>,
    tx_dependency: TxDependency,

    mv_memory: MVMemory,
    scheduler_ctx: SchedulerContext,
    custom_precompiles: Arc<Vec<(Address, DynPrecompile)>>,
    config: GrevmConfig,
    reserve_planner: Option<Arc<ReservePlanner>>,

    abort: AtomicBool,
    abort_reason: OnceLock<AbortReason<DB::Error>>,
    finality_wait: WaitSlot,
    commit_wait: WaitSlot,
    metrics: ExecuteMetricsCollector,
}

impl<DB> Debug for Scheduler<DB>
where
    DB: DatabaseRef,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Scheduler")
            .field("cfg", &self.cfg)
            .field("env", &self.env)
            .field("block_size", &self.block_size)
            .field("txs", &self.txs)
            .finish()
    }
}

impl<DB> Scheduler<DB>
where
    DB: DatabaseRef + Send + Sync,
    DB::Error: Clone + Send + Sync + 'static,
{
    /// Create a Scheduler for parallel execution
    pub fn new(
        cfg: CfgEnv,
        env: BlockEnv,
        txs: Arc<Vec<TxEnv>>,
        state: ParallelState<DB>,
        with_hints: bool,
        custom_precompiles: Option<Arc<Vec<(Address, DynPrecompile)>>>,
    ) -> Self {
        Self::new_with_config(
            cfg,
            env,
            txs,
            state,
            with_hints,
            custom_precompiles,
            GrevmConfig::from_env(),
        )
    }

    /// Create a scheduler with an explicit, block-scoped runtime configuration.
    pub fn new_with_config(
        cfg: CfgEnv,
        env: BlockEnv,
        txs: Arc<Vec<TxEnv>>,
        state: ParallelState<DB>,
        with_hints: bool,
        custom_precompiles: Option<Arc<Vec<(Address, DynPrecompile)>>>,
        config: GrevmConfig,
    ) -> Self {
        assert!(config.concurrency_level > 0, "grevm concurrency level must be greater than zero");
        Self::build(cfg, env, txs, state, with_hints, custom_precompiles, config)
    }

    /// Compatibility constructor for callers that only override delegated-account safety.
    #[deprecated(note = "use Scheduler::new_with_config and GrevmConfig")]
    pub fn new_with_delegated_safety(
        cfg: CfgEnv,
        env: BlockEnv,
        txs: Arc<Vec<TxEnv>>,
        state: ParallelState<DB>,
        with_hints: bool,
        custom_precompiles: Option<Arc<Vec<(Address, DynPrecompile)>>>,
        delegated_safety: DelegatedSafetyConfig,
    ) -> Self {
        Self::new_with_config(
            cfg,
            env,
            txs,
            state,
            with_hints,
            custom_precompiles,
            GrevmConfig::from_env().with_delegated_safety(delegated_safety),
        )
    }

    fn build(
        cfg: CfgEnv,
        env: BlockEnv,
        txs: Arc<Vec<TxEnv>>,
        state: ParallelState<DB>,
        with_hints: bool,
        custom_precompiles: Option<Arc<Vec<(Address, DynPrecompile)>>>,
        config: GrevmConfig,
    ) -> Self {
        let num_txs = txs.len();
        let tx_dependency = if with_hints {
            ParallelExecutionHints::new(txs.clone()).parse_hints()
        } else {
            TxDependency::new(num_txs)
        };
        // Construction is O(1): sender indexing and per-account maximum-cost suffixes remain lazy
        // until surviving delegated execution actually debits an account.
        let reserve_planner = config
            .delegated_safety
            .reserve_delegated_balance
            .then(|| Arc::new(ReservePlanner::new(txs.clone())));
        Self {
            cfg,
            env,
            block_size: num_txs,
            txs,
            state: Mutex::new(state),
            results: Mutex::new(vec![]),
            tx_states: (0..num_txs).map(|_| Mutex::new(TxState::default())).collect(),
            tx_results: (0..num_txs).map(|_| Mutex::new(None)).collect(),
            tx_dependency,
            mv_memory: MVMemory::new(),
            scheduler_ctx: SchedulerContext::new(num_txs),
            custom_precompiles: custom_precompiles.unwrap_or_else(|| Arc::new(Vec::new())),
            config,
            reserve_planner,
            abort: AtomicBool::new(false),
            abort_reason: OnceLock::new(),
            finality_wait: WaitSlot::new(),
            commit_wait: WaitSlot::new(),
            metrics: ExecuteMetricsCollector::default(),
        }
    }

    fn async_finality(&self) {
        self.finality_wait.register_current_thread();
        let mut last_progress = Instant::now();
        let mut finality_idx = 0;
        let mut lower_ts = 0;
        let dependency_distance = histogram!("grevm.dependency_distance");
        while !self.is_aborted() && finality_idx < self.block_size {
            let previous_finality_idx = finality_idx;
            while let Some((mut tx_state, effective_lower_ts)) =
                self.lock_finality_candidate(finality_idx, lower_ts)
            {
                lower_ts = effective_lower_ts;
                let incarnation = tx_state.incarnation;
                let dependency = tx_state.dependency;
                tx_state.status = TransactionStatus::Finality;
                drop(tx_state);

                let next_finality_idx = finality_idx + 1;
                self.scheduler_ctx.publish_finality(next_finality_idx);
                if finality_idx == previous_finality_idx {
                    // Start commit as soon as the first transaction in this batch is visible.
                    self.commit_wait.notify();
                }

                if incarnation > 1 {
                    self.metrics.conflict_txs.increment();
                }
                if let Some(dep_id) = dependency {
                    dependency_distance.record((finality_idx - dep_id) as f64);
                    if incarnation == 1 {
                        self.metrics.one_attempt_with_dependency.increment();
                    } else if incarnation > 2 {
                        self.metrics.more_attempts_with_dependency.increment();
                    }
                } else {
                    self.metrics.no_dependency_txs.increment();
                }
                finality_idx = next_finality_idx;
            }
            let progressed = finality_idx > previous_finality_idx;
            if progressed {
                last_progress = Instant::now();
                if finality_idx - previous_finality_idx > 1 {
                    // Commit may have caught the first notification while this batch was still
                    // publishing. Wake it once more for the completed suffix.
                    self.commit_wait.notify();
                }
                thread::yield_now();
            } else {
                self.finality_wait.wait_while(STALL_TIMEOUT, || {
                    !self.is_aborted() &&
                        self.lock_finality_candidate(finality_idx, lower_ts).is_none()
                });
            }

            if last_progress.elapsed() > STALL_TIMEOUT {
                last_progress = Instant::now();
                tracing::warn!(
                    target: "grevm::scheduler",
                    block_number = %self.env.number,
                    finality_idx = self.scheduler_ctx.finality_idx(),
                    validation_idx = self.scheduler_ctx.validation_idx(),
                    execution_idx = self.scheduler_ctx.execution_frontier(),
                    "parallel execution stuck",
                );
            }
        }
    }

    fn lock_finality_candidate(
        &self,
        finality_idx: usize,
        lower_ts: usize,
    ) -> Option<(MutexGuard<'_, TxState>, usize)> {
        if finality_idx >= self.block_size || finality_idx >= self.scheduler_ctx.validation_idx() {
            return None;
        }
        let tx_state = self.tx_states[finality_idx].lock();
        if tx_state.status != TransactionStatus::Unconfirmed {
            return None;
        }

        // Rolling back validation makes this and every later finality timestamp logically newer.
        let effective_lower_ts = max(lower_ts, self.scheduler_ctx.lower_timestamp(finality_idx));
        (self.scheduler_ctx.unconfirmed_timestamp(finality_idx) > effective_lower_ts)
            .then_some((tx_state, effective_lower_ts))
    }

    fn async_commit(&self, committer: &mut StateAsyncCommit<DB>) {
        self.commit_wait.register_current_thread();
        let mut commit_idx = 0;
        while !self.is_aborted() && commit_idx < self.block_size {
            let previous_commit_idx = commit_idx;
            while commit_idx < self.scheduler_ctx.finality_idx() {
                let Some(tx_result) = self.tx_results[commit_idx].lock().take() else {
                    self.abort(AbortReason::ParallelError {
                        txid: commit_idx,
                        message: "finalized transaction has no execution result",
                    });
                    return;
                };
                let Ok(result) = tx_result.execute_result else {
                    // A transaction with an EVM error must never reach finality. This is a
                    // parallel scheduler inconsistency, so replay it from the committed state
                    // instead of trusting the speculative result.
                    self.abort(AbortReason::ParallelError {
                        txid: commit_idx,
                        message: "failed transaction reached commit",
                    });
                    return;
                };
                let commit_start = Instant::now();
                let fallback = committer.commit(commit_idx, &self.txs[commit_idx], result);
                self.metrics.commit_time.add(commit_start.elapsed().as_nanos() as usize);
                if let Err(error) = committer.commit_result() {
                    // Commit errors do not live in `tx_results`. Keep the complete error in both
                    // `StateAsyncCommit::commit_result` and the abort reason so either
                    // error-handling path can return the correct txid and source error.
                    self.abort(AbortReason::CommitError(error.clone()));
                    return;
                }
                if fallback {
                    // `commit` deliberately leaves the problematic transaction uncommitted. Keep
                    // the committed-prefix boundary at `commit_idx` so sequential fallback
                    // revalidates this transaction before processing the suffix.
                    self.abort(AbortReason::FallbackSequential);
                    return;
                }
                let next_commit_idx = commit_idx + 1;
                self.scheduler_ctx.publish_commit(next_commit_idx);
                self.tx_dependency.commit(commit_idx);
                commit_idx = next_commit_idx;
            }
            if commit_idx > previous_commit_idx {
                thread::yield_now();
            } else {
                self.commit_wait.wait_while(STALL_TIMEOUT, || {
                    !self.is_aborted() && commit_idx >= self.scheduler_ctx.finality_idx()
                });
            }
        }
    }

    /// Take transaction outcomes and `ParallelState`.
    pub fn take_result_and_state(self) -> (Vec<TxExecutionOutcome>, ParallelState<DB>) {
        (self.results.into_inner(), self.state.into_inner())
    }

    /// Execute using the scheduler's unified runtime configuration.
    pub fn execute(&self) -> Result<(), GrevmError<DB::Error>> {
        self.parallel_execute(None)
    }

    /// Execute with an optional per-call concurrency override.
    ///
    /// New integrations should configure [`GrevmConfig::concurrency_level`] and call
    /// [`Self::execute`].
    pub fn parallel_execute(
        &self,
        concurrency_level: Option<usize>,
    ) -> Result<(), GrevmError<DB::Error>> {
        let start_time = Instant::now();
        self.metrics.total_tx_cnt.set(self.block_size);
        let concurrency_level = concurrency_level.unwrap_or(self.config.concurrency_level);
        assert!(concurrency_level > 0, "grevm concurrency level must be greater than zero");
        if self.config.force_sequential || self.block_size < self.config.min_parallel_txs {
            return self.fallback_sequential();
        }
        let (commit_result, committed_results) = {
            // This lock protects Scheduler's block-level ownership, not transaction processing. It
            // is acquired once and held while safe, disjoint field borrows are used by workers and
            // ordered commit.
            let mut state = self.state.lock();
            let (state_view, commit_state) = state.split_for_parallel();
            let mut committer = StateAsyncCommit::new(
                self.env.beneficiary,
                self.cfg.spec,
                self.env.basefee,
                commit_state,
                self.cfg.disable_nonce_check,
            );
            committer.init().map_err(|e| GrevmError { txid: 0, error: EVMError::Database(e) })?;
            thread::scope(|scope| {
                scope.spawn(|| {
                    self.async_finality();
                    self.metrics.execution_time.set(start_time.elapsed().as_nanos() as usize);
                });
                scope.spawn(|| {
                    self.async_commit(&mut committer);
                });
                for _ in 0..concurrency_level {
                    scope.spawn(|| {
                        let cache_db = CacheDB::new(
                            self.cfg.spec,
                            self.env.beneficiary,
                            &state_view,
                            &self.mv_memory,
                            self.scheduler_ctx.commit_cursor(),
                        );
                        let mut cfg = self.cfg.clone();
                        // Disable nonce checks during speculative execution. The commit thread
                        // checks the nonce against committed state; a mismatch leaves the
                        // transaction uncommitted and triggers sequential revalidation from that
                        // transaction.
                        cfg.disable_nonce_check = true;
                        let mut executor = GrevmExecutor::new(
                            cache_db,
                            cfg,
                            self.env.clone(),
                            self.custom_precompiles.as_ref(),
                            self.config.delegated_safety,
                            self.reserve_planner.clone(),
                        );
                        self.run_worker(&mut executor);
                    });
                }
            });
            (committer.commit_result().clone(), committer.take_result())
        };
        // Return fatal commit errors. Transaction-validation issues discovered while committing
        // request sequential fallback without populating `commit_result`.
        commit_result?;
        if !committed_results.is_empty() {
            self.results.lock().extend(committed_results);
        }
        // Return error if execution failed
        self.post_execute()?;
        self.metrics.reset_validation_idx_cnt.set(self.scheduler_ctx.validation_reset_count());
        self.metrics.total_time.set(start_time.elapsed().as_nanos() as usize);
        self.metrics.report();
        Ok(())
    }

    fn post_execute(&self) -> Result<(), GrevmError<DB::Error>> {
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
                        *txid,
                        "fatal execution abort has no matching transaction error",
                    );
                }
                // `parallel_execute` normally returns `commit_result` before reaching this branch.
                // Keeping the exact error here makes `post_execute` correct on its own as well.
                Some(AbortReason::CommitError(error)) => return Err(error.clone()),
                Some(AbortReason::ParallelError { txid, message }) => {
                    return self.fallback_after_parallel_error(*txid, message);
                }
                // Grevm maintains full compatibility with self-destruct operations while
                // preserving the ability to fall back to sequential execution when necessary.
                // Although this code path remains theoretically unreachable in normal
                // operation, we deliberately retain it as a safeguard. Notably, Grevm
                // implements an optimized rollback mechanism - when parallel execution fails,
                // the system can resume sequential processing from the problematic transaction
                // rather than restarting the entire block. This represents a significant
                // optimization for rare edge cases, effectively preventing severe performance
                // degradation that could otherwise drastically slow down parallel execution
                // throughput.
                Some(AbortReason::SelfDestructed | AbortReason::FallbackSequential) => {
                    return self.fallback_sequential();
                }
                None => {
                    return self.fallback_after_parallel_error(
                        self.scheduler_ctx.committed_idx(),
                        "parallel execution aborted without a reason",
                    );
                }
            }
        }
        Ok(())
    }

    fn abort(&self, abort_reason: AbortReason<DB::Error>) {
        self.abort_reason.get_or_init(|| abort_reason);
        self.abort.store(true, Ordering::Release);
        self.finality_wait.notify();
        self.commit_wait.notify();
    }

    #[inline]
    fn is_aborted(&self) -> bool {
        self.abort.load(Ordering::Acquire)
    }

    /// After execution, transactions are marked as conflict status in three scenarios:
    /// ​- EVM Execution Failure: The transaction fails during EVM processing
    /// - ​Read Estimate Data: The transaction accesses uncommitted state estimates
    /// - ​Unconfirmed Miner/Self-Destruct Accounts: The transaction interacts with miner rewards or
    ///   self-destructed accounts before their committing transaction is finalized (txid ≠
    ///   commit_idx)
    fn run_worker<'db, WorkerDB>(
        &self,
        executor: &mut impl ParallelTransactionExecutor<'db, WorkerDB>,
    ) where
        WorkerDB: DatabaseRef<Error = DB::Error> + 'db,
    {
        let mut task = self.next();
        while let Some(current_task) = task {
            task = match current_task {
                Task::Execution(version) => self.execute_task(executor, version),
                Task::Validation(version) => self.validate(version),
            };
            if task.is_none() && !self.is_aborted() {
                task = self.next();
            }
        }
    }

    fn execute_task<'db, WorkerDB>(
        &self,
        executor: &mut impl ParallelTransactionExecutor<'db, WorkerDB>,
        tx_version: TxVersion,
    ) -> Option<Task>
    where
        WorkerDB: DatabaseRef<Error = DB::Error> + 'db,
    {
        let TxVersion { txid, incarnation } = tx_version.clone();
        let mut tx_state = self.tx_states[txid].lock();
        if tx_state.status != TransactionStatus::Executing {
            return None;
        }
        if tx_state.incarnation != incarnation {
            self.abort(AbortReason::ParallelError {
                txid,
                message: "inconsistent incarnation during execution",
            });
            return None;
        }
        self.metrics.execution_cnt.increment();

        let tx_env = self.txs[txid].clone();
        let commit_idx = self.scheduler_ctx.committed_idx();
        let result = executor.transact(tx_version, tx_env);

        // The `​write_new_locations` mechanism optimizes validation by intelligently reducing
        // redundant verification tasks. Under standard validation logic, when a conflicted
        // transaction is re-executed, all subsequent transactions must undergo revalidation.
        // However, if the re-executed transaction hasn't written to any new storage locations (as
        // tracked by write_new_locations), subsequent transactions can skip this revalidation
        // process. This optimization significantly decreases the total number of required
        // validation tasks.
        let mut write_new_locations = false;
        let conflict;
        let mut next = None;
        match result {
            Ok(result_and_state) => {
                // only the miner involved in transaction should accumulate the rewards of finality
                // txs return true if the tx doesn't visit the miner account
                let read_accurate_origin = executor.db_mut().read_accurate_origin();

                let blocking_txs = executor.db_mut().take_estimate_txs();
                conflict = !read_accurate_origin || !blocking_txs.is_empty();
                let read_set = executor.db_mut().take_read_set();
                let write_set =
                    executor.db_mut().update_mv_memory(&result_and_state.state, conflict);

                let mut last_result = self.tx_results[txid].lock();
                if let Some(last_result) = last_result.as_ref() {
                    for location in write_set.iter() {
                        if !last_result.write_set.contains(location) {
                            write_new_locations = true;
                            break;
                        }
                    }
                    for location in &last_result.write_set {
                        if !write_set.contains(location) &&
                            let Some(mut written_transactions) = self.mv_memory.get_mut(location)
                        {
                            written_transactions.remove(&txid);
                        }
                    }
                } else {
                    write_new_locations = true;
                }

                if conflict {
                    self.metrics.conflict_cnt.increment();
                    if !read_accurate_origin {
                        self.metrics.conflict_by_miner.increment();
                        // Add all previous transactions as dependencies if miner doesn't accumulate
                        // the rewards
                        self.tx_dependency.key_tx(txid, self.scheduler_ctx.commit_cursor());
                    } else {
                        self.metrics.conflict_by_estimate.increment();
                        self.tx_dependency.add(txid, self.generate_dependent_tx(txid, &read_set));
                    }
                } else {
                    // Grevm employs an optimized thread scheduling strategy that differs
                    // fundamentally from Block-STM's approach while intelligently preserving its
                    // advantages. Unlike Block-STM where conflicted transactions persistently
                    // occupy threads through busy-waiting retries, Grevm normally yields the thread
                    // and re-schedules via DAG - except in critical path scenarios where it
                    // demonstrates adaptive behavior. When detecting strictly linear dependencies
                    // (where the next transaction immediately depends on the current one), Grevm
                    // makes a crucial optimization: it maintains thread continuity by directly
                    // executing the dependent transaction within the same thread rather than
                    // yielding. This hybrid approach combines the general efficiency of DAG-based
                    // scheduling for parallelizable workloads with Block-STM's optimal performance
                    // for sequential dependency chains, effectively minimizing both thread
                    // contention and scheduling overhead. The system automatically applies the most
                    // appropriate execution strategy based on real-time dependency analysis,
                    // ensuring neither purely optimistic (Block-STM) nor purely DAG-driven
                    // approaches impose unnecessary performance penalties in their respective
                    // worst-case scenarios.
                    next = self.tx_dependency.remove(txid, true);
                }
                *last_result = Some(TransactionResult {
                    read_set,
                    write_set,
                    execute_result: Ok(result_and_state),
                });
            }
            Err(e) => {
                let invalid_transaction = matches!(e, EVMError::Transaction(_));
                conflict = true;
                self.metrics.conflict_cnt.increment();
                self.metrics.conflict_by_error.increment();
                let mut write_set = HashSet::new();

                let mut last_result = self.tx_results[txid].lock();
                if let Some(last_result) = last_result.as_mut() {
                    write_set = std::mem::take(&mut last_result.write_set);
                    self.mark_estimate(txid, &write_set);
                }
                *last_result = Some(TransactionResult {
                    read_set: Default::default(),
                    write_set,
                    execute_result: Err(e),
                });
                if commit_idx == txid {
                    if invalid_transaction {
                        self.abort(AbortReason::FallbackSequential);
                    } else {
                        self.abort(AbortReason::FatalEvmError(txid));
                    }
                }
                self.tx_dependency.key_tx(txid, self.scheduler_ctx.commit_cursor());
            }
        }

        tx_state.status =
            if conflict { TransactionStatus::Conflict } else { TransactionStatus::Executed };
        self.scheduler_ctx.executed(txid);

        if let Some(next) = next {
            self.scheduler_ctx.rewind_validation_to(txid);
            drop(tx_state);
            return self.execution_task(next);
        }
        if conflict {
            self.scheduler_ctx.rewind_validation_to(txid + 1);
        } else {
            if write_new_locations {
                self.scheduler_ctx.rewind_validation_to(txid);
            } else {
                tx_state.status = TransactionStatus::Validating;
                return Some(Task::Validation(TxVersion::new(txid, incarnation)));
            }
        }
        None
    }

    fn validate(&self, tx_version: TxVersion) -> Option<Task> {
        let TxVersion { txid, incarnation } = tx_version;
        let mut tx_state = self.tx_states[txid].lock();
        let tx_result = self.tx_results[txid].lock();
        if tx_state.status != TransactionStatus::Validating {
            return None;
        }
        if tx_state.incarnation != incarnation {
            self.abort(AbortReason::ParallelError {
                txid,
                message: "inconsistent incarnation during validation",
            });
            return None;
        }
        self.metrics.validation_cnt.increment();
        let Some(result) = tx_result.as_ref() else {
            self.abort(AbortReason::ParallelError {
                txid,
                message: "transaction has no result during validation",
            });
            return None;
        };
        if result.execute_result.is_err() {
            self.abort(AbortReason::ParallelError {
                txid,
                message: "failed transaction reached validation",
            });
            return None;
        }

        let ts = self.scheduler_ctx.logical_timestamp();
        // check the read version of read set
        let mut conflict = false;
        let mut dependency: Option<TxId> = None;
        for (location, version) in result.read_set.iter() {
            if let Some(written_transactions) = self.mv_memory.get(location) {
                if let Some((&previous_id, latest_version)) =
                    written_transactions.range(..txid).next_back()
                {
                    dependency = Some(dependency.map_or(previous_id, |d| max(d, previous_id)));
                    if latest_version.estimate {
                        conflict = true;
                    } else if let ReadVersion::MvMemory(version) = version {
                        if version.txid != previous_id ||
                            version.incarnation != latest_version.incarnation
                        {
                            conflict = true;
                        }
                    } else {
                        conflict = true;
                    }
                } else if !matches!(version, ReadVersion::Storage) {
                    conflict = true;
                }
            } else if !matches!(version, ReadVersion::Storage) {
                conflict = true;
            }
        }
        if conflict {
            self.metrics.conflict_cnt.increment();
            self.metrics.conflict_by_version.increment();
            // mark write set as estimate
            self.mark_estimate(txid, &result.write_set);
        }

        // update transaction status
        tx_state.status = if conflict {
            self.scheduler_ctx.rewind_validation_to(txid + 1);
            TransactionStatus::Conflict
        } else {
            self.scheduler_ctx.unconfirmed(txid, ts);
            TransactionStatus::Unconfirmed
        };
        tx_state.dependency = dependency;

        if conflict {
            // update dependency
            let dep_tx = dependency.filter(|&dep| dep >= self.scheduler_ctx.finality_idx());
            self.tx_dependency.add(txid, dep_tx);
        }
        drop(tx_result);
        drop(tx_state);
        if txid == self.scheduler_ctx.finality_idx() {
            self.finality_wait.notify();
        }
        None
    }

    fn mark_estimate(&self, txid: TxId, write_set: &HashSet<LocationAndType>) {
        for location in write_set {
            if let Some(mut written_transactions) = self.mv_memory.get_mut(location) &&
                let Some(entry) = written_transactions.get_mut(&txid)
            {
                entry.estimate = true;
            }
        }
    }

    fn generate_dependent_tx(
        &self,
        txid: TxId,
        read_set: &HashMap<LocationAndType, ReadVersion>,
    ) -> Option<TxId> {
        let mut max_dep_id = None;
        for location in read_set.keys() {
            if let Some(written_transactions) = self.mv_memory.get(location) &&
                let Some((&dep_id, _)) = written_transactions.range(..txid).next_back() &&
                max_dep_id.is_none_or(|current| dep_id > current) &&
                dep_id >= self.scheduler_ctx.finality_idx()
            {
                // To prevent dependency explosion, keep only the highest preceding transaction.
                max_dep_id = Some(dep_id);
                if dep_id == txid - 1 {
                    return max_dep_id;
                }
            }
        }
        max_dep_id
    }

    fn execution_task(&self, execute_id: TxId) -> Option<Task> {
        let mut tx = self.tx_states[execute_id].lock();
        if matches!(tx.status, TransactionStatus::Initial | TransactionStatus::Conflict) {
            tx.status = TransactionStatus::Executing;
            tx.incarnation += 1;
            Some(Task::Execution(TxVersion::new(execute_id, tx.incarnation)))
        } else {
            self.tx_dependency.remove(execute_id, false);
            self.metrics.useless_dependent_update.increment();
            None
        }
    }

    fn next(&self) -> Option<Task> {
        while !self.scheduler_ctx.finished() && !self.is_aborted() {
            if !self.scheduler_ctx.should_schedule(self.tx_dependency.index()) {
                thread::yield_now();
            }

            if let Some(validation_idx) =
                self.scheduler_ctx.next_validation_idx(self.tx_dependency.index())
            {
                let mut tx = self.tx_states[validation_idx].lock();
                match tx.status {
                    TransactionStatus::Executed | TransactionStatus::Unconfirmed => {
                        tx.status = TransactionStatus::Validating;
                        return Some(Task::Validation(TxVersion::new(
                            validation_idx,
                            tx.incarnation,
                        )));
                    }
                    _ => {}
                }
            }

            if let Some(execute_id) = self.tx_dependency.next() &&
                let Some(task) = self.execution_task(execute_id)
            {
                return Some(task);
            }
        }
        None
    }
}
