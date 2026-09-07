//! The 8253/8254 programmable interval timer.
//!
//! The PIT is the kernel's first time source. It is not a good one — 18.2 Hz at
//! its slowest, no better than microsecond resolution, and one global counter
//! shared by every core — but it is reachable through port I/O alone, which
//! means it works before the kernel can map the local APIC's registers.
//!
//! Its jobs here are to prove interrupt delivery end to end and, later, to
//! calibrate the APIC timer against a known frequency. Once that is done, the
//! APIC becomes the scheduling tick and this becomes a fallback.

use core::sync::atomic::{AtomicU64, Ordering};

use super::io::outb;

/// The PIT's input frequency, fixed by the original PC design.
const INPUT_FREQUENCY_HZ: u32 = 1_193_182;

/// Data port of channel 0, which is wired to IRQ 0.
const CHANNEL_0_DATA: u16 = 0x40;
/// The mode/command register.
const COMMAND: u16 = 0x43;

/// Channel 0, low byte then high byte, mode 2 (rate generator), binary.
///
/// Mode 2 is the right choice for a periodic tick: the counter reloads itself,
/// so the interrupt keeps arriving without the handler reprogramming anything.
const MODE_RATE_GENERATOR: u8 = 0b0011_0100;

/// Ticks counted since the timer was started.
static TICKS: AtomicU64 = AtomicU64::new(0);

/// The configured tick rate, for converting ticks to wall time.
static FREQUENCY_HZ: AtomicU64 = AtomicU64::new(0);

/// Program channel 0 to interrupt at `frequency_hz`.
///
/// The achievable rate is `1193182 / divisor` for a 16-bit divisor, so the
/// actual frequency is returned and may differ slightly from the request.
///
/// # Safety
///
/// Reprograms timer hardware. Must run with interrupts disabled; the caller is
/// responsible for registering a handler and unmasking IRQ 0.
pub unsafe fn init(frequency_hz: u32) -> u32 {
    // Clamp to what a 16-bit divisor can express. A divisor of 0 means 65536,
    // giving the slowest rate of about 18.2 Hz; anything faster than the input
    // clock is impossible.
    let divisor = (INPUT_FREQUENCY_HZ / frequency_hz.max(19)).clamp(1, 65_535) as u16;
    let actual = INPUT_FREQUENCY_HZ / u32::from(divisor);

    // SAFETY: the standard PIT programming sequence, touching only its own
    // command and data ports.
    unsafe {
        outb(COMMAND, MODE_RATE_GENERATOR);
        outb(CHANNEL_0_DATA, divisor as u8);
        outb(CHANNEL_0_DATA, (divisor >> 8) as u8);
    }

    FREQUENCY_HZ.store(u64::from(actual), Ordering::Relaxed);
    actual
}

/// Record one tick. Called from the timer interrupt handler.
#[inline]
pub fn on_tick() {
    TICKS.fetch_add(1, Ordering::Relaxed);
}

/// Ticks counted since the timer started.
#[inline]
#[must_use]
pub fn ticks() -> u64 {
    TICKS.load(Ordering::Relaxed)
}

/// The tick rate actually programmed, or 0 if the timer is not running.
#[inline]
#[must_use]
pub fn frequency_hz() -> u64 {
    FREQUENCY_HZ.load(Ordering::Relaxed)
}

/// Milliseconds elapsed since the timer started.
#[must_use]
pub fn uptime_ms() -> u64 {
    let frequency = frequency_hz();
    if frequency == 0 {
        return 0;
    }
    ticks() * 1000 / frequency
}

/// Busy-wait for `milliseconds`.
///
/// Spins on the tick counter, so it requires interrupts to be enabled and the
/// timer to be running. Intended for early bring-up, where there is no
/// scheduler to sleep on.
pub fn busy_wait_ms(milliseconds: u64) {
    let frequency = frequency_hz();
    if frequency == 0 {
        return;
    }
    let target = ticks() + (milliseconds * frequency).div_ceil(1000);
    while ticks() < target {
        super::wait_for_interrupt();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The divisor arithmetic decides the real tick rate, and a wrong divisor
    /// silently skews every timing measurement built on top of it.
    fn divisor_for(frequency_hz: u32) -> u16 {
        (INPUT_FREQUENCY_HZ / frequency_hz.max(19)).clamp(1, 65_535) as u16
    }

    #[test]
    fn common_rates_produce_sensible_divisors() {
        assert_eq!(divisor_for(1000), 1193);
        assert_eq!(divisor_for(100), 11931);
        // The requested and achievable rates differ; callers use the returned
        // value, not the requested one.
        assert_eq!(INPUT_FREQUENCY_HZ / u32::from(divisor_for(1000)), 1000);
    }

    #[test]
    fn rates_outside_the_representable_range_are_clamped() {
        // Slower than the hardware can go: clamped to the 16-bit maximum
        // rather than wrapping to a very fast rate.
        assert_eq!(divisor_for(1), 65_535);
        // Faster than the input clock: clamped to the smallest divisor.
        assert_eq!(divisor_for(10_000_000), 1);
    }
}
