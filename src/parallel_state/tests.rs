use super::*;
use revm_database::{CacheDB, EmptyDB, StateBuilder};
use revm_state::{Account, EvmStorageSlot, TransactionId};
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
fn account_insertions_replace_cached_storage() {
    let address = Address::with_last_byte(2);
    let first_slot = U256::from(1);
    let second_slot = U256::from(2);
    let info = AccountInfo { nonce: 1, ..Default::default() };
    let mut state = ParallelState::new(EmptyDB::default(), false, false);

    state.insert_account_with_storage(
        address,
        info.clone(),
        [(first_slot, U256::from(10))].into_iter().collect(),
    );
    assert_eq!(state.storage_ref(address, first_slot).unwrap(), U256::from(10));

    state.insert_account(address, info.clone());
    assert_eq!(state.storage_ref(address, first_slot).unwrap(), U256::ZERO);

    state.insert_account_with_storage(
        address,
        info,
        [(second_slot, U256::from(20))].into_iter().collect(),
    );
    assert_eq!(state.storage_ref(address, first_slot).unwrap(), U256::ZERO);
    assert_eq!(state.storage_ref(address, second_slot).unwrap(), U256::from(20));

    state.insert_not_existing(address);
    assert_eq!(state.basic_ref(address).unwrap(), None);
    assert_eq!(state.storage_ref(address, second_slot).unwrap(), U256::ZERO);
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

    for (index, changes) in [pre.clone(), transaction.clone(), post.clone()].into_iter().enumerate()
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
fn taking_bundle_finalizes_pending_transitions_with_revm_semantics() {
    let address = Address::with_last_byte(0x24);
    let changes = changed_account_state(address, 7, 9, 3, 4);
    let mut parallel = ParallelState::for_block(EmptyDB::default());
    let mut reference =
        StateBuilder::new().with_database(EmptyDB::default()).with_bundle_update().build();

    parallel.commit(changes.clone());
    reference.commit(changes);
    reference.merge_transitions(BundleRetention::Reverts);

    assert_eq!(
        parallel.take_bundle_with_retention(BundleRetention::Reverts),
        reference.take_bundle()
    );
}

#[test]
fn canonical_cache_only_persists_changed_storage_slots() {
    for created in [false, true] {
        let address = Address::with_last_byte(0x40 + u8::from(created));
        let mut account = Account::from(AccountInfo {
            balance: U256::from(1),
            nonce: 1,
            code: created.then(Bytecode::default),
            ..Default::default()
        });
        let transaction_id = TransactionId::new(1).unwrap();
        account.storage.insert(
            U256::from(1),
            EvmStorageSlot::new_changed(U256::from(2), U256::from(3), transaction_id),
        );
        account.storage.insert(U256::from(4), EvmStorageSlot::new(U256::from(5), transaction_id));
        account.mark_touch();
        if created {
            account.mark_created();
        }

        let mut state = ParallelState::new(EmptyDB::default(), true, false);
        state.commit([(address, account)].into_iter().collect());

        let storage = state.cache.storage.get(&address).unwrap();
        assert_eq!(storage.len(), 1);
        assert_eq!(*storage.get(&U256::from(1)).unwrap(), U256::from(3));
        assert!(!storage.contains_key(&U256::from(4)));
    }
}

#[test]
fn canonical_state_hook_can_be_suspended_and_restored() {
    let address = Address::with_last_byte(0x23);
    let ignored = changed_account_state(address, 7, 8, 3, 4);
    let observed_state = changed_account_state(address, 8, 9, 4, 5);
    let observed = Arc::new(StdMutex::new(Vec::new()));
    let hook_observed = observed.clone();
    let mut state = ParallelState::new(EmptyDB::default(), true, false);
    state.set_state_hook(Some(Box::new(move |state| {
        hook_observed.lock().unwrap().push(state);
    })));

    let hook = state.take_state_hook();
    state.commit(ignored);
    assert!(observed.lock().unwrap().is_empty());

    state.set_state_hook(hook);
    state.commit(observed_state.clone());
    assert_eq!(observed.lock().unwrap().as_slice(), &[observed_state]);
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
