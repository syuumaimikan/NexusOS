//! Arithmetic modulo the order of the curve's prime-order subgroup.
//!
//! `L = 2²⁵² + 27742317777372353535851937790883648493`, which is not a nice
//! number and gets no clever treatment here. A five-hundred-and-twelve-bit
//! value is reduced by shifting and conditionally subtracting, five hundred and
//! twelve times — the way long division is done by hand.
//!
//! That is slow, and it is slow in a place where it does not matter: a
//! signature is verified a handful of times per boot, and the alternative is
//! the twenty-four packed limbs of `sc_reduce`, which is impossible to check by
//! reading. This can be checked by reading.
//!
//! Every step is unconditional. The subtraction happens whether or not it is
//! needed and a mask chooses the answer, so nothing about a secret scalar shows
//! up in how long this takes.

/// `L`, little endian.
const L: [u8; 32] = [
    0xED, 0xD3, 0xF5, 0x5C, 0x1A, 0x63, 0x12, 0x58, 0xD6, 0x9C, 0xF7, 0xA2, 0xDE, 0xF9, 0xDE, 0x14,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x10,
];

/// `L` as four little-endian limbs, for comparing.
const L_LIMBS: [u64; 4] = [
    0x5812_631a_5cf5_d3ed,
    0x14de_f9de_a2f7_9cd6,
    0x0000_0000_0000_0000,
    0x1000_0000_0000_0000,
];

/// Whether a thirty-two byte scalar is below `L`.
///
/// What a verifier checks about the `S` half of a signature. Without it a
/// signature can be turned into a second, different signature on the same
/// message that also verifies — which is fine for authenticity and disastrous
/// anywhere something is identified by the bytes of its signature.
#[must_use]
pub fn is_canonical(bytes: &[u8; 32]) -> bool {
    let value = to_limbs(bytes);
    for index in (0..4).rev() {
        if value[index] < L_LIMBS[index] {
            return true;
        }
        if value[index] > L_LIMBS[index] {
            return false;
        }
    }
    // Exactly L, which is zero and is not canonical as a scalar.
    false
}

/// Reduce sixty-four bytes modulo `L`.
///
/// What turns a SHA-512 digest into a scalar.
#[must_use]
pub fn reduce(wide: &[u8; 64]) -> [u8; 32] {
    // Shift the value in from the top, one bit at a time, subtracting L
    // whenever what has accumulated has reached it. This is long division, and
    // the remainder is what is wanted.
    let mut remainder = [0u64; 4];
    for index in (0..64).rev() {
        let byte = wide[index];
        for bit in (0..8).rev() {
            remainder = shift_left(remainder, (byte >> bit) & 1);
            remainder = subtract_l_if_at_least_l(remainder);
        }
    }
    from_limbs(remainder)
}

/// `(a * b + c) mod L`.
///
/// The one combination Ed25519 needs: signing computes `r + k·a`.
#[must_use]
pub fn multiply_add(a: &[u8; 32], b: &[u8; 32], c: &[u8; 32]) -> [u8; 32] {
    let left = to_limbs(a);
    let right = to_limbs(b);

    // Schoolbook into eight limbs. Both inputs are below L, which is below
    // 2²⁵³, so the product is below 2⁵⁰⁶ and adding a third value below L
    // cannot reach 2⁵¹².
    let mut wide = [0u128; 8];
    for (i, x) in left.iter().enumerate() {
        let mut carry = 0u128;
        for (j, y) in right.iter().enumerate() {
            let product = u128::from(*x) * u128::from(*y) + wide[i + j] + carry;
            wide[i + j] = product & u128::from(u64::MAX);
            carry = product >> 64;
        }
        wide[i + 4] += carry;
    }

    let addend = to_limbs(c);
    let mut carry = 0u128;
    for (index, limb) in addend.iter().enumerate() {
        let sum = wide[index] + u128::from(*limb) + carry;
        wide[index] = sum & u128::from(u64::MAX);
        carry = sum >> 64;
    }
    for slot in wide.iter_mut().skip(4) {
        let sum = *slot + carry;
        *slot = sum & u128::from(u64::MAX);
        carry = sum >> 64;
    }

    let mut bytes = [0u8; 64];
    for (index, limb) in wide.iter().enumerate() {
        bytes[index * 8..index * 8 + 8].copy_from_slice(&(*limb as u64).to_le_bytes());
    }
    reduce(&bytes)
}

/// Whether a scalar is zero.
#[must_use]
pub fn is_zero(bytes: &[u8; 32]) -> bool {
    bytes.iter().all(|byte| *byte == 0)
}

/// The order itself, for whoever wants to compare against it.
#[must_use]
pub fn order() -> [u8; 32] {
    L
}

/// Shift left by one and bring in a bit.
fn shift_left(value: [u64; 4], bit: u8) -> [u64; 4] {
    let mut out = [0u64; 4];
    let mut carry = u64::from(bit);
    for (slot, limb) in out.iter_mut().zip(value) {
        *slot = (limb << 1) | carry;
        carry = limb >> 63;
    }
    // Anything shifted out of the top is dropped, which is safe because the
    // remainder is always below L and L is below 2²⁵³: three shifts of headroom.
    out
}

/// `value - L` if that does not go negative, without branching on which.
fn subtract_l_if_at_least_l(value: [u64; 4]) -> [u64; 4] {
    let mut difference = [0u64; 4];
    let mut borrow = 0u64;
    for ((slot, left), right) in difference.iter_mut().zip(value).zip(L_LIMBS) {
        let (result, first) = left.overflowing_sub(right);
        let (result, second) = result.overflowing_sub(borrow);
        *slot = result;
        borrow = u64::from(first || second);
    }
    let mask = borrow.wrapping_sub(1);
    let mut out = [0u64; 4];
    for ((slot, taken), kept) in out.iter_mut().zip(difference).zip(value) {
        *slot = (taken & mask) | (kept & !mask);
    }
    out
}

/// Thirty-two little-endian bytes as four limbs.
fn to_limbs(bytes: &[u8; 32]) -> [u64; 4] {
    let mut limbs = [0u64; 4];
    for (index, limb) in limbs.iter_mut().enumerate() {
        let mut word = [0u8; 8];
        word.copy_from_slice(&bytes[index * 8..index * 8 + 8]);
        *limb = u64::from_le_bytes(word);
    }
    limbs
}

/// And back.
fn from_limbs(limbs: [u64; 4]) -> [u8; 32] {
    let mut bytes = [0u8; 32];
    for (index, limb) in limbs.iter().enumerate() {
        bytes[index * 8..index * 8 + 8].copy_from_slice(&limb.to_le_bytes());
    }
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;

    fn small(value: u64) -> [u8; 32] {
        let mut bytes = [0u8; 32];
        bytes[..8].copy_from_slice(&value.to_le_bytes());
        bytes
    }

    fn widen(narrow: &[u8; 32]) -> [u8; 64] {
        let mut wide = [0u8; 64];
        wide[..32].copy_from_slice(narrow);
        wide
    }

    #[test]
    fn small_values_are_unchanged() {
        for value in [0u64, 1, 2, 12345, u64::MAX] {
            assert_eq!(reduce(&widen(&small(value))), small(value), "{value}");
        }
    }

    #[test]
    fn the_order_reduces_to_zero() {
        assert!(is_zero(&reduce(&widen(&L))));
    }

    #[test]
    fn one_past_the_order_is_one() {
        let mut value = L;
        value[0] += 1;
        assert_eq!(reduce(&widen(&value)), small(1));
    }

    #[test]
    fn multiplication_and_addition_agree_with_small_arithmetic() {
        // Numbers small enough that the answer is obvious, so a reduction that
        // fired when it should not have is visible.
        let answer = multiply_add(&small(6), &small(7), &small(5));
        assert_eq!(answer, small(47));
    }

    #[test]
    fn multiplying_by_the_order_gives_the_addend() {
        // L is zero in this arithmetic, so anything times it vanishes and only
        // what was added is left.
        let answer = multiply_add(&L, &small(999), &small(17));
        assert_eq!(answer, small(17));
    }

    #[test]
    fn a_product_that_needs_the_whole_width_is_reduced() {
        // Two values just under L. Their product is nearly 2⁵⁰⁶ and the
        // reduction has to walk the whole of it.
        let mut nearly = L;
        nearly[0] -= 1;
        let answer = multiply_add(&nearly, &nearly, &small(0));
        // (L-1)² ≡ 1 (mod L).
        assert_eq!(answer, small(1));
    }

    #[test]
    fn what_is_canonical_and_what_is_not() {
        assert!(is_canonical(&small(0)));
        assert!(is_canonical(&small(1)));
        let mut just_under = L;
        just_under[0] -= 1;
        assert!(is_canonical(&just_under));
        // L itself and anything above it are not: they are other spellings of
        // a value that already has one.
        assert!(!is_canonical(&L));
        assert!(!is_canonical(&[0xFF; 32]));
    }

    #[test]
    fn the_order_is_the_one_the_curve_has() {
        // 2^252 + 27742317777372353535851937790883648493, checked by adding the
        // two halves rather than by trusting the byte string above.
        let mut power = [0u8; 32];
        power[31] = 0x10; // 2^252
        let low: u128 = 27_742_317_777_372_353_535_851_937_790_883_648_493;
        let mut sum = power;
        let mut carry = 0u16;
        for (index, byte) in low.to_le_bytes().iter().enumerate() {
            let total = u16::from(sum[index]) + u16::from(*byte) + carry;
            sum[index] = total as u8;
            carry = total >> 8;
        }
        assert_eq!(carry, 0);
        assert_eq!(sum, order());
    }
}
