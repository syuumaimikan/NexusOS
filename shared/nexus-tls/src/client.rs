//! The client: what to send, what to expect, and when to believe it.
//!
//! # It does no input and output
//!
//! Nothing here opens a socket or reads one. The caller feeds it bytes that
//! arrived and takes bytes to send, which is what lets the whole handshake be
//! tested against a real server's recorded traffic — and what lets it run on a
//! machine whose networking is a channel to a kernel service rather than a
//! file descriptor.
//!
//! ```text
//! let mut client = Client::start(host, roots, now, random)?;
//! send(client.take_outgoing());
//! loop {
//!     client.received(&bytes_from_the_network)?;
//!     send(client.take_outgoing());
//!     if client.ready() { break; }
//! }
//! ```
//!
//! # The order of messages is not negotiable
//!
//! TLS 1.3 sends the server's flight in a fixed order, and this refuses
//! anything else. A client that accepted `Finished` before `CertificateVerify`
//! would accept a connection nobody had proved they could speak for — and the
//! whole point of the sequence is that each message is only meaningful after
//! the ones before it.
//!
//! # What it will not do
//!
//! * continue when the certificate chain does not verify;
//! * continue when the server's `CertificateVerify` does not verify;
//! * continue when the server's `Finished` does not match;
//! * send anything at all before the server has proved who it is.

use alloc::string::String;
use alloc::vec::Vec;

use nexus_crypto::{sha256, x25519};

use crate::chain::{self, Roots};
use crate::handshake::{self, Kind};
use crate::record::{self, Sealed};
use crate::schedule::{self, Keys, Transcript};

/// Why a connection could not be made, or could not continue.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Trouble {
    /// The machine could not produce an unguessable key.
    ///
    /// Refused rather than worked around. A TLS connection whose private key an
    /// attacker can guess is a connection that looks secure and is not.
    NoRandomness,
    /// A record would not read or would not decrypt.
    Record(record::Trouble),
    /// A handshake message would not read, or was not one this expects.
    Handshake(handshake::Trouble),
    /// The certificate chain was not believed.
    Chain(chain::Trouble),
    /// The server's proof that it holds the leaf's private key did not verify.
    BadCertificateVerify,
    /// The server's `Finished` did not match, which means the handshake was
    /// tampered with somewhere it would otherwise not show.
    BadFinished,
    /// A message arrived that does not belong at this point in the handshake.
    OutOfOrder(&'static str),
    /// The peer sent an alert.
    Alert { level: u8, description: u8 },
    /// The host name is not one that can be put in a `server_name`.
    BadHost,
    /// The peer's key share is unusable, which for X25519 means a small-order
    /// point.
    BadKeyShare,
    /// More bytes arrived than a handshake should ever need.
    TooMuch,
}

impl core::fmt::Display for Trouble {
    fn fmt(&self, out: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::NoRandomness => out.write_str(
                "this machine cannot make an unguessable key, so it will not pretend \
                 to make a secure connection",
            ),
            Self::Record(why) => write!(out, "{why}"),
            Self::Handshake(why) => write!(out, "{why}"),
            Self::Chain(why) => write!(out, "{why}"),
            Self::BadCertificateVerify => {
                out.write_str("the server did not prove it holds the key in its certificate")
            }
            Self::BadFinished => {
                out.write_str("the handshake was altered in flight and the check caught it")
            }
            Self::OutOfOrder(what) => write!(out, "the server sent {what} out of order"),
            Self::Alert { level, description } => {
                write!(out, "the server sent alert {description} (level {level})")
            }
            Self::BadHost => out.write_str("that host name cannot be put in a TLS hello"),
            Self::BadKeyShare => out.write_str("the server's key share is one this must not use"),
            Self::TooMuch => out.write_str("the server sent more than a handshake ever needs"),
        }
    }
}

impl From<record::Trouble> for Trouble {
    fn from(why: record::Trouble) -> Self {
        Self::Record(why)
    }
}
impl From<handshake::Trouble> for Trouble {
    fn from(why: handshake::Trouble) -> Self {
        Self::Handshake(why)
    }
}
impl From<chain::Trouble> for Trouble {
    fn from(why: chain::Trouble) -> Self {
        Self::Chain(why)
    }
}

/// Where the handshake has got to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    /// Waiting for `ServerHello`.
    Hello,
    /// Waiting for `EncryptedExtensions`.
    Extensions,
    /// Waiting for `Certificate`.
    Certificate,
    /// Waiting for `CertificateVerify`.
    Verify,
    /// Waiting for the server's `Finished`.
    Finished,
    /// Done. Application data may be sent and received.
    Ready,
}

/// The most bytes a handshake may take before this gives up.
///
/// A certificate chain is a few kilobytes. A hundred is far beyond anything
/// real and bounds what a server can make this machine hold.
const MOST_HANDSHAKE: usize = 100 * 1024;

/// A TLS 1.3 client connection.
pub struct Client {
    step: Step,
    host: String,
    roots: Roots,
    now: Option<i64>,

    /// This connection's ephemeral private key, until the shared secret is made.
    private: Option<[u8; 32]>,
    transcript: Transcript,

    /// Bytes waiting to be sent.
    outgoing: Vec<u8>,
    /// Record bytes that arrived and have not been framed yet.
    incoming: Vec<u8>,
    /// Handshake bytes that have been decrypted and not yet parsed.
    ///
    /// Separate because a handshake message is not aligned to a record: one
    /// `Certificate` routinely spans several, and a client that parsed
    /// record-by-record would fail on every real server.
    pending: Vec<u8>,

    /// The keys for the handshake, then for the application.
    sending: Option<Sealed>,
    receiving: Option<Sealed>,
    /// The secrets, kept until the handshake is over because `Finished` and the
    /// application keys are both derived from them.
    handshake_secrets: Option<schedule::Handshake>,

    /// Plaintext the application has not taken yet.
    plaintext: Vec<u8>,
    /// The leaf certificate, between `Certificate` and `CertificateVerify`.
    ///
    /// Taken rather than copied when it is used, so a second
    /// `CertificateVerify` has nothing to check against.
    leaf: Option<Vec<u8>>,
}

impl Client {
    /// Begin a handshake with `host`.
    ///
    /// `random` must be thirty-two unguessable bytes and `private` another
    /// thirty-two. Both come from the caller because this crate has no source
    /// of randomness and must not invent one.
    pub fn start(
        host: &str,
        roots: Roots,
        now: Option<i64>,
        random: [u8; 32],
        private: [u8; 32],
    ) -> Result<Self, Trouble> {
        // A caller that handed over zeros is a caller whose generator failed
        // and did not say so. Refused here as well as there, because this is
        // the last place it can be caught.
        if random == [0u8; 32] || private == [0u8; 32] {
            return Err(Trouble::NoRandomness);
        }
        let Some(name) = handshake::sni_name(host) else {
            return Err(Trouble::BadHost);
        };

        let public = x25519::public(&private);
        let Some(hello) = handshake::client_hello(&random, &public, &name) else {
            return Err(Trouble::BadHost);
        };

        let mut transcript = Transcript::new();
        transcript.add(&hello);

        Ok(Self {
            step: Step::Hello,
            host: name,
            roots,
            now,
            private: Some(private),
            transcript,
            outgoing: record::frame(record::Kind::Handshake, &hello),
            incoming: Vec::new(),
            pending: Vec::new(),
            sending: None,
            receiving: None,
            handshake_secrets: None,
            plaintext: Vec::new(),
            leaf: None,
        })
    }

    /// Whether the handshake is finished and the server has been believed.
    #[must_use]
    pub fn ready(&self) -> bool {
        self.step == Step::Ready
    }

    /// Take whatever is waiting to be sent.
    #[must_use]
    pub fn take_outgoing(&mut self) -> Vec<u8> {
        core::mem::take(&mut self.outgoing)
    }

    /// Take whatever application data has arrived.
    #[must_use]
    pub fn take_plaintext(&mut self) -> Vec<u8> {
        core::mem::take(&mut self.plaintext)
    }

    /// Send application data. Only once the handshake is done.
    pub fn send(&mut self, data: &[u8]) -> Result<(), Trouble> {
        if self.step != Step::Ready {
            return Err(Trouble::OutOfOrder("data before the handshake finished"));
        }
        let Some(sending) = self.sending.as_mut() else {
            return Err(Trouble::OutOfOrder("data with no keys"));
        };
        for piece in data.chunks(record::MOST_PLAINTEXT) {
            let sealed = sealed_record(sending, record::Kind::ApplicationData, piece);
            self.outgoing.extend_from_slice(&sealed);
        }
        Ok(())
    }

    /// Feed bytes that arrived from the network.
    pub fn received(&mut self, bytes: &[u8]) -> Result<(), Trouble> {
        self.incoming.extend_from_slice(bytes);
        if self.incoming.len() > MOST_HANDSHAKE && self.step != Step::Ready {
            return Err(Trouble::TooMuch);
        }

        loop {
            let record = match record::read(&self.incoming) {
                Ok(record) => record,
                Err(record::Trouble::Incomplete) => return Ok(()),
                Err(why) => return Err(why.into()),
            };
            let mut header = [0u8; record::HEADER];
            header.copy_from_slice(&self.incoming[..record::HEADER]);
            let consumed = record.consumed;

            let (kind, body) = match self.receiving.as_mut() {
                // Before the keys exist, records are in the clear.
                None => (record.kind, record.body),
                Some(receiving) => {
                    // A ChangeCipherSpec after the keys are up is the
                    // "compatibility mode" middleboxes expect. RFC 8446 §5 says
                    // to drop it, and dropping it means not counting it: it is
                    // not encrypted and has no sequence number.
                    if record.kind == record::Kind::ChangeCipherSpec {
                        self.incoming.drain(..consumed);
                        continue;
                    }
                    let mut body = record.body;
                    receiving.open(&header, &mut body)?
                }
            };
            self.incoming.drain(..consumed);

            match kind {
                record::Kind::ChangeCipherSpec => {}
                record::Kind::Alert => {
                    let level = body.first().copied().unwrap_or(2);
                    let description = body.get(1).copied().unwrap_or(0);
                    // close_notify at warning level is an orderly shutdown
                    // rather than a failure.
                    if description == 0 {
                        return Ok(());
                    }
                    return Err(Trouble::Alert { level, description });
                }
                record::Kind::Handshake => {
                    self.pending.extend_from_slice(&body);
                    self.drain_handshake()?;
                }
                record::Kind::ApplicationData => {
                    if self.step != Step::Ready {
                        return Err(Trouble::OutOfOrder("application data"));
                    }
                    self.plaintext.extend_from_slice(&body);
                }
            }
        }
    }

    /// Parse every whole handshake message that has arrived.
    fn drain_handshake(&mut self) -> Result<(), Trouble> {
        loop {
            let (kind, body, whole) = {
                let Some(message) = handshake::read(&self.pending)? else {
                    return Ok(());
                };
                (message.kind, message.body.to_vec(), message.whole.to_vec())
            };
            self.pending.drain(..whole.len());
            self.one_message(kind, &body, &whole)?;
        }
    }

    /// Act on one handshake message.
    fn one_message(&mut self, kind: Kind, body: &[u8], whole: &[u8]) -> Result<(), Trouble> {
        match (self.step, kind) {
            (Step::Hello, Kind::ServerHello) => {
                let hello = handshake::server_hello(body)?;
                self.transcript.add(whole);

                let Some(private) = self.private.take() else {
                    return Err(Trouble::OutOfOrder("a second ServerHello"));
                };
                let Some(shared) = x25519::shared(&private, &hello.share) else {
                    return Err(Trouble::BadKeyShare);
                };

                // From here the handshake is encrypted.
                let secrets = schedule::Early::new().with_shared(&shared, &self.transcript.hash());
                self.receiving = Some(Sealed::new(Keys::from_secret(&secrets.server)));
                self.sending = Some(Sealed::new(Keys::from_secret(&secrets.client)));
                self.handshake_secrets = Some(secrets);
                self.step = Step::Extensions;
                Ok(())
            }

            (Step::Extensions, Kind::EncryptedExtensions) => {
                // Nothing in here is acted on: this client asks for no
                // extension whose answer changes what it does. Added to the
                // transcript, because every handshake message is.
                self.transcript.add(whole);
                self.step = Step::Certificate;
                Ok(())
            }

            // A server may ask for a client certificate. This has none, and
            // saying so is a message this client does not send -- so the
            // request is refused rather than ignored, because ignoring it
            // leaves the server waiting.
            (Step::Certificate, Kind::CertificateRequest) => {
                Err(Trouble::OutOfOrder("a request for a client certificate"))
            }

            (Step::Certificate, Kind::Certificate) => {
                let chain = handshake::certificates(body)?;
                // Hashed *before* the chain is judged, so that the transcript
                // is right either way -- and so that a rejection happens with
                // the handshake in a consistent state.
                self.transcript.add(whole);

                let der: Vec<Vec<u8>> = chain.into_iter().map(|one| one.der).collect();
                chain::verify(&der, &self.roots, &self.host, self.now)?;

                // Kept for CertificateVerify, which is checked against the
                // leaf's key.
                self.leaf = Some(der[0].clone());
                self.step = Step::Verify;
                Ok(())
            }

            (Step::Verify, Kind::CertificateVerify) => {
                let signed = self.transcript.hash();
                let parsed = handshake::certificate_verify(body)?;
                self.transcript.add(whole);

                let Some(leaf) = self.leaf.take() else {
                    return Err(Trouble::OutOfOrder(
                        "a CertificateVerify with no certificate",
                    ));
                };
                let certificate = crate::x509::parse(&leaf)
                    .map_err(|why| chain::Trouble::Unreadable(alloc::format!("{why}")))?;

                let context = handshake::verify_context(&signed);
                if !signature_matches(&certificate.key, parsed.scheme, &parsed.signature, &context)
                {
                    return Err(Trouble::BadCertificateVerify);
                }
                self.step = Step::Finished;
                Ok(())
            }

            (Step::Finished, Kind::Finished) => {
                let Some(secrets) = self.handshake_secrets.as_ref() else {
                    return Err(Trouble::OutOfOrder("a Finished with no keys"));
                };
                let expected = schedule::finished(&secrets.server, &self.transcript.hash());
                if !sha256::same(&expected, body) {
                    return Err(Trouble::BadFinished);
                }
                self.transcript.add(whole);

                // The client's own Finished, under the handshake keys, before
                // the keys change.
                let ours = schedule::finished(&secrets.client, &self.transcript.hash());
                let message = handshake::frame(Kind::Finished, &ours);
                if let Some(sending) = self.sending.as_mut() {
                    let sealed = sealed_record(sending, record::Kind::Handshake, &message);
                    self.outgoing.extend_from_slice(&sealed);
                }

                // And now the application keys, over the transcript up to and
                // including the *server's* Finished -- not the client's, which
                // is the one boundary in the whole schedule that is easy to get
                // one message wrong.
                let application = secrets.finish(&self.transcript.hash());
                self.sending = Some(Sealed::new(Keys::from_secret(&application.client)));
                self.receiving = Some(Sealed::new(Keys::from_secret(&application.server)));
                self.handshake_secrets = None;
                self.step = Step::Ready;
                Ok(())
            }

            // After the handshake a server may send session tickets and key
            // updates. Tickets are ignored because this client does not resume;
            // a key update is not handled and is refused rather than silently
            // desynchronising the connection.
            (Step::Ready, Kind::NewSessionTicket) => Ok(()),
            (Step::Ready, Kind::KeyUpdate) => Err(Trouble::OutOfOrder("a key update")),

            (_, kind) => Err(Trouble::OutOfOrder(name_of(kind))),
        }
    }
}

/// Seal one record, which is a method on `Sealed` and is here because the
/// borrow checker will not let `self.outgoing` and `self.sending` be touched at
/// once inside a method.
fn sealed_record(sending: &mut Sealed, kind: record::Kind, body: &[u8]) -> Vec<u8> {
    sending.seal(kind, body)
}

/// Whether a `CertificateVerify` signature is right, for the scheme it names.
///
/// The scheme must match the key: a server that named an RSA scheme over an
/// elliptic-curve key is one whose signature cannot be checked, and guessing
/// which it meant would be a client deciding for it.
fn signature_matches(
    key: &crate::x509::PublicKey,
    scheme: u16,
    signature: &[u8],
    message: &[u8],
) -> bool {
    use crate::handshake::scheme as s;
    use crate::x509::PublicKey;

    match (scheme, key) {
        (s::ECDSA_P256_SHA256, PublicKey::P256 { point }) => {
            crate::p256::verify(point, signature, message, false).is_ok()
        }
        (s::RSA_PSS_RSAE_SHA256, PublicKey::Rsa { modulus, exponent }) => {
            crate::rsa::verify_pss(modulus, exponent, signature, message).is_ok()
        }
        // RFC 8446 §4.2.3 forbids PKCS#1 v1.5 in CertificateVerify, even though
        // it is what most certificates are *signed with*. A server that used it
        // here is not speaking TLS 1.3, and accepting it would undo the reason
        // the restriction exists.
        _ => false,
    }
}

/// What to call a handshake message in an error.
fn name_of(kind: Kind) -> &'static str {
    match kind {
        Kind::ClientHello => "a ClientHello",
        Kind::ServerHello => "a ServerHello",
        Kind::NewSessionTicket => "a session ticket",
        Kind::EncryptedExtensions => "EncryptedExtensions",
        Kind::Certificate => "a Certificate",
        Kind::CertificateRequest => "a CertificateRequest",
        Kind::CertificateVerify => "a CertificateVerify",
        Kind::Finished => "a Finished",
        Kind::KeyUpdate => "a key update",
    }
}
