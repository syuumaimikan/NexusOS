//! Forking a private Linux address space and collecting its exit status.
//!
//! Address spaces are eagerly copied rather than made copy-on-write. That is
//! slower and uses more memory, but it preserves isolation with the page-fault
//! machinery this kernel has today. Unsupported shared mappings and a process
//! with other live threads are refused instead of being copied inconsistently.

extern crate alloc;

use alloc::collections::BTreeMap;
use alloc::sync::Arc;

use crate::arch::syscall::Frame;
use crate::compat::linux::error;
use crate::memory::address_snapshot::{self, SnapshotError};
use crate::process::{Completion, Personality, Process};
use crate::sync::IrqSpinLock;

const EAGAIN: u64 = (-11i64) as u64;
const EINTR: u64 = (-4i64) as u64;
const ECHILD: u64 = (-10i64) as u64;
const MAX_FORK_PAGES: usize = 64 * 1024;
const WNOHANG: u64 = 1;
const ALLOWED_FLAGS: u64 = 0xFF | 0x0008_0000 | 0x0010_0000 | 0x0020_0000 | 0x0100_0000;

struct Child {
    completion: Arc<Completion>,
    waiting: bool,
}

/// A parent owns each child until one successful `wait4` collects it.
static CHILDREN: IrqSpinLock<BTreeMap<(u64, u64), Child>> = IrqSpinLock::new(BTreeMap::new());

/// `clone` without `CLONE_VM`: create a process with an independent private
/// address space and return its PID to the parent and zero to the child.
pub fn fork(
    flags: u64,
    stack: u64,
    parent_tid: u64,
    child_tid: u64,
    tls: u64,
    frame: &Frame,
) -> u64 {
    use super::linux_threads::flag as f;

    if flags & !ALLOWED_FLAGS != 0 || !matches!(flags & 0xFF, 0 | 17) {
        return error::EINVAL;
    }
    if stack >= nexus_abi::layout::USER_SPACE_END {
        return error::EINVAL;
    }
    let Some(parent) = crate::sched::current_process() else {
        return error::ENOSYS;
    };
    if !crate::compat::linux_threads::siblings(parent.id.0).is_empty() {
        // The eager snapshot requires a quiescent source address space. This is
        // Linux's resource-unavailable answer and avoids copying during a race.
        return EAGAIN;
    }
    if flags & f::PARENT_SETTID != 0 && !writable_current_word(parent_tid) {
        return error::EFAULT;
    }

    let snapshot = match unsafe {
        address_snapshot::private_snapshot(&parent.address_space, MAX_FORK_PAGES)
    } {
        Ok(space) => space,
        Err(SnapshotError::OutOfMemory | SnapshotError::PageLimit) => return error::ENOMEM,
        Err(
            SnapshotError::SharedMapping
            | SnapshotError::LargeMapping
            | SnapshotError::SupervisorMapping
            | SnapshotError::RestrictedTable
            | SnapshotError::NoKernelSpace,
        ) => return EAGAIN,
    };

    let child = Process::forked(&parent, Arc::new(snapshot));
    let child_id = child.id.0;
    let child_space = Arc::clone(&child.address_space);
    let completion = Arc::clone(&child.completion);

    let thread_tls = if flags & f::SETTLS == 0 {
        None
    } else if parent.personality == Personality::Linux32 {
        let (entry, descriptor) = match super::linux32::clone_tls(tls) {
            Ok(value) => value,
            Err(reason) => return reason,
        };
        Some(crate::sched::ThreadTls::Compat { entry, descriptor })
    } else {
        if tls >= nexus_abi::layout::USER_SPACE_END {
            return error::EINVAL;
        }
        Some(crate::sched::ThreadTls::FsBase(tls))
    };

    if flags & f::CHILD_SETTID != 0 && !writable_word(&child, child_tid) {
        return error::EFAULT;
    }

    super::linux_files::inherit(parent.id.0, child_id);
    super::linux_more::inherit_descriptors(parent.id.0, child_id);
    super::linux_signal::inherit(parent.id.0, child_id);
    {
        let mut children = CHILDREN.lock();
        children.insert(
            (parent.id.0, child_id),
            Child {
                completion: Arc::clone(&completion),
                waiting: false,
            },
        );
    }

    let mut child_frame = *frame;
    child_frame.rax = 0;
    if stack != 0 {
        child_frame.rsp = stack;
    }
    let started = crate::sched::spawn_user_thread(child, child_frame, thread_tls, |thread| {
        // This callback runs under the scheduler lock, before the child is
        // visible to another processor.
        if flags & f::PARENT_SETTID != 0 {
            unsafe { core::ptr::write_unaligned(parent_tid as *mut u32, child_id as u32) };
        }
        if flags & f::CHILD_SETTID != 0 {
            write_word_snapshot(&child_space, child_tid, child_id as u32);
        }
        if flags & f::CHILD_CLEARTID != 0 && child_tid != 0 {
            super::linux_threads::register_clear_tid(thread.0, child_tid);
        }
    });
    match started {
        Ok(_) => {
            crate::kprintln!("[linux] forked process {child_id} from {}", parent.id.0);
            child_id
        }
        Err(_) => {
            CHILDREN.lock().remove(&(parent.id.0, child_id));
            error::ENOMEM
        }
    }
}

/// `wait4(pid, status, options, rusage)`. Supports one child or any child,
/// blocking waits, and WNOHANG. Other wait modes are explicitly refused.
pub fn wait4(pid: u64, status: u64, options: u64, usage: u64, i386: bool) -> u64 {
    if options & !WNOHANG != 0 {
        return error::EINVAL;
    }
    let Some(parent) = crate::sched::current_process() else {
        return error::ENOSYS;
    };
    let requested = pid as u32 as i32;
    if requested == 0 || requested < -1 {
        return ECHILD;
    }
    if status != 0 && crate::arch::syscall::user_range(status, 4, 4).is_none() {
        return error::EFAULT;
    }
    let usage_bytes = if i386 { 72 } else { 144 };
    let usage_alignment = if i386 { 4 } else { 8 };
    if usage != 0 && crate::arch::syscall::user_range(usage, usage_bytes, usage_alignment).is_none()
    {
        return error::EFAULT;
    }

    let reserved = {
        let mut children = CHILDREN.lock();
        let selected = children.iter_mut().find(|((owner, child), entry)| {
            *owner == parent.id.0
                && !entry.waiting
                && (requested == -1 || *child == requested as u64)
        });
        let Some((key, child)) = selected else {
            return ECHILD;
        };
        child.waiting = true;
        (*key, Arc::clone(&child.completion))
    };
    let ((owner, child_id), completion) = reserved;

    let child_status = match completion.status() {
        Some(status) => Some(status),
        None if options & WNOHANG != 0 => None,
        None => {
            let status = completion.wait();
            if completion.status().is_none() {
                release_wait(owner, child_id);
                return EINTR;
            }
            Some(status)
        }
    };
    let Some(child_status) = child_status else {
        release_wait(owner, child_id);
        return 0;
    };

    if status != 0 {
        let wait_status = if child_status == crate::process::KILLED {
            9u32
        } else {
            (child_status as u32 & 0xFF) << 8
        };
        unsafe { core::ptr::write_unaligned(status as *mut u32, wait_status) };
    }
    if usage != 0 {
        unsafe { core::ptr::write_bytes(usage as *mut u8, 0, usage_bytes as usize) };
    }

    let mut children = CHILDREN.lock();
    if children
        .get(&(owner, child_id))
        .is_some_and(|entry| Arc::ptr_eq(&entry.completion, &completion) && entry.waiting)
    {
        children.remove(&(owner, child_id));
        crate::kprintln!(
            "[linux] wait4 collected child {child_id} with exit status {child_status}"
        );
        child_id
    } else {
        ECHILD
    }
}

fn release_wait(owner: u64, child: u64) {
    if let Some(entry) = CHILDREN.lock().get_mut(&(owner, child)) {
        entry.waiting = false;
    }
}

pub fn forget_parent(parent: u64) {
    CHILDREN.lock().retain(|(owner, _), _| *owner != parent);
}

fn writable_current_word(address: u64) -> bool {
    if address == 0 || !address.is_multiple_of(4) {
        return false;
    }
    crate::arch::syscall::user_range(address, 4, 4).is_some()
}

fn writable_word(process: &Process, address: u64) -> bool {
    if address == 0 || !address.is_multiple_of(4) {
        return false;
    }
    (0..4).all(|offset| {
        let at = address + offset;
        at < nexus_abi::layout::USER_SPACE_END
            && crate::memory::paging::flags_in(process.address_space.root(), at).is_some_and(
                |flags| {
                    flags & (crate::memory::paging::USER | crate::memory::paging::WRITABLE)
                        == (crate::memory::paging::USER | crate::memory::paging::WRITABLE)
                },
            )
            && process.address_space.translate(at).is_some()
    })
}

fn write_word_snapshot(
    space: &crate::memory::address_space::AddressSpace,
    address: u64,
    value: u32,
) {
    for (offset, byte) in value.to_ne_bytes().into_iter().enumerate() {
        let at = address + offset as u64;
        let Some(physical) = space.translate(at) else {
            return;
        };
        // The address was checked before the child was published; individual
        // translations also make a word crossing a page boundary safe.
        unsafe {
            (nexus_abi::layout::phys_to_virt(physical) as *mut u8).write_volatile(byte);
        }
    }
}
