//! Ed25519: the curve, and signing and verifying on it.
//!
//! The curve is `-x² + y² = 1 + d·x²·y²` over the field of integers modulo
//! 2²⁵⁵ − 19, with `d = -121665/121666`. Points are held in extended
//! coordinates `(X : Y : Z : T)` where `x = X/Z`, `y = Y/Z` and `X·Y = Z·T`,
//! because in those coordinates addition needs no inversion and no special case
//! for doubling — the same formula works for a point and itself, which is one
//! fewer branch that could depend on a secret.
//!
//! # Why a point is thirty-two bytes
//!
//! Only `y` is stored, with the low bit of `x` in the spare top bit. Given `y`
//! the curve equation gives `x²`, and the square root is one of two values that
//! differ only in sign — so one bit says which. That is why decompression can
//! *fail*: not every `y` has a square root, and thirty-two bytes chosen at
//! random are usually not a point at all.
//!
//! # What the signature actually says
//!
//! `S·B = R + k·A`, where `B` is the standard base point, `A` is the public
//! key, `R` is the first half of the signature and `k` is the hash of `R`, `A`
//! and the message. Only somebody who knows the private key behind `A` can
//! produce an `S` that makes those two points equal, and every part of the
//! message is inside `k`.

use crate::field::{self, Fe};
use crate::scalar;
use crate::sha512;

/// Bytes in a public key.
pub const PUBLIC_KEY: usize = 32;
/// Bytes in a private key.
pub const SECRET_KEY: usize = 32;
/// Bytes in a signature.
pub const SIGNATURE: usize = 64;

/// A point on the curve, in extended coordinates.
#[derive(Clone, Copy)]
pub struct Point {
    x: Fe,
    y: Fe,
    z: Fe,
    t: Fe,
}

/// The identity: `(0 : 1 : 1 : 0)`, which is the point `(0, 1)`.
const IDENTITY: Point = Point {
    x: field::ZERO,
    y: field::ONE,
    z: field::ONE,
    t: field::ZERO,
};

/// `d`, the curve's one parameter, as bytes.
///
/// `-121665/121666` worked out once. Written down rather than computed at
/// startup because it is a constant of the curve, not of this program.
const D_BYTES: [u8; 32] = [
    0xA3, 0x78, 0x59, 0x13, 0xCA, 0x4D, 0xEB, 0x75, 0xAB, 0xD8, 0x41, 0x41, 0x4D, 0x0A, 0x70, 0x00,
    0x98, 0xE8, 0x79, 0x77, 0x79, 0x40, 0xC7, 0x8C, 0x73, 0xFE, 0x6F, 0x2B, 0xEE, 0x6C, 0x03, 0x52,
];

/// `sqrt(-1)`, which recovering `x` needs when the first candidate is wrong.
const SQRT_MINUS_ONE: [u8; 32] = [
    0xB0, 0xA0, 0x0E, 0x4A, 0x27, 0x1B, 0xEE, 0xC4, 0x78, 0xE4, 0x2F, 0xAD, 0x06, 0x18, 0x43, 0x2F,
    0xA7, 0xD7, 0xFB, 0x3D, 0x99, 0x00, 0x4D, 0x2B, 0x0B, 0xDF, 0xC1, 0x4F, 0x80, 0x24, 0x83, 0x2B,
];

/// The base point's `y`, which is `4/5`.
const BASE_Y: [u8; 32] = [
    0x58, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66,
    0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66, 0x66,
];

fn d() -> Fe {
    Fe::from_bytes(&D_BYTES)
}

fn sqrt_minus_one() -> Fe {
    Fe::from_bytes(&SQRT_MINUS_ONE)
}

/// The standard base point.
fn base() -> Point {
    // Its `y` is 4/5 and its `x` is even, which is all that is needed to
    // recover it -- so the base point is derived rather than written out, and
    // the derivation is the same decompression every public key goes through.
    let mut bytes = BASE_Y;
    bytes[31] &= 0x7F;
    decompress(&bytes).expect("the base point is on the curve")
}

impl Point {
    /// Add two points.
    ///
    /// The unified formula: it is correct when the two points are the same, so
    /// there is no doubling special case and no branch.
    ///
    /// Named rather than an operator for the same reason the field's is: the
    /// group law is not addition of numbers, and a `+` between two points
    /// invites the reader to think about carrying.
    #[allow(clippy::should_implement_trait)]
    #[must_use]
    pub fn add(self, other: Self) -> Self {
        let a = self.y.subtract(self.x).multiply(other.y.subtract(other.x));
        let b = self.y.add(self.x).multiply(other.y.add(other.x));
        let c = self
            .t
            .multiply(other.t)
            .multiply(d())
            .multiply(Fe([2, 0, 0, 0]));
        let e = self.z.multiply(other.z).multiply(Fe([2, 0, 0, 0]));

        let f = b.subtract(a);
        let g = e.subtract(c);
        let h = e.add(c);
        let i = b.add(a);

        Self {
            x: f.multiply(g),
            y: h.multiply(i),
            t: f.multiply(i),
            z: g.multiply(h),
        }
    }

    /// Negate: the same point with `x` the other way round.
    #[must_use]
    pub fn negate(self) -> Self {
        Self {
            x: self.x.negate(),
            y: self.y,
            z: self.z,
            t: self.t.negate(),
        }
    }

    /// Multiply by a scalar given as thirty-two little-endian bytes.
    ///
    /// Double-and-add, from the top bit down, with the addition performed on
    /// every bit and its result discarded when the bit is clear. Doing the work
    /// either way is what keeps how long this takes from depending on the
    /// scalar -- and the scalar, when signing, is the private key.
    #[must_use]
    pub fn multiply(self, scalar: &[u8; 32]) -> Self {
        let mut result = IDENTITY;
        for index in (0..32).rev() {
            for bit in (0..8).rev() {
                result = result.add(result);
                let sum = result.add(self);
                let chosen = (scalar[index] >> bit) & 1 == 1;
                result = Self {
                    x: result.x.select(sum.x, chosen),
                    y: result.y.select(sum.y, chosen),
                    z: result.z.select(sum.z, chosen),
                    t: result.t.select(sum.t, chosen),
                };
            }
        }
        result
    }

    /// Whether two points are the same, which in projective coordinates is not
    /// whether their coordinates are.
    ///
    /// `(X₁ : Y₁ : Z₁)` and `(X₂ : Y₂ : Z₂)` are the same point exactly when
    /// `X₁·Z₂ = X₂·Z₁` and `Y₁·Z₂ = Y₂·Z₁`. Comparing coordinates directly
    /// would call two spellings of one point different.
    #[must_use]
    pub fn equals(self, other: Self) -> bool {
        self.x.multiply(other.z).equals(other.x.multiply(self.z))
            && self.y.multiply(other.z).equals(other.y.multiply(self.z))
    }

    /// Thirty-two bytes: `y`, with the low bit of `x` on top.
    #[must_use]
    pub fn compress(self) -> [u8; 32] {
        let inverse = self.z.invert();
        let x = self.x.multiply(inverse);
        let y = self.y.multiply(inverse);
        let mut bytes = y.to_bytes();
        if x.is_odd() {
            bytes[31] |= 0x80;
        }
        bytes
    }
}

/// Read a point from thirty-two bytes, or decide it is not one.
///
/// # Errors
///
/// Returns `None` when `y` gives no `x` on the curve, which most byte strings
/// do not. This is the check that stops a forged public key or a forged `R`
/// from being treated as a point.
#[must_use]
pub fn decompress(bytes: &[u8; 32]) -> Option<Point> {
    let y = Fe::from_bytes(bytes);
    let want_odd = bytes[31] & 0x80 != 0;

    // x² = (y² - 1) / (d·y² + 1)
    let y_squared = y.square();
    let numerator = y_squared.subtract(field::ONE);
    let denominator = d().multiply(y_squared).add(field::ONE);

    // The square root is computed as u·v³·(u·v⁷)^((p-5)/8), which is the
    // standard way of doing it in one exponentiation rather than an inversion
    // and a separate root.
    let v3 = denominator.square().multiply(denominator);
    let v7 = v3.square().multiply(denominator);
    let mut x = numerator
        .multiply(v3)
        .multiply(numerator.multiply(v7).power_p_minus_5_over_8());

    // The candidate is right, wrong by a factor of sqrt(-1), or not a root at
    // all. Both possibilities are tried before giving up.
    let check = x.square().multiply(denominator);
    if !check.equals(numerator) {
        if check.equals(numerator.negate()) {
            x = x.multiply(sqrt_minus_one());
        } else {
            return None;
        }
    }
    if !x.square().multiply(denominator).equals(numerator) {
        return None;
    }

    // x = 0 has only one square root, so a point that asks for the odd one is
    // asking for something that does not exist.
    if x.is_zero() && want_odd {
        return None;
    }
    if x.is_odd() != want_odd {
        x = x.negate();
    }

    Some(Point {
        x,
        y,
        z: field::ONE,
        t: x.multiply(y),
    })
}

/// The public key belonging to a private one.
#[must_use]
pub fn public_key(secret: &[u8; SECRET_KEY]) -> [u8; PUBLIC_KEY] {
    let (scalar, _) = expand(secret);
    base().multiply(&scalar).compress()
}

/// Sign a message.
#[must_use]
pub fn sign(secret: &[u8; SECRET_KEY], message: &[u8]) -> [u8; SIGNATURE] {
    let (a, prefix) = expand(secret);
    let public = base().multiply(&a).compress();

    // r is derived from the private key and the message rather than from
    // randomness. That is the whole design: a signature scheme that needed a
    // random number would be a scheme that leaks the private key the first time
    // the number repeats, and this machine has no randomness it would trust.
    let mut hasher = sha512::Hasher::new();
    hasher.update(&prefix);
    hasher.update(message);
    let r = scalar::reduce(&hasher.finish());

    let big_r = base().multiply(&r).compress();

    let mut hasher = sha512::Hasher::new();
    hasher.update(&big_r);
    hasher.update(&public);
    hasher.update(message);
    let k = scalar::reduce(&hasher.finish());

    let s = scalar::multiply_add(&k, &a, &r);

    let mut signature = [0u8; SIGNATURE];
    signature[..32].copy_from_slice(&big_r);
    signature[32..].copy_from_slice(&s);
    signature
}

/// Check a signature.
///
/// Returns false for anything wrong: a bad signature, a public key that is not
/// a point, an `R` that is not a point, or an `S` that is not a canonical
/// scalar.
#[must_use]
pub fn verify(public: &[u8; PUBLIC_KEY], message: &[u8], signature: &[u8; SIGNATURE]) -> bool {
    let mut big_r = [0u8; 32];
    let mut s = [0u8; 32];
    big_r.copy_from_slice(&signature[..32]);
    s.copy_from_slice(&signature[32..]);

    // S must be below the group order. Without this check a signature can be
    // rewritten into a different one that also verifies, which is fine for
    // authenticity and wrong anywhere something is identified by the bytes of
    // its signature -- a package, for instance.
    if !scalar::is_canonical(&s) {
        return false;
    }

    let Some(a) = decompress(public) else {
        return false;
    };
    let Some(r_point) = decompress(&big_r) else {
        return false;
    };

    let mut hasher = sha512::Hasher::new();
    hasher.update(&big_r);
    hasher.update(public);
    hasher.update(message);
    let k = scalar::reduce(&hasher.finish());

    // S·B = R + k·A, checked as a point equality rather than by comparing
    // compressed forms, so that two spellings of one point are not called
    // different.
    let left = base().multiply(&s);
    let right = r_point.add(a.multiply(&k));
    left.equals(right)
}

/// Turn a private key into the scalar and the prefix that signing uses.
///
/// The scalar is *clamped*: the low three bits cleared so that it is a multiple
/// of the cofactor, and the top two bits set so that it is a fixed length. Both
/// are there to make every private key behave the same way regardless of what
/// its bits happen to be.
fn expand(secret: &[u8; SECRET_KEY]) -> ([u8; 32], [u8; 32]) {
    let hashed = sha512::digest(secret);
    let mut scalar = [0u8; 32];
    let mut prefix = [0u8; 32];
    scalar.copy_from_slice(&hashed[..32]);
    prefix.copy_from_slice(&hashed[32..]);

    scalar[0] &= 248;
    scalar[31] &= 127;
    scalar[31] |= 64;
    (scalar, prefix)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Read a hex string into bytes.
    fn bytes<const N: usize>(text: &str) -> [u8; N] {
        let mut out = [0u8; N];
        for (index, slot) in out.iter_mut().enumerate() {
            let pair = &text[index * 2..index * 2 + 2];
            *slot = u8::from_str_radix(pair, 16).expect("hex");
        }
        out
    }

    #[test]
    fn the_base_point_has_the_order_it_should() {
        // L·B is the identity. This is the single strongest check that the
        // curve constants, the point arithmetic and the scalar arithmetic all
        // agree: get any one of them wrong and this is not the identity.
        let order = scalar::order();
        assert!(base().multiply(&order).equals(IDENTITY));
    }

    #[test]
    fn a_point_survives_compression() {
        let compressed = base().compress();
        let recovered = decompress(&compressed).expect("the base point decompresses");
        assert!(recovered.equals(base()));
        assert_eq!(recovered.compress(), compressed);
    }

    #[test]
    fn most_byte_strings_are_not_points() {
        // Roughly half of all `y` values have no square root, so a handful of
        // arbitrary strings should include some that are refused. If every one
        // of them decompressed, the check would not be checking anything.
        let mut refused = 0;
        for seed in 0u8..40 {
            let mut candidate = [seed; 32];
            candidate[31] &= 0x7F;
            if decompress(&candidate).is_none() {
                refused += 1;
            }
        }
        assert!(refused > 0, "nothing was refused, so nothing is checked");
    }

    #[test]
    fn adding_the_identity_changes_nothing() {
        assert!(base().add(IDENTITY).equals(base()));
    }

    #[test]
    fn a_point_plus_its_negation_is_the_identity() {
        assert!(base().add(base().negate()).equals(IDENTITY));
    }

    #[test]
    fn doubling_and_adding_agree() {
        let doubled = base().add(base());
        let two = {
            let mut bytes = [0u8; 32];
            bytes[0] = 2;
            bytes
        };
        assert!(base().multiply(&two).equals(doubled));
    }

    // The vectors from RFC 8032, section 7.1. Same keys, same messages, same
    // signatures -- which is the only claim worth making about an
    // implementation of somebody else's algorithm.

    #[test]
    fn rfc_8032_test_1() {
        let secret: [u8; 32] =
            bytes("9d61b19deffd5a60ba844af492ec2cc44449c5697b326919703bac031cae7f60");
        let public: [u8; 32] =
            bytes("d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a");
        assert_eq!(public_key(&secret), public);

        let signature: [u8; 64] = bytes(
            "e5564300c360ac729086e2cc806e828a84877f1eb8e5d974d873e065224901555fb8821590a33bacc61e39701cf9b46bd25bf5f0595bbe24655141438e7a100b",
        );
        assert_eq!(sign(&secret, b""), signature);
        assert!(verify(&public, b"", &signature));
    }

    #[test]
    fn rfc_8032_test_2() {
        let secret: [u8; 32] =
            bytes("4ccd089b28ff96da9db6c346ec114e0f5b8a319f35aba624da8cf6ed4fb8a6fb");
        let public: [u8; 32] =
            bytes("3d4017c3e843895a92b70aa74d1b7ebc9c982ccf2ec4968cc0cd55f12af4660c");
        assert_eq!(public_key(&secret), public);

        let message = [0x72u8];
        let signature: [u8; 64] = bytes(
            "92a009a9f0d4cab8720e820b5f642540a2b27b5416503f8fb3762223ebdb69da085ac1e43e15996e458f3613d0f11d8c387b2eaeb4302aeeb00d291612bb0c00",
        );
        assert_eq!(sign(&secret, &message), signature);
        assert!(verify(&public, &message, &signature));
    }

    #[test]
    fn rfc_8032_test_3() {
        let secret: [u8; 32] =
            bytes("c5aa8df43f9f837bedb7442f31dcb7b166d38535076f094b85ce3a2e0b4458f7");
        let public: [u8; 32] =
            bytes("fc51cd8e6218a1a38da47ed00230f0580816ed13ba3303ac5deb911548908025");
        let message = [0xafu8, 0x82];
        let signature: [u8; 64] = bytes(
            "6291d657deec24024827e69c3abe01a30ce548a284743a445e3680d7db5ac3ac18ff9b538d16f290ae67f760984dc6594a7c15e9716ed28dc027beceea1ec40a",
        );
        assert_eq!(public_key(&secret), public);
        assert_eq!(sign(&secret, &message), signature);
        assert!(verify(&public, &message, &signature));
    }

    #[test]
    fn a_changed_message_does_not_verify() {
        let secret: [u8; 32] =
            bytes("4ccd089b28ff96da9db6c346ec114e0f5b8a319f35aba624da8cf6ed4fb8a6fb");
        let public = public_key(&secret);
        let signature = sign(&secret, b"install this package");
        assert!(verify(&public, b"install this package", &signature));
        assert!(!verify(&public, b"install this package!", &signature));
        assert!(!verify(&public, b"install that package", &signature));
    }

    #[test]
    fn a_changed_signature_does_not_verify() {
        let secret: [u8; 32] =
            bytes("4ccd089b28ff96da9db6c346ec114e0f5b8a319f35aba624da8cf6ed4fb8a6fb");
        let public = public_key(&secret);
        let message = b"a package";
        let good = sign(&secret, message);
        // Every single bit, one at a time. A verifier that ignored any part of
        // the signature would let one of these through.
        for index in 0..SIGNATURE {
            for bit in 0..8 {
                let mut bad = good;
                bad[index] ^= 1 << bit;
                assert!(!verify(&public, message, &bad), "byte {index} bit {bit}");
            }
        }
    }

    #[test]
    fn another_key_does_not_verify() {
        let mine: [u8; 32] =
            bytes("4ccd089b28ff96da9db6c346ec114e0f5b8a319f35aba624da8cf6ed4fb8a6fb");
        let theirs: [u8; 32] =
            bytes("9d61b19deffd5a60ba844af492ec2cc44449c5697b326919703bac031cae7f60");
        let message = b"a package";
        let signature = sign(&mine, message);
        assert!(!verify(&public_key(&theirs), message, &signature));
    }

    #[test]
    fn a_non_canonical_s_is_refused() {
        let secret: [u8; 32] =
            bytes("4ccd089b28ff96da9db6c346ec114e0f5b8a319f35aba624da8cf6ed4fb8a6fb");
        let public = public_key(&secret);
        let message = b"a package";
        let mut signature = sign(&secret, message);
        // S + L verifies under the curve equation, because L is zero there. It
        // is a *different* signature on the same message, and refusing it is
        // the difference between a signature and an identifier.
        let order = scalar::order();
        let mut carry = 0u16;
        for index in 0..32 {
            let sum = u16::from(signature[32 + index]) + u16::from(order[index]) + carry;
            signature[32 + index] = sum as u8;
            carry = sum >> 8;
        }
        assert!(!verify(&public, message, &signature));
    }
}
