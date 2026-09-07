//! Interrupt enable state, IRQ handlers, and the IDT the kernel runs on.

use core::sync::atomic::{AtomicU64, Ordering};

use super::idt::{InterruptDescriptorTable, InterruptStackFrame};
use super::{exceptions, gdt, pic, pit};
use crate::kprintln;

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

/// The periodic timer.
///
/// Deliberately does almost nothing: this is the highest-frequency code in the
/// kernel, and it runs with interrupts masked. Once the scheduler exists, this
/// is where preemption is triggered.
extern "x86-interrupt" fn timer_interrupt(_frame: InterruptStackFrame) {
    pit::on_tick();
    crate::sched::tick();

    // Acknowledge before any possible context switch. If the switch happened
    // first, the controller would still be holding this interrupt in service
    // and would deliver nothing further until this thread ran again -- which,
    // for a thread that never becomes runnable, is never.
    // SAFETY: called exactly once, from the handler for this vector.
    unsafe { pic::end_of_interrupt(pic::TIMER_VECTOR) };

    // Preempt if the running thread has used up its slice. This is safe from
    // inside the handler because the handler runs on the interrupted thread's
    // own kernel stack: the `iretq` frame stays there with the saved registers,
    // so resuming the thread later returns here and then returns from the
    // interrupt exactly where it left off.
    if crate::sched::needs_reschedule() {
        crate::sched::schedule();
    }
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
        gdt::init();

        let idt = &mut *core::ptr::addr_of_mut!(IDT);
        exceptions::install(idt);
        idt.set_handler(pic::TIMER_VECTOR, timer_interrupt as *const ());
        // Vector 7 of the primary PIC is where a dropped line surfaces.
        idt.set_handler(
            pic::PRIMARY_VECTOR_BASE + 7,
            spurious_interrupt as *const (),
        );

        // `load` needs a `&'static` table; `IDT` is a static, and it is never
        // mutated again after this point.
        (*core::ptr::addr_of!(IDT)).load();

        pic::init();
        let actual_hz = pit::init(timer_hz);
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
