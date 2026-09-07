//! The Global Descriptor Table and Task State Segment.
//!
//! Long mode barely uses segmentation, but three things still require a GDT the
//! kernel owns rather than the one the firmware left behind:
//!
//! * a Task State Segment, which is how the CPU finds a kernel stack when it
//!   takes an interrupt from user mode, and how it finds a *known-good* stack
//!   when the current one is unusable (the Interrupt Stack Table);
//! * ring 3 descriptors, needed before any user code can run;
//! * a descriptor layout `syscall`/`sysret` can use.
//!
//! ## Descriptor order
//!
//! The order below is not arbitrary. `sysret` derives both of its selectors
//! from one `STAR` field: `CS = STAR[63:48] + 16` and `SS = STAR[63:48] + 8`,
//! while `syscall` uses `CS = STAR[47:32]` and `SS = STAR[47:32] + 8`. Placing
//! the 32-bit user code descriptor between the kernel pair and the 64-bit user
//! pair is what makes both instructions land on the right selectors. Getting
//! this wrong is not detectable until the first return to user mode, so it is
//! fixed now.

use core::mem::size_of;

use crate::sync::SpinLock;

/// Selector of the kernel code segment.
pub const KERNEL_CODE_SELECTOR: u16 = 0x08;
/// Selector of the kernel data segment.
pub const KERNEL_DATA_SELECTOR: u16 = 0x10;
/// Selector of the 32-bit user code segment. Present only to satisfy the
/// `sysret` selector arithmetic; NexusOS does not run 32-bit user code.
pub const USER_CODE32_SELECTOR: u16 = 0x18 | 3;
/// Selector of the user data segment.
pub const USER_DATA_SELECTOR: u16 = 0x20 | 3;
/// Selector of the 64-bit user code segment.
pub const USER_CODE64_SELECTOR: u16 = 0x28 | 3;
/// Selector of the Task State Segment descriptor.
pub const TSS_SELECTOR: u16 = 0x30;

/// Interrupt Stack Table slot used by the double-fault handler.
///
/// A double fault is frequently caused by the stack itself being unusable — a
/// kernel stack overflow into its guard page is the classic case. Switching to
/// a private stack is what turns that from a triple-fault reset into a
/// diagnosable event.
pub const IST_DOUBLE_FAULT: u16 = 1;
/// Interrupt Stack Table slot used by the NMI handler.
pub const IST_NMI: u16 = 2;
/// Interrupt Stack Table slot used by the machine-check handler.
pub const IST_MACHINE_CHECK: u16 = 3;

/// Size of each Interrupt Stack Table stack.
const IST_STACK_SIZE: usize = 16 * 1024;

/// Backing storage for the IST stacks.
///
/// These live in `.bss` rather than being allocated, because they must exist
/// before the physical allocator does — the whole point is that they are
/// reachable when everything else has gone wrong.
#[repr(align(16))]
struct IstStack([u8; IST_STACK_SIZE]);

static mut DOUBLE_FAULT_STACK: IstStack = IstStack([0; IST_STACK_SIZE]);
static mut NMI_STACK: IstStack = IstStack([0; IST_STACK_SIZE]);
static mut MACHINE_CHECK_STACK: IstStack = IstStack([0; IST_STACK_SIZE]);

/// The x86-64 Task State Segment.
///
/// Packed to 4-byte alignment because the architectural layout puts 64-bit
/// fields at 4-byte-aligned offsets, which no naturally aligned Rust struct can
/// reproduce.
#[repr(C, packed(4))]
#[derive(Clone, Copy)]
struct TaskStateSegment {
    reserved_0: u32,
    /// Stack pointers for rings 0, 1 and 2.
    privilege_stack_table: [u64; 3],
    reserved_1: u64,
    /// The seven Interrupt Stack Table entries, indexed from 1 in descriptors.
    interrupt_stack_table: [u64; 7],
    reserved_2: u64,
    reserved_3: u16,
    /// Offset of the I/O permission bitmap. Setting it past the segment limit
    /// denies user mode all port access, which is what NexusOS wants.
    iomap_base: u16,
}

impl TaskStateSegment {
    const fn new() -> Self {
        Self {
            reserved_0: 0,
            privilege_stack_table: [0; 3],
            reserved_1: 0,
            interrupt_stack_table: [0; 7],
            reserved_2: 0,
            reserved_3: 0,
            iomap_base: size_of::<TaskStateSegment>() as u16,
        }
    }
}

/// The Global Descriptor Table.
///
/// Nine slots: null, kernel code, kernel data, user code 32, user data, user
/// code 64, and two consumed by the 16-byte TSS descriptor, plus one spare so
/// the table is 16-byte aligned in whole entries.
#[repr(C, align(16))]
struct GlobalDescriptorTable {
    entries: [u64; 9],
}

/// Operand of `lgdt` and `lidt`.
#[repr(C, packed)]
struct DescriptorTablePointer {
    limit: u16,
    base: u64,
}

// Segment descriptors. In long mode the base and limit of code and data
// segments are ignored, but the access and flag bytes still select privilege
// level, direction and the 64-bit `L` bit, so they are spelled out in full.
//
// Access byte:  P DPL DPL S  Type
// Flags nibble: G  D/B L  AVL
/// Present, DPL 0, code, readable, `L` set, granularity 4 KiB.
const KERNEL_CODE_DESCRIPTOR: u64 = 0x00AF_9A00_0000_FFFF;
/// Present, DPL 0, data, writable.
const KERNEL_DATA_DESCRIPTOR: u64 = 0x00CF_9200_0000_FFFF;
/// Present, DPL 3, code, readable, 32-bit.
const USER_CODE32_DESCRIPTOR: u64 = 0x00CF_FA00_0000_FFFF;
/// Present, DPL 3, data, writable.
const USER_DATA_DESCRIPTOR: u64 = 0x00CF_F200_0000_FFFF;
/// Present, DPL 3, code, readable, `L` set.
const USER_CODE64_DESCRIPTOR: u64 = 0x00AF_FA00_0000_FFFF;

static mut GDT: GlobalDescriptorTable = GlobalDescriptorTable { entries: [0; 9] };
static mut TSS: TaskStateSegment = TaskStateSegment::new();

/// Guards one-time initialisation, so a second call is a no-op rather than a
/// corrupted descriptor table.
static INITIALISED: SpinLock<bool> = SpinLock::new(false);

/// Build the two halves of a 16-byte TSS descriptor.
fn tss_descriptor(base: u64, limit: u32) -> (u64, u64) {
    let mut low = u64::from(limit & 0xFFFF);
    low |= (base & 0xFF_FFFF) << 16;
    // Access byte 0x89: present, DPL 0, system, available 64-bit TSS.
    low |= 0x89 << 40;
    low |= u64::from((limit >> 16) & 0xF) << 48;
    low |= ((base >> 24) & 0xFF) << 56;
    (low, base >> 32)
}

/// Install the GDT and TSS on the current processor.
///
/// Loads the descriptor table, reloads every segment register, and loads the
/// task register.
///
/// # Safety
///
/// Replaces the descriptor tables the processor is currently using. Must run
/// with interrupts disabled, before any interrupt handler could observe the
/// half-updated state.
pub unsafe fn init() {
    let mut initialised = INITIALISED.lock();
    if *initialised {
        return;
    }

    // SAFETY: this runs once, single-threaded, before interrupts are enabled,
    // so nothing else can observe or race these statics. Addresses are taken
    // with `addr_of_mut!` to avoid ever forming a reference to a `static mut`.
    unsafe {
        let tss = core::ptr::addr_of_mut!(TSS);
        (*tss).interrupt_stack_table[(IST_DOUBLE_FAULT - 1) as usize] =
            stack_top(core::ptr::addr_of_mut!(DOUBLE_FAULT_STACK));
        (*tss).interrupt_stack_table[(IST_NMI - 1) as usize] =
            stack_top(core::ptr::addr_of_mut!(NMI_STACK));
        (*tss).interrupt_stack_table[(IST_MACHINE_CHECK - 1) as usize] =
            stack_top(core::ptr::addr_of_mut!(MACHINE_CHECK_STACK));

        let (tss_low, tss_high) =
            tss_descriptor(tss as u64, size_of::<TaskStateSegment>() as u32 - 1);

        let gdt = core::ptr::addr_of_mut!(GDT);
        (*gdt).entries[0] = 0;
        (*gdt).entries[1] = KERNEL_CODE_DESCRIPTOR;
        (*gdt).entries[2] = KERNEL_DATA_DESCRIPTOR;
        (*gdt).entries[3] = USER_CODE32_DESCRIPTOR;
        (*gdt).entries[4] = USER_DATA_DESCRIPTOR;
        (*gdt).entries[5] = USER_CODE64_DESCRIPTOR;
        (*gdt).entries[6] = tss_low;
        (*gdt).entries[7] = tss_high;
        (*gdt).entries[8] = 0;

        let pointer = DescriptorTablePointer {
            limit: (size_of::<GlobalDescriptorTable>() - 1) as u16,
            base: gdt as u64,
        };

        core::arch::asm!(
            "lgdt [{pointer}]",
            pointer = in(reg) &pointer,
            options(readonly, nostack, preserves_flags),
        );

        load_code_segment(KERNEL_CODE_SELECTOR);
        load_data_segments(KERNEL_DATA_SELECTOR);

        core::arch::asm!(
            "ltr {selector:x}",
            selector = in(reg) TSS_SELECTOR,
            options(nostack, preserves_flags),
        );
    }

    *initialised = true;
}

/// Top (highest address) of an IST stack, 16-byte aligned as the ABI requires.
fn stack_top(stack: *mut IstStack) -> u64 {
    // Stacks grow down, so the CPU is given the end of the array.
    (stack as u64 + IST_STACK_SIZE as u64) & !0xF
}

/// Reload `cs`, which cannot be assigned to directly.
///
/// A far return is the standard way: push the target selector and address, then
/// let `retfq` load both at once.
///
/// # Safety
///
/// `selector` must index a present, executable, 64-bit code descriptor in the
/// currently loaded GDT.
unsafe fn load_code_segment(selector: u16) {
    // SAFETY: upheld by the caller. The far return lands on the label directly
    // below it, so control flow is unchanged apart from `cs`.
    unsafe {
        core::arch::asm!(
            "push {selector}",
            "lea {target}, [rip + 2f]",
            "push {target}",
            "retfq",
            "2:",
            selector = in(reg) u64::from(selector),
            target = lateout(reg) _,
            options(preserves_flags),
        );
    }
}

/// Reload the data segment registers.
///
/// # Safety
///
/// `selector` must index a present, writable data descriptor in the currently
/// loaded GDT.
unsafe fn load_data_segments(selector: u16) {
    // SAFETY: upheld by the caller. `fs` and `gs` are deliberately left alone:
    // their bases are set through MSRs and will carry per-CPU data.
    unsafe {
        core::arch::asm!(
            "mov ds, {selector:x}",
            "mov es, {selector:x}",
            "mov ss, {selector:x}",
            selector = in(reg) selector,
            options(nostack, preserves_flags),
        );
    }
}

/// Record the kernel stack the CPU should switch to on entry from user mode.
///
/// Called by the scheduler on every context switch once user mode exists: the
/// value must always be the top of the *current* thread's kernel stack.
///
/// # Safety
///
/// `stack_top` must be the top of a valid, mapped kernel stack.
pub unsafe fn set_kernel_stack(stack_top: u64) {
    // SAFETY: writing one field of this CPU's TSS; the CPU reads it only on a
    // privilege transition, which cannot be in progress here.
    unsafe {
        (*core::ptr::addr_of_mut!(TSS)).privilege_stack_table[0] = stack_top;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tss_matches_the_architectural_layout() {
        assert_eq!(size_of::<TaskStateSegment>(), 104);
    }

    /// `sysret` computes both selectors from one base; `syscall` from another.
    /// These relationships are what the descriptor ordering exists to satisfy.
    #[test]
    fn descriptor_order_satisfies_syscall_and_sysret() {
        // syscall: CS = base, SS = base + 8.
        assert_eq!(KERNEL_DATA_SELECTOR, KERNEL_CODE_SELECTOR + 8);
        // sysret: CS = base + 16, SS = base + 8, both at DPL 3.
        let sysret_base = USER_CODE32_SELECTOR & !3;
        assert_eq!(USER_CODE64_SELECTOR & !3, sysret_base + 16);
        assert_eq!(USER_DATA_SELECTOR & !3, sysret_base + 8);
        assert_eq!(USER_CODE64_SELECTOR & 3, 3);
        assert_eq!(USER_DATA_SELECTOR & 3, 3);
    }

    #[test]
    fn tss_descriptor_encodes_base_and_limit() {
        let base = 0x0000_1234_5678_9ABCu64;
        let (low, high) = tss_descriptor(base, 103);

        assert_eq!(low & 0xFFFF, 103, "limit bits 0..16");
        assert_eq!((low >> 16) & 0xFF_FFFF, base & 0xFF_FFFF, "base bits 0..24");
        assert_eq!(
            (low >> 40) & 0xFF,
            0x89,
            "available 64-bit TSS, present, DPL 0"
        );
        assert_eq!((low >> 56) & 0xFF, (base >> 24) & 0xFF, "base bits 24..32");
        assert_eq!(high, base >> 32, "base bits 32..64");
    }
}
