# Running software built for Linux

```
[linux] spawned wrote: a linux program wrote a file and read it back
[linux] process p18 "spawned" exited with status 0 through the Linux boundary
[user] init: a Linux program that wrote a file and read it back ran and exited
       through the translation
```

That program is a static x86-64 ELF. It creates a file, writes to it, closes
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

## What is translated

| `openat`, `open` | with `O_CREAT`, `O_TRUNC`, `O_APPEND`, `O_EXCL`, `O_DIRECTORY` |
| `read`, `write`, `close`, `lseek` | a position per descriptor, dropped when the process ends |
| `fstat`, `stat`, `newfstatat` | the 144-byte x86-64 `struct stat` |
| `getdents64` | as many whole entries as fit, the rest next call |
| `mkdirat`, `mkdir`, `unlinkat`, `unlink`, `rmdir` | |
| `access`, `faccessat` | existence, which is the only question this can answer |
| `getcwd`, `getrandom` | |
| `mmap`, `munmap`, `mprotect`, `arch_prctl` | as before |
| `write`, `writev` to 1 and 2 | to the boot log, as before |
| `uname`, `clock_gettime`, `getpid`, `gettid`, the identity calls | as before |

Fields the machine has no answer for are zero rather than invented. There are
no timestamps in a `struct stat` here, because a fabricated modification time
makes `make` rebuild nothing.

## What is not, and will not be soon

`brk` is refused with `ENOMEM`, deliberately: every libc worth running falls
back to `mmap`, and musl does not use `brk` at all. `rt_sigaction` is refused
rather than accepted, because accepting would promise to deliver a signal that
never arrives and a program that installed a fault handler and then took a
fault would sit in a loop instead of dying with a message.

No `fork`, no `execve`, no threads, no futexes, no sockets, no `poll`, no
dynamic linking. Each of those is real work and none of them is hidden behind a
stub that returns success.

## Steam

It was asked for, and it will not run. Not because of disk space — the disk is
eight gigabytes now — and this is worth writing down once, in order:

- **glibc.** Steam is dynamically linked against it. That needs `PT_INTERP`
  honoured, `ld-linux-x86-64.so.2` present, and then several hundred syscalls
  glibc's startup touches. Nothing here loads an interpreter yet.
- **32-bit.** The Steam bootstrap is i386. That is a second system-call table,
  a second ABI, and `CONFIG_IA32_EMULATION`'s worth of work.
- **X11 or Wayland.** Steam draws through one of them. This machine has a
  compositor with its own protocol and no X server.
- **OpenGL and Vulkan.** Steam's own interface is GPU-composited. There is no
  GPU driver here and no software rasteriser that speaks either API.
- **CEF.** The store is an embedded Chromium. That is tens of millions of lines
  of C++ expecting a full POSIX system, threads, and a GPU process.
- **Networking, audio, DRM.** Sockets, PulseAudio or ALSA, and a licensing
  handshake with Valve's servers.

Any one of those is larger than this whole operating system. Saying "Steam
works" after building a stub that draws a window would be the kind of claim
this project exists not to make.

What is real is the direction: a static Linux binary that uses files, memory and
the standard descriptors runs here today, unmodified, and the boundary grows by
reading the `[linux] call N is not translated yet` lines that real programs
produce.
