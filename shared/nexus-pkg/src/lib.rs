//! The `.nexus` package format.
//!
//! A package is one file: a header, a table saying what is in it and where, and
//! the contents laid end to end. It is read in place — nothing is decompressed,
//! nothing is allocated to parse it, and a reader that only wants one file out
//! of a package reads the table and then that file.
//!
//! # Why every entry carries its own digest
//!
//! A digest over the whole package says the package is intact. It does not say
//! *which* part is not, and it cannot be checked until the last byte has been
//! read — which on a machine installing a package means writing files out of a
//! package that may turn out to be corrupt, or holding the whole thing in
//! memory first. A digest per entry means each file is checked as it is
//! installed and named when it fails.
//!
//! The whole-package digest is there too, because it is what a signature signs:
//! signing every entry separately would let an attacker keep the signatures and
//! reorder, drop or duplicate the entries between them.
//!
//! # What this crate is not
//!
//! It does not install anything, fetch anything, or resolve dependencies. It
//! reads and writes the format, and it hashes. Everything else is the business
//! of whoever is doing the installing, which on this system is an ordinary
//! program with no privileges beyond the directory it was given.

#![no_std]
#![deny(unsafe_op_in_unsafe_fn)]

extern crate alloc;

pub mod sha256;

pub use sha256::{digest, digests_equal, Hasher, DIGEST};

/// What every package begins with.
pub const MAGIC: [u8; 8] = *b"NEXUSPKG";

/// The format this crate reads and writes.
pub const VERSION: u32 = 1;

/// Bytes of header.
pub const HEADER: usize = 176;
/// Bytes of one entry in the table.
pub const ENTRY: usize = 104;

/// The longest a package's name may be.
pub const MAX_NAME: usize = 32;
/// The longest a release string may be.
pub const MAX_RELEASE: usize = 16;
/// The longest a path inside a package may be.
pub const MAX_PATH: usize = 64;

/// Where each field of the header lives.
///
/// Offsets rather than a `#[repr(C)]` struct read through a pointer. A package
/// is data from somewhere else, and the arithmetic being visible is what makes
/// it checkable: there is no alignment to assume and no padding to be surprised
/// by on another compiler.
mod at {
    pub const MAGIC: usize = 0;
    pub const VERSION: usize = 8;
    pub const ENTRIES: usize = 12;
    pub const PAYLOAD: usize = 16;
    pub const TOTAL: usize = 20;
    pub const NAME: usize = 24;
    pub const RELEASE: usize = 56;
    pub const DIGEST: usize = 72;
    pub const SIGNATURE: usize = 104;
    // 168..176 is reserved and must be zero.
}

/// Where each field of an entry lives.
mod entry_at {
    pub const PATH: usize = 0;
    pub const OFFSET: usize = 64;
    pub const LENGTH: usize = 68;
    pub const DIGEST: usize = 72;
}

/// What can be wrong with a package.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    /// Not a package at all.
    NotAPackage,
    /// A format this code does not read.
    Version(u32),
    /// Shorter than it says it is, or than it must be.
    Truncated,
    /// A table that does not fit, or entries that overlap or run past the end.
    Malformed,
    /// The whole-package digest does not match.
    Corrupt,
    /// One file's digest does not match.
    FileCorrupt,
    /// A name or path with no terminator, or one that is not text.
    BadName,
    /// Nobody signed it.
    Unsigned,
    /// Somebody signed it, and not with the key this machine trusts.
    Forged,
}

impl core::fmt::Display for Error {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let text = match self {
            Self::NotAPackage => "not a package",
            Self::Version(_) => "a package format this cannot read",
            Self::Truncated => "the package is shorter than it says",
            Self::Malformed => "the package's table does not describe it",
            Self::Corrupt => "the package's digest does not match",
            Self::FileCorrupt => "a file in the package does not match its digest",
            Self::BadName => "a name in the package is not usable",
            Self::Unsigned => "the package is not signed",
            Self::Forged => "the package's signature is not from a trusted key",
        };
        formatter.write_str(text)
    }
}

/// One file in a package.
#[derive(Clone, Copy)]
pub struct Entry {
    /// Where it goes, relative to wherever the package is installed.
    pub path: [u8; MAX_PATH],
    /// Where its bytes are, from the start of the payload.
    pub offset: u32,
    pub length: u32,
    pub digest: [u8; DIGEST],
}

impl Entry {
    /// The path as text, up to the first zero byte.
    ///
    /// # Errors
    ///
    /// If the path has no terminator, is empty, is not UTF-8, or tries to climb
    /// out of where it is being installed. That last is the one that matters: a
    /// package containing `../../BIN/INIT.ELF` would otherwise install itself
    /// over the first program the machine runs.
    pub fn name(&self) -> Result<&str, Error> {
        let end = self
            .path
            .iter()
            .position(|byte| *byte == 0)
            .ok_or(Error::BadName)?;
        if end == 0 {
            return Err(Error::BadName);
        }
        let text = core::str::from_utf8(&self.path[..end]).map_err(|_| Error::BadName)?;
        if text.starts_with('/') || text.contains("..") || text.contains(':') {
            return Err(Error::BadName);
        }
        Ok(text)
    }
}

/// A package, read in place.
pub struct Package<'a> {
    bytes: &'a [u8],
    entries: usize,
    payload: usize,
}

impl<'a> Package<'a> {
    /// Read a package's header and table, checking that they describe what is
    /// actually there.
    ///
    /// The digest is *not* checked here: it is checked by [`Package::verify`],
    /// separately, because a reader may want to know what is in a package
    /// before hashing the whole of it.
    ///
    /// # Errors
    ///
    /// If the bytes are not a package this can read, or if its table does not
    /// describe the bytes that follow.
    pub fn open(bytes: &'a [u8]) -> Result<Self, Error> {
        if bytes.len() < HEADER {
            return Err(Error::Truncated);
        }
        if bytes[at::MAGIC..at::MAGIC + 8] != MAGIC {
            return Err(Error::NotAPackage);
        }
        let version = u32(bytes, at::VERSION);
        if version != VERSION {
            return Err(Error::Version(version));
        }

        let entries = u32(bytes, at::ENTRIES) as usize;
        let payload = u32(bytes, at::PAYLOAD) as usize;
        let total = u32(bytes, at::TOTAL) as usize;

        // Every one of these is a number from somewhere else, so every one is
        // checked. A table that claimed a million entries would otherwise be a
        // read of a hundred megabytes out of a file of two hundred bytes.
        if total != bytes.len() {
            return Err(Error::Truncated);
        }
        let table = entries.checked_mul(ENTRY).ok_or(Error::Malformed)?;
        let needed = HEADER.checked_add(table).ok_or(Error::Malformed)?;
        if payload != needed || payload > bytes.len() {
            return Err(Error::Malformed);
        }

        let package = Self {
            bytes,
            entries,
            payload,
        };

        // And every entry has to fit inside the payload. Checked now rather
        // than when the file is read, so that a caller iterating entries cannot
        // be handed a slice that runs past the end.
        for index in 0..entries {
            let entry = package.entry(index).ok_or(Error::Malformed)?;
            let end = (entry.offset as usize)
                .checked_add(entry.length as usize)
                .ok_or(Error::Malformed)?;
            if payload.checked_add(end).ok_or(Error::Malformed)? > bytes.len() {
                return Err(Error::Malformed);
            }
            entry.name()?;
        }

        Ok(package)
    }

    /// The package's name.
    ///
    /// # Errors
    ///
    /// If it is not text, or has no terminator.
    pub fn name(&self) -> Result<&str, Error> {
        text(&self.bytes[at::NAME..at::NAME + MAX_NAME])
    }

    /// Which release of it this is.
    ///
    /// # Errors
    ///
    /// As [`Package::name`].
    pub fn release(&self) -> Result<&str, Error> {
        text(&self.bytes[at::RELEASE..at::RELEASE + MAX_RELEASE])
    }

    /// How many files it contains.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries
    }

    /// Whether it contains nothing.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries == 0
    }

    /// The digest the package says it has.
    #[must_use]
    pub fn stated_digest(&self) -> [u8; DIGEST] {
        let mut out = [0u8; DIGEST];
        out.copy_from_slice(&self.bytes[at::DIGEST..at::DIGEST + DIGEST]);
        out
    }

    /// The signature, or all zeroes if the package is unsigned.
    #[must_use]
    pub fn signature(&self) -> [u8; 64] {
        let mut out = [0u8; 64];
        out.copy_from_slice(&self.bytes[at::SIGNATURE..at::SIGNATURE + 64]);
        out
    }

    /// One entry.
    #[must_use]
    pub fn entry(&self, index: usize) -> Option<Entry> {
        if index >= self.entries {
            return None;
        }
        let at = HEADER + index * ENTRY;
        let bytes = self.bytes.get(at..at + ENTRY)?;
        let mut entry = Entry {
            path: [0; MAX_PATH],
            offset: u32(bytes, entry_at::OFFSET),
            length: u32(bytes, entry_at::LENGTH),
            digest: [0; DIGEST],
        };
        entry
            .path
            .copy_from_slice(&bytes[entry_at::PATH..entry_at::PATH + MAX_PATH]);
        entry
            .digest
            .copy_from_slice(&bytes[entry_at::DIGEST..entry_at::DIGEST + DIGEST]);
        Some(entry)
    }

    /// The bytes of one entry.
    #[must_use]
    pub fn contents(&self, entry: &Entry) -> Option<&'a [u8]> {
        let start = self.payload.checked_add(entry.offset as usize)?;
        let end = start.checked_add(entry.length as usize)?;
        self.bytes.get(start..end)
    }

    /// What the whole package hashes to, which is what a signature covers.
    ///
    /// The digest and signature fields are treated as zero, because they cannot
    /// be part of what they describe.
    #[must_use]
    pub fn compute_digest(&self) -> [u8; DIGEST] {
        digest_of(self.bytes)
    }

    /// Check the package *and* that it was signed by the key given.
    ///
    /// The order is deliberate: the signature is checked first, because
    /// everything else is a statement the package makes about itself and the
    /// signature is the only statement somebody else makes about the package.
    /// Hashing every file in something nobody vouched for is work done on
    /// behalf of whoever sent it.
    ///
    /// # Errors
    ///
    /// [`Error::Unsigned`] if the package carries no signature, [`Error::Forged`]
    /// if it carries one that does not check, and then whatever [`Package::verify`]
    /// finds.
    pub fn verify_signed_by(&self, public: &[u8; 32]) -> Result<(), Error> {
        let signature = self.signature();
        if is_unsigned(&signature) {
            return Err(Error::Unsigned);
        }
        // Against the digest the package *has*, not the one it claims: a
        // signature over a stated digest that nothing checks would be a
        // signature over a number an attacker chose.
        let computed = self.compute_digest();
        if !nexus_crypto::verify(public, &computed, &signature) {
            return Err(Error::Forged);
        }
        self.verify()
    }

    /// Check that the package is what it says it is.
    ///
    /// # Errors
    ///
    /// [`Error::Corrupt`] if the whole-package digest does not match, and
    /// [`Error::FileCorrupt`] if any file's does.
    pub fn verify(&self) -> Result<(), Error> {
        if !digests_equal(&self.compute_digest(), &self.stated_digest()) {
            return Err(Error::Corrupt);
        }
        for index in 0..self.entries {
            let entry = self.entry(index).ok_or(Error::Malformed)?;
            let bytes = self.contents(&entry).ok_or(Error::Malformed)?;
            if !digests_equal(&digest(bytes), &entry.digest) {
                return Err(Error::FileCorrupt);
            }
        }
        Ok(())
    }
}

/// Sign a package in place.
///
/// What is signed is the package's digest, not the package: Ed25519 hashes what
/// it is given anyway, and signing the digest means the signature covers
/// exactly what the digest covers -- which is everything except the digest and
/// signature fields themselves.
///
/// The digest is recomputed here rather than read out of the header, so a
/// package whose digest field was wrong cannot be signed into looking right.
///
/// # Errors
///
/// If the bytes are too short to be a package.
pub fn sign(bytes: &mut [u8], secret: &[u8; 32]) -> Result<(), Error> {
    if bytes.len() < HEADER {
        return Err(Error::Truncated);
    }
    let whole = digest_of(bytes);
    bytes[at::DIGEST..at::DIGEST + DIGEST].copy_from_slice(&whole);
    let signature = nexus_crypto::sign(secret, &whole);
    bytes[at::SIGNATURE..at::SIGNATURE + 64].copy_from_slice(&signature);
    Ok(())
}

/// Whether a package carries no signature at all.
///
/// An all-zero field. Distinguished from a signature that does not check,
/// because the two are different failures: one package was never signed and the
/// other is claiming to be something it is not.
#[must_use]
pub fn is_unsigned(signature: &[u8; 64]) -> bool {
    signature.iter().all(|byte| *byte == 0)
}

/// Hash a package's bytes the way its digest field is defined.
///
/// Everything except the digest and the signature, which are read as zeroes.
/// Written as a free function so that whoever is *building* a package can use
/// exactly the same arithmetic on bytes that are not a package yet.
#[must_use]
pub fn digest_of(bytes: &[u8]) -> [u8; DIGEST] {
    let mut hasher = Hasher::new();
    hasher.update(&bytes[..at::DIGEST.min(bytes.len())]);
    if bytes.len() > at::DIGEST {
        // The two fields, as zeroes.
        hasher.update(&[0u8; DIGEST + 64]);
    }
    let after = at::SIGNATURE + 64;
    if bytes.len() > after {
        hasher.update(&bytes[after..]);
    }
    hasher.finish()
}

/// Read a big-endian... no: a little-endian `u32`.
///
/// Little endian throughout, unlike everything on a network. A package is not a
/// protocol between machines that disagree; it is a file read by the same kind
/// of machine that wrote it, and matching the processor means the numbers are
/// what a debugger shows.
fn u32(bytes: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]])
}

/// A NUL-padded field, as text.
fn text(field: &[u8]) -> Result<&str, Error> {
    let end = field
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(field.len());
    if end == 0 {
        return Err(Error::BadName);
    }
    core::str::from_utf8(&field[..end]).map_err(|_| Error::BadName)
}

/// Build a package out of named files.
///
/// Returns the bytes. Used by the tool that makes packages and by the tests
/// here; a machine that only installs them never calls it.
///
/// # Errors
///
/// If a name or path is too long for its field, or there are so many files that
/// the offsets would not fit.
pub fn build(
    name: &str,
    release: &str,
    files: &[(&str, &[u8])],
) -> Result<alloc::vec::Vec<u8>, Error> {
    use alloc::vec;

    if name.len() >= MAX_NAME || release.len() >= MAX_RELEASE {
        return Err(Error::BadName);
    }
    let payload_at = HEADER + files.len() * ENTRY;
    let mut bytes = vec![0u8; payload_at];

    // The table is written as the payload is appended, so an entry's offset is
    // simply how much payload there was before it.
    for (index, (path, contents)) in files.iter().enumerate() {
        if path.len() >= MAX_PATH || path.is_empty() {
            return Err(Error::BadName);
        }
        let offset = bytes.len() - payload_at;
        let at = HEADER + index * ENTRY;
        bytes[at + entry_at::PATH..at + entry_at::PATH + path.len()]
            .copy_from_slice(path.as_bytes());
        let offset = u32::try_from(offset).map_err(|_| Error::Malformed)?;
        let length = u32::try_from(contents.len()).map_err(|_| Error::Malformed)?;
        bytes[at + entry_at::OFFSET..at + entry_at::OFFSET + 4]
            .copy_from_slice(&offset.to_le_bytes());
        bytes[at + entry_at::LENGTH..at + entry_at::LENGTH + 4]
            .copy_from_slice(&length.to_le_bytes());
        let file_digest = digest(contents);
        bytes[at + entry_at::DIGEST..at + entry_at::DIGEST + DIGEST].copy_from_slice(&file_digest);
        bytes.extend_from_slice(contents);
    }

    bytes[at::MAGIC..at::MAGIC + 8].copy_from_slice(&MAGIC);
    bytes[at::VERSION..at::VERSION + 4].copy_from_slice(&VERSION.to_le_bytes());
    let count = u32::try_from(files.len()).map_err(|_| Error::Malformed)?;
    bytes[at::ENTRIES..at::ENTRIES + 4].copy_from_slice(&count.to_le_bytes());
    let payload = u32::try_from(payload_at).map_err(|_| Error::Malformed)?;
    bytes[at::PAYLOAD..at::PAYLOAD + 4].copy_from_slice(&payload.to_le_bytes());
    let total = u32::try_from(bytes.len()).map_err(|_| Error::Malformed)?;
    bytes[at::TOTAL..at::TOTAL + 4].copy_from_slice(&total.to_le_bytes());
    bytes[at::NAME..at::NAME + name.len()].copy_from_slice(name.as_bytes());
    bytes[at::RELEASE..at::RELEASE + release.len()].copy_from_slice(release.as_bytes());

    // Last, because it covers everything else.
    let whole = digest_of(&bytes);
    bytes[at::DIGEST..at::DIGEST + DIGEST].copy_from_slice(&whole);

    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> alloc::vec::Vec<u8> {
        build(
            "hello",
            "1.0.0",
            &[
                ("BIN/HELLO.ELF", b"not really an ELF, but it is bytes"),
                ("SHARE/README", b"a second file, so the table has to work"),
            ],
        )
        .expect("a package this small must build")
    }

    #[test]
    fn a_package_round_trips() {
        let bytes = sample();
        let package = Package::open(&bytes).expect("what build wrote, open must read");
        assert_eq!(package.name().unwrap(), "hello");
        assert_eq!(package.release().unwrap(), "1.0.0");
        assert_eq!(package.len(), 2);

        let first = package.entry(0).unwrap();
        assert_eq!(first.name().unwrap(), "BIN/HELLO.ELF");
        assert_eq!(
            package.contents(&first).unwrap(),
            b"not really an ELF, but it is bytes"
        );
        let second = package.entry(1).unwrap();
        assert_eq!(second.name().unwrap(), "SHARE/README");
        package.verify().expect("a package this built must verify");
    }

    #[test]
    fn one_flipped_bit_anywhere_in_a_file_is_caught() {
        let mut bytes = sample();
        let last = bytes.len() - 1;
        bytes[last] ^= 1;
        let package = Package::open(&bytes).expect("the header is still intact");
        // The whole-package digest catches it first, which is what a signature
        // would check.
        assert_eq!(package.verify(), Err(Error::Corrupt));
    }

    #[test]
    fn a_file_swapped_for_another_is_caught_by_name() {
        // The interesting attack is not corruption, it is substitution: keep
        // the package valid overall and change one file. Fixing up the
        // whole-package digest is easy for whoever made the change, so the
        // per-file digest has to catch it on its own.
        let mut bytes = sample();
        let last = bytes.len() - 1;
        bytes[last] ^= 1;
        // Recompute the outer digest, as an attacker with no key would.
        let whole = digest_of(&bytes);
        bytes[at::DIGEST..at::DIGEST + DIGEST].copy_from_slice(&whole);

        let package = Package::open(&bytes).unwrap();
        assert_eq!(package.verify(), Err(Error::FileCorrupt));
    }

    #[test]
    fn a_path_that_climbs_out_is_refused() {
        let bytes = build("evil", "1", &[("../../BIN/INIT.ELF", b"gotcha")]).unwrap();
        // Refused at open, before anything could be written anywhere: a package
        // that names a path outside where it is being installed is not a
        // package with one bad entry, it is not a package.
        assert!(matches!(Package::open(&bytes), Err(Error::BadName)));
    }

    #[test]
    fn an_absolute_path_is_refused() {
        let bytes = build("evil", "1", &[("/BIN/INIT.ELF", b"gotcha")]).unwrap();
        assert!(matches!(Package::open(&bytes), Err(Error::BadName)));
    }

    #[test]
    fn a_truncated_package_is_refused() {
        let bytes = sample();
        assert!(matches!(
            Package::open(&bytes[..bytes.len() - 1]),
            Err(Error::Truncated)
        ));
    }

    #[test]
    fn something_that_is_not_a_package_is_refused() {
        let bytes = [0u8; HEADER + 8];
        assert!(matches!(Package::open(&bytes), Err(Error::NotAPackage)));
    }

    #[test]
    fn a_table_that_claims_more_than_is_there_is_refused() {
        let mut bytes = sample();
        // A million entries, which would be a hundred megabytes of table in a
        // file of a few hundred bytes.
        bytes[at::ENTRIES..at::ENTRIES + 4].copy_from_slice(&1_000_000u32.to_le_bytes());
        assert!(matches!(Package::open(&bytes), Err(Error::Malformed)));
    }

    #[test]
    fn an_entry_pointing_past_the_end_is_refused() {
        let mut bytes = sample();
        let at = HEADER + entry_at::LENGTH;
        bytes[at..at + 4].copy_from_slice(&0xFFFF_0000u32.to_le_bytes());
        assert!(matches!(Package::open(&bytes), Err(Error::Malformed)));
    }

    #[test]
    fn an_empty_package_is_still_a_package() {
        let bytes = build("empty", "0", &[]).unwrap();
        let package = Package::open(&bytes).unwrap();
        assert!(package.is_empty());
        package.verify().unwrap();
    }

    #[test]
    fn the_digest_ignores_its_own_field_and_the_signature() {
        let mut bytes = sample();
        let before = digest_of(&bytes);
        // Scribble in both fields. Neither is part of what is hashed, so the
        // answer must not move -- which is what lets a package be signed after
        // it has been hashed.
        for index in at::DIGEST..at::SIGNATURE + 64 {
            bytes[index] ^= 0xA5;
        }
        assert_eq!(digest_of(&bytes), before);
    }

    /// A key that exists only in this test file.
    const TEST_SECRET: [u8; 32] = [
        0x4c, 0xcd, 0x08, 0x9b, 0x28, 0xff, 0x96, 0xda, 0x9d, 0xb6, 0xc3, 0x46, 0xec, 0x11, 0x4e,
        0x0f, 0x5b, 0x8a, 0x31, 0x9f, 0x35, 0xab, 0xa6, 0x24, 0xda, 0x8c, 0xf6, 0xed, 0x4f, 0xb8,
        0xa6, 0xfb,
    ];

    #[test]
    fn a_signed_package_verifies_under_its_key() {
        let mut bytes = sample();
        sign(&mut bytes, &TEST_SECRET).unwrap();
        let public = nexus_crypto::public_key(&TEST_SECRET);
        let package = Package::open(&bytes).unwrap();
        package.verify_signed_by(&public).unwrap();
    }

    #[test]
    fn an_unsigned_package_is_refused_as_unsigned() {
        let bytes = sample();
        let public = nexus_crypto::public_key(&TEST_SECRET);
        let package = Package::open(&bytes).unwrap();
        // Not "corrupt" and not "forged": nobody claimed anything about it, and
        // that is a different thing to report.
        assert_eq!(package.verify_signed_by(&public), Err(Error::Unsigned));
    }

    #[test]
    fn a_package_signed_by_someone_else_is_refused() {
        let mut bytes = sample();
        let mut other = TEST_SECRET;
        other[0] ^= 1;
        sign(&mut bytes, &other).unwrap();
        let public = nexus_crypto::public_key(&TEST_SECRET);
        let package = Package::open(&bytes).unwrap();
        assert_eq!(package.verify_signed_by(&public), Err(Error::Forged));
    }

    #[test]
    fn changing_a_signed_package_breaks_the_signature() {
        let mut bytes = sample();
        sign(&mut bytes, &TEST_SECRET).unwrap();
        let public = nexus_crypto::public_key(&TEST_SECRET);

        // Substitute a file and fix up the digest, which is what somebody with
        // no key can do. The signature is over the digest, so the digest moving
        // is exactly what it catches.
        let last = bytes.len() - 1;
        bytes[last] ^= 1;
        let whole = digest_of(&bytes);
        bytes[at::DIGEST..at::DIGEST + DIGEST].copy_from_slice(&whole);

        let package = Package::open(&bytes).unwrap();
        assert_eq!(package.verify_signed_by(&public), Err(Error::Forged));
    }

    #[test]
    fn signing_fixes_a_wrong_digest_rather_than_blessing_it() {
        let mut bytes = sample();
        // Scribble on the digest field before signing. `sign` recomputes it, so
        // what comes out is a package that verifies -- rather than a signature
        // over a number somebody else chose.
        bytes[at::DIGEST] ^= 0xFF;
        sign(&mut bytes, &TEST_SECRET).unwrap();
        let public = nexus_crypto::public_key(&TEST_SECRET);
        Package::open(&bytes)
            .unwrap()
            .verify_signed_by(&public)
            .unwrap();
    }
}
