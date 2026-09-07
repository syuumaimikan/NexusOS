//! The Interrupt Descriptor Table.
//!
//! Every vector the CPU can raise gets an entry here. Until this table is
//! loaded, a fault has nowhere to go: the CPU faults trying to dispatch the
//! fault, then faults again, and the third one resets the machine with no
//! diagnostic at all. Installing the IDT is therefore the single highest-value
//! thing the kernel does after it can print.

use core::mem::size_of;

use super::gdt::KERNEL_CODE_SELECTOR;

/// Number of vectors in an x86-64 IDT.
pub const VECTOR_COUNT: usize = 256;

/// First vector available for device interrupts.
///
/// Vectors 0..32 are reserved by the architecture for exceptions, so external
/// interrupts have to be remapped above them. The legacy PIC powers up
/// delivering IRQ 0 on vector 8 — the double-fault vector — which is why
/// remapping it is not optional.
pub const IRQ_BASE: u8 = 32;

/// Gate type and attributes: present, DPL 0, 64-bit interrupt gate.
///
/// Interrupt gates clear `IF` on entry, so a handler does not have to defend
/// against being re-entered by another device interrupt.
const INTERRUPT_GATE: u8 = 0x8E;

/// One 16-byte IDT entry.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct Entry {
    offset_low: u16,
    selector: u16,
    /// Bits 0..3 select an Interrupt Stack Table slot; 0 means "keep the
    /// current stack".
    ist: u8,
    flags: u8,
    offset_mid: u16,
    offset_high: u32,
    reserved: u32,
}

impl Entry {
    /// An entry with the present bit clear.
    pub const fn missing() -> Self {
        Self {
            offset_low: 0,
            selector: 0,
            ist: 0,
            flags: 0,
            offset_mid: 0,
            offset_high: 0,
            reserved: 0,
        }
    }

    /// Point this entry at `handler`, optionally on IST slot `ist`.
    fn set_handler(&mut self, handler: u64, ist: u16) {
        self.offset_low = handler as u16;
        self.offset_mid = (handler >> 16) as u16;
        self.offset_high = (handler >> 32) as u32;
        self.selector = KERNEL_CODE_SELECTOR;
        self.ist = (ist & 0x7) as u8;
        self.flags = INTERRUPT_GATE;
        self.reserved = 0;
    }
}

/// The interrupt descriptor table.
#[repr(C, align(16))]
pub struct InterruptDescriptorTable {
    pub entries: [Entry; VECTOR_COUNT],
}

impl InterruptDescriptorTable {
    /// A table in which every vector is absent.
    pub const fn new() -> Self {
        Self {
            entries: [Entry::missing(); VECTOR_COUNT],
        }
    }

    /// Install `handler` on `vector`, running on the current stack.
    ///
    /// # Safety
    ///
    /// `handler` must be a function with the `x86-interrupt` calling
    /// convention and the argument shape the vector requires: exceptions that
    /// push an error code need a handler that takes one.
    pub unsafe fn set_handler(&mut self, vector: u8, handler: *const ()) {
        self.entries[vector as usize].set_handler(handler as u64, 0);
    }

    /// Install `handler` on `vector`, switching to Interrupt Stack Table slot
    /// `ist` on entry.
    ///
    /// # Safety
    ///
    /// See [`InterruptDescriptorTable::set_handler`]. `ist` must name a slot
    /// the TSS has been given a stack for.
    pub unsafe fn set_handler_with_stack(&mut self, vector: u8, handler: *const (), ist: u16) {
        self.entries[vector as usize].set_handler(handler as u64, ist);
    }

    /// Load this table into the processor.
    ///
    /// # Safety
    ///
    /// The table must live for as long as it is loaded — in practice, forever —
    /// and every present entry must point at a valid handler.
    pub unsafe fn load(&'static self) {
        #[repr(C, packed)]
        struct DescriptorTablePointer {
            limit: u16,
            base: u64,
        }

        let pointer = DescriptorTablePointer {
            limit: (size_of::<InterruptDescriptorTable>() - 1) as u16,
            base: self as *const _ as u64,
        };

        // SAFETY: upheld by the caller; `pointer` describes this table exactly.
        unsafe {
            core::arch::asm!(
                "lidt [{pointer}]",
                pointer = in(reg) &pointer,
                options(readonly, nostack, preserves_flags),
            );
        }
    }
}

impl Default for InterruptDescriptorTable {
    fn default() -> Self {
        Self::new()
    }
}

/// The machine state the CPU pushes when it dispatches an interrupt.
///
/// Laid out exactly as the architecture defines, so a handler declared with the
/// `x86-interrupt` ABI receives it directly.
#[repr(C)]
#[derive(Clone, Copy)]
pub struct InterruptStackFrame {
    /// Address of the instruction that faulted, or the one to resume at.
    pub instruction_pointer: u64,
    pub code_segment: u64,
    pub cpu_flags: u64,
    pub stack_pointer: u64,
    pub stack_segment: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn entry_is_sixteen_bytes_and_the_table_is_four_kibibytes() {
        assert_eq!(size_of::<Entry>(), 16);
        assert_eq!(size_of::<InterruptDescriptorTable>(), 16 * 256);
    }

    #[test]
    fn set_handler_splits_the_address_across_three_fields() {
        let mut entry = Entry::missing();
        let handler = 0xFFFF_FFFF_8012_3456u64;
        entry.set_handler(handler, 0);

        assert_eq!(entry.offset_low, 0x3456);
        assert_eq!(entry.offset_mid, 0x8012);
        assert_eq!(entry.offset_high, 0xFFFF_FFFF);
        assert_eq!(entry.selector, KERNEL_CODE_SELECTOR);
        assert_eq!(entry.flags, INTERRUPT_GATE);
        assert_eq!(entry.ist, 0);
    }

    #[test]
    fn only_the_low_three_bits_of_an_ist_index_are_stored() {
        let mut entry = Entry::missing();
        entry.set_handler(0x1000, 1);
        assert_eq!(entry.ist, 1);

        // Slot numbers above 7 do not exist; the field must not spill into the
        // reserved bits beside it.
        entry.set_handler(0x1000, 0xFF);
        assert_eq!(entry.ist, 7);
    }

    #[test]
    fn irq_vectors_start_above_the_architectural_exceptions() {
        assert!(IRQ_BASE >= 32, "vectors 0..32 belong to the architecture");
    }
}
