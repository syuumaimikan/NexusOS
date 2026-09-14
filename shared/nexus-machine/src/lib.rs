//! What the machine is doing, as the kernel's service reports it.
//!
//! One request and one reply. This crate is the shape of both, written once so
//! that the kernel and every program that asks agree about where each number
//! is — a fixed-width record is exactly the kind of thing that gets written in
//! one file, read in another, and disagrees by four bytes.
//!
//! # The wire
//!
//! ```text
//! ask    "syst" <version:u16>
//! reply  "ok  " <version:u16> <length:u16> <payload>
//!        "err!" <code:u16>
//! ```
//!
//! Payload, version 1, seventy-six bytes, all little-endian:
//!
//! | Offset | Size | Field | Unit |
//! | --- | --- | --- | --- |
//! | 0 | 8 | `taken_at` | milliseconds since the timer started |
//! | 8 | 8 | `memory_total` | bytes |
//! | 16 | 8 | `memory_free` | bytes |
//! | 24 | 8 | `heap_used` | bytes |
//! | 32 | 8 | `heap_total` | bytes |
//! | 40 | 8 | `processes_started` | count since boot |
//! | 48 | 8 | `processes_ended` | count since boot |
//! | 56 | 8 | `context_switches` | count since boot |
//! | 64 | 4 | `processors` | count, online |
//! | 68 | 4 | `processes_running` | count |
//! | 72 | 4 | `threads` | count |
//!
//! Eight eight-byte fields and three four-byte ones: seventy-six bytes. The
//! first draft of this said seventy-two, which is the eight `u64`s and two of
//! the three `u32`s, and the third quietly landed on top of the second. The
//! round-trip test below is what found it, and it found it in the first run.
//!
//! Every count that grows for the life of the machine is eight bytes. A
//! context-switch total reaches thirteen million in five seconds of one
//! self-test; thirty-two bits of it wraps inside a day, and a counter that
//! wraps is a counter whose differences are sometimes enormous and negative.
//!
//! # What these numbers are not
//!
//! They are not one atomic reading. The kernel reads each from where it lives,
//! one after another, and the machine goes on running in between. `taken_at`
//! says when the reading started. Two of them subtracted is an estimate.
//!
//! # What is deliberately absent
//!
//! Process names, and any per-process detail. `BIN/BROWSE.ELF` is a fact about
//! what a person is doing and a list of them is a description of somebody's
//! afternoon. Numbers describe load; names describe a person.

#![no_std]
#![deny(unsafe_op_in_unsafe_fn)]

extern crate alloc;

/// Ask for a snapshot.
pub const ASK: &[u8] = b"syst";
/// The reply worked.
pub const GOOD: &[u8] = b"ok  ";
/// It did not.
pub const BAD: &[u8] = b"err!";

/// The payload layout this crate reads and the kernel writes.
pub const VERSION: u16 = 1;

/// How many bytes version 1 of the payload is.
pub const PAYLOAD: usize = 76;

/// How long a request is: the tag and the version.
pub const REQUEST: usize = 6;

/// The request to send.
#[must_use]
pub fn request() -> [u8; REQUEST] {
    let mut out = [0u8; REQUEST];
    out[..4].copy_from_slice(ASK);
    out[4..6].copy_from_slice(&VERSION.to_le_bytes());
    out
}

/// Why the kernel refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refused {
    /// It does not know that request.
    UnknownRequest,
    /// The request was the wrong shape.
    Malformed,
    /// It does not speak the version that was asked for.
    NoSuchVersion,
    /// A code this crate does not know.
    Other(u16),
}

impl Refused {
    #[must_use]
    pub const fn of(code: u16) -> Self {
        match code {
            1 => Self::UnknownRequest,
            2 => Self::Malformed,
            3 => Self::NoSuchVersion,
            other => Self::Other(other),
        }
    }
}

/// Why a reply could not be read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trouble {
    /// The kernel said no.
    Refused(Refused),
    /// The reply is not one of the two shapes this understands.
    NotAReply,
    /// The reply is a version this crate does not know.
    ///
    /// Refused rather than read as far as it goes. A later version may have
    /// moved a field, and a reader that took the prefix on faith would report a
    /// number that was never true.
    Version(u16),
    /// The reply says a length its payload does not have.
    Truncated,
}

impl core::fmt::Display for Trouble {
    fn fmt(&self, out: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Refused(Refused::UnknownRequest) => {
                out.write_str("the kernel does not know that request")
            }
            Self::Refused(Refused::Malformed) => out.write_str("the request was the wrong shape"),
            Self::Refused(Refused::NoSuchVersion) => {
                out.write_str("the kernel does not speak that version")
            }
            Self::Refused(Refused::Other(code)) => {
                write!(out, "the kernel refused with code {code}")
            }
            Self::NotAReply => out.write_str("that is not a reply from this service"),
            Self::Version(version) => write!(
                out,
                "the reply is version {version}, which this program does not read"
            ),
            Self::Truncated => out.write_str("the reply is shorter than it says it is"),
        }
    }
}

/// One reading of the machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Snapshot {
    /// Milliseconds since the timer started, when this reading began.
    pub taken_at: u64,
    /// Bytes of usable physical memory.
    pub memory_total: u64,
    /// And how many of them are free.
    pub memory_free: u64,
    /// Bytes of kernel heap in use, and how large it is.
    pub heap_used: u64,
    pub heap_total: u64,
    /// Processes created since boot, and ended.
    pub processes_started: u64,
    pub processes_ended: u64,
    /// Context switches since boot.
    pub context_switches: u64,
    /// Processors running.
    pub processors: u32,
    /// Processes that have started and not ended.
    pub processes_running: u32,
    /// Threads the scheduler knows about.
    pub threads: u32,
}

impl Snapshot {
    /// Bytes of physical memory in use.
    #[must_use]
    pub const fn memory_used(&self) -> u64 {
        self.memory_total.saturating_sub(self.memory_free)
    }

    /// How full memory is, in whole percent.
    ///
    /// Integer arithmetic, because this system has no floating point in its
    /// kernel and a percentage that needed one would be a percentage that could
    /// not be computed where it is wanted.
    #[must_use]
    pub const fn memory_percent(&self) -> u64 {
        if self.memory_total == 0 {
            return 0;
        }
        self.memory_used() * 100 / self.memory_total
    }

    /// Read one out of a reply.
    pub fn of(reply: &[u8]) -> Result<Self, Trouble> {
        let Some(tag) = reply.get(..4) else {
            return Err(Trouble::NotAReply);
        };

        if tag == BAD {
            let Some(code) = reply.get(4..6) else {
                return Err(Trouble::NotAReply);
            };
            return Err(Trouble::Refused(Refused::of(u16::from_le_bytes([
                code[0], code[1],
            ]))));
        }
        if tag != GOOD {
            return Err(Trouble::NotAReply);
        }

        let Some(header) = reply.get(4..8) else {
            return Err(Trouble::NotAReply);
        };
        let version = u16::from_le_bytes([header[0], header[1]]);
        if version != VERSION {
            return Err(Trouble::Version(version));
        }
        let length = u16::from_le_bytes([header[2], header[3]]) as usize;
        if length != PAYLOAD {
            return Err(Trouble::Truncated);
        }
        let Some(payload) = reply.get(8..8 + PAYLOAD) else {
            return Err(Trouble::Truncated);
        };

        let at = |offset: usize| -> u64 {
            let mut bytes = [0u8; 8];
            bytes.copy_from_slice(&payload[offset..offset + 8]);
            u64::from_le_bytes(bytes)
        };
        let small = |offset: usize| -> u32 {
            let mut bytes = [0u8; 4];
            bytes.copy_from_slice(&payload[offset..offset + 4]);
            u32::from_le_bytes(bytes)
        };

        Ok(Self {
            taken_at: at(0),
            memory_total: at(8),
            memory_free: at(16),
            heap_used: at(24),
            heap_total: at(32),
            processes_started: at(40),
            processes_ended: at(48),
            context_switches: at(56),
            processors: small(64),
            processes_running: small(68),
            threads: small(72),
        })
    }

    /// Write one as the kernel does. Here so that the layout is tested from
    /// both sides by the same file, which is the only way a fixed-width record
    /// stays agreed upon.
    #[must_use]
    pub fn to_bytes(self) -> [u8; PAYLOAD] {
        let mut out = [0u8; PAYLOAD];
        for (offset, value) in [
            (0, self.taken_at),
            (8, self.memory_total),
            (16, self.memory_free),
            (24, self.heap_used),
            (32, self.heap_total),
            (40, self.processes_started),
            (48, self.processes_ended),
            (56, self.context_switches),
        ] {
            out[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
        }
        for (offset, value) in [
            (64, self.processors),
            (68, self.processes_running),
            (72, self.threads),
        ] {
            out[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn example() -> Snapshot {
        Snapshot {
            taken_at: 1234,
            memory_total: 1016 * 1024 * 1024,
            memory_free: 988 * 1024 * 1024,
            heap_used: 865 * 1024,
            heap_total: 16 * 1024 * 1024,
            processes_started: 19,
            processes_ended: 17,
            context_switches: 13_040_097,
            processors: 4,
            processes_running: 2,
            threads: 13,
        }
    }

    /// One reply built and read back. The point of the test is the offsets: a
    /// record written in the kernel and read in a program is agreed upon only
    /// because both sides use this file.
    #[test]
    fn a_snapshot_survives_the_wire() {
        let snapshot = example();
        let mut reply = alloc::vec::Vec::new();
        reply.extend_from_slice(GOOD);
        reply.extend_from_slice(&VERSION.to_le_bytes());
        reply.extend_from_slice(&(PAYLOAD as u16).to_le_bytes());
        reply.extend_from_slice(&snapshot.to_bytes());
        assert_eq!(Snapshot::of(&reply), Ok(snapshot));
    }

    #[test]
    fn the_request_is_the_tag_and_the_version() {
        let asked = request();
        assert_eq!(&asked[..4], ASK);
        assert_eq!(u16::from_le_bytes([asked[4], asked[5]]), VERSION);
        assert_eq!(asked.len(), REQUEST);
    }

    #[test]
    fn a_refusal_reads_as_one() {
        for (code, expected) in [
            (1u16, Refused::UnknownRequest),
            (2, Refused::Malformed),
            (3, Refused::NoSuchVersion),
            (9, Refused::Other(9)),
        ] {
            let mut reply = alloc::vec::Vec::new();
            reply.extend_from_slice(BAD);
            reply.extend_from_slice(&code.to_le_bytes());
            assert_eq!(Snapshot::of(&reply), Err(Trouble::Refused(expected)));
        }
    }

    #[test]
    fn a_version_this_does_not_know_is_refused_rather_than_read() {
        let snapshot = example();
        let mut reply = alloc::vec::Vec::new();
        reply.extend_from_slice(GOOD);
        reply.extend_from_slice(&7u16.to_le_bytes());
        reply.extend_from_slice(&(PAYLOAD as u16).to_le_bytes());
        reply.extend_from_slice(&snapshot.to_bytes());
        assert_eq!(Snapshot::of(&reply), Err(Trouble::Version(7)));
    }

    #[test]
    fn a_short_reply_is_refused() {
        let snapshot = example();
        let mut reply = alloc::vec::Vec::new();
        reply.extend_from_slice(GOOD);
        reply.extend_from_slice(&VERSION.to_le_bytes());
        reply.extend_from_slice(&(PAYLOAD as u16).to_le_bytes());
        reply.extend_from_slice(&snapshot.to_bytes()[..PAYLOAD - 4]);
        assert_eq!(Snapshot::of(&reply), Err(Trouble::Truncated));

        // And one that says a length it does not mean.
        let mut wrong = alloc::vec::Vec::new();
        wrong.extend_from_slice(GOOD);
        wrong.extend_from_slice(&VERSION.to_le_bytes());
        wrong.extend_from_slice(&8u16.to_le_bytes());
        wrong.extend_from_slice(&snapshot.to_bytes());
        assert_eq!(Snapshot::of(&wrong), Err(Trouble::Truncated));
    }

    #[test]
    fn rubbish_is_not_a_reply() {
        assert_eq!(Snapshot::of(b""), Err(Trouble::NotAReply));
        assert_eq!(Snapshot::of(b"ab"), Err(Trouble::NotAReply));
        assert_eq!(Snapshot::of(b"what"), Err(Trouble::NotAReply));
        assert_eq!(Snapshot::of(b"ok  "), Err(Trouble::NotAReply));
    }

    #[test]
    fn the_derived_numbers_are_right() {
        let snapshot = example();
        assert_eq!(snapshot.memory_used(), 28 * 1024 * 1024);
        assert_eq!(snapshot.memory_percent(), 2);

        // And a machine that reported nothing does not divide by zero.
        assert_eq!(Snapshot::default().memory_percent(), 0);
    }

    /// The layout is a promise. If this test is changed, the kernel changed
    /// too, and the version went up.
    #[test]
    fn the_payload_is_seventy_six_bytes() {
        assert_eq!(PAYLOAD, 76);
        assert_eq!(example().to_bytes().len(), PAYLOAD);
    }
}
