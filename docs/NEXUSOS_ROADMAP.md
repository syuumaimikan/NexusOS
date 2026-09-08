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

Outstanding: shared memory, events and semaphores. A channel copies its message
twice, which is right for a request and wrong for a framebuffer.

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

Outstanding: nothing reads a partition table or a filesystem, so the disk holds
sectors and not files. The driver polls one request at a time and takes no
interrupt. NexusFS is not started.

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
