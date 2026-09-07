//! The kernel heap and its global allocator.
//!
//! Turning this on is what makes `alloc` usable inside the kernel: `Box`,
//! `Vec`, `BTreeMap` and `String` all become available, which every subsystem
//! above memory management needs.
//!
//! The heap is backed by frames from the physical allocator, mapped into the
//! kernel's own window at [`layout::KERNEL_HEAP_BASE`] rather than used through
//! the direct map. That costs a little page-table memory and buys two things:
//! the heap is contiguous in virtual address space no matter how fragmented
//! physical memory is, and it can grow without moving.

use core::alloc::{GlobalAlloc, Layout};
use core::ptr;

use nexus_abi::layout;
use nexus_mm::heap::Heap;
use nexus_mm::PAGE_SIZE;

use super::paging;
use crate::kprintln;
use crate::sync::IrqSpinLock;

/// Frames per physical allocation when growing the heap.
///
/// Order 9 is 2 MiB contiguous. Asking for large blocks keeps the number of
/// physical allocations down and leaves the buddy allocator's small blocks for
/// callers that actually need single frames.
const GROWTH_ORDER: usize = 9;
const GROWTH_BYTES: u64 = PAGE_SIZE << GROWTH_ORDER;

/// The kernel heap.
static HEAP: IrqSpinLock<Heap> = IrqSpinLock::new(Heap::new());

/// Virtual address just past the mapped end of the heap, so growth continues
/// where the last extension stopped.
static HEAP_END: IrqSpinLock<u64> = IrqSpinLock::new(layout::KERNEL_HEAP_BASE);

/// Why the heap could not be brought up or extended.
#[derive(Debug, Clone, Copy)]
pub enum HeapError {
    /// The physical allocator had no block of the size the heap grows by.
    OutOfFrames,
    /// The heap's virtual range could not be mapped.
    MapFailed(paging::MapError),
    /// Growing further would run past the region reserved for the heap.
    RegionExhausted,
}

impl core::fmt::Display for HeapError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::OutOfFrames => f.write_str("no physical memory available for the heap"),
            Self::MapFailed(error) => write!(f, "could not map heap pages: {error}"),
            Self::RegionExhausted => f.write_str("the kernel heap region is full"),
        }
    }
}

/// Size of the virtual window reserved for the heap.
///
/// The heap grows inside this window; exceeding it is a hard error rather than
/// something to paper over, because running into the next region of the address
/// space would corrupt whatever lives there.
const HEAP_REGION_SIZE: u64 = 1024 * 1024 * 1024;

/// Map another [`GROWTH_BYTES`] onto the end of the heap and give it to the
/// allocator.
///
/// # Safety
///
/// The heap's virtual window must belong to the heap alone.
unsafe fn grow(bytes: u64) -> Result<u64, HeapError> {
    let mut end = HEAP_END.lock();
    let mut added = 0u64;

    while added < bytes {
        if *end + GROWTH_BYTES > layout::KERNEL_HEAP_BASE + HEAP_REGION_SIZE {
            return Err(HeapError::RegionExhausted);
        }

        let frames = super::allocate_block(GROWTH_ORDER).ok_or(HeapError::OutOfFrames)?;

        // Heap memory is writable and never executed, so it is mapped
        // non-executable; and it is global because the kernel half of the
        // address space is identical in every process.
        // SAFETY: `frames` is a block this heap now owns, and `*end` is inside
        // the heap's own window, which nothing else maps.
        let result = unsafe {
            paging::map_range(
                *end,
                frames,
                GROWTH_BYTES,
                paging::WRITABLE | paging::NO_EXECUTE | paging::GLOBAL,
            )
        };
        if let Err(error) = result {
            // SAFETY: the block was allocated just above and never handed out.
            unsafe { super::free_block(frames, GROWTH_ORDER) };
            return Err(HeapError::MapFailed(error));
        }

        // SAFETY: the range was just mapped writable and belongs to the heap.
        unsafe { HEAP.lock().add_region(*end as usize, GROWTH_BYTES as usize) };

        *end += GROWTH_BYTES;
        added += GROWTH_BYTES;
    }

    Ok(added)
}

/// Bring up the kernel heap.
///
/// # Safety
///
/// Call once, after the physical allocator is running and before anything
/// allocates.
pub unsafe fn init() -> Result<(), HeapError> {
    // SAFETY: the heap window is reserved for the heap alone.
    let added = unsafe { grow(layout::KERNEL_HEAP_INITIAL_SIZE)? };

    let stats = HEAP.lock().stats();
    kprintln!(
        "[heap] {} KiB at {:#018x}, {} KiB free in {} block",
        added / 1024,
        layout::KERNEL_HEAP_BASE,
        stats.free() / 1024,
        stats.free_blocks
    );

    Ok(())
}

/// A snapshot of heap occupancy.
#[must_use]
pub fn stats() -> nexus_mm::HeapStats {
    HEAP.lock().stats()
}

/// The kernel's global allocator.
struct KernelAllocator;

// SAFETY: every allocation comes from a heap region that was mapped writable
// and is owned solely by this allocator, and the lock serialises access.
unsafe impl GlobalAlloc for KernelAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let mut heap = HEAP.lock();
        match heap.allocate(layout) {
            Some(pointer) => pointer.as_ptr(),
            None => {
                // Growing needs the heap lock, which this thread holds, so the
                // lock is released first. A concurrent allocation may take the
                // new space before this one retries, in which case the retry
                // simply grows again.
                drop(heap);
                // SAFETY: the heap window is reserved for the heap alone.
                if unsafe { grow(GROWTH_BYTES) }.is_err() {
                    return ptr::null_mut();
                }
                match HEAP.lock().allocate(layout) {
                    Some(pointer) => pointer.as_ptr(),
                    None => ptr::null_mut(),
                }
            }
        }
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        if let Some(pointer) = ptr::NonNull::new(pointer) {
            // SAFETY: the caller guarantees this came from `alloc` with the
            // same layout, which is exactly what `Heap::deallocate` requires.
            unsafe { HEAP.lock().deallocate(pointer, layout) };
        }
    }
}

#[global_allocator]
static ALLOCATOR: KernelAllocator = KernelAllocator;
