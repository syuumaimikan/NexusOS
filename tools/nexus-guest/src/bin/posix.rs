//! Descriptors: duplicating them, joining two with a pipe, waiting on them.
//!
//! These three are not interesting on their own and are underneath almost
//! everything that is. A shell redirects with `dup2`; a library talks to a
//! subprocess through a `pipe`; every event loop ever written blocks in `poll`
//! or `epoll_wait`. A translation layer without them runs programs that do one
//! thing and stop.
//!
//! | 100 | `dup` |
//! | 101 | `dup` gave back a descriptor that is already in use |
//! | 102 | writing through the duplicate did not reach the same place |
//! | 103 | `dup2` did not return the descriptor it was told to use |
//! | 104 | `close` of a duplicate |
//! | 105 | `pipe2` |
//! | 106 | the two descriptors a pipe returned are the same |
//! | 107 | writing into the pipe |
//! | 108 | reading out of it |
//! | 109 | what came out is not what went in |
//! | 110 | a second read did not block or report emptiness correctly |
//! | 111 | `poll` on an empty pipe said it was readable |
//! | 112 | `poll` on a pipe with something in it said it was not |
//! | 113 | `poll` did not report the write end as writable |
//! | 114 | closing the write end did not show up as a hangup |
//! | 115 | `epoll_create1` |
//! | 116 | `epoll_ctl` |
//! | 117 | `epoll_wait` did not report the descriptor that is ready |
//! | 118 | `epoll_wait` reported the wrong descriptor |
//! | 119 | reading the byte `epoll` promised |

#![no_std]
#![no_main]

use nexus_guest::{call, exit_group, expect, say, syscall1, syscall2, syscall3, syscall4};

nexus_guest::guest_main!(run);

/// `struct pollfd { int fd; short events; short revents; }` -- eight bytes.
#[repr(C)]
#[derive(Clone, Copy)]
struct PollFd {
    descriptor: i32,
    events: i16,
    revents: i16,
}

/// `struct epoll_event { uint32_t events; epoll_data_t data; }`, packed on
/// x86-64 -- twelve bytes, and the packing is part of the ABI rather than
/// something a compiler is free to choose.
#[repr(C, packed)]
#[derive(Clone, Copy)]
struct EpollEvent {
    events: u32,
    data: u64,
}

const POLLIN: i16 = 0x001;
const POLLOUT: i16 = 0x004;
const POLLHUP: i16 = 0x010;

const EPOLLIN: u32 = 0x001;
const EPOLL_CTL_ADD: u64 = 1;

fn run() -> ! {
    // ---- dup ------------------------------------------------------------
    //
    // A second descriptor for the same thing. Standard output is the one every
    // program has, so it is the one to duplicate -- and what makes this a test
    // rather than a call is that the duplicate has to *work*: a `dup` that
    // returned a plausible number and no descriptor would pass a check on the
    // number alone.
    let copy = syscall1(call::DUP, 1);
    expect(copy >= 0, 100);
    expect(copy != 0 && copy != 1 && copy != 2, 101);
    let wrote = nexus_guest::write(copy as u64, b"guest: wrote through a duplicate of stdout\n");
    expect(wrote > 0, 102);

    // `dup2` is the same operation with the caller choosing the number, which
    // is the whole reason a shell can redirect: it puts the descriptor it wants
    // at the number the program is going to use.
    let chosen = syscall2(call::DUP2, 1, 37);
    expect(chosen == 37, 103);
    let wrote = nexus_guest::write(37, b"guest: wrote through descriptor 37\n");
    expect(wrote > 0, 102);
    expect(syscall1(call::CLOSE, 37) == 0, 104);
    expect(syscall1(call::CLOSE, copy as u64) == 0, 104);

    // ---- a pipe ---------------------------------------------------------
    let mut ends = [0i32; 2];
    expect(syscall2(call::PIPE2, ends.as_mut_ptr() as u64, 0) == 0, 105);
    let (reading, writing) = (ends[0] as u64, ends[1] as u64);
    expect(reading != writing, 106);

    // Nothing in it yet, so `poll` must say so. This is the check that fails
    // for a `poll` that reports everything as ready -- which is a `poll` that
    // turns every event loop into a spin.
    let mut watch = [PollFd {
        descriptor: reading as i32,
        events: POLLIN,
        revents: 0,
    }];
    let ready = syscall3(call::POLL, watch.as_mut_ptr() as u64, 1, 0);
    expect(ready == 0 && watch[0].revents & POLLIN == 0, 111);

    // The write end, though, is ready: there is room in the pipe.
    let mut watch_write = [PollFd {
        descriptor: writing as i32,
        events: POLLOUT,
        revents: 0,
    }];
    let ready = syscall3(call::POLL, watch_write.as_mut_ptr() as u64, 1, 0);
    expect(ready == 1 && watch_write[0].revents & POLLOUT != 0, 113);

    const SENT: &[u8] = b"through a pipe";
    expect(
        syscall3(
            call::WRITE,
            writing,
            SENT.as_ptr() as u64,
            SENT.len() as u64,
        ) == SENT.len() as i64,
        107,
    );

    // And now it is readable.
    watch[0].revents = 0;
    let ready = syscall3(call::POLL, watch.as_mut_ptr() as u64, 1, 0);
    expect(ready == 1 && watch[0].revents & POLLIN != 0, 112);

    let mut received = [0u8; 32];
    let got = syscall3(
        call::READ,
        reading,
        received.as_mut_ptr() as u64,
        received.len() as u64,
    );
    expect(got == SENT.len() as i64, 108);
    expect(&received[..SENT.len()] == SENT, 109);

    // ---- epoll ----------------------------------------------------------
    //
    // The same question as `poll` asked a different way, and the way every
    // program that watches more than a handful of descriptors asks it.
    let set = syscall1(call::EPOLL_CREATE1, 0);
    expect(set >= 0, 115);
    let mut interest = EpollEvent {
        events: EPOLLIN,
        data: 0xABCD,
    };
    expect(
        syscall4(
            call::EPOLL_CTL,
            set as u64,
            EPOLL_CTL_ADD,
            reading,
            core::ptr::addr_of_mut!(interest) as u64,
        ) == 0,
        116,
    );

    // Nothing in the pipe, so nothing is ready. A zero here is the answer, not
    // a failure -- and an `epoll_wait` that returned one would be the same bug
    // as a `poll` that reports everything ready.
    let mut reported = [EpollEvent { events: 0, data: 0 }; 4];
    let ready = syscall4(
        call::EPOLL_WAIT,
        set as u64,
        reported.as_mut_ptr() as u64,
        reported.len() as u64,
        0,
    );
    expect(ready == 0, 117);

    const AGAIN: &[u8] = b"!";
    expect(
        syscall3(call::WRITE, writing, AGAIN.as_ptr() as u64, 1) == 1,
        107,
    );
    let ready = syscall4(
        call::EPOLL_WAIT,
        set as u64,
        reported.as_mut_ptr() as u64,
        reported.len() as u64,
        1000,
    );
    expect(ready == 1, 117);
    // The cookie the program gave `epoll_ctl`, handed back unchanged. That is
    // the whole point of it: a program with fifty descriptors uses it to find
    // out which one it was without a search.
    let data = reported[0].data;
    expect(data == 0xABCD, 118);

    let mut one = [0u8; 1];
    expect(
        syscall3(call::READ, reading, one.as_mut_ptr() as u64, 1) == 1 && one[0] == b'!',
        119,
    );

    // ---- a hangup -------------------------------------------------------
    //
    // The far end closing is the event a reader has to notice: without it, a
    // program waiting for more input waits for ever on a writer that has gone.
    expect(syscall1(call::CLOSE, writing) == 0, 104);
    watch[0].revents = 0;
    let ready = syscall3(call::POLL, watch.as_mut_ptr() as u64, 1, 100);
    expect(
        ready >= 1 && watch[0].revents & (POLLIN | POLLHUP) != 0,
        114,
    );
    // And a read of a pipe nobody is writing to is end of file, which is zero.
    let got = syscall3(call::READ, reading, received.as_mut_ptr() as u64, 8);
    expect(got == 0, 110);

    let _ = syscall1(call::CLOSE, reading);
    let _ = syscall1(call::CLOSE, set as u64);

    say("guest: descriptors, a pipe, poll and epoll all behaved");
    exit_group(0)
}
