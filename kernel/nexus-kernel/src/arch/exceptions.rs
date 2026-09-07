//! Handlers for the 32 architectural exceptions.
//!
//! Every one of them reports what happened, where, and — for the faults that
//! carry an error code — what the code decodes to, then stops the machine. A
//! kernel-mode exception at this stage of NexusOS means a bug in the kernel;
//! continuing past it would only obscure the cause. Once user mode exists, the
//! faults that are recoverable (page faults against valid mappings, above all)
//! will be handled here instead of reported.
//!
//! The value of this module is not that it fixes anything. It is that a fault
//! becomes a page of legible diagnostics rather than a machine that silently
//! reboots.

use super::idt::InterruptStackFrame;
use super::{gdt, halt_forever};
use crate::kprintln;

/// Read `cr2`, which holds the faulting address after a page fault.
#[inline]
fn read_cr2() -> u64 {
    let value: u64;
    // SAFETY: reading a control register is side-effect free.
    unsafe {
        core::arch::asm!("mov {}, cr2", out(reg) value, options(nomem, nostack, preserves_flags));
    }
    value
}

/// Describe the privilege level an exception was taken from.
fn privilege(frame: &InterruptStackFrame) -> &'static str {
    if frame.code_segment & 3 == 0 {
        "kernel"
    } else {
        "user"
    }
}

/// Print the common header shared by every exception report.
fn report_header(vector: u8, name: &str, frame: &InterruptStackFrame) {
    kprintln!();
    kprintln!("=======================================================");
    kprintln!(" EXCEPTION {vector}: {name}");
    kprintln!("=======================================================");
    kprintln!("  taken from : {} mode", privilege(frame));
    kprintln!("  rip        : {:#018x}", frame.instruction_pointer);
    kprintln!("  cs         : {:#06x}", frame.code_segment);
    kprintln!("  rflags     : {:#018x}", frame.cpu_flags);
    kprintln!("  rsp        : {:#018x}", frame.stack_pointer);
    kprintln!("  ss         : {:#06x}", frame.stack_segment);
}

/// Print the footer and stop.
fn report_footer() -> ! {
    kprintln!("=======================================================");
    kprintln!("the system has been halted");
    halt_forever()
}

/// Decode a selector error code, as pushed by the segment-related faults.
///
/// Bit 0 is the external flag, bits 1..3 select which table, and bits 3..16 are
/// the index into it.
fn report_selector_error(code: u64) {
    let external = code & 1 != 0;
    let table = match (code >> 1) & 3 {
        0 => "GDT",
        1 | 3 => "IDT",
        _ => "LDT",
    };
    let index = (code >> 3) & 0x1FFF;
    kprintln!("  error code : {code:#x}");
    kprintln!("    table    : {table}");
    kprintln!("    index    : {index}");
    kprintln!("    external : {external}");
}

/// A handler for a vector that pushes no error code.
macro_rules! simple_handler {
    ($name:ident, $vector:expr, $description:expr) => {
        extern "x86-interrupt" fn $name(frame: InterruptStackFrame) {
            report_header($vector, $description, &frame);
            report_footer()
        }
    };
}

/// A handler for a vector that pushes a selector error code.
macro_rules! selector_error_handler {
    ($name:ident, $vector:expr, $description:expr) => {
        extern "x86-interrupt" fn $name(frame: InterruptStackFrame, error_code: u64) {
            report_header($vector, $description, &frame);
            report_selector_error(error_code);
            report_footer()
        }
    };
}

simple_handler!(divide_error, 0, "divide error");
simple_handler!(debug_exception, 1, "debug");
simple_handler!(non_maskable_interrupt, 2, "non-maskable interrupt");
simple_handler!(overflow, 4, "overflow");
simple_handler!(bound_range_exceeded, 5, "bound range exceeded");
simple_handler!(invalid_opcode, 6, "invalid opcode");
simple_handler!(device_not_available, 7, "device not available");
simple_handler!(
    coprocessor_segment_overrun,
    9,
    "coprocessor segment overrun"
);
simple_handler!(x87_floating_point, 16, "x87 floating-point error");
simple_handler!(simd_floating_point, 19, "SIMD floating-point error");
simple_handler!(virtualization, 20, "virtualization exception");

selector_error_handler!(invalid_tss, 10, "invalid TSS");
selector_error_handler!(segment_not_present, 11, "segment not present");
selector_error_handler!(stack_segment_fault, 12, "stack-segment fault");
selector_error_handler!(control_protection, 21, "control-protection exception");

/// Breakpoint.
///
/// The one exception here that resumes rather than stopping: `int3` is a
/// deliberate trap, raised by debuggers and by the kernel's own interrupt
/// self-test. Returning from it means execution continues at the instruction
/// after the trap.
extern "x86-interrupt" fn breakpoint_trap(frame: InterruptStackFrame) {
    kprintln!(
        "[intr] breakpoint at {:#018x} ({} mode); resuming",
        frame.instruction_pointer,
        privilege(&frame)
    );
}

/// General protection fault.
///
/// Separated from the other selector-error faults only because it is the one
/// most often raised with a zero error code, where the selector decode would be
/// misleading noise.
extern "x86-interrupt" fn general_protection_fault(frame: InterruptStackFrame, error_code: u64) {
    report_header(13, "general protection fault", &frame);
    if error_code == 0 {
        kprintln!("  error code : 0 (no segment selector involved)");
    } else {
        report_selector_error(error_code);
    }
    report_footer()
}

/// Page fault.
///
/// The error code says why the access was refused, and `cr2` says which address
/// it was for. Together they identify almost every paging bug outright, so both
/// are decoded in full.
extern "x86-interrupt" fn page_fault(frame: InterruptStackFrame, error_code: u64) {
    let address = read_cr2();

    report_header(14, "page fault", &frame);
    kprintln!("  address    : {address:#018x}");
    kprintln!("  error code : {error_code:#x}");
    kprintln!(
        "    cause    : {}",
        if error_code & 1 == 0 {
            "page not present"
        } else {
            "protection violation"
        }
    );
    kprintln!(
        "    access   : {}",
        if error_code & (1 << 1) != 0 {
            "write"
        } else {
            "read"
        }
    );
    kprintln!(
        "    origin   : {}",
        if error_code & (1 << 2) != 0 {
            "user mode"
        } else {
            "kernel mode"
        }
    );
    if error_code & (1 << 3) != 0 {
        kprintln!("    note     : a reserved bit was set in a paging structure");
    }
    if error_code & (1 << 4) != 0 {
        kprintln!("    note     : the access was an instruction fetch");
    }
    if error_code & (1 << 5) != 0 {
        kprintln!("    note     : protection-key violation");
    }
    if error_code & (1 << 6) != 0 {
        kprintln!("    note     : shadow-stack access");
    }

    // The guard page below the boot stack is one page; a fault just under the
    // stack base is almost certainly an overflow rather than a stray pointer,
    // and saying so saves a long debugging detour.
    let stack_base = nexus_abi::layout::KERNEL_BOOT_STACK_BASE;
    if address < stack_base && stack_base - address <= 4096 {
        kprintln!("    note     : this is the boot stack guard page (stack overflow)");
    }

    report_footer()
}

/// Double fault.
///
/// Raised when the CPU cannot dispatch an earlier exception. Runs on its own
/// Interrupt Stack Table stack, because the usual reason the first dispatch
/// failed is that the current stack is unusable. It cannot return: the
/// architecture does not define what the interrupted state means.
extern "x86-interrupt" fn double_fault(frame: InterruptStackFrame, error_code: u64) -> ! {
    report_header(8, "double fault", &frame);
    kprintln!("  error code : {error_code:#x} (always zero)");
    kprintln!("  note       : running on the double-fault IST stack");
    kprintln!("  note       : an exception could not be dispatched; the usual");
    kprintln!("               cause is a kernel stack overflow");
    report_footer()
}

/// Machine check. Runs on its own stack; hardware has reported a fault it
/// cannot correct.
extern "x86-interrupt" fn machine_check(frame: InterruptStackFrame) -> ! {
    report_header(18, "machine check", &frame);
    kprintln!("  note       : the processor reported an uncorrectable error");
    report_footer()
}

/// Alignment check, which pushes an (always zero) error code.
extern "x86-interrupt" fn alignment_check(frame: InterruptStackFrame, _error_code: u64) {
    report_header(17, "alignment check", &frame);
    report_footer()
}

/// Hypervisor injection exception.
extern "x86-interrupt" fn hypervisor_injection(frame: InterruptStackFrame) {
    report_header(28, "hypervisor injection exception", &frame);
    report_footer()
}

/// VMM communication exception.
extern "x86-interrupt" fn vmm_communication(frame: InterruptStackFrame, error_code: u64) {
    report_header(29, "VMM communication exception", &frame);
    kprintln!("  error code : {error_code:#x}");
    report_footer()
}

/// Security exception.
extern "x86-interrupt" fn security_exception(frame: InterruptStackFrame, error_code: u64) {
    report_header(30, "security exception", &frame);
    kprintln!("  error code : {error_code:#x}");
    report_footer()
}

/// Catch-all for any vector without a specific handler.
///
/// Registering this everywhere means an unexpected interrupt produces a message
/// naming the vector, instead of a triple fault that names nothing.
extern "x86-interrupt" fn unhandled(frame: InterruptStackFrame) {
    report_header(255, "unhandled interrupt", &frame);
    kprintln!("  note       : no handler is registered for this vector");
    report_footer()
}

/// Register every architectural exception in `idt`.
///
/// # Safety
///
/// `idt` must not be loaded while it is being modified.
pub unsafe fn install(idt: &mut super::idt::InterruptDescriptorTable) {
    // SAFETY: each handler below is declared with the `x86-interrupt` ABI and
    // the argument shape its vector requires — an error-code parameter exactly
    // for the vectors that push one. The IST slots referenced are the ones the
    // GDT module gave stacks to.
    unsafe {
        // Fill every vector with the catch-all first, so nothing is left
        // absent; the specific handlers below then replace what they cover.
        for vector in 0..=255u8 {
            idt.set_handler(vector, unhandled as *const ());
        }

        idt.set_handler(0, divide_error as *const ());
        idt.set_handler(1, debug_exception as *const ());
        idt.set_handler_with_stack(2, non_maskable_interrupt as *const (), gdt::IST_NMI);
        idt.set_handler(3, breakpoint_trap as *const ());
        idt.set_handler(4, overflow as *const ());
        idt.set_handler(5, bound_range_exceeded as *const ());
        idt.set_handler(6, invalid_opcode as *const ());
        idt.set_handler(7, device_not_available as *const ());
        idt.set_handler_with_stack(8, double_fault as *const (), gdt::IST_DOUBLE_FAULT);
        idt.set_handler(9, coprocessor_segment_overrun as *const ());
        idt.set_handler(10, invalid_tss as *const ());
        idt.set_handler(11, segment_not_present as *const ());
        idt.set_handler(12, stack_segment_fault as *const ());
        idt.set_handler(13, general_protection_fault as *const ());
        idt.set_handler(14, page_fault as *const ());
        // 15 is reserved by the architecture.
        idt.set_handler(16, x87_floating_point as *const ());
        idt.set_handler(17, alignment_check as *const ());
        idt.set_handler_with_stack(18, machine_check as *const (), gdt::IST_MACHINE_CHECK);
        idt.set_handler(19, simd_floating_point as *const ());
        idt.set_handler(20, virtualization as *const ());
        idt.set_handler(21, control_protection as *const ());
        // 22..28 are reserved.
        idt.set_handler(28, hypervisor_injection as *const ());
        idt.set_handler(29, vmm_communication as *const ());
        idt.set_handler(30, security_exception as *const ());
        // 31 is reserved.
    }
}
