//! The legacy 8259A programmable interrupt controller.
//!
//! The PIC powers up delivering IRQ 0..7 on vectors 8..15, which collide with
//! the architectural exceptions — IRQ 0, the timer, arrives on vector 8, the
//! double-fault vector. Remapping it is not optional on any x86 machine, even
//! one that intends to use the APIC instead: a spurious legacy interrupt would
//! otherwise be indistinguishable from a double fault.
//!
//! NexusOS uses the PIC for its first timer because it is pure port I/O and
//! needs no MMIO mapping, which the kernel cannot yet create. The local APIC
//! replaces it once the virtual memory manager exists.

use super::idt::IRQ_BASE;
use super::io::{inb, outb};

/// Command port of the primary PIC.
const PRIMARY_COMMAND: u16 = 0x20;
/// Data port of the primary PIC.
const PRIMARY_DATA: u16 = 0x21;
/// Command port of the secondary PIC.
const SECONDARY_COMMAND: u16 = 0xA0;
/// Data port of the secondary PIC.
const SECONDARY_DATA: u16 = 0xA1;

/// ICW1: begin initialisation, expect ICW4.
const ICW1_INIT: u8 = 0x11;
/// ICW4: 8086/88 mode.
const ICW4_8086: u8 = 0x01;
/// OCW2: non-specific end of interrupt.
const END_OF_INTERRUPT: u8 = 0x20;

/// Vector the primary PIC's IRQ 0 is remapped to.
pub const PRIMARY_VECTOR_BASE: u8 = IRQ_BASE;
/// Vector the secondary PIC's IRQ 8 is remapped to.
pub const SECONDARY_VECTOR_BASE: u8 = IRQ_BASE + 8;

/// Vector the periodic timer arrives on.
pub const TIMER_VECTOR: u8 = PRIMARY_VECTOR_BASE;
/// Vector the PS/2 keyboard arrives on.
pub const KEYBOARD_VECTOR: u8 = PRIMARY_VECTOR_BASE + 1;

/// Waste a moment so an older PIC has time to latch a command word.
///
/// Port 0x80 is the POST diagnostic port: writing to it is harmless and takes
/// roughly a bus cycle, which is the conventional way to pace 8259 setup.
#[inline]
fn io_wait() {
    // SAFETY: port 0x80 is the POST code port; a write there has no effect on
    // any machine NexusOS targets.
    unsafe { outb(0x80, 0) };
}

/// Remap both PICs above the exception vectors and mask every line.
///
/// Lines are unmasked individually afterwards with [`unmask`].
///
/// # Safety
///
/// Reprograms interrupt-controller hardware. Must run with interrupts disabled.
pub unsafe fn init() {
    // SAFETY: the standard 8259A initialisation sequence, touching only the
    // four PIC ports and the POST port.
    unsafe {
        // Start initialisation on both chips.
        outb(PRIMARY_COMMAND, ICW1_INIT);
        io_wait();
        outb(SECONDARY_COMMAND, ICW1_INIT);
        io_wait();

        // ICW2: vector offsets.
        outb(PRIMARY_DATA, PRIMARY_VECTOR_BASE);
        io_wait();
        outb(SECONDARY_DATA, SECONDARY_VECTOR_BASE);
        io_wait();

        // ICW3: the secondary is cascaded onto the primary's IRQ 2.
        outb(PRIMARY_DATA, 1 << 2);
        io_wait();
        outb(SECONDARY_DATA, 2);
        io_wait();

        // ICW4: 8086 mode.
        outb(PRIMARY_DATA, ICW4_8086);
        io_wait();
        outb(SECONDARY_DATA, ICW4_8086);
        io_wait();

        // Mask everything. Nothing is delivered until a driver asks for it.
        outb(PRIMARY_DATA, 0xFF);
        outb(SECONDARY_DATA, 0xFF);
    }
}

/// Mask every line on both controllers.
///
/// Used when handing interrupt delivery over to the APIC.
///
/// # Safety
///
/// Reprograms interrupt-controller hardware.
pub unsafe fn mask_all() {
    // SAFETY: writing the interrupt mask registers of both PICs.
    unsafe {
        outb(PRIMARY_DATA, 0xFF);
        outb(SECONDARY_DATA, 0xFF);
    }
}

/// Allow `irq` (0..16) to be delivered.
///
/// # Safety
///
/// The caller must have registered a handler for the vector this IRQ maps to;
/// otherwise the first interrupt lands on the catch-all and stops the machine.
pub unsafe fn unmask(irq: u8) {
    let (port, bit) = if irq < 8 {
        (PRIMARY_DATA, irq)
    } else {
        (SECONDARY_DATA, irq - 8)
    };
    // SAFETY: read-modify-write of one PIC mask register.
    unsafe {
        let mask = inb(port) & !(1 << bit);
        outb(port, mask);
    }

    // A line on the secondary controller only reaches the CPU if the cascade
    // line on the primary is open too.
    if irq >= 8 {
        // SAFETY: as above.
        unsafe {
            let mask = inb(PRIMARY_DATA) & !(1 << 2);
            outb(PRIMARY_DATA, mask);
        }
    }
}

/// Acknowledge the interrupt currently being serviced.
///
/// Must be called by every IRQ handler before it returns, or the controller
/// will not deliver anything further on that line.
///
/// # Safety
///
/// Call exactly once per delivered interrupt, from its handler.
pub unsafe fn end_of_interrupt(vector: u8) {
    // SAFETY: writing OCW2 to the controllers that delivered this vector. An
    // interrupt from the secondary chip must be acknowledged on both, because
    // the primary is still holding the cascade line.
    unsafe {
        if vector >= SECONDARY_VECTOR_BASE {
            outb(SECONDARY_COMMAND, END_OF_INTERRUPT);
        }
        outb(PRIMARY_COMMAND, END_OF_INTERRUPT);
    }
}
