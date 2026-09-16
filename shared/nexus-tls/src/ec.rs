//! The arithmetic underneath the NIST prime curves, verifying only.
//!
//! P-256 and P-384 differ in exactly two things: how many limbs a number has,
//! and what the constants are. Everything else -- Montgomery multiplication,
//! the Jacobian group law, the ECDSA check -- is the same code.
//!
//! So it is the same code. This file is generic over the limb count and takes
//! the constants in a [`Curve`]; `p256.rs` and `p384.rs` are each a page of
//! numbers and a call into here.
//!
//! # Why that mattered enough to do
//!
//! The alternative was eight hundred lines copied with `4` changed to `6`. The
//! reason not to is not tidiness: it is that a mistake found in one copy has to
//! be remembered in the other, and the second copy is the one nobody is
//! looking at. This code refuses invalid-curve points and malleable
//! signatures, and both of those are things that must never be fixed in only
//! one place.
//!
//! The P-256 test suite is what made the change safe to make. Every vector in
//! `p256.rs` -- including OpenSSL's own signatures -- still passes against this
//! generic version, which is a stronger statement than "it compiles".
//!
//! # Verifying only
//!
//! As in [`crate::rsa`]: every number here is public. The public key is in the
//! certificate, the signature was sent in the clear, and the message is the
//! handshake transcript. There is no secret for a timing side channel to leak,
//! so the ladder branches on bits freely and the code is the clearer for it.
//!
//! A file that did **signing** would need every one of those branches removed
//! and should be a different file with that said at the top.
//!
//! # Jacobian coordinates
//!
//! Affine point addition needs a modular inversion each time, and an inversion
//! is an exponentiation -- hundreds of squarings. Jacobian coordinates carry a
//! denominator along instead, so there is exactly one inversion at the end.

/// The most limbs any curve here uses.
///
/// P-384 is six. It bounds the one scratch buffer that would otherwise need
/// `[u64; L + 2]`, which stable Rust will not let a const generic express.
pub const MAX_LIMBS: usize = 6;

/// Everything that distinguishes one curve from another.
///
/// `a` is not here: both curves have `a = -3`, which the doubling formula uses
/// directly. A curve with some other `a` would need a different `double`, and
/// putting `a` in this struct would suggest otherwise.
pub struct Curve<const L: usize> {
    /// The field's prime.
    pub p: [u64; L],
    /// The order of the base point.
    pub n: [u64; L],
    /// The curve's `b`.
    pub b: [u64; L],
    /// The base point.
    pub gx: [u64; L],
    pub gy: [u64; L],
    /// `-p⁻¹ mod 2⁶⁴` and `-n⁻¹ mod 2⁶⁴`, which Montgomery needs.
    pub p0: u64,
    pub n0: u64,
}

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

impl Trouble {
    /// How to say this about a particular curve.
    #[must_use]
    pub fn describe(self, curve: &str) -> &'static str {
        // The curve name is taken so that a caller can check it is talking
        // about the right one, and deliberately not interpolated: this is
        // `no_std` and a `Display` that allocated would be the only allocation
        // in the file.
        let _ = curve;
        match self {
            Self::BadKey => "the public key is not a point on the curve",
            Self::BadSignature => "the ECDSA signature is malformed",
            Self::Mismatch => "the signature is for a different message",
        }
    }
}

// ---------------------------------------------------------------------------
// Arithmetic modulo a prime of L limbs
// ---------------------------------------------------------------------------

/// Zero, of the right width.
#[must_use]
pub fn zero<const L: usize>() -> [u64; L] {
    [0u64; L]
}

/// One, of the right width.
#[must_use]
pub fn one<const L: usize>() -> [u64; L] {
    let mut value = [0u64; L];
    value[0] = 1;
    value
}

#[must_use]
pub fn is_zero<const L: usize>(value: &[u64; L]) -> bool {
    value.iter().all(|limb| *limb == 0)
}

/// Whether `one` is at least `other`.
#[must_use]
pub fn at_least<const L: usize>(one: &[u64; L], other: &[u64; L]) -> bool {
    for index in (0..L).rev() {
        match one[index].cmp(&other[index]) {
            core::cmp::Ordering::Greater => return true,
            core::cmp::Ordering::Less => return false,
            core::cmp::Ordering::Equal => {}
        }
    }
    true
}

/// `one + other mod modulus`.
#[must_use]
pub fn add_mod<const L: usize>(one: &[u64; L], other: &[u64; L], modulus: &[u64; L]) -> [u64; L] {
    let mut out = [0u64; L];
    let mut carry = 0u128;
    for index in 0..L {
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
#[must_use]
pub fn subtract<const L: usize>(one: &[u64; L], other: &[u64; L]) -> [u64; L] {
    let mut out = [0u64; L];
    let mut borrow = 0u64;
    for index in 0..L {
        let (first, one_borrowed) = one[index].overflowing_sub(other[index]);
        let (second, two_borrowed) = first.overflowing_sub(borrow);
        out[index] = second;
        borrow = u64::from(one_borrowed) + u64::from(two_borrowed);
    }
    out
}

/// `one - other mod modulus`.
#[must_use]
pub fn subtract_mod<const L: usize>(
    one: &[u64; L],
    other: &[u64; L],
    modulus: &[u64; L],
) -> [u64; L] {
    if at_least(one, other) {
        subtract(one, other)
    } else {
        // Add the modulus first so the subtraction stays positive.
        let mut sum = [0u64; L];
        let mut carry = 0u128;
        for index in 0..L {
            let value = u128::from(one[index]) + u128::from(modulus[index]) + carry;
            sum[index] = value as u64;
            carry = value >> 64;
        }
        subtract(&sum, other)
    }
}

/// `2 × value mod modulus`.
#[must_use]
pub fn double_mod<const L: usize>(value: &[u64; L], modulus: &[u64; L]) -> [u64; L] {
    add_mod(value, value, modulus)
}

/// `-modulus⁻¹ mod 2⁶⁴`, by Newton's method. Every modulus here is odd.
#[must_use]
pub const fn inverse_of_low_limb(low: u64) -> u64 {
    let mut inverse = 1u64;
    let mut step = 0;
    while step < 6 {
        inverse = inverse.wrapping_mul(2u64.wrapping_sub(low.wrapping_mul(inverse)));
        step += 1;
    }
    inverse.wrapping_neg()
}

/// Montgomery multiplication: `one × other × R⁻¹ mod modulus`.
#[must_use]
pub fn montgomery<const L: usize>(
    one: &[u64; L],
    other: &[u64; L],
    modulus: &[u64; L],
    n0: u64,
) -> [u64; L] {
    // `L + 2` limbs of accumulator, in a buffer sized for the widest curve.
    // Written that way because stable Rust will not size an array by `L + 2`,
    // and the alternative -- a heap allocation -- would be the only one in a
    // crate that has none.
    let mut t = [0u64; MAX_LIMBS + 2];
    for &b in other.iter() {
        let mut carry = 0u128;
        for index in 0..L {
            let sum = u128::from(t[index]) + u128::from(one[index]) * u128::from(b) + carry;
            t[index] = sum as u64;
            carry = sum >> 64;
        }
        let sum = u128::from(t[L]) + carry;
        t[L] = sum as u64;
        t[L + 1] = (sum >> 64) as u64;

        let m = t[0].wrapping_mul(n0);
        let mut carry = 0u128;
        for index in 0..L {
            let sum = u128::from(t[index]) + u128::from(m) * u128::from(modulus[index]) + carry;
            if index > 0 {
                t[index - 1] = sum as u64;
            }
            carry = sum >> 64;
        }
        let sum = u128::from(t[L]) + carry;
        t[L - 1] = sum as u64;
        t[L] = t[L + 1] + (sum >> 64) as u64;
    }

    let mut out = [0u64; L];
    out.copy_from_slice(&t[..L]);
    if t[L] != 0 || at_least(&out, modulus) {
        out = subtract(&out, modulus);
    }
    out
}

/// `R² mod modulus`, by doubling from one.
///
/// The bootstrap that lets everything else avoid division: each step is a shift
/// and a comparison.
#[must_use]
pub fn r_squared<const L: usize>(modulus: &[u64; L]) -> [u64; L] {
    let mut value = one::<L>();
    for _ in 0..(2 * L * 64) {
        value = double_mod(&value, modulus);
    }
    value
}

/// A field, with the constants Montgomery needs precomputed.
pub struct Field<const L: usize> {
    modulus: [u64; L],
    n0: u64,
    r2: [u64; L],
    /// One, in Montgomery form.
    one: [u64; L],
}

impl<const L: usize> Field<L> {
    #[must_use]
    pub fn new(modulus: [u64; L], n0: u64) -> Self {
        let r2 = r_squared(&modulus);
        let mut field = Self {
            modulus,
            n0,
            r2,
            one: [0; L],
        };
        field.one = field.enter(&one::<L>());
        field
    }

    /// Into Montgomery form.
    #[must_use]
    pub fn enter(&self, value: &[u64; L]) -> [u64; L] {
        montgomery(value, &self.r2, &self.modulus, self.n0)
    }

    /// Out of it.
    #[must_use]
    pub fn leave(&self, value: &[u64; L]) -> [u64; L] {
        montgomery(value, &one::<L>(), &self.modulus, self.n0)
    }

    #[must_use]
    pub fn multiply(&self, one: &[u64; L], other: &[u64; L]) -> [u64; L] {
        montgomery(one, other, &self.modulus, self.n0)
    }

    #[must_use]
    pub fn square(&self, value: &[u64; L]) -> [u64; L] {
        self.multiply(value, value)
    }

    #[must_use]
    pub fn add(&self, one: &[u64; L], other: &[u64; L]) -> [u64; L] {
        add_mod(one, other, &self.modulus)
    }

    #[must_use]
    pub fn subtract(&self, one: &[u64; L], other: &[u64; L]) -> [u64; L] {
        subtract_mod(one, other, &self.modulus)
    }

    #[must_use]
    pub fn double(&self, value: &[u64; L]) -> [u64; L] {
        double_mod(value, &self.modulus)
    }

    /// `value^exponent`, with the exponent public.
    #[must_use]
    pub fn power(&self, value: &[u64; L], exponent: &[u64; L]) -> [u64; L] {
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
    /// Every modulus here is prime, which is what makes this correct. It is the
    /// slowest operation in the file and is done twice per verification.
    #[must_use]
    pub fn invert(&self, value: &[u64; L]) -> [u64; L] {
        let mut exponent = self.modulus;
        // modulus − 2, and no modulus here ends in 0 or 1, so no borrow leaves
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
pub struct Point<const L: usize> {
    x: [u64; L],
    y: [u64; L],
    z: [u64; L],
}

impl<const L: usize> Point<L> {
    #[must_use]
    pub fn infinity() -> Self {
        Self {
            x: one::<L>(),
            y: one::<L>(),
            z: [0; L],
        }
    }

    #[must_use]
    pub fn is_infinity(&self) -> bool {
        is_zero(&self.z)
    }

    /// An affine point, as a Jacobian one with `z = 1`.
    ///
    /// Here so that the curve modules can build a point to test against without
    /// the coordinates being public -- a `Point` whose fields could be set
    /// individually would be one that could be given a `z` nothing else in this
    /// file expects.
    #[must_use]
    pub fn affine(x: [u64; L], y: [u64; L], field: &Field<L>) -> Self {
        Self { x, y, z: field.one }
    }
}

/// Double a point. The standard formula for curves with `a = −3`.
#[must_use]
pub fn double<const L: usize>(field: &Field<L>, point: &Point<L>) -> Point<L> {
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
#[must_use]
pub fn add_mixed<const L: usize>(
    field: &Field<L>,
    point: &Point<L>,
    x2: &[u64; L],
    y2: &[u64; L],
) -> Point<L> {
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
#[must_use]
pub fn affine_x<const L: usize>(field: &Field<L>, point: &Point<L>) -> Option<[u64; L]> {
    if point.is_infinity() {
        return None;
    }
    let inverse = field.invert(&point.z);
    let squared = field.square(&inverse);
    Some(field.leave(&field.multiply(&point.x, &squared)))
}

/// Whether `(x, y)`, in Montgomery form, satisfies `y² = x³ − 3x + b`.
#[must_use]
pub fn on_curve<const L: usize>(
    field: &Field<L>,
    curve: &Curve<L>,
    x: &[u64; L],
    y: &[u64; L],
) -> bool {
    let left = field.square(y);
    let x3 = field.multiply(&field.square(x), x);
    let three_x = field.add(&field.double(x), x);
    let b = field.enter(&curve.b);
    let right = field.add(&field.subtract(&x3, &three_x), &b);
    left == right
}

/// `first × G + second × key`, which is what verification needs.
///
/// Two separate ladders rather than an interleaved one: this runs once per
/// connection and the simpler version is the one that can be checked by eye.
#[must_use]
pub fn two_scalars<const L: usize>(
    field: &Field<L>,
    curve: &Curve<L>,
    first: &[u64; L],
    second: &[u64; L],
    key_x: &[u64; L],
    key_y: &[u64; L],
) -> Point<L> {
    let gx = field.enter(&curve.gx);
    let gy = field.enter(&curve.gy);

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
#[must_use]
pub fn from_bytes<const L: usize>(bytes: &[u8]) -> Option<[u64; L]> {
    if bytes.len() > L * 8 {
        return None;
    }
    let mut number = [0u64; L];
    for (index, byte) in bytes.iter().rev().enumerate() {
        number[index / 8] |= u64::from(*byte) << ((index % 8) * 8);
    }
    Some(number)
}

/// Verify an ECDSA signature over `curve`.
///
/// `point` is the uncompressed public key from the certificate: `0x04` then x
/// and y, each `L * 8` bytes. `signature` is the DER `SEQUENCE { INTEGER r,
/// INTEGER s }` that TLS and X.509 both carry. `digest` is the message hash,
/// already computed, because which hash to use is the caller's question and
/// not this one's.
///
/// # Errors
///
/// [`Trouble`], every variant of which means the signature must be rejected.
pub fn verify<const L: usize>(
    curve: &Curve<L>,
    point: &[u8],
    signature: &[u8],
    digest: &[u8],
) -> Result<(), Trouble> {
    use crate::der::{self, tag, Reader};

    let width = L * 8;
    if point.len() != 1 + 2 * width || point[0] != 0x04 {
        return Err(Trouble::BadKey);
    }
    let key_x = from_bytes::<L>(&point[1..=width]).ok_or(Trouble::BadKey)?;
    let key_y = from_bytes::<L>(&point[1 + width..]).ok_or(Trouble::BadKey)?;
    // Both coordinates must be inside the field, which a point read off the
    // wire need not be.
    if at_least(&key_x, &curve.p) || at_least(&key_y, &curve.p) {
        return Err(Trouble::BadKey);
    }

    let field = Field::new(curve.p, curve.p0);
    let key_x_form = field.enter(&key_x);
    let key_y_form = field.enter(&key_y);
    // A key that is not on the curve is one where the group law does not hold,
    // and accepting it is the classic invalid-curve attack.
    if !on_curve(&field, curve, &key_x_form, &key_y_form) {
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

    let r = from_bytes::<L>(r_bytes).ok_or(Trouble::BadSignature)?;
    let s = from_bytes::<L>(s_bytes).ok_or(Trouble::BadSignature)?;
    // Both must be in [1, n − 1]. Zero would make the arithmetic degenerate and
    // a value at or above the order is a second encoding of a smaller one --
    // which is signature malleability, and is refused.
    if is_zero(&r) || is_zero(&s) || at_least(&r, &curve.n) || at_least(&s, &curve.n) {
        return Err(Trouble::BadSignature);
    }

    // The hash, truncated to the order's width: the leftmost bytes, per
    // FIPS 186-4. A hash shorter than the order is used whole, which is what
    // happens when a P-384 key signs with SHA-256.
    let taken = digest.len().min(width);
    let mut e = from_bytes::<L>(&digest[..taken]).ok_or(Trouble::BadSignature)?;
    // Reduced modulo the order, which RFC 6979 §2.3.2 requires.
    if at_least(&e, &curve.n) {
        e = subtract(&e, &curve.n);
    }

    // u1 = e/s, u2 = r/s, both modulo the order.
    let scalars = Field::new(curve.n, curve.n0);
    let s_inverse = scalars.leave(&scalars.invert(&scalars.enter(&s)));
    let u1 = scalars.leave(&scalars.multiply(&scalars.enter(&e), &scalars.enter(&s_inverse)));
    let u2 = scalars.leave(&scalars.multiply(&scalars.enter(&r), &scalars.enter(&s_inverse)));

    let sum = two_scalars(&field, curve, &u1, &u2, &key_x_form, &key_y_form);
    let Some(x) = affine_x(&field, &sum) else {
        return Err(Trouble::Mismatch);
    };

    // The signature verifies when x mod n equals r.
    let mut reduced = x;
    if at_least(&reduced, &curve.n) {
        reduced = subtract(&reduced, &curve.n);
    }
    if reduced == r {
        Ok(())
    } else {
        Err(Trouble::Mismatch)
    }
}
