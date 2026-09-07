//! The kernel panic path.
//!
//! A kernel panic is unrecoverable by construction, so this does the two things
//! that still have value: get a precise description onto the serial console,
//! and stop every core before the damaged state can spread.

use core::panic::PanicInfo;
use core::sync::atomic::{AtomicBool, Ordering};

use crate::arch;

/// Set by the first core to panic, so that a fault inside the panic handler
/// itself cannot loop forever printing.
static PANICKING: AtomicBool = AtomicBool::new(false);

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    if PANICKING.swap(true, Ordering::SeqCst) {
        // A second panic while handling the first: the state we would need to
        // report is exactly the state we cannot trust. Stop immediately.
        arch::halt_forever();
    }

    // The lock-free writer is used on purpose: the panicking code may well have
    // been holding the serial lock, and a deadlock here would cost us the one
    // message that explains the failure.
    // SAFETY: the system is stopping; interleaved output is acceptable and no
    // further progress depends on the console's state.
    unsafe {
        crate::serial::write_fmt_unlocked(format_args!(
            "\n\n=======================================================\n\
             KERNEL PANIC\n\
             =======================================================\n"
        ));

        crate::serial::write_fmt_unlocked(format_args!(
            "thread:   {}
",
            crate::sched::current_id()
        ));

        if let Some(location) = info.location() {
            crate::serial::write_fmt_unlocked(format_args!(
                "location: {}:{}:{}\n",
                location.file(),
                location.line(),
                location.column()
            ));
        }

        crate::serial::write_fmt_unlocked(format_args!("message:  {}\n", info.message()));
        crate::serial::write_fmt_unlocked(format_args!(
            "=======================================================\n\
             the system has been halted\n"
        ));
    }

    arch::halt_forever()
}
