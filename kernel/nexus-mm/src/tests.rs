//! Tests for the buddy allocator.
//!
//! A physical allocator has one failure mode that matters more than all the
//! others: handing the same memory to two callers. It is silent, it corrupts
//! state far away from the allocator, and by the time anything notices, the
//! evidence is gone. Several of the tests below exist only to make that
//! specific bug impossible to introduce unnoticed.

use std::collections::{HashMap, HashSet};

use crate::buddy::{BlockLinks, BuddyAllocator, Links, MAX_ORDER, PAGE_SIZE};

/// A [`BlockLinks`] implementation backed by a map instead of real memory.
///
/// It also records which addresses have ever been written, which lets a test
/// assert that the allocator only ever touches blocks it owns.
#[derive(Default)]
struct MapLinks {
    storage: HashMap<u64, Links>,
}

// SAFETY: the map gives every address independent, stable storage, and reads
// return exactly what was last written.
unsafe impl BlockLinks for MapLinks {
    unsafe fn read_links(&self, phys: u64) -> Links {
        *self
            .storage
            .get(&phys)
            .expect("the allocator read links it never wrote")
    }

    unsafe fn write_links(&mut self, phys: u64, links: Links) {
        self.storage.insert(phys, links);
    }
}

/// Build an allocator over `frames` frames with nothing yet available.
fn allocator(frames: u64) -> (BuddyAllocator<'static, MapLinks>, ()) {
    let bytes = BuddyAllocator::<MapLinks>::required_bitmap_bytes(frames);
    // Leaked so the allocator can hold a `'static` borrow, which keeps the
    // test signatures free of lifetime plumbing.
    let bitmap: &'static mut [u8] = Box::leak(vec![0u8; bytes].into_boxed_slice());
    (
        BuddyAllocator::new(frames, bitmap, MapLinks::default()).expect("bitmap is large enough"),
        (),
    )
}

/// Build an allocator over `frames` frames with all of them available.
fn populated(frames: u64) -> BuddyAllocator<'static, MapLinks> {
    let (mut allocator, ()) = allocator(frames);
    // SAFETY: this is test-only bookkeeping over a map, not real memory.
    unsafe { allocator.add_free_range(0, frames * PAGE_SIZE) };
    allocator
}

#[test]
fn a_new_allocator_hands_out_nothing() {
    let (mut allocator, ()) = allocator(1024);
    // SAFETY: test-only backing store.
    unsafe {
        assert_eq!(allocator.allocate_frame(), None);
    }
    assert_eq!(allocator.free_frames(), 0);
    assert_eq!(allocator.managed_frames(), 0);
}

#[test]
fn added_memory_becomes_available() {
    let allocator = populated(1024);
    assert_eq!(allocator.managed_frames(), 1024);
    assert_eq!(allocator.free_frames(), 1024);
}

#[test]
fn an_allocation_reduces_the_free_count_by_its_whole_block() {
    let mut allocator = populated(1024);
    // SAFETY: test-only backing store.
    unsafe {
        allocator.allocate(3).expect("8 frames should be available");
    }
    assert_eq!(allocator.free_frames(), 1024 - 8);
}

#[test]
fn every_allocation_is_aligned_to_its_order() {
    let mut allocator = populated(1024);
    for order in 0..=6 {
        // SAFETY: test-only backing store.
        let block = unsafe { allocator.allocate(order) }.expect("memory is available");
        assert_eq!(
            block % (PAGE_SIZE << order),
            0,
            "an order-{order} block must be aligned to {} bytes",
            PAGE_SIZE << order
        );
    }
}

/// The failure this whole module exists to prevent.
#[test]
fn no_frame_is_ever_handed_out_twice() {
    let mut allocator = populated(512);
    let mut seen = HashSet::new();

    // SAFETY: test-only backing store.
    unsafe {
        while let Some(block) = allocator.allocate_frame() {
            assert!(seen.insert(block), "frame {block:#x} was handed out twice");
        }
    }

    assert_eq!(seen.len(), 512, "every frame should have been handed out");
    assert_eq!(allocator.free_frames(), 0);
}

#[test]
fn mixed_order_allocations_never_overlap() {
    let mut allocator = populated(1024);
    let mut blocks = Vec::new();
    let mut covered = HashSet::new();

    // A repeating pattern of orders, which forces repeated splitting of larger
    // blocks and interleaves the results.
    let orders = [0usize, 2, 1, 4, 0, 3, 2, 5, 1, 0];
    // SAFETY: test-only backing store.
    unsafe {
        for &order in orders.iter().cycle().take(120) {
            let Some(block) = allocator.allocate(order) else {
                break;
            };
            for frame in 0..(1u64 << order) {
                let address = block + frame * PAGE_SIZE;
                assert!(
                    covered.insert(address),
                    "frame {address:#x} was covered by two allocations"
                );
            }
            blocks.push((block, order));
        }
    }

    assert!(blocks.len() > 50, "the test should have made real progress");
    assert_eq!(
        allocator.free_frames(),
        1024 - covered.len() as u64,
        "the free count must agree with what was actually handed out"
    );
}

#[test]
fn freeing_everything_restores_the_original_state() {
    let mut allocator = populated(1024);
    let mut blocks = Vec::new();

    // SAFETY: test-only backing store.
    unsafe {
        for order in [0, 1, 2, 3, 4, 0, 1, 2] {
            blocks.push((allocator.allocate(order).expect("memory available"), order));
        }
        for (block, order) in blocks {
            allocator.deallocate(block, order);
        }
    }

    assert_eq!(allocator.free_frames(), 1024);
    // Full coalescing should have put the memory back into whole large blocks.
    let stats = allocator.stats();
    assert_eq!(
        stats.free_blocks[MAX_ORDER], 1,
        "1024 frames should coalesce back into a single order-10 block"
    );
    for order in 0..MAX_ORDER {
        assert_eq!(
            stats.free_blocks[order], 0,
            "no fragments should remain at order {order}"
        );
    }
}

#[test]
fn buddies_coalesce_back_into_a_larger_block() {
    let mut allocator = populated(2);

    // SAFETY: test-only backing store.
    unsafe {
        let first = allocator.allocate(0).expect("frame available");
        let second = allocator.allocate(0).expect("frame available");
        assert_eq!(allocator.stats().free_blocks[1], 0);

        allocator.deallocate(first, 0);
        // One half free: no merge is possible yet.
        assert_eq!(allocator.stats().free_blocks[0], 1);
        assert_eq!(allocator.stats().free_blocks[1], 0);

        allocator.deallocate(second, 0);
        // Both halves free: they must have merged rather than sitting as two
        // order-0 blocks, or a later order-1 request would fail with memory
        // available.
        assert_eq!(allocator.stats().free_blocks[0], 0);
        assert_eq!(allocator.stats().free_blocks[1], 1);
        assert!(allocator.allocate(1).is_some());
    }
}

#[test]
fn a_large_request_is_satisfied_after_the_memory_is_returned() {
    let mut allocator = populated(1 << MAX_ORDER);

    // SAFETY: test-only backing store.
    unsafe {
        // Fragment the arena completely.
        let mut frames = Vec::new();
        while let Some(frame) = allocator.allocate_frame() {
            frames.push(frame);
        }
        assert_eq!(allocator.allocate(MAX_ORDER), None);

        for frame in frames {
            allocator.deallocate_frame(frame);
        }

        // Coalescing must have rebuilt a whole maximum-order block.
        assert!(
            allocator.allocate(MAX_ORDER).is_some(),
            "the arena should be whole again after everything was returned"
        );
    }
}

#[test]
fn exhaustion_is_reported_rather_than_wrapping() {
    let mut allocator = populated(4);
    // SAFETY: test-only backing store.
    unsafe {
        assert!(allocator.allocate(2).is_some());
        assert_eq!(allocator.allocate(0), None);
        assert_eq!(allocator.allocate(2), None);
    }
    assert_eq!(allocator.free_frames(), 0);
}

#[test]
fn requests_larger_than_the_maximum_order_are_refused() {
    let mut allocator = populated(1 << MAX_ORDER);
    // SAFETY: test-only backing store.
    unsafe {
        assert_eq!(allocator.allocate(MAX_ORDER + 1), None);
    }
}

/// Frame counts are rarely powers of two on a real machine, and the buddy of a
/// block near the top may lie outside memory entirely.
#[test]
fn a_non_power_of_two_arena_never_merges_past_its_end() {
    let mut allocator = populated(100);
    assert_eq!(allocator.managed_frames(), 100);
    assert_eq!(allocator.free_frames(), 100);

    let mut seen = HashSet::new();
    // SAFETY: test-only backing store.
    unsafe {
        while let Some(frame) = allocator.allocate_frame() {
            assert!(frame / PAGE_SIZE < 100, "handed out a frame past the end");
            assert!(seen.insert(frame), "frame {frame:#x} handed out twice");
        }
    }
    assert_eq!(seen.len(), 100);
}

/// Real memory maps have holes: firmware reservations, MMIO, the kernel image.
#[test]
fn memory_added_in_disjoint_ranges_never_hands_out_the_holes() {
    let (mut allocator, ()) = allocator(1024);

    // Two usable windows with a reserved gap between them, and a reserved tail.
    // SAFETY: test-only backing store.
    unsafe {
        allocator.add_free_range(0, 100 * PAGE_SIZE);
        allocator.add_free_range(300 * PAGE_SIZE, 500 * PAGE_SIZE);
    }
    assert_eq!(allocator.managed_frames(), 300);

    let mut seen = HashSet::new();
    // SAFETY: test-only backing store.
    unsafe {
        while let Some(frame) = allocator.allocate_frame() {
            let index = frame / PAGE_SIZE;
            assert!(
                index < 100 || (300..500).contains(&index),
                "frame {index} came from a reserved hole"
            );
            assert!(seen.insert(frame), "frame {frame:#x} handed out twice");
        }
    }
    assert_eq!(seen.len(), 300);
}

#[test]
fn partial_pages_at_the_edges_of_a_range_are_not_handed_out() {
    let (mut allocator, ()) = allocator(16);
    // A range that starts and ends mid-page: only the whole pages inside it
    // may be used.
    // SAFETY: test-only backing store.
    unsafe {
        allocator.add_free_range(PAGE_SIZE / 2, 4 * PAGE_SIZE + PAGE_SIZE / 2);
    }
    // Pages 1, 2 and 3 are entirely inside; 0 and 4 are only partly covered.
    assert_eq!(allocator.managed_frames(), 3);

    // SAFETY: test-only backing store.
    unsafe {
        while let Some(frame) = allocator.allocate_frame() {
            let index = frame / PAGE_SIZE;
            assert!(
                (1..4).contains(&index),
                "frame {index} was only partly usable"
            );
        }
    }
}

#[test]
fn statistics_track_allocations_and_frees() {
    let mut allocator = populated(64);
    // SAFETY: test-only backing store.
    unsafe {
        let a = allocator.allocate(0).unwrap();
        let b = allocator.allocate(2).unwrap();
        let stats = allocator.stats();
        assert_eq!(stats.allocations, 2);
        assert_eq!(stats.deallocations, 0);
        assert_eq!(stats.used_frames(), 5);

        allocator.deallocate(a, 0);
        allocator.deallocate(b, 2);
        let stats = allocator.stats();
        assert_eq!(stats.deallocations, 2);
        assert_eq!(stats.used_frames(), 0);
        assert_eq!(stats.free_bytes(), 64 * PAGE_SIZE);
    }
}

#[test]
fn largest_available_order_reflects_fragmentation() {
    let mut allocator = populated(1 << MAX_ORDER);
    assert_eq!(allocator.stats().largest_available_order(), Some(MAX_ORDER));

    // SAFETY: test-only backing store.
    unsafe {
        // Taking a single frame splits the whole arena down to order 0, so the
        // largest remaining block drops one order below the maximum.
        allocator.allocate_frame().unwrap();
    }
    assert_eq!(
        allocator.stats().largest_available_order(),
        Some(MAX_ORDER - 1)
    );
}

#[test]
fn the_bitmap_is_sized_for_the_arena() {
    // About one bit per frame across all orders: 32 KiB per GiB of RAM.
    let frames_in_a_gibibyte = 1024 * 1024 * 1024 / PAGE_SIZE;
    let bytes = BuddyAllocator::<MapLinks>::required_bitmap_bytes(frames_in_a_gibibyte);
    assert!(
        (30 * 1024..36 * 1024).contains(&bytes),
        "expected roughly 32 KiB per GiB, got {bytes} bytes"
    );

    // A bitmap that is too small must be refused rather than overrun.
    let mut too_small = vec![0u8; bytes - 1];
    assert!(
        BuddyAllocator::new(frames_in_a_gibibyte, &mut too_small, MapLinks::default()).is_none()
    );
}

/// A long randomised run, which is what catches bookkeeping errors that a
/// hand-written sequence happens to step around.
#[test]
fn a_long_mixed_workload_keeps_the_allocator_consistent() {
    let mut allocator = populated(2048);
    let mut live: Vec<(u64, usize)> = Vec::new();
    let mut covered: HashSet<u64> = HashSet::new();

    // A deterministic xorshift, so a failure is reproducible.
    let mut state = 0x2545_F491_4F6C_DD1Du64;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };

    // SAFETY: test-only backing store.
    unsafe {
        for step in 0..20_000 {
            let allocating = live.is_empty() || (next() % 100 < 55);

            if allocating {
                let order = (next() % 5) as usize;
                if let Some(block) = allocator.allocate(order) {
                    for frame in 0..(1u64 << order) {
                        let address = block + frame * PAGE_SIZE;
                        assert!(
                            covered.insert(address),
                            "step {step}: {address:#x} was already allocated"
                        );
                    }
                    live.push((block, order));
                }
            } else {
                let index = (next() % live.len() as u64) as usize;
                let (block, order) = live.swap_remove(index);
                for frame in 0..(1u64 << order) {
                    covered.remove(&(block + frame * PAGE_SIZE));
                }
                allocator.deallocate(block, order);
            }

            assert_eq!(
                allocator.free_frames(),
                2048 - covered.len() as u64,
                "step {step}: the free count drifted from reality"
            );
        }

        // Returning everything must restore the arena exactly.
        for (block, order) in live {
            allocator.deallocate(block, order);
        }
    }

    assert_eq!(allocator.free_frames(), 2048);
    assert_eq!(
        allocator.stats().free_blocks[MAX_ORDER],
        2,
        "2048 frames should coalesce into two maximum-order blocks"
    );
}
