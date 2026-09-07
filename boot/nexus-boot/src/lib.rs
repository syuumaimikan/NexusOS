//! # The Nexus Bootloader library
//!
//! The bootloader's logic lives here rather than in the binary so that the
//! parts with real invariants — the UEFI structure layouts, the ELF parser, the
//! memory-map normalizer, the page-table index arithmetic — can be exercised by
//! `cargo test` on the host. `src/main.rs` is a thin UEFI entry point over this
//! library.
//!
//! Under `cfg(test)` the crate links against `std` so that the test harness
//! works; every other build is `no_std`. Nothing in this crate uses `std`
//! itself.

#![cfg_attr(not(test), no_std)]
#![deny(unsafe_op_in_unsafe_fn)]

pub mod elf;
pub mod fs;
pub mod graphics;
pub mod memory;
pub mod paging;
pub mod serial;
pub mod uefi;
