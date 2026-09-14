//! Signatures, so that a machine can tell where a package came from.
//!
//! Written out rather than depended on, for the same reason the hash is: what a
//! signature is worth rests on the algorithm, and an implementation is only
//! worth anything if it agrees with every other one. So it is checked against
//! the test vectors in RFC 8032 — the same public keys, the same messages, the
//! same signatures, byte for byte.
//!
//! # What is here
//!
//! Ed25519: SHA-512, arithmetic modulo 2²⁵⁵ − 19, the twisted Edwards curve
//! that sits over it, and arithmetic modulo the group order. Signing and
//! verifying both, because a system that could only verify would need its
//! packages signed by something else, and there is nothing else.
//!
//! # What is not
//!
//! Key generation from a source of randomness, because this machine has no
//! source it would trust yet: a key made from the uptime counter is a key an
//! attacker can guess. Keys are made off the machine and brought to it.

#![no_std]
#![deny(unsafe_op_in_unsafe_fn)]

extern crate alloc;

pub mod ed25519;
pub mod field;
pub mod password;
pub mod scalar;
pub mod sha512;

pub use ed25519::{public_key, sign, verify, PUBLIC_KEY, SECRET_KEY, SIGNATURE};
