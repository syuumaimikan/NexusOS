//! Architecture-specific support code.
//!
//! Everything that knows about x86-64 specifically lives under here, so that
//! porting NexusOS to another architecture is a matter of adding a sibling
//! module rather than auditing the whole kernel.
//!
//! The accessors here form the architecture's interface as a unit; the APIC
//! timer and the memory manager consume the rest of it directly.
#![allow(dead_code)]

pub mod apic;
pub mod exceptions;
pub mod gdt;
pub mod idt;
pub mod interrupts;
pub mod io;
pub mod ioapic;
pub mod percpu;
pub mod pic;
pub mod pit;
pub mod smp;
pub mod syscall;
pub mod time;
pub mod tlb;

/// Stop this processor permanently with interrupts masked.
///
/// Used by the panic handler and by cores that have nothing left to do.
pub fn halt_forever() -> ! {
    loop {
        // SAFETY: masking interrupts and halting is always valid in kernel
        // mode, and this function never returns, so nothing observes the
        // disabled interrupts afterwards.
        unsafe {
            core::arch::asm!("cli; hlt", options(nomem, nostack, preserves_flags));
        }
    }
}

/// Reset the processor by taking an interrupt with no table to handle it.
///
/// The last resort in [`crate::power::restart`], after the ACPI reset register
/// and the 8042 have both been tried. Loading an empty interrupt descriptor
/// table turns the next interrupt into a double fault, and with nothing to
/// handle that either the processor triple-faults and resets.
///
/// It always works. It is last because it gives the machine no chance to do
/// anything tidy: no device is quiesced and no firmware handler runs.
///
/// # Safety
///
/// This resets the machine. Nothing after it runs, and anything not already on
/// the disk is gone -- which for this kernel is nothing, because the block
/// cache writes through.
pub unsafe fn triple_fault() -> ! {
    // A descriptor table of no entries, at address zero.
    #[repr(C, packed)]
    struct Pointer {
        limit: u16,
        base: u64,
    }
    let nothing = Pointer { limit: 0, base: 0 };

    // SAFETY: the caller is asking for a reset. `lidt` with an empty table is
    // valid; the `int3` that follows then has no handler to dispatch to.
    unsafe {
        core::arch::asm!(
            "lidt [{table}]",
            "int3",
            table = in(reg) &nothing,
            options(nostack)
        );
    }
    // Not reached on any processor that exists, and halting is the only
    // sensible thing to write for one that somehow continued.
    halt_forever()
}

/// Halt until the next interrupt arrives.
#[inline]
pub fn wait_for_interrupt() {
    // SAFETY: `hlt` in kernel mode simply idles the core until an interrupt.
    unsafe {
        core::arch::asm!("hlt", options(nomem, nostack, preserves_flags));
    }
}

/// Read the current value of `cr3`, the root page-table physical address.
#[inline]
#[must_use]
pub fn read_cr3() -> u64 {
    let value: u64;
    // SAFETY: reading a control register has no side effects.
    unsafe {
        core::arch::asm!("mov {}, cr3", out(reg) value, options(nomem, nostack, preserves_flags));
    }
    value
}

/// Read the time-stamp counter.
#[inline]
#[must_use]
pub fn read_tsc() -> u64 {
    let low: u32;
    let high: u32;
    // SAFETY: `rdtsc` is unprivileged and side-effect free.
    unsafe {
        core::arch::asm!(
            "rdtsc",
            out("eax") low,
            out("edx") high,
            options(nomem, nostack, preserves_flags),
        );
    }
    (u64::from(high) << 32) | u64::from(low)
}
