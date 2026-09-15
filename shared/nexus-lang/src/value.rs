//! What a Nex expression evaluates to.

use alloc::rc::Rc;
use alloc::string::{String, ToString as _};

/// A value.
///
/// Strings are reference counted, because copying one on every assignment would
/// make `let b = a` cost as much as building `a` did. Nothing here can hold a
/// value, so there are no cycles to leak -- and that is only true because this
/// language has no lists and no closures. The day it gains either, this comment
/// becomes wrong and something has to be done about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    Int(i64),
    Str(Rc<String>),
    Bool(bool),
    Nil,
}

impl Value {
    /// A string value from anything that is one.
    #[must_use]
    pub fn string(text: impl Into<String>) -> Self {
        Self::Str(Rc::new(text.into()))
    }

    /// What this is called when a message has to name it.
    #[must_use]
    pub const fn kind(&self) -> &'static str {
        match self {
            Self::Int(_) => "a number",
            Self::Str(_) => "a string",
            Self::Bool(_) => "a boolean",
            Self::Nil => "nothing",
        }
    }

    /// Whether a condition made of this is taken.
    ///
    /// Only `true` is true, and only `false` is false. A number is not a
    /// condition here, and neither is a string: `if 1 { }` is refused rather
    /// than guessed at, because every language that guessed disagrees with
    /// every other one about what an empty string means.
    #[must_use]
    pub const fn as_condition(&self) -> Option<bool> {
        match self {
            Self::Bool(value) => Some(*value),
            _ => None,
        }
    }

    /// How it is written when printed.
    #[must_use]
    pub fn show(&self) -> String {
        match self {
            Self::Int(value) => value.to_string(),
            // Without quotes. `print("hello")` should put `hello` on the
            // screen, which is what somebody means by it.
            Self::Str(value) => value.as_ref().clone(),
            Self::Bool(true) => "true".to_string(),
            Self::Bool(false) => "false".to_string(),
            Self::Nil => "nil".to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn printing_a_string_has_no_quotes_round_it() {
        assert_eq!(Value::string("hello").show(), "hello");
    }

    /// The one decision here that a reader might expect to go the other way.
    #[test]
    fn a_number_is_not_a_condition() {
        assert_eq!(Value::Int(1).as_condition(), None);
        assert_eq!(Value::string("").as_condition(), None);
        assert_eq!(Value::Bool(true).as_condition(), Some(true));
    }
}
