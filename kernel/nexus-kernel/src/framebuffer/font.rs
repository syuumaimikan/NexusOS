//! The bitmap font, which lives in a crate of its own.
//!
//! It moved out when programs needed it. The kernel draws its banner and its
//! status panel with this face, and so does anything in user space that draws
//! text -- and a system with two faces would be a system where the same string
//! is two widths depending on who drew it.
//!
//! Generating it in a shared crate rather than in the kernel also puts the
//! rasteriser where both can see it: the face covers exactly the characters the
//! translations use, and that set does not belong to the kernel either.

pub use nexus_font::*;
