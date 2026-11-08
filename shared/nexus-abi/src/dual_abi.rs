//! Dual Subsystem ABI definitions, Calling Convention Thunking, and Win32/NT Interop.
//!
//! Provides the core data structures and abstraction primitives to bridge
//! between Linux (System V AMD64 ABI) and Windows (Microsoft x64 ABI), enabling
//! binary loader arbitration, NT system call translation, Win32 messaging,
//! registry emulation, COM components, and VFS path mapping.

/// Calling convention identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallingConvention {
    /// System V AMD64 ABI (Linux, BSD, NexusOS native):
    /// - Args: RDI, RSI, RDX, RCX, R8, R9.
    /// - Callee-saved: RBX, RSP, RBP, R12, R13, R14, R15.
    /// - Return: RAX, RDX.
    SysVAmd64,

    /// Microsoft x64 ABI (Windows Win32 / NT):
    /// - Args: RCX, RDX, R8, R9 (first 4 arguments).
    /// - Shadow Space: 32 bytes allocated by caller on stack above return address.
    /// - Callee-saved: RBX, RBP, RDI, RSI, RSP, R12, R13, R14, R15, XMM6-XMM15.
    /// - Return: RAX (or hidden RCX pointer for large structs).
    MsX64,
}

/// Shadow space size required by Microsoft x64 ABI (4 quadwords = 32 bytes).
pub const MS_X64_SHADOW_SPACE_BYTES: usize = 32;

/// Standard stack alignment requirement on function entry (16 bytes).
pub const ABI_STACK_ALIGNMENT: usize = 16;

/// Register state representation for thunking and ABI boundary translation.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[repr(C)]
pub struct ThunkRegisters {
    pub rax: u64,
    pub rbx: u64,
    pub rcx: u64,
    pub rdx: u64,
    pub rsi: u64,
    pub rdi: u64,
    pub rbp: u64,
    pub rsp: u64,
    pub r8: u64,
    pub r9: u64,
    pub r10: u64,
    pub r11: u64,
    pub r12: u64,
    pub r13: u64,
    pub r14: u64,
    pub r15: u64,
    pub rflags: u64,
}

impl ThunkRegisters {
    /// Convert incoming System V AMD64 argument registers into Microsoft x64 calling convention.
    ///
    /// System V: arg0=RDI, arg1=RSI, arg2=RDX, arg3=RCX, arg4=R8, arg5=R9
    /// MS x64:   arg0=RCX, arg1=RDX, arg2=R8,  arg3=R9,  arg4=Stack[0], arg5=Stack[8]
    ///
    /// Returns the MS x64 register state along with the 5th and 6th arguments destined for shadow stack.
    pub fn sysv_to_msx64(&self) -> (Self, u64, u64) {
        let mut ms = *self;
        ms.rcx = self.rdi; // arg 0
        ms.rdx = self.rsi; // arg 1
        ms.r8 = self.rdx; // arg 2
        ms.r9 = self.rcx; // arg 3
        let stack_arg4 = self.r8;
        let stack_arg5 = self.r9;
        (ms, stack_arg4, stack_arg5)
    }

    /// Convert incoming Microsoft x64 argument registers into System V AMD64 calling convention.
    ///
    /// MS x64:   arg0=RCX, arg1=RDX, arg2=R8, arg3=R9, stack_arg4, stack_arg5
    /// System V: arg0=RDI, arg1=RSI, arg2=RDX, arg3=RCX, arg4=R8, arg5=R9
    pub fn msx64_to_sysv(&self, stack_arg4: u64, stack_arg5: u64) -> Self {
        let mut sysv = *self;
        sysv.rdi = self.rcx; // arg 0
        sysv.rsi = self.rdx; // arg 1
        sysv.rdx = self.r8; // arg 2
        sysv.rcx = self.r9; // arg 3
        sysv.r8 = stack_arg4; // arg 4
        sysv.r9 = stack_arg5; // arg 5
        sysv
    }
}

// ============================================================================
// NT Native Subsystem & System Call Codes
// ============================================================================

/// NTSTATUS status code representation.
pub type NtStatus = i32;

pub const STATUS_SUCCESS: NtStatus = 0x0000_0000;
pub const STATUS_UNSUCCESSFUL: NtStatus = -1073741823; // 0xC0000001
pub const STATUS_NOT_IMPLEMENTED: NtStatus = -1073741822; // 0xC0000002
pub const STATUS_INVALID_HANDLE: NtStatus = -1073741816; // 0xC0000008
pub const STATUS_INVALID_PARAMETER: NtStatus = -1073741811; // 0xC000000D
pub const STATUS_NO_MEMORY: NtStatus = -1073741801; // 0xC0000017
pub const STATUS_ACCESS_DENIED: NtStatus = -1073741790; // 0xC0000022
pub const STATUS_BUFFER_TOO_SMALL: NtStatus = -1073741789; // 0xC0000023
pub const STATUS_OBJECT_NAME_NOT_FOUND: NtStatus = -1073741772; // 0xC0000034
pub const STATUS_OBJECT_NAME_COLLISION: NtStatus = -1073741771; // 0xC0000035
pub const STATUS_PENDING: NtStatus = 0x0000_0103;
pub const STATUS_TIMEOUT: NtStatus = 0x0000_0102;

/// Windows NT native system call identifiers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum NtSyscall {
    NtClose = 0x000F,
    NtCreateFile = 0x0055,
    NtOpenFile = 0x0033,
    NtReadFile = 0x0006,
    NtWriteFile = 0x0008,
    NtAllocateVirtualMemory = 0x0018,
    NtFreeVirtualMemory = 0x001E,
    NtProtectVirtualMemory = 0x0050,
    NtQueryVirtualMemory = 0x0023,
    NtCreateThreadEx = 0x00C1,
    NtTerminateProcess = 0x002C,
    NtWaitForSingleObject = 0x0004,
    NtWaitForMultipleObjects = 0x005A,
    NtCreateEvent = 0x0048,
    NtSetEvent = 0x000E,
    NtResetEvent = 0x0052,
    NtCreateMutant = 0x00B2,
    NtReleaseMutant = 0x001C,
    NtQuerySystemInformation = 0x0036,
}

// ============================================================================
// Win32 Messaging & Event Architecture
// ============================================================================

/// Win32 window handle.
pub type Hwnd = u64;

/// Point coordinate structure.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[repr(C)]
pub struct WinPoint {
    pub x: i32,
    pub y: i32,
}

/// Win32 MSG structure for message loop pump (`GetMessage`, `DispatchMessage`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[repr(C)]
pub struct WinMsg {
    pub hwnd: Hwnd,
    pub message: u32,
    pub w_param: u64,
    pub l_param: i64,
    pub time: u32,
    pub pt: WinPoint,
}

/// Standard Win32 Message Identifiers.
pub const WM_NULL: u32 = 0x0000;
pub const WM_CREATE: u32 = 0x0001;
pub const WM_DESTROY: u32 = 0x0002;
pub const WM_MOVE: u32 = 0x0003;
pub const WM_SIZE: u32 = 0x0005;
pub const WM_ACTIVATE: u32 = 0x0006;
pub const WM_SETFOCUS: u32 = 0x0007;
pub const WM_KILLFOCUS: u32 = 0x0008;
pub const WM_PAINT: u32 = 0x000F;
pub const WM_CLOSE: u32 = 0x0010;
pub const WM_QUIT: u32 = 0x0012;
pub const WM_KEYDOWN: u32 = 0x0100;
pub const WM_KEYUP: u32 = 0x0101;
pub const WM_CHAR: u32 = 0x0102;
pub const WM_MOUSEMOVE: u32 = 0x0200;
pub const WM_LBUTTONDOWN: u32 = 0x0201;
pub const WM_LBUTTONUP: u32 = 0x0202;
pub const WM_RBUTTONDOWN: u32 = 0x0204;
pub const WM_RBUTTONUP: u32 = 0x0205;
pub const WM_COMMAND: u32 = 0x0111;
pub const WM_TIMER: u32 = 0x0113;

/// Windows IME Message Identifiers.
pub const WM_IME_STARTCOMPOSITION: u32 = 0x010D;
pub const WM_IME_ENDCOMPOSITION: u32 = 0x010E;
pub const WM_IME_COMPOSITION: u32 = 0x010F;
pub const WM_IME_SETCONTEXT: u32 = 0x0281;
pub const WM_IME_NOTIFY: u32 = 0x0282;

// ============================================================================
// Windows Registry Definitions
// ============================================================================

/// Predefined Registry Root Keys (HKEY).
pub const HKEY_CLASSES_ROOT: u64 = 0x8000_0000;
pub const HKEY_CURRENT_USER: u64 = 0x8000_0001;
pub const HKEY_LOCAL_MACHINE: u64 = 0x8000_0002;
pub const HKEY_USERS: u64 = 0x8000_0003;
pub const HKEY_PERFORMANCE_DATA: u64 = 0x8000_0004;
pub const HKEY_CURRENT_CONFIG: u64 = 0x8000_0005;

/// Registry Value Types.
pub const REG_NONE: u32 = 0;
pub const REG_SZ: u32 = 1;
pub const REG_EXPAND_SZ: u32 = 2;
pub const REG_BINARY: u32 = 3;
pub const REG_DWORD: u32 = 4;
pub const REG_MULTI_SZ: u32 = 7;
pub const REG_QWORD: u32 = 11;

// ============================================================================
// COM (Component Object Model) Foundations
// ============================================================================

/// 128-bit Globally Unique Identifier (GUID / CLSID / IID).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[repr(C)]
pub struct Guid {
    pub data1: u32,
    pub data2: u16,
    pub data3: u16,
    pub data4: [u8; 8],
}

impl Guid {
    pub const fn new(data1: u32, data2: u16, data3: u16, data4: [u8; 8]) -> Self {
        Self {
            data1,
            data2,
            data3,
            data4,
        }
    }
}

/// Standard COM `IUnknown` interface IID: `{00000000-0000-0000-C000-000000000046}`.
pub const IID_IUNKNOWN: Guid = Guid::new(
    0x00000000,
    0x0000,
    0x0000,
    [0xC0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x46],
);

/// Standard COM `IClassFactory` interface IID: `{00000001-0000-0000-C000-000000000046}`.
pub const IID_ICLASSFACTORY: Guid = Guid::new(
    0x00000001,
    0x0000,
    0x0000,
    [0xC0, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x46],
);

pub type HResult = i32;

pub const S_OK: HResult = 0;
pub const S_FALSE: HResult = 1;
pub const E_NOINTERFACE: HResult = -2147467262; // 0x80004002
pub const E_POINTER: HResult = -2147467261; // 0x80004003
pub const E_FAIL: HResult = -2147467259; // 0x80004005

// ============================================================================
// VFS Path & File Semantics Normalization
// ============================================================================

/// Normalize Windows path (e.g. `C:\Windows\System32\cmd.exe`) into Unix root path (`/windows/system32/cmd.exe`).
pub fn win_to_unix_path(src: &str, dest: &mut [u8]) -> usize {
    let mut out_len = 0;
    let bytes = src.as_bytes();
    let mut i = 0;

    // Check for drive letter: e.g. "C:" or "c:"
    if bytes.len() >= 2 && bytes[1] == b':' {
        let drive = bytes[0].to_ascii_lowercase();
        if drive == b'c' {
            // C: maps directly to /
            i = 2;
        } else {
            // D: maps to /mnt/d
            let prefix = b"/mnt/";
            if out_len + prefix.len() < dest.len() {
                dest[out_len..out_len + prefix.len()].copy_from_slice(prefix);
                out_len += prefix.len();
                dest[out_len] = drive;
                out_len += 1;
            }
            i = 2;
        }
    }

    if out_len == 0
        && (i >= bytes.len() || (bytes[i] != b'/' && bytes[i] != b'\\'))
        && out_len < dest.len()
    {
        dest[out_len] = b'/';
        out_len += 1;
    }

    while i < bytes.len() && out_len < dest.len() {
        let b = bytes[i];
        if b == b'\\' {
            // Prevent consecutive slashes
            if out_len == 0 || dest[out_len - 1] != b'/' {
                dest[out_len] = b'/';
                out_len += 1;
            }
        } else if b == b'/' {
            if out_len == 0 || dest[out_len - 1] != b'/' {
                dest[out_len] = b'/';
                out_len += 1;
            }
        } else {
            dest[out_len] = b.to_ascii_lowercase();
            out_len += 1;
        }
        i += 1;
    }

    out_len
}

/// Case-insensitive ASCII comparison for file and registry paths.
pub fn ascii_case_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    for (x, y) in a.iter().zip(b.iter()) {
        if !x.eq_ignore_ascii_case(y) {
            return false;
        }
    }
    true
}

// ============================================================================
// Synchronization & Kernel Object Model Abstraction
// ============================================================================

/// Unified Kernel Object type for dual subsystem arbitration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KernelObjectType {
    Event {
        manual_reset: bool,
        signaled: bool,
    },
    Mutex {
        owner_thread: Option<u64>,
        recursion: u32,
    },
    Semaphore {
        count: u32,
        max_count: u32,
    },
    Process {
        exit_code: Option<u32>,
    },
    Thread {
        exit_code: Option<u32>,
    },
    FileHandle {
        vfs_handle: u64,
    },
    Channel {
        channel_handle: u64,
    },
}

/// Wait mode for `WaitForMultipleObjects`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitMode {
    /// Wait for any single object to become signaled (WaitAny).
    WaitAny,
    /// Wait for all objects to become signaled simultaneously (WaitAll).
    WaitAll,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_calling_convention_thunk_mapping() {
        let sysv = ThunkRegisters {
            rdi: 10,
            rsi: 20,
            rdx: 30,
            rcx: 40,
            r8: 50,
            r9: 60,
            ..Default::default()
        };

        let (msx64, s4, s5) = sysv.sysv_to_msx64();
        assert_eq!(msx64.rcx, 10);
        assert_eq!(msx64.rdx, 20);
        assert_eq!(msx64.r8, 30);
        assert_eq!(msx64.r9, 40);
        assert_eq!(s4, 50);
        assert_eq!(s5, 60);

        let roundtrip = msx64.msx64_to_sysv(s4, s5);
        assert_eq!(roundtrip.rdi, 10);
        assert_eq!(roundtrip.rsi, 20);
        assert_eq!(roundtrip.rdx, 30);
        assert_eq!(roundtrip.rcx, 40);
        assert_eq!(roundtrip.r8, 50);
        assert_eq!(roundtrip.r9, 60);
    }

    #[test]
    fn test_win_to_unix_path_conversion() {
        let mut dest = [0u8; 128];
        let len = win_to_unix_path("C:\\Windows\\System32\\cmd.exe", &mut dest);
        let s = core::str::from_utf8(&dest[..len]).unwrap();
        assert_eq!(s, "/windows/system32/cmd.exe");

        let mut dest_d = [0u8; 128];
        let len_d = win_to_unix_path("D:\\Games\\Doom.exe", &mut dest_d);
        let s_d = core::str::from_utf8(&dest_d[..len_d]).unwrap();
        assert_eq!(s_d, "/mnt/d/games/doom.exe");
    }

    #[test]
    fn test_ascii_case_equality() {
        assert!(ascii_case_eq(b"kernel32.dll", b"KERNEL32.DLL"));
        assert!(ascii_case_eq(b"Ntdll.Dll", b"ntdll.dll"));
        assert!(!ascii_case_eq(b"user32.dll", b"gdi32.dll"));
    }
}
