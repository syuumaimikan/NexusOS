//! Resolve foreign path operations through actual native directory authority.
//! Parent components are walked, not lexically erased: missing/.. is still
//! missing, and file/.. is still ENOTDIR. Absolute paths ignore dirfd; relative
//! paths retain the actual cwd or directory handle throughout the operation.

use super::{linux::error, linux_files};
use crate::{
    fs::store::{self, Node},
    ipc::{Object, Rights},
    process::Process,
};
use alloc::{collections::VecDeque, string::String, sync::Arc};

const ENOENT: u64 = (-2i64) as u64;
const ENOTDIR: u64 = (-20i64) as u64;
const ENAMETOOLONG: u64 = (-36i64) as u64;

pub(super) struct Resolved {
    pub path: String,
    pub node: Option<Arc<Node>>,
    pub parent: Option<Arc<Node>>,
    pub name: Option<String>,
    pub special: bool,
}

impl Resolved {
    /// Mutating a name must never erase a normalized '.' or '..' target.
    pub(super) fn entry(&self, supplied: &str) -> Result<(&Node, &str), u64> {
        if matches!(
            supplied.trim_end_matches('/').rsplit('/').next(),
            Some("." | "..")
        ) {
            return Err(error::EINVAL);
        }
        if self.special {
            return Err((-13i64) as u64);
        }
        Ok((
            self.parent.as_deref().ok_or((-16i64) as u64)?,
            self.name.as_deref().ok_or(error::EINVAL)?,
        ))
    }
    pub(super) fn node(&self) -> Result<Arc<Node>, u64> {
        if self.special {
            if let Some(result) = super::linux_proc::executable(&self.path) {
                return result;
            }
        }
        self.node.clone().ok_or(ENOENT)
    }
}

pub(super) fn cwd(process: &Process) -> Result<Arc<Node>, u64> {
    let node = process.working_directory.lock().clone();
    match node {
        Some(node) => Ok(node),
        None => linux_files::root(),
    }
}

pub(super) fn directory(process: &Process, descriptor: u64) -> Result<Arc<Node>, u64> {
    if linux_files::is_cwd(descriptor) {
        return cwd(process);
    }
    let id = u32::try_from(descriptor).map_err(|_| error::EBADF)?;
    let object = process
        .handles
        .object(id, Rights::NONE)
        .map_err(|_| error::EBADF)?;
    match object {
        Object::Node(node) if node.is_directory() => Ok(node),
        _ => Err(ENOTDIR),
    }
}

fn pathname(names: &[String]) -> String {
    let mut path = String::from("/");
    for (index, name) in names.iter().enumerate() {
        if index != 0 {
            path.push('/');
        }
        path.push_str(name);
    }
    path
}

pub(super) fn resolve(directory: u64, path: &str) -> Result<Resolved, u64> {
    resolve_with(directory, path, true, true)
}

pub(super) fn resolve_no_follow(directory: u64, path: &str) -> Result<Resolved, u64> {
    resolve_with(directory, path, false, true)
}

/// Mutation of a trailing symlink must never mutate the linked directory.
pub(super) fn resolve_entry(directory: u64, path: &str) -> Result<Resolved, u64> {
    resolve_with(directory, path, false, false)
}

fn resolve_with(
    directory: u64,
    path: &str,
    follow_final: bool,
    follow_trailing: bool,
) -> Result<Resolved, u64> {
    if path.is_empty() {
        return Err(ENOENT);
    }
    if path.len() >= 4096 {
        return Err(ENAMETOOLONG);
    }
    let root = linux_files::root()?;
    let (mut nodes, mut names) = if path.starts_with('/') {
        (alloc::vec![root.clone()], alloc::vec::Vec::new())
    } else {
        let process = crate::sched::current_process().ok_or(error::EBADF)?;
        let anchor = self::directory(&process, directory)?;
        let location = store::directory_location(&root, &anchor).map_err(linux_files::errno)?;
        (location.nodes, location.names)
    };
    let mut parts: VecDeque<String> = path
        .split('/')
        .filter(|part| !part.is_empty())
        .map(String::from)
        .collect();
    let mut require_directory = path.ends_with('/');
    let mut followed = 0;
    while let Some(part) = parts.pop_front() {
        let node = nodes.last().ok_or(ENOENT)?;
        if !node.is_directory() {
            return Err(ENOTDIR);
        }
        if part == "." {
            continue;
        }
        if part == ".." {
            if nodes.len() > 1 {
                nodes.pop();
                names.pop();
            }
            continue;
        }
        if part.len() > 255 {
            return Err(ENAMETOOLONG);
        }
        if nodes.len() == 1 && (part == "proc" || part == "dev") {
            let mut candidate = String::from("/");
            candidate.push_str(&part);
            for remainder in &parts {
                candidate.push('/');
                candidate.push_str(remainder);
            }
            if require_directory {
                candidate.push('/');
            }
            if super::linux_proc::known(&candidate)
                || super::linux_display::is_device(&candidate)
                || super::linux_random::known(&candidate)
            {
                return Ok(Resolved {
                    path: candidate,
                    node: None,
                    parent: None,
                    name: None,
                    special: true,
                });
            }
        }
        let child = match store::open_child(node, &part) {
            Ok(child) => child,
            Err(reason) => {
                let code = linux_files::errno(reason);
                if code == ENOENT && parts.is_empty() {
                    let parent = node.clone();
                    names.push(part.clone());
                    let mut canonical = pathname(&names);
                    if require_directory {
                        canonical.push('/');
                    }
                    return Ok(Resolved {
                        path: canonical,
                        node: None,
                        parent: Some(parent),
                        name: Some(part),
                        special: false,
                    });
                }
                return Err(code);
            }
        };
        if child.is_symlink()
            && (!parts.is_empty() || follow_final || (require_directory && follow_trailing))
        {
            followed += 1;
            if followed > 40 {
                return Err((-40i64) as u64);
            }
            let target = store::read_link(&child).map_err(linux_files::errno)?;
            if parts.is_empty() && target.ends_with('/') {
                require_directory = true;
            }
            if target.starts_with('/') {
                nodes.truncate(1);
                names.clear();
            }
            for component in target.split('/').filter(|part| !part.is_empty()).rev() {
                parts.push_front(String::from(component));
            }
            continue;
        }
        names.push(part);
        nodes.push(child);
    }
    let node = nodes.last().ok_or(ENOENT)?.clone();
    if require_directory && !node.is_directory() {
        return Err(ENOTDIR);
    }
    let parent = nodes.len().checked_sub(2).map(|index| nodes[index].clone());
    Ok(Resolved {
        path: pathname(&names),
        node: Some(node),
        parent,
        name: names.last().cloned(),
        special: false,
    })
}

/// symlinkat(target, directory, linkname), also used by the legacy symlink call.
pub(super) fn symlinkat(target: u64, directory: u64, linkname: u64) -> u64 {
    let target = match linux_files::path_of(target) {
        Ok(path) => path,
        Err(error) => return error,
    };
    if target.is_empty() {
        return ENOENT;
    }
    let path = match linux_files::path_of(linkname) {
        Ok(path) => path,
        Err(error) => return error,
    };
    if path.ends_with('/') {
        return ENOENT;
    }
    let resolved = match resolve_entry(directory, &path) {
        Ok(path) => path,
        Err(error) => return error,
    };
    if resolved.node.is_some() || resolved.special {
        return (-17i64) as u64;
    }
    let (parent, name) = match resolved.entry(&path) {
        Ok(entry) => entry,
        Err(error) => return error,
    };
    match store::create_link(parent, name, &target) {
        Ok(_) => 0,
        Err(error) => linux_files::errno(error),
    }
}

fn change(process: &Process, node: Arc<Node>) -> u64 {
    if !node.is_directory() {
        return ENOTDIR;
    }
    let previous = core::mem::replace(&mut *process.working_directory.lock(), Some(node));
    // Last-close cleanup may sleep on the filesystem. Drop outside the irq lock.
    drop(previous);
    0
}

pub(super) fn chdir(pointer: u64) -> u64 {
    let path = match linux_files::path_of(pointer) {
        Ok(path) => path,
        Err(error) => return error,
    };
    let node = match resolve(linux_files::AT_FDCWD as u32 as u64, &path)
        .and_then(|resolved| resolved.node())
    {
        Ok(node) => node,
        Err(error) => return error,
    };
    let Some(process) = crate::sched::current_process() else {
        return error::EBADF;
    };
    change(&process, node)
}

pub(super) fn fchdir(descriptor: u64) -> u64 {
    if linux_files::is_cwd(descriptor) {
        return error::EBADF;
    }
    let Some(process) = crate::sched::current_process() else {
        return error::EBADF;
    };
    let node = match directory(&process, descriptor) {
        Ok(node) => node,
        Err(error) => return error,
    };
    let root = match linux_files::root() {
        Ok(root) => root,
        Err(error) => return error,
    };
    if let Err(error) = store::directory_location(&root, &node) {
        return linux_files::errno(error);
    }
    change(&process, node)
}

pub(super) fn getcwd(buffer: u64, size: u64) -> u64 {
    if size == 0 {
        return error::EINVAL;
    }
    let Some(process) = crate::sched::current_process() else {
        return error::EBADF;
    };
    let root = match linux_files::root() {
        Ok(root) => root,
        Err(error) => return error,
    };
    let node = match cwd(&process) {
        Ok(node) => node,
        Err(error) => return error,
    };
    let location = match store::directory_location(&root, &node) {
        Ok(location) => location,
        Err(error) => return linux_files::errno(error),
    };
    let mut bytes = pathname(&location.names).into_bytes();
    bytes.push(0);
    if bytes.len() > 4096 {
        return ENAMETOOLONG;
    }
    if size < bytes.len() as u64 {
        return (-34i64) as u64;
    }
    let Some((output, _)) =
        crate::arch::syscall::user_range_for(&process, buffer, bytes.len() as u64, 4096, true)
    else {
        return error::EFAULT;
    };
    unsafe {
        core::ptr::copy_nonoverlapping(bytes.as_ptr(), output as *mut u8, bytes.len());
    }
    bytes.len() as u64
}
