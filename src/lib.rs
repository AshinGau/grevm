//! # Grevm
//!
//! Grevm is a high-performance, parallelized Ethereum Virtual Machine (EVM) inspired by BlockSTM
//! designed to handle concurrent transaction execution and validation. It provides utilities for
//! managing transaction states, dependencies, and memory, while leveraging multi-threading to
//! maximize throughput.
//!
//! ## Concurrency
//!
//! By default, all schedulers share an [`ExecutionResources`] budget capped at the logical
//! parallelism reported by [`std::thread::available_parallelism`] (falling back to one). A parallel
//! batch reserves two coordinator roles and uses the remaining allocation for speculative workers;
//! fewer than four available roles selects sequential execution. Integrations can set the worker
//! upper bound through [`SchedulerTuning::concurrency_level`] and combine that tuning with a named
//! [`ExecutionProfile`] when constructing [`GrevmConfig`].
//!
//! ## Error Handling
//!
//! Errors during execution are encapsulated in the `GrevmError` type, which includes the
//! transaction ID and the underlying EVM error. This allows for precise debugging and error
//! reporting.
mod account;
mod beneficiary;
mod concurrent_db;
mod config;
mod delegated_safety;
mod execution_resources;
mod incarnation_db;
mod model;
mod outcome;
mod parallel_state;
mod precompile;
mod scheduler;
#[cfg(feature = "test-utils")]
pub mod test_utils;
mod tx_dependency;

pub(crate) use model::{
    AbortReason, AccountBasic, LocationAndType, MVMemory, MemoryEntry, MemoryValue, ReadVersion,
    Task, TransactionResult, TransactionStatus, TxId, TxState, TxVersion,
};

pub use concurrent_db::{ConcurrentDatabase, DatabaseFactory, ReadCache};
pub use config::{
    ExecutionProfile, GrevmConfig, GrevmConfigError, InvalidTransactionPolicy, SchedulerTuning,
};
pub use delegated_safety::DelegatedSafetyConfig;
pub use execution_resources::ExecutionResources;
pub use outcome::{GrevmError, TxExecutionOutcome};
pub use parallel_state::{ParallelCacheState, ParallelState};
pub use precompile::{
    DynParallelPrecompile, ParallelPrecompile, ParallelPrecompileError, ParallelPrecompileInput,
    ParallelPrecompileResult, ParallelPrecompileState,
};
pub use revm_context::result::InvalidTransaction;
pub use scheduler::{
    Scheduler,
    session::{BatchExecutionResult, ExecutionSession},
};
