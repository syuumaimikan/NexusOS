//! Interrupt enable state, IRQ handlers, and the IDT the kernel runs on.

use core::sync::atomic::{AtomicU64, Ordering};

use super::idt::{InterruptDescriptorTable, InterruptStackFrame};
use super::time::{self, TimerSource};
use super::{apic, exceptions, gdt, pic, pit};
use crate::kprintln;

/// Vector the local APIC timer is delivered on.
///
/// Above the sixteen the 8259 was remapped onto, so both controllers can be
/// live during the handover without either landing on the other's handler.
pub const APIC_TIMER_VECTOR: u8 = super::idt::IRQ_BASE + 16;

/// Vector the keyboard is routed to through the I/O APIC.
///
/// Above the legacy range and clear of the APIC timer, so nothing has to be
/// unrouted before this can be used.
pub const KEYBOARD_VECTOR: u8 = super::idt::IRQ_BASE + 17;

/// Vector the disk is routed to through the I/O APIC.
///
/// Above the keyboard, for no reason but that it was added later: the two are
/// independent and the numbers only have to differ.
pub const DISK_VECTOR: u8 = super::idt::IRQ_BASE + 18;

/// Vector the mouse is routed to through the I/O APIC.
pub const MOUSE_VECTOR: u8 = super::idt::IRQ_BASE + 19;

/// Vector the local APIC reports spurious interrupts on.
///
/// The architecture requires the low four bits to be set on some older
/// processors, and 0xFF satisfies that on every one.
pub const SPURIOUS_VECTOR: u8 = 0xFF;

/// The TLB shootdown inter-processor interrupt.
///
/// Above the device vectors and below the spurious one. Priority matters here:
/// on x86 a higher vector number is higher priority, and a processor that is
/// slow to answer a shootdown holds up the one that sent it, so this sits above
/// the timer and the keyboard rather than queueing behind them.
pub const TLB_SHOOTDOWN_VECTOR: u8 = 0xFE;

/// The kernel's interrupt descriptor table.
///
/// A single table shared by every processor. Per-CPU tables buy nothing while
/// all cores run the same handlers, and the IST stacks that do need to be
/// per-CPU live in the TSS rather than here.
static mut IDT: InterruptDescriptorTable = InterruptDescriptorTable::new();

/// Count of interrupts delivered on each vector, for diagnostics.
static SPURIOUS_COUNT: AtomicU64 = AtomicU64::new(0);

/// Whether interrupts are currently enabled on this processor.
#[inline]
#[must_use]
pub fn are_enabled() -> bool {
    let flags: u64;
    // SAFETY: reading `rflags` has no side effects. `pushfq` writes to the
    // stack, so this cannot claim `nostack`.
    unsafe {
        core::arch::asm!("pushfq; pop {}", out(reg) flags, options(nomem, preserves_flags));
    }
    // Bit 9 is the interrupt flag.
    flags & (1 << 9) != 0
}

/// Enable interrupt delivery on this processor.
#[inline]
pub fn enable() {
    // SAFETY: `sti` is valid in kernel mode. The IDT is loaded before this is
    // ever called, so there is somewhere for an interrupt to go.
    unsafe {
        core::arch::asm!("sti", options(nomem, nostack));
    }
}

/// Disable interrupt delivery on this processor.
#[inline]
pub fn disable() {
    // SAFETY: `cli` is valid in kernel mode.
    unsafe {
        core::arch::asm!("cli", options(nomem, nostack));
    }
}

/// Run `f` with interrupts disabled, restoring the previous state afterwards.
///
/// Saving and restoring rather than unconditionally re-enabling is what makes
/// this safe to nest: an inner call must not enable interrupts that an outer
/// one deliberately disabled.
pub fn without_interrupts<F, R>(f: F) -> R
where
    F: FnOnce() -> R,
{
    let were_enabled = are_enabled();
    if were_enabled {
        disable();
    }
    let result = f();
    if were_enabled {
        enable();
    }
    result
}

/// The legacy PIT tick.
///
/// Once the local APIC timer takes over, this stops advancing the clock rather
/// than being unhooked. Unhooking would leave a window in which an interrupt
/// already in flight lands on a vector with no handler; falling silent means a
/// straggler is acknowledged and ignored, and the clock is never advanced twice
/// for the same instant.
extern "x86-interrupt" fn pit_interrupt(frame: InterruptStackFrame) {
    // Entered from ring 3 as readily as from the kernel, and a device
    // interrupt is the likeliest of all of them to land on user code.
    let _gs = super::idt::KernelGs::enter(&frame);
    let driving = time::is_source(TimerSource::Pit);
    if driving {
        time::on_tick();
        crate::sched::wake_sleepers();
        crate::sched::tick();
    }

    // Acknowledge before any possible context switch. If the switch happened
    // first, the controller would still be holding this interrupt in service
    // and would deliver nothing further until this thread ran again -- which,
    // for a thread that never becomes runnable, is never.
    // SAFETY: called exactly once, from the handler for this vector.
    unsafe { pic::end_of_interrupt(pic::TIMER_VECTOR) };

    if driving && crate::sched::needs_reschedule() {
        preempt();
    }
}

/// The local APIC timer tick, which is the scheduling tick once it is running.
///
/// Every processor has its own APIC timer and every one of them arrives here,
/// and every one of them schedules. What only the boot processor does is
/// advance the clock: four processors advancing one counter would make time run
/// four times too fast, and there is one clock because there is one system.
///
/// Waking sleepers is tied to the clock for the same reason — the processor
/// that moved time forward is the one that can know a deadline has passed —
/// while charging the tick to a thread and deciding to preempt it are per
/// processor, because the thread being charged is.
extern "x86-interrupt" fn apic_timer_interrupt(frame: InterruptStackFrame) {
    // Entered from ring 3 as readily as from the kernel, and a device
    // interrupt is the likeliest of all of them to land on user code.
    let _gs = super::idt::KernelGs::enter(&frame);
    // Counted on every processor, including the boot one: the count is how a
    // wedged core is spotted, and a core that is excluded from the count cannot
    // be seen to have stopped.
    if super::percpu::is_installed() {
        // SAFETY: this processor installed its own state before enabling
        // interrupts, and only it ever writes this field.
        unsafe { super::percpu::current().interrupt_count += 1 };
    }

    if super::percpu::cpu_index() == 0 {
        time::on_tick();
        crate::sched::wake_sleepers();
    }
    crate::sched::tick();

    // SAFETY: called exactly once, from the handler for this vector.
    unsafe { apic::end_of_interrupt() };

    // A program that makes no system calls and never waits cannot be stopped at
    // the system-call boundary, because it never reaches one. This is the other
    // place it can be made to notice: the timer arrives whether a program asks
    // for anything or not, so a loop that touches nothing is interrupted here
    // several hundred times a second and can be told to leave at any of them.
    //
    // Only when the interrupt came from ring 3. Kernel code is not asked to
    // stop -- there is no process to have been asked -- and a check that fired
    // in kernel context would be stopping a thread part-way through whatever
    // the kernel was doing on its behalf, which is the thing this whole
    // arrangement exists to avoid.
    //
    // Safe here for the same reason `preempt` is: the handler runs on the
    // interrupted thread's own kernel stack. The `GS` guard above is not
    // dropped, and must not be -- this never returns to ring 3, so the kernel's
    // base is the one that should stay.
    if frame.code_segment & 3 == 3 {
        crate::sched::stop_if_asked();
    }

    if crate::sched::needs_reschedule() {
        preempt();
    }
}

/// Another processor changed a mapping and needs this one's TLB brought up to
/// date before it can consider the change complete.
///
/// The handler is only half of how a request arrives: a processor spinning for
/// a lock has interrupts masked and would never take this, so the same mailbox
/// is polled from every spin loop. See [`super::tlb`].
extern "x86-interrupt" fn tlb_shootdown_interrupt(frame: InterruptStackFrame) {
    // Entered from ring 3 as readily as from the kernel, and a device
    // interrupt is the likeliest of all of them to land on user code.
    let _gs = super::idt::KernelGs::enter(&frame);
    super::tlb::on_interrupt();

    // Acknowledged after the invalidation, not before: the sender is waiting on
    // the mailbox rather than on this, but an early acknowledgement would let a
    // second request arrive mid-flush for no benefit.
    // SAFETY: called exactly once, from the handler for this vector.
    unsafe { apic::end_of_interrupt() };
}

/// Hand the processor to another thread from inside a timer handler.
///
/// Safe to do here because the handler runs on the interrupted thread's own
/// kernel stack: the `iretq` frame stays there with the saved registers, so
/// resuming the thread later returns to this point and then returns from the
/// interrupt exactly where it left off. It is also why neither timer vector may
/// use an Interrupt Stack Table slot.
#[inline]
fn preempt() {
    crate::sched::schedule();
}

/// The keyboard.
///
/// Reads one scancode into a queue and acknowledges. Decoding happens on a
/// thread: an interrupt handler runs with interrupts masked on this processor,
/// and decoding needs modifier state that a handler has no business locking.
extern "x86-interrupt" fn keyboard_interrupt(frame: InterruptStackFrame) {
    // Entered from ring 3 as readily as from the kernel, and a device
    // interrupt is the likeliest of all of them to land on user code.
    let _gs = super::idt::KernelGs::enter(&frame);
    // SAFETY: called only as the handler for this vector, and the read of the
    // controller's output buffer is what clears its interrupt.
    unsafe {
        crate::drivers::keyboard::on_interrupt();
        apic::end_of_interrupt();
    }
}

/// The mouse.
///
/// Reads one byte and, when three of them make a packet, sends it on. The
/// controller is shared with the keyboard, so the handler checks whose byte it
/// is before taking it -- reading the other device's would take it away from
/// the driver whose it is.
extern "x86-interrupt" fn mouse_interrupt(frame: InterruptStackFrame) {
    // Entered from ring 3 as readily as from the kernel, and a device
    // interrupt is the likeliest of all of them to land on user code.
    let _gs = super::idt::KernelGs::enter(&frame);
    // SAFETY: called only as the handler for this vector.
    unsafe {
        crate::drivers::mouse::on_interrupt();
        apic::end_of_interrupt();
    }
}

/// The disk's interrupt.
///
/// Acknowledges at the device -- which is a register read, and the only thing
/// that stops a level-triggered line raising again immediately -- and wakes
/// whoever was waiting for the request to finish. The waiting thread does the
/// rest: an interrupt handler runs with interrupts masked on this processor,
/// and copying half a kilobyte out of a scratch page is not its work.
extern "x86-interrupt" fn disk_interrupt(frame: InterruptStackFrame) {
    // Entered from ring 3 as readily as from the kernel, and a device
    // interrupt is the likeliest of all of them to land on user code.
    let _gs = super::idt::KernelGs::enter(&frame);
    // SAFETY: called only as the handler for this vector.
    unsafe {
        crate::drivers::virtio_blk::on_interrupt();
        apic::end_of_interrupt();
    }
}

/// The local APIC's spurious interrupt.
///
/// Counted, never acknowledged: the APIC raises no in-service bit for it, so an
/// end-of-interrupt here would clear a different interrupt that is genuinely
/// pending.
extern "x86-interrupt" fn apic_spurious_interrupt(frame: InterruptStackFrame) {
    // Entered from ring 3 as readily as from the kernel, and a device
    // interrupt is the likeliest of all of them to land on user code.
    let _gs = super::idt::KernelGs::enter(&frame);
    apic::on_spurious();
}

/// The PIC's spurious interrupt, raised when a line drops before it is
/// acknowledged.
///
/// It is counted rather than reported: a handful over a long uptime is normal
/// electrical behaviour, and treating it as a fault would be wrong. A steadily
/// climbing count, on the other hand, points at a real problem, which is why
/// the number is kept.
extern "x86-interrupt" fn spurious_interrupt(frame: InterruptStackFrame) {
    // Entered from ring 3 as readily as from the kernel, and a device
    // interrupt is the likeliest of all of them to land on user code.
    let _gs = super::idt::KernelGs::enter(&frame);
    SPURIOUS_COUNT.fetch_add(1, Ordering::Relaxed);
    // A genuine spurious interrupt must *not* be acknowledged: the controller
    // never raised its in-service bit, so an EOI would clear a real interrupt
    // that is still pending.
}

/// Number of spurious interrupts seen since boot.
#[must_use]
pub fn spurious_count() -> u64 {
    SPURIOUS_COUNT.load(Ordering::Relaxed)
}

/// Load the kernel's interrupt descriptor table on this processor.
///
/// The table is shared; the register that points at it is not. A processor
/// that never loaded it would take the first interrupt against whatever the
/// trampoline left behind.
///
/// # Safety
///
/// [`init`] must have built the table already.
pub unsafe fn load_on_this_processor() {
    // SAFETY: the table is a static, built by `init`, and never mutated again.
    unsafe { (*core::ptr::addr_of!(IDT)).load() };
}

/// Install the GDT, the IDT and the interrupt controllers, then start the
/// timer at `timer_hz`.
///
/// Interrupts are left disabled; the caller enables them when the rest of the
/// kernel is ready to be interrupted.
///
/// # Safety
///
/// Call once, early, with interrupts disabled.
pub unsafe fn init(timer_hz: u32) {
    // SAFETY: single-threaded early boot with interrupts disabled, which is
    // what each of these requires.
    unsafe {
        gdt::init(0);

        let idt = &mut *core::ptr::addr_of_mut!(IDT);
        exceptions::install(idt);
        idt.set_handler(pic::TIMER_VECTOR, pit_interrupt as *const ());
        // Vector 7 of the primary PIC is where a dropped line surfaces.
        idt.set_handler(
            pic::PRIMARY_VECTOR_BASE + 7,
            spurious_interrupt as *const (),
        );
        // Registered now, before the APIC exists, so that the vectors are never
        // reachable-but-unhandled during the handover.
        idt.set_handler(APIC_TIMER_VECTOR, apic_timer_interrupt as *const ());
        idt.set_handler(KEYBOARD_VECTOR, keyboard_interrupt as *const ());
        idt.set_handler(DISK_VECTOR, disk_interrupt as *const ());
        idt.set_handler(MOUSE_VECTOR, mouse_interrupt as *const ());
        idt.set_handler(SPURIOUS_VECTOR, apic_spurious_interrupt as *const ());
        idt.set_handler(TLB_SHOOTDOWN_VECTOR, tlb_shootdown_interrupt as *const ());

        // `load` needs a `&'static` table; `IDT` is a static, and it is never
        // mutated again after this point.
        (*core::ptr::addr_of!(IDT)).load();

        pic::init();
        let actual_hz = pit::init(timer_hz);
        time::set_source(TimerSource::Pit, u64::from(actual_hz));
        pic::unmask(0);

        kprintln!("[intr] GDT, TSS and IDT installed ({} vectors)", 256);
        kprintln!(
            "[intr] PIC remapped to vectors {}..{}",
            pic::PRIMARY_VECTOR_BASE,
            pic::PRIMARY_VECTOR_BASE + 16
        );
        kprintln!(
            "[intr] PIT running at {actual_hz} Hz on vector {}",
            pic::TIMER_VECTOR
        );
    }
}
