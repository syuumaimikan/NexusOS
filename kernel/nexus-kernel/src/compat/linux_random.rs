//! A character endpoint backed by the native hardware random service.
//! No seeded constant, uptime generator, entropy writes or fabricated pool.
use super::{linux::error, linux_files::Metadata};
use crate::ipc::{Object, Rights};
use alloc::sync::Arc;

pub(super) fn known(path: &str) -> bool {
    path == "/dev/urandom"
}

pub(super) fn metadata() -> Metadata {
    Metadata {
        device: 3,
        rdevice: 0x109,
        inode: 1,
        mode: 0o020444,
        links: 1,
        size: 0,
        blocks: 0,
    }
}

pub(super) fn open(flags: u64) -> u64 {
    if flags & 0o200000 != 0 {
        return (-20i64) as u64;
    }
    if flags & (0o100 | 0o200) == (0o100 | 0o200) {
        return (-17i64) as u64;
    }
    if flags & 3 != 0 || flags & 0o1000 != 0 {
        return (-13i64) as u64;
    }
    let Some(process) = crate::sched::current_process() else {
        return error::EBADF;
    };
    let device = Arc::new(crate::random::Device::new(flags & 0o4000 != 0));
    let descriptor = process.handles.insert(
        Object::Random(device),
        Rights::READ | Rights::CLOSE | Rights::TRANSFER,
    );
    descriptor as u64
}

pub(super) fn read(descriptor: u64, buffer: u64, count: u64) -> u64 {
    let Some(process) = crate::sched::current_process() else {
        return error::EBADF;
    };
    let Ok(id) = u32::try_from(descriptor) else {
        return error::EBADF;
    };
    if !matches!(
        process.handles.object(id, Rights::READ),
        Ok(Object::Random(_))
    ) {
        return error::EBADF;
    }
    let count = count.min(65536);
    if count == 0 {
        return 0;
    }
    let Some((output, length)) =
        crate::arch::syscall::user_range_for(&process, buffer, count, 65536, true)
    else {
        return error::EFAULT;
    };
    if !crate::random::available() {
        return (-19i64) as u64;
    }
    let mut bytes = alloc::vec![0u8; length];
    // The native service only succeeds after every word has been obtained.
    // Failure therefore leaves the user buffer untouched.
    if !crate::random::bytes(&mut bytes) {
        return (-11i64) as u64;
    }
    unsafe {
        core::ptr::copy_nonoverlapping(bytes.as_ptr(), output as *mut u8, length);
    }
    count
}

pub(super) fn descriptor_metadata(descriptor: u64) -> Option<Metadata> {
    let process = crate::sched::current_process()?;
    let id = u32::try_from(descriptor).ok()?;
    matches!(
        process.handles.object(id, Rights::NONE),
        Ok(Object::Random(_))
    )
    .then(metadata)
}
