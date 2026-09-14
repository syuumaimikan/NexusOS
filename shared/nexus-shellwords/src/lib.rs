//! Turning a line somebody typed into the words they meant.
//!
//! Splitting on spaces is wrong the first time somebody types a filename with a
//! space in it, and every shell that has ever existed has had to answer the same
//! three questions: what quoting is, what escaping is, and what happens when the
//! line ends in the middle of either.
//!
//! This is a small answer to those three, and it is a library on its own because
//! it is the part of a shell that can be tested without a window, a filesystem
//! or a machine. A command interpreter that got this wrong would look like a
//! filesystem that could not find files.
//!
//! # The rules
//!
//! * Words are separated by runs of whitespace.
//! * `'single quotes'` take everything up to the next `'`, backslashes and all.
//! * `"double quotes"` are the same except that `\` escapes the next character.
//! * Outside quotes, `\` escapes the next character, whatever it is.
//! * A quote that is never closed takes the rest of the line, and the caller is
//!   told so -- because a shell that silently completed it would run a command
//!   the person did not type.

#![no_std]
#![deny(unsafe_op_in_unsafe_fn)]

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;

/// What a line turned out to be.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Words {
    /// The words, in order. Empty for a blank line.
    pub words: Vec<String>,
    /// Whether a quote was left open.
    ///
    /// The words are still returned, because showing somebody what their line
    /// would have meant is more use than showing them nothing.
    pub unterminated: Option<char>,
}

impl Words {
    /// The command, which is the first word.
    #[must_use]
    pub fn command(&self) -> Option<&str> {
        self.words.first().map(String::as_str)
    }

    /// Everything after the command.
    #[must_use]
    pub fn arguments(&self) -> &[String] {
        self.words.get(1..).unwrap_or(&[])
    }

    /// The argument at `index`, counting from the one after the command.
    #[must_use]
    pub fn argument(&self, index: usize) -> Option<&str> {
        self.arguments().get(index).map(String::as_str)
    }

    /// Everything after the command, joined back together with single spaces.
    ///
    /// For the commands that take a sentence rather than a list -- `echo`, and
    /// anything that writes text into a file. The quoting is gone by then,
    /// which is the point: `echo "a  b"` says `a  b` and `echo a  b` says
    /// `a b`, and both are what somebody typing them expects.
    #[must_use]
    pub fn rest(&self) -> String {
        self.arguments().join(" ")
    }
}

/// Split a line into words.
#[must_use]
pub fn split(line: &str) -> Words {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut started = false;
    let mut quote: Option<char> = None;
    let mut escaped = false;

    for character in line.chars() {
        if escaped {
            current.push(character);
            escaped = false;
            started = true;
            continue;
        }

        match quote {
            Some('\'') => {
                if character == '\'' {
                    quote = None;
                } else {
                    current.push(character);
                }
            }
            Some('"') => match character {
                '"' => quote = None,
                '\\' => escaped = true,
                _ => current.push(character),
            },
            Some(_) => unreachable!("only two kinds of quote"),
            None => match character {
                '\'' | '"' => {
                    quote = Some(character);
                    // An empty quoted string is still a word: `cat ""` asks
                    // about a file with no name, and turning that into no
                    // argument at all would make it ask about nothing.
                    started = true;
                }
                '\\' => escaped = true,
                _ if character.is_whitespace() => {
                    if started {
                        words.push(core::mem::take(&mut current));
                        started = false;
                    }
                }
                _ => {
                    current.push(character);
                    started = true;
                }
            },
        }
    }

    if started || !current.is_empty() {
        words.push(current);
    }

    Words {
        words,
        // A trailing backslash is an unterminated escape, which is the same
        // kind of mistake and worth the same warning.
        unterminated: quote.or(if escaped { Some('\\') } else { None }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    fn words(line: &str) -> Vec<String> {
        split(line).words
    }

    #[test]
    fn a_blank_line_is_no_words() {
        assert!(words("").is_empty());
        assert!(words("   \t ").is_empty());
        assert_eq!(split("").command(), None);
    }

    #[test]
    fn runs_of_space_separate_once() {
        assert_eq!(words("ls   -l    /bin"), vec!["ls", "-l", "/bin"]);
    }

    #[test]
    fn single_quotes_take_everything() {
        assert_eq!(words("cat 'a file.txt'"), vec!["cat", "a file.txt"]);
        assert_eq!(words(r"echo 'a\b'"), vec!["echo", r"a\b"]);
    }

    #[test]
    fn double_quotes_let_a_backslash_escape() {
        assert_eq!(words(r#"echo "a\"b""#), vec!["echo", "a\"b"]);
        assert_eq!(words(r#"cat "a file.txt""#), vec!["cat", "a file.txt"]);
    }

    #[test]
    fn a_backslash_outside_quotes_escapes_a_space() {
        assert_eq!(words(r"cat a\ file.txt"), vec!["cat", "a file.txt"]);
    }

    #[test]
    fn quotes_can_be_stuck_to_a_word() {
        assert_eq!(words(r#"say"hello there""#), vec!["sayhello there"]);
    }

    #[test]
    fn an_empty_quoted_word_is_still_a_word() {
        assert_eq!(words(r#"cat "" x"#), vec!["cat", "", "x"]);
    }

    #[test]
    fn an_unterminated_quote_is_reported_and_the_words_still_come_back() {
        let read = split(r#"cat "never closed"#);
        assert_eq!(read.words, vec!["cat", "never closed"]);
        assert_eq!(read.unterminated, Some('"'));

        let escaped = split(r"echo a\");
        assert_eq!(escaped.unterminated, Some('\\'));
    }

    #[test]
    fn the_command_and_its_arguments_come_apart() {
        let read = split("write notes.txt hello there");
        assert_eq!(read.command(), Some("write"));
        assert_eq!(read.arguments().len(), 3);
        assert_eq!(read.argument(0), Some("notes.txt"));
        assert_eq!(read.rest(), "notes.txt hello there");
    }

    #[test]
    fn quoting_is_gone_by_the_time_the_rest_is_joined() {
        assert_eq!(split(r#"echo "a  b" c"#).rest(), "a  b c");
        assert_eq!(split("echo a  b").rest(), "a b");
    }
}
