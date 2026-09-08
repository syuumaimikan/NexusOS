# NexusOS Roadmap

The ordering principle throughout: **each phase must actually run before the
next one starts.** A phase is complete when it boots in QEMU, its serial output
proves the behaviour, and its tests pass — not when the code compiles.

Status: ✅ complete · 🚧 in progress · ⬜ not started

---

## Phase 0 — Repository audit ✅

Empty repository confirmed; toolchain, firmware and emulator verified. See
[NEXUSOS_ARCHITECTURE_AUDIT.md](NEXUSOS_ARCHITECTURE_AUDIT.md).

## Phase 1 — Boot ✅

UEFI → Nexus Bootloader → Nexus Kernel, with serial output and a painted
framebuffer.

Delivered: hand-written UEFI bindings, GOP mode selection, ESP file loading,
ELF64 loader, 4-level page tables (identity, direct, per-segment kernel, guarded
stack), memory-map normalization, `ExitBootServices`, higher-half kernel entry,
serial console, panic handler, QEMU build/run/screenshot harness, 16 unit tests.

## Phase 2 — CPU and interrupts ✅

Without this, every fault is a silent triple fault. Highest priority.

- GDT with kernel and user code/data descriptors
- TSS, with an IST stack for double faults so a stack overflow is still
  reportable
- IDT and handlers for all 32 architectural exceptions
- Decoded page-fault reporting (address, cause, faulting instruction)
- Legacy PIC masked; local APIC brought up
- APIC timer calibrated against the PIT
- IRQ routing, `end_of_interrupt`, interrupt-safe spinlocks

**Done when:** a deliberate fault prints a decoded diagnostic instead of
resetting, and a periodic timer interrupt ticks.

Delivered: GDT with descriptors ordered for `syscall`/`sysret`; TSS with IST
stacks for double fault, NMI and machine check; all 256 IDT vectors populated,
with decoded page-fault and selector-error reports; the 8259 remapped above the
exception vectors and masked; a 1000 Hz PIT tick; `IrqSpinLock`. Three
fault-injection builds prove the exception path by taking real faults.

The local APIC is deferred to after Phase 4: its registers sit above RAM, so
mapping them needs a virtual memory manager. The PIT reaches the same place
through port I/O alone.

Also delivered since: the I/O APIC. Its redirection entries are programmed from
the MADT, including the interrupt source overrides, so devices other than the
timer can raise interrupts — which is what made a keyboard possible at all.

## Phase 3 — Physical memory ✅

- Frame allocator over the handoff memory map (buddy allocator)
- Frame accounting and statistics
- Reserved-range handling for the kernel image, loader data and the framebuffer

**Done when:** the kernel allocates and frees frames under a stress test that
verifies no frame is ever handed out twice.

Delivered: a buddy allocator in the `nexus-mm` crate, with free-list links
stored inside the free blocks and coalescing driven by one parity bit per buddy
pair. 21 tests including a 20000-step randomised workload. On hardware: 1017 MiB
across 260513 frames from a 33 KiB bitmap.

## Phase 4 — Virtual memory ✅

- Kernel-owned page tables replacing the loader's, and teardown of the identity
  map
- `map`/`unmap`/`protect`, TLB shootdown
- Kernel heap and a global allocator, unlocking `alloc` in the kernel
- Guard pages, PAT setup for write-combining framebuffer access

**Done when:** the kernel runs on its own tables with `Vec` and `Box` available.

Delivered: `map`/`unmap`/`translate` over the bootloader's tables, teardown of
the identity map, and a 16 MiB kernel heap behind a `GlobalAlloc`. The heap
allocator lives in `nexus-mm` with 13 tests. The on-hardware test cross-checks a
heap address against the direct map through `translate`, which is what actually
proves the mappings are right.

Still outstanding from this phase: PAT setup for write-combining framebuffer
access, and TLB shootdown, which cannot be written before there is a second
core to shoot down.

## Phase 5 — Processes, threads, scheduling 🚧

- Process and thread objects, address spaces, TLS
- Context switching, preemption from the APIC timer
- Priorities, sleep/wake, per-CPU run queues
- TLB shootdown across processors
- SMP bring-up via the MADT, per-CPU data
- Ring 3 transition, `syscall`/`sysret`
- A separate address space per process

Delivered: kernel threads with their own guarded stacks, context switching,
preemption from the timer, strict priority with round-robin inside each level,
sleep and wake, thread exit and reaping, and an idle thread per processor.

**Done when:** a thread that never yields can still be taken off the processor.
Verified: a sleeping ticker woke on schedule five times while a non-yielding
thread at equal priority completed twelve million iterations.

Also delivered since: ACPI table parsing (RSDP, XSDT, MADT) and the local APIC
timer, calibrated against the PIT and now driving the scheduling tick with the
8259 and PIT shut down behind it.

SMP bring-up is also done: every processor the MADT reports is started, through
a real-mode trampoline and INIT/SIPI, and each runs on its own local APIC timer.

And every one of them now schedules. Which thread is running, which to fall back
to, and whether a preemption is due are per-processor, held in the `GS`-based
per-CPU area and readable without a lock; the thread table and the run queues
stay shared behind one lock. A thread completes its departure from a processor
only once the *next* thread there has run, which is what keeps a second core
from switching onto a stack whose pointer has not been saved. The boot self-test
records which processors ran its workers and fails if the answer is only one.

TLB shootdown came with it, because it had to. `invlpg` invalidates on the
processor that runs it and nowhere else, so unmapping a page while other cores
hold the translation is a silent read or write to memory that has been handed to
someone else. Each processor now has a shootdown mailbox that is both
interrupted and polled: the interrupt reaches a core running normally, and the
polling — from every spin loop — reaches one spinning for a lock with interrupts
masked, which is the case that otherwise deadlocks, since reaping a thread
unmaps its stack while holding the scheduler lock.

Ring 3 is open. A user program runs at user privilege, in pages of its own that
it cannot write and on a stack it cannot execute, and the only way back into the
kernel is `syscall`. The entry stub is the interesting part: `syscall` does not
switch stacks, so the processor arrives in ring 0 still standing on the user's,
and `swapgs` plus the per-CPU area is how it gets off without destroying a
register belonging to the caller. Every interrupt entry does the same, because
user code can zero `GS.base` with one instruction and the next timer tick would
otherwise read per-CPU state through a null pointer.

Five system calls, each one exercised from ring 3 by the program the kernel
starts at boot, because a call that has never been made from user mode is a
function with an unusual name. The boot test fails unless an interrupt was taken
from ring 3 — system calls alone would not prove user privilege, since `syscall`
is legal from ring 0 — and an injection build reads kernel memory from ring 3 to
show the boundary keeps something out rather than merely being crossable.

Address spaces followed. Each process has a page-table root of its own; the
kernel's upper half is shared by copying the top-level entries, so a kernel
mapping made later appears in every space at once, and the entries are all
created up front so that they never change afterwards. The scheduler loads the
incoming thread's root on every switch, skipping the write when it is already
right — writing `cr3` discards every non-global translation, so doing it
needlessly would throw away a working set.

The proof is on both sides of the boundary. The kernel compares the two
processes' page tables and reports the different frames one address maps to,
which is a fact rather than an interleaving; the two programs write their own
identifier to that address and read it back two hundred times, which would
collide within a few rounds if the page were shared. An injection build gives
them one page between them, so the second check is known to be able to fail.

Finding that took a scheduler bug with it. A newly created thread started with
its interrupt flag already set, so it could be preempted in the handful of
instructions between being switched to and releasing the thread it displaced —
and the displaced thread was then left ready, on no run queue, never to run
again. About one boot in a dozen lost a thread that way. New threads now start
with interrupts masked and enable them once the hand-off is done, the hand-off
slot asserts that it is not being overwritten, and the monitor checks every five
seconds that no thread is ready and unqueued.

Still outstanding: per-processor run queues, thread affinity, and a process
object worth the name — there is no parent, no exit status, and no way to create
one from inside the system.

## Input 🚧

Not a phase of its own either: the system had to become interactive before the
language could be *chosen* rather than cycled on a timer.

Delivered: the I/O APIC routing the keyboard's line to a vector, a PS/2 driver
reading scancode set 1, decoding to keys, and an input thread that acts on them
— typing edits a line on screen, F1 switches the interface language.

Verified from outside: `scripts/test-input.ps1` types through QEMU's monitor and
checks the guest reports back the word that was typed and the language change.
Nothing internal can prove an input path; only driving it from outside can.

The input thread no longer polls: it blocks on a wait queue that the keyboard
interrupt wakes. At ten seconds of uptime with the same keystrokes, that took
the system from 1766 context switches to 688.

Outstanding: there is no focus, no delivery to a process, and no mouse. Keys are
acted on by the kernel itself, which is where a window server will take over.

## Phase 6 — Handles, IPC, system calls 🚧

- Handle table with per-handle rights, the root of the capability model
- Channels, shared memory, events, semaphores, mutexes
- The Nexus system-call ABI
- Zero-copy message passing

Delivered: wait queues, a handle table with per-handle rights, message
channels, and ten system calls.

A handle is an index into a table that belongs to one process, and holding it is
the authority — there is no way to name a channel a process was not handed, so
there is no ambient check to forget. The rights beside it say what may be done,
so the same object can be given to one process to read and another to write.
Both compatibility layers will be tables above this one rather than a second
idea of authority inside the kernel.

A channel is two endpoints, each with an inbox; writing to one appends to the
*other* and wakes whoever waits there. Messages are whole, because a byte stream
pushes framing into every user of it. An endpoint holds its peer weakly, which
is what makes "the other end has gone" something the kernel states rather than
something a caller times out on.

Both halves are exercised from both sides. A kernel self-test starts a thread on
a channel and waits for the *scheduler* to report it blocked before sending
anything — a receiver that polled would never appear in that count — then checks
the bytes that came out. A user program creates a channel from ring 3, writes,
reads it back, is refused a handle it was never given, and is told the peer has
closed after closing it.

Two processes talk. The kernel creates a channel and gives one end to each
before either starts, so neither can name the other and neither needs to — the
handle is the introduction and the authority at once. The client asks once and
blocks for the answer; the server answers whatever arrives until the channel
closes, which is how it learns the client has gone. Nothing polls and nothing
times out. The boot test checks the order as well as the presence: a request
that arrived after its answer would be two monologues rather than a round trip.

And a handle can be carried in a message, which is what makes the whole thing a
capability system rather than a pair of pipes. The client makes a second channel
and sends one end of it down the first; everything after that happens somewhere
the kernel never arranged. Handles move rather than copy — they leave the
sender's table at the moment the message is built, so there is no instant where
both processes hold one — and a send that fails after taking them puts them
back, because losing authority to a full queue would be a leak the caller could
not have avoided. Passing one on needs a right of its own, so a process can be
given something it may use and may not delegate.

Shared memory is the second kind of handle. A channel copies its message twice,
once out of the sender and once into the receiver, which is right for a request
and wrong for a framebuffer; this is the other arrangement. The frames exist
once, and a process holding the handle maps them wherever suits it — the two
programs that demonstrate it deliberately choose different addresses, because
sharing memory does not mean agreeing on where it goes. The handle crosses the
channel; the contents never do.

It needed one bit in a page-table entry. An address space frees every frame it
maps when it is dropped, and a shared frame is mapped in more than one, so
without a way to say "not mine" the second space to go would free a frame the
first had already returned. Bits 9 to 11 of an entry are ignored by the
processor and available to the operating system, which is what makes saying it
possible; the object that owns the frames frees them when its last handle does,
and the boot test fails if the numbers do not match.

And the first pixels a process put on the screen. `paint` is a program on the
disk that is handed a rectangle and a handle to the framebuffer -- the same
shared-memory mechanism two programs use to talk, pointed at memory the firmware
chose instead of at pages the allocator made. It maps it at an address of its
own choosing, is told the stride rather than guessing it, checks the rectangle
against the screen rather than trusting what it was sent, and fills it.

That is the shape a compositor has: a process holding a handle to the display,
not a thing inside the kernel. This is not one. It has no windows and no
clients, and the kernel still draws its own chrome -- but it now repaints
*around* the rectangle it gave away, which it did not before, and the first
version of this looked exactly like a program that had failed to draw.

Two things came out of measuring it. Mapping a nine-megabyte framebuffer meant
two and a half thousand rounds of interrupting every processor to shoot down a
translation for a page that had never been present -- and the architecture does
not permit a processor to have cached one, because there was nothing to cache.
New mappings now invalidate locally and say nothing to anyone else; changing or
removing a present mapping still broadcasts. And the painter's completion
message had nobody reading it, which the boot test caught by counting messages
sent against messages received.

Outstanding: events and semaphores. A process waits on a channel or on nothing.

## Continuous integration ✅

Not a phase, and it should have come earlier. Two jobs: one that needs only a
toolchain -- formatting, lints, the host unit tests, and that all four targets
build -- and one that needs QEMU and boots the thing.

The second is the one that matters. Every claim in this project is verified by
running rather than by compiling, and those runs are the ones that cannot be
reproduced from a build log. A hosted runner has no emulation acceleration, so
boots take seconds instead of milliseconds; the timeouts allow for it, and the
one measurement that would otherwise be sensitive to a slow machine -- the APIC
calibration -- measures against the PIT rather than assuming a frequency, so a
slower host gives a smaller number and not a wrong one. A failing run keeps the
serial logs, because a red cross is not a diagnosis.

## Phase 7 — Storage and NexusFS 🚧

- virtio-blk, then NVMe and AHCI
- Block cache, VFS layer
- NexusFS: copy-on-write, journaling, checksums, snapshots, compression

Delivered: PCI enumeration and a virtio block driver.

Everything the machine has that is not on the processor is behind PCI, so
finding devices came first: every function of every device, through the legacy
port window, which reaches all of what enumeration and a virtio driver need.

The disk is driven the way a modern device is driven -- descriptors placed in
memory for the hardware to come and fetch, rather than bytes pushed through a
port. A virtqueue is three shared arrays: descriptors, an available ring the
driver appends to, and a used ring the device appends to. A block request is
three descriptors chained -- a header, the data, and one byte for the device to
report on.

The image every sector of which begins with its own number, written as text, is
the point of the test: the failure a block driver has to be caught making is
fetching a *different* sector than it was asked for, and a disk of zeroes cannot
tell that apart from working. The boot test reads the first sector, one in the
middle and the last, checks that a sector past the end is refused, writes a
position-dependent pattern to a scratch sector, reads it back, and restores what
was there.

And the image is a real one, built here rather than by a formatter: a protective
master boot record, a GPT with its backup at the far end, and a FAT32 written
sector by sector -- boot sector, FSInfo, two file allocation tables, directories
as cluster chains of 32-byte entries. A test boots a machine with nothing but
that image attached, so the firmware has to find the partition table, recognise
the EFI system partition, read the filesystem and load the bootloader out of it;
NexusOS had never done any of that before, because QEMU had always been
pretending a directory was a filesystem on its behalf.

The data disk and the bootable image are separate files on purpose. A machine
given two bootable disks leaves the firmware to choose between them, and it
chose the one whose kernel was older -- which made every fault-injection build
boot a binary that was not the one under test, and every result mean nothing.

And the kernel reads both for itself. The GPT is checked rather than trusted --
the header's own checksum and the one over the entry array are both verified,
because a partition table is the one structure where believing a corrupt value
means writing to the wrong part of a disk. FAT32 follows: the boot parameter
block, cluster chains through the allocation table, directories as runs of
32-byte entries, and files by path.

The test reads three files and refuses a fourth. One in the root; one a
directory down, so that walking a path is exercised rather than merely compiled;
one longer than a cluster with position-dependent contents, so that clusters
stitched together in the wrong order fail rather than being the right length;
and a name that does not exist, which has to come back as an error rather than
as whatever was next in the directory.

And a program now comes off it. `init` is built as its own binary for its own
target -- the same custom-target machinery as the kernel, with the small code
model, because a user program lives in the low half of the address space -- and
the kernel reads it off the filesystem, loads its segments into an address space
that did not exist a moment earlier, and enters it in ring 3. Everything that
ran at user privilege before this was assembled into the kernel and copied into
a page. This is the difference between a system that can run user code and one
that can run *programs*.

The ELF loader is the same one the bootloader uses, moved into the shared crate
now that there are two callers. The one thing that differed between them is how
physical memory is reached -- identity map for the bootloader, direct map for the
kernel -- so that became a parameter rather than an assumption either of them
made about the other.

Every page of a loaded image is mapped, gaps between segments included, with the
permissions of whichever segment covers it and the least of everything for the
gaps. That is not tidiness: the image is one contiguous block from the buddy
allocator, and the pages are handed back individually when the address space is
dropped. A page that was allocated and never mapped would never be freed, and
the block it came from would stay split for the life of the system.

And a program can ask for another. There is no system call that creates a
process: there is a channel, and holding one end of it is the authority to ask.
`init` is given that end when the kernel starts it, sends the path of a program
down it, and gets back a message carrying a *handle* — a channel to whatever was
started. The two then talk, and neither can name the other or find any way to.
A program that was never given the spawner handle cannot ask, and there is no
name it could use instead, which is the argument for handles over a global
namespace made concrete rather than argued.

Finding that took a deadlock with it, and a quiet one. Reaping a finished thread
held the scheduler's lock while dropping it, and dropping a thread can drop the
last reference to its process, which drops its handle table, which drops the
channel endpoints in it — and an endpoint's destructor wakes whoever was blocked
on the other end, which takes that same lock. The machine ran every test
correctly and then simply stopped. Threads are now taken out of the table under
the lock and dropped outside it.

And now the system has a filesystem of its own. The disk carries a second
partition, empty when the build writes it, with a type GUID that means NexusFS
lives there. The first boot finds no superblock and formats it; every boot after
that mounts what the first one made. That order is deliberate. A filesystem the
build script laid out would prove the build script works; the thing worth
proving is that the kernel can make a filesystem it can then read.

The format is the plain one, chosen so that every part of it can be explained.
Four-kilobyte blocks. A superblock saying where each region is, checksummed over
every field that says where something is, so a half-written one says so instead
of sending a reader to the wrong block. A bitmap of free blocks, with the
metadata region and the bits past the end of the volume marked taken before the
bitmap first reaches the disk. A table of 128-byte inodes. Directories are
ordinary files whose contents happen to be a list of names.

An inode carries eleven direct block numbers and one indirect, which puts the
largest file at just over two megabytes. That is a small number and an honest
one: a second level of indirection is four lines and would make it a gigabyte,
and adding it before anything needs it would be adding a path nothing has ever
walked. Files are read and written entire, for the same reason -- there is no
buffer cache underneath, so a byte-at-a-time interface would be a byte-at-a-time
disk.

Two things are checked that a single boot cannot check. The first is leakage:
the self-test records the free-block and free-inode counts, makes a directory, a
small file, a file past the direct blocks with position-dependent contents,
shrinks it, refuses a duplicate name, refuses a name with a separator, refuses
removing a directory with something in it, refuses a file larger than the format
can describe, deletes everything, and requires both counts to be exactly what
they were. A write path that allocated a block and forgot it passes every other
test ever written and fails that one.

The second is persistence, which needs two boots and therefore its own test.
The kernel keeps `/system/boots` and `/system/boot.log` and writes to both on
every start. `scripts/test-persistence.ps1` makes a fresh disk, boots twice
without rebuilding, and requires the first boot to *make* a filesystem and
report boot 1 with one line in the log, and the second to *mount* one and report
boot 2 with two. Reading a file back in the same boot proves the code agrees
with itself; only the second boot proves anything reached the platter.

Writing that test brought the two-bootable-disks trap back for a second visit --
the new script built the data disk with the bootloader in it, the firmware
preferred it, and all six injection tests began booting a kernel that was not
the one under test. It is now a check rather than a thing to remember: attaching
the data disk scans it for `BOOTX64.EFI` and refuses to start a machine that
would have a choice.

And programs reach it -- through handles, and only through handles. There is no
system call that takes a path. A process opens one name inside a directory it
already holds, so what it can reach is exactly the subtree under what it was
given, and a `/` in a name is refused rather than walked because a directory
handle that could be escaped with `../..` would not be an authority over
anything. A handle opened through another carries no more rights than its
parent, which is what makes handing a program a read-only directory mean
something.

`init` starts holding two things: the channel to the spawn service, and the root
directory. It lists the root, makes its own directory, reads what the previous
boot left there, writes a file, reads it back through the same handle, and
checks the refusals -- a buffer too small, a name with a separator, a name that
is not there, and removing a file that is still open. The persistence test now
requires it to *make* that file on the first boot and *find* it on the second,
which is the claim one level up from the kernel's own: not that the filesystem
persists, but that a program can put something in it and get it back across the
system-call boundary.

Two things fall out of doing it this way. A buffer too small is an error and
never a truncation, because half a file that reports its own length is
indistinguishable from a whole one. And removing a name is refused while any
handle still names it: a handle carries an inode number, an inode number is not
a reference, and freeing the inode would leave the handle pointing at a number
the filesystem is free to give to the next file.

And a program can now be waited for. Until this, a process could start another
and talk to it and had no way to learn that it had finished or whether it had
worked; the channel closing said the other end was gone, which is not the same
claim. `Exit` takes a status, the spawn service replies with two handles rather
than one -- a channel to talk to it and the process to wait for it -- and
`ProcessWait` blocks until it ends and returns the number.

The handle names a *completion* and not the process, and that is the whole
design decision. A handle to the process would keep its address space alive for
as long as anybody remembered it, so a parent that never closed one would be a
memory leak shaped like politeness. A completion is an identifier, a name and an
outcome; it outlives the process by design and costs nothing to keep.

Waiting has two paths and only one of them is easy. `init` waits for the program
it asked for, and by then that program has almost always exited already -- so
what a boot exercises is a wait on something already finished, which returns
without ever blocking. The other path is the one with the lost wake-up in it,
and it gets its own self-test: a thread waits *first*, is checked to have left
the run queues rather than spun, and only then is the completion finished. If
the ending were published without waking the queue, or the waiter joined after
the ending was published, that thread would wait forever and so would every
program that ever waits for a child.

Two flags rather than one, for the same reason. One claims the ending, so that
exactly one caller ever stores a status; the other publishes it, so a waiter
that sees the flag cannot read a status that has not been written yet.

Six assembly programs had to be edited for this, and the edit is the point:
`Exit` now reads `rdi`, and they had been written when it took no arguments, so
they exited with whatever happened to be in that register. One of them reported
a pointer as its status. The suite now requires every process in a boot to exit
with zero, because a garbage status looks exactly like a working system until a
parent believes it means failure.

Outstanding: NexusFS has no journal, so a power failure between two writes can
leave the bitmap saying a block is taken that no file points at -- space leaked,
nothing corrupted, which is the right way round for the failure to be. No
permissions, no timestamps beyond the tick a thing was made at, no partial
writes and no seek, so a large file is read and written whole. The FAT32 reader
still cannot write and skips long names. The block driver polls one request at a
time and takes no interrupt. A program gets no arguments, nothing can end a
process but its own thread, and there is no way to wait for several things at
once.

## Phase 8 — Drivers and user space ⬜

- PCI/PCIe enumeration, MSI/MSI-X, IOMMU
- User-space driver model over IPC
- `init`, service manager, libc, runtime, shell

## Phase 9 — Graphics and the compositor ⬜

Partially anticipated: the kernel already has a bitmap font, text rendering and
a live status screen redrawn by its own thread. That is a boot display, not a
compositor — no surfaces, no damage tracking, no windows — and it exists to be
replaced by the real one.

- Nexus Graphics abstraction over the framebuffer, later a GPU
- Nexus Compositor: surfaces, damage tracking, frame scheduling, multi-monitor
- Input routing: focus, event delivery to processes, mouse and touchpad

## Phase 10 — NexusUI ⬜

Declarative, reactive, GPU-accelerated widgets: layout, text, theming,
animation, accessibility, DPI scaling, localisation.

## Phase 11 — Nexus Desktop ⬜

Dock, launcher, workspaces, snap layouts, Mission Control, notifications,
settings, files, terminal, search.

## Phase 12 — Networking ⬜

Ethernet, ARP, IPv4/IPv6, ICMP, UDP, TCP, DHCP, DNS, sockets, firewall.

## Phase 13 — Packages ⬜

The `.nexus` format, repositories, dependency resolution, signatures, rollback.

## Phase 14 — Security ⬜

Capabilities, sandboxing, the permission model, secure storage, code signing,
application identity, audit logging.

## Phase 15 — Compatibility ⬜

Linux: ELF loader, POSIX layer, libc, syscall translation.
Windows: PE loader, Win32 layer, DirectX translation.

## Phase 16 — Gaming ⬜

Vulkan, GPU drivers, controllers, HDR, VRR, shader cache, frame pacing.

## Phase 17 — AI ⬜

Nexus Intelligence (local and remote models, embeddings, semantic index),
Nexus Agent under capability control, Nexus Workflow automation.

Non-negotiable: the agent gets explicit, revocable, auditable capabilities.
Never ambient root.

## Phase 18 — Virtualisation ⬜

VT-x/AMD-V, virtio devices, containers.

## Phase 19 — Production hardening ⬜

Installer, recovery environment, A/B updates with rollback, crash reporting,
diagnostics, performance work, security audit.

---

## Internationalisation 🚧

Not a phase of its own: the specification asks for Japanese and English from the
start, so it was built alongside the display rather than retrofitted.

Delivered: translations in `locales/*.txt` with named placeholders so languages
can reorder their arguments, a build that fails on a missing translation, UTF-8
text rendering with half- and full-width advances, build-time glyph
rasterisation for CJK, and runtime language switching. See
[i18n.md](i18n.md).

Outstanding: input methods — the keyboard now exists and F1 switches language,
but composing Japanese needs a conversion engine and a candidate window — plus
text shaping, vertical writing, and further languages.

## Cross-cutting work

Carried alongside the phases rather than scheduled as one:

- **Real disk images.** A GPT + FAT32 builder, replacing QEMU's VVFAT, before
  any hardware boot.
- **CI.** `cargo fmt --check`, `clippy`, tests, both builds, and an automated
  QEMU boot test that fails on a missing serial marker.
- **Benchmarks.** Boot time, syscall and context-switch latency, IPC
  throughput, allocator throughput — measured from the phase that introduces
  each, so regressions are visible immediately.
- **Documentation.** A design document per subsystem, written with the code.
