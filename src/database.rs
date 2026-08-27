//! Database adapters for parallel execution.

use parking_lot::Mutex;
use revm::{
    DatabaseRef,
    bytecode::Bytecode,
    primitives::{Address, B256, U256},
    state::{AccountId, AccountInfo},
};
use std::fmt;

/// Serializes reads from a `Send` database for use by scheduler workers.
pub struct LockedDatabase<DB>(Mutex<DB>);

impl<DB> LockedDatabase<DB> {
    /// Wraps a database in a lock.
    pub const fn new(database: DB) -> Self {
        Self(Mutex::new(database))
    }
}

impl<DB> fmt::Debug for LockedDatabase<DB> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LockedDatabase").finish_non_exhaustive()
    }
}

impl<DB: DatabaseRef> DatabaseRef for LockedDatabase<DB> {
    type Error = DB::Error;

    fn basic_ref(&self, address: Address) -> Result<Option<AccountInfo>, Self::Error> {
        self.0.lock().basic_ref(address)
    }

    fn code_by_hash_ref(&self, code_hash: B256) -> Result<Bytecode, Self::Error> {
        self.0.lock().code_by_hash_ref(code_hash)
    }

    fn storage_ref(&self, address: Address, index: U256) -> Result<U256, Self::Error> {
        self.0.lock().storage_ref(address, index)
    }

    fn storage_by_account_id_ref(
        &self,
        address: Address,
        account_id: AccountId,
        index: U256,
    ) -> Result<U256, Self::Error> {
        self.0.lock().storage_by_account_id_ref(address, account_id, index)
    }

    fn block_hash_ref(&self, number: u64) -> Result<B256, Self::Error> {
        self.0.lock().block_hash_ref(number)
    }
}
