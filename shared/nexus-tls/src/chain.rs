//! Deciding whether to believe a certificate chain.
//!
//! This is the file the whole crate exists for. Everything else — the record
//! layer, the key schedule, the arithmetic — protects a conversation with
//! *somebody*. This is what decides whether that somebody is who they said.
//!
//! # What must all be true
//!
//! A chain is believed only when every one of these holds:
//!
//! 1. the leaf covers the host name that was asked for;
//! 2. every certificate is within its validity period **now**;
//! 3. each certificate's signature verifies under the next one's key;
//! 4. every certificate above the leaf is a certificate authority, by its own
//!    `basicConstraints`, and its `keyUsage` allows signing certificates if it
//!    says anything at all;
//! 5. no authority's path length is exceeded;
//! 6. the top of the chain is signed by a root this machine already trusts.
//!
//! Any one failing fails the connection. There is no partial success, no
//! warning to click through and no way for a caller to ask for the chain
//! anyway — a function that returned "not verified, but here it is" would be a
//! function somebody eventually uses.
//!
//! # The server's chain is a hint, not the truth
//!
//! A server sends what it thinks the chain is. Nothing here trusts that order:
//! the leaf is the first certificate, and every link after it is found by
//! *looking* for a certificate whose subject matches the issuer, among the
//! ones sent and then among the roots. A server that sends them shuffled, or
//! sends extra ones, or sends its own fake root, gets the same answer.
//!
//! # No revocation
//!
//! There is no CRL fetching and no OCSP. Both need a network request in the
//! middle of establishing a connection, and OCSP stapling needs the extension
//! parsed and the response verified. It is absent and named here, rather than
//! left for somebody to assume.

use alloc::string::String;
use alloc::vec::Vec;

use crate::x509::{self, Algorithm, Certificate, PublicKey};

/// How long a chain may be, including the leaf and the root.
///
/// Real chains are three or four. Ten is generous and bounds the search, which
/// matters because the certificates being searched came from the peer.
const LONGEST: usize = 10;

/// Why a chain was not believed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Trouble {
    /// A certificate would not parse.
    Unreadable(String),
    /// The leaf does not cover the name that was asked for.
    WrongName { wanted: String, covers: Vec<String> },
    /// A certificate is not valid yet, or is no longer.
    Expired {
        subject: String,
        not_after: i64,
        now: i64,
    },
    NotYetValid {
        subject: String,
        not_before: i64,
        now: i64,
    },
    /// A signature did not verify.
    BadSignature(String),
    /// Nothing that could have issued a certificate was found.
    NoIssuer(String),
    /// A certificate in the middle of the chain is not an authority.
    NotAnAuthority(String),
    /// An authority's path length does not allow the chain below it.
    TooLong(String),
    /// The chain does not reach a root this machine trusts.
    Untrusted,
    /// The machine has no idea what time it is, so validity cannot be judged.
    ///
    /// Refused rather than skipped. A client that ignored dates when it did not
    /// know the time would accept every expired certificate ever issued on a
    /// machine whose clock had not been set.
    NoClock,
}

impl core::fmt::Display for Trouble {
    fn fmt(&self, out: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Unreadable(why) => write!(out, "a certificate could not be read: {why}"),
            Self::WrongName { wanted, covers } => write!(
                out,
                "the certificate is for {}, not for {wanted}",
                if covers.is_empty() {
                    String::from("no name at all")
                } else {
                    covers.join(", ")
                }
            ),
            Self::Expired { subject, .. } => {
                write!(out, "the certificate for {subject} has expired")
            }
            Self::NotYetValid { subject, .. } => {
                write!(out, "the certificate for {subject} is not valid yet")
            }
            Self::BadSignature(who) => write!(out, "the signature on {who} does not verify"),
            Self::NoIssuer(who) => write!(out, "nothing here could have issued {who}"),
            Self::NotAnAuthority(who) => write!(
                out,
                "{who} signed another certificate and is not a certificate authority"
            ),
            Self::TooLong(who) => {
                write!(out, "{who} does not allow a chain this long below it")
            }
            Self::Untrusted => out
                .write_str("the chain does not reach a certificate authority this machine trusts"),
            Self::NoClock => out.write_str(
                "this machine does not know what time it is, so it cannot judge whether \
                 a certificate has expired",
            ),
        }
    }
}

/// The certificates this machine trusts to be at the top of a chain.
#[derive(Debug)]
pub struct Roots {
    certificates: Vec<Certificate>,
}

impl Roots {
    /// A store with nothing in it, which trusts nothing.
    #[must_use]
    pub fn empty() -> Self {
        Self {
            certificates: Vec::new(),
        }
    }

    /// Add a root from its DER.
    ///
    /// Returns whether it was usable. A root that will not parse is skipped
    /// rather than failing the whole store: a bundle with one bad certificate
    /// in it should cost that one certificate.
    pub fn add(&mut self, der: &[u8]) -> bool {
        match x509::parse(der) {
            Ok(certificate) => {
                self.certificates.push(certificate);
                true
            }
            Err(_) => false,
        }
    }

    /// How many roots are held.
    #[must_use]
    pub fn len(&self) -> usize {
        self.certificates.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.certificates.is_empty()
    }

    /// Every root whose subject is `name`.
    fn issuers_named<'a>(&'a self, name: &'a [u8]) -> impl Iterator<Item = &'a Certificate> {
        self.certificates
            .iter()
            .filter(move |root| root.subject == name)
    }
}

/// Check that `chain` proves the server is `host`, at `now`.
///
/// `chain` is what the server sent, leaf first by convention -- though only the
/// first being the leaf is relied on.
///
/// `now` is seconds since the Unix epoch, or `None` on a machine whose clock
/// has never been set, which is refused rather than waved through.
pub fn verify(
    chain: &[Vec<u8>],
    roots: &Roots,
    host: &str,
    now: Option<i64>,
) -> Result<(), Trouble> {
    let Some(now) = now else {
        return Err(Trouble::NoClock);
    };
    if chain.is_empty() {
        return Err(Trouble::NoIssuer(String::from("an empty chain")));
    }

    let sent: Vec<Certificate> = {
        let mut parsed = Vec::with_capacity(chain.len());
        for der in chain {
            match x509::parse(der) {
                Ok(certificate) => parsed.push(certificate),
                Err(why) => return Err(Trouble::Unreadable(alloc::format!("{why}"))),
            }
        }
        parsed
    };
    let leaf = &sent[0];

    // The name first, because it is the check a person can reason about and
    // the one most likely to fail for an ordinary reason.
    if !x509::covers(&leaf.names, host) {
        return Err(Trouble::WrongName {
            wanted: String::from(host),
            covers: leaf.names.clone(),
        });
    }

    let mut current = leaf;
    let mut depth = 0usize;

    loop {
        within_validity(current, now)?;

        // A self-issued certificate that is not a trusted root is the end of
        // the line: following it would loop forever.
        let mut found = None;

        // Roots first. A server that sends a certificate with the same subject
        // as a real root must not be able to displace it -- the root is the one
        // this machine already decided to trust, and the one sent is just bytes
        // that arrived.
        for candidate in roots.issuers_named(&current.issuer) {
            if signature_verifies(current, candidate).is_ok() {
                within_validity(candidate, now)?;
                may_issue(candidate, depth)?;
                return Ok(());
            }
        }

        // Then the ones the server sent, skipping the certificate itself so a
        // self-signed one does not issue itself.
        for candidate in &sent {
            if candidate.subject != current.issuer {
                continue;
            }
            if candidate.subject == current.subject && candidate.tbs == current.tbs {
                continue;
            }
            if signature_verifies(current, candidate).is_ok() {
                found = Some(candidate);
                break;
            }
        }

        let Some(issuer) = found else {
            // Nothing signed it. Either the chain is broken or it ends at a
            // root this machine does not have -- and the second is the common
            // case, so it is worth its own message.
            return Err(if roots.is_empty() {
                Trouble::Untrusted
            } else {
                Trouble::NoIssuer(name_of(current))
            });
        };

        may_issue(issuer, depth)?;
        depth += 1;
        if depth >= LONGEST {
            return Err(Trouble::TooLong(name_of(issuer)));
        }
        current = issuer;
    }
}

/// Whether a certificate is inside its validity period.
fn within_validity(certificate: &Certificate, now: i64) -> Result<(), Trouble> {
    if now < certificate.not_before {
        return Err(Trouble::NotYetValid {
            subject: name_of(certificate),
            not_before: certificate.not_before,
            now,
        });
    }
    if now > certificate.not_after {
        return Err(Trouble::Expired {
            subject: name_of(certificate),
            not_after: certificate.not_after,
            now,
        });
    }
    Ok(())
}

/// Whether a certificate may have issued one `below` links down.
fn may_issue(issuer: &Certificate, below: usize) -> Result<(), Trouble> {
    if !issuer.is_authority {
        return Err(Trouble::NotAnAuthority(name_of(issuer)));
    }
    // `keyUsage` is optional; when it is there and says no, it means no.
    if issuer.may_sign_certificates == Some(false) {
        return Err(Trouble::NotAnAuthority(name_of(issuer)));
    }
    if let Some(limit) = issuer.path_length {
        // RFC 5280 §4.2.1.9: the limit counts the intermediates below this
        // certificate, not counting the leaf.
        if below as u32 > limit {
            return Err(Trouble::TooLong(name_of(issuer)));
        }
    }
    Ok(())
}

/// Whether `certificate`'s signature verifies under `issuer`'s key.
pub fn signature_verifies(certificate: &Certificate, issuer: &Certificate) -> Result<(), Trouble> {
    let bad = || Trouble::BadSignature(name_of(certificate));

    match (&certificate.algorithm, &issuer.key) {
        (Algorithm::RsaPkcs1Sha256, PublicKey::Rsa { modulus, exponent }) => {
            crate::rsa::verify_pkcs1(
                modulus,
                exponent,
                &certificate.signature,
                &certificate.tbs,
                crate::Hash::Sha256,
            )
            .map_err(|_| bad())
        }
        (Algorithm::RsaPkcs1Sha384, PublicKey::Rsa { modulus, exponent }) => {
            crate::rsa::verify_pkcs1(
                modulus,
                exponent,
                &certificate.signature,
                &certificate.tbs,
                crate::Hash::Sha384,
            )
            .map_err(|_| bad())
        }
        (Algorithm::RsaPss, PublicKey::Rsa { modulus, exponent }) => {
            crate::rsa::verify_pss(modulus, exponent, &certificate.signature, &certificate.tbs)
                .map_err(|_| bad())
        }
        (Algorithm::RsaPkcs1Sha512, PublicKey::Rsa { modulus, exponent }) => {
            crate::rsa::verify_pkcs1(
                modulus,
                exponent,
                &certificate.signature,
                &certificate.tbs,
                crate::Hash::Sha512,
            )
            .map_err(|_| bad())
        }
        // ECDSA. The algorithm says which hash and the key says which curve,
        // which is why these are two independent matches rather than five
        // named pairs.
        (Algorithm::EcdsaSha256, PublicKey::P256 { point }) => {
            crate::p256::verify(point, &certificate.signature, &certificate.tbs, false)
                .map_err(|_| bad())
        }
        (Algorithm::EcdsaSha384, PublicKey::P256 { point }) => {
            crate::p256::verify(point, &certificate.signature, &certificate.tbs, true)
                .map_err(|_| bad())
        }
        (algorithm, PublicKey::P384 { point }) => {
            let hash = match algorithm {
                Algorithm::EcdsaSha256 => crate::p384::Hash::Sha256,
                Algorithm::EcdsaSha384 => crate::p384::Hash::Sha384,
                Algorithm::EcdsaSha512 => crate::p384::Hash::Sha512,
                // An RSA signature cannot be checked with a P-384 key.
                _ => return Err(bad()),
            };
            crate::p384::verify(point, &certificate.signature, &certificate.tbs, hash)
                .map_err(|_| bad())
        }
        // A signature algorithm that does not match the key type. Not a
        // mismatch to work around: an RSA signature cannot be checked with an
        // elliptic-curve key, and a chain that tried is one somebody built.
        _ => Err(bad()),
    }
}

/// Something to call a certificate in a message.
///
/// The first DNS name it covers, or its serial-less subject as hex if it has
/// none. Not the common name: reading it means parsing the distinguished name,
/// and this is for a sentence in a log rather than for a decision.
fn name_of(certificate: &Certificate) -> String {
    if let Some(name) = certificate.names.first() {
        return name.clone();
    }
    use core::fmt::Write as _;
    let mut text = String::from("a certificate (");
    for byte in certificate.subject.iter().rev().take(6).rev() {
        let _ = write!(text, "{byte:02x}");
    }
    text.push(')');
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The moment the fixtures were made, plus a day: inside their validity.
    fn now() -> i64 {
        // The fixture certificates were issued in September 2026 for ten years.
        // A fixed moment rather than a real clock, so this does not start
        // failing in 2036 for a reason that has nothing to do with the code.
        1_790_000_000
    }

    fn rsa_leaf() -> Vec<u8> {
        include_bytes!("../fixtures/rsa-leaf.der").to_vec()
    }

    fn ecdsa_leaf() -> Vec<u8> {
        include_bytes!("../fixtures/ecdsa-leaf.der").to_vec()
    }

    #[test]
    fn a_self_signed_certificate_in_the_root_store_is_believed() {
        // The simplest chain there is: one certificate, which is its own
        // issuer and which this machine has been told to trust.
        let mut roots = Roots::empty();
        assert!(roots.add(&rsa_leaf()));
        assert_eq!(roots.len(), 1);

        verify(&[rsa_leaf()], &roots, "example.test", Some(now()))
            .expect("a trusted self-signed certificate should verify");
        // And the wildcard it carries.
        verify(&[rsa_leaf()], &roots, "www.example.test", Some(now())).unwrap();
    }

    #[test]
    fn the_same_certificate_is_not_believed_without_the_root() {
        // The whole point. Identical bytes, and the only difference is whether
        // this machine was told to trust the top of the chain.
        let roots = Roots::empty();
        assert_eq!(
            verify(&[rsa_leaf()], &roots, "example.test", Some(now())).unwrap_err(),
            Trouble::Untrusted
        );
    }

    #[test]
    fn a_server_cannot_supply_its_own_root() {
        // A server that sends a self-signed certificate and hopes it counts.
        // The root store is the only thing that makes a chain trusted, and a
        // certificate arriving over the wire is never in it.
        let mut roots = Roots::empty();
        // A root for something else entirely.
        assert!(roots.add(&ecdsa_leaf()));

        assert!(verify(&[rsa_leaf()], &roots, "example.test", Some(now())).is_err());
    }

    #[test]
    fn the_wrong_host_name_is_refused_before_anything_else() {
        let mut roots = Roots::empty();
        assert!(roots.add(&rsa_leaf()));

        match verify(&[rsa_leaf()], &roots, "example.com", Some(now())).unwrap_err() {
            Trouble::WrongName { wanted, covers } => {
                assert_eq!(wanted, "example.com");
                assert!(covers.contains(&String::from("example.test")));
            }
            other => panic!("expected WrongName, got {other:?}"),
        }
    }

    #[test]
    fn a_certificate_outside_its_validity_is_refused() {
        let mut roots = Roots::empty();
        assert!(roots.add(&rsa_leaf()));

        // Long before it was issued.
        assert!(matches!(
            verify(&[rsa_leaf()], &roots, "example.test", Some(0)).unwrap_err(),
            Trouble::NotYetValid { .. }
        ));
        // And long after it expires: 2050.
        assert!(matches!(
            verify(&[rsa_leaf()], &roots, "example.test", Some(2_524_608_000)).unwrap_err(),
            Trouble::Expired { .. }
        ));
    }

    #[test]
    fn a_machine_with_no_clock_refuses_rather_than_skipping_the_dates() {
        // A client that ignored dates when it did not know the time would
        // accept every expired certificate ever issued.
        let mut roots = Roots::empty();
        assert!(roots.add(&rsa_leaf()));
        assert_eq!(
            verify(&[rsa_leaf()], &roots, "example.test", None).unwrap_err(),
            Trouble::NoClock
        );
    }

    #[test]
    fn a_certificate_with_a_byte_changed_does_not_verify() {
        // The signature covers the TBS, so a flipped byte either breaks the
        // parse or breaks the signature. Both are refusals; neither is a
        // successful verification.
        let original = rsa_leaf();
        let mut roots = Roots::empty();
        assert!(roots.add(&original));

        let mut broken = original.clone();
        // Into the middle of the TBS, past the header and the serial.
        broken[60] ^= 0x01;
        assert!(verify(&[broken], &roots, "example.test", Some(now())).is_err());
    }

    #[test]
    fn an_ecdsa_certificate_verifies_by_the_same_route() {
        let mut roots = Roots::empty();
        assert!(roots.add(&ecdsa_leaf()));
        verify(&[ecdsa_leaf()], &roots, "ecdsa.test", Some(now()))
            .expect("a trusted ECDSA certificate should verify");
    }

    #[test]
    fn an_empty_chain_is_refused() {
        let roots = Roots::empty();
        assert!(verify(&[], &roots, "example.test", Some(now())).is_err());
    }

    #[test]
    fn a_chain_of_rubbish_is_refused_rather_than_panicking() {
        let mut roots = Roots::empty();
        assert!(roots.add(&rsa_leaf()));
        for rubbish in [
            alloc::vec![0u8],
            alloc::vec![0x30, 0x82, 0xFF, 0xFF],
            b"not a certificate at all".to_vec(),
        ] {
            assert!(verify(&[rubbish], &roots, "example.test", Some(now())).is_err());
        }
    }

    #[test]
    fn a_root_that_will_not_parse_is_skipped_rather_than_poisoning_the_store() {
        // A bundle with one bad certificate in it should cost that one
        // certificate, not the whole store.
        let mut roots = Roots::empty();
        assert!(!roots.add(b"rubbish"));
        assert!(roots.is_empty());
        assert!(roots.add(&rsa_leaf()));
        assert_eq!(roots.len(), 1);
    }

    #[test]
    fn an_rsa_signature_is_not_checked_with_an_elliptic_curve_key() {
        // A chain that tried is one somebody built. The types must line up,
        // and a mismatch is a refusal rather than something to work around.
        let rsa = x509::parse(&rsa_leaf()).unwrap();
        let ecdsa = x509::parse(&ecdsa_leaf()).unwrap();
        assert!(signature_verifies(&rsa, &ecdsa).is_err());
        assert!(signature_verifies(&ecdsa, &rsa).is_err());
        // And each under its own key, which is what self-signed means.
        assert!(signature_verifies(&rsa, &rsa).is_ok());
        assert!(signature_verifies(&ecdsa, &ecdsa).is_ok());
    }
}
