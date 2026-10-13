//! A counted notification object, shared by duplicated or transferred handles.
//! Counter updates are atomic; a generation wait closes the check/sleep race.

use crate::sched::wait::WaitQueue;
use crate::sync::IrqSpinLock;
use crate::waitset::{WaitSet, Watchers};
use alloc::sync::{Arc, Weak};

pub const MAX_COUNT: u64 = u64::MAX - 1;

struct State {
    count: u64,
    nonblocking: bool,
}

pub struct Counter {
    state: IrqSpinLock<State>,
    semaphore: bool,
    changed: WaitQueue,
    watchers: Watchers,
}

pub enum CounterError {
    WouldBlock,
    Cancelled,
    InvalidValue,
}

impl Counter {
    pub fn new(initial: u32, semaphore: bool, nonblocking: bool) -> Arc<Self> {
        Arc::new(Self {
            state: IrqSpinLock::new(State {
                count: u64::from(initial),
                nonblocking,
            }),
            semaphore,
            changed: WaitQueue::new(),
            watchers: Watchers::new(),
        })
    }

    pub fn readiness(&self) -> (bool, bool) {
        let count = self.state.lock().count;
        (count > 0, count < MAX_COUNT)
    }

    pub fn nonblocking(&self) -> bool {
        self.state.lock().nonblocking
    }

    pub fn set_nonblocking(&self, on: bool) {
        self.state.lock().nonblocking = on;
        self.notify();
    }

    pub fn watch(&self, set: Weak<WaitSet>) {
        self.watchers.add(set);
    }

    fn notify(&self) {
        // Wake outside the state lock: scheduler and wait-set locks must not
        // become nested under a counter's data lock.
        self.changed.wake_all();
        self.watchers.signal();
    }

    pub fn read(&self) -> Result<u64, CounterError> {
        loop {
            if crate::sched::cancelled() {
                return Err(CounterError::Cancelled);
            }
            let seen = self.changed.generation();
            let value = {
                let mut state = self.state.lock();
                if state.count != 0 {
                    let value = if self.semaphore { 1 } else { state.count };
                    state.count -= value;
                    Some(value)
                } else {
                    if state.nonblocking {
                        return Err(CounterError::WouldBlock);
                    }
                    None
                }
            };
            if let Some(value) = value {
                self.notify();
                return Ok(value);
            }
            self.changed.wait_if_unchanged(seen);
        }
    }

    pub fn write(&self, value: u64) -> Result<(), CounterError> {
        if value == u64::MAX {
            return Err(CounterError::InvalidValue);
        }
        loop {
            if crate::sched::cancelled() {
                return Err(CounterError::Cancelled);
            }
            let seen = self.changed.generation();
            let written = {
                let mut state = self.state.lock();
                if value <= MAX_COUNT - state.count {
                    state.count += value;
                    true
                } else {
                    if state.nonblocking {
                        return Err(CounterError::WouldBlock);
                    }
                    false
                }
            };
            if written {
                self.notify();
                return Ok(());
            }
            self.changed.wait_if_unchanged(seen);
        }
    }
}
