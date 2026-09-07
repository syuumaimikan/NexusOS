//! Kernel threads and their stacks.
//!
//! The priority levels and stack accessors describe the whole model, including
//! the parts the scheduler does not yet exercise.
#![allow(dead_code)]

use alloc::boxed::Box;
use alloc::string::String;

use nexus_abi::layout;

use super::context::{INITIAL_RFLAGS, SAVED_REGISTER_COUNT};
use crate::memory::{self, paging};

/// A thread's identity. Never reused, so a stale reference is always detectable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ThreadId(pub u64);

impl core::fmt::Display for ThreadId {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "#{}", self.0)
    }
}

/// Scheduling priority.
///
/// Strict priority: a thread at a higher level runs whenever it is ready, and
/// lower levels only run when nothing above them can. That is the right default
/// for an interactive desktop — input and compositing must not wait behind a
/// batch job — and it is why `Background` exists as a level that can be starved
/// without anyone minding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[repr(u8)]
pub enum Priority {
    /// Background work: indexing, cleanup, telemetry.
    Background = 0,
    /// Ordinary threads.
    Normal = 1,
    /// Threads a person is waiting on: input, compositing, audio.
    Interactive = 2,
    /// Kernel work that must not be delayed by ordinary threads.
    Realtime = 3,
}

impl Priority {
    /// Number of distinct priority levels.
    pub const COUNT: usize = 4;

    /// This priority as a run-queue index.
    #[must_use]
    pub const fn index(self) -> usize {
        self as usize
    }
}

/// What a thread is currently doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThreadState {
    /// On a run queue, waiting for a processor.
    Ready,
    /// Currently executing.
    Running,
    /// Waiting until the tick counter reaches this value.
    Sleeping { until_tick: u64 },
    /// Finished. Its stack is reclaimed and it will never run again.
    Finished,
}

/// A kernel thread stack: mapped pages plus the physical block behind them.
pub struct KernelStack {
    /// Lowest mapped address. The guard region sits immediately below.
    bottom: u64,
    /// Highest address, where the stack pointer starts.
    top: u64,
    /// Physical block backing the stack, kept so it can be freed.
    physical: u64,
    /// Buddy order of `physical`.
    order: usize,
}

impl KernelStack {
    /// Map a new stack into slot `index` of the kernel stack area.
    ///
    /// Returns `None` if physical memory or page tables are exhausted.
    pub fn allocate(index: u64) -> Option<Self> {
        let frames = layout::KERNEL_STACK_SIZE / 4096;
        let order = nexus_mm::order_for_frames(frames)?;
        let physical = memory::allocate_block(order)?;

        // Only the upper part of the slot is mapped; the rest is the guard.
        let slot = layout::KERNEL_STACK_AREA_BASE + index * layout::KERNEL_STACK_STRIDE;
        let bottom = slot + (layout::KERNEL_STACK_STRIDE - layout::KERNEL_STACK_SIZE);
        let top = bottom + layout::KERNEL_STACK_SIZE;

        // SAFETY: `physical` is a block this stack now owns, and the slot is
        // inside the stack area, which nothing else maps.
        let mapped = unsafe {
            paging::map_range(
                bottom,
                physical,
                layout::KERNEL_STACK_SIZE,
                paging::WRITABLE | paging::NO_EXECUTE | paging::GLOBAL,
            )
        };
        if mapped.is_err() {
            // SAFETY: the block was allocated just above and never used.
            unsafe { memory::free_block(physical, order) };
            return None;
        }

        Some(Self {
            bottom,
            top,
            physical,
            order,
        })
    }

    /// Highest address of the stack, where a fresh stack pointer starts.
    #[must_use]
    pub fn top(&self) -> u64 {
        self.top
    }

    /// Lowest mapped address.
    #[must_use]
    pub fn bottom(&self) -> u64 {
        self.bottom
    }
}

impl Drop for KernelStack {
    fn drop(&mut self) {
        // Unmap first, then free: releasing the frames while they are still
        // mapped would let the next owner of those frames be written through
        // this stack's stale mapping.
        let pages = layout::KERNEL_STACK_SIZE / 4096;
        for page in 0..pages {
            // SAFETY: this stack is being destroyed, so nothing is running on
            // it — a thread never drops its own stack while using it.
            unsafe {
                let _ = paging::unmap_page(self.bottom + page * 4096);
            }
        }
        // SAFETY: the block came from `allocate_block` at this order and the
        // mappings that referred to it are gone.
        unsafe { memory::free_block(self.physical, self.order) };
    }
}

/// The function a thread runs.
pub type ThreadEntry = fn(usize);

/// A kernel thread.
pub struct Thread {
    pub id: ThreadId,
    pub name: String,
    pub state: ThreadState,
    pub priority: Priority,
    /// Saved stack pointer while this thread is not running.
    pub stack_pointer: u64,
    /// The thread's stack. `None` for the boot thread, which runs on the stack
    /// the bootloader set up rather than one this module allocated.
    pub stack: Option<KernelStack>,
    /// Entry point and argument, read once by the trampoline.
    pub entry: Option<(ThreadEntry, usize)>,
    /// Remaining ticks in the current time slice.
    pub slice_remaining: u32,
    /// Total ticks this thread has been scheduled for.
    pub ticks_run: u64,
    /// How many times it has been switched to.
    pub switches: u64,
}

/// Ticks a thread runs before the scheduler considers preempting it.
///
/// At a 1000 Hz tick this is 10 ms: long enough that switching overhead is
/// negligible, short enough that a runaway loop cannot make the system feel
/// unresponsive.
pub const TIME_SLICE_TICKS: u32 = 10;

impl Thread {
    /// Create the thread representing the context the kernel booted on.
    ///
    /// It owns no stack: it is already running on the bootloader's, and that
    /// stack must outlive everything.
    pub fn boot_thread(id: ThreadId, name: &str) -> Box<Self> {
        Box::new(Self {
            id,
            name: String::from(name),
            state: ThreadState::Running,
            priority: Priority::Normal,
            stack_pointer: 0,
            stack: None,
            entry: None,
            slice_remaining: TIME_SLICE_TICKS,
            ticks_run: 0,
            switches: 0,
        })
    }

    /// Create a thread that will start at `entry(argument)`.
    ///
    /// `stack_index` selects the slot in the kernel stack area.
    pub fn new(
        id: ThreadId,
        name: &str,
        priority: Priority,
        entry: ThreadEntry,
        argument: usize,
        stack_index: u64,
    ) -> Option<Box<Self>> {
        let stack = KernelStack::allocate(stack_index)?;
        let stack_pointer = Self::prepare_stack(&stack);

        Some(Box::new(Self {
            id,
            name: String::from(name),
            state: ThreadState::Ready,
            priority,
            stack_pointer,
            stack: Some(stack),
            entry: Some((entry, argument)),
            slice_remaining: TIME_SLICE_TICKS,
            ticks_run: 0,
            switches: 0,
        }))
    }

    /// Fabricate the stack a first switch into this thread will pop.
    ///
    /// The layout must mirror [`super::context::context_switch`] exactly: it
    /// pops flags, then `r15` through `rbp`, then returns. So from the stack
    /// pointer upward this writes flags, six zeroed registers, and the address
    /// of the trampoline in the slot `ret` will take.
    ///
    /// The trampoline slot is deliberately 16-byte aligned. After `ret` pops
    /// it, the stack pointer is 8 modulo 16, which is exactly the state the
    /// SysV ABI defines at function entry — so the trampoline and everything it
    /// calls see a correctly aligned stack.
    fn prepare_stack(stack: &KernelStack) -> u64 {
        let top = stack.top() & !0xF;

        // One slot of zero above the return address terminates the call chain,
        // so a backtrace stops cleanly at the thread's entry instead of walking
        // off the top of the stack.
        let terminator = top - 8;
        let return_slot = top - 16;
        debug_assert!(return_slot % 16 == 0);

        // SAFETY: the whole range lies inside the stack just mapped, and no
        // thread is running on it yet.
        unsafe {
            (terminator as *mut u64).write(0);
            (return_slot as *mut u64).write(super::thread_trampoline as *const () as u64);

            // Six callee-saved registers, then the flags word, descending.
            for slot in 1..=6u64 {
                ((return_slot - slot * 8) as *mut u64).write(0);
            }
            let flags_slot = return_slot - 7 * 8;
            (flags_slot as *mut u64).write(INITIAL_RFLAGS);

            debug_assert_eq!(
                (return_slot - flags_slot) / 8,
                SAVED_REGISTER_COUNT as u64,
                "the fabricated frame must match what context_switch pops"
            );

            flags_slot
        }
    }

    /// Whether this thread is waiting for a deadline that has now passed.
    #[must_use]
    pub fn is_wakeable(&self, now: u64) -> bool {
        matches!(self.state, ThreadState::Sleeping { until_tick } if now >= until_tick)
    }
}
