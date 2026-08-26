use dashmap::{DashMap, Entry, mapref::one::RefMut};
use revm_database::{
    AccountStatus, CacheState, PlainAccount, StorageWithOriginalValues, TransitionAccount,
    states::{CacheAccount, StorageSlot, plain_account::PlainStorage},
};
use revm_primitives::{Address, B256, U256};
use revm_state::{
    Account, AccountInfo, Bytecode, EvmState, EvmStorage, EvmStorageSlot, TransactionId,
};
use std::borrow::Cow;

pub(super) type RevmTransition<'a> = TransitionAccount<Option<Cow<'a, EvmStorage>>>;

/// Cache state shared by speculative EVM workers and the ordered commit path.
#[derive(Clone, Debug, Default)]
pub struct ParallelCacheState {
    /// Cached accounts.
    pub(super) accounts: DashMap<Address, CacheAccountInfo>,
    /// Cached storage slots.
    pub(super) storage: DashMap<Address, DashMap<U256, U256>>,
    /// Cached contracts.
    pub(super) contracts: DashMap<B256, Bytecode>,
}

#[derive(Clone, Debug, Default)]
pub(super) struct CacheAccountInfo {
    pub(super) account: Option<AccountInfo>,
    pub(super) status: AccountStatus,
}

impl CacheAccountInfo {
    pub(super) fn new(account: Option<AccountInfo>, status: AccountStatus) -> Self {
        Self { account, status }
    }

    /// Consume self and make account as destroyed.
    ///
    /// Set account as None and set status to Destroyer or DestroyedAgain.
    fn selfdestruct<'a>(&mut self) -> Option<RevmTransition<'a>> {
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
    fn newly_created<'a>(&mut self, account: Cow<'a, Account>) -> RevmTransition<'a> {
        let previous_info = self.account.take();
        let previous_status = self.status;
        let (new_info, transition_storage) = match account {
            Cow::Borrowed(account) => (account.info.clone(), Cow::Borrowed(&account.storage)),
            Cow::Owned(account) => (account.info, Cow::Owned(account.storage)),
        };

        self.status = self.status.on_created();
        let transition = TransitionAccount {
            info: Some(new_info.clone()),
            status: self.status,
            previous_status,
            previous_info,
            storage: Some(transition_storage),
            storage_was_destroyed: false,
        };
        self.account = Some(new_info);
        transition
    }

    /// Touch empty account, related to EIP-161 state clear.
    fn touch_empty_eip161<'a>(&mut self) -> Option<RevmTransition<'a>> {
        let previous_info = self.account.take();
        let previous_status = self.status;

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

    fn change<'a>(&mut self, account: Cow<'a, Account>) -> RevmTransition<'a> {
        let previous_info = self.account.take();
        let previous_status = self.status;
        let (new_info, transition_storage) = match account {
            Cow::Borrowed(account) => (account.info.clone(), Cow::Borrowed(&account.storage)),
            Cow::Owned(account) => (account.info, Cow::Owned(account.storage)),
        };

        let had_no_nonce_and_code =
            previous_info.as_ref().map(AccountInfo::has_no_code_and_nonce).unwrap_or_default();
        self.status = self.status.on_changed(had_no_nonce_and_code);
        self.account = Some(new_info);

        TransitionAccount {
            info: self.account.clone(),
            status: self.status,
            previous_info,
            previous_status,
            storage: Some(transition_storage),
            storage_was_destroyed: false,
        }
    }
}

impl ParallelCacheState {
    /// Create an empty cache.
    pub fn new() -> Self {
        Self::default()
    }

    /// Copy the cached data into a revm [`CacheState`].
    pub fn as_cache_state(&self) -> CacheState {
        let mut state = CacheState::new();
        for entry in self.accounts.iter() {
            let info = entry.value();
            state.accounts.insert(
                *entry.key(),
                CacheAccount {
                    account: info
                        .account
                        .clone()
                        .map(|info| PlainAccount { info, storage: PlainStorage::default() }),
                    status: info.status,
                },
            );
        }
        for entry in self.contracts.iter() {
            state.contracts.insert(*entry.key(), entry.value().clone());
        }
        for entry in self.storage.iter() {
            let address = *entry.key();
            if let Some(account) = state.accounts.get_mut(&address) &&
                let Some(account) = account.account.as_mut()
            {
                for slot in entry.value().iter() {
                    account.storage.insert(*slot.key(), *slot.value());
                }
            }
        }
        state
    }

    /// Insert a non-existent account.
    ///
    /// This replaces any cached account and storage for `address`. Call it only while no execution
    /// worker is reading the cache.
    pub fn insert_not_existing(&self, address: Address) {
        self.storage.remove(&address);
        self.accounts
            .insert(address, CacheAccountInfo::new(None, AccountStatus::LoadedNotExisting));
    }

    /// Insert a loaded account.
    ///
    /// This replaces any cached account and storage for `address`. Call it only while no execution
    /// worker is reading the cache.
    pub fn insert_account(&self, address: Address, info: AccountInfo) {
        self.storage.remove(&address);
        self.insert_account_info(address, info);
    }

    fn insert_account_info(&self, address: Address, info: AccountInfo) {
        let account = if info.is_empty() {
            CacheAccountInfo::new(Some(AccountInfo::default()), AccountStatus::LoadedEmptyEIP161)
        } else {
            CacheAccountInfo::new(Some(info), AccountStatus::Loaded)
        };
        self.accounts.insert(address, account);
    }

    /// Insert a loaded account and its storage.
    ///
    /// This replaces any cached account and storage for `address`. Call it only while no execution
    /// worker is reading the cache.
    pub fn insert_account_with_storage(
        &self,
        address: Address,
        info: AccountInfo,
        storage: PlainStorage,
    ) {
        let slots = DashMap::new();
        for (slot, value) in storage {
            slots.insert(slot, value);
        }
        self.storage.insert(address, slots);
        self.insert_account_info(address, info);
    }

    /// Apply EVM output and return owned transitions.
    ///
    /// This compatibility API preserves the pre-REVM-42 GREVM interface. Canonical execution uses
    /// an internal streaming path to avoid allocating an intermediate vector.
    pub fn apply_evm_state(&mut self, evm_state: EvmState) -> Vec<(Address, TransitionAccount)> {
        let mut transitions = Vec::with_capacity(evm_state.len());
        self.apply_evm_state_with(
            evm_state.into_iter().map(|(address, account)| (address, Cow::Owned(account))),
            |address, transition| transitions.push((address, into_owned_transition(transition))),
        );
        transitions
    }

    pub(super) fn apply_evm_state_with<'a>(
        &self,
        evm_state: impl IntoIterator<Item = (Address, Cow<'a, Account>)>,
        mut on_transition: impl FnMut(Address, RevmTransition<'a>),
    ) {
        for (address, account) in evm_state {
            if let Some(transition) = self.apply_account_state(address, account) {
                on_transition(address, transition);
            }
        }
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
                let account = if account.is_loaded_as_not_existing() {
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
                entry.insert(account)
            }
        }
    }

    fn apply_account_state<'a>(
        &self,
        address: Address,
        account: Cow<'a, Account>,
    ) -> Option<RevmTransition<'a>> {
        if !account.is_touched() {
            return None;
        }

        let mut cached_account = self.get_or_insert_account_mut(address, &account);
        let transition = if account.is_selfdestructed() {
            self.storage.remove(&address);
            return cached_account.selfdestruct();
        } else if account.is_created() {
            let code_hash = account.info.code_hash;
            let code = account.info.code.clone().expect("created account must contain code");
            self.storage.remove(&address);
            let transition = cached_account.newly_created(account);
            self.contracts.entry(code_hash).or_insert(code);
            Some(transition)
        } else if account.is_empty() {
            // revm resolves fork-sensitive empty-account semantics before this commit layer. A
            // touched, empty, non-created account that reaches us must therefore be cleared.
            self.storage.remove(&address);
            cached_account.touch_empty_eip161()
        } else {
            Some(cached_account.change(account))
        };
        drop(cached_account);
        if let Some(storage) =
            transition.as_ref().and_then(|transition| transition.storage.as_ref())
        {
            self.update_changed_storage_slots(address, storage);
        }
        transition
    }

    fn update_changed_storage_slots(&self, address: Address, storage: &EvmStorage) {
        let mut changed = storage
            .iter()
            .filter_map(|(slot, value)| value.is_changed().then_some((*slot, value.present_value)));
        let Some((first_slot, first_value)) = changed.next() else { return };

        if let Some(slots) = self.storage.get(&address) {
            slots.insert(first_slot, first_value);
            for (slot, value) in changed {
                slots.insert(slot, value);
            }
            return;
        }

        match self.storage.entry(address) {
            Entry::Occupied(entry) => {
                entry.get().insert(first_slot, first_value);
                for (slot, value) in changed {
                    entry.get().insert(slot, value);
                }
            }
            Entry::Vacant(entry) => {
                let slots = DashMap::new();
                slots.insert(first_slot, first_value);
                for (slot, value) in changed {
                    slots.insert(slot, value);
                }
                entry.insert(slots);
            }
        }
    }
}

pub(super) fn into_revm_transition(transition: TransitionAccount) -> RevmTransition<'static> {
    transition.map_storage(|storage| {
        Some(Cow::Owned(
            storage
                .into_iter()
                .map(|(key, slot)| {
                    (
                        key,
                        EvmStorageSlot::new_changed(
                            slot.original_value(),
                            slot.present_value(),
                            TransactionId::ZERO,
                        ),
                    )
                })
                .collect(),
        ))
    })
}

fn into_owned_transition(transition: RevmTransition<'_>) -> TransitionAccount {
    transition.map_storage(|storage| {
        let mut changed = StorageWithOriginalValues::default();
        if let Some(storage) = storage {
            for (key, slot) in storage.iter() {
                if slot.is_changed() {
                    changed.insert(
                        *key,
                        StorageSlot::new_changed(slot.original_value, slot.present_value),
                    );
                }
            }
        }
        changed
    })
}
