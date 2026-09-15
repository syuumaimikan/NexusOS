//! TLS 1.3, so that this machine can fetch an `https://` page.
//!
//! RFC 8446, client side only, with one cipher suite and one key exchange.
//!
//! # What is here and what is deliberately not
//!
//! | | |
//! | --- | --- |
//! | TLS 1.3 | yes |
//! | TLS 1.2 and earlier | no, and not planned |
//! | `TLS_CHACHA20_POLY1305_SHA256` | yes |
//! | AES-GCM suites | no; see `nexus_crypto::chacha` for why |
//! | X25519 key exchange | yes |
//! | Other groups, and hello retry | no |
//! | Session resumption, PSK, early data | no |
//! | Client certificates | no |
//!
//! A server that will not meet those is a server this machine cannot reach, and
//! it is told so by name rather than left to fail obscurely.
//!
//! # The rule this crate exists under
//!
//! **A TLS client that does not verify certificates is worse than no TLS.** It
//! looks like security, it produces a padlock, and it protects against nothing
//! an attacker on the path cannot do anyway. So the browser does not get an
//! `https://` scheme until the chain is actually checked, and this crate
//! refuses a connection it could not verify rather than returning one with a
//! warning attached.

#![no_std]
#![deny(unsafe_op_in_unsafe_fn)]

extern crate alloc;

pub mod der;
pub mod handshake;
pub mod record;
pub mod schedule;
pub mod wire;
pub mod x509;
