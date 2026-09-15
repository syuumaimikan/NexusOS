//! The handshake messages: RFC 8446 §4.
//!
//! One `ClientHello` is built here and four messages are parsed —
//! `ServerHello`, `EncryptedExtensions`, `Certificate`, `CertificateVerify` —
//! plus `Finished`, which is thirty-two bytes and needs no structure.
//!
//! # Everything parsed here came from an attacker
//!
//! That is the assumption the whole file is written under. Every field is read
//! through [`crate::wire::Reader`], which cannot panic and cannot run past the
//! end of the buffer, and every length is checked against what is actually
//! there rather than trusted. A TLS client that panics on a malformed
//! `ServerHello` is a TLS client that any server on the internet can stop this
//! machine with.
//!
//! # One suite, one group
//!
//! The `ClientHello` offers `TLS_CHACHA20_POLY1305_SHA256` and X25519 and
//! nothing else, so there is nothing to negotiate and nothing to get wrong in
//! the negotiation. A server that will not meet it says so and the connection
//! ends with a sentence naming what was offered.

use alloc::string::String;
use alloc::vec::Vec;

use crate::wire::{Reader, Short, Writer};

/// TLS 1.3, as the `supported_versions` extension spells it.
pub const VERSION_1_3: u16 = 0x0304;

/// The version every record and hello claims, whatever it really is.
pub const LEGACY_VERSION: u16 = 0x0303;

/// `TLS_CHACHA20_POLY1305_SHA256`.
pub const SUITE: u16 = 0x1303;

/// X25519, as `supported_groups` and `key_share` spell it.
pub const X25519: u16 = 0x001d;

/// The signature schemes this client can actually verify.
///
/// Offered in the order a server should prefer them. A scheme that is offered
/// and cannot be verified is worse than one that is not offered: the server
/// picks it, and the connection fails at the last step of the handshake with a
/// message about a signature rather than about a missing feature.
pub mod scheme {
    /// ECDSA over P-256 with SHA-256.
    pub const ECDSA_P256_SHA256: u16 = 0x0403;
    /// ECDSA over P-384 with SHA-384.
    ///
    /// Offered because a server may hold a P-384 key, and because thirty-seven
    /// of the world's root authorities do -- a chain that ends at one of them
    /// needs this to be verifiable even when the leaf is P-256.
    pub const ECDSA_P384_SHA384: u16 = 0x0503;
    /// RSASSA-PSS with SHA-256, which TLS 1.3 requires for RSA keys.
    pub const RSA_PSS_RSAE_SHA256: u16 = 0x0804;
    /// RSASSA-PKCS1-v1_5 with SHA-256. Not allowed in `CertificateVerify` by
    /// RFC 8446 §4.2.3, but it is what most certificates are *signed with*, so
    /// it has to be offered for the chain to verify.
    pub const RSA_PKCS1_SHA256: u16 = 0x0401;
    /// And the same with SHA-384 and SHA-512, for the same reason.
    pub const RSA_PKCS1_SHA384: u16 = 0x0501;
    pub const RSA_PKCS1_SHA512: u16 = 0x0601;
}

/// What kind of handshake message this is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Kind {
    ClientHello = 1,
    ServerHello = 2,
    NewSessionTicket = 4,
    EncryptedExtensions = 8,
    Certificate = 11,
    CertificateRequest = 13,
    CertificateVerify = 15,
    Finished = 20,
    KeyUpdate = 24,
}

impl Kind {
    #[must_use]
    pub fn from_byte(byte: u8) -> Option<Self> {
        Some(match byte {
            1 => Self::ClientHello,
            2 => Self::ServerHello,
            4 => Self::NewSessionTicket,
            8 => Self::EncryptedExtensions,
            11 => Self::Certificate,
            13 => Self::CertificateRequest,
            15 => Self::CertificateVerify,
            20 => Self::Finished,
            24 => Self::KeyUpdate,
            _ => return None,
        })
    }
}

/// Extension numbers, from the IANA registry.
mod extension {
    pub const SERVER_NAME: u16 = 0;
    pub const SUPPORTED_GROUPS: u16 = 10;
    pub const SIGNATURE_ALGORITHMS: u16 = 13;
    pub const SUPPORTED_VERSIONS: u16 = 43;
    pub const KEY_SHARE: u16 = 51;
}

/// The special `ServerHello.random` that means "start again".
///
/// RFC 8446 §4.1.3. A HelloRetryRequest is a ServerHello with this exact
/// random, and a client that did not know would try to use it as a key share
/// and fail in a way that says nothing.
const HELLO_RETRY: [u8; 32] = [
    0xCF, 0x21, 0xAD, 0x74, 0xE5, 0x9A, 0x61, 0x11, 0xBE, 0x1D, 0x8C, 0x02, 0x1E, 0x65, 0xB8, 0x91,
    0xC2, 0xA2, 0x11, 0x16, 0x7A, 0xBB, 0x8C, 0x5E, 0x07, 0x9E, 0x09, 0xE2, 0xC8, 0xA8, 0x33, 0x9C,
];

/// Why a handshake message could not be used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Trouble {
    /// It ran out partway through.
    Short,
    /// A field says something that cannot be right.
    Malformed(&'static str),
    /// The server picked something that was never offered.
    NotOffered(&'static str),
    /// The server asked to start again, which this client does not do.
    RetryRequested,
    /// A message arrived that this client does not handle.
    Unexpected(u8),
}

impl From<Short> for Trouble {
    fn from(_: Short) -> Self {
        Self::Short
    }
}

impl core::fmt::Display for Trouble {
    fn fmt(&self, out: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Short => out.write_str("a handshake message ended in the middle"),
            Self::Malformed(what) => write!(out, "a handshake message is malformed: {what}"),
            Self::NotOffered(what) => {
                write!(out, "the server chose {what}, which was not offered")
            }
            Self::RetryRequested => out
                .write_str("the server asked for a second hello, which this client does not send"),
            Self::Unexpected(byte) => {
                write!(
                    out,
                    "the server sent handshake message {byte}, which is not expected"
                )
            }
        }
    }
}

/// Wrap a body in the four-byte handshake header.
#[must_use]
pub fn frame(kind: Kind, body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(4 + body.len());
    out.push(kind as u8);
    let length = body.len() as u32;
    out.extend_from_slice(&length.to_be_bytes()[1..]);
    out.extend_from_slice(body);
    out
}

/// One handshake message, read out of a stream of handshake bytes.
pub struct Message<'a> {
    pub kind: Kind,
    /// The body, without the header.
    pub body: &'a [u8],
    /// The whole message including its header, which is what the transcript
    /// hash is over.
    pub whole: &'a [u8],
}

/// Read one handshake message.
///
/// Returns `Ok(None)` when there is not a whole one yet, which is ordinary:
/// handshake messages are not aligned to records and one can span several.
pub fn read(bytes: &[u8]) -> Result<Option<Message<'_>>, Trouble> {
    if bytes.len() < 4 {
        return Ok(None);
    }
    let Some(kind) = Kind::from_byte(bytes[0]) else {
        return Err(Trouble::Unexpected(bytes[0]));
    };
    let length = u32::from_be_bytes([0, bytes[1], bytes[2], bytes[3]]) as usize;
    let end = 4usize.checked_add(length).ok_or(Trouble::Short)?;
    if bytes.len() < end {
        return Ok(None);
    }
    Ok(Some(Message {
        kind,
        body: &bytes[4..end],
        whole: &bytes[..end],
    }))
}

/// Build a `ClientHello`.
///
/// `random` and `public` come from the caller because both must be
/// unpredictable, and this crate has no opinion about where a machine gets
/// unpredictable bytes from -- see `nexus_tls::client` for what it insists on.
///
/// Returns `None` only if the host name is longer than a TLS vector can carry,
/// which is a name no DNS would resolve.
#[must_use]
pub fn client_hello(random: &[u8; 32], public: &[u8; 32], host: &str) -> Option<Vec<u8>> {
    let mut body = Writer::new();
    body.u16(LEGACY_VERSION);
    body.bytes(random);

    // An empty session id. RFC 8446 §4.1.2 allows a client that is not
    // pretending to be TLS 1.2 to send nothing here; the "compatibility mode"
    // 32-byte value exists to get through middleboxes that expect a resumption
    // to look plausible, and this client does not do that dance.
    if !body.vector8(&[]) {
        return None;
    }

    // One cipher suite.
    let mut suites = Writer::new();
    suites.u16(SUITE);
    if !body.vector16(suites.as_bytes()) {
        return None;
    }

    // One compression method, which must be "none". Compression in TLS is what
    // CRIME was.
    if !body.vector8(&[0]) {
        return None;
    }

    let mut extensions = Writer::new();

    // server_name: the host, so a server with several certificates knows which
    // to send. Without it most of the web answers with the wrong one.
    if !host.is_empty() {
        let mut names = Writer::new();
        names.u8(0); // host_name
        if !names.vector16(host.as_bytes()) {
            return None;
        }
        let mut list = Writer::new();
        if !list.vector16(names.as_bytes()) {
            return None;
        }
        extensions.u16(extension::SERVER_NAME);
        if !extensions.vector16(list.as_bytes()) {
            return None;
        }
    }

    // supported_groups: X25519 and nothing else.
    let mut groups = Writer::new();
    groups.u16(X25519);
    let mut body_of = Writer::new();
    if !body_of.vector16(groups.as_bytes()) {
        return None;
    }
    extensions.u16(extension::SUPPORTED_GROUPS);
    if !extensions.vector16(body_of.as_bytes()) {
        return None;
    }

    // signature_algorithms: what this client can actually verify, and nothing
    // it cannot.
    let mut schemes = Writer::new();
    schemes.u16(scheme::ECDSA_P256_SHA256);
    schemes.u16(scheme::ECDSA_P384_SHA384);
    schemes.u16(scheme::RSA_PSS_RSAE_SHA256);
    schemes.u16(scheme::RSA_PKCS1_SHA256);
    // RSA/SHA-384 and RSA/SHA-512 are offered for the same reason PKCS#1
    // SHA-256 is: they cannot appear in CertificateVerify, but certificates in
    // the chain are signed with them and a server chooses what to send partly
    // from this list.
    schemes.u16(scheme::RSA_PKCS1_SHA384);
    schemes.u16(scheme::RSA_PKCS1_SHA512);
    let mut body_of = Writer::new();
    if !body_of.vector16(schemes.as_bytes()) {
        return None;
    }
    extensions.u16(extension::SIGNATURE_ALGORITHMS);
    if !extensions.vector16(body_of.as_bytes()) {
        return None;
    }

    // supported_versions: TLS 1.3 and nothing else. This is the extension that
    // actually chooses the version; the field at the top of the hello says 1.2
    // on every TLS 1.3 connection.
    let mut versions = Writer::new();
    versions.u16(VERSION_1_3);
    let mut body_of = Writer::new();
    if !body_of.vector8(versions.as_bytes()) {
        return None;
    }
    extensions.u16(extension::SUPPORTED_VERSIONS);
    if !extensions.vector16(body_of.as_bytes()) {
        return None;
    }

    // key_share: the public key, offered up front so that one round trip is
    // enough. A server that wanted a different group would send a
    // HelloRetryRequest, which this client refuses by name.
    let mut entry = Writer::new();
    entry.u16(X25519);
    if !entry.vector16(public) {
        return None;
    }
    let mut shares = Writer::new();
    if !shares.vector16(entry.as_bytes()) {
        return None;
    }
    extensions.u16(extension::KEY_SHARE);
    if !extensions.vector16(shares.as_bytes()) {
        return None;
    }

    if !body.vector16(extensions.as_bytes()) {
        return None;
    }
    Some(frame(Kind::ClientHello, body.as_bytes()))
}

/// What a `ServerHello` said.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerHello {
    /// The server's X25519 public key.
    pub share: [u8; 32],
}

/// Parse a `ServerHello` body.
///
/// # Errors
///
/// Every way a server can disagree with what was offered, named. The version,
/// the suite and the group are each checked rather than assumed, because a
/// server that picks something else is a server this client cannot talk to and
/// the reason is worth knowing.
pub fn server_hello(body: &[u8]) -> Result<ServerHello, Trouble> {
    let mut reader = Reader::new(body);
    let _legacy = reader.u16()?;
    let random = reader.take(32)?;
    if random == HELLO_RETRY {
        return Err(Trouble::RetryRequested);
    }
    let _session = reader.vector8()?;

    let suite = reader.u16()?;
    if suite != SUITE {
        return Err(Trouble::NotOffered("a cipher suite"));
    }
    let compression = reader.u8()?;
    if compression != 0 {
        return Err(Trouble::Malformed("a compression method other than none"));
    }

    let mut extensions = reader.nested16()?;
    let mut share = None;
    let mut version = None;

    while !extensions.done() {
        let kind = extensions.u16()?;
        let body = extensions.vector16()?;
        match kind {
            extension::SUPPORTED_VERSIONS => {
                version = Some(Reader::new(body).u16()?);
            }
            extension::KEY_SHARE => {
                let mut entry = Reader::new(body);
                let group = entry.u16()?;
                if group != X25519 {
                    return Err(Trouble::NotOffered("a key exchange group"));
                }
                let key = entry.vector16()?;
                if key.len() != 32 {
                    return Err(Trouble::Malformed("an X25519 share of the wrong length"));
                }
                let mut fixed = [0u8; 32];
                fixed.copy_from_slice(key);
                share = Some(fixed);
            }
            // Everything else is ignored, which is what the specification says
            // to do: a ServerHello may carry extensions a client did not ask
            // about, and refusing them would break on the next revision.
            _ => {}
        }
    }

    // The version lives in the extension, not in the field at the top. A
    // server that does not send it is offering TLS 1.2 or earlier.
    if version != Some(VERSION_1_3) {
        return Err(Trouble::NotOffered("a version older than TLS 1.3"));
    }
    let Some(share) = share else {
        return Err(Trouble::Malformed("a ServerHello with no key share"));
    };
    Ok(ServerHello { share })
}

/// One certificate from the chain, as DER.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Certificate {
    pub der: Vec<u8>,
}

/// Parse a `Certificate` body into the chain it carries, leaf first.
pub fn certificates(body: &[u8]) -> Result<Vec<Certificate>, Trouble> {
    let mut reader = Reader::new(body);
    // The request context, which is empty in a server's Certificate unless the
    // handshake is a post-handshake authentication -- which this client never
    // asks for.
    let context = reader.vector8()?;
    if !context.is_empty() {
        return Err(Trouble::Malformed("a certificate request context"));
    }

    let mut list = Reader::new(reader.vector24()?);
    let mut chain = Vec::new();
    while !list.done() {
        let der = list.vector24()?;
        // Extensions per certificate, which are ignored: the ones defined so
        // far are OCSP stapling and SCTs, and neither changes whether the
        // chain verifies.
        let _ = list.vector16()?;
        if der.is_empty() {
            return Err(Trouble::Malformed("an empty certificate"));
        }
        chain.push(Certificate { der: der.to_vec() });
    }
    if chain.is_empty() {
        return Err(Trouble::Malformed(
            "a certificate message with no certificates",
        ));
    }
    Ok(chain)
}

/// What a `CertificateVerify` said.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CertificateVerify {
    pub scheme: u16,
    pub signature: Vec<u8>,
}

/// Parse a `CertificateVerify` body.
pub fn certificate_verify(body: &[u8]) -> Result<CertificateVerify, Trouble> {
    let mut reader = Reader::new(body);
    let scheme = reader.u16()?;
    let signature = reader.vector16()?.to_vec();
    if !reader.done() {
        return Err(Trouble::Malformed("trailing bytes after a signature"));
    }
    Ok(CertificateVerify { scheme, signature })
}

/// The bytes a `CertificateVerify` signature is over, RFC 8446 §4.4.3.
///
/// Sixty-four spaces, a context string, a zero byte, then the transcript hash.
/// The padding is not decoration: it is what stops a signature made for one
/// purpose being replayed as another, because no other protocol's signed data
/// begins with sixty-four spaces.
#[must_use]
pub fn verify_context(transcript: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(64 + 34 + transcript.len());
    out.extend_from_slice(&[0x20; 64]);
    out.extend_from_slice(b"TLS 1.3, server CertificateVerify");
    out.push(0);
    out.extend_from_slice(transcript);
    out
}

/// The host name, lower-cased, if it is one a `server_name` may carry.
///
/// Refuses anything that is not a plain host name: an address literal, which
/// RFC 6066 forbids in this extension, and anything with a byte a host name
/// cannot have.
#[must_use]
pub fn sni_name(host: &str) -> Option<String> {
    if host.is_empty() || host.len() > 255 {
        return None;
    }
    // An IPv4 literal is all digits and dots. RFC 6066 §3 says literals must
    // not be sent, and a server that gets one either ignores it or refuses.
    if host
        .bytes()
        .all(|byte| byte.is_ascii_digit() || byte == b'.')
    {
        return None;
    }
    if !host
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-' || byte == b'.')
    {
        return None;
    }
    Some(host.to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_client_hello_has_the_shape_the_specification_gives() {
        let hello = client_hello(&[0x11; 32], &[0x22; 32], "example.com").unwrap();

        assert_eq!(hello[0], Kind::ClientHello as u8);
        let length = u32::from_be_bytes([0, hello[1], hello[2], hello[3]]) as usize;
        assert_eq!(length, hello.len() - 4, "the header's length is the body's");

        let mut reader = Reader::new(&hello[4..]);
        assert_eq!(reader.u16().unwrap(), LEGACY_VERSION);
        assert_eq!(reader.take(32).unwrap(), &[0x11; 32]);
        assert!(reader.vector8().unwrap().is_empty(), "no session id");
        assert_eq!(reader.vector16().unwrap(), &SUITE.to_be_bytes());
        assert_eq!(reader.vector8().unwrap(), &[0], "compression must be none");

        // And every extension parses.
        let mut extensions = reader.nested16().unwrap();
        let mut seen = alloc::vec::Vec::new();
        while !extensions.done() {
            seen.push(extensions.u16().unwrap());
            extensions.vector16().unwrap();
        }
        assert!(seen.contains(&extension::SERVER_NAME));
        assert!(seen.contains(&extension::SUPPORTED_GROUPS));
        assert!(seen.contains(&extension::SIGNATURE_ALGORITHMS));
        assert!(seen.contains(&extension::SUPPORTED_VERSIONS));
        assert!(seen.contains(&extension::KEY_SHARE));
        assert!(reader.done(), "nothing after the extensions");
    }

    #[test]
    fn the_key_share_carries_the_public_key_given() {
        let hello = client_hello(&[0; 32], &[0x5a; 32], "x.test").unwrap();
        // Found rather than computed: the position depends on the host name's
        // length, and a test that hard-coded an offset would be a test that
        // breaks when an extension is added.
        let at = hello
            .windows(32)
            .position(|window| window == [0x5a; 32])
            .expect("the public key should be in there");
        // Preceded by the group and a length of 32.
        assert_eq!(&hello[at - 4..at], &[0x00, 0x1d, 0x00, 0x20]);
    }

    #[test]
    fn a_hello_without_a_host_leaves_the_name_out() {
        let hello = client_hello(&[0; 32], &[0; 32], "").unwrap();
        let mut reader = Reader::new(&hello[4..]);
        reader.u16().unwrap();
        reader.take(32).unwrap();
        reader.vector8().unwrap();
        reader.vector16().unwrap();
        reader.vector8().unwrap();
        let mut extensions = reader.nested16().unwrap();
        while !extensions.done() {
            let kind = extensions.u16().unwrap();
            extensions.vector16().unwrap();
            assert_ne!(kind, extension::SERVER_NAME);
        }
    }

    /// A ServerHello body, built the way a server would.
    fn a_server_hello(share: &[u8; 32], suite: u16, version: Option<u16>) -> alloc::vec::Vec<u8> {
        let mut body = Writer::new();
        body.u16(LEGACY_VERSION);
        body.bytes(&[0x33; 32]);
        assert!(body.vector8(&[]));
        body.u16(suite);
        body.u8(0);

        let mut extensions = Writer::new();
        if let Some(version) = version {
            extensions.u16(extension::SUPPORTED_VERSIONS);
            let mut inner = Writer::new();
            inner.u16(version);
            assert!(extensions.vector16(inner.as_bytes()));
        }
        extensions.u16(extension::KEY_SHARE);
        let mut entry = Writer::new();
        entry.u16(X25519);
        assert!(entry.vector16(share));
        assert!(extensions.vector16(entry.as_bytes()));

        assert!(body.vector16(extensions.as_bytes()));
        body.into_bytes()
    }

    #[test]
    fn a_server_hello_gives_up_its_key_share() {
        let body = a_server_hello(&[0x77; 32], SUITE, Some(VERSION_1_3));
        let hello = server_hello(&body).unwrap();
        assert_eq!(hello.share, [0x77; 32]);
    }

    #[test]
    fn a_server_that_picks_another_suite_is_refused_by_name() {
        let body = a_server_hello(&[0; 32], 0x1301, Some(VERSION_1_3));
        assert_eq!(
            server_hello(&body).unwrap_err(),
            Trouble::NotOffered("a cipher suite")
        );
    }

    #[test]
    fn a_server_that_does_not_say_tls_1_3_is_refused() {
        // The version is in the extension. A server that omits it is offering
        // TLS 1.2, whatever the field at the top of the hello says -- which is
        // exactly the confusion the extension exists to remove.
        let body = a_server_hello(&[0; 32], SUITE, None);
        assert_eq!(
            server_hello(&body).unwrap_err(),
            Trouble::NotOffered("a version older than TLS 1.3")
        );

        let body = a_server_hello(&[0; 32], SUITE, Some(0x0303));
        assert!(server_hello(&body).is_err());
    }

    #[test]
    fn a_hello_retry_request_is_recognised_rather_than_used_as_a_share() {
        let mut body = Writer::new();
        body.u16(LEGACY_VERSION);
        body.bytes(&HELLO_RETRY);
        assert!(body.vector8(&[]));
        body.u16(SUITE);
        body.u8(0);
        assert!(body.vector16(&[]));
        assert_eq!(
            server_hello(&body.into_bytes()).unwrap_err(),
            Trouble::RetryRequested
        );
    }

    #[test]
    fn every_truncation_of_a_server_hello_is_refused_rather_than_panicking() {
        let body = a_server_hello(&[0x77; 32], SUITE, Some(VERSION_1_3));
        for cut in 0..body.len() {
            // Whatever it decides, it returns rather than stopping the machine.
            let _ = server_hello(&body[..cut]);
        }
    }

    #[test]
    fn a_certificate_chain_comes_out_leaf_first() {
        let mut list = Writer::new();
        for der in [b"first".as_slice(), b"second", b"third"] {
            let length = der.len() as u32;
            list.bytes(&length.to_be_bytes()[1..]);
            list.bytes(der);
            assert!(list.vector16(&[]));
        }
        let mut body = Writer::new();
        assert!(body.vector8(&[]));
        let whole = list.into_bytes();
        let length = whole.len() as u32;
        body.bytes(&length.to_be_bytes()[1..]);
        body.bytes(&whole);

        let chain = certificates(&body.into_bytes()).unwrap();
        assert_eq!(chain.len(), 3);
        assert_eq!(chain[0].der, b"first");
        assert_eq!(chain[2].der, b"third");
    }

    #[test]
    fn a_certificate_message_with_nothing_in_it_is_refused() {
        let mut body = Writer::new();
        assert!(body.vector8(&[]));
        body.bytes(&[0, 0, 0]);
        assert_eq!(
            certificates(&body.into_bytes()).unwrap_err(),
            Trouble::Malformed("a certificate message with no certificates")
        );
    }

    #[test]
    fn a_certificate_verify_gives_up_its_scheme_and_signature() {
        let mut body = Writer::new();
        body.u16(scheme::ECDSA_P256_SHA256);
        assert!(body.vector16(b"a signature"));
        let parsed = certificate_verify(&body.into_bytes()).unwrap();
        assert_eq!(parsed.scheme, scheme::ECDSA_P256_SHA256);
        assert_eq!(parsed.signature, b"a signature");
    }

    #[test]
    fn trailing_bytes_after_a_signature_are_refused() {
        let mut body = Writer::new();
        body.u16(scheme::ECDSA_P256_SHA256);
        assert!(body.vector16(b"sig"));
        body.u8(0xff);
        assert!(certificate_verify(&body.into_bytes()).is_err());
    }

    #[test]
    fn the_signed_context_begins_with_sixty_four_spaces() {
        // What stops a signature made for one purpose being replayed as
        // another. RFC 8446 §4.4.3.
        let context = verify_context(&[0xab; 32]);
        assert_eq!(&context[..64], &[0x20; 64]);
        assert_eq!(&context[64..64 + 33], b"TLS 1.3, server CertificateVerify");
        assert_eq!(context[97], 0);
        assert_eq!(&context[98..], &[0xab; 32]);
    }

    #[test]
    fn a_framed_message_reads_back_with_its_header_intact() {
        let framed = frame(Kind::Finished, &[0x42; 32]);
        let message = read(&framed).unwrap().unwrap();
        assert_eq!(message.kind, Kind::Finished);
        assert_eq!(message.body, &[0x42; 32]);
        // `whole` is what the transcript hashes, and it has to include the
        // header -- leaving it out is the classic way to get a handshake that
        // fails at Finished with no clue why.
        assert_eq!(message.whole, &framed[..]);
    }

    #[test]
    fn a_message_that_is_not_all_there_yet_is_not_an_error() {
        let framed = frame(Kind::Finished, &[0x42; 32]);
        for cut in 0..framed.len() {
            assert!(
                read(&framed[..cut]).unwrap().is_none(),
                "a partial message at {cut} should be None rather than an error"
            );
        }
        assert!(read(&framed).unwrap().is_some());
    }

    #[test]
    fn a_host_name_that_is_not_one_is_not_sent() {
        assert_eq!(sni_name("Example.COM").as_deref(), Some("example.com"));
        assert_eq!(
            sni_name("a-b.example.co.uk").as_deref(),
            Some("a-b.example.co.uk")
        );
        // RFC 6066 §3: literals must not be sent.
        assert_eq!(sni_name("10.0.2.15"), None);
        assert_eq!(sni_name(""), None);
        assert_eq!(sni_name("has space.com"), None);
        assert_eq!(sni_name("under_score.com"), None);
    }
}
