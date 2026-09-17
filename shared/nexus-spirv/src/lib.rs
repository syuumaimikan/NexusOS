//! SPIR-V: the form a shader is in by the time it reaches a driver.
//!
//! Nobody writes SPIR-V. It is what `glslang`, `shaderc`, `naga` and every
//! other shader compiler *emits*, and it is what Vulkan takes — `vkCreateShader
//! Module` is handed a block of it and nothing else. So it is the first thing
//! between this system and any graphics interface worth the name: a machine
//! that cannot read SPIR-V cannot run a shader anybody else compiled, whatever
//! else it can draw.
//!
//! This crate reads a module and runs it. No hardware, no Vulkan, no driver.
//!
//! # Why this and not Mesa
//!
//! Mesa is the answer to "is there open-source OpenGL and Vulkan", and it is a
//! very good one: `lavapipe` is a software Vulkan, `llvmpipe` a software
//! OpenGL, and both are real implementations that pass real conformance tests.
//! None of it can be built for this system yet, and the reason is not graphics.
//! Mesa is a program *for a POSIX system*: it wants a C library, threads,
//! `dlopen`, and — for the fast software paths — LLVM. This machine has no
//! libc at all. Porting one is the gate that stands in front of Mesa, in front
//! of glibc's loader, and in front of Steam, and it is a bigger piece of work
//! than any of them.
//!
//! What can be done without a libc is this: take the *format*, which is a
//! published Khronos specification and not a piece of anybody's code, and
//! implement it. That gets a shader compiled by a normal toolchain running on
//! this machine, which is a real step and is honestly describable. It is not
//! Vulkan and it is not conformant, and this file says so in as many words.
//!
//! # The format
//!
//! A module is a stream of thirty-two bit words. Five of header:
//!
//! ```text
//!   magic (0x07230203) | version | generator | bound | schema
//! ```
//!
//! and then instructions, each of which begins with one word holding its length
//! *in words, including itself* in the high half and its opcode in the low
//! half:
//!
//! ```text
//!   (word count << 16) | opcode
//! ```
//!
//! That is the whole framing. It is deliberately close to Wayland's, and has
//! the same consequence: a reader that miscounts one instruction reads the next
//! one's header as an operand and never recovers, so the length is checked
//! against what is left rather than trusted.
//!
//! Most instructions produce a result, and a result is an **identifier** — a
//! number, unique in the module, below the header's `bound`. SPIR-V is in
//! static single assignment form, so an identifier is written exactly once and
//! everything after it refers to that one value. That is what makes a module
//! runnable by walking it: there is no question of which assignment to a name
//! is in force.
//!
//! # What is here and what is not
//!
//! Here: the header, the instruction stream, types, constants, `Input` and
//! `Output` variables with their `Location` and `BuiltIn` decorations, one
//! entry point, and an interpreter for the subset of instructions a small
//! fragment or vertex shader is made of.
//!
//! Not here, and each of these is a real thing a real shader uses: images and
//! samplers, uniform and storage buffers, push constants, matrices, structures
//! with member decorations, loops with real merge blocks, function calls,
//! integer arithmetic beyond comparison, atomics, subgroups, and the
//! `Kernel`/`OpenCL` half of the specification. An instruction that is not
//! implemented is **reported by number**, not skipped: a shader that silently
//! ignored an instruction would draw the wrong thing and say nothing.

#![no_std]
#![deny(unsafe_op_in_unsafe_fn)]

extern crate alloc;

pub mod assemble;
pub mod module;
pub mod run;

#[cfg(test)]
mod tests;

pub use module::{Decoration, EntryPoint, ExecutionModel, Module, Type, Variable};
pub use run::{Inputs, Outputs, Value};

/// The first word of every module. Endianness is detectable from it: a module
/// written the other way round reads as `0x03022307`.
pub const MAGIC: u32 = 0x0723_0203;

/// The byte-swapped magic, which is what a module from a machine of the other
/// endianness looks like.
const MAGIC_REVERSED: u32 = 0x0302_2307;

/// How long the header is, in words.
pub const HEADER_WORDS: usize = 5;

/// An identifier. Unique within a module and below its `bound`.
pub type Id = u32;

/// What went wrong.
///
/// Every variant names the thing that was wrong rather than "invalid module",
/// because the usual way to meet one of these is to have assembled a module by
/// hand and miscounted something.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// Fewer than five words: there is not even a header.
    NoHeader,
    /// The first word is not the magic number.
    NotSpirv(u32),
    /// The magic number is there, byte-swapped. This is a module from a machine
    /// of the other endianness, and swapping it is a thing a reader could do —
    /// but it would be untested code on a machine that will never produce one.
    WrongEndian,
    /// An instruction says it is longer than what is left, or claims to be
    /// zero words long, which would never advance.
    BadLength { at: usize, words: u32 },
    /// An instruction had fewer operands than it needs.
    TooFewOperands {
        opcode: u16,
        wanted: usize,
        had: usize,
    },
    /// A string operand is not terminated inside the instruction.
    UnterminatedString,
    /// An identifier at or above the module's `bound`, or zero, which is never
    /// a valid identifier.
    BadId(Id),
    /// An identifier used before it was defined. SPIR-V requires definitions to
    /// come first for everything this crate looks at, so this is a malformed
    /// module rather than something to resolve later.
    Undefined(Id),
    /// A type identifier that names something that is not a type.
    NotAType(Id),
    /// An instruction this crate does not implement. Reported rather than
    /// skipped — see the note at the top of this file.
    Unimplemented(u16),
    /// A `GLSL.std.450` function this crate does not implement.
    UnimplementedExtended(u32),
    /// The module has no `OpEntryPoint`, or more than one.
    NoEntryPoint,
    /// An operation was given a value of the wrong shape: a scalar where a
    /// vector was wanted, or two vectors of different widths.
    WrongShape,
    /// An `Input` the shader reads that the caller did not supply.
    MissingInput {
        location: Option<u32>,
        builtin: Option<u32>,
    },
    /// The shader ran for longer than [`run::MOST_STEPS`] without returning.
    /// A shader is not allowed to take unbounded time here: this interpreter
    /// runs inside whatever asked it to, and a loop with a condition that never
    /// goes false would be that program hanging.
    TooLong,
}

/// One instruction, borrowed out of the word stream.
#[derive(Debug, Clone, Copy)]
pub struct Instruction<'a> {
    pub opcode: u16,
    /// Everything after the length-and-opcode word.
    pub operands: &'a [u32],
}

impl Instruction<'_> {
    /// The `index`th operand, or an error naming what was short.
    pub fn operand(&self, index: usize) -> Result<u32, Error> {
        self.operands
            .get(index)
            .copied()
            .ok_or(Error::TooFewOperands {
                opcode: self.opcode,
                wanted: index + 1,
                had: self.operands.len(),
            })
    }

    /// A literal string starting at `index`, and how many words it took.
    ///
    /// SPIR-V packs strings four bytes to a word, little end first, with at
    /// least one zero byte at the end — so a string whose length is a multiple
    /// of four takes a whole extra word for the terminator. Getting that wrong
    /// puts every operand after the string one place out, which is the same
    /// class of bug as reading a Wayland `bind` at a fixed offset.
    pub fn string(&self, index: usize) -> Result<(alloc::string::String, usize), Error> {
        use alloc::string::String;
        let mut text = String::new();
        let mut at = index;
        loop {
            let word = self
                .operands
                .get(at)
                .copied()
                .ok_or(Error::UnterminatedString)?;
            at += 1;
            for shift in [0, 8, 16, 24] {
                let byte = ((word >> shift) & 0xFF) as u8;
                if byte == 0 {
                    return Ok((text, at - index));
                }
                text.push(byte as char);
            }
        }
    }
}

/// The header's `bound`: one more than the largest identifier the module uses.
///
/// Read separately from the instructions because everything else needs it to
/// check identifiers against.
pub fn bound(words: &[u32]) -> Result<u32, Error> {
    check_header(words)?;
    Ok(words[3])
}

/// Check the header, and say what is wrong with it if anything is.
pub fn check_header(words: &[u32]) -> Result<(), Error> {
    if words.len() < HEADER_WORDS {
        return Err(Error::NoHeader);
    }
    match words[0] {
        MAGIC => Ok(()),
        MAGIC_REVERSED => Err(Error::WrongEndian),
        other => Err(Error::NotSpirv(other)),
    }
}

/// Walk the instructions after the header.
pub fn instructions(words: &[u32]) -> Instructions<'_> {
    Instructions {
        words,
        at: HEADER_WORDS.min(words.len()),
    }
}

/// The instruction stream.
///
/// An `Iterator` of `Result`, so that a malformed length stops the walk with
/// the offset it was at rather than being skipped. It yields nothing more after
/// an error: there is no way to find the next instruction in a stream whose
/// only framing is the lengths.
pub struct Instructions<'a> {
    words: &'a [u32],
    at: usize,
}

impl<'a> Iterator for Instructions<'a> {
    type Item = Result<Instruction<'a>, Error>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.at >= self.words.len() {
            return None;
        }
        let header = self.words[self.at];
        let count = (header >> 16) as usize;
        let opcode = (header & 0xFFFF) as u16;
        if count == 0 || self.at + count > self.words.len() {
            let at = self.at;
            // Stop, rather than trying to resynchronise on something that looks
            // like a header. There is nothing to resynchronise on.
            self.at = self.words.len();
            return Some(Err(Error::BadLength {
                at,
                words: count as u32,
            }));
        }
        let operands = &self.words[self.at + 1..self.at + count];
        self.at += count;
        Some(Ok(Instruction { opcode, operands }))
    }
}

/// The opcodes this crate knows by name.
///
/// Only the ones it acts on. A list of all four hundred would be a list nobody
/// checks, and the ones left out are reported by number when they turn up.
pub mod op {
    pub const NOP: u16 = 0;
    pub const SOURCE: u16 = 3;
    pub const SOURCE_EXTENSION: u16 = 4;
    pub const NAME: u16 = 5;
    pub const MEMBER_NAME: u16 = 6;
    pub const STRING: u16 = 7;
    pub const LINE: u16 = 8;
    pub const EXTENSION: u16 = 10;
    pub const EXT_INST_IMPORT: u16 = 11;
    pub const EXT_INST: u16 = 12;
    pub const MEMORY_MODEL: u16 = 14;
    pub const ENTRY_POINT: u16 = 15;
    pub const EXECUTION_MODE: u16 = 16;
    pub const CAPABILITY: u16 = 17;

    pub const TYPE_VOID: u16 = 19;
    pub const TYPE_BOOL: u16 = 20;
    pub const TYPE_INT: u16 = 21;
    pub const TYPE_FLOAT: u16 = 22;
    pub const TYPE_VECTOR: u16 = 23;
    pub const TYPE_POINTER: u16 = 32;
    pub const TYPE_FUNCTION: u16 = 33;

    pub const CONSTANT_TRUE: u16 = 41;
    pub const CONSTANT_FALSE: u16 = 42;
    pub const CONSTANT: u16 = 43;
    pub const CONSTANT_COMPOSITE: u16 = 44;

    pub const FUNCTION: u16 = 54;
    pub const FUNCTION_END: u16 = 56;
    pub const VARIABLE: u16 = 59;
    pub const LOAD: u16 = 61;
    pub const STORE: u16 = 62;
    pub const ACCESS_CHAIN: u16 = 65;
    pub const DECORATE: u16 = 71;
    pub const MEMBER_DECORATE: u16 = 72;

    pub const VECTOR_SHUFFLE: u16 = 79;
    pub const COMPOSITE_CONSTRUCT: u16 = 80;
    pub const COMPOSITE_EXTRACT: u16 = 81;

    pub const CONVERT_S_TO_F: u16 = 111;
    pub const F_NEGATE: u16 = 127;
    pub const F_ADD: u16 = 129;
    pub const F_SUB: u16 = 131;
    pub const F_MUL: u16 = 133;
    pub const F_DIV: u16 = 136;
    pub const VECTOR_TIMES_SCALAR: u16 = 142;
    pub const DOT: u16 = 148;

    pub const LOGICAL_AND: u16 = 167;
    pub const LOGICAL_OR: u16 = 166;
    pub const LOGICAL_NOT: u16 = 168;
    pub const SELECT: u16 = 169;
    pub const F_ORD_EQUAL: u16 = 180;
    pub const F_ORD_LESS_THAN: u16 = 184;
    pub const F_ORD_GREATER_THAN: u16 = 186;
    pub const F_ORD_LESS_THAN_EQUAL: u16 = 188;
    pub const F_ORD_GREATER_THAN_EQUAL: u16 = 190;

    pub const PHI: u16 = 245;
    pub const LOOP_MERGE: u16 = 246;
    pub const SELECTION_MERGE: u16 = 247;
    pub const LABEL: u16 = 248;
    pub const BRANCH: u16 = 249;
    pub const BRANCH_CONDITIONAL: u16 = 250;
    pub const RETURN: u16 = 253;
    pub const UNREACHABLE: u16 = 255;
}

/// `GLSL.std.450`, the extended instruction set every compiled GLSL shader
/// imports. The numbers are positions in that specification's own list.
pub mod glsl {
    pub const F_ABS: u32 = 4;
    pub const FLOOR: u32 = 8;
    pub const FRACT: u32 = 10;
    pub const SQRT: u32 = 31;
    pub const F_MIN: u32 = 37;
    pub const F_MAX: u32 = 40;
    pub const F_CLAMP: u32 = 43;
    pub const F_MIX: u32 = 46;
    pub const STEP: u32 = 48;
    pub const LENGTH: u32 = 66;
    pub const NORMALIZE: u32 = 69;

    /// The name a module imports it under. Checked rather than assumed: a
    /// module may import several sets, and `OpExtInst` names which one by the
    /// identifier the import produced.
    pub const NAME: &str = "GLSL.std.450";
}

/// Storage classes, of which this crate acts on four.
pub mod storage {
    pub const UNIFORM_CONSTANT: u32 = 0;
    pub const INPUT: u32 = 1;
    pub const UNIFORM: u32 = 2;
    pub const OUTPUT: u32 = 3;
    pub const FUNCTION: u32 = 7;
    pub const PUSH_CONSTANT: u32 = 9;
}

/// The two decorations that say how a shader is wired to the outside.
pub mod decoration {
    /// Which slot an `Input` or `Output` is in. What a pipeline matches a
    /// vertex shader's outputs to a fragment shader's inputs by.
    pub const LOCATION: u32 = 30;
    /// That this variable is one the system provides rather than the pipeline:
    /// the fragment's position, the vertex's index, and so on.
    pub const BUILT_IN: u32 = 11;
}

/// The built-ins this crate knows.
pub mod built_in {
    /// A vertex shader's output position, in clip space.
    pub const POSITION: u32 = 0;
    /// A fragment shader's input position, in window coordinates. The one every
    /// shader that draws a gradient reads.
    pub const FRAG_COORD: u32 = 15;
    /// Which vertex this is.
    pub const VERTEX_INDEX: u32 = 42;
}
