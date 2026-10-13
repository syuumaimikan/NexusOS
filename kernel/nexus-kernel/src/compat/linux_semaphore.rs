//! System V semaphore sets backed by Nexus wait queues.
//!
//! Operations commit as one transaction. A blocked transaction changes no
//! values or undo adjustments. Generation-based waits close the gap between
//! checking values and joining the queue. Never wake a queue while holding the
//! registry lock: clone registers shared undo state under the scheduler lock.
//!
//! ABI references: Linux include/uapi/linux/sem.h, ipc/syscall.c and
//! arch/x86/include/uapi/asm/sembuf.h. Message queues and SysV shared memory
//! are separate services and remain ENOSYS in the old ipc multiplexer.

use super::linux::error;
use crate::arch::syscall::user_range;
use crate::sched::wait::WaitQueue;
use crate::sync::IrqSpinLock;
use alloc::collections::BTreeMap;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;

const MAX_SETS: usize = 128;
const MAX_SEMS: usize = 256;
const MAX_TOTAL: usize = 4096;
const MAX_OPS: usize = 128;
const MAX_UNDO: usize = 512;
const MAX_VALUE: i32 = 32767;
const CREATE: u64 = 0o1000;
const EXCLUSIVE: u64 = 0o2000;
const NOWAIT: u16 = 0o4000;
const UNDO: u16 = 0x1000;
const IPC64: u64 = 0x100;
const EAGAIN: u64 = (-11i64) as u64;
const EIDRM: u64 = (-43i64) as u64;
const ERANGE: u64 = (-34i64) as u64;
const ENOSPC: u64 = (-28i64) as u64;
const EINTR: u64 = (-4i64) as u64;

struct Set {
    slot: usize,
    sequence: u16,
    key: i32,
    uid: u32,
    gid: u32,
    mode: u32,
    values: Vec<u16>,
    pids: Vec<u32>,
    negative: Vec<u32>,
    zero: Vec<u32>,
    created: u64,
    operated: u64,
    changed: Arc<WaitQueue>,
}

struct UndoGroup {
    owners: usize,
    adjustments: Vec<((u32, usize), i32)>,
}

struct Registry {
    sets: BTreeMap<u32, Set>,
    sequences: [u16; MAX_SETS],
    threads: BTreeMap<u64, u64>,
    undo: BTreeMap<u64, UndoGroup>,
}

static REGISTRY: IrqSpinLock<Registry> = IrqSpinLock::new(Registry {
    sets: BTreeMap::new(),
    sequences: [0; MAX_SETS],
    threads: BTreeMap::new(),
    undo: BTreeMap::new(),
});

fn now() -> u64 {
    crate::drivers::rtc::now().unwrap_or(crate::arch::time::uptime_ms() / 1000)
}

fn caller_pid() -> Option<u32> {
    crate::sched::current_process().map(|process| process.id.0 as u32)
}

/// Find a named set or create a new, initially zero-valued set. IPC_PRIVATE
/// always creates; an existing key may be looked up with nsems == 0.
pub fn get(key: u64, count: u64, flags: u64) -> u64 {
    if count > MAX_SEMS as u64 || flags & !(0o777 | CREATE | EXCLUSIVE) != 0 {
        return error::EINVAL;
    }
    let key = key as u32 as i32;
    let mut registry = REGISTRY.lock();
    if key != 0 {
        if let Some((id, set)) = registry.sets.iter().find(|(_, set)| set.key == key) {
            if flags & (CREATE | EXCLUSIVE) == (CREATE | EXCLUSIVE) {
                return (-17i64) as u64;
            }
            return if count as usize > set.values.len() {
                error::EINVAL
            } else {
                u64::from(*id)
            };
        }
        if flags & CREATE == 0 {
            return (-2i64) as u64;
        }
    }
    if count == 0 {
        return error::EINVAL;
    }
    let total: usize = registry.sets.values().map(|set| set.values.len()).sum();
    if total + count as usize > MAX_TOTAL {
        return ENOSPC;
    }
    let Some(slot) = (0..MAX_SETS).find(|slot| {
        registry.sequences[*slot] < 0x8000 && !registry.sets.values().any(|set| set.slot == *slot)
    }) else {
        return ENOSPC;
    };
    let sequence = registry.sequences[slot];
    registry.sequences[slot] += 1;
    let id = (u32::from(sequence) << 16) | slot as u32;
    registry.sets.insert(
        id,
        Set {
            slot,
            sequence,
            key,
            uid: 0,
            gid: 0,
            mode: flags as u32 & 0o777,
            values: vec![0; count as usize],
            pids: vec![0; count as usize],
            negative: vec![0; count as usize],
            zero: vec![0; count as usize],
            created: now(),
            operated: 0,
            changed: Arc::new(WaitQueue::new()),
        },
    );
    u64::from(id)
}

/// Register undo ownership before a clone becomes runnable. CLONE_SYSVSEM
/// shares the parent's undo list; otherwise the child gets a list on first use.
pub fn inherit_undo(parent: u64, child: u64, shared: bool) {
    if !shared {
        return;
    }
    let mut registry = REGISTRY.lock();
    let group = undo_group(&mut registry, parent);
    registry.threads.insert(child, group);
    registry.undo.get_mut(&group).unwrap().owners += 1;
}

fn undo_group(registry: &mut Registry, thread: u64) -> u64 {
    if let Some(group) = registry.threads.get(&thread) {
        return *group;
    }
    registry.threads.insert(thread, thread);
    registry.undo.insert(
        thread,
        UndoGroup {
            owners: 1,
            adjustments: Vec::new(),
        },
    );
    thread
}

/// Apply an undo list only when its last sharing thread exits, including
/// forced termination. The scheduler calls this before taking its own lock.
pub fn ending(thread: u64) {
    // The process ID is a thread-group ID, including when the last owner
    // of a shared undo list is a worker rather than its original creator.
    let pid = caller_pid().unwrap_or(thread as u32);
    let changed = {
        let mut registry = REGISTRY.lock();
        let Some(group_id) = registry.threads.remove(&thread) else {
            return;
        };
        let Some(group) = registry.undo.get_mut(&group_id) else {
            return;
        };
        group.owners -= 1;
        if group.owners != 0 {
            return;
        }
        let group = registry.undo.remove(&group_id).unwrap();
        let mut changed = Vec::new();
        for ((id, index), adjustment) in group.adjustments {
            if let Some(set) = registry.sets.get_mut(&id) {
                let value = (i32::from(set.values[index]) + adjustment).clamp(0, MAX_VALUE);
                set.values[index] = value as u16;
                set.pids[index] = pid;
                set.operated = now();
                if !changed.iter().any(|queue| Arc::ptr_eq(queue, &set.changed)) {
                    changed.push(Arc::clone(&set.changed));
                }
            }
        }
        changed
    };
    for queue in changed {
        queue.wake_all();
    }
}

#[derive(Clone, Copy)]
struct Operation {
    index: usize,
    delta: i16,
    flags: u16,
}

fn read_operations(address: u64, count: u64) -> Result<Vec<Operation>, u64> {
    if count == 0 {
        return Err(error::EINVAL);
    }
    if count > MAX_OPS as u64 {
        return Err((-7i64) as u64);
    }
    let (pointer, _) = user_range(address, count * 6, MAX_OPS as u64 * 6).ok_or(error::EFAULT)?;
    let mut operations = Vec::with_capacity(count as usize);
    for at in 0..count as usize {
        let fields = unsafe { ((pointer + at as u64 * 6) as *const [u16; 3]).read_unaligned() };
        if fields[2] & !(NOWAIT | UNDO) != 0 {
            return Err(error::EINVAL);
        }
        operations.push(Operation {
            index: fields[0] as usize,
            delta: fields[1] as i16,
            flags: fields[2],
        });
    }
    Ok(operations)
}

fn deadline(address: u64, time32: bool) -> Result<Option<u64>, u64> {
    if address == 0 {
        return Ok(None);
    }
    let size = if time32 { 8 } else { 16 };
    let (pointer, _) = user_range(address, size, size).ok_or(error::EFAULT)?;
    let (seconds, nanos) = unsafe {
        if time32 {
            let [s, n] = (pointer as *const [i32; 2]).read_unaligned();
            (i64::from(s), i64::from(n))
        } else {
            let [s, n] = (pointer as *const [i64; 2]).read_unaligned();
            (s, n)
        }
    };
    if seconds < 0 || !(0..1_000_000_000).contains(&nanos) {
        return Err(error::EINVAL);
    }
    let rate = crate::arch::time::frequency_hz();
    let ticks = (seconds as u64)
        .saturating_mul(rate)
        .saturating_add((nanos as u64).saturating_mul(rate).div_ceil(1_000_000_000));
    Ok(Some(crate::arch::time::ticks().saturating_add(ticks)))
}

/// Atomically apply a vector of signed changes or zero-value waits.
pub fn operate(id: u64, address: u64, count: u64, timeout: u64, time32: bool) -> u64 {
    let Ok(id) = u32::try_from(id) else {
        return error::EINVAL;
    };
    let operations = match read_operations(address, count) {
        Ok(value) => value,
        Err(error) => return error,
    };
    let deadline = match deadline(timeout, time32) {
        Ok(value) => value,
        Err(error) => return error,
    };
    let Some(pid) = caller_pid() else {
        return error::ENOSYS;
    };
    let thread = crate::arch::percpu::current_thread();
    let mut was_valid = false;
    loop {
        if crate::sched::cancelled() {
            return EINTR;
        }
        let mut trial = [0u16; MAX_SEMS];
        let (queue, seen, blocked) = {
            let mut registry = REGISTRY.lock();
            let mut old_undo = [0i32; MAX_SEMS];
            if let Some(group) = registry
                .threads
                .get(&thread)
                .and_then(|group| registry.undo.get(group))
            {
                for ((set_id, index), value) in &group.adjustments {
                    if *set_id == id {
                        old_undo[*index] = *value;
                    }
                }
            }
            let mut adjustments = [0i32; MAX_SEMS];
            let Some(set) = registry.sets.get_mut(&id) else {
                return if was_valid { EIDRM } else { error::EINVAL };
            };
            was_valid = true;
            let seen = set.changed.generation();
            let size = set.values.len();
            // Check every index before attempting any operation.
            if operations.iter().any(|op| op.index >= size) {
                return (-27i64) as u64;
            }
            trial[..size].copy_from_slice(&set.values);
            let mut blocked = None;
            for op in &operations {
                let current = i32::from(trial[op.index]);
                let next = current + i32::from(op.delta);
                if next > MAX_VALUE {
                    return ERANGE;
                }
                if (op.delta == 0 && current != 0) || next < 0 {
                    if op.flags & NOWAIT != 0 {
                        return EAGAIN;
                    }
                    blocked = Some((op.index, op.delta == 0));
                    break;
                }
                if op.flags & UNDO != 0 {
                    adjustments[op.index] -= i32::from(op.delta);
                    if !(-MAX_VALUE - 1..=MAX_VALUE)
                        .contains(&(old_undo[op.index] + adjustments[op.index]))
                    {
                        return ERANGE;
                    }
                }
                trial[op.index] = next as u16;
            }
            let queue = Arc::clone(&set.changed);
            if let Some((index, zero)) = blocked {
                if deadline.is_some_and(|end| crate::arch::time::ticks() >= end) {
                    return EAGAIN;
                }
                let counters = if zero {
                    &mut set.zero
                } else {
                    &mut set.negative
                };
                counters[index] = counters[index].saturating_add(1);
            } else {
                // Only a successful vector commits its accumulated undo deltas.
                if adjustments[..size].iter().any(|delta| *delta != 0) {
                    let group_id = undo_group(&mut registry, thread);
                    let group = registry.undo.get_mut(&group_id).unwrap();
                    let mut new_entries = 0;
                    for (index, delta) in adjustments[..size]
                        .iter()
                        .enumerate()
                        .filter(|(_, delta)| **delta != 0)
                    {
                        let old = group
                            .adjustments
                            .iter()
                            .find(|(key, _)| *key == (id, index));
                        let next = old.map_or(0, |(_, value)| *value) + *delta;
                        if !(-MAX_VALUE - 1..=MAX_VALUE).contains(&next) {
                            return ERANGE;
                        }
                        if old.is_none() {
                            new_entries += 1;
                        }
                    }
                    if group.adjustments.len() + new_entries > MAX_UNDO {
                        return ENOSPC;
                    }
                    if group.adjustments.try_reserve(new_entries).is_err() {
                        return error::ENOMEM;
                    }
                    for (index, delta) in adjustments[..size]
                        .iter()
                        .enumerate()
                        .filter(|(_, delta)| **delta != 0)
                    {
                        if let Some((_, value)) = group
                            .adjustments
                            .iter_mut()
                            .find(|(key, _)| *key == (id, index))
                        {
                            *value += *delta;
                        } else {
                            group.adjustments.push(((id, index), *delta));
                        }
                    }
                    group.adjustments.retain(|(_, value)| *value != 0);
                }
                let set = registry.sets.get_mut(&id).unwrap();
                set.values.copy_from_slice(&trial[..size]);
                for op in &operations {
                    set.pids[op.index] = pid;
                }
                set.operated = now();
            }
            (queue, seen, blocked)
        };
        let Some((index, zero)) = blocked else {
            queue.wake_all();
            return 0;
        };
        if let Some(end) = deadline {
            queue.wait_if_unchanged_until(seen, end);
        } else {
            queue.wait_if_unchanged(seen);
        }
        let mut registry = REGISTRY.lock();
        if let Some(set) = registry.sets.get_mut(&id) {
            let counters = if zero {
                &mut set.zero
            } else {
                &mut set.negative
            };
            counters[index] = counters[index].saturating_sub(1);
        }
    }
}

#[derive(Clone, Copy)]
pub enum Layout {
    Native64,
    I386,
    I386Old,
}

fn write_u32(bytes: &mut [u8], offset: usize, value: u32) {
    bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}
fn write_u64(bytes: &mut [u8], offset: usize, value: u64) {
    bytes[offset..offset + 8].copy_from_slice(&value.to_le_bytes());
}

fn metadata(set: &Set, layout: Layout) -> Vec<u8> {
    let mut bytes = vec![
        0u8;
        match layout {
            Layout::Native64 => 104,
            Layout::I386 => 64,
            Layout::I386Old => 44,
        }
    ];
    write_u32(&mut bytes, 0, set.key as u32);
    match layout {
        Layout::I386Old => {
            for (offset, value) in [
                (4, set.uid),
                (6, set.gid),
                (12, set.mode),
                (14, u32::from(set.sequence)),
                (40, set.values.len() as u32),
            ] {
                bytes[offset..offset + 2].copy_from_slice(&(value as u16).to_le_bytes());
            }
            write_u32(&mut bytes, 16, set.operated as u32);
            write_u32(&mut bytes, 20, set.created as u32);
        }
        _ => {
            write_u32(&mut bytes, 4, set.uid);
            write_u32(&mut bytes, 8, set.gid);
            write_u32(&mut bytes, 20, set.mode);
            bytes[24..26].copy_from_slice(&set.sequence.to_le_bytes());
            if matches!(layout, Layout::I386) {
                write_u64(&mut bytes, 36, set.operated);
                write_u64(&mut bytes, 44, set.created);
                write_u32(&mut bytes, 52, set.values.len() as u32);
            } else {
                write_u64(&mut bytes, 48, set.operated);
                write_u64(&mut bytes, 64, set.created);
                write_u64(&mut bytes, 80, set.values.len() as u64);
            }
        }
    }
    bytes
}

/// semctl's argument is a value/pointer in the direct ABI; old i386 ipc
/// dereferences its union argument before calling here.
pub fn control(id: u64, index: u64, command: u64, argument: u64, layout: Layout) -> u64 {
    let command = command & !IPC64;
    let Some(process) = crate::sched::current_process() else {
        return error::ENOSYS;
    };
    let pid = process.id.0 as u32;
    // clone takes the scheduler lock before REGISTRY. Range validation inside
    // this lock must use the resolved process instead of reacquiring SCHEDULER.
    let range = |address, length, writing| {
        crate::arch::syscall::user_range_for(&process, address, length, length, writing)
    };
    let Ok(id) = u32::try_from(id) else {
        return error::EINVAL;
    };
    let mut registry = REGISTRY.lock();
    if command == 3 || command == 19 {
        let Some((pointer, _)) = range(argument, 40, true) else {
            return error::EFAULT;
        };
        let counts = if command == 19 {
            (
                registry.sets.len(),
                registry.sets.values().map(|s| s.values.len()).sum(),
            )
        } else {
            (20, MAX_VALUE as usize)
        };
        let info = [
            MAX_TOTAL as i32,
            MAX_SETS as i32,
            MAX_TOTAL as i32,
            MAX_TOTAL as i32,
            MAX_SEMS as i32,
            MAX_OPS as i32,
            MAX_UNDO as i32,
            counts.0 as i32,
            MAX_VALUE,
            counts.1 as i32,
        ];
        unsafe {
            (pointer as *mut [i32; 10]).write_unaligned(info);
        }
        return registry
            .sets
            .values()
            .map(|set| set.slot as u64)
            .max()
            .unwrap_or(0);
    }
    let (actual_id, answer_id) = if command == 18 || command == 20 {
        let Some((actual, _)) = registry
            .sets
            .iter()
            .find(|(_, set)| set.slot == id as usize)
        else {
            return error::EINVAL;
        };
        (*actual, u64::from(*actual))
    } else {
        (id, 0)
    };
    if command == 0 {
        let Some(set) = registry.sets.remove(&actual_id) else {
            return error::EINVAL;
        };
        for group in registry.undo.values_mut() {
            group
                .adjustments
                .retain(|((set_id, _), _)| *set_id != actual_id);
        }
        drop(registry);
        set.changed.wake_all();
        return 0;
    }
    let Some(set) = registry.sets.get_mut(&actual_id) else {
        return error::EINVAL;
    };
    let size = set.values.len();
    match command {
        1 => {
            let old = matches!(layout, Layout::I386Old);
            let length = if old { 16 } else { 36 };
            let Some((pointer, _)) = range(argument, length, false) else {
                return error::EFAULT;
            };
            let fields =
                unsafe { core::slice::from_raw_parts(pointer as *const u8, length as usize) };
            let field32 = |at| u32::from_le_bytes(fields[at..at + 4].try_into().unwrap());
            let field16 = |at| u16::from_le_bytes(fields[at..at + 2].try_into().unwrap()) as u32;
            set.uid = if old { field16(4) } else { field32(4) };
            set.gid = if old { field16(6) } else { field32(8) };
            set.mode = (if old { field16(12) } else { field32(20) }) & 0o777;
            set.created = now();
            return 0;
        }
        2 | 18 | 20 => {
            let bytes = metadata(set, layout);
            let Some((pointer, _)) = range(argument, bytes.len() as u64, true) else {
                return error::EFAULT;
            };
            unsafe {
                core::ptr::copy_nonoverlapping(bytes.as_ptr(), pointer as *mut u8, bytes.len());
            }
            return answer_id;
        }
        13 => {
            let Some((pointer, _)) = range(argument, (size * 2) as u64, true) else {
                return error::EFAULT;
            };
            for (index, value) in set.values.iter().enumerate() {
                unsafe {
                    ((pointer + (index * 2) as u64) as *mut u16).write_unaligned(*value);
                }
            }
            return 0;
        }
        11 | 12 | 14 | 15 | 16 => {
            if index >= size as u64 {
                return error::EINVAL;
            }
            let index = index as usize;
            match command {
                11 => return u64::from(set.pids[index]),
                12 => return u64::from(set.values[index]),
                14 => return u64::from(set.negative[index]),
                15 => return u64::from(set.zero[index]),
                _ => {
                    if (argument as u32 as i32) < 0 || argument as u32 > MAX_VALUE as u32 {
                        return ERANGE;
                    }
                    set.values[index] = argument as u16;
                    set.pids[index] = pid;
                }
            }
        }
        17 => {
            let Some((pointer, _)) = range(argument, (size * 2) as u64, false) else {
                return error::EFAULT;
            };
            let mut values = [0u16; MAX_SEMS];
            for (index, value) in values[..size].iter_mut().enumerate() {
                *value = unsafe { ((pointer + (index * 2) as u64) as *const u16).read_unaligned() };
                if i32::from(*value) > MAX_VALUE {
                    return ERANGE;
                }
            }
            set.values.copy_from_slice(&values[..size]);
            set.pids.fill(pid);
        }
        _ => return error::EINVAL,
    }
    set.created = now();
    let queue = Arc::clone(&set.changed);
    for group in registry.undo.values_mut() {
        group.adjustments.retain(|((set_id, semaphore), _)| {
            *set_id != actual_id || (command == 16 && *semaphore as u64 != index)
        });
    }
    drop(registry);
    queue.wake_all();
    0
}

/// The original i386 multiplexed ipc(2) ABI.
pub fn ipc32(call: u64, first: u64, second: u64, third: u64, pointer: u64, fifth: u64) -> u64 {
    match call & 0xffff {
        1 => operate(first, pointer, second, 0, true),
        2 => get(first, second, third),
        3 => {
            if pointer == 0 {
                return error::EINVAL;
            }
            let Some((at, _)) = user_range(pointer, 4, 4) else {
                return error::EFAULT;
            };
            let argument = unsafe { (at as *const u32).read_unaligned() };
            let layout = if third & IPC64 != 0 {
                Layout::I386
            } else {
                Layout::I386Old
            };
            control(first, second, third, u64::from(argument), layout)
        }
        4 => operate(first, pointer, second, fifth, true),
        other => {
            crate::kprintln!("[linux32] ipc operation {other} is not translated; answering ENOSYS");
            error::ENOSYS
        }
    }
}
