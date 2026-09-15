//! Bounded, transient context selection. Metadata is supplied by a trusted
//! collector after OS capability checks, never by a model or an untrusted file.
//! This module grants no authority and performs no I/O.
use crate::{Request, Snapshot};
use core::fmt::{self, Write};

pub const MAX_ENTRIES: usize = 16;
pub const MAX_BYTES: usize = 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Scope {
    pub session: u64,
    pub task: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Origin {
    System,
    User,
    File,
    Application,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sensitivity {
    Public,
    Private,
    Secret,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Destination {
    Local,
    Remote,
}

/// A borrowed input; raw text intentionally has no Debug implementation.
pub struct Entry<'a> {
    pub text: &'a str,
    pub origin: Origin,
    pub scope: Scope,
    pub collected_ms: u64,
    pub priority: u8,
    pub relevance: u8,
    pub sensitivity: Sensitivity,
    /// Trusted collector's read authorization for this specific item.
    pub authorized: bool,
    /// Separate, explicit permission to disclose this item to a remote model.
    pub remote_allowed: bool,
}

pub struct Policy {
    pub scope: Scope,
    pub now_ms: u64,
    pub max_age_ms: u64,
    pub destination: Destination,
    pub allow_private: bool,
    pub byte_budget: usize,
    /// Includes separators and tokenizer-added special tokens. Reserve space
    /// for user instructions and generated output separately in ModelSession.
    pub token_budget: usize,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    InvalidPolicy,
    TooManyEntries,
    Tokenization,
    Buffer,
}

/// Trusted, bounded, side-effect-free tokenizer adapter. An estimate is only
/// safe if it is an upper bound for the chosen model. Prefer its real tokenizer.
pub trait TokenCounter {
    fn count(&self, text: &str) -> Option<usize>;
}

/// Upper bound for the current byte-fallback BPE: one token per byte + BOS/space.
/// This is not a universal bound for arbitrary third-party tokenizers.
pub struct ByteBpeBudget;
impl TokenCounter for ByteBpeBudget {
    fn count(&self, text: &str) -> Option<usize> {
        text.len().checked_add(2)
    }
}

#[cfg(feature = "model")]
impl TokenCounter for crate::model::Tokenizer {
    fn count(&self, text: &str) -> Option<usize> {
        self.encode(text).ok().map(|tokens| tokens.len())
    }
}

/// Provenance stays separate from untrusted text. Offsets refer to UTF-8 bytes
/// in Context::text(), excluding the added newline separator.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Selected {
    pub input_index: usize,
    pub origin: Origin,
    pub collected_ms: u64,
    pub start: usize,
    pub end: usize,
}

#[derive(Default, Clone, Copy, Debug, PartialEq, Eq)]
pub struct Report {
    pub filtered: usize,
    pub redacted: usize,
    pub over_budget: usize,
}

/// Owns only selected text, with no logging, persistence or unbounded history.
/// Drop/clear overwrite its buffer; this does not erase caller-owned inputs or
/// compiler/stack copies and is not a cryptographic memory-erasure guarantee.
pub struct Context {
    bytes: [u8; MAX_BYTES],
    len: usize,
    selected: [Option<Selected>; MAX_ENTRIES],
    count: usize,
    tokens: usize,
    report: Report,
}

impl Context {
    pub fn select(
        entries: &[Entry<'_>],
        policy: &Policy,
        counter: &impl TokenCounter,
    ) -> Result<Self, Error> {
        if policy.scope.session == 0
            || policy.scope.task == 0
            || policy.byte_budget > MAX_BYTES
            || policy.byte_budget == 0
            || policy.token_budget == 0
        {
            return Err(Error::InvalidPolicy);
        }
        if entries.len() > MAX_ENTRIES {
            return Err(Error::TooManyEntries);
        }
        let mut output = Self {
            bytes: [0; MAX_BYTES],
            len: 0,
            selected: [None; MAX_ENTRIES],
            count: 0,
            tokens: 0,
            report: Report::default(),
        };
        let mut order = [0; MAX_ENTRIES];
        let mut eligible = 0;
        for (i, entry) in entries.iter().enumerate() {
            if !entry.authorized
                || entry.scope != policy.scope
                || entry.relevance == 0
                || entry.text.is_empty()
                || entry.text.len() > MAX_BYTES
                || entry.collected_ms > policy.now_ms
                || policy.now_ms - entry.collected_ms > policy.max_age_ms
                || (entry.sensitivity == Sensitivity::Private && !policy.allow_private)
                || (policy.destination == Destination::Remote && !entry.remote_allowed)
            {
                output.report.filtered += 1;
                continue;
            }
            if entry.sensitivity == Sensitivity::Secret || contains_secret(entry.text) {
                // Omit the WHOLE entry: no partial credential value, prefix or
                // key survives. Classification remains the primary protection.
                output.report.redacted += 1;
                continue;
            }
            order[eligible] = i;
            eligible += 1;
        }
        // Stable insertion sort: bounded 16 items; deterministic input-order ties.
        for i in 1..eligible {
            let mut j = i;
            while j > 0 && rank(&entries[order[j]]) > rank(&entries[order[j - 1]]) {
                order.swap(j, j - 1);
                j -= 1;
            }
        }
        for &i in &order[..eligible] {
            let entry = &entries[i];
            let start = output.len + usize::from(output.len != 0);
            let end = start + entry.text.len();
            if end > policy.byte_budget {
                output.report.over_budget += 1;
                continue;
            }
            if start > output.len {
                output.bytes[output.len] = b'\n';
            }
            output.bytes[start..end].copy_from_slice(entry.text.as_bytes());
            let text = core::str::from_utf8(&output.bytes[..end]).map_err(|_| Error::Buffer)?;
            let tokens = counter.count(text).ok_or(Error::Tokenization)?;
            if tokens > policy.token_budget {
                output.bytes[output.len..end].fill(0);
                output.report.over_budget += 1;
                continue;
            }
            output.selected[output.count] = Some(Selected {
                input_index: i,
                origin: entry.origin,
                collected_ms: entry.collected_ms,
                start,
                end,
            });
            output.count += 1;
            output.len = end;
            output.tokens = tokens;
        }
        Ok(output)
    }

    /// Untrusted data, never instructions or tool grants.
    pub fn text(&self) -> &str {
        core::str::from_utf8(&self.bytes[..self.len]).expect("only whole UTF-8 entries are copied")
    }
    pub fn selected(&self) -> impl Iterator<Item = &Selected> {
        self.selected[..self.count].iter().flatten()
    }
    /// Zero means no context will be injected, not the encoding of an empty prompt.
    pub fn tokens(&self) -> usize {
        self.tokens
    }
    pub fn report(&self) -> Report {
        self.report
    }
    pub fn clear(&mut self) {
        self.bytes.fill(0);
        self.selected.fill(None);
        self.len = 0;
        self.count = 0;
        self.tokens = 0;
        self.report = Report::default();
    }
}
impl Drop for Context {
    fn drop(&mut self) {
        self.clear();
    }
}

fn rank(entry: &Entry<'_>) -> (u8, u8, u64) {
    (entry.priority, entry.relevance, entry.collected_ms)
}

/// Defense in depth, deliberately conservative. Not a general secret detector.
/// Unknown/unlabelled secrets must be marked Secret by the trusted collector.
fn contains_secret(text: &str) -> bool {
    const MARKERS: &[&[u8]] = &[
        b"password",
        b"passwd",
        b"api_key",
        b"api-key",
        b"apikey",
        b"access_token",
        b"refresh_token",
        b"secret",
        b"authorization",
        b"bearer ",
        b"private key",
        b"sk-",
        b"ghp_",
        b"github_pat_",
        "パスワード".as_bytes(),
        "秘密鍵".as_bytes(),
    ];
    MARKERS.iter().any(|marker| {
        text.as_bytes()
            .windows(marker.len())
            .any(|window| window.eq_ignore_ascii_case(marker))
    })
}

/// Constructed only by Runtime after two observations pass verification.
/// A copy is historical; consumers must still enforce freshness and task scope.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SystemContext {
    request: Request,
    snapshot: Snapshot,
}
impl SystemContext {
    pub(crate) fn new(request: Request, snapshot: Snapshot) -> Self {
        Self { request, snapshot }
    }
    pub fn scope(&self) -> Scope {
        Scope {
            session: self.request.session,
            task: self.request.id,
        }
    }
    pub fn collected_ms(&self) -> u64 {
        self.snapshot.uptime_ms
    }
    /// Write only verified numeric facts. No CPU/RAM values are synthesized.
    pub fn write<'a>(&self, bytes: &'a mut [u8]) -> Result<&'a str, Error> {
        struct Buffer<'a> {
            bytes: &'a mut [u8],
            len: usize,
        }
        impl Write for Buffer<'_> {
            fn write_str(&mut self, text: &str) -> fmt::Result {
                let end = self.len.checked_add(text.len()).ok_or(fmt::Error)?;
                let target = self.bytes.get_mut(self.len..end).ok_or(fmt::Error)?;
                target.copy_from_slice(text.as_bytes());
                self.len = end;
                Ok(())
            }
        }
        let mut buffer = Buffer { bytes, len: 0 };
        write!(
            buffer,
            "uptime_ms={}\nservice_thread_id={}",
            self.snapshot.uptime_ms, self.snapshot.thread_id
        )
        .map_err(|_| Error::Buffer)?;
        let len = buffer.len;
        core::str::from_utf8(&bytes[..len]).map_err(|_| Error::Buffer)
    }
}

#[cfg(test)]
#[path = "context_tests.rs"]
mod tests;
