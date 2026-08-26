//! Concurrent parent-state database primitives.
//!
//! Parallel execution needs two properties that a typical storage provider does not offer on its
//! own: each worker needs an independent database handle, while identical cold reads should be
//! coalesced across workers. [`ConcurrentDatabase`] combines a [`DatabaseFactory`] with a shared
//! [`ReadCache`] to provide both properties without coupling Grevm to a particular storage engine.

use dashmap::{DashMap, mapref::entry::Entry};
use parking_lot::Mutex;
use revm::{DatabaseRef, database_interface::DBErrorMarker};
use revm_primitives::{Address, B256, U256};
use revm_state::{AccountId, AccountInfo, Bytecode};
use std::{
    fmt,
    hash::Hash,
    sync::{Arc, OnceLock},
    thread::{self, ThreadId},
};

/// A concurrent database backed by one lazily-created database handle per calling thread.
///
/// Clones share the factory, thread-local handles, and read cache. Database handles are retained
/// until [`Self::clear_thread_databases`] is called or the last clone is dropped. Integrations that
/// execute multiple batches should clear handles after all workers for a batch have joined; cached
/// parent-state reads remain available to later batches.
pub struct ConcurrentDatabase<F>
where
    F: DatabaseFactory,
{
    inner: Arc<ConcurrentDatabaseInner<F>>,
}

impl<F> ConcurrentDatabase<F>
where
    F: DatabaseFactory,
{
    /// Creates a concurrent database with an empty read cache.
    pub fn new(factory: F) -> Self {
        Self::with_cache(factory, ReadCache::new())
    }

    /// Creates a concurrent database backed by `cache`.
    ///
    /// This is useful for seeding reads retained by a previous execution attempt.
    pub fn with_cache(factory: F, cache: ReadCache<F::Error>) -> Self {
        Self {
            inner: Arc::new(ConcurrentDatabaseInner { factory, databases: DashMap::new(), cache }),
        }
    }

    /// Returns the shared parent-state read cache.
    pub fn cache(&self) -> &ReadCache<F::Error> {
        &self.inner.cache
    }

    /// Returns the number of retained per-thread database handles.
    pub fn thread_database_count(&self) -> usize {
        self.inner.databases.len()
    }

    /// Clears all retained per-thread database handles and returns how many entries were removed.
    ///
    /// Call this after the worker threads using the database have joined. Clearing concurrently
    /// with active reads is memory-safe, but an in-flight handle remains alive until its read ends
    /// and a later read on the same thread can create a second handle.
    pub fn clear_thread_databases(&self) -> usize {
        let count = self.inner.databases.len();
        self.inner.databases.clear();
        count
    }

    fn with_database<T>(
        &self,
        f: impl FnOnce(&F::Database) -> Result<T, F::Error>,
    ) -> Result<T, F::Error> {
        let thread_id = thread::current().id();
        let database = self.inner.databases.get(&thread_id).map(|entry| entry.value().clone());
        let database = if let Some(database) = database {
            database
        } else {
            // Creating a database can acquire a storage read transaction. Keep that operation
            // outside the DashMap shard lock so unrelated workers never wait behind storage I/O.
            let database = Arc::new(Mutex::new(self.inner.factory.create()?));
            match self.inner.databases.entry(thread_id) {
                Entry::Occupied(entry) => entry.get().clone(),
                Entry::Vacant(entry) => {
                    entry.insert(database.clone());
                    database
                }
            }
        };
        let database = database.lock();
        f(&database)
    }
}

impl<F> Clone for ConcurrentDatabase<F>
where
    F: DatabaseFactory,
{
    fn clone(&self) -> Self {
        Self { inner: self.inner.clone() }
    }
}

impl<F> fmt::Debug for ConcurrentDatabase<F>
where
    F: DatabaseFactory,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ConcurrentDatabase")
            .field("thread_databases", &self.thread_database_count())
            .field("cache", self.cache())
            .finish_non_exhaustive()
    }
}

impl<F> DatabaseRef for ConcurrentDatabase<F>
where
    F: DatabaseFactory,
{
    type Error = F::Error;

    fn basic_ref(&self, address: Address) -> Result<Option<AccountInfo>, Self::Error> {
        self.inner
            .cache
            .load_account(address, || self.with_database(|database| database.basic_ref(address)))
    }

    fn code_by_hash_ref(&self, code_hash: B256) -> Result<Bytecode, Self::Error> {
        self.inner.cache.load_code(code_hash, || {
            self.with_database(|database| database.code_by_hash_ref(code_hash))
        })
    }

    fn storage_ref(&self, address: Address, index: U256) -> Result<U256, Self::Error> {
        self.inner.cache.load_storage(address, index, || {
            self.with_database(|database| database.storage_ref(address, index))
        })
    }

    fn storage_by_account_id_ref(
        &self,
        address: Address,
        account_id: AccountId,
        index: U256,
    ) -> Result<U256, Self::Error> {
        self.inner.cache.load_storage(address, index, || {
            self.with_database(|database| {
                database.storage_by_account_id_ref(address, account_id, index)
            })
        })
    }

    fn block_hash_ref(&self, number: u64) -> Result<B256, Self::Error> {
        self.inner.cache.load_block_hash(number, || {
            self.with_database(|database| database.block_hash_ref(number))
        })
    }
}

/// Creates independent database handles for parallel execution workers.
///
/// A created database only needs to be [`Send`], not [`Sync`]: [`ConcurrentDatabase`] serializes
/// access to each handle and assigns handles by calling thread. Factory and database errors use the
/// same type so a creation failure can be returned through [`DatabaseRef`].
pub trait DatabaseFactory: Send + Sync {
    /// Database created for one worker thread.
    type Database: DatabaseRef<Error = Self::Error> + Send;
    /// Database creation and read error.
    type Error: DBErrorMarker + Clone;

    /// Creates a new database handle.
    fn create(&self) -> Result<Self::Database, Self::Error>;
}

impl<F, DB, E> DatabaseFactory for F
where
    F: Fn() -> Result<DB, E> + Send + Sync,
    DB: DatabaseRef<Error = E> + Send,
    E: DBErrorMarker + Clone,
{
    type Database = DB;
    type Error = E;

    fn create(&self) -> Result<Self::Database, Self::Error> {
        self()
    }
}

/// Shared, key-level single-flight cache for parent-state database reads.
///
/// Both successful reads and errors are cached. This gives every worker a coherent result for a
/// key during one execution attempt and prevents a failing storage backend from being hammered by
/// all workers. Export iterators yield only successful reads; integrations can retain them across
/// execution attempts without persisting transient errors. DashMap iteration is weakly consistent,
/// so integrations should export reads only after all execution workers using this cache have
/// joined.
pub struct ReadCache<E> {
    inner: Arc<ReadCacheInner<E>>,
}

impl<E> ReadCache<E> {
    /// Creates an empty read cache.
    pub fn new() -> Self {
        Self { inner: Arc::new(ReadCacheInner::default()) }
    }

    /// Seeds an account read if the address is not already cached.
    pub fn insert_account(&self, address: Address, account: Option<AccountInfo>) -> bool {
        insert_cached(&self.inner.accounts, address, account)
    }

    /// Seeds a bytecode read if the code hash is not already cached.
    pub fn insert_code(&self, code_hash: B256, code: Bytecode) -> bool {
        insert_cached(&self.inner.contracts, code_hash, code)
    }

    /// Seeds a storage read if the address and slot are not already cached.
    pub fn insert_storage(&self, address: Address, index: U256, value: U256) -> bool {
        insert_cached(&self.inner.storage, (address, index), value)
    }

    /// Seeds a block-hash read if the block number is not already cached.
    pub fn insert_block_hash(&self, number: u64, block_hash: B256) -> bool {
        insert_cached(&self.inner.block_hashes, number, block_hash)
    }

    /// Iterates over successfully cached account reads.
    pub fn account_reads(&self) -> impl Iterator<Item = (Address, Option<AccountInfo>)> + '_ {
        successful_reads(&self.inner.accounts)
    }

    /// Iterates over successfully cached bytecode reads.
    pub fn code_reads(&self) -> impl Iterator<Item = (B256, Bytecode)> + '_ {
        successful_reads(&self.inner.contracts)
    }

    /// Iterates over successfully cached storage reads.
    pub fn storage_reads(&self) -> impl Iterator<Item = ((Address, U256), U256)> + '_ {
        successful_reads(&self.inner.storage)
    }

    /// Iterates over successfully cached block-hash reads.
    pub fn block_hash_reads(&self) -> impl Iterator<Item = (u64, B256)> + '_ {
        successful_reads(&self.inner.block_hashes)
    }
}

impl<E> Clone for ReadCache<E> {
    fn clone(&self) -> Self {
        Self { inner: self.inner.clone() }
    }
}

impl<E> Default for ReadCache<E> {
    fn default() -> Self {
        Self::new()
    }
}

impl<E> fmt::Debug for ReadCache<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ReadCache")
            .field("accounts", &self.inner.accounts.len())
            .field("contracts", &self.inner.contracts.len())
            .field("storage", &self.inner.storage.len())
            .field("block_hashes", &self.inner.block_hashes.len())
            .finish()
    }
}

impl<E> ReadCache<E>
where
    E: Clone,
{
    fn load_account(
        &self,
        address: Address,
        load: impl FnOnce() -> Result<Option<AccountInfo>, E>,
    ) -> Result<Option<AccountInfo>, E> {
        cached(&self.inner.accounts, address, load)
    }

    fn load_code(
        &self,
        code_hash: B256,
        load: impl FnOnce() -> Result<Bytecode, E>,
    ) -> Result<Bytecode, E> {
        cached(&self.inner.contracts, code_hash, load)
    }

    fn load_storage(
        &self,
        address: Address,
        index: U256,
        load: impl FnOnce() -> Result<U256, E>,
    ) -> Result<U256, E> {
        cached(&self.inner.storage, (address, index), load)
    }

    fn load_block_hash(
        &self,
        number: u64,
        load: impl FnOnce() -> Result<B256, E>,
    ) -> Result<B256, E> {
        cached(&self.inner.block_hashes, number, load)
    }
}

type CachedResult<T, E> = Arc<OnceLock<Result<T, E>>>;

struct ConcurrentDatabaseInner<F>
where
    F: DatabaseFactory,
{
    factory: F,
    databases: DashMap<ThreadId, Arc<Mutex<F::Database>>>,
    cache: ReadCache<F::Error>,
}

struct ReadCacheInner<E> {
    accounts: DashMap<Address, CachedResult<Option<AccountInfo>, E>>,
    contracts: DashMap<B256, CachedResult<Bytecode, E>>,
    storage: DashMap<(Address, U256), CachedResult<U256, E>>,
    block_hashes: DashMap<u64, CachedResult<B256, E>>,
}

impl<E> Default for ReadCacheInner<E> {
    fn default() -> Self {
        Self {
            accounts: DashMap::new(),
            contracts: DashMap::new(),
            storage: DashMap::new(),
            block_hashes: DashMap::new(),
        }
    }
}

fn initialized<T, E>(value: T) -> CachedResult<T, E> {
    let cell = OnceLock::new();
    cell.set(Ok(value)).ok().expect("a new OnceLock is empty");
    Arc::new(cell)
}

fn insert_cached<K, V, E>(cache: &DashMap<K, CachedResult<V, E>>, key: K, value: V) -> bool
where
    K: Eq + Hash,
{
    match cache.entry(key) {
        Entry::Occupied(_) => false,
        Entry::Vacant(entry) => {
            entry.insert(initialized(value));
            true
        }
    }
}

fn cached<K, V, E>(
    cache: &DashMap<K, CachedResult<V, E>>,
    key: K,
    load: impl FnOnce() -> Result<V, E>,
) -> Result<V, E>
where
    K: Eq + Hash,
    V: Clone,
    E: Clone,
{
    let cell = cache.entry(key).or_insert_with(|| Arc::new(OnceLock::new())).clone();
    cell.get_or_init(load).clone()
}

fn successful_reads<K, V, E>(
    cache: &DashMap<K, CachedResult<V, E>>,
) -> impl Iterator<Item = (K, V)> + '_
where
    K: Copy + Eq + Hash,
    V: Clone,
{
    cache.iter().filter_map(|entry| {
        let value = entry.value().get()?.as_ref().ok()?.clone();
        Some((*entry.key(), value))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        error::Error,
        sync::{
            Barrier,
            atomic::{AtomicUsize, Ordering},
        },
    };

    const TEST_THREADS: usize = 8;

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    struct TestError;

    impl fmt::Display for TestError {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("test database error")
        }
    }

    impl Error for TestError {}
    impl DBErrorMarker for TestError {}

    struct TestDatabase {
        reads: Arc<AtomicUsize>,
        drops: Arc<AtomicUsize>,
        fail: bool,
    }

    impl Drop for TestDatabase {
        fn drop(&mut self) {
            self.drops.fetch_add(1, Ordering::Relaxed);
        }
    }

    impl DatabaseRef for TestDatabase {
        type Error = TestError;

        fn basic_ref(&self, address: Address) -> Result<Option<AccountInfo>, Self::Error> {
            self.reads.fetch_add(1, Ordering::Relaxed);
            if self.fail {
                Err(TestError)
            } else {
                Ok(Some(AccountInfo { nonce: address.as_slice()[0] as u64, ..Default::default() }))
            }
        }

        fn code_by_hash_ref(&self, _code_hash: B256) -> Result<Bytecode, Self::Error> {
            self.reads.fetch_add(1, Ordering::Relaxed);
            Ok(Bytecode::default())
        }

        fn storage_ref(&self, _address: Address, index: U256) -> Result<U256, Self::Error> {
            self.reads.fetch_add(1, Ordering::Relaxed);
            Ok(index)
        }

        fn block_hash_ref(&self, number: u64) -> Result<B256, Self::Error> {
            self.reads.fetch_add(1, Ordering::Relaxed);
            let mut block_hash = [0; 32];
            block_hash[24..].copy_from_slice(&number.to_be_bytes());
            Ok(B256::from(block_hash))
        }
    }

    fn test_database(
        fail: bool,
    ) -> (
        ConcurrentDatabase<impl DatabaseFactory<Database = TestDatabase, Error = TestError>>,
        Arc<AtomicUsize>,
        Arc<AtomicUsize>,
        Arc<AtomicUsize>,
    ) {
        let creates = Arc::new(AtomicUsize::new(0));
        let reads = Arc::new(AtomicUsize::new(0));
        let drops = Arc::new(AtomicUsize::new(0));
        let factory_creates = creates.clone();
        let factory_reads = reads.clone();
        let factory_drops = drops.clone();
        let database = ConcurrentDatabase::new(move || {
            factory_creates.fetch_add(1, Ordering::Relaxed);
            Ok::<_, TestError>(TestDatabase {
                reads: factory_reads.clone(),
                drops: factory_drops.clone(),
                fail,
            })
        });
        (database, creates, reads, drops)
    }

    #[test]
    fn concurrent_reads_of_one_key_are_single_flight() {
        let (database, creates, reads, _) = test_database(false);
        let barrier = Arc::new(Barrier::new(TEST_THREADS));
        let address = Address::from([0x11; 20]);

        thread::scope(|scope| {
            for _ in 0..TEST_THREADS {
                let database = database.clone();
                let barrier = barrier.clone();
                scope.spawn(move || {
                    barrier.wait();
                    assert_eq!(database.basic_ref(address).unwrap().unwrap().nonce, 0x11);
                });
            }
        });

        assert_eq!(reads.load(Ordering::Relaxed), 1);
        assert_eq!(creates.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn errors_are_single_flight_and_not_exported() {
        let (database, creates, reads, _) = test_database(true);
        let address = Address::from([0x22; 20]);

        for _ in 0..TEST_THREADS {
            assert_eq!(database.basic_ref(address), Err(TestError));
        }

        assert_eq!(reads.load(Ordering::Relaxed), 1);
        assert_eq!(creates.load(Ordering::Relaxed), 1);
        assert_eq!(database.cache().account_reads().count(), 0);
    }

    #[test]
    fn clearing_thread_databases_releases_worker_handles() {
        let (database, creates, _, drops) = test_database(false);
        let barrier = Arc::new(Barrier::new(TEST_THREADS));

        thread::scope(|scope| {
            for index in 0..TEST_THREADS {
                let database = database.clone();
                let barrier = barrier.clone();
                scope.spawn(move || {
                    barrier.wait();
                    let address = Address::from([(index + 1) as u8; 20]);
                    database.basic_ref(address).unwrap();
                });
            }
        });

        assert_eq!(creates.load(Ordering::Relaxed), TEST_THREADS);
        assert_eq!(database.thread_database_count(), TEST_THREADS);
        assert_eq!(database.clear_thread_databases(), TEST_THREADS);
        assert_eq!(database.thread_database_count(), 0);
        assert_eq!(drops.load(Ordering::Relaxed), TEST_THREADS);
        assert_eq!(database.cache().account_reads().count(), TEST_THREADS);
    }

    #[test]
    fn seeded_reads_are_exported_without_opening_a_database() {
        let (database, creates, reads, _) = test_database(false);
        let address = Address::from([0x33; 20]);
        let code_hash = B256::from([0x44; 32]);
        let block_hash = B256::from([0x55; 32]);
        let account = AccountInfo { nonce: 7, ..Default::default() };

        assert!(database.cache().insert_account(address, Some(account.clone())));
        assert!(database.cache().insert_code(code_hash, Bytecode::default()));
        assert!(database.cache().insert_storage(address, U256::from(1), U256::from(2)));
        assert!(database.cache().insert_block_hash(3, block_hash));
        assert!(!database.cache().insert_account(address, None));

        assert_eq!(database.basic_ref(address), Ok(Some(account.clone())));
        assert_eq!(database.storage_ref(address, U256::from(1)), Ok(U256::from(2)));
        assert_eq!(database.block_hash_ref(3), Ok(block_hash));
        assert_eq!(creates.load(Ordering::Relaxed), 0);
        assert_eq!(reads.load(Ordering::Relaxed), 0);

        assert_eq!(
            database.cache().account_reads().collect::<Vec<_>>(),
            vec![(address, Some(account))]
        );
        assert_eq!(
            database.cache().storage_reads().collect::<Vec<_>>(),
            vec![((address, U256::from(1)), U256::from(2))]
        );
        assert_eq!(database.cache().block_hash_reads().collect::<Vec<_>>(), vec![(3, block_hash)]);
        assert_eq!(database.cache().code_reads().count(), 1);
    }
}
