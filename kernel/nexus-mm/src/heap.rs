//! A free-list heap allocator.
//!
//! Backs the kernel's `GlobalAlloc`, which is what makes `Box`, `Vec` and the
//! rest of `alloc` usable inside the kernel.
//!
//! # Design
//!
//! Free space is kept on a single list of blocks, sorted by address, with the
//! list nodes stored inside the free blocks themselves. Sorting by address is
//! the point: it makes coalescing a check of the two neighbours a block is
//! being inserted between, so adjacent frees merge immediately and the heap
//! does not degrade into a pile of unusable fragments.
//!
//! # The granule invariant
//!
//! Every block address and every block size is a multiple of [`GRANULE`].
//!
//! This one invariant removes the whole class of bugs where a split leaves a
//! remainder too small to hold a list node. If addresses and sizes are always
//! granule multiples, and requests are rounded up to a granule multiple, then
//! any leftover is either zero or at least one whole granule — big enough to be
//! a block. It also means a deallocation can recompute the exact size that was
//! taken from its `Layout` alone, so nothing has to be stored alongside the
//! allocation and nothing is quietly leaked.
//!
//! Alignment requests larger than a granule preserve it too, since rounding a
//! granule-aligned address up to a larger power of two lands on another granule
//! multiple.

use core::alloc::Layout;
use core::ptr::NonNull;

/// The heap's allocation granule.
///
/// 32 bytes: large enough to hold a [`Node`] with room to spare, and a multiple
/// of the 16-byte alignment the x86-64 ABI wants for anything that might hold a
/// SIMD value.
pub const GRANULE: usize = 32;

/// A free block's header, stored in the first bytes of the block.
#[repr(C)]
struct Node {
    /// Size of this block in bytes, including the header.
    size: usize,
    /// Next free block, at a strictly higher address.
    next: Option<NonNull<Node>>,
}

const _: () = assert!(core::mem::size_of::<Node>() <= GRANULE);

/// Round `value` up to a multiple of `align`, which must be a power of two.
const fn align_up(value: usize, align: usize) -> usize {
    (value + align - 1) & !(align - 1)
}

/// Round `value` down to a multiple of `align`.
const fn align_down(value: usize, align: usize) -> usize {
    value & !(align - 1)
}

/// The number of bytes an allocation of `layout` actually consumes.
///
/// Both the allocating and freeing paths call this, which is what keeps them in
/// agreement without storing a size next to every allocation.
fn block_size(layout: Layout) -> usize {
    align_up(layout.size().max(1), GRANULE)
}

/// The alignment an allocation of `layout` is placed at.
fn block_align(layout: Layout) -> usize {
    layout.align().max(GRANULE)
}

/// Occupancy of a [`Heap`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HeapStats {
    /// Total bytes the heap manages.
    pub total: usize,
    /// Bytes currently handed out, including per-allocation rounding.
    pub used: usize,
    /// Number of free blocks; a proxy for fragmentation.
    pub free_blocks: usize,
    /// Size of the largest single free block.
    pub largest_free_block: usize,
}

impl HeapStats {
    /// Bytes not currently handed out.
    #[must_use]
    pub const fn free(&self) -> usize {
        self.total - self.used
    }
}

/// A free-list heap over regions of memory it is given.
pub struct Heap {
    /// First free block, or `None` when the heap is full.
    head: Option<NonNull<Node>>,
    total: usize,
    used: usize,
}

// SAFETY: `Heap` owns the memory it manages and holds no thread-local state;
// callers serialise access with a lock.
unsafe impl Send for Heap {}

impl Default for Heap {
    fn default() -> Self {
        Self::new()
    }
}

impl Heap {
    /// An empty heap that manages no memory.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            head: None,
            total: 0,
            used: 0,
        }
    }

    /// Give `[start, start + size)` to the heap.
    ///
    /// The range is trimmed inward to granule boundaries; a range too small to
    /// hold one granule after trimming is ignored.
    ///
    /// # Safety
    ///
    /// The range must be writable memory, not overlapping anything else the
    /// heap has been given or anything else in use, and it must stay valid for
    /// as long as the heap does.
    pub unsafe fn add_region(&mut self, start: usize, size: usize) {
        let aligned_start = align_up(start, GRANULE);
        let end = align_down(start.saturating_add(size), GRANULE);
        if end <= aligned_start || end - aligned_start < GRANULE {
            return;
        }

        let region = end - aligned_start;
        self.total += region;
        // SAFETY: the caller guarantees this range is writable and unaliased,
        // and it has just been trimmed to satisfy the granule invariant.
        unsafe { self.insert_free(aligned_start, region) };
    }

    /// Insert a free block, keeping the list address-sorted and merging with
    /// either neighbour it touches.
    ///
    /// # Safety
    ///
    /// `address` and `size` must be granule multiples, the range must be heap
    /// memory that is genuinely free, and it must not already be on the list.
    unsafe fn insert_free(&mut self, address: usize, size: usize) {
        debug_assert!(address.is_multiple_of(GRANULE) && size.is_multiple_of(GRANULE));

        // Find the last block before `address`.
        let mut previous: Option<NonNull<Node>> = None;
        let mut current = self.head;
        while let Some(node) = current {
            if node.as_ptr() as usize > address {
                break;
            }
            previous = Some(node);
            // SAFETY: `node` is a live list node.
            current = unsafe { node.as_ref().next };
        }

        // SAFETY: `address` is writable heap memory the caller has given up.
        let node = unsafe {
            let pointer = address as *mut Node;
            pointer.write(Node {
                size,
                next: current,
            });
            NonNull::new_unchecked(pointer)
        };

        match previous {
            // SAFETY: `previous` is a live list node.
            Some(mut before) => unsafe { before.as_mut().next = Some(node) },
            None => self.head = Some(node),
        }

        // Merge forward first, then backward: merging forward can only make the
        // block bigger, which is exactly what the backward merge then absorbs.
        // SAFETY: every node touched is live and on this list.
        unsafe {
            Self::merge_with_next(node);
            if let Some(before) = previous {
                Self::merge_with_next(before);
            }
        }
    }

    /// Merge `node` with the block after it if they are adjacent.
    ///
    /// # Safety
    ///
    /// `node` must be a live node on the list.
    unsafe fn merge_with_next(mut node: NonNull<Node>) {
        // SAFETY: upheld by the caller.
        unsafe {
            let Some(next) = node.as_ref().next else {
                return;
            };
            let node_end = node.as_ptr() as usize + node.as_ref().size;
            if node_end != next.as_ptr() as usize {
                return;
            }
            let combined = node.as_ref().size + next.as_ref().size;
            let after = next.as_ref().next;
            node.as_mut().size = combined;
            node.as_mut().next = after;
        }
    }

    /// Unlink the block after `previous`, or the head when `previous` is `None`.
    ///
    /// # Safety
    ///
    /// The block being removed must be on the list.
    unsafe fn unlink(&mut self, previous: Option<NonNull<Node>>, node: NonNull<Node>) {
        // SAFETY: upheld by the caller.
        unsafe {
            let next = node.as_ref().next;
            match previous {
                Some(mut before) => before.as_mut().next = next,
                None => self.head = next,
            }
        }
    }

    /// Allocate memory satisfying `layout`.
    ///
    /// Uses first fit: the list is address-sorted, so first fit keeps
    /// allocations clustered at low addresses and leaves the large blocks at
    /// the top intact for large requests.
    ///
    /// Returns `None` when no block can satisfy the request.
    pub fn allocate(&mut self, layout: Layout) -> Option<NonNull<u8>> {
        let size = block_size(layout);
        let align = block_align(layout);

        let mut previous: Option<NonNull<Node>> = None;
        let mut current = self.head;

        while let Some(node) = current {
            let start = node.as_ptr() as usize;
            // SAFETY: `node` is a live list node.
            let block = unsafe { node.as_ref().size };
            let next = unsafe { node.as_ref().next };

            let allocation_start = align_up(start, align);
            // `checked_add` rather than a bare sum: a pathological alignment
            // could otherwise wrap and produce an end inside the block.
            if let Some(allocation_end) = allocation_start.checked_add(size) {
                if allocation_end <= start + block {
                    let front = allocation_start - start;
                    let tail = start + block - allocation_end;

                    // The granule invariant guarantees both remainders are
                    // either zero or a whole granule, so neither can be too
                    // small to become a block.
                    debug_assert!(front.is_multiple_of(GRANULE) && tail.is_multiple_of(GRANULE));

                    // SAFETY: `node` is on the list, and the two remainders are
                    // disjoint from the allocation and from each other.
                    unsafe {
                        self.unlink(previous, node);
                        if front > 0 {
                            self.insert_free(start, front);
                        }
                        if tail > 0 {
                            self.insert_free(allocation_end, tail);
                        }
                    }

                    self.used += size;
                    // SAFETY: `allocation_start` is inside a block the heap
                    // owns and has just removed from its free list.
                    return Some(unsafe { NonNull::new_unchecked(allocation_start as *mut u8) });
                }
            }

            previous = Some(node);
            current = next;
        }

        None
    }

    /// Return an allocation to the heap.
    ///
    /// # Safety
    ///
    /// `pointer` must have come from [`Heap::allocate`] on this heap with an
    /// identical `layout`, and must not have been freed already.
    pub unsafe fn deallocate(&mut self, pointer: NonNull<u8>, layout: Layout) {
        let size = block_size(layout);
        self.used -= size;
        // SAFETY: the caller guarantees this range came from this heap and is
        // no longer in use, and `block_size` reproduces exactly what was taken.
        unsafe { self.insert_free(pointer.as_ptr() as usize, size) };
    }

    /// A snapshot of occupancy.
    #[must_use]
    pub fn stats(&self) -> HeapStats {
        let mut free_blocks = 0;
        let mut largest_free_block = 0;
        let mut current = self.head;
        while let Some(node) = current {
            // SAFETY: `node` is a live list node.
            let (size, next) = unsafe { (node.as_ref().size, node.as_ref().next) };
            free_blocks += 1;
            largest_free_block = largest_free_block.max(size);
            current = next;
        }

        HeapStats {
            total: self.total,
            used: self.used,
            free_blocks,
            largest_free_block,
        }
    }
}
