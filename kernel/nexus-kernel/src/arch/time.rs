//! The system tick.
//!
//! One counter, fed by whichever timer is currently driving the system. The PIT
//! starts it because it needs no MMIO mapping and therefore works before the
//! memory manager does; the local APIC timer takes over once it can be mapped
//! and calibrated.
//!
//! Keeping the counter here rather than in either timer's module is what makes
//! that handover invisible: the scheduler sleeps on ticks, the display shows
//! uptime, and neither has to know or care which piece of hardware is
//! interrupting.

use core::sync::atomic::{AtomicU64, AtomicU8, Ordering};

/// Ticks counted since the first timer started.
static TICKS: AtomicU64 = AtomicU64::new(0);

/// The tick rate, for converting ticks to wall time.
static FREQUENCY_HZ: AtomicU64 = AtomicU64::new(0);

/// Which timer is currently driving the tick.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum TimerSource {
    /// Nothing is ticking yet.
    None = 0,
    /// The 8253/8254 programmable interval timer.
    Pit = 1,
    /// The local APIC timer.
    LocalApic = 2,
}

static SOURCE: AtomicU8 = AtomicU8::new(TimerSource::None as u8);

/// Record that `source` now drives the tick at `frequency_hz`.
///
/// The counter is deliberately *not* reset. Uptime has to stay monotonic across
/// the handover from the PIT to the APIC timer, and anything that has already
/// slept on a deadline expects the counter it was given to keep counting.
pub fn set_source(source: TimerSource, frequency_hz: u64) {
    FREQUENCY_HZ.store(frequency_hz, Ordering::Relaxed);
    SOURCE.store(source as u8, Ordering::Release);
}

/// The timer currently driving the tick.
#[must_use]
pub fn source() -> TimerSource {
    match SOURCE.load(Ordering::Acquire) {
        1 => TimerSource::Pit,
        2 => TimerSource::LocalApic,
        _ => TimerSource::None,
    }
}

/// Whether `source` is the one currently driving the tick.
///
/// The PIT handler uses this to fall silent once the APIC has taken over,
/// rather than being unhooked: a stray legacy interrupt after the handover
/// would otherwise advance the clock a second time.
#[inline]
#[must_use]
pub fn is_source(candidate: TimerSource) -> bool {
    SOURCE.load(Ordering::Acquire) == candidate as u8
}

/// Record one tick.
#[inline]
pub fn on_tick() {
    TICKS.fetch_add(1, Ordering::Relaxed);
}

/// Ticks counted since the first timer started.
#[inline]
#[must_use]
pub fn ticks() -> u64 {
    TICKS.load(Ordering::Relaxed)
}

/// The current tick rate, or 0 before any timer is running.
#[inline]
#[must_use]
pub fn frequency_hz() -> u64 {
    FREQUENCY_HZ.load(Ordering::Relaxed)
}

/// Milliseconds elapsed since the first timer started.
///
/// Approximate across a change of source: the tick count is preserved but the
/// rate is not, so ticks accumulated at one rate are converted at the other.
/// The handover happens once, early, and the error is a few milliseconds — far
/// less than the alternative of uptime jumping backwards.
#[must_use]
pub fn uptime_ms() -> u64 {
    let frequency = frequency_hz();
    if frequency == 0 {
        return 0;
    }
    ticks() * 1000 / frequency
}

/// Convert milliseconds to ticks, rounding up so a sleep never returns early.
#[must_use]
pub fn ms_to_ticks(milliseconds: u64) -> u64 {
    let frequency = frequency_hz();
    if frequency == 0 {
        return 0;
    }
    (milliseconds * frequency).div_ceil(1000)
}

/// Busy-wait for `milliseconds`.
///
/// Spins on the tick counter, so it needs interrupts enabled and a timer
/// running. Intended for bring-up, where there is no scheduler to sleep on —
/// calibrating the APIC timer against the PIT is exactly that situation.
pub fn busy_wait_ms(milliseconds: u64) {
    let target = ticks() + ms_to_ticks(milliseconds);
    while ticks() < target {
        super::wait_for_interrupt();
    }
}
