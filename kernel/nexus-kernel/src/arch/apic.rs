//! The local APIC and its timer.
//!
//! Replaces the 8259 and the PIT as the system's interrupt controller and
//! scheduling tick. That matters for three reasons beyond tidiness: the local
//! APIC is per-processor, so on a multiprocessor machine each core gets its own
//! timer rather than sharing one; it can deliver inter-processor interrupts,
//! which is how other cores are started at all; and it is the only route to
//! MSI, which is how every modern device raises an interrupt.
//!
//! # Why this could not be done earlier
//!
//! The APIC's registers are memory-mapped at an address far above RAM —
//! 0xFEE00000 on essentially every machine — so reaching them needs a page
//! mapping, which needs the frame allocator and the virtual memory manager.
//! The PIT, being port I/O, needs nothing, which is why it starts the system
//! and hands over here.
//!
//! # Calibration
//!
//! Nothing tells software how fast the APIC timer counts: it runs off the bus
//! clock, divided. The only way to find out is to measure it against a clock
//! whose rate *is* known, which is what the PIT is still doing at this point.

use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use nexus_abi::layout;

use super::time::{self, TimerSource};
use crate::kprintln;
use crate::memory::paging;

/// Register offsets within the local APIC's window.
mod register {
    /// Local APIC identifier.
    pub const ID: usize = 0x020;
    /// Version, and the number of LVT entries.
    pub const VERSION: usize = 0x030;
    /// Task priority: the interrupt level below which delivery is deferred.
    pub const TASK_PRIORITY: usize = 0x080;
    /// End of interrupt.
    pub const END_OF_INTERRUPT: usize = 0x0B0;
    /// Spurious interrupt vector, and the software enable bit.
    pub const SPURIOUS: usize = 0x0F0;
    /// Local vector table entry for the timer.
    pub const LVT_TIMER: usize = 0x320;
    /// Local vector table entry for thermal events.
    pub const LVT_THERMAL: usize = 0x330;
    /// Local vector table entry for performance counters.
    pub const LVT_PERFORMANCE: usize = 0x340;
    /// Local vector table entry for the LINT0 pin.
    pub const LVT_LINT0: usize = 0x350;
    /// Local vector table entry for the LINT1 pin.
    pub const LVT_LINT1: usize = 0x360;
    /// Local vector table entry for internal APIC errors.
    pub const LVT_ERROR: usize = 0x370;
    /// Value the timer reloads from.
    pub const TIMER_INITIAL_COUNT: usize = 0x380;
    /// The timer's live count.
    pub const TIMER_CURRENT_COUNT: usize = 0x390;
    /// Timer clock divisor.
    pub const TIMER_DIVIDE: usize = 0x3E0;
}

/// `SPURIOUS` bit 8: the APIC is enabled.
const SPURIOUS_ENABLE: u32 = 1 << 8;
/// An LVT entry with this bit set delivers nothing.
const LVT_MASKED: u32 = 1 << 16;
/// LVT delivery mode `ExtINT`: take the vector from an external 8259.
const LVT_DELIVERY_EXTINT: u32 = 0b111 << 8;
/// LVT delivery mode `NMI`.
const LVT_DELIVERY_NMI: u32 = 0b100 << 8;
/// `LVT_TIMER` bit 17: reload automatically instead of firing once.
const LVT_TIMER_PERIODIC: u32 = 1 << 17;
/// `TIMER_DIVIDE` value selecting a divisor of 16.
///
/// The encoding is not sequential and bit 2 is reserved: the divisor is formed
/// from bits 3, 1 and 0, so 16 is `0b0011` and `0b1011` would be divide-by-one.
/// Dividing keeps the count from wrapping over a calibration window long enough
/// to be accurate.
const DIVIDE_BY_16: u32 = 0b0011;
/// The divisor `DIVIDE_BY_16` selects, for turning counts into a frequency.
const DIVISOR: u64 = 16;

/// `IA32_APIC_BASE`, which holds the window address and the hardware enable.
const IA32_APIC_BASE: u32 = 0x1B;
/// `IA32_APIC_BASE` bit 11: the APIC is enabled in hardware.
const APIC_BASE_ENABLE: u64 = 1 << 11;

/// Virtual address the register window is mapped at, or 0 before [`init`].
static REGISTERS: AtomicUsize = AtomicUsize::new(0);

/// Measured APIC timer frequency, in ticks per second.
static TIMER_FREQUENCY_HZ: AtomicU64 = AtomicU64::new(0);

/// Spurious interrupts counted since boot.
static SPURIOUS_COUNT: AtomicU64 = AtomicU64::new(0);

/// Read an APIC register.
///
/// # Safety
///
/// [`init`] must have mapped the window, and `offset` must name a readable
/// register.
unsafe fn read(offset: usize) -> u32 {
    let base = REGISTERS.load(Ordering::Acquire);
    debug_assert!(base != 0, "the local APIC is not mapped");
    // SAFETY: the window is mapped uncached and `offset` is register-aligned.
    unsafe { core::ptr::read_volatile((base + offset) as *const u32) }
}

/// Write an APIC register.
///
/// # Safety
///
/// See [`read`].
unsafe fn write(offset: usize, value: u32) {
    let base = REGISTERS.load(Ordering::Acquire);
    debug_assert!(base != 0, "the local APIC is not mapped");
    // SAFETY: as above.
    unsafe { core::ptr::write_volatile((base + offset) as *mut u32, value) }
}

/// Why the local APIC could not be brought up.
#[derive(Debug, Clone, Copy)]
pub enum ApicError {
    /// The processor reports no local APIC.
    Unsupported,
    /// The register window could not be mapped.
    MapFailed(paging::MapError),
    /// Calibration measured no ticks, so the timer cannot be programmed.
    CalibrationFailed,
    /// There is no running clock to calibrate against.
    NoReferenceClock,
}

impl core::fmt::Display for ApicError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Unsupported => f.write_str("this processor has no local APIC"),
            Self::MapFailed(error) => write!(f, "could not map the APIC registers: {error}"),
            Self::CalibrationFailed => {
                f.write_str("the APIC timer did not count during calibration")
            }
            Self::NoReferenceClock => {
                f.write_str("no running reference clock to calibrate the APIC timer against")
            }
        }
    }
}

/// Whether `CPUID` reports a local APIC.
fn is_supported() -> bool {
    let features: u32;
    // SAFETY: `cpuid` leaf 1 is available on every processor that can reach
    // long mode. `rbx` is preserved because LLVM reserves it.
    unsafe {
        core::arch::asm!(
            "push rbx",
            "cpuid",
            "pop rbx",
            inout("eax") 1u32 => _,
            out("ecx") _,
            out("edx") features,
            options(nostack, preserves_flags),
        );
    }
    // EDX bit 9: the processor contains a local APIC.
    features & (1 << 9) != 0
}

/// Read `IA32_APIC_BASE`.
///
/// # Safety
///
/// The processor must have a local APIC.
unsafe fn read_apic_base() -> u64 {
    // SAFETY: upheld by the caller.
    unsafe {
        let (low, high): (u32, u32);
        core::arch::asm!(
            "rdmsr",
            in("ecx") IA32_APIC_BASE,
            out("eax") low,
            out("edx") high,
            options(nomem, nostack, preserves_flags),
        );
        (u64::from(high) << 32) | u64::from(low)
    }
}

/// Write `IA32_APIC_BASE`.
///
/// # Safety
///
/// The value must keep the window at an address the kernel has mapped.
unsafe fn write_apic_base(value: u64) {
    // SAFETY: upheld by the caller.
    unsafe {
        core::arch::asm!(
            "wrmsr",
            in("ecx") IA32_APIC_BASE,
            in("eax") value as u32,
            in("edx") (value >> 32) as u32,
            options(nomem, nostack, preserves_flags),
        );
    }
}

/// Acknowledge the interrupt being serviced.
///
/// # Safety
///
/// Call exactly once per delivered interrupt, from its handler. A spurious
/// interrupt is the exception: it never raised an in-service bit, so
/// acknowledging it would clear a different, real interrupt.
pub unsafe fn end_of_interrupt() {
    // SAFETY: the window is mapped by the time any handler can run.
    unsafe { write(register::END_OF_INTERRUPT, 0) };
}

/// Record a spurious interrupt.
pub fn on_spurious() {
    SPURIOUS_COUNT.fetch_add(1, Ordering::Relaxed);
}

/// Spurious interrupts seen since boot.
#[must_use]
pub fn spurious_count() -> u64 {
    SPURIOUS_COUNT.load(Ordering::Relaxed)
}

/// Whether the local APIC is up.
#[must_use]
pub fn is_active() -> bool {
    REGISTERS.load(Ordering::Acquire) != 0
}

/// This processor's local APIC identifier.
#[must_use]
pub fn local_id() -> u32 {
    if !is_active() {
        return 0;
    }
    // SAFETY: the window is mapped. The identifier is in the upper 8 bits.
    unsafe { read(register::ID) >> 24 }
}

/// The measured APIC timer frequency, in hertz.
#[must_use]
pub fn timer_frequency_hz() -> u64 {
    TIMER_FREQUENCY_HZ.load(Ordering::Relaxed)
}

/// Measure how fast the APIC timer counts, against the PIT.
///
/// Returns ticks per second at the configured divisor, or `None` if the
/// reference clock never advanced.
///
/// Every wait here is bounded by the time-stamp counter as well as by the tick
/// it is actually waiting for. Calibration is the one place in boot that
/// depends on interrupts already being delivered, and an unbounded spin on a
/// clock that is not running is a hang with no output at all — which is exactly
/// what happened the first time this ran before interrupts were enabled.
///
/// # Safety
///
/// The window must be mapped and a reference clock must be running.
unsafe fn calibrate(sample_ms: u64) -> Result<u64, ApicError> {
    // Generous: a slow emulated processor still executes far more than this in
    // the tens of milliseconds a calibration takes.
    const SPIN_LIMIT_CYCLES: u64 = 50_000_000_000;

    // SAFETY: upheld by the caller.
    unsafe {
        write(register::TIMER_DIVIDE, DIVIDE_BY_16);
        // Mask the timer while calibrating: the count is what is wanted, not
        // interrupts from it.
        write(register::LVT_TIMER, LVT_MASKED);

        // Count down from the maximum so the window cannot underflow.
        write(register::TIMER_INITIAL_COUNT, u32::MAX);

        // Start on a tick boundary, so the error is at most one tick rather
        // than up to two.
        let abort_at = super::read_tsc() + SPIN_LIMIT_CYCLES;
        let start_tick = time::ticks();
        while time::ticks() == start_tick {
            if super::read_tsc() > abort_at {
                write(register::TIMER_INITIAL_COUNT, 0);
                return Err(ApicError::NoReferenceClock);
            }
            core::hint::spin_loop();
        }
        let begin = read(register::TIMER_CURRENT_COUNT);

        let deadline = time::ticks() + time::ms_to_ticks(sample_ms);
        let abort_at = super::read_tsc() + SPIN_LIMIT_CYCLES;
        while time::ticks() < deadline {
            if super::read_tsc() > abort_at {
                write(register::TIMER_INITIAL_COUNT, 0);
                return Err(ApicError::NoReferenceClock);
            }
            core::hint::spin_loop();
        }
        let end = read(register::TIMER_CURRENT_COUNT);

        write(register::TIMER_INITIAL_COUNT, 0);

        let elapsed = u64::from(begin.saturating_sub(end));
        if elapsed == 0 {
            kprintln!("[apic] calibration read {begin} then {end}; the count did not move");
            return Err(ApicError::CalibrationFailed);
        }
        // Counts are post-divisor, so scale back to the rate the timer is
        // actually driven at.
        Ok(elapsed * DIVISOR * 1000 / sample_ms.max(1))
    }
}

/// Bring up the local APIC and start its timer at `tick_hz`.
///
/// `apic_address` comes from the MADT. On return the APIC drives the system
/// tick and the caller should mask the 8259.
///
/// # Safety
///
/// Call once, after the virtual memory manager is running and while the PIT is
/// still ticking with interrupts enabled — calibration depends on both.
pub unsafe fn init(apic_address: u64, tick_hz: u64, timer_vector: u8) -> Result<(), ApicError> {
    if !is_supported() {
        return Err(ApicError::Unsupported);
    }
    // Calibration waits on the tick, which only advances if the reference
    // timer's interrupts are being delivered. Checking up front turns a silent
    // hang into a message.
    if !super::interrupts::are_enabled() || time::frequency_hz() == 0 {
        return Err(ApicError::NoReferenceClock);
    }

    // Map the register window uncached. Device registers must never be cached:
    // a write that sits in a cache line has not reached the device, and a read
    // that hits in cache returns a stale value rather than the hardware's.
    let virtual_address = layout::KERNEL_MMIO_BASE;
    // SAFETY: `apic_address` is a device window from firmware, and
    // `KERNEL_MMIO_BASE` is reserved for exactly this.
    unsafe {
        paging::map_page(
            virtual_address,
            apic_address,
            paging::WRITABLE | paging::NO_CACHE | paging::NO_EXECUTE | paging::GLOBAL,
        )
        .map_err(ApicError::MapFailed)?;
    }
    REGISTERS.store(virtual_address as usize, Ordering::Release);

    // SAFETY: the window is mapped and the processor has an APIC.
    unsafe {
        // Make sure the hardware enable is set. Firmware usually leaves it on,
        // but a machine that came through a warm reset may not have.
        let base = read_apic_base();
        if base & APIC_BASE_ENABLE == 0 {
            write_apic_base(base | APIC_BASE_ENABLE);
        }

        // Accept every interrupt priority. A non-zero task priority would make
        // the APIC quietly defer interrupts, which is a very confusing way for
        // a system to appear hung.
        write(register::TASK_PRIORITY, 0);

        // Mask the local sources nothing handles yet, so a machine that comes
        // up with stale LVT contents cannot deliver into a vector with no
        // handler.
        write(register::LVT_THERMAL, LVT_MASKED);
        write(register::LVT_PERFORMANCE, LVT_MASKED);
        write(register::LVT_ERROR, LVT_MASKED);

        // LINT0 and LINT1 are pins, not local sources, and on a PC the 8259's
        // INTR output is wired to LINT0. Enabling the APIC with LINT0 masked
        // therefore cuts off every legacy interrupt — including the PIT, which
        // is the clock this is about to calibrate against. That is not a
        // hypothetical: masking them here stopped the PIT dead and calibration
        // sat waiting for a tick that could no longer arrive.
        //
        // So the pins are configured the way firmware leaves them, which the
        // architecture calls virtual wire mode: LINT0 passes the 8259's vector
        // through as ExtINT, LINT1 carries the NMI. Both are unmasked until the
        // APIC timer has taken over and the PIC is shut down.
        write(register::LVT_LINT0, LVT_DELIVERY_EXTINT);
        write(register::LVT_LINT1, LVT_DELIVERY_NMI);

        // Software-enable, and route spurious interrupts to a vector that
        // counts them.
        write(
            register::SPURIOUS,
            SPURIOUS_ENABLE | u32::from(super::interrupts::SPURIOUS_VECTOR),
        );
    }

    // Prove the window is readable before trusting anything measured through
    // it. An unmapped or mis-attributed window reads as all-ones or all-zeros,
    // and either would surface later as a mystifying calibration result rather
    // than as the mapping problem it actually is.
    // SAFETY: the window is mapped.
    let version = unsafe { read(register::VERSION) };
    if version == 0 || version == u32::MAX {
        // All-zeros or all-ones is what an unmapped or mis-attributed window
        // reads as. Catching it here names the real problem instead of letting
        // it surface later as an inexplicable calibration result.
        kprintln!("[apic] the register window reads {version:#010x}; it is not mapped correctly");
        return Err(ApicError::MapFailed(paging::MapError::NotMapped));
    }

    // SAFETY: the window is mapped, the PIT is running, interrupts are on.
    let frequency = unsafe { calibrate(50) }?;
    TIMER_FREQUENCY_HZ.store(frequency, Ordering::Relaxed);

    let count = (frequency / DIVISOR / tick_hz.max(1)).max(1);
    // SAFETY: the window is mapped.
    unsafe {
        write(register::TIMER_DIVIDE, DIVIDE_BY_16);
        write(
            register::LVT_TIMER,
            u32::from(timer_vector) | LVT_TIMER_PERIODIC,
        );
        write(register::TIMER_INITIAL_COUNT, count as u32);
    }

    // The APIC now drives the clock. The PIT handler checks this and falls
    // silent rather than being unhooked, so a straggling legacy interrupt
    // cannot advance the clock twice.
    time::set_source(TimerSource::LocalApic, tick_hz);

    kprintln!(
        "[apic] local APIC {} version {:#x} mapped at {:#018x}",
        local_id(),
        version & 0xFF,
        virtual_address
    );
    kprintln!(
        "[apic] timer runs at {} MHz; ticking at {} Hz on vector {}",
        frequency / 1_000_000,
        tick_hz,
        timer_vector
    );

    Ok(())
}

/// Stop passing legacy 8259 interrupts through LINT0.
///
/// Called once the APIC timer is driving the system and the PIC has been
/// masked, so that nothing arrives through the legacy path any more.
///
/// # Safety
///
/// Nothing may still depend on legacy interrupt delivery.
pub unsafe fn disconnect_legacy_pic() {
    if !is_active() {
        return;
    }
    // SAFETY: the window is mapped and the caller has finished with the 8259.
    unsafe { write(register::LVT_LINT0, LVT_MASKED) };
}
