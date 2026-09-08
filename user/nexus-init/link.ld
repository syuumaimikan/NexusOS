/*
 * Link script for a NexusOS user program.
 *
 * Loaded at four megabytes: low enough to be obviously user space, high enough
 * that a null dereference is nowhere near it. The kernel maps each segment with
 * the permissions its header asks for, so text and data are separated onto page
 * boundaries and never share one.
 */

OUTPUT_FORMAT(elf64-x86-64)
OUTPUT_ARCH(i386:x86-64)
ENTRY(_start)

USER_BASE = 0x400000;

PHDRS
{
    text   PT_LOAD FLAGS(5); /* R-X */
    rodata PT_LOAD FLAGS(4); /* R-- */
    data   PT_LOAD FLAGS(6); /* RW- */
}

SECTIONS
{
    . = USER_BASE;

    .text ALIGN(4K) : { KEEP(*(.text.entry)) *(.text .text.*) } :text

    . = ALIGN(4K);
    .rodata ALIGN(4K) : { *(.rodata .rodata.*) } :rodata

    . = ALIGN(4K);
    .data ALIGN(4K) : { *(.data .data.*) *(.got .got.plt) } :data
    .bss ALIGN(4K) : { *(.bss .bss.*) *(COMMON) . = ALIGN(4K); } :data

    /DISCARD/ :
    {
        *(.eh_frame) *(.eh_frame_hdr) *(.note .note.*) *(.comment)
        *(.interp) *(.dynamic) *(.dynsym .dynstr) *(.hash .gnu.hash)
    }
}
