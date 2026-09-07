//! Kernel page-table manipulation.
//!
//! The kernel inherits the tables the bootloader built and edits them in place
//! rather than rebuilding: they already contain the direct map, the kernel image
//! with per-segment permissions, and the boot stack, all of which the kernel
//! would only have to reconstruct identically.
//!
//! Table pages come from the physical frame allocator, and every table is
//! reached through the direct map, so no recursive mapping or temporary window
//! is needed.

use nexus_abi::layout;

use super::allocate_frame;
use crate::arch;

/// The mapping is valid.
pub const PRESENT: u64 = 1 << 0;
/// Writes are permitted.
pub const WRITABLE: u64 = 1 << 1;
/// Reachable from CPL 3. Used once user address spaces exist.
#[allow(dead_code)]
pub const USER: u64 = 1 << 2;
/// Caching disabled; required for memory-mapped device registers. Used once
/// the local APIC and PCI devices are mapped.
#[allow(dead_code)]
pub const NO_CACHE: u64 = 1 << 4;
/// Maps a large page directly, at the page-directory or PDPT level.
pub const HUGE: u64 = 1 << 7;
/// The translation survives a `cr3` reload.
pub const GLOBAL: u64 = 1 << 8;
/// Instruction fetches through this mapping fault.
pub const NO_EXECUTE: u64 = 1 << 63;

/// Bits of an entry holding the physical frame address.
const ADDRESS_MASK: u64 = 0x000F_FFFF_FFFF_F000;

/// Why a mapping operation failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MapError {
    /// No physical frame was available for a new page table.
    OutOfMemory,
    /// A large page already covers this address, so a 4 KiB entry cannot be
    /// placed here without splitting it first.
    CoveredByLargePage,
    /// The address is already mapped.
    ///
    /// Reported rather than silently overwritten: replacing a live mapping is
    /// how a page ends up owned by two subsystems at once.
    AlreadyMapped,
    /// The address is not mapped, so it cannot be unmapped or changed.
    NotMapped,
}

impl core::fmt::Display for MapError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(match self {
            Self::OutOfMemory => "out of physical memory for a page table",
            Self::CoveredByLargePage => "a large page already covers this address",
            Self::AlreadyMapped => "the address is already mapped",
            Self::NotMapped => "the address is not mapped",
        })
    }
}

/// Read entry `index` of the table at physical address `table`.
///
/// # Safety
///
/// `table` must be a live page table reachable through the direct map.
unsafe fn read_entry(table: u64, index: usize) -> u64 {
    let pointer = layout::phys_to_virt(table) as *const u64;
    // SAFETY: upheld by the caller; `index` is masked to 0..512 by callers.
    unsafe { pointer.add(index).read_volatile() }
}

/// Write entry `index` of the table at physical address `table`.
///
/// # Safety
///
/// See [`read_entry`]. The caller is responsible for flushing any stale
/// translation this invalidates.
unsafe fn write_entry(table: u64, index: usize, value: u64) {
    let pointer = layout::phys_to_virt(table) as *mut u64;
    // SAFETY: upheld by the caller.
    unsafe { pointer.add(index).write_volatile(value) }
}

/// Invalidate the TLB entry for one page.
///
/// Single-processor only: once other cores are running, a mapping change also
/// needs a shootdown, which arrives with SMP.
#[inline]
pub fn flush(virt: u64) {
    // SAFETY: `invlpg` only discards a cached translation; it can never make
    // the address space less correct.
    unsafe {
        core::arch::asm!("invlpg [{}]", in(reg) virt, options(nostack, preserves_flags));
    }
}

/// Reload `cr3`, flushing every non-global translation.
///
/// # Safety
///
/// The current `cr3` must still describe a valid address space covering the
/// executing code and stack.
pub unsafe fn flush_all() {
    // SAFETY: reading `cr3` and writing it back is a no-op apart from the
    // flush it causes.
    unsafe {
        let root: u64;
        core::arch::asm!("mov {}, cr3", out(reg) root, options(nomem, nostack, preserves_flags));
        core::arch::asm!("mov cr3, {}", in(reg) root, options(nostack, preserves_flags));
    }
}

/// Physical address of the active root page table.
#[must_use]
pub fn active_root() -> u64 {
    arch::read_cr3() & ADDRESS_MASK
}

/// The four levels of the walk, from PML4 down to the page table.
const LEVEL_SHIFTS: [u32; 4] = [39, 30, 21, 12];

/// Index into the table at `level` for `virt`.
const fn index_for(virt: u64, level: usize) -> usize {
    ((virt >> LEVEL_SHIFTS[level]) & 0x1FF) as usize
}

/// Walk to the page table containing `virt`, creating tables as needed.
///
/// # Safety
///
/// `root` must be a live root page table reachable through the direct map.
unsafe fn walk_to_page_table(root: u64, virt: u64, create: bool) -> Result<u64, MapError> {
    let mut table = root;

    for level in 0..3 {
        let index = index_for(virt, level);
        // SAFETY: `table` is a live table, maintained by this loop.
        let entry = unsafe { read_entry(table, index) };

        if entry & PRESENT != 0 {
            if entry & HUGE != 0 {
                return Err(MapError::CoveredByLargePage);
            }
            table = entry & ADDRESS_MASK;
            continue;
        }

        if !create {
            return Err(MapError::NotMapped);
        }

        let frame = allocate_frame().ok_or(MapError::OutOfMemory)?;
        // SAFETY: the frame belongs to us and is reachable through the direct
        // map. A page table must start zeroed or its entries are garbage.
        unsafe {
            core::ptr::write_bytes(layout::phys_to_virt(frame) as *mut u8, 0, 4096);
            // Intermediate entries are permissive in write and restrictive in
            // user access: the effective permission is the AND across levels,
            // so the leaf decides what is writable, but leaving USER clear here
            // means a kernel table can never be reached from ring 3 even if a
            // leaf below it is later marked USER by mistake.
            write_entry(table, index, frame | PRESENT | WRITABLE);
        }
        table = frame;
    }

    Ok(table)
}

/// Map `virt` to `phys` with `flags`.
///
/// # Safety
///
/// `phys` must be a frame the caller owns, and `virt` must be an address the
/// caller is entitled to define. Creating a second mapping for a frame that is
/// already mapped elsewhere aliases it, which is the caller's responsibility.
pub unsafe fn map_page(virt: u64, phys: u64, flags: u64) -> Result<(), MapError> {
    let root = active_root();
    // SAFETY: `root` is the live root table; the direct map covers it.
    let table = unsafe { walk_to_page_table(root, virt, true)? };
    let index = index_for(virt, 3);

    // SAFETY: `table` is a live page table.
    unsafe {
        if read_entry(table, index) & PRESENT != 0 {
            return Err(MapError::AlreadyMapped);
        }
        write_entry(table, index, (phys & ADDRESS_MASK) | flags | PRESENT);
    }

    // A previously absent translation can still be cached as such on some
    // processors, so the flush is not optional.
    flush(virt);
    Ok(())
}

/// Map `size` bytes of contiguous physical memory.
///
/// On failure, any pages already mapped are unmapped again, so a partial
/// mapping is never left behind for a caller to trip over.
///
/// # Safety
///
/// See [`map_page`].
pub unsafe fn map_range(virt: u64, phys: u64, size: u64, flags: u64) -> Result<(), MapError> {
    let pages = size.div_ceil(4096);

    for page in 0..pages {
        let offset = page * 4096;
        // SAFETY: upheld by the caller.
        if let Err(error) = unsafe { map_page(virt + offset, phys + offset, flags) } {
            // Unwind so the caller sees all-or-nothing.
            for undo in 0..page {
                // SAFETY: these were mapped by this loop and by nothing else.
                unsafe {
                    let _ = unmap_page(virt + undo * 4096);
                }
            }
            return Err(error);
        }
    }
    Ok(())
}

/// Remove the mapping for `virt`, returning the physical frame it referred to.
///
/// The frame is *not* freed: the caller decides what happens to it, because
/// only the caller knows whether it is ordinary memory or a device register.
///
/// # Safety
///
/// Nothing may access `virt` afterwards. Unmapping memory that is still in use
/// turns every later access into a page fault.
pub unsafe fn unmap_page(virt: u64) -> Result<u64, MapError> {
    let root = active_root();
    // SAFETY: `root` is the live root table.
    let table = unsafe { walk_to_page_table(root, virt, false)? };
    let index = index_for(virt, 3);

    // SAFETY: `table` is a live page table.
    let entry = unsafe { read_entry(table, index) };
    if entry & PRESENT == 0 {
        return Err(MapError::NotMapped);
    }
    // SAFETY: as above.
    unsafe { write_entry(table, index, 0) };
    flush(virt);

    Ok(entry & ADDRESS_MASK)
}

/// Look up the physical address `virt` translates to, if any.
///
/// Follows large pages as well as 4 KiB entries, so it reports the truth about
/// the direct map rather than failing on it.
#[must_use]
pub fn translate(virt: u64) -> Option<u64> {
    let mut table = active_root();

    for level in 0..4 {
        let index = index_for(virt, level);
        // SAFETY: the walk only follows present, non-huge entries, each of
        // which points at a live table reachable through the direct map.
        let entry = unsafe { read_entry(table, index) };
        if entry & PRESENT == 0 {
            return None;
        }

        if level == 3 {
            return Some((entry & ADDRESS_MASK) | (virt & 0xFFF));
        }
        if entry & HUGE != 0 {
            // A large page at this level; the offset is everything below it.
            let page_mask = (1u64 << LEVEL_SHIFTS[level]) - 1;
            return Some((entry & ADDRESS_MASK & !page_mask) | (virt & page_mask));
        }
        table = entry & ADDRESS_MASK;
    }

    None
}

/// Tear down the bootloader's identity map.
///
/// The identity map exists only so the loader's own code stays addressable
/// across the `cr3` load that installs these tables. Once the kernel is running
/// at its higher-half address on its own stack, it is pure liability: it makes
/// a null or small-integer pointer dereference succeed silently instead of
/// faulting, and it leaves all of physical memory reachable from user-space
/// addresses.
///
/// # Safety
///
/// Nothing may still be using a low virtual address. In practice that means
/// this runs after the kernel has stopped touching the handoff block through
/// its raw physical pointer.
pub unsafe fn tear_down_identity_map() {
    let root = active_root();
    // The identity map occupies PML4 entry 0, covering the first 512 GiB.
    // SAFETY: `root` is the live root table; entry 0 is the identity map and
    // the caller guarantees nothing depends on it.
    unsafe {
        write_entry(root, 0, 0);
        // Clearing a top-level entry invalidates an enormous range, so flush
        // the whole non-global TLB rather than one page at a time.
        flush_all();
    }
}
