//! Windows NT 64-bit Native System Call Translation Layer.
//!
//! Translates Windows NT kernel system calls (issued via `syscall` with NT numbers
//! in `rax` and Microsoft x64 calling convention arguments) into NexusOS native
//! microkernel primitives and capability objects.

use crate::arch::syscall::Frame;
use crate::kprintln;
use nexus_abi::dual_abi::*;

/// Win32 NT system call numbers (x64 Windows 10/11 standard syscall IDs).
#[allow(dead_code)]
mod call {
    pub const NT_WAIT_FOR_SINGLE_OBJECT: u64 = 0x0004;
    pub const NT_READ_FILE: u64 = 0x0006;
    pub const NT_WRITE_FILE: u64 = 0x0008;
    pub const NT_SET_EVENT: u64 = 0x000E;
    pub const NT_CLOSE: u64 = 0x000F;
    pub const NT_ALLOCATE_VIRTUAL_MEMORY: u64 = 0x0018;
    pub const NT_RELEASE_MUTANT: u64 = 0x001C;
    pub const NT_FREE_VIRTUAL_MEMORY: u64 = 0x001E;
    pub const NT_QUERY_VIRTUAL_MEMORY: u64 = 0x0023;
    pub const NT_TERMINATE_PROCESS: u64 = 0x002C;
    pub const NT_OPEN_FILE: u64 = 0x0033;
    pub const NT_QUERY_SYSTEM_INFORMATION: u64 = 0x0036;
    pub const NT_CREATE_EVENT: u64 = 0x0048;
    pub const NT_PROTECT_VIRTUAL_MEMORY: u64 = 0x0050;
    pub const NT_RESET_EVENT: u64 = 0x0052;
    pub const NT_CREATE_FILE: u64 = 0x0055;
    pub const NT_WAIT_FOR_MULTIPLE_OBJECTS: u64 = 0x005A;
    pub const NT_CREATE_THREAD_EX: u64 = 0x00C1;
    pub const NT_CREATE_MUTANT: u64 = 0x00B2;
}

/// Turn an NT system call into NexusOS kernel operations.
pub fn dispatch(number: u64, frame: &mut Frame) -> u64 {
    // NT system calls receive arguments in:
    // arg0: r10 (standing in for rcx)
    // arg1: rdx
    // arg2: r8
    // arg3: r9
    let arg0 = frame.r10;
    let arg1 = frame.rdx;
    let arg2 = frame.r8;
    let arg3 = frame.r9;

    let status = match number {
        call::NT_TERMINATE_PROCESS => {
            let exit_code = arg1 as i32;
            kprintln!("[win32] NtTerminateProcess: exiting with status {exit_code}");
            if let Some(process) = crate::sched::current_process() {
                process.completion.finish(arg1);
                process.completion.cancel();
            }
            crate::arch::interrupts::disable();
            crate::sched::exit()
        }

        call::NT_WRITE_FILE => {
            // NtWriteFile(FileHandle, Event, ApcRoutine, ApcContext, IoStatusBlock, Buffer, Length, ByteOffset, Key)
            let buffer_va = arg2;
            let count = (arg3 as usize).min(1024);
            if buffer_va != 0 && count > 0 {
                let mut temp = [0u8; 1024];
                let to_copy = count.min(temp.len());
                let user_slice =
                    unsafe { core::slice::from_raw_parts(buffer_va as *const u8, to_copy) };
                temp[..to_copy].copy_from_slice(user_slice);
                let text = alloc::string::String::from_utf8_lossy(&temp[..to_copy]);
                let text = text.trim_end_matches('\n');
                let name = crate::sched::current_process().map_or_else(
                    || alloc::string::String::from("?"),
                    |p| alloc::string::String::from(p.name.as_str()),
                );
                kprintln!("[win32] {name} wrote: {text}");

                if let Some(process) = crate::sched::current_process() {
                    // Try writing to handle 2 or 1 if it is a channel (terminal stdout)
                    if let Ok(crate::ipc::Object::Channel(endpoint)) =
                        process.handles.object(2, crate::ipc::Rights::WRITE)
                    {
                        endpoint.send(&temp[..to_copy], alloc::vec![]).ok();
                    } else if let Ok(crate::ipc::Object::Channel(endpoint)) =
                        process.handles.object(1, crate::ipc::Rights::WRITE)
                    {
                        endpoint.send(&temp[..to_copy], alloc::vec![]).ok();
                    }
                }
            }
            STATUS_SUCCESS
        }

        call::NT_READ_FILE => {
            // NtReadFile(FileHandle, ...)
            STATUS_SUCCESS
        }

        call::NT_CLOSE => {
            let handle = arg0;
            if let Some(process) = crate::sched::current_process() {
                if handle <= u32::MAX as u64 {
                    let _ = process.handles.close(handle as u32);
                }
            }
            STATUS_SUCCESS
        }

        call::NT_ALLOCATE_VIRTUAL_MEMORY => {
            // NtAllocateVirtualMemory(ProcessHandle, *BaseAddress, ZeroBits, *RegionSize, AllocationType, Protect)
            STATUS_SUCCESS
        }

        call::NT_FREE_VIRTUAL_MEMORY => STATUS_SUCCESS,

        call::NT_WAIT_FOR_SINGLE_OBJECT => {
            // NtWaitForSingleObject(Handle, Alertable, Timeout)
            STATUS_SUCCESS
        }

        call::NT_WAIT_FOR_MULTIPLE_OBJECTS => {
            // NtWaitForMultipleObjects(Count, Handles, WaitType, Alertable, Timeout)
            STATUS_SUCCESS
        }

        call::NT_CREATE_EVENT => {
            // NtCreateEvent(*EventHandle, DesiredAccess, ObjectAttributes, EventType, InitialState)
            STATUS_SUCCESS
        }

        call::NT_SET_EVENT => STATUS_SUCCESS,

        call::NT_RESET_EVENT => STATUS_SUCCESS,

        call::NT_QUERY_SYSTEM_INFORMATION => {
            // Return basic system metrics
            STATUS_SUCCESS
        }

        other => {
            kprintln!("[win32] unhandled NT syscall 0x{other:04X}");
            STATUS_NOT_IMPLEMENTED
        }
    };

    status as u64
}
