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

/// Vector the local APIC reports spurious interrupts on.
///
/// The architecture requires the low four bits to be set on some older
/// processors, and 0xFF satisfies that on every one.
pub const SPURIOUS_VECTOR: u8 = 0xFF;

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
extern "x86-interrupt" fn pit_interrupt(_frame: InterruptStackFrame) {
    let driving = time::is_source(TimerSource::Pit);
    if driving {
        time::on_tick();
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
/// but only the boot processor advances the clock and drives the scheduler.
///
/// That restriction is deliberate and temporary. The scheduler still keeps a
/// single global notion of the running thread, so a second processor entering
/// it would pick a thread the first is already running and context-switch into
/// it — two cores on one stack, which is exactly what happened the first time
/// the application processors were allowed through: a page fault within
/// milliseconds. Four processors advancing one clock would also make time run
/// four times too fast.
///
/// Lifting this needs a per-processor current thread and run queue, which is
/// its own piece of work. Until then the other processors keep their timers
/// running, count their interrupts, and idle — which at least proves they are
/// alive and taking interrupts.
extern "x86-interrupt" fn apic_timer_interrupt(_frame: InterruptStackFrame) {
    let is_boot_processor = super::percpu::cpu_index() == 0;

    // Counted on every processor, including the boot one: the count is how a
    // wedged core is spotted, and a core that is excluded from the count cannot
    // be seen to have stopped.
    if super::percpu::is_installed() {
        // SAFETY: this processor installed its own state before enabling
        // interrupts, and only it ever writes this field.
        unsafe { super::percpu::current().interrupt_count += 1 };
    }

    if is_boot_processor {
        time::on_tick();
        crate::sched::tick();
    }

    // SAFETY: called exactly once, from the handler for this vector.
    unsafe { apic::end_of_interrupt() };

    if is_boot_processor && crate::sched::needs_reschedule() {
        preempt();
    }
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

/// The local APIC's spurious interrupt.
///
/// Counted, never acknowledged: the APIC raises no in-service bit for it, so an
/// end-of-interrupt here would clear a different interrupt that is genuinely
/// pending.
extern "x86-interrupt" fn apic_spurious_interrupt(_frame: InterruptStackFrame) {
    apic::on_spurious();
}

/// The PIC's spurious interrupt, raised when a line drops before it is
/// acknowledged.
///
/// It is counted rather than reported: a handful over a long uptime is normal
/// electrical behaviour, and treating it as a fault would be wrong. A steadily
/// climbing count, on the other hand, points at a real problem, which is why
/// the number is kept.
extern "x86-interrupt" fn spurious_interrupt(_frame: InterruptStackFrame) {
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
        idt.set_handler(SPURIOUS_VECTOR, apic_spurious_interrupt as *const ());

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
