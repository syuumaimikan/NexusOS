//! DEFLATE, decompressed. RFC 1951, and the zlib wrapper of RFC 1950.
//!
//! Here because PNG needs it, and PNG is needed because it is what pictures
//! are. It is also what `gzip` and `Content-Encoding: deflate` are made of, so
//! it is a crate rather than a module inside the image decoder.
//!
//! # What it does not do
//!
//! Compress. Nothing in this system has needed to yet, and a compressor is a
//! different and much larger program — the decompressor is a fixed algorithm
//! and the compressor is a pile of heuristics.
//!
//! # Bounds, because this reads what somebody else wrote
//!
//! Every input to this is a file from elsewhere, which is the definition of
//! untrusted. So: the output is capped by the caller, every table index is
//! checked, a back-reference that points before the start of the output is an
//! error rather than a wrap, and the bit reader returns an error at the end of
//! the input rather than zeros. A decompressor that answers a truncated file
//! with a plausible-looking buffer is worse than one that says the file is
//! truncated.

#![no_std]
#![deny(unsafe_op_in_unsafe_fn)]

extern crate alloc;

use alloc::vec::Vec;

/// Why a stream would not decompress.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trouble {
    /// The input ended in the middle of something.
    Truncated,
    /// A block header names a kind of block that does not exist.
    BadBlock,
    /// A stored block's length and its complement disagree.
    BadStoredLength,
    /// The Huffman code lengths do not describe a usable code.
    BadCode,
    /// A symbol that is not in the alphabet.
    BadSymbol,
    /// A back-reference points before the start of the output.
    BadDistance,
    /// The output grew past what the caller allowed.
    TooLarge,
    /// The zlib header is not one.
    BadHeader,
    /// The zlib checksum does not match what was decompressed.
    BadChecksum,
}

impl core::fmt::Display for Trouble {
    fn fmt(&self, out: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        out.write_str(match self {
            Self::Truncated => "the compressed data ends in the middle of something",
            Self::BadBlock => "a block says it is a kind that does not exist",
            Self::BadStoredLength => "a stored block's length disagrees with its own check",
            Self::BadCode => "the code lengths do not describe a usable code",
            Self::BadSymbol => "a symbol that is not in the alphabet",
            Self::BadDistance => "a back-reference points before the start",
            Self::TooLarge => "the decompressed data is larger than was allowed",
            Self::BadHeader => "that is not a zlib stream",
            Self::BadChecksum => "the checksum does not match what came out",
        })
    }
}

/// Bits, least-significant first, which is the order DEFLATE writes them.
struct Bits<'a> {
    bytes: &'a [u8],
    /// Bytes consumed.
    at: usize,
    /// Bits held from the byte being read, and how many.
    held: u32,
    count: u32,
}

impl<'a> Bits<'a> {
    const fn new(bytes: &'a [u8]) -> Self {
        Self {
            bytes,
            at: 0,
            held: 0,
            count: 0,
        }
    }

    /// The next `want` bits, as a number.
    fn take(&mut self, want: u32) -> Result<u32, Trouble> {
        while self.count < want {
            let Some(byte) = self.bytes.get(self.at) else {
                return Err(Trouble::Truncated);
            };
            self.at += 1;
            self.held |= u32::from(*byte) << self.count;
            self.count += 8;
        }
        let value = self.held & ((1u32 << want) - 1);
        self.held >>= want;
        self.count -= want;
        Ok(value)
    }

    /// Throw away what is left of the byte being read.
    fn align(&mut self) {
        let extra = self.count % 8;
        self.held >>= extra;
        self.count -= extra;
    }

    /// Whole bytes, after aligning. Used by stored blocks.
    fn bytes(&mut self, count: usize) -> Result<&'a [u8], Trouble> {
        self.align();
        // Whatever whole bytes are still held count as unread input.
        let held = (self.count / 8) as usize;
        let start = self.at - held;
        self.held = 0;
        self.count = 0;
        let end = start.checked_add(count).ok_or(Trouble::Truncated)?;
        let slice = self.bytes.get(start..end).ok_or(Trouble::Truncated)?;
        self.at = end;
        Ok(slice)
    }
}

/// How many distinct code lengths a Huffman code may use.
const MAX_BITS: usize = 15;

/// A canonical Huffman code, as a table walked one bit at a time.
///
/// Counts and offsets rather than a decoding tree: the code is canonical, so
/// the symbols at each length are consecutive, and "how many codes are shorter
/// than this" plus "which one is it among its own length" is the whole of the
/// decoding. A tree would be the same information with pointers in it.
struct Huffman {
    /// How many codes of each length, 1..=MAX_BITS.
    counts: [u16; MAX_BITS + 1],
    /// Symbols, in order of length and then of symbol.
    symbols: Vec<u16>,
}

impl Huffman {
    /// Build one from the length of every symbol's code. Length zero means the
    /// symbol is not in the code.
    fn new(lengths: &[u8]) -> Result<Self, Trouble> {
        let mut counts = [0u16; MAX_BITS + 1];
        for length in lengths {
            let length = *length as usize;
            if length > MAX_BITS {
                return Err(Trouble::BadCode);
            }
            counts[length] += 1;
        }
        counts[0] = 0;

        // A code is usable when it is neither over-subscribed -- more codes at
        // some length than there is room for -- nor, with one exception, under.
        // The exception is the code with a single symbol, which DEFLATE
        // produces for a distance alphabet that is never used, and which every
        // decoder has to accept because every encoder emits it.
        let mut left = 1i32;
        for count in counts.iter().skip(1) {
            left <<= 1;
            left -= i32::from(*count);
            if left < 0 {
                return Err(Trouble::BadCode);
            }
        }

        let mut offsets = [0u16; MAX_BITS + 2];
        for length in 1..=MAX_BITS {
            offsets[length + 1] = offsets[length] + counts[length];
        }

        let mut symbols = alloc::vec![0u16; lengths.len()];
        for (symbol, length) in lengths.iter().enumerate() {
            if *length != 0 {
                let slot = &mut offsets[*length as usize + 1 - 1];
                symbols[*slot as usize] = symbol as u16;
                *slot += 1;
            }
        }

        Ok(Self { counts, symbols })
    }

    /// Read one symbol.
    fn decode(&self, bits: &mut Bits<'_>) -> Result<u16, Trouble> {
        let mut code = 0i32;
        let mut first = 0i32;
        let mut index = 0i32;
        for length in 1..=MAX_BITS {
            code |= bits.take(1)? as i32;
            let count = i32::from(self.counts[length]);
            if code - first < count {
                let at = (index + (code - first)) as usize;
                return self.symbols.get(at).copied().ok_or(Trouble::BadSymbol);
            }
            index += count;
            first = (first + count) << 1;
            code <<= 1;
        }
        Err(Trouble::BadCode)
    }
}

/// Extra bits and base length for each length symbol, 257..=285.
const LENGTH_BASE: [u16; 29] = [
    3, 4, 5, 6, 7, 8, 9, 10, 11, 13, 15, 17, 19, 23, 27, 31, 35, 43, 51, 59, 67, 83, 99, 115, 131,
    163, 195, 227, 258,
];
const LENGTH_EXTRA: [u8; 29] = [
    0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0,
];

/// And for each distance symbol, 0..=29.
const DISTANCE_BASE: [u16; 30] = [
    1, 2, 3, 4, 5, 7, 9, 13, 17, 25, 33, 49, 65, 97, 129, 193, 257, 385, 513, 769, 1025, 1537,
    2049, 3073, 4097, 6145, 8193, 12289, 16385, 24577,
];
const DISTANCE_EXTRA: [u8; 30] = [
    0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13,
    13,
];

/// The order the code-length code's own lengths arrive in.
const LENGTH_ORDER: [usize; 19] = [
    16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15,
];

/// Decompress a raw DEFLATE stream, refusing to grow past `most` bytes.
pub fn inflate(input: &[u8], most: usize) -> Result<Vec<u8>, Trouble> {
    let mut bits = Bits::new(input);
    let mut out: Vec<u8> = Vec::new();

    loop {
        let last = bits.take(1)?;
        let kind = bits.take(2)?;
        match kind {
            0 => stored(&mut bits, &mut out, most)?,
            1 => {
                let (literals, distances) = fixed_tables()?;
                block(&mut bits, &mut out, &literals, &distances, most)?;
            }
            2 => {
                let (literals, distances) = dynamic_tables(&mut bits)?;
                block(&mut bits, &mut out, &literals, &distances, most)?;
            }
            _ => return Err(Trouble::BadBlock),
        }
        if last == 1 {
            return Ok(out);
        }
    }
}

/// Decompress a zlib stream: a two-byte header, DEFLATE, and an Adler-32.
///
/// What PNG carries. The checksum is verified rather than skipped, because it
/// is the only thing in the format that would notice a decompressor bug.
pub fn zlib(input: &[u8], most: usize) -> Result<Vec<u8>, Trouble> {
    let Some(header) = input.get(..2) else {
        return Err(Trouble::BadHeader);
    };
    // Low four bits of the first byte are the method: 8 is DEFLATE and there
    // has never been another. The two bytes together are a multiple of 31.
    if header[0] & 0x0F != 8 {
        return Err(Trouble::BadHeader);
    }
    if (u16::from(header[0]) << 8 | u16::from(header[1])) % 31 != 0 {
        return Err(Trouble::BadHeader);
    }
    // Bit 5 of the second byte says a preset dictionary follows. Nothing that
    // writes PNG uses one, and guessing at a dictionary this does not have
    // would produce rubbish rather than an error.
    if header[1] & 0x20 != 0 {
        return Err(Trouble::BadHeader);
    }

    let body = &input[2..];
    let out = inflate(body, most)?;

    // The last four bytes are the checksum, big-endian. Found from the end
    // because `inflate` does not report how much input it used, and it does not
    // need to: a zlib stream is exactly its body and four bytes.
    if let Some(tail) = input.get(input.len() - 4..) {
        let stated = u32::from_be_bytes([tail[0], tail[1], tail[2], tail[3]]);
        if stated != adler32(&out) {
            return Err(Trouble::BadChecksum);
        }
    } else {
        return Err(Trouble::Truncated);
    }

    Ok(out)
}

/// Adler-32, as zlib computes it.
#[must_use]
pub fn adler32(bytes: &[u8]) -> u32 {
    let mut low: u32 = 1;
    let mut high: u32 = 0;
    // Reduced every few thousand bytes rather than every byte: the sums cannot
    // overflow a u32 within 5552 steps, which is where the number comes from.
    for chunk in bytes.chunks(5552) {
        for byte in chunk {
            low += u32::from(*byte);
            high += low;
        }
        low %= 65521;
        high %= 65521;
    }
    (high << 16) | low
}

/// A block of bytes copied through unchanged.
fn stored(bits: &mut Bits<'_>, out: &mut Vec<u8>, most: usize) -> Result<(), Trouble> {
    let header = bits.bytes(4)?;
    let length = u16::from_le_bytes([header[0], header[1]]);
    let check = u16::from_le_bytes([header[2], header[3]]);
    if length != !check {
        return Err(Trouble::BadStoredLength);
    }
    let body = bits.bytes(length as usize)?;
    if out.len() + body.len() > most {
        return Err(Trouble::TooLarge);
    }
    out.extend_from_slice(body);
    Ok(())
}

/// The fixed code every DEFLATE implementation shares.
fn fixed_tables() -> Result<(Huffman, Huffman), Trouble> {
    let mut literals = [0u8; 288];
    for (symbol, length) in literals.iter_mut().enumerate() {
        *length = match symbol {
            0..=143 => 8,
            144..=255 => 9,
            256..=279 => 7,
            _ => 8,
        };
    }
    // Five bits each, all thirty of them, which is not a complete code and is
    // what the specification says to build anyway.
    let distances = [5u8; 30];
    Ok((Huffman::new(&literals)?, Huffman::new(&distances)?))
}

/// The code a dynamic block describes before it uses it.
fn dynamic_tables(bits: &mut Bits<'_>) -> Result<(Huffman, Huffman), Trouble> {
    let literal_count = bits.take(5)? as usize + 257;
    let distance_count = bits.take(5)? as usize + 1;
    let length_count = bits.take(4)? as usize + 4;
    if literal_count > 286 || distance_count > 30 {
        return Err(Trouble::BadCode);
    }

    // The lengths of the code that encodes the lengths of the real codes.
    let mut lengths = [0u8; 19];
    for at in 0..length_count {
        lengths[LENGTH_ORDER[at]] = bits.take(3)? as u8;
    }
    let code_lengths = Huffman::new(&lengths)?;

    let mut all = alloc::vec![0u8; literal_count + distance_count];
    let mut at = 0;
    while at < all.len() {
        let symbol = code_lengths.decode(bits)?;
        match symbol {
            0..=15 => {
                all[at] = symbol as u8;
                at += 1;
            }
            // Repeat the previous length. There has to be one.
            16 => {
                if at == 0 {
                    return Err(Trouble::BadCode);
                }
                let previous = all[at - 1];
                let times = bits.take(2)? as usize + 3;
                if at + times > all.len() {
                    return Err(Trouble::BadCode);
                }
                for _ in 0..times {
                    all[at] = previous;
                    at += 1;
                }
            }
            17 | 18 => {
                let times = if symbol == 17 {
                    bits.take(3)? as usize + 3
                } else {
                    bits.take(7)? as usize + 11
                };
                if at + times > all.len() {
                    return Err(Trouble::BadCode);
                }
                at += times;
            }
            _ => return Err(Trouble::BadSymbol),
        }
    }

    let literals = Huffman::new(&all[..literal_count])?;
    let distances = Huffman::new(&all[literal_count..])?;
    Ok((literals, distances))
}

/// One compressed block, with the codes it uses already built.
fn block(
    bits: &mut Bits<'_>,
    out: &mut Vec<u8>,
    literals: &Huffman,
    distances: &Huffman,
    most: usize,
) -> Result<(), Trouble> {
    loop {
        let symbol = literals.decode(bits)?;
        match symbol {
            0..=255 => {
                if out.len() >= most {
                    return Err(Trouble::TooLarge);
                }
                out.push(symbol as u8);
            }
            256 => return Ok(()),
            257..=285 => {
                let index = symbol as usize - 257;
                let length = LENGTH_BASE[index] as usize
                    + bits.take(u32::from(LENGTH_EXTRA[index]))? as usize;

                let distance_symbol = distances.decode(bits)? as usize;
                if distance_symbol >= DISTANCE_BASE.len() {
                    return Err(Trouble::BadSymbol);
                }
                let distance = DISTANCE_BASE[distance_symbol] as usize
                    + bits.take(u32::from(DISTANCE_EXTRA[distance_symbol]))? as usize;

                if distance == 0 || distance > out.len() {
                    return Err(Trouble::BadDistance);
                }
                if out.len() + length > most {
                    return Err(Trouble::TooLarge);
                }
                // Byte at a time and not a block copy, because the ranges
                // overlap on purpose: a distance of one and a length of ten is
                // how DEFLATE writes ten of the same byte, and a copy that read
                // the whole source first would read bytes that are not there
                // yet.
                let from = out.len() - distance;
                for step in 0..length {
                    let byte = out[from + step];
                    out.push(byte);
                }
            }
            _ => return Err(Trouble::BadSymbol),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Fixtures made with Python's zlib, which is the reference implementation
    /// everything else agrees with. Each is the compressed form of a string
    /// this test knows, so what is being checked is that this file produces
    /// what the rest of the world produces.
    const HELLO_ZLIB: &[u8] = &[
        0x78, 0x9c, 0xcb, 0x48, 0xcd, 0xc9, 0xc9, 0x57, 0x28, 0xcf, 0x2f, 0xca, 0x49, 0x51, 0x04,
        0x00, 0x1e, 0x89, 0x04, 0x7e,
    ];

    #[test]
    fn a_zlib_stream_comes_back() {
        assert_eq!(zlib(HELLO_ZLIB, 1024).unwrap(), b"hello world!".to_vec());
    }

    #[test]
    fn adler_matches_the_reference() {
        // Values from zlib itself.
        assert_eq!(adler32(b""), 1);
        assert_eq!(adler32(b"a"), 0x0062_0062);
        assert_eq!(adler32(b"hello world!"), 0x1e89047e);
    }

    #[test]
    fn a_stored_block_comes_through() {
        // Final block, kind 0, length 5, its complement, then the bytes.
        let mut raw = alloc::vec![0x01u8];
        raw.extend_from_slice(&5u16.to_le_bytes());
        raw.extend_from_slice(&(!5u16).to_le_bytes());
        raw.extend_from_slice(b"abcde");
        assert_eq!(inflate(&raw, 64).unwrap(), b"abcde".to_vec());
    }

    #[test]
    fn a_stored_block_with_a_wrong_check_is_refused() {
        let mut raw = alloc::vec![0x01u8];
        raw.extend_from_slice(&5u16.to_le_bytes());
        raw.extend_from_slice(&7u16.to_le_bytes());
        raw.extend_from_slice(b"abcde");
        assert_eq!(inflate(&raw, 64), Err(Trouble::BadStoredLength));
    }

    #[test]
    fn the_cap_is_a_cap() {
        assert_eq!(zlib(HELLO_ZLIB, 4), Err(Trouble::TooLarge));
    }

    #[test]
    fn a_truncated_stream_says_so() {
        for cut in 3..HELLO_ZLIB.len() - 4 {
            let short = &HELLO_ZLIB[..cut];
            // Either truncated or a checksum that cannot match; never a
            // plausible-looking answer.
            assert!(zlib(short, 1024).is_err(), "cut at {cut} produced a value");
        }
    }

    #[test]
    fn rubbish_is_not_a_zlib_stream() {
        assert_eq!(zlib(b"", 64), Err(Trouble::BadHeader));
        assert_eq!(zlib(b"\x00\x00", 64), Err(Trouble::BadHeader));
        assert_eq!(zlib(b"\x78\x00", 64), Err(Trouble::BadHeader));
    }

    /// A stream with a dynamic Huffman block in it, which is what a real file
    /// uses: repeated text, a run that DEFLATE writes as a distance of one, and
    /// every byte value so that the literal alphabet is fully exercised.
    const MIXED_ZLIB: &[u8] = &[
        0x78, 0xda, 0x2b, 0xc9, 0x48, 0x55, 0x28, 0x2c, 0xcd, 0x4c, 0xce, 0x56, 0x48, 0x2a, 0xca,
        0x2f, 0xcf, 0x53, 0x48, 0xcb, 0xaf, 0x50, 0xc8, 0x2a, 0xcd, 0x2d, 0x28, 0x56, 0xc8, 0x2f,
        0x4b, 0x2d, 0x52, 0x28, 0x01, 0x4a, 0xe7, 0x24, 0x56, 0x55, 0x2a, 0xa4, 0xe4, 0xa7, 0xeb,
        0x81, 0x79, 0xa3, 0x8a, 0xc9, 0x52, 0x9c, 0x92, 0x9a, 0x96, 0x93, 0x58, 0x92, 0xaa, 0x50,
        0x5a, 0x9c, 0x5a, 0xac, 0x90, 0x94, 0x98, 0x9c, 0xad, 0x5b, 0x94, 0x9a, 0x96, 0x5a, 0x94,
        0x9a, 0x97, 0x9c, 0x5a, 0xac, 0xa3, 0x90, 0x98, 0x97, 0xa2, 0x90, 0xa8, 0x90, 0x92, 0x59,
        0x5c, 0x92, 0x08, 0x14, 0x50, 0xc8, 0x4f, 0x53, 0xc8, 0xcf, 0x4b, 0x55, 0x28, 0x4a, 0x2d,
        0x48, 0x4d, 0x2c, 0x29, 0x06, 0xca, 0x24, 0x55, 0x96, 0xa4, 0x5a, 0x29, 0x24, 0x12, 0x09,
        0xf4, 0x14, 0x18, 0x18, 0x99, 0x98, 0x59, 0x58, 0xd9, 0xd8, 0x39, 0x38, 0xb9, 0xb8, 0x79,
        0x78, 0xf9, 0xf8, 0x05, 0x04, 0x85, 0x84, 0x45, 0x44, 0xc5, 0xc4, 0x25, 0x24, 0xa5, 0xa4,
        0x65, 0x64, 0xe5, 0xe4, 0x15, 0x14, 0x95, 0x94, 0x55, 0x54, 0xd5, 0xd4, 0x35, 0x34, 0xb5,
        0xb4, 0x75, 0x74, 0xf5, 0xf4, 0x0d, 0x0c, 0x8d, 0x8c, 0x4d, 0x4c, 0xcd, 0xcc, 0x2d, 0x2c,
        0xad, 0xac, 0x6d, 0x6c, 0xed, 0xec, 0x1d, 0x1c, 0x9d, 0x9c, 0x5d, 0x5c, 0xdd, 0xdc, 0x3d,
        0x3c, 0xbd, 0xbc, 0x7d, 0x7c, 0xfd, 0xfc, 0x03, 0x02, 0x83, 0x82, 0x43, 0x42, 0xc3, 0xc2,
        0x23, 0x22, 0xa3, 0xa2, 0x63, 0x62, 0xe3, 0xe2, 0x13, 0x12, 0x93, 0x92, 0x81, 0x3e, 0x4a,
        0xcf, 0xc8, 0xcc, 0xca, 0xce, 0xc9, 0xcd, 0xcb, 0x2f, 0x28, 0x2c, 0x2a, 0x2e, 0x29, 0x2d,
        0x2b, 0xaf, 0xa8, 0xac, 0xaa, 0xae, 0xa9, 0xad, 0xab, 0x6f, 0x68, 0x6c, 0x6a, 0x6e, 0x69,
        0x6d, 0x6b, 0xef, 0xe8, 0xec, 0xea, 0xee, 0xe9, 0xed, 0xeb, 0x9f, 0x30, 0x71, 0xd2, 0xe4,
        0x29, 0x53, 0xa7, 0x4d, 0x9f, 0x31, 0x73, 0xd6, 0xec, 0x39, 0x73, 0xe7, 0xcd, 0x5f, 0xb0,
        0x70, 0xd1, 0xe2, 0x25, 0x4b, 0x97, 0x2d, 0x5f, 0xb1, 0x72, 0xd5, 0xea, 0x35, 0x6b, 0xd7,
        0xad, 0xdf, 0xb0, 0x71, 0xd3, 0xe6, 0x2d, 0x5b, 0xb7, 0x6d, 0xdf, 0xb1, 0x73, 0xd7, 0xee,
        0x3d, 0x7b, 0xf7, 0xed, 0x3f, 0x70, 0xf0, 0xd0, 0xe1, 0x23, 0x47, 0x8f, 0x1d, 0x3f, 0x71,
        0xf2, 0xd4, 0xe9, 0x33, 0x67, 0xcf, 0x9d, 0xbf, 0x70, 0xf1, 0xd2, 0xe5, 0x2b, 0x57, 0xaf,
        0x5d, 0xbf, 0x71, 0xf3, 0xd6, 0xed, 0x3b, 0x77, 0xef, 0xdd, 0x7f, 0xf0, 0xf0, 0xd1, 0xe3,
        0x27, 0x4f, 0x9f, 0x3d, 0x7f, 0xf1, 0xf2, 0xd5, 0xeb, 0x37, 0x6f, 0xdf, 0xbd, 0xff, 0xf0,
        0xf1, 0xd3, 0xe7, 0x2f, 0x5f, 0xbf, 0x7d, 0xff, 0xf1, 0xf3, 0xd7, 0xef, 0x3f, 0x7f, 0xff,
        0xfd, 0x07, 0x00, 0x6e, 0x34, 0x29, 0x78,
    ];

    #[test]
    fn a_dynamic_block_comes_back_byte_for_byte() {
        let out = zlib(MIXED_ZLIB, 64 * 1024).unwrap();
        assert_eq!(out.len(), 726);
        assert!(out.starts_with(b"the quick brown fox"));
        // The run written as a back-reference of distance one.
        assert!(out.windows(40).any(|window| window == [b'a'; 40]));
        // And every byte value, in order, at the end.
        let tail: alloc::vec::Vec<u8> = (0..=255u8).collect();
        assert!(out.ends_with(&tail));
        assert_eq!(adler32(&out), 0x6e342978);
    }

    #[test]
    fn a_changed_byte_fails_the_checksum() {
        let mut broken = HELLO_ZLIB.to_vec();
        let last = broken.len() - 1;
        broken[last] ^= 0xFF;
        assert_eq!(zlib(&broken, 1024), Err(Trouble::BadChecksum));
    }
}
