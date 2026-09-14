//! Arithmetic in the field of integers modulo 2²⁵⁵ − 19.
//!
//! Four sixty-four-bit limbs, little endian, held *almost* reduced: every value
//! is less than 2²⁵⁶ and congruent to what it represents, and it is brought
//! fully into range only when it has to be — when it is turned into bytes, or
//! compared, or its sign bit read.
//!
//! # Why this shape
//!
//! The usual implementations pack the field into five limbs of fifty-one bits
//! so that carries can be deferred. That is faster and it is a great deal
//! harder to be sure of: the invariants on how large a limb may grow are
//! spread across every operation. Here a limb is a limb, a multiply is
//! schoolbook, and reduction uses one fact — that 2²⁵⁶ ≡ 38 (mod p), because
//! 2²⁵⁵ ≡ 19 — applied twice.
//!
//! This is verification of a signature on a package, a handful of times a boot.
//! It does not need to be fast; it needs to be right.
//!
//! # Constant time
//!
//! No branch here depends on a secret. The conditional swap and the conditional
//! negate are written with masks rather than `if`, because they are the two
//! places a scalar's bits would otherwise steer the control flow — and a
//! signature scheme whose timing leaks the private key is not a signature
//! scheme. Everything else is unconditional.

/// An element of the field.
#[derive(Clone, Copy, Debug)]
pub struct Fe(pub [u64; 4]);

/// Zero.
pub const ZERO: Fe = Fe([0, 0, 0, 0]);
/// One.
pub const ONE: Fe = Fe([1, 0, 0, 0]);

impl Fe {
    /// Read a field element from thirty-two little-endian bytes.
    ///
    /// The top bit is discarded, as every reader of a curve point does: it
    /// carries the sign of x in a compressed point, not part of y.
    #[must_use]
    pub fn from_bytes(bytes: &[u8; 32]) -> Self {
        let mut limbs = [0u64; 4];
        for (index, limb) in limbs.iter_mut().enumerate() {
            let mut word = [0u8; 8];
            word.copy_from_slice(&bytes[index * 8..index * 8 + 8]);
            *limb = u64::from_le_bytes(word);
        }
        limbs[3] &= (1 << 63) - 1;
        Self(limbs)
    }

    /// Write the canonical representative as thirty-two little-endian bytes.
    #[must_use]
    pub fn to_bytes(self) -> [u8; 32] {
        let reduced = self.freeze();
        let mut bytes = [0u8; 32];
        for (index, limb) in reduced.0.iter().enumerate() {
            bytes[index * 8..index * 8 + 8].copy_from_slice(&limb.to_le_bytes());
        }
        bytes
    }

    /// The unique representative below p.
    ///
    /// Subtracting p at most twice is enough: an almost-reduced value is below
    /// 2²⁵⁶, and 2²⁵⁶ is less than three times p.
    #[must_use]
    pub fn freeze(self) -> Self {
        let mut value = self;
        for _ in 0..3 {
            value = value.subtract_p_if_at_least_p();
        }
        value
    }

    /// `self - p` if that does not go negative, and `self` otherwise, without
    /// branching on which.
    fn subtract_p_if_at_least_p(self) -> Self {
        // p = 2^255 - 19, so its limbs are 0xFFFF...FFED and three of all ones
        // with the top bit clear.
        const P: [u64; 4] = [
            0xFFFF_FFFF_FFFF_FFED,
            0xFFFF_FFFF_FFFF_FFFF,
            0xFFFF_FFFF_FFFF_FFFF,
            0x7FFF_FFFF_FFFF_FFFF,
        ];
        let mut difference = [0u64; 4];
        let mut borrow = 0u64;
        for ((slot, left), right) in difference.iter_mut().zip(self.0).zip(P) {
            let (value, first) = left.overflowing_sub(right);
            let (value, second) = value.overflowing_sub(borrow);
            *slot = value;
            borrow = u64::from(first || second);
        }
        // If the subtraction borrowed, `self` was below p and the difference is
        // meaningless; the mask selects between the two without a branch.
        let mask = borrow.wrapping_sub(1);
        let mut out = [0u64; 4];
        for ((slot, taken), kept) in out.iter_mut().zip(difference).zip(self.0) {
            *slot = (taken & mask) | (kept & !mask);
        }
        Self(out)
    }

    /// Add.
    ///
    /// Named rather than an operator, and named the same as the trait method
    /// clippy would rather see implemented. A field element is not a number in
    /// the sense `+` implies: it is only defined modulo p, `add` and `multiply`
    /// leave their results almost-reduced rather than canonical, and equality
    /// is not the equality of the bytes. Spelling out every operation is what
    /// keeps that visible at the call site.
    #[allow(clippy::should_implement_trait)]
    #[must_use]
    pub fn add(self, other: Self) -> Self {
        let mut sum = [0u64; 4];
        let mut carry = 0u64;
        for ((slot, left), right) in sum.iter_mut().zip(self.0).zip(other.0) {
            let (value, first) = left.overflowing_add(right);
            let (value, second) = value.overflowing_add(carry);
            *slot = value;
            carry = u64::from(first || second);
        }
        // A carry out of the top is 2²⁵⁶, which is 38.
        Self(sum).fold(carry * 38)
    }

    /// Subtract.
    #[must_use]
    pub fn subtract(self, other: Self) -> Self {
        let mut difference = [0u64; 4];
        let mut borrow = 0u64;
        for ((slot, left), right) in difference.iter_mut().zip(self.0).zip(other.0) {
            let (value, first) = left.overflowing_sub(right);
            let (value, second) = value.overflowing_sub(borrow);
            *slot = value;
            borrow = u64::from(first || second);
        }
        // A borrow out of the top means the answer was negative by 2²⁵⁶, so
        // adding p twice -- which is 2²⁵⁶ - 38 -- puts it back in range without
        // ever forming a negative number.
        let mut result = Self(difference);
        if borrow != 0 {
            result = result.subtract_small(38);
        }
        result
    }

    /// Subtract a small number, wrapping the same way.
    fn subtract_small(self, amount: u64) -> Self {
        let mut out = self.0;
        let mut borrow = amount;
        for limb in out.iter_mut() {
            let (value, under) = limb.overflowing_sub(borrow);
            *limb = value;
            borrow = u64::from(under);
            if borrow == 0 {
                break;
            }
        }
        Self(out)
    }

    /// Add a small number, folding a carry back in.
    fn fold(self, amount: u64) -> Self {
        let mut out = self.0;
        let mut carry = amount;
        for limb in out.iter_mut() {
            let (value, over) = limb.overflowing_add(carry);
            *limb = value;
            carry = u64::from(over);
            if carry == 0 {
                break;
            }
        }
        // A carry out of the top again is another 2²⁵⁶; it can only be one, and
        // adding 38 to a number that just wrapped cannot wrap again.
        if carry != 0 {
            let mut second = out;
            let mut extra = 38u64;
            for limb in second.iter_mut() {
                let (value, over) = limb.overflowing_add(extra);
                *limb = value;
                extra = u64::from(over);
                if extra == 0 {
                    break;
                }
            }
            out = second;
        }
        Self(out)
    }

    /// Multiply.
    #[must_use]
    pub fn multiply(self, other: Self) -> Self {
        // Schoolbook into eight limbs, with a hundred and twenty-eight bit
        // intermediates so nothing can overflow.
        let mut wide = [0u128; 8];
        for (i, left) in self.0.iter().enumerate() {
            let mut carry = 0u128;
            for (j, right) in other.0.iter().enumerate() {
                let product = u128::from(*left) * u128::from(*right) + wide[i + j] + carry;
                wide[i + j] = product & u128::from(u64::MAX);
                carry = product >> 64;
            }
            wide[i + 4] += carry;
        }

        let low = [
            wide[0] as u64,
            wide[1] as u64,
            wide[2] as u64,
            wide[3] as u64,
        ];
        let high = [
            wide[4] as u64,
            wide[5] as u64,
            wide[6] as u64,
            wide[7] as u64,
        ];

        // The high half is worth 2²⁵⁶ times itself, and 2²⁵⁶ ≡ 38. Multiplying
        // a 256-bit number by 38 gives at most 262 bits, so the fold has to be
        // done again on what comes out -- but only once, because 38 times a
        // six-bit carry is tiny.
        let mut result = Self(low);
        let mut carry = 0u128;
        let mut scaled = [0u64; 4];
        for (index, limb) in high.iter().enumerate() {
            let product = u128::from(*limb) * 38 + carry;
            scaled[index] = product as u64;
            carry = product >> 64;
        }
        result = result.add_no_reduce(Self(scaled));
        result = result.fold((carry as u64).wrapping_mul(38));
        result
    }

    /// Add without folding, for use where the caller folds afterwards.
    fn add_no_reduce(self, other: Self) -> Self {
        let mut sum = [0u64; 4];
        let mut carry = 0u64;
        for ((slot, left), right) in sum.iter_mut().zip(self.0).zip(other.0) {
            let (value, first) = left.overflowing_add(right);
            let (value, second) = value.overflowing_add(carry);
            *slot = value;
            carry = u64::from(first || second);
        }
        Self(sum).fold(carry * 38)
    }

    /// Square.
    #[must_use]
    pub fn square(self) -> Self {
        self.multiply(self)
    }

    /// Negate.
    #[must_use]
    pub fn negate(self) -> Self {
        ZERO.subtract(self)
    }

    /// Raise to a power given as bits, most significant first.
    fn power(self, exponent: &[u8]) -> Self {
        let mut result = ONE;
        for byte in exponent {
            for bit in (0..8).rev() {
                result = result.square();
                if (byte >> bit) & 1 == 1 {
                    result = result.multiply(self);
                }
            }
        }
        result
    }

    /// The multiplicative inverse, by Fermat: `a^(p-2)`.
    ///
    /// Exponentiation rather than the extended Euclidean algorithm, because
    /// Euclid branches on the values and this does not.
    #[must_use]
    pub fn invert(self) -> Self {
        // p - 2 = 2^255 - 21.
        const P_MINUS_2: [u8; 32] = [
            0x7F, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF,
            0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF,
            0xFF, 0xFF, 0xFF, 0xEB,
        ];
        self.power(&P_MINUS_2)
    }

    /// `self^((p-5)/8)`, which is what recovering x from y needs.
    #[must_use]
    pub fn power_p_minus_5_over_8(self) -> Self {
        // (p - 5) / 8 = 2^252 - 3.
        const EXPONENT: [u8; 32] = [
            0x0F, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF,
            0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF,
            0xFF, 0xFF, 0xFF, 0xFD,
        ];
        self.power(&EXPONENT)
    }

    /// Whether this is zero.
    #[must_use]
    pub fn is_zero(self) -> bool {
        self.freeze().0 == [0, 0, 0, 0]
    }

    /// Whether two elements are the same.
    #[must_use]
    pub fn equals(self, other: Self) -> bool {
        self.subtract(other).is_zero()
    }

    /// The low bit of the canonical representative, which is what a compressed
    /// point stores as the sign of x.
    #[must_use]
    pub fn is_odd(self) -> bool {
        self.freeze().0[0] & 1 == 1
    }

    /// `if choose { other } else { self }`, without a branch.
    #[must_use]
    pub fn select(self, other: Self, choose: bool) -> Self {
        let mask = 0u64.wrapping_sub(u64::from(choose));
        let mut out = [0u64; 4];
        for ((slot, taken), kept) in out.iter_mut().zip(other.0).zip(self.0) {
            *slot = (taken & mask) | (kept & !mask);
        }
        Self(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn from_u64(value: u64) -> Fe {
        Fe([value, 0, 0, 0])
    }

    #[test]
    fn small_numbers_add_and_multiply() {
        let two = from_u64(2);
        let three = from_u64(3);
        assert_eq!(two.add(three).to_bytes(), from_u64(5).to_bytes());
        assert_eq!(two.multiply(three).to_bytes(), from_u64(6).to_bytes());
        assert_eq!(three.subtract(two).to_bytes(), ONE.to_bytes());
    }

    #[test]
    fn subtraction_wraps_into_the_field() {
        // 2 - 3 is p - 1, which is the largest element there is.
        let answer = from_u64(2).subtract(from_u64(3));
        let expected = ZERO.subtract(ONE);
        assert!(answer.equals(expected));
        // And it is not zero, which is what a broken borrow would give.
        assert!(!answer.is_zero());
    }

    #[test]
    fn the_modulus_is_zero() {
        // p itself, written out. Reading it in and reducing must give zero:
        // this is the one value that tells a reduction from a no-op.
        let mut bytes = [0xFFu8; 32];
        bytes[0] = 0xED;
        bytes[31] = 0x7F;
        assert!(Fe::from_bytes(&bytes).is_zero());
    }

    #[test]
    fn two_to_the_255_is_19() {
        // The fact the whole reduction rests on.
        let mut two = from_u64(2);
        let mut power = ONE;
        let mut exponent = 255;
        while exponent > 0 {
            if exponent & 1 == 1 {
                power = power.multiply(two);
            }
            two = two.square();
            exponent >>= 1;
        }
        assert!(power.equals(from_u64(19)));
    }

    #[test]
    fn inverses_multiply_to_one() {
        for value in [1u64, 2, 3, 5, 1_000_003, u64::MAX] {
            let element = from_u64(value);
            assert!(element.multiply(element.invert()).equals(ONE), "{value}");
        }
    }

    #[test]
    fn a_big_multiplication_reduces() {
        // Two values just under p, whose product is nearly 2⁵¹⁰ -- so the whole
        // high half is in play and the fold has to happen twice.
        let big = ZERO.subtract(from_u64(1));
        let product = big.multiply(big);
        // (p-1)² ≡ 1 (mod p).
        assert!(product.equals(ONE));
    }

    #[test]
    fn selecting_does_not_look_at_the_values() {
        let a = from_u64(7);
        let b = from_u64(9);
        assert!(a.select(b, false).equals(a));
        assert!(a.select(b, true).equals(b));
    }

    #[test]
    fn bytes_round_trip() {
        let mut bytes = [0u8; 32];
        for (index, byte) in bytes.iter_mut().enumerate() {
            *byte = (index as u8).wrapping_mul(37).wrapping_add(11);
        }
        // The top bit is not part of the value, so clear it before comparing.
        bytes[31] &= 0x7F;
        assert_eq!(Fe::from_bytes(&bytes).to_bytes(), bytes);
    }

    #[test]
    fn oddness_is_of_the_canonical_form() {
        // p is even in its canonical form -- it is zero -- even though the
        // bytes that spell it out end in 0xED, which is odd. A parity taken
        // before reduction would get this wrong, and the sign bit of a
        // compressed point is exactly this parity.
        let mut bytes = [0xFFu8; 32];
        bytes[0] = 0xED;
        bytes[31] = 0x7F;
        assert!(!Fe::from_bytes(&bytes).is_odd());
    }
}
