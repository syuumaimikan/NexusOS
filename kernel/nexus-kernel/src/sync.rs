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

/// A spinlock that also masks interrupts while it is held.
///
/// This is the primitive for any data an interrupt handler touches. A plain
/// [`SpinLock`] deadlocks in that situation: the handler interrupts a thread
/// that already holds the lock, spins waiting for it, and the thread can never
/// run to release it. Masking interrupts for the duration removes the only way
/// the reentrant case can arise on this core.
///
/// The previous interrupt state is saved and restored rather than
/// unconditionally re-enabled, so nesting these — or taking one inside an
/// interrupt handler, where interrupts are already masked — behaves correctly.
///
/// Hold times must be short: interrupts are delayed for the whole critical
/// section.
pub struct IrqSpinLock<T> {
    inner: SpinLock<T>,
}

// SAFETY: delegated to the inner lock, which serialises all access.
unsafe impl<T: Send> Sync for IrqSpinLock<T> {}
// SAFETY: moving the lock moves the value it guards.
unsafe impl<T: Send> Send for IrqSpinLock<T> {}

impl<T> IrqSpinLock<T> {
    /// Create an unlocked `IrqSpinLock` holding `value`.
    #[must_use]
    pub const fn new(value: T) -> Self {
        Self {
            inner: SpinLock::new(value),
        }
    }

    /// Mask interrupts and acquire the lock.
    pub fn lock(&self) -> IrqSpinLockGuard<'_, T> {
        // Interrupts are masked *before* the lock is taken. The other order
        // leaves a window in which this core holds the lock and can still be
        // interrupted into code that wants it.
        let were_enabled = crate::arch::interrupts::are_enabled();
        if were_enabled {
            crate::arch::interrupts::disable();
        }
        IrqSpinLockGuard {
            guard: Some(self.inner.lock()),
            restore_interrupts: were_enabled,
        }
    }
}

/// Proof of ownership of an [`IrqSpinLock`].
///
/// Releases the lock and then restores the previous interrupt state, in that
/// order.
pub struct IrqSpinLockGuard<'a, T> {
    /// Always `Some` until `drop` takes it, which is how the release is
    /// sequenced before interrupts come back.
    guard: Option<SpinLockGuard<'a, T>>,
    restore_interrupts: bool,
}

impl<T> Deref for IrqSpinLockGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        // `guard` is only taken in `drop`, after which this is unreachable.
        self.guard.as_ref().expect("guard held").deref()
    }
}

impl<T> DerefMut for IrqSpinLockGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        self.guard.as_mut().expect("guard held").deref_mut()
    }
}

impl<T> Drop for IrqSpinLockGuard<'_, T> {
    fn drop(&mut self) {
        // Drop the inner guard first: releasing the lock before re-enabling
        // interrupts means an interrupt that fires the instant they come back
        // finds the lock free.
        drop(self.guard.take());
        if self.restore_interrupts {
            crate::arch::interrupts::enable();
        }
    }
}
