//! Memory for a program that has no C library to get it from.
//!
//! Everything in this crate has been `#![no_std]` with no allocator, which is
//! fine until a program needs a `Vec`. Reading a SPIR-V module needs several:
//! the instructions, the operands, the value of every vector a shader computes.
//!
//! So this is an allocator. It is the simplest one that is honest about what it
//! is: a block of anonymous memory from `mmap`, and a pointer that only ever
//! moves forward.
//!
//! # It does not free
//!
//! `dealloc` does nothing. That is not laziness and it is not hidden: it is a
//! deliberate trade for a program whose allocation has a *shape*.
//!
//! A shader invocation allocates a few hundred small things and then has no
//! state at all — that is what a shader is, and it is why a graphics processor
//! can run thousands at once. So the program marks where the bump pointer is,
//! runs one invocation, takes the colour out, and winds the pointer back. The
//! memory is reused exactly, and nothing is ever tracked.
//!
//! A program with a different shape must not use this. A long-running one would
//! consume the arena and stop, which is why [`take`] returns a null pointer
//! rather than growing: a null from an allocator turns into a clean
//! `handle_alloc_error` rather than a program quietly writing past the end.
//!
//! # Why a bump pointer and not a real allocator
//!
//! A free list, size classes and coalescing are perhaps three hundred lines
//! and a week of being wrong in ways that only show up under load. This is
//! twenty lines and it is right by inspection. The Nexus side of this
//! repository has a real allocator; a Linux fixture does not need one, and
//! having one here would mean maintaining two.

use core::alloc::{GlobalAlloc, Layout};
use core::sync::atomic::{AtomicUsize, Ordering};

/// Where the arena starts, where the next allocation goes, and where it ends.
///
/// Atomics because a `GlobalAlloc` must be safe to call from several threads,
/// and two of the programs in this crate start threads. The bump is a
/// compare-and-exchange loop, so two threads allocating at once get different
/// memory rather than the same address twice.
static BASE: AtomicUsize = AtomicUsize::new(0);
static NEXT: AtomicUsize = AtomicUsize::new(0);
static END: AtomicUsize = AtomicUsize::new(0);

/// Take an arena of `bytes` from the kernel. Call once, before anything
/// allocates.
///
/// Returns false if the mapping failed, which a caller should report rather
/// than carry on from: every allocation afterwards will be a null pointer.
pub fn start(bytes: usize) -> bool {
    let Some(at) = crate::map_anonymous(bytes) else {
        return false;
    };
    let at = at as usize;
    BASE.store(at, Ordering::Release);
    NEXT.store(at, Ordering::Release);
    END.store(at + bytes, Ordering::Release);
    true
}

/// Where the bump pointer is now.
///
/// Kept so that a caller can wind back to it. Meaningless to anything else.
#[must_use]
pub fn mark() -> usize {
    NEXT.load(Ordering::Acquire)
}

/// Wind the bump pointer back to a mark.
///
/// # Safety
///
/// Nothing allocated after the mark was taken may still be in use. In practice
/// that means: take a mark, do a piece of work, let everything that work
/// allocated go out of scope, *then* reset. Resetting while a `Vec` is still
/// alive hands the same memory out twice.
pub unsafe fn reset_to(mark: usize) {
    NEXT.store(mark, Ordering::Release);
}

/// How much of the arena is in use, and how large it is.
#[must_use]
pub fn used() -> (usize, usize) {
    let base = BASE.load(Ordering::Acquire);
    (
        NEXT.load(Ordering::Acquire).saturating_sub(base),
        END.load(Ordering::Acquire).saturating_sub(base),
    )
}

/// Take `layout` out of the arena, or null.
fn take(layout: Layout) -> *mut u8 {
    let end = END.load(Ordering::Acquire);
    loop {
        let next = NEXT.load(Ordering::Acquire);
        // Rounded up to the alignment the caller asked for. A misaligned
        // allocation is not a slow allocation here, it is a fault on the first
        // sixteen-byte access.
        let aligned = (next + layout.align() - 1) & !(layout.align() - 1);
        let Some(after) = aligned.checked_add(layout.size()) else {
            return core::ptr::null_mut();
        };
        if after > end {
            // Out of arena. Null, so that the caller's `handle_alloc_error`
            // runs and the program stops where it can be seen.
            return core::ptr::null_mut();
        }
        if NEXT
            .compare_exchange(next, after, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            return aligned as *mut u8;
        }
    }
}

/// The allocator itself.
pub struct Arena;

// SAFETY: `alloc` returns either null or a block of `layout.size()` bytes,
// aligned as asked, taken from a mapping this program owns and handed out
// exactly once between resets. `dealloc` does nothing, which is always a valid
// implementation -- it is `alloc` that has to be correct.
unsafe impl GlobalAlloc for Arena {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        take(layout)
    }

    unsafe fn dealloc(&self, _pointer: *mut u8, _layout: Layout) {
        // Nothing. See the note at the top of this file.
    }
}

/// What a program that wants an allocator writes.
///
/// A macro rather than a `#[global_allocator]` in this crate, because a crate
/// that declared one would force it on every program here -- including the ones
/// that allocate nothing and should not be carrying an arena.
#[macro_export]
macro_rules! guest_heap {
    () => {
        // A failed allocation goes through the default handler, which panics
        // -- and the panic handler in `guest_main!` says so and exits 99. That
        // is enough: a program here that runs out of arena has a bug in how
        // much it asked for, and the number is the message.
        #[global_allocator]
        static ALLOCATOR: $crate::heap::Arena = $crate::heap::Arena;
    };
}
