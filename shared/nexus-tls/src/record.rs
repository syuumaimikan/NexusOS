//! The TLS record layer: RFC 8446 §5.
//!
//! Everything on a TLS connection travels in records of at most sixteen
//! kibibytes, each with a five-byte header. Before the handshake has produced
//! keys they are sent in the clear; after, every one is an AEAD sealed box with
//! its header as the additional data.
//!
//! # The header is authenticated but not encrypted
//!
//! That is the point of using it as the AEAD's additional data. An attacker can
//! see how long each record is and cannot change it: a length altered in flight
//! makes the tag fail. Both halves matter, and an implementation that leaves
//! the header out of the additional data has a connection an attacker can
//! re-frame.
//!
//! # The content type moves inside
//!
//! In TLS 1.3 an encrypted record's outer type is always `application_data`,
//! whatever it really carries, and the true type is the **last non-zero byte**
//! of the plaintext. That is what hides handshake traffic from somebody
//! watching, and it is why decrypting has to walk backwards past the padding
//! rather than reading a field.

use alloc::vec::Vec;

use nexus_crypto::chacha;

use crate::schedule::Keys;

/// The five-byte header every record starts with.
pub const HEADER: usize = 5;

/// The most a record may carry, before encryption. RFC 8446 §5.1.
pub const MOST_PLAINTEXT: usize = 16384;

/// And after, which allows for the content type and the tag.
pub const MOST_CIPHERTEXT: usize = MOST_PLAINTEXT + 256;

/// What a record carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Kind {
    ChangeCipherSpec = 20,
    Alert = 21,
    Handshake = 22,
    ApplicationData = 23,
}

impl Kind {
    #[must_use]
    pub fn from_byte(byte: u8) -> Option<Self> {
        Some(match byte {
            20 => Self::ChangeCipherSpec,
            21 => Self::Alert,
            22 => Self::Handshake,
            23 => Self::ApplicationData,
            _ => return None,
        })
    }
}

/// Why a record could not be read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trouble {
    /// Not enough bytes yet. Not an error: read more and try again.
    Incomplete,
    /// A length or a type that cannot be right.
    Malformed,
    /// A record longer than the specification allows.
    TooLong,
    /// The tag did not match. The connection is over: RFC 8446 §5.2 says a
    /// failed decryption is fatal, and it means it.
    Undecryptable,
    /// The peer sent a content type this does not handle.
    Unexpected(u8),
}

impl core::fmt::Display for Trouble {
    fn fmt(&self, out: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Incomplete => out.write_str("the record is not all here yet"),
            Self::Malformed => out.write_str("the record's header does not make sense"),
            Self::TooLong => out.write_str("the record is longer than TLS allows"),
            Self::Undecryptable => {
                out.write_str("a record would not decrypt; the connection cannot continue")
            }
            Self::Unexpected(byte) => {
                write!(
                    out,
                    "the peer sent a record of type {byte}, which is not expected"
                )
            }
        }
    }
}

/// One record, read out of a stream of bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    pub kind: Kind,
    pub body: Vec<u8>,
    /// How many bytes of the input this took, so the caller can advance.
    pub consumed: usize,
}

/// Read one record's header and body out of `bytes`, without decrypting.
///
/// # Errors
///
/// [`Trouble::Incomplete`] when the record is not all there, which is the
/// ordinary case on a stream and is why it is distinguished from the rest.
pub fn read(bytes: &[u8]) -> Result<Record, Trouble> {
    if bytes.len() < HEADER {
        return Err(Trouble::Incomplete);
    }
    let Some(kind) = Kind::from_byte(bytes[0]) else {
        return Err(Trouble::Unexpected(bytes[0]));
    };
    // The version field is legacy and says 0x0303 on every TLS 1.3 record
    // whatever the version really is. Not checked: RFC 8446 §5.1 says a client
    // must accept anything here, because middleboxes rewrite it.
    let length = usize::from(u16::from_be_bytes([bytes[3], bytes[4]]));
    if length > MOST_CIPHERTEXT {
        return Err(Trouble::TooLong);
    }
    if bytes.len() < HEADER + length {
        return Err(Trouble::Incomplete);
    }

    Ok(Record {
        kind,
        body: bytes[HEADER..HEADER + length].to_vec(),
        consumed: HEADER + length,
    })
}

/// Frame a record, unencrypted.
#[must_use]
pub fn frame(kind: Kind, body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(HEADER + body.len());
    out.push(kind as u8);
    // 0x0303 -- "TLS 1.2" -- on every record, because that is what gets through
    // the middleboxes that TLS 1.3 was designed around.
    out.extend_from_slice(&[0x03, 0x03]);
    out.extend_from_slice(&(body.len() as u16).to_be_bytes());
    out.extend_from_slice(body);
    out
}

/// One direction of an encrypted connection.
///
/// The sequence number lives here because it is per-direction and per-key: it
/// starts at zero when the keys change and counts records, and a nonce reused
/// with one key is the one mistake in AEAD that loses everything at once.
pub struct Sealed {
    keys: Keys,
    sequence: u64,
}

impl Sealed {
    #[must_use]
    pub fn new(keys: Keys) -> Self {
        Self { keys, sequence: 0 }
    }

    /// Start again with new keys.
    ///
    /// The sequence number goes back to zero, which RFC 8446 §5.3 requires: it
    /// is a counter for *this* key, and carrying it over would waste nonce
    /// space for no reason while looking like caution.
    pub fn rekey(&mut self, keys: Keys) {
        self.keys = keys;
        self.sequence = 0;
    }

    /// Seal one record.
    ///
    /// Returns the whole record, header and all, ready to send.
    #[must_use]
    pub fn seal(&mut self, kind: Kind, body: &[u8]) -> Vec<u8> {
        // The inner plaintext: the body, then the real content type. No padding
        // is added -- it is allowed and it hides message lengths, and a client
        // that padded would be paying for a property it does not otherwise try
        // to have.
        let inner_len = body.len() + 1;
        let total = inner_len + chacha::TAG;

        let mut out = Vec::with_capacity(HEADER + total);
        out.push(Kind::ApplicationData as u8);
        out.extend_from_slice(&[0x03, 0x03]);
        out.extend_from_slice(&(total as u16).to_be_bytes());

        // The header just written is the additional data, which is what binds
        // the length to the contents.
        let mut header = [0u8; HEADER];
        header.copy_from_slice(&out[..HEADER]);

        out.extend_from_slice(body);
        out.push(kind as u8);
        out.resize(HEADER + total, 0);

        let nonce = self.keys.nonce(self.sequence);
        self.sequence += 1;
        let sealed = chacha::seal(
            &self.keys.key,
            &nonce,
            &header,
            &mut out[HEADER..],
            0..inner_len,
        );
        debug_assert!(sealed, "the record buffer was sized wrong");
        out
    }

    /// Open one record, whose header is `header` and body is `body`.
    ///
    /// Returns what it really was and its contents.
    ///
    /// # Errors
    ///
    /// [`Trouble::Undecryptable`] if the tag does not match, which ends the
    /// connection. [`Trouble::Malformed`] if the plaintext is all padding and
    /// has no content type in it, which is a peer that is not following the
    /// specification.
    pub fn open(
        &mut self,
        header: &[u8; HEADER],
        body: &mut Vec<u8>,
    ) -> Result<(Kind, Vec<u8>), Trouble> {
        let nonce = self.keys.nonce(self.sequence);
        // Counted whether or not it works. A failed record is still a record
        // the peer sent, and the specification ends the connection on one --
        // so there is no case where the number should stay where it was.
        self.sequence += 1;

        let Some(length) = chacha::open(&self.keys.key, &nonce, header, body) else {
            return Err(Trouble::Undecryptable);
        };
        body.truncate(length);

        // The real content type is the last non-zero byte; everything after it
        // is padding. Walking backwards rather than reading a field is what
        // makes the padding free to be any length.
        let Some(at) = body.iter().rposition(|byte| *byte != 0) else {
            return Err(Trouble::Malformed);
        };
        let Some(kind) = Kind::from_byte(body[at]) else {
            return Err(Trouble::Unexpected(body[at]));
        };
        body.truncate(at);
        Ok((kind, core::mem::take(body)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::schedule::{Keys, IV, KEY};

    fn keys() -> Keys {
        Keys {
            key: [0x2a; KEY],
            iv: [0x5c; IV],
        }
    }

    #[test]
    fn a_framed_record_reads_back() {
        let framed = frame(Kind::Handshake, b"hello");
        assert_eq!(framed[0], 22);
        assert_eq!(&framed[1..3], &[0x03, 0x03]);
        assert_eq!(&framed[3..5], &[0, 5]);

        let record = read(&framed).unwrap();
        assert_eq!(record.kind, Kind::Handshake);
        assert_eq!(record.body, b"hello");
        assert_eq!(record.consumed, framed.len());
    }

    #[test]
    fn a_record_that_is_not_all_there_says_so_rather_than_failing() {
        // The ordinary case on a stream, and the reason it has its own variant:
        // a caller that treated it as an error would close a connection every
        // time a record crossed a packet boundary.
        let framed = frame(Kind::Handshake, b"hello there");
        for cut in 0..framed.len() {
            assert_eq!(
                read(&framed[..cut]).unwrap_err(),
                Trouble::Incomplete,
                "{cut}"
            );
        }
        assert!(read(&framed).is_ok());
    }

    #[test]
    fn several_records_in_one_buffer_are_read_one_at_a_time() {
        let mut stream = frame(Kind::Handshake, b"one");
        stream.extend_from_slice(&frame(Kind::ApplicationData, b"two"));
        stream.extend_from_slice(&frame(Kind::Alert, b"three"));

        let mut at = 0;
        let mut seen = alloc::vec::Vec::new();
        while at < stream.len() {
            let record = read(&stream[at..]).unwrap();
            at += record.consumed;
            seen.push((record.kind, record.body));
        }
        assert_eq!(seen.len(), 3);
        assert_eq!(seen[0].0, Kind::Handshake);
        assert_eq!(seen[1].1, b"two");
        assert_eq!(seen[2].0, Kind::Alert);
    }

    #[test]
    fn a_record_longer_than_tls_allows_is_refused() {
        let mut header = [0u8; HEADER];
        header[0] = 23;
        header[1] = 3;
        header[2] = 3;
        header[3..].copy_from_slice(&(u16::MAX).to_be_bytes());
        assert_eq!(read(&header).unwrap_err(), Trouble::TooLong);
    }

    #[test]
    fn a_content_type_nobody_defined_is_named_rather_than_guessed() {
        let mut framed = frame(Kind::Handshake, b"x");
        framed[0] = 99;
        assert_eq!(read(&framed).unwrap_err(), Trouble::Unexpected(99));
    }

    #[test]
    fn a_sealed_record_opens_as_what_it_was() {
        let mut out = Sealed::new(keys());
        let mut back = Sealed::new(keys());

        let sealed = out.seal(Kind::Handshake, b"a handshake message");
        // The outer type says application data, whatever is inside. That is
        // what hides handshake traffic from anybody watching.
        assert_eq!(sealed[0], Kind::ApplicationData as u8);

        let record = read(&sealed).unwrap();
        let mut header = [0u8; HEADER];
        header.copy_from_slice(&sealed[..HEADER]);
        let mut body = record.body;
        let (kind, plain) = back.open(&header, &mut body).unwrap();
        assert_eq!(kind, Kind::Handshake);
        assert_eq!(plain, b"a handshake message");
    }

    #[test]
    fn records_are_bound_to_their_order() {
        // The sequence number is in the nonce, so the second record cannot be
        // opened as the first. Without that, an attacker could replay or
        // reorder records freely.
        let mut out = Sealed::new(keys());
        let first = out.seal(Kind::ApplicationData, b"first");
        let second = out.seal(Kind::ApplicationData, b"second");

        let mut back = Sealed::new(keys());
        let mut header = [0u8; HEADER];
        header.copy_from_slice(&second[..HEADER]);
        let mut body = read(&second).unwrap().body;
        assert_eq!(
            back.open(&header, &mut body).unwrap_err(),
            Trouble::Undecryptable,
            "the second record must not open as the first"
        );

        // In order it works.
        let mut back = Sealed::new(keys());
        for record in [&first, &second] {
            let mut header = [0u8; HEADER];
            header.copy_from_slice(&record[..HEADER]);
            let mut body = read(record).unwrap().body;
            assert!(back.open(&header, &mut body).is_ok());
        }
    }

    #[test]
    fn a_changed_length_in_the_header_is_caught() {
        // The header is the additional data, so this is the test that it is
        // authenticated. Without it an attacker could re-frame the stream.
        let mut out = Sealed::new(keys());
        let sealed = out.seal(Kind::ApplicationData, b"the quick brown fox");

        let mut header = [0u8; HEADER];
        header.copy_from_slice(&sealed[..HEADER]);
        header[4] = header[4].wrapping_sub(1);
        let mut body = read(&sealed).unwrap().body;

        let mut back = Sealed::new(keys());
        assert_eq!(
            back.open(&header, &mut body).unwrap_err(),
            Trouble::Undecryptable
        );
    }

    #[test]
    fn a_changed_byte_of_ciphertext_is_caught() {
        let mut out = Sealed::new(keys());
        let mut sealed = out.seal(Kind::ApplicationData, b"the quick brown fox");
        sealed[HEADER + 2] ^= 1;

        let mut header = [0u8; HEADER];
        header.copy_from_slice(&sealed[..HEADER]);
        let mut body = read(&sealed).unwrap().body;

        let mut back = Sealed::new(keys());
        assert_eq!(
            back.open(&header, &mut body).unwrap_err(),
            Trouble::Undecryptable
        );
    }

    #[test]
    fn rekeying_starts_the_sequence_again() {
        let mut out = Sealed::new(keys());
        // Two records sealed and thrown away, only to move the sequence
        // number on -- which is the thing this test is about.
        let _ = out.seal(Kind::ApplicationData, b"one");
        let _ = out.seal(Kind::ApplicationData, b"two");
        out.rekey(keys());
        let after = out.seal(Kind::ApplicationData, b"three");

        // A fresh reader, at sequence zero, can open it -- which is what
        // "starts again" means and what the handshake relies on when it moves
        // from handshake keys to application keys.
        let mut back = Sealed::new(keys());
        let mut header = [0u8; HEADER];
        header.copy_from_slice(&after[..HEADER]);
        let mut body = read(&after).unwrap().body;
        let (kind, plain) = back.open(&header, &mut body).unwrap();
        assert_eq!(kind, Kind::ApplicationData);
        assert_eq!(plain, b"three");
    }

    #[test]
    fn an_empty_body_still_seals_and_opens() {
        // TLS sends empty records, and the inner plaintext is then one byte of
        // content type and nothing else -- the edge of the padding walk.
        let mut out = Sealed::new(keys());
        let sealed = out.seal(Kind::ApplicationData, b"");
        let mut header = [0u8; HEADER];
        header.copy_from_slice(&sealed[..HEADER]);
        let mut body = read(&sealed).unwrap().body;

        let mut back = Sealed::new(keys());
        let (kind, plain) = back.open(&header, &mut body).unwrap();
        assert_eq!(kind, Kind::ApplicationData);
        assert!(plain.is_empty());
    }

    #[test]
    fn padding_after_the_content_type_is_walked_past() {
        // Built by hand, because this implementation does not pad -- but every
        // server may, and a reader that took the last byte rather than the last
        // non-zero one would see a content type of zero on every padded record.
        let mut inner = b"hello".to_vec();
        inner.push(Kind::Handshake as u8);
        inner.extend_from_slice(&[0, 0, 0, 0]);

        let keys = keys();
        let total = inner.len() + chacha::TAG;
        let mut out = alloc::vec::Vec::with_capacity(HEADER + total);
        out.push(Kind::ApplicationData as u8);
        out.extend_from_slice(&[0x03, 0x03]);
        out.extend_from_slice(&(total as u16).to_be_bytes());
        let mut header = [0u8; HEADER];
        header.copy_from_slice(&out[..HEADER]);
        out.extend_from_slice(&inner);
        out.resize(HEADER + total, 0);
        assert!(chacha::seal(
            &keys.key,
            &keys.nonce(0),
            &header,
            &mut out[HEADER..],
            0..inner.len()
        ));

        let mut back = Sealed::new(keys);
        let mut body = read(&out).unwrap().body;
        let (kind, plain) = back.open(&header, &mut body).unwrap();
        assert_eq!(kind, Kind::Handshake);
        assert_eq!(plain, b"hello");
    }

    #[test]
    fn a_plaintext_that_is_all_padding_is_refused() {
        // No content type anywhere in it. A peer that sends this is not
        // following the specification, and guessing a type would be worse than
        // saying so.
        let keys = keys();
        let inner = [0u8; 8];
        let total = inner.len() + chacha::TAG;
        let mut out = alloc::vec::Vec::with_capacity(HEADER + total);
        out.push(Kind::ApplicationData as u8);
        out.extend_from_slice(&[0x03, 0x03]);
        out.extend_from_slice(&(total as u16).to_be_bytes());
        let mut header = [0u8; HEADER];
        header.copy_from_slice(&out[..HEADER]);
        out.extend_from_slice(&inner);
        out.resize(HEADER + total, 0);
        assert!(chacha::seal(
            &keys.key,
            &keys.nonce(0),
            &header,
            &mut out[HEADER..],
            0..inner.len()
        ));

        let mut back = Sealed::new(keys);
        let mut body = read(&out).unwrap().body;
        assert_eq!(
            back.open(&header, &mut body).unwrap_err(),
            Trouble::Malformed
        );
    }

    #[test]
    fn every_body_length_round_trips() {
        let mut out = Sealed::new(keys());
        let mut back = Sealed::new(keys());
        for length in 0..300usize {
            let body: alloc::vec::Vec<u8> = (0..length).map(|index| index as u8).collect();
            let sealed = out.seal(Kind::ApplicationData, &body);
            let mut header = [0u8; HEADER];
            header.copy_from_slice(&sealed[..HEADER]);
            let mut carried = read(&sealed).unwrap().body;
            let (kind, plain) = back.open(&header, &mut carried).unwrap();
            assert_eq!(kind, Kind::ApplicationData, "{length}");
            assert_eq!(plain, body, "length {length}");
        }
    }
}
