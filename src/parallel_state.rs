use core::hash::{BuildHasherDefault, Hasher};
use dashmap::{DashMap, Entry, mapref::one::RefMut};
use metrics::histogram;
use revm::{Database, DatabaseCommit, DatabaseRef, OnStateHook, database_interface::bal::BalState};
use revm_database::{
    AccountStatus, BundleState, CacheState, DatabaseCommitExt, PlainAccount, TransitionAccount,
    TransitionState,
    states::{CacheAccount, bundle_state::BundleRetention, plain_account::PlainStorage},
};
use revm_primitives::{Address, B256, U256};
use revm_state::{
    Account, AccountInfo, Bytecode, EvmState, EvmStorage,
    bal::{BlockAccessIndex, alloy::AlloyBal},
};
use std::{
    borrow::Cow,
    fmt::Formatter,
    time::{Duration, Instant},
    vec::Vec,
};

#[inline]
fn duration_micros(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1_000_000.0
}

type RevmTransition<'a> = TransitionAccount<Option<Cow<'a, EvmStorage>>>;

#[derive(Clone, Debug, Default)]
pub struct CacheAccountInfo {
    pub account: Option<AccountInfo>,
    pub status: AccountStatus,
}

impl CacheAccountInfo {
    pub fn new(account: Option<AccountInfo>, status: AccountStatus) -> Self {
        Self { account, status }
    }

    /// Consume self and make account as destroyed.
    ///
    /// Set account as None and set status to Destroyer or DestroyedAgain.
    pub fn selfdestruct<'a>(&mut self) -> Option<RevmTransition<'a>> {
        // account should be None after selfdestruct so we can take it.
        let previous_info = self.account.take();
        let previous_status = self.status;

        self.status = self.status.on_selfdestructed();

        if previous_status == AccountStatus::LoadedNotExisting {
            None
        } else {
            Some(TransitionAccount {
                info: None,
                status: self.status,
                previous_info,
                previous_status,
                storage: None,
                storage_was_destroyed: true,
            })
        }
    }

    /// Newly created account.
    pub fn newly_created<'a>(
        &mut self,
        account: Cow<'a, Account>,
    ) -> (RevmTransition<'a>, PlainStorage) {
        let previous_info = self.account.take();
        let previous_status = self.status;
        let new_bundle_storage =
            account.storage.iter().map(|(key, slot)| (*key, slot.present_value)).collect();
        let (new_info, new_storage) = match account {
            Cow::Borrowed(account) => (account.info.clone(), Some(Cow::Borrowed(&account.storage))),
            Cow::Owned(account) => (account.info, Some(Cow::Owned(account.storage))),
        };

        self.status = self.status.on_created();
        let transition_account = TransitionAccount {
            info: Some(new_info.clone()),
            status: self.status,
            previous_status,
            previous_info,
            storage: new_storage,
            storage_was_destroyed: false,
        };
        self.account = Some(new_info);
        (transition_account, new_bundle_storage)
    }

    /// Touch empty account, related to EIP-161 state clear.
    ///
    /// This account returns the Transition that is used to create the BundleState.
    pub fn touch_empty_eip161<'a>(&mut self) -> Option<RevmTransition<'a>> {
        // Set account to None.
        let previous_info = self.account.take();
        let previous_status = self.status;

        // Set account state to Destroyed as we need to clear the storage if it exist.
        self.status = self.status.on_touched_empty_post_eip161();

        if matches!(
            previous_status,
            AccountStatus::LoadedNotExisting |
                AccountStatus::Destroyed |
                AccountStatus::DestroyedAgain
        ) {
            None
        } else {
            Some(TransitionAccount {
                info: None,
                status: self.status,
                previous_info,
                previous_status,
                storage: None,
                storage_was_destroyed: true,
            })
        }
    }

    pub fn change<'a>(&mut self, account: Cow<'a, Account>) -> (RevmTransition<'a>, PlainStorage) {
        let previous_info = self.account.take();
        let previous_status = self.status;
        let new_bundle_storage =
            account.storage.iter().map(|(key, slot)| (*key, slot.present_value)).collect();
        let (new_info, storage) = match account {
            Cow::Borrowed(account) => (account.info.clone(), Some(Cow::Borrowed(&account.storage))),
            Cow::Owned(account) => (account.info, Some(Cow::Owned(account.storage))),
        };

        let had_no_nonce_and_code =
            previous_info.as_ref().map(AccountInfo::has_no_code_and_nonce).unwrap_or_default();
        self.status = self.status.on_changed(had_no_nonce_and_code);
        self.account = Some(new_info);

        (
            TransitionAccount {
                info: self.account.clone(),
                status: self.status,
                previous_info,
                previous_status,
                storage,
                storage_was_destroyed: false,
            },
            new_bundle_storage,
        )
    }
}

/// Cache state contains both modified and original values.
///
/// Cache state is main state that revm uses to access state.
/// It loads all accounts from database and applies revm output to it.
///
/// It generates transitions that is used to build BundleState.
#[derive(Clone, Debug, Default)]
pub struct ParallelCacheState {
    /// Cached accounts
    pub accounts: DashMap<Address, CacheAccountInfo>,
    /// Cached storage slots
    pub storage: DashMap<Address, DashMap<U256, U256>>,
    /// Cache contracts
    pub contracts: DashMap<B256, Bytecode>,
}

impl ParallelCacheState {
    /// New default state.
    pub fn new() -> Self {
        Self::default()
    }

    /// Copy the cached data and convert to CacheState
    pub fn as_cache_state(&self) -> CacheState {
        let mut state = CacheState::new();
        for kv in self.accounts.iter() {
            let info = kv.value();
            state.accounts.insert(
                *kv.key(),
                CacheAccount {
                    account: info
                        .account
                        .clone()
                        .map(|info| PlainAccount { info, storage: PlainStorage::default() }),
                    status: info.status,
                },
            );
        }
        for kv in self.contracts.iter() {
            state.contracts.insert(*kv.key(), kv.value().clone());
        }
        for kv in self.storage.iter() {
            let address = *kv.key();
            let slots = kv.value();
            if let Some(account) = state.accounts.get_mut(&address) &&
                let Some(plain_account) = account.account.as_mut()
            {
                for slot_value in slots.iter() {
                    plain_account.storage.insert(*slot_value.key(), *slot_value.value());
                }
            }
        }
        state
    }

    /// Insert not existing account.
    pub fn insert_not_existing(&self, address: Address) {
        self.accounts
            .insert(address, CacheAccountInfo::new(None, AccountStatus::LoadedNotExisting));
    }

    /// Insert Loaded (Or LoadedEmptyEip161 if account is empty) account.
    pub fn insert_account(&self, address: Address, info: AccountInfo) {
        let account = if !info.is_empty() {
            CacheAccountInfo::new(Some(info), AccountStatus::Loaded)
        } else {
            CacheAccountInfo::new(Some(AccountInfo::default()), AccountStatus::LoadedEmptyEIP161)
        };
        self.accounts.insert(address, account);
    }

    /// Similar to `insert_account` but with storage.
    pub fn insert_account_with_storage(
        &self,
        address: Address,
        info: AccountInfo,
        storage: PlainStorage,
    ) {
        self.update_storage_slot(address, storage);
        self.insert_account(address, info);
    }

    fn apply_evm_state_inner(
        &self,
        evm_state: EvmState,
    ) -> Vec<(Address, RevmTransition<'static>)> {
        let mut transitions = Vec::with_capacity(evm_state.len());
        for (address, account) in evm_state {
            if let Some(transition) = self.apply_account_state(address, Cow::Owned(account)) {
                transitions.push((address, transition));
            }
        }
        transitions
    }

    fn apply_evm_state_ref<'a>(
        &self,
        evm_state: &'a EvmState,
    ) -> Vec<(Address, RevmTransition<'a>)> {
        let mut transitions = Vec::with_capacity(evm_state.len());
        for (address, account) in evm_state {
            if let Some(transition) = self.apply_account_state(*address, Cow::Borrowed(account)) {
                transitions.push((*address, transition));
            }
        }
        transitions
    }

    fn get_or_insert_account_mut(
        &'_ self,
        address: Address,
        account: &Account,
    ) -> RefMut<'_, Address, CacheAccountInfo> {
        match self.accounts.entry(address) {
            Entry::Occupied(entry) => entry.into_ref(),
            Entry::Vacant(entry) => {
                // A canonical commit can originate from an EVM backed by a different database.
                // Preserve its original account metadata just as revm's CacheState does.
                let cache_account = if account.is_loaded_as_not_existing() {
                    CacheAccountInfo::new(None, AccountStatus::LoadedNotExisting)
                } else {
                    let original = account.original_info();
                    if original.is_empty() {
                        CacheAccountInfo::new(
                            Some(AccountInfo::default()),
                            AccountStatus::LoadedEmptyEIP161,
                        )
                    } else {
                        CacheAccountInfo::new(Some(original), AccountStatus::Loaded)
                    }
                };
                entry.insert(cache_account)
            }
        }
    }

    /// Apply updated account state to the cached account.
    /// Returns account transition if applicable.
    fn apply_account_state<'a>(
        &self,
        address: Address,
        account: Cow<'a, Account>,
    ) -> Option<RevmTransition<'a>> {
        if !account.is_touched() {
            return None;
        }
        let mut cached_account = self.get_or_insert_account_mut(address, &account);
        let (transition, changed_slots) = {
            // If it is marked as selfdestructed inside revm
            // we need to changed state to destroyed.
            if account.is_selfdestructed() {
                self.storage.remove(&address);
                return cached_account.selfdestruct();
            }

            // Note: it can happen that created contract get selfdestructed in same block
            // that is why is_created is checked after selfdestructed
            //
            // Note: Create2 opcode (Petersburg) was after state clear EIP (Spurious Dragon)
            //
            // Note: It is possibility to create KECCAK_EMPTY contract with some storage
            // by just setting storage inside CRATE constructor. Overlap of those contracts
            // is not possible because CREATE2 is introduced later.
            if account.is_created() {
                let code_hash = account.info.code_hash;
                let code = account.info.code.clone().expect("created account must contain code");
                self.storage.remove(&address);
                let (transition, changed_slots) = cached_account.newly_created(account);
                self.contracts.entry(code_hash).or_insert(code);
                (Some(transition), Some(changed_slots))
            }
            // Account is touched, but not selfdestructed or newly created.
            // Account can be touched and not changed.
            // And when empty account is touched it needs to be removed from database.
            // revm v40+ normalizes pre-EIP-161 empty-account semantics in the journal's
            // `finalize()`: newly materialized empty accounts are marked as created, while
            // pre-existing empty accounts are unmarked as touched. Therefore, an account that
            // reaches the commit layer as touched, empty, and not created must be cleared.
            else if account.is_empty() {
                self.storage.remove(&address);
                (cached_account.touch_empty_eip161(), None)
            } else {
                let (transition, changed_slots) = cached_account.change(account);
                (Some(transition), Some(changed_slots))
            }
        };
        drop(cached_account);
        if let Some(changed_slots) = changed_slots &&
            !changed_slots.is_empty()
        {
            self.update_storage_slot(address, changed_slots);
        }
        transition
    }

    fn update_storage_slot(&self, address: Address, storage: PlainStorage) {
        if let Some(slots) = self.storage.get(&address) {
            for (slot, value) in storage {
                slots.insert(slot, value);
            }
        } else {
            match self.storage.entry(address) {
                Entry::Occupied(entry) => {
                    for (slot, value) in storage.into_iter() {
                        entry.get().insert(slot, value);
                    }
                }
                Entry::Vacant(entry) => {
                    let new_storage = DashMap::new();
                    for (slot, value) in storage.into_iter() {
                        new_storage.insert(slot, value);
                    }
                    entry.insert(new_storage);
                }
            };
        }
    }
}

#[derive(Debug, Default)]
pub struct IdentityHasher(u64);
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
pub(crate) type BuildIdentityHasher = BuildHasherDefault<IdentityHasher>;

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
    pub cache: ParallelCacheState,
    /// Optional database that we use to fetch data from. If database is not present, we will
    /// return not existing account and storage.
    ///
    /// Note: It is marked as Send so database can be shared between threads.
    pub database: DB,
    /// Block state, it aggregates transactions transitions into one state.
    ///
    /// Build reverts and state that gets applied to the state.
    pub transition_state: Option<TransitionState>,
    /// After block is finishes we merge those changes inside bundle.
    /// Bundle is used to update database and create changesets.
    /// Bundle state can be set on initialization if we want to use preloaded bundle.
    pub bundle_state: BundleState,
    /// If EVM asks for block hash we will first check if they are found here.
    /// and then ask the database.
    ///
    /// This map can be used to give different values for block hashes if in case
    /// The fork block is different or some blocks are not saved inside database.
    pub block_hashes: DashMap<u64, B256, BuildIdentityHasher>,
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

    fn load_mut_cache_account(
        self,
        address: Address,
    ) -> Result<RefMut<'a, Address, CacheAccountInfo>, DB::Error> {
        if let Some(account) = self.cache.accounts.get_mut(&address) {
            return Ok(account);
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
            Entry::Vacant(entry) => Ok(entry.insert(account)),
            Entry::Occupied(entry) => Ok(entry.into_ref()),
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
        let transitions = cache.apply_evm_state_ref(&evm_state);
        if let Some(state) = transition_state.as_mut() {
            state.add_transitions(transitions);
        }
        hook.on_state(evm_state);
    } else {
        let transitions = cache.apply_evm_state_inner(evm_state);
        if let Some(state) = transition_state.as_mut() {
            state.add_transitions(transitions);
        }
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
    /// Like [CacheAccount::increment_balance], this assumes that incremented balances are not
    /// zero, and will not overflow once incremented. If using this to implement withdrawals, zero
    /// balances must be filtered out before calling this function.
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

    /// Take all transitions and merge them inside bundle state.
    /// This action will create final post state and all reverts so that
    /// we at any time revert state of bundle to the state before transition
    /// is applied.
    pub fn merge_transitions(&mut self, retention: BundleRetention) {
        if let Some(transition_state) = self.transition_state.as_mut().map(TransitionState::take) {
            self.bundle_state.apply_transitions_and_create_reverts(transition_state, retention);
        }
    }

    /// Get a mutable reference to the [`CacheAccount`] for the given address.
    /// If the account is not found in the cache, it will be loaded from the
    /// database and inserted into the cache.
    pub fn load_mut_cache_account(
        &self,
        address: Address,
    ) -> Result<RefMut<'_, Address, CacheAccountInfo>, DB::Error> {
        self.shared_view().load_mut_cache_account(address)
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

#[cfg(test)]
mod tests {
    use super::*;
    use revm_database::{CacheDB, EmptyDB, StateBuilder};
    use revm_state::{EvmStorageSlot, TransactionId};
    use std::sync::{Arc, Mutex as StdMutex};

    fn changed_account_state(
        address: Address,
        original_balance: u64,
        present_balance: u64,
        original_storage: u64,
        present_storage: u64,
    ) -> EvmState {
        let mut account = Account::from(AccountInfo {
            balance: U256::from(original_balance),
            nonce: 1,
            ..Default::default()
        });
        account.info.balance = U256::from(present_balance);
        account.storage.insert(
            U256::from(1),
            EvmStorageSlot::new_changed(
                U256::from(original_storage),
                U256::from(present_storage),
                TransactionId::new(1).unwrap(),
            ),
        );
        account.mark_touch();
        [(address, account)].into_iter().collect()
    }

    #[test]
    fn duration_micros_preserves_sub_microsecond_precision() {
        assert_eq!(duration_micros(Duration::from_nanos(1_500)), 1.5);
        assert_eq!(duration_micros(Duration::from_secs(2)), 2_000_000.0);
    }

    #[test]
    fn storage_ref_reads_uncached_existing_account() {
        let address = Address::with_last_byte(1);
        let index = U256::from(2);
        let expected = U256::from(3);
        let mut database = CacheDB::<EmptyDB>::default();
        let account = AccountInfo { nonce: 1, ..Default::default() };
        database.insert_account_info(address, account);
        database.insert_account_storage(address, index, expected).unwrap();
        let state = ParallelState::new(database, false, false);

        assert_eq!(state.storage_ref(address, index).unwrap(), expected);
        assert!(!state.cache.accounts.contains_key(&address));
        assert_eq!(*state.cache.storage.get(&address).unwrap().get(&index).unwrap(), expected);
    }

    #[test]
    fn storage_ref_reads_uncached_nonexistent_account() {
        let address = Address::with_last_byte(1);
        let state = ParallelState::new(EmptyDB::default(), false, false);

        assert_eq!(state.storage_ref(address, U256::ZERO).unwrap(), U256::ZERO);
        assert!(!state.cache.accounts.contains_key(&address));
    }

    #[test]
    fn canonical_commits_match_revm_bal_and_reach_state_hook_once() {
        let address = Address::with_last_byte(0x11);
        let pre = changed_account_state(address, 10, 11, 1, 1);
        let transaction = changed_account_state(address, 11, 14, 1, 2);
        let post = changed_account_state(address, 14, 15, 2, 2);

        let observed = Arc::new(StdMutex::new(Vec::new()));
        let hook_observed = observed.clone();
        let mut parallel = ParallelState::new(EmptyDB::default(), true, false).with_bal_builder();
        parallel.set_state_hook(Some(Box::new(move |state| {
            hook_observed.lock().unwrap().push(state);
        })));

        let mut reference = StateBuilder::new()
            .with_database(EmptyDB::default())
            .with_bundle_update()
            .with_bal_builder()
            .build();

        for (index, changes) in
            [pre.clone(), transaction.clone(), post.clone()].into_iter().enumerate()
        {
            let index = BlockAccessIndex::new(index as u64);
            parallel.set_bal_index(index);
            reference.set_bal_index(index);
            parallel.commit(changes.clone());
            reference.commit(changes);
        }

        assert_eq!(parallel.take_built_alloy_bal(), reference.take_built_alloy_bal());
        assert_eq!(observed.lock().unwrap().as_slice(), &[pre, transaction, post]);
    }

    #[test]
    fn canonical_commit_accepts_state_from_an_uncached_evm() {
        let address = Address::with_last_byte(0x22);
        let changes = changed_account_state(address, 7, 9, 3, 4);
        let mut state = ParallelState::new(EmptyDB::default(), true, false);

        state.commit(changes);

        let info = state.basic_ref(address).unwrap().expect("committed account");
        assert_eq!(info.balance, U256::from(9));
        assert_eq!(state.storage_ref(address, U256::from(1)).unwrap(), U256::from(4));
    }

    #[test]
    fn balance_increments_use_the_canonical_bal_and_hook_commit_path() {
        let address = Address::with_last_byte(0x33);
        let info = AccountInfo { balance: U256::from(5), nonce: 1, ..Default::default() };
        let observed = Arc::new(StdMutex::new(Vec::new()));
        let hook_observed = observed.clone();

        let mut parallel = ParallelState::new(EmptyDB::default(), true, false).with_bal_builder();
        parallel.insert_account(address, info.clone());
        parallel.set_state_hook(Some(Box::new(move |state| {
            hook_observed.lock().unwrap().push(state);
        })));

        let mut reference = StateBuilder::new()
            .with_database(EmptyDB::default())
            .with_bundle_update()
            .with_bal_builder()
            .build();
        reference.insert_account(address, info);

        parallel.increment_balances([(address, 8)]).unwrap();
        reference.increment_balances([(address, 8)]).unwrap();

        assert_eq!(parallel.take_built_alloy_bal(), reference.take_built_alloy_bal());
        assert_eq!(observed.lock().unwrap().len(), 1);
        assert_eq!(parallel.basic_ref(address).unwrap().unwrap().balance, U256::from(13));
    }
}
