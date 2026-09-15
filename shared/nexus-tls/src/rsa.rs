//! RSA signature verification, and the arithmetic underneath it.
//!
//! Verifying only. There is no key generation, no signing and no private-key
//! operation anywhere in this file, and that is what makes the arithmetic
//! straightforward: **every number here is public.** The modulus and the
//! exponent are in the certificate, the signature was sent in the clear, and
//! there is nothing an attacker could learn from how long any of it takes that
//! they did not already have.
//!
//! So this is written for clarity rather than for constant time. A file that
//! did private-key RSA would have to be written completely differently, and if
//! one is ever wanted it should be a different file with that said at the top.
//!
//! # Montgomery form
//!
//! Modular exponentiation needs a reduction after every multiply. Long division
//! is the obvious way and is fiddly to get right; Montgomery reduction replaces
//! it with a multiply and a shift, at the cost of keeping numbers in a
//! transformed form and converting at each end.
//!
//! The transform needs `R² mod n`, which needs a reduction — the thing being
//! avoided. It is computed by doubling: start at 1, double `2 × bits` times,
//! subtracting the modulus whenever the result reaches it. Each step is a shift
//! and a comparison, so nothing has to divide.

use alloc::vec;
use alloc::vec::Vec;

use nexus_crypto::sha256;

/// The most limbs a modulus may have: 8192 bits.
///
/// Larger is refused by the certificate parser as well. Verifying costs the
/// cube of the size, so an enormous modulus is a denial of service dressed as
/// a key.
const MOST_LIMBS: usize = 128;

/// Why a signature did not verify.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trouble {
    /// The modulus or exponent is not a shape this handles.
    BadKey(&'static str),
    /// The signature is not the same length as the modulus.
    WrongLength,
    /// The padding is not what the scheme requires.
    ///
    /// Deliberately one variant rather than several: which part of the padding
    /// was wrong is exactly what a forger wants to be told, and the caller can
    /// do nothing differently for one than for another.
    BadPadding,
    /// The hash in the signature is not the hash of the message.
    Mismatch,
}

impl core::fmt::Display for Trouble {
    fn fmt(&self, out: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::BadKey(what) => write!(out, "the RSA key is unusable: {what}"),
            Self::WrongLength => out.write_str("the signature is not the size of the modulus"),
            Self::BadPadding => out.write_str("the signature's padding is wrong"),
            Self::Mismatch => out.write_str("the signature is for a different message"),
        }
    }
}

// ---------------------------------------------------------------------------
// Multi-precision arithmetic
// ---------------------------------------------------------------------------

/// A number, as little-endian 64-bit limbs.
type Number = Vec<u64>;

/// Big-endian bytes into limbs.
fn from_bytes(bytes: &[u8], limbs: usize) -> Number {
    let mut number = vec![0u64; limbs];
    for (index, byte) in bytes.iter().rev().enumerate() {
        let limb = index / 8;
        if limb >= limbs {
            break;
        }
        number[limb] |= u64::from(*byte) << ((index % 8) * 8);
    }
    number
}

/// Limbs back into big-endian bytes, of exactly `length`.
fn to_bytes(number: &[u64], length: usize) -> Vec<u8> {
    let mut bytes = vec![0u8; length];
    for index in 0..length {
        let limb = index / 8;
        if limb < number.len() {
            bytes[length - 1 - index] = (number[limb] >> ((index % 8) * 8)) as u8;
        }
    }
    bytes
}

/// Whether `one` is at least `other`.
fn at_least(one: &[u64], other: &[u64]) -> bool {
    for index in (0..one.len()).rev() {
        match one[index].cmp(&other[index]) {
            core::cmp::Ordering::Greater => return true,
            core::cmp::Ordering::Less => return false,
            core::cmp::Ordering::Equal => {}
        }
    }
    true
}

/// `one -= other`, which the caller has checked will not go below zero.
fn subtract(one: &mut [u64], other: &[u64]) {
    let mut borrow = 0u64;
    for (slot, take) in one.iter_mut().zip(other.iter()) {
        let (first, one_borrowed) = slot.overflowing_sub(*take);
        let (second, two_borrowed) = first.overflowing_sub(borrow);
        *slot = second;
        borrow = u64::from(one_borrowed) + u64::from(two_borrowed);
    }
}

/// `value = 2 × value mod modulus`.
///
/// The one operation that needs no multiplication, which is why the whole
/// Montgomery setup can be bootstrapped out of it.
fn double_mod(value: &mut [u64], modulus: &[u64]) {
    let mut carry = 0u64;
    for slot in value.iter_mut() {
        let next = *slot >> 63;
        *slot = (*slot << 1) | carry;
        carry = next;
    }
    // A carry out of the top means the value is at least 2^bits, which is
    // larger than the modulus -- so one subtraction brings it back.
    if carry != 0 || at_least(value, modulus) {
        subtract(value, modulus);
    }
}

/// `-modulus⁻¹ mod 2⁶⁴`, by Newton's method.
///
/// Each step doubles the number of correct bits, so five steps take one to
/// sixty-four. Only defined for an odd modulus, which every RSA modulus is --
/// it is a product of two odd primes.
fn inverse_of_low_limb(modulus: &[u64]) -> u64 {
    let n0 = modulus[0];
    // x ≡ n⁻¹ mod 2³, since n is odd.
    let mut inverse = 1u64;
    for _ in 0..6 {
        inverse = inverse.wrapping_mul(2u64.wrapping_sub(n0.wrapping_mul(inverse)));
    }
    inverse.wrapping_neg()
}

/// Montgomery multiplication: `one × other × R⁻¹ mod modulus`.
///
/// The coarsely-integrated operand scanning form, which interleaves the
/// multiply and the reduction so only one pass over the limbs is needed.
fn montgomery_multiply(one: &[u64], other: &[u64], modulus: &[u64], n0: u64) -> Number {
    let limbs = modulus.len();
    let mut t = vec![0u64; limbs + 2];

    for &b in other.iter().take(limbs) {
        // t += one × b
        let mut carry = 0u128;
        for index in 0..limbs {
            let sum = u128::from(t[index]) + u128::from(one[index]) * u128::from(b) + carry;
            t[index] = sum as u64;
            carry = sum >> 64;
        }
        let sum = u128::from(t[limbs]) + carry;
        t[limbs] = sum as u64;
        t[limbs + 1] = (sum >> 64) as u64;

        // t += m × modulus, chosen so the bottom limb becomes zero, then shift.
        let m = t[0].wrapping_mul(n0);
        let mut carry = 0u128;
        for index in 0..limbs {
            let sum = u128::from(t[index]) + u128::from(m) * u128::from(modulus[index]) + carry;
            if index > 0 {
                t[index - 1] = sum as u64;
            }
            carry = sum >> 64;
        }
        let sum = u128::from(t[limbs]) + carry;
        t[limbs - 1] = sum as u64;
        t[limbs] = t[limbs + 1] + (sum >> 64) as u64;
    }

    let mut result: Number = t[..limbs].to_vec();
    // One conditional subtraction: the result is below 2 × modulus.
    if t[limbs] != 0 || at_least(&result, modulus) {
        subtract(&mut result, modulus);
    }
    result
}

/// `base^exponent mod modulus`, with everything public.
fn power_mod(base: &[u8], exponent: &[u8], modulus: &[u8]) -> Result<Vec<u8>, Trouble> {
    let limbs = modulus.len().div_ceil(8);
    if limbs == 0 || limbs > MOST_LIMBS {
        return Err(Trouble::BadKey("a modulus of an impossible size"));
    }
    let n = from_bytes(modulus, limbs);
    if n[0].is_multiple_of(2) {
        // Montgomery needs an odd modulus, and an even RSA modulus is not one
        // -- it would mean a factor of two, which is to say a key that is not
        // a key.
        return Err(Trouble::BadKey("an even modulus"));
    }

    let value = from_bytes(base, limbs);
    if at_least(&value, &n) {
        // RFC 8017 §5.2.2: a signature representative at least as large as the
        // modulus is out of range and the signature is invalid.
        return Err(Trouble::BadKey("a value not smaller than the modulus"));
    }

    let n0 = inverse_of_low_limb(&n);

    // R mod n, by doubling from one. Then R² mod n, by doubling again.
    let mut r = vec![0u64; limbs];
    r[0] = 1;
    for _ in 0..(limbs * 64) {
        double_mod(&mut r, &n);
    }
    let mut r2 = r.clone();
    for _ in 0..(limbs * 64) {
        double_mod(&mut r2, &n);
    }

    // Into Montgomery form, and a running result of one in the same form.
    let base_form = montgomery_multiply(&value, &r2, &n, n0);
    let mut result = r;

    // Square and multiply, most significant bit first. Not constant time, and
    // it does not need to be: the exponent is in the certificate.
    let mut started = false;
    for byte in exponent {
        for bit in (0..8).rev() {
            if started {
                result = montgomery_multiply(&result, &result, &n, n0);
            }
            if byte & (1 << bit) != 0 {
                if started {
                    result = montgomery_multiply(&result, &base_form, &n, n0);
                } else {
                    result = base_form.clone();
                    started = true;
                }
            }
        }
    }
    if !started {
        // An exponent of zero. Nothing legitimate has one, and the answer would
        // be 1 -- which would make every signature verify against a key whose
        // exponent somebody chose.
        return Err(Trouble::BadKey("an exponent of zero"));
    }

    // Out of Montgomery form: multiplying by one undoes the R.
    let mut one = vec![0u64; limbs];
    one[0] = 1;
    let plain = montgomery_multiply(&result, &one, &n, n0);
    Ok(to_bytes(&plain, modulus.len()))
}

// ---------------------------------------------------------------------------
// PKCS#1 v1.5
// ---------------------------------------------------------------------------

/// The DER prefix a PKCS#1 v1.5 SHA-256 signature wraps the hash in.
///
/// `DigestInfo ::= SEQUENCE { AlgorithmIdentifier, OCTET STRING }` with the
/// algorithm being id-sha256 and a NULL parameter. Written out as bytes because
/// it is a constant, and because building it would invite a parser on the way
/// back -- and *parsing* this is how Bleichenbacher's 2006 forgery worked.
const SHA256_PREFIX: &[u8] = &[
    0x30, 0x31, 0x30, 0x0d, 0x06, 0x09, 0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x01, 0x05,
    0x00, 0x04, 0x20,
];

/// And for SHA-384.
const SHA384_PREFIX: &[u8] = &[
    0x30, 0x41, 0x30, 0x0d, 0x06, 0x09, 0x60, 0x86, 0x48, 0x01, 0x65, 0x03, 0x04, 0x02, 0x02, 0x05,
    0x00, 0x04, 0x30,
];

/// Verify a PKCS#1 v1.5 signature over the SHA-256 of `message`.
///
/// # How this avoids Bleichenbacher's forgery
///
/// The 2006 attack worked against implementations that *parsed* the padded
/// block -- found the hash somewhere inside it and checked that, ignoring
/// whatever else was there. With a small exponent an attacker can construct a
/// cube root that happens to contain a valid-looking `DigestInfo` followed by
/// rubbish, and a parser accepts it.
///
/// So nothing here parses. The expected block is **built** and compared byte
/// for byte against what came out of the exponentiation. There is exactly one
/// correct answer and anything else fails.
pub fn verify_pkcs1(
    modulus: &[u8],
    exponent: &[u8],
    signature: &[u8],
    message: &[u8],
    sha384: bool,
) -> Result<(), Trouble> {
    if signature.len() != modulus.len() {
        return Err(Trouble::WrongLength);
    }
    let (prefix, digest): (&[u8], Vec<u8>) = if sha384 {
        // SHA-384 truncates SHA-512; this crate carries SHA-512, so it is the
        // one place the other hash is needed.
        (SHA384_PREFIX, sha384_of(message))
    } else {
        (SHA256_PREFIX, sha256::digest(message).to_vec())
    };

    let recovered = power_mod(signature, exponent, modulus)?;

    // The block that a correct signature must produce, built rather than read:
    //   0x00 0x01  0xFF...0xFF  0x00  DigestInfo
    let filler = modulus
        .len()
        .checked_sub(3 + prefix.len() + digest.len())
        .ok_or(Trouble::BadPadding)?;
    // At least eight bytes of padding, which RFC 8017 requires and which is
    // what stops a modulus barely larger than the hash.
    if filler < 8 {
        return Err(Trouble::BadPadding);
    }

    let mut expected = Vec::with_capacity(modulus.len());
    expected.push(0x00);
    expected.push(0x01);
    expected.extend(core::iter::repeat_n(0xFF, filler));
    expected.push(0x00);
    expected.extend_from_slice(prefix);
    expected.extend_from_slice(&digest);

    if sha256::same(&expected, &recovered) {
        Ok(())
    } else {
        Err(Trouble::Mismatch)
    }
}

/// SHA-384, for the RSA-SHA384 certificates that exist and are not rare.
///
/// In `nexus_crypto` rather than here, because it is SHA-512's block function
/// with a different starting state -- and a second copy of that function would
/// be a second thing to keep right. It is emphatically *not* SHA-512 truncated,
/// which is the mistake it would be easy to make and which
/// `sha512::sha384_tests` rules out by name.
fn sha384_of(message: &[u8]) -> Vec<u8> {
    nexus_crypto::sha512::digest_384(message).to_vec()
}

// ---------------------------------------------------------------------------
// PSS
// ---------------------------------------------------------------------------

/// Verify an RSASSA-PSS signature over the SHA-256 of `message`.
///
/// RFC 8017 §9.1.2, with SHA-256 for both the hash and the mask generation and
/// a salt as long as the hash -- which is what TLS 1.3 requires (RFC 8446
/// §4.2.3) and what every implementation uses.
pub fn verify_pss(
    modulus: &[u8],
    exponent: &[u8],
    signature: &[u8],
    message: &[u8],
) -> Result<(), Trouble> {
    if signature.len() != modulus.len() {
        return Err(Trouble::WrongLength);
    }
    const HASH: usize = 32;
    const SALT: usize = 32;

    let encoded = power_mod(signature, exponent, modulus)?;
    let bits = modulus.len() * 8;

    // The modulus's top bit decides how many bits of the encoding are used.
    // For the 2048-bit keys everything uses, `emBits` is 2047 and the leading
    // byte has one bit that must be zero.
    let em_bits = bits - 1;
    let em_len = em_bits.div_ceil(8);
    if em_len < HASH + SALT + 2 {
        return Err(Trouble::BadPadding);
    }
    // A modulus whose bit length is a multiple of eight has an encoding one
    // byte shorter than itself, with a leading zero.
    let encoded = if em_len < encoded.len() {
        if encoded[0] != 0 {
            return Err(Trouble::BadPadding);
        }
        &encoded[encoded.len() - em_len..]
    } else {
        &encoded[..]
    };

    if *encoded.last().ok_or(Trouble::BadPadding)? != 0xBC {
        return Err(Trouble::BadPadding);
    }
    let masked_len = em_len - HASH - 1;
    let masked = &encoded[..masked_len];
    let hash = &encoded[masked_len..em_len - 1];

    // The unused bits of the leading byte must be zero.
    let unused = 8 * em_len - em_bits;
    if masked[0] >> (8 - unused) != 0 {
        return Err(Trouble::BadPadding);
    }

    // Unmask, using MGF1 over the hash.
    let mut db = masked.to_vec();
    let mask = mgf1(hash, masked_len);
    for (slot, byte) in db.iter_mut().zip(mask.iter()) {
        *slot ^= byte;
    }
    db[0] &= 0xFF >> unused;

    // Everything before the salt must be zeros then a single one byte.
    let separator = masked_len - SALT - 1;
    if db[..separator].iter().any(|byte| *byte != 0) || db[separator] != 0x01 {
        return Err(Trouble::BadPadding);
    }
    let salt = &db[separator + 1..];

    // H' = SHA-256(eight zeros ‖ SHA-256(message) ‖ salt).
    let digest = sha256::digest(message);
    let mut inner = nexus_crypto::sha256::Sha256::new();
    inner.update(&[0u8; 8]);
    inner.update(&digest);
    inner.update(salt);
    let expected = inner.finish();

    if sha256::same(&expected, hash) {
        Ok(())
    } else {
        Err(Trouble::Mismatch)
    }
}

/// MGF1 with SHA-256, RFC 8017 appendix B.2.1.
fn mgf1(seed: &[u8], length: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(length + 32);
    let mut counter = 0u32;
    while out.len() < length {
        let mut hash = nexus_crypto::sha256::Sha256::new();
        hash.update(seed);
        hash.update(&counter.to_be_bytes());
        out.extend_from_slice(&hash.finish());
        counter += 1;
    }
    out.truncate(length);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn small_modular_exponentiation_agrees_with_arithmetic() {
        // 3^7 mod 11 = 2187 mod 11 = 9. Checked against a number a person can
        // do in their head, because the machinery underneath is not.
        let out = power_mod(&[3], &[7], &[11]).unwrap();
        assert_eq!(out, vec![9]);

        // 2^10 = 1024, and 1024 mod 1001 = 23. The modulus is odd because
        // Montgomery needs it to be -- 1000 would be refused, which the test
        // below checks separately.
        let out = power_mod(&[2], &[10], &[0x03, 0xE9]).unwrap();
        assert_eq!(out, vec![0x00, 23]);
    }

    #[test]
    fn a_known_rsa_pair() {
        // The textbook tiny key: n = 3233 = 61 × 53, e = 17, d = 413.
        // 65^17 mod 3233 = 2790, and 2790^413 mod 3233 = 65.
        let n = [0x0C, 0xA1];
        let cipher = power_mod(&[65], &[17], &n).unwrap();
        assert_eq!(cipher, vec![0x0A, 0xE6], "65^17 mod 3233 should be 2790");
        let plain = power_mod(&cipher, &[0x01, 0x9D], &n).unwrap();
        assert_eq!(plain, vec![0x00, 0x41], "and back to 65");
    }

    #[test]
    fn an_even_modulus_is_refused_rather_than_answered_wrongly() {
        // Montgomery needs an odd modulus. An even RSA modulus would mean a
        // factor of two, which is to say a key that is not a key -- but the
        // arithmetic would produce *something*, and something is worse than an
        // error here.
        assert_eq!(
            power_mod(&[3], &[5], &[10]).unwrap_err(),
            Trouble::BadKey("an even modulus")
        );
    }

    #[test]
    fn a_value_at_least_as_large_as_the_modulus_is_refused() {
        // RFC 8017 §5.2.2 calls it out of range. An implementation that reduced
        // it first would accept many signatures for each valid one.
        assert!(power_mod(&[11], &[3], &[11]).is_err());
        assert!(power_mod(&[12], &[3], &[11]).is_err());
        assert!(power_mod(&[10], &[3], &[11]).is_ok());
    }

    #[test]
    fn an_exponent_of_zero_is_refused() {
        // The answer would be 1, which would make every signature verify
        // against a key whose exponent somebody chose.
        assert_eq!(
            power_mod(&[5], &[0], &[11]).unwrap_err(),
            Trouble::BadKey("an exponent of zero")
        );
    }

    #[test]
    fn limbs_and_bytes_round_trip() {
        for bytes in [
            vec![0x01u8],
            vec![0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF],
            vec![0x12, 0x34, 0x56, 0x78, 0x9A, 0xBC, 0xDE, 0xF0, 0x11],
            vec![0xAB; 256],
        ] {
            let limbs = bytes.len().div_ceil(8);
            let number = from_bytes(&bytes, limbs);
            assert_eq!(to_bytes(&number, bytes.len()), bytes);
        }
    }

    #[test]
    fn the_inverse_of_the_low_limb_is_what_montgomery_needs() {
        for odd in [1u64, 3, 5, 0xFFFF_FFFF_FFFF_FFFF, 0x1234_5678_9ABC_DEF1] {
            let inverse = inverse_of_low_limb(&[odd]);
            // n × (−n⁻¹) ≡ −1 ≡ 2⁶⁴ − 1 mod 2⁶⁴.
            assert_eq!(
                odd.wrapping_mul(inverse),
                u64::MAX,
                "the inverse of {odd:#x} is wrong"
            );
        }
    }

    #[test]
    fn doubling_stays_inside_the_modulus() {
        let modulus = [11u64];
        let mut value = [1u64];
        // 1, 2, 4, 8, 16 mod 11 = 5, 10, 20 mod 11 = 9.
        for expected in [2u64, 4, 8, 5, 10, 9] {
            double_mod(&mut value, &modulus);
            assert_eq!(value[0], expected);
        }
    }

    #[test]
    fn a_real_signature_verifies_and_a_changed_message_does_not() {
        // Made by OpenSSL: the key, the message and the signature are all from
        // outside this project, which is the only way to know the padding and
        // the exponentiation agree with everybody else's.
        let (modulus, exponent) = test_key();
        let message = include_bytes!("../fixtures/signed.txt");
        let signature = include_bytes!("../fixtures/signed.pkcs1");

        verify_pkcs1(&modulus, &exponent, signature, message, false)
            .expect("a real signature should verify");

        // And the same signature over anything else must not.
        let mut changed = message.to_vec();
        changed[0] ^= 1;
        assert_eq!(
            verify_pkcs1(&modulus, &exponent, signature, &changed, false).unwrap_err(),
            Trouble::Mismatch
        );
    }

    #[test]
    fn every_single_bit_flipped_in_a_real_signature_is_refused() {
        // The test that would catch a verifier which parses the padded block
        // instead of comparing it -- Bleichenbacher's 2006 forgery, which
        // works against implementations that look *inside* the padding for a
        // hash rather than requiring the whole block to be right.
        let (modulus, exponent) = test_key();
        let message = include_bytes!("../fixtures/signed.txt");
        let signature = include_bytes!("../fixtures/signed.pkcs1");

        for at in 0..signature.len() {
            for bit in 0..8u8 {
                let mut broken = signature.to_vec();
                broken[at] ^= 1 << bit;
                assert!(
                    verify_pkcs1(&modulus, &exponent, &broken, message, false).is_err(),
                    "bit {bit} of byte {at} was flipped and the signature still verified"
                );
            }
        }
    }

    #[test]
    fn a_real_sha384_signature_verifies() {
        // RSA-SHA384 certificates exist and are not rare, and this used to be a
        // refusal with a note saying so. OpenSSL made this signature and
        // verifies it itself.
        let (modulus, exponent) = test_key();
        let message = include_bytes!("../fixtures/signed.txt");
        let signature = include_bytes!("../fixtures/signed384.pkcs1");

        verify_pkcs1(&modulus, &exponent, signature, message, true)
            .expect("a real SHA-384 signature should verify");

        // And it must not verify as SHA-256, nor the SHA-256 one as SHA-384:
        // the hash is named in the padding, so confusing them is a verifier
        // accepting a signature over a different digest.
        assert!(verify_pkcs1(&modulus, &exponent, signature, message, false).is_err());
        let sha256_signature = include_bytes!("../fixtures/signed.pkcs1");
        assert!(verify_pkcs1(&modulus, &exponent, sha256_signature, message, true).is_err());
    }

    #[test]
    fn a_real_pss_signature_verifies_and_a_changed_one_does_not() {
        let (modulus, exponent) = test_key();
        let message = include_bytes!("../fixtures/signed.txt");
        let signature = include_bytes!("../fixtures/signed.pss");

        verify_pss(&modulus, &exponent, signature, message)
            .expect("a real PSS signature should verify");

        let mut changed = message.to_vec();
        changed[1] ^= 0x80;
        assert!(verify_pss(&modulus, &exponent, signature, &changed).is_err());
    }

    #[test]
    fn a_pss_signature_does_not_verify_as_pkcs1_or_the_other_way_round() {
        // The two schemes use the same key and the same hash, and a verifier
        // that confused them would accept a signature made for one purpose as
        // though it were the other.
        let (modulus, exponent) = test_key();
        let message = include_bytes!("../fixtures/signed.txt");
        let pkcs1 = include_bytes!("../fixtures/signed.pkcs1");
        let pss = include_bytes!("../fixtures/signed.pss");

        assert!(verify_pss(&modulus, &exponent, pkcs1, message).is_err());
        assert!(verify_pkcs1(&modulus, &exponent, pss, message, false).is_err());
    }

    #[test]
    fn a_signature_of_the_wrong_length_is_refused_before_any_arithmetic() {
        let (modulus, exponent) = test_key();
        assert_eq!(
            verify_pkcs1(&modulus, &exponent, &[0u8; 128], b"x", false).unwrap_err(),
            Trouble::WrongLength
        );
    }

    /// The modulus and exponent of the key the fixtures were signed with.
    fn test_key() -> (alloc::vec::Vec<u8>, alloc::vec::Vec<u8>) {
        // Taken out of the certificate rather than written down separately, so
        // there is one copy of the key and no chance of them disagreeing.
        let der_bytes = include_bytes!("../fixtures/rsa-leaf.der");
        let certificate = crate::x509::parse(der_bytes).unwrap();
        match certificate.key {
            crate::x509::PublicKey::Rsa { modulus, exponent } => (modulus, exponent),
            other => panic!("expected an RSA key, got {other:?}"),
        }
    }
}
