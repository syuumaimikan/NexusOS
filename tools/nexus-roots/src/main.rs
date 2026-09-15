//! Turn a PEM bundle of certificate authorities into a root store this machine
//! can read.
//!
//! # The idea this tool is built around
//!
//! It parses every certificate with **the machine's own X.509 parser** --
//! `nexus_tls::x509` -- and ships only what that parser accepted. Nothing else
//! would do. A bundle assembled by OpenSSL and handed over unexamined would be
//! a hundred and twenty certificates of which some unknown number are unusable,
//! discovered one at a time, months later, as sites that mysteriously will not
//! load.
//!
//! So the filter is the point, and so is the report. Run it and it says how
//! many roots went in, how many came out, and the reason for every one that did
//! not -- an unsupported key type, an expiry, a critical extension this machine
//! refuses to guess at. That list is a to-do list for the TLS implementation,
//! written by the TLS implementation.
//!
//! # What it drops, and why each is right to drop
//!
//! | | |
//! | --- | --- |
//! | a key this machine cannot verify with | it could never complete a chain |
//! | not a certificate authority | `basicConstraints` says so; a chain through it would be refused |
//! | already expired | trusting it would be trusting a clock nobody keeps |
//! | a critical extension this does not understand | refusing is what critical means |
//!
//! A root dropped here is not a root quietly weakened; it is one this machine
//! is honest about not being able to use.
//!
//! # Usage
//!
//! ```text
//! nexus-roots <input.pem>... --out build/roots.nxr
//! nexus-roots --list <input.pem>      say what is in it and write nothing
//! ```

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use nexus_tls::x509::{self, Algorithm, PublicKey};

mod pem;

/// The moment expiry is judged against.
///
/// The host's clock, because a root that expires next week should be reported
/// now rather than on the day the machine stops being able to browse.
fn today() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|since| since.as_secs() as i64)
        .unwrap_or(0)
}

/// How soon an expiry is worth warning about.
const SOON: i64 = 180 * 24 * 60 * 60;

/// What became of one certificate.
enum Verdict {
    /// It is in the bundle.
    Kept { name: String, about: String },
    /// It is not, and this is why.
    Dropped { name: String, why: String },
}

fn main() -> ExitCode {
    let mut inputs: Vec<PathBuf> = Vec::new();
    let mut out: Option<PathBuf> = None;
    let mut listing = false;

    let mut arguments = std::env::args().skip(1);
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--out" | "-o" => match arguments.next() {
                Some(path) => out = Some(PathBuf::from(path)),
                None => {
                    eprintln!("--out needs a path after it");
                    return ExitCode::from(2);
                }
            },
            "--list" | "-l" => listing = true,
            "--help" | "-h" => {
                println!("{USAGE}");
                return ExitCode::SUCCESS;
            }
            other if other.starts_with('-') => {
                eprintln!("no such option: {other}\n\n{USAGE}");
                return ExitCode::from(2);
            }
            other => inputs.push(PathBuf::from(other)),
        }
    }

    if inputs.is_empty() {
        eprintln!("nothing to read\n\n{USAGE}");
        return ExitCode::from(2);
    }
    if out.is_none() && !listing {
        eprintln!("--out is needed, unless --list\n\n{USAGE}");
        return ExitCode::from(2);
    }

    match build(&inputs, out.as_deref(), listing) {
        Ok(0) => {
            eprintln!("\nnot one certificate survived, so there is no root store to write");
            ExitCode::FAILURE
        }
        Ok(_) => ExitCode::SUCCESS,
        Err(why) => {
            eprintln!("{why}");
            ExitCode::FAILURE
        }
    }
}

const USAGE: &str = "\
nexus-roots -- build the root store this machine trusts

  nexus-roots <input.pem>... --out <roots.nxr>
  nexus-roots --list <input.pem>...

Reads PEM certificate bundles and writes the binary root store described in
shared/nexus-tls/src/roots.rs. Every certificate is parsed by the machine's own
X.509 parser first, and anything that parser cannot use is dropped with a
reason rather than shipped to fail later.";

/// Read the inputs, filter, and write the bundle. Returns how many were kept.
fn build(inputs: &[PathBuf], out: Option<&Path>, listing: bool) -> Result<usize, String> {
    let now = today();
    let mut kept: Vec<Vec<u8>> = Vec::new();
    let mut verdicts: Vec<Verdict> = Vec::new();
    let mut seen: BTreeSet<Vec<u8>> = BTreeSet::new();
    let mut found = 0usize;
    let mut duplicates = 0usize;

    for input in inputs {
        let text = std::fs::read_to_string(input)
            .map_err(|why| format!("cannot read {}: {why}", input.display()))?;
        let blocks = pem::certificates(&text)
            .map_err(|why| format!("cannot read {}: {why}", input.display()))?;
        if blocks.is_empty() {
            return Err(format!("{} holds no certificates", input.display()));
        }
        println!("{}: {} certificates", input.display(), blocks.len());
        found += blocks.len();

        for der in blocks {
            // The same root in two bundles is the ordinary case when somebody
            // passes both the system store and an extra file.
            if !seen.insert(der.clone()) {
                duplicates += 1;
                continue;
            }
            match judge(&der, now) {
                Verdict::Kept { name, about } => {
                    verdicts.push(Verdict::Kept {
                        name: name.clone(),
                        about,
                    });
                    kept.push(der);
                }
                dropped => verdicts.push(dropped),
            }
        }
    }

    report(&verdicts, found, duplicates);

    if listing {
        return Ok(kept.len());
    }
    let Some(out) = out else {
        return Ok(kept.len());
    };

    let bundle = nexus_tls::roots::write(&kept).map_err(|why| format!("cannot write: {why}"))?;

    // Read it back with the reader the machine uses, before it is written
    // anywhere. A bundle that this tool can write and the machine cannot read
    // is the one failure that would not show up until it was on a disk image.
    let loaded = nexus_tls::roots::read(&bundle)
        .map_err(|why| format!("the bundle this wrote will not read back: {why}"))?;
    if loaded.roots.len() != kept.len() || !loaded.skipped.is_empty() {
        return Err(format!(
            "the bundle this wrote holds {} roots and {} were put in it",
            loaded.roots.len(),
            kept.len()
        ));
    }

    if let Some(directory) = out.parent() {
        if !directory.as_os_str().is_empty() {
            std::fs::create_dir_all(directory)
                .map_err(|why| format!("cannot make {}: {why}", directory.display()))?;
        }
    }
    std::fs::write(out, &bundle).map_err(|why| format!("cannot write {}: {why}", out.display()))?;
    println!(
        "\nwrote {} -- {} roots, {} bytes",
        out.display(),
        kept.len(),
        bundle.len()
    );
    Ok(kept.len())
}

/// Decide whether one certificate belongs in the store.
fn judge(der: &[u8], now: i64) -> Verdict {
    let certificate = match x509::parse(der) {
        Ok(certificate) => certificate,
        Err(why) => {
            return Verdict::Dropped {
                name: short_name(der),
                why: format!("{why}"),
            }
        }
    };
    let name = describe(&certificate.subject);

    if !certificate.is_authority {
        return Verdict::Dropped {
            name,
            why: String::from("not a certificate authority"),
        };
    }
    if certificate.may_sign_certificates == Some(false) {
        return Verdict::Dropped {
            name,
            why: String::from("keyUsage does not allow signing certificates"),
        };
    }
    if certificate.not_after < now {
        return Verdict::Dropped {
            name,
            why: String::from("expired"),
        };
    }
    if certificate.not_before > now {
        return Verdict::Dropped {
            name,
            why: String::from("not valid yet"),
        };
    }

    // The key is what an intermediate's signature will be checked against, so a
    // key type this machine cannot verify with is a root that could never
    // finish a chain. Dropping it here is the difference between one line in
    // this report and a site that will not load for reasons nobody can see.
    let key = match &certificate.key {
        PublicKey::Rsa { modulus, .. } => format!("RSA-{}", modulus.len() * 8),
        PublicKey::P256 { .. } => String::from("P-256"),
    };
    let signed = match certificate.algorithm {
        Algorithm::RsaPkcs1Sha256 => "RSA/SHA-256",
        Algorithm::RsaPkcs1Sha384 => "RSA/SHA-384",
        Algorithm::RsaPss => "RSA-PSS",
        Algorithm::EcdsaP256Sha256 => "ECDSA/SHA-256",
        Algorithm::EcdsaP256Sha384 => "ECDSA/SHA-384",
    };

    let mut about = format!("{key}, self-signed with {signed}");
    if certificate.not_after - now < SOON {
        about.push_str(&format!(", expires in {} days", (certificate.not_after - now) / 86_400));
    }
    Verdict::Kept { name, about }
}

/// Say what happened, in full.
fn report(verdicts: &[Verdict], found: usize, duplicates: usize) {
    let mut dropped: Vec<(&str, &str)> = Vec::new();
    let mut kept = 0usize;
    let mut expiring: Vec<&str> = Vec::new();

    for verdict in verdicts {
        match verdict {
            Verdict::Kept { name, about } => {
                kept += 1;
                if about.contains("expires in") {
                    expiring.push(name);
                }
            }
            Verdict::Dropped { name, why } => dropped.push((name, why)),
        }
    }

    println!("\n{found} read, {kept} kept, {} dropped", dropped.len());
    if duplicates > 0 {
        println!("{duplicates} were the same certificate twice");
    }

    if !dropped.is_empty() {
        // Grouped by reason, because the interesting question is "what can this
        // machine not do yet", and that is a list of four things rather than a
        // list of forty certificates.
        let mut reasons: std::collections::BTreeMap<&str, Vec<&str>> =
            std::collections::BTreeMap::new();
        for (name, why) in &dropped {
            reasons.entry(why).or_default().push(name);
        }
        println!("\nwhat was dropped, and why:");
        for (why, names) in reasons {
            println!("  {} x  {why}", names.len());
            for name in names.iter().take(4) {
                println!("          {name}");
            }
            if names.len() > 4 {
                println!("          and {} more", names.len() - 4);
            }
        }
    }

    if !expiring.is_empty() {
        println!("\nexpiring within six months, so this store wants rebuilding:");
        for name in expiring {
            println!("  {name}");
        }
    }
}

/// A readable name out of an encoded distinguished name.
///
/// A distinguished name is a `SEQUENCE` of `SET`s of (type, value) pairs, and
/// this walks it and keeps the values. It would be shorter to scan for runs of
/// printable bytes and join them -- that was the first version, and it printed
/// `example.test1, NexusOS Test`, because the `SET` tag is `0x31`, which is the
/// digit one. A report somebody reads to decide what their machine trusts
/// should not have the encoding showing through it.
///
/// It is still not a full DN implementation: attribute types are ignored, so
/// this says `Example CA, GB` rather than `CN=Example CA, C=GB`. Nothing
/// depends on it -- names are compared as bytes, over in `x509` -- and it only
/// has to be good enough to tell two authorities apart.
fn describe(encoded: &[u8]) -> String {
    let mut pieces: Vec<String> = Vec::new();
    collect(encoded, 0, &mut pieces);
    if pieces.is_empty() {
        String::from("(an unreadable name)")
    } else {
        pieces.join(", ")
    }
}

/// Walk a DER structure, keeping every text value.
///
/// `depth` bounds it, because the input is a file and a file can be shaped like
/// anything. Anything that will not read stops that branch rather than failing:
/// this is a label, and a certificate with an odd name is still a certificate
/// the parser proper has already had its say about.
fn collect(bytes: &[u8], depth: u32, into: &mut Vec<String>) {
    /// The string tags a distinguished name may use.
    const UTF8: u8 = 0x0C;
    const PRINTABLE: u8 = 0x13;
    const T61: u8 = 0x14;
    const IA5: u8 = 0x16;

    if depth > 8 {
        return;
    }
    let mut reader = nexus_tls::der::Reader::new(bytes);
    while !reader.done() {
        let Ok(value) = reader.any() else { return };
        match value.tag {
            UTF8 | PRINTABLE | T61 | IA5 => {
                let text = String::from_utf8_lossy(value.body).trim().to_string();
                if !text.is_empty() {
                    into.push(text);
                }
            }
            // Constructed, so there is more inside: bit five of the tag says so.
            tag if tag & 0x20 != 0 => collect(value.body, depth + 1, into),
            _ => {}
        }
    }
}

/// Something to call a certificate that would not parse at all.
fn short_name(der: &[u8]) -> String {
    format!("(unparsed, {} bytes)", der.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The crate's own fixture: a real certificate, made by OpenSSL.
    const LEAF: &[u8] = include_bytes!("../../../shared/nexus-tls/fixtures/rsa-leaf.der");

    #[test]
    fn a_self_signed_authority_is_kept_and_described() {
        // The fixture is self-signed with CA:TRUE, which is what a root is.
        match judge(LEAF, 1_790_000_000) {
            Verdict::Kept { name, about } => {
                assert_eq!(name, "example.test, NexusOS Test");
                assert!(about.starts_with("RSA-2048"), "{about}");
            }
            Verdict::Dropped { name, why } => panic!("{name} should be kept: {why}"),
        }
    }

    #[test]
    fn an_authority_that_has_expired_is_dropped() {
        // The same certificate, judged after it runs out in 2036. A store full
        // of expired authorities is a store that fails for obscure reasons.
        match judge(LEAF, 2_200_000_000) {
            Verdict::Dropped { why, .. } => assert_eq!(why, "expired"),
            Verdict::Kept { name, .. } => panic!("{name} expired in 2036"),
        }
    }

    #[test]
    fn an_authority_that_is_not_valid_yet_is_dropped() {
        match judge(LEAF, 1_000_000_000) {
            Verdict::Dropped { why, .. } => assert_eq!(why, "not valid yet"),
            Verdict::Kept { name, .. } => panic!("{name} was not issued until 2026"),
        }
    }

    #[test]
    fn an_expiry_coming_up_is_said_out_loud() {
        // The fixture runs out at 2104808255; this is ninety days before that,
        // inside the six months [`SOON`] covers. Somebody rebuilding the store
        // wants to know before the day it stops working, not after.
        match judge(LEAF, 2_097_032_255) {
            Verdict::Kept { about, .. } => assert!(about.contains("expires in"), "{about}"),
            Verdict::Dropped { why, .. } => panic!("should still be valid: {why}"),
        }
    }

    #[test]
    fn rubbish_is_dropped_with_the_parser_s_own_words() {
        match judge(&[0x30, 0x03, 0x02, 0x01, 0x00], 1_790_000_000) {
            Verdict::Dropped { name, why } => {
                assert!(name.contains("unparsed"));
                assert!(!why.is_empty());
            }
            Verdict::Kept { .. } => panic!("that is not a certificate"),
        }
    }

    #[test]
    fn a_name_comes_out_without_the_encoding_in_it() {
        // A real RDN: SEQUENCE { SET { SEQUENCE { OID 2.5.4.10, "Example CA" } } }.
        // The SET tag is 0x31, which is the digit one, and a decoder that
        // scanned for printable bytes would put it in the middle of the answer.
        let encoded = b"\x30\x15\x31\x13\x30\x11\x06\x03\x55\x04\x0a\x13\x0aExample CA";
        assert_eq!(describe(encoded), "Example CA");
    }

    #[test]
    fn a_name_with_nothing_readable_in_it_still_says_something() {
        assert_eq!(describe(&[0x30, 0x00]), "(an unreadable name)");
        assert_eq!(describe(&[]), "(an unreadable name)");
    }

    #[test]
    fn a_name_that_is_not_der_at_all_does_not_panic() {
        // It comes from a file, so it can be shaped like anything.
        assert_eq!(describe(&[0xFF, 0xFF, 0xFF]), "(an unreadable name)");
    }
}
