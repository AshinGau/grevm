use crate::atomic::RelaxedUsize;
use metrics_derive::Metrics;

#[derive(Metrics)]
#[metrics(scope = "grevm")]
struct ExecuteMetrics {
    /// Total transactions.
    total_tx_cnt: metrics::Histogram,
    /// Conflict incarnations.
    conflict_cnt: metrics::Histogram,
    /// Validation incarnations.
    validation_cnt: metrics::Histogram,
    /// Execution incarnations.
    execution_cnt: metrics::Histogram,
    /// Validation cursor resets.
    reset_validation_idx_cnt: metrics::Histogram,
    /// Dependency updates without work.
    useless_dependent_update: metrics::Histogram,
    /// Coinbase conflicts.
    conflict_by_miner: metrics::Histogram,
    /// EVM error conflicts.
    conflict_by_error: metrics::Histogram,
    /// Estimate read conflicts.
    conflict_by_estimate: metrics::Histogram,
    /// Version conflicts.
    conflict_by_version: metrics::Histogram,
    /// Transactions completed in one dependency attempt.
    one_attempt_with_dependency: metrics::Histogram,
    /// Transactions needing more than two dependency attempts.
    more_attempts_with_dependency: metrics::Histogram,
    /// Transactions without dependencies.
    no_dependency_txs: metrics::Histogram,
    /// Transactions with at least one conflict.
    conflict_txs: metrics::Histogram,
    /// Parallel execution time in nanoseconds.
    execution_time: metrics::Histogram,
    /// Commit time in nanoseconds.
    commit_time: metrics::Histogram,
    /// Total time in nanoseconds.
    total_time: metrics::Histogram,
}

#[derive(Default)]
pub(super) struct ExecuteMetricsCollector {
    pub(super) total_tx_cnt: RelaxedUsize,
    pub(super) conflict_cnt: RelaxedUsize,
    pub(super) validation_cnt: RelaxedUsize,
    pub(super) execution_cnt: RelaxedUsize,
    pub(super) reset_validation_idx_cnt: RelaxedUsize,
    pub(super) useless_dependent_update: RelaxedUsize,
    pub(super) conflict_by_miner: RelaxedUsize,
    pub(super) conflict_by_error: RelaxedUsize,
    pub(super) conflict_by_estimate: RelaxedUsize,
    pub(super) conflict_by_version: RelaxedUsize,
    pub(super) one_attempt_with_dependency: RelaxedUsize,
    pub(super) more_attempts_with_dependency: RelaxedUsize,
    pub(super) no_dependency_txs: RelaxedUsize,
    pub(super) conflict_txs: RelaxedUsize,
    pub(super) execution_time: RelaxedUsize,
    pub(super) commit_time: RelaxedUsize,
    pub(super) total_time: RelaxedUsize,
}

impl ExecuteMetricsCollector {
    pub(super) fn report(&self) {
        let metrics = ExecuteMetrics::default();
        metrics.total_tx_cnt.record(load(&self.total_tx_cnt));
        metrics.conflict_cnt.record(load(&self.conflict_cnt));
        metrics.validation_cnt.record(load(&self.validation_cnt));
        metrics.execution_cnt.record(load(&self.execution_cnt));
        metrics.reset_validation_idx_cnt.record(load(&self.reset_validation_idx_cnt));
        metrics.useless_dependent_update.record(load(&self.useless_dependent_update));
        metrics.conflict_by_miner.record(load(&self.conflict_by_miner));
        metrics.conflict_by_error.record(load(&self.conflict_by_error));
        metrics.conflict_by_estimate.record(load(&self.conflict_by_estimate));
        metrics.conflict_by_version.record(load(&self.conflict_by_version));
        metrics.one_attempt_with_dependency.record(load(&self.one_attempt_with_dependency));
        metrics.more_attempts_with_dependency.record(load(&self.more_attempts_with_dependency));
        metrics.no_dependency_txs.record(load(&self.no_dependency_txs));
        metrics.conflict_txs.record(load(&self.conflict_txs));
        let execution_time = self.execution_time.get();
        if execution_time > 0 {
            metrics.execution_time.record(execution_time as f64);
        }
        metrics.commit_time.record(load(&self.commit_time));
        metrics.total_time.record(load(&self.total_time));
    }
}

fn load(value: &RelaxedUsize) -> f64 {
    value.get() as f64
}
