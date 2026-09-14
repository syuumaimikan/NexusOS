//! DNS: a question, an answer, and what to believe of it.
//!
//! Only the part a client needs. This asks for the address of a name and reads
//! what comes back; it does not serve, does not sign, does not cache across
//! boots and speaks no record type but `A` and the `CNAME` it has to follow to
//! find one.
//!
//! # Why it is here and not in the kernel
//!
//! The kernel carries what needs the card, which is a bound port and a way to
//! send a datagram. Everything in this file is an opinion: which server to ask,
//! how long to wait, how many times to try, whether a name with dots in it is a
//! name at all or an address somebody typed. Opinions in the kernel are
//! opinions nothing can replace.
//!
//! # Compression, and why it is the interesting part
//!
//! A DNS message may write a name once and then point at it, with a two-byte
//! pointer whose top two bits are set. Answers use it constantly -- the name in
//! the answer is nearly always a pointer back to the name in the question -- so
//! a reader that did not follow pointers would fail on almost every real reply.
//!
//! A pointer can also point backwards into a name that contains another
//! pointer, and a message from a stranger can point at itself. So every jump is
//! counted and the count is small: a reader that followed pointers until they
//! stopped could be made to loop for ever by sixteen bytes from anybody.

#![no_std]
#![deny(unsafe_op_in_unsafe_fn)]

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;

/// The port a resolver listens on.
pub const PORT: u16 = 53;

/// The longest name this will ask about.
///
/// The protocol's own limit. A name longer than this cannot be encoded, so
/// refusing it here is refusing it where the reason is obvious.
pub const MAX_NAME: usize = 255;

/// The longest label inside a name.
pub const MAX_LABEL: usize = 63;

/// How many compression pointers one name may follow.
///
/// Enough for any message a server produces and few enough that a message which
/// points at itself ends in a refusal rather than a loop.
const MAX_JUMPS: usize = 8;

/// Record types this understands.
mod kind {
    /// An IPv4 address.
    pub const A: u16 = 1;
    /// Another name to ask about instead.
    pub const CNAME: u16 = 5;
}

/// The class every record here is in.
const CLASS_INTERNET: u16 = 1;

/// What went wrong.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// The name cannot be put in a question.
    BadName(&'static str),
    /// The reply stopped in the middle of something.
    Truncated,
    /// The reply is not an answer to the question that was asked.
    NotOurs,
    /// The server said the name does not exist.
    NoSuchName,
    /// The server said something else went wrong, with its code.
    ServerSaidNo(u8),
    /// The reply was well formed and had no address in it.
    NoAddress,
    /// A name pointed at itself, or close enough.
    Looping,
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::BadName(why) => write!(f, "that name cannot be looked up: {why}"),
            Self::Truncated => f.write_str("the answer stopped half way"),
            Self::NotOurs => f.write_str("that answer was to a different question"),
            Self::NoSuchName => f.write_str("there is no such name"),
            Self::ServerSaidNo(code) => write!(f, "the resolver refused, code {code}"),
            Self::NoAddress => f.write_str("that name has no address"),
            Self::Looping => f.write_str("the answer points at itself"),
        }
    }
}

/// Whether some text is an address written out rather than a name.
///
/// Checked before anything is asked, because `10.0.2.2` is not a question for a
/// resolver -- it is the answer, and sending it would be waiting for a server to
/// tell this machine what it already knows.
#[must_use]
pub fn as_address(text: &str) -> Option<[u8; 4]> {
    let mut parts = text.split('.');
    let mut address = [0u8; 4];
    for slot in &mut address {
        let part = parts.next()?;
        if part.is_empty() || part.len() > 3 {
            return None;
        }
        *slot = part.parse::<u8>().ok()?;
    }
    if parts.next().is_some() {
        return None;
    }
    Some(address)
}

/// Write a question about `name`, to be sent as one datagram.
///
/// `id` is what the answer has to carry back. A caller that reused one would be
/// unable to tell an answer to this question from an answer to the last one,
/// which is how a resolver is made to hand out the wrong address.
///
/// # Errors
///
/// If the name is empty, too long, or has a label that is.
pub fn question(id: u16, name: &str) -> Result<Vec<u8>, Error> {
    let name = name.trim().trim_end_matches('.');
    if name.is_empty() {
        return Err(Error::BadName("it is empty"));
    }
    if name.len() > MAX_NAME {
        return Err(Error::BadName("it is too long"));
    }

    let mut message = Vec::with_capacity(32 + name.len());
    message.extend_from_slice(&id.to_be_bytes());
    // Recursion desired, and nothing else. This machine wants an answer, not a
    // referral to another server it would then have to ask.
    message.extend_from_slice(&0x0100u16.to_be_bytes());
    message.extend_from_slice(&1u16.to_be_bytes()); // one question
    message.extend_from_slice(&0u16.to_be_bytes()); // no answers
    message.extend_from_slice(&0u16.to_be_bytes()); // no authority
    message.extend_from_slice(&0u16.to_be_bytes()); // no additional

    for label in name.split('.') {
        if label.is_empty() {
            return Err(Error::BadName("it has an empty piece"));
        }
        if label.len() > MAX_LABEL {
            return Err(Error::BadName("one of its pieces is too long"));
        }
        message.push(label.len() as u8);
        message.extend_from_slice(label.as_bytes());
    }
    message.push(0);

    message.extend_from_slice(&kind::A.to_be_bytes());
    message.extend_from_slice(&CLASS_INTERNET.to_be_bytes());
    Ok(message)
}

/// A big-endian `u16` at an offset, if the message is long enough.
fn be16(bytes: &[u8], at: usize) -> Option<u16> {
    let slice = bytes.get(at..at + 2)?;
    Some(u16::from_be_bytes([slice[0], slice[1]]))
}

/// Read a name, following compression pointers, and say where the *encoding*
/// ended.
///
/// The two are different numbers whenever a pointer was followed: the name
/// carries on somewhere else, and what the caller has to skip is the two bytes
/// of the pointer, not the whole of what it led to. Returning the wrong one
/// walks the reader into the middle of a record.
fn read_name(message: &[u8], start: usize) -> Result<(String, usize), Error> {
    let mut name = String::new();
    let mut at = start;
    let mut after: Option<usize> = None;
    let mut jumps = 0;

    loop {
        let length = *message.get(at).ok_or(Error::Truncated)? as usize;

        // The root label, which ends every name.
        if length == 0 {
            at += 1;
            break;
        }

        // A pointer: the top two bits set, and the remaining fourteen are where
        // the name carries on.
        if length & 0xC0 == 0xC0 {
            let pointer = be16(message, at).ok_or(Error::Truncated)? as usize & 0x3FFF;
            jumps += 1;
            if jumps > MAX_JUMPS {
                return Err(Error::Looping);
            }
            // Only the first jump decides where the caller carries on from:
            // after that, the reader is off in somebody else's name.
            if after.is_none() {
                after = Some(at + 2);
            }
            // Backwards only. A pointer that led forwards could point at itself
            // through a chain the jump count would eventually catch, and this
            // catches it immediately.
            if pointer >= at {
                return Err(Error::Looping);
            }
            at = pointer;
            continue;
        }

        if length > MAX_LABEL {
            return Err(Error::Truncated);
        }
        let label = message
            .get(at + 1..at + 1 + length)
            .ok_or(Error::Truncated)?;
        if !name.is_empty() {
            name.push('.');
        }
        // Names are ASCII in practice and case-insensitive by rule. Anything
        // else is passed through as it came rather than guessed at: a label
        // this cannot read is still a label the *server* matched on.
        for byte in label {
            name.push(byte.to_ascii_lowercase() as char);
        }
        at += 1 + length;
    }

    Ok((name, after.unwrap_or(at)))
}

/// What an answer said.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Answer {
    /// Every address in it, in the order they appeared.
    pub addresses: Vec<[u8; 4]>,
    /// The name the answer was finally about, after following any aliases.
    pub name: String,
}

/// Read an answer to the question `id` asked about `name`.
///
/// # Errors
///
/// If the message is truncated, is an answer to something else, says the name
/// does not exist, or carries no address.
pub fn answer(message: &[u8], id: u16, name: &str) -> Result<Answer, Error> {
    if message.len() < 12 {
        return Err(Error::Truncated);
    }
    if be16(message, 0) != Some(id) {
        return Err(Error::NotOurs);
    }
    let flags = be16(message, 2).ok_or(Error::Truncated)?;
    // The reply bit. A message that is still a question is not an answer,
    // whatever else it says.
    if flags & 0x8000 == 0 {
        return Err(Error::NotOurs);
    }
    match (flags & 0x000F) as u8 {
        0 => {}
        3 => return Err(Error::NoSuchName),
        code => return Err(Error::ServerSaidNo(code)),
    }

    let questions = be16(message, 4).ok_or(Error::Truncated)?;
    let answers = be16(message, 6).ok_or(Error::Truncated)?;

    // The question is read back and checked rather than skipped. A resolver
    // that answered about a different name -- by accident or otherwise -- would
    // otherwise have its address believed.
    let mut at = 12;
    let wanted = name.trim().trim_end_matches('.').to_ascii_lowercase();
    for index in 0..questions {
        let (asked, after) = read_name(message, at)?;
        at = after + 4;
        if index == 0 && asked != wanted {
            return Err(Error::NotOurs);
        }
    }

    // Aliases are followed by name: a `CNAME` says "ask about this instead",
    // and the address records for it are in the same message. Following by
    // name rather than by position is what makes the order of the records not
    // matter, and servers do put them in whatever order they like.
    let mut following = wanted;
    let mut addresses = Vec::new();
    let mut aliases: Vec<(String, String)> = Vec::new();

    for _ in 0..answers {
        let (owner, after) = read_name(message, at)?;
        at = after;
        let record = be16(message, at).ok_or(Error::Truncated)?;
        let class = be16(message, at + 2).ok_or(Error::Truncated)?;
        let length = be16(message, at + 8).ok_or(Error::Truncated)? as usize;
        at += 10;
        let data = message.get(at..at + length).ok_or(Error::Truncated)?;
        at += length;

        if class != CLASS_INTERNET {
            continue;
        }
        match record {
            kind::A if length == 4 => {
                addresses.push(([data[0], data[1], data[2], data[3]], owner));
            }
            kind::CNAME => {
                let (target, _) = read_name(message, at - length)?;
                aliases.push((owner, target));
            }
            _ => {}
        }
    }

    // Walk the chain of aliases, bounded for the same reason the pointers are.
    for _ in 0..MAX_JUMPS {
        let Some((_, target)) = aliases.iter().find(|(owner, _)| *owner == following) else {
            break;
        };
        following = target.clone();
    }

    let matching: Vec<[u8; 4]> = addresses
        .iter()
        .filter(|(_, owner)| *owner == following)
        .map(|(address, _)| *address)
        .collect();

    // Anything at all, if nothing matched the name. A server that returns
    // exactly one address record under a name this reader did not manage to
    // follow to is still a server that answered the question, and refusing it
    // would be refusing a working lookup on a technicality.
    let found = if matching.is_empty() {
        addresses.iter().map(|(address, _)| *address).collect()
    } else {
        matching
    };

    if found.is_empty() {
        return Err(Error::NoAddress);
    }
    Ok(Answer {
        addresses: found,
        name: following,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    /// Build an answer the way a server would, so the reader is tested against
    /// the format rather than against the way this file happens to write it.
    struct Builder {
        bytes: Vec<u8>,
    }

    impl Builder {
        fn new(id: u16, flags: u16, questions: u16, answers: u16) -> Self {
            let mut bytes = Vec::new();
            bytes.extend_from_slice(&id.to_be_bytes());
            bytes.extend_from_slice(&flags.to_be_bytes());
            bytes.extend_from_slice(&questions.to_be_bytes());
            bytes.extend_from_slice(&answers.to_be_bytes());
            bytes.extend_from_slice(&0u16.to_be_bytes());
            bytes.extend_from_slice(&0u16.to_be_bytes());
            Self { bytes }
        }

        fn name(&mut self, name: &str) -> &mut Self {
            for label in name.split('.') {
                self.bytes.push(label.len() as u8);
                self.bytes.extend_from_slice(label.as_bytes());
            }
            self.bytes.push(0);
            self
        }

        fn pointer(&mut self, to: usize) -> &mut Self {
            self.bytes
                .extend_from_slice(&(0xC000u16 | to as u16).to_be_bytes());
            self
        }

        fn question(&mut self, name: &str) -> &mut Self {
            self.name(name);
            self.bytes.extend_from_slice(&1u16.to_be_bytes());
            self.bytes.extend_from_slice(&1u16.to_be_bytes());
            self
        }

        fn record(&mut self, kind: u16, data: &[u8]) -> &mut Self {
            self.bytes.extend_from_slice(&kind.to_be_bytes());
            self.bytes.extend_from_slice(&1u16.to_be_bytes());
            self.bytes.extend_from_slice(&300u32.to_be_bytes());
            self.bytes
                .extend_from_slice(&(data.len() as u16).to_be_bytes());
            self.bytes.extend_from_slice(data);
            self
        }
    }

    #[test]
    fn an_address_is_not_a_question() {
        assert_eq!(as_address("10.0.2.2"), Some([10, 0, 2, 2]));
        assert_eq!(as_address("255.255.255.255"), Some([255, 255, 255, 255]));
        assert_eq!(as_address("example.com"), None);
        assert_eq!(as_address("10.0.2"), None);
        assert_eq!(as_address("10.0.2.2.2"), None);
        assert_eq!(as_address("10.0.2.256"), None);
        assert_eq!(as_address("10.0..2"), None);
    }

    #[test]
    fn a_question_is_labels_and_a_root() {
        let asked = question(0x1234, "example.com").expect("askable");
        assert_eq!(&asked[0..2], &[0x12, 0x34]);
        // Recursion desired.
        assert_eq!(&asked[2..4], &[0x01, 0x00]);
        assert_eq!(&asked[4..6], &[0x00, 0x01]);
        assert_eq!(&asked[12..], b"\x07example\x03com\x00\x00\x01\x00\x01");
    }

    #[test]
    fn a_trailing_dot_is_the_same_name() {
        assert_eq!(
            question(1, "example.com.").expect("askable"),
            question(1, "example.com").expect("askable")
        );
    }

    #[test]
    fn a_name_that_cannot_be_asked_about_is_refused() {
        assert!(matches!(question(1, ""), Err(Error::BadName(_))));
        assert!(matches!(question(1, "a..b"), Err(Error::BadName(_))));
        let long = "x".repeat(64);
        assert!(matches!(question(1, &long), Err(Error::BadName(_))));
    }

    #[test]
    fn an_answer_carries_the_address() {
        let mut builder = Builder::new(7, 0x8180, 1, 1);
        builder.question("example.com");
        builder.pointer(12);
        builder.record(kind::A, &[93, 184, 216, 34]);
        let read = answer(&builder.bytes, 7, "example.com").expect("readable");
        assert_eq!(read.addresses, vec![[93, 184, 216, 34]]);
    }

    #[test]
    fn a_compressed_name_is_followed() {
        // The answer's owner is a pointer back into the question, which is what
        // nearly every real server sends and what a reader that ignored
        // pointers would fail on.
        let mut builder = Builder::new(9, 0x8180, 1, 1);
        builder.question("www.example.com");
        builder.pointer(12);
        builder.record(kind::A, &[1, 2, 3, 4]);
        let read = answer(&builder.bytes, 9, "www.example.com").expect("readable");
        assert_eq!(read.addresses, vec![[1, 2, 3, 4]]);
        assert_eq!(read.name, "www.example.com");
    }

    #[test]
    fn an_alias_is_followed_to_its_address() {
        let mut builder = Builder::new(11, 0x8180, 1, 2);
        builder.question("www.example.com");
        // www.example.com CNAME host.example.com
        builder.pointer(12);
        let mut target = Vec::new();
        for label in "host.example.com".split('.') {
            target.push(label.len() as u8);
            target.extend_from_slice(label.as_bytes());
        }
        target.push(0);
        builder.record(kind::CNAME, &target);
        // host.example.com A 5.6.7.8
        builder.name("host.example.com");
        builder.record(kind::A, &[5, 6, 7, 8]);

        let read = answer(&builder.bytes, 11, "www.example.com").expect("readable");
        assert_eq!(read.addresses, vec![[5, 6, 7, 8]]);
        assert_eq!(read.name, "host.example.com");
    }

    #[test]
    fn an_answer_to_a_different_question_is_refused() {
        let mut builder = Builder::new(3, 0x8180, 1, 1);
        builder.question("elsewhere.example");
        builder.pointer(12);
        builder.record(kind::A, &[1, 1, 1, 1]);
        // Right identifier, wrong name.
        assert_eq!(
            answer(&builder.bytes, 3, "example.com"),
            Err(Error::NotOurs)
        );
        // Right name, wrong identifier -- which is the one that matters, since
        // it is what an off-path answer cannot guess.
        assert_eq!(
            answer(&builder.bytes, 4, "elsewhere.example"),
            Err(Error::NotOurs)
        );
    }

    #[test]
    fn a_question_is_not_an_answer() {
        let mut builder = Builder::new(5, 0x0100, 1, 0);
        builder.question("example.com");
        assert_eq!(
            answer(&builder.bytes, 5, "example.com"),
            Err(Error::NotOurs)
        );
    }

    #[test]
    fn no_such_name_is_said_as_such() {
        let mut builder = Builder::new(6, 0x8183, 1, 0);
        builder.question("nowhere.example");
        assert_eq!(
            answer(&builder.bytes, 6, "nowhere.example"),
            Err(Error::NoSuchName)
        );
    }

    #[test]
    fn an_answer_with_no_address_says_so() {
        let mut builder = Builder::new(8, 0x8180, 1, 0);
        builder.question("example.com");
        assert_eq!(
            answer(&builder.bytes, 8, "example.com"),
            Err(Error::NoAddress)
        );
    }

    #[test]
    fn a_name_that_points_at_itself_is_refused_rather_than_followed() {
        let mut builder = Builder::new(2, 0x8180, 1, 1);
        builder.question("example.com");
        // A pointer to itself: the answer's owner name is at offset 29 and
        // points at 29. A reader without the check loops for ever on this.
        let here = builder.bytes.len();
        builder
            .bytes
            .extend_from_slice(&(0xC000u16 | here as u16).to_be_bytes());
        builder.record(kind::A, &[1, 2, 3, 4]);
        assert_eq!(
            answer(&builder.bytes, 2, "example.com"),
            Err(Error::Looping)
        );
    }

    #[test]
    fn a_message_that_stops_half_way_is_refused() {
        let mut builder = Builder::new(1, 0x8180, 1, 1);
        builder.question("example.com");
        builder.pointer(12);
        builder.record(kind::A, &[1, 2, 3, 4]);
        for cut in 12..builder.bytes.len() {
            // Every truncation is an error and none of them is a panic, which
            // is the property that matters: this reads bytes from a stranger.
            let _ = answer(&builder.bytes[..cut], 1, "example.com");
        }
        assert!(answer(&builder.bytes[..14], 1, "example.com").is_err());
    }

    #[test]
    fn several_addresses_all_come_back() {
        let mut builder = Builder::new(12, 0x8180, 1, 2);
        builder.question("many.example");
        builder.pointer(12);
        builder.record(kind::A, &[1, 1, 1, 1]);
        builder.pointer(12);
        builder.record(kind::A, &[2, 2, 2, 2]);
        let read = answer(&builder.bytes, 12, "many.example").expect("readable");
        assert_eq!(read.addresses, vec![[1, 1, 1, 1], [2, 2, 2, 2]]);
    }
}
