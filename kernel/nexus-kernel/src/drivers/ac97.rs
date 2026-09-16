//! A real sound card: Intel's AC'97 controller, and the codec behind it.
//!
//! Until this existed the only thing that could make a noise on this machine
//! was the PC speaker, which is one bit: it is on or it is off, and a tone is a
//! square wave made by a counter. It cannot play a recording, cannot mix, and
//! cannot be quiet at one volume rather than another. The roadmap has carried
//! "a real audio card" for a long time and described it correctly: a DMA
//! engine, a ring of buffers, and a mixer.
//!
//! # The shape of it
//!
//! There are two devices here and it matters, because they are configured
//! through different windows and by different means.
//!
//! The **controller** is on the PCI bus and owns the DMA engine. It reads a
//! *buffer descriptor list* -- thirty-two entries, each an address and a sample
//! count -- and walks it, handing samples to the link. That is the second I/O
//! window, `NABM`.
//!
//! The **codec** is not on the PCI bus at all. It sits on the far side of a
//! serial link and is reached by reading and writing the first window, `NAM`,
//! which the controller turns into link traffic. Volume, sample rate and reset
//! live there. A codec that has not been brought out of reset answers every
//! read with the same value, which is the first thing to check when nothing
//! comes out.
//!
//! # What is here and what is not
//!
//! Sixteen-bit signed samples, two channels, forty-eight thousand a second,
//! which is the one rate an AC'97 2.1 codec is required to do without variable
//! rate support. No mixing of two streams, no capture, no volume control beyond
//! setting it once, and no variable rate: each of those is a real feature and
//! none of them is pretended at.
//!
//! # How it is known to work
//!
//! Not by listening, which nothing here can do. The controller publishes where
//! it has got to -- the index of the descriptor it is on, and how much of the
//! current buffer is left -- and those advance only because the engine is
//! reading memory. A driver that set everything up and never started the engine
//! reads the same numbers for ever, so "these numbers moved" is a claim that
//! can fail.

use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use crate::drivers::pci::{BaseAddress, Device};
use crate::memory;
use crate::sync::IrqSpinLock;
use nexus_abi::layout;

use crate::arch::io::{inb, inl, inw, outb, outl, outw};
use crate::kprintln;

/// Intel, and the 82801AA controller QEMU presents for `-device AC97`.
const INTEL: u16 = 0x8086;
const CONTROLLERS: [u16; 3] = [
    0x2415, // 82801AA
    0x2425, // 82801AB
    0x2445, // 82801BA
];

/// Registers in the mixer window, which is the codec.
mod mixer {
    /// Writing anything resets the codec. Reading gives what it can do.
    pub const RESET: u16 = 0x00;
    /// Master output volume: six bits of attenuation each side, mute at 15.
    pub const MASTER: u16 = 0x02;
    /// The volume of the stream this driver plays.
    pub const PCM_OUT: u16 = 0x18;
}

/// Registers in the bus-master window, which is the controller.
mod bus {
    /// The PCM-out box. Three boxes exist -- in, out and microphone -- and they
    /// have the same shape at different offsets; this driver plays, so it uses
    /// this one.
    pub const PCM_OUT: u16 = 0x10;

    /// Physical address of the descriptor list, 32 bits.
    pub const DESCRIPTORS: u16 = 0x00;
    /// Which descriptor the engine is reading, written by the controller.
    pub const CURRENT: u16 = 0x04;
    /// The last descriptor the driver has filled in.
    pub const LAST_VALID: u16 = 0x05;
    /// What has happened, and what is still happening.
    pub const STATUS: u16 = 0x06;
    /// Samples left in the buffer being read, written by the controller.
    pub const REMAINING: u16 = 0x08;
    /// Run, reset, and which completions raise an interrupt.
    pub const CONTROL: u16 = 0x0B;

    /// Everything the controller does, rather than one stream.
    pub const GLOBAL_CONTROL: u16 = 0x2C;
    pub const GLOBAL_STATUS: u16 = 0x30;
}

/// Bits of the per-stream control register.
mod control {
    /// Start the engine, or pause it.
    pub const RUN: u8 = 1 << 0;
    /// Reset this stream's registers. Clears itself when done.
    pub const RESET: u8 = 1 << 1;
    /// Interrupt when the last valid descriptor has been consumed.
    pub const ON_LAST: u8 = 1 << 2;
    /// Interrupt when a descriptor marked for it completes.
    pub const ON_COMPLETION: u8 = 1 << 4;
}

/// Bits of the per-stream status register.
mod status {
    /// The engine has run out of descriptors and stopped.
    pub const HALTED: u16 = 1 << 0;
    /// The last valid descriptor has been consumed.
    pub const REACHED_LAST: u16 = 1 << 2;
    /// A descriptor marked for it has completed.
    pub const COMPLETED: u16 = 1 << 3;
}

/// Samples a second, and the only rate this asks for.
pub const RATE: u32 = 48_000;
/// Two, because AC'97 carries a stereo pair whatever the source is.
pub const CHANNELS: usize = 2;

/// Descriptors the list holds. The hardware's number, not a choice.
const DESCRIPTORS: usize = 32;
/// Bytes in one descriptor: four of address, two of count, two of flags.
const DESCRIPTOR_SIZE: usize = 8;

/// How much sound the buffer holds, as an order of pages.
///
/// Five is thirty-two pages, which is 128 KiB, which is 32768 stereo samples,
/// which is 683 milliseconds. Long enough that a chime fits in one filling and
/// short enough that the memory is not worth grudging.
const BUFFER_ORDER: usize = 5;
const BUFFER_BYTES: usize = 4096 << BUFFER_ORDER;
/// Sixteen-bit samples, two channels.
const FRAMES: usize = BUFFER_BYTES / (2 * CHANNELS);

/// Flags on a descriptor.
mod descriptor {
    /// Raise an interrupt when this one has been consumed.
    pub const INTERRUPT: u16 = 1 << 15;
}

/// The card, once it has been found.
#[derive(Clone, Copy)]
struct Card {
    /// The bus-master window: the controller.
    ///
    /// The mixer window is not kept. The codec behind it is configured once, at
    /// bring-up, and nothing here changes it afterwards -- there is no volume
    /// control on this machine yet, and holding the number against the day
    /// there is would be holding a thing nothing reads.
    bus: u16,
    /// Physical address of the descriptor list.
    list: u64,
    /// Physical address of the sound itself.
    buffer: u64,
}

// SAFETY: every field is a plain number, and all access goes through the lock
// below; the card is driven from one place at a time on purpose.
unsafe impl Send for Card {}

static CARD: IrqSpinLock<Option<Card>> = IrqSpinLock::new(None);
/// Whether a stream is running, so that two callers cannot start one at once.
static PLAYING: AtomicBool = AtomicBool::new(false);
/// Tones played and samples handed to the card, for the monitor.
static PLAYED: AtomicU64 = AtomicU64::new(0);
static SAMPLES: AtomicU64 = AtomicU64::new(0);

/// Whether a real card is present.
#[must_use]
pub fn is_present() -> bool {
    CARD.lock().is_some()
}

/// Tones played, and samples handed over.
#[must_use]
pub fn statistics() -> (u64, u64) {
    (
        PLAYED.load(Ordering::Relaxed),
        SAMPLES.load(Ordering::Relaxed),
    )
}

/// Find the card, bring the codec out of reset, and get the engine ready.
///
/// Returns whether one was found. A machine with no AC'97 is not an error: the
/// speaker is still there, and saying "no card" once is better than a driver
/// that reports failures for hardware nobody has.
///
/// # Safety
///
/// Call once, after PCI enumeration and with the frame allocator running.
pub unsafe fn init(devices: &[Device]) -> bool {
    let Some(device) = devices
        .iter()
        .find(|device| device.vendor == INTEL && CONTROLLERS.contains(&device.device))
    else {
        return false;
    };

    // SAFETY: the device answered enumeration, so its configuration space is
    // readable, and this is the only driver touching it. `enable` turns on bus
    // mastering, without which the engine cannot read the descriptor list --
    // and the failure looks exactly like a card that is not there.
    unsafe { device.enable() };

    // SAFETY: as above.
    let (mixer, bus) = unsafe {
        match (device.base_address(0), device.base_address(1)) {
            (BaseAddress::Port(mixer), BaseAddress::Port(bus)) => (mixer, bus),
            _ => {
                kprintln!(
                    "[snd ] AC'97 at {} is memory mapped, which this driver does not drive",
                    device.address
                );
                return false;
            }
        }
    };

    let Some(list) = memory::allocate_frame() else {
        kprintln!("[snd ] no memory for the AC'97 descriptor list");
        return false;
    };
    let Some(buffer) = memory::allocate_block(BUFFER_ORDER) else {
        kprintln!("[snd ] no memory for the AC'97 sound buffer");
        return false;
    };
    // The controller holds these addresses in thirty-two bits. A buffer above
    // four gigabytes would be truncated into somebody else's memory and played,
    // which is a great deal worse than not playing.
    if list >= 1 << 32 || buffer + BUFFER_BYTES as u64 >= 1 << 32 {
        kprintln!(
            "[snd ] the AC'97 buffers landed above four gigabytes, which the card cannot address"
        );
        return false;
    }

    // SAFETY: the windows belong to this device, which nothing else drives, and
    // the frames were just allocated to this driver.
    unsafe {
        core::ptr::write_bytes(layout::phys_to_virt(list) as *mut u8, 0, 4096);
        core::ptr::write_bytes(layout::phys_to_virt(buffer) as *mut u8, 0, BUFFER_BYTES);

        // The link out of reset, and then the codec. The order is the protocol:
        // a codec whose link is still in reset does not hear the mixer writes,
        // and every one of them is silently lost.
        outl(bus + bus::GLOBAL_CONTROL, 0x0000_0002);
        // The codec needs time to come up. The status register says when: the
        // "primary codec ready" bit. Bounded, because a machine with no codec
        // behind the controller must report that rather than hang.
        let mut ready = false;
        for _ in 0..100_000 {
            if inl(bus + bus::GLOBAL_STATUS) & 0x0000_0100 != 0 {
                ready = true;
                break;
            }
            core::hint::spin_loop();
        }
        if !ready {
            kprintln!("[snd ] the AC'97 codec never reported itself ready");
            return false;
        }

        outw(mixer + mixer::RESET, 0);
        // Nought is loudest. The field is *attenuation*, six bits a side in
        // steps of one and a half decibels, so writing a large number is how a
        // card is made quiet -- and writing nothing at all leaves it muted on
        // most codecs, which is the other thing to check when there is silence.
        outw(mixer + mixer::MASTER, 0x0000);
        outw(mixer + mixer::PCM_OUT, 0x0000);

        // And the stream's own registers, reset and left stopped.
        outb(bus + bus::PCM_OUT + bus::CONTROL, control::RESET);
        for _ in 0..100_000 {
            if inb(bus + bus::PCM_OUT + bus::CONTROL) & control::RESET == 0 {
                break;
            }
            core::hint::spin_loop();
        }
    }

    *CARD.lock() = Some(Card { bus, list, buffer });

    kprintln!(
        "[snd ] AC'97 at {}: {} Hz, {} channels, {} KiB of buffer, codec ready",
        device.address,
        RATE,
        CHANNELS,
        BUFFER_BYTES / 1024
    );
    true
}

/// Play a square wave, and return when it has finished.
///
/// A square wave and not a sine, because there is no floating point in this
/// kernel and a sine needs either a table or an approximation -- and a square
/// wave at a known frequency is exactly as good for proving the engine runs.
/// What it sounds like is the same thing the speaker made; what is different is
/// that it goes through a codec at a chosen volume, and could as easily be a
/// recording.
///
/// Returns whether the card played it.
pub fn tone(hertz: u32, milliseconds: u64) -> bool {
    let Some(card) = *CARD.lock() else {
        return false;
    };
    if hertz == 0 || milliseconds == 0 {
        return false;
    }
    // One caller at a time. The second would overwrite the first's buffer while
    // the engine was reading it, which is a noise nobody asked for.
    if PLAYING.swap(true, Ordering::Acquire) {
        return false;
    }
    let played = play(card, hertz, milliseconds);
    PLAYING.store(false, Ordering::Release);
    played
}

/// The whole of one tone, with the gate held.
fn play(card: Card, hertz: u32, milliseconds: u64) -> bool {
    // How many stereo frames the tone is, capped at what the buffer holds.
    let wanted = (RATE as u64 * milliseconds / 1000) as usize;
    let frames = wanted.min(FRAMES);
    if frames == 0 {
        return false;
    }

    // Half a period, in frames. A frequency higher than half the sample rate
    // cannot be represented, and one that rounds to nothing would be a divide
    // by zero below rather than a quiet note.
    let half = (RATE / (hertz * 2)).max(1) as usize;

    // SAFETY: the buffer is this driver's, was allocated at bring-up, and the
    // engine is not running -- the gate above is held and the control register
    // was left stopped.
    unsafe {
        let samples = layout::phys_to_virt(card.buffer) as *mut i16;
        /// Loud enough to be heard, quiet enough not to clip when a codec adds
        /// its own gain. A quarter of full scale.
        const AMPLITUDE: i16 = 8192;
        for frame in 0..frames {
            let high = (frame / half).is_multiple_of(2);
            let value = if high { AMPLITUDE } else { -AMPLITUDE };
            samples.add(frame * CHANNELS).write_volatile(value);
            samples.add(frame * CHANNELS + 1).write_volatile(value);
        }

        // The descriptor list: as many entries as the tone needs, each covering
        // a slice of the buffer. The count in a descriptor is *samples*, which
        // is two per stereo frame, and the field is sixteen bits -- so one
        // descriptor cannot carry more than 0xFFFE of them.
        const PER_DESCRIPTOR: usize = 0xFFFE / CHANNELS;
        let list = layout::phys_to_virt(card.list) as *mut u8;
        let mut done = 0usize;
        let mut index = 0usize;
        while done < frames && index < DESCRIPTORS {
            let take = (frames - done).min(PER_DESCRIPTOR);
            let at = list.add(index * DESCRIPTOR_SIZE);
            let address = card.buffer + (done * CHANNELS * 2) as u64;
            (at as *mut u32).write_volatile(address as u32);
            (at.add(4) as *mut u16).write_volatile((take * CHANNELS) as u16);
            // Every descriptor asks for an interrupt. Nothing waits on one --
            // the wait below watches the engine's own position -- but a
            // descriptor that raises none also leaves the status register
            // silent, and the status register is how "it finished" is known.
            (at.add(6) as *mut u16).write_volatile(descriptor::INTERRUPT);
            done += take;
            index += 1;
        }
        if index == 0 {
            return false;
        }

        // Clear whatever the last tone left, or the first read below sees a
        // completion that has already happened.
        outw(
            card.bus + bus::PCM_OUT + bus::STATUS,
            status::HALTED | status::REACHED_LAST | status::COMPLETED,
        );
        outl(card.bus + bus::PCM_OUT + bus::DESCRIPTORS, card.list as u32);
        outb(card.bus + bus::PCM_OUT + bus::LAST_VALID, (index - 1) as u8);
        // And go. Everything above had to be in place first: the engine begins
        // reading the moment this bit is set.
        outb(
            card.bus + bus::PCM_OUT + bus::CONTROL,
            control::RUN | control::ON_COMPLETION | control::ON_LAST,
        );
    }

    let moved = wait_for_end(card, milliseconds);

    // SAFETY: as above.
    unsafe {
        outb(card.bus + bus::PCM_OUT + bus::CONTROL, 0);
    }

    if moved {
        PLAYED.fetch_add(1, Ordering::Relaxed);
        SAMPLES.fetch_add((frames * CHANNELS) as u64, Ordering::Relaxed);
    }
    moved
}

/// Wait for the engine to finish, and say whether it ever started.
///
/// "Ever started" is the whole point of the return value. The engine publishes
/// which descriptor it is on and how much of the buffer is left, and both only
/// change because it is reading memory -- so a driver that configured
/// everything and forgot to set the run bit, or whose bus mastering was never
/// enabled, reads the same numbers from beginning to end. That is reported as a
/// failure rather than as a silent success, which is what a card nobody can
/// hear would otherwise look like.
fn wait_for_end(card: Card, milliseconds: u64) -> bool {
    // Generously longer than the tone, because the wait is for the engine and
    // not for the clock: a machine that was busy elsewhere should not have its
    // sound cut off.
    let deadline = crate::arch::time::uptime_ms() + milliseconds * 2 + 200;
    let mut moved = false;

    loop {
        // SAFETY: the window belongs to this device and these two registers are
        // read-only to the driver.
        let (position, index, state) = unsafe {
            (
                inw(card.bus + bus::PCM_OUT + bus::REMAINING),
                inb(card.bus + bus::PCM_OUT + bus::CURRENT),
                inw(card.bus + bus::PCM_OUT + bus::STATUS),
            )
        };
        if position != 0 || index != 0 {
            moved = true;
        }
        if state & (status::REACHED_LAST | status::HALTED) != 0 {
            return moved;
        }
        if crate::arch::time::uptime_ms() >= deadline {
            return moved;
        }
        // Sleeping rather than spinning: the thing being waited for is a card
        // playing sound, which takes as long as the sound takes.
        crate::sched::sleep_ms(5);
    }
}
