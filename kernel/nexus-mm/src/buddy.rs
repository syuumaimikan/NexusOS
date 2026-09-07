//! A buddy allocator for physical frames.
//!
//! # Why a buddy allocator
//!
//! The kernel needs three things from its physical allocator that a plain
//! bitmap scan does not give: allocation of *contiguous* multi-page blocks (page
//! tables want one page, DMA buffers and huge pages want many), allocation in
//! time that does not depend on how full memory is, and coalescing, so that a
//! long run of allocate/free traffic does not leave memory too fragmented to
//! satisfy a large request.
//!
//! # Representation
//!
//! Free blocks are kept on one doubly linked list per order. The links live
//! *inside the free blocks themselves*, which costs no separate metadata — a
//! free page has nothing better to do with its first sixteen bytes. Reaching
//! them is what the [`BlockLinks`] trait abstracts: the kernel writes through
//! the direct physical map, and tests use an ordinary map, which is what makes
//! this logic testable off the machine.
//!
//! Merging is driven by one bit per *pair* of buddies per order, holding the
//! parity of how many of the two are free. Freeing a block toggles its pair's
//! bit; if the result is zero, both halves are free and they merge. This is a
//! bit per pair rather than per block, and it needs no search to decide whether
//! a merge is possible.
//!
//! # What this deliberately is not
//!
//! There is no per-CPU cache and no lock inside this type. Both belong a layer
//! up, where the kernel knows about processors; keeping them out leaves this
//! module as pure, testable logic.

use core::fmt;

/// Size of a physical frame.
pub const PAGE_SIZE: u64 = 4096;

/// Largest allocation order. Order `k` is `2^k` frames, so order 10 is 4 MiB.
///
/// Large enough to back a 2 MiB huge page (order 9) with room above it, and
/// small enough that the pair bitmap stays a rounding error against RAM.
pub const MAX_ORDER: usize = 10;

/// Sentinel for "no block", used in place of a null physical address because
/// physical address zero is a real frame.
const NULL: u64 = u64::MAX;

/// The links stored inside a free block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Links {
    pub next: u64,
    pub prev: u64,
}

impl Links {
    /// Links for a block that is alone on its list.
    pub const DETACHED: Links = Links {
        next: NULL,
        prev: NULL,
    };
}

/// How the allocator reaches the links it stores inside free blocks.
///
/// # Safety
///
/// An implementation must give the allocator exclusive, stable storage for at
/// least 16 bytes at every physical address it is handed, and reads must return
/// what was last written there.
pub unsafe trait BlockLinks {
    /// Read the links stored in the free block at `phys`.
    ///
    /// # Safety
    ///
    /// `phys` must be a block the allocator currently holds on a free list.
    unsafe fn read_links(&self, phys: u64) -> Links;

    /// Write the links stored in the free block at `phys`.
    ///
    /// # Safety
    ///
    /// `phys` must be a block the allocator owns and no one else is reading.
    unsafe fn write_links(&mut self, phys: u64, links: Links);
}

/// A snapshot of allocator occupancy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Stats {
    /// Frames the allocator was told it may hand out.
    pub managed_frames: u64,
    /// Frames currently free.
    pub free_frames: u64,
    /// Number of free blocks at each order.
    pub free_blocks: [u64; MAX_ORDER + 1],
    /// Total allocations served since boot.
    pub allocations: u64,
    /// Total deallocations served since boot.
    pub deallocations: u64,
}

impl Stats {
    /// Frames currently handed out.
    #[must_use]
    pub const fn used_frames(&self) -> u64 {
        self.managed_frames - self.free_frames
    }

    /// Bytes currently free.
    #[must_use]
    pub const fn free_bytes(&self) -> u64 {
        self.free_frames * PAGE_SIZE
    }

    /// The largest order that can still be satisfied, if any.
    #[must_use]
    pub fn largest_available_order(&self) -> Option<usize> {
        (0..=MAX_ORDER).rev().find(|&k| self.free_blocks[k] > 0)
    }
}

impl fmt::Display for Stats {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} MiB free of {} MiB ({} allocations, {} frees)",
            self.free_frames * PAGE_SIZE / (1024 * 1024),
            self.managed_frames * PAGE_SIZE / (1024 * 1024),
            self.allocations,
            self.deallocations
        )
    }
}

/// Number of blocks of `order` needed to cover `frame_count` frames.
const fn blocks_at(frame_count: u64, order: usize) -> u64 {
    frame_count.div_ceil(1u64 << order)
}

/// Number of buddy pairs at `order`.
const fn pairs_at(frame_count: u64, order: usize) -> u64 {
    blocks_at(frame_count, order).div_ceil(2)
}

/// A buddy allocator over the physical range `[0, frame_count * PAGE_SIZE)`.
///
/// Nothing is available until [`BuddyAllocator::add_free_range`] hands it
/// memory: the allocator starts believing every frame is taken, so any range it
/// is not explicitly given — firmware reservations, MMIO windows, the kernel
/// image — can never be handed out.
pub struct BuddyAllocator<'a, L: BlockLinks> {
    links: L,
    /// One bit per buddy pair per order, holding the parity of how many of the
    /// pair are free. Orders are concatenated at `order_bit_offset`.
    pair_state: &'a mut [u8],
    order_bit_offset: [usize; MAX_ORDER + 2],
    free_list_head: [u64; MAX_ORDER + 1],
    free_blocks: [u64; MAX_ORDER + 1],
    frame_count: u64,
    managed_frames: u64,
    free_frames: u64,
    allocations: u64,
    deallocations: u64,
}

impl<'a, L: BlockLinks> BuddyAllocator<'a, L> {
    /// Bytes of pair-state bitmap needed to manage `frame_count` frames.
    ///
    /// Roughly one bit per frame in total across all orders, so about 32 KiB
    /// per gibibyte of RAM.
    #[must_use]
    pub const fn required_bitmap_bytes(frame_count: u64) -> usize {
        let mut bits = 0u64;
        let mut order = 0;
        while order <= MAX_ORDER {
            bits += pairs_at(frame_count, order);
            order += 1;
        }
        bits.div_ceil(8) as usize
    }

    /// Create an allocator covering `frame_count` frames, with every frame
    /// initially unavailable.
    ///
    /// `pair_state` must be at least [`BuddyAllocator::required_bitmap_bytes`]
    /// long; it is zeroed here, which is the "everything is taken" state.
    ///
    /// Returns `None` if the bitmap is too small, rather than corrupting memory
    /// by indexing past it.
    pub fn new(frame_count: u64, pair_state: &'a mut [u8], links: L) -> Option<Self> {
        if pair_state.len() < Self::required_bitmap_bytes(frame_count) {
            return None;
        }
        pair_state.fill(0);

        let mut order_bit_offset = [0usize; MAX_ORDER + 2];
        let mut offset = 0u64;
        for (order, slot) in order_bit_offset.iter_mut().enumerate().take(MAX_ORDER + 1) {
            *slot = offset as usize;
            offset += pairs_at(frame_count, order);
        }
        order_bit_offset[MAX_ORDER + 1] = offset as usize;

        Some(Self {
            links,
            pair_state,
            order_bit_offset,
            free_list_head: [NULL; MAX_ORDER + 1],
            free_blocks: [0; MAX_ORDER + 1],
            frame_count,
            managed_frames: 0,
            free_frames: 0,
            allocations: 0,
            deallocations: 0,
        })
    }

    /// Bit index of the pair containing the block at `phys` of `order`.
    fn pair_index(&self, phys: u64, order: usize) -> usize {
        let block = (phys / PAGE_SIZE) >> order;
        self.order_bit_offset[order] + (block >> 1) as usize
    }

    /// Flip the pair bit and return its new value.
    fn toggle_pair(&mut self, phys: u64, order: usize) -> bool {
        let index = self.pair_index(phys, order);
        let byte = &mut self.pair_state[index / 8];
        let mask = 1u8 << (index % 8);
        *byte ^= mask;
        *byte & mask != 0
    }

    /// Push `phys` onto the free list for `order`.
    ///
    /// # Safety
    ///
    /// `phys` must be a block of `order` that the allocator owns and that is
    /// not already on a list.
    unsafe fn list_push(&mut self, order: usize, phys: u64) {
        let head = self.free_list_head[order];
        // SAFETY: `phys` is owned by the allocator, so its links are ours to
        // write; `head`, if present, is on this same list.
        unsafe {
            self.links.write_links(
                phys,
                Links {
                    next: head,
                    prev: NULL,
                },
            );
            if head != NULL {
                let mut head_links = self.links.read_links(head);
                head_links.prev = phys;
                self.links.write_links(head, head_links);
            }
        }
        self.free_list_head[order] = phys;
        self.free_blocks[order] += 1;
    }

    /// Unlink `phys` from the free list for `order`.
    ///
    /// # Safety
    ///
    /// `phys` must currently be on that list.
    unsafe fn list_remove(&mut self, order: usize, phys: u64) {
        // SAFETY: `phys` is on this list, so its links and its neighbours' are
        // owned by the allocator.
        unsafe {
            let links = self.links.read_links(phys);
            if links.prev != NULL {
                let mut prev = self.links.read_links(links.prev);
                prev.next = links.next;
                self.links.write_links(links.prev, prev);
            } else {
                self.free_list_head[order] = links.next;
            }
            if links.next != NULL {
                let mut next = self.links.read_links(links.next);
                next.prev = links.prev;
                self.links.write_links(links.next, next);
            }
        }
        self.free_blocks[order] -= 1;
    }

    /// Take the first block from the free list for `order`.
    ///
    /// # Safety
    ///
    /// The allocator's lists must be consistent.
    unsafe fn list_pop(&mut self, order: usize) -> Option<u64> {
        let head = self.free_list_head[order];
        if head == NULL {
            return None;
        }
        // SAFETY: `head` is on this list by construction.
        unsafe { self.list_remove(order, head) };
        Some(head)
    }

    /// Allocate `2^order` contiguous frames.
    ///
    /// The returned address is aligned to the block size, which is what makes
    /// this usable for page tables and for huge pages.
    ///
    /// # Safety
    ///
    /// The allocator must own the memory it was given, and callers must not use
    /// a block after freeing it — the allocator writes its list links into the
    /// first sixteen bytes of every free block.
    pub unsafe fn allocate(&mut self, order: usize) -> Option<u64> {
        if order > MAX_ORDER {
            return None;
        }

        // Find the smallest order with something to give.
        let mut source = order;
        while source <= MAX_ORDER && self.free_list_head[source] == NULL {
            source += 1;
        }
        if source > MAX_ORDER {
            return None;
        }

        // SAFETY: the lists are consistent and `source` has a block.
        let block = unsafe { self.list_pop(source)? };
        self.toggle_pair(block, source);

        // Split down to the requested order, returning the upper half of each
        // split to the free list. The lower half stays as the block we return,
        // which is why the result keeps the alignment of the larger block.
        let mut level = source;
        while level > order {
            level -= 1;
            let buddy = block + (PAGE_SIZE << level);
            // SAFETY: `buddy` is the upper half of a block we own, and it is
            // not on any list yet.
            unsafe { self.list_push(level, buddy) };
            // One toggle covers the pair: the halves share a bit, and after
            // the split exactly one of them is free.
            self.toggle_pair(buddy, level);
        }

        self.free_frames -= 1u64 << order;
        self.allocations += 1;
        Some(block)
    }

    /// Allocate a single frame.
    ///
    /// # Safety
    ///
    /// See [`BuddyAllocator::allocate`].
    pub unsafe fn allocate_frame(&mut self) -> Option<u64> {
        // SAFETY: delegated.
        unsafe { self.allocate(0) }
    }

    /// Return `2^order` frames at `phys` to the allocator.
    ///
    /// # Safety
    ///
    /// `phys` must have come from [`BuddyAllocator::allocate`] with the same
    /// `order` and must not have been freed since. Freeing a block twice, or at
    /// the wrong order, corrupts the pair bitmap and will eventually hand the
    /// same memory to two callers.
    pub unsafe fn deallocate(&mut self, phys: u64, order: usize) {
        debug_assert!(order <= MAX_ORDER, "order out of range");
        debug_assert!(
            phys.is_multiple_of(PAGE_SIZE << order),
            "block is not aligned to its order"
        );

        let mut block = phys;
        let mut level = order;

        loop {
            // The bit holds the parity of how many of the pair are free. This
            // block has just become free, so a zero means both halves are.
            let both_free = !self.toggle_pair(block, level);
            if !both_free || level == MAX_ORDER {
                break;
            }

            let buddy = block ^ (PAGE_SIZE << level);
            // A buddy beyond the managed range was never freed and cannot be
            // merged with, which matters whenever the frame count is not a
            // power of two.
            if buddy / PAGE_SIZE >= self.frame_count {
                break;
            }

            // SAFETY: `both_free` means the buddy is on the free list for this
            // order, so removing it is valid.
            unsafe { self.list_remove(level, buddy) };
            block = block.min(buddy);
            level += 1;
        }

        // SAFETY: `block` is a block of `level` that the allocator owns and
        // that is not on any list.
        unsafe { self.list_push(level, block) };
        self.free_frames += 1u64 << order;
        self.deallocations += 1;
    }

    /// Free a single frame.
    ///
    /// # Safety
    ///
    /// See [`BuddyAllocator::deallocate`].
    pub unsafe fn deallocate_frame(&mut self, phys: u64) {
        // SAFETY: delegated.
        unsafe { self.deallocate(phys, 0) };
    }

    /// Hand `[start, end)` to the allocator as free memory.
    ///
    /// Both addresses are rounded inward to page boundaries, so a partially
    /// usable page is never handed out. The range is broken into the largest
    /// aligned blocks that fit, which keeps the free lists compact from the
    /// first allocation rather than relying on later merges.
    ///
    /// # Safety
    ///
    /// The range must be real, writable RAM that nothing else owns. Anything
    /// added here can be handed to any caller.
    pub unsafe fn add_free_range(&mut self, start: u64, end: u64) {
        let mut frame = start.div_ceil(PAGE_SIZE);
        let end_frame = (end / PAGE_SIZE).min(self.frame_count);

        while frame < end_frame {
            // The largest order that is both aligned at `frame` and fits in
            // what is left.
            let mut order = MAX_ORDER;
            loop {
                let size = 1u64 << order;
                if order == 0 || (frame.is_multiple_of(size) && size <= end_frame - frame) {
                    break;
                }
                order -= 1;
            }

            self.managed_frames += 1u64 << order;
            // `deallocate` does the real work: it sets the pair bits and links
            // the block in, merging with anything adjacent that is already
            // free. It also counts a deallocation, which this is not — memory
            // entering the allocator is not memory being returned to it — so
            // that one statistic is corrected.
            //
            // SAFETY: the caller guarantees this range is usable RAM the
            // allocator may own.
            unsafe { self.deallocate(frame * PAGE_SIZE, order) };
            self.deallocations -= 1;

            frame += 1u64 << order;
        }
    }

    /// A snapshot of occupancy.
    #[must_use]
    pub fn stats(&self) -> Stats {
        Stats {
            managed_frames: self.managed_frames,
            free_frames: self.free_frames,
            free_blocks: self.free_blocks,
            allocations: self.allocations,
            deallocations: self.deallocations,
        }
    }

    /// Total frames the allocator may hand out.
    #[must_use]
    pub fn managed_frames(&self) -> u64 {
        self.managed_frames
    }

    /// Frames currently free.
    #[must_use]
    pub fn free_frames(&self) -> u64 {
        self.free_frames
    }
}
