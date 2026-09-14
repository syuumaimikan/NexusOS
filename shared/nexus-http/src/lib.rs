//! HTTP/1.1, from the side that asks.
//!
//! A URL to take apart, a request to write, and a response to read as it
//! arrives. Nothing here touches a connection: it is given bytes and hands back
//! bytes, which is what makes it testable without a network and what makes the
//! program using it able to draw while the bytes are still coming.
//!
//! # Reading as it arrives
//!
//! A response does not turn up all at once, so [`Response`] is fed whatever has
//! come so far and asked what it knows. That is more work than parsing a whole
//! buffer and it is the only shape that lets a browser show a page before the
//! last byte lands -- and, more to the point, the only one that does not need
//! the whole page in memory twice.
//!
//! # What it does not do
//!
//! No HTTPS. There is no TLS on this machine, and a client that pretended
//! otherwise -- by asking for `http` when it was told `https`, say -- would be
//! quietly downgrading somebody's connection. So `https` is refused, in words,
//! and the reason is said.
//!
//! No keep-alive: every request says `Connection: close` and the connection
//! ending is part of how the body's end is known. No compression, because
//! nothing here can undo it and asking for what cannot be read would be asking
//! servers to send an unreadable page.

#![no_std]
#![deny(unsafe_op_in_unsafe_fn)]

extern crate alloc;

use alloc::format;
use alloc::string::{String, ToString as _};
use alloc::vec::Vec;

/// The port a URL means when it does not say.
pub const DEFAULT_PORT: u16 = 80;

/// The most header bytes this will read before giving up on a response.
///
/// A bound rather than a trust: the headers come from a server that need not be
/// cooperating, and a response that never ends its headers would otherwise be a
/// program that allocates until it dies.
pub const MAX_HEADERS: usize = 16 * 1024;

/// What a URL turned out to be.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Url {
    /// The name or address to connect to, without the port.
    pub host: String,
    pub port: u16,
    /// Everything from the first `/`, with the query still on it. Always starts
    /// with `/`, because a request line without one is not a request line.
    pub path: String,
}

impl Url {
    /// How this would be written down, to show somebody.
    #[must_use]
    pub fn to_text(&self) -> String {
        if self.port == DEFAULT_PORT {
            format!("http://{}{}", self.host, self.path)
        } else {
            format!("http://{}:{}{}", self.host, self.port, self.path)
        }
    }
}

/// Why a URL could not be used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UrlError {
    /// A scheme this cannot speak, named so the caller can say which.
    Scheme(String),
    /// There is no host in it.
    NoHost,
    /// The port is not a number, or not one that fits.
    BadPort,
}

impl core::fmt::Display for UrlError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Scheme(what) if what == "https" => f.write_str(
                "this machine cannot speak https yet, and will not quietly ask for http instead",
            ),
            Self::Scheme(what) => write!(f, "nothing here speaks {what}"),
            Self::NoHost => f.write_str("that address has no host in it"),
            Self::BadPort => f.write_str("that port is not a number"),
        }
    }
}

/// Take a URL apart.
///
/// A missing scheme is read as `http`, because somebody typing `example.com`
/// into an address bar means a web page and not a protocol error.
///
/// # Errors
///
/// If the scheme is one this cannot speak, there is no host, or the port is not
/// a number.
pub fn parse(text: &str) -> Result<Url, UrlError> {
    let text = text.trim();

    let (scheme, rest) = match text.find("://") {
        Some(at) => (text[..at].to_ascii_lowercase(), &text[at + 3..]),
        None => (String::from("http"), text),
    };
    if scheme != "http" {
        return Err(UrlError::Scheme(scheme));
    }

    // The fragment never goes to the server. It names a place inside the page,
    // which is the client's business and nobody else's.
    let rest = rest.split('#').next().unwrap_or(rest);

    let (authority, path) = match rest.find('/') {
        Some(at) => (&rest[..at], &rest[at..]),
        None => (rest, "/"),
    };
    // Credentials in a URL are not supported, and are dropped rather than sent
    // somewhere they were not meant to go.
    let authority = match authority.rfind('@') {
        Some(at) => &authority[at + 1..],
        None => authority,
    };
    if authority.is_empty() {
        return Err(UrlError::NoHost);
    }

    let (host, port) = match authority.rfind(':') {
        Some(at) => {
            let port = authority[at + 1..]
                .parse::<u16>()
                .map_err(|_| UrlError::BadPort)?;
            (&authority[..at], port)
        }
        None => (authority, DEFAULT_PORT),
    };
    if host.is_empty() {
        return Err(UrlError::NoHost);
    }

    Ok(Url {
        host: host.to_ascii_lowercase(),
        port,
        path: if path.is_empty() {
            String::from("/")
        } else {
            path.to_string()
        },
    })
}

/// Resolve a link found in a page against the page it was found in.
///
/// Absolute links are taken as they are; `/here` keeps the host; anything else
/// is relative to the directory the page is in. That last case is the one worth
/// getting right -- a link to `next.html` on `/a/b/c.html` means `/a/b/next.html`
/// and not `/next.html`, and a browser that got it wrong would follow half the
/// links on the web to the wrong place.
///
/// # Errors
///
/// If the result is not a URL this can use.
pub fn resolve(base: &Url, link: &str) -> Result<Url, UrlError> {
    let link = link.trim();
    if link.contains("://") {
        return parse(link);
    }
    // A fragment on its own is the same page.
    if link.starts_with('#') || link.is_empty() {
        return Ok(base.clone());
    }
    // Scheme-relative: keep the scheme, take the rest.
    if let Some(rest) = link.strip_prefix("//") {
        return parse(rest);
    }
    if let Some(rest) = link.strip_prefix('/') {
        return Ok(Url {
            host: base.host.clone(),
            port: base.port,
            path: format!("/{rest}"),
        });
    }

    let directory = match base.path.rfind('/') {
        Some(at) => &base.path[..=at],
        None => "/",
    };
    Ok(Url {
        host: base.host.clone(),
        port: base.port,
        path: tidy(&format!("{directory}{link}")),
    })
}

/// Remove `.` and `..` from a path.
///
/// Done here rather than sent to the server, because a server is entitled to
/// treat `..` as a name -- and because a path that climbs above the root is a
/// path somebody wrote wrongly, not a request for the directory above the site.
fn tidy(path: &str) -> String {
    let (path, tail) = match path.find(['?', '#']) {
        Some(at) => (&path[..at], &path[at..]),
        None => (path, ""),
    };
    let mut pieces: Vec<&str> = Vec::new();
    for piece in path.split('/') {
        match piece {
            "" | "." => {}
            ".." => {
                pieces.pop();
            }
            other => pieces.push(other),
        }
    }
    let mut out = String::from("/");
    out.push_str(&pieces.join("/"));
    // A path that ended in a separator still does: `/a/b/` and `/a/b` are
    // different requests, and servers treat them differently.
    if path.ends_with('/') && !out.ends_with('/') {
        out.push('/');
    }
    out.push_str(tail);
    out
}

/// Write the request for a URL.
///
/// `Host` because HTTP/1.1 requires it and because a server with several sites
/// on one address has nothing else to go on. `Connection: close` because this
/// client does not reuse connections, and saying so lets the server end the
/// body by ending the connection.
#[must_use]
pub fn request(url: &Url) -> Vec<u8> {
    let host = if url.port == DEFAULT_PORT {
        url.host.clone()
    } else {
        format!("{}:{}", url.host, url.port)
    };
    format!(
        "GET {} HTTP/1.1\r\n\
         Host: {host}\r\n\
         User-Agent: NexusOS\r\n\
         Accept: text/html, text/plain, */*\r\n\
         Accept-Language: en, ja\r\n\
         Connection: close\r\n\
         \r\n",
        url.path
    )
    .into_bytes()
}

/// How the body's length is known.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Framing {
    /// Not decided yet.
    Unknown,
    /// This many bytes, and then it is over.
    Length(usize),
    /// In pieces, each with its length in front.
    Chunked,
    /// Until the connection closes, which is what `Connection: close` buys.
    UntilClose,
}

/// Where the reader has got to in a chunked body.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Chunk {
    /// Reading the line that says how long the next piece is.
    Size,
    /// Inside a piece, with this many bytes to go.
    Body(usize),
    /// Between pieces, skipping the newline that follows one.
    AfterBody,
    /// The zero-length piece has arrived; everything after is trailers.
    Done,
}

/// A response, read as it arrives.
///
/// Fed with [`feed`](Self::feed) and asked what it knows. The body accumulates;
/// the headers do not, once they are past.
#[derive(Debug)]
pub struct Response {
    /// What has arrived and not yet been made sense of.
    pending: Vec<u8>,
    /// Whether the headers are finished.
    headers_done: bool,
    /// The status line's number, once it is known.
    pub status: u16,
    /// And the words after it.
    pub reason: String,
    /// Every header, as it was written, with the name lowercased.
    pub headers: Vec<(String, String)>,
    /// The body so far.
    pub body: Vec<u8>,
    framing: Framing,
    chunk: Chunk,
    /// Whether the body is known to be complete.
    complete: bool,
    /// Set when the response cannot be read at all.
    pub trouble: Option<String>,
}

impl Default for Response {
    fn default() -> Self {
        Self::new()
    }
}

impl Response {
    /// A reader with nothing in it yet.
    #[must_use]
    pub fn new() -> Self {
        Self {
            pending: Vec::new(),
            headers_done: false,
            status: 0,
            reason: String::new(),
            headers: Vec::new(),
            body: Vec::new(),
            framing: Framing::Unknown,
            chunk: Chunk::Size,
            complete: false,
            trouble: None,
        }
    }

    /// Take more bytes off the connection.
    pub fn feed(&mut self, bytes: &[u8]) {
        if self.trouble.is_some() || self.complete {
            return;
        }
        if self.headers_done {
            self.take_body(bytes);
            return;
        }

        self.pending.extend_from_slice(bytes);
        if self.pending.len() > MAX_HEADERS && !self.headers_done {
            self.trouble = Some(String::from("the server's headers never ended"));
            return;
        }

        // The blank line that ends the headers. Both spellings, because a
        // server that uses bare newlines is a server that exists.
        let Some((at, skip)) = find_blank_line(&self.pending) else {
            return;
        };
        let head = self.pending[..at].to_vec();
        let rest = self.pending[at + skip..].to_vec();
        self.pending.clear();
        self.read_head(&head);
        if self.trouble.is_none() {
            self.take_body(&rest);
        }
    }

    /// The connection has closed: nothing more is coming.
    ///
    /// For a body whose end is the close, that is what completes it. For one
    /// with a declared length that has not arrived, it is a truncated page --
    /// said as such, because a page cut short and a page that ended are the
    /// same bytes and different facts.
    pub fn finish(&mut self) {
        if self.complete || self.trouble.is_some() {
            return;
        }
        match self.framing {
            Framing::UntilClose => self.complete = true,
            Framing::Unknown if !self.headers_done => {
                self.trouble = Some(String::from("the server closed without answering"));
            }
            Framing::Length(wanted) if self.body.len() < wanted => {
                self.trouble = Some(format!(
                    "the page stopped after {} of {wanted} bytes",
                    self.body.len()
                ));
            }
            _ => self.complete = true,
        }
    }

    /// Whether the whole body has arrived.
    #[must_use]
    pub const fn is_complete(&self) -> bool {
        self.complete
    }

    /// Whether the status line and headers have been read.
    #[must_use]
    pub const fn has_headers(&self) -> bool {
        self.headers_done
    }

    /// What a header says, if it was sent.
    #[must_use]
    pub fn header(&self, name: &str) -> Option<&str> {
        let name = name.to_ascii_lowercase();
        self.headers
            .iter()
            .find(|(key, _)| *key == name)
            .map(|(_, value)| value.as_str())
    }

    /// Where this response says to go instead, if it does.
    ///
    /// Only for the statuses that mean it. A `Location` on a `200` is a header
    /// with no meaning, and following it would be leaving a page that arrived.
    #[must_use]
    pub fn redirect(&self) -> Option<&str> {
        match self.status {
            301 | 302 | 303 | 307 | 308 => self.header("location"),
            _ => None,
        }
    }

    /// What the body is, as the server labelled it.
    #[must_use]
    pub fn content_type(&self) -> &str {
        self.header("content-type").unwrap_or("")
    }

    /// Whether the server said this is HTML.
    #[must_use]
    pub fn is_html(&self) -> bool {
        let kind = self.content_type().to_ascii_lowercase();
        kind.starts_with("text/html") || kind.starts_with("application/xhtml")
    }

    /// The character set the server named, lowercased, if it named one.
    #[must_use]
    pub fn charset(&self) -> Option<String> {
        let kind = self.content_type().to_ascii_lowercase();
        let at = kind.find("charset=")?;
        let rest = &kind[at + 8..];
        let value = rest
            .split(';')
            .next()
            .unwrap_or(rest)
            .trim()
            .trim_matches('"');
        if value.is_empty() {
            None
        } else {
            Some(value.to_string())
        }
    }

    /// Read the status line and the headers.
    fn read_head(&mut self, head: &[u8]) {
        let text = String::from_utf8_lossy(head);
        let mut lines = text.split('\n');

        let Some(status_line) = lines.next() else {
            self.trouble = Some(String::from("the server sent no status line"));
            return;
        };
        let status_line = status_line.trim_end_matches('\r');
        let mut parts = status_line.splitn(3, ' ');
        let version = parts.next().unwrap_or("");
        if !version.starts_with("HTTP/") {
            self.trouble = Some(format!("that is not an HTTP answer: {status_line}"));
            return;
        }
        self.status = parts.next().unwrap_or("").parse::<u16>().unwrap_or(0);
        self.reason = parts.next().unwrap_or("").trim().to_string();
        if self.status == 0 {
            self.trouble = Some(format!(
                "the server's status line made no sense: {status_line}"
            ));
            return;
        }

        for line in lines {
            let line = line.trim_end_matches('\r');
            if line.is_empty() {
                continue;
            }
            // A header continued on the next line, which is obsolete and still
            // sent. Joined onto the one before rather than dropped.
            if line.starts_with(' ') || line.starts_with('\t') {
                if let Some(last) = self.headers.last_mut() {
                    last.1.push(' ');
                    last.1.push_str(line.trim());
                }
                continue;
            }
            let Some((name, value)) = line.split_once(':') else {
                continue;
            };
            self.headers
                .push((name.trim().to_ascii_lowercase(), value.trim().to_string()));
        }
        self.headers_done = true;

        // How the body ends. Chunked wins over a length, because a server that
        // sends both is describing the transfer with the first and the resource
        // with the second -- and believing the second would stop the reader in
        // the middle of a chunk header.
        let chunked = self
            .header("transfer-encoding")
            .is_some_and(|value| value.to_ascii_lowercase().contains("chunked"));
        let length = self
            .header("content-length")
            .and_then(|value| value.trim().parse::<usize>().ok());

        self.framing = if self.no_body_by_status() {
            Framing::Length(0)
        } else if chunked {
            Framing::Chunked
        } else if let Some(length) = length {
            Framing::Length(length)
        } else {
            Framing::UntilClose
        };
        if self.framing == Framing::Length(0) {
            self.complete = true;
        }
    }

    /// Whether the status itself says there is no body.
    fn no_body_by_status(&self) -> bool {
        matches!(self.status, 204 | 304) || (100..200).contains(&self.status)
    }

    /// Take body bytes, however the body is framed.
    fn take_body(&mut self, bytes: &[u8]) {
        match self.framing {
            Framing::Length(wanted) => {
                let room = wanted.saturating_sub(self.body.len());
                let taken = bytes.len().min(room);
                self.body.extend_from_slice(&bytes[..taken]);
                if self.body.len() >= wanted {
                    self.complete = true;
                }
            }
            Framing::UntilClose => self.body.extend_from_slice(bytes),
            Framing::Chunked => self.take_chunked(bytes),
            Framing::Unknown => self.body.extend_from_slice(bytes),
        }
    }

    /// Take body bytes that arrive in pieces with their lengths in front.
    fn take_chunked(&mut self, bytes: &[u8]) {
        self.pending.extend_from_slice(bytes);
        loop {
            match self.chunk {
                Chunk::Size => {
                    let Some(at) = find_newline(&self.pending) else {
                        // The size line is not all here yet. A server may split
                        // anywhere, including inside the four bytes that say
                        // how long the next megabyte is.
                        if self.pending.len() > 64 {
                            self.trouble =
                                Some(String::from("the server's chunk header made no sense"));
                        }
                        return;
                    };
                    let line = String::from_utf8_lossy(&self.pending[..at]).to_string();
                    self.pending.drain(..at + 1);
                    // The extensions after a semicolon are not used here and
                    // are skipped rather than refused.
                    let size_text = line.trim().split(';').next().unwrap_or("").trim();
                    let Ok(size) = usize::from_str_radix(size_text, 16) else {
                        self.trouble = Some(format!("a chunk length made no sense: {size_text}"));
                        return;
                    };
                    self.chunk = if size == 0 {
                        Chunk::Done
                    } else {
                        Chunk::Body(size)
                    };
                }
                Chunk::Body(left) => {
                    let taken = left.min(self.pending.len());
                    self.body.extend(self.pending.drain(..taken));
                    let left = left - taken;
                    self.chunk = if left == 0 {
                        Chunk::AfterBody
                    } else {
                        Chunk::Body(left)
                    };
                    if left > 0 {
                        return;
                    }
                }
                Chunk::AfterBody => {
                    let Some(at) = find_newline(&self.pending) else {
                        return;
                    };
                    self.pending.drain(..at + 1);
                    self.chunk = Chunk::Size;
                }
                Chunk::Done => {
                    // Whatever follows is trailers, which nothing here reads.
                    self.pending.clear();
                    self.complete = true;
                    return;
                }
            }
        }
    }
}

/// Where the headers end, and how many bytes the blank line takes.
fn find_blank_line(bytes: &[u8]) -> Option<(usize, usize)> {
    let crlf = bytes.windows(4).position(|window| window == b"\r\n\r\n");
    let bare = bytes.windows(2).position(|window| window == b"\n\n");
    match (crlf, bare) {
        (Some(a), Some(b)) if b < a => Some((b, 2)),
        (Some(a), _) => Some((a, 4)),
        (None, Some(b)) => Some((b, 2)),
        (None, None) => None,
    }
}

/// Where the next line ends, counting a `\r\n` as ending at the `\r`.
fn find_newline(bytes: &[u8]) -> Option<usize> {
    bytes.iter().position(|byte| *byte == b'\n')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_bare_host_is_a_web_page() {
        let url = parse("example.com").expect("usable");
        assert_eq!(url.host, "example.com");
        assert_eq!(url.port, 80);
        assert_eq!(url.path, "/");
    }

    #[test]
    fn a_url_comes_apart() {
        let url = parse("http://Example.COM:8080/a/b?c=d#e").expect("usable");
        assert_eq!(url.host, "example.com");
        assert_eq!(url.port, 8080);
        // The fragment never goes to the server; the query does.
        assert_eq!(url.path, "/a/b?c=d");
        assert_eq!(url.to_text(), "http://example.com:8080/a/b?c=d");
    }

    #[test]
    fn https_is_refused_rather_than_downgraded() {
        let error = parse("https://example.com").expect_err("refused");
        assert_eq!(error, UrlError::Scheme(String::from("https")));
        // And it says why, in words somebody can act on.
        assert!(alloc::format!("{error}").contains("https"));
    }

    #[test]
    fn a_scheme_nothing_speaks_is_refused() {
        assert!(matches!(
            parse("gopher://example.com"),
            Err(UrlError::Scheme(_))
        ));
        assert_eq!(parse("http://"), Err(UrlError::NoHost));
        assert_eq!(
            parse("http://example.com:notaport/"),
            Err(UrlError::BadPort)
        );
    }

    #[test]
    fn a_relative_link_is_relative_to_the_directory() {
        let base = parse("http://example.com/a/b/c.html").expect("usable");
        assert_eq!(
            resolve(&base, "next.html").expect("usable").path,
            "/a/b/next.html"
        );
        assert_eq!(resolve(&base, "/top").expect("usable").path, "/top");
        assert_eq!(
            resolve(&base, "../up.html").expect("usable").path,
            "/a/up.html"
        );
        assert_eq!(
            resolve(&base, "./same.html").expect("usable").path,
            "/a/b/same.html"
        );
        // A link that climbs past the root stops at it rather than escaping.
        assert_eq!(resolve(&base, "../../../../x").expect("usable").path, "/x");
    }

    #[test]
    fn an_absolute_link_leaves_the_page_behind() {
        let base = parse("http://example.com/a/").expect("usable");
        let away = resolve(&base, "http://elsewhere.test/z").expect("usable");
        assert_eq!(away.host, "elsewhere.test");
        assert_eq!(away.path, "/z");
        let scheme_relative = resolve(&base, "//other.test/q").expect("usable");
        assert_eq!(scheme_relative.host, "other.test");
    }

    #[test]
    fn a_fragment_on_its_own_is_the_same_page() {
        let base = parse("http://example.com/a/b.html").expect("usable");
        assert_eq!(resolve(&base, "#here").expect("usable"), base);
    }

    #[test]
    fn a_trailing_separator_survives_tidying() {
        let base = parse("http://example.com/a/b/c.html").expect("usable");
        assert_eq!(resolve(&base, "d/").expect("usable").path, "/a/b/d/");
    }

    #[test]
    fn the_request_says_who_it_is_talking_to() {
        let url = parse("http://example.com/page").expect("usable");
        let text = String::from_utf8(request(&url)).expect("text");
        assert!(text.starts_with("GET /page HTTP/1.1\r\n"));
        assert!(text.contains("Host: example.com\r\n"));
        assert!(text.contains("Connection: close\r\n"));
        assert!(text.ends_with("\r\n\r\n"));
        // A non-default port belongs in the Host header too.
        let other = parse("http://example.com:8080/").expect("usable");
        let text = String::from_utf8(request(&other)).expect("text");
        assert!(text.contains("Host: example.com:8080\r\n"));
    }

    #[test]
    fn a_response_with_a_length_ends_where_it_says() {
        let mut response = Response::new();
        response
            .feed(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\nContent-Type: text/html\r\n\r\nhel");
        assert!(response.has_headers());
        assert_eq!(response.status, 200);
        assert_eq!(response.reason, "OK");
        assert!(!response.is_complete());
        response.feed(b"lo and then some more");
        assert!(response.is_complete());
        assert_eq!(response.body, b"hello");
        assert!(response.is_html());
    }

    #[test]
    fn a_response_arriving_one_byte_at_a_time_reads_the_same() {
        let whole =
            b"HTTP/1.1 200 OK\r\nContent-Length: 11\r\nContent-Type: text/plain\r\n\r\nhello world";
        let mut response = Response::new();
        for byte in whole {
            response.feed(&[*byte]);
        }
        assert!(response.is_complete());
        assert_eq!(response.body, b"hello world");
        assert_eq!(response.status, 200);
    }

    #[test]
    fn a_chunked_body_is_put_back_together() {
        let mut response = Response::new();
        response.feed(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n");
        response.feed(b"5\r\nhello\r\n");
        response.feed(b"6\r\n world\r\n");
        assert!(!response.is_complete());
        response.feed(b"0\r\n\r\n");
        assert!(response.is_complete());
        assert_eq!(response.body, b"hello world");
    }

    #[test]
    fn a_chunk_header_split_across_reads_still_reads() {
        let mut response = Response::new();
        response.feed(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n");
        // The length "1a" arrives in two pieces, which a server is entitled to
        // do and which a reader that assumed whole lines would get wrong.
        response.feed(b"1");
        response.feed(b"a\r\n");
        response.feed(b"abcdefghijklmnopqrstuvwxyz\r\n0\r\n\r\n");
        assert!(response.is_complete());
        assert_eq!(response.body.len(), 26);
    }

    #[test]
    fn chunk_extensions_are_skipped() {
        let mut response = Response::new();
        response.feed(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n");
        response.feed(b"3;name=value\r\nabc\r\n0\r\n\r\n");
        assert!(response.is_complete());
        assert_eq!(response.body, b"abc");
    }

    #[test]
    fn chunked_wins_over_a_length_that_is_also_there() {
        let mut response = Response::new();
        response.feed(
            b"HTTP/1.1 200 OK\r\nContent-Length: 99\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n0\r\n\r\n",
        );
        assert!(response.is_complete());
        assert_eq!(response.body, b"abc");
    }

    #[test]
    fn a_body_that_ends_with_the_connection_ends_with_it() {
        let mut response = Response::new();
        response.feed(b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\n\r\nsome text");
        assert!(!response.is_complete());
        response.finish();
        assert!(response.is_complete());
        assert_eq!(response.body, b"some text");
    }

    #[test]
    fn a_page_cut_short_says_so() {
        let mut response = Response::new();
        response.feed(b"HTTP/1.1 200 OK\r\nContent-Length: 100\r\n\r\nonly this much");
        response.finish();
        assert!(!response.is_complete());
        assert!(response.trouble.is_some());
    }

    #[test]
    fn a_server_that_says_nothing_says_so() {
        let mut response = Response::new();
        response.finish();
        assert!(response.trouble.is_some());
    }

    #[test]
    fn a_redirect_is_only_a_redirect_at_the_right_status() {
        let mut moved = Response::new();
        moved.feed(b"HTTP/1.1 301 Moved\r\nLocation: /elsewhere\r\nContent-Length: 0\r\n\r\n");
        assert_eq!(moved.redirect(), Some("/elsewhere"));

        let mut fine = Response::new();
        fine.feed(b"HTTP/1.1 200 OK\r\nLocation: /elsewhere\r\nContent-Length: 0\r\n\r\n");
        assert_eq!(fine.redirect(), None);
    }

    #[test]
    fn a_status_that_cannot_have_a_body_does_not_wait_for_one() {
        let mut response = Response::new();
        response.feed(b"HTTP/1.1 304 Not Modified\r\nETag: \"x\"\r\n\r\n");
        assert!(response.is_complete());
        assert!(response.body.is_empty());
    }

    #[test]
    fn the_charset_comes_off_the_content_type() {
        let mut response = Response::new();
        response.feed(b"HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=Shift_JIS\r\n\r\n");
        assert_eq!(response.charset().as_deref(), Some("shift_jis"));
        assert!(response.is_html());

        let mut plain = Response::new();
        plain.feed(b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\n\r\n");
        assert_eq!(plain.charset(), None);
        assert!(!plain.is_html());
    }

    #[test]
    fn a_folded_header_is_joined_rather_than_dropped() {
        let mut response = Response::new();
        response.feed(b"HTTP/1.1 200 OK\r\nX-Long: one\r\n  two\r\nContent-Length: 0\r\n\r\n");
        assert_eq!(response.header("x-long"), Some("one two"));
    }

    #[test]
    fn something_that_is_not_http_is_refused() {
        let mut response = Response::new();
        response.feed(b"GARBAGE\r\n\r\n");
        assert!(response.trouble.is_some());
    }

    #[test]
    fn headers_that_never_end_are_given_up_on() {
        let mut response = Response::new();
        response.feed(b"HTTP/1.1 200 OK\r\n");
        for _ in 0..200 {
            response.feed(&[b'x'; 100]);
        }
        assert!(response.trouble.is_some());
    }
}
