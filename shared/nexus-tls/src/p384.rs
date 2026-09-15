//! ECDSA over NIST P-384, verifying only.
//!
//! # Why this exists
//!
//! Because of a number. `tools/nexus-roots` builds the machine's root store by
//! parsing Mozilla's list with this crate's own X.509 parser and shipping what
//! it can use, and before this file existed it said:
//!
//! ```text
//! 121 read, 76 kept, 45 dropped
//!   37 x  this cannot verify the elliptic curve 1.3.132.0.34
//! ```
//!
//! `1.3.132.0.34` is secp384r1. Thirty-seven of the world's certificate
//! authorities were unusable on this machine for one missing curve -- not
//! because anything was insecure, but because the arithmetic was two limbs too
//! short.
//!
//! # What is here
//!
//! The constants, and nothing else. [`crate::ec`] holds every line of the
//! arithmetic, generic over the limb count, and the whole of P-384 is six-limb
//! numbers passed to it. The P-256 test suite -- including OpenSSL's own
//! signatures -- passes unchanged against that shared code, which is what makes
//! it reasonable to trust it with a second curve.
//!
//! The constants come from FIPS 186-4 and were turned into limbs by a script
//! rather than by hand, then checked: [`the_base_point_is_on_the_curve`] is the
//! test that catches a mistranscribed digit, because a wrong constant makes a
//! point that is not on the curve.
//!
//! [`the_base_point_is_on_the_curve`]: tests::the_base_point_is_on_the_curve

use crate::ec::{self, Curve};

pub use crate::ec::Trouble;

/// A field element or a scalar: 384 bits, little-endian limbs.
type Number = [u64; 6];

/// The field's prime: 2³⁸⁴ − 2¹²⁸ − 2⁹⁶ + 2³² − 1.
const P: Number = [
    0x0000_0000_FFFF_FFFF,
    0xFFFF_FFFF_0000_0000,
    0xFFFF_FFFF_FFFF_FFFE,
    0xFFFF_FFFF_FFFF_FFFF,
    0xFFFF_FFFF_FFFF_FFFF,
    0xFFFF_FFFF_FFFF_FFFF,
];

/// The order of the base point.
const N: Number = [
    0xECEC_196A_CCC5_2973,
    0x581A_0DB2_48B0_A77A,
    0xC763_4D81_F437_2DDF,
    0xFFFF_FFFF_FFFF_FFFF,
    0xFFFF_FFFF_FFFF_FFFF,
    0xFFFF_FFFF_FFFF_FFFF,
];

/// The curve's `b`. `a` is −3, as on P-256, so the same doubling formula works.
const B: Number = [
    0x2A85_C8ED_D3EC_2AEF,
    0xC656_398D_8A2E_D19D,
    0x0314_088F_5013_875A,
    0x181D_9C6E_FE81_4112,
    0x988E_056B_E3F8_2D19,
    0xB331_2FA7_E23E_E7E4,
];

/// The base point's x, and its y.
const GX: Number = [
    0x3A54_5E38_7276_0AB7,
    0x5502_F25D_BF55_296C,
    0x59F7_41E0_8254_2A38,
    0x6E1D_3B62_8BA7_9B98,
    0x8EB1_C71E_F320_AD74,
    0xAA87_CA22_BE8B_0537,
];
const GY: Number = [
    0x7A43_1D7C_90EA_0E5F,
    0x0A60_B1CE_1D7E_819D,
    0xE9DA_3113_B5F0_B8C0,
    0xF8F4_1DBD_289A_147C,
    0x5D9E_98BF_9292_DC29,
    0x3617_DE4A_9626_2C6F,
];

const P0: u64 = ec::inverse_of_low_limb(P[0]);
const N0: u64 = ec::inverse_of_low_limb(N[0]);

/// P-384, ready to verify with.
#[must_use]
pub fn curve() -> Curve<6> {
    Curve {
        p: P,
        n: N,
        b: B,
        gx: GX,
        gy: GY,
        p0: P0,
        n0: N0,
    }
}

pub use crate::Hash;

/// Verify an ECDSA signature made over P-384.
///
/// `point` is the uncompressed public key from the certificate: `0x04` then x
/// and y, forty-eight bytes each. `signature` is the DER
/// `SEQUENCE { INTEGER r, INTEGER s }`.
///
/// # Errors
///
/// [`Trouble`], every variant of which means the signature must be rejected.
pub fn verify(
    point: &[u8],
    signature: &[u8],
    message: &[u8],
    hash: Hash,
) -> Result<(), Trouble> {
    match hash {
        Hash::Sha256 => {
            // Shorter than the order, so it is used whole. FIPS 186-4 truncates
            // only when the hash is longer.
            let digest = nexus_crypto::sha256::digest(message);
            ec::verify(&curve(), point, signature, &digest)
        }
        Hash::Sha384 => {
            let digest = nexus_crypto::sha512::digest_384(message);
            ec::verify(&curve(), point, signature, &digest)
        }
        Hash::Sha512 => {
            // Longer than the order; `ec::verify` takes the leftmost 48 bytes.
            let digest = nexus_crypto::sha512::digest(message);
            ec::verify(&curve(), point, signature, &digest)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ec::{add_mixed, affine_x, double, on_curve, Field, Point};

    #[test]
    fn the_base_point_is_on_the_curve() {
        // The test that catches a mistranscribed digit. Every constant above
        // feeds into this one equation, and a wrong limb anywhere in P, B, GX
        // or GY makes it fail.
        let field = Field::new(P, P0);
        let x = field.enter(&GX);
        let y = field.enter(&GY);
        assert!(on_curve(&field, &curve(), &x, &y));
    }

    #[test]
    fn a_point_not_on_the_curve_is_seen_to_be_off_it() {
        let field = Field::new(P, P0);
        let x = field.enter(&GX);
        let mut wrong = GY;
        wrong[0] ^= 1;
        assert!(!on_curve(&field, &curve(), &x, &field.enter(&wrong)));
    }

    #[test]
    fn montgomery_form_round_trips_at_six_limbs() {
        // The reduction loop in `ec::montgomery` indexes by `L`, and an
        // off-by-one there would show up here before anywhere else.
        let field = Field::new(P, P0);
        for value in [[1u64, 0, 0, 0, 0, 0], GX, GY, B, N] {
            assert_eq!(field.leave(&field.enter(&value)), value);
        }
    }

    #[test]
    fn multiplication_agrees_with_small_arithmetic() {
        let field = Field::new(P, P0);
        let three = field.enter(&[3, 0, 0, 0, 0, 0]);
        let five = field.enter(&[5, 0, 0, 0, 0, 0]);
        assert_eq!(
            field.leave(&field.multiply(&three, &five)),
            [15, 0, 0, 0, 0, 0]
        );
        assert_eq!(field.leave(&field.square(&five)), [25, 0, 0, 0, 0, 0]);
    }

    #[test]
    fn inverses_multiply_to_one() {
        for (modulus, n0) in [(P, P0), (N, N0)] {
            let field = Field::new(modulus, n0);
            for value in [[2u64, 0, 0, 0, 0, 0], [7, 0, 0, 0, 0, 0], GX] {
                let entered = field.enter(&value);
                let inverse = field.invert(&entered);
                assert_eq!(
                    field.leave(&field.multiply(&entered, &inverse)),
                    [1, 0, 0, 0, 0, 0],
                    "the inverse of {value:?} is wrong"
                );
            }
        }
    }

    #[test]
    fn doubling_the_base_point_lands_where_it_should() {
        // 2G's x, computed from the specification's own base point by plain
        // affine arithmetic -- not by this code, so the two disagree if the
        // Jacobian formula is wrong.
        let field = Field::new(P, P0);
        let g = Point::affine(field.enter(&GX), field.enter(&GY), &field);
        let doubled = double(&field, &g);
        let x = affine_x(&field, &doubled).unwrap();
        assert_eq!(
            x,
            [
                0x5B96_A9C7_5295_DF61,
                0x4FE0_E86E_BE0E_64F8,
                0x51D2_07D1_9FB9_6E9E,
                0x8902_5959_A6F4_34D6,
                0x6926_0045_C55B_97F0,
                0x08D9_9905_7BA3_D2D9,
            ],
            "2G is not where it should be"
        );
    }

    #[test]
    fn adding_a_point_to_itself_is_doubling_it() {
        let field = Field::new(P, P0);
        let gx = field.enter(&GX);
        let gy = field.enter(&GY);
        let g = Point::affine(gx, gy, &field);
        assert_eq!(
            affine_x(&field, &add_mixed(&field, &g, &gx, &gy)),
            affine_x(&field, &double(&field, &g))
        );
    }

    #[test]
    fn a_point_plus_its_negation_is_the_identity() {
        let field = Field::new(P, P0);
        let gx = field.enter(&GX);
        let gy = field.enter(&GY);
        let g = Point::affine(gx, gy, &field);
        let negated = field.subtract(&[0; 6], &gy);
        let sum = add_mixed(&field, &g, &gx, &negated);
        assert!(sum.is_infinity());
        assert_eq!(affine_x(&field, &sum), None);
    }

    #[test]
    fn a_real_signature_verifies_and_a_changed_message_does_not() {
        // OpenSSL made the key and the signature, and verified the signature
        // itself before it was committed. This is the whole of P-384 ECDSA
        // checked against somebody else's implementation.
        let point = key_point();
        let message = include_bytes!("../fixtures/signed.txt");
        let signature = include_bytes!("../fixtures/signed.p384");

        verify(&point, signature, message, Hash::Sha384).expect("a real signature should verify");

        let mut changed = message.to_vec();
        changed[0] ^= 1;
        assert_eq!(
            verify(&point, signature, &changed, Hash::Sha384).unwrap_err(),
            Trouble::Mismatch
        );
    }

    #[test]
    fn the_same_signature_under_the_wrong_hash_does_not_verify() {
        // The algorithm identifier says which hash was used, and a verifier
        // that ignored it would accept a signature made over a different
        // digest of the same bytes.
        let point = key_point();
        let message = include_bytes!("../fixtures/signed.txt");
        let signature = include_bytes!("../fixtures/signed.p384");
        for wrong in [Hash::Sha256, Hash::Sha512] {
            assert_eq!(
                verify(&point, signature, message, wrong).unwrap_err(),
                Trouble::Mismatch,
                "{wrong:?} should not verify a SHA-384 signature"
            );
        }
    }

    #[test]
    fn a_key_that_is_not_on_the_curve_is_refused() {
        // The invalid-curve attack, at six limbs.
        let mut point = key_point();
        point[60] ^= 0x01;
        let message = include_bytes!("../fixtures/signed.txt");
        let signature = include_bytes!("../fixtures/signed.p384");
        assert_eq!(
            verify(&point, signature, message, Hash::Sha384).unwrap_err(),
            Trouble::BadKey
        );
    }

    #[test]
    fn a_p256_key_is_not_a_p384_key() {
        // Sixty-five bytes where ninety-seven are wanted. Refused on length
        // before any arithmetic touches it.
        let der_bytes = include_bytes!("../fixtures/ecdsa-leaf.der");
        let certificate = crate::x509::parse(der_bytes).unwrap();
        let crate::x509::PublicKey::P256 { point } = certificate.key else {
            panic!("the fixture should carry a P-256 key");
        };
        assert_eq!(
            verify(&point, b"", b"", Hash::Sha384).unwrap_err(),
            Trouble::BadKey
        );
    }

    #[test]
    fn a_signature_with_r_or_s_out_of_range_is_refused() {
        use crate::der::{tag, write};
        let point = key_point();
        let message = include_bytes!("../fixtures/signed.txt");

        for bad in [alloc::vec![0u8], {
            let mut bytes = alloc::vec![0u8];
            bytes.extend_from_slice(&to_big_endian(&N));
            bytes
        }] {
            let mut body = write(tag::INTEGER, &bad);
            body.extend_from_slice(&write(tag::INTEGER, &[1]));
            let signature = write(tag::SEQUENCE, &body);
            assert_eq!(
                verify(&point, &signature, message, Hash::Sha384).unwrap_err(),
                Trouble::BadSignature,
                "r = {bad:?} should be refused"
            );
        }
    }

    #[test]
    fn every_byte_of_a_real_signature_matters() {
        let point = key_point();
        let message = include_bytes!("../fixtures/signed.txt");
        let signature: alloc::vec::Vec<u8> = include_bytes!("../fixtures/signed.p384").to_vec();

        for at in 0..signature.len() {
            let mut broken = signature.clone();
            broken[at] ^= 0x01;
            assert!(
                verify(&point, &broken, message, Hash::Sha384).is_err(),
                "byte {at} was changed and the signature still verified"
            );
        }
    }

    fn to_big_endian(number: &Number) -> [u8; 48] {
        let mut bytes = [0u8; 48];
        for index in 0..48 {
            bytes[47 - index] = (number[index / 8] >> ((index % 8) * 8)) as u8;
        }
        bytes
    }

    /// The public key from the P-384 certificate fixture.
    fn key_point() -> alloc::vec::Vec<u8> {
        let der_bytes = include_bytes!("../fixtures/p384-ca.der");
        let certificate = crate::x509::parse(der_bytes).expect("the fixture should parse");
        match certificate.key {
            crate::x509::PublicKey::P384 { point } => point,
            other => panic!("expected a P-384 key, got {other:?}"),
        }
    }
}
