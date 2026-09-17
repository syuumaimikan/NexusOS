# Steam and graphics compatibility

Implementation boundary, 2026-09-17. The chosen direction is to extend the
NexusOS kernel and Linux compatibility layer; a Linux guest is not the solution.

**Steam does not run. Vulkan and OpenGL are not implemented, and no part of
glibc or musl is present on this machine.** Downloading and
unpacking a Debian archive is not a runnable Steam installation. Do not mark
Steam installed or create a working Steam launcher until the runtime and its
dependencies actually execute.

This page keeps two things apart that are easy to run together: what the machine
can now *do*, each line of which has a script that boots it in QEMU and fails if
the claim stops holding, and what Steam would *need*, most of which is not
started.

## Verified

Named beside each claim is the script that checks it.

### Programs built for Linux, compiled here

`cargo guest` · `tools/nexus-guest`

The fixtures used to be hand-assembled, byte by byte, because building a Linux
binary needs a Linux toolchain and this machine has none. It turns out not to
need one: `rustc` targets `x86_64-unknown-linux-gnu` with `core` rebuilt from
source, and `rust-lld` links a static `ET_EXEC` with no C runtime. The programs
in `tools/nexus-guest` are real Linux executables produced with no Linux
toolchain and no network.

The hand-assembled fixtures stay. They are the ones that prove the *loader*
works on an image nobody could have shaped to suit, and one of them is a dynamic
linker, which is not something a compiler will emit.

### Dynamic execution

`scripts/test-linux.ps1`

Almost every real Linux program is `ET_DYN` with a `PT_INTERP`, and starting one
is loading *two* images into one address space, entering the second, and handing
it four numbers it cannot work out for itself.

- `nexus_abi::elf` loads `ET_DYN` at a caller-chosen bias and reports
  `PT_INTERP`. The two kinds of image and the two kinds of bias must agree.
- The kernel loads the interpreter named by `PT_INTERP` from the Linux root,
  into the same address space, and enters *it*.
- The auxiliary vector carries `AT_BASE`, `AT_ENTRY`, `AT_PHDR`, `AT_PHENT`,
  `AT_PHNUM`, `AT_RANDOM`, `AT_EXECFN` and the identity entries. `AT_PHDR`
  describes the program, never the interpreter.
- `BIN/LINUX.LST` on the EFI partition says what to install into the Linux root
  at boot, which is how an interpreter gets somewhere a program can name it.

**This is not glibc's `ld-linux-x86-64.so.2`.** The interpreter it was tested
against is hand-assembled and lives in this repository. What is verified is the
kernel's half of the contract; whether glibc's loader works against it is
untested and unknown.

### The vector unit

`scripts/test-linux.ps1` checks the boot line; every compiled guest depends on it

This kernel and its own programs are built `+soft-float` with SSE off, and the
machine had therefore never enabled SSE at all. The x86-64 System V ABI *is*
SSE — `double` is passed in `xmm0`, and every `memcpy` in every C library is
written with it — so the first compiled Linux program to run here raised
**invalid opcode** on its first `xorps`.

`arch::fpu` enables it per processor and gives every thread its own 512-byte
`fxsave` area, saved and restored on the context switch. Enabling it without
that would have been worse than leaving it off: two threads would share sixteen
vector registers.

AVX is still absent: that needs `XCR0` and `xsave`, and a program built with
`-mavx2` will take `#UD`.

### Memory

`scripts/test-linux.ps1`

`MAP_FIXED` and `MAP_FIXED_NOREPLACE`; file-backed private mappings at an
offset; `PROT_EXEC`; `mprotect` that changes the page tables and leaves the
contents alone; per-process placement. Refused, deliberately:
writable-and-executable in one mapping, so no just-in-time compiler runs;
`MAP_SHARED` of a *file*, because the promise cannot be kept; `PROT_NONE`.

`memfd_create` and `ftruncate` make memory with a descriptor on it — a
`MemoryObject`, which is what this system already calls memory more than one
process can see. `mmap` of one with `MAP_SHARED` maps the same frames, and the
mapping keeps the object alive after the descriptor is closed, which is the
normal pattern and was a real defect until it was not.

### Threads

`scripts/test-linux.ps1`

`clone` with `CLONE_VM|THREAD|SIGHAND|FILES` makes a second thread in the same
Nexus process. The new thread starts with a **copy of its parent's whole
register state**, which is why the system-call entry stub now saves all sixteen
registers; every C library's thread entry reads the function to call out of a
register set before the call. `CLONE_SETTLS`, with `FS_BASE` carried across the
context switch. `futex` `WAIT`/`WAKE`, with and without a timeout.
`CLONE_CHILD_CLEARTID`, which is what makes `pthread_join` return. `exit` ends a
thread; `exit_group` ends the program.

`fork` is refused with `ENOSYS` rather than quietly made into a thread.

### Descriptors, pipes and readiness

`scripts/test-linux.ps1`

`dup`, `dup2`, `dup3`; `pipe` and `pipe2`; `poll`, `ppoll`, `epoll_create1`,
`epoll_ctl`, `epoll_wait`, `epoll_pwait`. Level-triggered, because a level
cannot be lost; `EPOLLET` is refused rather than quietly given the other
behaviour.

A pipe is a new Nexus object — a byte stream with two ends, with its own system
call, available to Nexus programs too. A channel carries *messages* and could
not be made into one without losing something.

### Sockets

`scripts/test-linux.ps1`

`AF_UNIX` stream sockets: `socket`, `socketpair`, `bind`, `listen`, `accept`,
`accept4`, `connect`, `send`/`recv`, `sendmsg`/`recvmsg`, both the filesystem
and the abstract namespace. **`SCM_RIGHTS` works**: a descriptor sent across a
connection arrives as a descriptor on the other side, because a descriptor here
is a handle and a handle is a thing that crosses boundaries.

`AF_INET` is refused: this machine's TCP stack is reached through a different
object, and connecting the two is real work.

### Signals

`scripts/test-linux.ps1`

`rt_sigaction`, `rt_sigprocmask`, `rt_sigreturn`, `kill`, `tgkill`. A handler
runs on the program's own stack below the red zone, returns through a trampoline
the kernel puts in the program's address space when its C library has not
supplied one, and the interrupted call returns what it was going to return.
Blocking, unblocking and ignoring all behave.

Delivery happens on the way out of a system call and nowhere else, so a program
that spins without making one cannot be signalled. There are no kernel-raised
signals: no `SIGSEGV` on a fault, no `SIGPIPE`, no `SIGCHLD`.

### Becoming another program

`scripts/test-linux.ps1`

`execve`. The process survives — same identifier, same handles — and its memory
does not: the address space is emptied and the new program is loaded into the
same one. Arguments and environment are carried across, and so are descriptors,
which is what makes a bootstrapper able to hand the program it starts a
connection it had already made. `O_CLOEXEC` is not honoured. A program with more
than one thread is refused rather than raced.

### Thirty-two bit programs

`scripts/test-linux.ps1`

An i386 program is not a smaller x86-64 one. It is a different image format
(`ELFCLASS32`, `EM_386`, a fifty-two byte header, program headers whose fields
are in another order), a different way into the kernel (`int 0x80` rather than
`syscall`), a different table of call numbers (`write` is 4 rather than 1), and
different argument registers.

All four are here. The processor enters compatibility mode through a code
segment that was already in the descriptor table — `sysret` requires the user
segments to be laid out with the thirty-two bit code segment first, so it has
been there since the system-call boundary was written, and nothing had ever been
entered through it. There is a gate at vector `0x80` at privilege level three,
which is the only vector in the table a program may invoke.

The program tested is compiled: Rust for `i686-unknown-linux-gnu`, static, no C
runtime. It writes, asks its identifier, maps a page with `mmap2` — whose offset
is in pages, which is why i386 has a second `mmap` — reads it back, unmaps it,
and walks its own auxiliary vector, which is in *four-byte* words. The last
check compares `AT_ENTRY` against where `_start` really is.

What is translated is the calls whose arguments are integers and pointers.
Everything that passes a structure is refused by name, because the structures
differ: `struct stat64` is not `struct stat`, an `iovec` is eight bytes rather
than sixteen, a `timespec` is eight rather than sixteen, and `set_thread_area`
wants a local descriptor table this system does not have. So a thirty-two bit
program that reads, writes, maps memory and exits runs; one that asks for the
time or installs a signal handler is told so by name.

There is no thirty-two bit interpreter, so an `ET_DYN` i386 image is refused
rather than loaded and entered unrelocated.

### A window for a Linux program

`scripts/test-linux-window.ps1`, evidence in `build/linux-window.png`

A static Linux executable opens `/dev/nexus/display`, asks how large its window
is, maps the buffer `MAP_SHARED`, fills it and says which rectangle changed. The
compositor composites it beside this system's own windows, through the same path
and with the same endowments.

### Wayland, and a window that behaves like one

`scripts/test-wayland.ps1`, evidence in `build/wayland.png`

Two programs built for Linux. One binds a Unix socket at `/tmp/wayland-0` and
opens `/dev/nexus/display` for a window; the other is started with **no window
of its own** and connects to that name. The second is right: a Wayland client's
window is its `wl_surface`, and that lives in the Wayland compositor.

The client does what a real one does, in the order a real one does it:

1. `wl_display.get_registry`, and bind `wl_compositor`, `wl_shm`,
   `xdg_wm_base`, `wl_seat` and `wl_output`.
2. `wl_compositor.create_surface`, `xdg_wm_base.get_xdg_surface`,
   `xdg_surface.get_toplevel`, a title and an application identifier, and then a
   commit with **nothing attached** — which is `xdg-shell`'s way of saying "tell
   me how big to be". Attaching a buffer before the first configure is a
   protocol error.
3. The compositor answers `xdg_toplevel.configure` with a size and states, then
   `xdg_surface.configure` with a serial, which is the two-message pattern that
   makes a configuration atomic.
4. Only then: `memfd_create`, `mmap` shared, draw, `wl_shm.create_pool` with the
   **descriptor** sent across by `SCM_RIGHTS`, `create_buffer`, `attach`,
   `damage`, `frame`, `commit`.
5. And then a loop: `wl_buffer.release`, the frame callback, a `ping` that needs
   a `pong`, a second configure at a different size, keys through
   `wl_keyboard`, and `xdg_toplevel.close`.

The messages on that socket are the Wayland wire protocol: the object
identifiers, opcodes, sizes and padding of `wayland.xml` and `xdg-shell.xml`.

What the test checks is the four things the gate below names, and the last two
from outside the machine:

- **It presents frames.** The client's colours are in a screendump.
- **It resizes.** The compositor asks for 240x150 having given 320x200, and the
  band on the screen is 240 wide afterwards — with a third colour in it that the
  client draws only in the second frame. A frame that had not been redrawn would
  still be 320 wide.
- **It receives input.** A key typed at the machine's keyboard goes keyboard →
  kernel → this machine's compositor → the focused window, which is the Wayland
  compositor → `/dev/nexus/display` → `wl_keyboard.key`, and arrives at the
  client as evdev code 30.
- **It releases buffers.** `wl_buffer.release` and the frame callback, twice
  each, checked by the client's own exit status.

**It is still not a Wayland compositor.** One client, one toplevel; no
subsurfaces, no popups, no regions, no pointer, no touch, no output scaling, no
`linux-dmabuf`, and no protocol errors — a client that misused an object is
ignored where a compositor would disconnect it. The keymap is sent with format
`NO_KEYMAP`, because producing an XKB one needs `xkbcommon` and a wrong keymap
puts letters under the wrong keys. Window management is a script rather than a
policy: the compositor resizes the window once and then closes it, because there
is nobody there to drag an edge.

There is also a translation in it that a real compositor never does. This
machine's own keyboard protocol carries *characters*; Wayland carries evdev key
codes, which are positions on a keyboard. The compositor maps back, assuming an
English layout, and that is the one place in the program that guesses.

It has never been tested against `libwayland`, because there is none on this
machine and no toolchain here could build one. It was tested against the client
in this repository, written from the same protocol description. That is a weaker
claim than "Wayland works" and it is the one being made.

### A SPIR-V shader, read and run

`scripts/test-shader.ps1`, evidence in `build/shader.png` · `cargo test -p nexus-spirv`

Nobody writes SPIR-V. It is what `glslang`, `shaderc` and `naga` *emit*, and it
is what Vulkan takes: `vkCreateShaderModule` is handed a block of it and nothing
else. A machine that cannot read SPIR-V cannot run a shader anybody else
compiled, whatever else it can draw — so it is the first thing between this
system and any graphics interface worth the name.

`shared/nexus-spirv` reads a module and runs it. Twenty-one host tests cover the
header, the instruction stream, strings (whose length decides where every later
operand is), types, constants, `Location` and `BuiltIn` decorations, the
extended instruction set, and `OpPhi` — which is the part that looks strange and
is not: in single-assignment form a value that depends on which way control came
has to say so.

`tools/nexus-guest/src/bin/shader.rs` is a program **built for Linux** that
assembles a module, reads it back, and runs it once per fragment into the window
the compositor gave it. The shader is the one every tutorial starts with:

```glsl
vec2 uv = gl_FragCoord.xy / vec2(160.0, 100.0);
float ring = 1.0 - clamp(length(uv - vec2(0.5)) * 2.4, 0.0, 1.0);
colour = vec4(uv.x * 0.25 + ring, uv.y * 0.35 + ring * 0.2, ring, 1.0);
```

It uses what makes a shader a shader rather than a loop: an input it did not
declare the contents of, component arithmetic, and two functions out of
`GLSL.std.450`. The test checks from outside the machine that the *disc* is on
the screen and that it fades outwards — a window of one flat colour, or one the
shader never touched, fails that.

Two things fell out of it that are worth naming:

- **A `no_std` program with no libc still needs a heap.** Reading a module needs
  `Vec`. `tools/nexus-guest/src/heap.rs` is a bump pointer over one `mmap`, and
  it never frees — which works only because a shader invocation has *no state
  that outlives it*. The program takes a mark, runs one fragment, and winds the
  pointer back. Sixteen thousand invocations, each allocating hundreds of times,
  leave the arena holding the module and the colours and nothing else.
- **`core` has no `sqrt` and no `floor`.** They are one instruction on this
  processor and library calls elsewhere, so the standard library owns them.
  `nexus_spirv::run` carries its own.

**This is not Vulkan, and it is not conformant SPIR-V.** No images or samplers,
no uniform or storage buffers, no push constants, no matrices, no function
calls, no atomics; an instruction that is not implemented is reported *by
number* rather than skipped. And the module it runs was assembled in this
repository, not produced by `glslang` — there is no shader compiler on this
machine, so what is proved is that the reader agrees with the specification as
written down here.

### Earlier, and still true

`pread64` with positive offsets, EOF and negative-offset rejection, leaving the
descriptor's cursor alone. A native CPU rasterizer in `nexus_ui::raster3d`: a
native API that cannot load SPIR-V, expose Vulkan entry points, or give Mesa the
interfaces it needs.

## Required work and acceptance gates

| Stage | State | What is left | Acceptance evidence |
| --- | --- | --- | --- |
| Dynamic ELF execution | **Kernel half done** | A real libc on the disk | An unmodified program starts through *glibc's* loader |
| Process/runtime services | **Threads, futex, TLS, signals, pipes, poll/epoll, Unix sockets, `execve` done** | `fork`, kernel-raised signals, `O_CLOEXEC`, filesystem links and permissions, `AF_INET` sockets | libc and threading test suites run in the guest without success-returning stubs |
| 32-bit execution | **A static i386 program runs** | Structure-passing calls (`stat64`, `iovec`, `timespec`, `sigaction`), `set_thread_area` and an LDT for thread-local storage, a 32-bit interpreter and 32-bit libraries | The Steam bootstrap starts |
| Linux desktop transport | **A window is negotiated, drawn, resized, typed at and closed** | Tested against `libwayland` rather than against a client written from the same description; an XKB keymap; pointer and touch; subsurfaces, popups and regions; several clients; protocol errors; then Xwayland | An **unmodified** `libwayland` client presents frames, resizes, receives input and releases buffers |
| Shaders | **A SPIR-V module is read and executed, and its pixels reach the screen** | A module from a real compiler rather than from this repository; images, samplers, buffers, push constants, matrices, function calls; compiling a module instead of interpreting it | `glslang`'s output for a non-trivial shader runs and matches a reference image |
| Vulkan | **Not started** | An actual ICD and its OS interfaces — which needs a libc first; or, for Venus, capset negotiation, contexts, blob resources, host-visible mappings, synchronization | The Vulkan loader discovers the ICD, `vulkaninfo` succeeds and `vkcube` presents; then conformance |
| Steam installation | **Not started** | Package decompression, dependencies and scripts; the runtime bootstrap; graphics, audio and network integration | Download the official installer in the Nexus browser, install, launch the genuine client, reach sign-in, restart |

## Can Mesa not just be used?

It is the right question and the answer is worth writing down, because "is there
open-source OpenGL and Vulkan" has an emphatic yes attached to it. Mesa contains
`lavapipe`, a software Vulkan that passes conformance; `llvmpipe`, a software
OpenGL; `zink`, OpenGL over Vulkan; and drivers for real hardware. SwiftShader is
a second software Vulkan. `virglrenderer` and Venus pass the work to a host.
All of it is free, and none of it is the obstacle.

**The obstacle is that Mesa is a program for a POSIX system, and this is not one
yet.** It wants a C library, `pthreads`, `dlopen`, `/dev/dri` or a windowing
backend, and — for every path that is not painfully slow — LLVM. This machine
has *no libc at all*. Nothing on it can link against one, because there is not
one to link against.

So the ordering is not a matter of taste:

| | needs |
|---|---|
| Mesa, SwiftShader, any real ICD | a C library, threads for C, `dlopen`, and LLVM for the fast paths |
| glibc's own `ld-linux-x86-64.so.2` | the same C library it is part of |
| Steam's bootstrap | a 32-bit C library, and a 64-bit one for everything after |

All three roads run through the same gate, and it is not a graphics gate. A
libc — musl is the plausible one, being small and static-friendly — is the piece
that unlocks Mesa, the real dynamic loader, and Steam at once. Everything in the
table above that says "not started" is downstream of it.

What *can* be done without a libc is what has been done: take the published
formats, which are Khronos specifications rather than anybody's code, and
implement them. That is what `shared/nexus-spirv` is. It is not a substitute for
Mesa and is not pretending to be one — it is the half of the problem that does
not need a C library, done first because it could be.

## Verification

```powershell
cargo test --offline -p nexus-abi
cargo test --offline -p nexus-spirv
cargo kernel --offline
cargo guest --offline
cargo guest32 --offline
./scripts/build.ps1
./scripts/configure-disk.ps1
./scripts/test-linux.ps1
./scripts/test-linux-window.ps1
./scripts/test-wayland.ps1
./scripts/test-shader.ps1
```

`build.ps1` builds the guest programs, stages them, and writes `BIN/LINUX.LST`,
which the kernel reads on the first boot to install them into the Linux root.
`configure-disk.ps1` answers the first-run wizard, which the two graphical tests
need because they drive a desktop.

`test-linux.ps1` runs ten programs at boot and maps every exit status back to
the step that failed. `test-linux-window.ps1` presses F4; `test-wayland.ps1`
presses F5 and F6. Both take a screendump over the QEMU monitor and look for the
colours the program wrote, in the order it wrote them.

If a script reports that the disk image is held by another process, a QEMU from
an earlier run is still alive; stop it and run again.

## Upstream integration references

- [Valve's official launcher package](https://repo.steampowered.com/steam/)
  identifies the download as a launcher/bootstrapper.
- [Mesa Venus requirements](https://docs.mesa3d.org/drivers/venus.html)
  describe the actual virtio-gpu and host Vulkan prerequisites. Enabling a QEMU
  flag alone cannot implement the guest driver or ICD.
- [Wayland architecture](https://wayland.freedesktop.org/architecture.html)
  describes compositor/client buffer and input exchange.
- [Xwayland integration](https://wayland.freedesktop.org/docs/book/Xwayland.html)
  explains the additional X11 compatibility server and window manager channel.
