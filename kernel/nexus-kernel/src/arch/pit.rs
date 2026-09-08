//! The 8253/8254 programmable interval timer.
//!
//! The PIT is the kernel's first time source. It is not a good one — 18.2 Hz at
//! its slowest, no better than microsecond resolution, and one global counter
//! shared by every core — but it is reachable through port I/O alone, which
//! means it works before the kernel can map the local APIC's registers.
//!
//! Its jobs here are to prove interrupt delivery end to end and to calibrate
//! the APIC timer against a known frequency. Once that is done the APIC becomes
//! the scheduling tick and this falls silent.
//!
//! The tick counter itself lives in [`super::time`], shared with whichever
//! timer is driving it, so the handover is invisible to everything above.

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
    let divisor = divisor_for(frequency_hz);
    let actual = INPUT_FREQUENCY_HZ / u32::from(divisor);

    // SAFETY: the standard PIT programming sequence, touching only its own
    // command and data ports.
    unsafe {
        outb(COMMAND, MODE_RATE_GENERATOR);
        outb(CHANNEL_0_DATA, divisor as u8);
        outb(CHANNEL_0_DATA, (divisor >> 8) as u8);
    }

    actual
}

/// The 16-bit divisor that comes closest to `frequency_hz`.
fn divisor_for(frequency_hz: u32) -> u16 {
    (INPUT_FREQUENCY_HZ / frequency_hz.max(19)).clamp(1, 65_535) as u16
}

/// Stop the timer from interrupting.
///
/// # Safety
///
/// Reprograms timer hardware.
pub unsafe fn stop() {
    // Mode 0 with a zero divisor: the counter runs down once and then stays
    // there, so no further interrupts are generated.
    // SAFETY: touches only the PIT's own ports.
    unsafe {
        outb(COMMAND, 0b0011_0000);
        outb(CHANNEL_0_DATA, 0);
        outb(CHANNEL_0_DATA, 0);
    }
}

/// Check the divisor arithmetic.
///
/// The counter is sixteen bits, so a rate the hardware cannot reach has to be
/// clamped rather than allowed to wrap -- a wrapped divisor asks for a very
/// fast interrupt instead of a very slow one, which is a machine that spends
/// all its time in the timer handler. Run at boot; see [`crate::selftest`].
pub fn divisor_self_test() -> Result<(), &'static str> {
    if divisor_for(1000) != 1193 || divisor_for(100) != 11931 {
        return Err("an ordinary rate produced the wrong divisor");
    }
    // The requested and achievable rates differ, which is why callers use the
    // frequency that comes back rather than the one they asked for.
    if INPUT_FREQUENCY_HZ / u32::from(divisor_for(1000)) != 1000 {
        return Err("the divisor for 1 kHz does not divide down to 1 kHz");
    }

    // A rate slower than the counter can express has to come out as a *large*
    // divisor. This is the check worth having: the failure it guards against is
    // an unclamped division producing a small one, which asks for a very fast
    // interrupt instead of a very slow one and gives a machine that spends all
    // its time in the timer handler.
    //
    // Not an exact number. The frequency is floored before the division rather
    // than the divisor clamped after it, so the largest divisor this can
    // produce is `INPUT_FREQUENCY_HZ / 19` and not 65535 -- which is what an
    // earlier version of this check asserted, for as long as it never ran.
    let slowest = divisor_for(1);
    if slowest < 60_000 {
        return Err("a rate below the hardware's range came out as a fast one");
    }
    if INPUT_FREQUENCY_HZ / u32::from(slowest) > 20 {
        return Err("the slowest rate the divisor asks for is not slow");
    }
    // And zero, which is the one argument that would divide by it.
    if divisor_for(0) != slowest {
        return Err("a rate of zero was not treated as the slowest possible");
    }

    // Faster than the input clock, which has to be the smallest divisor rather
    // than zero -- a zero divisor means 65536 on this hardware, not "as fast as
    // possible".
    if divisor_for(10_000_000) != 1 {
        return Err("a rate above the input clock was not clamped");
    }
    Ok(())
}
