# NexusOS, as it actually is

Written by Claude Code for GPT-6 Astra, 2026-09-14.

This is a briefing, not a roadmap. Everything below is in the repository and
runs; where something is absent it says so. `docs/NEXUSOS_ROADMAP.md` has the
long version and an honest "what is not here" table near the end.

Keep this file current. A collaborator working from a stale description of the
machine writes code that cannot compile against it.

## What the machine is

x86_64, UEFI, booted under QEMU. A microkernel-shaped system: the kernel owns
memory, threads, channels, the filesystem and the network card, and everything a
person sees is a user program talking to the compositor over a channel.

Rust first, assembly where it must be, C nowhere yet. `no_std` everywhere. No
floating point in the kernel — the FPU is not enabled, so anything that needs a
cosine computes it with integer cross-multiplication (see `shared/nexus-index`).

## The authority model, which is the part that constrains everything

**Nothing is ambient.** A program starts holding channel handle `1` and nothing
else. Every other power it has arrived in a message: a directory handle, a
channel to a service, a shared-memory handle. There is no path from a program to
a file it was not lent, no way to start a program except by asking a service that
holds the authority to, and no global namespace to look anything up in.

Handles carry rights: `READ`, `WRITE`, `TRANSFER`, `CLOSE`. `TRANSFER` is the
right to cross a channel at all. A handle lent without `CLOSE` cannot be taken
away from the lender. `duplicate` may only narrow rights, never widen them.

This is not aspirational. `user/nexus-find` — the existing agent — is given one
directory with read and transfer, and the *first thing it does* is try to create
a file there and report being refused. A program that merely does not write is
not a program that cannot.

**Anything the AI Runtime does must fit this.** An AI agent with a path string
and an open() that consults a root directory would be the one ambient authority
in the system. If you need the runtime to reach something, the answer is which
handle it is lent and by whom, not which path it opens.

## System calls

Thirty-two, in `kernel/nexus-kernel/src/arch/syscall.rs`, wrapped by
`user/nexus-user/src/lib.rs`:

```
 0 Exit            5 ChannelCreate    10 MemoryCreate    20 ProcessWait
 1 Log             6 ChannelWrite     11 MemoryMap       21 WaitSetCreate
 2 Uptime          7 ChannelRead      12 MemorySize      22 WaitSetAdd
 3 Yield           8 HandleClose      13 NodeOpen        23 WaitSetRemove
 4 ThreadId        9 HandleRights     14 NodeCreate      24 WaitSetWait
                                      15 NodeRemove      25 HandleDuplicate
26 Sleep          29 NodeReadAt       16 NodeList        27 ProcessKill
31 Now            30 NodeWriteAt      17 NodeRead        28 MemoryUnmap
                                      18 NodeWrite
                                      19 NodeSize
```

Notable absences, because they shape any design: **no threads in user space**
(one thread per process), **no standard output or pipes**, **no fork**, **no
mmap of a file**, **no dynamic linking**, **no signals**. A message is at most
256 bytes and carries at most 4 handles.

`WaitSetWait` takes a millisecond deadline; `u64::MAX` means forever.

## Blocking, and the rule that keeps it correct

Every blocking wait in this system follows one discipline, and getting it wrong
has cost this repository two hangs:

```rust
loop {
    let seen = queue.generation();   // read the counter FIRST
    if condition() { return; }       // then test
    queue.wait_if_unchanged(seen);   // then block, atomically against the counter
}
```

Read-then-test-then-block. A waker between the test and the block moves the
counter, and `wait_if_unchanged` refuses to sleep. `WaitQueue::wait()` was
deleted because it could not be used correctly.

## IPC

Channels are bidirectional, bounded, and carry bytes plus handles. Services are
**request-and-reply only** — nothing is ever pushed at a program that did not
ask. Four-byte tags, little-endian payloads, `MAX_MESSAGE = 256`.

The network, sound and spawn services all work this way. If the AI Runtime needs
a service, that is the shape to use; a subscription would be the first thing in
the system that pushes.

## Windows

`user/nexus-window` (new, this session) is the client half of the compositor's
protocol. A program implements `App`:

```rust
pub trait App {
    fn draw(&mut self, canvas: &mut Canvas);
    fn key(&mut self, key: Key) -> bool;         // did anything change?
    fn resized(&mut self, width: u32, height: u32) {}
    fn tick_ms(&mut self) -> Option<u64> { None } // wake on a clock
    fn ticked(&mut self) -> bool { false }        // clock, or a watched handle
    fn running(&self) -> bool { true }
}
```

and calls `Window::open(COMPOSITOR, SURFACE_AT, &mut lent)?` then `window.run(&mut app)`.
`lent` receives whatever authority the compositor was told to hand this program.
`window.watch(handle, key)` adds anything else to wait on; readiness arrives as
`ticked()`.

Drawing is `nexus-ui`: a canvas, rectangles, gradients, a font that covers
Japanese, and a one-dimensional column layout. There is no toolkit, no retained
scene and no widget library. A frame is drawn from nothing every time.

The handshake: draw, send `damaged`, wait for `shown`. Never draw before `shown`
comes back — the compositor is reading that memory.

**AI UI would be a window like any other.** What it is lent is the compositor's
decision, made in `open_window` in `user/nexus-compositor/src/main.rs`.

## What exists in the AI area already

* `shared/nexus-index` — character 3-gram hashing into 256 buckets, cosine
  compared by exact integer cross-multiplication. **It is not a model.** Nothing
  is trained and there is no learned weight anywhere in it. Characters rather
  than words, because Japanese has no spaces.
* `user/nexus-find` — the smallest thing that is honestly an agent: given one
  directory, it decides which files to read and returns the closest match.

There is no model runtime, no inference, no tokeniser beyond the n-gram hash, no
tensor code, and no network client an agent may use. There is a working TCP
stack and an HTTP client (`shared/nexus-http`, `shared/nexus-dns`) that the
browser uses; an agent would have to be *lent* the network handle to reach it.

## Removing a file

`remove` unlinks. The name goes at once; the blocks go when the last handle
closes. It used to refuse while anybody held the file open, and that cost more
than it was worth: several programs read the settings file on a clock, their
reads take microseconds, and replacing that file therefore failed at random with
an error no program could do anything sensible about.

The property the refusal protected is still guaranteed and is now checked
directly by `init`: **after a name has gone, a handle that was already open
still reads the same bytes.** An inode freed under a live handle would leave
that handle naming a number the filesystem is free to give the next file.

If the machine stops between the name going and the blocks being freed, the
inode leaks. That is the safe direction, and the filesystem's own check finds
and reclaims exactly that.

## Text, and where strings live

Every string a person sees is in `locales/en-US.txt` and `locales/ja-JP.txt`,
keyed, and reached with `nexus_i18n::text("key")` or `nexus_i18n::format`. **A
user-visible string literal in code is a defect here**, not a style preference.
Both files must gain the same keys or the build fails.

## Settings

One text file, `system/settings.txt`, `key = value`. Nothing is pushed when it
changes: the wallpaper re-reads every two seconds, the desktop every second, the
kernel when it next brings the network up. Every reader treats an unreadable
value as its default. See `docs/settings.md`.

## Building and testing

```
powershell -File scripts/build.ps1        # kernel, bootloader, programs, disk image
powershell -File scripts/run.ps1          # boot it
powershell -File scripts/test.ps1         # the whole suite, sixteen stages
powershell -File scripts/soak.ps1 -Count 40   # boot repeatedly, catch rare hangs
```

Per-crate lint, which is what CI enforces:

```
cargo +nightly clippy -p <crate> --target targets/x86_64-nexus-user.json \
    -Zbuild-std=core,compiler_builtins,alloc \
    -Zbuild-std-features=compiler-builtins-mem -- -D warnings
```

A new user program needs four registrations: `Cargo.toml` members, the `user`
alias in `.cargo/config.toml`, a `Publish-Program` line in `scripts/build.ps1`,
and — if it is a window — a `What` variant and a `desk::` message in the
compositor plus a button in `user/nexus-shell`.

## House rules that are not negotiable

* Build after writing code. Never leave the tree broken.
* No `todo!()`, no `unimplemented!()`, no mock layer. If a thing is not
  implemented, it is absent and the roadmap says so.
* `unsafe` is allowed and must carry a `// SAFETY:` comment saying why it holds.
* Nothing is "verified" because it compiled. It is verified because it ran.
* Never ignore a warning.

## Known-open problems, stated plainly

* **A rare boot hang.** One boot in roughly seventy stopped between two kernel
  self-tests with no diagnostic. The self-tests are now named as they start
  (`[test] begin <name>`) so the next occurrence says where, and `scripts/soak.ps1`
  exists to hunt it. Sixty-six boots since without a recurrence.
* **Test-harness reliability.** Killing QEMU can lose the tail of the serial log,
  which has produced at least two failures that looked like product bugs. Tests
  are being moved to a clean monitor `quit`. Treat an abrupt end to a serial log
  as suspect before treating it as a hang.
* **No damage rectangles per client.** A client says "I drew"; the compositor
  repaints that window's whole rectangle. An animated full-screen wallpaper
  therefore costs a full composite per frame and is capped at 4 Hz.
* **One thread per process.** Anything that wants concurrency inside a program
  has to be an event loop, because there is no second thread to give it.
