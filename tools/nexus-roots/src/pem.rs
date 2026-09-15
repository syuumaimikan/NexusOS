//! Reading PEM, strictly.
//!
//! PEM is base64 between two marker lines. That is the whole format, and it
//! would be twenty lines if it were not for the decision made here: this
//! decoder **refuses** input that a lenient one would accept.
//!
//! # Why strict
//!
//! A lenient base64 decoder skips characters it does not recognise. Point one
//! at a file that has been half-converted between line endings, or truncated in
//! the middle of a certificate, or had a diff marker left in it, and it returns
//! bytes -- fewer bytes, different bytes, but bytes, which then fail to parse
//! as DER with a message about tags and lengths that has nothing to do with
//! what went wrong.
//!
//! Since what comes out of here becomes the list of authorities this machine
//! trusts, "something went wrong and here is roughly where" is worth far more
//! than a best effort. So: only the alphabet, padding only at the end, and a
//! length that is a whole number of groups.

/// Pull every `CERTIFICATE` block out of a PEM file, in order.
///
/// Other block types -- keys, parameters, trust settings -- are skipped
/// silently. A system bundle with a private key alongside the certificates is
/// an ordinary thing, and refusing the file over it would help nobody.
///
/// # Errors
///
/// A block that begins and never ends, or one whose base64 is not base64.
pub fn certificates(text: &str) -> Result<Vec<Vec<u8>>, String> {
    const BEGIN: &str = "-----BEGIN CERTIFICATE-----";
    const END: &str = "-----END CERTIFICATE-----";

    let mut out = Vec::new();
    let mut body = String::new();
    let mut inside = false;

    for (number, line) in text.lines().enumerate() {
        let line = line.trim();
        if line == BEGIN {
            if inside {
                return Err(format!("line {}: a second BEGIN inside a block", number + 1));
            }
            inside = true;
            body.clear();
        } else if line == END {
            if !inside {
                return Err(format!("line {}: an END with no BEGIN", number + 1));
            }
            inside = false;
            let der = decode(&body)
                .map_err(|why| format!("in the block ending at line {}: {why}", number + 1))?;
            if der.is_empty() {
                return Err(format!("line {}: an empty certificate", number + 1));
            }
            out.push(der);
        } else if inside {
            body.push_str(line);
        }
    }

    if inside {
        return Err(String::from("a certificate begins and never ends"));
    }
    Ok(out)
}

/// Decode base64, refusing anything that is not exactly base64.
///
/// # Errors
///
/// A character outside the alphabet, padding anywhere but the end, or a length
/// that is not a multiple of four.
pub fn decode(text: &str) -> Result<Vec<u8>, String> {
    let bytes = text.as_bytes();
    if !bytes.len().is_multiple_of(4) {
        return Err(format!(
            "{} characters of base64, which is not a whole number of groups",
            bytes.len()
        ));
    }

    let mut out = Vec::with_capacity(bytes.len() / 4 * 3);
    let (groups, _) = bytes.as_chunks::<4>();
    let mut groups = groups.iter().peekable();
    while let Some(group) = groups.next() {
        let last = groups.peek().is_none();
        let mut value = 0u32;
        let mut padding = 0usize;
        for (index, &character) in group.iter().enumerate() {
            if character == b'=' {
                // Padding is only ever the last one or two characters of the
                // last group. Anywhere else it means the file is damaged.
                if !last || index < 2 {
                    return Err(String::from("padding in the middle"));
                }
                padding += 1;
                value <<= 6;
                continue;
            }
            if padding > 0 {
                return Err(String::from("a character after the padding"));
            }
            let six = sextet(character)
                .ok_or_else(|| format!("{:?} is not base64", character as char))?;
            value = (value << 6) | u32::from(six);
        }
        let whole = value.to_be_bytes();
        out.extend_from_slice(&whole[1..4 - padding]);
    }
    Ok(out)
}

/// One base64 character as its six bits.
fn sextet(character: u8) -> Option<u8> {
    match character {
        b'A'..=b'Z' => Some(character - b'A'),
        b'a'..=b'z' => Some(character - b'a' + 26),
        b'0'..=b'9' => Some(character - b'0' + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_rfc_4648_vectors() {
        // The published ones, which between them exercise both amounts of
        // padding and none.
        for (encoded, plain) in [
            ("", ""),
            ("Zg==", "f"),
            ("Zm8=", "fo"),
            ("Zm9v", "foo"),
            ("Zm9vYg==", "foob"),
            ("Zm9vYmE=", "fooba"),
            ("Zm9vYmFy", "foobar"),
        ] {
            assert_eq!(decode(encoded).expect(encoded), plain.as_bytes(), "{encoded}");
        }
    }

    #[test]
    fn every_byte_round_trips() {
        // Against a table built here rather than against this decoder, so the
        // test cannot agree with a mistake in it.
        const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
        let plain: Vec<u8> = (0..=255u8).collect();
        let mut encoded = String::new();
        for group in plain.chunks(3) {
            let mut value = 0u32;
            for index in 0..3 {
                value = (value << 8) | u32::from(group.get(index).copied().unwrap_or(0));
            }
            for index in 0..4 {
                if index <= group.len() {
                    let six = (value >> (18 - index * 6)) & 0x3F;
                    encoded.push(ALPHABET[six as usize] as char);
                } else {
                    encoded.push('=');
                }
            }
        }
        assert_eq!(decode(&encoded).expect("decodable"), plain);
    }

    #[test]
    fn a_character_that_is_not_base64_is_refused_rather_than_skipped() {
        // The case the whole file exists for. A lenient decoder returns
        // "foobar" here and the damage is never noticed.
        let why = decode("Zm9v*mFy").expect_err("refused");
        assert!(why.contains("not base64"), "{why}");
    }

    #[test]
    fn whitespace_inside_a_group_is_not_ignored() {
        assert!(decode("Zm9 vYmFy").is_err());
    }

    #[test]
    fn a_truncated_group_is_refused() {
        let why = decode("Zm9vYmF").expect_err("refused");
        assert!(why.contains("whole number of groups"), "{why}");
    }

    #[test]
    fn padding_in_the_middle_is_refused() {
        assert!(decode("Zg==Zg==").is_err());
        assert!(decode("Z===").is_err());
    }

    #[test]
    fn a_whole_pem_file_reads() {
        let text = "\
# a comment some bundles carry
-----BEGIN CERTIFICATE-----
Zm9vYmFy
-----END CERTIFICATE-----
-----BEGIN CERTIFICATE-----
Zm9v
YmFy
-----END CERTIFICATE-----
";
        let blocks = certificates(text).expect("readable");
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0], b"foobar");
        // Wrapped across lines is the ordinary case, and joins to the same
        // thing.
        assert_eq!(blocks[1], b"foobar");
    }

    #[test]
    fn other_block_types_are_skipped() {
        let text = "\
-----BEGIN PRIVATE KEY-----
Zm9vYmFy
-----END PRIVATE KEY-----
-----BEGIN CERTIFICATE-----
Zm9vYmFy
-----END CERTIFICATE-----
";
        assert_eq!(certificates(text).expect("readable").len(), 1);
    }

    #[test]
    fn a_block_that_never_ends_is_refused() {
        let text = "-----BEGIN CERTIFICATE-----\nZm9vYmFy\n";
        let why = certificates(text).expect_err("refused");
        assert!(why.contains("never ends"), "{why}");
    }

    #[test]
    fn carriage_returns_do_not_break_it() {
        // The file may well have come from a machine that ends lines the other
        // way, and that is not damage.
        let text = "-----BEGIN CERTIFICATE-----\r\nZm9vYmFy\r\n-----END CERTIFICATE-----\r\n";
        assert_eq!(certificates(text).expect("readable")[0], b"foobar");
    }

    #[test]
    fn a_file_with_no_certificates_in_it_is_not_an_error_here() {
        // The caller decides what an empty bundle means; this only reports
        // what was in the file.
        assert!(certificates("nothing to see").expect("readable").is_empty());
    }
}
