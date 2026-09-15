//! Reading and writing the shapes TLS is made of.
//!
//! Every structure in RFC 8446 is fixed fields and length-prefixed vectors,
//! where the prefix is one, two or three bytes. This is those two operations
//! and nothing else.
//!
//! # Why a reader type rather than indexing
//!
//! Because the alternative is `bytes[at..at + 2]` several hundred times, and
//! every one of those is a panic on a message a peer truncated. A TLS client
//! parses bytes an attacker chose, so *every* read has to be a check, and the
//! only way to get that reliably is to make the checked version the easy one.
//!
//! Nothing here can panic on any input.

use alloc::vec::Vec;

/// Ran out of bytes, or a length that cannot be right.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Short;

impl core::fmt::Display for Short {
    fn fmt(&self, out: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        out.write_str("the message ended in the middle of something")
    }
}

/// A position in a byte slice, and the operations that move it.
pub struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    #[must_use]
    pub fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, at: 0 }
    }

    /// How many bytes are left.
    #[must_use]
    pub fn left(&self) -> usize {
        self.bytes.len().saturating_sub(self.at)
    }

    /// Whether everything has been read.
    #[must_use]
    pub fn done(&self) -> bool {
        self.left() == 0
    }

    /// The next `count` bytes.
    pub fn take(&mut self, count: usize) -> Result<&'a [u8], Short> {
        let end = self.at.checked_add(count).ok_or(Short)?;
        let slice = self.bytes.get(self.at..end).ok_or(Short)?;
        self.at = end;
        Ok(slice)
    }

    pub fn u8(&mut self) -> Result<u8, Short> {
        Ok(self.take(1)?[0])
    }

    pub fn u16(&mut self) -> Result<u16, Short> {
        let bytes = self.take(2)?;
        Ok(u16::from_be_bytes([bytes[0], bytes[1]]))
    }

    /// A 24-bit length, which is how TLS spells the long ones.
    pub fn u24(&mut self) -> Result<u32, Short> {
        let bytes = self.take(3)?;
        Ok(u32::from_be_bytes([0, bytes[0], bytes[1], bytes[2]]))
    }

    /// A vector with a one-byte length prefix.
    pub fn vector8(&mut self) -> Result<&'a [u8], Short> {
        let length = usize::from(self.u8()?);
        self.take(length)
    }

    /// A vector with a two-byte length prefix.
    pub fn vector16(&mut self) -> Result<&'a [u8], Short> {
        let length = usize::from(self.u16()?);
        self.take(length)
    }

    /// A vector with a three-byte length prefix.
    pub fn vector24(&mut self) -> Result<&'a [u8], Short> {
        let length = self.u24()? as usize;
        self.take(length)
    }

    /// A reader over a two-byte-prefixed vector, for nesting.
    pub fn nested16(&mut self) -> Result<Reader<'a>, Short> {
        Ok(Reader::new(self.vector16()?))
    }
}

/// A growing buffer, and the operations that fill it.
#[derive(Default)]
pub struct Writer(Vec<u8>);

impl Writer {
    #[must_use]
    pub fn new() -> Self {
        Self(Vec::new())
    }

    #[must_use]
    pub fn into_bytes(self) -> Vec<u8> {
        self.0
    }

    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }

    pub fn u8(&mut self, value: u8) {
        self.0.push(value);
    }

    pub fn u16(&mut self, value: u16) {
        self.0.extend_from_slice(&value.to_be_bytes());
    }

    pub fn bytes(&mut self, value: &[u8]) {
        self.0.extend_from_slice(value);
    }

    /// Write a vector with a one-byte length prefix.
    ///
    /// Silently refuses to write a body that will not fit the prefix, and says
    /// so. A caller that ignored that would produce a message whose length
    /// field disagrees with its contents, which a peer reads as a different
    /// message entirely.
    #[must_use]
    pub fn vector8(&mut self, body: &[u8]) -> bool {
        let Ok(length) = u8::try_from(body.len()) else {
            return false;
        };
        self.0.push(length);
        self.0.extend_from_slice(body);
        true
    }

    #[must_use]
    pub fn vector16(&mut self, body: &[u8]) -> bool {
        let Ok(length) = u16::try_from(body.len()) else {
            return false;
        };
        self.u16(length);
        self.0.extend_from_slice(body);
        true
    }

    /// Reserve a two-byte length, and fill it in when the body is written.
    ///
    /// For the nested vectors whose contents are not built separately. The
    /// returned position is handed back to [`Self::close16`].
    #[must_use]
    pub fn open16(&mut self) -> usize {
        self.0.extend_from_slice(&[0, 0]);
        self.0.len()
    }

    /// Fill in a length opened by [`Self::open16`].
    #[must_use]
    pub fn close16(&mut self, from: usize) -> bool {
        let length = self.0.len().saturating_sub(from);
        let Ok(length) = u16::try_from(length) else {
            return false;
        };
        self.0[from - 2..from].copy_from_slice(&length.to_be_bytes());
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_come_back_as_they_went_in() {
        let mut writer = Writer::new();
        writer.u8(0x12);
        writer.u16(0x3456);
        writer.bytes(&[0x78, 0x9a]);

        let bytes = writer.into_bytes();
        let mut reader = Reader::new(&bytes);
        assert_eq!(reader.u8().unwrap(), 0x12);
        assert_eq!(reader.u16().unwrap(), 0x3456);
        assert_eq!(reader.take(2).unwrap(), &[0x78, 0x9a]);
        assert!(reader.done());
    }

    #[test]
    fn a_twenty_four_bit_length_is_big_endian() {
        let bytes = [0x01, 0x02, 0x03];
        assert_eq!(Reader::new(&bytes).u24().unwrap(), 0x0001_0203);
    }

    #[test]
    fn vectors_carry_their_own_length() {
        let mut writer = Writer::new();
        assert!(writer.vector8(b"short"));
        assert!(writer.vector16(b"longer one"));

        let bytes = writer.into_bytes();
        let mut reader = Reader::new(&bytes);
        assert_eq!(reader.vector8().unwrap(), b"short");
        assert_eq!(reader.vector16().unwrap(), b"longer one");
        assert!(reader.done());
    }

    #[test]
    fn reading_past_the_end_is_an_error_and_not_a_panic() {
        // The whole reason this type exists. Every one of these is a message a
        // peer could send, and a client that indexed instead would stop the
        // machine rather than close the connection.
        let bytes = [0x00, 0x05, 0x01];
        let mut reader = Reader::new(&bytes);
        assert_eq!(reader.vector16().unwrap_err(), Short);

        let mut reader = Reader::new(&[]);
        assert_eq!(reader.u8().unwrap_err(), Short);
        assert_eq!(reader.u16().unwrap_err(), Short);
        assert_eq!(reader.u24().unwrap_err(), Short);
        assert_eq!(reader.vector8().unwrap_err(), Short);

        let mut reader = Reader::new(&[0xff]);
        assert_eq!(reader.vector8().unwrap_err(), Short);
    }

    #[test]
    fn a_length_that_would_overflow_the_position_is_an_error() {
        let bytes = [1, 2, 3];
        let mut reader = Reader::new(&bytes);
        reader.take(1).unwrap();
        assert_eq!(reader.take(usize::MAX).unwrap_err(), Short);
    }

    #[test]
    fn every_truncation_of_a_message_is_refused_rather_than_panicking() {
        // Built, then cut at every length, and every cut has to be an error
        // rather than a panic. This is the test that says the parser is safe
        // against a peer that stops sending halfway.
        let mut writer = Writer::new();
        writer.u16(0x0303);
        assert!(writer.vector8(&[1, 2, 3]));
        assert!(writer.vector16(&[4, 5, 6, 7]));
        let whole = writer.into_bytes();

        for cut in 0..whole.len() {
            let mut reader = Reader::new(&whole[..cut]);
            // Whatever it does, it returns.
            let _ = reader.u16().and_then(|_| {
                reader.vector8()?;
                reader.vector16()
            });
        }
    }

    #[test]
    fn a_reserved_length_is_filled_in_afterwards() {
        let mut writer = Writer::new();
        writer.u8(0xaa);
        let at = writer.open16();
        writer.u8(1);
        writer.u8(2);
        writer.u8(3);
        assert!(writer.close16(at));

        let bytes = writer.into_bytes();
        assert_eq!(bytes, &[0xaa, 0x00, 0x03, 1, 2, 3]);
    }

    #[test]
    fn a_body_too_long_for_its_prefix_is_refused_rather_than_truncated() {
        // A length field that disagrees with its contents is a message the peer
        // reads as something else entirely, so this says no rather than
        // writing something wrong.
        let mut writer = Writer::new();
        let long = alloc::vec![0u8; 256];
        assert!(!writer.vector8(&long));
        assert!(writer.as_bytes().is_empty(), "nothing was written");

        let mut writer = Writer::new();
        let longer = alloc::vec![0u8; 65536];
        assert!(!writer.vector16(&longer));
    }

    #[test]
    fn nesting_reads_only_what_the_inner_length_says() {
        let mut writer = Writer::new();
        let at = writer.open16();
        writer.u16(0x1111);
        writer.u16(0x2222);
        assert!(writer.close16(at));
        writer.u16(0x3333);

        let bytes = writer.into_bytes();
        let mut reader = Reader::new(&bytes);
        let mut inner = reader.nested16().unwrap();
        assert_eq!(inner.u16().unwrap(), 0x1111);
        assert_eq!(inner.u16().unwrap(), 0x2222);
        assert!(inner.done(), "the inner reader stops at its own length");
        assert_eq!(reader.u16().unwrap(), 0x3333);
    }
}
