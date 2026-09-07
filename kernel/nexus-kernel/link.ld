/*
 * Link script for the Nexus kernel.
 *
 * The kernel is a static, non-PIE ELF executable linked into the top 2 GiB of
 * the address space, which is what LLVM's `kernel` code model assumes.
 *
 * Each output segment starts on a page boundary and carries distinct
 * permissions, so the bootloader can map `.text` read-only/executable and
 * everything else writable/non-executable without any range overlapping a page
 * that needs different rights.
 */

OUTPUT_FORMAT(elf64-x86-64)
OUTPUT_ARCH(i386:x86-64)
ENTRY(_start)

KERNEL_VIRTUAL_BASE = 0xFFFFFFFF80000000;

PHDRS
{
    text   PT_LOAD FLAGS(5); /* R-X */
    rodata PT_LOAD FLAGS(4); /* R-- */
    data   PT_LOAD FLAGS(6); /* RW- */
}

SECTIONS
{
    . = KERNEL_VIRTUAL_BASE;
    __kernel_start = .;

    .text ALIGN(4K) : AT(ADDR(.text))
    {
        __text_start = .;
        /* The entry stub is placed first so that the kernel's load address and
           its entry point coincide, which makes early debugging simpler. */
        KEEP(*(.text.boot))
        *(.text .text.*)
        __text_end = .;
    } :text

    . = ALIGN(4K);
    .rodata ALIGN(4K) : AT(ADDR(.rodata))
    {
        __rodata_start = .;
        *(.rodata .rodata.*)
        *(.gcc_except_table .gcc_except_table.*)
        __rodata_end = .;
    } :rodata

    . = ALIGN(4K);
    .data ALIGN(4K) : AT(ADDR(.data))
    {
        __data_start = .;
        *(.data .data.*)
        *(.got .got.plt)
        __data_end = .;
    } :data

    .bss ALIGN(4K) : AT(ADDR(.bss))
    {
        __bss_start = .;
        *(.bss .bss.*)
        *(COMMON)
        . = ALIGN(4K);
        __bss_end = .;
    } :data

    . = ALIGN(4K);
    __kernel_end = .;

    /* Unwinding is never used: the kernel is built with `panic = "abort"`. */
    /DISCARD/ :
    {
        *(.eh_frame)
        *(.eh_frame_hdr)
        *(.note .note.*)
        *(.comment)
        *(.interp)
        *(.dynamic)
        *(.dynsym .dynstr)
        *(.hash .gnu.hash)
    }
}
