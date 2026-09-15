//! **Nex**: a small language, written here, that runs on this machine.
//!
//! # Why it is not Python and not C
//!
//! Because those were asked for and cannot honestly be delivered. CPython is
//! about six hundred thousand lines of C and needs a C library underneath it;
//! GCC is millions and needs an assembler, a linker and a target description.
//! Either would be a year of work and neither would be *this* system's -- and
//! a thing called "Python" that ran a tenth of Python would be worse than
//! useless, because every program anybody brought to it would fail in a way
//! they could not predict.
//!
//! So this is a real language with a small honest name. Nobody arrives at `nex`
//! expecting their existing programs to run.
//!
//! # What it is
//!
//! A tree-walking interpreter: characters to tokens, tokens to a tree, and the
//! tree evaluated directly. No bytecode, no optimiser, no garbage collector --
//! values are reference counted and a cycle leaks, which is written down in
//! [`value`] rather than left to be discovered.
//!
//! ```text
//! fn fib(n) {
//!     if n < 2 { return n }
//!     return fib(n - 1) + fib(n - 2)
//! }
//! let i = 0
//! while i < 10 {
//!     print(fib(i))
//!     i = i + 1
//! }
//! ```
//!
//! # What it has
//!
//! Integers, strings, booleans and nothing. Variables, assignment, `if`/`else`,
//! `while`, functions with recursion, and the operators those need. Six
//! built-in functions. Errors carry the line they happened on.
//!
//! # What it has not
//!
//! No floating point, because this system has none in the kernel and a language
//! whose numbers behave differently depending on where it runs is a language
//! nobody can reason about. No lists, no maps, no closures over an enclosing
//! scope, no modules, no `for`. Each of those is a real feature and each is
//! named here rather than half-built.

#![no_std]
#![deny(unsafe_op_in_unsafe_fn)]

extern crate alloc;

pub mod interpreter;
pub mod lexer;
pub mod parser;
pub mod value;

use alloc::string::String;
use alloc::vec::Vec;

pub use interpreter::Interpreter;
pub use value::Value;

/// Anything that went wrong, and where.
///
/// One type for all three stages. A person running a program does not care
/// whether the trouble was found by the lexer or the evaluator; they care what
/// is wrong and which line it is on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Trouble {
    pub line: usize,
    pub what: String,
}

impl Trouble {
    /// One, at a line.
    #[must_use]
    pub fn at(line: usize, what: impl Into<String>) -> Self {
        Self {
            line,
            what: what.into(),
        }
    }
}

impl core::fmt::Display for Trouble {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "line {}: {}", self.line, self.what)
    }
}

/// Run a program, and return everything it printed.
///
/// The output is collected rather than written, so that the same function
/// serves a program on this machine and a test on the host -- and so that
/// nothing in the language itself has to know what a channel is.
///
/// # Errors
///
/// The first thing that went wrong, with its line.
pub fn run(source: &str) -> Result<Vec<String>, Trouble> {
    let tokens = lexer::lex(source)?;
    let program = parser::parse(&tokens)?;
    let mut interpreter = Interpreter::new();
    interpreter.run(&program)?;
    Ok(interpreter.take_output())
}
