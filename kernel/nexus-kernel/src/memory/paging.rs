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
pub(super) const ADDRESS_MASK: u64 = 0x000F_FFFF_FFFF_F000;

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
pub(super) unsafe fn read_table_entry(table: u64, index: usize) -> u64 {
    // SAFETY: upheld by the caller.
    unsafe { read_entry(table, index) }
}

/// Write one entry of the page table at physical address `table`.
///
/// # Safety
///
/// See [`read_table_entry`], and the value must be a well-formed entry for the
/// level `table` sits at.
pub(super) unsafe fn write_table_entry(table: u64, index: usize, value: u64) {
    // SAFETY: upheld by the caller.
    unsafe { write_entry(table, index, value) };
}

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

/// Invalidate the TLB entry for one page **on this processor only**.
///
/// Almost never what a caller wants directly. A mapping change has to reach
/// every processor, which is [`crate::arch::tlb::shoot_down`]'s job; this is
/// the piece it is built from, and is correct on its own only where no other
/// processor can hold the translation.
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
/// `user` marks every table on the path reachable from ring 3, and must be set
/// exactly when the leaf being installed is a user mapping. The processor takes
/// the effective permission as the AND across all four levels, so a user leaf
/// under a kernel-only table is simply unreachable — which faults on the first
/// instruction of the first user program and looks nothing like the missing bit
/// that it is.
///
/// Kernel mappings leave it clear, so a kernel table can never be reached from
/// ring 3 even if a leaf below it is later marked user by mistake. Nothing is
/// lost by being permissive at the intermediate levels of the *user* half:
/// every leaf there is a user leaf, and a leaf without the bit is still
/// unreachable whatever the tables above it say.
///
/// # Safety
///
/// `root` must be a live root page table reachable through the direct map.
unsafe fn walk_to_page_table(
    root: u64,
    virt: u64,
    create: bool,
    user: bool,
) -> Result<u64, MapError> {
    let mut table = root;

    for level in 0..3 {
        let index = index_for(virt, level);
        // SAFETY: `table` is a live table, maintained by this loop.
        let entry = unsafe { read_entry(table, index) };

        if entry & PRESENT != 0 {
            if entry & HUGE != 0 {
                return Err(MapError::CoveredByLargePage);
            }
            // A table created for an earlier kernel mapping and now on the path
            // to a user one has to gain the bit; the first user page under a
            // given table is where that happens.
            if user && entry & USER == 0 {
                // SAFETY: `table` is a live table and `index` is in range.
                unsafe { write_entry(table, index, entry | USER) };
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
            let extra = if user { USER } else { 0 };
            write_entry(table, index, frame | PRESENT | WRITABLE | extra);
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
    // SAFETY: upheld by the caller.
    unsafe { map_page_in(active_root(), virt, phys, flags) }
}

/// Map `virt` in the address space rooted at `root`.
///
/// The shootdown is conditional on `root` being the one this processor is
/// running in. A space that is not active anywhere has no cached translations
/// to invalidate, and interrupting every processor to tell it about an address
/// space it has never loaded is pure cost. Comparing against `cr3` here rather
/// than making the caller decide means the correct answer is the default one.
///
/// # Safety
///
/// See [`map_page`], and `root` must be a live root page table.
pub unsafe fn map_page_in(root: u64, virt: u64, phys: u64, flags: u64) -> Result<(), MapError> {
    // SAFETY: `root` is a live root table; the direct map covers it.
    let table = unsafe { walk_to_page_table(root, virt, true, flags & USER != 0)? };
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
    if root == active_root() {
        // SAFETY: the entry already holds the new mapping.
        unsafe { crate::arch::tlb::shoot_down(virt, 1) };
    }
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
    let table = unsafe { walk_to_page_table(root, virt, false, false)? };
    let index = index_for(virt, 3);

    // SAFETY: `table` is a live page table.
    let entry = unsafe { read_entry(table, index) };
    if entry & PRESENT == 0 {
        return Err(MapError::NotMapped);
    }
    // SAFETY: as above.
    unsafe { write_entry(table, index, 0) };

    // Every processor, not just this one. The frame is about to go back to the
    // allocator and be handed to something else, and a core still holding the
    // translation would read or write it with no fault to say so.
    //
    // SAFETY: the entry is already cleared, so nothing can re-cache it.
    unsafe { crate::arch::tlb::shoot_down(virt, 1) };

    Ok(entry & ADDRESS_MASK)
}

/// Point an already-mapped page at a different frame.
///
/// Distinct from unmapping and mapping again, and not merely as a convenience:
/// between the two the address is absent, and anything touching it in that
/// window takes a page fault. Rewriting the entry in place leaves no window.
///
/// # Safety
///
/// See [`map_page`]. The previous frame becomes the caller's to dispose of.
pub unsafe fn remap_page(virt: u64, phys: u64, flags: u64) -> Result<u64, MapError> {
    let root = active_root();
    // SAFETY: `root` is the live root table; the direct map covers it.
    let table = unsafe { walk_to_page_table(root, virt, false, flags & USER != 0)? };
    let index = index_for(virt, 3);

    // SAFETY: `table` is a live page table.
    let previous = unsafe { read_entry(table, index) };
    if previous & PRESENT == 0 {
        return Err(MapError::NotMapped);
    }
    // SAFETY: as above.
    unsafe { write_entry(table, index, (phys & ADDRESS_MASK) | flags | PRESENT) };

    // The old translation is now wrong everywhere, not merely here.
    //
    // SAFETY: the entry already holds the new mapping, so a processor that
    // re-walks during the shootdown caches the new translation, not the old.
    unsafe { crate::arch::tlb::shoot_down(virt, 1) };

    Ok(previous & ADDRESS_MASK)
}

/// Unmap `pages` consecutive pages, with a single shootdown for the range.
///
/// Unmapping a sixteen-page stack one page at a time means sixteen rounds of
/// interrupting every other processor and waiting for it. The mapping changes
/// are independent, so they can all be made first and announced once.
///
/// # Safety
///
/// See [`unmap_page`]. Returns the number of pages that were mapped and are
/// now not; an already-absent page is skipped rather than being an error.
pub unsafe fn unmap_range(virt: u64, pages: u64) -> u64 {
    let mut removed = 0;

    for page in 0..pages {
        let address = virt + page * 4096;
        let root = active_root();
        // SAFETY: `root` is the live root table.
        let Ok(table) = (unsafe { walk_to_page_table(root, address, false, false) }) else {
            continue;
        };
        let index = index_for(address, 3);

        // SAFETY: `table` is a live page table.
        unsafe {
            if read_entry(table, index) & PRESENT == 0 {
                continue;
            }
            write_entry(table, index, 0);
        }
        removed += 1;
    }

    // SAFETY: every entry in the range is cleared, so nothing can re-cache one.
    unsafe { crate::arch::tlb::shoot_down(virt, pages) };
    removed
}

/// Look up the physical address `virt` translates to, if any.
///
/// Follows large pages as well as 4 KiB entries, so it reports the truth about
/// the direct map rather than failing on it.
#[must_use]
pub fn translate(virt: u64) -> Option<u64> {
    translate_in(active_root(), virt)
}

/// Look up `virt` in the address space rooted at `root`.
#[must_use]
pub fn translate_in(root: u64, virt: u64) -> Option<u64> {
    let mut table = root;

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
        // Clearing a top-level entry invalidates an enormous range, so discard
        // everything rather than walking it a page at a time. Broadcast even
        // though this runs before the other processors start: it costs nothing
        // when this is the only one, and it stops the correctness of this call
        // from depending on where it sits in the bring-up order.
        crate::arch::tlb::shoot_down_all();
    }
}
