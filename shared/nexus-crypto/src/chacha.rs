//! ChaCha20-Poly1305, the AEAD that TLS 1.3 connections are encrypted with.
//!
//! RFC 8439, and checked against its test vectors rather than against itself —
//! for the same reason as every other algorithm here, and more so: an AEAD that
//! is subtly wrong does not produce a slightly wrong answer. It produces a
//! connection that either refuses to work or, if the other end made the same
//! mistake, works and protects nothing.
//!
//! # Why this one and not AES-GCM
//!
//! Both are mandatory to implement for TLS 1.3 clients and every server
//! supports at least one; nearly all support both. ChaCha20-Poly1305 is chosen
//! here because it is **all arithmetic on 32-bit words**.
//!
//! AES done in software is a table lookup per byte per round, and the table
//! index depends on the key — so the time it takes depends on which cache lines
//! are warm, and that is a side channel that has been used to recover keys from
//! across a network. Doing AES *safely* in software means bitslicing it, which
//! is several times the code and much slower. Doing it in hardware means AES-NI,
//! which means detecting the instruction and writing assembly for it.
//!
//! ChaCha has no tables and no data-dependent branches. Every operation is an
//! add, an exclusive-or or a rotate on a 32-bit word, and it takes the same time
//! whatever the key is. For a system with no AES instructions wired up, it is
//! the honest choice rather than the fashionable one.
//!
//! # What is not here
//!
//! AES-GCM, for the reason above. A server that will not speak
//! ChaCha20-Poly1305 is a server this machine cannot talk to, and the TLS code
//! says so by name rather than failing obscurely.

use core::ops::Range;

/// The key, which is always 256 bits.
pub const KEY: usize = 32;

/// The nonce, which is always 96 bits in the AEAD construction.
pub const NONCE: usize = 12;

/// The authentication tag, which is always 128 bits.
pub const TAG: usize = 16;

/// One ChaCha20 block.
const BLOCK: usize = 64;

/// "expand 32-byte k", as four little-endian words.
///
/// The constant is written out rather than spelled as bytes so that the state's
/// word order is visible: this is the one place where getting the endianness
/// wrong produces a cipher that works and is not ChaCha20.
const SIGMA: [u32; 4] = [0x6170_7865, 0x3320_646e, 0x7962_2d32, 0x6b20_6574];

/// One quarter-round, on four indices of the state.
///
/// The whole of ChaCha is this, twenty times, on different columns and
/// diagonals.
#[inline]
fn quarter(state: &mut [u32; 16], a: usize, b: usize, c: usize, d: usize) {
    state[a] = state[a].wrapping_add(state[b]);
    state[d] = (state[d] ^ state[a]).rotate_left(16);
    state[c] = state[c].wrapping_add(state[d]);
    state[b] = (state[b] ^ state[c]).rotate_left(12);
    state[a] = state[a].wrapping_add(state[b]);
    state[d] = (state[d] ^ state[a]).rotate_left(8);
    state[c] = state[c].wrapping_add(state[d]);
    state[b] = (state[b] ^ state[c]).rotate_left(7);
}

/// The state for one block, before it is stirred.
fn state_of(key: &[u8; KEY], counter: u32, nonce: &[u8; NONCE]) -> [u32; 16] {
    let word = |bytes: &[u8], at: usize| {
        u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
    };
    [
        SIGMA[0],
        SIGMA[1],
        SIGMA[2],
        SIGMA[3],
        word(key, 0),
        word(key, 4),
        word(key, 8),
        word(key, 12),
        word(key, 16),
        word(key, 20),
        word(key, 24),
        word(key, 28),
        counter,
        word(nonce, 0),
        word(nonce, 4),
        word(nonce, 8),
    ]
}

/// One sixty-four byte block of key stream.
#[must_use]
pub fn block(key: &[u8; KEY], counter: u32, nonce: &[u8; NONCE]) -> [u8; BLOCK] {
    let start = state_of(key, counter, nonce);
    let mut state = start;

    // Twenty rounds: ten of columns and diagonals, in pairs.
    for _ in 0..10 {
        quarter(&mut state, 0, 4, 8, 12);
        quarter(&mut state, 1, 5, 9, 13);
        quarter(&mut state, 2, 6, 10, 14);
        quarter(&mut state, 3, 7, 11, 15);
        quarter(&mut state, 0, 5, 10, 15);
        quarter(&mut state, 1, 6, 11, 12);
        quarter(&mut state, 2, 7, 8, 13);
        quarter(&mut state, 3, 4, 9, 14);
    }

    // Added back to the starting state, which is what stops the permutation
    // being invertible and is easy to leave out.
    let mut out = [0u8; BLOCK];
    for (chunk, (mixed, original)) in out
        .as_chunks_mut::<4>()
        .0
        .iter_mut()
        .zip(state.iter().zip(start.iter()))
    {
        *chunk = mixed.wrapping_add(*original).to_le_bytes();
    }
    out
}

/// Encrypt or decrypt in place. The two are the same operation.
pub fn apply(key: &[u8; KEY], counter: u32, nonce: &[u8; NONCE], data: &mut [u8]) {
    for (index, chunk) in data.chunks_mut(BLOCK).enumerate() {
        // The counter is 32 bits and wraps at four gibibytes of one message.
        // Wrapping rather than saturating: a message that long is not something
        // this system produces, and a silently repeated key stream is worse
        // than an obviously wrong one.
        let stream = block(key, counter.wrapping_add(index as u32), nonce);
        for (byte, key_byte) in chunk.iter_mut().zip(stream.iter()) {
            *byte ^= key_byte;
        }
    }
}

/// Poly1305, the one-time authenticator.
///
/// Arithmetic modulo 2¹³⁰ − 5, carried in five 26-bit limbs so that products
/// fit in a `u64` without a 128-bit type. The clamping of `r` is part of the
/// specification and not an optimisation: without it the modular reduction
/// below can carry further than the limbs allow.
struct Poly1305 {
    r: [u32; 5],
    h: [u32; 5],
    pad: [u32; 4],
    buffer: [u8; 16],
    buffered: usize,
}

impl Poly1305 {
    fn new(key: &[u8; 32]) -> Self {
        let word = |at: usize| u32::from_le_bytes([key[at], key[at + 1], key[at + 2], key[at + 3]]);
        // The clamp, limb by limb. RFC 8439 §2.5 clears certain bits of `r`,
        // and it is not an optimisation: without it the reduction below can
        // carry further than five 26-bit limbs hold. The masks are the
        // specification's own, written out rather than derived, because a
        // clever expression here is one nobody can check against the document.
        let t0 = word(0);
        let t1 = word(4);
        let t2 = word(8);
        let t3 = word(12);
        let r = [
            t0 & 0x03ff_ffff,
            ((t0 >> 26) | (t1 << 6)) & 0x03ff_ff03,
            ((t1 >> 20) | (t2 << 12)) & 0x03ff_c0ff,
            ((t2 >> 14) | (t3 << 18)) & 0x03f0_3fff,
            (t3 >> 8) & 0x000f_ffff,
        ];

        Self {
            r,
            h: [0; 5],
            pad: [word(16), word(20), word(24), word(28)],
            buffer: [0; 16],
            buffered: 0,
        }
    }

    /// One sixteen-byte block, with the high bit set unless this is a short
    /// final block.
    fn absorb(&mut self, block: &[u8; 16], final_bit: u32) {
        let word = |at: usize| {
            u32::from_le_bytes([block[at], block[at + 1], block[at + 2], block[at + 3]])
        };
        let t0 = word(0);
        let t1 = word(4);
        let t2 = word(8);
        let t3 = word(12);

        self.h[0] += t0 & 0x03ff_ffff;
        self.h[1] += ((t0 >> 26) | (t1 << 6)) & 0x03ff_ffff;
        self.h[2] += ((t1 >> 20) | (t2 << 12)) & 0x03ff_ffff;
        self.h[3] += ((t2 >> 14) | (t3 << 18)) & 0x03ff_ffff;
        self.h[4] += (t3 >> 8) | final_bit;

        // h *= r, modulo 2^130 - 5. The 5s come from that reduction: a bit at
        // position 130 is worth 5 at position 0.
        let s: [u64; 4] = [
            u64::from(self.r[1]) * 5,
            u64::from(self.r[2]) * 5,
            u64::from(self.r[3]) * 5,
            u64::from(self.r[4]) * 5,
        ];
        let h: [u64; 5] = [
            u64::from(self.h[0]),
            u64::from(self.h[1]),
            u64::from(self.h[2]),
            u64::from(self.h[3]),
            u64::from(self.h[4]),
        ];
        let r: [u64; 5] = [
            u64::from(self.r[0]),
            u64::from(self.r[1]),
            u64::from(self.r[2]),
            u64::from(self.r[3]),
            u64::from(self.r[4]),
        ];

        let d0 = h[0] * r[0] + h[1] * s[3] + h[2] * s[2] + h[3] * s[1] + h[4] * s[0];
        let d1 = h[0] * r[1] + h[1] * r[0] + h[2] * s[3] + h[3] * s[2] + h[4] * s[1];
        let d2 = h[0] * r[2] + h[1] * r[1] + h[2] * r[0] + h[3] * s[3] + h[4] * s[2];
        let d3 = h[0] * r[3] + h[1] * r[2] + h[2] * r[1] + h[3] * r[0] + h[4] * s[3];
        let d4 = h[0] * r[4] + h[1] * r[3] + h[2] * r[2] + h[3] * r[1] + h[4] * r[0];

        let mut carry = d0 >> 26;
        self.h[0] = (d0 & 0x03ff_ffff) as u32;
        let d1 = d1 + carry;
        carry = d1 >> 26;
        self.h[1] = (d1 & 0x03ff_ffff) as u32;
        let d2 = d2 + carry;
        carry = d2 >> 26;
        self.h[2] = (d2 & 0x03ff_ffff) as u32;
        let d3 = d3 + carry;
        carry = d3 >> 26;
        self.h[3] = (d3 & 0x03ff_ffff) as u32;
        let d4 = d4 + carry;
        carry = d4 >> 26;
        self.h[4] = (d4 & 0x03ff_ffff) as u32;
        self.h[0] += (carry * 5) as u32;
        let carry = self.h[0] >> 26;
        self.h[0] &= 0x03ff_ffff;
        self.h[1] += carry;
    }

    fn update(&mut self, mut bytes: &[u8]) {
        if self.buffered > 0 {
            let take = (16 - self.buffered).min(bytes.len());
            self.buffer[self.buffered..self.buffered + take].copy_from_slice(&bytes[..take]);
            self.buffered += take;
            bytes = &bytes[take..];
            if self.buffered == 16 {
                let block = self.buffer;
                self.absorb(&block, 1 << 24);
                self.buffered = 0;
            }
        }
        while bytes.len() >= 16 {
            let mut block = [0u8; 16];
            block.copy_from_slice(&bytes[..16]);
            self.absorb(&block, 1 << 24);
            bytes = &bytes[16..];
        }
        if !bytes.is_empty() {
            self.buffer[..bytes.len()].copy_from_slice(bytes);
            self.buffered = bytes.len();
        }
    }

    fn finish(mut self) -> [u8; TAG] {
        if self.buffered > 0 {
            // A short final block: a one byte after the data, and no high bit.
            let at = self.buffered;
            self.buffer[at] = 1;
            for byte in self.buffer.iter_mut().skip(at + 1) {
                *byte = 0;
            }
            let block = self.buffer;
            self.absorb(&block, 0);
        }

        // Fully carry.
        let mut carry = self.h[1] >> 26;
        self.h[1] &= 0x03ff_ffff;
        self.h[2] += carry;
        carry = self.h[2] >> 26;
        self.h[2] &= 0x03ff_ffff;
        self.h[3] += carry;
        carry = self.h[3] >> 26;
        self.h[3] &= 0x03ff_ffff;
        self.h[4] += carry;
        carry = self.h[4] >> 26;
        self.h[4] &= 0x03ff_ffff;
        self.h[0] += carry * 5;
        carry = self.h[0] >> 26;
        self.h[0] &= 0x03ff_ffff;
        self.h[1] += carry;

        // h + -p, to see whether h is already above the modulus.
        let mut g = [0u32; 5];
        let mut borrow = self.h[0].wrapping_add(5);
        g[0] = borrow & 0x03ff_ffff;
        borrow >>= 26;
        for (slot, limb) in g[1..4].iter_mut().zip(self.h[1..4].iter()) {
            borrow += *limb;
            *slot = borrow & 0x03ff_ffff;
            borrow >>= 26;
        }
        let g4 = self.h[4].wrapping_add(borrow).wrapping_sub(1 << 26);
        g[4] = g4;

        // Chosen without branching on the value: `g4 >> 31` is 1 when the
        // subtraction went negative, which says h was already below p.
        let mask = 0u32.wrapping_sub((g4 >> 31) ^ 1);
        for (limb, above) in self.h.iter_mut().zip(g.iter()) {
            *limb = (*limb & !mask) | (above & mask);
        }

        // Back into four 32-bit words, then add the second half of the key.
        let mut h = [0u32; 4];
        h[0] = self.h[0] | (self.h[1] << 26);
        h[1] = (self.h[1] >> 6) | (self.h[2] << 20);
        h[2] = (self.h[2] >> 12) | (self.h[3] << 14);
        h[3] = (self.h[3] >> 18) | (self.h[4] << 8);

        let mut tag = [0u8; TAG];
        let mut carry = 0u64;
        for (chunk, (word, pad)) in tag
            .as_chunks_mut::<4>()
            .0
            .iter_mut()
            .zip(h.iter().zip(self.pad.iter()))
        {
            let sum = u64::from(*word) + u64::from(*pad) + carry;
            carry = sum >> 32;
            *chunk = (sum as u32).to_le_bytes();
        }
        tag
    }
}

/// The Poly1305 key for one message, from the cipher key and nonce.
///
/// Block zero of the key stream, which is why the data starts at block one.
/// Using block zero for data as well would use the same bytes as the
/// authentication key, and the whole construction would fall apart.
fn one_time_key(key: &[u8; KEY], nonce: &[u8; NONCE]) -> [u8; 32] {
    let stream = block(key, 0, nonce);
    let mut out = [0u8; 32];
    out.copy_from_slice(&stream[..32]);
    out
}

/// The authenticated data and the ciphertext, laid out as RFC 8439 §2.8 says.
fn authenticate(mac: &mut Poly1305, extra: &[u8], ciphertext: &[u8]) {
    mac.update(extra);
    // Each part is padded to a multiple of sixteen. Without the padding, an
    // attacker can move bytes between the two parts without changing the tag.
    let pad = [0u8; 16];
    if !extra.len().is_multiple_of(16) {
        mac.update(&pad[..16 - extra.len() % 16]);
    }
    mac.update(ciphertext);
    if !ciphertext.len().is_multiple_of(16) {
        mac.update(&pad[..16 - ciphertext.len() % 16]);
    }
    mac.update(&(extra.len() as u64).to_le_bytes());
    mac.update(&(ciphertext.len() as u64).to_le_bytes());
}

/// Encrypt `data` in place and append the tag.
///
/// `data` must have [`TAG`] bytes of room past `plaintext` for the tag. The
/// range says which part of it is the message; everything after is written.
pub fn seal(
    key: &[u8; KEY],
    nonce: &[u8; NONCE],
    extra: &[u8],
    data: &mut [u8],
    plaintext: Range<usize>,
) -> bool {
    if plaintext.end + TAG > data.len() {
        return false;
    }
    let mac_key = one_time_key(key, nonce);
    // Block one, because block zero made the authentication key.
    apply(key, 1, nonce, &mut data[plaintext.clone()]);

    let mut mac = Poly1305::new(&mac_key);
    authenticate(&mut mac, extra, &data[plaintext.clone()]);
    let tag = mac.finish();
    data[plaintext.end..plaintext.end + TAG].copy_from_slice(&tag);
    true
}

/// Check the tag and decrypt in place.
///
/// Returns how many bytes of plaintext there are, or `None` if the tag does not
/// match — in which case **nothing is decrypted**. That order is the whole
/// point of an AEAD: decrypting first and checking afterwards hands a caller
/// bytes an attacker chose, and every serious protocol failure of the last
/// twenty years has some version of that in it.
#[must_use]
pub fn open(key: &[u8; KEY], nonce: &[u8; NONCE], extra: &[u8], data: &mut [u8]) -> Option<usize> {
    if data.len() < TAG {
        return None;
    }
    let split = data.len() - TAG;
    let (ciphertext, tag) = data.split_at_mut(split);

    let mac_key = one_time_key(key, nonce);
    let mut mac = Poly1305::new(&mac_key);
    authenticate(&mut mac, extra, ciphertext);
    let expected = mac.finish();

    if !crate::sha256::same(&expected, tag) {
        return None;
    }
    apply(key, 1, nonce, ciphertext);
    Some(split)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;
    use alloc::vec::Vec;

    fn unhex(text: &str) -> Vec<u8> {
        let clean: Vec<u8> = text
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
    fn the_rfc_8439_block_vector() {
        // §2.3.2: the worked example, key 00..1f, nonce 00:00:00:09:00:00:00:4a
        // with counter 1.
        let key: [u8; 32] = core::array::from_fn(|index| index as u8);
        let nonce = unhex("000000090000004a00000000");
        let mut fixed = [0u8; NONCE];
        fixed.copy_from_slice(&nonce);
        let out = block(&key, 1, &fixed);
        assert_eq!(
            hex(&out),
            "10f1e7e4d13b5915500fdd1fa32071c4c7d1f4c733c068030422aa9ac3d46c4e\
             d2826446079faa0914c2d705d98b02a2b5129cd1de164eb9cbd083e8a2503c4e"
        );
    }

    #[test]
    fn the_rfc_8439_encryption_vector() {
        // §2.4.2: the Sunscreen text.
        let key: [u8; 32] = core::array::from_fn(|index| index as u8);
        let nonce = {
            let mut fixed = [0u8; NONCE];
            fixed.copy_from_slice(&unhex("000000000000004a00000000"));
            fixed
        };
        // The backslash continuation in a byte string keeps the newline out
        // but not the indentation that follows it, so this is one long line:
        // the vector is of the sentence, not of the sentence with runs of
        // spaces in the middle.
        let mut text = b"Ladies and Gentlemen of the class of '99: If I could offer you only one tip for the future, sunscreen would be it."
            .to_vec();
        apply(&key, 1, &nonce, &mut text);
        assert_eq!(
            hex(&text),
            "6e2e359a2568f98041ba0728dd0d6981e97e7aec1d4360c20a27afccfd9fae0b\
             f91b65c5524733ab8f593dabcd62b3571639d624e65152ab8f530c359f0861d8\
             07ca0dbf500d6a6156a38e088a22b65e52bc514d16ccf806818ce91ab7793736\
             5af90bbf74a35be6b40b8eedf2785e42874d"
        );
    }

    #[test]
    fn the_rfc_8439_poly1305_vector() {
        // §2.5.2.
        let key = unhex("85d6be7857556d337f4452fe42d506a80103808afb0db2fd4abff6af4149f51b");
        let mut fixed = [0u8; 32];
        fixed.copy_from_slice(&key);
        let mut mac = Poly1305::new(&fixed);
        mac.update(b"Cryptographic Forum Research Group");
        assert_eq!(hex(&mac.finish()), "a8061dc1305136c6c22b8baf0c0127a9");
    }

    #[test]
    fn the_rfc_8439_aead_vector() {
        // §2.8.2: the full construction, with additional data.
        let key = {
            let mut fixed = [0u8; KEY];
            fixed.copy_from_slice(&unhex(
                "808182838485868788898a8b8c8d8e8f909192939495969798999a9b9c9d9e9f",
            ));
            fixed
        };
        let nonce = {
            let mut fixed = [0u8; NONCE];
            fixed.copy_from_slice(&unhex("070000004041424344454647"));
            fixed
        };
        let extra = unhex("50515253c0c1c2c3c4c5c6c7");
        let message =
            b"Ladies and Gentlemen of the class of '99: If I could offer you only one tip \
for the future, sunscreen would be it.";

        let mut buffer = vec![0u8; message.len() + TAG];
        buffer[..message.len()].copy_from_slice(message);
        assert!(seal(&key, &nonce, &extra, &mut buffer, 0..message.len()));

        assert_eq!(
            hex(&buffer[..message.len()]),
            "d31a8d34648e60db7b86afbc53ef7ec2a4aded51296e08fea9e2b5a736ee62d6\
             3dbea45e8ca9671282fafb69da92728b1a71de0a9e060b2905d6a5b67ecd3b36\
             92ddbd7f2d778b8c9803aee328091b58fab324e4fad675945585808b4831d7bc\
             3ff4def08e4b7a9de576d26586cec64b6116"
        );
        assert_eq!(
            hex(&buffer[message.len()..]),
            "1ae10b594f09e26a7e902ecbd0600691"
        );

        // And back again.
        let opened = open(&key, &nonce, &extra, &mut buffer).expect("the tag should match");
        assert_eq!(&buffer[..opened], message.as_slice());
    }

    #[test]
    fn a_changed_byte_of_ciphertext_is_refused() {
        let key = [7u8; KEY];
        let nonce = [9u8; NONCE];
        let message = b"the quick brown fox";
        let mut buffer = vec![0u8; message.len() + TAG];
        buffer[..message.len()].copy_from_slice(message);
        assert!(seal(&key, &nonce, b"extra", &mut buffer, 0..message.len()));

        let mut broken = buffer.clone();
        broken[3] ^= 1;
        assert!(open(&key, &nonce, b"extra", &mut broken).is_none());
        // And nothing was decrypted: the bytes are as they were.
        assert_eq!(
            broken[..message.len()],
            {
                let mut expected = buffer[..message.len()].to_vec();
                expected[3] ^= 1;
                expected
            }[..]
        );
    }

    #[test]
    fn a_changed_byte_of_additional_data_is_refused() {
        // The additional data is not encrypted, so this is the check that it is
        // *authenticated* -- which is the only thing that makes it useful.
        let key = [7u8; KEY];
        let nonce = [9u8; NONCE];
        let message = b"the quick brown fox";
        let mut buffer = vec![0u8; message.len() + TAG];
        buffer[..message.len()].copy_from_slice(message);
        assert!(seal(&key, &nonce, b"header", &mut buffer, 0..message.len()));
        assert!(open(&key, &nonce, b"heaser", &mut buffer).is_none());
    }

    #[test]
    fn a_changed_tag_is_refused() {
        let key = [7u8; KEY];
        let nonce = [9u8; NONCE];
        let mut buffer = vec![0u8; 4 + TAG];
        buffer[..4].copy_from_slice(b"abcd");
        assert!(seal(&key, &nonce, b"", &mut buffer, 0..4));
        let last = buffer.len() - 1;
        buffer[last] ^= 0x80;
        assert!(open(&key, &nonce, b"", &mut buffer).is_none());
    }

    #[test]
    fn the_wrong_nonce_is_refused() {
        let key = [7u8; KEY];
        let mut buffer = vec![0u8; 4 + TAG];
        buffer[..4].copy_from_slice(b"abcd");
        assert!(seal(&key, &[1u8; NONCE], b"", &mut buffer, 0..4));
        assert!(open(&key, &[2u8; NONCE], b"", &mut buffer).is_none());
    }

    #[test]
    fn an_empty_message_still_authenticates() {
        // Length zero exercises the padding arithmetic at its edge, and an AEAD
        // that cannot seal nothing cannot send an empty TLS record.
        let key = [3u8; KEY];
        let nonce = [4u8; NONCE];
        let mut buffer = vec![0u8; TAG];
        assert!(seal(&key, &nonce, b"", &mut buffer, 0..0));
        assert_eq!(open(&key, &nonce, b"", &mut buffer), Some(0));
    }

    #[test]
    fn every_length_around_the_block_boundary_round_trips() {
        // Sixteen for Poly1305's block and sixty-four for ChaCha's. The
        // padding rules differ at each, and a length that is a multiple of one
        // and not the other is where an off-by-one lives.
        let key = [0x5au8; KEY];
        let nonce = [0xa5u8; NONCE];
        for length in 0..200usize {
            let message: Vec<u8> = (0..length).map(|index| index as u8).collect();
            let mut buffer = vec![0u8; length + TAG];
            buffer[..length].copy_from_slice(&message);
            assert!(
                seal(&key, &nonce, b"aad", &mut buffer, 0..length),
                "{length}"
            );
            assert_eq!(
                open(&key, &nonce, b"aad", &mut buffer),
                Some(length),
                "length {length} did not open"
            );
            assert_eq!(&buffer[..length], &message[..], "length {length} differs");
        }
    }

    #[test]
    fn sealing_without_room_for_the_tag_is_refused() {
        let mut buffer = vec![0u8; 8];
        assert!(!seal(&[0u8; KEY], &[0u8; NONCE], b"", &mut buffer, 0..8));
    }
}
