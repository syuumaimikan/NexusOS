//! Context switching.
//!
//! # How a switch works
//!
//! [`context_switch`] saves the callee-saved registers and the flags of the
//! outgoing thread onto its own stack, records that stack pointer, loads the
//! incoming thread's, and pops its registers back. The caller-saved registers
//! need no attention: [`context_switch`] is an ordinary `sysv64` call, so the
//! compiler has already spilled anything live across it.
//!
//! The consequence is that a thread's entire saved state is one `u64` — its
//! stack pointer — and everything else lives on the stack it points at.
//!
//! # Switching from an interrupt handler
//!
//! The timer handler calls into the scheduler, which can switch away in the
//! middle of servicing the interrupt. That works, and it is worth being
//! explicit about why: the handler is running on the interrupted thread's own
//! kernel stack, so the `iretq` frame the processor pushed stays on that stack
//! along with the registers saved here. When the thread is next scheduled, it
//! resumes inside `context_switch`, returns into the interrupt handler, and the
//! handler's `iretq` puts it back exactly where it was interrupted.
//!
//! This is also why the timer must not use an Interrupt Stack Table slot: an
//! IST vector runs on one shared stack, so switching away from it would leave
//! the saved state where the next interrupt would overwrite it.

use core::arch::naked_asm;

/// Flags a freshly created thread starts with.
///
/// Bit 1 is reserved and always reads as one; bit 9 is the interrupt flag, set
/// so that a new thread is immediately preemptible.
pub const INITIAL_RFLAGS: u64 = 0x0000_0000_0000_0202;

/// Number of `u64` slots [`context_switch`] pushes: `rflags` plus six
/// callee-saved registers.
pub const SAVED_REGISTER_COUNT: usize = 7;

/// Save the current context, switch stacks, and restore the other one.
///
/// Writes the outgoing stack pointer through `save_rsp_to`, then continues on
/// `load_rsp`. Returns — on the outgoing thread's stack — whenever that thread
/// is scheduled again.
///
/// # Safety
///
/// `load_rsp` must point at a stack prepared either by a previous call to this
/// function or by [`super::thread::Thread::prepare_stack`]. `save_rsp_to` must
/// be a valid, writable location that outlives the switch. Interrupts must be
/// disabled: the window where neither thread owns the stack pointer is not
/// re-entrant.
#[unsafe(naked)]
pub unsafe extern "sysv64" fn context_switch(save_rsp_to: *mut u64, load_rsp: u64) {
    // The push order here and the pop order below define the stack layout that
    // `Thread::prepare_stack` fabricates for a new thread. The two must agree
    // exactly, so neither may be changed alone.
    naked_asm!(
        "push rbp",
        "push rbx",
        "push r12",
        "push r13",
        "push r14",
        "push r15",
        "pushfq",
        // rdi is `save_rsp_to`, rsi is `load_rsp`, per the sysv64 ABI.
        "mov [rdi], rsp",
        "mov rsp, rsi",
        "popfq",
        "pop r15",
        "pop r14",
        "pop r13",
        "pop r12",
        "pop rbx",
        "pop rbp",
        "ret",
    )
}
