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

/// Restores the kernel's `GS` base for as long as a handler entered from ring 3
/// is running, and puts the user's back on the way out.
///
/// Every entry into the kernel from user mode has to do this, and doing it in
/// the handler rather than in an assembly stub works because nothing between
/// the processor pushing this frame and the guard being constructed touches
/// `GS`: the `x86-interrupt` prologue only saves registers, and reading the
/// frame is stack-relative.
///
/// The alternative — trusting `GS` across a trip through ring 3 — is not
/// available. User code can zero `GS.base` with a single `mov gs, ax`, and the
/// next timer interrupt would then read this processor's state through a null
/// pointer.
pub struct KernelGs(bool);

/// Interrupts and exceptions taken while a processor was in ring 3.
///
/// The one piece of evidence that user code really ran at user privilege, and
/// that it was preemptible while it did. A user program can be observed making
/// system calls without either being true -- `syscall` is legal from ring 0 --
/// so this counts the entries that could only have come from ring 3, which is
/// the ones where the saved code selector says so.
static FROM_USER: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);

/// How many interrupts and exceptions have arrived from ring 3.
#[must_use]
pub fn entries_from_user() -> u64 {
    FROM_USER.load(core::sync::atomic::Ordering::Relaxed)
}

impl KernelGs {
    /// Swap in the kernel's `GS` if `frame` was pushed by user code.
    #[inline]
    #[must_use]
    pub fn enter(frame: &InterruptStackFrame) -> Self {
        // The low two bits of the saved code selector are the privilege the
        // interrupted code was running at.
        let from_user = frame.code_segment & 3 == 3;
        if from_user {
            FROM_USER.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
            // SAFETY: entered from ring 3, so the kernel's base is the
            // inactive one; `Drop` performs the matching swap.
            unsafe { super::percpu::swap_gs() };
        }
        Self(from_user)
    }
}

impl Drop for KernelGs {
    fn drop(&mut self) {
        if self.0 {
            // SAFETY: pairs with the swap in `enter`.
            unsafe { super::percpu::swap_gs() };
        }
    }
}

/// Check the descriptor layout against the architecture.
///
/// None of this touches the processor: it is what a descriptor has to look like
/// in memory before one is ever loaded, and getting it wrong produces a machine
/// that triple-faults on the first interrupt with nothing to say about why.
/// Run at boot rather than under `cargo test`; see [`crate::selftest`].
pub fn layout_self_test() -> Result<(), &'static str> {
    if size_of::<Entry>() != 16 || size_of::<InterruptDescriptorTable>() != 16 * 256 {
        return Err("a descriptor or the table is not the size the architecture defines");
    }

    // A handler address is stored in three separate fields, which is the one
    // part of the encoding a reader is likely to get wrong.
    let mut entry = Entry::missing();
    entry.set_handler(0xFFFF_FFFF_8012_3456, 0);
    if entry.offset_low != 0x3456 || entry.offset_mid != 0x8012 || entry.offset_high != 0xFFFF_FFFF
    {
        return Err("a handler address was not split across the three offset fields");
    }
    if entry.selector != KERNEL_CODE_SELECTOR || entry.flags != INTERRUPT_GATE || entry.ist != 0 {
        return Err("a gate was not built as a kernel interrupt gate");
    }

    // Stack slots above seven do not exist, and the field must not spill into
    // the reserved bits beside it.
    entry.set_handler(0x1000, 1);
    if entry.ist != 1 {
        return Err("an interrupt-stack slot was not stored");
    }
    entry.set_handler(0x1000, 0xFF);
    if entry.ist != 7 {
        return Err("an out-of-range stack slot spilled into the reserved bits");
    }

    if IRQ_BASE < 32 {
        return Err("device vectors overlap the architectural exceptions");
    }
    Ok(())
}
