//! Bounded read-only tool runtime. No inference, ambient I/O or grant API.
#![no_std]

pub mod wire;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u16)]
pub enum Tool {
    SystemInfo = 1,
    FileRead = 2,
    FileWrite = 3,
    FileRemove = 4,
    TerminalExecute = 5,
    ProcessStop = 6,
    SettingsWrite = 7,
    NetworkConnect = 8,
    KernelMemory = 9,
}

impl Tool {
    pub fn from_id(id: u16) -> Option<Self> {
        Some(match id {
            1 => Self::SystemInfo,
            2 => Self::FileRead,
            3 => Self::FileWrite,
            4 => Self::FileRemove,
            5 => Self::TerminalExecute,
            6 => Self::ProcessStop,
            7 => Self::SettingsWrite,
            8 => Self::NetworkConnect,
            9 => Self::KernelMemory,
            _ => return None,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Permission {
    Read,
    Write,
    Execute,
    Network,
    ProcessControl,
    SystemConfiguration,
    Privileged,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Safe,
    ConfirmRequired,
    Privileged,
    Blocked,
}

pub fn requirement(tool: Tool) -> (Permission, Level) {
    use Level::*;
    match tool {
        Tool::SystemInfo | Tool::FileRead => (Permission::Read, Safe),
        Tool::FileWrite | Tool::FileRemove => (Permission::Write, ConfirmRequired),
        Tool::TerminalExecute => (Permission::Execute, ConfirmRequired),
        Tool::ProcessStop => (Permission::ProcessControl, ConfirmRequired),
        Tool::SettingsWrite => (Permission::SystemConfiguration, Privileged),
        Tool::NetworkConnect => (Permission::Network, ConfirmRequired),
        Tool::KernelMemory => (Permission::Privileged, Blocked),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u16)]
pub enum Status {
    Verified = 0,
    Blocked = 1,
    ConfirmRequired = 2,
    Privileged = 3,
    Unsupported = 4,
    Invalid = 5,
    Expired = 6,
    Exhausted = 7,
    Failed = 8,
    Cancelled = 9,
}

impl Status {
    pub fn from_id(id: u16) -> Option<Self> {
        Some(match id {
            0 => Self::Verified,
            1 => Self::Blocked,
            2 => Self::ConfirmRequired,
            3 => Self::Privileged,
            4 => Self::Unsupported,
            5 => Self::Invalid,
            6 => Self::Expired,
            7 => Self::Exhausted,
            8 => Self::Failed,
            9 => Self::Cancelled,
            _ => return None,
        })
    }
}

/// Policy classification only, independent of a service's implemented tools.
/// `Ok(())` grants no authority: callers still need a suitably scoped OS
/// capability and must validate arguments. Confirmation outcomes remain denials.
pub fn permitted(tool: Tool) -> Result<(), Status> {
    match requirement(tool).1 {
        Level::Blocked => Err(Status::Blocked),
        Level::Privileged => Err(Status::Privileged),
        Level::ConfirmRequired => Err(Status::ConfirmRequired),
        Level::Safe => Ok(()),
    }
}

/// Fixed service allowlist after policy evaluation. This does not grant an OS
/// capability. Other capability-holding executors may use [`permitted`] instead.
pub fn authorize(tool: Tool) -> Result<(), Status> {
    permitted(tool)?;
    match tool {
        Tool::SystemInfo => Ok(()),
        _ => Err(Status::Unsupported),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Request {
    pub session: u64,
    pub id: u64,
    pub tool: Tool,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Snapshot {
    pub uptime_ms: u64,
    /// The service's current thread, not a process count or caller identity.
    pub thread_id: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Unavailable;

/// Trusted adapter. Its calls must be bounded; the core cannot preempt a call.
pub trait SystemSource {
    fn snapshot(&mut self) -> Result<Snapshot, Unavailable>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Response {
    pub session: u64,
    pub id: u64,
    pub status: Status,
    pub snapshot: Snapshot,
}

impl Response {
    pub fn error(session: u64, id: u64, status: Status) -> Self {
        Self {
            session,
            id,
            status,
            snapshot: Snapshot::default(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Ready,
    Executing,
    Verifying,
    Complete,
    Rejected,
    Failed,
    Cancelled,
}

/// Metadata only: never includes input text, file data or credentials.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Audit {
    pub request: Request,
    pub status: Status,
    pub attempts: u8,
    pub started_ms: u64,
    pub finished_ms: u64,
}

/// One session per channel. At most 64 requests and two observation pairs per
/// request. No externally supplied policy, retry count or resource limits.
pub struct Runtime {
    session: u64,
    last_id: u64,
    remaining: u8,
    state: State,
    audit: Option<Audit>,
}

impl Runtime {
    /// Session IDs correlate requests; the owning channel supplies authority.
    pub const fn new(session: u64) -> Self {
        Self {
            session,
            last_id: 0,
            remaining: 64,
            state: State::Ready,
            audit: None,
        }
    }

    pub fn state(&self) -> State {
        self.state
    }
    pub fn audit(&self) -> Option<Audit> {
        self.audit
    }
    pub fn cancel(&mut self) {
        self.state = State::Cancelled;
    }

    pub fn execute(
        &mut self,
        request: Request,
        now: u64,
        source: &mut impl SystemSource,
    ) -> Response {
        let mut attempts = 0;
        let mut finished = now;
        let result = self.run(request, now, source, &mut attempts, &mut finished);
        let response = match result {
            Ok(snapshot) => {
                self.state = State::Complete;
                Response {
                    session: request.session,
                    id: request.id,
                    status: Status::Verified,
                    snapshot,
                }
            }
            Err(status) => {
                if self.state != State::Cancelled {
                    self.state = if matches!(status, Status::Failed | Status::Expired) {
                        State::Failed
                    } else {
                        State::Rejected
                    };
                }
                Response::error(request.session, request.id, status)
            }
        };
        self.audit = Some(Audit {
            request,
            status: response.status,
            attempts,
            started_ms: now,
            finished_ms: finished,
        });
        response
    }

    fn run(
        &mut self,
        request: Request,
        now: u64,
        source: &mut impl SystemSource,
        attempts: &mut u8,
        finished: &mut u64,
    ) -> Result<Snapshot, Status> {
        if self.state == State::Cancelled {
            return Err(Status::Cancelled);
        }
        if request.session == 0
            || request.session != self.session
            || request.id == 0
            || request.id <= self.last_id
        {
            return Err(Status::Invalid);
        }
        if self.remaining == 0 {
            return Err(Status::Exhausted);
        }
        // Valid denied requests consume budget too: probing is not free.
        self.last_id = request.id;
        self.remaining -= 1;
        authorize(request.tool)?;
        let deadline = now.checked_add(500).ok_or(Status::Expired)?;
        for _ in 0..2 {
            *attempts += 1;
            self.state = State::Executing;
            let Ok(first) = source.snapshot() else {
                continue;
            };
            *finished = first.uptime_ms;
            if first.uptime_ms < now || first.uptime_ms >= deadline {
                return Err(Status::Expired);
            }
            self.state = State::Verifying;
            let Ok(second) = source.snapshot() else {
                continue;
            };
            *finished = second.uptime_ms;
            if second.uptime_ms < first.uptime_ms || second.uptime_ms >= deadline {
                return Err(Status::Expired);
            }
            // With the current single-thread adapter this identity check holds
            // by construction; it does not verify machine-wide system health.
            if first.thread_id != 0 && second.thread_id == first.thread_id {
                return Ok(second);
            }
        }
        Err(Status::Failed)
    }
}

#[cfg(test)]
mod tests;
