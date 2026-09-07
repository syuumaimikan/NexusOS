//! x86 port-mapped I/O.
//!
//! This is the complete byte and word accessor set for the architecture rather
//! than only what today's callers use: PCI configuration space needs the 32-bit
//! pair as soon as bus enumeration lands, and splitting the vocabulary across
//! commits makes it harder to review as a unit.
#![allow(dead_code)]

/// Write a byte to an I/O port.
///
/// # Safety
///
/// Port I/O is an arbitrary side effect on hardware. The caller must know that
/// `port` belongs to a device for which this write is meaningful.
#[inline]
pub unsafe fn outb(port: u16, value: u8) {
    // SAFETY: `out` is well-defined for any port; the caller vouches for the
    // device behind it.
    unsafe {
        core::arch::asm!(
            "out dx, al",
            in("dx") port,
            in("al") value,
            options(nomem, nostack, preserves_flags),
        );
    }
}

/// Read a byte from an I/O port.
///
/// # Safety
///
/// See [`outb`]. Reads can have side effects on some devices.
#[inline]
pub unsafe fn inb(port: u16) -> u8 {
    let value: u8;
    // SAFETY: as above.
    unsafe {
        core::arch::asm!(
            "in al, dx",
            out("al") value,
            in("dx") port,
            options(nomem, nostack, preserves_flags),
        );
    }
    value
}

/// Write a 32-bit word to an I/O port.
///
/// # Safety
///
/// See [`outb`].
#[inline]
pub unsafe fn outl(port: u16, value: u32) {
    // SAFETY: as above.
    unsafe {
        core::arch::asm!(
            "out dx, eax",
            in("dx") port,
            in("eax") value,
            options(nomem, nostack, preserves_flags),
        );
    }
}

/// Read a 32-bit word from an I/O port.
///
/// # Safety
///
/// See [`outb`].
#[inline]
pub unsafe fn inl(port: u16) -> u32 {
    let value: u32;
    // SAFETY: as above.
    unsafe {
        core::arch::asm!(
            "in eax, dx",
            out("eax") value,
            in("dx") port,
            options(nomem, nostack, preserves_flags),
        );
    }
    value
}
