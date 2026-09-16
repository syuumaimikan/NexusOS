#![no_std]
//! Making characters out of bytes that arrive a few at a time.
//!
//! A message boundary is not a character boundary. A program producing text
//! sends it when it has some, and "some" is a number of bytes -- so a three-byte
//! Japanese character can arrive as two bytes in one message and one in the
//! next. A reader that called `from_utf8` on each message would see a failure
//! on the first message and a failure on the second, and would print two
//! replacement marks where there is one perfectly good character.
//!
//! This is the little state machine that fixes that, and it is here rather than
//! in each program because every program that reads text from a channel needs
//! it and none of them should have to know why.
//!
//! In `shared/` rather than beside the program that wanted it, because it is
//! the one piece of that route with logic in it -- and logic that fails
//! *quietly*, by printing a replacement mark instead of a character. A thing
//! that can be wrong without saying so has to be testable on the host, and
//! this is.

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;

/// Bytes in, characters out, holding what is not a character yet.
#[derive(Default)]
pub struct Utf8 {
    /// The start of a sequence whose remaining bytes have not arrived.
    ///
    /// At most three bytes: the longest UTF-8 sequence is four, and a held one
    /// is by definition incomplete.
    held: Vec<u8>,
}

impl Utf8 {
    /// Nothing held.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Everything in `bytes` that completes a character, with the rest kept.
    ///
    /// Invalid bytes -- not a truncated sequence, but one that cannot begin a
    /// valid character at all -- become the replacement character, one per
    /// offending byte, rather than being dropped. A program that silently drops
    /// what it cannot read is a program whose output is subtly wrong and says
    /// nothing about it.
    pub fn push(&mut self, bytes: &[u8]) -> String {
        self.held.extend_from_slice(bytes);

        let mut out = String::new();
        loop {
            match core::str::from_utf8(&self.held) {
                Ok(text) => {
                    out.push_str(text);
                    self.held.clear();
                    return out;
                }
                Err(error) => {
                    let good = error.valid_up_to();
                    // SAFETY-free: `valid_up_to` is exactly the length that
                    // parsed, so this cannot fail.
                    if let Ok(text) = core::str::from_utf8(&self.held[..good]) {
                        out.push_str(text);
                    }
                    match error.error_len() {
                        // Genuinely invalid: skip that many bytes and say so.
                        Some(bad) => {
                            out.push(char::REPLACEMENT_CHARACTER);
                            self.held.drain(..good + bad);
                        }
                        // Truncated: keep it and wait for the rest.
                        None => {
                            self.held.drain(..good);
                            return out;
                        }
                    }
                }
            }
        }
    }

    /// Whatever is still held, as replacement characters.
    ///
    /// For the end of a stream: bytes that were waiting for the rest of their
    /// character and will not get it. Called when the far end has closed, so
    /// that a truncated last character is visible rather than silently missing.
    pub fn finish(&mut self) -> String {
        let mut out = String::new();
        for _ in 0..self.held.len() {
            out.push(char::REPLACEMENT_CHARACTER);
        }
        self.held.clear();
        out
    }

    /// Whether anything is waiting for the rest of its character.
    #[must_use]
    pub fn pending(&self) -> bool {
        !self.held.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::Utf8;
    use alloc::string::String;

    /// The case this exists for: one character, split across two messages.
    #[test]
    fn a_character_split_across_two_messages() {
        // U+3042 HIRAGANA A is e3 81 82.
        let mut text = Utf8::new();
        assert_eq!(text.push(&[0xE3, 0x81]), "");
        assert!(text.pending());
        assert_eq!(text.push(&[0x82]), "\u{3042}");
        assert!(!text.pending());
    }

    /// And split at every possible place, because an off-by-one in the holding
    /// is the way this goes wrong.
    #[test]
    fn split_at_every_offset() {
        let whole = "\u{65E5}\u{672C}\u{8A9E}\u{306E}\u{30C6}\u{30AD}\u{30B9}\u{30C8}";
        let bytes = whole.as_bytes();
        for cut in 0..=bytes.len() {
            let mut text = Utf8::new();
            let mut out = String::new();
            out.push_str(&text.push(&bytes[..cut]));
            out.push_str(&text.push(&bytes[cut..]));
            assert_eq!(out, whole, "cut at {cut}");
            assert!(!text.pending(), "cut at {cut} left something held");
        }
    }

    /// A byte at a time, which is the worst a producer can do.
    #[test]
    fn one_byte_at_a_time() {
        let whole = "a\u{3042}i\u{3046}e";
        let mut text = Utf8::new();
        let mut out = String::new();
        for byte in whole.as_bytes() {
            out.push_str(&text.push(&[*byte]));
        }
        assert_eq!(out, whole);
    }

    /// Ascii is unchanged and never held.
    #[test]
    fn ascii_passes_straight_through() {
        let mut text = Utf8::new();
        assert_eq!(text.push(b"hello"), "hello");
        assert!(!text.pending());
    }

    /// A byte that cannot begin a character becomes one replacement mark, and
    /// what follows it still arrives. Dropping it silently is the failure this
    /// exists to prevent, so it is checked rather than assumed.
    #[test]
    fn an_impossible_byte_is_reported_not_dropped() {
        let mut text = Utf8::new();
        assert_eq!(text.push(&[b'a', 0xFF, b'b']), "a\u{FFFD}b");
        assert!(!text.pending());
    }

    /// A stream that stops in the middle of a character says so.
    #[test]
    fn a_truncated_ending_is_visible() {
        let mut text = Utf8::new();
        assert_eq!(text.push(&[0xE3, 0x81]), "");
        assert_eq!(text.finish(), "\u{FFFD}\u{FFFD}");
        assert!(!text.pending());
    }

    /// Nothing in, nothing out, and nothing held.
    #[test]
    fn nothing_is_nothing() {
        let mut text = Utf8::new();
        assert_eq!(text.push(&[]), "");
        assert_eq!(text.finish(), "");
    }

    /// The four-byte case, which is the one a three-byte assumption breaks on.
    #[test]
    fn four_byte_characters() {
        let whole = "\u{1F600}\u{1F601}";
        let bytes = whole.as_bytes();
        for cut in 0..=bytes.len() {
            let mut text = Utf8::new();
            let mut out = String::new();
            out.push_str(&text.push(&bytes[..cut]));
            out.push_str(&text.push(&bytes[cut..]));
            assert_eq!(out, whole, "cut at {cut}");
        }
    }
}
