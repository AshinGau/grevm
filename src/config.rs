use crate::DelegatedSafetyConfig;
use std::fmt;

/// Policy for a transaction that is invalid against the ordered committed state.
///
/// EVM reverts and halts are valid executions and are unaffected by this policy.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum InvalidTransactionPolicy {
    /// Return the invalid transaction as an execution error.
    ///
    /// Use this for fixed blocks, where every transaction must be valid.
    #[default]
    Abort,
    /// Omit the invalid candidate without advancing the EIP-7928 block index.
    ///
    /// Use this while building a block from a transaction pool.
    Omit,
    /// Keep the invalid transaction as a block-positioned no-op.
    ///
    /// This preserves Grevm's historical skip behavior for protocols that encode invalid
    /// transactions as no-ops in the block.
    IncludeNoop,
}

/// Performance-only scheduler settings.
///
/// These values select how Grevm schedules work; they never change transaction validity or EVM
/// semantics. Use an [`ExecutionProfile`] when constructing [`GrevmConfig`] so chain-specific
/// behavior cannot be accidentally inherited from performance tuning.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SchedulerTuning {
    /// Requested upper bound for speculative execution workers.
    ///
    /// The actual worker count is limited by the shared [`crate::ExecutionResources`] budget and
    /// may fall back to sequential execution when fewer than two workers can be allocated.
    pub concurrency_level: usize,
    /// Execute the whole block through the sequential path.
    pub force_sequential: bool,
    /// Blocks smaller than this threshold use the sequential path.
    pub min_parallel_txs: usize,
}

impl SchedulerTuning {
    /// Builds scheduler tuning from environment variables.
    pub fn from_env() -> Self {
        let defaults = Self::default();
        Self {
            concurrency_level: env_or("GREVM_CONCURRENT_LEVEL", defaults.concurrency_level),
            force_sequential: env_or("GREVM_FALLBACK_SEQUENTIAL", defaults.force_sequential),
            min_parallel_txs: env_or("GREVM_MIN_PARALLEL_TXS", defaults.min_parallel_txs),
        }
    }

    /// Validates scheduler invariants before worker resources are allocated.
    pub const fn validate(self) -> Result<(), GrevmConfigError> {
        if self.concurrency_level == 0 {
            return Err(GrevmConfigError::ZeroConcurrency)
        }
        Ok(())
    }
}

impl Default for SchedulerTuning {
    fn default() -> Self {
        Self {
            concurrency_level: std::thread::available_parallelism().map_or(8, |value| value.get()),
            force_sequential: false,
            min_parallel_txs: 64,
        }
    }
}

/// A complete set of chain-sensitive execution semantics.
///
/// Fields are intentionally private: integrations select a named profile instead of independently
/// combining invalid-transaction behavior with delegated-account guards. In particular, both
/// Ethereum profiles always preserve upstream revm semantics by disabling Gravity's EIP-7702
/// guards.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ExecutionProfile {
    invalid_transaction_policy: InvalidTransactionPolicy,
    delegated_safety: DelegatedSafetyConfig,
}

impl ExecutionProfile {
    /// Ethereum payload building from a candidate transaction set.
    ///
    /// Invalid candidates are omitted and both Gravity-specific delegated-account guards are
    /// disabled.
    pub const fn ethereum_builder() -> Self {
        Self {
            invalid_transaction_policy: InvalidTransactionPolicy::Omit,
            delegated_safety: DelegatedSafetyConfig::disabled(),
        }
    }

    /// Ethereum validation or historical execution of a fixed block.
    ///
    /// Invalid transactions abort execution and both Gravity-specific delegated-account guards are
    /// disabled.
    pub const fn ethereum_block() -> Self {
        Self {
            invalid_transaction_policy: InvalidTransactionPolicy::Abort,
            delegated_safety: DelegatedSafetyConfig::disabled(),
        }
    }

    /// Gravity execution with its invalid no-op and delegated-account semantics enabled.
    pub const fn gravity() -> Self {
        Self {
            invalid_transaction_policy: InvalidTransactionPolicy::IncludeNoop,
            delegated_safety: DelegatedSafetyConfig::enabled(),
        }
    }

    /// How invalid transactions affect execution and block indexing.
    pub const fn invalid_transaction_policy(self) -> InvalidTransactionPolicy {
        self.invalid_transaction_policy
    }

    /// The delegated-account policy selected by this profile.
    pub const fn delegated_safety(self) -> DelegatedSafetyConfig {
        self.delegated_safety
    }
}

impl Default for ExecutionProfile {
    fn default() -> Self {
        Self::ethereum_block()
    }
}

/// Runtime configuration for one grevm scheduler.
///
/// Environment variables are read once when [`Self::from_env`] is called. Callers that need
/// consensus-stable behavior should construct this value explicitly and pass it to
/// [`crate::Scheduler::try_new_with_runtime_config`]. Chain-sensitive fields are crate-private so
/// integrations must select a complete named profile; scheduler tuning remains directly
/// inspectable for compatibility.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GrevmConfig {
    /// Requested upper bound for speculative execution workers.
    ///
    /// The actual worker count is limited by the shared [`crate::ExecutionResources`] budget and
    /// may fall back to sequential execution when fewer than two workers can be allocated.
    pub concurrency_level: usize,
    /// Execute the whole block through the sequential path.
    pub force_sequential: bool,
    /// Blocks smaller than this threshold use the sequential path.
    pub min_parallel_txs: usize,
    /// How invalid transactions affect execution and the EIP-7928 block index.
    pub(crate) invalid_transaction_policy: InvalidTransactionPolicy,
    /// EIP-7702 delegated-account safety policy.
    pub(crate) delegated_safety: DelegatedSafetyConfig,
}

impl GrevmConfig {
    /// Builds the Grevm runtime configuration from environment variables.
    pub fn from_env() -> Self {
        Self::new(SchedulerTuning::from_env(), ExecutionProfile::default())
    }

    /// Combines performance-only tuning with a chain-sensitive execution profile.
    pub const fn new(tuning: SchedulerTuning, profile: ExecutionProfile) -> Self {
        Self {
            concurrency_level: tuning.concurrency_level,
            force_sequential: tuning.force_sequential,
            min_parallel_txs: tuning.min_parallel_txs,
            invalid_transaction_policy: profile.invalid_transaction_policy(),
            delegated_safety: profile.delegated_safety(),
        }
    }

    /// Creates a configuration for Ethereum payload building.
    pub const fn ethereum_builder(tuning: SchedulerTuning) -> Self {
        Self::new(tuning, ExecutionProfile::ethereum_builder())
    }

    /// Creates a configuration for Ethereum fixed-block validation or historical execution.
    pub const fn ethereum_block(tuning: SchedulerTuning) -> Self {
        Self::new(tuning, ExecutionProfile::ethereum_block())
    }

    /// Creates a configuration with Gravity execution semantics.
    pub const fn gravity(tuning: SchedulerTuning) -> Self {
        Self::new(tuning, ExecutionProfile::gravity())
    }

    /// Returns only the performance-related portion of this configuration.
    pub const fn scheduler_tuning(&self) -> SchedulerTuning {
        SchedulerTuning {
            concurrency_level: self.concurrency_level,
            force_sequential: self.force_sequential,
            min_parallel_txs: self.min_parallel_txs,
        }
    }

    /// Validates scheduler invariants before worker resources are allocated.
    pub const fn validate(&self) -> Result<(), GrevmConfigError> {
        self.scheduler_tuning().validate()
    }

    /// Returns how invalid transactions affect execution and block indexing.
    pub const fn invalid_transaction_policy(&self) -> InvalidTransactionPolicy {
        self.invalid_transaction_policy
    }

    /// Returns the delegated-account policy selected by the execution profile.
    pub const fn delegated_safety(&self) -> DelegatedSafetyConfig {
        self.delegated_safety
    }

    /// Replaces all performance settings without changing execution semantics.
    pub const fn with_scheduler_tuning(mut self, tuning: SchedulerTuning) -> Self {
        self.concurrency_level = tuning.concurrency_level;
        self.force_sequential = tuning.force_sequential;
        self.min_parallel_txs = tuning.min_parallel_txs;
        self
    }

    /// Replaces all chain-sensitive settings with a named execution profile.
    pub const fn with_execution_profile(mut self, profile: ExecutionProfile) -> Self {
        self.invalid_transaction_policy = profile.invalid_transaction_policy();
        self.delegated_safety = profile.delegated_safety();
        self
    }

    /// Overrides the delegated-account policy for compatibility and policy tests.
    #[cfg(test)]
    pub fn with_delegated_safety(mut self, delegated_safety: DelegatedSafetyConfig) -> Self {
        self.delegated_safety = delegated_safety;
        self
    }

    /// Overrides invalid-transaction behavior for compatibility and policy tests.
    #[cfg(test)]
    pub const fn with_invalid_transaction_policy(
        mut self,
        policy: InvalidTransactionPolicy,
    ) -> Self {
        self.invalid_transaction_policy = policy;
        self
    }
}

/// Invalid GREVM runtime configuration.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GrevmConfigError {
    /// At least one execution worker is required.
    ZeroConcurrency,
}

impl fmt::Display for GrevmConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ZeroConcurrency => {
                f.write_str("GREVM concurrency level must be greater than zero")
            }
        }
    }
}

impl std::error::Error for GrevmConfigError {}

impl Default for GrevmConfig {
    fn default() -> Self {
        Self::ethereum_block(SchedulerTuning::default())
    }
}

fn env_or<T>(name: &str, default: T) -> T
where
    T: std::str::FromStr,
{
    std::env::var(name).ok().and_then(|value| value.parse().ok()).unwrap_or(default)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_configuration_is_safe_and_bounded() {
        let config = GrevmConfig::default();
        assert!(config.concurrency_level > 0);
        assert_eq!(config.min_parallel_txs, 64);
        assert!(!config.force_sequential);
        assert_eq!(config.invalid_transaction_policy, InvalidTransactionPolicy::Abort);
        assert!(!config.delegated_safety.forbid_delegated_create);
        assert!(!config.delegated_safety.reserve_delegated_balance);
        assert_eq!(config, GrevmConfig::ethereum_block(SchedulerTuning::default()));
    }

    #[test]
    fn named_profiles_bind_transaction_and_delegated_semantics() {
        let tuning =
            SchedulerTuning { concurrency_level: 3, force_sequential: true, min_parallel_txs: 7 };

        let builder = GrevmConfig::ethereum_builder(tuning);
        assert_eq!(builder.invalid_transaction_policy, InvalidTransactionPolicy::Omit);
        assert_eq!(builder.delegated_safety, DelegatedSafetyConfig::disabled());
        assert_eq!(builder.scheduler_tuning(), tuning);

        let block = GrevmConfig::ethereum_block(tuning);
        assert_eq!(block.invalid_transaction_policy, InvalidTransactionPolicy::Abort);
        assert_eq!(block.delegated_safety, DelegatedSafetyConfig::disabled());
        assert_eq!(block.scheduler_tuning(), tuning);

        let gravity = GrevmConfig::gravity(tuning);
        assert_eq!(gravity.invalid_transaction_policy, InvalidTransactionPolicy::IncludeNoop);
        assert_eq!(gravity.delegated_safety, DelegatedSafetyConfig::enabled());
        assert_eq!(gravity.scheduler_tuning(), tuning);
    }

    #[test]
    fn zero_concurrency_is_rejected() {
        let tuning = SchedulerTuning { concurrency_level: 0, ..SchedulerTuning::default() };

        assert_eq!(tuning.validate(), Err(GrevmConfigError::ZeroConcurrency));
        assert_eq!(
            GrevmConfig::ethereum_builder(tuning).validate(),
            Err(GrevmConfigError::ZeroConcurrency),
        );
    }

    #[test]
    fn applying_profile_replaces_all_semantics_and_preserves_tuning() {
        let config = GrevmConfig {
            concurrency_level: 3,
            force_sequential: true,
            min_parallel_txs: 7,
            invalid_transaction_policy: InvalidTransactionPolicy::IncludeNoop,
            delegated_safety: DelegatedSafetyConfig::enabled(),
        }
        .with_execution_profile(ExecutionProfile::ethereum_builder());

        assert_eq!(config.scheduler_tuning().concurrency_level, 3);
        assert!(config.scheduler_tuning().force_sequential);
        assert_eq!(config.scheduler_tuning().min_parallel_txs, 7);
        assert_eq!(config.invalid_transaction_policy, InvalidTransactionPolicy::Omit);
        assert_eq!(config.delegated_safety, DelegatedSafetyConfig::disabled());
    }

    #[test]
    fn delegated_policy_builder_preserves_scheduler_settings() {
        let config = GrevmConfig {
            concurrency_level: 3,
            force_sequential: true,
            min_parallel_txs: 7,
            invalid_transaction_policy: InvalidTransactionPolicy::Abort,
            delegated_safety: DelegatedSafetyConfig::default(),
        }
        .with_delegated_safety(DelegatedSafetyConfig::enabled());

        assert_eq!(config.concurrency_level, 3);
        assert!(config.force_sequential);
        assert_eq!(config.min_parallel_txs, 7);
        assert_eq!(config.invalid_transaction_policy, InvalidTransactionPolicy::Abort);
        assert!(config.delegated_safety.forbid_delegated_create);
        assert!(config.delegated_safety.reserve_delegated_balance);
    }
}
