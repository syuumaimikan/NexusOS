//! The TLS 1.3 key schedule: RFC 8446 §7.1.
//!
//! Everything a connection is protected with comes out of this, and every one
//! of the derivations has to match the other end exactly. A schedule that is
//! subtly wrong does not degrade — the handshake simply fails, or, if the peer
//! made the same mistake, succeeds and protects nothing.
//!
//! ```text
//!              0
//!              |
//!    PSK ->  HKDF-Extract  =  Early Secret
//!              |
//!        Derive-Secret(., "derived", "")
//!              |
//!  (EC)DHE ->  HKDF-Extract  =  Handshake Secret
//!              |
//!              +--> Derive-Secret(., "c hs traffic", ClientHello..ServerHello)
//!              +--> Derive-Secret(., "s hs traffic", ClientHello..ServerHello)
//!              |
//!        Derive-Secret(., "derived", "")
//!              |
//!        0 ->  HKDF-Extract  =  Master Secret
//!              |
//!              +--> Derive-Secret(., "c ap traffic", ClientHello..server Finished)
//!              +--> Derive-Secret(., "s ap traffic", ClientHello..server Finished)
//! ```
//!
//! # Why every step has its own name here
//!
//! The specification's diagram is a chain of extracts and derives, and it would
//! be shorter to write as one function with a loop. It is written out because
//! *which transcript* goes into each derivation is the part everybody gets
//! wrong: "c hs traffic" is over ClientHello..ServerHello and not over the
//! whole handshake so far, and using the wrong one gives a key that is
//! plausible, deterministic and useless.
//!
//! # No PSK, no early data, no resumption
//!
//! The PSK arms of the schedule are absent, and so are the branches that hang
//! off them: binders, early traffic secrets, resumption. A client that only
//! ever does a full handshake needs none of them, and the roadmap says they are
//! not here rather than this file pretending.

use nexus_crypto::sha256::{self, Sha256, DIGEST};

/// The key a record is encrypted with. ChaCha20-Poly1305's is always 32 bytes.
pub const KEY: usize = 32;

/// The per-connection IV. Twelve bytes, which the sequence number is folded
/// into to make each record's nonce.
pub const IV: usize = 12;

/// The all-zero secret, used where the schedule says "0".
const ZEROS: [u8; DIGEST] = [0; DIGEST];

/// `Derive-Secret(Secret, Label, Messages)`, RFC 8446 §7.1.
///
/// `transcript` is the hash of the messages, not the messages — because the
/// caller has a running hash and handing it the bytes again would mean keeping
/// every byte of the handshake.
#[must_use]
pub fn derive(secret: &[u8; DIGEST], label: &str, transcript: &[u8; DIGEST]) -> [u8; DIGEST] {
    let mut out = [0u8; DIGEST];
    // Cannot fail: the label is short, the context is a digest, and the output
    // is one digest. Written as an assertion rather than ignored so that a
    // future label long enough to break it is caught here.
    let worked = sha256::expand_label(secret, label, transcript, &mut out);
    debug_assert!(worked, "a key schedule label was too long");
    out
}

/// `Derive-Secret(Secret, Label, "")`, which is the same thing over an empty
/// transcript.
#[must_use]
pub fn derive_empty(secret: &[u8; DIGEST], label: &str) -> [u8; DIGEST] {
    derive(secret, label, &sha256::digest(&[]))
}

/// The traffic keys for one direction, from that direction's secret.
#[derive(Clone)]
pub struct Keys {
    pub key: [u8; KEY],
    pub iv: [u8; IV],
}

impl Keys {
    /// RFC 8446 §7.3.
    #[must_use]
    pub fn from_secret(secret: &[u8; DIGEST]) -> Self {
        let mut key = [0u8; KEY];
        let mut iv = [0u8; IV];
        sha256::expand_label(secret, "key", &[], &mut key);
        sha256::expand_label(secret, "iv", &[], &mut iv);
        Self { key, iv }
    }

    /// The nonce for record number `sequence`.
    ///
    /// RFC 8446 §5.3: the sequence number, big-endian, padded to the IV's
    /// length on the left, exclusive-ored with the IV. Not concatenated — an
    /// implementation that appends it instead produces a nonce of the wrong
    /// length and a connection that never works, which is at least loud.
    #[must_use]
    pub fn nonce(&self, sequence: u64) -> [u8; IV] {
        let mut nonce = self.iv;
        let counter = sequence.to_be_bytes();
        for (slot, byte) in nonce[IV - 8..].iter_mut().zip(counter.iter()) {
            *slot ^= byte;
        }
        nonce
    }
}

/// The running hash of every handshake message, in order.
///
/// Kept incrementally, and cloned whenever a digest is wanted, because the
/// schedule needs the transcript at four different points and the hash carries
/// on past each of them.
#[derive(Clone, Default)]
pub struct Transcript(Sha256);

impl Transcript {
    #[must_use]
    pub fn new() -> Self {
        Self(Sha256::new())
    }

    /// Add one handshake message, **including its four-byte header**.
    ///
    /// The transcript is over the handshake structures as they appear on the
    /// wire, not over their bodies. Leaving the header out is the single most
    /// common way to get a handshake that fails at `Finished` with no clue why.
    pub fn add(&mut self, message: &[u8]) {
        self.0.update(message);
    }

    /// The hash of everything so far.
    #[must_use]
    pub fn hash(&self) -> [u8; DIGEST] {
        self.0.clone().finish()
    }
}

/// Where the schedule has got to.
///
/// A type per stage rather than one struct with everything optional, because
/// the stages happen in an order and a secret used before it exists is a bug
/// this shape makes impossible rather than one a check would catch.
pub struct Early([u8; DIGEST]);

/// After the key exchange: the handshake is encrypted from here on.
pub struct Handshake {
    secret: [u8; DIGEST],
    pub client: [u8; DIGEST],
    pub server: [u8; DIGEST],
}

/// After the server's Finished: application data is encrypted with these.
pub struct Application {
    pub client: [u8; DIGEST],
    pub server: [u8; DIGEST],
}

impl Early {
    /// The start of the schedule, with no pre-shared key.
    ///
    /// `HKDF-Extract(salt = 0, IKM = 0)`. The result is a constant — the same
    /// on every connection that does not resume — which is why it can be
    /// checked against a published value.
    #[must_use]
    pub fn new() -> Self {
        Self(sha256::extract(&[], &ZEROS))
    }

    /// Mix in the shared secret from the key exchange.
    #[must_use]
    pub fn with_shared(self, shared: &[u8], transcript: &[u8; DIGEST]) -> Handshake {
        let derived = derive_empty(&self.0, "derived");
        let secret = sha256::extract(&derived, shared);
        Handshake {
            secret,
            client: derive(&secret, "c hs traffic", transcript),
            server: derive(&secret, "s hs traffic", transcript),
        }
    }
}

impl Default for Early {
    fn default() -> Self {
        Self::new()
    }
}

impl Handshake {
    /// Move on to the application secrets.
    ///
    /// `transcript` is over ClientHello..server Finished — *including* the
    /// server's Finished and not including the client's, which is the one
    /// boundary in the whole schedule that is easy to get one message wrong.
    #[must_use]
    pub fn finish(&self, transcript: &[u8; DIGEST]) -> Application {
        let derived = derive_empty(&self.secret, "derived");
        let master = sha256::extract(&derived, &ZEROS);
        Application {
            client: derive(&master, "c ap traffic", transcript),
            server: derive(&master, "s ap traffic", transcript),
        }
    }
}

/// The `verify_data` in a Finished message, RFC 8446 §4.4.4.
///
/// `HMAC(HKDF-Expand-Label(base, "finished", "", 32), transcript)`.
#[must_use]
pub fn finished(base: &[u8; DIGEST], transcript: &[u8; DIGEST]) -> [u8; DIGEST] {
    let mut key = [0u8; DIGEST];
    sha256::expand_label(base, "finished", &[], &mut key);
    sha256::hmac(&key, transcript)
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::string::String;
    use alloc::vec::Vec;

    fn unhex(text: &str) -> Vec<u8> {
        let clean: Vec<u8> = text
            .bytes()
            .filter(|byte| !byte.is_ascii_whitespace())
            .collect();
        clean
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| {
                let digit = |byte: u8| match byte {
                    b'0'..=b'9' => byte - b'0',
                    b'a'..=b'f' => byte - b'a' + 10,
                    b'A'..=b'F' => byte - b'A' + 10,
                    _ => panic!("not hexadecimal"),
                };
                digit(pair[0]) * 16 + digit(pair[1])
            })
            .collect()
    }

    fn fixed(text: &str) -> [u8; DIGEST] {
        let bytes = unhex(text);
        let mut out = [0u8; DIGEST];
        out.copy_from_slice(&bytes);
        out
    }

    fn hex(bytes: &[u8]) -> String {
        use core::fmt::Write as _;
        let mut text = String::new();
        for byte in bytes {
            write!(text, "{byte:02x}").unwrap();
        }
        text
    }

    #[test]
    fn the_early_secret_with_no_psk_is_the_published_constant() {
        // Every full handshake starts here, so this value is the same on every
        // connection in the world that does not resume -- which is why it is
        // published and why it is worth checking against rather than against
        // this code's own arithmetic.
        assert_eq!(
            hex(&Early::new().0),
            "33ad0a1c607ec03b09e6cd9893680ce210adf300aa1f2660e1b22e10f170f92a"
        );
    }

    #[test]
    fn the_derived_secret_after_the_early_one_is_the_published_constant() {
        // Derive-Secret(Early, "derived", "") -- also a constant for the same
        // reason, and the value that the shared secret is extracted against.
        assert_eq!(
            hex(&derive_empty(&Early::new().0, "derived")),
            "6f2615a108c702c5678f54fc9dbab69716c076189c48250cebeac3576c3611ba"
        );
    }

    #[test]
    fn the_rfc_8448_traffic_keys() {
        // RFC 8448 §3's worked handshake, which uses **AES-128-GCM** -- so its
        // keys are sixteen bytes and this connection's are thirty-two. The
        // difference is the cipher suite and not the derivation: the same
        // `HKDF-Expand-Label(secret, "key", "", n)` produces both, and asking
        // for sixteen is what makes this comparable with the document.
        //
        // Checked at sixteen on purpose. A test that only exercised the length
        // this code actually uses would have nothing published to check
        // against, and would be this crate agreeing with itself.
        let secret = fixed("b3eddb126e067f35a780b3abf45e2d8f3b1a950738f52e9600746a0e27a55a21");
        let mut key = [0u8; 16];
        assert!(sha256::expand_label(&secret, "key", &[], &mut key));
        assert_eq!(hex(&key), "dbfaa693d1762c5b666af5d950258d01");

        // The IV is twelve bytes in every TLS 1.3 suite, so this one is
        // compared exactly as the connection derives it.
        let keys = Keys::from_secret(&secret);
        assert_eq!(hex(&keys.iv), "5bd3c71b836e0b76bb73265f");
        assert_eq!(keys.key.len(), 32, "ChaCha20's key is thirty-two bytes");

        // And the server's, the same way.
        let secret = fixed("b67b7d690cc16c4e75e54213cb2d37b4e9c912bcded9105d42befd59d391ad38");
        let mut key = [0u8; 16];
        assert!(sha256::expand_label(&secret, "key", &[], &mut key));
        assert_eq!(hex(&key), "3fce516009c21727d0f2e4e86ee403bc");
        assert_eq!(
            hex(&Keys::from_secret(&secret).iv),
            "5d313eb2671276ee13000b30"
        );
    }

    #[test]
    fn the_nonce_folds_the_sequence_number_in_rather_than_appending_it() {
        let keys = Keys {
            key: [0; KEY],
            iv: [0x11; IV],
        };
        // Record zero is the IV unchanged.
        assert_eq!(keys.nonce(0), [0x11; IV]);

        // Record one differs in the last byte only.
        let one = keys.nonce(1);
        assert_eq!(one[..IV - 1], [0x11; IV - 1]);
        assert_eq!(one[IV - 1], 0x10);

        // And a large number reaches further in, right-aligned.
        let big = keys.nonce(0x0102_0304_0506_0708);
        assert_eq!(big[..4], [0x11; 4], "the first four bytes are untouched");
        assert_eq!(big[4], 0x11 ^ 0x01);
        assert_eq!(big[IV - 1], 0x11 ^ 0x08);
    }

    #[test]
    fn the_transcript_is_over_the_bytes_it_was_given() {
        let mut transcript = Transcript::new();
        assert_eq!(transcript.hash(), sha256::digest(&[]));

        transcript.add(b"first");
        transcript.add(b"second");
        assert_eq!(transcript.hash(), sha256::digest(b"firstsecond"));

        // And asking for the hash does not end it: the schedule needs the
        // transcript at four points and the handshake carries on past each.
        let midway = transcript.hash();
        transcript.add(b"third");
        assert_eq!(transcript.hash(), sha256::digest(b"firstsecondthird"));
        assert_eq!(midway, sha256::digest(b"firstsecond"));
    }

    #[test]
    fn the_stages_chain_and_each_one_differs() {
        // Not a vector: a shape check. Every secret in the schedule has to be
        // different from every other, and a copy-and-paste that derived two of
        // them with the same label would show up here and nowhere else until a
        // real server rejected the Finished.
        let shared = [0x42u8; 32];
        let hello = sha256::digest(b"ClientHello..ServerHello");
        let handshake = Early::new().with_shared(&shared, &hello);

        let done = sha256::digest(b"ClientHello..server Finished");
        let application = handshake.finish(&done);

        let all = [
            handshake.client,
            handshake.server,
            application.client,
            application.server,
        ];
        for (index, one) in all.iter().enumerate() {
            for (other_index, other) in all.iter().enumerate() {
                if index != other_index {
                    assert_ne!(one, other, "secrets {index} and {other_index} are the same");
                }
            }
        }
    }

    #[test]
    fn a_different_transcript_gives_different_secrets() {
        // What binds the keys to the handshake that produced them. Without it,
        // an attacker who changed a handshake message would get a connection
        // with the same keys.
        let shared = [0x42u8; 32];
        let one = Early::new().with_shared(&shared, &sha256::digest(b"one"));
        let other = Early::new().with_shared(&shared, &sha256::digest(b"other"));
        assert_ne!(one.client, other.client);
        assert_ne!(one.server, other.server);
    }

    #[test]
    fn a_different_shared_secret_gives_different_keys() {
        let hello = sha256::digest(b"hello");
        let one = Early::new().with_shared(&[1u8; 32], &hello);
        let other = Early::new().with_shared(&[2u8; 32], &hello);
        assert_ne!(one.client, other.client);
    }

    #[test]
    fn finished_depends_on_both_the_key_and_the_transcript() {
        let base = [0x11u8; DIGEST];
        let one = finished(&base, &sha256::digest(b"a"));
        let same = finished(&base, &sha256::digest(b"a"));
        let different_transcript = finished(&base, &sha256::digest(b"b"));
        let different_key = finished(&[0x22u8; DIGEST], &sha256::digest(b"a"));

        assert_eq!(one, same);
        assert_ne!(one, different_transcript);
        assert_ne!(one, different_key);
    }
}
