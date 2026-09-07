//! # Nexus memory management
//!
//! Physical memory management for the Nexus Kernel, kept in its own crate so
//! that the allocator is ordinary testable code rather than something only
//! observable by booting a machine. The kernel supplies the one piece that
//! cannot be tested off-target — access to physical memory through the direct
//! map — through the [`buddy::BlockLinks`] trait.
//!
//! Under `cfg(test)` this crate links against `std` for the test harness. It
//! uses no `std` facilities itself.

#![cfg_attr(not(test), no_std)]
#![deny(unsafe_op_in_unsafe_fn)]

pub mod buddy;

#[cfg(test)]
mod tests;

pub use buddy::{BlockLinks, BuddyAllocator, Links, Stats, MAX_ORDER, PAGE_SIZE};

/// The smallest order that holds at least `frames` frames.
///
/// Buddy allocation is always a power of two, so a request for three frames
/// consumes four. Callers that care about the difference should say so by
/// asking for a specific order.
#[must_use]
pub fn order_for_frames(frames: u64) -> Option<usize> {
    if frames == 0 {
        return None;
    }
    let order = (frames.next_power_of_two().trailing_zeros()) as usize;
    (order <= MAX_ORDER).then_some(order)
}

/// The smallest order that holds at least `bytes`.
#[must_use]
pub fn order_for_bytes(bytes: u64) -> Option<usize> {
    order_for_frames(bytes.div_ceil(PAGE_SIZE))
}

#[cfg(test)]
mod helper_tests {
    use super::*;

    #[test]
    fn order_for_frames_rounds_up_to_a_power_of_two() {
        assert_eq!(order_for_frames(1), Some(0));
        assert_eq!(order_for_frames(2), Some(1));
        assert_eq!(order_for_frames(3), Some(2));
        assert_eq!(order_for_frames(4), Some(2));
        assert_eq!(order_for_frames(5), Some(3));
        assert_eq!(order_for_frames(1 << MAX_ORDER), Some(MAX_ORDER));
    }

    #[test]
    fn order_for_frames_rejects_zero_and_oversized_requests() {
        assert_eq!(order_for_frames(0), None);
        assert_eq!(order_for_frames((1 << MAX_ORDER) + 1), None);
    }

    #[test]
    fn order_for_bytes_rounds_up_to_whole_pages() {
        assert_eq!(order_for_bytes(1), Some(0));
        assert_eq!(order_for_bytes(PAGE_SIZE), Some(0));
        assert_eq!(order_for_bytes(PAGE_SIZE + 1), Some(1));
        assert_eq!(order_for_bytes(4 * PAGE_SIZE), Some(2));
    }
}
