//! Certificates: RFC 5280, as much of it as verifying a chain needs.
//!
//! # What is read, and what is deliberately ignored
//!
//! A certificate has a great many fields. This reads the ones a decision rests
//! on — who issued it, who it is for, when it is valid, what key it carries,
//! whether it may sign others, and which names it covers — and ignores the
//! rest.
//!
//! Ignoring is not the same as accepting. A **critical** extension this does
//! not understand makes the certificate unusable, which is what RFC 5280 §4.2
//! requires and is the whole point of the critical flag: an authority marking
//! something critical is saying "refuse this rather than misunderstand it".
//!
//! # Names are compared as bytes
//!
//! RFC 5280 defines a normalisation for distinguished names — case folding,
//! whitespace collapsing, per-attribute rules — and getting it wrong in the
//! lenient direction means two different names comparing equal, which is a
//! chain that links where it should not.
//!
//! So issuer and subject are compared as their **encoded bytes**. That is
//! stricter than the specification: two names that ought to match but are
//! encoded differently will not. In practice a certificate's issuer field is
//! copied byte for byte from its issuer's subject field, precisely so that this
//! works — and being too strict means a chain that fails to build, which is
//! visible, rather than one that builds wrongly, which is not.

use alloc::string::String;
use alloc::vec::Vec;

use crate::der::{self, tag, Reader};

/// Object identifiers, as their encoded bytes.
///
/// Compared as bytes rather than parsed into numbers: it is exact, it needs no
/// allocation, and there is nothing to get wrong.
pub mod oid {
    /// sha256WithRSAEncryption, 1.2.840.113549.1.1.11.
    pub const RSA_SHA256: &[u8] = &[0x2A, 0x86, 0x48, 0x86, 0xF7, 0x0D, 0x01, 0x01, 0x0B];
    /// sha384WithRSAEncryption, 1.2.840.113549.1.1.12.
    pub const RSA_SHA384: &[u8] = &[0x2A, 0x86, 0x48, 0x86, 0xF7, 0x0D, 0x01, 0x01, 0x0C];
    /// rsassa-pss, 1.2.840.113549.1.1.10.
    pub const RSA_PSS: &[u8] = &[0x2A, 0x86, 0x48, 0x86, 0xF7, 0x0D, 0x01, 0x01, 0x0A];
    /// ecdsa-with-SHA256, 1.2.840.10045.4.3.2.
    pub const ECDSA_SHA256: &[u8] = &[0x2A, 0x86, 0x48, 0xCE, 0x3D, 0x04, 0x03, 0x02];
    /// ecdsa-with-SHA384, 1.2.840.10045.4.3.3.
    pub const ECDSA_SHA384: &[u8] = &[0x2A, 0x86, 0x48, 0xCE, 0x3D, 0x04, 0x03, 0x03];

    /// rsaEncryption, 1.2.840.113549.1.1.1 -- the key type.
    pub const RSA_KEY: &[u8] = &[0x2A, 0x86, 0x48, 0x86, 0xF7, 0x0D, 0x01, 0x01, 0x01];
    /// id-ecPublicKey, 1.2.840.10045.2.1.
    pub const EC_KEY: &[u8] = &[0x2A, 0x86, 0x48, 0xCE, 0x3D, 0x02, 0x01];
    /// prime256v1 (P-256), 1.2.840.10045.3.1.7.
    pub const P256: &[u8] = &[0x2A, 0x86, 0x48, 0xCE, 0x3D, 0x03, 0x01, 0x07];

    /// id-ce-subjectAltName, 2.5.29.17.
    pub const SUBJECT_ALT_NAME: &[u8] = &[0x55, 0x1D, 0x11];
    /// id-ce-basicConstraints, 2.5.29.19.
    pub const BASIC_CONSTRAINTS: &[u8] = &[0x55, 0x1D, 0x13];
    /// id-ce-keyUsage, 2.5.29.15.
    pub const KEY_USAGE: &[u8] = &[0x55, 0x1D, 0x0F];
    /// id-ce-extKeyUsage, 2.5.29.37.
    pub const EXT_KEY_USAGE: &[u8] = &[0x55, 0x1D, 0x25];
    /// id-at-commonName, 2.5.4.3.
    pub const COMMON_NAME: &[u8] = &[0x55, 0x04, 0x03];
}

/// Why a certificate could not be used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Trouble {
    /// The DER would not read.
    Encoding(der::Trouble),
    /// A field is there but says something impossible.
    Malformed(&'static str),
    /// A critical extension this does not understand.
    ///
    /// Refused rather than ignored: an authority marking an extension critical
    /// is saying "refuse this rather than misunderstand it", and that is the
    /// only useful reading of the flag.
    UnknownCritical(String),
    /// An algorithm this cannot verify.
    Unsupported(String),
}

impl From<der::Trouble> for Trouble {
    fn from(trouble: der::Trouble) -> Self {
        Self::Encoding(trouble)
    }
}

impl core::fmt::Display for Trouble {
    fn fmt(&self, out: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Encoding(why) => write!(out, "{why}"),
            Self::Malformed(what) => write!(out, "the certificate is malformed: {what}"),
            Self::UnknownCritical(oid) => write!(
                out,
                "the certificate has a critical extension this does not understand ({oid}), \
                 so it is refused rather than misread"
            ),
            Self::Unsupported(what) => write!(out, "this cannot verify {what}"),
        }
    }
}

/// A moment, as the seconds since the Unix epoch.
///
/// Certificates carry two spellings of time and both become this, so that
/// comparing them is comparing numbers.
pub type Moment = i64;

/// What key a certificate carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PublicKey {
    /// An RSA key: the modulus and the exponent, big-endian and without
    /// leading zeros.
    Rsa { modulus: Vec<u8>, exponent: Vec<u8> },
    /// A P-256 key, as the uncompressed point the certificate carries: a `0x04`
    /// byte then x and y, thirty-two bytes each.
    P256 { point: Vec<u8> },
}

/// How a signature was made.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Algorithm {
    RsaPkcs1Sha256,
    RsaPkcs1Sha384,
    RsaPss,
    EcdsaP256Sha256,
    EcdsaP256Sha384,
}

/// Everything a decision about a certificate rests on.
#[derive(Debug, Clone)]
pub struct Certificate {
    /// The encoded `TBSCertificate`, which is what the signature is over.
    ///
    /// Kept as the bytes that arrived rather than re-encoded from the parsed
    /// fields: re-encoding would not reproduce them, and a signature over
    /// slightly different bytes is a signature that does not verify.
    pub tbs: Vec<u8>,
    /// How the issuer signed it.
    pub algorithm: Algorithm,
    pub signature: Vec<u8>,
    /// The issuer's and this certificate's names, as their encoded bytes.
    pub issuer: Vec<u8>,
    pub subject: Vec<u8>,
    pub not_before: Moment,
    pub not_after: Moment,
    pub key: PublicKey,
    /// Whether it may sign other certificates, and how far below it.
    pub is_authority: bool,
    pub path_length: Option<u32>,
    /// The DNS names it covers, from the subject alternative name extension.
    pub names: Vec<String>,
    /// Whether `keyUsage` is present and allows signing certificates.
    ///
    /// `None` when the extension is absent, which RFC 5280 reads as "no
    /// restriction".
    pub may_sign_certificates: Option<bool>,
}

/// Parse a certificate.
pub fn parse(der_bytes: &[u8]) -> Result<Certificate, Trouble> {
    let mut outer = Reader::new(der_bytes);
    let mut certificate = outer.nested(tag::SEQUENCE)?;

    let tbs_value = certificate.expect(tag::SEQUENCE)?;
    let algorithm = read_algorithm(&mut certificate)?;
    let signature = der::bit_string(&certificate.expect(tag::BIT_STRING)?)?.to_vec();

    let mut tbs = Reader::new(tbs_value.body);

    // version [0] EXPLICIT, defaulting to v1. A certificate without it is v1,
    // which has no extensions -- and a v1 certificate presented as a CA today
    // is refused elsewhere for having no basicConstraints.
    let _version = tbs.nested_optional(tag::context(0))?;
    let _serial = tbs.expect(tag::INTEGER)?;

    // The algorithm appears twice, and the two must agree. A certificate where
    // they differ is one where the outer field says how to verify and the inner
    // field is what was signed -- and an attacker who could make them differ
    // could have a signature checked one way and read another.
    let inner_algorithm = read_algorithm(&mut tbs)?;
    if inner_algorithm != algorithm {
        return Err(Trouble::Malformed(
            "two different signature algorithms in one certificate",
        ));
    }

    let issuer = tbs.expect(tag::SEQUENCE)?.whole.to_vec();

    let mut validity = tbs.nested(tag::SEQUENCE)?;
    let not_before = read_time(&mut validity)?;
    let not_after = read_time(&mut validity)?;
    if not_after < not_before {
        return Err(Trouble::Malformed(
            "a validity period that ends before it starts",
        ));
    }

    let subject = tbs.expect(tag::SEQUENCE)?.whole.to_vec();
    let spki = tbs.expect(tag::SEQUENCE)?;
    let key = read_key(spki.body)?;

    // The two unique identifiers, which nothing has used since 1988 and which
    // still have to be stepped over to reach the extensions.
    let _ = tbs.optional(tag::context_primitive(1))?;
    let _ = tbs.optional(tag::context_primitive(2))?;

    let mut is_authority = false;
    let mut path_length = None;
    let mut names = Vec::new();
    let mut may_sign_certificates = None;

    if let Some(mut wrapper) = tbs.nested_optional(tag::context(3))? {
        let mut extensions = wrapper.nested(tag::SEQUENCE)?;
        while !extensions.done() {
            let mut extension = extensions.nested(tag::SEQUENCE)?;
            let id = extension.expect(tag::OID)?.body;
            let critical = match extension.optional(tag::BOOLEAN)? {
                Some(value) => value.body.first().copied().unwrap_or(0) != 0,
                None => false,
            };
            let body = extension.expect(tag::OCTET_STRING)?.body;

            match id {
                oid::BASIC_CONSTRAINTS => {
                    let (authority, limit) = read_basic_constraints(body)?;
                    is_authority = authority;
                    path_length = limit;
                }
                oid::SUBJECT_ALT_NAME => names = read_alt_names(body)?,
                oid::KEY_USAGE => may_sign_certificates = Some(read_key_usage(body)?),
                // Understood well enough to ignore: this client does not check
                // extended key usage, certificate policies, CRL distribution
                // points, authority information access or key identifiers.
                // Named here so that "ignored" is a decision rather than an
                // oversight.
                oid::EXT_KEY_USAGE => {}
                _ if critical => {
                    return Err(Trouble::UnknownCritical(der::oid_text(id)));
                }
                _ => {}
            }
        }
    }

    Ok(Certificate {
        tbs: tbs_value.whole.to_vec(),
        algorithm,
        signature,
        issuer,
        subject,
        not_before,
        not_after,
        key,
        is_authority,
        path_length,
        names,
        may_sign_certificates,
    })
}

/// An `AlgorithmIdentifier`, as something this can act on.
fn read_algorithm(reader: &mut Reader<'_>) -> Result<Algorithm, Trouble> {
    let mut sequence = reader.nested(tag::SEQUENCE)?;
    let id = sequence.expect(tag::OID)?.body;
    Ok(match id {
        oid::RSA_SHA256 => Algorithm::RsaPkcs1Sha256,
        oid::RSA_SHA384 => Algorithm::RsaPkcs1Sha384,
        oid::RSA_PSS => Algorithm::RsaPss,
        oid::ECDSA_SHA256 => Algorithm::EcdsaP256Sha256,
        oid::ECDSA_SHA384 => Algorithm::EcdsaP256Sha384,
        other => {
            return Err(Trouble::Unsupported(alloc::format!(
                "signatures of type {}",
                der::oid_text(other)
            )))
        }
    })
}

/// A `SubjectPublicKeyInfo`'s contents.
fn read_key(body: &[u8]) -> Result<PublicKey, Trouble> {
    let mut reader = Reader::new(body);
    let mut algorithm = reader.nested(tag::SEQUENCE)?;
    let id = algorithm.expect(tag::OID)?.body;
    let bits = der::bit_string(&reader.expect(tag::BIT_STRING)?)?;

    match id {
        oid::RSA_KEY => {
            let mut key = Reader::new(bits);
            let mut sequence = key.nested(tag::SEQUENCE)?;
            let modulus = der::positive_integer(&sequence.expect(tag::INTEGER)?)?.to_vec();
            let exponent = der::positive_integer(&sequence.expect(tag::INTEGER)?)?.to_vec();
            // A modulus shorter than 1024 bits is one nobody should still be
            // trusting, and one longer than 8192 is a denial of service dressed
            // as a key: the cost of verifying grows with the cube of its size.
            if modulus.len() < 128 || modulus.len() > 1024 {
                return Err(Trouble::Malformed("an RSA modulus of an unreasonable size"));
            }
            if exponent.is_empty() || exponent.len() > 8 {
                return Err(Trouble::Malformed(
                    "an RSA exponent of an unreasonable size",
                ));
            }
            Ok(PublicKey::Rsa { modulus, exponent })
        }
        oid::EC_KEY => {
            // The curve is a parameter of the algorithm, and the only one this
            // handles is P-256.
            let curve = algorithm.expect(tag::OID)?.body;
            if curve != oid::P256 {
                return Err(Trouble::Unsupported(alloc::format!(
                    "the elliptic curve {}",
                    der::oid_text(curve)
                )));
            }
            // Uncompressed only. The compressed form needs a square root in the
            // field to recover y, and every certificate authority issues the
            // uncompressed form.
            if bits.len() != 65 || bits[0] != 0x04 {
                return Err(Trouble::Malformed("a P-256 point that is not uncompressed"));
            }
            Ok(PublicKey::P256 {
                point: bits.to_vec(),
            })
        }
        other => Err(Trouble::Unsupported(alloc::format!(
            "public keys of type {}",
            der::oid_text(other)
        ))),
    }
}

/// `basicConstraints`: whether this may sign others, and how far below it.
fn read_basic_constraints(body: &[u8]) -> Result<(bool, Option<u32>), Trouble> {
    let mut reader = Reader::new(body);
    let mut sequence = reader.nested(tag::SEQUENCE)?;
    let authority = match sequence.optional(tag::BOOLEAN)? {
        Some(value) => value.body.first().copied().unwrap_or(0) != 0,
        None => false,
    };
    let limit = match sequence.optional(tag::INTEGER)? {
        Some(value) => {
            let bytes = der::positive_integer(&value)?;
            // A path length longer than four bytes is a number no chain needs.
            if bytes.len() > 4 {
                return Err(Trouble::Malformed("an absurd path length"));
            }
            let mut length = 0u32;
            for byte in bytes {
                length = length * 256 + u32::from(*byte);
            }
            Some(length)
        }
        None => None,
    };
    Ok((authority, limit))
}

/// `keyUsage`: whether bit 5, `keyCertSign`, is set.
fn read_key_usage(body: &[u8]) -> Result<bool, Trouble> {
    let mut reader = Reader::new(body);
    let value = reader.expect(tag::BIT_STRING)?;
    // Not `der::bit_string`: a keyUsage legitimately does not end on a byte,
    // because it is a handful of named bits.
    let Some((unused, bits)) = value.body.split_first() else {
        return Err(Trouble::Malformed("an empty key usage"));
    };
    if *unused > 7 {
        return Err(Trouble::Malformed(
            "a bit string with more than a byte unused",
        ));
    }
    // Bit 5 counting from the most significant bit of the first byte, which is
    // how DER numbers the bits of a named bit list.
    let Some(first) = bits.first() else {
        return Ok(false);
    };
    Ok(first & (1 << 2) != 0)
}

/// `subjectAltName`, keeping only the DNS names.
fn read_alt_names(body: &[u8]) -> Result<Vec<String>, Trouble> {
    let mut reader = Reader::new(body);
    let mut sequence = reader.nested(tag::SEQUENCE)?;
    let mut names = Vec::new();
    while !sequence.done() {
        let value = sequence.any()?;
        // dNSName is [2] IMPLICIT IA5String. Everything else -- email
        // addresses, IP addresses, URIs, directory names -- is skipped, because
        // this client only ever matches a host name.
        if value.tag == tag::context_primitive(2) {
            if let Ok(text) = core::str::from_utf8(value.body) {
                names.push(text.to_ascii_lowercase());
            }
        }
    }
    Ok(names)
}

/// A `UTCTime` or a `GeneralizedTime`, as seconds since the epoch.
fn read_time(reader: &mut Reader<'_>) -> Result<Moment, Trouble> {
    let value = reader.any()?;
    let bytes = value.body;

    let (year, rest) = match value.tag {
        tag::UTC_TIME => {
            // Two digits, and RFC 5280 §4.1.2.5.1 says 50 and above is 19xx.
            if bytes.len() < 13 {
                return Err(Trouble::Malformed("a UTCTime that is too short"));
            }
            let two = number(&bytes[0..2])?;
            (if two >= 50 { 1900 + two } else { 2000 + two }, &bytes[2..])
        }
        tag::GENERALIZED_TIME => {
            if bytes.len() < 15 {
                return Err(Trouble::Malformed("a GeneralizedTime that is too short"));
            }
            (number(&bytes[0..4])?, &bytes[4..])
        }
        _ => return Err(Trouble::Malformed("a validity field that is not a time")),
    };

    // Both spellings must end in Z. RFC 5280 requires it, and a local time in a
    // certificate is one whose meaning depends on where it is read.
    if rest.last() != Some(&b'Z') {
        return Err(Trouble::Malformed("a time that is not in UTC"));
    }
    if rest.len() < 11 {
        return Err(Trouble::Malformed("a time with no seconds"));
    }

    let month = number(&rest[0..2])?;
    let day = number(&rest[2..4])?;
    let hour = number(&rest[4..6])?;
    let minute = number(&rest[6..8])?;
    let second = number(&rest[8..10])?;

    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return Err(Trouble::Malformed("a date that is not one"));
    }
    if hour > 23 || minute > 59 || second > 60 {
        return Err(Trouble::Malformed("a time that is not one"));
    }

    Ok(days_from_civil(year, month, day) * 86_400
        + i64::from(hour) * 3600
        + i64::from(minute) * 60
        + i64::from(second))
}

/// An unsigned number from ASCII digits.
fn number(bytes: &[u8]) -> Result<u32, Trouble> {
    let mut value = 0u32;
    for byte in bytes {
        if !byte.is_ascii_digit() {
            return Err(Trouble::Malformed(
                "a date with something other than a digit in it",
            ));
        }
        value = value * 10 + u32::from(byte - b'0');
    }
    Ok(value)
}

/// Days from 1970-01-01 to a civil date. Howard Hinnant's algorithm.
fn days_from_civil(year: u32, month: u32, day: u32) -> i64 {
    let year = i64::from(year) - i64::from(month <= 2);
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let month = i64::from(month);
    let shifted = if month > 2 { month - 3 } else { month + 9 };
    let day_of_year = (153 * shifted + 2) / 5 + i64::from(day) - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// Whether a host name is covered by one of a certificate's names.
///
/// Wildcards are allowed in the leftmost label only, and only as the *whole*
/// label. `*.example.com` covers `a.example.com` and does **not** cover
/// `example.com` or `a.b.example.com`, which is what RFC 6125 §6.4.3 says and
/// is stricter than what some implementations do.
#[must_use]
pub fn covers(names: &[String], host: &str) -> bool {
    let host = host.to_ascii_lowercase();
    names.iter().any(|name| matches_one(name, &host))
}

fn matches_one(name: &str, host: &str) -> bool {
    let Some(rest) = name.strip_prefix("*.") else {
        return name == host;
    };
    // A wildcard that is not the whole label -- `*a.example.com` -- is refused
    // rather than matched partially. Partial wildcards are legal in some
    // readings and have been used to make `*.com`-shaped certificates look
    // narrower than they are.
    if rest.is_empty() || rest.contains('*') {
        return false;
    }
    // It covers exactly one label, so the rest of the host must equal the rest
    // of the name and the part before must have no dot in it.
    let Some(at) = host.find('.') else {
        return false;
    };
    let (label, tail) = host.split_at(at);
    !label.is_empty() && &tail[1..] == rest
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::der::write;
    use alloc::vec;

    #[test]
    fn a_real_rsa_certificate_parses() {
        // Made by OpenSSL, which is the point: everything else in this file is
        // this project's idea of X.509 checked against itself, and this is
        // X.509 as somebody else writes it.
        let der_bytes = include_bytes!("../fixtures/rsa-leaf.der");
        let certificate = parse(der_bytes).expect("a real certificate should parse");

        assert_eq!(certificate.algorithm, Algorithm::RsaPkcs1Sha256);
        assert!(
            matches!(certificate.key, PublicKey::Rsa { ref modulus, .. } if modulus.len() == 256)
        );

        // Self-signed, so the issuer and the subject are the same bytes -- and
        // being the *same bytes* is what chain building relies on.
        assert_eq!(certificate.issuer, certificate.subject);

        // The names from the subjectAltName, including the wildcard.
        assert!(certificate.names.contains(&String::from("example.test")));
        assert!(certificate.names.contains(&String::from("*.example.test")));
        assert!(covers(&certificate.names, "example.test"));
        assert!(covers(&certificate.names, "www.example.test"));
        assert!(!covers(&certificate.names, "example.com"));

        // Ten years, as it was asked for. Checked as a span rather than against
        // fixed dates, so this does not start failing in 2036.
        let span = certificate.not_after - certificate.not_before;
        assert!(
            (3640 * 86_400..=3660 * 86_400).contains(&span),
            "the validity is {span} seconds, which is not about ten years"
        );

        // A self-signed certificate from `openssl req -x509` is a CA.
        assert!(certificate.is_authority);

        // And the TBS bytes are the ones that arrived, tag and all -- which is
        // what the signature is over and what re-encoding would not reproduce.
        assert_eq!(certificate.tbs[0], tag::SEQUENCE);
        assert!(
            der_bytes
                .windows(certificate.tbs.len())
                .any(|window| window == certificate.tbs),
            "the TBS bytes should be a run of the certificate as it arrived"
        );
    }

    #[test]
    fn a_real_ecdsa_certificate_parses() {
        let der_bytes = include_bytes!("../fixtures/ecdsa-leaf.der");
        let certificate = parse(der_bytes).expect("a real certificate should parse");

        assert_eq!(certificate.algorithm, Algorithm::EcdsaP256Sha256);
        match &certificate.key {
            PublicKey::P256 { point } => {
                assert_eq!(point.len(), 65);
                assert_eq!(point[0], 0x04, "uncompressed");
            }
            other => panic!("expected a P-256 key, got {other:?}"),
        }
        assert!(covers(&certificate.names, "ecdsa.test"));
    }

    #[test]
    fn every_truncation_of_a_real_certificate_is_refused_rather_than_panicking() {
        // The test that says this is safe on the input it exists to read. A
        // certificate arrives from whoever answered the connection, before
        // anything has been verified.
        let der_bytes = include_bytes!("../fixtures/rsa-leaf.der");
        for cut in 0..der_bytes.len() {
            let _ = parse(&der_bytes[..cut]);
        }
    }

    #[test]
    fn a_real_certificate_with_a_byte_changed_is_refused_or_differs() {
        // Not a signature check -- that comes later -- but a parser check: a
        // certificate with a byte flipped either fails to parse or parses to
        // something different. What it must never do is parse to the *same*
        // thing, which would mean a byte nobody is looking at.
        let der_bytes = include_bytes!("../fixtures/rsa-leaf.der");
        let original = parse(der_bytes).unwrap();

        let mut differed = 0;
        for at in 0..der_bytes.len() {
            let mut broken = der_bytes.to_vec();
            broken[at] ^= 0x01;
            match parse(&broken) {
                Err(_) => differed += 1,
                Ok(other) => {
                    if other.tbs != original.tbs
                        || other.signature != original.signature
                        || other.subject != original.subject
                    {
                        differed += 1;
                    }
                }
            }
        }
        // Most bytes of a certificate are inside the TBS or the signature, so
        // nearly every flip should show up somewhere.
        assert!(
            differed > der_bytes.len() * 9 / 10,
            "only {differed} of {} flipped bytes changed anything",
            der_bytes.len()
        );
    }

    #[test]
    fn a_wildcard_covers_one_label_and_no_more() {
        let names = vec![String::from("*.example.com")];
        assert!(covers(&names, "a.example.com"));
        assert!(covers(&names, "www.example.com"));
        assert!(covers(&names, "WWW.EXAMPLE.COM"), "matching ignores case");

        // Not the bare domain: RFC 6125 §6.4.3.
        assert!(!covers(&names, "example.com"));
        // And not two labels down.
        assert!(!covers(&names, "a.b.example.com"));
        // And not a different domain.
        assert!(!covers(&names, "a.example.org"));
    }

    #[test]
    fn a_partial_wildcard_covers_nothing() {
        // Legal in some readings, and used to make certificates look narrower
        // than they are.
        for name in ["*a.example.com", "a*.example.com", "*.*.com", "*."] {
            let names = vec![String::from(name)];
            assert!(!covers(&names, "aa.example.com"), "{name}");
            assert!(!covers(&names, "a.b.com"), "{name}");
        }
    }

    #[test]
    fn an_exact_name_covers_itself_and_nothing_else() {
        let names = vec![String::from("example.com")];
        assert!(covers(&names, "example.com"));
        assert!(!covers(&names, "www.example.com"));
        assert!(!covers(&names, "notexample.com"));
        assert!(!covers(&names, "example.com.evil.test"));
    }

    #[test]
    fn a_certificate_with_no_names_covers_nothing() {
        assert!(!covers(&[], "example.com"));
    }

    #[test]
    fn the_epoch_and_some_known_dates() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(days_from_civil(1970, 1, 2), 1);
        assert_eq!(days_from_civil(2000, 3, 1), 11017);
        assert_eq!(days_from_civil(2024, 2, 29), 19782);
    }

    /// A UTCTime or GeneralizedTime value, for the tests below.
    fn a_time(tag_byte: u8, text: &str) -> Vec<u8> {
        write(tag_byte, text.as_bytes())
    }

    #[test]
    fn both_spellings_of_time_read_as_the_same_moment() {
        let utc = a_time(tag::UTC_TIME, "240229120000Z");
        let general = a_time(tag::GENERALIZED_TIME, "20240229120000Z");
        let one = read_time(&mut Reader::new(&utc)).unwrap();
        let other = read_time(&mut Reader::new(&general)).unwrap();
        assert_eq!(one, other);
        assert_eq!(one, days_from_civil(2024, 2, 29) * 86_400 + 12 * 3600);
    }

    #[test]
    fn a_two_digit_year_of_fifty_or_more_is_last_century() {
        // RFC 5280 §4.1.2.5.1. Getting this backwards makes a certificate
        // issued in 1999 look valid until 2099.
        let old = a_time(tag::UTC_TIME, "990101000000Z");
        let new = a_time(tag::UTC_TIME, "490101000000Z");
        assert_eq!(
            read_time(&mut Reader::new(&old)).unwrap(),
            days_from_civil(1999, 1, 1) * 86_400
        );
        assert_eq!(
            read_time(&mut Reader::new(&new)).unwrap(),
            days_from_civil(2049, 1, 1) * 86_400
        );
    }

    #[test]
    fn a_time_that_is_not_in_utc_is_refused() {
        // A local time in a certificate is one whose meaning depends on where
        // it is read, and RFC 5280 forbids it.
        let local = a_time(tag::UTC_TIME, "240229120000+0100");
        assert!(read_time(&mut Reader::new(&local)).is_err());
    }

    #[test]
    fn a_date_that_is_not_one_is_refused() {
        for text in [
            "241329120000Z", // month 13
            "240032120000Z", // day 0 and month 0
            "2402291200ooZ", // letters
            "24022912Z",     // no seconds
        ] {
            let bytes = a_time(tag::UTC_TIME, text);
            assert!(read_time(&mut Reader::new(&bytes)).is_err(), "{text}");
        }
    }

    #[test]
    fn key_usage_reads_the_certificate_signing_bit() {
        // keyCertSign is bit 5, which in DER's numbering is 1 << 2 of the first
        // byte. A certificate with only digitalSignature (bit 0) must not be
        // read as one that may sign others.
        let sign_certificates = write(tag::BIT_STRING, &[1, 0b0000_0100]);
        assert!(read_key_usage(&sign_certificates).unwrap());

        let digital_signature = write(tag::BIT_STRING, &[7, 0b1000_0000]);
        assert!(!read_key_usage(&digital_signature).unwrap());
    }

    #[test]
    fn basic_constraints_say_whether_a_certificate_may_sign_others() {
        // An empty SEQUENCE means cA is false, which is the default and the
        // reason a certificate without the extension cannot be an authority.
        let empty = write(tag::SEQUENCE, &[]);
        assert_eq!(read_basic_constraints(&empty).unwrap(), (false, None));

        let mut body = write(tag::BOOLEAN, &[0xFF]);
        body.extend_from_slice(&write(tag::INTEGER, &[2]));
        let authority = write(tag::SEQUENCE, &body);
        assert_eq!(read_basic_constraints(&authority).unwrap(), (true, Some(2)));
    }

    #[test]
    fn subject_alternative_names_keep_only_the_dns_ones() {
        let mut body = write(tag::context_primitive(2), b"example.com");
        // An email address, [1], which is skipped.
        body.extend_from_slice(&write(tag::context_primitive(1), b"nobody@example.com"));
        body.extend_from_slice(&write(tag::context_primitive(2), b"WWW.Example.COM"));
        // An IP address, [7].
        body.extend_from_slice(&write(tag::context_primitive(7), &[10, 0, 0, 1]));
        let extension = write(tag::SEQUENCE, &body);

        let names = read_alt_names(&extension).unwrap();
        assert_eq!(names, vec!["example.com", "www.example.com"]);
    }

    #[test]
    fn an_rsa_key_of_an_unreasonable_size_is_refused() {
        // A tiny modulus is one nobody should trust; an enormous one is a
        // denial of service dressed as a key, because verifying costs the cube
        // of its size.
        let build = |modulus_bytes: usize| {
            let mut numbers = write(tag::INTEGER, &vec![0x01; modulus_bytes]);
            numbers.extend_from_slice(&write(tag::INTEGER, &[0x01, 0x00, 0x01]));
            let key = write(tag::SEQUENCE, &numbers);
            let mut bits = vec![0u8];
            bits.extend_from_slice(&key);

            let mut spki = write(tag::SEQUENCE, &write(tag::OID, oid::RSA_KEY));
            spki.extend_from_slice(&write(tag::BIT_STRING, &bits));
            spki
        };

        assert!(read_key(&build(64)).is_err(), "512 bits is too small");
        assert!(read_key(&build(2048)).is_err(), "16384 bits is too large");
        assert!(read_key(&build(256)).is_ok(), "2048 bits is ordinary");
    }

    #[test]
    fn a_p256_point_that_is_not_uncompressed_is_refused() {
        let build = |bits: Vec<u8>| {
            let mut algorithm = write(tag::OID, oid::EC_KEY);
            algorithm.extend_from_slice(&write(tag::OID, oid::P256));
            let mut spki = write(tag::SEQUENCE, &algorithm);
            let mut body = vec![0u8];
            body.extend_from_slice(&bits);
            spki.extend_from_slice(&write(tag::BIT_STRING, &body));
            spki
        };

        // Compressed: 0x02 or 0x03 and thirty-two bytes. Legal ASN.1 and not
        // something this recovers y from.
        let mut compressed = vec![0x02];
        compressed.extend_from_slice(&[0x11; 32]);
        assert!(read_key(&build(compressed)).is_err());

        let mut uncompressed = vec![0x04];
        uncompressed.extend_from_slice(&[0x22; 64]);
        assert!(read_key(&build(uncompressed)).is_ok());
    }

    #[test]
    fn a_curve_this_does_not_know_is_named_rather_than_guessed() {
        let mut algorithm = write(tag::OID, oid::EC_KEY);
        // secp384r1, 1.3.132.0.34.
        algorithm.extend_from_slice(&write(tag::OID, &[0x2B, 0x81, 0x04, 0x00, 0x22]));
        let mut spki = write(tag::SEQUENCE, &algorithm);
        let mut body = vec![0u8, 0x04];
        body.extend_from_slice(&[0x33; 96]);
        spki.extend_from_slice(&write(tag::BIT_STRING, &body));

        match read_key(&spki) {
            Err(Trouble::Unsupported(what)) => {
                assert!(what.contains("1.3.132.0.34"), "{what}");
            }
            other => panic!("expected an Unsupported naming the curve, got {other:?}"),
        }
    }
}
