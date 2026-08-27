use super::{ReserveJournalExt, ReservePlanner};
use crate::{
    TxId,
    beneficiary::{BeneficiaryMode, DeferredBeneficiaryReward, SpeculativeResult},
};
use metrics::counter;
use revm::{
    context_interface::journaled_state::account::JournaledAccountTr,
    handler::{EvmTr, EvmTrError, FrameResult, FrameTr, Handler, execution, post_execution},
    interpreter::{
        CallOutcome, GasTracker, InitialAndFloorGas, InstructionResult, InterpreterResult,
        interpreter_action::FrameInit,
    },
};
use revm_context::{
    BlockEnv, ContextTr, JournalTr, Transaction, TxEnv,
    journaled_state::JournalCheckpoint,
    result::{ExecutionResult, HaltReason, InvalidTransaction, ResultAndState, ResultGas},
};
use revm_primitives::Bytes;
use revm_state::EvmState;

use core::cell::Cell;

/// Handler output plus any non-zero beneficiary reward deferred to ordered commit.
pub(crate) struct GrevmHandlerOutput {
    result: ExecutionResult<HaltReason>,
    deferred_reward: Option<DeferredBeneficiaryReward>,
}

impl GrevmHandlerOutput {
    /// Combine the handler output with finalized EVM state for speculative scheduling.
    pub(crate) fn into_speculative(self, state: EvmState) -> SpeculativeResult {
        let result_and_state = ResultAndState { result: self.result, state };
        match self.deferred_reward {
            Some(reward) => SpeculativeResult::deferred(result_and_state, reward),
            None => SpeculativeResult::settled(result_and_state),
        }
    }

    /// Consume an immediate-mode output after asserting that nothing was deferred.
    pub(crate) fn into_immediate_result(self) -> ExecutionResult<HaltReason> {
        assert!(
            self.deferred_reward.is_none(),
            "sequential beneficiary rewards must be applied immediately",
        );
        self.result
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum ReserveMode<'a> {
    /// Preserve revm's default pre-execution lifecycle; do not checkpoint or scan the journal.
    NoReserve,
    /// Check delegated debits against the future costs of transactions after `txid`.
    WithReserve {
        /// Logical position in the original block, including during sequential suffix fallback.
        txid: TxId,
        /// Shared lazy costs for transactions strictly after `txid`.
        planner: &'a ReservePlanner,
    },
}

impl<'a> ReserveMode<'a> {
    /// Converts the scheduler's optional planner into an explicit transaction execution mode.
    pub(crate) fn from_planner(txid: TxId, planner: Option<&'a ReservePlanner>) -> Self {
        match planner {
            Some(planner) => Self::WithReserve { txid, planner },
            None => Self::NoReserve,
        }
    }
}

/// Grevm transaction handler with independent reserve and beneficiary policies.
///
/// `ReserveMode::NoReserve` dispatches to a handler that overrides only beneficiary handling, so
/// validation, pre-execution, execution, and post-execution ordering come directly from revm's
/// default `Handler` implementation. `ReserveMode::WithReserve` dispatches to a handler that also
/// overrides `execution` to insert the reserve checkpoint after the authorization and runtime-code
/// phases.
///
/// The handler deliberately relies on revm's native journal entries and checkpoints. Internal
/// frame reverts have already removed their entries when `has_reserve_violation` runs, so no
/// second undo stack or balance-mutation wrapper is needed.
///
/// When reserve protection is enabled, `execution` creates a checkpoint after revm has deducted
/// gas, bumped a CALL sender's nonce, applied EIP-7702 authorizations, and completed the EIP-2780
/// runtime-code phase. This handler reproduces revm's post-execution ordering, inserts the reserve
/// check after caller reimbursement, and applies the configured beneficiary policy:
///
/// ```text
/// validate -> deduct gas / bump CALL nonce -> apply EIP-7702 authorizations
///                                                        |
///                                              execution checkpoint
///                                                        |
///           execute -> gas/refund rules -> reimburse caller -> scan delegated debits
///                                                        |
///                              +-------------------------+-----------------------+
///                              |                                                 |
///                         reserve holds                                  reserve violated
///                              |                                                 |
///                     commit checkpoint                    revert checkpoint + force REVERT
///                                                                        |
///                                                   restore CREATE-tx nonce if needed
///                                                                        |
///                                        keep authorization refund, reimburse caller
///                              |                                                 |
///                              +-------------------------+-----------------------+
///                                                        |
///                                     apply beneficiary policy (immediate/deferred)
/// ```
///
/// With reserve protection disabled, no checkpoint or journal scan is performed.
///
/// Consequently, a reserve violation cannot become a free skipped transaction: execution state
/// is reverted, while charged gas, the transaction nonce, and authorization effects survive.
pub(crate) struct GrevmHandler<'a> {
    /// Selects the default or reserve-aware revm handler implementation.
    reserve_mode: ReserveMode<'a>,
    /// Chooses who applies the beneficiary write; it does not change reserve semantics.
    beneficiary_mode: BeneficiaryMode,
}

impl<'a> GrevmHandler<'a> {
    pub(crate) fn new(reserve_mode: ReserveMode<'a>, beneficiary_mode: BeneficiaryMode) -> Self {
        Self { reserve_mode, beneficiary_mode }
    }

    pub(crate) fn run<EVM, ERROR, FRAME>(self, evm: &mut EVM) -> Result<GrevmHandlerOutput, ERROR>
    where
        EVM: EvmTr<
                Context: ContextTr<
                    Block = BlockEnv,
                    Tx = TxEnv,
                    Journal: JournalTr<State = EvmState> + ReserveJournalExt,
                >,
                Frame = FRAME,
            >,
        ERROR: EvmTrError<EVM>,
        FRAME: FrameTr<FrameResult = FrameResult, FrameInit = FrameInit>,
    {
        let (result, deferred_reward) = match self.reserve_mode {
            ReserveMode::NoReserve => {
                let mut handler = NoReserveHandler::<EVM, ERROR, FRAME>::new(self.beneficiary_mode);
                let result = Handler::run(&mut handler, evm)?;
                (result, handler.deferred_reward.get())
            }
            ReserveMode::WithReserve { txid, planner } => {
                let mut handler = WithReserveHandler::<EVM, ERROR, FRAME>::new(
                    txid,
                    planner,
                    self.beneficiary_mode,
                );
                let result = Handler::run(&mut handler, evm)?;
                (result, handler.deferred_reward.get())
            }
        };
        Ok(GrevmHandlerOutput { result, deferred_reward })
    }
}

/// `ReserveMode::NoReserve` implementation. Deliberately does not override `pre_execution`.
struct NoReserveHandler<EVM, ERROR, FRAME> {
    beneficiary_mode: BeneficiaryMode,
    deferred_reward: Cell<Option<DeferredBeneficiaryReward>>,
    _phantom: core::marker::PhantomData<(EVM, ERROR, FRAME)>,
}

impl<EVM, ERROR, FRAME> NoReserveHandler<EVM, ERROR, FRAME> {
    fn new(beneficiary_mode: BeneficiaryMode) -> Self {
        Self {
            beneficiary_mode,
            deferred_reward: Cell::new(None),
            _phantom: core::marker::PhantomData,
        }
    }
}

impl<EVM, ERROR, FRAME> Handler for NoReserveHandler<EVM, ERROR, FRAME>
where
    EVM: EvmTr<Context: ContextTr<Journal: JournalTr<State = EvmState>>, Frame = FRAME>,
    ERROR: EvmTrError<EVM>,
    FRAME: FrameTr<FrameResult = FrameResult, FrameInit = FrameInit>,
{
    type Evm = EVM;
    type Error = ERROR;
    type HaltReason = HaltReason;

    fn reward_beneficiary(
        &self,
        evm: &mut Self::Evm,
        exec_result: &mut FrameResult,
    ) -> Result<(), Self::Error> {
        self.beneficiary_mode.apply(evm, exec_result, &self.deferred_reward)
    }
}

/// `ReserveMode::WithReserve` implementation. Adds only the two hooks needed by the guard.
struct WithReserveHandler<'a, EVM, ERROR, FRAME> {
    /// Logical position in the original block, including during sequential suffix fallback.
    txid: TxId,
    /// Shared lazy costs for transactions strictly after `txid`.
    planner: &'a ReservePlanner,
    beneficiary_mode: BeneficiaryMode,
    deferred_reward: Cell<Option<DeferredBeneficiaryReward>>,
    /// Transaction-execution checkpoint created by `execution` and consumed exactly once by
    /// `post_execution`. `Cell` is needed because revm exposes the latter hook through `&self`.
    execution_checkpoint: Cell<Option<JournalCheckpoint>>,
    _phantom: core::marker::PhantomData<(EVM, ERROR, FRAME)>,
}

impl<'a, EVM, ERROR, FRAME> WithReserveHandler<'a, EVM, ERROR, FRAME> {
    fn new(txid: TxId, planner: &'a ReservePlanner, beneficiary_mode: BeneficiaryMode) -> Self {
        Self {
            txid,
            planner,
            beneficiary_mode,
            deferred_reward: Cell::new(None),
            execution_checkpoint: Cell::new(None),
            _phantom: core::marker::PhantomData,
        }
    }
}

impl<EVM, ERROR, FRAME> Handler for WithReserveHandler<'_, EVM, ERROR, FRAME>
where
    EVM: EvmTr<
            Context: ContextTr<
                Block = BlockEnv,
                Tx = TxEnv,
                Journal: JournalTr<State = EvmState> + ReserveJournalExt,
            >,
            Frame = FRAME,
        >,
    ERROR: EvmTrError<EVM>,
    FRAME: FrameTr<FrameResult = FrameResult, FrameInit = FrameInit>,
{
    type Evm = EVM;
    type Error = ERROR;
    type HaltReason = HaltReason;

    fn execution(
        &mut self,
        evm: &mut Self::Evm,
        checkpoint: JournalCheckpoint,
        gas: &mut GasTracker,
    ) -> Result<Option<FrameResult>, Self::Error> {
        let Some(first_frame_input) = self.first_frame_input(evm, gas)? else {
            execution::runtime_oog_unwind(evm.ctx(), checkpoint)?;
            return Ok(None)
        };

        // Authorization and EIP-2780 runtime state must survive a reserve rollback. Open the
        // reserve checkpoint only after revm has completed and committed that phase.
        evm.ctx().journal_mut().checkpoint_commit();

        debug_assert!(self.execution_checkpoint.get().is_none());
        self.execution_checkpoint.set(Some(evm.ctx().journal_mut().checkpoint()));

        let mut frame_result = self.run_exec_loop(evm, first_frame_input)?;
        self.last_frame_result(evm, &mut frame_result, gas)?;
        Ok(Some(frame_result))
    }

    fn post_execution(
        &self,
        evm: &mut Self::Evm,
        exec_result: &mut FrameResult,
        init_and_floor_gas: InitialAndFloorGas,
        eip7702_gas_refund: i64,
    ) -> Result<ResultGas, Self::Error> {
        // Preserve the gas state before revm merges execution refunds with the authorization
        // refund. A reserve violation rolls back execution, so only the independently earned
        // EIP-7702 authorization refund may be reapplied to this snapshot.
        let execution_gas = *exec_result.gas();

        self.refund(evm, exec_result, eip7702_gas_refund)?;
        // Match revm's default lifecycle: capture the pre-floor gas components. `ResultGas`
        // applies the EIP-7623 floor when deriving `tx_gas_used`, while retaining the actual
        // pre-floor spend and refund for downstream consumers.
        let mut result_gas = post_execution::build_result_gas(
            exec_result.instruction_result().is_halt(),
            exec_result.gas(),
            init_and_floor_gas,
        );
        self.eip7623_check_gas_floor(evm, exec_result, init_and_floor_gas);
        self.reimburse_caller(evm, exec_result)?;
        if let Some(reserve_result_gas) = self.enforce_reserve(
            evm,
            exec_result,
            execution_gas,
            init_and_floor_gas,
            eip7702_gas_refund,
        )? {
            result_gas = reserve_result_gas;
        }
        self.beneficiary_mode.apply::<EVM, ERROR>(evm, exec_result, &self.deferred_reward)?;
        Ok(result_gas)
    }
}

impl<EVM, ERROR, FRAME> WithReserveHandler<'_, EVM, ERROR, FRAME>
where
    EVM: EvmTr<
            Context: ContextTr<
                Block = BlockEnv,
                Tx = TxEnv,
                Journal: JournalTr<State = EvmState> + ReserveJournalExt,
            >,
            Frame = FRAME,
        >,
    ERROR: EvmTrError<EVM>,
    FRAME: FrameTr<FrameResult = FrameResult, FrameInit = FrameInit>,
{
    fn enforce_reserve(
        &self,
        evm: &mut EVM,
        exec_result: &mut FrameResult,
        execution_gas: revm::interpreter::Gas,
        init_and_floor_gas: InitialAndFloorGas,
        eip7702_gas_refund: i64,
    ) -> Result<Option<ResultGas>, ERROR> {
        // Pre-execution validation and EIP-2780 runtime setup can run out of gas before the
        // reserve checkpoint exists. Revm still invokes post-execution for those outcomes, but
        // there is no delegated execution state to inspect or roll back.
        let Some(execution_checkpoint) = self.execution_checkpoint.take() else { return Ok(None) };

        // Revm invokes this hook after refund calculation, gas-floor enforcement, and caller
        // reimbursement. The inspected balance is therefore the real post-transaction balance.
        if self.has_reserve_violation(evm, execution_checkpoint)? {
            // CALL transactions bumped their sender nonce before the checkpoint. A top-level
            // CREATE transaction bumps it while constructing the first frame, so that one nonce
            // must be restored explicitly after reverting the execution checkpoint.
            let recreate_sender_nonce = evm.ctx_ref().tx().kind().is_create();
            evm.ctx().journal_mut().checkpoint_revert(execution_checkpoint);
            *exec_result = reserve_violation_result(execution_gas);
            if recreate_sender_nonce {
                reapply_create_sender_nonce::<EVM, ERROR>(evm)?;
            }

            // Execution-state refunds disappeared with the reverted checkpoint. Reapply only the
            // authorization refund, then recompute both the normal refund cap and EIP-7623 floor
            // from the pre-refund execution gas. The first reimbursement was also reverted, so
            // apply it again using this synthetic top-level REVERT result.
            self.refund(evm, exec_result, eip7702_gas_refund)?;
            let result_gas = post_execution::build_result_gas(
                exec_result.instruction_result().is_halt(),
                exec_result.gas(),
                init_and_floor_gas,
            );
            self.eip7623_check_gas_floor(evm, exec_result, init_and_floor_gas);
            self.reimburse_caller(evm, exec_result)?;
            return Ok(Some(result_gas))
        } else {
            evm.ctx().journal_mut().checkpoint_commit();
        }
        Ok(None)
    }

    fn has_reserve_violation(
        &self,
        evm: &mut EVM,
        checkpoint: JournalCheckpoint,
    ) -> Result<bool, ERROR> {
        // The journal helper excludes top-level transaction.value and ordinary contract debits.
        // Only surviving value movement from an EIP-7702 state context reaches this loop.
        let candidates =
            evm.ctx_ref().journal().delegated_debits_since(checkpoint, evm.ctx_ref().tx());
        if !candidates.is_empty() {
            counter!("grevm.reserve_debit_candidates").increment(candidates.len() as u64);
        }
        for candidate in candidates {
            let future_cost = self.planner.required_after(self.txid, candidate.address);
            if future_cost.is_zero() {
                continue;
            }
            // Never demand money the account did not have before delegated execution. If it was
            // already below the conservative future-cost sum, delegated execution may not make
            // that shortage worse, but unrelated filtering remains outside this policy.
            let required = candidate.balance_before.min(future_cost);
            if candidate.final_balance < required {
                return Ok(true);
            }
        }
        Ok(false)
    }
}

fn reserve_violation_result(mut gas: revm::interpreter::Gas) -> FrameResult {
    // Preserve the gas spent by the attempted execution, but discard its execution-state refund
    // and output. The caller reapplies the checkpoint-independent authorization refund before
    // reimbursement.
    gas.set_refund(0);
    FrameResult::Call(CallOutcome::new(
        InterpreterResult { result: InstructionResult::Revert, output: Bytes::new(), gas },
        0..0,
    ))
}

fn reapply_create_sender_nonce<EVM, ERROR>(evm: &mut EVM) -> Result<(), ERROR>
where
    EVM: EvmTr<Context: ContextTr<Tx = TxEnv, Journal: JournalTr<State = EvmState>>>,
    ERROR: EvmTrError<EVM>,
{
    // Reproduce the nonce bump normally performed by revm's top-level CREATE frame. It must be a
    // journaled mutation so a surrounding transaction/error rollback still has correct semantics.
    let sender = evm.ctx_ref().tx().caller();
    let mut account = evm.ctx().journal_mut().load_account_mut(sender)?;
    if !account.data.bump_nonce() {
        return Err(InvalidTransaction::NonceOverflowInTransaction.into());
    }
    Ok(())
}
