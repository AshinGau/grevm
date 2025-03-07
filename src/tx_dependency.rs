use crate::TxId;
use ahash::AHashSet as HashSet;
use parking_lot::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

static EXECUTE_DISTANCE: usize = 16;
static VALIDATION_DISTANCE: usize = 8;

#[derive(Debug, Clone)]
struct DependentState {
    onboard: bool,
    dynamic: bool,
    dependency: Option<TxId>,
}

impl Default for DependentState {
    fn default() -> Self {
        Self { onboard: true, dynamic: false, dependency: None }
    }
}

pub struct TxDependency {
    num_txs: usize,
    dependent_state: Vec<Mutex<DependentState>>,
    affect_txs: Vec<Mutex<HashSet<TxId>>>,
    num_onboard: AtomicUsize,
    index: AtomicUsize,
}

impl TxDependency {
    pub fn new(num_txs: usize) -> Self {
        Self {
            num_txs,
            dependent_state: (0..num_txs).map(|_| Default::default()).collect(),
            affect_txs: (0..num_txs).map(|_| Default::default()).collect(),
            num_onboard: AtomicUsize::new(num_txs),
            index: AtomicUsize::new(0),
        }
    }

    pub fn create(dependent_tx: Vec<Option<TxId>>, affect_txs: Vec<HashSet<TxId>>) -> Self {
        assert_eq!(dependent_tx.len(), affect_txs.len());
        let num_txs = dependent_tx.len();
        Self {
            num_txs,
            dependent_state: dependent_tx
                .into_iter()
                .map(|dep| {
                    Mutex::new(DependentState { onboard: true, dynamic: false, dependency: dep })
                })
                .collect(),
            affect_txs: affect_txs.into_iter().map(|affects| Mutex::new(affects)).collect(),
            num_onboard: AtomicUsize::new(num_txs),
            index: AtomicUsize::new(0),
        }
    }

    pub fn next(&self, finality_idx: TxId) -> Vec<TxId> {
        let mut txs = vec![];
        while self.index.load(Ordering::Relaxed) < self.num_txs {
            let index = self.index.fetch_add(1, Ordering::Relaxed);
            if index < self.num_txs {
                let mut state = self.dependent_state[index].lock();
                if state.onboard && state.dependency.is_none() {
                    txs.push(index);
                    state.onboard = false;
                    self.num_onboard.fetch_sub(1, Ordering::Relaxed);
                    if index == finality_idx {
                        let mut continuous = index;
                        while continuous < self.num_txs - 1 {
                            let next = continuous + 1;
                            let mut affects = self.affect_txs[continuous].lock();
                            let mut dep = self.dependent_state[next].lock();
                            if affects.contains(&next) &&
                                dep.dependency == Some(continuous) &&
                                dep.onboard
                            {
                                txs.push(next);
                                affects.remove(&next);
                                dep.dependency = None;
                                dep.onboard = false;
                                self.num_onboard.fetch_sub(1, Ordering::Relaxed);
                                continuous += 1;
                            } else {
                                break;
                            }
                        }
                    }
                    break;
                }
            }
        }
        txs
    }

    fn remove(&self, txid: TxId, distance: usize) {
        let mut affects = self.affect_txs[txid].lock();
        if affects.is_empty() {
            return;
        }
        let mut removed_affect_txs = Vec::with_capacity(affects.len());
        for &tx in affects.iter() {
            let mut dependent = self.dependent_state[tx].lock();
            if !dependent.dynamic || tx - txid >= distance {
                if dependent.dependency == Some(txid) {
                    dependent.dependency = None;
                    if dependent.onboard {
                        self.index.fetch_min(tx, Ordering::Relaxed);
                    }
                }
                removed_affect_txs.push(tx);
            }
        }
        if removed_affect_txs.len() == affects.len() {
            affects.clear();
        } else {
            for tx in removed_affect_txs.iter() {
                affects.remove(tx);
            }
        }
    }

    pub fn remove_after_execution(&self, txid: TxId) {
        self.remove(txid, EXECUTE_DISTANCE);
    }

    pub fn remove_after_validation(&self, txid: TxId) {
        self.remove(txid, VALIDATION_DISTANCE);
    }

    pub fn remove_after_commit(&self, txid: TxId) {
        self.remove(txid, 0);
    }

    pub fn add(&self, txid: TxId, dep_id: Option<TxId>) {
        if let Some(dep_id) = dep_id {
            let mut dep = self.affect_txs[dep_id].lock();
            let mut dep_state = self.dependent_state[dep_id].lock();
            let mut state = self.dependent_state[txid].lock();
            assert!(!state.onboard && state.dependency.is_none());
            state.dependency = Some(dep_id);
            state.onboard = true;
            state.dynamic = true;
            self.num_onboard.fetch_add(1, Ordering::Relaxed);

            if dep.insert(txid) {
                if !dep_state.onboard {
                    dep_state.onboard = true;
                    self.num_onboard.fetch_add(1, Ordering::Relaxed);
                }
                if dep_state.dependency.is_none() {
                    self.index.fetch_min(dep_id, Ordering::Relaxed);
                }
            } else {
                assert!(dep_state.onboard);
            }
        } else {
            let mut state = self.dependent_state[txid].lock();
            assert!(!state.onboard && state.dependency.is_none());
            state.onboard = true;
            self.index.fetch_min(txid, Ordering::Relaxed);
            self.num_onboard.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub fn print(&self) {
        let dependent_tx: Vec<(TxId, DependentState)> =
            self.dependent_state.iter().map(|dep| dep.lock().clone()).enumerate().collect();
        let affect_txs: Vec<(TxId, HashSet<TxId>)> =
            self.affect_txs.iter().map(|affects| affects.lock().clone()).enumerate().collect();
        println!("tx_states: {:?}", dependent_tx);
        println!("affect_txs: {:?}", affect_txs);
    }
}
