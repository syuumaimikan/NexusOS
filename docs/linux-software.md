# Running software built for Linux

```
[linux] spawned wrote: a linux program wrote a file and read it back
[linux] process p18 "spawned" exited with status 0 through the Linux boundary
[user] init: a Linux program that wrote a file and read it back ran and exited
       through the translation
```

That program is a static x86-64 ELF, and it is the first of ten that run at
every boot. The others ask for memory; use descriptors, a pipe and `poll`;
become another program with `execve`; raise and handle a signal; run a server
and a client over a Unix socket and pass a descriptor between them; make a
thread and wait on a futex for it; start through an interpreter; and draw a
window.

The first one is a static x86-64 ELF. It creates a file, writes to it, closes
it, opens it again, calls `fstat`, reads it back and compares the bytes, seeks
to the end, asks where it is, asks for randomness, lists a directory, and looks
for `AT_RANDOM` in its own auxiliary vector. Every step exits with its own
number when what came back is wrong, so exiting zero is a claim about all of
them.

And the file it wrote is in the host's copy of the disk, at `0x14017000`, with
its directory entry at `0x14016008`. That check is outside the machine on
purpose: a translation layer that accepted every byte and kept none passes
every check inside it.

## The rule this is built on

Linux compatibility is a layer *above* the Nexus system-call interface, never a
fork of it. Nothing below `compat/` knows Linux exists, and there is no
operation a translated program can perform that a Nexus program could not. When
a Linux call needs something Nexus does not have, the answer is to add it to
Nexus — for everybody — and then translate.

Applied to files, that has one sharp consequence: **a Linux file descriptor is
a Nexus handle.** `openat` puts the node in the process's ordinary handle table
and returns the handle number. `read(fd)` looks it up through the same rights
check a Nexus program's `read(handle)` uses. Opening for reading yields a handle
without `WRITE`, so a descriptor opened read-only cannot be written through —
not because this layer remembers a flag, but because the handle does not carry
the right.

It also forces a decision that would otherwise be invisible. Handle numbers
start at one, and one is standard output. A Linux process whose first `open`
returned handle 1 would have opened a file that every `printf` then wrote into,
and every test would still pass, because writing to the file and writing to the
log would be the same call. So a Linux process starts its handles at three, and
the test program checks the descriptor against three before writing anything
through it.

## Where `/` is

`linux/` in the machine's own store, made the first time a program asks for it.
A translated program's `/etc/hostname` is this machine's `linux/etc/hostname`.
`..` is refused rather than walked, which is the same rule `node_open` enforces
for every Nexus program: a directory handle is the authority to reach what is
under it, and a path that could climb out would make that authority mean
nothing.

The working directory is `/` and does not move. `chdir` is not translated, so
`getcwd` answering `/` is the truth and not a placeholder.

## The bug that cost the afternoon

The test failed at step eleven — `openat` refused — with no log line from inside
`openat` at all, because it had returned before reaching one.

`dirfd` is an `int`. A compiler loads `AT_FDCWD` with a thirty-two bit `mov`,
which zero-extends, so the register holds `0x00000000ffffff9c` and not the
sign-extended `0xffffffffffffff9c`. Comparing the whole register against −100 as
a long rejects every `openat` a real program makes. The fix is one cast and it
is in `is_cwd`, with the reason written beside it.

## The auxiliary vector

It used to be `AT_NULL` straight away. That is a legal thing for a kernel to
put there and not a thing any real program survives.

`AT_PHDR` is how a C library finds its own `PT_TLS` to set up thread-local
storage. `AT_RANDOM` points at sixteen bytes a program is entitled to, and is
where the stack guard comes from — so a binary built with `-fstack-protector`,
which is every distribution's default, reads that pointer before it reaches
`main` and dies on the spot if it is null. Both are now filled in, along with
`AT_PHENT`, `AT_PHNUM`, `AT_PAGESZ`, `AT_ENTRY`, the four identity entries,
`AT_SECURE` and `AT_CLKTCK`.

`AT_PHDR` is the address the table can be *read from* once the image is in
place, worked out by asking which loadable segment covers the file offset —
not `virt_base + e_phoff`, which would be a pointer into the program's own code
for any image whose first segment does not start at offset zero. When nothing
maps the table the answer is zero, which is honest; there are tests for both.

## Where the programs come from

Two places, and the difference matters.

The first four are **hand-assembled**, byte by byte, with the field each byte
belongs to named beside it — see `tools/nexus-linux-example`. That was once the
only way to have a Linux binary at all: building one needs a Linux toolchain and
the machine this repository is developed on does not have one.

It turns out not to need one. `rustc` can target `x86_64-unknown-linux-gnu` with
`core` rebuilt from source, and `rust-lld` can link a static `ET_EXEC` with no C
runtime — so a real Linux executable comes out of the toolchain this repository
already uses, with no Linux toolchain and no network. The rest of the programs
are in `tools/nexus-guest`, written in Rust, `#![no_std]`, with their own
`_start`.

The hand-assembled ones stay. They are the ones that prove the *loader* works on
an image nobody could have shaped to suit, and one of them is a dynamic linker,
which is not a thing a compiler will emit for you.

### The two things a compiler found that an assembler had not

**The machine had never enabled SSE.** This kernel and its own programs are
built `+soft-float` with every SSE feature off, which is the ordinary choice for
a kernel and a reasonable one for its programs. The x86-64 System V ABI, though,
*is* SSE: `double` is passed in `xmm0`, and a compiler zeroes sixteen bytes of
stack with `xorps` because that is the cheapest way. The first compiled Linux
program to run here got as far as its third system call and then:

```
EXCEPTION 6: invalid opcode
  taken from : user mode
  rip        : 0x00000000004014ca      ; xorps %xmm0, %xmm0
```

Not a subtle failure, and not one a compatibility layer can translate around:
there is no system call involved. `arch::fpu` turns the unit on per processor
and gives every thread its own five hundred and twelve bytes of it, saved and
restored on the switch — because enabling it without that would be two threads
sharing sixteen vector registers, which is a failure with no error and no
pattern.

**A process is not entered the way a function is called.** `_start` is jumped
to, with a sixteen-byte aligned stack pointer; a function reached by `call` is
entered with the return address already pushed, so its stack pointer is eight
past a boundary — and a compiler lays out every frame on that assumption. An
`extern "C" fn _start` is therefore a function whose every stack slot is eight
bytes out, and nothing notices until the first sixteen-byte aligned access,
which for a compiler zeroing a local is `movaps` — an instruction that *faults*
on a misaligned address. What it looked like was a general protection fault
three calls in, with nothing in the program's source anywhere near it.

That is why the `_start` in every real C library is written in assembly, and why
the one in `tools/nexus-guest` is too.

## Programs that need a loader

A program with a `PT_INTERP` is *dynamically linked*: none of its calls into a
shared library have been resolved, so entering it directly reaches a symbol
table nobody filled in. Almost every real Linux program is one of these.

Starting one is not loading a file. It is loading two images into one address
space, entering the second, and telling it four things it cannot work out for
itself:

```
[linux] spawned needs /lib/ld-nexus-x86-64.so.1: 794 bytes loaded at 0x7f0000000000
[user] process p25 "spawned" loaded from /usr/bin/dyn: 441 bytes,
       entry 0x7f0000000078, 1 segments at 0x555555554000
[linux] spawned wrote: a dynamically linked linux program ran through its interpreter
```

Two addresses, in one process. `0x5555_5555_4000` is the program, which is
`ET_DYN` and names no address of its own; `0x7f00_0000_0000` is the interpreter.
The entry point is the interpreter's, because the kernel enters *it* — the
program's own entry is in the auxiliary vector as `AT_ENTRY`, and reaching it is
the last thing the interpreter does.

The four numbers are `AT_BASE` (where the interpreter went), `AT_ENTRY` (where
the program starts), and `AT_PHDR`/`AT_PHNUM` (the *program's* headers, never
the interpreter's, which is the distinction that makes the whole thing work).
Every one of them can be wrong in a way that produces no message: a wrong
`AT_BASE` is a loader relocating itself against the wrong address, and what that
looks like is a fault at a nonsense address with no symbols loaded.

### Where the interpreter comes from

The Linux root, by the absolute path the program names — so something has to put
it there. The build stages files on the EFI partition, which is a different
filesystem that a translated program cannot see and should not be able to: it
holds this system's own kernel and programs.

So the build also writes `BIN/LINUX.LST`, a list of `source destination` pairs,
and the kernel installs from it on the first boot. It is the smallest form of
what a package installer will eventually do, and it is deliberately not a
special case for one file.

### What this does not establish

The interpreter it runs against is not `ld-linux-x86-64.so.2`. glibc's loader is
part of glibc, glibc needs a Linux toolchain to build, and the machine this
repository is developed on does not have one — the same reason every fixture
here is hand-assembled.

What is verified is the kernel's half of the contract, by an interpreter that
checks each of those numbers against what the processor says and refuses to
carry on when one is wrong. Whether glibc's loader works against it is untested.
See `tools/nexus-linux-example/src/dynamic.rs` and
[Steam and graphics compatibility](steam-graphics.md).

## Threads

```
[linux] spawned wrote: two linux threads met at a futex
[linux] a thread of process p24 "spawned" exited with status 0; 1 left
[linux] process p24 "spawned" exited with status 0 through the Linux boundary,
        asking 1 other thread(s) to stop
```

A Linux thread here is a Nexus thread in the same Nexus process. That is not an
analogy: `clone` puts another thread in the process the caller already belongs
to, holding the same address space and the same handle table, so two Linux
threads see the same memory and the same descriptors because they *are* the same
process.

Three things had to be true for that to work, and two of them were not.

**A new thread begins with a copy of its parent's registers.** All of them, bar
`rax` — which is zeroed, and is the only thing that tells the child apart from
the parent — and the stack pointer, which the caller nominates. Every C
library's thread entry sequence reads the function it is to call out of a
register it set *before* the call: musl and glibc both use `r9`. A kernel that
restored a stack pointer and an address would start a thread that jumps to
whatever was left there. The system-call entry stub therefore saves all sixteen
registers now rather than three.

**Thread-local storage is per thread.** `arch_prctl(ARCH_SET_FS)` wrote the
register and the value stayed on whichever processor it was written on. That was
invisible for exactly as long as one process had one thread — and would have
been two threads sharing one `errno` the moment it did not, with a Nexus thread
scheduled in between running with a foreign program's thread pointer still
installed. `FS_BASE` is part of the context switch now.

**A futex has to check and sleep without a gap.** `FUTEX_WAIT` is "sleep unless
the word has already changed", and a waiter that checked and then slept would
miss a wake that landed in between — and would sleep for ever holding a lock
nobody can take.

`exit` ends a thread and `exit_group` ends the program; they were one call while
a process had one thread and are not one call any more. `fork` — a `clone`
without `CLONE_VM` — is refused with `ENOSYS`, because copying an address space
means copy-on-write and a fault handler that knows about it, and making a thread
instead would be two "processes" sharing one heap.

## A window

```
[linux] spawned opened /dev/nexus/display: a 960x582 window on descriptor 4
[user] compositor: started a program built for Linux, and gave it a surface
```

A Linux program draws through X11 or Wayland: it connects to a server over a
Unix socket, is handed a shared buffer, writes pixels into it, and says which
part changed. This machine's compositor works the same way — surface, pixels,
damage, acknowledgement — over its own channels, and `/dev/nexus/display` is the
bridge:

```
fd     = openat(AT_FDCWD, "/dev/nexus/display", O_RDWR)
         ioctl(fd, NEXUS_DISPLAY_INFO, &info)     // width, height, stride, format
pixels = mmap(NULL, info.stride * info.height, PROT_READ|PROT_WRITE,
              MAP_SHARED, fd, 0)
         ioctl(fd, NEXUS_DISPLAY_PRESENT, &rect)  // and wait to be shown
         ioctl(fd, NEXUS_DISPLAY_EVENT, &event)   // a key, a resize, or nothing
```

The descriptor *is* the channel to the compositor, duplicated — which is the
rule this whole layer follows, applied to drawing: a Linux file descriptor is a
Nexus handle, and what can be done through it is what the handle carries. A
program started by anything other than the compositor has no such channel, and
opening the device tells it so rather than handing it the screen.

`MAP_SHARED` is required here and refused everywhere else. The window buffer
really is shared with the compositor; a file mapping is not, and a promise that
cannot be kept is refused rather than approximated.

**It is not Wayland.** No `wl_display`, no object registry, no `wl_shm`, no
`xdg_surface`, no Unix socket, no descriptor passing. A Wayland client would
fail at its first `connect`. What this is is the layer *underneath* all of that
— and that is no longer hypothetical: `tools/nexus-guest/src/bin/wlserver.rs` is
a program in user space speaking the real wire protocol, and it draws through
exactly this device. The keys it forwards as `wl_keyboard.key` come out of this
device's `EVENT` request.

## Descriptors, pipes and waiting

```
[linux] spawned wrote: guest: descriptors, a pipe, poll and epoll all behaved
```

A shell redirects with `dup2`; a library talks to a subprocess through a `pipe`;
every event loop ever written blocks in `poll` or `epoll_wait`. A translation
layer without them runs programs that do one thing at a time.

A **pipe** is a new object of this system rather than a Linux one. A channel
here carries *messages* — what comes out of one read is exactly what went into
one write — and that boundary is a feature. A pipe has none: three writes of ten
bytes are thirty bytes, and a reader asking for seven gets seven. Neither can be
built out of the other without losing something, so `crate::pipe` is its own
object with its own Nexus system call, and `pipe2` is a translation of it.

A **duplicate of standard output** forced a second one. A descriptor here is a
handle, so a second descriptor for the log has to be a second handle to
something — and there was nothing for it to be a handle to, because every
program could already reach the log ambiently. `Object::Console` is that
something.

`poll` and `epoll` are **level-triggered**, which is the same decision the
wait-set layer made and for the same reason: an edge is a fact about a moment,
and one delivered while nobody was waiting is lost. `EPOLLET` is refused rather
than quietly given the other behaviour, because the difference decides whether a
program's loop terminates.

## Sockets

```
[linux] spawned is listening at \0nexus-guest-test
[linux] spawned wrote: guest: a server and a client met over a socket,
        and one handed the other a descriptor
```

A Unix domain socket is how two programs on one machine talk when neither
started the other: a server puts a name somewhere and serves whoever arrives.
Every channel in this system until now existed because somebody with both ends
handed one over — a parent to a child, the compositor to a client. That is the
right default and it is not how a desktop protocol works.

A connection is two pipes and a queue of handles. `SCM_RIGHTS` — a descriptor
sent *across* a connection — is the handle transfer this system already has, so
a descriptor that arrives came with the rights it was sent with and no more.

One thing about it is not faithful and is written down rather than discovered:
on Linux a set of descriptors is attached to a particular byte in the stream,
and here they are a separate queue. Every protocol the author knows of sends its
descriptors with a message the receiver reads whole, so the difference does not
arise — but a receiver that arrives *before* the sender must look for handles
again after its read, and forgetting that was a bug that delivered a message
with the descriptor still in the queue.

## Signals

```
[linux] spawned wrote: guest: a signal was raised, handled, blocked,
        unblocked and ignored
```

A signal is the one thing in this interface that runs a program's code at a
moment the program did not choose. `rt_sigaction` used to be refused, and that
was right while there was no delivery: accepting it would have promised a
handler that never runs.

The handler is entered the way Linux enters one, because there is no other way
that leaves the program able to continue: the whole register state goes onto the
program's own stack *below the red zone*, a return address is pushed that points
at a few bytes of code the kernel put in the program's address space, and `rip`
becomes the handler. `rt_sigreturn` reads the frame back and returns to where
the program was — including the value its interrupted call was going to give it.

The red zone is the part that is easy to miss: a hundred and twenty-eight bytes
below `rsp` that a leaf function may use without reserving. A signal frame
written over it corrupts the locals of whatever was running, which is why the
test checks an array that was live across the signal.

Delivery happens on the way out of a system call and nowhere else. A program
that spins without making one cannot be signalled here, which Linux does not
have and which is a real difference.

## Becoming another program

```
[linux] process p22 "spawned" became /usr/bin/execed: entry 0x4018c0, 3 argument(s)
[linux] spawned wrote: guest: a program became another one, with its arguments
        and its descriptors
```

`execve` with no `fork` in front of it, which is what a bootstrapper is: a
program that works out what to run and then *is* it. The process survives — same
identifier, same handles — and its memory does not.

Descriptors surviving is what makes it useful: a launcher hands the program it
starts a connection it had already made. `O_CLOEXEC` is meant to say otherwise
and is not honoured, because nothing records it yet. A program with more than
one thread is refused rather than raced: killing the others means tearing the
address space down underneath whatever has not noticed yet, and the failure mode
of getting that slightly wrong is the worst one this system has.

## Thirty-two bit

```
[linux32] /usr/bin/thirty-two: esp 0xbfffdf70, entry 0x80492d7, 3 environment
[linux] spawned wrote: guest32: a thirty-two bit linux program is running
[linux] spawned wrote: guest32: mmap2, getpid and a 32-bit auxiliary vector all behaved
```

An i386 program is not a smaller x86-64 one. Four things differ, and each of
them is a thing a kernel has to do separately:

| | x86-64 | i386 |
|---|---|---|
| the image | `ELFCLASS64`, 64-byte header, 56-byte program headers | `ELFCLASS32`, 52 and 32 — and `p_flags` in a different position |
| the way in | the `syscall` instruction | `int 0x80`, a software interrupt |
| `write` | call number 1 | call number 4 |
| arguments | `rdi`, `rsi`, `rdx`, `r10`, `r8`, `r9` | `ebx`, `ecx`, `edx`, `esi`, `edi`, `ebp` |

The processor stays in long mode. What changes is the code segment: a
descriptor with `L` clear and `D` set makes it decode thirty-two bit
instructions and truncate the stack pointer. That descriptor was already in the
table — `sysret` requires the user segments to be laid out with the thirty-two
bit code segment first, so it has been there since the system-call boundary was
written and nothing had ever been entered through it.

`int 0x80`'s gate is the only one in the table at privilege level three. Every
other vector is raised by the processor or by a device, and a program that could
invoke one of those with `int` could fabricate a page fault or a timer tick.

The memory layout is its own, because it has to fit: an i386 executable is
linked at `0x08048000`, its stack goes just under three gigabytes where Linux
puts one, and `mmap2` hands addresses out from one gigabyte. A mapping above
four gigabytes would come back in `eax` *truncated* — which for the sixty-four
bit region is zero, so a program checking its mapping for failure would find it
had failed while the mapping was made.

What is translated is what does not change shape with the width: a buffer and a
count are a buffer and a count. Anything that passes a structure is refused by
name, because `struct stat64` is not `struct stat`, an `iovec` is eight bytes
rather than sixteen, and `set_thread_area` wants a local descriptor table this
system does not have. And there is no thirty-two bit interpreter, so an `ET_DYN`
i386 image is refused rather than entered unrelocated.

## What is translated

| `openat`, `open` | with `O_CREAT`, `O_TRUNC`, `O_APPEND`, `O_EXCL`, `O_DIRECTORY` |
| `read`, `write`, `close`, `lseek` | a position per descriptor, dropped when the process ends |
| `pread64` | reads at an explicit nonnegative offset without changing that position |
| `fstat`, `stat`, `newfstatat` | the 144-byte x86-64 `struct stat` |
| `getdents64` | as many whole entries as fit, the rest next call |
| `mkdirat`, `mkdir`, `unlinkat`, `unlink`, `rmdir` | |
| `access`, `faccessat` | existence, which is the only question this can answer |
| `getcwd`, `getrandom` | |
| `mmap`, `munmap`, `mprotect`, `arch_prctl` | as before |
| `write`, `writev` to 1 and 2 | to the boot log, as before |
| `uname`, `clock_gettime`, `getpid`, `gettid`, the identity calls | as before |
| `mmap` | anonymous or file-backed, private, `MAP_FIXED` and `MAP_FIXED_NOREPLACE`, `PROT_EXEC`, at an offset |
| `mprotect` | really changes the page tables; the contents survive it |
| `clone` | `CLONE_VM\|THREAD\|SIGHAND\|FILES`, with `SETTLS`, `PARENT_SETTID`, `CHILD_SETTID`, `CHILD_CLEARTID` |
| `futex` | `WAIT` and `WAKE`, private, with and without a timeout |
| `set_robust_list`, `get_robust_list` | recorded; nothing walks the list at thread exit |
| `sched_yield`, `sched_getaffinity`, `getcpu` | |
| `exit` | this thread; `exit_group` is the program |
| `ioctl` on `/dev/nexus/display` | a window: size, present, event |
| `dup`, `dup2`, `dup3` | including a duplicate of the log |
| `pipe`, `pipe2` | a byte stream with two ends |
| `poll`, `ppoll` | level-triggered, blocking on a wait set |
| `epoll_create`, `epoll_create1`, `epoll_ctl`, `epoll_wait`, `epoll_pwait` | `EPOLLET` refused |
| `socket`, `socketpair`, `bind`, `listen`, `accept`, `accept4`, `connect` | `AF_UNIX` streams only |
| `send`, `recv`, `sendto`, `recvfrom`, `sendmsg`, `recvmsg` | `SCM_RIGHTS` carries handles |
| `rt_sigaction`, `rt_sigprocmask`, `rt_sigreturn`, `kill`, `tgkill` | a handler really runs |
| `execve` | one thread only; descriptors survive |
| `memfd_create`, `ftruncate` | memory with a descriptor on it |
| `mmap` of a memory descriptor, `MAP_SHARED` | how a frame is passed between programs |

Fields the machine has no answer for are zero rather than invented. There are
no timestamps in a `struct stat` here, because a fabricated modification time
makes `make` rebuild nothing.

## What is not

`brk` is refused with `ENOMEM`, deliberately: every libc worth running falls
back to `mmap`, and musl does not use `brk` at all.

No `fork`. No signals raised by the *kernel*: a fault still ends the process,
there is no `SIGPIPE` on a write to a pipe nobody reads, and no `SIGCHLD`. No
`O_CLOEXEC`, no filesystem links or permissions, no `AF_INET` sockets, no
datagram sockets, no `shutdown` of one direction, and no futex on memory shared
between processes.

Nothing thirty-two bit that passes a structure, and no thirty-two bit dynamic
linking. No AVX: that needs `XCR0` and `xsave`, and a program built
with `-mavx2` takes an invalid opcode. No page that is writable and executable
at once, which means no just-in-time compiler.

Each of those is real work and none of them is hidden behind a stub that returns
success.

## Steam

It was asked for, and it will not run. This is worth writing down once, in
order, with what has changed marked:

- **glibc.** Steam is dynamically linked against it. That needs `PT_INTERP`
  honoured, `ld-linux-x86-64.so.2` present, and then several hundred calls
  glibc's startup touches. *The kernel now honours `PT_INTERP`*, and a great
  many of those calls are now translated -- but no part of glibc is on this
  machine, and building it needs a Linux toolchain this repository does not
  have. Until one is here, "a dynamically linked program starts" means the
  program in this repository and not one from a distribution.
- **32-bit.** The Steam bootstrap is i386. *A static i386 program compiled here
  now runs*: compatibility mode, `int 0x80`, the i386 call table and a
  thirty-two bit auxiliary vector all work. What is missing is the calls that
  pass structures, thread-local storage through a local descriptor table, and
  thirty-two bit libraries — and the bootstrap is dynamically linked, so it
  needs a thirty-two bit interpreter as well.
- **X11 or Wayland.** Steam draws through one of them. *A Wayland client and a
  Wayland compositor, both built for Linux, now negotiate a window through
  `xdg-shell`, draw it, resize it, put a key into it and close it* -- with
  `wl_shm`, `SCM_RIGHTS` and shared memory underneath, `wl_seat` for the
  keyboard, `wl_output` for the screen, frame callbacks and buffer damage. That
  is the set a real client needs before it will draw anything. What is missing
  is that it has never been run against `libwayland` itself; an XKB keymap; a
  pointer; subsurfaces, popups and regions; more than one client at a time; and
  protocol errors. There is still no X server.
- **OpenGL and Vulkan.** Steam's own interface is GPU-composited. There is no
  Vulkan or OpenGL implementation here, and nothing in this direction has been
  started. The virtio-gpu driver provides 2D scanout; the native CPU triangle
  renderer does not expose either API.
- **CEF.** The store is an embedded Chromium. That is tens of millions of lines
  of C++ expecting a full POSIX system, processes, and a GPU process. It also
  needs pages that are writable and executable, which this system does not make
  for anybody.
- **Networking, audio, DRM.** PulseAudio or ALSA, and a licensing handshake with
  Valve's servers. *Threads, futexes, signals and Unix sockets exist now*;
  `AF_INET` sockets do not, and there is no audio interface a Linux program can
  reach.

Any one of the unfinished ones is larger than this whole operating system.
Saying "Steam works" after building a stub that draws a window would be the kind
of claim this project exists not to make — and the window in
`build/linux-window.png` is a Linux program drawing two coloured bands, which is
exactly and only what it looks like.

What is real is the direction: a static Linux binary that uses files, memory and
the standard descriptors runs here today, unmodified, and the boundary grows by
reading the `[linux] call N is not translated yet` lines that real programs
produce.

The implementation plan and current verification boundary are tracked in
[Steam and graphics compatibility](steam-graphics.md). `scripts/test-linux.ps1`
runs the ten programs above at boot and maps every exit status back to the step
that failed; `scripts/test-linux-window.ps1` opens a window for the drawing one
and `scripts/test-wayland.ps1` starts a Wayland compositor and a client for it.
Both of the last two look for the program's own colours in a screendump taken
from outside the machine -- and the Wayland one measures how *wide* they are,
because the frame drawn before the resize and the frame drawn after it are the
same colours at different sizes.
