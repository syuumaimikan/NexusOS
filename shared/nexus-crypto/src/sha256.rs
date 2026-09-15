//! SHA-256, and the HMAC and HKDF built on it.
//!
//! Written out rather than depended on, for the same reason [`crate::sha512`]
//! is: what a signature or a session key is worth rests on the algorithm
//! agreeing with every other implementation, byte for byte. So every piece here
//! is checked against published test vectors — FIPS 180-4 and RFC 6234 for the
//! hash, RFC 4231 for HMAC, RFC 5869 for HKDF — rather than against itself.
//!
//! That distinction matters more here than almost anywhere else in this system.
//! A JPEG decoder that is subtly wrong shows a slightly wrong picture. A key
//! schedule that is subtly wrong produces a connection that fails to handshake,
//! or -- far worse -- one that handshakes with a peer that made the same
//! mistake, which is a connection secured by nothing.
//!
//! # Why SHA-256 as well as SHA-512
//!
//! Ed25519 is defined over SHA-512 and that is what `sha512.rs` is for. TLS 1.3
//! with the cipher suites anybody actually negotiates is defined over SHA-256,
//! and the two are different functions rather than the same one truncated.

/// How many bytes a SHA-256 digest is.
pub const DIGEST: usize = 32;

/// How many bytes are hashed at a time.
pub const BLOCK: usize = 64;

/// The first thirty-two bits of the fractional parts of the cube roots of the
/// first sixty-four primes.
///
/// A table rather than computed: these are the constants the specification
/// gives, and a machine that derived them would be a machine where a rounding
/// difference silently produced a different hash function.
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

/// The first thirty-two bits of the fractional parts of the square roots of the
/// first eight primes.
const INITIAL: [u32; 8] = [
    0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19,
];

/// A SHA-256 in progress.
///
/// Incremental because TLS needs a running hash of every handshake message and
/// a digest of it at several points along the way; a function that only took a
/// whole message would mean keeping every byte of the handshake to hash again.
#[derive(Clone)]
pub struct Sha256 {
    state: [u32; 8],
    /// Bytes not yet part of a full block.
    pending: [u8; BLOCK],
    pending_len: usize,
    /// How many bytes have been absorbed in total.
    ///
    /// The padding encodes this as a count of *bits*, which is why it is a u64
    /// here: a u32 of bits would wrap at half a gibibyte.
    total: u64,
}

impl Default for Sha256 {
    fn default() -> Self {
        Self::new()
    }
}

impl Sha256 {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            state: INITIAL,
            pending: [0; BLOCK],
            pending_len: 0,
            total: 0,
        }
    }

    /// Absorb some bytes.
    pub fn update(&mut self, mut bytes: &[u8]) {
        self.total = self.total.wrapping_add(bytes.len() as u64);

        // Finish the partial block first, if there is one.
        if self.pending_len > 0 {
            let room = BLOCK - self.pending_len;
            let take = room.min(bytes.len());
            self.pending[self.pending_len..self.pending_len + take].copy_from_slice(&bytes[..take]);
            self.pending_len += take;
            bytes = &bytes[take..];
            if self.pending_len == BLOCK {
                let block = self.pending;
                self.compress(&block);
                self.pending_len = 0;
            }
        }

        // Then whole blocks straight from the input, without copying.
        while bytes.len() >= BLOCK {
            let (block, rest) = bytes.split_at(BLOCK);
            let mut whole = [0u8; BLOCK];
            whole.copy_from_slice(block);
            self.compress(&whole);
            bytes = rest;
        }

        // And keep whatever is left.
        if !bytes.is_empty() {
            self.pending[..bytes.len()].copy_from_slice(bytes);
            self.pending_len = bytes.len();
        }
    }

    /// Finish, and give the digest.
    #[must_use]
    pub fn finish(mut self) -> [u8; DIGEST] {
        // A one bit, then zeros, then the length in bits as a big-endian u64.
        let bits = self.total.wrapping_mul(8);
        self.update(&[0x80]);
        // `update` counted that byte, so the padding length is worked out from
        // where the buffer actually is rather than from the running total.
        while self.pending_len != BLOCK - 8 {
            self.update(&[0]);
        }
        // Written straight into the buffer: going through `update` would add
        // eight to the total, and the total has already been turned into the
        // bit count above.
        self.pending[BLOCK - 8..].copy_from_slice(&bits.to_be_bytes());
        let block = self.pending;
        self.compress(&block);

        let mut digest = [0u8; DIGEST];
        for (chunk, word) in digest
            .as_chunks_mut::<4>()
            .0
            .iter_mut()
            .zip(self.state.iter())
        {
            *chunk = word.to_be_bytes();
        }
        digest
    }

    /// One block.
    fn compress(&mut self, block: &[u8; BLOCK]) {
        let mut w = [0u32; 64];
        for (slot, chunk) in w.iter_mut().zip(block.as_chunks::<4>().0.iter()) {
            *slot = u32::from_be_bytes(*chunk);
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
            let one = h
                .wrapping_add(s1)
                .wrapping_add(choose)
                .wrapping_add(K[index])
                .wrapping_add(w[index]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let majority = (a & b) ^ (a & c) ^ (b & c);
            let two = s0.wrapping_add(majority);

            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(one);
            d = c;
            c = b;
            b = a;
            a = one.wrapping_add(two);
        }

        for (slot, value) in self.state.iter_mut().zip([a, b, c, d, e, f, g, h]) {
            *slot = slot.wrapping_add(value);
        }
    }
}

/// The SHA-256 of one slice.
#[must_use]
pub fn digest(bytes: &[u8]) -> [u8; DIGEST] {
    let mut hash = Sha256::new();
    hash.update(bytes);
    hash.finish()
}

/// HMAC-SHA-256, as RFC 2104 defines it.
///
/// A key longer than a block is hashed first; one shorter is padded with zeros.
/// Both of those are in the specification and both are easy to leave out, and
/// leaving either out gives a MAC that is wrong only for some keys -- which is
/// the worst kind of wrong, because it works in testing.
#[must_use]
pub fn hmac(key: &[u8], message: &[u8]) -> [u8; DIGEST] {
    let mut padded = [0u8; BLOCK];
    if key.len() > BLOCK {
        padded[..DIGEST].copy_from_slice(&digest(key));
    } else {
        padded[..key.len()].copy_from_slice(key);
    }

    let mut inner_key = [0u8; BLOCK];
    let mut outer_key = [0u8; BLOCK];
    for index in 0..BLOCK {
        inner_key[index] = padded[index] ^ 0x36;
        outer_key[index] = padded[index] ^ 0x5c;
    }

    let mut inner = Sha256::new();
    inner.update(&inner_key);
    inner.update(message);
    let inner = inner.finish();

    let mut outer = Sha256::new();
    outer.update(&outer_key);
    outer.update(&inner);
    outer.finish()
}

/// HKDF-Extract, as RFC 5869 defines it.
///
/// The salt is the key and the input keying material is the message, which
/// looks backwards and is what the specification says.
#[must_use]
pub fn extract(salt: &[u8], material: &[u8]) -> [u8; DIGEST] {
    hmac(salt, material)
}

/// HKDF-Expand, as RFC 5869 defines it.
///
/// Fills `out`, which may be up to 255 times the digest length. Longer is
/// refused rather than truncated: the counter is one byte, and a caller asking
/// for more is a caller whose arithmetic is wrong.
pub fn expand(key: &[u8], info: &[u8], out: &mut [u8]) -> bool {
    if out.len() > 255 * DIGEST {
        return false;
    }
    let mut previous: [u8; DIGEST] = [0; DIGEST];
    let mut written = 0usize;
    let mut counter = 1u8;

    while written < out.len() {
        let mut round = Sha256::new();
        // Every block but the first is prefixed with the one before it, which
        // is what chains them.
        let mut block = [0u8; BLOCK];
        let mut inner_key = [0u8; BLOCK];
        let mut key_block = [0u8; BLOCK];
        if key.len() > BLOCK {
            key_block[..DIGEST].copy_from_slice(&digest(key));
        } else {
            key_block[..key.len()].copy_from_slice(key);
        }
        for index in 0..BLOCK {
            inner_key[index] = key_block[index] ^ 0x36;
            block[index] = key_block[index] ^ 0x5c;
        }

        round.update(&inner_key);
        if counter > 1 {
            round.update(&previous);
        }
        round.update(info);
        round.update(&[counter]);
        let inner = round.finish();

        let mut outer = Sha256::new();
        outer.update(&block);
        outer.update(&inner);
        previous = outer.finish();

        let take = (out.len() - written).min(DIGEST);
        out[written..written + take].copy_from_slice(&previous[..take]);
        written += take;
        counter = counter.wrapping_add(1);
    }
    true
}

/// HKDF-Expand-Label, as TLS 1.3 defines it in RFC 8446 §7.1.
///
/// The label is prefixed with `tls13 ` and the whole thing is length-prefixed
/// in the shape the specification gives. Written here rather than in the TLS
/// code because it is the one piece of the key schedule that is pure hashing,
/// and because it has its own test vectors.
pub fn expand_label(secret: &[u8], label: &str, context: &[u8], out: &mut [u8]) -> bool {
    // struct {
    //     uint16 length;
    //     opaque label<7..255>;    // "tls13 " + label
    //     opaque context<0..255>;
    // } HkdfLabel;
    let full = label.len() + 6;
    if full > 255 || context.len() > 255 || out.len() > u16::MAX as usize {
        return false;
    }

    let mut info = alloc::vec::Vec::with_capacity(2 + 1 + full + 1 + context.len());
    info.extend_from_slice(&(out.len() as u16).to_be_bytes());
    info.push(full as u8);
    info.extend_from_slice(b"tls13 ");
    info.extend_from_slice(label.as_bytes());
    info.push(context.len() as u8);
    info.extend_from_slice(context);

    expand(secret, &info, out)
}

/// Whether two slices are the same, in time that does not depend on where they
/// first differ.
///
/// For comparing a MAC against the one that was expected. `==` returns as soon
/// as it finds a difference, and how long that took says which byte it was --
/// which is enough to forge a MAC one byte at a time.
#[must_use]
pub fn same(one: &[u8], other: &[u8]) -> bool {
    if one.len() != other.len() {
        return false;
    }
    let mut difference = 0u8;
    for (a, b) in one.iter().zip(other.iter()) {
        difference |= a ^ b;
    }
    difference == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    /// Bytes from a hexadecimal string, for reading test vectors as they are
    /// published.
    fn unhex(text: &str) -> alloc::vec::Vec<u8> {
        let clean: alloc::vec::Vec<u8> = text
            .bytes()
            .filter(|byte| !byte.is_ascii_whitespace())
            .collect();
        clean
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| {
                let digit = |byte: u8| match byte {
                    b'0'..=b'9' => byte - b'0',
                    b'a'..=b'f' => byte - b'a' + 10,
                    b'A'..=b'F' => byte - b'A' + 10,
                    _ => panic!("not hexadecimal"),
                };
                digit(pair[0]) * 16 + digit(pair[1])
            })
            .collect()
    }

    fn hex(bytes: &[u8]) -> alloc::string::String {
        use core::fmt::Write as _;
        let mut text = alloc::string::String::new();
        for byte in bytes {
            write!(text, "{byte:02x}").unwrap();
        }
        text
    }

    #[test]
    fn the_fips_vectors() {
        // FIPS 180-4's own two examples, and the empty string, which is the one
        // every implementation gets wrong first because the padding is the
        // whole message.
        assert_eq!(
            hex(&digest(b"abc")),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            hex(&digest(
                b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"
            )),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
        assert_eq!(
            hex(&digest(b"")),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn a_million_letters() {
        // The third FIPS vector. It is here because it is the only one that
        // crosses enough blocks to catch a length counter that wraps, and
        // because it crosses the 55/56-byte boundary many times over.
        let mut hash = Sha256::new();
        for _ in 0..1000 {
            hash.update(&[b'a'; 1000]);
        }
        assert_eq!(
            hex(&hash.finish()),
            "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0"
        );
    }

    #[test]
    fn the_padding_boundary_is_right_at_every_length() {
        // 55 bytes fits the length in the same block; 56 does not and needs a
        // second. Getting this wrong gives a hash that is right for most
        // messages and wrong for a few, which is the hardest kind to notice.
        for length in 0..200usize {
            let message = vec![b'x'; length];
            let mut incremental = Sha256::new();
            for byte in &message {
                incremental.update(&[*byte]);
            }
            assert_eq!(
                incremental.finish(),
                digest(&message),
                "length {length} differs byte-at-a-time from all-at-once"
            );
        }
    }

    #[test]
    fn feeding_it_in_pieces_is_the_same_as_all_at_once() {
        let message: alloc::vec::Vec<u8> = (0..500u32).map(|index| index as u8).collect();
        for split in [1usize, 7, 63, 64, 65, 127, 128, 200] {
            let mut hash = Sha256::new();
            for piece in message.chunks(split) {
                hash.update(piece);
            }
            assert_eq!(hash.finish(), digest(&message), "split {split}");
        }
    }

    #[test]
    fn the_rfc_4231_hmac_vectors() {
        // Case 1: a 20-byte key of 0x0b, "Hi There".
        assert_eq!(
            hex(&hmac(&[0x0b; 20], b"Hi There")),
            "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
        );
        // Case 2: a key shorter than the block, which is the ordinary case.
        assert_eq!(
            hex(&hmac(b"Jefe", b"what do ya want for nothing?")),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
        // Case 3: a 20-byte key of 0xaa and fifty 0xdd bytes.
        assert_eq!(
            hex(&hmac(&[0xaa; 20], &[0xdd; 50])),
            "773ea91e36800e46854db8ebd09181a72959098b3ef8c122d9635514ced565fe"
        );
        // Case 6: a key **longer than a block**, which has to be hashed first.
        // The one case an implementation that ignores the rule still passes
        // every other vector.
        assert_eq!(
            hex(&hmac(
                &[0xaa; 131],
                b"Test Using Larger Than Block-Size Key - Hash Key First"
            )),
            "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54"
        );
    }

    #[test]
    fn the_rfc_5869_hkdf_vectors() {
        // Test Case 1: the basic one, with SHA-256.
        let material = [0x0b; 22];
        let salt = unhex("000102030405060708090a0b0c");
        let info = unhex("f0f1f2f3f4f5f6f7f8f9");
        let prk = extract(&salt, &material);
        assert_eq!(
            hex(&prk),
            "077709362c2e32df0ddc3f0dc47bba6390b6c73bb50f9c3122ec844ad7c2b3e5"
        );
        let mut out = [0u8; 42];
        assert!(expand(&prk, &info, &mut out));
        assert_eq!(
            hex(&out),
            "3cb25f25faacd57a90434f64d0362f2a2d2d0a90cf1a5a4c5db02d56ecc4c5bf34007208d5b887185865"
        );
    }

    #[test]
    fn the_rfc_5869_hkdf_vector_with_no_salt() {
        // Test Case 3: empty salt and empty info. The empty salt is the case a
        // careless implementation turns into "no HMAC key", which is different
        // from "a key of thirty-two zero bytes" -- and the specification means
        // the latter.
        let material = [0x0b; 22];
        let prk = extract(&[], &material);
        assert_eq!(
            hex(&prk),
            "19ef24a32c717b167f33a91d6f648bdf96596776afdb6377ac434c1c293ccb04"
        );
        let mut out = [0u8; 42];
        assert!(expand(&prk, &[], &mut out));
        assert_eq!(
            hex(&out),
            "8da4e775a563c18f715f802a063c5a31b8a11f5c5ee1879ec3454e5f3c738d2d9d201395faa4b61a96c8"
        );
    }

    #[test]
    fn a_long_hkdf_output_chains_correctly() {
        // Test Case 2: an 82-byte output, which needs three blocks and so is
        // the one that catches a chain that does not feed the previous block
        // back in.
        let material: alloc::vec::Vec<u8> = (0..80u32).map(|index| index as u8).collect();
        let salt: alloc::vec::Vec<u8> = (0x60..0xb0u32).map(|index| index as u8).collect();
        let info: alloc::vec::Vec<u8> = (0xb0..0x100u32).map(|index| index as u8).collect();
        let prk = extract(&salt, &material);
        assert_eq!(
            hex(&prk),
            "06a6b88c5853361a06104c9ceb35b45cef760014904671014a193f40c15fc244"
        );
        let mut out = [0u8; 82];
        assert!(expand(&prk, &info, &mut out));
        assert_eq!(
            hex(&out),
            "b11e398dc80327a1c8e7f78c596a49344f012eda2d4efad8a050cc4c19afa97c\
             59045a99cac7827271cb41c65e590e09da3275600c2f09b8367793a9aca3db71\
             cc30c58179ec3e87c14c01d5c1f3434f1d87"
        );
    }

    #[test]
    fn asking_for_more_than_hkdf_can_give_is_refused() {
        let mut out = vec![0u8; 255 * DIGEST + 1];
        assert!(!expand(&[0u8; 32], b"", &mut out));
        // And exactly the limit is allowed.
        let mut out = vec![0u8; 255 * DIGEST];
        assert!(expand(&[0u8; 32], b"", &mut out));
    }

    #[test]
    fn expand_label_builds_what_the_tls_specification_says() {
        // From RFC 8446's own worked example (the one in RFC 8448): the
        // client_handshake_traffic_secret's "key" expansion. What is checked
        // here is the *shape* -- the label prefix, the two length bytes -- by
        // hashing it, because the shape is what the two ends have to agree on.
        let secret = unhex("b3eddb126e067f35a780b3abf45e2d8f3b1a950738f52e9600746a0e27a55a21");
        let mut key = [0u8; 16];
        assert!(expand_label(&secret, "key", &[], &mut key));
        assert_eq!(hex(&key), "dbfaa693d1762c5b666af5d950258d01");

        let mut iv = [0u8; 12];
        assert!(expand_label(&secret, "iv", &[], &mut iv));
        assert_eq!(hex(&iv), "5bd3c71b836e0b76bb73265f");
    }

    #[test]
    fn a_comparison_that_does_not_leak_where_it_differed() {
        assert!(same(b"abcdef", b"abcdef"));
        assert!(!same(b"abcdef", b"abcdeg"));
        assert!(!same(b"abcdef", b"zbcdef"));
        assert!(!same(b"abcdef", b"abcde"));
        assert!(same(b"", b""));
    }
}
