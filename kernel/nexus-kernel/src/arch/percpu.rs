//! Per-processor state.
//!
//! Once more than one processor is running, "the current thread" stops being a
//! single global fact. Each core needs its own answer, reachable without a lock
//! and without knowing its own index — an interrupt handler cannot afford to
//! look either up.
//!
//! The mechanism is the `GS` segment base. `GS` is set on each processor to
//! point at that processor's [`PerCpu`], so `gs:[0]` is always this core's
//! state, in one instruction, from anywhere. The first field is a pointer back
//! to the structure itself, which is what makes that work: a segment-relative
//! load can only produce a value, and what is wanted is an address.
//!
//! `GS` rather than `FS` because the architecture reserves `swapgs` for exactly
//! this, and it is how the kernel will recover its own per-CPU pointer on entry
//! from user mode later.

use core::sync::atomic::{AtomicUsize, Ordering};

/// Largest number of processors the kernel will bring up.
///
/// Fixed rather than dynamic so that per-processor state is statically
/// allocated and available before the heap is: the boot processor needs its own
/// state during early bring-up, and an application processor needs its state
/// before it can safely take a lock.
///
/// Sixteen is ample for the desktops NexusOS targets, and the number matters
/// because the descriptor tables reserve Interrupt Stack Table storage for
/// every possible processor whether or not it exists.
pub const MAX_PROCESSORS: usize = 16;

/// State private to one processor.
///
/// `repr(C)` and the self-pointer first: both are load-bearing, since assembly
/// reaches this through `gs:[0]`.
#[repr(C, align(64))]
pub struct PerCpu {
    /// Address of this structure. Must stay at offset zero.
    pub self_pointer: u64,
    /// Dense index, 0 for the boot processor.
    pub cpu_index: u32,
    /// This processor's local APIC identifier.
    pub apic_id: u32,
    /// Identifier of the thread running here, mirrored out of the scheduler so
    /// an interrupt handler can read it without a lock.
    pub current_thread: u64,
    /// The idle thread this processor falls back to.
    pub idle_thread: u64,
    /// Top of the stack this processor entered the kernel on.
    pub kernel_stack_top: u64,
    /// Interrupts handled here, for diagnostics.
    pub interrupt_count: u64,
    /// Whether this processor has finished starting.
    pub online: bool,
    _padding: [u8; 7],
}

impl PerCpu {
    const fn new() -> Self {
        Self {
            self_pointer: 0,
            cpu_index: 0,
            apic_id: 0,
            current_thread: 0,
            idle_thread: 0,
            kernel_stack_top: 0,
            interrupt_count: 0,
            online: false,
            _padding: [0; 7],
        }
    }
}

/// Per-processor state, one entry per possible processor.
///
/// Aligned to a cache line by `PerCpu` itself, so two processors updating their
/// own state never contend for the same line.
static mut PER_CPU: [PerCpu; MAX_PROCESSORS] = [const { PerCpu::new() }; MAX_PROCESSORS];

/// Number of processors that have come online.
static ONLINE_COUNT: AtomicUsize = AtomicUsize::new(0);

/// `IA32_GS_BASE`, the segment base `gs:` offsets are taken from.
const IA32_GS_BASE: u32 = 0xC000_0101;

/// Prepare slot `index` and install it as this processor's `GS` base.
///
/// # Safety
///
/// Call once per processor, on that processor, before anything reads per-CPU
/// state. `index` must be unique across processors and below
/// [`MAX_PROCESSORS`].
pub unsafe fn install(index: usize, apic_id: u32, kernel_stack_top: u64) {
    debug_assert!(index < MAX_PROCESSORS);

    // SAFETY: each processor writes only its own slot, and the caller
    // guarantees the index is unique, so there is no aliasing between cores.
    let slot = unsafe { core::ptr::addr_of_mut!(PER_CPU[index]) };
    // SAFETY: as above.
    unsafe {
        (*slot).self_pointer = slot as u64;
        (*slot).cpu_index = index as u32;
        (*slot).apic_id = apic_id;
        (*slot).kernel_stack_top = kernel_stack_top;
        (*slot).online = true;

        write_gs_base(slot as u64);
    }

    ONLINE_COUNT.fetch_add(1, Ordering::Release);
}

/// Set `IA32_GS_BASE`.
///
/// # Safety
///
/// `base` must point at a live [`PerCpu`] whose self-pointer is correct.
unsafe fn write_gs_base(base: u64) {
    // SAFETY: upheld by the caller.
    unsafe {
        core::arch::asm!(
            "wrmsr",
            in("ecx") IA32_GS_BASE,
            in("eax") base as u32,
            in("edx") (base >> 32) as u32,
            options(nomem, nostack, preserves_flags),
        );
    }
}

/// This processor's state.
///
/// # Safety
///
/// [`install`] must have run on this processor. Calling before that reads
/// through a `GS` base of zero and dereferences a null pointer.
#[inline]
pub unsafe fn current() -> &'static mut PerCpu {
    let pointer: u64;
    // SAFETY: `gs:[0]` is the self-pointer that `install` wrote. The exclusive
    // reference is sound because each processor only ever reaches its own slot
    // through its own `GS` base.
    unsafe {
        core::arch::asm!(
            "mov {}, gs:[0]",
            out(reg) pointer,
            options(nostack, preserves_flags, readonly),
        );
        &mut *(pointer as *mut PerCpu)
    }
}

/// Whether per-CPU state is installed on this processor.
///
/// Checked rather than assumed on paths that can run before bring-up finishes,
/// such as an early exception.
#[inline]
#[must_use]
pub fn is_installed() -> bool {
    let base: u64;
    // SAFETY: reading an MSR that always exists on x86-64.
    unsafe {
        let (low, high): (u32, u32);
        core::arch::asm!(
            "rdmsr",
            in("ecx") IA32_GS_BASE,
            out("eax") low,
            out("edx") high,
            options(nomem, nostack, preserves_flags),
        );
        base = (u64::from(high) << 32) | u64::from(low);
    }
    base != 0
}

/// This processor's index, or 0 before per-CPU state is installed.
#[inline]
#[must_use]
pub fn cpu_index() -> u32 {
    if !is_installed() {
        return 0;
    }
    // SAFETY: `is_installed` just confirmed the base is set.
    unsafe { current().cpu_index }
}

/// Processors that have come online.
#[must_use]
pub fn online_count() -> usize {
    ONLINE_COUNT.load(Ordering::Acquire)
}

/// Read another processor's state.
///
/// Used for reporting. The values are read without synchronisation, so they may
/// be a moment stale; nothing here makes a decision on them.
#[must_use]
pub fn snapshot(index: usize) -> Option<(u32, u64, bool)> {
    if index >= MAX_PROCESSORS {
        return None;
    }
    // SAFETY: the slot exists for the life of the kernel, and every field read
    // here is a plain integer, so a concurrent write cannot tear one.
    let slot = unsafe { &*core::ptr::addr_of!(PER_CPU[index]) };
    if !slot.online {
        return None;
    }
    Some((slot.apic_id, slot.interrupt_count, slot.online))
}
