mod cache;
#[cfg(test)]
mod tests;

use cache::{CacheAccountInfo, into_revm_transition};
use core::hash::{BuildHasherDefault, Hasher};
use dashmap::{DashMap, Entry};
use metrics::histogram;
use revm::{Database, DatabaseCommit, DatabaseRef, OnStateHook, database_interface::bal::BalState};
use revm_database::{
    AccountStatus, BundleState, DatabaseCommitExt, TransitionAccount, TransitionState,
    states::{bundle_state::BundleRetention, plain_account::PlainStorage},
};
use revm_primitives::{Address, B256, U256};
use revm_state::{
    AccountInfo, Bytecode, EvmState,
    bal::{BlockAccessIndex, alloy::AlloyBal},
};
use std::{
    borrow::Cow,
    fmt::Formatter,
    time::{Duration, Instant},
    vec::Vec,
};

pub use cache::ParallelCacheState;

/// State of blockchain.
///
/// Fork-sensitive account semantics, including EIP-161 state clearing, are resolved by revm's
/// journal according to `CfgEnv::spec` before the finalized state reaches this commit layer.
///
/// Represents the state of a parallelized execution environment, managing
/// cache, database interactions, and state transitions.
///
/// # Type Parameters
/// - `DB`: A type that implements the `DatabaseRef` trait, representing the database backend.
///
/// This struct provides methods for managing account balances, applying
/// transitions, and interacting with the underlying database. It also supports
/// metrics collection for database operations.
pub struct ParallelState<DB> {
    /// Cached state contains both changed from evm execution and cached/loaded account/storages
    /// from database. This allows us to have only one layer of cache where we can fetch data.
    /// Additionally we can introduce some preloading of data from database.
    cache: ParallelCacheState,
    /// Optional database that we use to fetch data from. If database is not present, we will
    /// return not existing account and storage.
    ///
    /// Note: It is marked as Send so database can be shared between threads.
    database: DB,
    /// Block state, it aggregates transactions transitions into one state.
    ///
    /// Build reverts and state that gets applied to the state.
    pub(crate) transition_state: Option<TransitionState>,
    /// After block is finishes we merge those changes inside bundle.
    /// Bundle is used to update database and create changesets.
    /// Bundle state can be set on initialization if we want to use preloaded bundle.
    pub(crate) bundle_state: BundleState,
    /// If EVM asks for block hash we will first check if they are found here.
    /// and then ask the database.
    ///
    /// This map can be used to give different values for block hashes if in case
    /// The fork block is different or some blocks are not saved inside database.
    block_hashes: DashMap<u64, B256, BuildIdentityHasher>,
    /// EIP-7928 builder state. Only canonical commits mutate the BAL builder.
    ///
    /// Input-BAL reads require a transaction-indexed worker view and are intentionally outside
    /// this state type.
    bal_state: BalState,

    /// Hook invoked for canonical state commits.
    state_hook: Option<Box<dyn OnStateHook>>,

    update_db_metrics: bool,
    db_latency: metrics::Histogram,
}

#[derive(Debug, Default)]
struct IdentityHasher(u64);

impl Hasher for IdentityHasher {
    fn finish(&self) -> u64 {
        self.0
    }

    fn write(&mut self, _: &[u8]) {
        unreachable!()
    }

    fn write_u64(&mut self, id: u64) {
        self.0 = id;
    }

    fn write_usize(&mut self, id: usize) {
        self.0 = id as u64;
    }
}

type BuildIdentityHasher = BuildHasherDefault<IdentityHasher>;

#[inline]
fn duration_micros(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1_000_000.0
}

/// Borrowed view of the state that is safe to share with speculative workers.
///
/// The view deliberately excludes `transition_state` and `bundle_state`: those fields are owned by
/// the ordered commit path while workers are running. All mutation reachable through this view is
/// provided by the concurrent cache and block-hash maps.
pub(crate) struct ParallelStateView<'a, DB> {
    cache: &'a ParallelCacheState,
    database: &'a DB,
    block_hashes: &'a DashMap<u64, B256, BuildIdentityHasher>,
    update_db_metrics: bool,
    db_latency: &'a metrics::Histogram,
}

impl<DB> Copy for ParallelStateView<'_, DB> {}

impl<DB> Clone for ParallelStateView<'_, DB> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<DB> std::fmt::Debug for ParallelStateView<'_, DB> {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ParallelStateView").field("cache", self.cache).finish_non_exhaustive()
    }
}

impl<'a, DB: DatabaseRef> ParallelStateView<'a, DB> {
    fn with_metrics<F, R>(self, func: F) -> R
    where
        F: FnOnce() -> R,
    {
        if self.update_db_metrics {
            let start = Instant::now();
            let result = func();
            self.db_latency.record(duration_micros(start.elapsed()));
            result
        } else {
            func()
        }
    }

    fn db_basic(self, address: Address) -> Result<Option<AccountInfo>, DB::Error> {
        if let Some(account) = self.cache.accounts.get(&address) {
            return Ok(account.account.clone());
        }
        let info = self.with_metrics(|| self.database.basic_ref(address))?;
        let account = match info {
            None => CacheAccountInfo::new(None, AccountStatus::LoadedNotExisting),
            Some(acc) if acc.is_empty() => CacheAccountInfo::new(
                Some(AccountInfo::default()),
                AccountStatus::LoadedEmptyEIP161,
            ),
            Some(acc) => CacheAccountInfo::new(Some(acc), AccountStatus::Loaded),
        };
        match self.cache.accounts.entry(address) {
            Entry::Vacant(entry) => Ok(entry.insert(account).account.clone()),
            Entry::Occupied(entry) => Ok(entry.into_ref().account.clone()),
        }
    }

    fn db_code_by_hash(self, code_hash: B256) -> Result<Bytecode, DB::Error> {
        if let Some(code) = self.cache.contracts.get(&code_hash) {
            return Ok(code.value().clone());
        }
        let code = self.with_metrics(|| self.database.code_by_hash_ref(code_hash))?;
        match self.cache.contracts.entry(code_hash) {
            Entry::Occupied(entry) => Ok(entry.get().clone()),
            Entry::Vacant(entry) => {
                entry.insert(code.clone());
                Ok(code)
            }
        }
    }

    fn db_storage(self, address: Address, index: U256) -> Result<U256, DB::Error> {
        if let Some(slots) = self.cache.storage.get(&address) &&
            let Some(value) = slots.get(&index)
        {
            return Ok(*value.value());
        }
        // As in revm State::storage_ref, the account is not guaranteed to be cached. In that case,
        // the backing database remains the source of truth.
        let is_storage_known =
            self.cache.accounts.get(&address).is_some_and(|account| {
                account.status.is_storage_known() || account.account.is_none()
            });

        let value = if is_storage_known {
            U256::ZERO
        } else {
            self.with_metrics(|| self.database.storage_ref(address, index))?
        };
        let value = if let Some(slots) = self.cache.storage.get(&address) {
            *slots.entry(index).or_insert(value).value()
        } else {
            match self.cache.storage.entry(address) {
                Entry::Occupied(entry) => *entry.get().entry(index).or_insert(value).value(),
                Entry::Vacant(entry) => {
                    *entry.insert(Default::default()).entry(index).or_insert(value).value()
                }
            }
        };
        Ok(value)
    }

    fn db_block_hash(self, number: u64) -> Result<B256, DB::Error> {
        match self.block_hashes.entry(number) {
            Entry::Occupied(entry) => Ok(*entry.get()),
            Entry::Vacant(entry) => {
                Ok(*entry.insert(self.with_metrics(|| self.database.block_hash_ref(number))?))
            }
        }
    }
}

impl<DB: DatabaseRef> DatabaseRef for ParallelStateView<'_, DB> {
    type Error = DB::Error;

    fn basic_ref(&self, address: Address) -> Result<Option<AccountInfo>, Self::Error> {
        (*self).db_basic(address)
    }

    fn code_by_hash_ref(&self, code_hash: B256) -> Result<Bytecode, Self::Error> {
        (*self).db_code_by_hash(code_hash)
    }

    fn storage_ref(&self, address: Address, index: U256) -> Result<U256, Self::Error> {
        (*self).db_storage(address, index)
    }

    fn block_hash_ref(&self, number: u64) -> Result<B256, Self::Error> {
        (*self).db_block_hash(number)
    }
}

/// Mutable state owned exclusively by the ordered commit thread.
///
/// Cache updates still go through the shared `DashMap`-backed view; only transition aggregation is
/// mutably borrowed. This makes the actual field-level concurrency contract explicit.
pub(crate) struct ParallelStateCommit<'a, DB> {
    shared: ParallelStateView<'a, DB>,
    transition_state: &'a mut Option<TransitionState>,
    bal_state: &'a mut BalState,
    state_hook: &'a mut Option<Box<dyn OnStateHook>>,
}

impl<DB> ParallelStateCommit<'_, DB> {
    pub(crate) fn bump_bal_index(&mut self) {
        self.bal_state.bump_bal_index();
    }
}

impl<DB: DatabaseRef> DatabaseRef for ParallelStateCommit<'_, DB> {
    type Error = DB::Error;

    fn basic_ref(&self, address: Address) -> Result<Option<AccountInfo>, Self::Error> {
        self.shared.basic_ref(address)
    }

    fn code_by_hash_ref(&self, code_hash: B256) -> Result<Bytecode, Self::Error> {
        self.shared.code_by_hash_ref(code_hash)
    }

    fn storage_ref(&self, address: Address, index: U256) -> Result<U256, Self::Error> {
        self.shared.storage_ref(address, index)
    }

    fn block_hash_ref(&self, number: u64) -> Result<B256, Self::Error> {
        self.shared.block_hash_ref(number)
    }
}

impl<DB: DatabaseRef> DatabaseCommit for ParallelStateCommit<'_, DB> {
    fn commit(&mut self, evm_state: EvmState) {
        commit_canonical_state(
            self.shared.cache,
            self.transition_state,
            self.bal_state,
            self.state_hook,
            evm_state,
        );
    }
}

fn commit_canonical_state(
    cache: &ParallelCacheState,
    transition_state: &mut Option<TransitionState>,
    bal_state: &mut BalState,
    state_hook: &mut Option<Box<dyn OnStateHook>>,
    evm_state: EvmState,
) {
    bal_state.commit(&evm_state);
    if let Some(hook) = state_hook.as_mut() {
        cache.apply_evm_state_with(
            evm_state.iter().map(|(address, account)| (*address, Cow::Borrowed(account))),
            |address, transition| {
                if let Some(state) = transition_state.as_mut() {
                    state.add_transition(address, transition);
                }
            },
        );
        hook.on_state(evm_state);
    } else {
        cache.apply_evm_state_with(
            evm_state.into_iter().map(|(address, account)| (address, Cow::Owned(account))),
            |address, transition| {
                if let Some(state) = transition_state.as_mut() {
                    state.add_transition(address, transition);
                }
            },
        );
    }
}

impl<DB> std::fmt::Debug for ParallelState<DB> {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ParallelState")
            .field("cache", &self.cache)
            .field("transition_state", &self.transition_state)
            .finish()
    }
}

impl<DB> ParallelState<DB> {
    /// Returns the controlled preload and compatibility cache API.
    pub const fn cache(&self) -> &ParallelCacheState {
        &self.cache
    }

    /// Returns mutable access to the controlled cache API.
    pub const fn cache_mut(&mut self) -> &mut ParallelCacheState {
        &mut self.cache
    }

    /// Returns the backing database.
    pub const fn database(&self) -> &DB {
        &self.database
    }

    /// Returns mutable access to the backing database while the state is exclusively borrowed.
    pub const fn database_mut(&mut self) -> &mut DB {
        &mut self.database
    }

    /// Consumes the state and returns its backing database.
    ///
    /// Pending transitions and bundle state are discarded. Finalize or extract them before using
    /// this method when they are required by the integration.
    pub fn into_database(self) -> DB {
        self.database
    }

    /// Inserts or replaces a cached block hash used by EVM `BLOCKHASH` reads.
    pub fn insert_block_hash(&self, number: u64, hash: B256) -> Option<B256> {
        self.block_hashes.insert(number, hash)
    }
}

impl<DB: DatabaseRef> ParallelState<DB> {
    /// Create a ParallelState
    /// #Parameters
    /// - `database`: the inner database to read the data not in cache
    /// - `with_bundle_update`: whether to update the bundle states
    /// - `update_db_metrics`: whether to report the database latency metrics
    pub fn new(database: DB, with_bundle_update: bool, update_db_metrics: bool) -> Self {
        Self {
            cache: ParallelCacheState::default(),
            database,
            transition_state: with_bundle_update.then(TransitionState::default),
            bundle_state: BundleState::default(),
            block_hashes: DashMap::default(),
            bal_state: BalState::default(),
            state_hook: None,
            update_db_metrics,
            db_latency: histogram!("grevm.db_latency_us"),
        }
    }

    fn shared_view(&self) -> ParallelStateView<'_, DB> {
        ParallelStateView {
            cache: &self.cache,
            database: &self.database,
            block_hashes: &self.block_hashes,
            update_db_metrics: self.update_db_metrics,
            db_latency: &self.db_latency,
        }
    }

    /// Split the state into the fields shared by speculative workers and the fields exclusively
    /// owned by ordered commit.
    pub(crate) fn split_for_parallel(
        &mut self,
    ) -> (ParallelStateView<'_, DB>, ParallelStateCommit<'_, DB>) {
        let Self {
            cache,
            database,
            transition_state,
            bundle_state: _,
            block_hashes,
            bal_state,
            state_hook,
            update_db_metrics,
            db_latency,
        } = self;
        let shared = ParallelStateView {
            cache,
            database,
            block_hashes,
            update_db_metrics: *update_db_metrics,
            db_latency,
        };
        (shared, ParallelStateCommit { shared, transition_state, bal_state, state_hook })
    }

    /// Enable EIP-7928 block access list construction.
    pub fn with_bal_builder(mut self) -> Self {
        self.bal_state = core::mem::take(&mut self.bal_state).with_bal_builder();
        self
    }

    /// Enable EIP-7928 block access list construction when `enabled` is true.
    pub fn with_bal_builder_if(self, enabled: bool) -> Self {
        if enabled { self.with_bal_builder() } else { self }
    }

    /// Set the EIP-7928 index used by the next canonical commit.
    pub const fn set_bal_index(&mut self, index: BlockAccessIndex) {
        self.bal_state.bal_index = index;
    }

    /// Advance the EIP-7928 index used by the next canonical commit.
    pub const fn bump_bal_index(&mut self) {
        self.bal_state.bump_bal_index();
    }

    /// Return the EIP-7928 index used by the next canonical commit.
    pub const fn bal_index(&self) -> BlockAccessIndex {
        self.bal_state.bal_index()
    }

    /// Take the constructed EIP-7928 block access list.
    pub fn take_built_alloy_bal(&mut self) -> Option<AlloyBal> {
        self.bal_state.take_built_alloy_bal()
    }

    /// Install or clear the canonical state commit hook.
    pub fn set_state_hook(&mut self, hook: Option<Box<dyn OnStateHook>>) {
        self.state_hook = hook;
    }

    /// Take the canonical state commit hook, leaving no hook installed.
    ///
    /// This is useful when an integration needs to perform metadata-only empty commits before
    /// restoring the same hook for later consensus state changes.
    pub fn take_state_hook(&mut self) -> Option<Box<dyn OnStateHook>> {
        self.state_hook.take()
    }

    /// Returns the size hint for the inner bundle state.
    /// See [BundleState::size_hint] for more info.
    pub fn bundle_size_hint(&self) -> usize {
        self.bundle_state.size_hint()
    }

    /// Iterate over received balances and increment all account balances.
    /// If account is not found inside cache state it will be loaded from database.
    ///
    /// Update will create transitions for all accounts that are updated.
    ///
    /// Like [`revm_database::states::CacheAccount::increment_balance`], this assumes that
    /// incremented balances are not zero, and will not overflow once incremented. If using this to
    /// implement withdrawals, zero balances must be filtered out before calling this function.
    pub fn increment_balances(
        &mut self,
        balances: impl IntoIterator<Item = (Address, u128)>,
    ) -> Result<(), DB::Error> {
        DatabaseCommitExt::increment_balances(self, balances)
    }

    /// Drain balances from given account and return those values.
    ///
    /// It is used for DAO hardfork state change to move values from given accounts.
    pub fn drain_balances(
        &mut self,
        addresses: impl IntoIterator<Item = Address>,
    ) -> Result<Vec<u128>, DB::Error> {
        DatabaseCommitExt::drain_balances(self, addresses)
    }

    /// Insert non-existent account
    pub fn insert_not_existing(&self, address: Address) {
        self.cache.insert_not_existing(address)
    }

    /// Insert account with specified `AccountInfo`
    pub fn insert_account(&self, address: Address, info: AccountInfo) {
        self.cache.insert_account(address, info)
    }

    /// Insert account with `AccountInfo` and `PlainStorage`
    pub fn insert_account_with_storage(
        &self,
        address: Address,
        info: AccountInfo,
        storage: PlainStorage,
    ) {
        self.cache.insert_account_with_storage(address, info, storage)
    }

    /// Add already constructed account transitions to the block transition state.
    ///
    /// This compatibility API is retained for integrations that applied
    /// [`ParallelCacheState::apply_evm_state`] separately. New code should prefer
    /// [`DatabaseCommit::commit`], which also updates the BAL builder and state hook.
    pub fn apply_transition(&mut self, transitions: Vec<(Address, TransitionAccount)>) {
        if let Some(state) = self.transition_state.as_mut() {
            state.add_transitions(
                transitions
                    .into_iter()
                    .map(|(address, transition)| (address, into_revm_transition(transition))),
            );
        }
    }

    /// Take all transitions and merge them inside bundle state.
    /// This action will create final post state and all reverts so that
    /// we at any time revert state of bundle to the state before transition
    /// is applied.
    pub fn merge_transitions(&mut self, retention: BundleRetention) {
        if let Some(transition_state) = self.transition_state.as_mut().map(TransitionState::take) {
            self.bundle_state.apply_transitions_and_create_reverts(transition_state, retention);
        }
    }

    // TODO make cache aware of transitions dropping by having global transition counter.
    /// Takes the accumulated [`BundleState`], replacing it with an empty one.
    ///
    /// This is a low-level, destructive operation: it does not apply or drain a pending
    /// [`TransitionState`]. Call [`crate::ParallelTakeBundle::parallel_take_bundle`] when producing
    /// a finalized block bundle; use this method directly only after transitions have already been
    /// merged.
    pub fn take_bundle(&mut self) -> BundleState {
        core::mem::take(&mut self.bundle_state)
    }

    // Database stuff
    fn db_basic(&self, address: Address) -> Result<Option<AccountInfo>, DB::Error> {
        self.shared_view().db_basic(address)
    }

    fn db_code_by_hash(&self, code_hash: B256) -> Result<Bytecode, DB::Error> {
        self.shared_view().db_code_by_hash(code_hash)
    }

    fn db_storage(&self, address: Address, index: U256) -> Result<U256, DB::Error> {
        self.shared_view().db_storage(address, index)
    }

    fn db_block_hash(&self, number: u64) -> Result<B256, DB::Error> {
        self.shared_view().db_block_hash(number)
    }
}

impl<DB: DatabaseRef> Database for ParallelState<DB> {
    type Error = DB::Error;

    fn basic(&mut self, address: Address) -> Result<Option<AccountInfo>, Self::Error> {
        self.db_basic(address)
    }

    fn code_by_hash(&mut self, code_hash: B256) -> Result<Bytecode, Self::Error> {
        self.db_code_by_hash(code_hash)
    }

    fn storage(&mut self, address: Address, index: U256) -> Result<U256, Self::Error> {
        self.db_storage(address, index)
    }

    fn block_hash(&mut self, number: u64) -> Result<B256, Self::Error> {
        self.db_block_hash(number)
    }
}

impl<DB: DatabaseRef> DatabaseRef for ParallelState<DB> {
    type Error = DB::Error;

    fn basic_ref(&self, address: Address) -> Result<Option<AccountInfo>, Self::Error> {
        self.db_basic(address)
    }

    fn code_by_hash_ref(&self, code_hash: B256) -> Result<Bytecode, Self::Error> {
        self.db_code_by_hash(code_hash)
    }

    fn storage_ref(&self, address: Address, index: U256) -> Result<U256, Self::Error> {
        self.db_storage(address, index)
    }

    fn block_hash_ref(&self, number: u64) -> Result<B256, Self::Error> {
        self.db_block_hash(number)
    }
}

impl<DB: DatabaseRef> DatabaseCommit for ParallelState<DB> {
    fn commit(&mut self, evm_state: EvmState) {
        commit_canonical_state(
            &self.cache,
            &mut self.transition_state,
            &mut self.bal_state,
            &mut self.state_hook,
            evm_state,
        );
    }
}

impl<DB: DatabaseRef> alloy_evm::block::BalIndexedDatabase for ParallelState<DB> {
    fn set_bal_index(&mut self, index: u64) {
        ParallelState::set_bal_index(self, BlockAccessIndex::new(index));
    }

    fn bump_bal_index(&mut self) {
        ParallelState::bump_bal_index(self);
    }
}
