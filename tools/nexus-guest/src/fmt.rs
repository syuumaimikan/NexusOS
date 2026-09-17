//! Just enough formatting to say a number.
//!
//! There is no `format!` here: no allocator, no standard library. A program
//! that wants to report a value it did not expect has to turn it into digits
//! itself, and a program that could not would report "something was wrong"
//! instead of what.

/// Room for the longest signed 64-bit number and its sign.
const MOST: usize = 21;

/// A number, as text, in a buffer the caller owns.
pub struct Number {
    digits: [u8; MOST],
    at: usize,
}

impl Number {
    /// Render `value` in base ten.
    #[must_use]
    pub fn of(value: i64) -> Self {
        let mut digits = [0u8; MOST];
        let mut at = MOST;

        // Built from the least significant digit backwards, which is the only
        // direction the arithmetic gives them in.
        //
        // The magnitude is taken as unsigned, because the most negative i64 has
        // no positive counterpart -- negating it overflows, and on a build with
        // overflow checks that is a panic in the code that exists to report a
        // failure.
        let negative = value < 0;
        let mut magnitude = value.unsigned_abs();
        loop {
            at -= 1;
            digits[at] = b'0' + (magnitude % 10) as u8;
            magnitude /= 10;
            if magnitude == 0 {
                break;
            }
        }
        if negative {
            at -= 1;
            digits[at] = b'-';
        }
        Self { digits, at }
    }

    /// The text of it.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.digits[self.at..]
    }
}

/// The most a line assembled here may be.
const LINE: usize = 192;

/// A line built in one buffer and written in one call.
///
/// One `write` and not several, and that is not efficiency. Standard output
/// here is a channel to a log that puts a prefix on every write it is given, so
/// a line assembled from three writes arrives as three lines -- and a test
/// looking for the whole of it never finds it. That was a real failure: a
/// window title went to the log on a line of its own, with the sentence that
/// introduced it on the line before.
///
/// Anything past the end is dropped rather than wrapped. A log line is a thing
/// somebody reads, and half of one is more useful than two halves of one that
/// look like two lines.
pub struct Line {
    bytes: [u8; LINE],
    at: usize,
}

impl Default for Line {
    fn default() -> Self {
        Self::new()
    }
}

impl Line {
    #[must_use]
    pub fn new() -> Self {
        Self {
            bytes: [0u8; LINE],
            at: 0,
        }
    }

    /// Add some text.
    pub fn text(&mut self, text: &str) -> &mut Self {
        for byte in text.as_bytes() {
            if self.at < LINE {
                self.bytes[self.at] = *byte;
                self.at += 1;
            }
        }
        self
    }

    /// Add a number, in base ten.
    pub fn number(&mut self, value: i64) -> &mut Self {
        let number = Number::of(value);
        for byte in number.as_bytes() {
            if self.at < LINE {
                self.bytes[self.at] = *byte;
                self.at += 1;
            }
        }
        self
    }

    /// Write it, with the newline that ends it, in one call.
    pub fn say(&mut self) {
        // The newline is put in here rather than asked of the caller, so that
        // there is no way to build a line and forget it -- and so that it is
        // inside the one write rather than a second one after it.
        if self.at < LINE {
            self.bytes[self.at] = b'\n';
            self.at += 1;
        }
        let _ = crate::write(1, &self.bytes[..self.at]);
    }
}

/// Say a line with one number in it.
pub fn say_with(prefix: &str, value: i64) {
    Line::new().text(prefix).number(value).say();
}
