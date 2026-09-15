//! ECDSA over NIST P-256, verifying only.
//!
//! Most leaf certificates issued today carry a P-256 key even when the
//! authority above them signs with RSA, so a client with only RSA verifies
//! most of a chain and then fails at the bottom.
//!
//! # Where the code went
//!
//! The arithmetic is in [`crate::ec`], generic over the number of limbs,
//! because P-384 needs every line of it with `4` changed to `6`. What is left
//! here is the curve's constants and the choice of hash — which is all that was
//! ever specific to P-256.
//!
//! The constants below are from FIPS 186-4 and are worth checking against that
//! rather than against another implementation.

use nexus_crypto::sha256;

use crate::ec::{self, Curve};

pub use crate::ec::Trouble;

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

const P0: u64 = ec::inverse_of_low_limb(P[0]);
const N0: u64 = ec::inverse_of_low_limb(N[0]);

/// P-256, ready to verify with.
#[must_use]
pub fn curve() -> Curve<4> {
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

/// Verify an ECDSA signature made over P-256.
///
/// `point` is the uncompressed public key from the certificate: `0x04` then x
/// and y. `signature` is the DER `SEQUENCE { INTEGER r, INTEGER s }` that TLS
/// and X.509 both carry.
///
/// # Errors
///
/// [`Trouble`], every variant of which means the signature must be rejected.
pub fn verify(point: &[u8], signature: &[u8], message: &[u8], sha384: bool) -> Result<(), Trouble> {
    // Chosen here rather than in `ec`, because which hash a signature used is a
    // fact about the algorithm identifier that named it, not about the curve.
    if sha384 {
        let digest = nexus_crypto::sha512::digest_384(message);
        ec::verify(&curve(), point, signature, &digest)
    } else {
        let digest = sha256::digest(message);
        ec::verify(&curve(), point, signature, &digest)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ec::{add_mixed, affine_x, double, on_curve, Field, Point};

    #[test]
    fn the_base_point_is_on_the_curve() {
        // The first thing to check: if this fails, every formula below is
        // being tested against the wrong curve.
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
        let g = Point::affine(field.enter(&GX), field.enter(&GY), &field);
        let doubled = double(&field, &g);
        let x = affine_x(&field, &doubled).unwrap();
        assert_eq!(
            x,
            [
                0xA60B_48FC_4766_9978,
                0xC089_69E2_77F2_1B35,
                0x8A52_3803_04B5_1AC3,
                0x7CF2_7B18_8D03_4F7E,
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
        let g = Point::affine(gx, gy, &field);
        let added = add_mixed(&field, &g, &gx, &gy);
        let doubled = double(&field, &g);
        assert_eq!(affine_x(&field, &added), affine_x(&field, &doubled));
    }

    #[test]
    fn a_point_plus_its_negation_is_the_identity() {
        let field = Field::new(P, P0);
        let gx = field.enter(&GX);
        let gy = field.enter(&GY);
        let g = Point::affine(gx, gy, &field);
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
