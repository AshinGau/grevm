//! Reusable block execution session for ordered transaction batches.

use super::{Scheduler, control::SchedulerExecutionError};
use crate::{DynParallelPrecompile, GrevmConfig, GrevmError, ParallelState, TxExecutionOutcome};
use revm::DatabaseRef;
use revm_context::{BlockEnv, CfgEnv, TxEnv, result::EVMError};
use revm_primitives::Address;
use std::sync::Arc;

/// The canonical result of executing one ordered transaction batch.
///
/// Every non-invariant variant owns the ordered outcomes that match the state retained by
/// [`ExecutionSession`]. A processed prefix can contain [`TxExecutionOutcome::Skipped`]
/// candidates, which do not mutate state (and, under Ethereum builder semantics, do not advance
/// the EIP-7928 index).
#[derive(Debug)]
#[must_use = "batch execution can interrupt or fail after advancing the session state"]
pub enum BatchExecutionResult<DBError> {
    /// Every candidate in the batch reached a final ordered outcome and no interruption was
    /// observed.
    Complete(Vec<TxExecutionOutcome>),
    /// A cancellation check stopped execution after processing a contiguous candidate prefix.
    Interrupted {
        /// Outcomes corresponding exactly to the processed candidate prefix retained by the
        /// session.
        processed_prefix: Vec<TxExecutionOutcome>,
    },
    /// Execution failed after processing a contiguous candidate prefix.
    Failed {
        /// Outcomes corresponding exactly to the processed candidate prefix retained by the
        /// session.
        processed_prefix: Vec<TxExecutionOutcome>,
        /// The error that stopped execution.
        error: GrevmError<DBError>,
    },
    /// The scheduler returned an internally inconsistent outcome count. The session is poisoned
    /// and no longer exposes or accepts state after this result.
    InvariantViolation {
        /// The invariant error that invalidated the session.
        error: GrevmError<DBError>,
    },
}

impl<DBError> BatchExecutionResult<DBError> {
    /// Returns the ordered outcomes represented by the session's current state, or `None` if an
    /// invariant violation poisoned the session.
    pub fn processed_outcomes(&self) -> Option<&[TxExecutionOutcome]> {
        match self {
            Self::Complete(outcomes) |
            Self::Interrupted { processed_prefix: outcomes } |
            Self::Failed { processed_prefix: outcomes, .. } => Some(outcomes),
            Self::InvariantViolation { .. } => None,
        }
    }

    /// Returns whether every candidate reached a final ordered outcome without an interruption.
    pub const fn is_complete(&self) -> bool {
        matches!(self, Self::Complete(_))
    }
}

/// Block-scoped GREVM execution state shared by multiple ordered transaction batches.
///
/// A session owns the canonical [`ParallelState`], EVM environment, scheduler configuration, and
/// custom precompiles for the lifetime of one block. Each [`execute_batch`](Self::execute_batch)
/// invocation creates a fresh one-shot scheduler and then restores its canonical state into the
/// session, so cache, bundle transitions, EIP-7928 state, and state hooks continue across batches.
#[derive(Debug)]
pub struct ExecutionSession<DB>
where
    DB: DatabaseRef,
{
    cfg: CfgEnv,
    block: BlockEnv,
    state: Option<ParallelState<DB>>,
    custom_precompiles: Option<Arc<Vec<(Address, DynParallelPrecompile)>>>,
    config: GrevmConfig,
}

impl<DB> ExecutionSession<DB>
where
    DB: DatabaseRef + Send + Sync,
    DB::Error: Clone + Send + Sync + 'static,
{
    /// Create a block-scoped execution session.
    ///
    /// `custom_precompiles` must satisfy the speculative retry-safety contract documented on
    /// [`Scheduler::new`].
    ///
    /// # Panics
    ///
    /// Panics if [`GrevmConfig::concurrency_level`] is zero.
    pub fn new(
        cfg: CfgEnv,
        block: BlockEnv,
        state: ParallelState<DB>,
        custom_precompiles: Option<Arc<Vec<(Address, DynParallelPrecompile)>>>,
        config: GrevmConfig,
    ) -> Self {
        assert!(config.concurrency_level > 0, "grevm concurrency level must be greater than zero");
        Self { cfg, block, state: Some(state), custom_precompiles, config }
    }

    /// Returns the canonical state accumulated by every completed batch.
    ///
    /// # Panics
    ///
    /// Panics while a batch is executing or after [`BatchExecutionResult::InvariantViolation`].
    pub fn state(&self) -> &ParallelState<DB> {
        self.state
            .as_ref()
            .expect("execution session state is unavailable after an invariant violation")
    }

    /// Returns mutable access to the canonical state between batch executions.
    ///
    /// Integrations can use this to install a state hook, apply pre/post-execution changes, or
    /// inspect EIP-7928 state without taking ownership away from the session.
    ///
    /// # Panics
    ///
    /// Panics while a batch is executing or after [`BatchExecutionResult::InvariantViolation`].
    pub fn state_mut(&mut self) -> &mut ParallelState<DB> {
        self.state
            .as_mut()
            .expect("execution session state is unavailable after an invariant violation")
    }

    /// Consume the session and return its canonical state.
    ///
    /// # Panics
    ///
    /// Panics after [`BatchExecutionResult::InvariantViolation`].
    pub fn into_state(mut self) -> ParallelState<DB> {
        self.state
            .take()
            .expect("execution session state is unavailable after an invariant violation")
    }

    /// Execute one ordered candidate batch using this session's scheduler configuration.
    pub fn execute_batch(&mut self, transactions: Vec<TxEnv>) -> BatchExecutionResult<DB::Error> {
        self.execute_batch_inner(transactions, None)
    }

    /// Execute one ordered candidate batch with a caller-borrowing cancellation check.
    ///
    /// The closure does not need to be `'static`: GREVM retains it only for this synchronous call,
    /// including its scoped scheduler threads. This is suitable for borrowing an RAII cancellation
    /// guard whose clone would have drop side effects. It is polled concurrently and repeatedly,
    /// so it must be cheap, non-blocking, and thread-safe. Interruption is cooperative and cannot
    /// preempt an EVM invocation already in progress.
    pub fn execute_batch_with_cancellation<C>(
        &mut self,
        transactions: Vec<TxEnv>,
        cancellation: C,
    ) -> BatchExecutionResult<DB::Error>
    where
        C: Fn() -> bool + Send + Sync,
    {
        self.execute_batch_inner(transactions, Some(&cancellation))
    }

    fn execute_batch_inner(
        &mut self,
        transactions: Vec<TxEnv>,
        cancellation: Option<&(dyn Fn() -> bool + Send + Sync)>,
    ) -> BatchExecutionResult<DB::Error> {
        let transaction_count = transactions.len();
        let state = self
            .state
            .take()
            .expect("execution session does not allow overlapping batch executions");
        let scheduler = Scheduler::new_with_runtime_config(
            self.cfg.clone(),
            self.block.clone(),
            Arc::new(transactions),
            state,
            self.custom_precompiles.clone(),
            self.config.clone(),
        );
        let status = match cancellation {
            Some(cancellation) => scheduler.execute_with_typed_cancellation(cancellation),
            None => scheduler.execute().map_err(SchedulerExecutionError::Failed),
        };
        let (outcomes, state) = scheduler.take_result_and_state();

        if outcomes.len() > transaction_count ||
            status.is_ok() && outcomes.len() != transaction_count
        {
            let outcome_count = outcomes.len();
            return BatchExecutionResult::InvariantViolation {
                error: GrevmError {
                    txid: outcome_count.min(transaction_count.saturating_sub(1)),
                    error: EVMError::Custom(format!(
                        "scheduler completed with {outcome_count} outcomes for {transaction_count} transactions",
                    )),
                },
            }
        }
        self.state = Some(state);

        match status {
            Ok(()) => BatchExecutionResult::Complete(outcomes),
            Err(SchedulerExecutionError::Interrupted) => {
                BatchExecutionResult::Interrupted { processed_prefix: outcomes }
            }
            Err(SchedulerExecutionError::Failed(error)) => {
                BatchExecutionResult::Failed { processed_prefix: outcomes, error }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{DelegatedSafetyConfig, InvalidTransactionPolicy};
    use revm::DatabaseRef;
    use revm_context::result::InvalidTransaction;
    use revm_database::EmptyDB;
    use revm_primitives::{TxKind, U256, hardfork::SpecId};
    use revm_state::AccountInfo;
    use std::sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
    };

    const GAS_LIMIT: u64 = 21_000;

    fn sequential_config(policy: InvalidTransactionPolicy) -> GrevmConfig {
        GrevmConfig {
            concurrency_level: 1,
            force_sequential: true,
            min_parallel_txs: 0,
            invalid_transaction_policy: policy,
            delegated_safety: DelegatedSafetyConfig::disabled(),
        }
    }

    fn parallel_config(policy: InvalidTransactionPolicy) -> GrevmConfig {
        GrevmConfig {
            concurrency_level: 2,
            force_sequential: false,
            min_parallel_txs: 0,
            invalid_transaction_policy: policy,
            delegated_safety: DelegatedSafetyConfig::disabled(),
        }
    }

    fn transfer(caller: Address, recipient: Address, nonce: u64) -> TxEnv {
        TxEnv {
            caller,
            kind: TxKind::Call(recipient),
            value: U256::from(1),
            gas_limit: GAS_LIMIT,
            nonce,
            ..Default::default()
        }
    }

    fn session_with_config(config: GrevmConfig) -> ExecutionSession<EmptyDB> {
        let caller = Address::with_last_byte(1);
        let state = ParallelState::new(EmptyDB::default(), true, false).with_bal_builder();
        state.insert_account(
            caller,
            AccountInfo { balance: U256::from(1_000_000), ..Default::default() },
        );
        ExecutionSession::new(
            CfgEnv::new_with_spec(SpecId::SHANGHAI),
            BlockEnv::default(),
            state,
            None,
            config,
        )
    }

    fn session(policy: InvalidTransactionPolicy) -> ExecutionSession<EmptyDB> {
        session_with_config(sequential_config(policy))
    }

    #[test]
    fn canonical_state_bal_and_hook_continue_across_batches() {
        let caller = Address::with_last_byte(1);
        let recipient = Address::with_last_byte(2);
        let observed = Arc::new(Mutex::new(0));
        let hook_observed = observed.clone();
        let mut session = session_with_config(parallel_config(InvalidTransactionPolicy::Abort));
        session.state_mut().set_state_hook(Some(Box::new(move |_| {
            *hook_observed.lock().unwrap() += 1;
        })));

        let first = session.execute_batch(vec![transfer(caller, recipient, 0)]);
        let second = session.execute_batch(vec![transfer(caller, recipient, 1)]);

        assert!(first.is_complete());
        assert!(second.is_complete());
        assert_eq!(first.processed_outcomes().unwrap().len(), 1);
        assert_eq!(second.processed_outcomes().unwrap().len(), 1);
        assert_eq!(session.state().bal_index().get(), 2);
        assert_eq!(*observed.lock().unwrap(), 2);
        assert_eq!(session.state().basic_ref(caller).unwrap().unwrap().nonce, 2);
    }

    #[test]
    fn failed_batch_returns_the_exact_committed_prefix() {
        let caller = Address::with_last_byte(1);
        let recipient = Address::with_last_byte(2);
        let mut session = session(InvalidTransactionPolicy::Abort);

        let result = session
            .execute_batch(vec![transfer(caller, recipient, 0), transfer(caller, recipient, 3)]);

        let BatchExecutionResult::Failed { processed_prefix, error } = result else {
            panic!("invalid fixed-block transaction must fail")
        };
        assert_eq!(processed_prefix.len(), 1);
        assert_eq!(error.txid, 1);
        assert!(matches!(
            error.error,
            revm_context::result::EVMError::Transaction(InvalidTransaction::NonceTooHigh { .. })
        ));
        assert_eq!(session.state().bal_index().get(), 1);
        assert_eq!(session.state().basic_ref(caller).unwrap().unwrap().nonce, 1);
    }

    #[test]
    fn cancellation_callback_can_borrow_and_preserves_the_committed_prefix() {
        let caller = Address::with_last_byte(1);
        let recipient = Address::with_last_byte(2);
        let cancelled = Arc::new(AtomicBool::new(false));
        let cancel_after_commit = cancelled.clone();
        let mut session = session_with_config(parallel_config(InvalidTransactionPolicy::Abort));
        session.state_mut().set_state_hook(Some(Box::new(move |_| {
            cancel_after_commit.store(true, Ordering::Release);
        })));

        // This closure borrows `cancelled`; the API must not require a `'static` observer.
        let result = session.execute_batch_with_cancellation(
            vec![transfer(caller, recipient, 0), transfer(caller, recipient, 1)],
            || cancelled.load(Ordering::Acquire),
        );

        let BatchExecutionResult::Interrupted { processed_prefix } = result else {
            panic!("cancellation after the first commit must stop the suffix")
        };
        assert_eq!(processed_prefix.len(), 1);
        assert_eq!(session.state().bal_index().get(), 1);
        assert_eq!(session.state().basic_ref(caller).unwrap().unwrap().nonce, 1);
    }

    #[test]
    fn cancellation_is_checked_before_parallel_or_sequential_work() {
        let caller = Address::with_last_byte(1);
        let recipient = Address::with_last_byte(2);

        for config in [
            sequential_config(InvalidTransactionPolicy::Abort),
            parallel_config(InvalidTransactionPolicy::Abort),
        ] {
            let mut session = session_with_config(config);
            let result = session
                .execute_batch_with_cancellation(vec![transfer(caller, recipient, 0)], || true);

            let BatchExecutionResult::Interrupted { processed_prefix } = result else {
                panic!("an initial cancellation must interrupt before execution")
            };
            assert!(processed_prefix.is_empty());
            assert_eq!(session.state().bal_index().get(), 0);
            assert_eq!(session.state().basic_ref(caller).unwrap().unwrap().nonce, 0);
        }
    }
}
