# Use Grevm with reth

## Add the dependency

```toml
[dependencies]
grevm = { git = "https://github.com/AshinGau/grevm.git", rev = "<immutable-commit>" }
```

## Standalone usage

Grevm's public surface is small: build a `ParallelState` over any read-only database, hand it to a
`Scheduler` together with the config/block environment and the transactions, then call
`execute`. The database implements revm's read-only `DatabaseRef` trait and is `Send + Sync`; its
error type is `Clone + Send + Sync + 'static`.

```rust
use std::sync::Arc;

use grevm::{
    GrevmConfig, ParallelState, ParallelTakeBundle, Scheduler, SchedulerTuning,
    TxExecutionOutcome,
};
use revm::DatabaseRef;
use revm_context::{BlockEnv, CfgEnv, TxEnv};
use revm_database::states::bundle_state::BundleRetention;

fn execute_block<DB>(cfg: CfgEnv, env: BlockEnv, txs: Vec<TxEnv>, db: DB)
where
    DB: DatabaseRef + Send + Sync + 'static,
    DB::Error: Clone + Send + Sync + 'static,
{
    let db = Arc::new(db);
    let txs = Arc::new(txs);

    // with_bundle_update = true  -> track transitions so we can extract a BundleState afterwards
    // update_db_metrics  = false -> set true to record the `grevm.db_latency_us` metric
    let state = ParallelState::new(db.clone(), true, false);

    // Dependencies are discovered dynamically from speculative reads and writes. Passing an
    // explicit runtime config keeps block execution independent of process environment variables.
    let scheduler = Scheduler::new_with_runtime_config(
        cfg,
        env,
        txs,
        state,
        None, // optional custom precompiles
        GrevmConfig::ethereum_block(SchedulerTuning::default()),
    );

    scheduler.execute().expect("block execution failed");

    let (results, mut state) = scheduler.take_result_and_state();
    let bundle = state.parallel_take_bundle(BundleRetention::Reverts);

    // The safe default rejects invalid fixed-block transactions. `Skipped` is only returned when
    // the caller explicitly selects Omit or IncludeNoop policy.
    for outcome in &results {
        match outcome {
            TxExecutionOutcome::Executed(result) => {
                let _gas_used = result.tx_gas_used();
            }
            TxExecutionOutcome::Skipped(reason) => {
                eprintln!("transaction skipped: {reason:?}");
            }
        }
    }
    // `bundle`:  the `BundleState` to persist to your database.
    let _ = (results, bundle);
}
```

Key signatures:

```rust
use std::sync::Arc;

use grevm::{
    DynParallelPrecompile, ExecutionProfile, GrevmConfig, GrevmError, ParallelState,
    ParallelTakeBundle, Scheduler, SchedulerTuning, TxExecutionOutcome,
};
use revm::DatabaseRef;
use revm_context::{BlockEnv, CfgEnv, TxEnv};
use revm_database::{BundleState, states::bundle_state::BundleRetention};
use revm_primitives::Address;

impl<DB> Scheduler<DB>
where
    DB: DatabaseRef + Send + Sync,
    DB::Error: Clone + Send + Sync + 'static,
{
    pub fn new(
        cfg: CfgEnv,
        env: BlockEnv,
        txs: Arc<Vec<TxEnv>>,
        state: ParallelState<DB>,
        custom_precompiles: Option<Arc<Vec<(Address, DynParallelPrecompile)>>>,
    ) -> Self;

    pub fn new_with_runtime_config(
        cfg: CfgEnv,
        env: BlockEnv,
        txs: Arc<Vec<TxEnv>>,
        state: ParallelState<DB>,
        custom_precompiles: Option<Arc<Vec<(Address, DynParallelPrecompile)>>>,
        config: GrevmConfig,
    ) -> Self;

    pub fn execute(&self) -> Result<(), GrevmError<DB::Error>>;

    pub fn take_result_and_state(self) -> (Vec<TxExecutionOutcome>, ParallelState<DB>);
}

impl<DB: DatabaseRef> ParallelState<DB> {
    pub fn new(database: DB, with_bundle_update: bool, update_db_metrics: bool) -> Self;
    pub const fn database(&self) -> &DB;
    pub const fn database_mut(&mut self) -> &mut DB;
    pub fn into_database(self) -> DB;
}

impl GrevmConfig {
    pub const fn new(tuning: SchedulerTuning, profile: ExecutionProfile) -> Self;
    pub const fn ethereum_builder(tuning: SchedulerTuning) -> Self;
    pub const fn ethereum_block(tuning: SchedulerTuning) -> Self;
    pub const fn gravity(tuning: SchedulerTuning) -> Self;
}

impl<DB: DatabaseRef> ParallelTakeBundle for ParallelState<DB> {
    fn parallel_take_bundle(&mut self, retention: BundleRetention) -> BundleState;
}
```

Public items re-exported from the crate root include `Scheduler`, `ExecutionSession`,
`BatchExecutionResult`, `ConcurrentDatabase`, `DatabaseFactory`, `ReadCache`, `GrevmConfig`,
`SchedulerTuning`, `ExecutionProfile`, `InvalidTransactionPolicy`,
`DelegatedSafetyConfig`, `ParallelState`, `ParallelCacheState`, `TxExecutionOutcome`,
`InvalidTransaction`, `GrevmError`, `ParallelPrecompile`, `DynParallelPrecompile`,
`ParallelPrecompileInput`, `ParallelPrecompileState`, `ParallelPrecompileResult`, and
`ParallelPrecompileError`.
`ParallelBundleState` is the lower-level extension for applying transitions directly to revm's
`BundleState`; `ParallelTakeBundle` finalizes and extracts a block bundle.

## Block-scoped sessions and concurrent databases

Payload builders commonly execute several bounded candidate batches while retaining one block's
state, BAL builder, state hook, and parent-state cache. `ExecutionSession` owns those block-scoped
objects and creates a fresh one-shot scheduler for each batch:

```rust
use grevm::{
    BatchExecutionResult, ExecutionSession, GrevmConfig, ParallelState, SchedulerTuning,
};

let tuning = SchedulerTuning::default();
let state = ParallelState::new(database, true, false);
let mut session = ExecutionSession::new(
    cfg,
    block,
    state,
    None,
    GrevmConfig::ethereum_builder(tuning),
);

match session.execute_batch_with_cancellation(candidates, || cancel.is_interrupted()) {
    BatchExecutionResult::Complete(outcomes) => consume(outcomes),
    BatchExecutionResult::Interrupted { processed_prefix } => {
        // A builder decides whether its external token means hard cancellation (discard) or
        // finalization (consume the prefix and seal). Fixed-block execution always rejects it.
        consume(processed_prefix);
    }
    BatchExecutionResult::Failed { error, .. } => return Err(error.into()),
    BatchExecutionResult::InvariantViolation { error } => return Err(error.into()),
}

let state = session.into_state();
```

The cancellation predicate is borrowed only for the synchronous call. It is invoked concurrently
and repeatedly, so it must be cheap, non-blocking, and thread-safe. Cancellation is cooperative at
scheduler boundaries; it does not preempt an EVM invocation already in progress.

`ConcurrentDatabase` provides one lazily created `DatabaseRef` handle per worker thread and a
shared key-level single-flight `ReadCache`. This is the storage boundary for integrations whose
provider handle is `Send` but not `Sync`:

```rust
use grevm::{ConcurrentDatabase, ReadCache};

let cache = ReadCache::new();
let database = ConcurrentDatabase::with_cache(
    move || open_parent_state_database(),
    cache.clone(),
);
let database_handle = database.clone();

// Build ParallelState / ExecutionSession over `database`.
// After every synchronous batch, all scheduler threads have joined:
database_handle.clear_thread_databases();
```

Successful cache entries can be seeded and exported through `account_reads`, `storage_reads`,
`code_reads`, and `block_hash_reads`. Errors are single-flight within an attempt but are not
exported to the next attempt.

Grevm 3 keeps `ParallelState` internals private. Integrations use its state/BAL/hook/bundle methods,
the controlled `cache()`/`cache_mut()` preload API, and `database()`/`into_database()` instead of
mutating cache maps or transition fields directly. This prevents external code from breaking the
account/storage replacement invariant.

The canonical `new_with_runtime_config` path uses only the supplied `GrevmConfig`.
`Scheduler::new` and explicit `GrevmConfig::from_env()` opt into environment variables
(`GREVM_MIN_PARALLEL_TXS`, `GREVM_FALLBACK_SEQUENTIAL`, `GREVM_CONCURRENT_LEVEL`). See
[Testing & Benchmarking](testing.md#environment-variable-knobs) for the full list and a working
end-to-end harness (`src/test_utils/common/execute.rs`).

## Execution profiles

Scheduler tuning and consensus behavior are separate. Integrations should select one named profile
instead of setting `InvalidTransactionPolicy` and `DelegatedSafetyConfig` independently:

| Constructor | Intended use | Invalid transaction | Delegated guards |
| --- | --- | --- | --- |
| `GrevmConfig::ethereum_builder(tuning)` | Payload builder candidates | Omit | Disabled |
| `GrevmConfig::ethereum_block(tuning)` | Validator and history sync | Abort | Disabled |
| `GrevmConfig::gravity(tuning)` | Gravity execution | Include as no-op | Enabled |

The Ethereum constructors always set both `forbid_delegated_create` and
`reserve_delegated_balance` to `false`, preserving upstream revm semantics independently of JIT or
AOT selection. The legacy field setters remain available for compatibility and specialized
protocol experiments, but new integrations should use a named profile.

## Optional delegated-account policy

`DelegatedSafetyConfig` contains two Grevm/Gravity-specific, opt-in EIP-7702 policies. Both are
disabled by default to preserve stock revm/Ethereum execution semantics. They are automatically
inactive before Prague, so one block-scoped policy configuration can safely be reused while
replaying historical blocks:

- `forbid_delegated_create` makes `CREATE` and `CREATE2` halt as not activated while executing in a
  delegated account's context.
- `reserve_delegated_balance` rolls back transaction execution state when a surviving delegated
  debit would consume funds conservatively reserved for later block transactions. It returns a
  charged top-level revert while retaining the transaction nonce, EIP-7702 authorization effects,
  and authorization refund.

Gravity enables both policies explicitly through its named profile:

```rust
use grevm::{GrevmConfig, SchedulerTuning};

let config = GrevmConfig::gravity(SchedulerTuning::default());
```

`with_delegated_safety` remains available when a Gravity deployment intentionally needs only one
of the two guards.

## Integration with reth

Grevm is integrated into Gravity's reth fork,
[gravity-reth](https://github.com/Galxe/gravity-reth). The
`reth_evm::parallel_execute::ParallelExecutor` trait defines the integration boundary;
`reth_evm_ethereum::parallel_execute::GrevmExecutor` drives block execution through this crate's
`Scheduler`, and `reth-pipe-exec-layer-ext-v2` consumes that interface. Refer to gravity-reth for
the full node wiring; this crate provides the parallel execution engine itself.

## Metrics

Grevm reports execution metrics via the [`metrics`](https://crates.io/crates/metrics) crate (scope
`grevm`). Integrate the [Prometheus exporter](https://crates.io/crates/metrics-exporter-prometheus)
to scrape them. Scheduler metrics below are histograms with one sample per accepted execution
attempt, including attempts that return an execution error. Count fields describe that attempt,
not process-lifetime totals; `execution_time` is omitted on purely sequential paths.

| Metric | Description |
| --- | --- |
| `grevm.total_tx_cnt` | Total number of transactions. |
| `grevm.execution_cnt` | Number of execution incarnations. |
| `grevm.validation_cnt` | Number of validation incarnations. |
| `grevm.conflict_cnt` | Number of conflict incarnations. |
| `grevm.reset_validation_idx_cnt` | Number of validation resets. |
| `grevm.useless_dependent_update` | Number of useless dependency updates. |
| `grevm.conflict_by_miner` | Beneficiary-history reads blocked by an unresolved predecessor (name retained for compatibility). |
| `grevm.conflict_by_error` | Conflicts caused by an EVM error. |
| `grevm.conflict_by_estimate` | Conflicts caused by an estimate (speculative read). |
| `grevm.conflict_by_version` | Conflicts caused by a version mismatch. |
| `grevm.no_dependency_txs` | Transactions executed with no dependency. |
| `grevm.one_attempt_with_dependency` | Dependent transactions finalized on the first incarnation. |
| `grevm.more_attempts_with_dependency` | Dependent transactions needing more than two incarnations. |
| `grevm.conflict_txs` | Number of conflicting transactions. |
| `grevm.execution_time` | Parallel finality-loop duration from block start (nanoseconds; omitted on the sequential path). |
| `grevm.commit_time` | Cumulative ordered-commit attempt time for the block (nanoseconds). |
| `grevm.total_time` | End-to-end scheduler duration, including recovery replay (nanoseconds). |

The following metrics are recorded per event rather than once per block:

| Metric | Kind | Description |
| --- | --- | --- |
| `grevm.dependency_distance` | histogram | Distance from a successfully validated transaction to its latest recorded preceding writer. |
| `grevm.db_latency_us` | histogram | Backing `DatabaseRef` call latency on cache misses, in microseconds; enabled by `ParallelState::new(..., update_db_metrics = true)`. |
| `grevm.reserve_query_count` | counter | Delegated-balance reserve queries. |
| `grevm.reserve_schedule_build_count` | counter | Per-account reserve schedules built lazily. |
| `grevm.reserve_index_build_count` | counter | Lazy sender indexes built. |
| `grevm.reserve_debit_candidates` | counter | Journal debit candidates inspected by reserve protection. |
| `grevm.reserve_schedule_build_time` | histogram | Per-account reserve-schedule build time in nanoseconds. |
| `grevm.reserve_index_build_time` | histogram | Sender-index build time in nanoseconds. |
