//! The application-processor startup trampoline.
//!
//! A processor woken by a startup inter-processor interrupt begins in **16-bit
//! real mode**, at `CS = vector << 8`, with no paging, no long mode and no
//! stack. Everything the rest of the kernel takes for granted has to be built
//! from that, in assembly, before any Rust can run.
//!
//! # Why it lives at a fixed low address
//!
//! The startup IPI carries a vector, not an address: the processor begins at
//! `vector << 12`, which must therefore be page-aligned and below 1 MiB. The
//! kernel is linked in the top 2 GiB and cannot be executed from there in real
//! mode, so this code is assembled separately and copied down to
//! [`TRAMPOLINE_ADDRESS`] before the IPI is sent. That page is excluded from the
//! frame allocator so nothing else can ever be given it.
//!
//! # Why the descriptor tables are built at runtime
//!
//! They could have been assembled into this page as data, but then `lgdt` would
//! have to address them as the difference between two labels, and the assembler
//! rejects more than one symbol in a memory operand. Placing them at fixed
//! offsets and filling them in from Rust turns every address the assembly needs
//! into a single constant. It also keeps the tables next to the parameters that
//! are already written this way.
//!
//! # The identity mapping
//!
//! Between enabling paging and the far jump into the higher half, the processor
//! is still executing at its low physical address. Those instructions must be
//! mapped where they are, so the kernel restores a low identity mapping for the
//! duration of bring-up and removes it afterwards.

use core::arch::global_asm;

/// Physical address the trampoline is copied to and started at.
///
/// Must be page-aligned and below 1 MiB for the startup IPI's vector encoding.
/// 0x8000 is clear of the interrupt vector table, the BIOS data area and the
/// EBDA on every machine this targets.
pub const TRAMPOLINE_ADDRESS: u64 = 0x8000;

/// Startup IPI vector corresponding to [`TRAMPOLINE_ADDRESS`].
pub const TRAMPOLINE_VECTOR: u8 = (TRAMPOLINE_ADDRESS >> 12) as u8;

/// Offsets, within the trampoline page, of everything the boot processor fills
/// in before starting a processor.
pub mod parameter {
    /// Root page table for the starting processor to load, as a `u64`.
    pub const PAGE_TABLE: usize = 0x0F00;
    /// Top of the stack it should run on, as a `u64`.
    pub const STACK_TOP: usize = 0x0F08;
    /// Address of the Rust entry point to jump to, as a `u64`.
    pub const ENTRY: usize = 0x0F10;
    /// Index the starting processor should adopt, as a `u64`.
    pub const CPU_INDEX: usize = 0x0F18;
    /// Set to 1 by the starting processor once it reaches long mode.
    pub const ACKNOWLEDGE: usize = 0x0F20;

    /// Three 8-byte descriptors: null, 32-bit code, 32-bit data.
    pub const GDT32: usize = 0x0F40;
    /// `lgdt` operand for [`GDT32`]: a 2-byte limit and a 4-byte base.
    pub const GDT32_POINTER: usize = 0x0F60;
    /// Three 8-byte descriptors: null, 64-bit code, data.
    pub const GDT64: usize = 0x0F70;
    /// `lgdt` operand for [`GDT64`], also with a 4-byte base: it is loaded
    /// while the processor is still in 32-bit mode, where `lgdt` reads six
    /// bytes rather than ten.
    pub const GDT64_POINTER: usize = 0x0F90;
}

global_asm!(
    r#"
.section .rodata.trampoline, "a"
.balign 4096
.code16
.globl nexus_ap_trampoline_start
nexus_ap_trampoline_start:
    cli
    cld

    // Real mode has no absolute addressing, so point every data segment at the
    // code segment and address this page as offsets within it.
    mov ax, cs
    mov ds, ax
    mov es, ax
    mov ss, ax

    // Enter protected mode. The descriptor table lives at a fixed offset in
    // this page, so the operand is one constant. Its base is below 16 MiB,
    // which the 16-bit form of lgdt -- only 24 bits of base -- can reach.
    lgdt [{off_gdt32_pointer}]
    mov eax, cr0
    or eax, 1
    mov cr0, eax

    // Far jump to load the new code segment, emitted as bytes because a far
    // immediate jump has no unambiguous Intel-syntax spelling across
    // assemblers. 0x66 0xEA is jmp far with a 32-bit offset in 16-bit mode.
    .byte 0x66, 0xEA
    .long {base} + (protected_entry - nexus_ap_trampoline_start)
    .word 0x08

.code32
protected_entry:
    mov ax, 0x10
    mov ds, ax
    mov es, ax
    mov ss, ax
    mov fs, ax
    mov gs, ax

    // Physical Address Extension, which long mode requires.
    mov eax, cr4
    or eax, (1 << 5)
    mov cr4, eax

    // Adopt the kernel's page tables, which the boot processor left here.
    mov eax, [{param_page_table}]
    mov cr3, eax

    // Long mode enable, and no-execute enable. NXE is not optional: the
    // kernel's tables set the no-execute bit, and without NXE those entries are
    // reserved-bit violations, so the first instruction fetch after paging
    // would fault.
    mov ecx, 0xC0000080
    rdmsr
    or eax, (1 << 8) | (1 << 11)
    wrmsr

    // Paging on. From here until the jump below, this code runs at its physical
    // address, which the identity mapping covers.
    mov eax, cr0
    or eax, (1 << 31)
    mov cr0, eax

    lgdt [{param_gdt64_pointer}]

    .byte 0xEA
    .long {base} + (long_entry - nexus_ap_trampoline_start)
    .word 0x08

.code64
long_entry:
    // Segment registers other than CS are ignored in long mode, but a stale
    // 32-bit selector in one would fault the first time anything reloaded it.
    xor eax, eax
    mov ds, ax
    mov es, ax
    mov ss, ax
    mov fs, ax
    mov gs, ax

    mov rsp, [{param_stack_top}]
    xor rbp, rbp

    // Tell the boot processor this core got this far, so a core that hangs
    // earlier is distinguishable from one that never started at all.
    mov qword ptr [{param_acknowledge}], 1

    // The processor index is the entry point's only argument.
    mov rdi, [{param_cpu_index}]
    mov rax, [{param_entry}]

    // A zero return address terminates the call chain for backtraces and leaves
    // the stack aligned as the ABI expects at a function's entry.
    push 0
    jmp rax

.globl nexus_ap_trampoline_end
nexus_ap_trampoline_end:
"#,
    base = const TRAMPOLINE_ADDRESS,
    off_gdt32_pointer = const parameter::GDT32_POINTER,
    param_page_table = const TRAMPOLINE_ADDRESS as usize + parameter::PAGE_TABLE,
    param_stack_top = const TRAMPOLINE_ADDRESS as usize + parameter::STACK_TOP,
    param_entry = const TRAMPOLINE_ADDRESS as usize + parameter::ENTRY,
    param_cpu_index = const TRAMPOLINE_ADDRESS as usize + parameter::CPU_INDEX,
    param_acknowledge = const TRAMPOLINE_ADDRESS as usize + parameter::ACKNOWLEDGE,
    param_gdt64_pointer = const TRAMPOLINE_ADDRESS as usize + parameter::GDT64_POINTER,
);

extern "C" {
    /// First byte of the assembled trampoline.
    static nexus_ap_trampoline_start: u8;
    /// One past its last byte.
    static nexus_ap_trampoline_end: u8;
}

/// The assembled trampoline, as bytes to copy.
#[must_use]
pub fn code() -> &'static [u8] {
    // SAFETY: both symbols are defined by the block above; the region between
    // them is the trampoline, in `.rodata`, and is never written.
    unsafe {
        let start = core::ptr::addr_of!(nexus_ap_trampoline_start);
        let end = core::ptr::addr_of!(nexus_ap_trampoline_end);
        core::slice::from_raw_parts(start, end as usize - start as usize)
    }
}

/// Descriptors the trampoline switches through, in the order it loads them.
///
/// A flat 4 GiB code and data pair for protected mode, then a 64-bit code
/// descriptor with the long-mode bit set. The kernel's real GDT is loaded much
/// later, once the processor is running Rust.
mod descriptor {
    /// 32-bit code, base 0, limit 4 GiB, granularity 4 KiB.
    pub const CODE32: u64 = 0x00CF_9A00_0000_FFFF;
    /// 32-bit data, base 0, limit 4 GiB.
    pub const DATA32: u64 = 0x00CF_9200_0000_FFFF;
    /// 64-bit code, with the `L` bit set.
    pub const CODE64: u64 = 0x00AF_9A00_0000_FFFF;
    /// Data, as long mode ignores its base and limit.
    pub const DATA64: u64 = 0x00CF_9200_0000_FFFF;
}

/// Write the descriptor tables and their `lgdt` operands into the page.
///
/// # Safety
///
/// The trampoline page must be mapped writable, and no processor may be
/// executing from it.
pub unsafe fn write_descriptor_tables(page: *mut u8) {
    /// Descriptors per table.
    const ENTRIES: usize = 3;

    // SAFETY: the caller guarantees the page is writable and idle; every offset
    // below is inside it.
    unsafe {
        let gdt32 = page.add(parameter::GDT32) as *mut u64;
        gdt32.write_unaligned(0);
        gdt32.add(1).write_unaligned(descriptor::CODE32);
        gdt32.add(2).write_unaligned(descriptor::DATA32);

        let gdt64 = page.add(parameter::GDT64) as *mut u64;
        gdt64.write_unaligned(0);
        gdt64.add(1).write_unaligned(descriptor::CODE64);
        gdt64.add(2).write_unaligned(descriptor::DATA64);

        let limit = (ENTRIES * 8 - 1) as u16;

        let pointer32 = page.add(parameter::GDT32_POINTER);
        (pointer32 as *mut u16).write_unaligned(limit);
        (pointer32.add(2) as *mut u32)
            .write_unaligned(TRAMPOLINE_ADDRESS as u32 + parameter::GDT32 as u32);

        let pointer64 = page.add(parameter::GDT64_POINTER);
        (pointer64 as *mut u16).write_unaligned(limit);
        (pointer64.add(2) as *mut u32)
            .write_unaligned(TRAMPOLINE_ADDRESS as u32 + parameter::GDT64 as u32);
    }
}
