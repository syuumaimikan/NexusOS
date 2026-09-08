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
    /// The thread this processor switched away from and has not yet released.
    ///
    /// [`NO_THREAD`] when there is none. A thread cannot complete its own
    /// departure: between announcing where it is going and the stack switch
    /// that takes it there it is still executing, so another processor acting
    /// on the announcement would be acting on a thread that has not stopped.
    /// The *next* thread to run here finishes the job, by which point the
    /// outgoing one has provably left.
    pub previous: u64,
    /// Interrupts handled here, for diagnostics.
    pub interrupt_count: u64,
    /// Whether this processor has finished starting.
    pub online: bool,
    /// Set when the thread running here should be preempted at the first
    /// opportunity. Per-processor because the answer differs per processor.
    pub needs_reschedule: bool,
    _padding: [u8; 6],
    /// Top of the kernel stack a `syscall` from ring 3 should switch to.
    ///
    /// The running thread's own, not the processor's: a system call has to be
    /// able to block, and a thread that blocks on a stack shared with the
    /// processor would be resumed on top of whatever ran there in between.
    /// Updated by the scheduler on every switch.
    pub syscall_stack_top: u64,
    /// Where the entry stub parks the user stack pointer for the duration.
    pub user_stack_pointer: u64,
}

/// Byte offsets the `syscall` entry stub reaches through `gs:`.
///
/// Assembly cannot ask Rust for a field offset, so these are written out and
/// then checked against the real layout below. Getting one wrong would not fail
/// to compile; it would switch to a stack that is not a stack.
pub mod offset {
    /// [`PerCpu::syscall_stack_top`].
    pub const SYSCALL_STACK_TOP: usize = 64;
    /// [`PerCpu::user_stack_pointer`].
    pub const USER_STACK_POINTER: usize = 72;
}

const _: () = {
    assert!(core::mem::offset_of!(PerCpu, self_pointer) == 0);
    assert!(core::mem::offset_of!(PerCpu, syscall_stack_top) == offset::SYSCALL_STACK_TOP);
    assert!(core::mem::offset_of!(PerCpu, user_stack_pointer) == offset::USER_STACK_POINTER);
};

/// The thread fields' "nothing here" value.
///
/// Not zero: thread zero is the context the kernel booted on, and it is a real
/// thread that can be switched away from like any other.
pub const NO_THREAD: u64 = u64::MAX;

impl PerCpu {
    const fn new() -> Self {
        Self {
            self_pointer: 0,
            cpu_index: 0,
            apic_id: 0,
            current_thread: NO_THREAD,
            idle_thread: NO_THREAD,
            kernel_stack_top: 0,
            previous: NO_THREAD,
            interrupt_count: 0,
            online: false,
            needs_reschedule: false,
            _padding: [0; 6],
            syscall_stack_top: 0,
            user_stack_pointer: 0,
        }
    }
}

/// Per-processor state, one entry per possible processor.
///
/// Aligned to a cache line by `PerCpu` itself, so two processors updating their
/// own state never contend for the same line.
static mut PER_CPU: [PerCpu; MAX_PROCESSORS] = [const { PerCpu::new() }; MAX_PROCESSORS];

/// Where the accessors below point before `GS` is set, and if it never is.
///
/// Per-processor state is installed only once the local APIC is up, because a
/// processor's identifier is not known before then, and the APIC can fail to
/// come up at all. Everything between kernel entry and that point — and the
/// whole of a uniprocessor fallback — still needs somewhere to keep a current
/// thread. One extra slot is cheaper than making every caller handle the
/// possibility of there being nowhere to write.
static mut FALLBACK: PerCpu = PerCpu::new();

/// This processor's state, or the fallback slot before `GS` is set.
#[inline]
fn this() -> *mut PerCpu {
    if is_installed() {
        // SAFETY: `is_installed` confirmed the base is set, and `current`
        // resolves it through this processor's own `GS`.
        unsafe { core::ptr::from_mut(current()) }
    } else {
        core::ptr::addr_of_mut!(FALLBACK)
    }
}

/// The thread running on this processor.
#[inline]
#[must_use]
pub fn current_thread() -> u64 {
    // SAFETY: `this` returns a live slot, and only this processor writes it.
    unsafe { (*this()).current_thread }
}

/// Record the thread now running on this processor.
pub fn set_current_thread(id: u64) {
    // SAFETY: as above.
    unsafe { (*this()).current_thread = id };
}

/// The thread this processor falls back to when nothing is ready.
#[inline]
#[must_use]
pub fn idle_thread() -> u64 {
    // SAFETY: as above.
    unsafe { (*this()).idle_thread }
}

/// Record this processor's idle thread.
pub fn set_idle_thread(id: u64) {
    // SAFETY: as above.
    unsafe { (*this()).idle_thread = id };
}

/// Take the thread this processor switched away from, leaving nothing behind.
pub fn take_previous() -> Option<u64> {
    // SAFETY: as above.
    unsafe {
        let slot = this();
        let id = (*slot).previous;
        (*slot).previous = NO_THREAD;
        (id != NO_THREAD).then_some(id)
    }
}

/// Note that `id` is leaving this processor and still has to be released.
///
/// The slot holds at most one thread. Overwriting an occupied one loses that
/// thread silently -- it stays ready, on no run queue, and never runs again --
/// so the case is asserted rather than trusted. It has happened, when a new
/// thread began with interrupts already enabled and was preempted before it
/// could release its predecessor.
pub fn set_previous(id: u64) {
    // SAFETY: as above.
    unsafe {
        debug_assert_eq!(
            (*this()).previous,
            NO_THREAD,
            "a thread was still awaiting release when another switched away"
        );
        (*this()).previous = id;
    }
}

/// Whether the thread running here should be preempted.
#[inline]
#[must_use]
pub fn needs_reschedule() -> bool {
    // SAFETY: as above.
    unsafe { (*this()).needs_reschedule }
}

/// Top of the stack this processor entered the kernel on.
#[inline]
#[must_use]
pub fn kernel_stack_top() -> u64 {
    // SAFETY: as above.
    unsafe { (*this()).kernel_stack_top }
}

/// Record the kernel stack a `syscall` arriving on this processor should use.
pub fn set_syscall_stack_top(top: u64) {
    // SAFETY: as above.
    unsafe { (*this()).syscall_stack_top = top };
}

/// Set or clear this processor's preemption request.
pub fn set_needs_reschedule(value: bool) {
    // SAFETY: as above.
    unsafe { (*this()).needs_reschedule = value };
}

/// Ask every online processor to reschedule at its next opportunity.
///
/// Used when a thread becomes runnable: whichever processor is idling should
/// pick it up, and the one that woke it has no way of knowing which that is.
///
/// A plain store into another processor's slot rather than an inter-processor
/// interrupt. The target notices at its next timer tick, so the latency is one
/// millisecond, and the failure modes of a racing store are an extra trip
/// through the scheduler or a request that arrives a tick late — neither of
/// which is a correctness problem. An IPI would cost a delivery and an
/// acknowledgement on every wake to save that millisecond, which is not a
/// trade worth making until something is measured that cares.
pub fn request_reschedule_everywhere() {
    for index in 0..MAX_PROCESSORS {
        // SAFETY: the slot exists for the life of the kernel. Writing another
        // processor's flag races with that processor's own writes, and a
        // single-byte store cannot tear; the value is a hint either way.
        unsafe {
            let slot = core::ptr::addr_of_mut!(PER_CPU[index]);
            if (*slot).online {
                (*slot).needs_reschedule = true;
            }
        }
    }
}

/// Number of processors that have come online.
static ONLINE_COUNT: AtomicUsize = AtomicUsize::new(0);

/// `IA32_GS_BASE`, the segment base `gs:` offsets are taken from.
const IA32_GS_BASE: u32 = 0xC000_0101;

/// `IA32_KERNEL_GS_BASE`, the value `swapgs` exchanges with the active one.
const IA32_KERNEL_GS_BASE: u32 = 0xC000_0102;

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
        // Both bases, not just the active one. User code can zero `GS.base`
        // simply by loading a segment selector, so the kernel cannot rely on
        // it surviving a trip through ring 3; what it relies on is the
        // *inactive* base still holding this processor's state, which is what
        // `swapgs` brings back on entry. Setting both here means the very
        // first entry from user mode has something correct to swap in.
        write_kernel_gs_base(slot as u64);
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

/// Set `IA32_KERNEL_GS_BASE`.
///
/// # Safety
///
/// `base` must point at a live [`PerCpu`] whose self-pointer is correct.
unsafe fn write_kernel_gs_base(base: u64) {
    // SAFETY: upheld by the caller.
    unsafe {
        core::arch::asm!(
            "wrmsr",
            in("ecx") IA32_KERNEL_GS_BASE,
            in("eax") base as u32,
            in("edx") (base >> 32) as u32,
            options(nomem, nostack, preserves_flags),
        );
    }
}

/// Exchange `IA32_GS_BASE` with `IA32_KERNEL_GS_BASE`.
///
/// # Safety
///
/// Must be paired. Executing this an odd number of times between entering and
/// leaving the kernel leaves `GS` pointing at the wrong thing, which is not a
/// fault but a silent read of another address space's idea of per-CPU state.
#[inline]
pub unsafe fn swap_gs() {
    // SAFETY: upheld by the caller.
    unsafe {
        core::arch::asm!("swapgs", options(nomem, nostack, preserves_flags));
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
