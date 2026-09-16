//! One line, in one `write`.
//!
//! Assembled into a buffer first, and that is not tidiness. The machine logs
//! each `write` as its own line, so a `say` that wrote five pieces produced
//! five log lines and a reader had to reassemble the sentence from them. What
//! this program produces is a list, and a list is only a list if each entry is
//! on one line.

/// The longest line this will write. Anything past it is dropped rather than
/// wrapped: a truncated entry is obvious, and a wrapped one reads as two.
const WIDEST: usize = 160;

pub fn line(parts: &[&str], write: impl Fn(&[u8])) {
    let mut out = [0u8; WIDEST];
    let mut at = 0;
    for part in parts {
        for byte in part.as_bytes() {
            if at < WIDEST - 1 {
                out[at] = *byte;
                at += 1;
            }
        }
    }
    out[at] = b'\n';
    at += 1;
    write(&out[..at]);
}
