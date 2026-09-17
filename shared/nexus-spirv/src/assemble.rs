//! A SPIR-V assembler: modules built a word at a time.
//!
//! The same idea as the hand-assembled ELF fixtures in `tools/`. There is no
//! shader compiler on this machine — `glslang` and `shaderc` are C++ programs
//! against a C library that is not here — so the modules this repository reads
//! are built word by word, with the field each word belongs to named beside it.
//!
//! That is a weaker fixture than one `glslang` emitted, and the difference is
//! worth naming: a hand-built module is one somebody who has read the
//! specification believes is correct, and a compiler's output is one that has
//! been through a validator. What the tests prove is that the reader agrees
//! with the specification as written down here.
//!
//! Public rather than test-only, because a program on the machine needs it for
//! the same reason the tests do: it has no compiler either.

extern crate alloc;

use alloc::vec;
use alloc::vec::Vec;

use crate::{Id, HEADER_WORDS, MAGIC};

/// A module being built.
pub struct Builder {
    words: Vec<u32>,
    next: Id,
}

impl Default for Builder {
    fn default() -> Self {
        Self::new()
    }
}

impl Builder {
    /// A module with a header and nothing in it.
    ///
    /// The version is 1.0, which is what every consumer accepts, and the
    /// generator is zero: that field names the tool that produced the module
    /// and nothing reads it but a tool reporting bugs to the right project.
    pub fn new() -> Self {
        Self {
            // magic, version 1.0, generator, bound (filled in later), schema
            words: vec![MAGIC, 0x0001_0000, 0, 0, 0],
            next: 1,
        }
    }

    /// The next identifier. They start at one: zero is never valid.
    pub fn id(&mut self) -> Id {
        let id = self.next;
        self.next += 1;
        id
    }

    /// One instruction: its length and opcode, then its operands.
    pub fn op(&mut self, opcode: u16, operands: &[u32]) {
        let count = (operands.len() + 1) as u32;
        self.words.push((count << 16) | u32::from(opcode));
        self.words.extend_from_slice(operands);
    }

    /// The finished module, with `bound` filled in.
    pub fn finish(mut self) -> Vec<u32> {
        self.words[3] = self.next;
        self.words
    }

    /// The words so far, for a test that wants to damage one.
    pub fn words(&self) -> &[u32] {
        &self.words
    }
}

/// A literal string, four bytes to a word, little end first, with at least one
/// zero byte at the end.
///
/// The "at least" is the part worth having a function for: a string whose
/// length is a multiple of four takes a whole extra word, and writing it
/// without one puts every operand after it in the wrong place.
pub fn text(value: &str) -> Vec<u32> {
    let mut words = Vec::new();
    let mut word = 0u32;
    let mut filled = 0;
    for byte in value.as_bytes() {
        word |= u32::from(*byte) << (8 * filled);
        filled += 1;
        if filled == 4 {
            words.push(word);
            word = 0;
            filled = 0;
        }
    }
    // The terminator, and the padding after it, which are the same zero bytes.
    words.push(word);
    words
}

/// Several runs of operands, one after another.
pub fn joined(parts: &[&[u32]]) -> Vec<u32> {
    let mut all = Vec::new();
    for part in parts {
        all.extend_from_slice(part);
    }
    all
}

/// How long the header is, so a test can talk about offsets into a module.
pub const HEADER: usize = HEADER_WORDS;
