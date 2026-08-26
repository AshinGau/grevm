//! This module contains utilities for testing and benchmarking the Grevm library.

use crate::{
    DelegatedSafetyConfig, ExecutionResources, GrevmConfig, InvalidTransactionPolicy,
    SchedulerTuning,
};
use std::num::NonZeroUsize;

pub mod common;
pub mod erc20;
pub mod uniswap;

/// Gas limit for native transfer transactions.
pub const TRANSFER_GAS_LIMIT: u64 = 21_000;

/// Builds a runtime configuration with independently selectable policies for differential tests.
///
/// Production integrations should use a complete named [`GrevmConfig`] profile. This feature-gated
/// helper exists only for testing individual policy combinations.
pub fn runtime_config_with_policies(
    tuning: SchedulerTuning,
    invalid_transaction_policy: InvalidTransactionPolicy,
    delegated_safety: DelegatedSafetyConfig,
) -> GrevmConfig {
    GrevmConfig {
        concurrency_level: tuning.concurrency_level,
        force_sequential: tuning.force_sequential,
        min_parallel_txs: tuning.min_parallel_txs,
        invalid_transaction_policy,
        delegated_safety,
    }
}

/// Creates an isolated role budget large enough to exercise at least two speculative workers.
///
/// This prevents low-core CI runners from silently downgrading tests intended to cover the
/// parallel scheduler. A configuration that explicitly requests one worker or forces sequential
/// execution still uses the sequential path. Production code should use
/// [`ExecutionResources::process_default`].
pub fn execution_resources_for_workers(workers: usize) -> ExecutionResources {
    let roles = workers.max(2).saturating_add(2);
    ExecutionResources::dedicated(
        NonZeroUsize::new(roles).expect("test execution resource capacity is non-zero"),
    )
}
