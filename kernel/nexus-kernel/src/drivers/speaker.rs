//! The one thing on a PC that can make a noise without a driver for a card.
//!
//! Channel two of the same interval timer the kernel used for its clock, wired
//! to a speaker instead of to an interrupt. Two port writes start a tone and one
//! stops it, and between those it makes the sound on its own — which is what
//! makes this the only sound device on this machine that costs nothing while it
//! is playing.
//!
//! # What it is and is not
//!
//! A square wave at one frequency. There is no volume, no envelope, no second
//! voice and no sampled sound: the hardware has one bit, and everything above a
//! beep needs a real audio card. This is here because a machine that cannot make
//! a sound at all cannot tell you anything when you are not looking at it — and
//! because the first thing a person wants from a new operating system is for it
//! to go *ding* when it starts.
//!
//! What comes next is AC'97 or Intel HD Audio, which is a DMA engine, a ring of
//! buffers and a mixer. That is a driver, not a port write, and it is not
//! pretended at here.
//!
//! # Why the timer is safe to use
//!
//! Channel two is not the system clock. The kernel moved its tick to the local
//! APIC timer during bring-up and stopped channel zero; channel two was never
//! used and is wired to the speaker gate rather than to the interrupt
//! controller. Programming it cannot disturb the clock because it is not the
//! clock.
//!
//! The command register at `0x43` *is* shared with channel zero, and there is no
//! lock between the two. What makes that safe is the order of events rather than
//! an argument about atomicity: channel zero is programmed during bring-up and
//! stopped before the scheduler has anything else to run, and this thread does
//! not exist until afterwards. A command byte for channel two names channel two
//! in its top two bits and leaves channel zero's counter alone in any case.

use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use crate::arch::io::{inb, outb};
use crate::sync::IrqSpinLock;

/// The timer's channel-two data port.
const CHANNEL_2: u16 = 0x42;
/// And its command port.
const COMMAND: u16 = 0x43;
/// The keyboard controller's port B, whose low two bits gate the speaker.
const GATE: u16 = 0x61;

/// The frequency the timer counts at.
const TIMER_HZ: u32 = 1_193_182;

/// The lowest and highest tones this will play.
///
/// Below the first the divisor stops fitting in sixteen bits; above the second
/// the speaker is producing something nobody would call a tone. Both are
/// clamped rather than refused, because a caller asking for a note outside the
/// range wants a noise and not an error.
pub const LOWEST_HZ: u32 = 20;
pub const HIGHEST_HZ: u32 = 20_000;

/// Whether the machine has been told it has one.
///
/// There is no way to ask. A PC has had this since 1981 and a virtual machine
/// emulates it; what this records is whether the kernel has *used* it, so that
/// the monitor can say so and a machine with no speaker is a machine that is
/// silent rather than one that crashes.
static PRESENT: AtomicBool = AtomicBool::new(false);

/// How many tones have been played.
static PLAYED: AtomicU64 = AtomicU64::new(0);

/// Nothing may program the timer while something else is.
static LOCK: IrqSpinLock<()> = IrqSpinLock::new(());

/// Tones played, and whether the speaker has been used at all.
#[must_use]
pub fn statistics() -> (u64, bool) {
    (
        PLAYED.load(Ordering::Relaxed),
        PRESENT.load(Ordering::Relaxed),
    )
}

/// Start a tone, and leave it playing.
///
/// The caller is responsible for stopping it. That is the honest interface for
/// a device that makes a sound until it is told not to, and it is what lets a
/// caller play a note for as long as something takes rather than for a number
/// of milliseconds guessed in advance.
pub fn start(hertz: u32) {
    let hertz = hertz.clamp(LOWEST_HZ, HIGHEST_HZ);
    let divisor = TIMER_HZ / hertz;
    // Sixteen bits, and never zero: a divisor of zero means 65536 on this
    // hardware, which is a tone nobody asked for.
    let divisor = (divisor.max(1).min(0xFFFF)) as u16;

    let _guard = LOCK.lock();
    // SAFETY: these are the interval timer's channel-two ports and the
    // keyboard controller's port B. Channel two is not the system clock -- the
    // tick moved to the local APIC timer during bring-up -- and the two bits
    // touched in port B are the speaker's gate and nothing else.
    unsafe {
        // Channel two, both bytes, square wave, binary.
        outb(COMMAND, 0b1011_0110);
        outb(CHANNEL_2, (divisor & 0xFF) as u8);
        outb(CHANNEL_2, (divisor >> 8) as u8);

        // The gate. Read first and write back with two bits set, because the
        // other six belong to things that are not the speaker and clearing one
        // of them would be turning off somebody else's hardware.
        let gate = inb(GATE);
        if gate & 0b11 != 0b11 {
            outb(GATE, gate | 0b11);
        }
    }
    PRESENT.store(true, Ordering::Relaxed);
    PLAYED.fetch_add(1, Ordering::Relaxed);
}

/// Stop whatever is playing.
pub fn stop() {
    let _guard = LOCK.lock();
    // SAFETY: as above; this clears only the two bits that gate the speaker.
    unsafe {
        let gate = inb(GATE);
        outb(GATE, gate & !0b11);
    }
}

/// Play one tone for a while, and stop.
///
/// Sleeps, so it must be called from a thread. A tone that busy-waited would be
/// a processor spent on a noise.
pub fn tone(hertz: u32, milliseconds: u64) {
    start(hertz);
    crate::sched::sleep_ms(milliseconds.min(5_000));
    stop();
}
