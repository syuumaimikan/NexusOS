//! Current-process views backed by the image and string ranges the native
//! loader installed. This is deliberately not a fabricated proc filesystem.
//! Other processes, fd links, maps and PR_SET_MM are not implemented.

use super::{
    linux::error,
    linux_files::{self, Metadata},
};
use crate::{
    ipc::{Object, Rights},
    process::{Process, ProcessData},
    sync::IrqSpinLock,
};
use alloc::{string::String, sync::Arc};

enum Entry {
    Executable,
    Arguments,
    Environment,
}

pub(super) fn known(path: &str) -> bool {
    crate::sched::current_process().is_some_and(|process| entry(&process, path).is_some())
}

fn entry(process: &Process, path: &str) -> Option<Entry> {
    let mut components = path
        .split('/')
        .filter(|part| !part.is_empty() && *part != ".");
    if components.next()? != "proc" {
        return None;
    }
    let owner = components.next()?;
    if owner != "self" && owner != "thread-self" && owner.parse::<u64>().ok()? != process.id.0 {
        return None;
    }
    let leaf = components.next()?;
    if components.next().is_some() || path.ends_with('/') {
        return None;
    }
    match leaf {
        "exe" => Some(Entry::Executable),
        "cmdline" => Some(Entry::Arguments),
        "environ" => Some(Entry::Environment),
        _ => None,
    }
}

pub(super) fn executable(path: &str) -> Option<Result<Arc<crate::fs::store::Node>, u64>> {
    let process = crate::sched::current_process()?;
    if !matches!(entry(&process, path)?, Entry::Executable) {
        return None;
    }
    let file = process.image.lock().file.clone();
    Some(file.ok_or((-2i64) as u64))
}

pub(crate) fn link(path: &str) -> Option<Result<String, u64>> {
    let process = crate::sched::current_process()?;
    if !matches!(entry(&process, path)?, Entry::Executable) {
        return None;
    }
    let image = process.image.lock();
    Some(if image.file.is_some() {
        Ok(image.path.clone())
    } else {
        Err((-2i64) as u64)
    })
}

pub(super) fn open(path: &str, flags: u64) -> Option<u64> {
    let process = crate::sched::current_process()?;
    let entry = entry(&process, path)?;
    if flags & 0o200000 != 0 {
        return Some((-20i64) as u64);
    } // O_DIRECTORY
    if flags & 0o100 != 0 && flags & 0o200 != 0 {
        return Some((-17i64) as u64);
    }
    if matches!(entry, Entry::Executable) {
        if flags & 0o400000 != 0 {
            return Some((-40i64) as u64);
        } // O_NOFOLLOW
        if flags & 3 != 0 || flags & 0o1000 != 0 {
            return Some((-26i64) as u64);
        }
        return None; // ordinary node open, through executable() in walk()
    }
    if flags & 3 != 0 || flags & 0o1000 != 0 {
        return Some((-13i64) as u64);
    }
    let view = Arc::new(ProcessData {
        owner: process.id,
        environment: matches!(entry, Entry::Environment),
        position: IrqSpinLock::new(0),
        nonblocking: core::sync::atomic::AtomicBool::new(flags & 0o4000 != 0),
    });
    let handle = process.handles.insert(
        Object::ProcessData(view),
        Rights::READ | Rights::CLOSE | Rights::TRANSFER,
    );
    super::linux_more::remember_cloexec(process.id.0, u64::from(handle), flags & 0o2000000 != 0);
    Some(u64::from(handle))
}

fn data_metadata(process: &Process, environment: bool) -> Metadata {
    // A distinct native virtual device, and stable identity for each actual
    // process's information. proc byte files report st_size == 0 on Linux.
    Metadata {
        device: 2,
        rdevice: 0,
        inode: process.id.0 * 4 + if environment { 2 } else { 1 },
        mode: if environment { 0o100400 } else { 0o100444 },
        links: 1,
        size: 0,
        blocks: 0,
    }
}

pub(super) fn metadata(path: &str, nofollow: bool) -> Option<Result<Metadata, u64>> {
    let process = crate::sched::current_process()?;
    Some(match entry(&process, path)? {
        Entry::Executable if nofollow => {
            let image = process.image.lock();
            if image.file.is_none() {
                Err((-2i64) as u64)
            } else {
                Ok(Metadata {
                    device: 2,
                    rdevice: 0,
                    inode: process.id.0 * 4 + 3,
                    mode: 0o120777,
                    links: 1,
                    size: image.path.len() as u64,
                    blocks: 0,
                })
            }
        }
        Entry::Executable => linux_files::walk(path).and_then(|node| Metadata::node(&node)),
        Entry::Arguments => Ok(data_metadata(&process, false)),
        Entry::Environment => Ok(data_metadata(&process, true)),
    })
}

pub(super) fn descriptor_metadata(descriptor: u64) -> Option<Result<Metadata, u64>> {
    let linux_files::Descriptor::Other(Object::ProcessData(view)) =
        linux_files::describe(descriptor).ok()?
    else {
        return None;
    };
    let process = crate::sched::current_process()?;
    Some(if process.id == view.owner {
        Ok(data_metadata(&process, view.environment))
    } else {
        Err((-13i64) as u64)
    })
}

/// Foreign-process reads are refused until native remote-memory authority is
/// available. Current-process pages are checked before every copy, including
/// writable output pages, without acquiring scheduler locks under cursor locks.
pub(super) fn read(view: &ProcessData, buffer: u64, count: u64, offset: Option<u64>) -> u64 {
    let Some(process) = crate::sched::current_process() else {
        return error::EBADF;
    };
    if process.id != view.owner {
        return (-13i64) as u64;
    }
    let (start, end) = {
        let image = process.image.lock();
        if view.environment {
            image.environment
        } else {
            image.arguments
        }
    };
    let wanted = count.min(65536);
    if wanted == 0 {
        return 0;
    }
    // Bound the view to the actual loader range. PR_SET_MM and Linux's
    // setproctitle extension past the original argv terminator are not supported.
    let mut cursor = view.position.lock();
    let at = offset.unwrap_or(*cursor);
    let available = end.saturating_sub(start);
    let taken = wanted.min(available.saturating_sub(at));
    if taken == 0 {
        return 0;
    }
    let Some((output, _)) =
        crate::arch::syscall::user_range_for(&process, buffer, taken, 65536, true)
    else {
        return error::EFAULT;
    };
    let Some((input, _)) =
        crate::arch::syscall::user_range_for(&process, start + at, taken, 65536, false)
    else {
        return 0;
    };
    // The source and destination may overlap (a caller can read into argv).
    unsafe {
        core::ptr::copy(input as *const u8, output as *mut u8, taken as usize);
    }
    if offset.is_none() {
        *cursor += taken;
    }
    taken
}

pub(super) fn seek(view: &ProcessData, offset: u64, whence: u64) -> u64 {
    let mut cursor = view.position.lock();
    let base = match whence {
        0 | 2 => 0,
        1 => *cursor,
        _ => return error::EINVAL,
    };
    let Some(to) = (base as i64).checked_add(offset as i64) else {
        return error::EINVAL;
    };
    if to < 0 {
        return error::EINVAL;
    }
    *cursor = to as u64;
    *cursor
}
