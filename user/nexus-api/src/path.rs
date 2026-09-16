//! Walking a path of several components from a directory handle.
//!
//! `nexus_user::open` takes *one* component and refuses a name with a separator
//! in it. That is not a limitation to work around; it is what makes a directory
//! handle mean "this subtree and nothing above it", and every program that
//! wanted `a/b/c` split it and walked it by hand.
//!
//! This is that walk, written once. It keeps the property: each step is a
//! single `open` from the handle before it, `..` is refused rather than
//! followed, and there is no path a caller can write that reaches above the
//! handle it started from.

use alloc::string::String;
use alloc::vec::Vec;

use nexus_user::{Error, Handle, Kind};

/// Open what a path names, walking one component at a time.
///
/// Leading, trailing and repeated separators are ignored, and `.` is skipped:
/// `/a//b/` and `a/./b` both name the same thing. `..` is refused with
/// [`Error::NotFound`], because a directory handle is the authority to reach
/// what is under it and a path that could climb out would make that mean
/// nothing.
///
/// The handle returned is the caller's to close. Every intermediate one is
/// closed here.
///
/// # Errors
///
/// As `nexus_user::open`, from whichever component failed.
pub fn open(directory: Handle, path: &str) -> Result<Handle, Error> {
    let mut at = directory;
    let mut borrowed = false;

    for component in path.split('/') {
        if component.is_empty() || component == "." {
            continue;
        }
        if component == ".." {
            if borrowed {
                nexus_user::close(at).ok();
            }
            return Err(Error::NotFound);
        }
        let next = nexus_user::open(at, component);
        if borrowed {
            nexus_user::close(at).ok();
        }
        at = next?;
        borrowed = true;
    }

    if borrowed {
        Ok(at)
    } else {
        // The path named the directory itself. Duplicated rather than returned,
        // so that the caller may close what it is given without closing the
        // handle it passed in.
        nexus_user::duplicate(
            directory,
            nexus_user::rights::READ | nexus_user::rights::WRITE,
        )
        .or_else(|_| nexus_user::duplicate(directory, nexus_user::rights::READ))
    }
}

/// The most [`read`] will return.
///
/// A megabyte. Larger than anything on this machine, and small enough that a
/// program handed something enormous is told so rather than asking for memory
/// it will not get.
pub const MAX_FILE: usize = 1024 * 1024;

/// Read a whole file.
///
/// # Errors
///
/// As [`open`], or the read failing.
pub fn read(directory: Handle, path: &str) -> Result<Vec<u8>, Error> {
    let file = open(directory, path)?;
    let size = nexus_user::size(file).unwrap_or(0).min(MAX_FILE);
    let mut bytes = alloc::vec![0u8; size];
    let read = nexus_user::read_at(file, 0, &mut bytes);
    nexus_user::close(file).ok();
    let read = read?;
    bytes.truncate(read);
    Ok(bytes)
}

/// Write a whole file, making it if it is not there and replacing what was.
///
/// The directory has to have been lent with the right to write, or this fails
/// where the create does -- which is the point of lending one read-only.
///
/// # Errors
///
/// As [`open`], or the create or write failing. A short write is reported as
/// [`Error::TooBig`] rather than as success: what is on the disk is then
/// neither the old file nor the new one, and a caller told it worked would go
/// on to rely on it.
pub fn write(directory: Handle, path: &str, bytes: &[u8]) -> Result<(), Error> {
    let (parent, name) = split(directory, path)?;
    let file =
        nexus_user::create(parent, name, Kind::File).or_else(|_| nexus_user::open(parent, name))?;
    let written = nexus_user::write(file, bytes);
    nexus_user::close(file).ok();
    if parent != directory {
        nexus_user::close(parent).ok();
    }
    match written? {
        count if count == bytes.len() => Ok(()),
        _ => Err(Error::TooBig),
    }
}

/// What a directory holds, as names and kinds.
///
/// `.` and `..` are left out: every directory has them, no caller has ever
/// wanted them, and every caller that did not remove them had a bug the first
/// time somebody walked a tree.
///
/// # Errors
///
/// As [`open`], or the listing failing -- which is what a file rather than a
/// directory gives.
pub fn list(directory: Handle, path: &str) -> Result<Vec<(String, Kind)>, Error> {
    let target = open(directory, path)?;
    let mut packed = alloc::vec![0u8; 8192];
    let length = nexus_user::list(target, &mut packed);
    nexus_user::close(target).ok();
    let length = length?;

    let mut out = Vec::new();
    for entry in nexus_user::entries(&packed[..length]) {
        if entry.name == "." || entry.name == ".." {
            continue;
        }
        out.push((String::from(entry.name), entry.kind));
    }
    Ok(out)
}

/// The directory holding the last component of a path, and that component.
///
/// The handle returned is the caller's to close *unless* it is the one passed
/// in, which is what a path of one component gives. Callers compare the two.
fn split(directory: Handle, path: &str) -> Result<(Handle, &str), Error> {
    let trimmed = path.trim_matches('/');
    match trimmed.rsplit_once('/') {
        Some((parent, name)) if !name.is_empty() => Ok((open(directory, parent)?, name)),
        _ if trimmed.is_empty() => Err(Error::NotFound),
        _ => Ok((directory, trimmed)),
    }
}
