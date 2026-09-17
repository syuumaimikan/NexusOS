//! Linux's x86-64 system call numbers.
//!
//! Its own numbering, which is per-architecture: `write` is 1 here and 4 on
//! i386. That the number depends on the architecture is exactly why a
//! translation layer is per-architecture work, and why a 32-bit Linux program
//! needs a second table rather than the same one.
//!
//! Only the ones the programs in this crate use. A list of all three hundred
//! and fifty would be a list nobody checks.

pub const READ: u64 = 0;
pub const WRITE: u64 = 1;
pub const OPEN: u64 = 2;
pub const CLOSE: u64 = 3;
pub const POLL: u64 = 7;
pub const MMAP: u64 = 9;
pub const MPROTECT: u64 = 10;
pub const MUNMAP: u64 = 11;
pub const RT_SIGACTION: u64 = 13;
pub const RT_SIGPROCMASK: u64 = 14;
pub const RT_SIGRETURN: u64 = 15;
pub const IOCTL: u64 = 16;
pub const PIPE: u64 = 22;
pub const SCHED_YIELD: u64 = 24;
pub const DUP: u64 = 32;
pub const DUP2: u64 = 33;
pub const NANOSLEEP: u64 = 35;
pub const GETPID: u64 = 39;
pub const SOCKET: u64 = 41;
pub const CONNECT: u64 = 42;
pub const ACCEPT: u64 = 43;
pub const SENDTO: u64 = 44;
pub const RECVFROM: u64 = 45;
pub const SENDMSG: u64 = 46;
pub const RECVMSG: u64 = 47;
pub const SHUTDOWN: u64 = 48;
pub const BIND: u64 = 49;
pub const LISTEN: u64 = 50;
pub const GETSOCKNAME: u64 = 51;
pub const SOCKETPAIR: u64 = 53;
pub const SETSOCKOPT: u64 = 54;
pub const GETSOCKOPT: u64 = 55;
pub const CLONE: u64 = 56;
pub const EXECVE: u64 = 59;
pub const EXIT: u64 = 60;
pub const KILL: u64 = 62;
pub const UNAME: u64 = 63;
pub const FCNTL: u64 = 72;
pub const GETDENTS64: u64 = 217;
pub const SET_TID_ADDRESS: u64 = 218;
pub const CLOCK_GETTIME: u64 = 228;
pub const EXIT_GROUP: u64 = 231;
pub const EPOLL_CREATE1: u64 = 291;
pub const EPOLL_CTL: u64 = 233;
pub const EPOLL_WAIT: u64 = 232;
pub const TGKILL: u64 = 234;
pub const OPENAT: u64 = 257;
pub const FUTEX: u64 = 202;
pub const SET_ROBUST_LIST: u64 = 273;
pub const ACCEPT4: u64 = 288;
pub const PIPE2: u64 = 293;
pub const DUP3: u64 = 292;
pub const GETRANDOM: u64 = 318;
pub const MEMFD_CREATE: u64 = 319;
pub const FTRUNCATE: u64 = 77;
