//! SHA-256.
//!
//! Written out rather than depended on. A package format needs a digest that
//! everyone computes the same way, and the whole value of one is that an
//! attacker cannot produce a different file with the same answer — which is a
//! property of the algorithm, not of who implemented it. So it is implemented
//! here, and checked against the vectors the standard publishes: a hash that
//! agrees with FIPS 180-4 on the empty string, on "abc", on a message that
//! straddles a block boundary and on a million characters is a hash.
//!
//! # The part that is easy to get wrong
//!
//! Padding. A message is followed by a single one bit, then zeroes, then its
//! length in bits as a 64-bit big-endian number — and the length is of the
//! *message*, not of the padded block. When the message ends within nine bytes
//! of a block boundary the length does not fit, so another whole block is
//! added. An implementation that forgot that case would agree with every other
//! implementation except on messages of exactly the wrong length.

/// The first thirty-two bits of the fractional parts of the cube roots of the
/// first sixty-four primes. Constants, in the strict sense: nothing about them
/// is a choice.
const K: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

/// The square roots of the first eight primes, likewise.
const INITIAL: [u32; 8] = [
    0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19,
];

/// Bytes in a digest.
pub const DIGEST: usize = 32;

/// A hash being computed.
///
/// Incremental, because the things being hashed are files: a package is read a
/// piece at a time and there is nowhere to put a whole one first.
pub struct Hasher {
    state: [u32; 8],
    /// Bytes not yet part of a whole block.
    buffer: [u8; 64],
    buffered: usize,
    /// How long the message is so far, in bytes.
    length: u64,
}

impl Default for Hasher {
    fn default() -> Self {
        Self::new()
    }
}

impl Hasher {
    /// Start.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            state: INITIAL,
            buffer: [0; 64],
            buffered: 0,
            length: 0,
        }
    }

    /// Add some of the message.
    pub fn update(&mut self, mut data: &[u8]) {
        self.length = self.length.wrapping_add(data.len() as u64);

        // Fill whatever is left of the part-built block first, so that the fast
        // path below only ever sees whole blocks.
        if self.buffered > 0 {
            let wanted = (64 - self.buffered).min(data.len());
            self.buffer[self.buffered..self.buffered + wanted].copy_from_slice(&data[..wanted]);
            self.buffered += wanted;
            data = &data[wanted..];
            if self.buffered < 64 {
                // Everything given was taken and the block is still not full,
                // so there is nothing more to do -- and in particular the tail
                // below must not run, because it would set `buffered` back to
                // the length of what is left, which is nothing. That is exactly
                // the bug this had: every call threw away the part-built block,
                // so a hash fed in pieces never finished padding and the loop
                // in `finish` never ended.
                return;
            }
            let block = self.buffer;
            self.compress(&block);
            self.buffered = 0;
        }

        while data.len() >= 64 {
            let mut block = [0u8; 64];
            block.copy_from_slice(&data[..64]);
            self.compress(&block);
            data = &data[64..];
        }

        self.buffer[..data.len()].copy_from_slice(data);
        self.buffered = data.len();
    }

    /// Finish, and say what the message hashed to.
    #[must_use]
    pub fn finish(mut self) -> [u8; DIGEST] {
        // The length is of the message, in bits, before any padding is added.
        let bits = self.length.wrapping_mul(8);

        // One bit, then zeroes. The one bit is a whole byte because nothing
        // here hashes a message that is not a whole number of bytes.
        self.update(&[0x80]);
        // `update` counted that byte; the length written below must not.
        self.length = self.length.wrapping_sub(1);

        // Zeroes until there are exactly eight bytes left in the block. When
        // the message ended within nine bytes of the boundary this fills the
        // rest of one block and all but the last eight of another, which is
        // the case an implementation forgets.
        while self.buffered != 56 {
            self.update(&[0]);
            self.length = self.length.wrapping_sub(1);
        }

        let mut tail = [0u8; 8];
        tail.copy_from_slice(&bits.to_be_bytes());
        self.update(&tail);

        let mut digest = [0u8; DIGEST];
        for (index, word) in self.state.iter().enumerate() {
            digest[index * 4..index * 4 + 4].copy_from_slice(&word.to_be_bytes());
        }
        digest
    }

    /// One block.
    fn compress(&mut self, block: &[u8; 64]) {
        let mut w = [0u32; 64];
        for (index, word) in w.iter_mut().take(16).enumerate() {
            *word = u32::from_be_bytes([
                block[index * 4],
                block[index * 4 + 1],
                block[index * 4 + 2],
                block[index * 4 + 3],
            ]);
        }
        for index in 16..64 {
            let s0 = w[index - 15].rotate_right(7)
                ^ w[index - 15].rotate_right(18)
                ^ (w[index - 15] >> 3);
            let s1 = w[index - 2].rotate_right(17)
                ^ w[index - 2].rotate_right(19)
                ^ (w[index - 2] >> 10);
            w[index] = w[index - 16]
                .wrapping_add(s0)
                .wrapping_add(w[index - 7])
                .wrapping_add(s1);
        }

        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = self.state;

        for index in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let choose = (e & f) ^ ((!e) & g);
            let temp1 = h
                .wrapping_add(s1)
                .wrapping_add(choose)
                .wrapping_add(K[index])
                .wrapping_add(w[index]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let majority = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = s0.wrapping_add(majority);

            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(temp1);
            d = c;
            c = b;
            b = a;
            a = temp1.wrapping_add(temp2);
        }

        for (slot, value) in self.state.iter_mut().zip([a, b, c, d, e, f, g, h]) {
            *slot = slot.wrapping_add(value);
        }
    }
}

/// Hash something in one go.
#[must_use]
pub fn digest(data: &[u8]) -> [u8; DIGEST] {
    let mut hasher = Hasher::new();
    hasher.update(data);
    hasher.finish()
}

/// Whether two digests are the same, in time that does not depend on where they
/// differ.
///
/// A comparison that stopped at the first difference would leak, byte by byte,
/// how much of a forged digest was right — and an attacker who can measure that
/// can find the rest one byte at a time instead of all at once.
#[must_use]
pub fn digests_equal(a: &[u8; DIGEST], b: &[u8; DIGEST]) -> bool {
    let mut difference = 0u8;
    for index in 0..DIGEST {
        difference |= a[index] ^ b[index];
    }
    difference == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Render a digest the way the standard prints one.
    fn hex(digest: &[u8; DIGEST]) -> alloc::string::String {
        use core::fmt::Write as _;
        let mut out = alloc::string::String::new();
        for byte in digest {
            let _ = write!(out, "{byte:02x}");
        }
        out
    }

    #[test]
    fn the_empty_message() {
        assert_eq!(
            hex(&digest(b"")),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn the_first_vector() {
        assert_eq!(
            hex(&digest(b"abc")),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn a_message_that_straddles_a_block() {
        // Fifty-six bytes: the message ends exactly where the length would go,
        // so a whole extra block of padding is required. This is the case an
        // implementation gets wrong and then agrees with everyone else on every
        // other input.
        let message = b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq";
        assert_eq!(message.len(), 56);
        assert_eq!(
            hex(&digest(message)),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
    }

    #[test]
    fn a_message_of_exactly_one_block() {
        // Sixty-four bytes, which is where an off-by-one in the buffering shows
        // up: the block is complete and there is nothing left over.
        let message = [b'a'; 64];
        assert_eq!(
            hex(&digest(&message)),
            "ffe054fe7ae0cb6dc65c3af9b61d5209f439851db43d0ba5997337df154668eb"
        );
    }

    #[test]
    fn a_million_letters() {
        // The standard's long vector. It also exercises the incremental path
        // for real: a million bytes cannot be one call in a kernel.
        let mut hasher = Hasher::new();
        for _ in 0..1_000 {
            hasher.update(&[b'a'; 1_000]);
        }
        assert_eq!(
            hex(&hasher.finish()),
            "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0"
        );
    }

    #[test]
    fn feeding_it_in_pieces_gives_the_same_answer() {
        // The whole point of an incremental hash: where the boundaries fall
        // must not matter. Sizes chosen to land inside a block, exactly on one,
        // and across two.
        let message: alloc::vec::Vec<u8> = (0..200u32).map(|byte| byte as u8).collect();
        let whole = digest(&message);
        for chunk in [1usize, 7, 63, 64, 65, 128] {
            let mut hasher = Hasher::new();
            for piece in message.chunks(chunk) {
                hasher.update(piece);
            }
            assert_eq!(hasher.finish(), whole, "chunks of {chunk}");
        }
    }

    #[test]
    fn a_digest_that_differs_anywhere_is_not_equal() {
        let a = digest(b"abc");
        for index in 0..DIGEST {
            let mut b = a;
            b[index] ^= 1;
            assert!(!digests_equal(&a, &b), "byte {index}");
        }
        assert!(digests_equal(&a, &a));
    }
}
