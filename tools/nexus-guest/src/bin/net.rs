//! A server and a client on one machine, over a Unix domain socket.
//!
//! This is the shape every desktop protocol on Linux has. A server binds a name
//! in the filesystem, listens, and accepts whoever arrives; a client that was
//! started by something else entirely connects to that name, sends a request
//! and reads a reply. X11 does it, Wayland does it, PulseAudio does it.
//!
//! The program is both sides. It makes a thread for the server, so that the
//! `accept` and the `connect` are really happening at the same time in two
//! threads of one process -- which is what makes the blocking behaviour worth
//! anything: a server that only worked because the client had already connected
//! would pass a test where `accept` never had to wait.
//!
//! # Passing a descriptor
//!
//! The last exchange is the one that matters most for what comes after. The
//! client makes a pipe, sends the *reading end* to the server over the socket
//! with `SCM_RIGHTS`, and writes a word into the pipe; the server receives a
//! descriptor it never opened, reads the word out of it, and sends it back.
//!
//! That is exactly how a Wayland client gives a compositor a buffer to draw
//! from. Without it a client can talk to a server and cannot hand it anything.
//!
//! | 180 | `socketpair` |
//! | 181 | what went into one side did not come out of the other |
//! | 182 | `socket` |
//! | 183 | `bind` |
//! | 184 | `listen` |
//! | 185 | the server thread could not be started |
//! | 186 | `connect` |
//! | 187 | sending the request |
//! | 188 | reading the reply |
//! | 189 | the reply is not what the server was asked for |
//! | 190 | `pipe2` for the descriptor to pass |
//! | 191 | `sendmsg` with the descriptor on it |
//! | 192 | the server never reported what it read |
//! | 193 | the server read the wrong thing through the passed descriptor |
//! | 194 | the server never accepted the connection |
//! | 195 | `poll` did not report the listening socket as ready |

#![no_std]
#![no_main]

use nexus_guest::{call, exit_group, expect, say, syscall1, syscall2, syscall3, syscall4};

nexus_guest::guest_main!(run);

/// Where the server listens.
///
/// An abstract name -- a leading NUL, which Linux uses for a socket that is not
/// a file. Chosen so the test leaves nothing behind in the Linux root, and
/// because it exercises the half of `sockaddr_un` that is easy to get wrong.
const NAME: &[u8] = b"\0nexus-guest-test";

const AF_UNIX: u16 = 1;
const SOCK_STREAM: u64 = 1;

/// What the client asks and what the server answers.
const REQUEST: &[u8] = b"who is there";
const REPLY: &[u8] = b"a server, and it heard: who is there";
/// The word the client puts in the pipe it hands over.
const THROUGH_PIPE: &[u8] = b"handed over";

/// The flags a thread is made with. See `nexus-linux-example`'s threads fixture
/// for why each is required.
const THREAD_FLAGS: u64 = 0x100 | 0x200 | 0x400 | 0x800 | 0x1_0000;

/// `struct sockaddr_un`: a two-byte family and a hundred and eight of path.
#[repr(C)]
#[derive(Clone, Copy)]
struct SockAddrUn {
    family: u16,
    path: [u8; 108],
}

impl SockAddrUn {
    const fn empty() -> Self {
        Self {
            family: AF_UNIX,
            path: [0u8; 108],
        }
    }

    fn named(name: &[u8]) -> (Self, u64) {
        let mut address = Self::empty();
        let take = if name.len() > 108 { 108 } else { name.len() };
        let mut index = 0;
        while index < take {
            address.path[index] = name[index];
            index += 1;
        }
        (address, 2 + take as u64)
    }
}

/// `struct iovec`.
#[repr(C)]
#[derive(Clone, Copy)]
struct IoVec {
    base: u64,
    length: u64,
}

/// `struct msghdr`, as x86-64 lays it out: fifty-six bytes.
#[repr(C)]
struct MsgHdr {
    name: u64,
    name_length: u32,
    _pad: u32,
    vectors: u64,
    vector_count: u64,
    control: u64,
    control_length: u64,
    flags: i32,
    _pad2: i32,
}

/// Room for a `cmsghdr` and one descriptor, aligned as the ABI wants it.
#[repr(C, align(8))]
struct Control {
    bytes: [u8; 24],
}

/// Where the two threads leave things for each other.
///
/// A shared page, because that is what two threads of one process have. The
/// server writes what it read through the passed descriptor here, and the
/// client checks it.
#[repr(C)]
struct Shared {
    /// Set to one when the server has accepted and answered.
    served: u32,
    /// Set to one when the server has read the passed descriptor.
    read_through: u32,
    /// What it read there.
    seen: [u8; 32],
    seen_length: u64,
    /// The name the server listens at, where the server thread can reach it.
    address: SockAddrUn,
    address_length: u64,
    /// The listening descriptor, made by the first thread.
    listening: i32,
}

fn run() -> ! {
    // ---- a connected pair, with no name at all ---------------------------
    //
    // The simplest connection there is, and the one that says whether a stream
    // socket carries bytes at all before any of the naming is involved.
    let mut pair = [0i32; 2];
    expect(
        syscall4(
            call::SOCKETPAIR,
            1,
            SOCK_STREAM,
            0,
            pair.as_mut_ptr() as u64,
        ) == 0,
        180,
    );
    expect(
        syscall3(
            call::WRITE,
            pair[0] as u64,
            REQUEST.as_ptr() as u64,
            REQUEST.len() as u64,
        ) == REQUEST.len() as i64,
        181,
    );
    let mut back = [0u8; 64];
    let got = syscall3(
        call::READ,
        pair[1] as u64,
        back.as_mut_ptr() as u64,
        back.len() as u64,
    );
    expect(
        got == REQUEST.len() as i64 && &back[..REQUEST.len()] == REQUEST,
        181,
    );
    let _ = syscall1(call::CLOSE, pair[0] as u64);
    let _ = syscall1(call::CLOSE, pair[1] as u64);

    // ---- a server at a name ----------------------------------------------
    let Some(page) = nexus_guest::map_anonymous(4096) else {
        nexus_guest::fail(190)
    };
    // SAFETY: a page of anonymous memory this program owns, large enough for
    // the structure, and zeroed by the kernel.
    let shared = unsafe { &mut *(page as *mut Shared) };
    let (address, address_length) = SockAddrUn::named(NAME);
    shared.address = address;
    shared.address_length = address_length;

    let listening = syscall3(call::SOCKET, 1, SOCK_STREAM, 0);
    expect(listening >= 0, 182);
    shared.listening = listening as i32;
    expect(
        syscall3(
            call::BIND,
            listening as u64,
            core::ptr::addr_of!(shared.address) as u64,
            shared.address_length,
        ) == 0,
        183,
    );
    expect(syscall2(call::LISTEN, listening as u64, 4) == 0, 184);

    // The server runs in a thread of its own, so that the `accept` below really
    // is waiting when the `connect` happens.
    let Some(stack) = nexus_guest::map_anonymous(64 * 1024) else {
        nexus_guest::fail(185)
    };
    // SAFETY: the top of a region this program just mapped, aligned down to
    // sixteen as the ABI wants a stack pointer to be.
    let stack_top = ((stack as u64) + 64 * 1024) & !15;
    let made = clone_thread(stack_top, serve as *const () as u64, page as u64);
    expect(made > 0, 185);

    // ---- the client -------------------------------------------------------
    let client = syscall3(call::SOCKET, 1, SOCK_STREAM, 0);
    expect(client >= 0, 182);
    expect(
        syscall3(
            call::CONNECT,
            client as u64,
            core::ptr::addr_of!(shared.address) as u64,
            shared.address_length,
        ) == 0,
        186,
    );
    expect(
        syscall3(
            call::WRITE,
            client as u64,
            REQUEST.as_ptr() as u64,
            REQUEST.len() as u64,
        ) == REQUEST.len() as i64,
        187,
    );

    let mut reply = [0u8; 64];
    let got = syscall3(
        call::READ,
        client as u64,
        reply.as_mut_ptr() as u64,
        reply.len() as u64,
    );
    expect(got > 0, 188);
    expect(&reply[..got as usize] == REPLY, 189);

    // ---- handing a descriptor over ---------------------------------------
    //
    // A pipe, whose reading end goes to the server. This is how a Wayland
    // client gives a compositor a buffer, with the buffer replaced by something
    // a test can check.
    let mut ends = [0i32; 2];
    expect(syscall2(call::PIPE2, ends.as_mut_ptr() as u64, 0) == 0, 190);
    expect(
        syscall3(
            call::WRITE,
            ends[1] as u64,
            THROUGH_PIPE.as_ptr() as u64,
            THROUGH_PIPE.len() as u64,
        ) == THROUGH_PIPE.len() as i64,
        190,
    );

    let marker = b"here is a descriptor";
    let vector = IoVec {
        base: marker.as_ptr() as u64,
        length: marker.len() as u64,
    };
    let mut control = Control { bytes: [0u8; 24] };
    // `struct cmsghdr`: length, level, type, then the descriptors.
    control.bytes[0..8].copy_from_slice(&20u64.to_le_bytes());
    control.bytes[8..12].copy_from_slice(&1u32.to_le_bytes()); // SOL_SOCKET
    control.bytes[12..16].copy_from_slice(&1u32.to_le_bytes()); // SCM_RIGHTS
    control.bytes[16..20].copy_from_slice(&(ends[0] as u32).to_le_bytes());
    let message = MsgHdr {
        name: 0,
        name_length: 0,
        _pad: 0,
        vectors: core::ptr::addr_of!(vector) as u64,
        vector_count: 1,
        control: core::ptr::addr_of!(control) as u64,
        control_length: 20,
        flags: 0,
        _pad2: 0,
    };
    expect(
        syscall3(
            call::SENDMSG,
            client as u64,
            core::ptr::addr_of!(message) as u64,
            0,
        ) > 0,
        191,
    );

    // Wait for the server to say what it read there. A bounded wait: a server
    // that never answers is a failure with a number rather than a boot that
    // stops with nothing to say.
    let mut waited = 0;
    while read_once(&shared.read_through) == 0 && waited < 400 {
        let _ = syscall0_yield();
        waited += 1;
    }
    expect(read_once(&shared.read_through) == 1, 192);
    let length = shared.seen_length as usize;
    expect(
        length == THROUGH_PIPE.len() && &shared.seen[..length] == THROUGH_PIPE,
        193,
    );

    let _ = syscall1(call::CLOSE, client as u64);
    let _ = syscall1(call::CLOSE, ends[1] as u64);
    let _ = syscall1(call::CLOSE, listening as u64);

    say("guest: a server and a client met over a socket, and one handed the other a descriptor");
    exit_group(0)
}

/// The server thread.
///
/// Not called from Rust: `clone` enters it with the shared page in `rdi`.
///
/// # Safety
///
/// Entered once, by `clone`, with a stack of its own.
unsafe extern "C" fn serve(page: u64) -> ! {
    // SAFETY: the page the first thread mapped and filled in before the clone.
    let shared = unsafe { &mut *(page as *mut Shared) };

    // Somebody is waiting to be accepted -- or will be. `poll` on a listening
    // socket reports it readable, which is the convention every server relies
    // on, and checking it here is what says the readiness side works for a
    // listener and not only for a connection.
    let mut watch = [PollFd {
        descriptor: shared.listening,
        events: 1, // POLLIN
        revents: 0,
    }];
    let ready = syscall3(call::POLL, watch.as_mut_ptr() as u64, 1, 5000);
    if ready < 1 || watch[0].revents & 1 == 0 {
        nexus_guest::fail(195)
    }

    let served = syscall4(call::ACCEPT, shared.listening as u64, 0, 0, 0);
    if served < 0 {
        nexus_guest::fail(194)
    }

    let mut asked = [0u8; 64];
    let got = syscall3(
        call::READ,
        served as u64,
        asked.as_mut_ptr() as u64,
        asked.len() as u64,
    );
    if got <= 0 {
        nexus_guest::fail(194)
    }
    let _ = syscall3(
        call::WRITE,
        served as u64,
        REPLY.as_ptr() as u64,
        REPLY.len() as u64,
    );
    shared.served = 1;

    // And the descriptor the client is about to send.
    let mut body = [0u8; 64];
    let vector = IoVec {
        base: body.as_mut_ptr() as u64,
        length: body.len() as u64,
    };
    let mut control = Control { bytes: [0u8; 24] };
    let mut message = MsgHdr {
        name: 0,
        name_length: 0,
        _pad: 0,
        vectors: core::ptr::addr_of!(vector) as u64,
        vector_count: 1,
        control: core::ptr::addr_of_mut!(control) as u64,
        control_length: 24,
        flags: 0,
        _pad2: 0,
    };
    let got = syscall3(
        call::RECVMSG,
        served as u64,
        core::ptr::addr_of_mut!(message) as u64,
        0,
    );
    if got < 0 || message.control_length < 20 {
        nexus_guest::fail(192)
    }
    let mut four = [0u8; 4];
    four.copy_from_slice(&control.bytes[16..20]);
    let passed = u32::from_le_bytes(four);

    // A descriptor this thread never opened, reachable because the other side
    // sent it. Reading it is the whole claim.
    let read = syscall3(
        call::READ,
        u64::from(passed),
        shared.seen.as_mut_ptr() as u64,
        shared.seen.len() as u64,
    );
    if read > 0 {
        shared.seen_length = read as u64;
        shared.read_through = 1;
    }

    let _ = syscall1(call::CLOSE, u64::from(passed));
    let _ = syscall1(call::CLOSE, served as u64);
    // This thread, not the program: the client is still running.
    let _ = syscall1(call::EXIT, 0);
    loop {
        core::hint::spin_loop();
    }
}

/// `struct pollfd`.
#[repr(C)]
#[derive(Clone, Copy)]
struct PollFd {
    descriptor: i32,
    events: i16,
    revents: i16,
}

/// Make a thread that starts at `entry` with `argument` in `rdi`.
///
/// `clone` returns twice, and the child returns *into this function* with a
/// stack of its own -- so the child cannot simply fall out of it, because the
/// stack it would return onto is empty. It jumps to `entry` instead.
fn clone_thread(stack_top: u64, entry: u64, argument: u64) -> i64 {
    let out: i64;
    // SAFETY: `stack_top` is the top of a region this program mapped, `entry`
    // is a function in this image, and the child jumps to it without returning.
    unsafe {
        core::arch::asm!(
            "syscall",
            "test rax, rax",
            "jnz 2f",
            // The child. Its stack is the one the kernel was given, and `r12`
            // and `r13` are this program's registers, copied with everything
            // else -- which is the whole of what `clone` means.
            "mov rdi, r13",
            "call r12",
            // Unreachable: `serve` never returns.
            "ud2",
            "2:",
            inlateout("rax") call::CLONE as i64 => out,
            in("rdi") THREAD_FLAGS,
            in("rsi") stack_top,
            in("rdx") 0,
            in("r10") 0,
            in("r8") 0,
            in("r12") entry,
            in("r13") argument,
            lateout("rcx") _,
            lateout("r11") _,
            options(nostack),
        );
    }
    out
}

/// Give up the rest of this thread's slice.
fn syscall0_yield() -> i64 {
    nexus_guest::syscall0(call::SCHED_YIELD)
}

/// Read a word the other thread writes.
///
/// Through a volatile read, because the compiler is entitled to assume nothing
/// else changes this program's memory and would otherwise hoist the load out of
/// the loop that is waiting for it to change.
fn read_once(at: &u32) -> u32 {
    // SAFETY: an aligned `u32` in memory this program owns.
    unsafe { core::ptr::read_volatile(at) }
}
