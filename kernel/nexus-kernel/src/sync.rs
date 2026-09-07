//! Kernel synchronisation primitives.
//!
//! Only a spinlock exists so far, which is the right primitive for the code
//! that runs before there is a scheduler to block on. Once threads exist this
//! module gains blocking mutexes, and spinlocks stay reserved for short
//! critical sections and for code that runs in interrupt context.
//!
//! The non-blocking accessors are part of the primitive's contract even before
//! a caller needs them: `try_lock` is what interrupt handlers will use, and
//! `is_locked` is what the panic path uses to decide whether the console lock
//! can be trusted.
#![allow(dead_code)]

use core::cell::UnsafeCell;
use core::ops::{Deref, DerefMut};
use core::sync::atomic::{AtomicBool, Ordering};

/// A mutual-exclusion lock that spins rather than blocking.
///
/// # Interrupt safety
///
/// Taking a `SpinLock` does *not* disable interrupts. Any lock that is also
/// taken from an interrupt handler must be wrapped in an interrupt-disabled
/// region by the caller, or the handler can deadlock against the code it
/// interrupted. Interrupts are not enabled until the kernel reaches
/// `interrupts::enable`, so early boot code is unaffected.
pub struct SpinLock<T> {
    locked: AtomicBool,
    value: UnsafeCell<T>,
}

// SAFETY: the lock serialises all access to `value`, so sharing a `SpinLock`
// across cores is sound exactly when `T` can be sent to another core.
unsafe impl<T: Send> Sync for SpinLock<T> {}
// SAFETY: moving the lock moves the value it guards.
unsafe impl<T: Send> Send for SpinLock<T> {}

impl<T> SpinLock<T> {
    /// Create an unlocked `SpinLock` holding `value`.
    #[must_use]
    pub const fn new(value: T) -> Self {
        Self {
            locked: AtomicBool::new(false),
            value: UnsafeCell::new(value),
        }
    }

    /// Acquire the lock, spinning until it is free.
    pub fn lock(&self) -> SpinLockGuard<'_, T> {
        while self
            .locked
            .compare_exchange_weak(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            // Read without the exclusive-ownership traffic of a failed CAS
            // until the lock looks free, which keeps a contended lock from
            // saturating the interconnect.
            while self.locked.load(Ordering::Relaxed) {
                core::hint::spin_loop();
            }
        }
        SpinLockGuard { lock: self }
    }

    /// Acquire the lock if it is free, without spinning.
    pub fn try_lock(&self) -> Option<SpinLockGuard<'_, T>> {
        self.locked
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .ok()
            .map(|_| SpinLockGuard { lock: self })
    }

    /// Whether the lock is currently held.
    ///
    /// Advisory only: the answer can be stale by the time the caller sees it.
    /// Intended for panic paths and diagnostics.
    pub fn is_locked(&self) -> bool {
        self.locked.load(Ordering::Relaxed)
    }
}

/// Proof of ownership of a [`SpinLock`], releasing it on drop.
pub struct SpinLockGuard<'a, T> {
    lock: &'a SpinLock<T>,
}

impl<T> Deref for SpinLockGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        // SAFETY: holding the guard means this core has exclusive access.
        unsafe { &*self.lock.value.get() }
    }
}

impl<T> DerefMut for SpinLockGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: as above, and `&mut self` rules out aliasing the guard.
        unsafe { &mut *self.lock.value.get() }
    }
}

impl<T> Drop for SpinLockGuard<'_, T> {
    fn drop(&mut self) {
        self.lock.locked.store(false, Ordering::Release);
    }
}
