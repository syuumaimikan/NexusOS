//! Architecture-specific support code.
//!
//! Everything that knows about x86-64 specifically lives under here, so that
//! porting NexusOS to another architecture is a matter of adding a sibling
//! module rather than auditing the whole kernel.
//!
//! The accessors here form the architecture's interface as a unit; the APIC
//! timer and the memory manager consume the rest of it directly.
#![allow(dead_code)]

pub mod exceptions;
pub mod gdt;
pub mod idt;
pub mod interrupts;
pub mod io;
pub mod pic;
pub mod pit;

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
