//! Checks that need no hardware, run at boot because they cannot run anywhere
//! else.
//!
//! Most of what this kernel claims is verified by booting it. Some of it is not
//! about the machine at all: that a descriptor is sixteen bytes and splits an
//! address across three fields, that a divisor is clamped rather than wrapped,
//! that a structure survives being written to bytes and read back. Those are
//! the things a unit test is for.
//!
//! # Why they are not unit tests
//!
//! The kernel is a bare-metal binary with its own panic handler and its own
//! `_start`. `cargo test` links against `std`, which brings a second
//! `panic_impl` and a second entry point, and the build fails outright. There
//! is no configuration in which `#[cfg(test)] mod tests` inside this crate is
//! compiled, let alone run.
//!
//! Three modules had such a block anyway. They were written, they were correct,
//! and they had never once executed -- which is the failure mode a test exists
//! to prevent, arrived at by the test itself. They run here instead, on the
//! machine, on every boot, and their failures reach the same serial log as
//! everything else.
//!
//! # Adding one
//!
//! A module exposes `fn something_self_test() -> Result<(), &'static str>`
//! returning the name of the first check that failed, and it is added to
//! [`CHECKS`]. Nothing else. A check that needs a device, a clock or a
//! scheduler does not belong here -- it belongs in the boot self-tests, where
//! there is something to test against.

use crate::kprintln;

/// One named group of checks.
type Check = (&'static str, fn() -> Result<(), &'static str>);

/// Every group, run in this order.
const CHECKS: &[Check] = &[
    ("interrupt descriptors", crate::arch::idt::layout_self_test),
    ("timer divisors", crate::arch::pit::divisor_self_test),
    (
        "the NexusFS on-disk format",
        crate::fs::nexusfs::format_self_test,
    ),
];

/// Run them all, and say so.
///
/// Every group is run even after one fails, because they are independent and a
/// single boot that reports three broken things is worth three boots that each
/// report one.
pub fn run() -> bool {
    let mut failed = 0;
    for (name, check) in CHECKS {
        if let Err(what) = check() {
            kprintln!("[test] FAILED: {name}: {what}");
            failed += 1;
        }
    }

    if failed == 0 {
        kprintln!(
            "[test] {} structural checks passed: {}",
            CHECKS.len(),
            names()
        );
    }
    failed == 0
}

/// The groups, comma separated, for the line above.
fn names() -> alloc::string::String {
    let mut text = alloc::string::String::new();
    for (index, (name, _)) in CHECKS.iter().enumerate() {
        if index > 0 {
            text.push_str(", ");
        }
        text.push_str(name);
    }
    text
}
