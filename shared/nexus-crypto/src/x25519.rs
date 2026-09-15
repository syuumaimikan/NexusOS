//! X25519, the key exchange every TLS 1.3 handshake starts with.
//!
//! RFC 7748, and checked against its test vectors — including the iterated one,
//! which is the only test that catches an implementation that is right for most
//! inputs and wrong for a few.
//!
//! # It reuses the field arithmetic that was already here
//!
//! Ed25519 and X25519 are different curves used differently, but both are
//! arithmetic modulo 2²⁵⁵ − 19, and [`crate::field`] already had that — written,
//! tested and in use for package signatures. So this file is the Montgomery
//! ladder and the clamping, and nothing else.
//!
//! # The ladder does not branch on the secret
//!
//! Every step swaps two points or does not, depending on one bit of the scalar.
//! Written as an `if`, the time it takes would depend on the bit, and the
//! secret could be read off by watching. So the swap is arithmetic:
//! [`crate::field::Fe::select`] picks between two values with a mask, touching
//! both either way.
//!
//! That is the whole reason this is not fifteen lines shorter.

use crate::field::{Fe, ONE, ZERO};

/// How many bytes a private or public key is.
pub const KEY: usize = 32;

/// 121665, which is (a − 2) / 4 for this curve's a = 486662.
///
/// The one curve constant the ladder needs.
const A24: Fe = Fe([121_665, 0, 0, 0]);

/// Clamp a private key, as RFC 7748 §5 requires.
///
/// Clears the bottom three bits so the scalar is a multiple of the cofactor,
/// and sets bit 254 while clearing bit 255 so it is in a fixed range. Both are
/// part of the definition rather than hygiene: a scalar without them is a
/// scalar for a different function, and two implementations that disagree about
/// clamping produce different shared secrets from the same keys.
#[must_use]
pub fn clamp(secret: &[u8; KEY]) -> [u8; KEY] {
    let mut clamped = *secret;
    clamped[0] &= 248;
    clamped[31] &= 127;
    clamped[31] |= 64;
    clamped
}

/// The scalar multiplication at the heart of it: `secret × point`.
///
/// The point is given by its u-coordinate, little-endian, as the wire format
/// always is.
#[must_use]
pub fn multiply(secret: &[u8; KEY], point: &[u8; KEY]) -> [u8; KEY] {
    let scalar = clamp(secret);

    // The high bit of the u-coordinate is ignored, which RFC 7748 §5 requires
    // in so many words. A peer that sets it is not sending a different point;
    // it is sending the same point with a stray bit, and refusing would break
    // interoperability for no gain.
    let mut u = *point;
    u[31] &= 127;
    let x1 = Fe::from_bytes(&u);

    // The ladder's state: two projective points, (x2:z2) and (x3:z3).
    let mut x2 = ONE;
    let mut z2 = ZERO;
    let mut x3 = x1;
    let mut z3 = ONE;
    // Whether the two are currently the other way round. Carried rather than
    // acted on immediately, so that each step does exactly one conditional
    // swap instead of two.
    let mut swapped = false;

    for position in (0..255).rev() {
        let bit = ((scalar[position >> 3] >> (position & 7)) & 1) == 1;
        let swap = swapped ^ bit;
        let (a, b) = (x2.select(x3, swap), x3.select(x2, swap));
        x2 = a;
        x3 = b;
        let (a, b) = (z2.select(z3, swap), z3.select(z2, swap));
        z2 = a;
        z3 = b;
        swapped = bit;

        // One Montgomery ladder step, exactly as RFC 7748 §5 writes it.
        let a = x2.add(z2);
        let aa = a.square();
        let b = x2.subtract(z2);
        let bb = b.square();
        let e = aa.subtract(bb);
        let c = x3.add(z3);
        let d = x3.subtract(z3);
        let da = d.multiply(a);
        let cb = c.multiply(b);
        x3 = da.add(cb).square();
        z3 = x1.multiply(da.subtract(cb).square());
        x2 = aa.multiply(bb);
        z2 = e.multiply(aa.add(A24.multiply(e)));
    }

    // The last swap, which the loop deferred. Only the first of each pair is
    // wanted -- (x3:z3) has done its work -- but the selection still touches
    // both, because that is what makes it take the same time either way.
    x2 = x2.select(x3, swapped);
    z2 = z2.select(z3, swapped);

    x2.multiply(z2.invert()).to_bytes()
}

/// The public key for a private one: `secret × 9`.
#[must_use]
pub fn public(secret: &[u8; KEY]) -> [u8; KEY] {
    let mut base = [0u8; KEY];
    base[0] = 9;
    multiply(secret, &base)
}

/// The shared secret from our private key and their public one.
///
/// `None` when the result is all zeros, which is what happens if the peer sent
/// a point of small order. RFC 7748 §6.1 says a check is optional for X25519
/// and recommended where the result is used directly; TLS 1.3 requires it
/// (RFC 8446 §7.4.2), so it is done here rather than left to the caller.
#[must_use]
pub fn shared(secret: &[u8; KEY], theirs: &[u8; KEY]) -> Option<[u8; KEY]> {
    let result = multiply(secret, theirs);
    if result.iter().all(|byte| *byte == 0) {
        return None;
    }
    Some(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec::Vec;

    fn unhex(text: &str) -> [u8; 32] {
        let clean: Vec<u8> = text
            .bytes()
            .filter(|byte| !byte.is_ascii_whitespace())
            .collect();
        let mut out = [0u8; 32];
        for (slot, pair) in out.iter_mut().zip(clean.as_chunks::<2>().0.iter()) {
            let digit = |byte: u8| match byte {
                b'0'..=b'9' => byte - b'0',
                b'a'..=b'f' => byte - b'a' + 10,
                b'A'..=b'F' => byte - b'A' + 10,
                _ => panic!("not hexadecimal"),
            };
            *slot = digit(pair[0]) * 16 + digit(pair[1]);
        }
        out
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
    fn the_rfc_7748_scalar_multiplication_vectors() {
        // §5.2, both of them.
        let scalar = unhex("a546e36bf0527c9d3b16154b82465edd62144c0ac1fc5a18506a2244ba449ac4");
        let point = unhex("e6db6867583030db3594c1a424b15f7c726624ec26b3353b10a903a6d0ab1c4c");
        assert_eq!(
            hex(&multiply(&scalar, &point)),
            "c3da55379de9c6908e94ea4df28d084f32eccf03491c71f754b4075577a28552"
        );

        let scalar = unhex("4b66e9d4d1b4673c5ad22691957d6af5c11b6421e0ea01d42ca4169e7918ba0d");
        let point = unhex("e5210f12786811d3f4b7959d0538ae2c31dbe7106fc03c3efc4cd549c715a493");
        assert_eq!(
            hex(&multiply(&scalar, &point)),
            "95cbde9476e8907d7aade45cb4b873f88b595a68799fa152e6f8f7647aac7957"
        );
    }

    #[test]
    fn the_rfc_7748_public_key_vectors() {
        // §6.1: Alice's and Bob's keys, and the secret they agree on. This is
        // the whole of X25519 as it is actually used.
        let alice = unhex("77076d0a7318a57d3c16c17251b26645df4c2f87ebc0992ab177fba51db92c2a");
        let bob = unhex("5dab087e624a8a4b79e17f8b83800ee66f3bb1292618b6fd1c2f8b27ff88e0eb");

        assert_eq!(
            hex(&public(&alice)),
            "8520f0098930a754748b7ddcb43ef75a0dbf3a0d26381af4eba4a98eaa9b4e6a"
        );
        assert_eq!(
            hex(&public(&bob)),
            "de9edb7d7b7dc1b4d35b61c2ece435373f8343c85b78674dadfc7e146f882b4f"
        );

        let expected = "4a5d9d5ba4ce2de1728e3bf480350f25e07e21c947d19e3376f09b3c1e161742";
        assert_eq!(hex(&shared(&alice, &public(&bob)).unwrap()), expected);
        assert_eq!(hex(&shared(&bob, &public(&alice)).unwrap()), expected);
    }

    #[test]
    fn the_iterated_vector() {
        // §5.2's iterated test, one thousand rounds.
        //
        // This is the test worth having. A ladder that is right for most inputs
        // and wrong for a few passes every single-shot vector and fails here,
        // because a thousand rounds walks through a thousand different scalars
        // and points -- and the RFC publishes the answer at 1, at 1000 and at a
        // million.
        let mut k = unhex("0900000000000000000000000000000000000000000000000000000000000000");
        let mut u = k;
        for round in 1..=1000u32 {
            let out = multiply(&k, &u);
            u = k;
            k = out;
            if round == 1 {
                assert_eq!(
                    hex(&k),
                    "422c8e7a6227d7bca1350b3e2bb7279f7897b87bb6854b783c60e80311ae3079"
                );
            }
        }
        assert_eq!(
            hex(&k),
            "684cf59ba83309552800ef566f2f4d3c1c3887c49360e3875f2eb94d99532c51"
        );
    }

    #[test]
    fn clamping_is_what_the_specification_says() {
        let clamped = clamp(&[0xffu8; 32]);
        assert_eq!(clamped[0], 0xf8, "the bottom three bits are cleared");
        assert_eq!(clamped[31], 0x7f, "bit 255 cleared and bit 254 set");

        let clamped = clamp(&[0x00u8; 32]);
        assert_eq!(clamped[0], 0x00);
        assert_eq!(clamped[31], 0x40, "bit 254 is set even on a zero scalar");
    }

    #[test]
    fn the_high_bit_of_a_peers_point_is_ignored() {
        // RFC 7748 §5 in so many words. A peer that sets it is sending the same
        // point with a stray bit, and refusing would break interoperability for
        // nothing.
        let secret = unhex("77076d0a7318a57d3c16c17251b26645df4c2f87ebc0992ab177fba51db92c2a");
        let point = unhex("de9edb7d7b7dc1b4d35b61c2ece435373f8343c85b78674dadfc7e146f882b4f");
        let mut with_bit = point;
        with_bit[31] |= 0x80;
        assert_eq!(multiply(&secret, &point), multiply(&secret, &with_bit));
    }

    #[test]
    fn a_small_order_point_gives_nothing() {
        // The all-zero point, and the other small-order ones RFC 7748 lists.
        // Every one of them makes the shared secret all zeros, which is a
        // secret the peer knew before it started -- so TLS requires the check
        // and this refuses rather than returning it.
        for small in [
            "0000000000000000000000000000000000000000000000000000000000000000",
            "0100000000000000000000000000000000000000000000000000000000000000",
            "e0eb7a7c3b41b8ae1656e3faf19fc46ada098deb9c32b1fd866205165f49b800",
            "5f9c95bca3508c24b1d0b1559c83ef5b04445cc4581c8e86d8224eddd09f1157",
            "ecffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff7f",
        ] {
            let secret = unhex("77076d0a7318a57d3c16c17251b26645df4c2f87ebc0992ab177fba51db92c2a");
            assert!(
                shared(&secret, &unhex(small)).is_none(),
                "{small} should give no shared secret"
            );
        }
    }

    #[test]
    fn two_sides_agree_for_many_keys() {
        // Not a vector, a property: whatever the keys, both ends compute the
        // same thing. A ladder with a swap in the wrong place passes the fixed
        // vectors and fails this.
        for seed in 1..40u8 {
            let mut alice = [0u8; 32];
            let mut bob = [0u8; 32];
            for index in 0..32 {
                alice[index] = seed.wrapping_mul(index as u8).wrapping_add(7);
                bob[index] = seed.wrapping_add(index as u8).wrapping_mul(3);
            }
            let one = shared(&alice, &public(&bob));
            let other = shared(&bob, &public(&alice));
            assert_eq!(one, other, "seed {seed}");
            assert!(one.is_some(), "seed {seed} gave nothing");
        }
    }
}
