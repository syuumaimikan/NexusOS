//! Address spaces.
//!
//! Until now there was one set of page tables and ring 3 was kept out of the
//! kernel's memory by the user bit alone. That is a privilege boundary, not an
//! isolation boundary: two user programs sharing one set of tables can read
//! each other completely. An address space per process is what makes the word
//! "process" mean something, and it is what this module adds.
//!
//! # The upper half is shared, and it is shared by pointer
//!
//! Every address space maps the same kernel: the direct map, the image, the
//! heap, the kernel stacks. Copying those mappings into each new space would be
//! both wasteful and wrong — a kernel mapping made afterwards would exist in
//! some spaces and not others.
//!
//! So the top-level entries for the upper half are copied, and everything below
//! them is shared. A copied entry names a page-directory-pointer table; the
//! kernel's later mappings are made *inside* those tables, which every space
//! already points at, so they appear everywhere at once.
//!
//! That only holds while the top-level entries themselves never change, which
//! is why [`pin_kernel_upper_half`] fills in all 256 of them up front. A
//! megabyte of empty tables, in exchange for the entire class of bug where a
//! kernel mapping made after a process started is invisible inside it.
//!
//! # What a space owns
//!
//! Its lower half, and nothing else: the tables that describe user memory, and
//! the frames those tables map. Dropping it frees exactly that, and touches
//! nothing above the halfway line no matter what the tables there say.

use core::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use super::paging;
use super::{allocate_frame, free_frame};
use crate::kprintln;
use nexus_abi::layout;

/// Entries in a page table at any level.
const ENTRIES: usize = 512;

/// First top-level entry belonging to the kernel.
///
/// The canonical-address split: entries 0 to 255 cover the lower half, which is
/// user space, and 256 to 511 the upper half, which is the kernel.
const KERNEL_FIRST_ENTRY: usize = 256;

/// The kernel's own root table, captured once.
///
/// Every space is built from it, and every space falls back to it when no user
/// process is running.
static KERNEL_ROOT: AtomicU64 = AtomicU64::new(0);

/// Address spaces created and destroyed, for diagnostics.
static CREATED: AtomicUsize = AtomicUsize::new(0);
static DESTROYED: AtomicUsize = AtomicUsize::new(0);

/// Physical address of the kernel's root page table.
#[must_use]
pub fn kernel_root() -> u64 {
    KERNEL_ROOT.load(Ordering::Relaxed)
}

/// Adopt the running page tables as the kernel's, and make their upper half
/// permanent.
///
/// # Safety
///
/// Call once, on the boot processor, with the frame allocator running and
/// before any address space is created.
pub unsafe fn init() -> Result<(), paging::MapError> {
    let root = paging::active_root();
    KERNEL_ROOT.store(root, Ordering::Relaxed);

    // SAFETY: `root` is the live root table.
    let filled = unsafe { pin_kernel_upper_half(root)? };
    kprintln!("[mem ] kernel address space rooted at {root:#x}, {filled} top-level entries added");
    Ok(())
}

/// Make sure every upper-half top-level entry is present.
///
/// The entries are what a new address space copies. Copying is only sound if
/// they never change afterwards, and the way to guarantee that is to create
/// them all now, while there is exactly one address space to create them in.
///
/// # Safety
///
/// `root` must be the live root page table.
unsafe fn pin_kernel_upper_half(root: u64) -> Result<usize, paging::MapError> {
    let mut added = 0;

    for index in KERNEL_FIRST_ENTRY..ENTRIES {
        // SAFETY: `root` is a live table and `index` is in range.
        let entry = unsafe { paging::read_table_entry(root, index) };
        if entry & paging::PRESENT != 0 {
            continue;
        }

        let frame = allocate_frame().ok_or(paging::MapError::OutOfMemory)?;
        // SAFETY: the frame was just allocated and is reachable through the
        // direct map. A page table must start zeroed.
        unsafe {
            core::ptr::write_bytes(layout::phys_to_virt(frame) as *mut u8, 0, 4096);
            // No user bit: nothing in the upper half is ever reachable from
            // ring 3, and an empty table that says otherwise would be one
            // mistake away from being reachable.
            paging::write_table_entry(root, index, frame | paging::PRESENT | paging::WRITABLE);
        }
        added += 1;
    }

    Ok(added)
}

/// Why an address space could not be created.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpaceError {
    /// No frame for the root table.
    OutOfMemory,
    /// [`init`] has not run.
    NoKernelSpace,
}

impl core::fmt::Display for SpaceError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::OutOfMemory => f.write_str("out of memory for an address space"),
            Self::NoKernelSpace => f.write_str("the kernel address space was never adopted"),
        }
    }
}

/// A set of page tables: one kernel, shared, and one user half of its own.
pub struct AddressSpace {
    root: u64,
}

impl AddressSpace {
    /// Create a space that maps the kernel and nothing else.
    pub fn new() -> Result<Self, SpaceError> {
        let kernel = kernel_root();
        if kernel == 0 {
            return Err(SpaceError::NoKernelSpace);
        }

        let root = allocate_frame().ok_or(SpaceError::OutOfMemory)?;

        // SAFETY: the frame was just allocated, so nothing else refers to it,
        // and both tables are reachable through the direct map.
        unsafe {
            core::ptr::write_bytes(layout::phys_to_virt(root) as *mut u8, 0, 4096);
            for index in KERNEL_FIRST_ENTRY..ENTRIES {
                let entry = paging::read_table_entry(kernel, index);
                paging::write_table_entry(root, index, entry);
            }
        }

        CREATED.fetch_add(1, Ordering::Relaxed);
        Ok(Self { root })
    }

    /// Physical address of this space's root table.
    #[must_use]
    pub fn root(&self) -> u64 {
        self.root
    }

    /// Map `virt` to `phys` in this space.
    ///
    /// # Safety
    ///
    /// `phys` must be a frame this space may own, and `virt` must be in the
    /// user half — a mapping above the halfway line would be made in a table
    /// every other space shares, and freed when this one is dropped.
    pub unsafe fn map(&self, virt: u64, phys: u64, flags: u64) -> Result<(), paging::MapError> {
        debug_assert!(virt < layout::USER_SPACE_END);
        // SAFETY: upheld by the caller; `self.root` is live for as long as
        // `self` is.
        unsafe { paging::map_page_in(self.root, virt, phys, flags) }
    }

    /// What `virt` translates to in this space, if anything.
    #[must_use]
    pub fn translate(&self, virt: u64) -> Option<u64> {
        paging::translate_in(self.root, virt)
    }
}

impl Drop for AddressSpace {
    fn drop(&mut self) {
        // SAFETY: an `AddressSpace` is dropped only once nothing refers to it,
        // which for a running space means no processor has it in `cr3` -- the
        // scheduler moves a processor to the kernel root before the last
        // reference can go, because a thread has to be switched away from
        // before it can be reaped.
        // Skipped in the shared-page injection build, where two spaces map one
        // frame on purpose and freeing what each maps would free it twice. That
        // build exists to make a test fail, not to be correct.
        #[cfg(not(feature = "inject-shared-user-page"))]
        unsafe {
            free_user_half(self.root)
        };
        // SAFETY: the root table is now unreferenced.
        unsafe { free_frame(self.root) };
        DESTROYED.fetch_add(1, Ordering::Relaxed);
    }
}

/// Free the lower half of the space rooted at `root`: its tables and the frames
/// they map.
///
/// Stops at [`KERNEL_FIRST_ENTRY`], which is what keeps a process teardown from
/// walking into tables every other space is still using.
///
/// # Safety
///
/// No processor may be running in this space, and nothing may still refer to
/// the frames it maps.
#[cfg_attr(feature = "inject-shared-user-page", allow(dead_code))]
unsafe fn free_user_half(root: u64) {
    for index in 0..KERNEL_FIRST_ENTRY {
        // SAFETY: `root` is a live table and `index` is in range.
        let entry = unsafe { paging::read_table_entry(root, index) };
        if entry & paging::PRESENT == 0 {
            continue;
        }
        // SAFETY: the entry names a live page-directory-pointer table.
        unsafe { free_table(entry & paging::ADDRESS_MASK, 1) };
    }
}

/// Free the table at `table`, which sits at `level`, and everything under it.
///
/// # Safety
///
/// As [`free_user_half`].
#[cfg_attr(feature = "inject-shared-user-page", allow(dead_code))]
unsafe fn free_table(table: u64, level: usize) {
    for index in 0..ENTRIES {
        // SAFETY: `table` is a live table and `index` is in range.
        let entry = unsafe { paging::read_table_entry(table, index) };
        if entry & paging::PRESENT == 0 {
            continue;
        }

        let frame = entry & paging::ADDRESS_MASK;
        if level == 3 || entry & paging::HUGE != 0 {
            // A leaf. The frame is this space's to free unless it was marked as
            // belonging to something else -- a shared memory object maps the
            // same frame into every space that holds a handle to it, and the
            // second of those to be dropped would otherwise free a frame the
            // first had already returned.
            if entry & paging::SHARED == 0 {
                // SAFETY: nothing refers to it any more.
                unsafe { free_frame(frame) };
            }
        } else {
            // SAFETY: another level of tables, owned by this space alone.
            unsafe { free_table(frame, level + 1) };
        }
    }

    // SAFETY: every entry has been dealt with, so the table itself is free.
    unsafe { free_frame(table) };
}

/// Load `root` into `cr3`, if it is not already there.
///
/// The check is not an optimisation to skip: writing `cr3` discards every
/// non-global translation, so a redundant write on every context switch would
/// throw away the working set of a thread that never left its address space.
///
/// # Safety
///
/// `root` must be a live root page table mapping the executing code and stack.
pub unsafe fn activate_root(root: u64) {
    if paging::active_root() == root {
        return;
    }
    // SAFETY: upheld by the caller.
    unsafe {
        core::arch::asm!("mov cr3, {}", in(reg) root, options(nostack, preserves_flags));
    }
}

/// Address spaces created and destroyed since boot.
#[must_use]
pub fn statistics() -> (usize, usize) {
    (
        CREATED.load(Ordering::Relaxed),
        DESTROYED.load(Ordering::Relaxed),
    )
}
