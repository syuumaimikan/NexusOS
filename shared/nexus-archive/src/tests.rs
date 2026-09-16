//! What the readers do with archives they did not make.

use alloc::string::String;
use alloc::vec::Vec;

use super::fixtures::{DEB, EVIL_TAR, LONG_TAR, PLAIN_TAR, XZ_DEB};
use super::{ar, compression, deb, safe_name, tar, Kind, Trouble};

/// The names, in order, of everything a tar holds.
fn names(archive: &[u8]) -> Vec<String> {
    tar(archive)
        .expect("a well-formed archive")
        .into_iter()
        .map(|entry| entry.name)
        .collect()
}

#[test]
fn a_tar_made_elsewhere() {
    let entries = tar(PLAIN_TAR).expect("a well-formed archive");
    assert_eq!(entries.len(), 3);

    // The leading `./` every tar writes is gone, because a name is going to be
    // joined onto a directory on this machine.
    assert_eq!(entries[0].name, "usr");
    assert_eq!(entries[0].kind, Kind::Directory);
    assert_eq!(entries[1].name, "usr/bin/hello");
    assert_eq!(entries[1].kind, Kind::File);

    // And the contents come back byte for byte.
    assert_eq!(
        entries[1].bytes(PLAIN_TAR).unwrap(),
        b"#!/bin/sh\necho hi\n"
    );
    assert_eq!(
        entries[2].bytes(PLAIN_TAR).unwrap(),
        b"A file from a tar made elsewhere.\n"
    );
}

/// A directory has no contents, and saying it has some would make an unpacker
/// create a file where a folder belongs.
#[test]
fn a_directory_carries_no_bytes() {
    let entries = tar(PLAIN_TAR).unwrap();
    let directory = &entries[0];
    assert_eq!(directory.length, 0);
    assert!(directory.bytes(PLAIN_TAR).unwrap().is_empty());
}

/// Over a hundred bytes, so it goes through the long-name member rather than
/// the header field -- a path a reader never exercises by accident.
#[test]
fn a_name_too_long_for_the_header() {
    let found = names(LONG_TAR);
    assert_eq!(found.len(), 1);
    assert!(found[0].len() > 100, "the fixture is not actually long");
    assert!(found[0].ends_with("deep.txt"));
    assert!(!found[0].contains("//"));
}

/// The whole archive is refused, not the one member.
#[test]
fn an_archive_that_climbs_out_is_refused_entirely() {
    assert_eq!(tar(EVIL_TAR), Err(Trouble::UnsafeName));
}

#[test]
fn names_that_must_not_be_joined_to_a_directory() {
    for bad in [
        "../etc/passwd",
        "a/../../b",
        "..",
        "/",
        "",
        "./",
        "a/b/../../../c",
    ] {
        assert_eq!(safe_name(bad), Err(Trouble::UnsafeName), "accepted {bad:?}");
    }
}

#[test]
fn names_that_are_merely_untidy() {
    assert_eq!(safe_name("./usr/bin/x").unwrap(), "usr/bin/x");
    assert_eq!(safe_name("/usr/bin/x").unwrap(), "usr/bin/x");
    assert_eq!(safe_name("usr//bin///x").unwrap(), "usr/bin/x");
    assert_eq!(safe_name("usr/./bin/x").unwrap(), "usr/bin/x");
    // A file called `..something` is not a climb and must survive.
    assert_eq!(safe_name("usr/..config").unwrap(), "usr/..config");
}

/// Truncation is how a download goes wrong, so it gets its own test rather
/// than being assumed to fall out of the size checks.
#[test]
fn a_truncated_tar_is_refused() {
    // Inside the first member's body.
    assert_eq!(tar(&PLAIN_TAR[..700]), Err(Trouble::Truncated));
    // And less than a single header. This used to be asserted as `is_ok`, on
    // the reasoning that the loop simply ends -- which it does, and which is
    // exactly the hole: an archive that stops before its end-of-archive marker
    // is a truncated one whether it stopped mid-header or mid-member, and
    // reading it as a small complete archive is how half a download passes for
    // a whole one.
    assert_eq!(tar(&PLAIN_TAR[..300]), Err(Trouble::Truncated));
    assert_eq!(tar(&[]), Err(Trouble::Truncated));
}

#[test]
fn a_corrupted_header_is_refused() {
    let mut broken: Vec<u8> = PLAIN_TAR.to_vec();
    // A byte inside the first name, which the checksum covers.
    broken[2] ^= 0xFF;
    assert_eq!(tar(&broken), Err(Trouble::Corrupt));
}

#[test]
fn an_ar_archive() {
    let members = ar(DEB).expect("a well-formed ar");
    let found: Vec<&str> = members.iter().map(|m| m.name.as_str()).collect();
    assert_eq!(found, ["debian-binary", "control.tar.gz", "data.tar.gz"]);
    assert_eq!(members[0].bytes(DEB).unwrap(), b"2.0\n");
}

#[test]
fn not_an_ar_archive() {
    assert_eq!(ar(b"not an archive"), Err(Trouble::NotThisFormat));
    assert_eq!(ar(PLAIN_TAR), Err(Trouble::NotThisFormat));
}

#[test]
fn a_debian_package() {
    let package = deb(DEB, 1 << 20).expect("a well-formed package");

    assert_eq!(package.field("Package"), Some("demo"));
    assert_eq!(package.field("Version"), Some("1.0-1"));
    // Field names are matched without regard to case, because the format does
    // not promise one.
    assert_eq!(package.field("package"), Some("demo"));
    assert_eq!(package.field("Nothing-Like-This"), None);

    let installed: Vec<&str> = package
        .files
        .iter()
        .map(|(name, _, _)| name.as_str())
        .collect();
    assert_eq!(
        installed,
        ["usr", "usr/bin/demo", "usr/share/demo/notes.txt"]
    );

    let notes = &package.files[2];
    assert_eq!(notes.1, Kind::File);
    assert_eq!(notes.2, b"installed by nexus\n");
}

/// The case that matters most in practice, because most packages built in the
/// last few years are one of these. The refusal has to name the compression:
/// "could not install" is not something anybody can act on.
#[test]
fn a_package_compressed_with_something_unread_says_which() {
    assert!(matches!(deb(XZ_DEB, 1 << 20), Err(Trouble::NotThisFormat)));
    assert_eq!(compression(XZ_DEB), Some("xz"));
    assert_eq!(compression(DEB), Some("gzip"));
    assert_eq!(compression(b"not an archive"), None);
}

#[test]
fn a_truncated_package_is_refused() {
    assert!(deb(&DEB[..DEB.len() / 2], 1 << 20).is_err());
}
