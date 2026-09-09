//! SHA-512, because Ed25519 is defined in terms of it.
//!
//! The same shape as SHA-256 with wider words: sixty-four bits instead of
//! thirty-two, eighty rounds instead of sixty-four, blocks of a hundred and
//! twenty-eight bytes instead of sixty-four, and a length field of a hundred
//! and twenty-eight bits instead of sixty-four.
//!
//! It is here rather than in `nexus-pkg` because a signature scheme cannot
//! choose its hash: Ed25519 *is* SHA-512 applied in particular places, and a
//! signature made with anything else is not an Ed25519 signature.

/// The first sixty-four bits of the fractional parts of the cube roots of the
/// first eighty primes.
const K: [u64; 80] = [
    0x428a2f98d728ae22,
    0x7137449123ef65cd,
    0xb5c0fbcfec4d3b2f,
    0xe9b5dba58189dbbc,
    0x3956c25bf348b538,
    0x59f111f1b605d019,
    0x923f82a4af194f9b,
    0xab1c5ed5da6d8118,
    0xd807aa98a3030242,
    0x12835b0145706fbe,
    0x243185be4ee4b28c,
    0x550c7dc3d5ffb4e2,
    0x72be5d74f27b896f,
    0x80deb1fe3b1696b1,
    0x9bdc06a725c71235,
    0xc19bf174cf692694,
    0xe49b69c19ef14ad2,
    0xefbe4786384f25e3,
    0x0fc19dc68b8cd5b5,
    0x240ca1cc77ac9c65,
    0x2de92c6f592b0275,
    0x4a7484aa6ea6e483,
    0x5cb0a9dcbd41fbd4,
    0x76f988da831153b5,
    0x983e5152ee66dfab,
    0xa831c66d2db43210,
    0xb00327c898fb213f,
    0xbf597fc7beef0ee4,
    0xc6e00bf33da88fc2,
    0xd5a79147930aa725,
    0x06ca6351e003826f,
    0x142929670a0e6e70,
    0x27b70a8546d22ffc,
    0x2e1b21385c26c926,
    0x4d2c6dfc5ac42aed,
    0x53380d139d95b3df,
    0x650a73548baf63de,
    0x766a0abb3c77b2a8,
    0x81c2c92e47edaee6,
    0x92722c851482353b,
    0xa2bfe8a14cf10364,
    0xa81a664bbc423001,
    0xc24b8b70d0f89791,
    0xc76c51a30654be30,
    0xd192e819d6ef5218,
    0xd69906245565a910,
    0xf40e35855771202a,
    0x106aa07032bbd1b8,
    0x19a4c116b8d2d0c8,
    0x1e376c085141ab53,
    0x2748774cdf8eeb99,
    0x34b0bcb5e19b48a8,
    0x391c0cb3c5c95a63,
    0x4ed8aa4ae3418acb,
    0x5b9cca4f7763e373,
    0x682e6ff3d6b2b8a3,
    0x748f82ee5defb2fc,
    0x78a5636f43172f60,
    0x84c87814a1f0ab72,
    0x8cc702081a6439ec,
    0x90befffa23631e28,
    0xa4506cebde82bde9,
    0xbef9a3f7b2c67915,
    0xc67178f2e372532b,
    0xca273eceea26619c,
    0xd186b8c721c0c207,
    0xeada7dd6cde0eb1e,
    0xf57d4f7fee6ed178,
    0x06f067aa72176fba,
    0x0a637dc5a2c898a6,
    0x113f9804bef90dae,
    0x1b710b35131c471b,
    0x28db77f523047d84,
    0x32caab7b40c72493,
    0x3c9ebe0a15c9bebc,
    0x431d67c49c100d4c,
    0x4cc5d4becb3e42b6,
    0x597f299cfc657e2a,
    0x5fcb6fab3ad6faec,
    0x6c44198c4a475817,
];

/// The square roots of the first eight primes.
const INITIAL: [u64; 8] = [
    0x6a09e667f3bcc908,
    0xbb67ae8584caa73b,
    0x3c6ef372fe94f82b,
    0xa54ff53a5f1d36f1,
    0x510e527fade682d1,
    0x9b05688c2b3e6c1f,
    0x1f83d9abfb41bd6b,
    0x5be0cd19137e2179,
];

/// Bytes in a digest.
pub const DIGEST: usize = 64;
/// Bytes in a block.
const BLOCK: usize = 128;

/// A hash being computed.
pub struct Hasher {
    state: [u64; 8],
    buffer: [u8; BLOCK],
    buffered: usize,
    length: u128,
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
            buffer: [0; BLOCK],
            buffered: 0,
            length: 0,
        }
    }

    /// Add some of the message.
    pub fn update(&mut self, mut data: &[u8]) {
        self.length = self.length.wrapping_add(data.len() as u128);

        if self.buffered > 0 {
            let wanted = (BLOCK - self.buffered).min(data.len());
            self.buffer[self.buffered..self.buffered + wanted].copy_from_slice(&data[..wanted]);
            self.buffered += wanted;
            data = &data[wanted..];
            if self.buffered < BLOCK {
                // Everything was taken and the block is not full. Returning
                // here is what keeps the tail below from resetting the count of
                // what is buffered -- the same mistake this code's SHA-256
                // sibling made, where a hash fed in pieces silently threw away
                // every part-built block.
                return;
            }
            let block = self.buffer;
            self.compress(&block);
            self.buffered = 0;
        }

        while data.len() >= BLOCK {
            let mut block = [0u8; BLOCK];
            block.copy_from_slice(&data[..BLOCK]);
            self.compress(&block);
            data = &data[BLOCK..];
        }

        self.buffer[..data.len()].copy_from_slice(data);
        self.buffered = data.len();
    }

    /// Finish.
    #[must_use]
    pub fn finish(mut self) -> [u8; DIGEST] {
        let bits = self.length.wrapping_mul(8);

        self.update(&[0x80]);
        self.length = self.length.wrapping_sub(1);
        // Until sixteen bytes are left, because the length here is a hundred
        // and twenty-eight bits rather than sixty-four.
        while self.buffered != BLOCK - 16 {
            self.update(&[0]);
            self.length = self.length.wrapping_sub(1);
        }
        let tail = bits.to_be_bytes();
        self.update(&tail);

        let mut digest = [0u8; DIGEST];
        for (index, word) in self.state.iter().enumerate() {
            digest[index * 8..index * 8 + 8].copy_from_slice(&word.to_be_bytes());
        }
        digest
    }

    /// One block.
    fn compress(&mut self, block: &[u8; BLOCK]) {
        let mut w = [0u64; 80];
        for (index, word) in w.iter_mut().take(16).enumerate() {
            let mut bytes = [0u8; 8];
            bytes.copy_from_slice(&block[index * 8..index * 8 + 8]);
            *word = u64::from_be_bytes(bytes);
        }
        for index in 16..80 {
            let s0 = w[index - 15].rotate_right(1)
                ^ w[index - 15].rotate_right(8)
                ^ (w[index - 15] >> 7);
            let s1 =
                w[index - 2].rotate_right(19) ^ w[index - 2].rotate_right(61) ^ (w[index - 2] >> 6);
            w[index] = w[index - 16]
                .wrapping_add(s0)
                .wrapping_add(w[index - 7])
                .wrapping_add(s1);
        }

        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = self.state;
        for index in 0..80 {
            let s1 = e.rotate_right(14) ^ e.rotate_right(18) ^ e.rotate_right(41);
            let choose = (e & f) ^ ((!e) & g);
            let temp1 = h
                .wrapping_add(s1)
                .wrapping_add(choose)
                .wrapping_add(K[index])
                .wrapping_add(w[index]);
            let s0 = a.rotate_right(28) ^ a.rotate_right(34) ^ a.rotate_right(39);
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

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(digest: &[u8]) -> alloc::string::String {
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
            "cf83e1357eefb8bdf1542850d66d8007d620e4050b5715dc83f4a921d36ce9ce\
             47d0d13c5d85f2b0ff8318d2877eec2f63b931bd47417a81a538327af927da3e"
        );
    }

    #[test]
    fn the_first_vector() {
        assert_eq!(
            hex(&digest(b"abc")),
            "ddaf35a193617abacc417349ae20413112e6fa4e89a97ea20a9eeee64b55d39a\
             2192992a274fc1a836ba3c23a3feebbd454d4423643ce80e2a9ac94fa54ca49f"
        );
    }

    #[test]
    fn a_message_that_needs_a_second_block_of_padding() {
        // A hundred and twelve bytes is exactly where the length field would
        // go, so the whole of another block is padding. This is the case an
        // implementation gets wrong and then agrees with everyone else on
        // everything else.
        let message = [b'a'; 112];
        assert_eq!(
            hex(&digest(&message)),
            "c01d080efd492776a1c43bd23dd99d0a2e626d481e16782e75d54c2503b5dc32\
             bd05f0f1ba33e568b88fd2d970929b719ecbb152f58f130a407c8830604b70ca"
        );
    }

    #[test]
    fn feeding_it_in_pieces_gives_the_same_answer() {
        let message: alloc::vec::Vec<u8> = (0..500u32).map(|byte| byte as u8).collect();
        let whole = digest(&message);
        for chunk in [1usize, 13, 127, 128, 129, 256] {
            let mut hasher = Hasher::new();
            for piece in message.chunks(chunk) {
                hasher.update(piece);
            }
            assert_eq!(hasher.finish(), whole, "chunks of {chunk}");
        }
    }
}
