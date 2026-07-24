mod context;
mod control;
mod executor;
mod fallback;
mod metrics;
mod ordered_commit;
#[cfg(test)]
mod tests;
mod wait;

use crate::{
    AbortReason, GrevmConfig, GrevmError, LocationAndType, MVMemory, ParallelState, ReadVersion,
    Task, TransactionResult, TransactionStatus, TxExecutionOutcome, TxId, TxState, TxVersion,
    cache_db::CacheDB,
    delegated_safety::{DelegatedSafetyConfig, ReservePlanner},
    tx_dependency::TxDependency,
};
use ahash::{AHashMap as HashMap, AHashSet as HashSet};
use alloy_evm::precompiles::DynPrecompile;
use context::SchedulerContext;
use control::ExecutionLifecycle;
use executor::{GrevmExecutor, ParallelTransactionExecutor};
use metrics::ExecuteMetricsCollector;
use ordered_commit::{CommitOutcome, CommittedPrefixEnd, OrderedCommitOutput, OrderedCommitter};
use parking_lot::{Mutex, MutexGuard};
use revm::DatabaseRef;
use revm_context::{BlockEnv, CfgEnv, TxEnv, result::EVMError};
use revm_primitives::Address;

use std::{
    cmp::max,
    fmt::Debug,
    sync::{Arc, OnceLock, atomic::AtomicBool},
    thread,
    time::{Duration, Instant},
};
use wait::WaitSlot;

const STALL_TIMEOUT: Duration = Duration::from_secs(8);

struct CommitLoopResult<DBError> {
    committed: OrderedCommitOutput,
    error: Option<GrevmError<DBError>>,
}

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

    execution: ExecutionLifecycle,
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
    /// Create a scheduler using the legacy environment-based runtime configuration.
    ///
    /// `legacy_with_hints` is retained for source compatibility and ignored. Static hint inference
    /// has been removed; dependencies are discovered from speculative reads and writes.
    pub fn new(
        cfg: CfgEnv,
        env: BlockEnv,
        txs: Arc<Vec<TxEnv>>,
        state: ParallelState<DB>,
        legacy_with_hints: bool,
        custom_precompiles: Option<Arc<Vec<(Address, DynPrecompile)>>>,
    ) -> Self {
        let _ = legacy_with_hints;
        Self::new_with_runtime_config(
            cfg,
            env,
            txs,
            state,
            custom_precompiles,
            GrevmConfig::from_env(),
        )
    }

    /// Create a scheduler with an explicit, block-scoped Grevm runtime configuration.
    ///
    /// # Panics
    ///
    /// Panics if [`GrevmConfig::concurrency_level`] is zero.
    pub fn new_with_runtime_config(
        cfg: CfgEnv,
        env: BlockEnv,
        txs: Arc<Vec<TxEnv>>,
        state: ParallelState<DB>,
        custom_precompiles: Option<Arc<Vec<(Address, DynPrecompile)>>>,
        config: GrevmConfig,
    ) -> Self {
        assert!(config.concurrency_level > 0, "grevm concurrency level must be greater than zero");
        Self::build(cfg, env, txs, state, custom_precompiles, config)
    }

    /// Compatibility constructor retaining the legacy static-hints argument.
    ///
    /// `legacy_with_hints` is retained for source compatibility. Static hint inference was removed;
    /// dependencies are discovered from speculative reads and writes during execution.
    pub fn new_with_config(
        cfg: CfgEnv,
        env: BlockEnv,
        txs: Arc<Vec<TxEnv>>,
        state: ParallelState<DB>,
        legacy_with_hints: bool,
        custom_precompiles: Option<Arc<Vec<(Address, DynPrecompile)>>>,
        config: GrevmConfig,
    ) -> Self {
        let _ = legacy_with_hints;
        Self::new_with_runtime_config(cfg, env, txs, state, custom_precompiles, config)
    }

    /// Compatibility constructor for callers that only override delegated-account safety.
    #[deprecated(note = "use Scheduler::new_with_runtime_config and GrevmConfig")]
    pub fn new_with_delegated_safety(
        cfg: CfgEnv,
        env: BlockEnv,
        txs: Arc<Vec<TxEnv>>,
        state: ParallelState<DB>,
        legacy_with_hints: bool,
        custom_precompiles: Option<Arc<Vec<(Address, DynPrecompile)>>>,
        delegated_safety: DelegatedSafetyConfig,
    ) -> Self {
        let _ = legacy_with_hints;
        Self::new_with_runtime_config(
            cfg,
            env,
            txs,
            state,
            custom_precompiles,
            GrevmConfig::from_env().with_delegated_safety(delegated_safety),
        )
    }

    fn build(
        cfg: CfgEnv,
        env: BlockEnv,
        txs: Arc<Vec<TxEnv>>,
        state: ParallelState<DB>,
        custom_precompiles: Option<Arc<Vec<(Address, DynPrecompile)>>>,
        config: GrevmConfig,
    ) -> Self {
        let num_txs = txs.len();
        // Reserve-planner construction is O(1): sender indexing and per-account maximum-cost
        // suffixes remain lazy until surviving delegated execution actually debits an account.
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
            tx_dependency: TxDependency::new(num_txs),
            mv_memory: MVMemory::new(),
            scheduler_ctx: SchedulerContext::new(num_txs),
            custom_precompiles: custom_precompiles.unwrap_or_else(|| Arc::new(Vec::new())),
            config,
            reserve_planner,
            execution: ExecutionLifecycle::new(),
            abort: AtomicBool::new(false),
            abort_reason: OnceLock::new(),
            finality_wait: WaitSlot::new(),
            commit_wait: WaitSlot::new(),
            metrics: ExecuteMetricsCollector::default(),
        }
    }

    fn run_finality_loop(&self) {
        self.finality_wait.register_current_thread();
        let mut last_progress = Instant::now();
        let mut finality_idx = 0;
        let mut lower_ts = 0;
        let dependency_distance = self.metrics.dependency_distance_histogram();
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

                self.metrics.record_finalized(incarnation, dependency.is_some());
                if let Some(dep_id) = dependency {
                    dependency_distance.record((finality_idx - dep_id) as f64);
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

    fn run_commit_loop(&self, committer: &mut OrderedCommitter<DB>) -> CommitLoopResult<DB::Error> {
        self.commit_wait.register_current_thread();
        let mut output = OrderedCommitOutput::with_capacity(self.block_size);
        let mut commit_idx = 0;
        while !self.is_aborted() && commit_idx < self.block_size {
            let previous_commit_idx = commit_idx;
            while commit_idx < self.scheduler_ctx.finality_idx() {
                let Some(tx_result) = self.tx_results[commit_idx].lock().take() else {
                    self.abort(AbortReason::ParallelError {
                        txid: commit_idx,
                        message: "finalized transaction has no execution result",
                    });
                    return CommitLoopResult { committed: output, error: None };
                };
                let Ok(result) = tx_result.execute_result else {
                    // A transaction with an EVM error must never reach finality. This is a
                    // parallel scheduler inconsistency, so replay it from the committed state
                    // instead of trusting the speculative result.
                    self.abort(AbortReason::ParallelError {
                        txid: commit_idx,
                        message: "failed transaction reached commit",
                    });
                    return CommitLoopResult { committed: output, error: None };
                };
                let commit_start = Instant::now();
                let outcome =
                    committer.commit(commit_idx, &self.txs[commit_idx], result, &mut output);
                self.metrics.record_commit_time(commit_start.elapsed());
                match outcome {
                    Ok(CommitOutcome::Committed(committed)) => {
                        let next_commit_idx = committed.index();
                        self.scheduler_ctx.publish_commit(next_commit_idx);
                        self.tx_dependency.commit(commit_idx);
                        commit_idx = next_commit_idx;
                    }
                    Ok(CommitOutcome::NeedsSequentialFallback) => {
                        // The problematic transaction remains uncommitted. Keep the cursor at its
                        // index so sequential fallback revalidates it before processing the suffix.
                        self.abort(AbortReason::FallbackSequential);
                        return CommitLoopResult { committed: output, error: None };
                    }
                    Err(error) => {
                        // Wake every scheduler thread immediately, while also returning the exact
                        // txid and database error directly to the scoped-thread caller.
                        self.abort(AbortReason::CommitError(error.clone()));
                        return CommitLoopResult { committed: output, error: Some(error) };
                    }
                }
            }
            if commit_idx > previous_commit_idx {
                thread::yield_now();
            } else {
                self.commit_wait.wait_while(STALL_TIMEOUT, || {
                    !self.is_aborted() && commit_idx >= self.scheduler_ctx.finality_idx()
                });
            }
        }
        CommitLoopResult { committed: output, error: None }
    }

    fn install_commit_loop_result(
        &self,
        result: CommitLoopResult<DB::Error>,
    ) -> Result<CommittedPrefixEnd, GrevmError<DB::Error>> {
        let CommitLoopResult { committed: output, error } = result;
        let committed = output.end();
        assert_eq!(
            committed.index(),
            self.scheduler_ctx.committed_idx(),
            "ordered output and published commit cursor must describe the same prefix",
        );
        let mut results = self.results.lock();
        assert!(results.is_empty(), "ordered commit outcomes may only be installed once");
        *results = output.into_outcomes();
        drop(results);
        error.map_or(Ok(committed), Err)
    }

    fn parallel_execute_inner(
        &self,
        concurrency_level: usize,
        start_time: Instant,
    ) -> Result<(), GrevmError<DB::Error>> {
        if self.config.force_sequential || self.block_size < self.config.min_parallel_txs {
            return self.replay_uncommitted_suffix(CommittedPrefixEnd::ZERO);
        }
        let commit_thread_result = {
            // This lock protects Scheduler's block-level ownership, not transaction processing. It
            // is acquired once and held while safe, disjoint field borrows are used by workers and
            // ordered commit.
            let mut state = self.state.lock();
            let (state_view, commit_state) = state.split_for_parallel();
            let mut committer = OrderedCommitter::try_new(
                self.env.beneficiary,
                self.cfg.spec,
                self.env.basefee,
                commit_state,
                self.cfg.disable_nonce_check,
            )
            .map_err(|e| GrevmError { txid: 0, error: EVMError::Database(e) })?;
            thread::scope(|scope| {
                scope.spawn(|| {
                    self.run_finality_loop();
                    self.metrics.record_execution_time(start_time.elapsed());
                });
                let commit_thread = scope.spawn(|| self.run_commit_loop(&mut committer));
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
                commit_thread.join().unwrap_or_else(|panic| std::panic::resume_unwind(panic))
            })
        };
        let committed = self.install_commit_loop_result(commit_thread_result)?;
        // Return error if execution failed
        self.post_execute(committed)?;
        Ok(())
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
        self.metrics.record_execution_attempt();

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
                    if !read_accurate_origin {
                        self.metrics.record_coinbase_conflict();
                        // Add all previous transactions as dependencies if miner doesn't accumulate
                        // the rewards
                        self.tx_dependency.key_tx(txid, self.scheduler_ctx.commit_cursor());
                    } else {
                        self.metrics.record_estimate_conflict();
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
                self.metrics.record_evm_error_conflict();
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
        self.metrics.record_validation_attempt();
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
            self.metrics.record_version_conflict();
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
            self.metrics.record_useless_dependency_update();
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
