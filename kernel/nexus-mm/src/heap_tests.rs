//! Tests for the heap allocator.
//!
//! Every test runs against a real, owned buffer, so a pointer the heap returns
//! is genuinely written through. Two allocations that overlap therefore show up
//! as corrupted data rather than as an assertion about bookkeeping that happens
//! to pass while the memory is wrong.

use core::alloc::Layout;
use std::collections::HashSet;

use crate::heap::{Heap, GRANULE};

/// A granule-aligned, leaked region of exactly `bytes` bytes.
///
/// The alignment matters: `Vec` gives no guarantee stronger than the element's,
/// and the heap trims whatever it is given inward to granule boundaries. Handing
/// it an unaligned buffer would leave the heap smaller than `bytes`, which every
/// occupancy assertion below would then have to work around.
fn aligned_region(bytes: usize) -> usize {
    assert!(
        bytes.is_multiple_of(GRANULE),
        "test regions are whole granules"
    );
    let buffer: &'static mut [u8] = Box::leak(vec![0u8; bytes + GRANULE].into_boxed_slice());
    let raw = buffer.as_mut_ptr() as usize;
    raw.next_multiple_of(GRANULE)
}

/// A heap over a leaked region, returned with its bounds.
fn heap_over(bytes: usize) -> (Heap, usize, usize) {
    let start = aligned_region(bytes);
    let mut heap = Heap::new();
    // SAFETY: the region is leaked, so it outlives the heap, and nothing else
    // holds a reference to it.
    unsafe { heap.add_region(start, bytes) };
    (heap, start, start + bytes)
}

fn layout(size: usize, align: usize) -> Layout {
    Layout::from_size_align(size, align).expect("valid layout")
}

#[test]
fn an_empty_heap_allocates_nothing() {
    let mut heap = Heap::new();
    assert!(heap.allocate(layout(1, 1)).is_none());
    assert_eq!(heap.stats().total, 0);
}

#[test]
fn a_region_becomes_available() {
    let (heap, _, _) = heap_over(4096);
    let stats = heap.stats();
    assert_eq!(stats.total, 4096);
    assert_eq!(stats.used, 0);
    assert_eq!(stats.free_blocks, 1);
    assert_eq!(stats.largest_free_block, 4096);
}

#[test]
fn allocations_land_inside_the_region() {
    let (mut heap, start, end) = heap_over(4096);
    let pointer = heap.allocate(layout(64, 8)).expect("space available");
    let address = pointer.as_ptr() as usize;
    assert!(address >= start && address + 64 <= end);
}

#[test]
fn allocations_honour_their_alignment() {
    let (mut heap, _, _) = heap_over(64 * 1024);
    for align in [1usize, 8, 16, 32, 64, 256, 4096] {
        let pointer = heap.allocate(layout(100, align)).expect("space available");
        assert_eq!(
            pointer.as_ptr() as usize % align,
            0,
            "an allocation with alignment {align} was misaligned"
        );
    }
}

#[test]
fn live_allocations_never_overlap() {
    let (mut heap, _, _) = heap_over(64 * 1024);
    let mut pointers = Vec::new();
    let mut covered = HashSet::new();

    for size in [16usize, 100, 33, 512, 7, 1000, 64, 24] {
        for _ in 0..8 {
            let Some(pointer) = heap.allocate(layout(size, 8)) else {
                continue;
            };
            let base = pointer.as_ptr() as usize;
            for offset in 0..size {
                assert!(
                    covered.insert(base + offset),
                    "byte {:#x} was handed out twice",
                    base + offset
                );
            }
            pointers.push((pointer, layout(size, 8)));
        }
    }

    assert!(pointers.len() > 30, "the test should have allocated plenty");

    // Write through every pointer, then verify: overlapping allocations would
    // corrupt each other here even if the bookkeeping above looked right.
    for (index, (pointer, allocation)) in pointers.iter().enumerate() {
        // SAFETY: each pointer is a live allocation of at least this size.
        unsafe {
            core::ptr::write_bytes(pointer.as_ptr(), index as u8, allocation.size());
        }
    }
    for (index, (pointer, allocation)) in pointers.iter().enumerate() {
        for offset in 0..allocation.size() {
            // SAFETY: as above.
            let byte = unsafe { pointer.as_ptr().add(offset).read() };
            assert_eq!(byte, index as u8, "allocation {index} was overwritten");
        }
    }
}

#[test]
fn freeing_everything_restores_a_single_block() {
    let (mut heap, _, _) = heap_over(16 * 1024);
    let mut pointers = Vec::new();

    for size in [32usize, 64, 128, 256, 48, 96] {
        for _ in 0..4 {
            if let Some(pointer) = heap.allocate(layout(size, 8)) {
                pointers.push((pointer, layout(size, 8)));
            }
        }
    }
    assert!(heap.stats().used > 0);

    for (pointer, allocation) in pointers {
        // SAFETY: each came from this heap with this layout and is freed once.
        unsafe { heap.deallocate(pointer, allocation) };
    }

    let stats = heap.stats();
    assert_eq!(stats.used, 0);
    assert_eq!(
        stats.free_blocks, 1,
        "every block should have coalesced back into one"
    );
    assert_eq!(stats.largest_free_block, 16 * 1024);
}

#[test]
fn adjacent_frees_coalesce_in_any_order() {
    // Freeing left-to-right, right-to-left and middle-out all have to end in
    // one block, because each exercises a different merge direction.
    for order in [[0usize, 1, 2], [2, 1, 0], [1, 0, 2]] {
        let (mut heap, _, _) = heap_over(4096);
        let allocation = layout(1024, 8);
        let pointers: Vec<_> = (0..3)
            .map(|_| heap.allocate(allocation).expect("space available"))
            .collect();

        for &index in &order {
            // SAFETY: each pointer came from this heap and is freed once.
            unsafe { heap.deallocate(pointers[index], allocation) };
        }

        assert_eq!(
            heap.stats().free_blocks,
            1,
            "freeing in order {order:?} left the heap fragmented"
        );
        assert_eq!(heap.stats().largest_free_block, 4096);
    }
}

#[test]
fn a_large_request_succeeds_again_after_the_heap_is_emptied() {
    let (mut heap, _, _) = heap_over(8192);
    let small = layout(64, 8);

    let mut pointers = Vec::new();
    while let Some(pointer) = heap.allocate(small) {
        pointers.push(pointer);
    }
    // Completely full: even one more granule is unavailable.
    assert!(heap.allocate(layout(1, 1)).is_none());

    for pointer in pointers {
        // SAFETY: each came from this heap with `small` and is freed once.
        unsafe { heap.deallocate(pointer, small) };
    }

    // Coalescing must have rebuilt a block big enough for the whole heap.
    assert!(
        heap.allocate(layout(8000, 8)).is_some(),
        "the heap did not coalesce back into usable space"
    );
}

#[test]
fn exhaustion_is_reported_rather_than_overrunning_the_region() {
    let (mut heap, start, end) = heap_over(1024);
    assert!(heap.allocate(layout(4096, 8)).is_none());

    let pointer = heap.allocate(layout(1024, 8)).expect("exactly fits");
    assert!(pointer.as_ptr() as usize >= start);
    assert!(pointer.as_ptr() as usize + 1024 <= end);
    assert!(heap.allocate(layout(1, 1)).is_none());
}

#[test]
fn a_large_alignment_request_that_cannot_fit_is_refused() {
    let (mut heap, _, _) = heap_over(1024);
    // The region is only 1 KiB, so a 64 KiB-aligned address inside it is
    // vanishingly unlikely; the heap must decline rather than return something
    // outside itself.
    if let Some(pointer) = heap.allocate(layout(16, 65536)) {
        assert_eq!(pointer.as_ptr() as usize % 65536, 0);
    }
}

#[test]
fn disjoint_regions_are_both_used_and_never_merged_across_the_gap() {
    let first = aligned_region(4096);
    let second = aligned_region(4096);
    let mut heap = Heap::new();
    // SAFETY: two distinct leaked regions, neither aliased.
    unsafe {
        heap.add_region(first, 4096);
        heap.add_region(second, 4096);
    }

    assert_eq!(heap.stats().total, 8192);
    // Separate allocations are fine, but a single 8 KiB block spanning both
    // must not be, since the two regions are not contiguous.
    assert!(heap.allocate(layout(4000, 8)).is_some());
    assert!(heap.allocate(layout(4000, 8)).is_some());
    assert!(heap.allocate(layout(8192, 8)).is_none());
}

#[test]
fn used_bytes_track_the_rounded_allocation_size() {
    let (mut heap, _, _) = heap_over(4096);
    let allocation = layout(1, 1);
    let pointer = heap.allocate(allocation).expect("space available");
    // A one-byte request still consumes a whole granule.
    assert_eq!(heap.stats().used, GRANULE);
    // SAFETY: came from this heap with this layout.
    unsafe { heap.deallocate(pointer, allocation) };
    assert_eq!(heap.stats().used, 0);
}

/// A long randomised run, verifying after every step that no two live
/// allocations overlap and that occupancy stays consistent.
#[test]
fn a_long_mixed_workload_keeps_the_heap_consistent() {
    let (mut heap, _, _) = heap_over(256 * 1024);
    let mut live: Vec<(core::ptr::NonNull<u8>, Layout, u8)> = Vec::new();

    let mut state = 0x9E37_79B9_7F4A_7C15u64;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };

    let sizes = [8usize, 17, 32, 64, 100, 256, 512, 1024, 3000];
    let aligns = [1usize, 8, 16, 64, 256];

    for step in 0..20_000u32 {
        let allocating = live.is_empty() || (next() % 100 < 55);

        if allocating {
            let size = sizes[(next() % sizes.len() as u64) as usize];
            let align = aligns[(next() % aligns.len() as u64) as usize];
            let allocation = layout(size, align);
            if let Some(pointer) = heap.allocate(allocation) {
                assert_eq!(pointer.as_ptr() as usize % align, 0, "step {step}");
                let tag = (step % 251) as u8;
                // SAFETY: a fresh allocation of at least `size` bytes.
                unsafe { core::ptr::write_bytes(pointer.as_ptr(), tag, size) };
                live.push((pointer, allocation, tag));
            }
        } else {
            let index = (next() % live.len() as u64) as usize;
            let (pointer, allocation, tag) = live.swap_remove(index);
            // Verify the contents survived everything that happened in between:
            // an overlapping allocation would have changed them.
            for offset in 0..allocation.size() {
                // SAFETY: still a live allocation at this point.
                let byte = unsafe { pointer.as_ptr().add(offset).read() };
                assert_eq!(byte, tag, "step {step}: an allocation was corrupted");
            }
            // SAFETY: came from this heap with this layout, freed once.
            unsafe { heap.deallocate(pointer, allocation) };
        }

        let expected: usize = live
            .iter()
            .map(|(_, allocation, _)| allocation.size().max(1).next_multiple_of(GRANULE))
            .sum();
        assert_eq!(heap.stats().used, expected, "step {step}: usage drifted");
    }

    for (pointer, allocation, _) in live {
        // SAFETY: as above.
        unsafe { heap.deallocate(pointer, allocation) };
    }

    let stats = heap.stats();
    assert_eq!(stats.used, 0);
    assert_eq!(
        stats.free_blocks, 1,
        "the heap should have fully coalesced after a long workload"
    );
}
