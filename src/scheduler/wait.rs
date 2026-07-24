use std::{
    sync::{
        OnceLock,
        atomic::{AtomicU8, Ordering},
    },
    thread::{self, Thread},
    time::Duration,
};

const IDLE: u8 = 0;
const ARMED: u8 = 1;
const PARKED: u8 = 2;

trait AtomicWaitState {
    #[cfg(test)]
    fn load(&self, ordering: Ordering) -> u8;
    fn compare_exchange(
        &self,
        current: u8,
        new: u8,
        success: Ordering,
        failure: Ordering,
    ) -> Result<u8, u8>;
    fn swap(&self, value: u8, ordering: Ordering) -> u8;
}

impl AtomicWaitState for AtomicU8 {
    #[cfg(test)]
    #[inline]
    fn load(&self, ordering: Ordering) -> u8 {
        AtomicU8::load(self, ordering)
    }

    #[inline]
    fn compare_exchange(
        &self,
        current: u8,
        new: u8,
        success: Ordering,
        failure: Ordering,
    ) -> Result<u8, u8> {
        AtomicU8::compare_exchange(self, current, new, success, failure)
    }

    #[inline]
    fn swap(&self, value: u8, ordering: Ordering) -> u8 {
        AtomicU8::swap(self, value, ordering)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Notify {
    Coalesced,
    Wake,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BeginPark {
    Notified,
    Park,
}

#[derive(Debug)]
struct WaitState<A> {
    value: A,
}

impl<A> WaitState<A>
where
    A: AtomicWaitState,
{
    #[inline]
    fn new(value: A) -> Self {
        Self { value }
    }

    #[inline]
    fn arm(&self) {
        self.value
            .compare_exchange(IDLE, ARMED, Ordering::AcqRel, Ordering::Acquire)
            .expect("scheduler wait slot armed from a non-idle state");
    }

    #[inline]
    fn cancel(&self) {
        let previous = self.value.swap(IDLE, Ordering::AcqRel);
        assert_ne!(previous, PARKED, "a parked wait slot cannot cancel itself");
    }

    #[inline]
    fn notify(&self) -> Notify {
        if self.value.swap(IDLE, Ordering::AcqRel) == PARKED {
            Notify::Wake
        } else {
            Notify::Coalesced
        }
    }

    #[inline]
    fn begin_park(&self) -> BeginPark {
        match self.value.compare_exchange(ARMED, PARKED, Ordering::AcqRel, Ordering::Acquire) {
            Ok(_) => BeginPark::Park,
            Err(IDLE) => BeginPark::Notified,
            Err(state) => panic!("scheduler wait called from invalid state {state}"),
        }
    }

    /// Restore the idle state after `park_timeout`, returning whether a notifier won the race.
    #[inline]
    fn finish_park(&self) -> bool {
        let previous = self.value.swap(IDLE, Ordering::AcqRel);
        if previous == IDLE {
            true
        } else {
            debug_assert_eq!(previous, PARKED);
            false
        }
    }

    #[cfg(test)]
    fn get(&self) -> u8 {
        self.value.load(Ordering::Acquire)
    }
}

/// A single-consumer, coalescing notification slot.
///
/// The consumer moves `IDLE -> ARMED`, rechecks its predicate, and only then moves
/// `ARMED -> PARKED`. A notification observed in `ARMED` cancels the pending wait without leaving
/// an `unpark` token; a notification observed in `PARKED` wakes the sleeping thread.
#[derive(Debug)]
pub(super) struct WaitSlot {
    thread: OnceLock<Thread>,
    state: WaitState<AtomicU8>,
}

impl WaitSlot {
    pub(super) fn new() -> Self {
        Self { thread: OnceLock::new(), state: WaitState::new(AtomicU8::new(IDLE)) }
    }

    pub(super) fn register_current_thread(&self) {
        let thread = thread::current();
        self.thread.set(thread).expect("scheduler wait thread registered more than once");
    }

    fn arm(&self) {
        self.state.arm();
    }

    fn cancel(&self) {
        self.state.cancel();
    }

    pub(super) fn notify(&self) {
        if self.state.notify() == Notify::Wake {
            self.thread
                .get()
                .expect("scheduler wait thread must be registered before arming")
                .unpark();
        }
    }

    fn wait(&self, timeout: Duration) {
        self.wait_with_parked_hook(timeout, || {});
    }

    fn wait_with_parked_hook(&self, timeout: Duration, parked: impl FnOnce()) {
        match self.state.begin_park() {
            BeginPark::Park => {
                parked();
                thread::park_timeout(timeout);
                if self.state.finish_park() {
                    // If `notify` already delivered a token as the timeout fired, consume it now.
                    // A later token is harmless: Rust parkers permit spurious wakeups.
                    thread::park_timeout(Duration::ZERO);
                }
            }
            BeginPark::Notified => {
                // A producer notified after `arm` but before this transition. Its state update is
                // visible through the failed acquire operation, so the caller can recheck.
            }
        }
    }

    /// Park only if `blocked` remains true after the wait has been armed.
    ///
    /// Arming before the callback closes the check/park race: a producer either changes the
    /// predicate before the callback observes it, cancels the armed wait, or unparks an already
    /// sleeping consumer. The callback may be evaluated twice around one bounded yield.
    pub(super) fn wait_while(&self, timeout: Duration, mut blocked: impl FnMut() -> bool) {
        self.arm();
        if !blocked() {
            self.cancel();
            return;
        }

        // Most scheduler stalls close within one worker timeslice. Keep that path hot without
        // returning to unbounded polling; only a second failed predicate check parks the thread.
        thread::yield_now();
        if blocked() {
            self.wait(timeout);
        } else {
            self.cancel();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        atomic::{AtomicBool, Ordering},
        mpsc,
    };

    impl AtomicWaitState for loom::sync::atomic::AtomicU8 {
        fn load(&self, ordering: Ordering) -> u8 {
            loom::sync::atomic::AtomicU8::load(self, ordering)
        }

        fn compare_exchange(
            &self,
            current: u8,
            new: u8,
            success: Ordering,
            failure: Ordering,
        ) -> Result<u8, u8> {
            loom::sync::atomic::AtomicU8::compare_exchange(self, current, new, success, failure)
        }

        fn swap(&self, value: u8, ordering: Ordering) -> u8 {
            loom::sync::atomic::AtomicU8::swap(self, value, ordering)
        }
    }

    #[test]
    fn notification_between_arm_and_park_is_not_lost() {
        let slot = WaitSlot::new();
        slot.register_current_thread();

        slot.wait_while(Duration::from_secs(1), || {
            slot.notify();
            true
        });
        assert_eq!(slot.state.get(), IDLE);
    }

    #[test]
    fn notification_wakes_a_parked_consumer() {
        let slot = WaitSlot::new();
        let (parked_tx, parked_rx) = mpsc::channel();
        let (done_tx, done_rx) = mpsc::channel();

        thread::scope(|scope| {
            scope.spawn(|| {
                slot.register_current_thread();
                slot.arm();
                slot.wait_with_parked_hook(Duration::from_secs(10), || {
                    parked_tx.send(()).unwrap();
                });
                done_tx.send(()).unwrap();
            });

            parked_rx
                .recv_timeout(Duration::from_secs(2))
                .expect("consumer did not enter the parked state");
            slot.notify();
            done_rx
                .recv_timeout(Duration::from_secs(2))
                .expect("parked consumer did not observe its notification");
        });
        assert_eq!(slot.state.get(), IDLE);
    }

    #[test]
    fn timeout_restores_idle_state_and_slot_can_be_reused() {
        let slot = WaitSlot::new();
        slot.register_current_thread();

        slot.wait_while(Duration::from_millis(1), || true);
        assert_eq!(slot.state.get(), IDLE);

        slot.wait_while(Duration::from_millis(1), || true);
        assert_eq!(slot.state.get(), IDLE);

        slot.arm();
        slot.notify();
        slot.wait(Duration::from_secs(1));
        assert_eq!(slot.state.get(), IDLE);
    }

    #[test]
    fn notification_before_registration_is_coalesced_by_the_predicate() {
        let slot = WaitSlot::new();
        let aborted = AtomicBool::new(false);

        aborted.store(true, Ordering::Release);
        slot.notify();
        slot.register_current_thread();
        slot.wait_while(Duration::from_secs(1), || !aborted.load(Ordering::Acquire));

        assert_eq!(slot.state.get(), IDLE);
    }

    #[test]
    fn arm_notify_park_protocol_never_loses_a_wakeup() {
        loom::model(|| {
            use loom::{
                sync::{
                    Arc,
                    atomic::{AtomicBool, AtomicU8, Ordering},
                },
                thread,
            };

            let state = Arc::new(WaitState::new(AtomicU8::new(IDLE)));
            let blocked = Arc::new(AtomicBool::new(true));
            let parked = Arc::new(AtomicBool::new(false));
            let wake = Arc::new(AtomicBool::new(false));

            let consumer_state = Arc::clone(&state);
            let consumer_blocked = Arc::clone(&blocked);
            let consumer_parked = Arc::clone(&parked);
            let consumer = thread::spawn(move || {
                consumer_state.arm();
                if !consumer_blocked.load(Ordering::Acquire) {
                    consumer_state.cancel();
                    return
                }

                thread::yield_now();
                if consumer_blocked.load(Ordering::Acquire) {
                    if consumer_state.begin_park() == BeginPark::Park {
                        consumer_parked.store(true, Ordering::Release);
                    }
                } else {
                    consumer_state.cancel();
                }
            });

            let producer_state = Arc::clone(&state);
            let producer_blocked = Arc::clone(&blocked);
            let producer_wake = Arc::clone(&wake);
            let producer = thread::spawn(move || {
                producer_blocked.store(false, Ordering::Release);
                if producer_state.notify() == Notify::Wake {
                    producer_wake.store(true, Ordering::Release);
                }
            });

            consumer.join().unwrap();
            producer.join().unwrap();

            if parked.load(Ordering::Acquire) {
                assert!(wake.load(Ordering::Acquire), "a parked waiter must be explicitly woken");
            }
            assert_eq!(state.get(), IDLE);
        });
    }
}
