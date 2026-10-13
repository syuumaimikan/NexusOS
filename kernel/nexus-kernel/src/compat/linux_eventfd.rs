//! Linux eventfd/eventfd2 translated to a Nexus counted notification object.
//! ABI reference: Linux fs/eventfd.c. Reads accept >= 8 bytes, writes exactly 8.
//! Nonblocking status belongs to the shared object, CLOEXEC to its descriptor.

use super::linux::error;
use crate::event::{Counter, CounterError};
use crate::ipc::{Object, Rights};

pub fn create(initial: u64, flags: u64) -> u64 {
    const SEMAPHORE: u64 = 1;
    const NONBLOCK: u64 = 0o4000;
    const CLOEXEC: u64 = 0o2_000_000;
    if flags & !(SEMAPHORE | NONBLOCK | CLOEXEC) != 0 {
        return error::EINVAL;
    }
    let Some(process) = crate::sched::current_process() else {
        return error::ENOSYS;
    };
    let counter = Counter::new(
        initial as u32,
        flags & SEMAPHORE != 0,
        flags & NONBLOCK != 0,
    );
    let descriptor = process
        .handles
        .insert(Object::Counter(counter), Rights::ALL);
    super::linux_more::remember_cloexec(process.id.0, u64::from(descriptor), flags & CLOEXEC != 0);
    u64::from(descriptor)
}

fn errno(error: CounterError) -> u64 {
    match error {
        CounterError::WouldBlock => (-11i64) as u64,
        CounterError::Cancelled => (-4i64) as u64,
        CounterError::InvalidValue => error::EINVAL,
    }
}

fn counter(descriptor: u64, rights: Rights) -> Result<alloc::sync::Arc<Counter>, u64> {
    let Some(process) = crate::sched::current_process() else {
        return Err(error::ENOSYS);
    };
    let id = u32::try_from(descriptor).map_err(|_| error::EBADF)?;
    match process
        .handles
        .object(id, rights)
        .map_err(|_| error::EBADF)?
    {
        Object::Counter(counter) => Ok(counter),
        _ => Err(error::EBADF),
    }
}

pub fn read(descriptor: u64, address: u64, count: u64) -> u64 {
    let counter = match counter(descriptor, Rights::READ) {
        Ok(value) => value,
        Err(error) => return error,
    };
    if count < 8 {
        return error::EINVAL;
    }
    let Some(process) = crate::sched::current_process() else {
        return error::ENOSYS;
    };
    let Some((out, _)) = crate::arch::syscall::user_range_for(&process, address, 8, 8, true) else {
        return error::EFAULT;
    };
    drop(process);
    let value = match counter.read() {
        Ok(value) => value,
        Err(error) => return errno(error),
    };
    // Another thread can unmap or protect the output while this read waits.
    let Some(process) = crate::sched::current_process() else {
        return error::ENOSYS;
    };
    if crate::arch::syscall::user_range_for(&process, out, 8, 8, true).is_none() {
        return error::EFAULT;
    }
    unsafe {
        (out as *mut u64).write_unaligned(value);
    }
    8
}

pub fn write(descriptor: u64, address: u64, count: u64) -> u64 {
    let counter = match counter(descriptor, Rights::WRITE) {
        Ok(value) => value,
        Err(error) => return error,
    };
    if count != 8 {
        return error::EINVAL;
    }
    let Some((input, _)) = crate::arch::syscall::user_range(address, 8, 8) else {
        return error::EFAULT;
    };
    let value = unsafe { (input as *const u64).read_unaligned() };
    match counter.write(value) {
        Ok(()) => 8,
        Err(error) => errno(error),
    }
}
