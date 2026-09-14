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

extern crate alloc;

use alloc::format;
use alloc::string::{String, ToString as _};
use alloc::vec::Vec;

use nexus_netclient as net;
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
            Self::Resolving | Self::Connecting | Self::Sending | Self::Reading(_)
        )
    }
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
            started_at: now,
            redirects,
        };

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
                self.stage = Stage::Sending;
                true
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
        match net::send_all(self.service, id, &self.outgoing) {
            Ok(left) => {
                let sent = self.outgoing.len() - left.len();
                self.outgoing.drain(..sent);
                if self.outgoing.is_empty() {
                    // Nothing more is going out, and saying so is what lets the
                    // server answer and close rather than wait for a request
                    // that has already finished.
                    net::shutdown(self.service, id).ok();
                    self.stage = Stage::Reading(0);
                    return true;
                }
                sent > 0
            }
            Err(error) => {
                let text = format!("{error}");
                self.fail(&text);
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
                self.response.feed(&bytes);
                self.stage = Stage::Reading(self.response.body.len());
                changed = true;
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
