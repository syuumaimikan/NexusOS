# NexusOS Architecture Audit

**Date:** 2026-09-08 (updated after I/O APIC and keyboard input)
**Scope:** full repository
**Verified by:** building both components and booting them in QEMU with edk2 firmware

---

## 1. Starting point

The repository was empty. There was no source, no build system, no bootloader,
no kernel, no CI, and no documentation — so there was nothing to preserve and
no existing design to work around. Every decision recorded below was made from
scratch for this project.

This matters for one requirement in particular: NexusOS must not be a Linux
derivative. Starting from an empty tree means the kernel has no Linux lineage to
disentangle. Linux compatibility, when it arrives, will be a translation layer
*above* the Nexus system-call interface, not a modified Linux kernel.

## 2. Status by subsystem

Legend: **DONE** works and is verified · **PARTIAL** real but incomplete ·
**MISSING** not started

### Boot and platform

| Subsystem | Status | Notes |
|---|---|---|
| UEFI bootloader | **DONE** | Hand-written UEFI bindings, no third-party crates |
| Display mode selection | **DONE** | GOP mode ranking, caps at 1920×1200 |
| ESP file loading | **DONE** | Reads from the volume the loader itself booted from |
| ELF64 loading | **DONE** | `PT_LOAD` segments, `.bss` zeroing, malformed-input rejection |
| Page table construction | **DONE** | Identity map, direct map, per-segment kernel map, guarded stack |
| Memory map handoff | **DONE** | Normalized, sorted, coalesced |
| ACPI RSDP discovery | **DONE** | ACPI 2.0 preferred, 1.0 fallback |
| `ExitBootServices` | **DONE** | With map-key retry loop |
| ACPI table parsing | **DONE** | RSDP, XSDT/RSDT and MADT, with checksums verified |
| Secure Boot / measured boot | **MISSING** | Deferred to Phase 19 |
| A/B update, recovery | **MISSING** | Deferred to Phase 19 |

### Kernel core

| Subsystem | Status | Notes |
|---|---|---|
| Kernel entry, higher half | **DONE** | Non-PIE at `0xFFFFFFFF80000000`, kernel code model |
| Serial console | **DONE** | 16550 at COM1, survives `ExitBootServices` |
| Panic handler | **DONE** | Lock-free output, re-entry guard |
| Spinlock | **DONE** | Plain and interrupt-masking variants |
| GDT / TSS | **DONE** | `syscall`/`sysret` ordering; IST stacks |
| IDT / exceptions | **DONE** | All 256 vectors; decoded fault reports |
| Legacy PIC / PIT timer | **DONE** | Remapped above the exception vectors; 1000 Hz |
| Physical memory allocator | **DONE** | Buddy allocator; 1017 MiB over 260513 frames |
| Virtual memory manager | **DONE** | map/unmap/translate; identity map torn down |
| Kernel heap | **DONE** | 16 MiB, `GlobalAlloc`, `alloc` available |
| Kernel threads, scheduler | **DONE** | Preemptive, priority + round-robin, sleep/wake, reaping |
| Framebuffer drawing | **PARTIAL** | Text, rectangles, gradients; no compositor, no windows |
| Localisation | **DONE** | English and Japanese, switchable at runtime |
| Text rendering | **PARTIAL** | UTF-8, half and full width, integer scaling; no shaping |
| Keyboard input | **DONE** | PS/2 set 1, interrupt driven, decoded to keys |
| Input dispatch | **PARTIAL** | The kernel acts on keys itself; no focus, no delivery |
| IME | **MISSING** | Latin only; nothing to compose Japanese with |
| Local APIC, APIC timer | **DONE** | Calibrated against the PIT; drives the tick |
| SMP bring-up | **DONE** | All processors started, each on its own APIC timer |
| SMP scheduling | **DONE** | Every processor schedules; shared run queues, per-CPU current thread |
| TLB shootdown | **DONE** | Mailbox per processor, interrupted and polled; verified by injection |
| I/O APIC | **DONE** | Redirection entries programmed, source overrides honoured |
| PCI enumeration | **DONE** | Every function of every device, through the legacy port window |
| Block device | **PARTIAL** | virtio-blk over the legacy transport, one request at a time, polled |
| Disk image | **DONE** | GPT, protective MBR and a FAT32 the firmware boots from |
| Partition table | **DONE** | GPT read and both checksums verified |
| Filesystems | **PARTIAL** | FAT32 read only: paths, cluster chains, no long names, no writing |
| MSI, MSI-X | **MISSING** | Devices are found; nothing routes their interrupts yet |
| Ring 3 | **DONE** | A user thread runs at CPL 3, preemptible, in its own pages |
| User programs | **PARTIAL** | Built as their own binaries and loaded from disk; no arguments, no dynamic linking |
| Process creation | **PARTIAL** | A process can ask a service over a channel; no exit status, no parent |
| System calls | **PARTIAL** | `syscall`/`sysret` entry, ten calls; no shared memory or events |
| Processes, address spaces | **DONE** | A page-table root per process; kernel upper half shared by pointer |
| Handles, capabilities | **DONE** | Per-process table, rights checked on every use |
| IPC | **DONE** | Blocking channels, handles carried in messages, rights checked on transfer |
| Wait queues | **DONE** | Blocking with no lost wake-ups; the input thread no longer polls |

### Everything above the kernel

Drivers, NexusFS, networking, USB, audio, the compositor, NexusUI, the desktop,
the package manager, security/capabilities, Linux and Windows compatibility,
virtualisation, containers, and the AI subsystems are all **MISSING**. They are
sequenced in the roadmap and none of them can be built honestly before the
kernel core exists.

## 3. What is actually verified

Not asserted — observed, on every boot:

- Firmware hands control to the bootloader; the loader logs over serial.
- A 1920x1200 BGRX framebuffer is selected and reported at `0x80000000`.
- The ACPI RSDP is found in the configuration table.
- The kernel ELF is read from the ESP and its three `PT_LOAD` segments are
  loaded into physical memory.
- Page tables are built in 15 pages and `cr3` is switched; the kernel reads back
  the same root the loader installed, which proves the switch took effect.
- The kernel runs at its linked higher-half address, on the guarded boot stack.
- 1017 MiB of usable RAM is classified across 22 regions.
- The framebuffer is painted through the direct map, confirmed by screenshot.
- The GDT, TSS and a 256-vector IDT are installed; a deliberate `int3` is
  dispatched and *resumed from*, which exercises the whole `iretq` path.
- The PIT ticks at exactly 1000 Hz, with no spurious interrupts over 30 seconds.
- The frame allocator hands out 64 frames, each of which is stamped with a
  distinct pattern through the direct map and read back correctly, plus a 1 MiB
  block that is 1 MiB aligned; nothing leaks.
- The heap serves a 50000-element `Vec`, a boxed 4 KiB array, a 1000-entry
  `BTreeMap` and a formatted `String`; a heap address is translated back to
  physical and the same bytes are read through the direct map, which is what
  proves the mappings are correct rather than merely plausible.
- After teardown, a low address no longer translates.
- ACPI 2.0 tables are located and validated: the XSDT and the MADT, reporting
  four processors, one I/O APIC and the presence of a legacy 8259.
- All four processors ACPI reports are started, and each takes within a few
  interrupts of the same count over 25 seconds — about 1000 Hz apiece, which is
  what says each has its own working timer rather than sharing one.
- The local APIC timer is calibrated against the PIT at around 1.2 GHz, takes
  over the tick at 1000 Hz, and the 8259 and PIT are shut down behind it.
  Uptime then tracks wall clock to within a millisecond per five seconds, which
  is what says the handover preserved the clock rather than merely survived it.
- Four worker threads run to completion with exactly the expected iteration
  count, and their stacks are unmapped and returned when they are reaped.
- A thread that never yields, at equal priority, is preempted: a sleeping
  ticker wakes on schedule five times while the non-yielding thread completes
  around twelve million iterations. Cooperative scheduling would hang here, so
  this is the check that distinguishes real preemption from the appearance of
  it.
- The keyboard's interrupt is routed through the I/O APIC to its vector, with
  the MADT's source overrides applied. Keys typed into QEMU's monitor arrive as
  scancodes, decode to the letters that were sent, and reach something that
  acts on them: `scripts/test-input.ps1` types `nexus` and F1 and checks the
  guest reports the word back and switches language. A unit test could check a
  scancode table; only this checks that the pin, the vector, the handler's
  drain of the controller and the delivery all work.
- A 1920x1200 status screen renders live uptime, memory, heap, thread and
  context-switch figures, repainted twice a second by its own thread and
  captured by `scripts/screenshot.ps1`.
- That screen renders correctly in both English and Japanese, with the memory
  line reordered by the translation rather than by the code, and the panel
  resized to the text it actually holds. Both are captured as screenshots.

50 host unit tests cover the UEFI structure offsets, the ELF parser, memory-map
normalization, the buddy allocator and the heap — the last two including
20000-step randomised workloads that check after every operation that no memory
is covered by two live allocations.

Three fault-injection builds take real CPU faults and confirm each is reported
rather than resetting the machine, including a stack overflow that runs guard
page to page fault to double fault to IST stack.

The build produces zero warnings and passes `clippy -D warnings`.

## 4. Known deficiencies

These are real and are tracked, not hidden:

1. **The block driver polls, one request at a time.** Completion is waited for
   by reading the used ring rather than by taking the device's interrupt, and
   the driver holds its lock across the whole transfer. That is the wrong shape
   for a system that cares about power or throughput, and the right shape for a
   first driver: an interrupt-driven request needs somewhere to hand the buffer
   back to, which is the block layer that does not exist yet.
2. **No device teardown.** Nothing resets a device or frees its queue, because
   nothing shuts down. A device left mastering the bus into memory the
   allocator had taken back would be serious, so this wants doing before there
   is any path that frees a driver's memory.
3. **No shared memory, events or semaphores.** A channel copies its message
   twice, once out of the sender and once into the receiver, which is right for
   a request and wrong for a framebuffer. Shared memory with a handle to it is
   the next object kind, and the compositor will need it before anything else
   does.
4. **A process has no parent and no exit status.** One can be started at a
   process's request, over a channel, and what comes back is a way to talk to
   it — but nothing says when it ended or how, and nothing outlives it. Waiting
   on a process wants an object of its own, which is the next kind of handle.
5. **A program gets no arguments and no environment.** It is entered with a
   stack and nothing on it. Whatever a program needs to be told should reach it
   through a handle it was given, which is the right shape and is not yet wired
   to anything.
6. **No user memory copy helpers.** `Call::Log` validates its range and then
   reads it directly. A user pointer that is unmapped faults in the kernel, on
   the kernel's stack, and is reported as a kernel fault; it should be turned
   into an error returned to the caller. That needs a fault handler that knows
   about a fixup table, which is its own piece of work.
7. **The run queues are shared, not per-processor.** One lock covers the thread
   table and all four queues. That is correct and it is what makes every
   processor able to take work, but it is a point of contention that will matter
   once there are more processors or more threads than a desktop has today.
   Per-processor queues with balancing between them is the next step, and it
   wants contention to measure rather than to be guessed at.
8. **No thread affinity.** A thread can be resumed on any processor, which is
   right for fairness and wrong for cache locality. There is nothing to measure
   it with yet.
9. **Shootdowns are broadcast to every processor.** A processor that never
   loaded the address space is interrupted anyway, because nothing tracks which
   spaces are live where. Correct, and more work than necessary now that there
   is more than one address space to be wrong about.
10. **The framebuffer is mapped write-back, not write-combining.** Correct in
    QEMU, slow on real hardware. Needs PAT configuration.
11. **Bootloader allocations are over-conservative.** Page tables, the handoff
    block and the kernel image are allocated as `RuntimeServicesData`, which the
    kernel treats as permanently reserved. This wastes on the order of 100 KiB.
12. **The filesystem reader cannot write, and skips long names.** FAT32 is
    read only: a writer has to keep two allocation tables and the free-cluster
    count consistent through a power failure, and nothing yet needs to write to
    the boot partition. Long names are skipped rather than half-assembled,
    because a partial implementation would look like it worked. NexusFS is not
    started.
13. **CI has never run.** The workflow is written and its commands are checked
    locally, but nothing has pushed to the remote, so no run exists to point
    at. It also has no acceleration available on a hosted runner, which makes
    the QEMU layers minutes rather than seconds.
14. **The heap never shrinks.** It grows on demand and keeps what it takes.
    Acceptable for a kernel of this size; worth revisiting when there are
    long-running workloads.
15. **The kernel binary has no host test harness.** It is `no_main` with its own
    panic handler, so tests written inside it would compile and never run. What
    can be checked statically is checked with const assertions; the rest is
    covered by boot-marker checks, fault injection and screenshots. Logic worth
    unit testing is moved into a library crate instead, which is why the
    allocators live in `nexus-mm`.
16. **No ageing in the scheduler.** Strict priority means a busy high-priority
    thread starves everything below it. Deliberate for now, and it needs real
    workloads before it can be tuned honestly.
17. **CJK glyphs depend on the build machine.** They are rasterised at build
    time from an installed font, because bundling one would redistribute it.
    A machine without a suitable font still builds, but non-Latin text renders
    as placeholder boxes. See [i18n.md](i18n.md).
18. **Input goes nowhere but the kernel.** Keys are decoded and acted on
    inside the kernel because there is no focus, no window and no process to
    deliver them to. There is no IME either, so Japanese can be displayed but
    not typed.
19. **No text shaping.** Each glyph sits on a fixed grid: no vertical writing,
    no bidirectional text, no ligatures or combining marks.

Resolved since the first audit: the missing IDT (Phase 2), the
non-interrupt-safe spinlock (`IrqSpinLock`, Phase 2), the unstripped kernel
image on the ESP, which was costing megabytes of boot-time reads for a
hundred-kilobyte load, the two scripts that staged that ESP differently, so
whichever ran last decided what the next boot would load, and the
single-processor scheduler: every processor now runs threads, and the boot test
fails if the workers all land on one of them; booting only from a directory QEMU
pretended was a filesystem, which is now a genuine GPT and FAT32 that a test
boots with nothing else attached, and which the kernel now reads for itself;
programs that could only be assembled into the kernel, which are now files on
that filesystem, built as their own binaries and loaded into address spaces of
their own; and process creation that only the kernel could decide on, which is
now a request a program makes over a channel; the absence of any user mode at
all — a program now runs at ring 3, and the boot test fails unless an interrupt
was taken from it; the single address space, which is now one per process, with
the boot test comparing the two processes' page tables and the injection suite
building a kernel that shares a page between them; and a scheduler bug that lost
about one thread per dozen boots, described below; and polling in the input
thread, which now blocks on a wait queue -- 1766 context switches in ten seconds
became 688. Stale translations on the other
cores went with it: mapping changes are shot down across every processor, and a
build that deliberately keeps the shootdown local is one of the injection tests,
so the check that looks for staleness has been seen to fail when there is some.

## 5. Architectural decisions and why

**Hybrid kernel, not a pure microkernel.** Scheduling, memory, IPC and the
handle table stay in the kernel; drivers move out as the driver model matures.
The spec ranks correctness, reliability and debuggability above architectural
purity, and a pure microkernel pays an IPC cost on every path before any of that
is in place.

**Zero third-party crates in the boot path.** Every line of the loader and
kernel is project code. It costs more to write and it is the only way the claim
of a fully self-built OS survives scrutiny.

**Custom target specification.** `targets/x86_64-nexus.json` disables the red
zone and SSE and selects the kernel code model. Soft float is a deliberate early
choice: enabling SSE requires FPU state management that belongs with the
scheduler, not before it.

**Direct physical map at `0xFFFF800000000000`.** A permanent window onto all RAM
makes the physical allocator and page-table code straightforward. It costs 64 TiB
of address space, which on x86-64 is free.

**Per-segment kernel mapping.** `.text` is read-only and executable, `.rodata`
read-only and non-executable, `.data`/`.bss` writable and non-executable, from
the first instruction the kernel runs. Retrofitting W^X later is much harder.

**Serial as the primary console.** It works before anything else does, survives
`ExitBootServices`, and is machine-readable, which is what makes automated boot
testing possible.

## 6. Next actions

In order, and for the reason given:

1. **An exit status, and something to wait on it**, so that a process that
   started another can find out how it ended rather than only that it began.
2. **Shared memory**, which is the next kind of handle and the one the
   compositor will need: a channel copies its message twice, which is right for
   a request and wrong for a framebuffer.

See [NEXUSOS_ROADMAP.md](NEXUSOS_ROADMAP.md) for the full sequence.
