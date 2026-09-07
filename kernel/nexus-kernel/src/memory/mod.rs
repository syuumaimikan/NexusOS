//! Physical memory management.
//!
//! Wires the [`nexus_mm`] buddy allocator to real hardware: the allocator's
//! logic is generic and tested off-target, and everything machine-specific —
//! reaching physical memory through the direct map, and finding somewhere to
//! put the allocator's own bitmap before an allocator exists — lives here.

pub mod heap;
pub mod paging;

use nexus_abi::{layout, BootInfo, MemoryKind, MemoryRegion};
use nexus_mm::buddy::{BlockLinks, BuddyAllocator, Links};
use nexus_mm::PAGE_SIZE;

use crate::kprintln;
use crate::sync::IrqSpinLock;

/// Reaches a free block's links through the direct physical map.
pub struct DirectMapLinks;

// SAFETY: the direct map covers every frame the allocator is given, is writable,
// and is a linear one-to-one window, so each physical address has its own stable
// sixteen bytes and a read returns the last write.
unsafe impl BlockLinks for DirectMapLinks {
    unsafe fn read_links(&self, phys: u64) -> Links {
        let pointer = layout::phys_to_virt(phys) as *const u64;
        // SAFETY: the allocator only asks about blocks it owns, which are
        // mapped writable through the direct map.
        unsafe {
            Links {
                next: pointer.read(),
                prev: pointer.add(1).read(),
            }
        }
    }

    unsafe fn write_links(&mut self, phys: u64, links: Links) {
        let pointer = layout::phys_to_virt(phys) as *mut u64;
        // SAFETY: as above. The block is free, so nothing else is reading the
        // bytes being overwritten.
        unsafe {
            pointer.write(links.next);
            pointer.add(1).write(links.prev);
        }
    }
}

/// The kernel's physical frame allocator.
///
/// `None` until [`init`] runs. Guarded by an interrupt-masking lock because
/// interrupt handlers will allocate once there are drivers.
static FRAME_ALLOCATOR: IrqSpinLock<Option<BuddyAllocator<'static, DirectMapLinks>>> =
    IrqSpinLock::new(None);

/// Why physical memory could not be brought up.
#[derive(Debug, Clone, Copy)]
pub enum InitError {
    /// No usable region was large enough to hold the allocator's bitmap.
    NoRoomForBitmap { needed_bytes: usize },
    /// The bitmap was allocated but the allocator rejected it.
    BitmapTooSmall,
    /// The memory map described no usable RAM at all.
    NoUsableMemory,
}

impl core::fmt::Display for InitError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::NoRoomForBitmap { needed_bytes } => write!(
                f,
                "no usable region large enough for a {needed_bytes} byte frame bitmap"
            ),
            Self::BitmapTooSmall => f.write_str("the frame bitmap was sized incorrectly"),
            Self::NoUsableMemory => f.write_str("the memory map describes no usable RAM"),
        }
    }
}

/// Add `[start, end)` to `allocator`, skipping any part covered by
/// `exclusions`.
///
/// `exclusions` must be sorted by start address and must not overlap each
/// other, which is what lets this walk both lists once.
///
/// # Safety
///
/// Everything not excluded must be real, writable RAM that nothing else owns.
unsafe fn add_range_excluding(
    allocator: &mut BuddyAllocator<'static, DirectMapLinks>,
    start: u64,
    end: u64,
    exclusions: &[(u64, u64)],
) {
    let mut cursor = start;

    for &(excluded_start, excluded_end) in exclusions {
        if excluded_end <= cursor || excluded_start >= end {
            continue;
        }
        if excluded_start > cursor {
            // SAFETY: upheld by the caller for the part before the exclusion.
            unsafe { allocator.add_free_range(cursor, excluded_start) };
        }
        cursor = cursor.max(excluded_end);
        if cursor >= end {
            return;
        }
    }

    if cursor < end {
        // SAFETY: upheld by the caller for the remaining tail.
        unsafe { allocator.add_free_range(cursor, end) };
    }
}

/// Bring up the physical frame allocator from the boot memory map.
///
/// # Safety
///
/// Call once, before anything allocates. `boot_info` must be the validated
/// handoff block, and its memory map must accurately describe physical memory:
/// every frame marked usable is one this allocator will hand out.
pub unsafe fn init(boot_info: &BootInfo) -> Result<(), InitError> {
    let regions = memory_regions(boot_info);

    // The allocator covers the whole direct-mapped range so that a physical
    // address maps to a bitmap index by simple arithmetic. Frames outside the
    // usable regions are never added, so they can never be handed out.
    let frame_count = boot_info.phys_map_limit / PAGE_SIZE;
    let bitmap_bytes = BuddyAllocator::<DirectMapLinks>::required_bitmap_bytes(frame_count);

    // Bootstrap: the bitmap has to live somewhere, and there is no allocator
    // yet to ask. Take it from the front of the first usable region big enough,
    // then exclude that span when the regions are handed over.
    let bitmap_phys =
        find_bitmap_home(regions, bitmap_bytes as u64).ok_or(InitError::NoRoomForBitmap {
            needed_bytes: bitmap_bytes,
        })?;

    // SAFETY: `bitmap_phys` is inside a usable region, is page aligned, and has
    // at least `bitmap_bytes` of room; the direct map makes it writable. It is
    // excluded from the ranges given to the allocator below, so nothing else
    // ever receives it.
    let bitmap: &'static mut [u8] = unsafe {
        core::slice::from_raw_parts_mut(layout::phys_to_virt(bitmap_phys) as *mut u8, bitmap_bytes)
    };

    let mut allocator = BuddyAllocator::new(frame_count, bitmap, DirectMapLinks)
        .ok_or(InitError::BitmapTooSmall)?;

    let exclusions = [
        // Frame 0 is never handed out, so that a physical address of zero
        // stays unambiguously invalid and a null-pointer bug in code that
        // works with physical addresses is caught rather than working by luck.
        (0, PAGE_SIZE),
        (
            bitmap_phys,
            bitmap_phys + (bitmap_bytes as u64).div_ceil(PAGE_SIZE) * PAGE_SIZE,
        ),
    ];

    for region in regions {
        if region.kind != MemoryKind::Usable {
            continue;
        }
        // SAFETY: the bootloader classified these as usable, meaning firmware
        // no longer owns them and the kernel image is not in them; the two
        // exclusions cover the only parts the kernel has already claimed.
        unsafe {
            add_range_excluding(&mut allocator, region.start, region.end(), &exclusions);
        }
    }

    if allocator.managed_frames() == 0 {
        return Err(InitError::NoUsableMemory);
    }

    let stats = allocator.stats();
    *FRAME_ALLOCATOR.lock() = Some(allocator);

    kprintln!(
        "[mem ] frame allocator: {} MiB across {} frames",
        stats.free_frames * PAGE_SIZE / (1024 * 1024),
        stats.free_frames
    );
    kprintln!(
        "[mem ] bitmap {} KiB at {:#018x}, covering {} MiB of address space",
        bitmap_bytes / 1024,
        bitmap_phys,
        boot_info.phys_map_limit / (1024 * 1024)
    );

    Ok(())
}

/// Borrow the memory map the bootloader left behind.
fn memory_regions(boot_info: &BootInfo) -> &'static [MemoryRegion] {
    // SAFETY: the bootloader allocated this array outside usable RAM, reported
    // its length, and the direct map makes it readable. It is never written
    // again, so a shared reference for the life of the kernel is sound.
    unsafe {
        core::slice::from_raw_parts(
            layout::phys_to_virt(boot_info.memory_map.regions_phys) as *const MemoryRegion,
            boot_info.memory_map.count as usize,
        )
    }
}

/// Find a page-aligned home for the frame bitmap.
///
/// Prefers the *largest* usable region rather than the first that fits, which
/// keeps the bitmap out of the small low-memory regions that DMA-constrained
/// devices will want later.
fn find_bitmap_home(regions: &[MemoryRegion], bytes: u64) -> Option<u64> {
    let mut best: Option<&MemoryRegion> = None;
    for region in regions {
        if region.kind != MemoryKind::Usable || region.len() < bytes {
            continue;
        }
        // Skip the first page: frame 0 is excluded from the allocator anyway,
        // and keeping the bitmap off it avoids a needless split of the range.
        if region.start == 0 && region.len() < bytes + PAGE_SIZE {
            continue;
        }
        if best.is_none_or(|current| region.len() > current.len()) {
            best = Some(region);
        }
    }

    best.map(|region| {
        let start = region.start.max(PAGE_SIZE);
        start.div_ceil(PAGE_SIZE) * PAGE_SIZE
    })
}

/// Allocate one physical frame.
///
/// Returns the physical address, or `None` when memory is exhausted.
#[must_use]
pub fn allocate_frame() -> Option<u64> {
    let mut guard = FRAME_ALLOCATOR.lock();
    // SAFETY: the allocator owns every frame it hands out, and callers reach
    // frames through the direct map rather than holding references across a
    // free.
    unsafe { guard.as_mut()?.allocate_frame() }
}

/// Allocate `2^order` contiguous frames, aligned to the block size.
#[must_use]
pub fn allocate_block(order: usize) -> Option<u64> {
    let mut guard = FRAME_ALLOCATOR.lock();
    // SAFETY: as above.
    unsafe { guard.as_mut()?.allocate(order) }
}

/// Return a frame obtained from [`allocate_frame`].
///
/// # Safety
///
/// `phys` must have come from [`allocate_frame`] and must not be in use or
/// already freed.
pub unsafe fn free_frame(phys: u64) {
    let mut guard = FRAME_ALLOCATOR.lock();
    if let Some(allocator) = guard.as_mut() {
        // SAFETY: upheld by the caller.
        unsafe { allocator.deallocate_frame(phys) };
    }
}

/// Return a block obtained from [`allocate_block`].
///
/// # Safety
///
/// `phys` and `order` must match a live allocation from [`allocate_block`].
pub unsafe fn free_block(phys: u64, order: usize) {
    let mut guard = FRAME_ALLOCATOR.lock();
    if let Some(allocator) = guard.as_mut() {
        // SAFETY: upheld by the caller.
        unsafe { allocator.deallocate(phys, order) };
    }
}

/// A snapshot of physical memory occupancy, or `None` before [`init`].
#[must_use]
pub fn stats() -> Option<nexus_mm::Stats> {
    FRAME_ALLOCATOR.lock().as_ref().map(|a| a.stats())
}
