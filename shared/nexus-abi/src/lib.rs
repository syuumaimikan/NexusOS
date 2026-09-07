//! # Nexus ABI
//!
//! Types shared across the NexusOS boot/kernel boundary and, later, across the
//! kernel/user-space system-call boundary.
//!
//! Everything in this crate is `#[repr(C)]` and must stay layout-stable: the
//! bootloader writes these structures before `ExitBootServices` and the kernel
//! reads them after it has switched to its own page tables.  A layout mismatch
//! is not detectable at runtime beyond the magic/version check below, so treat
//! changes here as changes to a published binary interface.

#![no_std]
#![deny(unsafe_op_in_unsafe_fn)]

pub mod boot;
pub mod layout;

pub use boot::*;
