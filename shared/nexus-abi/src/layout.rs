//! The NexusOS virtual address space layout (x86_64, 4-level paging).
//!
//! ```text
//! 0x0000_0000_0000_0000 .. 0x0000_8000_0000_0000   user space (128 TiB)
//! 0xFFFF_8000_0000_0000 .. 0xFFFF_C000_0000_0000   direct physical map (64 TiB)
//! 0xFFFF_C000_0000_0000 .. 0xFFFF_C800_0000_0000   kernel heap
//! 0xFFFF_C800_0000_0000 .. 0xFFFF_D000_0000_0000   kernel dynamic mappings (MMIO, framebuffer)
//! 0xFFFF_FFFF_8000_0000 .. 0xFFFF_FFFF_FFFF_FFFF   kernel image (-2 GiB, `code-model = kernel`)
//! ```
//!
//! The kernel image lives in the top 2 GiB because the kernel is compiled with
//! LLVM's `kernel` code model, which assumes exactly that.

/// Base of the direct map of all physical memory.
pub const PHYS_MAP_BASE: u64 = 0xFFFF_8000_0000_0000;
/// Size of the direct-map window (64 TiB).
pub const PHYS_MAP_SIZE: u64 = 0x0000_4000_0000_0000;

/// Base of the kernel heap region.
pub const KERNEL_HEAP_BASE: u64 = 0xFFFF_C000_0000_0000;
/// Initial kernel heap size (16 MiB); grows on demand.
pub const KERNEL_HEAP_INITIAL_SIZE: u64 = 16 * 1024 * 1024;

/// Base of the region used for dynamic kernel mappings (MMIO, framebuffer...).
pub const KERNEL_MMIO_BASE: u64 = 0xFFFF_C800_0000_0000;

/// Base of the stack the kernel starts on.
///
/// It sits in its own hole rather than in the direct map so that the unmapped
/// page immediately below acts as a guard page: overflowing the boot stack
/// faults instead of silently corrupting whatever physical memory happens to
/// be mapped underneath.
pub const KERNEL_BOOT_STACK_BASE: u64 = 0xFFFF_C7FF_FF00_0000;
/// Size of the boot stack.
pub const KERNEL_BOOT_STACK_SIZE: u64 = 64 * 1024;

/// Virtual base address the kernel image is linked at.
pub const KERNEL_IMAGE_BASE: u64 = 0xFFFF_FFFF_8000_0000;

/// Exclusive upper bound of the user-space half of the address space.
pub const USER_SPACE_END: u64 = 0x0000_8000_0000_0000;

/// Architectural page size.
pub const PAGE_SIZE: u64 = 4096;

/// Translate a physical address to its direct-map virtual address.
///
/// This is a pure arithmetic helper; it does not check that `phys` is actually
/// backed by RAM or that the direct map covers it.
#[inline]
#[must_use]
pub const fn phys_to_virt(phys: u64) -> u64 {
    PHYS_MAP_BASE + phys
}

/// Round `value` up to the next multiple of [`PAGE_SIZE`].
#[inline]
#[must_use]
pub const fn page_align_up(value: u64) -> u64 {
    (value + PAGE_SIZE - 1) & !(PAGE_SIZE - 1)
}

/// Round `value` down to a multiple of [`PAGE_SIZE`].
#[inline]
#[must_use]
pub const fn page_align_down(value: u64) -> u64 {
    value & !(PAGE_SIZE - 1)
}
