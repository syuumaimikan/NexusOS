//! The file the machine's trust comes out of.
//!
//! A root store is a list of certificates this machine will believe without
//! being shown anything else. It has to come from somewhere, and on a machine
//! with no package manager and no network at first boot, that somewhere is a
//! file on the disk image.
//!
//! # Why a bundle rather than a directory of files
//!
//! A directory would need no format at all, which is a real argument for it.
//! Against it: a hundred and twenty small files on this machine's store is a
//! hundred and twenty directory entries to walk and open at the moment somebody
//! is waiting for a page, and a directory that is *partly* there reads as a
//! smaller root store rather than as a broken one.
//!
//! That second point is the whole reason this format has a digest in it. A
//! truncated bundle would otherwise load as a store with fewer roots in it, and
//! the symptom would be perfectly good sites failing to verify -- which looks
//! like a network fault, or like a certificate problem at the far end, and is
//! neither. So the file is checked whole before any of it is believed, and a
//! damaged store is an error with a sentence on it.
//!
//! # The format
//!
//! | offset | | |
//! | --- | --- | --- |
//! | 0 | 8 bytes | `NXROOTS\n` |
//! | 8 | `u32` | version, 1 |
//! | 12 | `u32` | how many certificates |
//! | 16 | 32 bytes | SHA-256 of everything after this field |
//! | 48 | | the certificates |
//!
//! and each certificate is a `u32` length and that many bytes of DER. Numbers
//! are little-endian, like everywhere else here.
//!
//! There is no compression, no index and no ordering requirement. The store is
//! read once at start-up and searched by subject name, and an index over a
//! hundred entries would cost more to maintain than it saves.
//!
//! # What is not in it
//!
//! No revocation, which is the honest gap. A root that is withdrawn stays
//! trusted here until the bundle is rebuilt. Revocation is a real piece of work
//! -- CRLs or OCSP, and a policy for what to do when neither is reachable --
//! and pretending to have it would be worse than saying where the edge is.

extern crate alloc;

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;

use crate::chain::Roots;

/// What every bundle starts with.
pub const MAGIC: [u8; 8] = *b"NXROOTS\n";

/// The version this reads and writes.
pub const VERSION: u32 = 1;

/// Where the certificates start.
pub const HEADER: usize = 48;

/// The largest certificate this will read.
///
/// A root is a few kilobytes. The bound is here so that a corrupt length field
/// asks for a plausible amount of memory rather than four gigabytes of it.
pub const MAX_CERTIFICATE: usize = 64 * 1024;

/// The most roots one bundle may hold.
///
/// Mozilla's list is around a hundred and twenty. A thousand is room to grow
/// and still a refusal rather than an allocation if the count field is wrong.
pub const MAX_ROOTS: usize = 1024;

/// Why a bundle could not be read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Trouble {
    /// It does not begin like a bundle.
    NotABundle,
    /// A version this does not know.
    Version(u32),
    /// The file is shorter than it says it is.
    Truncated,
    /// The digest does not match, so something has changed underneath.
    Damaged,
    /// It claims more roots than this will hold.
    TooMany(usize),
}

impl core::fmt::Display for Trouble {
    fn fmt(&self, out: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::NotABundle => out.write_str("that file is not a root store"),
            Self::Version(saw) => write!(
                out,
                "that root store is version {saw} and this machine reads version {VERSION}"
            ),
            Self::Truncated => out.write_str("that root store is cut short"),
            Self::Damaged => out.write_str(
                "that root store does not match its own digest, so it has been damaged \
                 or altered and none of it is trusted",
            ),
            Self::TooMany(count) => write!(
                out,
                "that root store claims {count} certificates and the most this reads is {MAX_ROOTS}"
            ),
        }
    }
}

/// What came of reading a bundle.
#[derive(Debug)]
pub struct Loaded {
    /// The store, ready to verify with.
    pub roots: Roots,
    /// How many certificates the file held.
    pub held: usize,
    /// The ones that would not parse, by position.
    ///
    /// Not fatal -- see [`Roots::add`] -- but worth saying, because a bundle
    /// built by the tool in `tools/nexus-roots` should have none, and one that
    /// does means the tool and this parser have drifted apart.
    pub skipped: Vec<usize>,
}

/// Read a bundle.
///
/// The digest is checked before a single certificate is parsed. That order is
/// deliberate: a store that has been damaged should be one error, not a
/// hundred and twenty.
///
/// # Errors
///
/// [`Trouble`], every variant of which means the store must not be used.
pub fn read(bytes: &[u8]) -> Result<Loaded, Trouble> {
    if bytes.len() < HEADER || bytes[..8] != MAGIC {
        return Err(Trouble::NotABundle);
    }
    let version = word(&bytes[8..12]);
    if version != VERSION {
        return Err(Trouble::Version(version));
    }
    let count = word(&bytes[12..16]) as usize;
    if count > MAX_ROOTS {
        return Err(Trouble::TooMany(count));
    }

    let mut expected = [0u8; 32];
    expected.copy_from_slice(&bytes[16..48]);
    let body = &bytes[HEADER..];
    if !nexus_crypto::sha256::same(&nexus_crypto::sha256::digest(body), &expected) {
        return Err(Trouble::Damaged);
    }

    let mut roots = Roots::empty();
    let mut skipped = Vec::new();
    let mut at = 0usize;
    for index in 0..count {
        if at + 4 > body.len() {
            return Err(Trouble::Truncated);
        }
        let length = word(&body[at..at + 4]) as usize;
        at += 4;
        if length == 0 || length > MAX_CERTIFICATE || at + length > body.len() {
            return Err(Trouble::Truncated);
        }
        if !roots.add(&body[at..at + length]) {
            skipped.push(index);
        }
        at += length;
    }

    Ok(Loaded {
        roots,
        held: count,
        skipped,
    })
}

/// Build a bundle from certificates already in DER.
///
/// Here rather than in the host tool so that the writer and the reader are the
/// same file: a format described in one place and implemented in two is a
/// format that will disagree with itself eventually.
///
/// # Errors
///
/// A count or a certificate above the bounds this reads back.
pub fn write(certificates: &[Vec<u8>]) -> Result<Vec<u8>, String> {
    if certificates.len() > MAX_ROOTS {
        return Err(format!(
            "{} certificates, and the most a bundle holds is {MAX_ROOTS}",
            certificates.len()
        ));
    }
    let mut body = Vec::new();
    for der in certificates {
        if der.is_empty() || der.len() > MAX_CERTIFICATE {
            return Err(format!(
                "a certificate of {} bytes, and the most one may be is {MAX_CERTIFICATE}",
                der.len()
            ));
        }
        body.extend_from_slice(&(der.len() as u32).to_le_bytes());
        body.extend_from_slice(der);
    }

    let mut out = Vec::with_capacity(HEADER + body.len());
    out.extend_from_slice(&MAGIC);
    out.extend_from_slice(&VERSION.to_le_bytes());
    out.extend_from_slice(&(certificates.len() as u32).to_le_bytes());
    out.extend_from_slice(&nexus_crypto::sha256::digest(&body));
    out.extend_from_slice(&body);
    Ok(out)
}

/// Four little-endian bytes.
fn word(bytes: &[u8]) -> u32 {
    u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    /// A certificate that parses, taken from the crate's own fixtures.
    fn a_certificate() -> Vec<u8> {
        include_bytes!("../fixtures/rsa-leaf.der").to_vec()
    }

    #[test]
    fn a_bundle_written_here_reads_back_here() {
        let one = a_certificate();
        let bytes = write(&[one.clone(), one.clone()]).expect("writable");
        let loaded = read(&bytes).expect("readable");
        assert_eq!(loaded.held, 2);
        assert_eq!(loaded.roots.len(), 2);
        assert!(loaded.skipped.is_empty());
    }

    #[test]
    fn an_empty_bundle_is_a_bundle() {
        // An empty store trusts nothing, which is a legitimate thing to ship
        // and a very different thing from a file that will not read.
        let bytes = write(&[]).expect("writable");
        let loaded = read(&bytes).expect("readable");
        assert_eq!(loaded.held, 0);
        assert!(loaded.roots.is_empty());
    }

    #[test]
    fn something_else_entirely_is_not_a_bundle() {
        assert_eq!(read(b"hello").unwrap_err(), Trouble::NotABundle);
        assert_eq!(read(&[0u8; 64]).unwrap_err(), Trouble::NotABundle);
    }

    #[test]
    fn a_later_version_is_refused_rather_than_guessed_at() {
        let mut bytes = write(&[a_certificate()]).expect("writable");
        bytes[8] = 2;
        assert_eq!(read(&bytes).unwrap_err(), Trouble::Version(2));
    }

    #[test]
    fn a_single_changed_byte_makes_the_whole_store_untrusted() {
        // The test this format exists for. A bundle that has been altered must
        // not load as a smaller bundle, or as a bundle with one odd root in it.
        let bytes = write(&[a_certificate()]).expect("writable");
        for at in [HEADER, HEADER + 7, bytes.len() - 1] {
            let mut damaged = bytes.clone();
            damaged[at] ^= 0x01;
            assert_eq!(read(&damaged).unwrap_err(), Trouble::Damaged, "at {at}");
        }
    }

    #[test]
    fn a_truncated_store_is_refused_rather_than_read_as_far_as_it_goes() {
        let bytes = write(&[a_certificate(), a_certificate()]).expect("writable");
        let short = &bytes[..bytes.len() - 40];
        // Cutting the file changes the body, so the digest catches it first --
        // which is the behaviour wanted: one error, not a partial store.
        assert_eq!(read(short).unwrap_err(), Trouble::Damaged);
    }

    #[test]
    fn a_length_that_runs_off_the_end_is_refused() {
        // Past the digest, because the digest is recomputed over the damage.
        // This is the case where somebody builds a bundle by hand and gets the
        // arithmetic wrong, and it must not be read as far as it goes either.
        let mut body = Vec::new();
        body.extend_from_slice(&9999u32.to_le_bytes());
        body.extend_from_slice(b"short");
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&MAGIC);
        bytes.extend_from_slice(&VERSION.to_le_bytes());
        bytes.extend_from_slice(&1u32.to_le_bytes());
        bytes.extend_from_slice(&nexus_crypto::sha256::digest(&body));
        bytes.extend_from_slice(&body);
        assert_eq!(read(&bytes).unwrap_err(), Trouble::Truncated);
    }

    #[test]
    fn an_impossible_count_is_refused_before_anything_is_allocated() {
        let mut bytes = write(&[]).expect("writable");
        bytes[12..16].copy_from_slice(&500_000u32.to_le_bytes());
        assert_eq!(read(&bytes).unwrap_err(), Trouble::TooMany(500_000));
    }

    #[test]
    fn a_certificate_that_will_not_parse_costs_that_certificate_and_no_more() {
        let bytes = write(&[a_certificate(), vec![0x30, 0x02, 0xFF, 0xFF]]).expect("writable");
        let loaded = read(&bytes).expect("readable");
        assert_eq!(loaded.held, 2);
        assert_eq!(loaded.roots.len(), 1);
        assert_eq!(loaded.skipped, vec![1]);
    }

    #[test]
    fn too_many_to_write_is_refused_rather_than_written_unreadable() {
        let many = vec![a_certificate(); MAX_ROOTS + 1];
        assert!(write(&many).is_err());
    }
}
