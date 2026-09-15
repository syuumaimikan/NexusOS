//! ECDSA over NIST P-256, verifying only.
//!
//! The other half of what a certificate chain needs. Most leaf certificates
//! issued today carry a P-256 key even when the authority above them signs with
//! RSA, so a client with only RSA verifies most of a chain and then fails at
//! the bottom.
//!
//! # Verifying only, again
//!
//! As in [`crate::rsa`]: every number here is public. The public key is in the
//! certificate, the signature was sent in the clear, and the message is the
//! handshake transcript. There is no secret for a timing side channel to leak,
//! so the ladder branches on bits freely and the code is the clearer for it.
//!
//! A file that did **signing** would need every one of those branches removed,
//! and should be a different file with that said at the top.
//!
//! # Fixed at four limbs
//!
//! P-256 is 256 bits, so every value here is `[u64; 4]` and nothing allocates.
//! That is the main reason this does not reuse [`crate::rsa`]'s arithmetic,
//! which is sized at run time for a modulus that can be any length.
//!
//! # Jacobian coordinates
//!
//! Affine point addition needs a modular inversion each time, and an inversion
//! is an exponentiation — about 256 squarings. A double-and-add over a 256-bit
//! scalar would do five hundred of them. Jacobian coordinates carry a
//! denominator along instead, so there is exactly one inversion at the very
//! end.

use nexus_crypto::sha256;

/// A field element or a scalar: 256 bits, little-endian limbs.
type Number = [u64; 4];

/// The field's prime: 2²⁵⁶ − 2²²⁴ + 2¹⁹² + 2⁹⁶ − 1.
const P: Number = [
    0xFFFF_FFFF_FFFF_FFFF,
    0x0000_0000_FFFF_FFFF,
    0x0000_0000_0000_0000,
    0xFFFF_FFFF_0000_0001,
];

/// The order of the base point.
const N: Number = [
    0xF3B9_CAC2_FC63_2551,
    0xBCE6_FAAD_A717_9E84,
    0xFFFF_FFFF_FFFF_FFFF,
    0xFFFF_FFFF_0000_0000,
];

/// The curve's `b`. `a` is −3 and is handled by the doubling formula directly.
const B: Number = [
    0x3BCE_3C3E_27D2_604B,
    0x651D_06B0_CC53_B0F6,
    0xB3EB_BD55_7698_86BC,
    0x5AC6_35D8_AA3A_93E7,
];

/// The base point's x, and its y.
const GX: Number = [
    0xF4A1_3945_D898_C296,
    0x7703_7D81_2DEB_33A0,
    0xF8BC_E6E5_63A4_40F2,
    0x6B17_D1F2_E12C_4247,
];
const GY: Number = [
    0xCBB6_4068_37BF_51F5,
    0x2BCE_3357_6B31_5ECE,
    0x8EE7_EB4A_7C0F_9E16,
    0x4FE3_42E2_FE1A_7F9B,
];

/// Why a signature did not verify.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trouble {
    /// The point in the certificate is not on the curve, or is not a point.
    BadKey,
    /// The signature is not two numbers of the right size, or one is out of
    /// range.
    BadSignature,
    /// It verified against something else.
    Mismatch,
}

impl core::fmt::Display for Trouble {
    fn fmt(&self, out: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::BadKey => out.write_str("the P-256 public key is not a point on the curve"),
            Self::BadSignature => out.write_str("the ECDSA signature is malformed"),
            Self::Mismatch => out.write_str("the signature is for a different message"),
        }
    }
}

// ---------------------------------------------------------------------------
// Arithmetic modulo a 256-bit prime
// ---------------------------------------------------------------------------

fn is_zero(value: &Number) -> bool {
    value.iter().all(|limb| *limb == 0)
}

/// Whether `one` is at least `other`.
fn at_least(one: &Number, other: &Number) -> bool {
    for index in (0..4).rev() {
        match one[index].cmp(&other[index]) {
            core::cmp::Ordering::Greater => return true,
            core::cmp::Ordering::Less => return false,
            core::cmp::Ordering::Equal => {}
        }
    }
    true
}

/// `one + other mod modulus`.
fn add_mod(one: &Number, other: &Number, modulus: &Number) -> Number {
    let mut out = [0u64; 4];
    let mut carry = 0u128;
    for index in 0..4 {
        let sum = u128::from(one[index]) + u128::from(other[index]) + carry;
        out[index] = sum as u64;
        carry = sum >> 64;
    }
    if carry != 0 || at_least(&out, modulus) {
        out = subtract(&out, modulus);
    }
    out
}

/// `one - other`, wrapping, which the caller has checked does not go negative.
fn subtract(one: &Number, other: &Number) -> Number {
    let mut out = [0u64; 4];
    let mut borrow = 0u64;
    for index in 0..4 {
        let (first, one_borrowed) = one[index].overflowing_sub(other[index]);
        let (second, two_borrowed) = first.overflowing_sub(borrow);
        out[index] = second;
        borrow = u64::from(one_borrowed) + u64::from(two_borrowed);
    }
    out
}

/// `one - other mod modulus`.
fn subtract_mod(one: &Number, other: &Number, modulus: &Number) -> Number {
    if at_least(one, other) {
        subtract(one, other)
    } else {
        // Add the modulus first so the subtraction stays positive.
        let mut sum = [0u64; 4];
        let mut carry = 0u128;
        for index in 0..4 {
            let value = u128::from(one[index]) + u128::from(modulus[index]) + carry;
            sum[index] = value as u64;
            carry = value >> 64;
        }
        subtract(&sum, other)
    }
}

/// `2 × value mod modulus`.
fn double_mod(value: &Number, modulus: &Number) -> Number {
    add_mod(value, value, modulus)
}

/// `-modulus⁻¹ mod 2⁶⁴`, by Newton's method. Both P and N are odd.
const fn inverse_of_low_limb(low: u64) -> u64 {
    let mut inverse = 1u64;
    let mut step = 0;
    while step < 6 {
        inverse = inverse.wrapping_mul(2u64.wrapping_sub(low.wrapping_mul(inverse)));
        step += 1;
    }
    inverse.wrapping_neg()
}

const P0: u64 = inverse_of_low_limb(P[0]);
const N0: u64 = inverse_of_low_limb(N[0]);

/// Montgomery multiplication: `one × other × R⁻¹ mod modulus`.
fn montgomery(one: &Number, other: &Number, modulus: &Number, n0: u64) -> Number {
    let mut t = [0u64; 6];
    for &b in other.iter() {
        let mut carry = 0u128;
        for index in 0..4 {
            let sum = u128::from(t[index]) + u128::from(one[index]) * u128::from(b) + carry;
            t[index] = sum as u64;
            carry = sum >> 64;
        }
        let sum = u128::from(t[4]) + carry;
        t[4] = sum as u64;
        t[5] = (sum >> 64) as u64;

        let m = t[0].wrapping_mul(n0);
        let mut carry = 0u128;
        for index in 0..4 {
            let sum = u128::from(t[index]) + u128::from(m) * u128::from(modulus[index]) + carry;
            if index > 0 {
                t[index - 1] = sum as u64;
            }
            carry = sum >> 64;
        }
        let sum = u128::from(t[4]) + carry;
        t[3] = sum as u64;
        t[4] = t[5] + (sum >> 64) as u64;
    }

    let mut out = [t[0], t[1], t[2], t[3]];
    if t[4] != 0 || at_least(&out, modulus) {
        out = subtract(&out, modulus);
    }
    out
}

/// `R² mod modulus`, by doubling from one.
///
/// The bootstrap that lets everything else avoid division: each step is a shift
/// and a comparison.
fn r_squared(modulus: &Number) -> Number {
    let mut value = [1u64, 0, 0, 0];
    for _ in 0..(2 * 256) {
        value = double_mod(&value, modulus);
    }
    value
}

/// A field, with the constants Montgomery needs precomputed.
struct Field {
    modulus: Number,
    n0: u64,
    r2: Number,
    /// One, in Montgomery form.
    one: Number,
}

impl Field {
    fn new(modulus: Number, n0: u64) -> Self {
        let r2 = r_squared(&modulus);
        let mut field = Self {
            modulus,
            n0,
            r2,
            one: [0; 4],
        };
        field.one = field.enter(&[1, 0, 0, 0]);
        field
    }

    /// Into Montgomery form.
    fn enter(&self, value: &Number) -> Number {
        montgomery(value, &self.r2, &self.modulus, self.n0)
    }

    /// Out of it.
    fn leave(&self, value: &Number) -> Number {
        montgomery(value, &[1, 0, 0, 0], &self.modulus, self.n0)
    }

    fn multiply(&self, one: &Number, other: &Number) -> Number {
        montgomery(one, other, &self.modulus, self.n0)
    }

    fn square(&self, value: &Number) -> Number {
        self.multiply(value, value)
    }

    fn add(&self, one: &Number, other: &Number) -> Number {
        add_mod(one, other, &self.modulus)
    }

    fn subtract(&self, one: &Number, other: &Number) -> Number {
        subtract_mod(one, other, &self.modulus)
    }

    fn double(&self, value: &Number) -> Number {
        double_mod(value, &self.modulus)
    }

    /// `value^exponent`, with the exponent public.
    fn power(&self, value: &Number, exponent: &Number) -> Number {
        let mut result = self.one;
        for limb in exponent.iter().rev() {
            for bit in (0..64).rev() {
                result = self.square(&result);
                if (limb >> bit) & 1 == 1 {
                    result = self.multiply(&result, value);
                }
            }
        }
        result
    }

    /// `value⁻¹`, by Fermat: `value^(modulus − 2)`.
    ///
    /// Both moduli here are prime, which is what makes this correct. It is the
    /// slowest operation in the file and is done twice per verification.
    fn invert(&self, value: &Number) -> Number {
        let mut exponent = self.modulus;
        // modulus − 2, and neither modulus ends in 0 or 1 so no borrow leaves
        // the bottom limb.
        exponent[0] = exponent[0].wrapping_sub(2);
        self.power(value, &exponent)
    }
}

// ---------------------------------------------------------------------------
// The curve
// ---------------------------------------------------------------------------

/// A point in Jacobian coordinates: the affine point is `(x/z², y/z³)`.
///
/// `z == 0` is the point at infinity, which is the identity.
#[derive(Clone, Copy)]
struct Point {
    x: Number,
    y: Number,
    z: Number,
}

impl Point {
    fn infinity() -> Self {
        Self {
            x: [1, 0, 0, 0],
            y: [1, 0, 0, 0],
            z: [0; 4],
        }
    }

    fn is_infinity(&self) -> bool {
        is_zero(&self.z)
    }
}

/// Double a point. The standard formula for curves with `a = −3`.
fn double(field: &Field, point: &Point) -> Point {
    if point.is_infinity() {
        return *point;
    }
    let zz = field.square(&point.z);
    // delta = 3(x − z²)(x + z²), which is where a = −3 is used.
    let a = field.subtract(&point.x, &zz);
    let b = field.add(&point.x, &zz);
    let mut delta = field.multiply(&a, &b);
    delta = field.add(&field.double(&delta), &delta);

    let yy = field.square(&point.y);
    let s = field.double(&field.double(&field.multiply(&point.x, &yy)));

    let x = field.subtract(&field.square(&delta), &field.double(&s));
    let mut y = field.multiply(&delta, &field.subtract(&s, &x));
    let yyyy = field.square(&yy);
    y = field.subtract(&y, &field.double(&field.double(&field.double(&yyyy))));
    let z = field.double(&field.multiply(&point.y, &point.z));

    if is_zero(&z) {
        return Point::infinity();
    }
    Point { x, y, z }
}

/// Add two points, where `other` is affine (its `z` is one).
///
/// Mixed addition, because one operand is always a fixed point or a key.
fn add_mixed(field: &Field, point: &Point, x2: &Number, y2: &Number) -> Point {
    if point.is_infinity() {
        return Point {
            x: *x2,
            y: *y2,
            z: field.one,
        };
    }
    let zz = field.square(&point.z);
    let u2 = field.multiply(x2, &zz);
    let s2 = field.multiply(y2, &field.multiply(&zz, &point.z));

    if point.x == u2 {
        if point.y == s2 {
            return double(field, point);
        }
        // P + (−P) is the identity.
        return Point::infinity();
    }

    let h = field.subtract(&u2, &point.x);
    let r = field.subtract(&s2, &point.y);
    let hh = field.square(&h);
    let hhh = field.multiply(&hh, &h);
    let v = field.multiply(&point.x, &hh);

    let x = field.subtract(&field.subtract(&field.square(&r), &hhh), &field.double(&v));
    let y = field.subtract(
        &field.multiply(&r, &field.subtract(&v, &x)),
        &field.multiply(&point.y, &hhh),
    );
    let z = field.multiply(&point.z, &h);

    Point { x, y, z }
}

/// The affine x of a Jacobian point, or `None` at infinity.
fn affine_x(field: &Field, point: &Point) -> Option<Number> {
    if point.is_infinity() {
        return None;
    }
    let inverse = field.invert(&point.z);
    let squared = field.square(&inverse);
    Some(field.leave(&field.multiply(&point.x, &squared)))
}

/// Whether `(x, y)`, in Montgomery form, satisfies `y² = x³ − 3x + b`.
fn on_curve(field: &Field, x: &Number, y: &Number) -> bool {
    let left = field.square(y);
    let x3 = field.multiply(&field.square(x), x);
    let three_x = field.add(&field.double(x), x);
    let b = field.enter(&B);
    let right = field.add(&field.subtract(&x3, &three_x), &b);
    left == right
}

/// `first × G + second × key`, which is what verification needs.
///
/// Two separate ladders rather than an interleaved one: this runs once per
/// connection and the simpler version is the one that can be checked by eye.
fn two_scalars(
    field: &Field,
    first: &Number,
    second: &Number,
    key_x: &Number,
    key_y: &Number,
) -> Point {
    let gx = field.enter(&GX);
    let gy = field.enter(&GY);

    let mut result = Point::infinity();
    for (scalar, (x, y)) in [(first, (&gx, &gy)), (second, (key_x, key_y))] {
        let mut term = Point::infinity();
        for limb in scalar.iter().rev() {
            for bit in (0..64).rev() {
                term = double(field, &term);
                if (limb >> bit) & 1 == 1 {
                    term = add_mixed(field, &term, x, y);
                }
            }
        }
        // Adding a Jacobian point to a Jacobian point, done by bringing the
        // second to affine first. It happens once, so the inversion is cheap
        // beside the ladder that produced it.
        if !term.is_infinity() {
            let inverse = field.invert(&term.z);
            let squared = field.square(&inverse);
            let cubed = field.multiply(&squared, &inverse);
            let ax = field.multiply(&term.x, &squared);
            let ay = field.multiply(&term.y, &cubed);
            result = add_mixed(field, &result, &ax, &ay);
        }
    }
    result
}

// ---------------------------------------------------------------------------
// ECDSA
// ---------------------------------------------------------------------------

/// Big-endian bytes into limbs.
fn from_bytes(bytes: &[u8]) -> Option<Number> {
    if bytes.len() > 32 {
        return None;
    }
    let mut number = [0u64; 4];
    for (index, byte) in bytes.iter().rev().enumerate() {
        number[index / 8] |= u64::from(*byte) << ((index % 8) * 8);
    }
    Some(number)
}

/// Verify an ECDSA signature.
///
/// `point` is the uncompressed public key from the certificate: `0x04` then x
/// and y. `signature` is the DER `SEQUENCE { INTEGER r, INTEGER s }` that TLS
/// and X.509 both carry.
pub fn verify(point: &[u8], signature: &[u8], message: &[u8], sha384: bool) -> Result<(), Trouble> {
    use crate::der::{self, tag, Reader};

    if point.len() != 65 || point[0] != 0x04 {
        return Err(Trouble::BadKey);
    }
    let key_x = from_bytes(&point[1..33]).ok_or(Trouble::BadKey)?;
    let key_y = from_bytes(&point[33..65]).ok_or(Trouble::BadKey)?;
    // Both coordinates must be inside the field, which a point read off the
    // wire need not be.
    if at_least(&key_x, &P) || at_least(&key_y, &P) {
        return Err(Trouble::BadKey);
    }

    let field = Field::new(P, P0);
    let key_x_form = field.enter(&key_x);
    let key_y_form = field.enter(&key_y);
    // A key that is not on the curve is one where the group law does not hold,
    // and accepting it is the classic invalid-curve attack.
    if !on_curve(&field, &key_x_form, &key_y_form) {
        return Err(Trouble::BadKey);
    }

    let mut reader = Reader::new(signature);
    let mut sequence = reader
        .nested(tag::SEQUENCE)
        .map_err(|_| Trouble::BadSignature)?;
    let r_bytes = der::positive_integer(
        &sequence
            .expect(tag::INTEGER)
            .map_err(|_| Trouble::BadSignature)?,
    )
    .map_err(|_| Trouble::BadSignature)?;
    let s_bytes = der::positive_integer(
        &sequence
            .expect(tag::INTEGER)
            .map_err(|_| Trouble::BadSignature)?,
    )
    .map_err(|_| Trouble::BadSignature)?;
    if !sequence.done() {
        return Err(Trouble::BadSignature);
    }

    let r = from_bytes(r_bytes).ok_or(Trouble::BadSignature)?;
    let s = from_bytes(s_bytes).ok_or(Trouble::BadSignature)?;
    // Both must be in [1, n − 1]. Zero would make the arithmetic degenerate and
    // a value at or above the order is a second encoding of a smaller one --
    // which is signature malleability, and is refused.
    if is_zero(&r) || is_zero(&s) || at_least(&r, &N) || at_least(&s, &N) {
        return Err(Trouble::BadSignature);
    }

    // The message hash, truncated to the order's bit length. Both hashes here
    // are at least 256 bits, so this takes the leftmost 256.
    let digest: [u8; 32] = if sha384 {
        let long = nexus_crypto::sha512::digest_384(message);
        let mut short = [0u8; 32];
        short.copy_from_slice(&long[..32]);
        short
    } else {
        sha256::digest(message)
    };
    let mut e = from_bytes(&digest).ok_or(Trouble::BadSignature)?;
    // Reduced modulo the order, which RFC 6979 §2.3.2 requires.
    if at_least(&e, &N) {
        e = subtract(&e, &N);
    }

    // u1 = e/s, u2 = r/s, both modulo the order.
    let scalars = Field::new(N, N0);
    let s_inverse = scalars.leave(&scalars.invert(&scalars.enter(&s)));
    let u1 = scalars.leave(&scalars.multiply(&scalars.enter(&e), &scalars.enter(&s_inverse)));
    let u2 = scalars.leave(&scalars.multiply(&scalars.enter(&r), &scalars.enter(&s_inverse)));

    let sum = two_scalars(&field, &u1, &u2, &key_x_form, &key_y_form);
    let Some(x) = affine_x(&field, &sum) else {
        return Err(Trouble::Mismatch);
    };

    // The signature verifies when x mod n equals r.
    let mut reduced = x;
    if at_least(&reduced, &N) {
        reduced = subtract(&reduced, &N);
    }
    if reduced == r {
        Ok(())
    } else {
        Err(Trouble::Mismatch)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_base_point_is_on_the_curve() {
        // The first thing to check: if this fails, every formula below is
        // being tested against the wrong curve.
        let field = Field::new(P, P0);
        let x = field.enter(&GX);
        let y = field.enter(&GY);
        assert!(on_curve(&field, &x, &y));
    }

    #[test]
    fn a_point_not_on_the_curve_is_seen_to_be_off_it() {
        let field = Field::new(P, P0);
        let x = field.enter(&GX);
        let mut wrong = GY;
        wrong[0] ^= 1;
        assert!(!on_curve(&field, &x, &field.enter(&wrong)));
    }

    #[test]
    fn montgomery_form_round_trips() {
        let field = Field::new(P, P0);
        for value in [[1u64, 0, 0, 0], GX, GY, B] {
            assert_eq!(field.leave(&field.enter(&value)), value);
        }
    }

    #[test]
    fn multiplication_agrees_with_small_arithmetic() {
        let field = Field::new(P, P0);
        let three = field.enter(&[3, 0, 0, 0]);
        let five = field.enter(&[5, 0, 0, 0]);
        assert_eq!(field.leave(&field.multiply(&three, &five)), [15, 0, 0, 0]);
        assert_eq!(field.leave(&field.square(&five)), [25, 0, 0, 0]);
    }

    #[test]
    fn inverses_multiply_to_one() {
        for (modulus, n0) in [(P, P0), (N, N0)] {
            let field = Field::new(modulus, n0);
            for value in [[2u64, 0, 0, 0], [7, 0, 0, 0], GX] {
                let entered = field.enter(&value);
                let inverse = field.invert(&entered);
                assert_eq!(
                    field.leave(&field.multiply(&entered, &inverse)),
                    [1, 0, 0, 0],
                    "the inverse of {value:?} is wrong"
                );
            }
        }
    }

    #[test]
    fn doubling_the_base_point_lands_where_it_should() {
        // 2G's x, which is published in every reference for this curve. If the
        // doubling formula is wrong, this is where it shows.
        let field = Field::new(P, P0);
        let g = Point {
            x: field.enter(&GX),
            y: field.enter(&GY),
            z: field.one,
        };
        let doubled = double(&field, &g);
        let x = affine_x(&field, &doubled).unwrap();
        assert_eq!(
            x,
            [
                0xA60B_48FC_4766_9978,
                0xC08969E277F21B35,
                0x8A52380304B51AC3,
                0x7CF27B188D034F7E,
            ],
            "2G is not where it should be"
        );
    }

    #[test]
    fn adding_a_point_to_itself_is_doubling_it() {
        // The mixed-addition path has a special case for it, and a formula that
        // fell through to the general case would divide by zero.
        let field = Field::new(P, P0);
        let gx = field.enter(&GX);
        let gy = field.enter(&GY);
        let g = Point {
            x: gx,
            y: gy,
            z: field.one,
        };
        let added = add_mixed(&field, &g, &gx, &gy);
        let doubled = double(&field, &g);
        assert_eq!(affine_x(&field, &added), affine_x(&field, &doubled));
    }

    #[test]
    fn a_point_plus_its_negation_is_the_identity() {
        let field = Field::new(P, P0);
        let gx = field.enter(&GX);
        let gy = field.enter(&GY);
        let g = Point {
            x: gx,
            y: gy,
            z: field.one,
        };
        let negated = field.subtract(&[0; 4], &gy);
        let sum = add_mixed(&field, &g, &gx, &negated);
        assert!(sum.is_infinity());
        assert_eq!(affine_x(&field, &sum), None);
    }

    #[test]
    fn a_real_signature_verifies_and_a_changed_message_does_not() {
        // OpenSSL made the key and the signature. Everything above is this
        // project's arithmetic checked against published constants; this is the
        // whole of ECDSA checked against somebody else's implementation.
        let point = key_point();
        let message = include_bytes!("../fixtures/signed.txt");
        let signature = include_bytes!("../fixtures/signed.ecdsa");

        verify(&point, signature, message, false).expect("a real signature should verify");

        let mut changed = message.to_vec();
        changed[0] ^= 1;
        assert_eq!(
            verify(&point, signature, &changed, false).unwrap_err(),
            Trouble::Mismatch
        );
    }

    #[test]
    fn a_key_that_is_not_on_the_curve_is_refused() {
        // The invalid-curve attack: a peer sends a point on a *different* curve
        // where the discrete log is easy, and an implementation that does not
        // check the equation does arithmetic that leaks.
        let mut point = key_point();
        point[40] ^= 0x01;
        let message = include_bytes!("../fixtures/signed.txt");
        let signature = include_bytes!("../fixtures/signed.ecdsa");
        assert_eq!(
            verify(&point, signature, message, false).unwrap_err(),
            Trouble::BadKey
        );
    }

    #[test]
    fn a_signature_with_r_or_s_out_of_range_is_refused() {
        use crate::der::{tag, write};
        let point = key_point();
        let message = include_bytes!("../fixtures/signed.txt");

        // Zero is degenerate; the order itself is a second encoding of zero,
        // which is malleability.
        for bad in [alloc::vec![0u8], {
            let mut bytes = alloc::vec![0u8];
            bytes.extend_from_slice(&to_big_endian(&N));
            bytes
        }] {
            let mut body = write(tag::INTEGER, &bad);
            body.extend_from_slice(&write(tag::INTEGER, &[1]));
            let signature = write(tag::SEQUENCE, &body);
            assert_eq!(
                verify(&point, &signature, message, false).unwrap_err(),
                Trouble::BadSignature,
                "r = {bad:?} should be refused"
            );
        }
    }

    #[test]
    fn a_signature_that_is_not_two_integers_is_refused() {
        use crate::der::{tag, write};
        let point = key_point();
        let message = include_bytes!("../fixtures/signed.txt");

        assert_eq!(
            verify(&point, b"not der at all", message, false).unwrap_err(),
            Trouble::BadSignature
        );

        // Trailing bytes after the two integers.
        let mut body = write(tag::INTEGER, &[1]);
        body.extend_from_slice(&write(tag::INTEGER, &[2]));
        body.extend_from_slice(&write(tag::INTEGER, &[3]));
        let signature = write(tag::SEQUENCE, &body);
        assert_eq!(
            verify(&point, &signature, message, false).unwrap_err(),
            Trouble::BadSignature
        );
    }

    #[test]
    fn every_byte_of_a_real_signature_matters() {
        let point = key_point();
        let message = include_bytes!("../fixtures/signed.txt");
        let signature: alloc::vec::Vec<u8> = include_bytes!("../fixtures/signed.ecdsa").to_vec();

        for at in 0..signature.len() {
            let mut broken = signature.clone();
            broken[at] ^= 0x01;
            assert!(
                verify(&point, &broken, message, false).is_err(),
                "byte {at} was changed and the signature still verified"
            );
        }
    }

    fn to_big_endian(number: &Number) -> [u8; 32] {
        let mut bytes = [0u8; 32];
        for index in 0..32 {
            bytes[31 - index] = (number[index / 8] >> ((index % 8) * 8)) as u8;
        }
        bytes
    }

    /// The public key from the ECDSA certificate fixture.
    fn key_point() -> alloc::vec::Vec<u8> {
        let der_bytes = include_bytes!("../fixtures/ecdsa-leaf.der");
        let certificate = crate::x509::parse(der_bytes).unwrap();
        match certificate.key {
            crate::x509::PublicKey::P256 { point } => point,
            other => panic!("expected a P-256 key, got {other:?}"),
        }
    }
}
