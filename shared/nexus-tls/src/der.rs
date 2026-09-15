//! DER, the encoding certificates are written in.
//!
//! ASN.1 has several encodings; DER is the one where every value has exactly
//! one representation, which is what makes a signature over a structure
//! meaningful. This reads it.
//!
//! # Every byte here came from an attacker
//!
//! A certificate arrives from whoever answered the connection, *before*
//! anything has been verified — so this parser runs on hostile input by
//! definition, earlier in the process than any other part of TLS. It cannot
//! panic, cannot run past the end of its buffer, and cannot recurse without
//! bound.
//!
//! # What it refuses that a lenient parser would accept
//!
//! DER's whole point is that there is one encoding per value, and a parser that
//! accepts the others gives up the property the signature relies on. So:
//!
//! * a length in more bytes than it needs is refused, because the same value
//!   written two ways would let two different byte strings mean one certificate
//! * the indefinite-length form is refused; it is BER and not DER
//! * a length with a leading zero byte is refused, for the same reason
//! * nesting is bounded, because a certificate is a fixed shape and anything
//!   deeper is something trying to use the stack
//!
//! None of these matter for certificates that real authorities issue. They
//! matter for the ones somebody writes to get past a checker.

use alloc::vec::Vec;

/// How deep a structure may nest.
///
/// A certificate is about six levels at its deepest. Sixteen is room to spare
/// and is far short of what it would take to run out of stack.
const DEPTH: usize = 16;

/// The tags this needs to know by name.
pub mod tag {
    pub const BOOLEAN: u8 = 0x01;
    pub const INTEGER: u8 = 0x02;
    pub const BIT_STRING: u8 = 0x03;
    pub const OCTET_STRING: u8 = 0x04;
    pub const NULL: u8 = 0x05;
    pub const OID: u8 = 0x06;
    pub const UTF8_STRING: u8 = 0x0C;
    pub const PRINTABLE_STRING: u8 = 0x13;
    pub const IA5_STRING: u8 = 0x16;
    pub const UTC_TIME: u8 = 0x17;
    pub const GENERALIZED_TIME: u8 = 0x18;
    pub const SEQUENCE: u8 = 0x30;
    pub const SET: u8 = 0x31;

    /// A context-specific tag, constructed: `[n]` in the ASN.1.
    #[must_use]
    pub const fn context(number: u8) -> u8 {
        0xA0 | number
    }

    /// A context-specific tag, primitive: `[n] IMPLICIT` over a simple type.
    #[must_use]
    pub const fn context_primitive(number: u8) -> u8 {
        0x80 | number
    }
}

/// Why some DER could not be read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trouble {
    /// It ended in the middle of something.
    Short,
    /// A length is encoded in a way DER does not allow.
    BadLength,
    /// A tag that is not what was expected here.
    WrongTag { wanted: u8, found: u8 },
    /// Nested deeper than anything real.
    TooDeep,
    /// A value whose contents do not make sense for its tag.
    Malformed(&'static str),
}

impl core::fmt::Display for Trouble {
    fn fmt(&self, out: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Short => out.write_str("the certificate ended in the middle of a value"),
            Self::BadLength => out.write_str("a length is not encoded the one way DER allows"),
            Self::WrongTag { wanted, found } => {
                write!(out, "expected tag {wanted:#04x} and found {found:#04x}")
            }
            Self::TooDeep => out.write_str("the structure nests deeper than any certificate does"),
            Self::Malformed(what) => write!(out, "a value is malformed: {what}"),
        }
    }
}

/// One tag-length-value, with its contents and its whole encoding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Value<'a> {
    pub tag: u8,
    /// The contents, without the tag or the length.
    pub body: &'a [u8],
    /// Tag, length and contents together -- which is what a signature is over.
    pub whole: &'a [u8],
}

/// A position in some DER.
#[derive(Clone)]
pub struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
    depth: usize,
}

impl<'a> Reader<'a> {
    #[must_use]
    pub fn new(bytes: &'a [u8]) -> Self {
        Self {
            bytes,
            at: 0,
            depth: 0,
        }
    }

    /// Whether everything has been read.
    #[must_use]
    pub fn done(&self) -> bool {
        self.at >= self.bytes.len()
    }

    /// What tag comes next, without moving.
    #[must_use]
    pub fn peek(&self) -> Option<u8> {
        self.bytes.get(self.at).copied()
    }

    /// Read the next value, whatever its tag.
    pub fn any(&mut self) -> Result<Value<'a>, Trouble> {
        let start = self.at;
        let tag = *self.bytes.get(self.at).ok_or(Trouble::Short)?;
        self.at += 1;

        let first = *self.bytes.get(self.at).ok_or(Trouble::Short)?;
        self.at += 1;

        let length = if first < 0x80 {
            // The short form: the length is the byte.
            usize::from(first)
        } else if first == 0x80 {
            // Indefinite length. Legal in BER, forbidden in DER, and the shape
            // a parser that accepts it can be walked off the end of.
            return Err(Trouble::BadLength);
        } else if first == 0xFF {
            return Err(Trouble::BadLength);
        } else {
            let count = usize::from(first & 0x7F);
            // More than eight bytes of length is a number no buffer can hold,
            // and more than the machine's pointer width is an overflow waiting
            // to happen.
            if count > 8 {
                return Err(Trouble::BadLength);
            }
            let bytes = self
                .bytes
                .get(self.at..self.at + count)
                .ok_or(Trouble::Short)?;
            self.at += count;

            // A leading zero means it could have been written shorter, and DER
            // says it must be. Two encodings of one length is two byte strings
            // meaning one certificate, which is exactly what a signature is
            // supposed to rule out.
            if bytes[0] == 0 {
                return Err(Trouble::BadLength);
            }
            let mut length = 0usize;
            for byte in bytes {
                length = length.checked_mul(256).ok_or(Trouble::BadLength)?;
                length = length
                    .checked_add(usize::from(*byte))
                    .ok_or(Trouble::BadLength)?;
            }
            // And the short form must be used where it fits.
            if length < 0x80 {
                return Err(Trouble::BadLength);
            }
            length
        };

        let end = self.at.checked_add(length).ok_or(Trouble::Short)?;
        let body = self.bytes.get(self.at..end).ok_or(Trouble::Short)?;
        self.at = end;

        Ok(Value {
            tag,
            body,
            whole: self.bytes.get(start..end).ok_or(Trouble::Short)?,
        })
    }

    /// Read the next value and require its tag.
    pub fn expect(&mut self, tag: u8) -> Result<Value<'a>, Trouble> {
        let value = self.any()?;
        if value.tag != tag {
            return Err(Trouble::WrongTag {
                wanted: tag,
                found: value.tag,
            });
        }
        Ok(value)
    }

    /// Read the next value if it has this tag, and otherwise leave it.
    ///
    /// For the OPTIONAL fields a certificate is full of.
    pub fn optional(&mut self, tag: u8) -> Result<Option<Value<'a>>, Trouble> {
        if self.peek() != Some(tag) {
            return Ok(None);
        }
        self.expect(tag).map(Some)
    }

    /// A reader over a nested structure's contents.
    pub fn nested(&mut self, tag: u8) -> Result<Reader<'a>, Trouble> {
        if self.depth + 1 >= DEPTH {
            return Err(Trouble::TooDeep);
        }
        let value = self.expect(tag)?;
        Ok(Reader {
            bytes: value.body,
            at: 0,
            depth: self.depth + 1,
        })
    }

    /// A reader over a nested structure, if the next value is one.
    pub fn nested_optional(&mut self, tag: u8) -> Result<Option<Reader<'a>>, Trouble> {
        if self.peek() != Some(tag) {
            return Ok(None);
        }
        self.nested(tag).map(Some)
    }
}

/// The contents of a BIT STRING, refusing any that does not end on a byte.
///
/// The first byte says how many bits of the last byte are padding. Everything a
/// certificate uses a BIT STRING for -- a signature, a public key -- is a whole
/// number of bytes, so anything else is malformed rather than something to
/// handle.
pub fn bit_string<'a>(value: &Value<'a>) -> Result<&'a [u8], Trouble> {
    if value.tag != tag::BIT_STRING {
        return Err(Trouble::WrongTag {
            wanted: tag::BIT_STRING,
            found: value.tag,
        });
    }
    let (unused, bits) = value
        .body
        .split_first()
        .ok_or(Trouble::Malformed("an empty bit string"))?;
    if *unused != 0 {
        return Err(Trouble::Malformed(
            "a bit string that does not end on a byte",
        ));
    }
    Ok(bits)
}

/// An INTEGER's bytes, with the sign byte removed.
///
/// DER integers are signed and two's complement, so a positive number whose
/// top bit is set carries a leading zero. Every integer in a certificate that
/// this looks at -- a modulus, an exponent, a signature component -- is
/// positive, so the leading zero is stripped and a genuinely negative one is
/// refused.
pub fn positive_integer<'a>(value: &Value<'a>) -> Result<&'a [u8], Trouble> {
    if value.tag != tag::INTEGER {
        return Err(Trouble::WrongTag {
            wanted: tag::INTEGER,
            found: value.tag,
        });
    }
    let bytes = value.body;
    if bytes.is_empty() {
        return Err(Trouble::Malformed("an empty integer"));
    }
    if bytes[0] & 0x80 != 0 {
        return Err(Trouble::Malformed("a negative integer where one cannot be"));
    }
    // DER requires the shortest form: a leading zero is allowed only to keep
    // the number positive, so two of them, or one before a byte whose top bit
    // is clear, is not minimal.
    if bytes.len() > 1 && bytes[0] == 0 && bytes[1] & 0x80 == 0 {
        return Err(Trouble::Malformed(
            "an integer with a needless leading zero",
        ));
    }
    Ok(bytes.strip_prefix(&[0]).unwrap_or(bytes))
}

/// An object identifier, as the dotted decimal a person reads.
///
/// Only used for saying what an unknown algorithm was, in a message. Matching
/// is done against the encoded bytes, which is exact and needs no allocation.
#[must_use]
pub fn oid_text(bytes: &[u8]) -> alloc::string::String {
    use core::fmt::Write as _;
    let mut text = alloc::string::String::new();
    let Some((first, rest)) = bytes.split_first() else {
        return text;
    };
    // The first two arcs are packed into one byte: 40 × first + second.
    let _ = write!(text, "{}.{}", first / 40, first % 40);

    let mut value: u64 = 0;
    for byte in rest {
        value = value.wrapping_mul(128).wrapping_add(u64::from(byte & 0x7F));
        if byte & 0x80 == 0 {
            let _ = write!(text, ".{value}");
            value = 0;
        }
    }
    text
}

/// Build a DER value, for tests and for the few things this has to write.
#[must_use]
pub fn write(tag: u8, body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(body.len() + 6);
    out.push(tag);
    let length = body.len();
    if length < 0x80 {
        out.push(length as u8);
    } else {
        // The shortest form that fits, which is what DER requires.
        let bytes = length.to_be_bytes();
        let first = bytes.iter().position(|byte| *byte != 0).unwrap_or(0);
        let used = &bytes[first..];
        out.push(0x80 | used.len() as u8);
        out.extend_from_slice(used);
    }
    out.extend_from_slice(body);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    #[test]
    fn a_short_value_reads_back() {
        let bytes = write(tag::INTEGER, &[0x2a]);
        assert_eq!(bytes, vec![0x02, 0x01, 0x2a]);

        let value = Reader::new(&bytes).expect(tag::INTEGER).unwrap();
        assert_eq!(value.body, &[0x2a]);
        assert_eq!(value.whole, &bytes[..]);
    }

    #[test]
    fn a_long_value_uses_the_long_form() {
        let body = vec![0xAAu8; 300];
        let bytes = write(tag::OCTET_STRING, &body);
        // 0x82 says two length bytes follow: 0x012C is 300.
        assert_eq!(&bytes[..4], &[0x04, 0x82, 0x01, 0x2C]);

        let value = Reader::new(&bytes).expect(tag::OCTET_STRING).unwrap();
        assert_eq!(value.body.len(), 300);
    }

    #[test]
    fn every_length_round_trips() {
        for length in [0usize, 1, 127, 128, 255, 256, 1000, 65535, 65536] {
            let body = vec![0x5Au8; length];
            let bytes = write(tag::OCTET_STRING, &body);
            let value = Reader::new(&bytes).expect(tag::OCTET_STRING).unwrap();
            assert_eq!(value.body.len(), length, "length {length}");
        }
    }

    #[test]
    fn the_indefinite_length_form_is_refused() {
        // Legal in BER, forbidden in DER, and the shape a lenient parser can be
        // walked off the end of.
        let bytes = [0x30, 0x80, 0x00, 0x00];
        assert_eq!(Reader::new(&bytes).any().unwrap_err(), Trouble::BadLength);
    }

    #[test]
    fn a_length_written_longer_than_it_needs_is_refused() {
        // DER has one encoding per value, and a parser that accepts two gives
        // up the property a signature relies on. `0x81 0x05` is five written in
        // the long form, where the short form would do.
        let bytes = [0x02, 0x81, 0x05, 1, 2, 3, 4, 5];
        assert_eq!(Reader::new(&bytes).any().unwrap_err(), Trouble::BadLength);

        // And a leading zero in the length.
        let bytes = [0x02, 0x82, 0x00, 0x05, 1, 2, 3, 4, 5];
        assert_eq!(Reader::new(&bytes).any().unwrap_err(), Trouble::BadLength);
    }

    #[test]
    fn a_length_longer_than_the_buffer_is_refused() {
        let bytes = [0x04, 0x10, 1, 2, 3];
        assert_eq!(Reader::new(&bytes).any().unwrap_err(), Trouble::Short);
    }

    #[test]
    fn every_truncation_of_a_structure_is_refused_rather_than_panicking() {
        // The test that says this is safe on the hostile input it exists to
        // read. A certificate arrives before anything is verified.
        let inner = write(tag::INTEGER, &[1, 2, 3]);
        let mut body = inner.clone();
        body.extend_from_slice(&write(tag::OCTET_STRING, &[4, 5, 6, 7]));
        let whole = write(tag::SEQUENCE, &body);

        for cut in 0..whole.len() {
            let mut reader = Reader::new(&whole[..cut]);
            let _ = reader.nested(tag::SEQUENCE).map(|mut inner| {
                let _ = inner.any();
                let _ = inner.any();
            });
        }
    }

    #[test]
    fn nesting_deeper_than_a_certificate_goes_is_refused() {
        // Built to be deep on purpose. Without the bound, this is a stack
        // overflow that a peer chooses the depth of.
        let mut bytes = write(tag::INTEGER, &[0]);
        for _ in 0..64 {
            bytes = write(tag::SEQUENCE, &bytes);
        }

        let mut reader = Reader::new(&bytes);
        let mut depth = 0;
        loop {
            match reader.nested(tag::SEQUENCE) {
                Ok(inner) => {
                    reader = inner;
                    depth += 1;
                }
                Err(Trouble::TooDeep) => break,
                Err(other) => panic!("expected TooDeep and got {other:?} at depth {depth}"),
            }
        }
        assert!(
            depth < DEPTH,
            "gave up at {depth}, which is within the bound"
        );
    }

    #[test]
    fn a_wrong_tag_says_what_it_wanted_and_what_it_found() {
        let bytes = write(tag::INTEGER, &[1]);
        assert_eq!(
            Reader::new(&bytes).expect(tag::SEQUENCE).unwrap_err(),
            Trouble::WrongTag {
                wanted: tag::SEQUENCE,
                found: tag::INTEGER
            }
        );
    }

    #[test]
    fn an_optional_field_that_is_absent_is_not_an_error() {
        let bytes = write(tag::INTEGER, &[1]);
        let mut reader = Reader::new(&bytes);
        assert!(reader.optional(tag::context(0)).unwrap().is_none());
        // And the integer is still there to be read.
        assert_eq!(reader.expect(tag::INTEGER).unwrap().body, &[1]);
    }

    #[test]
    fn a_bit_string_that_does_not_end_on_a_byte_is_refused() {
        let value = Value {
            tag: tag::BIT_STRING,
            body: &[3, 0xFF],
            whole: &[],
        };
        assert!(bit_string(&value).is_err());

        let value = Value {
            tag: tag::BIT_STRING,
            body: &[0, 0xDE, 0xAD],
            whole: &[],
        };
        assert_eq!(bit_string(&value).unwrap(), &[0xDE, 0xAD]);
    }

    #[test]
    fn a_positive_integer_loses_its_sign_byte() {
        // 0x00 0xFF is 255: the leading zero is there to keep it positive.
        let value = Value {
            tag: tag::INTEGER,
            body: &[0x00, 0xFF],
            whole: &[],
        };
        assert_eq!(positive_integer(&value).unwrap(), &[0xFF]);

        // 0x7F needs no sign byte and keeps its length.
        let value = Value {
            tag: tag::INTEGER,
            body: &[0x7F],
            whole: &[],
        };
        assert_eq!(positive_integer(&value).unwrap(), &[0x7F]);
    }

    #[test]
    fn a_negative_integer_where_one_cannot_be_is_refused() {
        let value = Value {
            tag: tag::INTEGER,
            body: &[0x80, 0x01],
            whole: &[],
        };
        assert!(positive_integer(&value).is_err());
    }

    #[test]
    fn an_integer_with_a_needless_leading_zero_is_refused() {
        // Not minimal, so not DER -- and a modulus written two ways is two
        // certificates that a signature cannot tell apart.
        let value = Value {
            tag: tag::INTEGER,
            body: &[0x00, 0x7F],
            whole: &[],
        };
        assert!(positive_integer(&value).is_err());
    }

    #[test]
    fn an_object_identifier_reads_as_its_dotted_form() {
        // 1.2.840.113549.1.1.11 -- sha256WithRSAEncryption, the algorithm most
        // certificates in the world are signed with.
        let bytes = [0x2A, 0x86, 0x48, 0x86, 0xF7, 0x0D, 0x01, 0x01, 0x0B];
        assert_eq!(oid_text(&bytes), "1.2.840.113549.1.1.11");

        // 1.2.840.10045.4.3.2 -- ecdsa-with-SHA256.
        let bytes = [0x2A, 0x86, 0x48, 0xCE, 0x3D, 0x04, 0x03, 0x02];
        assert_eq!(oid_text(&bytes), "1.2.840.10045.4.3.2");
    }

    #[test]
    fn a_sequence_is_walked_one_value_at_a_time() {
        let mut body = write(tag::INTEGER, &[1]);
        body.extend_from_slice(&write(tag::INTEGER, &[2]));
        body.extend_from_slice(&write(tag::OCTET_STRING, b"three"));
        let bytes = write(tag::SEQUENCE, &body);

        let mut outer = Reader::new(&bytes);
        let mut inner = outer.nested(tag::SEQUENCE).unwrap();
        assert_eq!(inner.expect(tag::INTEGER).unwrap().body, &[1]);
        assert_eq!(inner.expect(tag::INTEGER).unwrap().body, &[2]);
        assert_eq!(inner.expect(tag::OCTET_STRING).unwrap().body, b"three");
        assert!(inner.done());
        assert!(outer.done());
    }

    #[test]
    fn the_whole_of_a_value_is_what_a_signature_would_be_over() {
        // The reason `whole` exists: a certificate's signature is over the
        // encoded TBSCertificate, tag and length included, and re-encoding it
        // from the parsed fields would not reproduce those bytes.
        let body = write(tag::INTEGER, &[42]);
        let bytes = write(tag::SEQUENCE, &body);
        let value = Reader::new(&bytes).expect(tag::SEQUENCE).unwrap();
        assert_eq!(value.whole, &bytes[..]);
        assert_eq!(value.body, &body[..]);
    }
}
