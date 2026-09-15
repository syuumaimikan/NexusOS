//! Getting a page, one step at a time.
//!
//! A fetch is a name to resolve, a connection to open, a request to send and a
//! response to read, and every one of those takes longer than a frame. So it is
//! a state machine that is *stepped* rather than a function that is called: the
//! window asks it to make what progress it can, gets told what happened, and
//! goes back to drawing.
//!
//! That shape is the whole reason a page can load without the window freezing,
//! and it is why nothing in here blocks. The one place that would be simpler
//! with a blocking call -- waiting for the DNS answer -- is also the place most
//! likely to wait for ever, which is the argument against blocking anywhere.
//!
//! # Where TLS sits
//!
//! Between the connection and the request, and nowhere else. A secure fetch has
//! one extra stage -- [`Stage::Securing`] -- and two extra lines in the two
//! stages either side of it: what would have gone out goes through
//! [`Carrier::send`] instead, and what came in comes back through
//! [`Carrier::received`].
//!
//! Everything else is untouched. The request writer does not know, the response
//! reader does not know, the redirect logic does not know. That is the test of
//! whether the layering was right, and it is why `https://` cost a stage rather
//! than a second fetcher.

extern crate alloc;

use alloc::format;
use alloc::string::{String, ToString as _};
use alloc::vec::Vec;

use alloc::boxed::Box;
use alloc::rc::Rc;

use nexus_netclient as net;
use nexus_tls::chain::Roots;
use nexus_tls::client::Client;
use nexus_user::Handle;

/// How long a name lookup waits before asking again.
const RESOLVE_RETRY_MS: u64 = 900;
/// And how many times, before it gives up.
const RESOLVE_TRIES: u32 = 4;

/// How long the whole of a fetch may take.
///
/// A bound on the machine's patience rather than on the network's. Without one
/// a server that accepts a connection and then says nothing is a window that
/// says "loading" until somebody turns the machine off.
const FETCH_MS: u64 = 30_000;

/// How many redirects will be followed before it looks like a loop.
pub const MAX_REDIRECTS: u32 = 5;

/// The most bytes of a page this will hold.
///
/// A page, not a download. What is above this is not something this program can
/// show, and reading it anyway would be filling a heap to draw the first
/// screenful.
pub const MAX_PAGE: usize = 512 * 1024;

/// Where a fetch has got to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Stage {
    /// Waiting for a name to become an address.
    Resolving,
    /// Waiting for the connection to open.
    Connecting,
    /// Agreeing on keys, and checking who is at the other end.
    ///
    /// Only for `https://`. It is a separate stage rather than part of
    /// `Connecting` because it is the one that can fail for a reason worth
    /// reading -- an untrusted certificate, a name that does not match -- and
    /// those deserve to be told apart from "that server did not answer".
    Securing,
    /// The request is going out.
    Sending,
    /// The answer is coming back, with how many bytes so far.
    Reading(usize),
    /// It worked.
    Done,
    /// It did not, and this is what to tell somebody.
    Failed(String),
}

impl Stage {
    /// Whether the machine is still working on it.
    #[must_use]
    pub const fn is_busy(&self) -> bool {
        matches!(
            self,
            Self::Resolving
                | Self::Connecting
                | Self::Securing
                | Self::Sending
                | Self::Reading(_)
        )
    }
}

/// What carries the bytes: the connection itself, or TLS over it.
///
/// The whole of the difference between `http://` and `https://` lives in this
/// enum. Every other part of a fetch asks it to carry something and is not told
/// which one it is talking to.
enum Carrier {
    /// Straight down the connection.
    Plain,
    /// Through a TLS session, which is boxed because it is large -- key
    /// schedules, a transcript, a verified chain -- and a `Fetch` with no TLS
    /// in it should not be that size.
    Secure(Box<Client>),
}

impl Carrier {
    /// Turn what is to be sent into what actually goes on the wire.
    ///
    /// # Errors
    ///
    /// Only TLS can fail here, and only by being asked to send before the
    /// handshake finished, which would be this file's mistake rather than the
    /// network's.
    fn send(&mut self, bytes: &[u8]) -> Result<Vec<u8>, String> {
        match self {
            Self::Plain => Ok(bytes.to_vec()),
            Self::Secure(client) => {
                client
                    .send(bytes)
                    .map_err(|why| alloc::format!("{why}"))?;
                Ok(client.take_outgoing())
            }
        }
    }

    /// Turn what arrived into what was meant.
    ///
    /// Returns the plaintext, and anything TLS wants sent back -- which after
    /// the handshake is nothing, and during it is most of the handshake.
    ///
    /// # Errors
    ///
    /// A record that will not open, an alert from the server, a certificate
    /// that does not verify. Every one of them ends the fetch: there is no
    /// "carry on anyway" for a connection whose other end cannot be
    /// established.
    fn received(&mut self, bytes: &[u8]) -> Result<(Vec<u8>, Vec<u8>), String> {
        match self {
            Self::Plain => Ok((bytes.to_vec(), Vec::new())),
            Self::Secure(client) => {
                client
                    .received(bytes)
                    .map_err(|why| alloc::format!("{why}"))?;
                Ok((client.take_plaintext(), client.take_outgoing()))
            }
        }
    }

    /// Whether the far end is established and the request may go.
    fn ready(&self) -> bool {
        match self {
            Self::Plain => true,
            Self::Secure(client) => client.ready(),
        }
    }
}

/// Everything a secure fetch needs that a plain one does not.
///
/// Passed in rather than reached for, because a `Fetch` has no way to open a
/// file and no business having one. The browser holds the root store and the
/// browser is handed the root store, in a message, at start-up.
#[derive(Clone)]
pub struct Trust {
    /// The certificate authorities this machine believes.
    pub roots: Rc<Roots>,
    /// What time it is, or `None` on a machine whose clock was never set.
    ///
    /// `None` makes every certificate fail to verify, which is correct and is
    /// said in those words: a machine that does not know the date cannot tell a
    /// current certificate from one that was withdrawn years ago.
    pub now: Option<i64>,
}

/// One fetch, in progress or finished.
pub struct Fetch {
    /// The channel to the kernel's network service.
    service: Handle,
    /// Where the answers to a name lookup come back.
    port: Option<u16>,
    /// Which resolver to ask.
    resolver: net::Address,

    pub url: nexus_http::Url,
    pub stage: Stage,
    /// The response as it arrives.
    pub response: nexus_http::Response,

    /// What this fetch is waiting on.
    query_id: u16,
    asked_at: u64,
    tries: u32,
    connection: Option<u32>,
    /// What is left of the request to send.
    outgoing: Vec<u8>,
    /// What is left to push down the connection, already sealed if it is going
    /// to be. Kept apart from `outgoing` because TLS turns one request into a
    /// different number of bytes, and offering the plaintext to the connection
    /// twice would send the request twice.
    pending: Vec<u8>,
    /// Plain, or TLS.
    carrier: Carrier,
    /// What a secure fetch needs, kept for the moment the connection opens.
    trust: Option<Trust>,
    started_at: u64,
    /// How many redirects have been followed to get here.
    pub redirects: u32,
}

impl Fetch {
    /// Start fetching a URL. Nothing is waited for.
    pub fn start(
        service: Handle,
        resolver: net::Address,
        url: nexus_http::Url,
        redirects: u32,
        trust: Option<Trust>,
    ) -> Self {
        let now = nexus_user::uptime();
        let mut fetch = Self {
            service,
            port: None,
            resolver,
            url,
            stage: Stage::Resolving,
            response: nexus_http::Response::new(),
            // Derived from the clock, because an identifier a stranger can
            // guess is an answer a stranger can forge.
            query_id: (now as u16) ^ 0x5A5A,
            asked_at: 0,
            tries: 0,
            connection: None,
            outgoing: Vec::new(),
            pending: Vec::new(),
            carrier: Carrier::Plain,
            trust,
            started_at: now,
            redirects,
        };

        // Said now rather than at the connection, because it is a fact about
        // this machine and not about the far end: there is no point resolving a
        // name in order to fail for a reason that was true before anybody
        // asked.
        if fetch.url.secure && fetch.trust.is_none() {
            fetch.stage = Stage::Failed(
                nexus_i18n::text("browse.https.noroots").to_string(),
            );
            return fetch;
        }

        // An address typed with dots is not a question for a resolver: it is
        // the answer, and asking would be waiting for a server to say what this
        // machine already knows.
        if let Some(address) = nexus_dns::as_address(&fetch.url.host) {
            fetch.connect(address);
        }
        fetch
    }

    /// Give up, closing anything that is open.
    pub fn abandon(&mut self) {
        if let Some(id) = self.connection.take() {
            net::close(self.service, id).ok();
        }
        if let Some(port) = self.port.take() {
            net::unbind(self.service, port).ok();
        }
    }

    /// Make whatever progress can be made now, and say whether anything changed.
    ///
    /// Called from the window's own loop. Returns `true` when something worth
    /// redrawing happened, so that a fetch which is merely waiting costs a
    /// poll and not a frame.
    pub fn step(&mut self) -> bool {
        if !self.stage.is_busy() {
            return false;
        }
        if nexus_user::uptime().saturating_sub(self.started_at) > FETCH_MS {
            self.fail("that page took too long");
            return true;
        }

        match self.stage {
            Stage::Resolving => self.step_resolving(),
            Stage::Connecting => self.step_connecting(),
            Stage::Securing => self.step_securing(),
            Stage::Sending => self.step_sending(),
            Stage::Reading(_) => self.step_reading(),
            _ => false,
        }
    }

    /// Say what went wrong, and stop.
    fn fail(&mut self, why: &str) {
        self.abandon();
        self.stage = Stage::Failed(why.to_string());
    }

    /// Ask the resolver, and read what comes back.
    fn step_resolving(&mut self) -> bool {
        let now = nexus_user::uptime();

        if self.port.is_none() {
            match net::bind(self.service) {
                Ok(port) => self.port = Some(port),
                Err(error) => {
                    self.fail(&format!("cannot look up a name: {error}"));
                    return true;
                }
            }
        }
        let port = self.port.unwrap_or(0);

        // Ask, and ask again if nothing came. A datagram is allowed to be lost
        // and this one is the first thing the machine sends to the resolver, so
        // losing it is the ordinary case rather than the exception.
        if self.tries == 0 || now.saturating_sub(self.asked_at) > RESOLVE_RETRY_MS {
            if self.tries >= RESOLVE_TRIES {
                self.fail(&format!("no answer about {}", self.url.host));
                return true;
            }
            if self.resolver == [0, 0, 0, 0] {
                self.fail("this machine was told of no resolver");
                return true;
            }
            // A new identifier per attempt, so a late answer to the first
            // question is not read as the answer to the second.
            self.query_id = self.query_id.wrapping_mul(31).wrapping_add(7);
            match nexus_dns::question(self.query_id, &self.url.host) {
                Ok(message) => {
                    net::send_datagram(
                        self.service,
                        port,
                        self.resolver,
                        nexus_dns::PORT,
                        &message,
                    )
                    .ok();
                }
                Err(error) => {
                    self.fail(&format!("{error}"));
                    return true;
                }
            }
            self.tries += 1;
            self.asked_at = now;
            return self.tries == 1;
        }

        let Ok(Some((from, source, bytes))) = net::read_datagram(self.service, port) else {
            return false;
        };
        // From the server that was asked, from the port it was asked on.
        // Neither check is sufficient against somebody on the path and both are
        // free, and together with the identifier they are what an off-path
        // forgery has to get right.
        if from != self.resolver || source != nexus_dns::PORT {
            return false;
        }
        match nexus_dns::answer(&bytes, self.query_id, &self.url.host) {
            Ok(answer) => {
                let Some(address) = answer.addresses.first().copied() else {
                    self.fail("that name has no address");
                    return true;
                };
                self.connect(address);
                true
            }
            // A stale answer to a previous attempt. Ignored rather than fatal:
            // the question that is outstanding may still be answered.
            Err(nexus_dns::Error::NotOurs) => false,
            Err(error) => {
                self.fail(&format!("{error}"));
                true
            }
        }
    }

    /// Open the connection, now that there is somewhere to open it to.
    fn connect(&mut self, address: net::Address) {
        if let Some(port) = self.port.take() {
            net::unbind(self.service, port).ok();
        }
        match net::open(self.service, address, self.url.port) {
            Ok(id) => {
                self.connection = Some(id);
                self.outgoing = nexus_http::request(&self.url);
                self.stage = Stage::Connecting;
            }
            Err(error) => self.fail(&format!("{error}")),
        }
    }

    /// Begin the handshake, now that there is a connection to do it over.
    ///
    /// The two thirty-two byte values are the only unguessable things in the
    /// whole connection, and everything else rests on them. They come from the
    /// processor's hardware generator through [`nexus_user::random`], and if
    /// there is not one this refuses -- it does not fall back to the clock, and
    /// it does not fall back to `http://`.
    fn secure(&mut self) -> bool {
        let Some(trust) = self.trust.clone() else {
            self.fail(&nexus_i18n::text("browse.https.noroots").to_string());
            return true;
        };

        let mut random = [0u8; 32];
        let mut private = [0u8; 32];
        if nexus_user::random(&mut random).is_err() || nexus_user::random(&mut private).is_err() {
            self.fail(&nexus_i18n::text("browse.https.norandom").to_string());
            return true;
        }

        match Client::start(&self.url.host, trust.roots, trust.now, random, private) {
            Ok(client) => {
                self.carrier = Carrier::Secure(Box::new(client));
                // The hello is already waiting inside it.
                match self.carrier.received(&[]) {
                    Ok((_, out)) => self.pending = out,
                    Err(why) => {
                        self.fail(&why);
                        return true;
                    }
                }
                self.stage = Stage::Securing;
                true
            }
            Err(why) => {
                self.fail(&format!("{why}"));
                true
            }
        }
    }

    /// Push whatever is waiting down the connection.
    ///
    /// Shared by the handshake and the request, because at this level they are
    /// the same thing: bytes that have to leave, however many messages it takes.
    /// `Ok(true)` means everything queued has gone.
    fn push(&mut self) -> Result<bool, String> {
        if self.pending.is_empty() {
            return Ok(true);
        }
        let Some(id) = self.connection else {
            return Err(String::from("the connection went away"));
        };
        match net::send_all(self.service, id, &self.pending) {
            Ok(left) => {
                let sent = self.pending.len() - left.len();
                self.pending.drain(..sent);
                Ok(self.pending.is_empty())
            }
            Err(error) => Err(format!("{error}")),
        }
    }

    /// Carry the handshake until the far end is established.
    ///
    /// This is where an `https://` address is either proved or refused. The
    /// certificate check happens inside [`Client::received`], and a chain that
    /// does not verify arrives here as an error with a sentence on it -- which
    /// is then what the window shows, because a padlock nobody can explain is
    /// no better than no padlock.
    fn step_securing(&mut self) -> bool {
        let Some(id) = self.connection else {
            self.fail("the connection went away");
            return true;
        };

        match self.push() {
            Ok(_) => {}
            Err(why) => {
                self.fail(&why);
                return true;
            }
        }

        let Ok((state, _, bytes)) = net::read(self.service, id) else {
            self.fail("the network service stopped answering");
            return true;
        };
        if bytes.is_empty() {
            if state.is_over() {
                // A server that hangs up during a handshake has said something
                // by doing it: usually that it does not speak TLS 1.3, or does
                // not have the one cipher suite this machine offers.
                self.fail(&nexus_i18n::text("browse.https.hungup").to_string());
                return true;
            }
            return false;
        }

        match self.carrier.received(&bytes) {
            Ok((_, out)) => {
                // Nothing the server says before the handshake is finished is
                // page content, so the plaintext half is discarded rather than
                // fed to the response reader.
                self.pending.extend_from_slice(&out);
            }
            Err(why) => {
                self.fail(&why);
                return true;
            }
        }

        if let Err(why) = self.push() {
            self.fail(&why);
            return true;
        }

        if self.carrier.ready() {
            self.stage = Stage::Sending;
        }
        true
    }

    /// Wait for the handshake.
    fn step_connecting(&mut self) -> bool {
        let Some(id) = self.connection else {
            self.fail("the connection went away");
            return true;
        };
        let Ok((state, _, _)) = net::read(self.service, id) else {
            self.fail("the network service stopped answering");
            return true;
        };
        match state {
            net::State::Connecting => false,
            net::State::Open | net::State::PeerDone => {
                if self.url.secure {
                    self.secure()
                } else {
                    self.stage = Stage::Sending;
                    true
                }
            }
            net::State::Closed | net::State::Unknown => {
                let why = net::why(self.service, id).unwrap_or_default();
                let why = if why.is_empty() {
                    String::from("that server did not answer")
                } else {
                    why
                };
                self.fail(&why);
                true
            }
        }
    }

    /// Push the request out, however many messages that takes.
    fn step_sending(&mut self) -> bool {
        let Some(id) = self.connection else {
            self.fail("the connection went away");
            return true;
        };

        // Sealed once, on the first pass. The carrier turns one request into
        // however many bytes the wire needs, and doing that again on a later
        // pass would encrypt the same request under a later record number and
        // send it twice.
        if !self.outgoing.is_empty() {
            let request = core::mem::take(&mut self.outgoing);
            match self.carrier.send(&request) {
                Ok(sealed) => self.pending = sealed,
                Err(why) => {
                    self.fail(&why);
                    return true;
                }
            }
        }

        match self.push() {
            Ok(false) => false,
            Ok(true) => {
                // Nothing more is going out, and saying so is what lets the
                // server answer and close rather than wait for a request that
                // has already finished.
                //
                // Not for TLS: half-closing the connection under a TLS session
                // is not how a TLS session ends, and a server reading it as the
                // end of the stream would answer with an alert instead of a
                // page. `Connection: close` in the request is what gets the
                // same result there.
                if !self.url.secure {
                    net::shutdown(self.service, id).ok();
                }
                self.stage = Stage::Reading(0);
                true
            }
            Err(why) => {
                self.fail(&why);
                true
            }
        }
    }

    /// Take whatever has arrived.
    fn step_reading(&mut self) -> bool {
        let Some(id) = self.connection else {
            self.fail("the connection went away");
            return true;
        };
        let mut changed = false;

        // Several reads per step, because one message is a couple of hundred
        // bytes and a page is not. Bounded, so that a fast server cannot keep
        // this loop from returning to the window that has to draw.
        for _ in 0..24 {
            let Ok((state, waiting, bytes)) = net::read(self.service, id) else {
                self.fail("the network service stopped answering");
                return true;
            };
            if !bytes.is_empty() {
                if self.response.body.len() > MAX_PAGE {
                    self.finish_reading();
                    return true;
                }
                // Through the carrier, which for `http://` hands the same bytes
                // straight back and for `https://` opens the records. A record
                // that will not open ends the fetch: it means either the server
                // sent an alert or somebody on the path altered the stream, and
                // neither is a page.
                let (plain, out) = match self.carrier.received(&bytes) {
                    Ok(both) => both,
                    Err(why) => {
                        self.fail(&why);
                        return true;
                    }
                };
                if !out.is_empty() {
                    self.pending.extend_from_slice(&out);
                    if let Err(why) = self.push() {
                        self.fail(&why);
                        return true;
                    }
                }
                if !plain.is_empty() {
                    self.response.feed(&plain);
                    self.stage = Stage::Reading(self.response.body.len());
                    changed = true;
                }
            }
            if let Some(trouble) = self.response.trouble.clone() {
                self.fail(&trouble);
                return true;
            }
            if self.response.is_complete() {
                self.finish_reading();
                return true;
            }
            if state.is_over() && waiting == 0 && bytes.is_empty() {
                // The connection ended. For a body whose length was never
                // declared that is what completes it; for one that was, it is a
                // page cut short, and `finish` is what tells the two apart.
                self.response.finish();
                if let Some(trouble) = self.response.trouble.clone() {
                    self.fail(&trouble);
                } else {
                    self.finish_reading();
                }
                return true;
            }
            if bytes.is_empty() {
                break;
            }
        }
        changed
    }

    /// Everything has arrived.
    fn finish_reading(&mut self) {
        self.response.finish();
        self.abandon();
        self.stage = Stage::Done;
    }
}
