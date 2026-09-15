//! Talking to the kernel's removable-drive service.
//!
//! The wire format is four bytes of tag, then a body. It is written out here
//! and read in `kernel/nexus-kernel/src/removable.rs`, and the two have to
//! agree -- which is why every offset in this file names the field it is.
//!
//! # Why a channel rather than a directory
//!
//! Because a removable drive can be absent, and can be pulled out between two
//! calls. Every request names the drive again, so there is no handle to become
//! stale. The kernel's side says more about that decision.

extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;

use nexus_user::Handle;

/// What a reply starts with when it worked.
const GOOD: &[u8] = b"ok  ";

/// The most one reply carries, which the service also knows.
pub const CHUNK: usize = 192;

/// Why a request was refused, as the service numbers them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trouble {
    NoDrive,
    NoFilesystem,
    NotFound,
    Malformed,
    ReadOnly,
    TooBig,
    Full,
    BadName,
    /// The service did not answer at all.
    Silent,
}

impl Trouble {
    /// Which string to show for it.
    #[must_use]
    pub const fn key(self) -> &'static str {
        match self {
            Self::NoDrive => "files.usb.nodrive",
            Self::NoFilesystem => "files.usb.nofilesystem",
            Self::NotFound => "files.usb.notfound",
            Self::ReadOnly => "files.usb.readonly",
            Self::Full => "files.usb.full",
            Self::BadName => "files.usb.badname",
            Self::TooBig | Self::Malformed | Self::Silent => "files.usb.refused",
        }
    }

    fn of(reason: u16) -> Self {
        match reason {
            1 => Self::NoDrive,
            2 => Self::NoFilesystem,
            3 => Self::NotFound,
            4 => Self::Malformed,
            5 => Self::ReadOnly,
            6 => Self::TooBig,
            7 => Self::Full,
            8 => Self::BadName,
            _ => Self::Silent,
        }
    }
}

/// What one drive is.
#[derive(Debug, Clone, Copy)]
pub struct Drive {
    pub blocks: u64,
    pub block_size: u32,
    /// Whether it has a filesystem this machine reads.
    pub mountable: bool,
}

impl Drive {
    /// How big it is, in mebibytes.
    #[must_use]
    pub const fn megabytes(&self) -> u64 {
        self.blocks * self.block_size as u64 / (1024 * 1024)
    }
}

/// One thing in a directory.
#[derive(Debug, Clone)]
pub struct Entry {
    pub name: String,
    pub size: u32,
    pub is_directory: bool,
}

/// Ask, and get the body of the answer.
fn ask(service: Handle, request: &[u8]) -> Result<Vec<u8>, Trouble> {
    if nexus_user::send(service, request, &[]).is_err() {
        return Err(Trouble::Silent);
    }
    let mut buffer = [0u8; 256];
    let mut none = [Handle(0); 1];
    let Ok(received) = nexus_user::receive(service, &mut buffer, &mut none) else {
        return Err(Trouble::Silent);
    };
    let reply = &buffer[..received.bytes];
    if reply.len() < 4 {
        return Err(Trouble::Silent);
    }
    if &reply[..4] == GOOD {
        return Ok(reply[4..].to_vec());
    }
    if reply.len() >= 6 {
        return Err(Trouble::of(u16::from_le_bytes([reply[4], reply[5]])));
    }
    Err(Trouble::Silent)
}

/// What drives are plugged in.
///
/// # Errors
///
/// If the service does not answer.
pub fn drives(service: Handle) -> Result<Vec<Drive>, Trouble> {
    let body = ask(service, b"drv?")?;
    if body.is_empty() {
        return Ok(Vec::new());
    }
    let count = body[0] as usize;
    let mut out = Vec::with_capacity(count);
    for index in 0..count {
        let at = 1 + index * 13;
        if at + 13 > body.len() {
            break;
        }
        out.push(Drive {
            blocks: u64::from_le_bytes(body[at..at + 8].try_into().unwrap_or_default()),
            block_size: u32::from_le_bytes(body[at + 8..at + 12].try_into().unwrap_or_default()),
            mountable: body[at + 12] != 0,
        });
    }
    Ok(out)
}

/// What is in a directory on one.
///
/// # Errors
///
/// No such drive, no filesystem, or no such directory.
pub fn list(service: Handle, drive: u8, path: &str) -> Result<Vec<Entry>, Trouble> {
    let mut request = alloc::vec![b'l', b'i', b's', b't', drive];
    request.extend_from_slice(path.as_bytes());
    let body = ask(service, &request)?;
    if body.is_empty() {
        return Ok(Vec::new());
    }

    let count = body[0] as usize;
    let mut out = Vec::with_capacity(count);
    let mut at = 1usize;
    for _ in 0..count {
        if at >= body.len() {
            break;
        }
        let length = body[at] as usize;
        at += 1;
        if at + length + 5 > body.len() {
            break;
        }
        let name = String::from_utf8_lossy(&body[at..at + length]).into_owned();
        at += length;
        let is_directory = body[at] != 0;
        at += 1;
        let size = u32::from_le_bytes(body[at..at + 4].try_into().unwrap_or_default());
        at += 4;
        out.push(Entry {
            name,
            size,
            is_directory,
        });
    }
    Ok(out)
}

/// Read a whole file off a drive.
///
/// In pieces, because a message carries less than a file. The reply says how
/// long the whole thing is, so this reads towards a known end rather than
/// asking until it gets nothing.
///
/// # Errors
///
/// No such file, or the service refusing.
pub fn read(service: Handle, drive: u8, name: &str, limit: usize) -> Result<Vec<u8>, Trouble> {
    let mut out: Vec<u8> = Vec::new();
    // How long the whole file is, learned from the first reply. `None` until
    // then, rather than zero: zero is a real length and a file of no bytes
    // should stop after one pass rather than looking like an unknown one.
    let mut whole: Option<u32> = None;

    // Bounded, so a service that kept answering with nothing cannot spin this.
    for _ in 0..512 {
        let mut request = alloc::vec![b'r', b'e', b'a', b'd', drive];
        request.extend_from_slice(&(out.len() as u32).to_le_bytes());
        request.extend_from_slice(name.as_bytes());
        let body = ask(service, &request)?;
        if body.len() < 4 {
            break;
        }
        let said = u32::from_le_bytes(body[..4].try_into().unwrap_or_default());
        whole = Some(said);
        let piece = &body[4..];
        if piece.is_empty() {
            break;
        }
        out.extend_from_slice(piece);
        if out.len() >= said as usize || out.len() >= limit {
            break;
        }
    }
    // Cut to what the service said the file is, as well as to the caller's
    // limit: a reply carrying more than it promised would otherwise lengthen
    // the file.
    if let Some(whole) = whole {
        out.truncate(whole as usize);
    }
    out.truncate(limit);
    Ok(out)
}

/// Write a whole file onto a drive.
///
/// In pieces as well, and the first one replaces the file. That ordering
/// matters: a caller that started at an offset would be appending to whatever
/// was there before.
///
/// # Errors
///
/// A full drive, a name that will not fit, or the service refusing.
pub fn write(service: Handle, drive: u8, name: &str, data: &[u8]) -> Result<(), Trouble> {
    let name_bytes = name.as_bytes();
    if name_bytes.len() > 200 {
        return Err(Trouble::BadName);
    }
    // What is left of a message once the tag, the drive, the offset, the name
    // length and the name are in it.
    let room = CHUNK.saturating_sub(4 + 1 + 4 + 1 + name_bytes.len());
    if room == 0 {
        return Err(Trouble::BadName);
    }

    let mut offset = 0usize;
    loop {
        let take = (data.len() - offset).min(room);
        let mut request = alloc::vec![b'w', b'r', b'i', b't', drive];
        request.extend_from_slice(&(offset as u32).to_le_bytes());
        request.push(name_bytes.len() as u8);
        request.extend_from_slice(name_bytes);
        request.extend_from_slice(&data[offset..offset + take]);
        ask(service, &request)?;
        offset += take;
        // An empty file still takes one pass, which is what creates it.
        if offset >= data.len() {
            break;
        }
    }
    Ok(())
}

/// Remove a file from a drive.
///
/// # Errors
///
/// No such file, or the service refusing.
pub fn remove(service: Handle, drive: u8, name: &str) -> Result<(), Trouble> {
    let mut request = alloc::vec![b'd', b'e', b'l', b'e', drive];
    request.extend_from_slice(name.as_bytes());
    ask(service, &request).map(|_| ())
}
